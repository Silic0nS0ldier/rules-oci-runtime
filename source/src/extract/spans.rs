//! Extracting a resolved image span by span.
//!
//! Once the whole image has been resolved there is no reason to walk it a
//! layer at a time. Every surviving file is known, along with the layer it
//! comes from and where its body sits in that layer's uncompressed stream, and
//! the checkpoint index says where inflating can start. So the image is cut
//! into spans and handed to a pool.
//!
//! A worker inflates its own span and writes the files that begin inside it,
//! straight out of the buffer it inflated into. Nothing is handed between
//! threads, so nothing is copied, and bytes are written while they are still
//! in the cache of the core that produced them. That is the difference between
//! this and an earlier attempt that had one thread parse and others write:
//! buffering a body to hand it over roughly doubled the memory traffic per
//! byte, and no amount of parallelism paid that back.
//!
//! Work is claimed from one queue covering every layer, so a worker held up by
//! a large file does not stall the rest and later layers start as soon as
//! there is capacity for them.

use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use camino::Utf8Path;
use sha2::{Digest, Sha256};

use crate::entries::{Entry, Table};
use crate::error::{Error, IoContext, Result};
use crate::image::{Descriptor, Layout, hex_encode, parse_digest};
use crate::log::log;
use crate::sys::Mapping;
use crate::zinfo;

use super::file::{finish_file, set_symlink_mtime};
use super::plan::{Plan, Work};

/// Everything one layer contributes, held open for the length of the run.
struct Layer {
    descriptor: Descriptor,
    blob: Mapping,
    index: zinfo::Index,
}

/// A claim on the queue.
enum Unit {
    /// Inflate from a checkpoint and write the files beginning inside it.
    Span {
        layer: usize,
        checkpoint: usize,
        /// Indices into the layer's entry table, ordered by body offset.
        entries: std::ops::Range<usize>,
    },
    /// Check a blob against the digest its descriptor names. The bytes are
    /// mapped once and read by both this and the spans, so it costs a pass
    /// over memory rather than a second read of the file.
    Digest { layer: usize },
}

impl Unit {
    /// Roughly how long the unit will take, so the longest go first and the
    /// tail of the run is short.
    fn weight(&self, layers: &[Layer], work: &Work, plan: &Plan) -> u64 {
        match self {
            Unit::Digest { layer } => layers[*layer].blob.len() as u64,
            Unit::Span {
                layer,
                checkpoint,
                entries,
            } => {
                let index = &layers[*layer].index;
                let table = plan.table(*layer);
                let last = work.files[*layer][entries.end - 1] as usize;
                let entry = &table.entries[last];
                entry.offset + entry.size - index.checkpoints[*checkpoint].out_offset
            }
        }
    }
}

/// Places every surviving entry of a resolved image.
///
/// The directories are already there, so a worker never has to work out where
/// an entry can go: it opens the path exclusively, which neither follows a
/// symlink nor overwrites anything. The plan says each path is placed once, so
/// a path that is already occupied means the plan and the tree disagree, and
/// that is an error rather than something to write through.
pub fn extract(
    rootfs: &Utf8Path,
    layout: &Layout,
    descriptors: &[Descriptor],
    plan: &Plan,
    work: &Work,
    indexes: Vec<zinfo::Index>,
) -> Result<()> {
    let layers = open_layers(layout, descriptors, indexes)?;
    let root = rootfs.as_std_path();

    // Symlinks first: nothing resolves through one here, or the plan would
    // have refused the image, but a link under another link needs it standing.
    for &(layer, entry) in &work.symlinks {
        let entry = &plan.table(layer as usize).entries[entry as usize];
        place_symlink(root, entry)?;
    }

    let units = plan_units(&layers, work, plan);
    log!(
        "Extracting {} layers from {} checkpoints as {} units on {} workers",
        layers.len(),
        layers
            .iter()
            .map(|layer| layer.index.checkpoints.len())
            .sum::<usize>(),
        units.len(),
        workers(units.len())
    );
    run(&units, &layers, work, plan, root)?;

    // Hard links last: the files they name are on disk by now.
    for &(layer, entry) in &work.hard_links {
        let entry = &plan.table(layer as usize).entries[entry as usize];
        place_hard_link(root, entry)?;
    }
    Ok(())
}

fn open_layers(
    layout: &Layout,
    descriptors: &[Descriptor],
    indexes: Vec<zinfo::Index>,
) -> Result<Vec<Layer>> {
    descriptors
        .iter()
        .zip(indexes)
        .map(|(descriptor, index)| {
            let file = layout.open_blob(descriptor)?;
            let len = file.metadata().map_or(0, |m| m.len()) as usize;
            let blob = Mapping::of(&file, len).ok_or_else(|| {
                Error::io(
                    format!("mapping layer {}", descriptor.digest),
                    std::io::Error::other("the blob could not be mapped"),
                )
            })?;
            Ok(Layer {
                descriptor: descriptor.clone(),
                blob,
                index,
            })
        })
        .collect()
}

/// Cuts the image into units: the files beginning within one checkpoint's
/// span, plus one digest check per layer.
///
/// A body can run past the end of the span it starts in, and whoever writes it
/// inflates far enough to reach the end. The next unit picks up from the last
/// checkpoint that one had to read, so a run of straddling files is not read
/// twice over: cutting at every checkpoint regardless cost a third again as
/// much processor time.
///
/// The window a unit takes from is fixed by the checkpoint it starts at, not
/// by what it has absorbed. Letting that grow chains a whole layer into one
/// unit, which is correct and useless: it leaves the other cores nothing.
fn plan_units(layers: &[Layer], work: &Work, plan: &Plan) -> Vec<Unit> {
    let mut units = Vec::new();
    for (l, layer) in layers.iter().enumerate() {
        units.push(Unit::Digest { layer: l });

        let files = &work.files[l];
        let table = plan.table(l);
        let checkpoints = &layer.index.checkpoints;
        let span_end = |c: usize| {
            checkpoints
                .get(c + 1)
                .map_or(layer.index.uncompressed_len, |next| next.out_offset)
        };
        let body = |i: usize| {
            let entry = &table.entries[files[i] as usize];
            (entry.offset, entry.offset + entry.size)
        };

        let mut at = 0;
        let mut checkpoint = 0;
        while at < files.len() {
            while checkpoint + 1 < checkpoints.len()
                && checkpoints[checkpoint + 1].out_offset <= body(at).0
            {
                checkpoint += 1;
            }

            let first = at;
            let limit = span_end(checkpoint);
            let mut end = body(at).1;
            while at < files.len() && body(at).0 < limit {
                end = body(at).1;
                at += 1;
            }
            units.push(Unit::Span {
                layer: l,
                checkpoint,
                entries: first..at,
            });

            // Resume from the last checkpoint this unit had to inflate. Files
            // still beginning there put the next unit back on it, which is the
            // only place the two overlap.
            while checkpoint + 1 < checkpoints.len() && span_end(checkpoint) < end {
                checkpoint += 1;
            }
        }
    }
    units.sort_by_key(|unit| std::cmp::Reverse(unit.weight(layers, work, plan)));
    units
}

/// Past a point another worker buys nothing: inflating and writing are both
/// memory bound, and the cores end up queueing for the same bandwidth. On a
/// sixteen core machine eight was the peak, and sixteen was worse than four
/// on both wall clock and processor time.
const MAX_WORKERS: usize = 8;

fn workers(units: usize) -> usize {
    thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(units.max(1))
        .min(MAX_WORKERS)
}

fn run(
    units: &[Unit],
    layers: &[Layer],
    work: &Work,
    plan: &Plan,
    root: &Path,
) -> Result<()> {
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    // Reported by position in the queue rather than by whoever failed first,
    // so the same broken image fails the same way every time.
    let failure: Mutex<Option<(usize, Error)>> = Mutex::new(None);

    thread::scope(|scope| {
        for _ in 0..workers(units.len()) {
            let (next, stop, failure) = (&next, &stop, &failure);
            scope.spawn(move || {
                let mut buffer = Vec::new();
                let mut path = PathBuf::new();
                while !stop.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(unit) = units.get(i) else { break };
                    let result = match unit {
                        Unit::Digest { layer } => verify(&layers[*layer]),
                        Unit::Span {
                            layer,
                            checkpoint,
                            entries,
                        } => run_span(
                            &layers[*layer],
                            plan.table(*layer),
                            &work.files[*layer][entries.clone()],
                            *checkpoint,
                            root,
                            &mut buffer,
                            &mut path,
                        ),
                    };
                    if let Err(err) = result {
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

/// Inflates from `checkpoint` far enough to cover `entries`, then writes each
/// of them out of the buffer it inflated into.
fn run_span(
    layer: &Layer,
    table: &Table,
    entries: &[u32],
    checkpoint: usize,
    root: &Path,
    buffer: &mut Vec<u8>,
    path: &mut PathBuf,
) -> Result<()> {
    let base = layer.index.checkpoints[checkpoint].out_offset;
    let last = &table.entries[*entries.last().expect("a unit has entries") as usize];
    let needed = last.offset + last.size - base;

    // A body can run past the end of its own span, so the run continues into
    // the ones after it. They are claimed by whoever needs them.
    //
    // `filled` is what this unit inflated, which is not the buffer's length:
    // the buffer keeps whatever the last unit grew it to, so that growing it
    // is the only thing that has to zero anything.
    let mut filled = 0usize;
    let mut at = checkpoint;
    while (filled as u64) < needed {
        if at >= layer.index.checkpoints.len() {
            return Err(Error::io(
                format!("extracting layer {}", layer.descriptor.digest),
                std::io::Error::other("an entry runs past the end of the layer"),
            ));
        }
        filled += layer.index.extract_span_into(&layer.blob, at, buffer, filled)?;
        at += 1;
    }

    for &entry in entries {
        let entry = &table.entries[entry as usize];
        let from = (entry.offset - base) as usize;
        let to = from + entry.size as usize;
        // Bounded by what this unit inflated rather than by the buffer, which
        // keeps the length the widest unit before it needed. `needed` above is
        // taken from the last entry, so this holds already; slicing says so
        // rather than leaving the buffer free to answer with stale bytes.
        let body = buffer[..filled].get(from..to).ok_or_else(|| {
            Error::io(
                format!("extracting layer {}", layer.descriptor.digest),
                std::io::Error::other("an entry lies outside the span it was planned into"),
            )
        })?;
        write_file(root, path, entry, body)?;
    }
    Ok(())
}

fn write_file(root: &Path, path: &mut PathBuf, entry: &Entry, body: &[u8]) -> Result<()> {
    resolve(root, path, entry);
    let context = || format!("extracting {:?}", String::from_utf8_lossy(&entry.path));
    // Exclusive, so this neither follows a symlink standing here nor writes
    // over something the plan did not account for.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(entry.mode)
        .open(&path)
        .io_context(context)?;
    file.write_all(body).io_context(context)?;
    finish_file(&file, entry.mode, Some(entry.mtime)).io_context(context)
}

fn place_symlink(root: &Path, entry: &Entry) -> Result<()> {
    let mut path = PathBuf::new();
    resolve(root, &mut path, entry);
    let target = Path::new(std::ffi::OsStr::from_bytes(&entry.link));
    std::os::unix::fs::symlink(target, &path)
        .io_context(|| format!("linking {:?}", String::from_utf8_lossy(&entry.path)))?;
    set_symlink_mtime(&path, entry.mtime);
    Ok(())
}

fn place_hard_link(root: &Path, entry: &Entry) -> Result<()> {
    let mut path = PathBuf::new();
    resolve(root, &mut path, entry);
    let mut source = PathBuf::new();
    source.push(root);
    source.push(std::ffi::OsStr::from_bytes(&entry.link));
    fs::hard_link(&source, &path)
        .io_context(|| format!("linking {:?}", String::from_utf8_lossy(&entry.path)))
}

fn resolve(root: &Path, path: &mut PathBuf, entry: &Entry) {
    path.clear();
    path.push(root);
    path.push(std::ffi::OsStr::from_bytes(&entry.path));
}

fn verify(layer: &Layer) -> Result<()> {
    let descriptor = &layer.descriptor;
    if descriptor.size != 0 && descriptor.size != layer.blob.len() as u64 {
        return Err(Error::SizeMismatch {
            digest: descriptor.digest.clone(),
            expected: descriptor.size,
            actual: layer.blob.len() as u64,
        });
    }
    let actual = hex_encode(&Sha256::digest(&*layer.blob));
    if actual != parse_digest(&descriptor.digest)?.hex {
        return Err(Error::DigestMismatch {
            digest: descriptor.digest.clone(),
            actual,
        });
    }
    Ok(())
}
