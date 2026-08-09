//! Turning a compressed layer blob into a stream of bytes for the unpacker.
//!
//! Decompression is CPU bound and writing the rootfs is IO bound, so the two
//! run on separate threads and overlap. With a checkpoint index the inflating
//! side additionally spreads over the idle cores, since checkpoints let
//! disjoint spans of one gzip member decompress independently.

use std::fs;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::image::{Descriptor, hex_encode, parse_digest};
use crate::zinfo;

/// Size of each buffer handed from the decompressor to the writer.
pub(super) const CHUNK_BYTES: usize = 256 * 1024;

/// How many chunks may be in flight, bounding the pipeline to 2 MiB.
pub(super) const PIPELINE_DEPTH: usize = 8;

/// A pipeline buffer and how much of it the producer filled.
///
/// The buffer keeps its full length for its whole life, so a recycled one is
/// handed straight back to `Read::read` without being zeroed again; `len` is
/// what makes the rest of it invisible.
pub(super) struct Chunk {
    pub(super) buf: Vec<u8>,
    pub(super) len: usize,
}

impl std::ops::Deref for Chunk {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// The producer's end of a buffer pool.
///
/// A quarter of a megabyte is above the threshold at which an allocator stops
/// serving from its heap: musl sends every one of these to `mmap` and gives it
/// back on free, which for a 535 MiB layer is thousands of mappings and a
/// page fault for each of their pages. Circulating a handful of buffers costs
/// one channel operation instead.
pub(super) struct Pool(Receiver<Vec<u8>>);

impl Pool {
    pub(super) fn take(&self) -> Vec<u8> {
        self.0.try_recv().unwrap_or_else(|_| vec![0u8; CHUNK_BYTES])
    }
}

/// Creates a pool and the handle used to return buffers to it. Returns never
/// block, so the channel holds every buffer the pipeline can have in flight.
pub(super) fn buffer_pool() -> (Pool, SyncSender<Vec<u8>>) {
    let (ret, free) = sync_channel(PIPELINE_DEPTH * 2);
    (Pool(free), ret)
}

#[derive(Default)]
pub(super) struct HashState {
    pub(super) hasher: Sha256,
    pub(super) bytes: u64,
}

/// Reads the blob and hashes it, handing each buffer to the decompressor by
/// move. With decompression on the critical path, sha256 of the compressed
/// bytes is time the inflater is not inflating, and the read it accompanies is
/// nearly free, so the two belong together on a thread of their own.
///
/// The hash covers exactly the bytes the decompressor is given, so the digest
/// still describes what was extracted rather than a second read of the file.
pub(super) fn read_and_hash(
    mut file: fs::File,
    sender: SyncSender<io::Result<Chunk>>,
    pool: Pool,
) -> HashState {
    let mut state = HashState::default();
    loop {
        let mut buf = pool.take();
        match file.read(&mut buf) {
            Ok(0) => return state,
            Ok(read) => {
                state.hasher.update(&buf[..read]);
                state.bytes += read as u64;
                // The consumer has stopped, and has an error of its own to report.
                if sender.send(Ok(Chunk { buf, len: read })).is_err() {
                    return state;
                }
            }
            Err(err) => {
                let _ = sender.send(Err(err));
                return state;
            }
        }
    }
}

/// Runs on the decompression thread: inflates the blob into `sender` and checks
/// it against its descriptor, while a third thread reads and hashes it.
/// Reaching the end of the blob is what makes the digest meaningful, so the
/// size and digest are only reported when the consumer took everything; a
/// consumer that stopped early has an error of its own to report.
pub(super) fn inflate_blob(
    file: fs::File,
    descriptor: &Descriptor,
    sender: SyncSender<io::Result<Chunk>>,
    pool: Pool,
) -> Result<()> {
    let (raw_sender, raw_receiver) = sync_channel(PIPELINE_DEPTH);
    let (raw_pool, raw_ret) = buffer_pool();
    let hashing = thread::spawn(move || read_and_hash(file, raw_sender, raw_pool));
    let counted = ChunkReader::new(raw_receiver, Some(raw_ret));
    let mut decoder = match decompressor(&descriptor.media_type, counted) {
        Ok(decoder) => decoder,
        Err(err) => {
            let _ = sender.send(Err(io::Error::other(err.to_string())));
            return Err(err);
        }
    };

    loop {
        let mut buf = pool.take();
        let read = match decoder.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) => {
                let message = err.to_string();
                let _ = sender.send(Err(err));
                return Err(Error::io(
                    format!("reading layer {}", descriptor.digest),
                    io::Error::other(message),
                ));
            }
        };
        if sender.send(Ok(Chunk { buf, len: read })).is_err() {
            return Ok(());
        }
    }
    // The reader thread may still be blocked handing over a buffer, so the
    // decoder, and with it the receiving end, has to go before this joins.
    drop(decoder);
    let state = hashing.join().unwrap_or_default();
    if descriptor.size != 0 && descriptor.size != state.bytes {
        return Err(Error::SizeMismatch {
            digest: descriptor.digest.clone(),
            expected: descriptor.size,
            actual: state.bytes,
        });
    }
    let actual = hex_encode(&state.hasher.finalize());
    if actual != parse_digest(&descriptor.digest)?.hex {
        return Err(Error::DigestMismatch {
            digest: descriptor.digest.clone(),
            actual,
        });
    }
    Ok(())
}

/// Runs on the decompression thread when the layer has a checkpoint index:
/// inflates disjoint spans of the blob on every available core and hands them
/// to `sender` in order. Spans need random access, so the whole compressed
/// blob is mapped; hashing it for the digest check then happens over the same
/// pages on a thread of its own, instead of alongside a read.
pub(super) fn inflate_indexed(
    mut file: fs::File,
    index: &zinfo::Index,
    descriptor: &Descriptor,
    sender: SyncSender<io::Result<Chunk>>,
) -> Result<()> {
    let len = file.metadata().map_or(0, |m| m.len()) as usize;
    let mapped = crate::sys::Mapping::of(&file, len);
    // Nothing to map, or the kernel would not: the spans still need the whole
    // blob, so fall back to a copy of it.
    let mut read = Vec::new();
    if mapped.is_none()
        && let Err(err) = file.read_to_end(&mut read)
    {
        let _ = sender.send(Err(io::Error::other(err.to_string())));
        return Err(Error::io(
            format!("reading layer {}", descriptor.digest),
            err,
        ));
    }
    let blob: &[u8] = match &mapped {
        Some(mapping) => mapping,
        None => &read,
    };

    let spans = index.checkpoints.len();
    let workers = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(spans);
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);

    let mut span_error = None;
    let digest = thread::scope(|scope| {
        let hashing = scope.spawn(|| Sha256::digest(blob));

        // Workers claim span indices; completed spans are put back in order
        // here. The channel bound plus one finished span per worker caps how
        // far decompression runs ahead of the writer.
        let (span_sender, span_receiver) = sync_channel::<(usize, Result<Vec<u8>>)>(workers);
        for _ in 0..workers {
            let span_sender = span_sender.clone();
            let (next, stop) = (&next, &stop);
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= spans {
                        break;
                    }
                    let result = index.extract_span(blob, i);
                    let failed = result.is_err();
                    if span_sender.send((i, result)).is_err() || failed {
                        break;
                    }
                }
            });
        }
        drop(span_sender);

        let mut pending = std::collections::HashMap::new();
        let mut want = 0;
        'reorder: while let Ok((i, result)) = span_receiver.recv() {
            pending.insert(i, result);
            while let Some(result) = pending.remove(&want) {
                want += 1;
                match result {
                    Ok(span) => {
                        let len = span.len();
                        if sender.send(Ok(Chunk { buf: span, len })).is_err() {
                            // The writer stopped; it has an error of its own.
                            stop.store(true, Ordering::Relaxed);
                            break 'reorder;
                        }
                    }
                    Err(err) => {
                        let _ = sender.send(Err(io::Error::other(err.to_string())));
                        span_error = Some(err);
                        stop.store(true, Ordering::Relaxed);
                        break 'reorder;
                    }
                }
            }
        }
        // Dropping the receiver at the end of the scope unblocks any worker
        // still sending, so the implicit joins cannot deadlock.
        hashing.join().unwrap_or_default()
    });

    // The digest verdict comes first: a blob that fails it explains any span
    // error, since the index describes the blob the descriptor names.
    if descriptor.size != 0 && descriptor.size != blob.len() as u64 {
        return Err(Error::SizeMismatch {
            digest: descriptor.digest.clone(),
            expected: descriptor.size,
            actual: blob.len() as u64,
        });
    }
    let actual = hex_encode(&digest);
    if actual != parse_digest(&descriptor.digest)?.hex {
        return Err(Error::DigestMismatch {
            digest: descriptor.digest.clone(),
            actual,
        });
    }
    match span_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Presents the inflated chunks as a stream for `tar` to walk, returning each
/// buffer to the producer's pool once it has been drained.
pub(super) struct ChunkReader {
    chunks: Receiver<io::Result<Chunk>>,
    current: Chunk,
    taken: usize,
    ret: Option<SyncSender<Vec<u8>>>,
}

impl ChunkReader {
    pub(super) fn new(
        chunks: Receiver<io::Result<Chunk>>,
        ret: Option<SyncSender<Vec<u8>>>,
    ) -> Self {
        ChunkReader {
            chunks,
            current: Chunk {
                buf: Vec::new(),
                len: 0,
            },
            taken: 0,
            ret,
        }
    }
}

impl Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let available = self.current.len - self.taken;
            if available > 0 {
                let take = available.min(buf.len());
                buf[..take].copy_from_slice(&self.current.buf[self.taken..self.taken + take]);
                self.taken += take;
                return Ok(take);
            }
            // A full pool means the producer has all the buffers it can use,
            // so the spare is dropped rather than blocking the pipeline on it.
            if let Some(ret) = &self.ret
                && !self.current.buf.is_empty()
            {
                let _ = ret.try_send(std::mem::take(&mut self.current.buf));
            }
            match self.chunks.recv() {
                Ok(Ok(chunk)) => {
                    self.current = chunk;
                    self.taken = 0;
                }
                Ok(Err(err)) => return Err(err),
                // The sender is gone, so the blob has been read in full.
                Err(_) => return Ok(0),
            }
        }
    }
}

fn decompressor<'a, R: Read + 'a>(media_type: &str, reader: R) -> Result<Box<dyn Read + 'a>> {
    match compression_of(media_type) {
        Some(Compression::None) => Ok(Box::new(reader)),
        Some(Compression::Gzip) => Ok(Box::new(flate2::read::MultiGzDecoder::new(reader))),
        Some(Compression::Zstd) => {
            let decoder = ruzstd::decoding::StreamingDecoder::new(reader)
                .map_err(|err| Error::io("initialising zstd decoder", io::Error::other(err)))?;
            Ok(Box::new(decoder))
        }
        None => Err(Error::UnsupportedMediaType(media_type.to_string())),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
}

pub fn compression_of(media_type: &str) -> Option<Compression> {
    // Non-distributable layer types carry the same payload, hence the suffix match.
    match media_type {
        "application/vnd.oci.image.layer.v1.tar"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar"
        | "application/vnd.docker.image.rootfs.diff.tar"
        | "application/x-tar" => Some(Compression::None),
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip"
        | "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip" => Some(Compression::Gzip),
        "application/vnd.oci.image.layer.v1.tar+zstd"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd" => Some(Compression::Zstd),
        _ => None,
    }
}
