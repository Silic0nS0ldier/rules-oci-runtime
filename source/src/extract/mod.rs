//! Unpacking image layers into a root filesystem, replacing the previous
//! `undocker | tar -x` pipeline.
//!
//! A layer arrives as a compressed blob and leaves as files on disk.
//! [`pipeline`] turns the blob into bytes, [`entry`] walks the tar stream those
//! bytes carry, and [`file`] places whatever each entry names.

mod entry;
mod file;
mod pipeline;
mod plan;
mod spans;
#[cfg(test)]
mod tests;
mod whiteout;

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::mpsc::sync_channel;
use std::thread;

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{Error, IoContext, Result};
use crate::fsutil;
use crate::image::{Descriptor, Layout, parse_digest};
use crate::log::{log, warning};
use crate::zinfo;

use pipeline::{ChunkReader, PIPELINE_DEPTH, buffer_pool, inflate_blob, inflate_indexed};

pub use pipeline::{Compression, compression_of};

/// Applies layers in order, deferring directory permissions so that read-only
/// directories in one layer do not block writes from the next.
pub struct RootfsExtractor {
    rootfs: Utf8PathBuf,
    index_dir: Option<Utf8PathBuf>,
    deferred_modes: Vec<(PathBuf, u32)>,
    parents: fsutil::ParentCache,
    plan: plan::Plan,
    /// What the layer currently being walked has placed. A whiteout hides the
    /// layers below it, so it has to be able to tell them apart. Ordered, so
    /// that a directory can be asked whether anything of this layer's is under
    /// it, which is what keeps one it made to hold an entry.
    written: BTreeSet<PathBuf>,
    /// Refuse an image that asks for extended attributes, rather than
    /// extracting one the container will not match.
    strict_xattrs: bool,
}

impl RootfsExtractor {
    pub fn new(
        rootfs: &Utf8Path,
        index_dir: Option<&Utf8Path>,
        strict_xattrs: bool,
    ) -> Result<Self> {
        fs::create_dir_all(rootfs).io_context(|| format!("creating {rootfs}"))?;
        Ok(RootfsExtractor {
            parents: fsutil::ParentCache::new(rootfs.as_std_path())?,
            rootfs: rootfs.to_owned(),
            index_dir: index_dir.map(Utf8Path::to_owned),
            deferred_modes: Vec::new(),
            plan: plan::Plan::default(),
            written: BTreeSet::new(),
            strict_xattrs,
        })
    }

    /// Nothing restores extended attributes. The one that matters,
    /// `security.capability`, needs a privilege the extractor does not have
    /// when it runs rootless, which is the usual case.
    ///
    /// Reported once per entry, from whichever source saw it: the tables when
    /// the image resolved, the tar headers when it did not.
    pub(super) fn report_xattrs(&self, layer: &str, path: &[u8], names: &[u8]) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let attributes = String::from_utf8_lossy(names).replace('\0', ", ");
        let path = String::from_utf8_lossy(path).into_owned();
        if self.strict_xattrs {
            return Err(Error::UnsupportedXattrs {
                layer: layer.to_string(),
                path,
                attributes,
            });
        }
        log!("Not restoring extended attributes on /{path}: {attributes}");
        Ok(())
    }

    /// Resolves the image before extracting it, so that entries a later layer
    /// replaces are never written. Without an entry table for every layer this
    /// plans nothing and each layer is placed in full, as before.
    pub fn plan(&mut self, layers: &[Descriptor]) -> Result<()> {
        self.plan = plan::Plan::build(self.index_dir.as_deref(), layers);
        if !self.plan.is_resolved() {
            return Ok(());
        }

        // The tables describe every layer, so this is the whole image in one
        // pass; the walk reports the same thing when there are no tables.
        for (descriptor, table) in layers.iter().zip(self.plan.tables()) {
            for entry in &table.entries {
                self.report_xattrs(&descriptor.digest, &entry.path, &entry.xattrs)?;
            }
        }
        Ok(())
    }

    /// Creates the directories the image ends up with, so that nothing placing
    /// an entry has to work out where it can go.
    ///
    /// Only for the route that places entries straight from the plan: a walk
    /// builds the tree as it goes, and a tree standing before the first layer
    /// runs is a tree the layers can see. A symlink resolving to a directory
    /// that will not exist until a later layer is kept rather than replaced,
    /// which is a different image.
    fn create_planned_directories(&mut self) -> Result<()> {
        let root = self.rootfs.clone();
        let mut path = PathBuf::new();
        for (relative, mode) in self.plan.directories() {
            fsutil::join_under(root.as_std_path(), relative, &mut path);
            file::create_directory(&path)?;
            self.deferred_modes.push((path.clone(), *mode));
        }
        log!("Created {} directories", self.plan.directories().len());
        Ok(())
    }

    /// Places every layer of the image.
    ///
    /// A resolved image whose entries are all placeable goes straight from the
    /// plan: the spans of every layer become one queue of work, and each is
    /// inflated and written by the same thread. Anything else is walked a
    /// layer at a time, which is what has to happen when the plan cannot say
    /// where an entry ends up without building the tree to find out.
    pub fn apply(&mut self, layout: &Layout, descriptors: &[Descriptor]) -> Result<()> {
        if self.plan.work().is_some() {
            // Every layer needs a checkpoint index, since a span is where a
            // worker starts inflating. One checkpoint is enough: it means the
            // layer is one span.
            let indexes: Option<Vec<zinfo::Index>> = self
                .index_dir
                .as_deref()
                .map(|dir| descriptors.iter().map(|d| index_at(dir, d)).collect())
                .unwrap_or_default();
            if let Some(indexes) = indexes {
                self.create_planned_directories()?;
                let work = self.plan.work().expect("the work this route needs");
                return spans::extract(&self.rootfs, layout, descriptors, &self.plan, work, indexes);
            }
        }
        for descriptor in descriptors {
            self.apply_layer(layout, descriptor)?;
        }
        Ok(())
    }

    /// Decompression is CPU bound and writing the rootfs is IO bound, so a
    /// second thread inflates the blob while this one writes the entries. The
    /// two overlap rather than run back to back, which on a large layer is
    /// worth roughly the whole decompression time.
    pub fn apply_layer(&mut self, layout: &Layout, descriptor: &Descriptor) -> Result<()> {
        let index = self.layer_index(descriptor);
        match &index {
            Some(index) => log!(
                "Extracting layer {} ({}) using {} checkpoints",
                descriptor.digest,
                descriptor.media_type,
                index.checkpoints.len()
            ),
            None => log!("Extracting layer {} ({})", descriptor.digest, descriptor.media_type),
        }

        let file = layout.open_blob(descriptor)?;
        let digest = descriptor.digest.clone();
        let (sender, receiver) = sync_channel(PIPELINE_DEPTH);
        let (pool, ret) = buffer_pool();
        // Indexed spans are sized by the index rather than by CHUNK_BYTES, so
        // there is nothing for the streaming pool to hand them back to.
        let ret = index.is_none().then_some(ret);
        let owned = descriptor.clone();
        let inflate = thread::spawn(move || match index {
            Some(index) => inflate_indexed(file, &index, &owned, sender),
            None => inflate_blob(file, &owned, sender, pool),
        });

        let mut reader = ChunkReader::new(receiver, ret);
        let mut unpacked = self.unpack(&mut reader, &digest);
        if unpacked.is_ok() {
            // The tar stream ends at its marker, but the digest covers the blob.
            unpacked = io::copy(&mut reader, &mut io::sink())
                .map(|_| ())
                .io_context(|| format!("reading layer {digest}"));
        }
        // Releasing the receiver lets the inflater stop early when unpacking failed.
        drop(reader);

        match inflate.join() {
            // An unverified blob explains any unpacking failure, so it wins.
            Ok(inflated) => inflated.and(unpacked),
            Err(_) => Err(Error::io(
                "decompressing a layer",
                io::Error::other("the decompression thread panicked"),
            )),
        }
    }

    /// The checkpoint index for this layer, when there is one and the
    /// streaming pipeline can put it to use. One checkpoint is the whole blob,
    /// which buys that path nothing.
    fn layer_index(&self, descriptor: &Descriptor) -> Option<zinfo::Index> {
        index_at(self.index_dir.as_deref()?, descriptor)
            .filter(|index| index.checkpoints.len() > 1)
    }

    /// Applies the recorded directory permissions, deepest first.
    pub fn finish(mut self) -> Result<()> {
        self.deferred_modes
            .sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        for (path, mode) in &self.deferred_modes {
            // A later layer may have put something else at the path, and this
            // is a directory's mode: applying it to whatever took its place
            // would give that the directory's permissions instead of its own.
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_dir() => {}
                _ => continue,
            }
            // Keep traversal rights: without them cleanup and later runs would fail.
            let mode = mode | 0o700;
            if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(mode))
                && err.kind() != io::ErrorKind::NotFound
            {
                warning!("could not set mode on {}: {err}", path.display());
            }
        }
        Ok(())
    }
}

/// Reads the checkpoint index recorded for a layer, if there is a usable one.
fn index_at(dir: &Utf8Path, descriptor: &Descriptor) -> Option<zinfo::Index> {
    if compression_of(&descriptor.media_type) != Some(Compression::Gzip) {
        return None;
    }
    if thread::available_parallelism().map_or(1, |n| n.get()) < 2 {
        return None;
    }
    let hex = parse_digest(&descriptor.digest).ok()?.hex;
    let path = crate::sidecar::checkpoints_at(dir, &hex);
    crate::sidecar::read(&path, zinfo::Index::read_from)
}
