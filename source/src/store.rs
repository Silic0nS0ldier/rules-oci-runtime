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

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::rc::Rc;
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
const CLAIMS: &str = "claims";
const GC_LOCK: &str = "gc.lock";
const GC_CURSOR: &str = "gc.cursor";

/// How often any launch collects, going by when the last one did.
const GC_INTERVAL_SECS: i64 = 60 * 60;

/// How long an object may go unread before it is collected.
const RETENTION_SECS: i64 = 14 * 24 * 60 * 60;

/// Tells stores apart for the descriptors each thread keeps on their claims.
static NEXT_STORE: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// This thread's own descriptor on each store's claims file. OFD locks on
    /// one open file description never conflict with each other, so two
    /// threads sharing one would never see each other's claims.
    static CLAIM_FILES: RefCell<HashMap<u64, Option<Rc<File>>>> = RefCell::new(HashMap::new());
}

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
    id: u64,
    deferred: AtomicU64,
    done_elsewhere: AtomicU64,
    taken_over: AtomicU64,
    helped: AtomicU64,
    found_after_helping: AtomicU64,
}

/// What trying for a claim came to.
pub enum Claim {
    /// This launch has it until the guard goes.
    Acquired(#[allow(dead_code)] ClaimGuard),
    /// Another launch is on it.
    Held,
    /// There is no claiming here, so everyone works alone.
    Unavailable,
}

/// A claim, released when dropped. Held by the thread that took it.
pub struct ClaimGuard {
    file: Rc<File>,
    offset: i64,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        let _ = lock_byte(&self.file, self.offset, libc::F_UNLCK as libc::c_short);
    }
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
            // A scope that can neither make the store nor see one is ordinary,
            // and every sandboxed test would otherwise say so.
            Err(reason) if !root.exists() => {
                log!("Not using the content cache at {root}: {reason}");
                None
            }
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

        // Making a directory that exists answers EEXIST even on a read-only
        // mount, so only opening something for writing says whether it can be.
        let written = make_dir(root.as_raw_fd(), OBJECTS).and_then(|()| {
            open_beneath(
                root.as_raw_fd(),
                CLAIMS,
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW,
                0o600,
            )
        });
        if writable && let Err(err) = written {
            log!(
                "The content cache at {root_path} is read-only here ({err}); under Bazel's \
                 sandbox, --sandbox_writable_path={root_path} makes it writable"
            );
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
            id: NEXT_STORE.fetch_add(1, Ordering::Relaxed),
            deferred: AtomicU64::new(0),
            done_elsewhere: AtomicU64::new(0),
            taken_over: AtomicU64::new(0),
            helped: AtomicU64::new(0),
            found_after_helping: AtomicU64::new(0),
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

    /// Tries once for the claim on inflating span `span` of `layer`.
    ///
    /// A claim is a hint, not what keeps the store right: two launches that
    /// inflate the same span only cost CPU. So nothing ever waits for one.
    pub fn claim(&self, layer: &str, span: u64) -> Claim {
        if !self.is_writable() {
            return Claim::Unavailable;
        }
        let Some(file) = self.claims_file() else {
            return Claim::Unavailable;
        };
        let mut key = Sha256::new();
        key.update(layer.as_bytes());
        key.update(span.to_le_bytes());
        let key = key.finalize();
        let mut offset = [0u8; 8];
        offset[..5].copy_from_slice(&key[..5]);
        let offset = i64::from_le_bytes(offset);
        match lock_byte(&file, offset, libc::F_WRLCK as libc::c_short) {
            Ok(()) => Claim::Acquired(ClaimGuard { file, offset }),
            Err(err) if matches!(err.raw_os_error(), Some(libc::EAGAIN | libc::EACCES)) => {
                Claim::Held
            }
            Err(_) => Claim::Unavailable,
        }
    }

    fn claims_file(&self) -> Option<Rc<File>> {
        CLAIM_FILES.with(|files| {
            files
                .borrow_mut()
                .entry(self.id)
                .or_insert_with(|| {
                    open_beneath(
                        self.root.as_raw_fd(),
                        CLAIMS,
                        libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW,
                        0o600,
                    )
                    .ok()
                    .map(|fd| Rc::new(File::from(fd)))
                })
                .clone()
        })
    }

    /// Counts a unit of work put off because another launch had claimed it.
    pub fn deferred(&self) {
        self.deferred.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a deferred unit that another launch finished in the meantime.
    pub fn done_elsewhere(&self) {
        self.done_elsewhere.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a claimed unit done here anyway, with nothing else left to do.
    pub fn taken_over(&self) {
        self.taken_over.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a span inflated while another launch held the one wanted.
    pub fn helped(&self) {
        self.helped.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a wanted file found in the store after helping.
    pub fn found_after_helping(&self) {
        self.found_after_helping.fetch_add(1, Ordering::Relaxed);
    }

    /// Removes objects nobody has read for the retention period, from one
    /// shard, if no launch has in the last interval.
    ///
    /// Called as a launch ends. One shard a run means no launch ever walks the
    /// whole store, and being killed part way leaves nothing to put right.
    /// Removing an object another launch has open is harmless: its descriptor
    /// keeps working, and a lookup racing the unlink just misses.
    pub fn collect(&self) {
        if !self.is_writable() {
            return;
        }
        let open = |name| {
            open_beneath(
                self.root.as_raw_fd(),
                name,
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW,
                0o600,
            )
            .map(File::from)
        };
        let Ok(lock) = open(GC_LOCK) else {
            return;
        };
        // SAFETY: the descriptor is open for the duration of the call.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return;
        }
        let Ok(cursor) = open(GC_CURSOR) else {
            return;
        };
        let Ok(meta) = cursor.metadata() else {
            return;
        };
        if meta.len() > 0 && now() - meta.mtime() < GC_INTERVAL_SECS {
            return;
        }
        let mut next = [0u8; 2];
        let shard = match std::os::unix::fs::FileExt::read_exact_at(&cursor, &mut next, 0) {
            Ok(()) => std::str::from_utf8(&next)
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .unwrap_or(0),
            Err(_) => 0,
        };

        idle();
        let reaped = self.reap(shard, now() - RETENTION_SECS);
        let _ = std::os::unix::fs::FileExt::write_all_at(
            &cursor,
            format!("{:02x}\n", shard.wrapping_add(1)).as_bytes(),
            0,
        );
        log!(
            "Collected shard {shard:02x} of the content cache: {reaped} objects unread for 14 days"
        );
    }

    /// Unlinks the objects of `shard` last read before `before`. Only regular
    /// files the caller owns, named as an object is, are ever touched.
    fn reap(&self, shard: u8, before: i64) -> usize {
        let Some(dir) = self.shard(shard, false) else {
            return 0;
        };
        let mut reaped = 0;
        for name in names_in(dir) {
            let is_object = name.as_bytes().len() == 64
                && name
                    .as_bytes()
                    .iter()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
            if !is_object {
                continue;
            }
            // SAFETY: stat is plain data, and zero is a valid value for it.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: the name is NUL terminated, `stat` is the struct fstatat
            // fills, and the descriptor is open.
            let found =
                unsafe { libc::fstatat(dir, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) };
            let stale = found == 0
                && stat.st_mode & libc::S_IFMT == libc::S_IFREG
                && stat.st_uid == crate::sys::euid()
                && (stat.st_atime as i64) < before;
            // SAFETY: as above, for unlinkat.
            if stale && unsafe { libc::unlinkat(dir, name.as_ptr(), 0) } == 0 {
                reaped += 1;
            }
        }
        reaped
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
        log!(
            "Claims: {} deferred, {} of them done by another launch, {} taken over, \
             {} helped with while waiting, {} found done after helping",
            self.deferred.load(Ordering::Relaxed),
            self.done_elsewhere.load(Ordering::Relaxed),
            self.taken_over.load(Ordering::Relaxed),
            self.helped.load(Ordering::Relaxed),
            self.found_after_helping.load(Ordering::Relaxed),
        );
    }
}

/// Takes or releases the lock on one byte of `file`, never waiting.
fn lock_byte(file: &File, offset: i64, kind: libc::c_short) -> io::Result<()> {
    // SAFETY: flock is plain data, and zero is a valid value for it.
    let mut range: libc::flock = unsafe { std::mem::zeroed() };
    range.l_type = kind;
    range.l_whence = libc::SEEK_SET as libc::c_short;
    range.l_start = offset;
    range.l_len = 1;
    // SAFETY: the descriptor is open and `range` is the struct F_OFD_SETLK
    // reads.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &range) } == 0 {
        return Ok(());
    }
    Err(io::Error::last_os_error())
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

/// The names in the directory `dir` is open on.
fn names_in(dir: RawFd) -> Vec<CString> {
    // SAFETY: duplicating a descriptor has no preconditions.
    let copy = unsafe { libc::fcntl(dir, libc::F_DUPFD_CLOEXEC, 0) };
    if copy < 0 {
        return Vec::new();
    }
    // SAFETY: the stream takes over the duplicate, and is closed below.
    let stream = unsafe { libc::fdopendir(copy) };
    if stream.is_null() {
        // SAFETY: fdopendir failed, so the duplicate is still ours to close.
        unsafe { libc::close(copy) };
        return Vec::new();
    }
    // The duplicate shares the original's offset, which an earlier read may
    // have left at the end.
    // SAFETY: the stream is open.
    unsafe { libc::rewinddir(stream) };
    let mut names = Vec::new();
    loop {
        // SAFETY: the stream is open; the entry stays valid until the next
        // call, and its name is copied out before then.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: readdir returned an entry whose name is NUL terminated.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        names.push(name.to_owned());
    }
    // SAFETY: the stream is open, and closing it closes the duplicate.
    unsafe { libc::closedir(stream) };
    names
}

/// Collection is nobody's priority, so it runs only when nothing else wants
/// the processor or the disk. This is the launch's last thread of work.
fn idle() {
    // SAFETY: sched_param is plain data; SCHED_IDLE takes a priority of zero.
    let param: libc::sched_param = unsafe { std::mem::zeroed() };
    // SAFETY: zero names the calling thread, and the parameters are valid.
    let _ = unsafe { libc::sched_setscheduler(0, libc::SCHED_IDLE, &param) };
    const IOPRIO_WHO_PROCESS: libc::c_long = 1;
    const IOPRIO_CLASS_IDLE: libc::c_long = 3;
    // SAFETY: ioprio_set takes three integers and changes only this thread.
    let _ = unsafe {
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0 as libc::c_long,
            IOPRIO_CLASS_IDLE << 13,
        )
    };
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
    fn a_store_this_scope_cannot_write_to_is_read_only() {
        // Permissions do not stop root, which is the only way to test this
        // without a read-only mount.
        if crate::sys::euid() == 0 {
            return;
        }
        let root = scratch("unwritable");
        let sha = named(b"hello");
        Store::open(CacheMode::Auto, Some(&root))
            .expect("store")
            .publish(&sha, b"hello")
            .expect("published");
        std::fs::remove_file(root.join(CLAIMS)).expect("claims");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500)).expect("chmod");

        let store = Store::open(CacheMode::Auto, Some(&root)).expect("store");
        assert!(
            !store.is_writable(),
            "a store whose directories exist is not writable for that"
        );
        assert!(store.lookup(&sha, 5).is_some(), "it is still read");
        assert!(store.bundles().is_none());

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).expect("chmod");
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
    fn a_claim_is_held_against_every_other_open_file_description() {
        let root = scratch("claims");
        let store = std::sync::Arc::new(Store::open(CacheMode::Auto, Some(&root)).expect("store"));
        let elsewhere = |store: &std::sync::Arc<Store>, span| {
            let store = store.clone();
            std::thread::spawn(move || match store.claim("sha256:layer", span) {
                Claim::Acquired(_) => "acquired",
                Claim::Held => "held",
                Claim::Unavailable => "unavailable",
            })
            .join()
            .expect("thread")
        };

        let guard = match store.claim("sha256:layer", 3) {
            Claim::Acquired(guard) => guard,
            _ => panic!("a free claim is acquired"),
        };
        assert_eq!(elsewhere(&store, 3), "held");
        assert_eq!(elsewhere(&store, 4), "acquired", "another span is free");
        drop(guard);
        assert_eq!(elsewhere(&store, 3), "acquired", "a released claim is free");

        let read_only = Store::open(CacheMode::ReadOnly, Some(&root)).expect("store");
        assert!(matches!(
            read_only.claim("sha256:layer", 3),
            Claim::Unavailable
        ));
        let _ = crate::fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn collection_reaps_stale_objects_of_one_shard_and_nothing_else() {
        let root = scratch("collect");
        let store = Store::open(CacheMode::Auto, Some(&root)).expect("store");
        let named_in = |shard: u8, n: u8| {
            let mut sha = [n; 32];
            sha[0] = shard;
            sha
        };
        let age = |file: &File, secs: i64| {
            let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64);
            file.set_times(std::fs::FileTimes::new().set_accessed(when))
                .expect("atime");
        };
        let stale = store.publish(&named_in(0xab, 1), b"stale").expect("stale");
        age(&stale, now() - 2 * RETENTION_SECS);
        let fresh = store.publish(&named_in(0xab, 2), b"fresh").expect("fresh");
        let elsewhere = store.publish(&named_in(0xac, 3), b"other").expect("other");
        age(&elsewhere, now() - 2 * RETENTION_SECS);
        let shard = root.join("objects/ab");
        std::fs::write(shard.join("not-an-object"), b"x").expect("foreign");
        std::os::unix::fs::symlink("/etc/passwd", shard.join("f".repeat(64))).expect("symlink");

        std::fs::write(root.join(GC_CURSOR), "ab\n").expect("cursor");
        let cursor = File::open(root.join(GC_CURSOR)).expect("cursor");
        let long_ago = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
        cursor
            .set_times(std::fs::FileTimes::new().set_modified(long_ago))
            .expect("mtime");

        store.collect();
        let object = |sha: &[u8; 32]| shard.join(crate::image::hex_encode(sha));
        assert!(!object(&named_in(0xab, 1)).exists(), "a stale object goes");
        assert!(object(&named_in(0xab, 2)).exists(), "a fresh object stays");
        assert!(
            root.join("objects/ac")
                .join(crate::image::hex_encode(&named_in(0xac, 3)))
                .exists(),
            "another shard waits its turn"
        );
        assert!(shard.join("not-an-object").exists());
        assert!(shard.join("f".repeat(64)).symlink_metadata().is_ok());
        assert_eq!(
            std::fs::read_to_string(root.join(GC_CURSOR)).expect("cursor"),
            "ac\n"
        );

        // Within the interval, nothing is collected at all.
        std::fs::write(root.join(GC_CURSOR), "ac\n").expect("cursor");
        store.collect();
        assert!(
            root.join("objects/ac")
                .join(crate::image::hex_encode(&named_in(0xac, 3)))
                .exists()
        );
        drop((stale, fresh, elsewhere));
        let _ = crate::fsutil::force_remove_dir_all(root.as_std_path());
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
