//! Serving an image instead of extracting it.
//!
//! Extraction pays for every file in an image before the container runs, and
//! most images are mostly files nothing ever opens. Where the sidecars resolve
//! the image, the same plan that tells the span route what to write tells this
//! what the rootfs holds, and a filesystem answering out of that needs no
//! bytes at all until something reads a file.
//!
//! Everything the kernel needs to serve one is there or it is not, and where
//! it is not the run extracts as it always has. Nothing is half served.
//!
//! # Fetching ahead
//!
//! What a container reads is nearly the same on every run of it, so a recorded
//! list of paths says what to fetch before it asks. [`ahead`] fetches that
//! list, and a recording run writes it down where the kernel asks rather than
//! where a body is fetched, so a run doing both does not record its own
//! guesses as reads.

mod ahead;
mod fs;
mod source;
mod tree;

use std::fs::OpenOptions;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

use camino::Utf8Path;
use fuser::{BackgroundSession, MountOption, Session, SessionACL};

use crate::cli::RootfsMode;
use crate::error::{Error, IoContext, Result};
use crate::extract::RootfsExtractor;
use crate::image::{Descriptor, Layout};
use crate::log::{log, warning};
use crate::profile::Profile;
use crate::sys;

use self::ahead::Ahead;
use self::fs::Recorder;

/// The character device every FUSE mount goes through. Its absence is the
/// clearest sign a host cannot serve one.
const DEVICE: &str = "/dev/fuse";

/// Where a host says whether an unprivileged caller may open a mount to
/// others, which is what auto-unmounting is only offered alongside.
const FUSE_CONF: &str = "/etc/fuse.conf";

/// Serving is request bound rather than throughput bound, and past a point the
/// threads only queue for the same locks.
const MAX_WORKERS: usize = 8;

/// What a run does about the files the container has not asked for yet.
pub struct Fetching<'a> {
    /// The profile to fetch ahead from, where a run was given one.
    pub profile: Option<&'a Profile>,
    /// Whether to write down what the container opens.
    pub record: bool,
    /// How many of the profile's files to fetch before the container starts.
    pub barrier: usize,
}

/// A live mount. Dropping it unmounts, which has to happen before the bundle
/// holding the mount point is taken away.
pub struct Mount {
    session: Option<BackgroundSession>,
    at: PathBuf,
    /// Whether a helper is waiting to take the mount down when this process
    /// goes, which changes who has to take it down when it goes willingly.
    automatic: bool,
    ahead: Option<Ahead>,
    recorder: Option<Arc<Recorder>>,
    served: fs::Rootfs,
}

impl Mount {
    /// Stops fetching ahead, which is called for when the container it was
    /// for has gone.
    pub fn settle(&mut self) {
        if let Some(ahead) = &mut self.ahead {
            ahead.stop();
        }
    }

    /// What the container opened, first opened first, as the entry tables name
    /// it. `None` unless this run was recording.
    pub fn recorded(&self) -> Option<Vec<Vec<u8>>> {
        self.recorder.as_ref().map(|recorder| recorder.recorded())
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        // Nothing may be reading the mount as it goes.
        self.settle();
        log!(
            "The container waited for {} files to be fetched",
            self.served.waited()
        );
        log!("Unmounting {}", self.at.display());
        // The helper behind an auto-unmounting mount waits for this process to
        // exit, and the session will not finish until the mount goes, so
        // leaving it to either of them is a deadlock. Taking the mount down
        // here ends both.
        if self.automatic
            && let Err(err) = unmount(&self.at)
        {
            warning!("could not unmount {}: {err}", self.at.display());
        }
        if let Err(err) = session.umount_and_join() {
            warning!("could not unmount {}: {err}", self.at.display());
        }
    }
}

/// Detaches the mount at `at`, through the setuid helper where the caller may
/// not do it itself.
///
/// Detached rather than plain: the mount is on its way out either way, and
/// what it costs to wait for the last reference to it is a launcher that never
/// returns.
fn unmount(at: &Path) -> std::io::Result<()> {
    let target = std::ffi::CString::new(at.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: the path is a live NUL terminated string.
    if unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) } == 0 {
        return Ok(());
    }
    let refused = std::io::Error::last_os_error();
    if refused.raw_os_error() != Some(libc::EPERM) {
        return Err(refused);
    }
    // Linux refuses every unprivileged caller, however it came by the mount.
    let helper = fusermount().ok_or(refused)?;
    let status = Command::new(helper)
        .args(["-u", "-q", "-z", "--"])
        .arg(at)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("fusermount {status}")))
    }
}

/// The paths, of those given, that this image does not hold as a regular file.
///
/// `None` when the image cannot be resolved at all, which is a different thing
/// from a profile naming files an image does not have: one is a profile that
/// has gone stale, the other an image nothing could have served in the first
/// place.
pub fn absent<'a>(
    extractor: &RootfsExtractor,
    descriptors: &[Descriptor],
    paths: impl Iterator<Item = &'a [u8]>,
) -> Option<Vec<Vec<u8>>> {
    let (plan, work, indexes) = extractor.resolved(descriptors)?;
    drop(indexes);
    let (tree, _, names) = tree::Tree::build(plan.directories(), plan.tables(), work)?;
    let absent = paths
        .filter(|path| {
            !names
                .ino(&crate::profile::to_entry_path(path))
                .and_then(|ino| tree.get(ino))
                .is_some_and(|node| matches!(node.kind, tree::Kind::File(_)))
        })
        .map(<[u8]>::to_vec)
        .collect();
    Some(absent)
}

/// Mounts the image at `rootfs`, or returns `None` when it has to be extracted
/// instead.
///
/// `backing` is where the bytes of the files something actually opens are
/// written. It must be somewhere the container never sees.
pub fn serve(
    mode: RootfsMode,
    rootfs: &Utf8Path,
    backing: &Utf8Path,
    layout: &Layout,
    descriptors: &[Descriptor],
    extractor: &RootfsExtractor,
    fetching: Fetching<'_>,
) -> Result<Option<Mount>> {
    if mode == RootfsMode::Extract {
        return Ok(None);
    }
    // Asked for by name, a host that cannot serve is an error rather than
    // something to quietly work around: the two routes cost wildly different
    // amounts, and a run that silently took the other one measures nothing.
    let refuse = |reason: String| match mode {
        RootfsMode::Fuse => Err(Error::CannotServe(reason)),
        _ => {
            log!("Extracting the image rather than serving it: {reason}");
            Ok(None)
        }
    };

    let Some((plan, work, indexes)) = extractor.resolved(descriptors) else {
        return refuse("the layers have no entry tables or no checkpoint indexes".to_string());
    };
    let Some((tree, bodies, names)) = tree::Tree::build(plan.directories(), plan.tables(), work)
    else {
        return refuse("the resolved image does not describe a whole tree".to_string());
    };
    if let Err(err) = OpenOptions::new().read(true).write(true).open(DEVICE) {
        return refuse(format!("{DEVICE} cannot be opened ({err})"));
    }

    // Nothing here reads a layer whole, so this is the one thing serving has
    // to do eagerly: a blob checked after the container has read from it has
    // not been checked at all.
    let source = source::Source::open(layout, descriptors, indexes)?;
    source.verify(descriptors)?;

    std::fs::create_dir_all(backing).io_context(|| format!("creating {backing}"))?;
    let recorder = fetching
        .record
        .then(|| Arc::new(Recorder::new(names.by_ino(tree.len()))));
    let served = fs::Rootfs::new(
        tree,
        bodies,
        source,
        descriptors.len(),
        backing.as_std_path().to_owned(),
        sys::euid(),
        sys::egid(),
        recorder.clone(),
    );

    // Auto-unmount first, so that a launcher that is killed outright does not
    // leave a mount standing where its bundle used to be.
    let mut session = None;
    let mut refusal = None;
    for (automatic, config) in configurations() {
        match Session::new(served.clone(), rootfs.as_std_path(), &config) {
            Ok(open) => {
                session = Some((open, automatic));
                break;
            }
            Err(err) => refusal = Some(err),
        }
    }
    let Some((session, automatic)) = session else {
        let err = refusal.expect("a mount was attempted");
        return refuse(format!("{rootfs} could not be mounted ({err})"));
    };
    let session = match session.spawn() {
        Ok(session) => session,
        Err(err) => return refuse(format!("the filesystem could not be started ({err})")),
    };
    log!(
        "Serving {} layers at {rootfs} on {} threads",
        descriptors.len(),
        workers()
    );
    if automatic {
        log!("The mount goes away with this process");
    } else {
        log!("Killing this process will leave the mount behind");
    }
    let ahead = fetching.profile.map(|profile| {
        Ahead::start(
            &served,
            &names,
            profile,
            fetching.barrier,
            workers().div_ceil(2),
        )
    });
    Ok(Some(Mount {
        session: Some(session),
        at: rootfs.as_std_path().to_owned(),
        automatic,
        ahead,
        recorder,
        served,
    }))
}

/// The mounts to try, best first, and whether each takes itself down.
///
/// Auto-unmounting hands the mount to a `fusermount3` that outlives this
/// process and takes the mount down when the process goes, however it goes.
/// It is only offered alongside an access control wider than the owner, which
/// in the kernel means `allow_other` and nothing narrower.
///
/// Nothing is given away by that here: the mount point sits inside a bundle
/// directory this process alone can enter, so no other user can reach it
/// whatever the filesystem says.
fn configurations() -> Vec<(bool, fuser::Config)> {
    let plain = || {
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            MountOption::FSName("rules_oci_runtime".to_string()),
            MountOption::DefaultPermissions,
        ];
        config.n_threads = Some(workers());
        config
    };
    let mut attempts = Vec::new();
    if auto_unmountable() {
        let mut config = plain();
        config.mount_options.push(MountOption::AutoUnmount);
        config.acl = SessionACL::All;
        attempts.push((true, config));
    }
    attempts.push((false, plain()));
    attempts
}

/// Whether asking for an auto-unmounting mount is worth the attempt: it goes
/// through `fusermount3`, which has to be there and has to be willing to pass
/// `allow_other` on. Getting this wrong only costs a failed attempt, since the
/// mount is tried again without it.
fn auto_unmountable() -> bool {
    fusermount().is_some() && (sys::euid() == 0 || allows_other())
}

fn fusermount() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FUSERMOUNT_PATH") {
        return Some(PathBuf::from(path));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|directory| {
        ["fusermount3", "fusermount"]
            .into_iter()
            .map(|name| directory.join(name))
            .find(|at| at.is_file())
    })
}

/// `fusermount3` refuses `allow_other` from an unprivileged caller unless the
/// host has said otherwise.
fn allows_other() -> bool {
    let Ok(config) = std::fs::read_to_string(FUSE_CONF) else {
        return false;
    };
    config
        .lines()
        .map(str::trim)
        .any(|line| line == "user_allow_other")
}

fn workers() -> usize {
    thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_WORKERS)
}
