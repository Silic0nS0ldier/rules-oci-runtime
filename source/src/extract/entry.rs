//! Walking a layer's tar stream and placing each entry it names.

use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use tar::EntryType;

use crate::error::{Error, IoContext, Result};
use crate::fsutil;
use crate::log::{log, warning};

use super::RootfsExtractor;
use super::file::{prepare_directory, set_symlink_mtime, unpack_regular};
use super::pipeline::CHUNK_BYTES;

pub(super) const WHITEOUT_PREFIX: &str = ".wh.";
pub(super) const OPAQUE_WHITEOUT: &str = ".wh..wh..opq";

impl RootfsExtractor {
    pub(super) fn unpack(&mut self, reader: &mut dyn Read, layer: &str) -> Result<()> {
        let rootfs = self.rootfs.clone();
        let root = rootfs.as_std_path();
        let mut archive = tar::Archive::new(reader);
        archive.set_overwrite(true);
        let entries = archive
            .entries()
            .io_context(|| format!("reading layer {layer}"))?;

        // One buffer for the whole layer: allocating per file would cost more
        // than the copy it serves.
        let mut buffer = vec![0u8; CHUNK_BYTES];
        // Reused for every entry, so a layer costs two path allocations rather
        // than a handful per entry.
        let mut path = PathBuf::new();
        let mut dst = PathBuf::new();

        for entry in entries {
            let mut entry = entry.io_context(|| format!("reading layer {layer}"))?;
            path.clear();
            path.push(
                entry
                    .path()
                    .io_context(|| format!("reading entry path in layer {layer}"))?
                    .as_ref(),
            );

            if !fsutil::resolve_under(root, &path, &mut dst) {
                return Err(Error::UnsafeEntry {
                    layer: layer.to_string(),
                    path: path.display().to_string(),
                });
            }

            let name = dst.file_name().unwrap_or_default().as_bytes();
            if name == OPAQUE_WHITEOUT.as_bytes() {
                dst.pop();
                let dir = std::mem::take(&mut dst);
                self.apply_opaque_whiteout(root, &dir)?;
                dst = dir;
                continue;
            }
            if let Some(target) = name.strip_prefix(WHITEOUT_PREFIX.as_bytes()) {
                // `target` borrows the buffer the new name has to go into.
                let target = std::ffi::OsStr::from_bytes(target).to_owned();
                dst.set_file_name(target);
                if self.parents.contains_parent_of(&dst)? {
                    log!("Whiteout: removing /{}", relative_display(root, &dst));
                    if fsutil::remove_any(&dst)? {
                        self.parents.forget(&dst);
                    }
                }
                continue;
            }

            let entry_type = entry.header().entry_type();
            if !is_supported(entry_type) {
                warning!(
                    "skipping unsupported entry {:?} of type {:?} in layer {layer}",
                    path.display(),
                    entry_type
                );
                continue;
            }

            // A body a later layer replaces is written and then thrown away,
            // so the plan takes it out before any of that happens.
            if matches!(entry_type, EntryType::Regular | EntryType::Continuous)
                && self.plan.is_shadowed(layer, path.as_os_str().as_bytes())
            {
                continue;
            }

            if !self.parents.prepare(&dst)? {
                return Err(Error::UnsafeEntry {
                    layer: layer.to_string(),
                    path: path.display().to_string(),
                });
            }

            let mode = entry.header().mode().unwrap_or(0o755) & 0o7777;

            // Regular files are the bulk of a layer, and tar copies them
            // through a buffer of std's default size, which is one write
            // syscall per 8 KiB. Ours is 32 times larger.
            if matches!(entry_type, EntryType::Regular | EntryType::Continuous) {
                entry.set_preserve_mtime(true);
                let replaced = unpack_regular(&mut entry, &dst, mode, &mut buffer)
                    .io_context(|| format!("extracting {:?} from layer {layer}", path.display()))?;
                if replaced {
                    self.parents.forget(&dst);
                }
                continue;
            }

            if entry_type.is_dir() {
                if prepare_directory(&dst)? {
                    self.parents.forget(&dst);
                }
                self.deferred_modes.push((dst.clone(), mode));
                match fs::create_dir(&dst) {
                    Ok(()) => {}
                    // `prepare_directory` cleared anything that was not a
                    // directory, so what is left is one to keep.
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(err) => {
                        return Err(Error::io(format!("creating {}", dst.display()), err));
                    }
                }
                continue;
            }

            // Replacing whatever is here is what `set_overwrite` bought from
            // `tar`, and the link calls below will not do it themselves.
            if fsutil::remove_any(&dst)? {
                self.parents.forget(&dst);
            }

            let unsafe_entry = || Error::UnsafeEntry {
                layer: layer.to_string(),
                path: path.display().to_string(),
            };

            match entry_type {
                EntryType::Symlink => {
                    let target = link_name(&entry, layer)?.ok_or_else(unsafe_entry)?;
                    std::os::unix::fs::symlink(&target, &dst).io_context(|| {
                        format!("extracting {:?} from layer {layer}", path.display())
                    })?;
                    if let Ok(mtime) = entry.header().mtime() {
                        set_symlink_mtime(&dst, mtime);
                    }
                }
                EntryType::Link => {
                    let target = link_name(&entry, layer)?.ok_or_else(unsafe_entry)?;
                    // A hard link names an earlier entry of the same archive,
                    // so it is rooted at the rootfs like any other entry path.
                    let mut source = PathBuf::new();
                    if !fsutil::resolve_under(root, &target, &mut source) {
                        return Err(unsafe_entry());
                    }
                    if !self.parents.contains_parent_of(&source)? {
                        return Err(unsafe_entry());
                    }
                    fs::hard_link(&source, &dst).io_context(|| {
                        format!("extracting {:?} from layer {layer}", path.display())
                    })?;
                }
                // Sparse files still need `tar` to place the holes.
                _ => {
                    entry.set_preserve_permissions(true);
                    entry.set_preserve_mtime(true);
                    let unpacked = entry.unpack_in(root).io_context(|| {
                        format!("extracting {:?} from layer {layer}", path.display())
                    })?;
                    if !unpacked {
                        return Err(unsafe_entry());
                    }
                }
            }
        }
        Ok(())
    }

    /// `.wh..wh..opq` hides everything the lower layers put in this directory.
    fn apply_opaque_whiteout(&mut self, root: &Path, dir: &Path) -> Result<()> {
        // The marker applies to a directory, and only to one inside the
        // rootfs. Reading through a symlink here would clear whatever it points
        // at, which a layer is free to aim anywhere on the host.
        match fs::symlink_metadata(dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(Error::io(format!("inspecting {}", dir.display()), err)),
        }
        // A directory can still sit outside the rootfs when one of its parents
        // is a symlink, so the resolved path has to be checked as well.
        if !self.parents.contains(dir)? {
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
        self.parents.forget(dir);
        Ok(())
    }
}

pub(super) fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

pub(super) fn is_supported(entry_type: EntryType) -> bool {
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

/// The target of a link entry, or `None` when the header does not name one.
/// An empty target is no target: `symlinkat` would take it and leave a link
/// that resolves to nothing.
fn link_name<R: Read>(entry: &tar::Entry<'_, R>, layer: &str) -> Result<Option<PathBuf>> {
    let target = entry
        .link_name()
        .io_context(|| format!("reading a link name in layer {layer}"))?;
    Ok(target
        .filter(|target| target.as_os_str().as_bytes() != b"")
        .map(|target| target.into_owned()))
}
