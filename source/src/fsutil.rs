//! Filesystem helpers shared by extraction and cleanup.

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

/// True when `path`'s parent directory resolves to somewhere inside `root`.
pub fn parent_is_within(root: &Path, path: &Path) -> Result<bool> {
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    let canonical_root = root
        .canonicalize()
        .io_context(|| format!("resolving {}", root.display()))?;
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
    Ok(canonical_parent.starts_with(&canonical_root))
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
