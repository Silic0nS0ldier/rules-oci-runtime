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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use crate::error::{Error, IoContext, Result};
use crate::image::{Descriptor, Layout};
use crate::sys::Blob;
use crate::zinfo;

use super::tree::Body;

/// Checking digests is memory bound like everything else here, so the same cap
/// the span route settled on applies.
const MAX_WORKERS: usize = 8;

/// One layer, held open for the length of the run.
struct Layer {
    digest: String,
    blob: Blob,
    index: zinfo::Index,
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
                    digest: descriptor.digest.clone(),
                    blob: layout.map_blob(descriptor)?,
                    index,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Source { layers })
    }

    /// Checks every blob against the digest its descriptor names.
    ///
    /// Extraction reads each blob whole and so gets this for the cost of a
    /// pass over memory. Serving reads only what is asked for, so this is the
    /// one thing it has to do eagerly: a blob checked after the container has
    /// already read from it has not been checked at all. The blobs are
    /// independent, so the check runs on as many threads as there are layers.
    pub fn verify(&self, descriptors: &[Descriptor]) -> Result<()> {
        let next = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let failure: std::sync::Mutex<Option<(usize, Error)>> = std::sync::Mutex::new(None);
        let workers = thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(descriptors.len().max(1))
            .min(MAX_WORKERS);

        thread::scope(|scope| {
            for _ in 0..workers {
                let (next, stop, failure) = (&next, &stop, &failure);
                scope.spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(descriptor) = descriptors.get(i) else {
                            break;
                        };
                        if let Err(err) = crate::image::verify(descriptor, &self.layers[i].blob) {
                            let mut slot = failure.lock().expect("a worker failure");
                            if slot.as_ref().is_none_or(|(first, _)| i < *first) {
                                *slot = Some((i, err));
                            }
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                });
            }
        });

        match failure.into_inner().expect("a worker failure") {
            Some((_, err)) => Err(err),
            None => Ok(()),
        }
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

    /// Inflates the span `body` starts in, and however far past it the body
    /// runs, into `scratch`.
    ///
    /// The window is what came back, which is everything the caller can place
    /// without inflating anything again.
    pub fn inflate(&self, body: Body, scratch: &mut Scratch) -> Result<Window> {
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

    fn malformed(&self, layer: u32, what: &str) -> Error {
        Error::io(
            format!("serving layer {}", self.layers[layer as usize].digest),
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
        tv_sec: mtime as libc::time_t,
        tv_nsec: 0,
    };
    let times = [time, time];
    // SAFETY: the descriptor is open and `times` holds the two values
    // futimens reads.
    let _ = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
}
