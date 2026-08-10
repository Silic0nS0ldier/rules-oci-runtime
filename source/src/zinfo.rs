//! Checkpoint index for gzip layer blobs, in the spirit of zlib's `zran.c`
//! and SOCI's zTOC. A checkpoint records enough inflate state (bit offset and
//! 32 KiB window) to resume decompression at a deflate block boundary, which
//! lets independent threads inflate disjoint spans of one gzip member.
//!
//! Built at Bazel build time by `oci_runtime index`; the image blobs are never
//! modified. Each span also carries a CRC-32 of its uncompressed bytes: blob
//! digests only cover the compressed bytes, so a corrupt index would otherwise
//! corrupt the rootfs silently.

use std::io::{self, Read, Write};

use crate::error::{Error, Result};

const MAGIC: &[u8; 4] = b"OZI1";
const WINDOW_SIZE: usize = 32 * 1024;
/// Checkpoints are only useful while spans decompress in bounded memory.
const MAX_CHECKPOINTS: u32 = 1 << 20;

/// Resume point at a deflate block boundary (or a gzip member start).
#[derive(Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// Offset of the first compressed byte not yet consumed at the boundary.
    pub in_offset: u64,
    /// Unconsumed high bits of the byte at `in_offset - 1`; 0 when byte aligned.
    pub bits: u8,
    /// Uncompressed offset this checkpoint resumes at.
    pub out_offset: u64,
    /// Sliding window at the boundary; empty at a stream or member start,
    /// where inflate instead parses the gzip header.
    pub window: Vec<u8>,
    /// CRC-32 of the uncompressed span from here to the next checkpoint.
    pub crc: u32,
}

/// A blob's checkpoints, always starting with the stream start, so that the
/// spans between consecutive checkpoints cover the whole uncompressed output.
#[derive(Debug, PartialEq, Eq)]
pub struct Index {
    pub uncompressed_len: u64,
    pub checkpoints: Vec<Checkpoint>,
}

impl Index {
    /// Decompresses `blob` once, dropping a checkpoint at the first block
    /// boundary after every `span` uncompressed bytes.
    pub fn build(blob: &[u8], span: u64) -> Result<Self> {
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
            uncompressed_len: out_pos,
            checkpoints,
        })
    }

    /// Inflates the span between checkpoint `i` and its successor (or the end
    /// of the stream) and verifies it against the recorded CRC.
    pub fn extract_span(&self, blob: &[u8], i: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.extract_span_into(blob, i, &mut out, 0)?;
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

        // An empty window is a stream or member start: parse the header there.
        let at_header = point.window.is_empty() && point.bits == 0;
        let mut inflater = Inflater::new(if at_header {
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

        if out.len() < at + len {
            out.resize(at + len, 0);
        }
        let span = &mut out[at..at + len];
        let mut in_pos = point.in_offset as usize;
        let mut filled = 0usize;
        while filled < len {
            let (consumed, produced, ret) = inflater
                .inflate(&blob[in_pos..], &mut span[filled..], Flush::None)
                .map_err(|e| Error::io(context, e))?;
            in_pos += consumed;
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
                    inflater =
                        Inflater::new(HEADER_WINDOW_BITS).map_err(|e| Error::io(context, e))?;
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

        let mut crc = flate2::Crc::new();
        crc.update(span);
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
            uncompressed_len,
            checkpoints,
        })
    }
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
        let index = Index::build(&blob, 128 << 10).unwrap();
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
        let index = Index::build(&blob, 128 << 10).unwrap();

        let mut buffer = vec![0xab; 4 << 20];
        let stale = buffer.len();
        let written = index
            .extract_span_into(&blob, 1, &mut buffer, 0)
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
    fn a_second_gzip_member_gets_its_own_checkpoint() {        let first = sample_data(300 << 10);
        let second = sample_data(200 << 10);
        let mut blob = gzip(&first);
        let member_start = blob.len() as u64;
        blob.extend_from_slice(&gzip(&second));
        let mut data = first;
        data.extend_from_slice(&second);

        let index = Index::build(&blob, 100 << 10).unwrap();
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
        let index = Index::build(&blob, 4 << 20).unwrap();
        assert_eq!(index.checkpoints.len(), 1);
        assert_spans_reproduce(&index, &blob, &data);
    }

    #[test]
    fn the_index_survives_serialisation() {
        let data = sample_data(1 << 20);
        let blob = gzip(&data);
        let index = Index::build(&blob, 128 << 10).unwrap();
        let mut bytes = Vec::new();
        index.write_to(&mut bytes).unwrap();
        let read = Index::read_from(&bytes[..]).unwrap();
        assert_eq!(read, index);
        assert_spans_reproduce(&read, &blob, &data);
    }

    #[test]
    fn a_truncated_blob_is_an_error_not_a_hang() {
        let blob = gzip(&sample_data(1 << 20));
        assert!(Index::build(&blob[..blob.len() / 2], 128 << 10).is_err());
    }

    #[test]
    fn garbage_input_is_rejected() {
        assert!(Index::build(&sample_data(4096), 1 << 10).is_err());
        assert!(Index::read_from(&b"not an index"[..]).is_err());
    }

    #[test]
    fn a_tampered_window_is_caught_by_the_span_checksum() {
        let data = sample_data(1 << 20);
        let blob = gzip(&data);
        let mut index = Index::build(&blob, 128 << 10).unwrap();
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
        let mut index = Index::build(&blob, 128 << 10).unwrap();
        assert!(index.checkpoints.len() >= 3);
        index.checkpoints.swap(1, 2);
        let mut bytes = Vec::new();
        index.write_to(&mut bytes).unwrap();
        assert!(Index::read_from(&bytes[..]).is_err());
    }
}
