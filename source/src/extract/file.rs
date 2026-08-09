//! Placing a single filesystem object, once its path has been checked.
//!
//! These replace the parts of `tar`'s own `unpack_in` the extractor needs.
//! Every path reaching them has already been resolved and had its parent
//! created, so none of them re-derives that.

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::error::{Error, Result};
use crate::fsutil;

/// Writes a regular file, replacing `tar`'s own unpacking so that the copy can
/// use a buffer sized for the pipeline rather than std's default. This only
/// has to reproduce the parts of `unpack_in` that a regular file needs: the
/// contents, the mode and the modification time.
///
/// Returns whether something already at `dst` had to be removed first.
pub(super) fn unpack_regular<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    dst: &Path,
    mode: u32,
    buffer: &mut [u8],
) -> io::Result<bool> {
    // Creating with the final mode avoids a window in which the file is more
    // permissive than the layer asked for. Permissions are checked when the
    // file is opened, so a read-only mode does not stop the writes below.
    //
    // The first attempt is exclusive, which costs nothing when the path is
    // free, as it is for every file in the first and largest layer. It also
    // refuses to follow a symlink already sitting there, so the path only has
    // to be cleared on the rare occasion a later layer replaces something,
    // rather than being stat'd and unlinked for every file in the image.
    let mut replaced = false;
    let mut file = match open_exclusive(dst, mode) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            fsutil::remove_any(dst).map_err(io::Error::other)?;
            replaced = true;
            open_exclusive(dst, mode)?
        }
        Err(err) => return Err(err),
    };

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

    // `mode` is what the file was created with, but the umask applies to
    // creation and not to this, so it is still needed to get the mode asked for.
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    if let Ok(mtime) = entry.header().mtime() {
        set_mtime(&file, mtime);
    }
    Ok(replaced)
}

fn open_exclusive(dst: &Path, mode: u32) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(dst)
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

/// The same for a symlink, which has no descriptor to hang the call on and
/// must not be followed to the file it names.
pub(super) fn set_symlink_mtime(path: &Path, mtime: u64) {
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    let time = libc::timespec {
        tv_sec: mtime as libc::time_t,
        tv_nsec: 0,
    };
    let times = [time, time];
    // SAFETY: the path is a live NUL terminated string and `times` holds the
    // two values utimensat reads.
    let _ = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
}

/// Keeps existing directories (including symlinks to directories) intact so
/// that layouts such as `/lib -> /usr/lib` survive later layers. Returns
/// whether something that was not a directory had to be removed.
pub(super) fn prepare_directory(dst: &Path) -> Result<bool> {
    match fs::symlink_metadata(dst) {
        Ok(metadata) => {
            let resolves_to_dir = metadata.is_dir()
                || (metadata.file_type().is_symlink()
                    && fs::metadata(dst).map(|m| m.is_dir()).unwrap_or(false));
            if resolves_to_dir {
                return Ok(false);
            }
            fsutil::remove_any(dst)?;
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(Error::io(format!("inspecting {}", dst.display()), err)),
    }
}
