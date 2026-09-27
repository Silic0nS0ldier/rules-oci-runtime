//! The resolved image for one platform manifest, recorded at build time.
//!
//! Planning an image from its layers' entry tables gives the same answer on
//! every launch, so `oci_runtime stitch` does it once and the launcher reads
//! the result instead. Only what survives is kept: shadowed entries,
//! whiteouts and opaque markers are gone, and each layer holds just the
//! entries something is placed from.
//!
//! Like every sidecar this is an optimisation. A missing or unreadable table
//! means planning at run time. A table naming another manifest or other
//! layers means the inputs are wrong, and fails the run.

use std::io::{self, Read, Write};

use camino::Utf8Path;

use crate::entries::{self, Kind, Table as Entries};
use crate::error::{Error, Result};
use crate::extract::{Plan, Work, Xattrs};
use crate::fsutil;
use crate::image::Descriptor;

const MAGIC: &[u8; 4] = b"OTR1";

/// The root of the rootfs, the one path that is not a canonical entry path.
const ROOT_ENTRY: &[u8] = b".";

#[derive(Debug, PartialEq, Eq)]
pub struct Table {
    pub manifest: String,
    pub platform: String,
    /// One per manifest layer, in order, holding only the entries placed from
    /// it. A layer that places nothing is still listed, and is empty.
    pub layers: Vec<Entries>,
    pub directories: Vec<(Vec<u8>, u32)>,
    /// Indices into `layers`.
    pub work: Work,
    pub xattrs: Vec<Xattrs>,
}

impl Table {
    /// Resolves the image the tables describe, or `None` when it is not one
    /// that can be placed straight from a plan and so has to be planned (and
    /// walked) at run time.
    pub fn stitch(
        manifest: &str,
        platform: &str,
        descriptors: &[Descriptor],
        tables: Vec<Entries>,
    ) -> Option<Table> {
        let plan = Plan::resolve(descriptors, tables);
        let work = plan.work()?;

        let mut layers = Vec::with_capacity(descriptors.len());
        let mut renumbered = Vec::with_capacity(descriptors.len());
        for (l, table) in plan.tables().iter().enumerate() {
            let mut used = vec![false; table.entries.len()];
            let placed = work.files[l].iter().copied().chain(
                work.symlinks
                    .iter()
                    .chain(&work.hard_links)
                    .filter(|(layer, _)| *layer as usize == l)
                    .map(|(_, entry)| *entry),
            );
            for entry in placed {
                used[entry as usize] = true;
            }
            let mut index = vec![u32::MAX; table.entries.len()];
            let mut kept = Vec::new();
            for (e, entry) in table.entries.iter().enumerate() {
                if used[e] {
                    index[e] = kept.len() as u32;
                    kept.push(entry.clone());
                }
            }
            layers.push(Entries {
                layer: descriptors[l].digest.clone(),
                entries: kept,
            });
            renumbered.push(index);
        }

        // Renumbering keeps the order within a layer, so the files stay in
        // stream order and the links in the order the layers named them.
        let pair =
            |&(layer, entry): &(u32, u32)| (layer, renumbered[layer as usize][entry as usize]);
        let work = Work {
            files: work
                .files
                .iter()
                .enumerate()
                .map(|(l, files)| files.iter().map(|&e| renumbered[l][e as usize]).collect())
                .collect(),
            symlinks: work.symlinks.iter().map(pair).collect(),
            hard_links: work.hard_links.iter().map(pair).collect(),
        };
        Some(Table {
            manifest: manifest.to_string(),
            platform: platform.to_string(),
            layers,
            directories: plan.directories().to_vec(),
            work,
            xattrs: plan.xattrs().to_vec(),
        })
    }

    /// The plan this table records, provided it was stitched for this very
    /// manifest and these layers.
    pub fn into_plan(self, path: &Utf8Path, manifest: &str, layers: &[Descriptor]) -> Result<Plan> {
        let mismatch = |expected: &str, actual: &str| Error::MismatchedSidecar {
            path: path.to_string(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        };
        if self.manifest != manifest {
            return Err(mismatch(manifest, &self.manifest));
        }
        if self.layers.len() != layers.len() {
            return Err(mismatch(
                &format!("{} layers", layers.len()),
                &format!("{} layers", self.layers.len()),
            ));
        }
        for (recorded, descriptor) in self.layers.iter().zip(layers) {
            if recorded.layer != descriptor.digest {
                return Err(mismatch(&descriptor.digest, &recorded.layer));
            }
        }
        Ok(Plan::resolved(
            self.directories,
            self.layers,
            self.work,
            self.xattrs,
        ))
    }

    pub fn write_to(&self, writer: impl Write) -> io::Result<()> {
        let mut body = Vec::new();
        entries::write_bytes(&mut body, self.manifest.as_bytes())?;
        entries::write_bytes(&mut body, self.platform.as_bytes())?;

        put_u32(&mut body, self.layers.len());
        for layer in &self.layers {
            entries::write_bytes(&mut body, layer.layer.as_bytes())?;
            put_u32(&mut body, layer.entries.len());
            for entry in &layer.entries {
                entries::write_entry(&mut body, entry)?;
            }
        }

        put_u32(&mut body, self.directories.len());
        for (path, mode) in &self.directories {
            entries::write_bytes(&mut body, path)?;
            body.extend_from_slice(&mode.to_le_bytes());
        }

        for files in &self.work.files {
            put_u32(&mut body, files.len());
            for entry in files {
                body.extend_from_slice(&entry.to_le_bytes());
            }
        }
        for pairs in [&self.work.symlinks, &self.work.hard_links] {
            put_u32(&mut body, pairs.len());
            for (layer, entry) in pairs {
                body.extend_from_slice(&layer.to_le_bytes());
                body.extend_from_slice(&entry.to_le_bytes());
            }
        }

        put_u32(&mut body, self.xattrs.len());
        for (layer, path, names) in &self.xattrs {
            body.extend_from_slice(&layer.to_le_bytes());
            entries::write_bytes(&mut body, path)?;
            entries::write_bytes(&mut body, names)?;
        }
        entries::write_framed(MAGIC, &body, writer)
    }

    pub fn read_from(reader: impl Read) -> io::Result<Self> {
        let body = entries::read_framed(MAGIC, "a rootfs table", reader)?;
        let at = &mut 0;
        let text = |bytes: Vec<u8>| {
            String::from_utf8(bytes).map_err(|_| io::Error::other("a digest is not text"))
        };
        let manifest = text(entries::take_bytes(&body, at)?)?;
        let platform = text(entries::take_bytes(&body, at)?)?;

        let mut layers = Vec::new();
        for _ in 0..take_count(&body, at)? {
            let layer = text(entries::take_bytes(&body, at)?)?;
            let count = take_count(&body, at)?;
            let mut kept = Vec::with_capacity(count);
            for _ in 0..count {
                kept.push(entries::take_entry(&body, at)?);
            }
            layers.push(Entries {
                layer,
                entries: kept,
            });
        }

        let mut directories = Vec::new();
        for _ in 0..take_count(&body, at)? {
            let path = entries::take_bytes(&body, at)?;
            directories.push((path, entries::take_u32(&body, at)?));
        }

        let mut work = Work::default();
        for _ in 0..layers.len() {
            let count = take_count(&body, at)?;
            let mut files = Vec::with_capacity(count);
            for _ in 0..count {
                files.push(entries::take_u32(&body, at)?);
            }
            work.files.push(files);
        }
        for pairs in [&mut work.symlinks, &mut work.hard_links] {
            for _ in 0..take_count(&body, at)? {
                pairs.push((entries::take_u32(&body, at)?, entries::take_u32(&body, at)?));
            }
        }

        let mut xattrs = Vec::new();
        for _ in 0..take_count(&body, at)? {
            let layer = entries::take_u32(&body, at)?;
            let path = entries::take_bytes(&body, at)?;
            xattrs.push((layer, path, entries::take_bytes(&body, at)?));
        }
        if *at != body.len() {
            return Err(io::Error::other("trailing bytes in rootfs table"));
        }

        let table = Table {
            manifest,
            platform,
            layers,
            directories,
            work,
            xattrs,
        };
        table.check()?;
        Ok(table)
    }

    /// Everything that places from this table indexes it without asking, and
    /// joins its paths under the rootfs, so both are held to what a stitch
    /// could have written.
    fn check(&self) -> io::Result<()> {
        let malformed = |what: &str| Err(io::Error::other(format!("rootfs table {what}")));
        let entry = |layer: u32, entry: u32, kind: Kind| {
            self.layers
                .get(layer as usize)
                .and_then(|table| table.entries.get(entry as usize))
                .filter(|found| found.kind == kind)
        };

        for (l, files) in self.work.files.iter().enumerate() {
            let mut last = 0;
            for &e in files {
                let Some(file) = entry(l as u32, e, Kind::File) else {
                    return malformed("names a file it does not hold");
                };
                if file.offset < last {
                    return malformed("lists a layer's files out of order");
                }
                last = file.offset;
            }
        }
        for &(l, e) in &self.work.symlinks {
            if entry(l, e, Kind::Symlink).is_none() {
                return malformed("names a symlink it does not hold");
            }
        }
        for &(l, e) in &self.work.hard_links {
            match entry(l, e, Kind::HardLink) {
                Some(link) if canonical(&link.link) => {}
                _ => return malformed("names a hard link it does not hold"),
            }
        }
        if self
            .xattrs
            .iter()
            .any(|(l, ..)| *l as usize >= self.layers.len())
        {
            return malformed("names a layer it does not hold");
        }
        let paths = self
            .layers
            .iter()
            .flat_map(|table| table.entries.iter().map(|entry| entry.path.as_slice()));
        let directories = self
            .directories
            .iter()
            .map(|(path, _)| path.as_slice())
            .filter(|path| *path != ROOT_ENTRY);
        if !paths.chain(directories).all(canonical) {
            return malformed("holds a path that is not under the rootfs");
        }
        Ok(())
    }
}

fn canonical(path: &[u8]) -> bool {
    let mut out = Vec::new();
    fsutil::canonical_entry_path(path, &mut out) && out == path
}

fn put_u32(body: &mut Vec<u8>, count: usize) {
    body.extend_from_slice(&(count as u32).to_le_bytes());
}

/// A count, refused before anything is sized from it when the body could not
/// hold that many of anything.
fn take_count(body: &[u8], at: &mut usize) -> io::Result<usize> {
    let count = entries::take_u32(body, at)? as usize;
    if count > body.len() - *at {
        return Err(io::Error::other("implausible count in rootfs table"));
    }
    Ok(count)
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

    fn entry(kind: Kind, path: &str, link: &str, offset: u64) -> Entry {
        Entry {
            kind,
            mode: 0o644,
            mtime: 1,
            offset,
            size: 3,
            path: path.as_bytes().to_vec(),
            link: link.as_bytes().to_vec(),
            xattrs: Vec::new(),
            sha256: kind.is_file().then_some([7; 32]),
        }
    }

    fn layers() -> (Vec<Descriptor>, Vec<Entries>) {
        let mut noted = entry(Kind::File, "./etc/shadowed", "", 0);
        noted.xattrs = b"user.note".to_vec();
        let lower = Entries {
            layer: descriptor(0).digest,
            entries: vec![
                entry(Kind::Directory, "etc", "", 0),
                noted,
                entry(Kind::File, "etc/kept", "", 512),
                entry(Kind::File, "etc/gone", "", 1024),
            ],
        };
        let upper = Entries {
            layer: descriptor(1).digest,
            entries: vec![
                entry(Kind::File, "etc/.wh.gone", "", 0),
                entry(Kind::File, "etc/shadowed", "", 512),
                entry(Kind::Symlink, "etc/link", "kept", 0),
                entry(Kind::HardLink, "etc/same", "etc/kept", 0),
            ],
        };
        (vec![descriptor(0), descriptor(1)], vec![lower, upper])
    }

    fn stitched() -> Table {
        let (descriptors, tables) = layers();
        Table::stitch("sha256:m", "linux/amd64", &descriptors, tables).expect("stitched")
    }

    fn paths(table: &Entries) -> Vec<&str> {
        table
            .entries
            .iter()
            .map(|entry| std::str::from_utf8(&entry.path).expect("utf-8"))
            .collect()
    }

    #[test]
    fn only_what_survives_is_kept() {
        let table = stitched();
        assert_eq!(paths(&table.layers[0]), ["etc/kept"]);
        assert_eq!(
            paths(&table.layers[1]),
            ["etc/shadowed", "etc/link", "etc/same"],
            "the whiteout and what it hid are gone"
        );
        assert_eq!(table.work.files, [vec![0], vec![0]]);
        assert_eq!(table.work.symlinks, [(1, 1)]);
        assert_eq!(table.work.hard_links, [(1, 2)]);
        assert_eq!(table.directories, [(b"etc".to_vec(), 0o644)]);
    }

    /// Strict mode refuses an image for an attribute on any entry, shadowed
    /// or not, so the table keeps them all.
    #[test]
    fn extended_attributes_of_shadowed_entries_are_kept() {
        assert_eq!(
            stitched().xattrs,
            [(0, b"etc/shadowed".to_vec(), b"user.note".to_vec())]
        );
    }

    #[test]
    fn a_table_survives_serialisation() {
        let table = stitched();
        let mut bytes = Vec::new();
        table.write_to(&mut bytes).expect("write");
        assert_eq!(Table::read_from(&bytes[..]).expect("read"), table);
    }

    #[test]
    fn a_table_is_only_used_for_its_own_manifest_and_layers() {
        let (descriptors, _) = layers();
        let path = Utf8Path::new("m.rootfs");
        assert!(matches!(
            stitched().into_plan(path, "sha256:other", &descriptors),
            Err(Error::MismatchedSidecar { .. })
        ));
        assert!(matches!(
            stitched().into_plan(path, "sha256:m", &[descriptor(0), descriptor(2)]),
            Err(Error::MismatchedSidecar { .. })
        ));
        let plan = stitched()
            .into_plan(path, "sha256:m", &descriptors)
            .expect("plan");
        assert!(plan.work().is_some());
    }

    #[test]
    fn an_image_only_a_walk_can_place_is_not_stitched() {
        let (descriptors, mut tables) = layers();
        tables[1].entries[1].kind = Kind::Sparse;
        assert!(Table::stitch("sha256:m", "", &descriptors, tables).is_none());
    }

    #[test]
    fn a_table_that_would_place_outside_the_rootfs_is_refused() {
        let mut table = stitched();
        table.layers[0].entries[0].path = b"../escaped".to_vec();
        let mut bytes = Vec::new();
        table.write_to(&mut bytes).expect("write");
        assert!(Table::read_from(&bytes[..]).is_err());

        let mut table = stitched();
        table.work.files[0] = vec![5];
        let mut bytes = Vec::new();
        table.write_to(&mut bytes).expect("write");
        assert!(Table::read_from(&bytes[..]).is_err());
    }
}
