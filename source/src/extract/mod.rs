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
#[cfg(test)]
mod tests;

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
}

impl RootfsExtractor {
    pub fn new(rootfs: &Utf8Path, index_dir: Option<&Utf8Path>) -> Result<Self> {
        fs::create_dir_all(rootfs).io_context(|| format!("creating {rootfs}"))?;
        Ok(RootfsExtractor {
            parents: fsutil::ParentCache::new(rootfs.as_std_path())?,
            rootfs: rootfs.to_owned(),
            index_dir: index_dir.map(Utf8Path::to_owned),
            deferred_modes: Vec::new(),
            plan: plan::Plan::default(),
        })
    }

    /// Resolves the image before extracting it, so that entries a later layer
    /// replaces are never written. Without an entry table for every layer this
    /// plans nothing and each layer is placed in full, as before.
    pub fn plan(&mut self, layers: &[Descriptor]) {
        self.plan = plan::Plan::build(self.index_dir.as_deref(), layers);
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

    /// The checkpoint index for this layer, when there is one and parallel
    /// decompression can put it to use. An index is an optimisation, so
    /// anything wrong with it means falling back, not failing: the streaming
    /// path decides what the blob actually contains.
    fn layer_index(&self, descriptor: &Descriptor) -> Option<zinfo::Index> {
        let dir = self.index_dir.as_ref()?;
        if compression_of(&descriptor.media_type) != Some(Compression::Gzip) {
            return None;
        }
        if thread::available_parallelism().map_or(1, |n| n.get()) < 2 {
            return None;
        }
        let hex = parse_digest(&descriptor.digest).ok()?.hex;
        let path = dir.join(format!("{hex}.zinfo"));
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
            Err(err) => {
                warning!("ignoring layer index {path}: {err}");
                return None;
            }
        };
        match zinfo::Index::read_from(io::BufReader::new(file)) {
            Ok(index) if index.checkpoints.len() > 1 => Some(index),
            Ok(_) => None,
            Err(err) => {
                warning!("ignoring layer index {path}: {err}");
                None
            }
        }
    }

    /// Applies the recorded directory permissions, deepest first.
    pub fn finish(mut self) -> Result<()> {
        self.deferred_modes
            .sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        for (path, mode) in &self.deferred_modes {
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
