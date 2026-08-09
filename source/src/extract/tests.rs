//! Tests for layer extraction.
//!
//! These drive whole layers through the extractor rather than any one of its
//! pieces, so they live beside the module rather than inside it.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};
use tar::EntryType;

use crate::error::{Error, Result};
use crate::fsutil;
use crate::image::{Descriptor, Layout, hex_encode, parse_digest};
use crate::zinfo;

use super::RootfsExtractor;
use super::entry::{OPAQUE_WHITEOUT, WHITEOUT_PREFIX, is_supported};
use super::pipeline::{
    CHUNK_BYTES, Chunk, ChunkReader, PIPELINE_DEPTH, buffer_pool, compression_of, read_and_hash,
};
use std::sync::mpsc::sync_channel;

use super::*;

#[test]
fn known_layer_media_types_map_to_compression() {
    assert_eq!(
        compression_of("application/vnd.oci.image.layer.v1.tar+gzip"),
        Some(Compression::Gzip)
    );
    assert_eq!(
        compression_of("application/vnd.oci.image.layer.v1.tar+zstd"),
        Some(Compression::Zstd)
    );
    assert_eq!(
        compression_of("application/vnd.oci.image.layer.v1.tar"),
        Some(Compression::None)
    );
    assert_eq!(
        compression_of("application/vnd.docker.image.rootfs.diff.tar.gzip"),
        Some(Compression::Gzip)
    );
    assert_eq!(compression_of("application/vnd.oci.image.config.v1+json"), None);
}

#[test]
fn device_entries_are_not_extracted() {
    assert!(!is_supported(EntryType::Char));
    assert!(!is_supported(EntryType::Block));
    assert!(!is_supported(EntryType::Fifo));
    assert!(is_supported(EntryType::Regular));
    assert!(is_supported(EntryType::Symlink));
    assert!(is_supported(EntryType::Link));
    assert!(is_supported(EntryType::Directory));
}

#[test]
fn the_hashing_thread_sees_every_byte_it_hands_on() {
    let mut blob = scratch("hashing");
    blob.push("blob");
    fs::write(&blob, b"hello").expect("blob");

    let (sender, receiver) = sync_channel(PIPELINE_DEPTH);
    let (pool, ret) = buffer_pool();
    let state = read_and_hash(fs::File::open(&blob).expect("open"), sender, pool);

    let mut passed_on = Vec::new();
    ChunkReader::new(receiver, Some(ret))
        .read_to_end(&mut passed_on)
        .expect("read");
    assert_eq!(passed_on, b"hello");
    assert_eq!(state.bytes, 5);
    assert_eq!(
        hex_encode(&state.hasher.finalize()),
        hex_encode(&Sha256::digest(b"hello"))
    );
}

/// A recycled buffer still holds the previous chunk's bytes, so the length
/// travelling with it is the only thing keeping them out of the stream.
#[test]
fn recycled_buffers_do_not_leak_the_previous_chunk() {
    let (sender, receiver) = sync_channel(PIPELINE_DEPTH);
    let (pool, ret) = buffer_pool();

    let mut first = pool.take();
    first[..4].copy_from_slice(b"aaaa");
    sender.send(Ok(Chunk { buf: first, len: 4 })).expect("send");
    let mut second = pool.take();
    second[..2].copy_from_slice(b"bb");
    sender
        .send(Ok(Chunk {
            buf: second,
            len: 2,
        }))
        .expect("send");
    drop(sender);

    let mut streamed = Vec::new();
    ChunkReader::new(receiver, Some(ret))
        .read_to_end(&mut streamed)
        .expect("read");
    assert_eq!(streamed, b"aaaabb");

    // Draining a chunk hands its buffer back with the stale tail intact.
    let recycled = pool.take();
    assert_eq!(&recycled[..4], b"aaaa");
}

#[test]
fn whiteout_names_are_recognised() {
    assert!(OPAQUE_WHITEOUT.starts_with(WHITEOUT_PREFIX));
    assert_eq!(".wh.foo".strip_prefix(WHITEOUT_PREFIX), Some("foo"));
    assert_eq!("foo".strip_prefix(WHITEOUT_PREFIX), None);
}

const GZIP_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const PLAIN_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";

fn scratch(name: &str) -> Utf8PathBuf {
    let dir = Utf8PathBuf::from(std::env::temp_dir().to_string_lossy().into_owned())
        .join(format!("oci-runtime-extract-{name}-{}", std::process::id()));
    let _ = fsutil::force_remove_dir_all(dir.as_std_path());
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A tar holding a directory, a file spanning several pipeline chunks, a
/// small file and a symlink.
fn sample_tar() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Directory);
    header.set_mode(0o755);
    header.set_size(0);
    builder
        .append_data(&mut header, "dir/", io::empty())
        .expect("dir");

    let large = vec![b'a'; CHUNK_BYTES * 2 + 17];
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(large.len() as u64);
    builder
        .append_data(&mut header, "dir/large", &large[..])
        .expect("large");

    let mut header = tar::Header::new_gnu();
    header.set_mode(0o600);
    header.set_size(5);
    builder
        .append_data(&mut header, "dir/small", &b"hello"[..])
        .expect("small");

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Symlink);
    header.set_mode(0o777);
    header.set_size(0);
    builder
        .append_link(&mut header, "link", "dir/small")
        .expect("link");

    builder.into_inner().expect("tar")
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).expect("compress");
    encoder.finish().expect("compress")
}

/// Writes a blob and returns a descriptor that matches it.
fn install_blob(root: &Utf8Path, media_type: &str, blob: &[u8]) -> Descriptor {
    let hex = hex_encode(&Sha256::digest(blob));
    let blobs = root.join("blobs").join("sha256");
    fs::create_dir_all(&blobs).expect("blobs");
    fs::write(blobs.join(&hex), blob).expect("blob");
    fs::write(
        root.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .expect("layout");
    fs::write(root.join("index.json"), br#"{"manifests":[]}"#).expect("index");
    Descriptor {
        media_type: media_type.to_string(),
        digest: format!("sha256:{hex}"),
        size: blob.len() as u64,
        platform: None,
    }
}

fn extract(root: &Utf8Path, descriptor: &Descriptor) -> Result<Utf8PathBuf> {
    extract_indexed(root, descriptor, None)
}

fn extract_indexed(
    root: &Utf8Path,
    descriptor: &Descriptor,
    index_dir: Option<&Utf8Path>,
) -> Result<Utf8PathBuf> {
    let layout = Layout::open(root)?;
    let rootfs = root.join("rootfs");
    let mut extractor = RootfsExtractor::new(&rootfs, index_dir)?;
    extractor.apply_layer(&layout, descriptor)?;
    extractor.finish()?;
    Ok(rootfs)
}

#[test]
fn layers_are_unpacked_while_they_are_inflated() {
    for (name, media_type, blob) in [
        ("gzip", GZIP_LAYER, gzip(&sample_tar())),
        ("plain", PLAIN_LAYER, sample_tar()),
    ] {
        let root = scratch(&format!("pipeline-{name}"));
        let descriptor = install_blob(&root, media_type, &blob);
        let rootfs = extract(&root, &descriptor).expect("extract");

        assert!(rootfs.join("dir").is_dir(), "{name}: directory");
        assert_eq!(
            fs::read(rootfs.join("dir/large")).expect("large").len(),
            CHUNK_BYTES * 2 + 17,
            "{name}: file spanning chunk boundaries"
        );
        assert_eq!(
            fs::read_to_string(rootfs.join("dir/small")).expect("small"),
            "hello",
            "{name}: small file"
        );
        assert_eq!(
            fs::read_link(rootfs.join("link")).expect("link"),
            Path::new("dir/small"),
            "{name}: symlink"
        );
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }
}

/// Symlinks, hard links and directories are placed by this module rather
/// than by `tar`, so each of their behaviours needs its own cover.
#[test]
fn link_entries_are_placed_with_their_metadata() {
    let root = scratch("link-entries");
    let mut builder = tar::Builder::new(Vec::new());

    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(5);
    header.set_mtime(1_000_000);
    builder
        .append_data(&mut header, "target", &b"hello"[..])
        .expect("file");

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Symlink);
    header.set_size(0);
    header.set_mtime(1_234_567);
    builder
        .append_link(&mut header, "sym", "target")
        .expect("symlink");

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Link);
    header.set_size(0);
    builder
        .append_link(&mut header, "hard", "target")
        .expect("hard link");

    let blob = builder.into_inner().expect("tar");
    let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
    let rootfs = extract(&root, &descriptor).expect("extract");

    assert_eq!(
        fs::read_link(rootfs.join("sym")).expect("symlink"),
        Path::new("target")
    );
    let metadata = fs::symlink_metadata(rootfs.join("sym")).expect("symlink metadata");
    assert_eq!(
        metadata
            .modified()
            .expect("mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_secs(),
        1_234_567,
        "the symlink itself carries the mtime, not what it points at"
    );
    assert_eq!(
        fs::read_to_string(rootfs.join("hard")).expect("hard link"),
        "hello"
    );

    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

#[test]
fn a_hard_link_cannot_name_a_target_outside_the_rootfs() {
    let root = scratch("hard-link-escape");
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Link);
    header.set_size(0);
    builder
        .append_link(&mut header, "stolen", "../../etc/passwd")
        .expect("hard link");
    let blob = builder.into_inner().expect("tar");

    let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
    let err = extract(&root, &descriptor).expect_err("the entry must be refused");
    assert!(
        matches!(err, Error::UnsafeEntry { .. }),
        "expected an unsafe entry, got {err:?}"
    );

    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

#[test]
fn a_link_entry_without_a_target_is_refused() {
    let root = scratch("empty-link");
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Symlink);
    header.set_size(0);
    header.set_path("sym").expect("path");
    header.set_link_name_literal("").expect("empty link name");
    header.set_cksum();
    builder.append(&header, &[][..]).expect("symlink");
    let blob = builder.into_inner().expect("tar");

    let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
    let err = extract(&root, &descriptor).expect_err("the entry must be refused");
    assert!(
        matches!(err, Error::UnsafeEntry { .. }),
        "expected an unsafe entry, got {err:?}"
    );

    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

#[test]
fn an_entry_cannot_escape_through_a_symlinked_parent() {        // A layer can ship a symlink out of the rootfs and then an entry
    // underneath it. The parent of that entry does not exist and cannot be
    // resolved, which used to count as safe, so creating it followed the
    // symlink and wrote the file outside the rootfs.
    let root = scratch("escape");
    let outside = scratch("escape-outside");

    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Symlink);
    header.set_mode(0o777);
    header.set_size(0);
    builder
        .append_link(&mut header, "lnk", outside.as_std_path())
        .expect("link");

    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(7);
    builder
        .append_data(&mut header, "lnk/sub/file", &b"escaped"[..])
        .expect("file");
    let blob = builder.into_inner().expect("tar");

    let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
    let err = extract(&root, &descriptor).expect_err("the entry must be refused");
    assert!(
        matches!(err, Error::UnsafeEntry { .. }),
        "expected an unsafe entry, got {err:?}"
    );
    assert!(
        fs::read_dir(outside.as_std_path())
            .expect("outside")
            .next()
            .is_none(),
        "nothing may be written outside the rootfs"
    );

    let _ = fsutil::force_remove_dir_all(root.as_std_path());
    let _ = fsutil::force_remove_dir_all(outside.as_std_path());
}

#[test]
fn an_opaque_whiteout_cannot_clear_a_directory_outside_the_rootfs() {
    // The marker names the directory it applies to, and a layer can point
    // that name at a symlink leading out of the rootfs. Reading through it
    // deleted everything at the far end.
    let root = scratch("opaque-escape");
    let outside = scratch("opaque-escape-outside");
    fs::write(outside.join("keep"), b"important").expect("keep");

    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Symlink);
    header.set_mode(0o777);
    header.set_size(0);
    builder
        .append_link(&mut header, "lnk", outside.as_std_path())
        .expect("link");

    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(0);
    builder
        .append_data(&mut header, "lnk/.wh..wh..opq", &b""[..])
        .expect("opaque");
    let blob = builder.into_inner().expect("tar");

    let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
    extract(&root, &descriptor).expect("extract");
    assert_eq!(
        fs::read(outside.join("keep")).expect("keep"),
        b"important",
        "files outside the rootfs may not be removed"
    );

    let _ = fsutil::force_remove_dir_all(root.as_std_path());
    let _ = fsutil::force_remove_dir_all(outside.as_std_path());
}

#[test]
fn a_verified_directory_replaced_by_a_symlink_is_checked_again() {
    // Parents are remembered once they have been checked, so a layer that
    // swaps a directory it has already used for a symlink out of the rootfs
    // must drop that memory again, or the entries after it are waved
    // through on the strength of a check that no longer holds.
    let root = scratch("stale");
    let outside = scratch("stale-outside");

    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(5);
    builder
        .append_data(&mut header, "a/b/first", &b"first"[..])
        .expect("first");

    // `a/b` is now a checked parent. Replace it with a way out.
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Symlink);
    header.set_mode(0o777);
    header.set_size(0);
    builder
        .append_link(&mut header, "a/b", outside.as_std_path())
        .expect("link");

    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(6);
    builder
        .append_data(&mut header, "a/b/second", &b"second"[..])
        .expect("second");
    let blob = builder.into_inner().expect("tar");

    let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
    let err = extract(&root, &descriptor).expect_err("the entry must be refused");
    assert!(
        matches!(err, Error::UnsafeEntry { .. }),
        "expected an unsafe entry, got {err:?}"
    );
    assert!(
        !outside.join("second").exists(),
        "nothing may be written outside the rootfs"
    );

    let _ = fsutil::force_remove_dir_all(root.as_std_path());
    let _ = fsutil::force_remove_dir_all(outside.as_std_path());
}

#[test]
fn file_modes_and_timestamps_survive_extraction() {
    // Unpacking regular files no longer goes through tar, so the parts of
    // its behaviour we still rely on are pinned here. A read-only mode is
    // the interesting case: the file is created with it and written after.
    let mut builder = tar::Builder::new(Vec::new());
    for (name, mode, mtime, contents) in [
        ("readonly", 0o400u32, 1_000_000_000u64, "secret"),
        ("program", 0o755, 1_234_567_890, "#!/bin/sh\n"),
        ("data", 0o644, 7, "plain"),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_mode(mode);
        header.set_mtime(mtime);
        header.set_size(contents.len() as u64);
        builder
            .append_data(&mut header, name, contents.as_bytes())
            .expect("entry");
    }
    let blob = builder.into_inner().expect("tar");

    let root = scratch("modes");
    let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
    let rootfs = extract(&root, &descriptor).expect("extract");

    for (name, mode, mtime, contents) in [
        ("readonly", 0o400u32, 1_000_000_000u64, "secret"),
        ("program", 0o755, 1_234_567_890, "#!/bin/sh\n"),
        ("data", 0o644, 7, "plain"),
    ] {
        let path = rootfs.join(name);
        assert_eq!(
            fs::read_to_string(&path).expect(name),
            contents,
            "{name}: contents"
        );
        let metadata = fs::metadata(&path).expect(name);
        assert_eq!(metadata.permissions().mode() & 0o7777, mode, "{name}: mode");
        let modified = metadata
            .modified()
            .expect(name)
            .duration_since(std::time::UNIX_EPOCH)
            .expect(name)
            .as_secs();
        assert_eq!(modified, mtime, "{name}: mtime");
    }
    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

#[test]
fn padding_after_the_tar_marker_is_still_verified() {
    // More padding than the pipeline can hold, so the blob is only read to
    // the end, and therefore only checked, because unpacking drains it.
    let mut tar = sample_tar();
    tar.extend_from_slice(&vec![0u8; PIPELINE_DEPTH * CHUNK_BYTES * 2]);
    let blob = gzip(&tar);

    let root = scratch("drain-ok");
    let descriptor = install_blob(&root, GZIP_LAYER, &blob);
    extract(&root, &descriptor).expect("a padded blob extracts");
    let _ = fsutil::force_remove_dir_all(root.as_std_path());

    let root = scratch("drain-short");
    let mut descriptor = install_blob(&root, GZIP_LAYER, &blob);
    descriptor.size -= 1;
    match extract(&root, &descriptor) {
        Err(Error::SizeMismatch { .. }) => {}
        other => panic!("expected a size mismatch, got {other:?}"),
    }
    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

#[test]
fn a_blob_that_does_not_match_its_digest_is_rejected() {
    let root = scratch("digest");
    let mut descriptor = install_blob(&root, GZIP_LAYER, &gzip(&sample_tar()));

    // Leave the blob where the descriptor says it is, but change what is in it.
    let mut altered = sample_tar();
    altered.extend_from_slice(&[0u8; 1024]);
    let altered = gzip(&altered);
    let path = Layout::open(&root)
        .expect("layout")
        .blob_path(&descriptor.digest)
        .expect("blob path");
    fs::write(&path, &altered).expect("blob");
    descriptor.size = altered.len() as u64;

    match extract(&root, &descriptor) {
        Err(Error::DigestMismatch { .. }) => {}
        other => panic!("expected a digest mismatch, got {other:?}"),
    }
    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

#[test]
fn a_blob_that_does_not_match_its_size_is_rejected() {
    let root = scratch("size");
    let mut descriptor = install_blob(&root, GZIP_LAYER, &gzip(&sample_tar()));
    descriptor.size += 1;
    match extract(&root, &descriptor) {
        Err(Error::SizeMismatch { .. }) => {}
        other => panic!("expected a size mismatch, got {other:?}"),
    }
    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

#[test]
fn a_corrupt_blob_is_reported_rather_than_hanging() {
    let root = scratch("corrupt");
    let mut blob = gzip(&sample_tar());
    let tail = blob.len() - 64;
    blob[tail..].fill(0xff);
    let descriptor = install_blob(&root, GZIP_LAYER, &blob);
    assert!(extract(&root, &descriptor).is_err());
    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

#[test]
fn an_unsupported_media_type_is_reported() {
    let root = scratch("media-type");
    let descriptor = install_blob(&root, "application/x-nonsense", &sample_tar());
    match extract(&root, &descriptor) {
        Err(Error::UnsupportedMediaType(_)) => {}
        other => panic!("expected an unsupported media type, got {other:?}"),
    }
    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

/// Compressible but non-repeating, so deflate emits many dynamic blocks
/// and a small span yields plenty of checkpoints.
fn random_bytes(len: usize) -> Vec<u8> {
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

/// A tar large enough for several spans: one incompressible file plus the
/// small entries the pipeline tests use.
fn multi_span_tar() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let random = random_bytes(1 << 20);
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(random.len() as u64);
    builder
        .append_data(&mut header, "random", &random[..])
        .expect("random");

    let mut header = tar::Header::new_gnu();
    header.set_mode(0o600);
    header.set_size(5);
    builder
        .append_data(&mut header, "small", &b"hello"[..])
        .expect("small");
    builder.into_inner().expect("tar")
}

/// Compresses at the default level; `gzip` above uses the fast level,
/// which emits too few deflate block boundaries to checkpoint.
fn gzip_default(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).expect("compress");
    encoder.finish().expect("compress")
}

/// Builds a checkpoint index for the blob and installs it where the
/// extractor looks, returning the index directory.
fn install_index(root: &Utf8Path, descriptor: &Descriptor, blob: &[u8]) -> Utf8PathBuf {
    let index = zinfo::Index::build(blob, 64 * 1024).expect("index");
    assert!(
        index.checkpoints.len() > 2,
        "the sample must produce several checkpoints, got {}",
        index.checkpoints.len()
    );
    let dir = root.join("indexes");
    fs::create_dir_all(&dir).expect("index dir");
    let hex = parse_digest(&descriptor.digest).expect("digest").hex;
    let mut bytes = Vec::new();
    index.write_to(&mut bytes).expect("serialise");
    fs::write(dir.join(format!("{hex}.zinfo")), bytes).expect("install index");
    dir
}

#[test]
fn an_indexed_layer_extracts_the_same_bytes() {
    let root = scratch("indexed");
    let tar = multi_span_tar();
    let blob = gzip_default(&tar);
    let descriptor = install_blob(&root, GZIP_LAYER, &blob);
    let dir = install_index(&root, &descriptor, &blob);

    let rootfs = extract_indexed(&root, &descriptor, Some(&dir)).expect("extract");
    assert_eq!(
        fs::read(rootfs.join("random")).expect("random"),
        random_bytes(1 << 20),
        "indexed extraction must reproduce the exact bytes"
    );
    assert_eq!(
        fs::read_to_string(rootfs.join("small")).expect("small"),
        "hello"
    );
    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

#[test]
fn a_layer_without_an_index_file_streams_as_before() {
    let root = scratch("indexed-absent");
    let blob = gzip_default(&multi_span_tar());
    let descriptor = install_blob(&root, GZIP_LAYER, &blob);
    let dir = root.join("indexes");
    fs::create_dir_all(&dir).expect("empty index dir");

    let rootfs = extract_indexed(&root, &descriptor, Some(&dir)).expect("extract");
    assert_eq!(
        fs::read_to_string(rootfs.join("small")).expect("small"),
        "hello"
    );
    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

#[test]
fn a_tampered_index_fails_the_extraction() {
    let root = scratch("indexed-tampered");
    let blob = gzip_default(&multi_span_tar());
    let descriptor = install_blob(&root, GZIP_LAYER, &blob);
    let dir = install_index(&root, &descriptor, &blob);

    // Flip the first checkpoint's span CRC (after magic, length and count).
    let hex = parse_digest(&descriptor.digest).expect("digest").hex;
    let path = dir.join(format!("{hex}.zinfo"));
    let mut bytes = fs::read(&path).expect("index");
    bytes[32] ^= 0xff;
    fs::write(&path, bytes).expect("index");

    let err = extract_indexed(&root, &descriptor, Some(&dir))
        .expect_err("a corrupt index must not extract");
    assert!(
        err.to_string().contains("span checksum"),
        "expected a span checksum failure, got {err}"
    );
    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

#[test]
fn an_index_for_another_blob_is_an_error_not_a_panic() {
    let root = scratch("indexed-stale");
    let blob = gzip_default(&multi_span_tar());
    let descriptor = install_blob(&root, GZIP_LAYER, &blob);

    // An index built from a different, shorter blob: checkpoints point
    // into compressed bytes that do not exist.
    let other = gzip_default(&random_bytes(512 * 1024));
    let index = zinfo::Index::build(&other, 64 * 1024).expect("index");
    let dir = root.join("indexes");
    fs::create_dir_all(&dir).expect("index dir");
    let hex = parse_digest(&descriptor.digest).expect("digest").hex;
    let mut bytes = Vec::new();
    index.write_to(&mut bytes).expect("serialise");
    fs::write(dir.join(format!("{hex}.zinfo")), bytes).expect("install index");

    assert!(extract_indexed(&root, &descriptor, Some(&dir)).is_err());
    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

#[test]
fn a_digest_mismatch_wins_over_index_errors() {
    let root = scratch("indexed-digest");
    let blob = gzip_default(&multi_span_tar());
    let descriptor = install_blob(&root, GZIP_LAYER, &blob);
    let dir = install_index(&root, &descriptor, &blob);

    // Replace the blob: the index no longer matches, but the reason is
    // that the blob is not what the descriptor promised.
    let altered = gzip_default(&random_bytes(1 << 20));
    let path = Layout::open(&root)
        .expect("layout")
        .blob_path(&descriptor.digest)
        .expect("blob path");
    fs::write(&path, &altered).expect("blob");
    let mut descriptor = descriptor;
    descriptor.size = altered.len() as u64;

    match extract_indexed(&root, &descriptor, Some(&dir)) {
        Err(Error::DigestMismatch { .. }) => {}
        other => panic!("expected a digest mismatch, got {other:?}"),
    }
    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}
