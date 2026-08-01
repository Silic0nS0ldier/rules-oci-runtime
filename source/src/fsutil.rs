//! Filesystem helpers shared by extraction and cleanup.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use crate::error::{IoContext, Result};

/// Splits a tar entry path into safe components, or returns `None` when the
/// entry tries to escape (absolute paths are rooted at the rootfs, `..` is
/// never allowed).
pub fn sanitize_relative_path(path: &Path) -> Option<Vec<std::ffi::OsString>> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => continue,
            Component::ParentDir => return None,
            Component::Normal(part) => {
                if part.is_empty() {
                    continue;
                }
                parts.push(part.to_owned())
            }
        }
    }
    if parts.is_empty() { None } else { Some(parts) }
}

/// Removes a tree even when directories were extracted without write permission.
pub fn force_remove_dir_all(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => return Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }
    make_writable_recursive(path)?;
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(crate::error::Error::io(format!("removing {}", path.display()), err)),
    }
}

fn make_writable_recursive(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(crate::error::Error::io(
                format!("inspecting {}", path.display()),
                err,
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        let mode = metadata.permissions().mode();
        if mode & 0o700 != 0o700 {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700));
        }
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(crate::error::Error::io(
                    format!("listing {}", path.display()),
                    err,
                ));
            }
        };
        for entry in entries {
            let entry = entry.io_context(|| format!("listing {}", path.display()))?;
            make_writable_recursive(&entry.path())?;
        }
    }
    Ok(())
}

/// Removes a single filesystem object, following no symlinks.
pub fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => force_remove_dir_all(path),
        Ok(_) => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(crate::error::Error::io(
                format!("removing {}", path.display()),
                err,
            )),
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(crate::error::Error::io(
            format!("inspecting {}", path.display()),
            err,
        )),
    }
}

/// Resolves entry parents against the rootfs, remembering the ones already
/// checked.
///
/// Checking a parent means resolving it with `canonicalize`, which reads every
/// symlink along the way: about sixteen `readlink` calls per entry on a distro
/// base image, all of them failing, and the root itself is resolved again each
/// time. A layer names the same few hundred directories over and over, so
/// remembering which ones have been found to resolve inside the root turns
/// nearly all of that into a hash lookup.
pub struct ParentCache {
    canonical_root: PathBuf,
    verified: HashSet<PathBuf>,
}

impl ParentCache {
    pub fn new(root: &Path) -> Result<Self> {
        let canonical_root = root
            .canonicalize()
            .io_context(|| format!("resolving {}", root.display()))?;
        let mut verified = HashSet::new();
        verified.insert(root.to_owned());
        verified.insert(canonical_root.clone());
        Ok(ParentCache {
            canonical_root,
            verified,
        })
    }

    /// True when `path`'s parent resolves to somewhere inside the root, having
    /// created it if needed. See [`prepare_directory_within`] for why creating
    /// it is part of the check rather than left to the caller.
    pub fn prepare(&mut self, path: &Path) -> Result<bool> {
        let Some(parent) = path.parent() else {
            return Ok(false);
        };
        if self.verified.contains(parent) {
            return Ok(true);
        }
        if !prepare_directory_within(&self.canonical_root, parent)? {
            return Ok(false);
        }
        self.verified.insert(parent.to_owned());
        Ok(true)
    }

    /// True when `path` itself resolves to somewhere inside the root. A path
    /// that is not there counts as outside: there is nothing at it to act on.
    pub fn contains(&self, path: &Path) -> Result<bool> {
        match path.canonicalize() {
            Ok(canonical) => Ok(canonical.starts_with(&self.canonical_root)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(crate::error::Error::io(
                format!("resolving {}", path.display()),
                err,
            )),
        }
    }

    /// True when `path`'s parent resolves to somewhere inside the root, for
    /// callers that must not create anything.
    pub fn contains_parent_of(&self, path: &Path) -> Result<bool> {
        parent_is_within(&self.canonical_root, path)
    }

    /// Forgets `path` and everything under it, because what was verified there
    /// has been removed and a later layer may put a symlink in its place.
    pub fn forget(&mut self, path: &Path) {
        self.verified.retain(|verified| !verified.starts_with(path));
    }
}

/// Resolves `dir`, creating it if needed, and reports whether it ended up
/// inside `canonical_root`.
///
/// Creating the directory is part of the check rather than the caller's job. A
/// layer may ship a symlink such as `lnk -> /etc` and then an entry
/// `lnk/sub/file`, whose parent `lnk/sub` does not exist and so cannot be
/// resolved; creating it with `create_dir_all` would follow the symlink and put
/// it outside the root. So the deepest ancestor that does exist is resolved and
/// checked first, and everything below it is created one component at a time,
/// which cannot traverse a symlink that is not there.
fn prepare_directory_within(canonical_root: &Path, dir: &Path) -> Result<bool> {
    let mut missing = Vec::new();
    let mut existing = dir;
    let canonical = loop {
        match existing.canonicalize() {
            Ok(canonical) => break canonical,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else {
                    return Ok(false);
                };
                missing.push(name.to_owned());
                existing = parent;
            }
            Err(err) => {
                return Err(crate::error::Error::io(
                    format!("resolving {}", existing.display()),
                    err,
                ));
            }
        }
    };
    if !canonical.starts_with(canonical_root) {
        return Ok(false);
    }

    // Everything below here is created rather than resolved, so each component
    // is a real directory and the chain cannot leave the root.
    let mut path = existing.to_path_buf();
    for name in missing.iter().rev() {
        path.push(name);
        match fs::create_dir(&path) {
            Ok(()) => {}
            // Something is already there that `canonicalize` could not resolve,
            // a dangling symlink among other things, so check where it points.
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let canonical = path
                    .canonicalize()
                    .io_context(|| format!("resolving {}", path.display()))?;
                if !canonical.starts_with(canonical_root) {
                    return Ok(false);
                }
            }
            Err(err) => {
                return Err(crate::error::Error::io(
                    format!("creating {}", path.display()),
                    err,
                ));
            }
        }
    }
    Ok(true)
}

/// True when `path`'s parent directory resolves to somewhere inside `root`, for
/// callers that must not create anything. A parent that does not exist cannot
/// be escaped through, so it counts as within: there is nothing at the far end
/// to act on either way.
fn parent_is_within(canonical_root: &Path, path: &Path) -> Result<bool> {
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    let canonical_parent = match parent.canonicalize() {
        Ok(canonical_parent) => canonical_parent,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(err) => {
            return Err(crate::error::Error::io(
                format!("resolving {}", parent.display()),
                err,
            ));
        }
    };
    Ok(canonical_parent.starts_with(canonical_root))
}

pub fn join_components(root: &Path, parts: &[std::ffi::OsString]) -> PathBuf {
    let mut path = root.to_path_buf();
    for part in parts {
        path.push(part);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn parts(path: &str) -> Option<Vec<String>> {
        sanitize_relative_path(Path::new(path))
            .map(|parts| parts.into_iter().map(|p| p.to_string_lossy().into_owned()).collect())
    }

    #[test]
    fn absolute_paths_are_rooted_at_the_rootfs() {
        assert_eq!(parts("/etc/passwd"), Some(vec!["etc".into(), "passwd".into()]));
    }

    #[test]
    fn leading_dot_slash_is_stripped() {
        assert_eq!(parts("./usr/bin/env"), Some(vec!["usr".into(), "bin".into(), "env".into()]));
    }

    #[test]
    fn redundant_separators_are_collapsed() {
        assert_eq!(parts("usr//bin///env"), Some(vec!["usr".into(), "bin".into(), "env".into()]));
    }

    #[test]
    fn parent_traversal_is_rejected() {
        assert_eq!(parts("../etc/passwd"), None);
        assert_eq!(parts("usr/../../etc/passwd"), None);
        assert_eq!(parts("/../etc"), None);
    }

    #[test]
    fn empty_paths_are_rejected() {
        assert_eq!(parts("."), None);
        assert_eq!(parts("/"), None);
        assert_eq!(parts(""), None);
    }

    #[test]
    fn components_are_joined_in_order() {
        let joined = join_components(
            Path::new("/tmp/rootfs"),
            &[OsString::from("etc"), OsString::from("hosts")],
        );
        assert_eq!(joined, PathBuf::from("/tmp/rootfs/etc/hosts"));
    }

    #[test]
    fn missing_paths_are_removable() {
        assert!(remove_any(Path::new("/nonexistent/path/for/tests")).is_ok());
        assert!(force_remove_dir_all(Path::new("/nonexistent/path/for/tests")).is_ok());
    }
}
