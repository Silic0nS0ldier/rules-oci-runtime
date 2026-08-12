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

use ruzstd::decoding::{FrameDecoder, StreamingDecoder};

use crate::error::{Error, Result};
use crate::image::{Descriptor, verify, verify_digest};
use crate::sys::Blob;
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

/// The producer's end of the pipeline: the two things an inflater does with
/// the consumer, and the convention they share.
///
/// A consumer that has gone away is not a failure. It stopped because it has
/// an error of its own to report, and that is the one worth having, so a
/// producer finding the channel closed stops quietly rather than saying why.
pub(super) struct Sink(SyncSender<io::Result<Chunk>>);

impl Sink {
    pub(super) fn new(chunks: SyncSender<io::Result<Chunk>>) -> Self {
        Sink(chunks)
    }

    /// Hands on the first `len` bytes of `buf`. False when the consumer has
    /// stopped and there is nothing left to hand it.
    #[must_use]
    fn chunk(&self, buf: Vec<u8>, len: usize) -> bool {
        self.0.send(Ok(Chunk { buf, len })).is_ok()
    }

    /// Stops the consumer, so that it does not wait on bytes that are not
    /// coming, and gives `err` back for the producer to report as it sees fit.
    /// The consumer gets it as an `io::Error` because that is all a reader can
    /// carry; the typed error goes to whoever joins the producer.
    fn fail<E: std::fmt::Display>(&self, err: E) -> E {
        let _ = self.0.send(Err(io::Error::other(err.to_string())));
        err
    }
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
pub(super) fn read_and_hash(mut file: fs::File, sink: Sink, pool: Pool) -> HashState {
    let mut state = HashState::default();
    loop {
        let mut buf = pool.take();
        match file.read(&mut buf) {
            Ok(0) => return state,
            Ok(read) => {
                state.hasher.update(&buf[..read]);
                state.bytes += read as u64;
                if !sink.chunk(buf, read) {
                    return state;
                }
            }
            Err(err) => {
                sink.fail(err);
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
    sink: Sink,
    pool: Pool,
) -> Result<()> {
    let (raw_sender, raw_receiver) = sync_channel(PIPELINE_DEPTH);
    let (raw_pool, raw_ret) = buffer_pool();
    let hashing = thread::spawn(move || read_and_hash(file, Sink::new(raw_sender), raw_pool));
    let counted = ChunkReader::new(raw_receiver, Some(raw_ret));
    let mut decoder = match decompressor(&descriptor.media_type, counted) {
        Ok(decoder) => decoder,
        Err(err) => return Err(sink.fail(err)),
    };

    loop {
        let mut buf = pool.take();
        let read = match decoder.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) => {
                return Err(Error::io(
                    format!("reading layer {}", descriptor.digest),
                    sink.fail(err),
                ));
            }
        };
        if !sink.chunk(buf, read) {
            return Ok(());
        }
    }
    // The reader thread may still be blocked handing over a buffer, so the
    // decoder, and with it the receiving end, has to go before this joins.
    drop(decoder);
    let state = hashing.join().unwrap_or_default();
    verify_digest(descriptor, state.bytes, &state.hasher.finalize())
}

/// Runs on the decompression thread when the layer has a checkpoint index:
/// inflates disjoint spans of the blob on every available core and hands them
/// to `sender` in order. Spans need random access, so the whole compressed
/// blob is mapped; hashing it for the digest check then happens over the same
/// pages on a thread of its own, instead of alongside a read.
pub(super) fn inflate_indexed(
    file: fs::File,
    index: &zinfo::Index,
    descriptor: &Descriptor,
    sink: Sink,
) -> Result<()> {
    let blob = match Blob::of(&file) {
        Ok(blob) => blob,
        Err(err) => {
            return Err(Error::io(
                format!("reading layer {}", descriptor.digest),
                sink.fail(err),
            ));
        }
    };
    let blob: &[u8] = &blob;

    let spans = index.checkpoints.len();
    let workers = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(spans);
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);

    let mut span_error = None;
    let verified = thread::scope(|scope| {
        // The same check the span route makes on the same mapping, so a blob
        // is judged the one way however it is being read.
        let hashing = scope.spawn(|| verify(descriptor, blob));

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
                        if !sink.chunk(span, len) {
                            stop.store(true, Ordering::Relaxed);
                            break 'reorder;
                        }
                    }
                    Err(err) => {
                        span_error = Some(sink.fail(err));
                        stop.store(true, Ordering::Relaxed);
                        break 'reorder;
                    }
                }
            }
        }
        // Dropping the receiver at the end of the scope unblocks any worker
        // still sending, so the implicit joins cannot deadlock.
        hashing.join().unwrap_or_else(|_| {
            Err(Error::io(
                "verifying a layer",
                io::Error::other("the hashing thread panicked"),
            ))
        })
    });

    // The digest verdict comes first: a blob that fails it explains any span
    // error, since the index describes the blob the descriptor names.
    verified?;
    match span_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Presents the chunks as a stream for whoever drains them -- the
/// decompressor at one end of the pipeline, `tar` at the other -- returning
/// each buffer to the producer's pool once it has been read out.
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

impl io::BufRead for ChunkReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        while self.taken == self.current.len {
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
                Err(_) => return Ok(&[]),
            }
        }
        Ok(&self.current.buf[self.taken..self.current.len])
    }

    fn consume(&mut self, amt: usize) {
        self.taken = (self.taken + amt).min(self.current.len);
    }
}

impl Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use io::BufRead;

        let available = self.fill_buf()?;
        let take = available.len().min(buf.len());
        buf[..take].copy_from_slice(&available[..take]);
        self.consume(take);
        Ok(take)
    }
}

/// A decompressor reading straight out of the pipeline buffers. Without this
/// the adapters wrap the reader in a `BufReader` of their own, which copies
/// every compressed byte of the blob into it on the way past.
fn decompressor<'a, R: io::BufRead + 'a>(
    media_type: &str,
    reader: R,
) -> Result<Box<dyn Read + 'a>> {
    match compression_of(media_type) {
        Some(compression) => Ok(decompressed(compression, reader)),
        None => Err(Error::UnsupportedMediaType(media_type.to_string())),
    }
}

/// The same decompressor, for a caller that already knows the format. Building
/// an entry table goes through this so the table describes the stream
/// extraction will see, rather than a second opinion about the blob.
pub fn decompressed<'a, R: io::BufRead + 'a>(
    compression: Compression,
    reader: R,
) -> Box<dyn Read + 'a> {
    match compression {
        Compression::None => Box::new(reader),
        Compression::Gzip => Box::new(flate2::bufread::MultiGzDecoder::new(reader)),
        Compression::Zstd => Box::new(MultiFrameZstd::new(reader)),
    }
}

/// Reads a blob's zstd frames as one stream.
///
/// A blob may hold several frames back to back: concatenating them is how the
/// format appends, and a compressor that splits the work across threads emits
/// one per thread. `ruzstd`'s decoder ends at the frame it was given, so
/// without this a layer stops short wherever the first frame does, which the
/// tar reader then reports as a truncated archive. `MultiGzDecoder` spans
/// gzip members for the same reason.
///
/// Skippable frames are stepped over here rather than by the decoder, which
/// reports one as an error after consuming the reader it was handed.
struct MultiFrameZstd<R: io::BufRead>(Frames<R>);

enum Frames<R: io::BufRead> {
    /// Between frames, holding the decoder so the next frame reuses its
    /// window rather than allocating one of its own.
    Between(Peeked<R>, FrameDecoder),
    Decoding(StreamingDecoder<Peeked<R>, FrameDecoder>),
    /// The blob ended, or a frame would not start.
    Done,
}

impl<R: io::BufRead> MultiFrameZstd<R> {
    fn new(reader: R) -> Self {
        MultiFrameZstd(Frames::Between(Peeked::new(reader), FrameDecoder::new()))
    }
}

impl<R: io::BufRead> Read for MultiFrameZstd<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use io::BufRead;

        // Starting a frame consumes its header, and an empty buffer would
        // leave nowhere to put what follows it.
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            match std::mem::replace(&mut self.0, Frames::Done) {
                Frames::Decoding(mut decoder) => match decoder.read(buf) {
                    // This frame is spent; the blob may hold another.
                    Ok(0) => {
                        let (source, frames) = decoder.into_parts();
                        self.0 = Frames::Between(source, frames);
                    }
                    read => {
                        self.0 = Frames::Decoding(decoder);
                        return read;
                    }
                },
                Frames::Between(mut source, frames) => {
                    let header = source.peek(FRAME_HEADER_PEEK)?;
                    if header.is_empty() {
                        return Ok(0);
                    }
                    if let Some(len) = zinfo::skippable_frame_len(header) {
                        source.consume(FRAME_HEADER_PEEK);
                        source.skip(len - FRAME_HEADER_PEEK)?;
                        self.0 = Frames::Between(source, frames);
                        continue;
                    }
                    self.0 = Frames::Decoding(
                        StreamingDecoder::new_with_decoder(source, frames)
                            .map_err(io::Error::other)?,
                    );
                }
                Frames::Done => return Ok(0),
            }
        }
    }
}

/// Enough to hold a skippable frame's magic and length.
const FRAME_HEADER_PEEK: usize = 8;

/// A reader that can be looked at before it is handed on.
///
/// `BufRead::fill_buf` cannot answer this: it returns whatever the producer
/// last handed over, which at a chunk boundary can be a single byte.
struct Peeked<R> {
    prefix: [u8; FRAME_HEADER_PEEK],
    start: usize,
    end: usize,
    inner: R,
}

impl<R: io::BufRead> Peeked<R> {
    fn new(inner: R) -> Self {
        Peeked {
            prefix: [0; FRAME_HEADER_PEEK],
            start: 0,
            end: 0,
            inner,
        }
    }

    /// The next `n` bytes without consuming them, or fewer at the end of the
    /// blob.
    fn peek(&mut self, n: usize) -> io::Result<&[u8]> {
        self.prefix.copy_within(self.start..self.end, 0);
        self.end -= self.start;
        self.start = 0;
        while self.end < n {
            match self.inner.read(&mut self.prefix[self.end..n])? {
                0 => break,
                read => self.end += read,
            }
        }
        Ok(&self.prefix[self.start..self.end])
    }

    /// Discards `n` bytes of a frame this decoder does not read.
    fn skip(&mut self, mut n: usize) -> io::Result<()> {
        use io::BufRead;

        while n > 0 {
            let available = self.fill_buf()?.len().min(n);
            if available == 0 {
                return Err(io::Error::other("truncated skippable frame"));
            }
            self.consume(available);
            n -= available;
        }
        Ok(())
    }
}

impl<R: io::BufRead> io::BufRead for Peeked<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.start < self.end {
            return Ok(&self.prefix[self.start..self.end]);
        }
        self.start = 0;
        self.end = 0;
        self.inner.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        if self.start < self.end {
            self.start = (self.start + amt).min(self.end);
            return;
        }
        self.inner.consume(amt);
    }
}

impl<R: io::BufRead> Read for Peeked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use io::BufRead;

        let available = self.fill_buf()?;
        let take = available.len().min(buf.len());
        buf[..take].copy_from_slice(&available[..take]);
        self.consume(take);
        Ok(take)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
