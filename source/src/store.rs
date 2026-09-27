//! A content-addressed store of file bodies, shared by every launch of the
//! same user on the same host.
//!
//! An object is named by the SHA-256 of what it holds and is never written
//! once it has a name: it is written into an unnamed `O_TMPFILE`, checked,
//! and only then linked in. So a name that exists holds the bytes it names,
//! two launches publishing the same body only cost the loser some CPU, and a
//! launch killed half way leaves nothing behind.
//!
//! The store is optional throughout. A store that cannot be opened, is not
//! the caller's alone, or turns out not to be writable leaves the launch doing
//! what it did before, for as much as it cannot share.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};

use crate::cli::CacheMode;
use crate::log::{log, warning};

/// The layout version. A new one starts from an empty store.
const LAYOUT: &str = "v1";

/// How stale an object's atime may be before a hit refreshes it. Collection
/// goes by atime, and many mounts and read paths never update it themselves.
const TOUCH_AFTER_SECS: i64 = 24 * 60 * 60;

const OBJECTS: &str = "objects";
const BUNDLES: &str = "bundles";

/// How many bundles one launch looks at when sweeping, so that no launch pays
/// for a backlog.
const SWEEP_LIMIT: usize = 16;

/// A bundle with no lock file is one a launch died making, or one being
/// removed. Only the first is worth sweeping, and it has to be old to be sure.
const UNLOCKED_AFTER_SECS: i64 = 24 * 60 * 60;

pub struct Store {
    root_path: Utf8PathBuf,
    root: OwnedFd,
    objects: OwnedFd,
    shards: Vec<OnceLock<OwnedFd>>,
    writable: AtomicBool,
    publishing: AtomicBool,
    /// Why the store is not writable, said once.
    explained: AtomicBool,
    hits: AtomicU64,
    published: AtomicU64,
    lost_races: AtomicU64,
}

impl Store {
    /// Opens the store `mode` asks for, or `None` when the launch runs
    /// without one.
    pub fn open(mode: CacheMode, dir: Option<&Utf8Path>) -> Option<Store> {
        if mode == CacheMode::Off {
            log!("The content cache is off");
            return None;
        }
        let (root, named) = match dir {
            Some(dir) => (dir.to_owned(), true),
            None => (default_root()?, false),
        };
        let mut writable = mode == CacheMode::Auto;
        if writable && !named && under_temporary(&root) {
            log!("Not writing to the content cache at {root}: nothing written there is read again");
            writable = false;
        }
        match Store::open_at(root.clone(), writable) {
            Ok(store) => Some(store),
            Err(reason) => {
                warning!("not using the content cache at {root}: {reason}");
                None
            }
        }
    }

    fn open_at(root_path: Utf8PathBuf, mut writable: bool) -> Result<Store, String> {
        // A scope that cannot create the store may still be able to read one.
        if let Err(err) = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root_path)
        {
            log!("Could not create {root_path}: {err}");
        }
        let root = open_dir(&root_path).map_err(|err| err.to_string())?;

        let meta = File::from(root.try_clone().map_err(|err| err.to_string())?)
            .metadata()
            .map_err(|err| err.to_string())?;
        if meta.uid() != crate::sys::euid() {
            return Err(format!("it is owned by uid {}", meta.uid()));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(format!(
                "its mode is {:o}, and it must be 0700",
                meta.mode() & 0o777
            ));
        }
        if on_nfs(&root) {
            return Err("it is on NFS, which cannot be trusted with it".to_string());
        }

        if writable && let Err(err) = make_dir(root.as_raw_fd(), OBJECTS) {
            log!("The content cache at {root_path} cannot be written: {err}");
            writable = false;
        }
        let objects = open_beneath(root.as_raw_fd(), OBJECTS, libc::O_DIRECTORY, 0)
            .map_err(|err| format!("opening {OBJECTS}: {err}"))?;
        log!(
            "Using the content cache at {root_path} ({})",
            if writable { "read-write" } else { "read-only" }
        );
        Ok(Store {
            root_path,
            root,
            objects,
            shards: (0..256).map(|_| OnceLock::new()).collect(),
            writable: AtomicBool::new(writable),
            publishing: AtomicBool::new(true),
            explained: AtomicBool::new(false),
            hits: AtomicU64::new(0),
            published: AtomicU64::new(0),
            lost_races: AtomicU64::new(0),
        })
    }

    pub fn is_writable(&self) -> bool {
        self.writable.load(Ordering::Relaxed)
    }

    /// Where bundles go while the store is writable, which puts them on the
    /// same mount as the objects they are cloned from. Sweeps it on the way.
    pub fn bundles(&self) -> Option<Utf8PathBuf> {
        if !self.is_writable() {
            return None;
        }
        if let Err(err) = make_dir(self.root.as_raw_fd(), BUNDLES) {
            self.refuse(err);
            return None;
        }
        let dir = self.root_path.join(BUNDLES);
        sweep(&dir, SWEEP_LIMIT);
        Some(dir)
    }

    /// The object holding `size` bytes named `sha256`, or `None` for a miss.
    /// An object of the wrong size is one a crash emptied, and misses too.
    pub fn lookup(&self, sha256: &[u8; 32], size: u64) -> Option<File> {
        let (shard, name) = split(sha256);
        let dir = self.shard(shard, false)?;
        let file = File::from(open_beneath(dir, &name, libc::O_RDONLY | libc::O_NOFOLLOW, 0).ok()?);
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.len() != size {
            return None;
        }
        if self.is_writable() && now() - meta.atime() > TOUCH_AFTER_SECS {
            touch(&file);
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(file)
    }

    /// Publishes `bytes` as the object `sha256`, which the caller has checked
    /// they are. Returns the unnamed file they were written to, published or
    /// not, or `None` when the store could not take them at all.
    ///
    /// Whoever links first wins, and what they linked is these same bytes, so
    /// the file in hand is as good as the published one either way.
    pub fn publish(&self, sha256: &[u8; 32], bytes: &[u8]) -> Option<File> {
        if !self.is_writable() || !self.publishing.load(Ordering::Relaxed) {
            return None;
        }
        let (shard, name) = split(sha256);
        let dir = self.shard(shard, true)?;
        let mut file = match open_beneath(dir, ".", libc::O_TMPFILE | libc::O_RDWR, 0o444) {
            Ok(fd) => File::from(fd),
            Err(err) => {
                self.refuse(err);
                return None;
            }
        };
        if let Err(err) = file.write_all(bytes) {
            self.refuse(err);
            return None;
        }

        let from = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()?;
        let to = CString::new(name).ok()?;
        // SAFETY: both strings are NUL terminated and outlive the call; the
        // directory descriptor is open.
        let linked = unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                from.as_ptr(),
                dir,
                to.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        };
        if linked == 0 {
            self.published.fetch_add(1, Ordering::Relaxed);
        } else {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EEXIST) => {
                    self.lost_races.fetch_add(1, Ordering::Relaxed);
                }
                _ => self.refuse(err),
            }
        }
        Some(file)
    }

    /// Stops writing, for this launch, what the store will not take.
    fn refuse(&self, err: io::Error) {
        let read_only = matches!(
            err.raw_os_error(),
            Some(libc::EROFS | libc::EACCES | libc::EPERM | libc::EOPNOTSUPP | libc::EISDIR)
        );
        if read_only {
            self.writable.store(false, Ordering::Relaxed);
        } else {
            self.publishing.store(false, Ordering::Relaxed);
        }
        if self.explained.swap(true, Ordering::Relaxed) {
            return;
        }
        let expected = read_only
            || matches!(
                err.raw_os_error(),
                Some(libc::ENOSPC | libc::EDQUOT | libc::ENOENT)
            );
        if !expected {
            warning!(
                "not publishing to the content cache at {}: {err}",
                self.root_path
            );
        } else if read_only {
            log!(
                "The content cache at {} is read-only here ({err}); under Bazel's sandbox, \
                 --sandbox_writable_path={} makes it writable",
                self.root_path,
                self.root_path
            );
        } else {
            log!(
                "Not publishing to the content cache at {}: {err}",
                self.root_path
            );
        }
    }

    /// The shard directory for objects starting with `byte`, made on first use
    /// when `create` asks for it.
    fn shard(&self, byte: u8, create: bool) -> Option<RawFd> {
        let slot = &self.shards[byte as usize];
        if let Some(fd) = slot.get() {
            return Some(fd.as_raw_fd());
        }
        let name = format!("{byte:02x}");
        if create && let Err(err) = make_dir(self.objects.as_raw_fd(), &name) {
            self.refuse(err);
            return None;
        }
        let fd = open_beneath(self.objects.as_raw_fd(), &name, libc::O_DIRECTORY, 0).ok()?;
        Some(slot.get_or_init(|| fd).as_raw_fd())
    }

    /// What this launch got out of the store, for `--verbose`.
    pub fn report(&self) {
        log!(
            "The content cache at {} ({}): {} hits, {} published, {} lost to another launch",
            self.root_path,
            if self.is_writable() {
                "read-write"
            } else {
                "read-only"
            },
            self.hits.load(Ordering::Relaxed),
            self.published.load(Ordering::Relaxed),
            self.lost_races.load(Ordering::Relaxed),
        );
    }
}

/// True when `bytes` are what `sha256` names.
pub fn holds(sha256: &[u8; 32], bytes: &[u8]) -> bool {
    Sha256::digest(bytes).as_slice() == sha256
}

/// Removes up to `limit` bundles no launch holds the lock of.
///
/// Nothing else cleans up after a launch that was killed, or a remover the
/// sandbox took down with it, the way `/tmp` is cleaned. Two launches may
/// sweep the same bundle at once, and whichever gets there second finds
/// nothing to remove.
fn sweep(dir: &Utf8Path, limit: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut swept = 0;
    for entry in entries.flatten().take(limit) {
        let path = entry.path();
        let lock = match File::open(path.join(crate::bundle::LOCK)) {
            Ok(lock) => lock,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let old = entry
                    .metadata()
                    .is_ok_and(|meta| now() - meta.mtime() > UNLOCKED_AFTER_SECS);
                if old && crate::fsutil::force_remove_dir_all(&path).is_ok() {
                    swept += 1;
                }
                continue;
            }
            Err(_) => continue,
        };
        // SAFETY: the descriptor is open for the duration of the call.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            continue;
        }
        if crate::fsutil::force_remove_dir_all(&path).is_ok() {
            swept += 1;
        }
    }
    if swept > 0 {
        log!("Removed {swept} bundles left behind in {dir}");
    }
}

fn split(sha256: &[u8; 32]) -> (u8, String) {
    (sha256[0], crate::image::hex_encode(sha256))
}

/// `$HOME` and `$XDG_CACHE_HOME` are ignored: Bazel's test runner repoints the
/// first and strips the second, and the store is only worth anything if every
/// scope finds the same one.
fn default_root() -> Option<Utf8PathBuf> {
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 16 << 10];
    let mut found = std::ptr::null_mut();
    // SAFETY: every pointer is to storage that outlives the call, and the
    // buffer length is the one given.
    let status = unsafe {
        libc::getpwuid_r(
            crate::sys::euid(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() || entry.pw_dir.is_null() {
        log!("Not using the content cache: this user has no home directory");
        return None;
    }
    // SAFETY: getpwuid_r succeeded, so pw_dir points into `buffer` at a NUL
    // terminated string.
    let home = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
    let home = Utf8PathBuf::from(home.to_str().ok()?);
    Some(home.join(".cache/rules_oci_runtime").join(LAYOUT))
}

fn under_temporary(root: &Utf8Path) -> bool {
    ["TEST_TMPDIR", "TMPDIR"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .filter(|dir| !dir.is_empty())
        .any(|dir| root.starts_with(dir))
}

fn open_dir(path: &Utf8Path) -> io::Result<OwnedFd> {
    let path = CString::new(path.as_str())?;
    // SAFETY: the path is NUL terminated and outlives the call.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor was just opened and is owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn on_nfs(dir: &OwnedFd) -> bool {
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open and `stat` is the struct fstatfs fills.
    let status = unsafe { libc::fstatfs(dir.as_raw_fd(), &mut stat) };
    status == 0 && stat.f_type as i64 == libc::NFS_SUPER_MAGIC as i64
}

/// Makes a directory the store holds, content that it was already there.
fn make_dir(parent: RawFd, name: &str) -> io::Result<()> {
    let name = CString::new(name)?;
    // SAFETY: the name is NUL terminated and the descriptor is open.
    if unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) } == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EEXIST) => Ok(()),
        _ => Err(err),
    }
}

/// Opens `path` under `dir` without following a symlink anywhere on the way,
/// so nothing in the store can send a launch outside it.
fn open_beneath(dir: RawFd, path: &str, flags: libc::c_int, mode: u32) -> io::Result<OwnedFd> {
    let path = CString::new(path)?;
    // SAFETY: open_how is plain data, and zero is a valid value for it.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (flags | libc::O_CLOEXEC) as u64;
    how.mode = mode as u64;
    how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;
    // SAFETY: the path is NUL terminated, `how` is the size given, and both
    // outlive the call.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir,
            path.as_ptr(),
            &how as *const libc::open_how,
            std::mem::size_of::<libc::open_how>(),
        )
    };
    if fd >= 0 {
        // SAFETY: the descriptor was just opened and is owned by nothing else.
        return Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) });
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::ENOSYS) {
        return Err(err);
    }
    // Before 5.6. The store is the caller's alone, so nothing else can have
    // put a symlink in it.
    // SAFETY: as above, for openat.
    let fd = unsafe {
        libc::openat(
            dir,
            path.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor was just opened and is owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn touch(file: &File) {
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        },
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
    ];
    // SAFETY: the descriptor is open and `times` holds the two values
    // futimens reads.
    let _ = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn scratch(name: &str) -> Utf8PathBuf {
        let dir = Utf8PathBuf::from(std::env::temp_dir().to_str().expect("utf-8 tmpdir"))
            .join(format!("oci-runtime-store-{name}-{}", std::process::id()));
        let _ = crate::fsutil::force_remove_dir_all(dir.as_std_path());
        dir
    }

    fn named(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    fn contents(mut file: &File) -> Vec<u8> {
        let mut read = Vec::new();
        std::io::Seek::rewind(&mut file).expect("rewind");
        file.read_to_end(&mut read).expect("read");
        read
    }

    #[test]
    fn a_published_object_is_found_by_its_hash() {
        let root = scratch("publish");
        let store = Store::open(CacheMode::Auto, Some(&root)).expect("store");
        let sha = named(b"hello");
        assert!(store.lookup(&sha, 5).is_none(), "a cold store misses");

        let file = store.publish(&sha, b"hello").expect("published");
        assert_eq!(contents(&file), b"hello");
        let found = store.lookup(&sha, 5).expect("a hit");
        assert_eq!(contents(&found), b"hello");
        let mode = found.metadata().expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o444, "objects are never writable");
        assert_eq!(store.published.load(Ordering::Relaxed), 1);
        let _ = crate::fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn losing_the_race_still_serves_from_the_unnamed_file() {
        let root = scratch("race");
        let store = Store::open(CacheMode::Auto, Some(&root)).expect("store");
        let sha = named(b"hello");
        store.publish(&sha, b"hello").expect("first");
        let second = store.publish(&sha, b"hello").expect("second");
        assert_eq!(contents(&second), b"hello");
        assert_eq!(store.lost_races.load(Ordering::Relaxed), 1);
        let _ = crate::fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn an_object_of_the_wrong_size_is_a_miss() {
        let root = scratch("size");
        let store = Store::open(CacheMode::Auto, Some(&root)).expect("store");
        let sha = named(b"hello");
        store.publish(&sha, b"hello").expect("published");
        assert!(store.lookup(&sha, 4).is_none());
        let _ = crate::fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn a_read_only_store_is_read_but_never_written() {
        let root = scratch("read-only");
        let sha = named(b"hello");
        Store::open(CacheMode::Auto, Some(&root))
            .expect("store")
            .publish(&sha, b"hello")
            .expect("published");

        let store = Store::open(CacheMode::ReadOnly, Some(&root)).expect("store");
        assert!(store.lookup(&sha, 5).is_some());
        assert!(store.publish(&named(b"other"), b"other").is_none());
        assert!(store.bundles().is_none());
        let _ = crate::fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn a_store_others_can_write_to_is_not_used() {
        let root = scratch("mode");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o770)).expect("chmod");
        assert!(Store::open(CacheMode::Auto, Some(&root)).is_none());
        let _ = crate::fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn an_off_cache_opens_nothing() {
        let root = scratch("off");
        assert!(Store::open(CacheMode::Off, Some(&root)).is_none());
        assert!(!root.exists());
    }

    #[test]
    fn what_the_store_refuses_decides_what_stops() {
        let root = scratch("refuse");
        let store = Store::open(CacheMode::Auto, Some(&root)).expect("store");
        store.refuse(io::Error::from_raw_os_error(libc::ENOSPC));
        assert!(store.is_writable(), "a full store is still read");
        assert!(store.publish(&named(b"x"), b"x").is_none());

        store.refuse(io::Error::from_raw_os_error(libc::EROFS));
        assert!(!store.is_writable());
        let _ = crate::fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn a_hit_refreshes_a_stale_atime_and_leaves_a_fresh_one() {
        let root = scratch("touch");
        let store = Store::open(CacheMode::Auto, Some(&root)).expect("store");
        let sha = named(b"hello");
        let file = store.publish(&sha, b"hello").expect("published");

        let set_atime = |secs: i64| {
            let times = [
                libc::timespec {
                    tv_sec: secs,
                    tv_nsec: 0,
                },
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                },
            ];
            // SAFETY: as in `touch`.
            unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
        };
        let atime = || file.metadata().expect("metadata").atime();

        let recent = now() - 60;
        set_atime(recent);
        store.lookup(&sha, 5).expect("hit");
        assert_eq!(atime(), recent, "a fresh atime costs nothing");

        set_atime(now() - 2 * TOUCH_AFTER_SECS);
        store.lookup(&sha, 5).expect("hit");
        assert!(atime() >= now() - 60, "a stale atime is refreshed");
        let _ = crate::fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn a_body_is_held_to_its_hash() {
        assert!(holds(&named(b"hello"), b"hello"));
        assert!(!holds(&named(b"hello"), b"hellp"));
    }

    #[test]
    fn only_bundles_nothing_holds_are_swept() {
        let root = scratch("sweep");
        let store = Store::open(CacheMode::Auto, Some(&root)).expect("store");
        let dir = store.bundles().expect("bundles");
        let live = crate::bundle::Bundle::create(&dir, "live", false, true).expect("live");
        let dead = crate::bundle::Bundle::create(&dir, "dead", true, true).expect("dead");
        drop(dead);
        std::fs::create_dir_all(dir.join("young-and-unlocked")).expect("unlocked");

        store.bundles().expect("bundles");
        assert!(live.dir().exists(), "a held bundle stays");
        assert!(!dir.join("dead").exists(), "a bundle nothing holds goes");
        assert!(
            dir.join("young-and-unlocked").exists(),
            "a bundle without a lock may be one being made"
        );
        drop(live);
        let _ = crate::fsutil::force_remove_dir_all(root.as_std_path());
    }
}
