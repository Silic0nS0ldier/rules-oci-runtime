//! Deciding what an image's layers actually need to place.
//!
//! A path a later layer overwrites, or a whiteout removes, is written and then
//! thrown away. On a 22 layer image that is most of a gigabyte of writes whose
//! only lasting effect is to be replaced. The entry tables recorded beside the
//! checkpoint indexes say what every layer holds, so the whole image can be
//! resolved before the first byte is written and the doomed entries skipped.
//!
//! Skipping is decided on the paths as the layers spell them, while extraction
//! still resolves those paths against the filesystem it is building. That is
//! sound in the one direction it is used: if two layers name the same path and
//! the later one is placed, the earlier one cannot survive. Either both names
//! resolve to the same file, and the later write replaces it, or something
//! between them was replaced to make them differ, and that replacement removed
//! the earlier file anyway.
//!
//! It is not sound in the other direction, so nothing here decides *where* an
//! entry goes, only whether it is worth placing at all.

use std::collections::{BTreeMap, HashMap, HashSet};

use camino::Utf8Path;

use crate::entries::{Kind, Table};
use crate::image::{Descriptor, parse_digest};
use crate::log::{log, warning};

const WHITEOUT_PREFIX: &[u8] = b".wh.";
const OPAQUE_WHITEOUT: &[u8] = b".wh..wh..opq";

/// The entries each layer can skip, keyed by layer digest.
#[derive(Default)]
pub struct Plan {
    shadowed: HashMap<String, HashSet<Vec<u8>>>,
}

impl Plan {
    /// Resolves the image, or returns an empty plan when it cannot be.
    ///
    /// Every layer has to be accounted for: one missing table means unknown
    /// entries, and an entry that is not known about cannot be shown to be
    /// safely skippable. Planning is an optimisation, so falling back to
    /// placing everything is always available.
    pub fn build(index_dir: Option<&Utf8Path>, layers: &[Descriptor]) -> Plan {
        let Some(dir) = index_dir else {
            return Plan::default();
        };
        let mut tables = Vec::with_capacity(layers.len());
        for layer in layers {
            let Ok(digest) = parse_digest(&layer.digest) else {
                return Plan::default();
            };
            let path = dir.join(format!("{}.entries", digest.hex));
            let file = match std::fs::File::open(&path) {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Plan::default(),
                Err(err) => {
                    warning!("ignoring entry table {path}: {err}");
                    return Plan::default();
                }
            };
            match Table::read_from(std::io::BufReader::new(file)) {
                Ok(table) => tables.push(table),
                Err(err) => {
                    warning!("ignoring entry table {path}: {err}");
                    return Plan::default();
                }
            }
        }
        Plan::resolve(layers, &tables)
    }

    fn resolve(layers: &[Descriptor], tables: &[Table]) -> Plan {
        // A hard link is made against what is already on disk, so whatever it
        // names has to be placed even where a later layer replaces it. There
        // are single figures of these in a real image.
        let linked: HashSet<&[u8]> = tables
            .iter()
            .flat_map(|table| &table.entries)
            .filter(|entry| entry.kind == Kind::HardLink)
            .map(|entry| entry.link.as_slice())
            .collect();

        // Sorted by path, so everything under a directory is one range and a
        // whiteout costs what it removes rather than a pass over the image.
        let mut winner: BTreeMap<&[u8], (usize, usize)> = BTreeMap::new();
        for (l, table) in tables.iter().enumerate() {
            for (e, entry) in table.entries.iter().enumerate() {
                match whiteout(&entry.path) {
                    Some(Whiteout::Opaque(dir)) => remove_under(&mut winner, &dir),
                    Some(Whiteout::Named(target)) => {
                        winner.remove(target.as_slice());
                        remove_under(&mut winner, &target);
                    }
                    None => {
                        winner.insert(&entry.path, (l, e));
                    }
                }
            }
        }

        let mut shadowed: HashMap<String, HashSet<Vec<u8>>> = HashMap::new();
        let mut total = 0;
        let mut bytes = 0u64;
        for (l, table) in tables.iter().enumerate() {
            let mut doomed = HashSet::new();
            for (e, entry) in table.entries.iter().enumerate() {
                // Only bodies are worth skipping, and a whiteout marker has to
                // reach the extractor or nothing gets removed.
                if entry.kind != Kind::File || whiteout(&entry.path).is_some() {
                    continue;
                }
                if linked.contains(entry.path.as_slice()) {
                    continue;
                }
                if winner.get(entry.path.as_slice()) != Some(&(l, e)) {
                    bytes += entry.size;
                    doomed.insert(entry.path.clone());
                }
            }
            total += doomed.len();
            if !doomed.is_empty() {
                shadowed.insert(layers[l].digest.clone(), doomed);
            }
        }
        if total > 0 {
            log!(
                "Skipping {total} files ({} MiB) that later layers replace",
                bytes >> 20
            );
        }
        Plan { shadowed }
    }

    /// True when this layer's copy of `path` is replaced or removed by a later
    /// one, and so never has to be written.
    pub fn is_shadowed(&self, digest: &str, path: &[u8]) -> bool {
        self.shadowed
            .get(digest)
            .is_some_and(|doomed| doomed.contains(path))
    }
}

enum Whiteout {
    /// `.wh..wh..opq`: hides everything the lower layers put in this directory.
    Opaque(Vec<u8>),
    /// `.wh.name`: hides the one path it names.
    Named(Vec<u8>),
}

fn whiteout(path: &[u8]) -> Option<Whiteout> {
    let (dir, name) = match path.iter().rposition(|&b| b == b'/') {
        Some(at) => (&path[..at], &path[at + 1..]),
        None => (&path[..0], path),
    };
    if name == OPAQUE_WHITEOUT {
        return Some(Whiteout::Opaque(dir.to_vec()));
    }
    let target = name.strip_prefix(WHITEOUT_PREFIX)?;
    // `.wh.` with nothing after it names nothing.
    if target.is_empty() {
        return None;
    }
    // The marker sits in the middle of the path, so what it names has to be
    // put back together rather than sliced out.
    let mut named = Vec::with_capacity(dir.len() + 1 + target.len());
    if !dir.is_empty() {
        named.extend_from_slice(dir);
        named.push(b'/');
    }
    named.extend_from_slice(target);
    Some(Whiteout::Named(named))
}

/// Drops every entry beneath `dir`, without touching a sibling whose name
/// merely starts the same way.
fn remove_under(winner: &mut BTreeMap<&[u8], (usize, usize)>, dir: &[u8]) {
    let mut low = dir.to_vec();
    low.push(b'/');
    let mut high = low.clone();
    // One past every path that continues with a separator.
    *high.last_mut().expect("a trailing separator") = b'/' + 1;
    let doomed: Vec<&[u8]> = winner
        .range::<[u8], _>((
            std::ops::Bound::Included(low.as_slice()),
            std::ops::Bound::Excluded(high.as_slice()),
        ))
        .map(|(path, _)| *path)
        .collect();
    for path in doomed {
        winner.remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entries::Entry;

    fn descriptor(n: u8) -> Descriptor {
        Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            digest: format!("sha256:{:064x}", n),
            size: 0,
            platform: None,
        }
    }

    fn file(path: &str) -> Entry {
        Entry {
            kind: Kind::File,
            mode: 0o644,
            mtime: 0,
            offset: 0,
            size: 1,
            path: path.as_bytes().to_vec(),
            link: Vec::new(),
        }
    }

    fn table(paths: &[&str]) -> Table {
        Table {
            entries: paths.iter().map(|p| file(p)).collect(),
        }
    }

    fn plan_of(layers: &[Table]) -> (Plan, Vec<Descriptor>) {
        let descriptors: Vec<Descriptor> = (0..layers.len() as u8).map(descriptor).collect();
        (Plan::resolve(&descriptors, layers), descriptors)
    }

    #[test]
    fn a_later_layer_shadows_an_earlier_copy() {
        let (plan, d) = plan_of(&[table(&["a", "keep"]), table(&["a"])]);
        assert!(plan.is_shadowed(&d[0].digest, b"a"));
        assert!(!plan.is_shadowed(&d[0].digest, b"keep"));
        assert!(!plan.is_shadowed(&d[1].digest, b"a"), "the winner stays");
    }

    #[test]
    fn a_whiteout_shadows_what_it_hides() {
        let (plan, d) = plan_of(&[table(&["dir/a", "dir/b", "other"]), table(&["dir/.wh.a"])]);
        assert!(plan.is_shadowed(&d[0].digest, b"dir/a"));
        assert!(!plan.is_shadowed(&d[0].digest, b"dir/b"));
        assert!(!plan.is_shadowed(&d[0].digest, b"other"));
    }

    /// The marker is itself a regular file entry, and the extractor has to see
    /// it or nothing is removed from the tree the lower layers built.
    #[test]
    fn a_whiteout_marker_is_never_shadowed() {
        let (plan, d) = plan_of(&[table(&["dir/.wh.a"]), table(&["dir/.wh.a"])]);
        assert!(!plan.is_shadowed(&d[0].digest, b"dir/.wh.a"));
        assert!(!plan.is_shadowed(&d[1].digest, b"dir/.wh.a"));
    }

    #[test]
    fn an_opaque_whiteout_shadows_the_directory_it_names() {
        let (plan, d) = plan_of(&[
            table(&["dir/a", "dir/sub/b", "dirty", "elsewhere"]),
            table(&["dir/.wh..wh..opq"]),
        ]);
        assert!(plan.is_shadowed(&d[0].digest, b"dir/a"));
        assert!(plan.is_shadowed(&d[0].digest, b"dir/sub/b"));
        assert!(
            !plan.is_shadowed(&d[0].digest, b"dirty"),
            "a sibling that merely starts the same way is not underneath it"
        );
        assert!(!plan.is_shadowed(&d[0].digest, b"elsewhere"));
    }

    #[test]
    fn a_whiteout_of_a_directory_shadows_what_is_under_it() {
        let (plan, d) = plan_of(&[table(&["dir/a", "dir", "dirty"]), table(&["\
.wh.dir"])]);
        assert!(plan.is_shadowed(&d[0].digest, b"dir/a"));
        assert!(plan.is_shadowed(&d[0].digest, b"dir"));
        assert!(!plan.is_shadowed(&d[0].digest, b"dirty"));
    }

    /// A later layer restoring a path the whiteout removed keeps it.
    #[test]
    fn a_path_written_after_a_whiteout_survives() {
        let (plan, d) = plan_of(&[table(&["a"]), table(&[".wh.a"]), table(&["a"])]);
        assert!(plan.is_shadowed(&d[0].digest, b"a"));
        assert!(!plan.is_shadowed(&d[2].digest, b"a"));
    }

    /// A hard link is made against the tree as it stands, so the copy it names
    /// has to be there even when a later layer replaces it.
    #[test]
    fn a_file_a_hard_link_names_is_never_shadowed() {
        let mut linking = table(&["link"]);
        linking.entries[0].kind = Kind::HardLink;
        linking.entries[0].link = b"target".to_vec();

        let (plan, d) = plan_of(&[table(&["target"]), linking, table(&["target"])]);
        assert!(
            !plan.is_shadowed(&d[0].digest, b"target"),
            "the copy the link was made against has to be placed"
        );
    }

    #[test]
    fn an_absent_table_plans_nothing() {
        let plan = Plan::build(None, &[descriptor(0)]);
        assert!(!plan.is_shadowed(&descriptor(0).digest, b"anything"));
    }

    #[test]
    fn whiteout_names_are_recognised() {
        assert!(matches!(whiteout(b"dir/.wh..wh..opq"), Some(Whiteout::Opaque(d)) if d == b"dir"));
        assert!(matches!(whiteout(b".wh..wh..opq"), Some(Whiteout::Opaque(d)) if d.is_empty()));
        assert!(matches!(whiteout(b"dir/.wh.name"), Some(Whiteout::Named(n)) if n == b"dir/name"));
        assert!(matches!(whiteout(b".wh.name"), Some(Whiteout::Named(n)) if n == b"name"));
        assert!(whiteout(b"dir/name").is_none());
        assert!(whiteout(b"dir/.wh.").is_none());
    }
}
