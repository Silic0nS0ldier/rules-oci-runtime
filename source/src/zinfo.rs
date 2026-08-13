//! Checkpoint index for compressed layer blobs, in the spirit of zlib's
//! `zran.c` and SOCI's zTOC. A checkpoint records where decompression can be
//! resumed part way through a blob, which lets independent threads decompress
//! disjoint spans of it.
//!
//! What that costs depends on the format. Deflate carries its state in a
//! 32 KiB sliding window, so a gzip checkpoint stores one and can sit at any
//! block boundary. A zstd frame instead declares its own window -- 8 MiB is
//! the size the RFC asks decoders to support, and the format permits far more
//! -- so storing one per span would rival the blob. Frames are independent of
//! each other, though, so a zstd checkpoint sits at a frame boundary and
//! stores nothing at all. A blob written as a single frame therefore indexes
//! as a single span, which is the price of not rewriting the blob.
//!
//! Built at Bazel build time by `oci_runtime index`; the image blobs are never
//! modified. Each span also carries a CRC-32 of its uncompressed bytes: blob
//! digests only cover the compressed bytes, so a corrupt index would otherwise
//! corrupt the rootfs silently.

use std::io::{self, Read, Write};

use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};

use crate::error::{Error, Result};

const MAGIC: &[u8; 4] = b"OZI2";
const WINDOW_SIZE: usize = 32 * 1024;
/// How much of a span is decompressed before it is checksummed. Small enough
/// that the bytes are still in cache when the checksum reads them, large
/// enough that the extra decompressor calls do not show.
const CHECK_BYTES: usize = 1048576;
/// Checkpoints are only useful while spans decompress in bounded memory.
const MAX_CHECKPOINTS: u32 = 1 << 20;

/// Which compressed format an index describes, and so what a checkpoint in it
/// means. An index is only usable against the format it was built from, so
/// this is recorded rather than inferred from the layer's media type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Gzip,
    Zstd,
}

impl Flavor {
    fn code(self) -> u8 {
        match self {
            Flavor::Gzip => 0,
            Flavor::Zstd => 1,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Flavor::Gzip),
            1 => Some(Flavor::Zstd),
            _ => None,
        }
    }
}

/// Resume point: a deflate block boundary (or gzip member start) for
/// [`Flavor::Gzip`], a frame start for [`Flavor::Zstd`].
#[derive(Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// Offset of the first compressed byte not yet consumed at the boundary.
    pub in_offset: u64,
    /// Unconsumed high bits of the byte at `in_offset - 1`; 0 when byte aligned.
    pub bits: u8,
    /// Uncompressed offset this checkpoint resumes at.
    pub out_offset: u64,
    /// Sliding window at the boundary; empty at a stream or member start,
    /// where inflate instead parses the gzip header, and always empty for zstd.
    pub window: Vec<u8>,
    /// CRC-32 of the uncompressed span from here to the next checkpoint.
    pub crc: u32,
}

/// A blob's checkpoints, always starting with the stream start, so that the
/// spans between consecutive checkpoints cover the whole uncompressed output.
#[derive(Debug, PartialEq, Eq)]
pub struct Index {
    pub flavor: Flavor,
    pub uncompressed_len: u64,
    pub checkpoints: Vec<Checkpoint>,
}

impl Index {
    /// Decompresses `blob` once, dropping a checkpoint at the first resumable
    /// boundary after every `span` uncompressed bytes.
    pub fn build(flavor: Flavor, blob: &[u8], span: u64) -> Result<Self> {
        match flavor {
            Flavor::Gzip => Self::build_gzip(blob, span),
            Flavor::Zstd => Self::build_zstd(blob, span),
        }
    }

    fn build_gzip(blob: &[u8], span: u64) -> Result<Self> {
        let context = "indexing gzip blob";
        let mut inflater = Inflater::new(HEADER_WINDOW_BITS).map_err(|e| Error::io(context, e))?;
        let mut buf = vec![0u8; 1 << 20];
        let mut checkpoints = vec![Checkpoint {
            in_offset: 0,
            bits: 0,
            out_offset: 0,
            window: Vec::new(),
            crc: 0,
        }];
        let mut crc = flate2::Crc::new();
        let mut in_pos: usize = 0;
        let mut out_pos: u64 = 0;
        let mut last = 0u64;

        loop {
            let (consumed, produced, ret) = inflater
                .inflate(&blob[in_pos..], &mut buf, Flush::Block)
                .map_err(|e| Error::io(context, e))?;
            in_pos += consumed;
            out_pos += produced as u64;
            crc.update(&buf[..produced]);

            match ret {
                Z_STREAM_END if in_pos == blob.len() => break,
                Z_STREAM_END => {
                    // Another gzip member follows; its start is a checkpoint.
                    checkpoints.last_mut().expect("non-empty").crc = crc.sum();
                    crc.reset();
                    checkpoints.push(Checkpoint {
                        in_offset: in_pos as u64,
                        bits: 0,
                        out_offset: out_pos,
                        window: Vec::new(),
                        crc: 0,
                    });
                    last = out_pos;
                    inflater.reset().map_err(|e| Error::io(context, e))?;
                    continue;
                }
                Z_OK if consumed == 0 && produced == 0 => {
                    return Err(Error::io(
                        context,
                        io::Error::other("truncated or corrupt gzip stream"),
                    ));
                }
                Z_OK => {}
                _ => unreachable!("Inflater::inflate returned {ret}"),
            }

            // data_type: bits 0..=2 count the unconsumed bits of the last
            // consumed byte, 128 marks a block boundary, 64 the end of stream.
            let data_type = inflater.data_type();
            if data_type & 128 != 0 && data_type & 64 == 0 && out_pos >= last + span {
                if checkpoints.len() as u32 == MAX_CHECKPOINTS {
                    return Err(Error::io(context, io::Error::other("too many checkpoints")));
                }
                checkpoints.last_mut().expect("non-empty").crc = crc.sum();
                crc.reset();
                checkpoints.push(Checkpoint {
                    in_offset: in_pos as u64,
                    bits: (data_type & 7) as u8,
                    out_offset: out_pos,
                    window: inflater.window().map_err(|e| Error::io(context, e))?,
                    crc: 0,
                });
                last = out_pos;
            }
        }
        checkpoints.last_mut().expect("non-empty").crc = crc.sum();

        Ok(Index {
            flavor: Flavor::Gzip,
            uncompressed_len: out_pos,
            checkpoints,
        })
    }

    /// Walks `blob` frame by frame, starting a checkpoint at the first frame
    /// boundary past every `span` uncompressed bytes.
    ///
    /// Coalescing frames matters as much as splitting them: a `zstd:chunked`
    /// or seekable-format blob can hold thousands of small frames, and one
    /// span each would be more queue than work.
    fn build_zstd(blob: &[u8], span: u64) -> Result<Self> {
        let context = "indexing zstd blob";
        let mut checkpoints = vec![Checkpoint {
            in_offset: 0,
            bits: 0,
            out_offset: 0,
            window: Vec::new(),
            crc: 0,
        }];
        let mut decoder = FrameDecoder::new();
        let mut scratch = vec![0u8; 1 << 20];
        let mut crc = flate2::Crc::new();
        let mut pos = 0usize;
        let mut out_pos = 0u64;
        let mut last = 0u64;

        while pos < blob.len() {
            if let Some(len) = skippable_frame_len(&blob[pos..]) {
                pos = pos
                    .checked_add(len)
                    .filter(|end| *end <= blob.len())
                    .ok_or_else(|| {
                        Error::io(context, io::Error::other("truncated skippable frame"))
                    })?;
                continue;
            }
            if out_pos >= last + span {
                if checkpoints.len() as u32 == MAX_CHECKPOINTS {
                    return Err(Error::io(context, io::Error::other("too many checkpoints")));
                }
                checkpoints.last_mut().expect("non-empty").crc = crc.sum();
                crc.reset();
                checkpoints.push(Checkpoint {
                    in_offset: pos as u64,
                    bits: 0,
                    out_offset: out_pos,
                    window: Vec::new(),
                    crc: 0,
                });
                last = out_pos;
            }
            let mut produced = 0u64;
            pos = decode_frame(&mut decoder, blob, pos, &mut scratch, |bytes| {
                crc.update(bytes);
                produced += bytes.len() as u64;
            })
            .map_err(|e| Error::io(context, e))?;
            out_pos += produced;
        }
        checkpoints.last_mut().expect("non-empty").crc = crc.sum();

        Ok(Index {
            flavor: Flavor::Zstd,
            uncompressed_len: out_pos,
            checkpoints,
        })
    }

    /// Decompresses the span between checkpoint `i` and its successor (or the
    /// end of the stream) and verifies it against the recorded CRC.
    pub fn extract_span(&self, blob: &[u8], i: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.extract_span_into(blob, i, &mut out, 0, &mut Decoders::default())?;
        Ok(out)
    }

    /// The same, into `out` at `at`, returning how much was written.
    ///
    /// `out` is only ever grown, never truncated, so a caller working through
    /// span after span pays to zero a buffer once rather than once per span.
    /// Only the bytes this reports are freshly inflated; anything after them
    /// is whatever the last caller left behind.
    pub fn extract_span_into(
        &self,
        blob: &[u8],
        i: usize,
        out: &mut Vec<u8>,
        at: usize,
        decoders: &mut Decoders,
    ) -> Result<usize> {
        let context = "resuming from checkpoint";
        let point = &self.checkpoints[i];
        let end = self
            .checkpoints
            .get(i + 1)
            .map_or(self.uncompressed_len, |next| next.out_offset);
        let len = (end - point.out_offset) as usize;

        // The index describes the blob its digest names, but the blob handed
        // in could still be another file entirely.
        if point.in_offset as usize > blob.len() || (point.bits != 0 && point.in_offset == 0) {
            return Err(Error::io(
                context,
                io::Error::other(format!("checkpoint {i} lies beyond the blob")),
            ));
        }

        if out.len() < at + len {
            out.resize(at + len, 0);
        }
        let span = &mut out[at..at + len];
        let mut crc = flate2::Crc::new();
        match self.flavor {
            Flavor::Gzip => inflate_span(blob, point, span, &mut crc, decoders)?,
            Flavor::Zstd => decode_span(blob, point, span, &mut crc, decoders)?,
        }

        if crc.sum() != point.crc {
            return Err(Error::io(
                context,
                io::Error::other(format!("checkpoint {i} does not match its span checksum")),
            ));
        }
        Ok(len)
    }

    pub fn write_to(&self, mut writer: impl Write) -> io::Result<()> {
        writer.write_all(MAGIC)?;
        writer.write_all(&[self.flavor.code()])?;
        writer.write_all(&self.uncompressed_len.to_le_bytes())?;
        writer.write_all(&(self.checkpoints.len() as u32).to_le_bytes())?;
        for point in &self.checkpoints {
            writer.write_all(&point.in_offset.to_le_bytes())?;
            writer.write_all(&point.out_offset.to_le_bytes())?;
            writer.write_all(&point.crc.to_le_bytes())?;
            writer.write_all(&[point.bits])?;
            let mut encoder =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&point.window)?;
            let compressed = encoder.finish()?;
            writer.write_all(&(compressed.len() as u32).to_le_bytes())?;
            writer.write_all(&compressed)?;
        }
        Ok(())
    }

    pub fn read_from(mut reader: impl Read) -> io::Result<Self> {
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;
        if magic != *MAGIC {
            return Err(io::Error::other("not a checkpoint index"));
        }
        let mut flavor = [0u8];
        reader.read_exact(&mut flavor)?;
        let flavor = Flavor::from_code(flavor[0])
            .ok_or_else(|| io::Error::other("unknown compressed format"))?;
        let uncompressed_len = read_u64(&mut reader)?;
        let count = read_u32(&mut reader)?;
        if count == 0 || count > MAX_CHECKPOINTS {
            return Err(io::Error::other("implausible checkpoint count"));
        }
        let mut checkpoints = Vec::with_capacity(count as usize);
        let mut previous = None;
        for _ in 0..count {
            let in_offset = read_u64(&mut reader)?;
            let out_offset = read_u64(&mut reader)?;
            let crc = read_u32(&mut reader)?;
            let mut bits = [0u8];
            reader.read_exact(&mut bits)?;
            let compressed_len = read_u32(&mut reader)? as usize;
            let mut window = Vec::with_capacity(WINDOW_SIZE);
            flate2::read::DeflateDecoder::new(reader.by_ref().take(compressed_len as u64))
                .take(WINDOW_SIZE as u64 + 1)
                .read_to_end(&mut window)?;
            if window.len() > WINDOW_SIZE || bits[0] > 7 {
                return Err(io::Error::other("malformed checkpoint"));
            }
            // A zstd checkpoint is a frame start, which resumes from nothing.
            if flavor == Flavor::Zstd && (!window.is_empty() || bits[0] != 0) {
                return Err(io::Error::other("malformed checkpoint"));
            }
            if previous.is_some_and(|(i, o)| in_offset < i || out_offset < o) {
                return Err(io::Error::other("checkpoints out of order"));
            }
            previous = Some((in_offset, out_offset));
            checkpoints.push(Checkpoint {
                in_offset,
                bits: bits[0],
                out_offset,
                window,
                crc,
            });
        }
        if previous.is_some_and(|(_, o)| uncompressed_len < o) {
            return Err(io::Error::other("checkpoints out of order"));
        }
        Ok(Index {
            flavor,
            uncompressed_len,
            checkpoints,
        })
    }
}

/// Scratch that a span decode needs and the next one can have back.
///
/// Both decoders own buffers measured in tens of kilobytes to a megabyte, and
/// a worker decodes span after span. Building them per span left the allocator
/// asking the kernel for the same memory hundreds of times, and every unmap
/// interrupts the other workers to shoot down their TLBs.
#[derive(Default)]
pub struct Decoders {
    gzip: Option<Inflater>,
    zstd: Option<(FrameDecoder, Vec<u8>)>,
}

impl Decoders {
    /// An inflater rewound to `window_bits`, built on first use.
    fn inflater(&mut self, window_bits: i32) -> io::Result<&mut Inflater> {
        match &mut self.gzip {
            Some(inflater) => inflater.reset_to(window_bits)?,
            slot => *slot = Some(Inflater::new(window_bits)?),
        }
        Ok(self.gzip.as_mut().expect("just filled"))
    }

    fn zstd(&mut self) -> (&mut FrameDecoder, &mut Vec<u8>) {
        let (decoder, scratch) = self
            .zstd
            .get_or_insert_with(|| (FrameDecoder::new(), vec![0u8; 1 << 20]));
        (decoder, scratch)
    }
}

/// Inflates one gzip span into `span`, which is exactly its length,
/// checksumming each piece as it lands.
fn inflate_span(
    blob: &[u8],
    point: &Checkpoint,
    span: &mut [u8],
    crc: &mut flate2::Crc,
    decoders: &mut Decoders,
) -> Result<()> {
    let context = "resuming from checkpoint";
    // An empty window is a stream or member start: parse the header there.
    let at_header = point.window.is_empty() && point.bits == 0;
    let inflater = decoders
        .inflater(if at_header {
            HEADER_WINDOW_BITS
        } else {
            RAW_WINDOW_BITS
        })
        .map_err(|e| Error::io(context, e))?;
    if point.bits != 0 {
        // The boundary sits inside the previous byte: feed its unconsumed
        // high bits, then continue from the byte boundary.
        let byte = blob[point.in_offset as usize - 1];
        inflater
            .prime(point.bits, byte >> (8 - point.bits))
            .map_err(|e| Error::io(context, e))?;
    }
    if !point.window.is_empty() {
        inflater
            .set_dictionary(&point.window)
            .map_err(|e| Error::io(context, e))?;
    }

    let len = span.len();
    let mut in_pos = point.in_offset as usize;
    let mut filled = 0usize;
    while filled < len {
        // Inflating a whole span before checksumming it means the checksum
        // reads from memory what inflate has long since evicted. A span is
        // megabytes; a piece stays in cache.
        let piece = (filled + CHECK_BYTES).min(len);
        let (consumed, produced, ret) = inflater
            .inflate(&blob[in_pos..], &mut span[filled..piece], Flush::None)
            .map_err(|e| Error::io(context, e))?;
        in_pos += consumed;
        crc.update(&span[filled..filled + produced]);
        filled += produced;
        match ret {
            Z_STREAM_END if filled < len => {
                // The span continues into the next gzip member.
                if in_pos == blob.len() {
                    return Err(Error::io(
                        context,
                        io::Error::other("stream ended before the indexed span"),
                    ));
                }
                inflater
                    .reset_to(HEADER_WINDOW_BITS)
                    .map_err(|e| Error::io(context, e))?;
            }
            Z_OK if consumed == 0 && produced == 0 => {
                return Err(Error::io(
                    context,
                    io::Error::other("truncated or corrupt gzip stream"),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Decodes zstd frames from the checkpoint until `span` is full.
///
/// A span ends where the next checkpoint begins, which is a frame boundary, so
/// the frames decoded here produce exactly `span.len()` bytes. Producing more
/// than that means the index does not describe this blob.
fn decode_span(
    blob: &[u8],
    point: &Checkpoint,
    span: &mut [u8],
    crc: &mut flate2::Crc,
    decoders: &mut Decoders,
) -> Result<()> {
    let context = "resuming from checkpoint";
    let len = span.len();
    let (decoder, scratch) = decoders.zstd();
    let mut pos = point.in_offset as usize;
    let mut filled = 0usize;

    while filled < len {
        if pos >= blob.len() {
            return Err(Error::io(
                context,
                io::Error::other("stream ended before the indexed span"),
            ));
        }
        if let Some(skip) = skippable_frame_len(&blob[pos..]) {
            pos = pos
                .checked_add(skip)
                .filter(|end| *end <= blob.len())
                .ok_or_else(|| Error::io(context, io::Error::other("truncated skippable frame")))?;
            continue;
        }
        let mut overflowed = false;
        pos = decode_frame(decoder, blob, pos, scratch, |bytes| {
            let take = bytes.len().min(len - filled);
            span[filled..filled + take].copy_from_slice(&bytes[..take]);
            crc.update(&bytes[..take]);
            filled += take;
            overflowed |= take < bytes.len();
        })
        .map_err(|e| Error::io(context, e))?;
        if overflowed {
            return Err(Error::io(
                context,
                io::Error::other("the span decoded to more than the index recorded"),
            ));
        }
    }
    Ok(())
}

/// The bytes a zstd skippable frame occupies at the start of `bytes`, its
/// 8 byte header included; `None` when a data frame starts there instead.
///
/// Skippable frames are how the seekable format carries its jump table and how
/// `zstd:chunked` carries its manifest, so a real layer can begin or end with
/// one, and every decoder is required to ignore them.
pub fn skippable_frame_len(bytes: &[u8]) -> Option<usize> {
    const SKIPPABLE: std::ops::RangeInclusive<u32> = 0x184D_2A50..=0x184D_2A5F;
    let magic = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
    if !SKIPPABLE.contains(&magic) {
        return None;
    }
    let len = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?);
    Some(8 + len as usize)
}

/// Decodes the one zstd frame starting at `blob[at]`, handing each run of
/// output to `emit`, and returns the offset just past it.
fn decode_frame(
    decoder: &mut FrameDecoder,
    blob: &[u8],
    at: usize,
    scratch: &mut [u8],
    mut emit: impl FnMut(&[u8]),
) -> io::Result<usize> {
    let mut source = &blob[at..];
    decoder.init(&mut source).map_err(io::Error::other)?;
    loop {
        let finished = decoder.is_finished();
        loop {
            let read = decoder.read(scratch)?;
            if read == 0 {
                break;
            }
            emit(&scratch[..read]);
        }
        if finished {
            break;
        }
        decoder
            .decode_blocks(&mut source, BlockDecodingStrategy::UptoBytes(scratch.len()))
            .map_err(io::Error::other)?;
    }
    Ok(blob.len() - source.len())
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

/// Accept a gzip (or zlib) header before the deflate stream.
const HEADER_WINDOW_BITS: i32 = 32 + 15;
/// Raw deflate, for resuming mid-stream.
const RAW_WINDOW_BITS: i32 = -15;

const Z_OK: i32 = 0;
const Z_STREAM_END: i32 = 1;

#[repr(i32)]
#[derive(Clone, Copy)]
enum Flush {
    None = 0,
    /// Return at every deflate block boundary.
    Block = 5,
}

/// The one place that talks to libz-rs-sys. flate2 hides exactly the calls
/// checkpointing needs (`inflatePrime`, `inflate{Set,Get}Dictionary`, Z_BLOCK),
/// so this drives the same zlib-rs inflate through its C-shaped API.
struct Inflater(libz_rs_sys::z_stream);

impl Inflater {
    fn new(window_bits: i32) -> io::Result<Self> {
        let mut stream = std::mem::MaybeUninit::<libz_rs_sys::z_stream>::zeroed();
        // SAFETY: `stream` is a zeroed z_stream, which inflateInit2_ accepts
        // and initialises; version and size describe this build's z_stream.
        let ret = unsafe {
            libz_rs_sys::inflateInit2_(
                stream.as_mut_ptr(),
                window_bits,
                libz_rs_sys::zlibVersion(),
                std::mem::size_of::<libz_rs_sys::z_stream>() as i32,
            )
        };
        if ret != Z_OK {
            return Err(io::Error::other("initialising inflate failed"));
        }
        // SAFETY: inflateInit2_ returned Z_OK, so the stream is initialised.
        Ok(Inflater(unsafe { stream.assume_init() }))
    }

    /// Returns (input consumed, output produced, Z_OK or Z_STREAM_END).
    fn inflate(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: Flush,
    ) -> io::Result<(usize, usize, i32)> {
        self.0.next_in = input.as_ptr().cast_mut();
        self.0.avail_in = input.len().min(u32::MAX as usize) as u32;
        self.0.next_out = output.as_mut_ptr();
        self.0.avail_out = output.len().min(u32::MAX as usize) as u32;
        // SAFETY: the stream is initialised and next_in/avail_in and
        // next_out/avail_out describe the two slices above.
        let ret = unsafe { libz_rs_sys::inflate(&mut self.0, flush as i32) };
        let consumed = input.len().min(u32::MAX as usize) - self.0.avail_in as usize;
        let produced = output.len().min(u32::MAX as usize) - self.0.avail_out as usize;
        match ret {
            Z_OK | Z_STREAM_END => Ok((consumed, produced, ret)),
            // Z_BUF_ERROR just means no progress was possible; let the caller
            // decide whether more input exists.
            -5 => Ok((consumed, produced, Z_OK)),
            _ => Err(io::Error::other(format!("inflate failed ({ret})"))),
        }
    }

    fn data_type(&self) -> i32 {
        self.0.data_type
    }

    /// Feeds the sub-byte bits a resumed stream starts inside of.
    fn prime(&mut self, bits: u8, value: u8) -> io::Result<()> {
        // SAFETY: the stream is initialised; bits <= 7 by construction.
        let ret = unsafe { libz_rs_sys::inflatePrime(&mut self.0, bits as i32, value as i32) };
        if ret != Z_OK {
            return Err(io::Error::other("inflatePrime failed"));
        }
        Ok(())
    }

    /// Loads a checkpoint window; raw inflate skips the Adler-32 check.
    fn set_dictionary(&mut self, window: &[u8]) -> io::Result<()> {
        // SAFETY: the stream is initialised and the pointer/length pair
        // describes the `window` slice.
        let ret = unsafe {
            libz_rs_sys::inflateSetDictionary(&mut self.0, window.as_ptr(), window.len() as u32)
        };
        if ret != Z_OK {
            return Err(io::Error::other("inflateSetDictionary failed"));
        }
        Ok(())
    }

    /// Copies out the current sliding window, unrotated.
    fn window(&mut self) -> io::Result<Vec<u8>> {
        let mut window = vec![0u8; WINDOW_SIZE];
        let mut len: u32 = 0;
        // SAFETY: the stream is initialised and `window` has the 32 KiB the
        // dictionary can occupy at most.
        let ret = unsafe {
            libz_rs_sys::inflateGetDictionary(&mut self.0, window.as_mut_ptr(), &mut len)
        };
        if ret != Z_OK || len as usize > WINDOW_SIZE {
            return Err(io::Error::other("inflateGetDictionary failed"));
        }
        window.truncate(len as usize);
        Ok(window)
    }

    /// Rewinds to before the header, for the next gzip member.
    fn reset(&mut self) -> io::Result<()> {
        // SAFETY: the stream is initialised.
        let ret = unsafe { libz_rs_sys::inflateReset(&mut self.0) };
        if ret != Z_OK {
            return Err(io::Error::other("inflateReset failed"));
        }
        Ok(())
    }

    /// Rewinds to before the header and changes what header to expect, so one
    /// inflater serves spans that resume at a member start and spans that
    /// resume mid stream.
    fn reset_to(&mut self, window_bits: i32) -> io::Result<()> {
        // SAFETY: the stream is initialised; inflateReset2 accepts the same
        // window bits inflateInit2_ does.
        let ret = unsafe { libz_rs_sys::inflateReset2(&mut self.0, window_bits) };
        if ret != Z_OK {
            return Err(io::Error::other("inflateReset2 failed"));
        }
        Ok(())
    }
}

impl Drop for Inflater {
    fn drop(&mut self) {
        // SAFETY: the stream is initialised; inflateEnd is the matching free.
        unsafe { libz_rs_sys::inflateEnd(&mut self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compressible but non-repeating, so deflate emits many dynamic blocks.
    fn sample_data(len: usize) -> Vec<u8> {
        let words = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];
        let mut out = Vec::with_capacity(len + 16);
        let mut state: u64 = 0x2545F4914F6CDD1D;
        while out.len() < len {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            out.extend_from_slice(words[(state >> 33) as usize % words.len()].as_bytes());
            out.extend_from_slice(state.to_le_bytes()[..3].as_ref());
        }
        out.truncate(len);
        out
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    /// One zstd frame, as any single-threaded compressor writes.
    fn zstd(data: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
    }

    /// `data` cut into frames of `each` bytes, which is what `pzstd`, the
    /// seekable format and `zstd:chunked` all produce.
    fn zstd_framed(data: &[u8], each: usize) -> Vec<u8> {
        data.chunks(each).flat_map(zstd).collect()
    }

    /// A skippable frame carrying `payload`, as the seekable format's jump
    /// table and `zstd:chunked`'s manifest are stored.
    fn skippable(payload: &[u8]) -> Vec<u8> {
        let mut frame = 0x184D_2A50u32.to_le_bytes().to_vec();
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    fn assert_spans_reproduce(index: &Index, blob: &[u8], data: &[u8]) {
        assert_eq!(index.uncompressed_len, data.len() as u64);
        let mut reassembled = Vec::new();
        for i in 0..index.checkpoints.len() {
            reassembled.extend_from_slice(&index.extract_span(blob, i).unwrap());
        }
        assert_eq!(reassembled, data);
    }

    #[test]
    fn every_span_resumes_to_the_reference_bytes() {
        let data = sample_data(2 << 20);
        let blob = gzip(&data);
        let index = Index::build(Flavor::Gzip, &blob, 128 << 10).unwrap();
        assert!(
            index.checkpoints.len() > 4,
            "expected several checkpoints, found {}",
            index.checkpoints.len()
        );
        assert!(index.checkpoints[1..].iter().any(|p| p.bits != 0));
        assert_spans_reproduce(&index, &blob, &data);
    }

    /// The buffer is reused between spans and only ever grows, so what a span
    /// reports is the only part of it that belongs to that span. A caller
    /// reading past it would be handed whatever the last, longer span left.
    #[test]
    fn a_span_owns_only_what_it_reports_writing() {
        let data = sample_data(2 << 20);
        let blob = gzip(&data);
        let index = Index::build(Flavor::Gzip, &blob, 128 << 10).unwrap();

        let mut buffer = vec![0xab; 4 << 20];
        let stale = buffer.len();
        let written = index
            .extract_span_into(&blob, 1, &mut buffer, 0, &mut Decoders::default())
            .expect("span");

        let start = index.checkpoints[1].out_offset as usize;
        assert_eq!(&buffer[..written], &data[start..start + written]);
        assert_eq!(buffer.len(), stale, "a buffer this size needs no growing");
        assert!(
            buffer[written..].iter().all(|&byte| byte == 0xab),
            "nothing past the span may be touched"
        );
    }

    #[test]
    fn a_second_gzip_member_gets_its_own_checkpoint() {
        let first = sample_data(300 << 10);
        let second = sample_data(200 << 10);
        let mut blob = gzip(&first);
        let member_start = blob.len() as u64;
        blob.extend_from_slice(&gzip(&second));
        let mut data = first;
        data.extend_from_slice(&second);

        let index = Index::build(Flavor::Gzip, &blob, 100 << 10).unwrap();
        assert!(
            index
                .checkpoints
                .iter()
                .any(|p| p.in_offset == member_start && p.window.is_empty() && p.bits == 0),
            "no checkpoint at the member boundary"
        );
        assert_spans_reproduce(&index, &blob, &data);
    }

    #[test]
    fn a_blob_smaller_than_a_span_still_round_trips() {
        let data = b"tiny".to_vec();
        let blob = gzip(&data);
        let index = Index::build(Flavor::Gzip, &blob, 4 << 20).unwrap();
        assert_eq!(index.checkpoints.len(), 1);
        assert_spans_reproduce(&index, &blob, &data);
    }

    #[test]
    fn the_index_survives_serialisation() {
        let data = sample_data(1 << 20);
        let blob = gzip(&data);
        let index = Index::build(Flavor::Gzip, &blob, 128 << 10).unwrap();
        let mut bytes = Vec::new();
        index.write_to(&mut bytes).unwrap();
        let read = Index::read_from(&bytes[..]).unwrap();
        assert_eq!(read, index);
        assert_spans_reproduce(&read, &blob, &data);
    }

    #[test]
    fn a_truncated_blob_is_an_error_not_a_hang() {
        let blob = gzip(&sample_data(1 << 20));
        assert!(Index::build(Flavor::Gzip, &blob[..blob.len() / 2], 128 << 10).is_err());
    }

    #[test]
    fn garbage_input_is_rejected() {
        assert!(Index::build(Flavor::Gzip, &sample_data(4096), 1 << 10).is_err());
        assert!(Index::read_from(&b"not an index"[..]).is_err());
    }

    #[test]
    fn a_tampered_window_is_caught_by_the_span_checksum() {
        let data = sample_data(1 << 20);
        let blob = gzip(&data);
        let mut index = Index::build(Flavor::Gzip, &blob, 128 << 10).unwrap();
        let point = index
            .checkpoints
            .iter_mut()
            .find(|p| !p.window.is_empty())
            .unwrap();
        // Any back-reference the resumed span makes then reads wrong history.
        for byte in &mut point.window {
            *byte ^= 0xFF;
        }
        let i = index
            .checkpoints
            .iter()
            .position(|p| !p.window.is_empty())
            .unwrap();
        // The bytes inflate fine; only the checksum can tell they are wrong.
        let err = index.extract_span(&blob, i).unwrap_err();
        assert!(err.to_string().contains("checksum"), "got: {err}");
    }

    #[test]
    fn out_of_order_checkpoints_are_rejected_on_read() {
        let data = sample_data(1 << 20);
        let blob = gzip(&data);
        let mut index = Index::build(Flavor::Gzip, &blob, 128 << 10).unwrap();
        assert!(index.checkpoints.len() >= 3);
        index.checkpoints.swap(1, 2);
        let mut bytes = Vec::new();
        index.write_to(&mut bytes).unwrap();
        assert!(Index::read_from(&bytes[..]).is_err());
    }

    #[test]
    fn every_zstd_frame_boundary_can_start_a_span() {
        let data = sample_data(2 << 20);
        let blob = zstd_framed(&data, 64 << 10);
        let index = Index::build(Flavor::Zstd, &blob, 128 << 10).unwrap();
        assert!(
            index.checkpoints.len() > 4,
            "expected several checkpoints, found {}",
            index.checkpoints.len()
        );
        assert!(
            index
                .checkpoints
                .iter()
                .all(|p| p.window.is_empty() && p.bits == 0),
            "a frame start resumes from no state at all"
        );
        assert_spans_reproduce(&index, &blob, &data);
    }

    /// Frames far smaller than the span are gathered into one, or a
    /// `zstd:chunked` layer would be more queue than work.
    #[test]
    fn small_zstd_frames_are_gathered_into_spans() {
        let data = sample_data(1 << 20);
        let blob = zstd_framed(&data, 4 << 10);
        let index = Index::build(Flavor::Zstd, &blob, 256 << 10).unwrap();
        assert!(
            (4..=6).contains(&index.checkpoints.len()),
            "expected roughly one checkpoint per span, found {}",
            index.checkpoints.len()
        );
        assert_spans_reproduce(&index, &blob, &data);
    }

    /// What `bsdtar` and the `zstd` tool write. There is nowhere to resume, so
    /// the layer is one span; it must still index and extract.
    #[test]
    fn a_single_frame_zstd_blob_indexes_as_one_span() {
        let data = sample_data(1 << 20);
        let blob = zstd(&data);
        let index = Index::build(Flavor::Zstd, &blob, 128 << 10).unwrap();
        assert_eq!(index.checkpoints.len(), 1);
        assert_spans_reproduce(&index, &blob, &data);
    }

    #[test]
    fn skippable_frames_are_stepped_over() {
        let data = sample_data(1 << 20);
        let mut blob = skippable(b"a leading identifier");
        blob.extend_from_slice(&zstd_framed(&data, 64 << 10));
        blob.extend_from_slice(&skippable(&[0u8; 512]));

        let index = Index::build(Flavor::Zstd, &blob, 128 << 10).unwrap();
        assert!(index.checkpoints.len() > 2);
        assert_spans_reproduce(&index, &blob, &data);
    }

    #[test]
    fn the_zstd_index_survives_serialisation() {
        let data = sample_data(1 << 20);
        let blob = zstd_framed(&data, 64 << 10);
        let index = Index::build(Flavor::Zstd, &blob, 128 << 10).unwrap();
        let mut bytes = Vec::new();
        index.write_to(&mut bytes).unwrap();
        let read = Index::read_from(&bytes[..]).unwrap();
        assert_eq!(read.flavor, Flavor::Zstd);
        assert_eq!(read, index);
        assert_spans_reproduce(&read, &blob, &data);
    }

    /// The two formats resume from entirely different state, so an index has
    /// to say which it describes rather than leaving it to the file name.
    #[test]
    fn an_index_records_which_format_it_describes() {
        let data = sample_data(256 << 10);
        let mut bytes = Vec::new();
        Index::build(Flavor::Gzip, &gzip(&data), 64 << 10)
            .unwrap()
            .write_to(&mut bytes)
            .unwrap();
        assert_eq!(Index::read_from(&bytes[..]).unwrap().flavor, Flavor::Gzip);

        // A gzip window in a zstd index would be read as a frame start.
        bytes[4] = Flavor::Zstd.code();
        assert!(Index::read_from(&bytes[..]).is_err());
    }

    #[test]
    fn a_truncated_zstd_blob_is_an_error_not_a_hang() {
        let blob = zstd(&sample_data(1 << 20));
        assert!(Index::build(Flavor::Zstd, &blob[..blob.len() / 2], 128 << 10).is_err());
    }

    #[test]
    fn a_zstd_span_that_decodes_too_far_is_refused() {
        let data = sample_data(1 << 20);
        let blob = zstd_framed(&data, 64 << 10);
        let mut index = Index::build(Flavor::Zstd, &blob, 128 << 10).unwrap();
        // Claim the first span ends earlier than the frames it covers do.
        index.checkpoints[1].out_offset -= 1024;
        let err = index.extract_span(&blob, 0).unwrap_err();
        assert!(
            err.to_string().contains("more than the index"),
            "got: {err}"
        );
    }
}
