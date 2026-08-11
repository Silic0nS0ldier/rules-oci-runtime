//! Tests for layer extraction.
//!
//! These drive whole layers through the extractor rather than any one of its
//! pieces, so they live beside the module rather than inside it.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};
use tar::EntryType;

use crate::error::{Error, Result};
use crate::fsutil;
use crate::image::{Descriptor, Layout, hex_encode, parse_digest};
use crate::zinfo;

use super::RootfsExtractor;
use super::entry::is_supported;
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
    let mut extractor = RootfsExtractor::new(&rootfs, index_dir, false)?;
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

/// Applies several layers to one rootfs with the plan in play, the way a real
/// image runs, so that what the plan decides is checked against what the
/// extractor then does rather than on its own.
fn extract_planned(root: &Utf8Path, layers: &[Vec<u8>]) -> Result<Utf8PathBuf> {
    let index_dir = root.join("indexes");
    fs::create_dir_all(&index_dir).expect("index dir");

    let mut descriptors = Vec::new();
    for tar in layers {
        let descriptor = install_blob(root, PLAIN_LAYER, tar);
        let hex = parse_digest(&descriptor.digest).expect("digest").hex;
        let table = crate::entries::Table::build(&tar[..]).expect("entry table");
        let mut bytes = Vec::new();
        table.write_to(&mut bytes).expect("serialise");
        fs::write(index_dir.join(format!("{hex}.entries")), bytes).expect("install table");
        descriptors.push(descriptor);
    }
    // `install_blob` rewrites index.json each time; the layers live in the
    // manifest the caller passes to the extractor, not in the layout.
    let layout = Layout::open(root)?;
    let rootfs = root.join("rootfs");
    let mut extractor = RootfsExtractor::new(&rootfs, Some(&index_dir), false)?;
    extractor.plan(&descriptors)?;
    for descriptor in &descriptors {
        extractor.apply_layer(&layout, descriptor)?;
    }
    extractor.finish()?;
    Ok(rootfs)
}

fn tar_of(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    build(&mut builder);
    builder.into_inner().expect("tar")
}

fn append_dir(builder: &mut tar::Builder<Vec<u8>>, path: &str) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Directory);
    header.set_mode(0o755);
    header.set_size(0);
    builder.append_data(&mut header, path, io::empty()).expect("dir");
}

fn append_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, body: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(body.len() as u64);
    builder.append_data(&mut header, path, body).expect("file");
}

fn append_symlink(builder: &mut tar::Builder<Vec<u8>>, path: &str, target: &str) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Symlink);
    header.set_size(0);
    builder.append_link(&mut header, path, target).expect("symlink");
}

fn append_hard_link(builder: &mut tar::Builder<Vec<u8>>, path: &str, target: &str) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Link);
    header.set_size(0);
    builder.append_link(&mut header, path, target).expect("hard link");
}

fn append_file_moded(builder: &mut tar::Builder<Vec<u8>>, path: &str, body: &[u8], mode: u32) {
    let mut header = tar::Header::new_gnu();
    header.set_mode(mode);
    header.set_size(body.len() as u64);
    builder.append_data(&mut header, path, body).expect("file");
}

/// A hostile layer has to be written by hand: `tar::Header` will not spell a
/// path with `..` in it at all, and a real one is not built by this crate.
fn append_file_named(builder: &mut tar::Builder<Vec<u8>>, name: &[u8], body: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(body.len() as u64);
    header.as_gnu_mut().expect("gnu header").name[..name.len()].copy_from_slice(name);
    header.set_cksum();
    builder.append(&header, body).expect("file");
}

fn append_of_type(builder: &mut tar::Builder<Vec<u8>>, path: &str, entry_type: EntryType) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(entry_type);
    header.set_mode(0o644);
    header.set_size(0);
    builder.append_data(&mut header, path, io::empty()).expect("entry");
}

/// A later layer turning a directory into a symlink is the case the plan has
/// to get right: the files the earlier layer put in that directory are gone,
/// and writing them anyway would send them through the link to somewhere they
/// were never meant to be.
#[test]
fn a_directory_replaced_by_a_symlink_takes_its_contents_with_it() {
    let root = scratch("planned-dir-to-symlink");
    let rootfs = extract_planned(
        &root,
        &[
            tar_of(|b| {
                append_dir(b, "usr/");
                append_dir(b, "usr/lib/");
                append_file(b, "usr/lib/real", b"real");
                append_dir(b, "lib/");
                append_file(b, "lib/stale", b"stale");
            }),
            tar_of(|b| append_symlink(b, "lib", "usr/lib")),
        ],
    )
    .expect("extract");

    assert_eq!(
        fs::read_link(rootfs.join("lib")).expect("symlink"),
        Path::new("usr/lib"),
        "the link the last layer asked for is what stands"
    );
    assert!(
        !rootfs.join("usr/lib/stale").exists(),
        "the replaced directory's contents must not reappear through the link"
    );
    assert_eq!(
        fs::read_to_string(rootfs.join("usr/lib/real")).expect("real"),
        "real"
    );

    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

/// The reverse: a symlink already standing where a later layer ships a
/// directory keeps the link, so entries under it follow it.
#[test]
fn a_standing_symlink_survives_a_later_directory_entry() {
    let root = scratch("planned-symlink-kept");
    let rootfs = extract_planned(
        &root,
        &[
            tar_of(|b| {
                append_dir(b, "usr/");
                append_dir(b, "usr/lib/");
                append_symlink(b, "lib", "usr/lib");
            }),
            tar_of(|b| {
                append_dir(b, "lib/");
                append_file(b, "lib/through", b"through");
            }),
        ],
    )
    .expect("extract");

    assert!(rootfs.join("lib").is_symlink(), "the link has to survive");
    assert_eq!(
        fs::read_to_string(rootfs.join("usr/lib/through")).expect("through"),
        "through",
        "an entry under the link lands where the link points"
    );

    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

/// Planning must not lose a whiteout: the marker is a regular file entry, and
/// the tree it clears was built by layers the plan also resolved.
#[test]
fn a_planned_image_still_applies_its_whiteouts() {
    let root = scratch("planned-whiteout");
    let rootfs = extract_planned(
        &root,
        &[
            tar_of(|b| {
                append_dir(b, "d/");
                append_file(b, "d/gone", b"gone");
                append_file(b, "d/kept", b"kept");
            }),
            tar_of(|b| append_file(b, "d/.wh.gone", b"")),
        ],
    )
    .expect("extract");

    assert!(!rootfs.join("d/gone").exists());
    assert_eq!(
        fs::read_to_string(rootfs.join("d/kept")).expect("kept"),
        "kept"
    );

    let _ = fsutil::force_remove_dir_all(root.as_std_path());
}

/// The ways a layer set can reach the rootfs. Which one runs is decided by
/// what sidecars sit beside the blobs, so a fixture can be put through all
/// three and the results compared.
#[derive(Clone, Copy, Debug)]
enum Route {
    /// No sidecars: every layer is walked and applied in turn.
    Streaming,
    /// Entry tables only: the plan resolves the tree, but without a
    /// checkpoint index the layers are still walked.
    Planned,
    /// Entry tables and checkpoint indexes: the plan becomes a queue of spans
    /// extracted in parallel.
    Spans,
}

impl Route {
    const ALL: [Route; 3] = [Route::Streaming, Route::Planned, Route::Spans];
}

/// Installs `layers` as blobs, with exactly the sidecars `route` reads.
fn install_for(
    route: Route,
    root: &Utf8Path,
    layers: &[Vec<u8>],
) -> (Utf8PathBuf, Vec<Descriptor>) {
    let index_dir = root.join("indexes");
    fs::create_dir_all(&index_dir).expect("index dir");

    let mut descriptors = Vec::new();
    for tar in layers {
        let blob = gzip_default(tar);
        let descriptor = install_blob(root, GZIP_LAYER, &blob);
        let hex = parse_digest(&descriptor.digest).expect("digest").hex;

        if matches!(route, Route::Planned | Route::Spans) {
            let table = crate::entries::Table::build(&tar[..]).expect("entry table");
            let mut bytes = Vec::new();
            table.write_to(&mut bytes).expect("serialise");
            fs::write(index_dir.join(format!("{hex}.entries")), bytes).expect("install table");
        }
        if matches!(route, Route::Spans) {
            let index = zinfo::Index::build(&blob, 64 * 1024).expect("index");
            let mut bytes = Vec::new();
            index.write_to(&mut bytes).expect("serialise");
            fs::write(index_dir.join(format!("{hex}.zinfo")), bytes).expect("install index");
        }
        descriptors.push(descriptor);
    }
    (index_dir, descriptors)
}

/// Applies `layers` by the given route, installing exactly the sidecars that
/// route needs.
fn extract_by(
    route: Route,
    root: &Utf8Path,
    layers: &[Vec<u8>],
    strict_xattrs: bool,
) -> Result<Utf8PathBuf> {
    let (index_dir, descriptors) = install_for(route, root, layers);

    // `install_blob` rewrites index.json each time, so the layout is opened
    // once every blob is in place.
    let layout = Layout::open(root)?;
    let rootfs = root.join("rootfs");
    let dir = match route {
        Route::Streaming => None,
        Route::Planned | Route::Spans => Some(index_dir.as_path()),
    };
    let mut extractor = RootfsExtractor::new(&rootfs, dir, strict_xattrs)?;
    extractor.plan(&descriptors)?;

    // A route that quietly falls back to another one would compare equal for
    // the wrong reason, so each is held to the plan it is supposed to produce.
    match route {
        Route::Streaming => assert!(
            !extractor.plan.is_resolved(),
            "streaming must not resolve a plan"
        ),
        Route::Planned => {
            assert!(extractor.plan.is_resolved(), "the plan must resolve");
            assert!(
                descriptors.iter().all(|d| index_at(&index_dir, d).is_none()),
                "the planned route must have no checkpoint index to fall back on"
            );
        }
        Route::Spans => {
            assert!(extractor.plan.work().is_some(), "the plan must produce work");
            assert!(
                descriptors.iter().all(|d| index_at(&index_dir, d).is_some()),
                "every layer needs a checkpoint index for the span route"
            );
        }
    }

    extractor.apply(&layout, &descriptors)?;
    extractor.finish()?;
    Ok(rootfs)
}

/// A rootfs reduced to sorted lines, so two of them can be compared and the
/// first difference read directly.
///
/// Directory mtimes are left out: nothing sets them, so they hold whenever the
/// directory happened to be created.
fn snapshot(rootfs: &Utf8Path) -> Vec<String> {
    let mut lines = Vec::new();
    let mut inodes = BTreeMap::new();
    walk(rootfs, rootfs, &mut lines, &mut inodes);
    lines.sort();
    lines
}

fn walk(
    root: &Utf8Path,
    dir: &Utf8Path,
    lines: &mut Vec<String>,
    inodes: &mut BTreeMap<(u64, u64), String>,
) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|entry| entry.expect("entry"))
        .collect();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = Utf8PathBuf::from_path_buf(entry.path()).expect("utf8 path");
        let at = path.strip_prefix(root).expect("relative").to_string();
        let meta = fs::symlink_metadata(&path).expect("stat");
        let mode = meta.permissions().mode() & 0o7777;

        if meta.is_symlink() {
            let target = fs::read_link(&path).expect("readlink");
            lines.push(format!("link {at} -> {}", target.display()));
        } else if meta.is_dir() {
            lines.push(format!("dir  {at} mode={mode:04o}"));
            walk(root, &path, lines, inodes);
        } else {
            let body = fs::read(&path).expect("read");
            let digest = hex_encode(&Sha256::digest(&body));
            // Hard links are the same inode under two names, which only shows
            // up by pairing the names that share one.
            let shared = match inodes.get(&(meta.dev(), meta.ino())) {
                Some(first) => format!(" linked-to={first}"),
                None => {
                    inodes.insert((meta.dev(), meta.ino()), at.clone());
                    String::new()
                }
            };
            lines.push(format!(
                "file {at} mode={mode:04o} size={} mtime={} sha={}{shared}",
                body.len(),
                meta.mtime(),
                &digest[..16],
            ));
        }
    }
}

/// Applies the layers by each of `routes` and fails on the first pair that
/// differ.
///
/// The routes share no code past the plan, so agreement between them is the
/// only thing keeping the fast one honest. Which routes apply is stated by the
/// caller rather than discovered, so a fixture the span route declines says so
/// instead of quietly comparing one route against itself.
fn assert_routes_agree(name: &str, routes: &[Route], layers: &[Vec<u8>]) {
    let mut baseline: Option<(Route, Vec<String>)> = None;

    for &route in routes {
        let root = scratch(&format!("{name}-{route:?}"));
        let rootfs = extract_by(route, &root, layers, true)
            .unwrap_or_else(|err| panic!("{name}: {route:?} failed to extract: {err}"));
        let tree = snapshot(&rootfs);

        match &baseline {
            None => baseline = Some((route, tree)),
            Some((first, expected)) => assert_eq!(
                *expected, tree,
                "{name}: {first:?} and {route:?} produced different trees"
            ),
        }

        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }
}

/// Applies `layers` by every route and fails unless each of them refuses the
/// image.
///
/// No route is held to a plan shape here: an image the plan will not resolve
/// falls back to the walk, and being refused there is the point.
fn assert_routes_refuse(name: &str, layers: &[Vec<u8>]) {
    for route in Route::ALL {
        let root = scratch(&format!("{name}-{route:?}"));
        let (index_dir, descriptors) = install_for(route, &root, layers);
        let layout = Layout::open(&root).expect("layout");
        let rootfs = root.join("rootfs");
        let dir = match route {
            Route::Streaming => None,
            Route::Planned | Route::Spans => Some(index_dir.as_path()),
        };

        let mut extractor = RootfsExtractor::new(&rootfs, dir, true).expect("extractor");
        let err = extractor
            .plan(&descriptors)
            .and_then(|()| extractor.apply(&layout, &descriptors))
            .expect_err(&format!("{name}: {route:?} extracted what it must refuse"));
        assert!(
            matches!(err, Error::UnsafeEntry { .. }),
            "{name}: {route:?} expected an unsafe entry, got {err:?}"
        );
        assert!(
            !root.join("escaped").exists(),
            "{name}: {route:?} wrote outside the rootfs"
        );

        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }
}

/// The sidecars describe the same hostile layer, and the routes that read them
/// join the paths those sidecars hold straight onto the rootfs.
#[test]
fn no_route_places_an_entry_that_climbs_out_of_the_rootfs() {
    assert_routes_refuse(
        "route-escape",
        &[tar_of(|b| {
            append_dir(b, "etc/");
            append_file_named(b, b"etc/../../escaped", b"nope");
        })],
    );
}

#[test]
fn no_route_places_a_hard_link_that_climbs_out_of_the_rootfs() {
    assert_routes_refuse(
        "route-hard-link-escape",
        &[tar_of(|b| {
            append_file(b, "keep", b"keep");
            append_hard_link(b, "stolen", "../../etc/passwd");
        })],
    );
}

/// One file spelled two ways is one file. A route that thought otherwise
/// would place both copies, and the span route would then collide on a path
/// the plan promised it owned.
#[test]
fn every_route_agrees_when_layers_spell_one_path_differently() {
    assert_routes_agree(
        "route-spelling",
        &Route::ALL,
        &[
            tar_of(|b| {
                append_dir(b, "./etc/");
                append_file(b, "./etc/config", b"first");
            }),
            tar_of(|b| {
                append_dir(b, "etc/");
                append_file(b, "etc/config", b"second");
            }),
        ],
    );
}

#[test]
fn every_route_agrees_on_a_layered_image() {    assert_routes_agree(
        "route-layered",
        &Route::ALL,
        &[
            tar_of(|b| {
                append_dir(b, "etc/");
                append_file(b, "etc/config", b"first");
                append_dir(b, "bin/");
                append_file(b, "bin/tool", b"tool");
                append_symlink(b, "bin/tool-link", "tool");
            }),
            tar_of(|b| {
                append_file(b, "etc/config", b"second");
                append_file(b, "etc/added", b"added");
            }),
            tar_of(|b| {
                append_file(b, "etc/.wh.added", b"");
                append_file(b, "bin/other", b"other");
            }),
        ],
    );
}

#[test]
fn every_route_agrees_on_hard_links() {
    assert_routes_agree(
        "route-hard-links",
        &Route::ALL,
        &[
            tar_of(|b| {
                append_dir(b, "d/");
                append_file(b, "d/original", b"body");
                append_hard_link(b, "d/same", "d/original");
            }),
            tar_of(|b| append_hard_link(b, "d/later", "d/original")),
        ],
    );
}

#[test]
fn every_route_agrees_when_a_directory_becomes_a_symlink() {
    assert_routes_agree(
        "route-dir-to-symlink",
        &Route::ALL,
        &[
            tar_of(|b| {
                append_dir(b, "usr/");
                append_dir(b, "usr/lib/");
                append_file(b, "usr/lib/real", b"real");
                append_dir(b, "lib/");
                append_file(b, "lib/shadowed", b"shadowed");
            }),
            tar_of(|b| append_symlink(b, "lib", "usr/lib")),
        ],
    );
}

#[test]
fn every_route_agrees_when_a_layer_writes_through_a_symlink() {
    // The plan declines the span route outright when an entry sits behind a
    // symlink, since it cannot say where the entry lands without building the
    // tree to find out.
    assert_routes_agree(
        "route-through-symlink",
        &[Route::Streaming, Route::Planned],
        &[
            tar_of(|b| {
                append_dir(b, "usr/");
                append_dir(b, "usr/lib/");
                append_symlink(b, "lib", "usr/lib");
            }),
            tar_of(|b| {
                append_dir(b, "lib/");
                append_file(b, "lib/through", b"through");
            }),
        ],
    );
}


/// An opaque whiteout hides what is *in* a directory. The directory itself
/// stays, and a route that resolves the tree up front has to keep it too.
#[test]
fn every_route_agrees_on_an_opaque_whiteout() {
    assert_routes_agree(
        "route-opaque",
        &Route::ALL,
        &[
            tar_of(|b| {
                append_dir(b, "d/");
                append_file(b, "d/one", b"one");
                append_dir(b, "d/sub/");
                append_file(b, "d/sub/two", b"two");
            }),
            tar_of(|b| {
                append_dir(b, "d/");
                append_file(b, "d/.wh..wh..opq", b"");
            }),
        ],
    );
}

/// The marker names the rootfs itself, which is not a path under it, so the
/// plan had nothing to clear from and kept what the walk removed.
#[test]
fn every_route_agrees_on_an_opaque_whiteout_at_the_top_of_a_layer() {
    assert_routes_agree(
        "route-opaque-root",
        &Route::ALL,
        &[
            tar_of(|b| {
                append_dir(b, "etc/");
                append_file(b, "etc/gone", b"gone");
                append_file(b, "gone", b"gone");
            }),
            tar_of(|b| {
                append_file(b, ".wh..wh..opq", b"");
                append_file(b, "kept", b"kept");
            }),
        ],
    );
}

/// Runs the fixture by each route and hands the resulting rootfs to `check`.
///
/// Routes agreeing with each other is not the same as either being right, so a
/// conformance case says what the tree must hold rather than only that the
/// routes match.
fn for_each_route(
    name: &str,
    routes: &[Route],
    layers: &[Vec<u8>],
    check: impl Fn(Route, &Utf8Path),
) {
    for &route in routes {
        let root = scratch(&format!("{name}-{route:?}"));
        let rootfs = extract_by(route, &root, layers, true)
            .unwrap_or_else(|err| panic!("{name}: {route:?} failed to extract: {err}"));
        check(route, &rootfs);
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }
}

/// The spec's own example: an opaque whiteout is "applied first, before
/// creating the new version of `a/b`, regardless of the ordering in which the
/// whiteout file was encountered". Here it is encountered last.
#[test]
fn an_opaque_whiteout_applies_before_the_entries_beside_it() {
    for_each_route(
        "opq-ordering",
        &Route::ALL,
        &[
            tar_of(|b| {
                append_dir(b, "a/");
                append_dir(b, "a/b/");
                append_dir(b, "a/b/c/");
                append_file(b, "a/b/c/bar", b"bar");
            }),
            tar_of(|b| {
                append_dir(b, "a/");
                append_dir(b, "a/b/");
                append_dir(b, "a/b/c/");
                append_file(b, "a/b/c/foo", b"foo");
                append_file(b, "a/.wh..wh..opq", b"");
            }),
        ],
        |route, rootfs| {
            assert!(
                !rootfs.join("a/b/c/bar").exists(),
                "{route:?}: the lower layer's file is hidden"
            );
            assert_eq!(
                fs::read_to_string(rootfs.join("a/b/c/foo")).expect("foo"),
                "foo",
                "{route:?}: the marker's own layer is not hidden by it"
            );
        },
    );
}

/// "Files that are present in the same layer as a whiteout file can only be
/// hidden by whiteout files in subsequent layers."
#[test]
fn a_whiteout_does_not_hide_its_own_layer() {
    for_each_route(
        "wh-same-layer",
        &Route::ALL,
        &[
            tar_of(|b| {
                append_dir(b, "d/");
                append_file(b, "d/gone", b"lower");
            }),
            tar_of(|b| {
                append_file(b, "d/gone", b"upper");
                append_file(b, "d/.wh.gone", b"");
            }),
        ],
        |route, rootfs| {
            assert_eq!(
                fs::read_to_string(rootfs.join("d/gone")).expect("gone"),
                "upper",
                "{route:?}: the whiteout hides the lower layer, not its own"
            );
        },
    );
}

/// "A `.wh.` file, without a basename to delete, is invalid and
/// implementations SHOULD return an error when encountering such an entry."
/// Taking the empty name at face value pointed it at the directory the marker
/// sits in, which was then removed.
#[test]
fn a_whiteout_naming_nothing_is_refused() {
    // The span route is not among them: the plan refuses to place the entry,
    // which drops the image to the walk, and the walk is what reports it.
    for route in [Route::Streaming, Route::Planned] {
        let root = scratch(&format!("bare-wh-{route:?}"));
        let err = extract_by(
            route,
            &root,
            &[
                tar_of(|b| {
                    append_dir(b, "d/");
                    append_file(b, "d/keep", b"keep");
                }),
                tar_of(|b| append_file(b, "d/.wh.", b"")),
            ],
            true,
        )
        .expect_err("a whiteout naming nothing must be refused");

        assert!(
            matches!(err, Error::InvalidWhiteout { .. }),
            "{route:?}: expected an invalid whiteout, got {err:?}"
        );
        assert!(
            root.join("rootfs/d/keep").exists(),
            "{route:?}: the directory the marker sits in is not the target"
        );

        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }
}

/// The set-user-ID, set-group-ID and sticky bits are part of the mode the
/// layer ships, and dropping one silently changes what the image does.
#[test]
fn every_route_agrees_on_the_bits_above_the_permission_bits() {
    assert_routes_agree(
        "route-modes",
        &Route::ALL,
        &[tar_of(|b| {
            append_dir(b, "d/");
            append_file_moded(b, "d/setuid", b"u", 0o4755);
            append_file_moded(b, "d/setgid", b"g", 0o2755);
            append_file_moded(b, "d/sticky", b"s", 0o1777);
            append_file_moded(b, "d/private", b"p", 0o600);
        })],
    );
}

#[test]
fn set_user_id_survives_extraction() {
    for_each_route(
        "setuid",
        &Route::ALL,
        &[tar_of(|b| {
            append_dir(b, "d/");
            append_file_moded(b, "d/setuid", b"u", 0o4755);
        })],
        |route, rootfs| {
            let mode = fs::metadata(rootfs.join("d/setuid")).expect("stat").permissions().mode();
            assert_eq!(mode & 0o7777, 0o4755, "{route:?}: the mode the layer ships");
        },
    );
}

#[test]
fn every_route_agrees_on_empty_files_and_layers() {
    assert_routes_agree(
        "route-empty",
        &Route::ALL,
        &[
            tar_of(|b| {
                append_dir(b, "d/");
                append_file(b, "d/empty", b"");
            }),
            // A layer that adds nothing at all.
            tar_of(|_| {}),
            tar_of(|b| append_file(b, "d/after", b"after")),
        ],
    );
}

/// `tar` cannot hold a path this long in the header field, so it ships a
/// separate long-name entry ahead of it. That entry is not the file.
#[test]
fn every_route_agrees_on_paths_too_long_for_a_tar_header() {
    let deep = format!("d/{}/leaf", vec!["directory-with-a-long-name"; 6].join("/"));
    assert_routes_agree(
        "route-long-paths",
        &Route::ALL,
        &[tar_of(|b| {
            append_dir(b, "d/");
            append_file(b, &deep, b"deep");
        })],
    );
}

/// A whiteout for something no lower layer ever put there is not an error:
/// the layer is asking for a path to be absent, and it is.
#[test]
fn a_whiteout_of_a_path_that_was_never_there_is_not_an_error() {
    assert_routes_agree(
        "route-absent-whiteout",
        &Route::ALL,
        &[
            tar_of(|b| {
                append_dir(b, "d/");
                append_file(b, "d/kept", b"kept");
            }),
            tar_of(|b| {
                append_file(b, "d/.wh.never-existed", b"");
                append_file(b, ".wh.not-here-either", b"");
            }),
        ],
    );
}

#[test]
fn every_route_agrees_on_nested_opaque_whiteouts() {
    assert_routes_agree(
        "route-nested-opaque",
        &Route::ALL,
        &[
            tar_of(|b| {
                append_dir(b, "a/");
                append_file(b, "a/top", b"top");
                append_dir(b, "a/b/");
                append_file(b, "a/b/middle", b"middle");
                append_dir(b, "a/b/c/");
                append_file(b, "a/b/c/deep", b"deep");
            }),
            tar_of(|b| {
                append_dir(b, "a/");
                append_file(b, "a/.wh..wh..opq", b"");
                append_dir(b, "a/b/");
                append_file(b, "a/b/.wh..wh..opq", b"");
                append_file(b, "a/b/kept", b"kept");
            }),
        ],
    );
}

/// Sockets, FIFOs and device nodes cannot be created without privileges we do
/// not have, so they are skipped. The layer around them still extracts.
#[test]
fn an_unsupported_entry_does_not_stop_the_layer() {
    for_each_route(
        "unsupported",
        &Route::ALL,
        &[tar_of(|b| {
            append_dir(b, "d/");
            append_file(b, "d/before", b"before");
            append_of_type(b, "d/pipe", EntryType::Fifo);
            append_of_type(b, "d/node", EntryType::Char);
            append_file(b, "d/after", b"after");
        })],
        |route, rootfs| {
            assert!(!rootfs.join("d/pipe").exists(), "{route:?}: the fifo is skipped");
            assert!(!rootfs.join("d/node").exists(), "{route:?}: the device is skipped");
            assert_eq!(
                fs::read_to_string(rootfs.join("d/after")).expect("after"),
                "after",
                "{route:?}: the entries after it still land"
            );
        },
    );
}

/// A hard link names an inode, and when that inode is a symlink the result is
/// a second name for the link itself, not for whatever it resolves to.
#[test]
fn a_hard_link_to_a_symlink_links_the_symlink() {
    for_each_route(
        "hard-link-to-symlink",
        &Route::ALL,
        &[tar_of(|b| {
            append_dir(b, "d/");
            append_file(b, "d/target", b"target");
            append_symlink(b, "d/link", "target");
            append_hard_link(b, "d/linked", "d/link");
        })],
        |route, rootfs| {
            let linked = rootfs.join("d/linked");
            assert!(
                linked.is_symlink(),
                "{route:?}: linking a symlink gives another symlink"
            );
            assert_eq!(
                fs::read_link(&linked).expect("readlink"),
                Path::new("target"),
                "{route:?}: and it points where the original did"
            );
            assert_eq!(
                fs::symlink_metadata(&linked).expect("stat").ino(),
                fs::symlink_metadata(rootfs.join("d/link")).expect("stat").ino(),
                "{route:?}: the two names share one inode"
            );
        },
    );
}

/// The spec says a layer "MUST NOT include duplicate entries for file paths",
/// but says nothing about what to do with one that does. Whatever we do, the
/// routes have to do the same thing.
#[test]
fn every_route_agrees_on_a_duplicated_path() {
    assert_routes_agree(
        "route-duplicates",
        &Route::ALL,
        &[tar_of(|b| {
            append_dir(b, "d/");
            append_file(b, "d/twice", b"first");
            append_file(b, "d/twice", b"second");
        })],
    );
}

#[test]
fn the_last_of_a_duplicated_path_is_the_one_kept() {
    for_each_route(
        "duplicates",
        &Route::ALL,
        &[tar_of(|b| {
            append_dir(b, "d/");
            append_file(b, "d/twice", b"first");
            append_file(b, "d/twice", b"second");
        })],
        |route, rootfs| {
            assert_eq!(
                fs::read_to_string(rootfs.join("d/twice")).expect("twice"),
                "second",
                "{route:?}: the later entry wins, as a later layer would"
            );
        },
    );
}

/// A GNU sparse entry's body is a map of data segments rather than a flat run
/// of the stream, so `tar` has to place it and the span route cannot.
fn append_sparse(builder: &mut tar::Builder<Vec<u8>>, path: &str, at: u64, body: &[u8], real: u64) {
    fn octal(field: &mut [u8], value: u64) {
        let text = format!("{:0width$o}\0", value, width = field.len() - 1);
        field.copy_from_slice(text.as_bytes());
    }

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::GNUSparse);
    header.set_mode(0o644);
    // The size in the header is what the archive stores, not what the file is.
    header.set_size(body.len() as u64);
    header.set_path(path).expect("path");
    {
        let gnu = header.as_gnu_mut().expect("gnu header");
        octal(&mut gnu.realsize, real);
        octal(&mut gnu.sparse[0].offset, at);
        octal(&mut gnu.sparse[0].numbytes, body.len() as u64);
    }
    header.set_cksum();
    builder.append(&header, body).expect("sparse");
}

/// A sparse layer extracts with its hole in place, and the plan declines the
/// span route rather than treating the stored bytes as the file.
#[test]
fn a_sparse_file_keeps_its_hole() {
    let layers = [tar_of(|b| {
        append_dir(b, "d/");
        append_sparse(b, "d/holey", 1024, &[7u8; 512], 1536);
    })];

    // Not the span route: a sparse body is not a flat run of the stream, so
    // the plan refuses to place it and the image drops to the walk.
    for_each_route("sparse", &[Route::Streaming, Route::Planned], &layers, |route, rootfs| {
        let body = fs::read(rootfs.join("d/holey")).expect("holey");
        assert_eq!(body.len(), 1536, "{route:?}: the file is its real size");
        assert!(
            body[..1024].iter().all(|&byte| byte == 0),
            "{route:?}: the hole reads as zeroes"
        );
        assert_eq!(&body[1024..], &[7u8; 512], "{route:?}: and the data follows it");
    });
}

/// `tar -C dir .` puts the archive root in the layer as `./`, and the spec's
/// own worked example lists it as the first entry. It names the rootfs rather
/// than anything under it, which is not the same as escaping.
#[test]
fn every_route_agrees_on_a_layer_rooted_at_dot() {
    assert_routes_agree(
        "route-dot-root",
        &Route::ALL,
        &[
            tar_of(|b| {
                append_dir(b, "./");
                append_dir(b, "./etc/");
                append_file(b, "./etc/config", b"config");
                append_symlink(b, "./link", "etc/config");
            }),
            tar_of(|b| {
                append_dir(b, "./");
                append_file(b, "./etc/added", b"added");
            }),
        ],
    );
}

#[test]
fn a_layer_rooted_at_dot_extracts_its_entries() {
    for_each_route(
        "dot-root",
        &Route::ALL,
        &[tar_of(|b| {
            append_dir(b, "./");
            append_dir(b, "./etc/");
            append_file(b, "./etc/config", b"config");
        })],
        |route, rootfs| {
            assert_eq!(
                fs::read_to_string(rootfs.join("etc/config")).expect("config"),
                "config",
                "{route:?}: an entry under the archive root still lands"
            );
        },
    );
}

/// Writes a PAX extended header ahead of `path`, which is how a layer carries
/// extended attributes: one `SCHILY.xattr.<name>` record per attribute.
fn append_with_xattrs(
    builder: &mut tar::Builder<Vec<u8>>,
    path: &str,
    body: &[u8],
    xattrs: &[(&str, &[u8])],
) {
    let mut records = Vec::new();
    for (name, value) in xattrs {
        let field = format!("SCHILY.xattr.{name}=");
        // A record counts its own length, so the length grows the length.
        let mut len = field.len() + value.len() + 2;
        loop {
            let candidate = len.to_string().len() + 1 + field.len() + value.len() + 1;
            if candidate == len {
                break;
            }
            len = candidate;
        }
        records.extend_from_slice(format!("{len} {field}").as_bytes());
        records.extend_from_slice(value);
        records.push(b'\n');
    }

    let mut header = tar::Header::new_ustar();
    header.set_entry_type(EntryType::XHeader);
    header.set_mode(0o644);
    header.set_size(records.len() as u64);
    header.set_cksum();
    builder.append(&header, &records[..]).expect("pax header");

    append_file(builder, path, body);
}

/// A layer asking for extended attributes gets none, so by default the image
/// is refused rather than extracted into a container that will not match it.
#[test]
fn a_layer_setting_extended_attributes_is_refused() {
    for route in Route::ALL {
        let root = scratch(&format!("xattr-strict-{route:?}"));
        let err = extract_by(route, &root, &xattr_layers(), true)
            .expect_err("an image asking for attributes must be refused");
        match err {
            Error::UnsupportedXattrs { attributes, .. } => {
                assert_eq!(attributes, "security.capability")
            }
            other => panic!("{route:?}: expected unsupported attributes, got {other:?}"),
        }
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }
}

/// Told to carry on, the layer extracts whole and simply keeps no attributes.
/// They sit on entries an image cannot do without, so dropping the attribute
/// must not drop the file.
#[test]
fn extended_attributes_are_dropped_when_the_image_is_taken_anyway() {
    for route in Route::ALL {
        let root = scratch(&format!("xattr-lenient-{route:?}"));
        let rootfs = extract_by(route, &root, &xattr_layers(), false)
            .unwrap_or_else(|err| panic!("{route:?} failed to extract: {err}"));

        for name in ["capable", "noted", "plain"] {
            let path = rootfs.join("bin").join(name);
            assert_eq!(
                fs::read_to_string(&path).expect(name),
                "body",
                "{route:?}: {name} extracts whatever it carries"
            );
            assert!(
                xattr_names(&path).is_empty(),
                "{route:?}: {name} keeps no attributes"
            );
        }
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }
}

fn xattr_layers() -> Vec<Vec<u8>> {
    let capability = [1u8, 0, 0, 2, 0, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    vec![tar_of(|b| {
        append_dir(b, "bin/");
        append_with_xattrs(b, "bin/capable", b"body", &[("security.capability", &capability)]);
        append_with_xattrs(b, "bin/noted", b"body", &[("user.note", b"hello")]);
        append_file(b, "bin/plain", b"body");
    })]
}

fn xattr_names(path: &Utf8Path) -> Vec<String> {
    let c_path = std::ffi::CString::new(path.as_str()).expect("path");
    let mut buffer = vec![0u8; 4096];
    // SAFETY: both pointers are valid for the length passed.
    let len = unsafe {
        libc::llistxattr(c_path.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len())
    };
    if len <= 0 {
        return Vec::new();
    }
    buffer.truncate(len as usize);
    buffer
        .split(|&byte| byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect()
}
