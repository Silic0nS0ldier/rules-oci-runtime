//! Where a served file's bytes come from.
//!
//! Every layer is mapped and kept mapped for the length of the run, and the
//! checkpoint index says where inflating can start for any offset in it. A
//! body is therefore one span (or the few a large file spans) rather than a
//! pass over the layer, which is what makes fetching a file on demand cheaper
//! than extracting the image that holds it.

use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::OnceLock;

use crate::error::{Error, IoContext, Result};
use crate::image::{Descriptor, Layout};
use crate::sys::Blob;
use crate::zinfo;

use super::tree::Body;

/// One layer, held open for the length of the run.
struct Layer {
    descriptor: Descriptor,
    blob: Blob,
    index: zinfo::Index,
    /// Whether the blob matched its digest, once something has needed to know.
    verified: OnceLock<bool>,
}

/// The layers an image is served out of.
pub struct Source {
    layers: Vec<Layer>,
}

/// Scratch a thread reuses from body to body: the buffer keeps whatever the
/// widest span before it needed, and the decoders keep their windows.
#[derive(Default)]
pub struct Scratch {
    buffer: Vec<u8>,
    decoders: zinfo::Decoders,
}

impl Source {
    pub fn open(
        layout: &Layout,
        descriptors: &[Descriptor],
        indexes: Vec<zinfo::Index>,
    ) -> Result<Source> {
        let layers = descriptors
            .iter()
            .zip(indexes)
            .map(|(descriptor, index)| {
                Ok(Layer {
                    descriptor: descriptor.clone(),
                    blob: layout.map_blob(descriptor)?,
                    index,
                    verified: OnceLock::new(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Source { layers })
    }

    /// Checks a layer's blob against its digest before the first byte of it
    /// is used, and never again.
    ///
    /// Only the layers something is read from are checked, and only when it
    /// is: a launch whose every file comes out of the content store hashes no
    /// blob at all. Threads needing the same layer wait for the one checking
    /// it, which is a wait on this launch alone.
    fn verify(&self, layer: u32) -> Result<()> {
        let layer = &self.layers[layer as usize];
        let mut failure = None;
        let matched = *layer.verified.get_or_init(|| {
            match crate::image::verify(&layer.descriptor, &layer.blob) {
                Ok(()) => true,
                Err(err) => {
                    failure = Some(err);
                    false
                }
            }
        });
        match (matched, failure) {
            (true, _) => Ok(()),
            (false, Some(err)) => Err(err),
            (false, None) => Err(Error::io(
                format!("reading layer {}", layer.descriptor.digest),
                std::io::Error::other("it does not match its digest"),
            )),
        }
    }

    /// How many layers were checked against their digests, of how many.
    pub fn verified(&self) -> (usize, usize) {
        let checked = self
            .layers
            .iter()
            .filter(|layer| layer.verified.get().is_some())
            .count();
        (checked, self.layers.len())
    }

    /// Which of the layer's spans a body starts in. Two requests for the same
    /// span are worth serialising: the second would inflate what the first is
    /// already inflating.
    pub fn span_of(&self, body: Body) -> usize {
        // `partition_point` counts the checkpoints at or before the body, so
        // the last of them is one back.
        self.layers[body.layer as usize]
            .index
            .checkpoints
            .partition_point(|point| point.out_offset <= body.offset)
            .saturating_sub(1)
    }

    /// The stretch of `layer`'s stream span `span` covers, if it has one.
    pub fn window_of(&self, layer: u32, span: usize) -> Option<std::ops::Range<u64>> {
        let index = &self.layers.get(layer as usize)?.index;
        let start = index.checkpoints.get(span)?.out_offset;
        let end = index
            .checkpoints
            .get(span + 1)
            .map_or(index.uncompressed_len, |next| next.out_offset);
        Some(start..end)
    }

    /// Inflates the span `body` starts in, and however far past it the body
    /// runs, into `scratch`.
    ///
    /// The window is what came back, which is everything the caller can place
    /// without inflating anything again.
    pub fn inflate(&self, body: Body, scratch: &mut Scratch) -> Result<Window> {
        self.verify(body.layer)?;
        let layer = &self.layers[body.layer as usize];
        let checkpoints = &layer.index.checkpoints;
        let start = self.span_of(body);
        let base = checkpoints[start].out_offset;
        let needed = (body.offset + body.size - base) as usize;

        // At least the whole span, so that the files after this one in it are
        // there to be placed, and further where the body runs past its end.
        let mut filled = 0usize;
        let mut at = start;
        while filled < needed || at == start {
            if at >= checkpoints.len() {
                return Err(self.malformed(body.layer, "an entry runs past the end of the layer"));
            }
            filled += layer.index.extract_span_into(
                &layer.blob,
                at,
                &mut scratch.buffer,
                filled,
                &mut scratch.decoders,
            )?;
            at += 1;
        }
        Ok(Window {
            base,
            end: base + filled as u64,
        })
    }

    /// The bytes of `body`, out of a window that covers it.
    pub fn bytes<'a>(&self, body: Body, window: &Window, scratch: &'a Scratch) -> Result<&'a [u8]> {
        let from = (body.offset - window.base) as usize;
        let to = from + body.size as usize;
        // Bounded by what was inflated rather than by the buffer, which keeps
        // the length the widest span before it needed and would otherwise
        // answer with stale bytes.
        scratch.buffer[..(window.end - window.base) as usize]
            .get(from..to)
            .ok_or_else(|| self.malformed(body.layer, "an entry lies outside its own layer"))
    }

    /// Writes bytes out at `path`, with the timestamp the image gave them.
    ///
    /// The mode is the daemon's own: nothing but the daemon opens these files,
    /// and what the container sees is the mode the image asked for, which the
    /// tree holds.
    pub fn place(path: &Path, bytes: &[u8], mtime: u64) -> Result<()> {
        let context = || format!("materialising {}", path.display());
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .io_context(context)?;
        file.write_all(bytes).io_context(context)?;
        set_mtime(&file, mtime);
        Ok(())
    }

    /// The digest of layer `layer`, for saying which one something came from.
    pub fn digest(&self, layer: u32) -> &str {
        &self.layers[layer as usize].descriptor.digest
    }

    fn malformed(&self, layer: u32, what: &str) -> Error {
        Error::io(
            format!(
                "serving layer {}",
                self.layers[layer as usize].descriptor.digest
            ),
            std::io::Error::other(what),
        )
    }
}

/// The stretch of a layer's uncompressed stream that is in hand.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub base: u64,
    pub end: u64,
}

/// Timestamps are cosmetic, so a failure is not worth failing a read over.
fn set_mtime(file: &fs::File, mtime: u64) {
    let time = libc::timespec {
        tv_sec: mtime as _,
        tv_nsec: 0,
    };
    let times = [time, time];
    // SAFETY: the descriptor is open and `times` holds the two values
    // futimens reads.
    let _ = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
}
