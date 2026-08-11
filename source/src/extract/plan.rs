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

/// What `create_dir` would have given a directory nothing names.
const DEFAULT_DIRECTORY_MODE: u32 = 0o755;

/// The entries each layer can skip, keyed by layer digest, and the directory
/// tree they all need.
#[derive(Default)]
pub struct Plan {
    shadowed: HashMap<String, HashSet<Vec<u8>>>,
    directories: Vec<(Vec<u8>, u32)>,
    /// False when the image could not be resolved, and every layer therefore
    /// has to be placed in full.
    resolved: bool,
    /// What survives, for a caller that means to place it directly rather than
    /// by walking the layers. `None` when something in the image rules that
    /// out; see [`Plan::placeable`].
    work: Option<Work>,
    tables: Vec<Table>,
}

/// The surviving entries, grouped the way they have to be placed.
#[derive(Default)]
pub struct Work {
    /// Regular files, by layer, ordered by where their bodies sit in the
    /// layer's uncompressed stream.
    pub files: Vec<Vec<u32>>,
    /// Symlinks, shallowest path first, so one standing under another has
    /// somewhere to be.
    pub symlinks: Vec<(u32, u32)>,
    /// Hard links, which need the files they name to be on disk already.
    pub hard_links: Vec<(u32, u32)>,
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
        Plan::resolve(layers, tables)
    }

    fn resolve(layers: &[Descriptor], tables: Vec<Table>) -> Plan {
        // A hard link is made against what is already on disk, so whatever it
        // names has to be placed even where a later layer replaces it. There
        // are single figures of these in a real image.
        let linked: HashSet<&[u8]> = tables
            .iter()
            .flat_map(|table| &table.entries)
            .filter(|entry| entry.kind == Kind::HardLink)
            .map(|entry| entry.link.as_slice())
            .collect();

        let tree = replay(&tables);

        let mut shadowed: HashMap<String, HashSet<Vec<u8>>> = HashMap::new();
        let mut total = 0;
        let mut bytes = 0u64;
        for (l, table) in tables.iter().enumerate() {
            let mut doomed = HashSet::new();
            for entry in table.entries.iter() {
                // Only bodies are worth skipping, and a whiteout marker has to
                // reach the extractor or nothing gets removed.
                if !entry.kind.is_file() || whiteout(&entry.path).is_some() {
                    continue;
                }
                if linked.contains(entry.path.as_slice()) {
                    continue;
                }
                // Whether the layer wins the path, not whether this entry
                // does: a layer that names one path twice is skipped by path,
                // so shadowing the loser would take the winner with it.
                let survives = tree
                    .get(entry.path.as_slice())
                    .is_some_and(|node| node.owner.0 == l);
                if !survives {
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
        let directories = directories(&tree);
        let work = placeable(&tree, &tables);
        Plan {
            shadowed,
            directories,
            resolved: true,
            work,
            tables,
        }
    }

    /// True when the whole image was resolved, so the directory tree below is
    /// the one it ends up with and the layers need not build it themselves.
    pub fn is_resolved(&self) -> bool {
        self.resolved
    }

    /// The directories the image needs, parents before children.
    pub fn directories(&self) -> &[(Vec<u8>, u32)] {
        &self.directories
    }

    /// What survives, when the image is one that can be placed straight from
    /// the plan rather than by walking its layers.
    pub fn work(&self) -> Option<&Work> {
        self.work.as_ref()
    }

    pub fn table(&self, layer: usize) -> &Table {
        &self.tables[layer]
    }

    /// True when this layer's copy of `path` is replaced or removed by a later
    /// one, and so never has to be written.
    pub fn is_shadowed(&self, digest: &str, path: &[u8]) -> bool {
        self.shadowed
            .get(digest)
            .is_some_and(|doomed| doomed.contains(path))
    }
}

/// Groups the surviving entries for a caller that will place them directly.
///
/// Returns `None` where the image holds something that only a walk of the
/// layers can deal with:
///
/// - a sparse file, whose body is not a flat run of the stream, so nothing can
///   be written from an offset and a length;
/// - an entry that resolves through a symlink, where the path the layer spells
///   is not the path it lands on, and working that out means resolving against
///   the tree as it is built.
fn placeable(tree: &BTreeMap<&[u8], Node>, tables: &[Table]) -> Option<Work> {
    let mut work = Work {
        files: vec![Vec::new(); tables.len()],
        ..Work::default()
    };
    for (path, node) in tree {
        let (layer, entry) = node.owner;
        // A `.wh.` naming nothing is invalid and the walk rejects it, so the
        // plan must not quietly place it as an ordinary file instead.
        if basename(path) == WHITEOUT_PREFIX {
            return None;
        }
        match node.kind {
            Kind::Directory | Kind::Unsupported => continue,
            Kind::Sparse => return None,
            _ if behind_a_link(tree, path) => return None,
            Kind::File => work.files[layer].push(entry as u32),
            // A `BTreeMap` orders by path, so a link under another link is
            // already after it.
            Kind::Symlink => work.symlinks.push((layer as u32, entry as u32)),
            Kind::HardLink => work.hard_links.push((layer as u32, entry as u32)),
        }
    }
    for (layer, entries) in work.files.iter_mut().enumerate() {
        entries.sort_unstable_by_key(|&e| tables[layer].entries[e as usize].offset);
    }
    Some(work)
}

/// What the layers leave at a path once all of them have been applied.
struct Node {
    kind: Kind,
    /// The layer and entry that put it there.
    owner: (usize, usize),
    mode: u32,
}

/// Replays every layer in order into the tree they build between them.
///
/// This mirrors what the extractor does to the filesystem, entry for entry,
/// which is what makes the result something the extractor can be held to:
/// a directory keeps a symlink already standing where it wants to be, and
/// anything else takes over the path it names and everything underneath it.
///
/// Sorted by path, so everything under a directory is one contiguous range and
/// a removal costs what it removes rather than a pass over the whole image.
fn replay(tables: &[Table]) -> BTreeMap<&[u8], Node> {
    let mut tree: BTreeMap<&[u8], Node> = BTreeMap::new();
    let mut bound = Vec::new();
    for (l, table) in tables.iter().enumerate() {
        for (e, entry) in table.entries.iter().enumerate() {
            match whiteout(&entry.path) {
                // A whiteout hides the layers below it and never its own, so
                // where the marker sits in the layer does not matter.
                Some(Whiteout::Opaque(dir)) => remove_under(&mut tree, &dir, &mut bound, l),
                Some(Whiteout::Named(target)) => {
                    if tree
                        .get(target.as_slice())
                        .is_some_and(|node| node.owner.0 < l)
                    {
                        tree.remove(target.as_slice());
                    }
                    remove_under(&mut tree, &target, &mut bound, l);
                }
                None => {
                    let node = Node {
                        kind: entry.kind,
                        owner: (l, e),
                        mode: entry.mode,
                    };
                    match entry.kind {
                        // The extractor warns and moves on, leaving the path
                        // as it found it.
                        Kind::Unsupported => {}
                        Kind::Directory => {
                            // `prepare_directory` keeps a symlink that already
                            // resolves to a directory, so the layer's own
                            // directory entry does not take the path back.
                            let keep = tree
                                .get(entry.path.as_slice())
                                .is_some_and(|at| at.kind == Kind::Symlink);
                            if !keep {
                                tree.insert(&entry.path, node);
                            }
                        }
                        // Everything else clears the path first, which takes
                        // the tree underneath it as well.
                        _ => {
                            remove_under(&mut tree, &entry.path, &mut bound, l + 1);
                            tree.insert(&entry.path, node);
                        }
                    }
                }
            }
        }
    }
    tree
}

/// The directories the final tree needs, parents before children.
///
/// A path that ends up a symlink is left out however many entries sit under
/// it: what it points at is where they actually go, and creating a directory
/// there would displace the link.
fn directories(tree: &BTreeMap<&[u8], Node>) -> Vec<(Vec<u8>, u32)> {
    let mut wanted: BTreeMap<&[u8], u32> = BTreeMap::new();
    for (path, node) in tree {
        if node.kind == Kind::Directory && !behind_a_link(tree, path) {
            wanted.insert(path, node.mode);
        }
        // Ancestors nothing names still have to exist. `create_dir` would have
        // made them with the default mode, so they get it here too.
        for ancestor in ancestors(path) {
            if tree.contains_key(ancestor)
                || wanted.contains_key(ancestor)
                || behind_a_link(tree, ancestor)
            {
                continue;
            }
            wanted.insert(ancestor, DEFAULT_DIRECTORY_MODE);
        }
    }
    // A `BTreeMap` orders by path, so a parent always precedes what is under
    // it and the tree can be created in one pass.
    wanted
        .into_iter()
        .map(|(path, mode)| (path.to_vec(), mode))
        .collect()
}

/// Every strict ancestor of `path`, shallowest first.
fn ancestors(path: &[u8]) -> impl Iterator<Item = &[u8]> {    path.iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(move |(at, _)| &path[..at])
        .filter(|ancestor| !ancestor.is_empty())
}

/// True when something that is not a directory stands on the way to `path`.
///
/// Whatever that is resolves somewhere else, and it is not created here, so
/// there would be nothing to create this under. The extractor resolves the
/// entries that live there against the tree as it builds it, as it always has.
fn behind_a_link(tree: &BTreeMap<&[u8], Node>, path: &[u8]) -> bool {
    ancestors(path).any(|ancestor| {
        tree.get(ancestor)
            .is_some_and(|node| node.kind != Kind::Directory)
    })
}

enum Whiteout {
    /// `.wh..wh..opq`: hides everything the lower layers put in this directory.
    Opaque(Vec<u8>),
    /// `.wh.name`: hides the one path it names.
    Named(Vec<u8>),
}

fn basename(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&byte| byte == b'/') {
        Some(at) => &path[at + 1..],
        None => path,
    }
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

/// Drops every entry beneath `dir` that a layer before `below` put there,
/// without touching a sibling whose name merely starts the same way.
///
/// Every entry that is not a directory replays through here, so the bound it
/// seeks from is built in a buffer the caller keeps rather than a fresh pair
/// of allocations per path.
fn remove_under(tree: &mut BTreeMap<&[u8], Node>, dir: &[u8], bound: &mut Vec<u8>, below: usize) {
    bound.clear();
    bound.extend_from_slice(dir);
    bound.push(b'/');

    // Everything under `dir` sorts from the bound onwards and is contiguous,
    // so the first path that is not under it ends the range.
    let mut doomed: Vec<&[u8]> = Vec::new();
    for (path, node) in tree.range::<[u8], _>((
        std::ops::Bound::Included(bound.as_slice()),
        std::ops::Bound::Unbounded,
    )) {
        if !path.starts_with(bound) {
            break;
        }
        if node.owner.0 < below {
            doomed.push(path);
        }
    }
    for path in doomed {
        tree.remove(path);
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
            xattrs: Vec::new(),
        }
    }

    fn of_kind(path: &str, kind: Kind) -> Entry {
        Entry {
            kind,
            mode: if kind == Kind::Directory { 0o755 } else { 0o644 },
            ..file(path)
        }
    }

    fn dir(path: &str) -> Entry {
        of_kind(path, Kind::Directory)
    }

    fn symlink(path: &str, target: &str) -> Entry {
        Entry {
            link: target.as_bytes().to_vec(),
            ..of_kind(path, Kind::Symlink)
        }
    }

    fn layer(entries: Vec<Entry>) -> Table {
        Table { entries }
    }

    /// The paths the plan would create as directories, for readability.
    fn dirs_of(layers: Vec<Table>) -> Vec<String> {
        let (plan, _) = plan_of(layers);
        plan.directories()
            .iter()
            .map(|(path, _)| String::from_utf8_lossy(path).into_owned())
            .collect()
    }

    fn table(paths: &[&str]) -> Table {
        Table {
            entries: paths.iter().map(|p| file(p)).collect(),
        }
    }

    fn plan_of(layers: Vec<Table>) -> (Plan, Vec<Descriptor>) {
        let descriptors: Vec<Descriptor> = (0..layers.len() as u8).map(descriptor).collect();
        (Plan::resolve(&descriptors, layers), descriptors)
    }

    #[test]
    fn a_later_layer_shadows_an_earlier_copy() {
        let (plan, d) = plan_of(vec![table(&["a", "keep"]), table(&["a"])]);
        assert!(plan.is_shadowed(&d[0].digest, b"a"));
        assert!(!plan.is_shadowed(&d[0].digest, b"keep"));
        assert!(!plan.is_shadowed(&d[1].digest, b"a"), "the winner stays");
    }

    /// Shadowing is recorded by path, so a layer naming one path twice would
    /// mark it doomed for the copy that loses and skip the copy that wins.
    #[test]
    fn a_path_a_layer_names_twice_is_not_shadowed_in_that_layer() {
        let (plan, d) = plan_of(vec![layer(vec![file("twice"), file("twice")])]);
        assert!(!plan.is_shadowed(&d[0].digest, b"twice"));
    }

    #[test]
    fn a_whiteout_shadows_what_it_hides() {
        let (plan, d) = plan_of(vec![table(&["dir/a", "dir/b", "other"]), table(&["dir/.wh.a"])]);
        assert!(plan.is_shadowed(&d[0].digest, b"dir/a"));
        assert!(!plan.is_shadowed(&d[0].digest, b"dir/b"));
        assert!(!plan.is_shadowed(&d[0].digest, b"other"));
    }

    /// The marker is itself a regular file entry, and the extractor has to see
    /// it or nothing is removed from the tree the lower layers built.
    #[test]
    fn a_whiteout_marker_is_never_shadowed() {
        let (plan, d) = plan_of(vec![table(&["dir/.wh.a"]), table(&["dir/.wh.a"])]);
        assert!(!plan.is_shadowed(&d[0].digest, b"dir/.wh.a"));
        assert!(!plan.is_shadowed(&d[1].digest, b"dir/.wh.a"));
    }

    /// An opaque whiteout empties a directory; it does not delete it. The
    /// directory sorts at the front of its own subtree, so clearing the
    /// subtree used to take it as well.
    #[test]
    fn an_opaque_whiteout_leaves_the_directory_it_empties() {
        assert_eq!(
            dirs_of(vec![
                layer(vec![dir("dir"), dir("dir/sub")]),
                layer(vec![dir("dir"), file("dir/.wh..wh..opq")]),
            ]),
            ["dir"],
            "the directory stays and only what was under it goes"
        );
    }

    #[test]
    fn an_opaque_whiteout_shadows_the_directory_it_names() {
        let (plan, d) = plan_of(vec![
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
        let (plan, d) = plan_of(vec![table(&["dir/a", "dir", "dirty"]), table(&["\
.wh.dir"])]);
        assert!(plan.is_shadowed(&d[0].digest, b"dir/a"));
        assert!(plan.is_shadowed(&d[0].digest, b"dir"));
        assert!(!plan.is_shadowed(&d[0].digest, b"dirty"));
    }

    /// A later layer restoring a path the whiteout removed keeps it.
    #[test]
    fn a_path_written_after_a_whiteout_survives() {
        let (plan, d) = plan_of(vec![table(&["a"]), table(&[".wh.a"]), table(&["a"])]);
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

        let (plan, d) = plan_of(vec![table(&["target"]), linking, table(&["target"])]);
        assert!(
            !plan.is_shadowed(&d[0].digest, b"target"),
            "the copy the link was made against has to be placed"
        );
    }

    /// The walk refuses a `.wh.` that names nothing, so the plan must not
    /// place it as an ordinary file and reach a different answer.
    #[test]
    fn a_whiteout_naming_nothing_is_not_placeable() {
        let (plan, _) = plan_of(vec![layer(vec![dir("d"), file("d/.wh.")])]);
        assert!(plan.work().is_none());
    }

    /// A sparse body is a map of segments rather than a flat run of the
    /// stream, so only `tar` can place it.
    #[test]
    fn a_sparse_entry_is_not_placeable() {
        let (plan, _) = plan_of(vec![layer(vec![of_kind("holey", Kind::Sparse)])]);
        assert!(plan.work().is_none());
    }

    #[test]
    fn an_absent_table_plans_nothing() {
        let plan = Plan::build(None, &[descriptor(0)]);
        assert!(!plan.is_shadowed(&descriptor(0).digest, b"anything"));
    }

    /// Replacing a directory with anything else takes the tree under it away,
    /// exactly as `remove_any` does during extraction. Without this the plan
    /// would keep entries that no longer have anywhere to be, and once the
    /// directory tree is built up front they would be written through whatever
    /// took the directory's place.
    mod a_directory_replaced_by_something_else {
        use super::*;

        #[test]
        fn a_symlink_takes_the_tree_under_it() {
            let (plan, d) = plan_of(vec![
                layer(vec![dir("lib"), file("lib/a"), file("lib/sub/b"), file("libre")]),
                layer(vec![symlink("lib", "usr/lib")]),
            ]);
            assert!(plan.is_shadowed(&d[0].digest, b"lib/a"));
            assert!(plan.is_shadowed(&d[0].digest, b"lib/sub/b"));
            assert!(
                !plan.is_shadowed(&d[0].digest, b"libre"),
                "a sibling that merely starts the same way is untouched"
            );
        }

        #[test]
        fn a_file_takes_the_tree_under_it() {
            let (plan, d) = plan_of(vec![
                layer(vec![dir("d"), file("d/a")]),
                layer(vec![file("d")]),
            ]);
            assert!(plan.is_shadowed(&d[0].digest, b"d/a"));
        }

        #[test]
        fn a_hard_link_takes_the_tree_under_it() {
            let mut link = of_kind("d", Kind::HardLink);
            link.link = b"elsewhere".to_vec();
            let (plan, d) = plan_of(vec![
                layer(vec![dir("d"), file("d/a"), file("elsewhere")]),
                layer(vec![link]),
            ]);
            assert!(plan.is_shadowed(&d[0].digest, b"d/a"));
        }

        /// The path is gone, so nothing may be created there as a directory.
        #[test]
        fn the_replaced_path_is_not_created_as_a_directory() {
            let dirs = dirs_of(vec![
                layer(vec![dir("lib"), file("lib/a")]),
                layer(vec![symlink("lib", "usr/lib")]),
            ]);
            assert!(
                !dirs.iter().any(|d| d == "lib"),
                "a symlink must not be displaced by a directory: {dirs:?}"
            );
        }

        /// Restoring the directory afterwards puts everything back in play.
        #[test]
        fn a_directory_put_back_afterwards_is_a_directory_again() {
            let dirs = dirs_of(vec![
                layer(vec![dir("d"), file("d/a")]),
                layer(vec![file("d")]),
                layer(vec![dir("d"), file("d/b")]),
            ]);
            assert!(dirs.iter().any(|entry| entry == "d"), "{dirs:?}");

            let (plan, digests) = plan_of(vec![
                layer(vec![dir("d"), file("d/a")]),
                layer(vec![file("d")]),
                layer(vec![dir("d"), file("d/b")]),
            ]);
            assert!(plan.is_shadowed(&digests[0].digest, b"d/a"));
            assert!(!plan.is_shadowed(&digests[2].digest, b"d/b"));
        }

        /// Nothing under the replacement is created either. A directory there
        /// would have to be made through whatever took the path, which is not
        /// created here and so is not there yet to be followed.
        #[test]
        fn nothing_beneath_the_replacement_is_created_either() {
            let dirs = dirs_of(vec![
                layer(vec![symlink("lib", "usr/lib"), dir("usr"), dir("usr/lib")]),
                layer(vec![dir("lib/sub"), file("lib/sub/a")]),
            ]);
            assert!(
                !dirs.iter().any(|entry| entry.starts_with("lib")),
                "nothing may be created behind the link: {dirs:?}"
            );
            assert!(dirs.iter().any(|entry| entry == "usr/lib"), "{dirs:?}");
        }
    }

    /// A directory entry does not take a path back from a symlink that already
    /// resolves to a directory, which is what keeps `/lib -> usr/lib` standing
    /// when a later layer also ships `lib/`.
    #[test]
    fn a_directory_entry_leaves_a_standing_symlink_alone() {
        let dirs = dirs_of(vec![
            layer(vec![symlink("lib", "usr/lib"), dir("usr"), dir("usr/lib")]),
            layer(vec![dir("lib")]),
        ]);
        assert!(!dirs.iter().any(|d| d == "lib"), "{dirs:?}");
        assert!(dirs.iter().any(|d| d == "usr/lib"), "{dirs:?}");
    }

    #[test]
    fn directories_are_listed_parents_first() {
        let dirs = dirs_of(vec![layer(vec![dir("a/b/c"), file("a/b/c/d"), dir("a")])]);
        assert_eq!(dirs, ["a", "a/b", "a/b/c"], "including the ones nothing names");
    }

    #[test]
    fn a_whiteout_takes_the_directory_it_names_out_of_the_tree() {
        let dirs = dirs_of(vec![
            layer(vec![dir("gone"), file("gone/a"), dir("kept")]),
            layer(vec![file(".wh.gone")]),
        ]);
        assert_eq!(dirs, ["kept"]);
    }

    #[test]
    fn whiteout_names_are_recognised() {        assert!(matches!(whiteout(b"dir/.wh..wh..opq"), Some(Whiteout::Opaque(d)) if d == b"dir"));
        assert!(matches!(whiteout(b".wh..wh..opq"), Some(Whiteout::Opaque(d)) if d.is_empty()));
        assert!(matches!(whiteout(b"dir/.wh.name"), Some(Whiteout::Named(n)) if n == b"dir/name"));
        assert!(matches!(whiteout(b".wh.name"), Some(Whiteout::Named(n)) if n == b"name"));
        assert!(whiteout(b"dir/name").is_none());
        assert!(whiteout(b"dir/.wh.").is_none());
    }
}
