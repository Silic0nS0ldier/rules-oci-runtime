//! The files recorded beside a layer at build time: a checkpoint index and an
//! entry table, named after the layer's digest.
//!
//! Both are optimisations. Whoever ends up reading the layer decides what it
//! actually contains, so a sidecar that is missing, unreadable or not what it
//! claims means falling back to reading the layer rather than failing.
//!
//! The names live here because `oci_runtime index` writes them and extraction
//! reads them, and a convention spelled out in both places is one that can
//! quietly stop matching: nothing fails, the sidecars are simply never found.

use std::fs::File;
use std::io::{self, BufReader};

use camino::{Utf8Path, Utf8PathBuf};

use crate::log::warning;

/// Where a layer's checkpoint index goes.
pub fn checkpoints_at(dir: &Utf8Path, hex: &str) -> Utf8PathBuf {
    dir.join(format!("{hex}.zinfo"))
}

/// Where a layer's entry table goes.
pub fn entries_at(dir: &Utf8Path, hex: &str) -> Utf8PathBuf {
    dir.join(format!("{hex}.entries"))
}

/// Reads a sidecar, or `None` when there is not a usable one.
///
/// One that is not there is ordinary: an image may simply not have been
/// indexed. One that is there and will not read is worth saying out loud,
/// since something built it and it is not being used.
pub fn read<T>(path: &Utf8Path, parse: impl FnOnce(BufReader<File>) -> io::Result<T>) -> Option<T> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            warning!("ignoring {path}: {err}");
            return None;
        }
    };
    match parse(BufReader::new(file)) {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            warning!("ignoring {path}: {err}");
            None
        }
    }
}
