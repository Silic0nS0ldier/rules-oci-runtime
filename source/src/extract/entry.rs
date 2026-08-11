//! Walking a layer's tar stream and placing each entry it names.

use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::entries::{Kind, mode_of, mtime_of};
use crate::error::{Error, IoContext, Result};
use crate::fsutil;
use crate::log::{log, warning};

use super::RootfsExtractor;
use super::file::{create_directory, place_symlink, prepare_directory, unpack_regular};
use super::pipeline::CHUNK_BYTES;
use super::whiteout::{self, Whiteout};

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
        let mut relative = Vec::new();
        let mut path = PathBuf::new();
        let mut dst = PathBuf::new();
        self.written.clear();

        for entry in entries {
            let mut entry = entry.io_context(|| format!("reading layer {layer}"))?;
            path.clear();
            path.push(
                entry
                    .path()
                    .io_context(|| format!("reading entry path in layer {layer}"))?
                    .as_ref(),
            );

            // A resolved image had every table read up front, so reporting
            // here as well would say it twice.
            if !self.plan.is_resolved() {
                let names = crate::entries::xattr_names(&mut entry)
                    .io_context(|| format!("reading entry attributes in layer {layer}"))?;
                self.report_xattrs(layer, path.as_os_str().as_bytes(), &names)?;
            }

            // `./` names the rootfs, so there is nothing to place: the mode it
            // carries is deferred like any other directory's, and a layer
            // naming the root as anything but a directory is refused.
            if fsutil::names_the_root(path.as_os_str().as_bytes()) {
                if Kind::of(entry.header()) != Kind::Directory {
                    return Err(Error::UnsafeEntry {
                        layer: layer.to_string(),
                        path: path.display().to_string(),
                    });
                }
                self.deferred_modes
                    .push((root.to_path_buf(), mode_of(entry.header())));
                continue;
            }

            if !fsutil::canonical_entry_path(path.as_os_str().as_bytes(), &mut relative) {
                return Err(Error::UnsafeEntry {
                    layer: layer.to_string(),
                    path: path.display().to_string(),
                });
            }
            fsutil::join_under(root, &relative, &mut dst);

            match whiteout::of(&relative) {
                Some(Whiteout::Opaque(dir)) => {
                    let dir = match dir.is_empty() {
                        // A marker at the top of the layer names the rootfs.
                        true => root.to_path_buf(),
                        false => {
                            let mut at = PathBuf::new();
                            fsutil::join_under(root, &dir, &mut at);
                            at
                        }
                    };
                    self.apply_opaque_whiteout(root, &dir)?;
                    continue;
                }
                Some(Whiteout::Named(target)) => {
                    fsutil::join_under(root, &target, &mut dst);
                    // A whiteout hides the layers below it, never the one it is in.
                    if !self.written.contains(&dst) && self.parents.contains_parent_of(&dst)? {
                        log!("Whiteout: removing /{}", relative_display(root, &dst));
                        if fsutil::remove_any(&dst)? {
                            self.parents.forget(&dst);
                        }
                    }
                    continue;
                }
                Some(Whiteout::Invalid) => {
                    return Err(Error::InvalidWhiteout {
                        layer: layer.to_string(),
                        path: path.display().to_string(),
                    });
                }
                None => {}
            }

            let kind = Kind::of(entry.header());
            if kind == Kind::Unsupported {
                warning!(
                    "skipping unsupported entry {:?} of type {:?} in layer {layer}",
                    path.display(),
                    entry.header().entry_type()
                );
                continue;
            }

            // Recorded before the entry is placed rather than after, so that a
            // whiteout later in the layer can tell this layer's work from the
            // work of the layers below whatever the plan does with the entry.
            self.written.insert(dst.clone());

            if !self.parents.prepare(&dst)? {
                return Err(Error::UnsafeEntry {
                    layer: layer.to_string(),
                    path: path.display().to_string(),
                });
            }

            // A body a later layer replaces is written and then thrown away,
            // so the plan takes it out before any of that happens. What
            // writing it would have cleared away still goes: only bodies are
            // skipped, so a directory standing here was never planned away
            // and would be left for a write that no longer happens.
            if kind.is_file() && self.plan.is_shadowed(layer, &relative) {
                if fsutil::remove_any(&dst)? {
                    self.parents.forget(&dst);
                }
                continue;
            }

            let mode = mode_of(entry.header());

            // Regular files are the bulk of a layer, and tar copies them
            // through a buffer of std's default size, which is one write
            // syscall per 8 KiB. Ours is 32 times larger. A sparse body is not
            // a flat run of the stream, so `tar` still places those.
            if kind == Kind::File {
                let replaced = unpack_regular(&mut entry, &dst, mode, &mut buffer)
                    .io_context(|| format!("extracting {:?} from layer {layer}", path.display()))?;
                if replaced {
                    self.parents.forget(&dst);
                }
                continue;
            }

            if kind == Kind::Directory {
                if prepare_directory(&dst)? {
                    self.parents.forget(&dst);
                }
                self.deferred_modes.push((dst.clone(), mode));
                // `prepare_directory` cleared anything that was not a
                // directory, so what is left is one to keep.
                create_directory(&dst)?;
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

            match kind {
                Kind::Symlink => {
                    let target = link_name(&entry, layer)?.ok_or_else(unsafe_entry)?;
                    let mtime = mtime_of(entry.header());
                    place_symlink(&dst, target.as_os_str().as_bytes(), mtime).io_context(|| {
                        format!("extracting {:?} from layer {layer}", path.display())
                    })?;
                }
                Kind::HardLink => {
                    let target = link_name(&entry, layer)?.ok_or_else(unsafe_entry)?;
                    // A hard link names an earlier entry of the same archive,
                    // so it is rooted at the rootfs like any other entry path.
                    let target = target.as_os_str().as_bytes();
                    let mut canonical = Vec::new();
                    if !fsutil::canonical_entry_path(target, &mut canonical) {
                        return Err(unsafe_entry());
                    }
                    let mut source = PathBuf::new();
                    fsutil::join_under(root, &canonical, &mut source);
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
        log!("Whiteout: clearing /{}", relative_display(root, dir));
        self.clear_lower_layers(dir)?;
        self.parents.forget(dir);
        Ok(())
    }

    /// Removes everything under `dir` that this layer did not put there.
    ///
    /// A path this layer wrote stays, and so does the directory holding it —
    /// including one no entry names, made only to hold what went in it. What
    /// the layer wrote is taken from the record rather than from the disk: a
    /// body the plan skipped counts as written, and the directory it would
    /// have gone in is still one to keep.
    fn clear_lower_layers(&mut self, dir: &Path) -> Result<()> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(Error::io(format!("listing {}", dir.display()), err)),
        };
        let mut keep = Vec::new();
        for entry in entries {
            let entry = entry.io_context(|| format!("listing {}", dir.display()))?;
            let path = entry.path();
            let is_dir = entry
                .file_type()
                .io_context(|| format!("inspecting {}", path.display()))?
                .is_dir();
            if is_dir && (self.written.contains(&path) || self.wrote_under(&path)) {
                keep.push(path);
                continue;
            }
            if self.written.contains(&path) {
                continue;
            }
            fsutil::remove_any(&path)?;
        }
        for path in keep {
            self.clear_lower_layers(&path)?;
        }
        Ok(())
    }

    /// True when this layer has placed anything under `dir`.
    fn wrote_under(&self, dir: &Path) -> bool {
        self.written
            .range(dir.to_path_buf()..)
            .take_while(|path| path.starts_with(dir))
            .any(|path| path != dir)
    }
}

pub(super) fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
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
