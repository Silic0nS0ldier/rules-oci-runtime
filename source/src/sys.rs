//! The small amount of libc this binary needs: identity, tty detection,
//! randomness, file mapping and signal forwarding.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::error::{IoContext, Result};

static CHILD_PID: AtomicI32 = AtomicI32::new(0);

/// Signals that should reach the container rather than kill the launcher.
const FORWARDED: [libc::c_int; 7] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGWINCH,
];

/// A read-only mapping of a whole file.
///
/// Callers that need random access over a blob would otherwise have to copy it
/// into the heap first; a mapping lets them read the page cache in place, and
/// lets several threads fault in the pages they touch concurrently.
///
/// The bytes are only valid while the file behind them is: truncating it under
/// a live mapping turns reads past the new end into SIGBUS rather than a short
/// read. Every caller here maps an immutable content addressed blob.
pub struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}

// SAFETY: the mapping is read-only for its whole life and owns its address
// range, so handing it between threads exposes nothing thread-affine.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Maps `len` bytes of `file`, or returns `None` when there is nothing to
    /// map or the kernel refuses. Mapping is an optimisation: a caller that
    /// gets `None` is expected to read the file instead.
    pub fn of(file: &File, len: usize) -> Option<Self> {
        if len == 0 {
            return None;
        }
        // SAFETY: a null hint asks the kernel to choose the address, and the
        // descriptor is open for the duration of the call.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return None;
        }
        Some(Mapping { ptr, len })
    }
}

impl std::ops::Deref for Mapping {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: mmap returned `len` readable bytes at `ptr`, and the mapping
        // outlives the slice.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the pointer and length are the ones mmap handed back.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// A whole file as bytes, mapped where the kernel allows it and read into
/// memory where it does not.
///
/// Random access over a blob is what the span route and the index builder both
/// need, and neither can do anything with a mapping that was refused, so the
/// fallback belongs with the mapping rather than at each of them.
pub struct Blob {
    mapped: Option<Mapping>,
    read: Vec<u8>,
}

impl Blob {
    pub fn of(mut file: &File) -> std::io::Result<Self> {
        let len = file.metadata().map_or(0, |metadata| metadata.len()) as usize;
        if let Some(mapped) = Mapping::of(file, len) {
            return Ok(Blob {
                mapped: Some(mapped),
                read: Vec::new(),
            });
        }
        let mut read = Vec::with_capacity(len);
        file.read_to_end(&mut read)?;
        Ok(Blob { mapped: None, read })
    }
}

impl std::ops::Deref for Blob {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.mapped {
            Some(mapped) => mapped,
            None => &self.read,
        }
    }
}

pub fn euid() -> u32 {
    // SAFETY: geteuid is always successful and has no preconditions.
    unsafe { libc::geteuid() }
}

pub fn egid() -> u32 {
    // SAFETY: getegid is always successful and has no preconditions.
    unsafe { libc::getegid() }
}

pub fn stdin_is_tty() -> bool {
    // SAFETY: isatty only inspects the given descriptor.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

/// `kill` is async-signal-safe, so forwarding straight from the handler is sound.
extern "C" fn forward(signal: libc::c_int) {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill with a positive pid only signals that process.
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

pub struct SignalForwarder;

impl SignalForwarder {
    pub fn install(child_pid: i32) -> Self {
        CHILD_PID.store(child_pid, Ordering::SeqCst);
        for signal in FORWARDED {
            // SAFETY: `forward` is async-signal-safe and the handler outlives the process.
            unsafe {
                libc::signal(signal, forward as *const () as libc::sighandler_t);
            }
        }
        SignalForwarder
    }
}

impl Drop for SignalForwarder {
    fn drop(&mut self) {
        CHILD_PID.store(0, Ordering::SeqCst);
        for signal in FORWARDED {
            // SAFETY: restoring the default disposition is always valid.
            unsafe {
                libc::signal(signal, libc::SIG_DFL);
            }
        }
    }
}

/// Random bytes for container identifiers, so concurrent runs cannot collide.
pub fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0u8; bytes];
    File::open("/dev/urandom")
        .io_context(|| "opening /dev/urandom".to_string())?
        .read_exact(&mut buffer)
        .io_context(|| "reading /dev/urandom".to_string())?;
    Ok(crate::image::hex_encode(&buffer))
}

/// Translates a wait status into the shell convention of `128 + signal`.
pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => code,
        (None, Some(signal)) => 128 + signal,
        (None, None) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_identifiers_are_hex_and_unique() {
        let first = random_hex(8).expect("random");
        let second = random_hex(8).expect("random");
        assert_eq!(first.len(), 16);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn exit_codes_follow_the_shell_convention() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(0)), 0);
        // Exit status 3 is encoded in the high byte.
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(
            exit_code(std::process::ExitStatus::from_raw(libc::SIGTERM)),
            128 + libc::SIGTERM
        );
    }

    #[test]
    fn identity_is_readable() {
        // Only asserts the calls are wired up; values depend on the environment.
        let _ = euid();
        let _ = egid();
    }
}
