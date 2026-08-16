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
//! list of paths would say what to fetch before it asks. There is nothing to
//! build for that beyond the recording and the list: [`fs::Rootfs`] resolves a
//! path to an inode through the same tree `lookup` uses, and fetching one is
//! safe from any thread and already reaches the whole span the file is in. A
//! profile is therefore a walk of paths handed to the same fetch a read takes,
//! on a pool, between the mount going up here and the bundle reaching runc.

mod fs;
mod source;
mod tree;

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::thread;

use camino::Utf8Path;
use fuser::{BackgroundSession, MountOption, Session};

use crate::cli::RootfsMode;
use crate::error::{Error, IoContext, Result};
use crate::extract::RootfsExtractor;
use crate::image::{Descriptor, Layout};
use crate::log::{log, warning};
use crate::sys;

/// The character device every FUSE mount goes through. Its absence is the
/// clearest sign a host cannot serve one.
const DEVICE: &str = "/dev/fuse";

/// Serving is request bound rather than throughput bound, and past a point the
/// threads only queue for the same locks.
const MAX_WORKERS: usize = 8;

/// A live mount. Dropping it unmounts, which has to happen before the bundle
/// holding the mount point is taken away.
pub struct Mount {
    session: Option<BackgroundSession>,
    at: PathBuf,
}

impl Drop for Mount {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        log!("Unmounting {}", self.at.display());
        if let Err(err) = session.umount_and_join() {
            warning!("could not unmount {}: {err}", self.at.display());
        }
    }
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
    let Some((tree, bodies)) = tree::Tree::build(plan.directories(), plan.tables(), work) else {
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
    let served = fs::Rootfs::new(
        tree,
        bodies,
        source,
        descriptors.len(),
        backing.as_std_path().to_owned(),
        sys::euid(),
        sys::egid(),
    );

    let mut config = fuser::Config::default();
    config.mount_options = vec![
        MountOption::FSName("rules_oci_runtime".to_string()),
        MountOption::DefaultPermissions,
    ];
    config.n_threads = Some(workers());

    let session = match Session::new(served, rootfs.as_std_path(), &config) {
        Ok(session) => session,
        Err(err) => return refuse(format!("{rootfs} could not be mounted ({err})")),
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
    Ok(Some(Mount {
        session: Some(session),
        at: rootfs.as_std_path().to_owned(),
    }))
}

fn workers() -> usize {
    thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_WORKERS)
}
