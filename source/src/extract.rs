//! Unpacking image layers into a root filesystem, replacing the previous
//! `undocker | tar -x` pipeline.

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;

use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};
use tar::EntryType;

use crate::error::{Error, IoContext, Result};
use crate::fsutil;
use crate::image::{Descriptor, Layout, hex_encode, parse_digest};
use crate::log::{log, warning};

const WHITEOUT_PREFIX: &str = ".wh.";
const OPAQUE_WHITEOUT: &str = ".wh..wh..opq";

/// Size of each buffer handed from the decompressor to the writer.
const CHUNK_BYTES: usize = 256 * 1024;

/// How many chunks may be in flight, bounding the pipeline to 2 MiB.
const PIPELINE_DEPTH: usize = 8;

/// Applies layers in order, deferring directory permissions so that read-only
/// directories in one layer do not block writes from the next.
pub struct RootfsExtractor {
    rootfs: Utf8PathBuf,
    deferred_modes: Vec<(PathBuf, u32)>,
}

impl RootfsExtractor {
    pub fn new(rootfs: &Utf8Path) -> Result<Self> {
        fs::create_dir_all(rootfs).io_context(|| format!("creating {rootfs}"))?;
        Ok(RootfsExtractor {
            rootfs: rootfs.to_owned(),
            deferred_modes: Vec::new(),
        })
    }

    /// Decompression is CPU bound and writing the rootfs is IO bound, so a
    /// second thread inflates the blob while this one writes the entries. The
    /// two overlap rather than run back to back, which on a large layer is
    /// worth roughly the whole decompression time.
    pub fn apply_layer(&mut self, layout: &Layout, descriptor: &Descriptor) -> Result<()> {
        log!("Extracting layer {} ({})", descriptor.digest, descriptor.media_type);

        let file = layout.open_blob(descriptor)?;
        let digest = descriptor.digest.clone();
        let (sender, receiver) = sync_channel(PIPELINE_DEPTH);
        let owned = descriptor.clone();
        let inflate = thread::spawn(move || inflate_blob(file, &owned, sender));

        let mut reader = ChunkReader::new(receiver);
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

    fn unpack(&mut self, reader: &mut dyn Read, layer: &str) -> Result<()> {
        let root = Path::new(self.rootfs.as_std_path());
        let mut archive = tar::Archive::new(reader);
        archive.set_overwrite(true);
        let entries = archive
            .entries()
            .io_context(|| format!("reading layer {layer}"))?;

        // One buffer for the whole layer: allocating per file would cost more
        // than the copy it serves.
        let mut buffer = vec![0u8; CHUNK_BYTES];

        for entry in entries {
            let mut entry = entry.io_context(|| format!("reading layer {layer}"))?;
            let path = entry
                .path()
                .io_context(|| format!("reading entry path in layer {layer}"))?
                .into_owned();

            let Some(parts) = fsutil::sanitize_relative_path(&path) else {
                return Err(Error::UnsafeEntry {
                    layer: layer.to_string(),
                    path: path.display().to_string(),
                });
            };

            let name = parts.last().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            if name == OPAQUE_WHITEOUT {
                let dir = fsutil::join_components(root, &parts[..parts.len() - 1]);
                self.apply_opaque_whiteout(root, &dir)?;
                continue;
            }
            if let Some(target) = name.strip_prefix(WHITEOUT_PREFIX) {
                let mut whiteout = parts[..parts.len() - 1].to_vec();
                whiteout.push(target.into());
                let dst = fsutil::join_components(root, &whiteout);
                if fsutil::parent_is_within(root, &dst)? {
                    log!("Whiteout: removing /{}", relative_display(root, &dst));
                    fsutil::remove_any(&dst)?;
                }
                continue;
            }

            let dst = fsutil::join_components(root, &parts);
            let entry_type = entry.header().entry_type();
            if !is_supported(entry_type) {
                warning!(
                    "skipping unsupported entry {:?} of type {:?} in layer {layer}",
                    path.display(),
                    entry_type
                );
                continue;
            }

            if !fsutil::parent_is_within(root, &dst)? {
                return Err(Error::UnsafeEntry {
                    layer: layer.to_string(),
                    path: path.display().to_string(),
                });
            }

            let mode = entry.header().mode().unwrap_or(0o755) & 0o7777;
            if entry_type.is_dir() {
                prepare_directory(&dst)?;
                self.deferred_modes.push((dst.clone(), mode));
                entry.set_preserve_permissions(false);
            } else {
                fsutil::remove_any(&dst)?;
                entry.set_preserve_permissions(true);
            }
            entry.set_preserve_mtime(true);

            // Regular files are the bulk of a layer, and tar copies them
            // through a buffer of std's default size, which is one write
            // syscall per 8 KiB. Ours is 32 times larger.
            if matches!(entry_type, EntryType::Regular | EntryType::Continuous) {
                unpack_regular(&mut entry, &dst, mode, &mut buffer)
                    .io_context(|| format!("extracting {:?} from layer {layer}", path.display()))?;
                continue;
            }

            let unpacked = entry
                .unpack_in(root)
                .io_context(|| format!("extracting {:?} from layer {layer}", path.display()))?;
            if !unpacked {
                return Err(Error::UnsafeEntry {
                    layer: layer.to_string(),
                    path: path.display().to_string(),
                });
            }
        }
        Ok(())
    }

    /// `.wh..wh..opq` hides everything the lower layers put in this directory.
    fn apply_opaque_whiteout(&self, root: &Path, dir: &Path) -> Result<()> {
        if !fsutil::parent_is_within(root, dir)? {
            return Ok(());
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(Error::io(format!("listing {}", dir.display()), err)),
        };
        log!("Whiteout: clearing /{}", relative_display(root, dir));
        for entry in entries {
            let entry = entry.io_context(|| format!("listing {}", dir.display()))?;
            fsutil::remove_any(&entry.path())?;
        }
        Ok(())
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

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn is_supported(entry_type: EntryType) -> bool {
    matches!(
        entry_type,
        EntryType::Regular
            | EntryType::Directory
            | EntryType::Symlink
            | EntryType::Link
            | EntryType::Continuous
            | EntryType::GNUSparse
    )
}

/// Writes a regular file, replacing `tar`'s own unpacking so that the copy can
/// use a buffer sized for the pipeline rather than std's default. The path has
/// already been checked, so this only has to reproduce the parts of `unpack_in`
/// that a regular file needs: the parent directory, the contents, the mode and
/// the modification time.
fn unpack_regular<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    dst: &Path,
    mode: u32,
    buffer: &mut [u8],
) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    // Creating with the final mode avoids a window in which the file is more
    // permissive than the layer asked for. Permissions are checked when the
    // file is opened, so a read-only mode does not stop the writes below.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(dst)?;

    let mut filled = 0;
    loop {
        match entry.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => {
                filled += read;
                // Only flush full buffers, so that a stream handing over small
                // reads still turns into large writes.
                if filled == buffer.len() {
                    file.write_all(buffer)?;
                    filled = 0;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    if filled > 0 {
        file.write_all(&buffer[..filled])?;
    }

    // The file may have existed with a different mode before it was truncated.
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    if let Ok(mtime) = entry.header().mtime() {
        set_mtime(&file, mtime);
    }
    Ok(())
}

/// `tar` reaches for the `filetime` crate to do this; `futimens` on the open
/// file is the same call without the dependency. Timestamps are cosmetic, so a
/// failure is not worth failing the run over.
fn set_mtime(file: &fs::File, mtime: u64) {
    let time = libc::timespec {
        tv_sec: mtime as libc::time_t,
        tv_nsec: 0,
    };
    let times = [time, time];
    let _ = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
}

/// Keeps existing directories (including symlinks to directories) intact so
/// that layouts such as `/lib -> /usr/lib` survive later layers.
fn prepare_directory(dst: &Path) -> Result<()> {
    match fs::symlink_metadata(dst) {
        Ok(metadata) => {
            let resolves_to_dir = metadata.is_dir()
                || (metadata.file_type().is_symlink()
                    && fs::metadata(dst).map(|m| m.is_dir()).unwrap_or(false));
            if resolves_to_dir {
                return Ok(());
            }
            fsutil::remove_any(dst)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::io(format!("inspecting {}", dst.display()), err)),
    }
}

#[derive(Default)]
struct HashState {
    hasher: Sha256,
    bytes: u64,
}

/// Reads the blob and hashes it, handing each buffer to the decompressor by
/// move. With decompression on the critical path, sha256 of the compressed
/// bytes is time the inflater is not inflating, and the read it accompanies is
/// nearly free, so the two belong together on a thread of their own.
///
/// The hash covers exactly the bytes the decompressor is given, so the digest
/// still describes what was extracted rather than a second read of the file.
fn read_and_hash(mut file: fs::File, sender: SyncSender<io::Result<Vec<u8>>>) -> HashState {
    let mut state = HashState::default();
    loop {
        let mut chunk = vec![0u8; CHUNK_BYTES];
        match file.read(&mut chunk) {
            Ok(0) => return state,
            Ok(read) => {
                chunk.truncate(read);
                state.hasher.update(&chunk);
                state.bytes += read as u64;
                // The consumer has stopped, and has an error of its own to report.
                if sender.send(Ok(chunk)).is_err() {
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
fn inflate_blob(
    file: fs::File,
    descriptor: &Descriptor,
    sender: SyncSender<io::Result<Vec<u8>>>,
) -> Result<()> {
    let (raw_sender, raw_receiver) = sync_channel(PIPELINE_DEPTH);
    let hashing = thread::spawn(move || read_and_hash(file, raw_sender));
    let counted = ChunkReader::new(raw_receiver);
    let mut decoder = match decompressor(&descriptor.media_type, counted) {
        Ok(decoder) => decoder,
        Err(err) => {
            let _ = sender.send(Err(io::Error::other(err.to_string())));
            return Err(err);
        }
    };

    loop {
        let mut chunk = vec![0u8; CHUNK_BYTES];
        let read = match decoder.read(&mut chunk) {
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
        chunk.truncate(read);
        if sender.send(Ok(chunk)).is_err() {
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

/// Presents the inflated chunks as a stream for `tar` to walk.
struct ChunkReader {
    chunks: Receiver<io::Result<Vec<u8>>>,
    current: io::Cursor<Vec<u8>>,
}

impl ChunkReader {
    fn new(chunks: Receiver<io::Result<Vec<u8>>>) -> Self {
        ChunkReader {
            chunks,
            current: io::Cursor::new(Vec::new()),
        }
    }
}

impl Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let read = self.current.read(buf)?;
            if read > 0 {
                return Ok(read);
            }
            match self.chunks.recv() {
                Ok(Ok(chunk)) => self.current = io::Cursor::new(chunk),
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

#[cfg(test)]
mod tests {
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
        let state = read_and_hash(fs::File::open(&blob).expect("open"), sender);

        let mut passed_on = Vec::new();
        ChunkReader::new(receiver)
            .read_to_end(&mut passed_on)
            .expect("read");
        assert_eq!(passed_on, b"hello");
        assert_eq!(state.bytes, 5);
        assert_eq!(
            hex_encode(&state.hasher.finalize()),
            hex_encode(&Sha256::digest(b"hello"))
        );
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
        let layout = Layout::open(root)?;
        let rootfs = root.join("rootfs");
        let mut extractor = RootfsExtractor::new(&rootfs)?;
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
}
