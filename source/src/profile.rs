//! What a container read last time, so the next run can fetch it before it is
//! asked for.
//!
//! A served rootfs pays for a file when something opens it, one span at a time
//! and one after another, so a container that reads a hundred files waits a
//! hundred times. The list of what it read is nearly the same on every run, and
//! a recorded one turns those waits into work that has already happened.
//!
//! Profiles are written by a recording run and read by every run after it, so
//! the file is a source file: sorted, one path a line, and merged rather than
//! replaced so that repeated recordings accumulate instead of fighting. What a
//! container reads depends on which manifest it ran, so the platform is in the
//! header and in the file name; two platforms recorded to one base name do not
//! meet.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{Error, IoContext, Result};
use crate::image::Platform;

/// Bumped when a reader can no longer make sense of an older file.
const VERSION: u32 = 1;

const MAGIC: &str = "oci-runtime-profile";

/// What every profile file ends with, whatever platform it describes.
pub const SUFFIX: &str = ".profile";

/// The rootfs itself, as the entry tables spell it.
const ROOT_ENTRY: &[u8] = b".";

/// How often a path has been read, and how early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// Recording runs that read this path. A path that only one run in ten
    /// wants is still worth fetching, but this is what says so.
    pub hits: u64,
    /// Mean position in the order the recording runs reached it. Fetching
    /// follows this, so the file the container blocks on first is fetched
    /// first.
    pub rank: u64,
}

#[derive(Debug, Clone)]
pub struct Profile {
    /// The image platform this was recorded against, in `os/arch` spelling.
    platform: String,
    /// The image configuration the recording ran, which is what a registry
    /// calls the image ID. Provenance: an image is rebuilt far more often than
    /// what it holds changes, so a difference here is worth reporting and not
    /// worth failing over.
    image: String,
    runs: u64,
    /// Keyed by the absolute path, which is both what is written and what
    /// sorts the file.
    entries: BTreeMap<Vec<u8>, Entry>,
}

impl Profile {
    pub fn empty(platform: &Platform) -> Profile {
        Profile {
            platform: platform.to_string(),
            image: String::new(),
            runs: 0,
            entries: BTreeMap::new(),
        }
    }

    /// Reads a profile, or `None` when there is not one there yet. A recording
    /// run has to be able to create the file it merges into.
    pub fn read(path: &Utf8Path) -> Result<Option<Profile>> {
        let text = match std::fs::read(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::io(format!("reading profile {path}"), err)),
        };
        Profile::parse(&text, path).map(Some)
    }

    pub fn parse(text: &[u8], path: &Utf8Path) -> Result<Profile> {
        let fail = |message: String| Error::Profile {
            path: path.to_string(),
            message,
        };
        let mut profile = Profile {
            platform: String::new(),
            image: String::new(),
            runs: 0,
            entries: BTreeMap::new(),
        };
        let mut version = None;
        for (number, line) in text.split(|&byte| byte == b'\n').enumerate() {
            let line = strip_suffix(line, b'\r');
            let at = |message: &str| fail(format!("line {}: {message}", number + 1));
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix(b"#") {
                let rest = String::from_utf8_lossy(rest);
                if let Some(declared) = rest.trim().strip_prefix(MAGIC) {
                    version = Some(
                        declared
                            .trim()
                            .parse::<u32>()
                            .map_err(|_| at("unreadable format version"))?,
                    );
                }
                continue;
            }
            if version.is_none() {
                return Err(fail(format!("does not begin with `# {MAGIC} {VERSION}`")));
            }
            let (key, rest) = split_once(line, b' ').ok_or_else(|| at("expected `key value`"))?;
            let value = || String::from_utf8_lossy(rest).trim().to_string();
            match key {
                b"platform" => profile.platform = value(),
                b"image" => profile.image = value(),
                b"runs" => {
                    profile.runs = value().parse().map_err(|_| at("unreadable run count"))?;
                }
                _ => {
                    let (rank, path) =
                        split_once(rest, b' ').ok_or_else(|| at("expected `hits rank path`"))?;
                    let entry = Entry {
                        hits: number_of(key).ok_or_else(|| at("unreadable hit count"))?,
                        rank: number_of(rank).ok_or_else(|| at("unreadable rank"))?,
                    };
                    let path = unescape(path).ok_or_else(|| at("unreadable path"))?;
                    if !path.starts_with(b"/") {
                        return Err(at("path is not absolute"));
                    }
                    profile.entries.insert(path, entry);
                }
            }
        }
        match version {
            None => Err(fail(format!("does not begin with `# {MAGIC} {VERSION}`"))),
            Some(version) if version > VERSION => Err(fail(format!(
                "is version {version}, and this launcher reads {VERSION}"
            ))),
            Some(_) if profile.platform.is_empty() => Err(fail("names no platform".to_string())),
            Some(_) => Ok(profile),
        }
    }

    /// Adds one run's reads, in the order the container reached them.
    ///
    /// Recording is merged rather than replacing what is there: one run of a
    /// container is one path through it, and a profile that only knows the
    /// last one forgets everything the run before it found.
    pub fn merge_run(&mut self, image: &str, order: &[Vec<u8>]) {
        for (index, path) in order.iter().enumerate() {
            let index = index as u64;
            self.entries
                .entry(path.clone())
                .and_modify(|entry| {
                    entry.rank = (entry.rank * entry.hits + index) / (entry.hits + 1);
                    entry.hits += 1;
                })
                .or_insert(Entry {
                    hits: 1,
                    rank: index,
                });
        }
        self.runs += 1;
        self.image = image.to_string();
    }

    /// The paths to fetch, first read first.
    pub fn ordered(&self) -> Vec<&[u8]> {
        let mut paths: Vec<(&Entry, &[u8])> = self
            .entries
            .iter()
            .map(|(path, entry)| (entry, path.as_slice()))
            .collect();
        paths.sort_by_key(|(entry, path)| (entry.rank, *path));
        paths.into_iter().map(|(_, path)| path).collect()
    }

    pub fn paths(&self) -> impl Iterator<Item = &[u8]> {
        self.entries.keys().map(Vec::as_slice)
    }

    pub fn platform(&self) -> &str {
        &self.platform
    }

    pub fn image(&self) -> &str {
        &self.image
    }

    pub fn runs(&self) -> u64 {
        self.runs
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Writes the profile where a reviewer can read the diff: header first,
    /// then a line per path in path order, whatever order they are fetched in.
    ///
    /// Written beside the destination and renamed over it, so a run
    /// interrupted half way through leaves the profile it started with.
    pub fn write(&self, path: &Utf8Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).io_context(|| format!("creating {parent}"))?;
        }
        let staged = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&staged, self.render()).io_context(|| format!("writing {staged}"))?;
        std::fs::rename(&staged, path).io_context(|| format!("writing {path}"))
    }

    fn render(&self) -> String {
        let mut out = format!("# {MAGIC} {VERSION}\n");
        let _ = writeln!(out, "platform {}", self.platform);
        if !self.image.is_empty() {
            let _ = writeln!(out, "image {}", self.image);
        }
        let _ = writeln!(out, "runs {}", self.runs);
        for (path, entry) in &self.entries {
            let _ = writeln!(out, "{} {} {}", entry.hits, entry.rank, escape(path));
        }
        out
    }
}

/// Where a recording for `platform` goes, given the base name a rule or a
/// caller chose.
///
/// The qualifier is what stops a recording made on one platform from landing
/// on another's: the two ran different manifests and read different files.
pub fn qualified(base: &Utf8Path, platform: &Platform) -> Utf8PathBuf {
    let mut name = format!(
        "{}.{}-{}",
        base.file_name().unwrap_or_default(),
        platform.os,
        platform.architecture
    );
    if let Some(variant) = platform.variant.as_deref().filter(|v| !v.is_empty()) {
        name.push('-');
        name.push_str(variant);
    }
    name.push_str(SUFFIX);
    base.with_file_name(name)
}

/// The platform a profile file name claims, or `None` when it is not named
/// like one.
pub fn platform_of(path: &Utf8Path) -> Option<String> {
    let name = path.file_name()?.strip_suffix(SUFFIX)?;
    let (_, qualifier) = name.rsplit_once('.')?;
    let (os, rest) = qualifier.split_once('-')?;
    let (architecture, variant) = match rest.split_once('-') {
        Some((architecture, variant)) => (architecture, Some(variant)),
        None => (rest, None),
    };
    Some(
        Platform {
            os: os.to_string(),
            architecture: architecture.to_string(),
            variant: variant.map(str::to_string),
        }
        .to_string(),
    )
}

/// A profile path as the entry tables spell it: relative to the rootfs, with
/// the rootfs itself spelled `.`.
pub fn to_entry_path(path: &[u8]) -> Vec<u8> {
    match path.strip_prefix(b"/") {
        Some(rest) if rest.is_empty() => ROOT_ENTRY.to_vec(),
        Some(rest) => rest.to_vec(),
        None => path.to_vec(),
    }
}

/// An entry table path as a profile spells it: absolute, which is how the
/// container named it and how a reader recognises it.
pub fn from_entry_path(path: &[u8]) -> Vec<u8> {
    if path == ROOT_ENTRY {
        return b"/".to_vec();
    }
    let mut out = Vec::with_capacity(path.len() + 1);
    out.push(b'/');
    out.extend_from_slice(path);
    out
}

/// Renders a path as one line of a text file.
///
/// Paths are bytes and may hold anything but `/` and NUL, so the escaping has
/// to survive a round trip through a line. Valid UTF-8 is left alone beyond
/// the characters that would break the format, since a profile nobody can read
/// is a profile nobody reviews.
fn escape(path: &[u8]) -> String {
    let utf8 = std::str::from_utf8(path).is_ok();
    let mut out: Vec<u8> = Vec::with_capacity(path.len());
    for &byte in path {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0x20..=0x7e => out.push(byte),
            // Whole multi-byte characters, pushed as they came.
            _ if utf8 && byte >= 0x80 => out.push(byte),
            _ => out.extend_from_slice(format!("\\x{byte:02x}").as_bytes()),
        }
    }
    // Everything added above is either ASCII or a byte of the valid UTF-8 that
    // was passed through whole.
    String::from_utf8(out).expect("escaped paths are UTF-8")
}

/// Undoes [`escape`].
fn unescape(line: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(line.len());
    let mut bytes = line.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        match bytes.next()? {
            b'\\' => out.push(b'\\'),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b'x' => {
                let hex = [bytes.next()?, bytes.next()?];
                let hex = std::str::from_utf8(&hex).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
            }
            _ => return None,
        }
    }
    Some(out)
}

fn number_of(bytes: &[u8]) -> Option<u64> {
    std::str::from_utf8(bytes).ok()?.trim().parse().ok()
}

fn split_once(line: &[u8], separator: u8) -> Option<(&[u8], &[u8])> {
    let at = line.iter().position(|&byte| byte == separator)?;
    Some((&line[..at], &line[at + 1..]))
}

fn strip_suffix(line: &[u8], byte: u8) -> &[u8] {
    match line.split_last() {
        Some((&last, rest)) if last == byte => rest,
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform() -> Platform {
        Platform {
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
            variant: None,
        }
    }

    fn parse(text: &str) -> Result<Profile> {
        Profile::parse(text.as_bytes(), Utf8Path::new("p.linux-amd64.profile"))
    }

    #[test]
    fn a_recorded_run_round_trips() {
        let mut profile = Profile::empty(&platform());
        profile.merge_run("sha256:aa", &[b"/bin/sh".to_vec(), b"/etc/passwd".to_vec()]);
        let parsed = parse(&profile.render()).expect("profile");
        assert_eq!(parsed.platform(), "linux/amd64");
        assert_eq!(parsed.image(), "sha256:aa");
        assert_eq!(parsed.runs(), 1);
        assert_eq!(parsed.ordered(), [b"/bin/sh".as_slice(), b"/etc/passwd"]);
    }

    #[test]
    fn entries_are_written_in_path_order_and_fetched_in_read_order() {
        let mut profile = Profile::empty(&platform());
        profile.merge_run("", &[b"/z".to_vec(), b"/a".to_vec()]);
        let rendered = profile.render();
        let written: Vec<&str> = rendered
            .lines()
            .filter(|line| line.starts_with('1'))
            .collect();
        assert_eq!(written, ["1 1 /a", "1 0 /z"]);
        assert_eq!(profile.ordered(), [b"/z".as_slice(), b"/a"]);
    }

    #[test]
    fn merging_counts_runs_and_averages_the_rank() {
        let mut profile = Profile::empty(&platform());
        profile.merge_run("", &[b"/a".to_vec(), b"/b".to_vec()]);
        profile.merge_run("", &[b"/b".to_vec(), b"/c".to_vec()]);
        assert_eq!(profile.runs(), 2);
        // `/b` was second, then first: reached before `/c`, which one run
        // never asked for at all.
        assert_eq!(profile.ordered(), [b"/a".as_slice(), b"/b", b"/c"]);
        assert_eq!(
            profile.entries[b"/b".as_slice()],
            Entry { hits: 2, rank: 0 }
        );
        assert_eq!(
            profile.entries[b"/a".as_slice()],
            Entry { hits: 1, rank: 0 }
        );
    }

    #[test]
    fn awkward_paths_survive_a_line() {
        let awkward: Vec<Vec<u8>> = vec![
            b"/a path/with spaces".to_vec(),
            b"/back\\slash".to_vec(),
            b"/new\nline".to_vec(),
            b"/caf\xc3\xa9/menu".to_vec(),
            vec![b'/', 0xff, 0xfe],
        ];
        let mut profile = Profile::empty(&platform());
        profile.merge_run("", &awkward);
        let parsed = parse(&profile.render()).expect("profile");
        let mut expected = awkward.clone();
        expected.sort();
        let read: Vec<Vec<u8>> = parsed.paths().map(<[u8]>::to_vec).collect();
        assert_eq!(read, expected);
    }

    #[test]
    fn a_file_that_is_not_a_profile_is_refused() {
        assert!(parse("/bin/sh\n").is_err());
        assert!(parse("# oci-runtime-profile 99\nplatform linux/amd64\n").is_err());
        assert!(parse("# oci-runtime-profile 1\n1 0 /bin/sh\n").is_err());
        assert!(parse("# oci-runtime-profile 1\nplatform linux/amd64\n1 0 bin/sh\n").is_err());
    }

    #[test]
    fn the_file_name_carries_the_platform() {
        let arm = Platform {
            os: "linux".to_string(),
            architecture: "arm".to_string(),
            variant: Some("v7".to_string()),
        };
        assert_eq!(
            qualified(Utf8Path::new("profiles/container"), &platform()),
            "profiles/container.linux-amd64.profile"
        );
        assert_eq!(
            qualified(Utf8Path::new("profiles/container"), &arm),
            "profiles/container.linux-arm-v7.profile"
        );
        assert_eq!(
            platform_of(Utf8Path::new("profiles/container.linux-amd64.profile")).as_deref(),
            Some("linux/amd64")
        );
        assert_eq!(
            platform_of(Utf8Path::new("profiles/container.linux-arm-v7.profile")).as_deref(),
            Some("linux/arm/v7")
        );
        assert_eq!(platform_of(Utf8Path::new("container.profile")), None);
    }

    #[test]
    fn profile_paths_and_entry_paths_convert_both_ways() {
        assert_eq!(to_entry_path(b"/etc/passwd"), b"etc/passwd");
        assert_eq!(to_entry_path(b"/"), b".");
        assert_eq!(from_entry_path(b"etc/passwd"), b"/etc/passwd");
        assert_eq!(from_entry_path(b"."), b"/");
    }
}
