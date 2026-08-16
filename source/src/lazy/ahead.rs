//! Fetching what the container is about to read, before it asks.
//!
//! A served file costs a span of inflating the first time something opens it,
//! and the container waits for all of it. What it opens is nearly the same on
//! every run, so a recorded list of paths is a list of waits that can happen
//! ahead of time instead, on threads of their own.
//!
//! Ahead of time is not the same as up front. The container starts as soon as
//! the file it blocks on first is there, and the rest is fetched underneath it
//! while it runs; a profile is a guess, and the reads that disprove it must not
//! queue behind it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};

use super::fs::Rootfs;
use super::tree::Names;
use crate::log::log;
use crate::profile::{self, Profile};

/// Fetching ahead running on this thread until it has done enough for the
/// container to start, and on its own threads after that.
pub struct Ahead {
    stop: Arc<AtomicBool>,
    fetched: Arc<AtomicUsize>,
    wanted: usize,
    workers: Vec<JoinHandle<()>>,
}

impl Ahead {
    /// Fetches the first `barrier` files the profile names, then hands the
    /// rest to `workers` threads and returns.
    pub fn start(
        rootfs: &Rootfs,
        names: &Names,
        profile: &Profile,
        barrier: usize,
        workers: usize,
    ) -> Ahead {
        let (order, missing) = resolve(profile, names);
        if missing > 0 {
            log!("{missing} of the profile's files are not in this image");
        }
        let stop = Arc::new(AtomicBool::new(false));
        let fetched = Arc::new(AtomicUsize::new(0));
        let wanted = order.len();

        // The container is held up by exactly this much, and a fetch reaches
        // the whole span its file is in, so one entry is usually the whole of
        // the first read.
        for &ino in order.iter().take(barrier) {
            rootfs.fetch_ahead(ino);
            fetched.fetch_add(1, Ordering::Relaxed);
        }
        log!(
            "Fetching {} files ahead of the container, {} of them before it starts",
            wanted,
            barrier.min(wanted)
        );

        let order = Arc::new(order);
        let cursor = Arc::new(AtomicUsize::new(barrier));
        let workers = (0..workers.min(wanted.saturating_sub(barrier)))
            .map(|_| {
                let rootfs = rootfs.clone();
                let order = order.clone();
                let cursor = cursor.clone();
                let stop = stop.clone();
                let fetched = fetched.clone();
                thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let at = cursor.fetch_add(1, Ordering::Relaxed);
                        let Some(&ino) = order.get(at) else {
                            break;
                        };
                        rootfs.fetch_ahead(ino);
                        fetched.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();

        Ahead {
            stop,
            fetched,
            wanted,
            workers,
        }
    }

    /// Gives up on whatever is left.
    ///
    /// The container is what this was for, so once it has gone the rest is
    /// work nobody is waiting for: a run that failed a second in should not
    /// spend a minute finishing a fetch of the image.
    pub fn stop(&mut self) {
        if self.workers.is_empty() {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        log!(
            "Fetched {} of the {} files the profile named",
            self.fetched.load(Ordering::Relaxed),
            self.wanted
        );
    }
}

impl Drop for Ahead {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The inodes to fetch, first read first, and how many of the profile's paths
/// this image no longer has.
///
/// A profile is recorded against one build of an image and used against the
/// next, so paths do fall out of it. That is what the build time check is for;
/// here it is only worth counting.
fn resolve(profile: &Profile, names: &Names) -> (Vec<u64>, usize) {
    let mut order = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut missing = 0;
    for path in profile.ordered() {
        // Hard linked names are one file, and fetching it twice is fetching
        // its span twice.
        match names.ino(&profile::to_entry_path(path)) {
            Some(ino) if seen.insert(ino) => order.push(ino),
            Some(_) => {}
            None => missing += 1,
        }
    }
    (order, missing)
}
