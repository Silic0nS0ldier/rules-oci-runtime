//! Unpacking image layers into a root filesystem, replacing the previous
//! `undocker | tar -x` pipeline.

use std::cell::RefCell;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};
use tar::EntryType;

use crate::error::{Error, IoContext, Result};
use crate::fsutil;
use crate::image::{Descriptor, Layout, hex_encode, parse_digest};
use crate::log::{log, warning};

const WHITEOUT_PREFIX: &str = ".wh.";
const OPAQUE_WHITEOUT: &str = ".wh..wh..opq";

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

    pub fn apply_layer(&mut self, layout: &Layout, descriptor: &Descriptor) -> Result<()> {
        log!("Extracting layer {} ({})", descriptor.digest, descriptor.media_type);

        let file = layout.open_blob(descriptor)?;
        let state = Rc::new(RefCell::new(HashState::default()));
        let counted = HashingReader {
            inner: file,
            state: Rc::clone(&state),
        };
        let mut decoder = decompressor(&descriptor.media_type, counted)?;
        self.unpack(&mut decoder, &descriptor.digest)?;

        // Drain so the digest covers the whole blob, not just what tar consumed.
        io::copy(&mut decoder, &mut io::sink())
            .io_context(|| format!("reading layer {}", descriptor.digest))?;
        drop(decoder);

        let state = state.borrow();
        if descriptor.size != 0 && descriptor.size != state.bytes {
            return Err(Error::SizeMismatch {
                digest: descriptor.digest.clone(),
                expected: descriptor.size,
                actual: state.bytes,
            });
        }
        let actual = hex_encode(&state.hasher.clone().finalize());
        if actual != parse_digest(&descriptor.digest)?.hex {
            return Err(Error::DigestMismatch {
                digest: descriptor.digest.clone(),
                actual,
            });
        }
        Ok(())
    }

    fn unpack(&mut self, reader: &mut dyn Read, layer: &str) -> Result<()> {
        let root = Path::new(self.rootfs.as_std_path());
        let mut archive = tar::Archive::new(reader);
        archive.set_overwrite(true);
        let entries = archive
            .entries()
            .io_context(|| format!("reading layer {layer}"))?;

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

struct HashingReader<R> {
    inner: R,
    state: Rc<RefCell<HashState>>,
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read > 0 {
            let mut state = self.state.borrow_mut();
            state.hasher.update(&buf[..read]);
            state.bytes += read as u64;
        }
        Ok(read)
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
    fn hashing_reader_tracks_bytes_and_digest() {
        let state = Rc::new(RefCell::new(HashState::default()));
        let mut reader = HashingReader {
            inner: &b"hello"[..],
            state: Rc::clone(&state),
        };
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read");
        let state = state.borrow();
        assert_eq!(state.bytes, 5);
        assert_eq!(
            hex_encode(&state.hasher.clone().finalize()),
            hex_encode(&Sha256::digest(b"hello"))
        );
    }

    #[test]
    fn whiteout_names_are_recognised() {
        assert!(OPAQUE_WHITEOUT.starts_with(WHITEOUT_PREFIX));
        assert_eq!(".wh.foo".strip_prefix(WHITEOUT_PREFIX), Some("foo"));
        assert_eq!("foo".strip_prefix(WHITEOUT_PREFIX), None);
    }
}
