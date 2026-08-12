//! What the launcher asks of the kernel, and what it leaves on disk, held to
//! exact numbers.
//!
//! Time is not asserted anywhere here. A CI runner's wall clock moves by more
//! than any change worth catching, and this repository has already attributed
//! a 4.6% "regression" to a binary that turned out to be innocent. Counts do
//! not move: a syscall is made or it is not, an entry is placed or it is not,
//! and a body the plan skipped is a body that was never read. So a change that
//! adds a `stat` per entry, or stops skipping a shadowed body, fails here even
//! though nothing measurable happened to the clock.
//!
//! The numbers come from `bench_image --profile small`, whose whole
//! reproduction is its seed. When the generator changes they change with it:
//! run this test, read the numbers it reports, and update them here.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Entries under `rootfs`, not counting the three files the launcher writes
/// itself.
const ENTRIES: usize = 172;

/// What the plan works out before a single body is written.
const SKIPPED_FILES: usize = 165;
const PLANNED_DIRECTORIES: usize = 56;
const SPAN_UNITS: usize = 12;

/// Syscalls neither libc nor the host can account for: every one of these is
/// the extractor placing something. Grouped into families because the
/// architecture decides which member libc reaches for -- arm64 has no `mkdir`
/// syscall at all, only `mkdirat`.
const WALK_SYSCALLS: [(&str, u64); 7] = [
    ("mkdir", 61),
    ("symlink", 11),
    ("link", 2),
    ("unlink", 244),
    ("chmod", 60),
    ("fchmod", 347),
    ("utimensat", 358),
];

/// The span route places the same tree, and the difference is the point of it:
/// it opens and writes only the bodies that survive, and it never has to
/// unlink what a later layer replaces because it never wrote it.
const SPAN_SYSCALLS: [(&str, u64); 7] = [
    ("mkdir", 61),
    ("symlink", 11),
    ("link", 2),
    ("unlink", 0),
    ("chmod", 60),
    ("fchmod", 103),
    ("utimensat", 114),
];

const FAMILIES: [(&str, &[&str]); 7] = [
    ("mkdir", &["mkdir", "mkdirat"]),
    ("symlink", &["symlink", "symlinkat"]),
    ("link", &["link", "linkat"]),
    ("unlink", &["unlink", "unlinkat"]),
    ("chmod", &["chmod", "fchmodat", "fchmodat2"]),
    ("fchmod", &["fchmod"]),
    (
        "utimensat",
        &["utimensat", "utimensat_time64", "utime", "utimes"],
    ),
];

/// The launcher writes these into the rootfs itself, so they say nothing about
/// the image.
const LAUNCHER_WRITES: [&str; 3] = ["etc/hosts", "etc/hostname", "etc/resolv.conf"];

fn scratch() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = match std::env::var("TEST_TMPDIR") {
            Ok(dir) => PathBuf::from(dir),
            Err(_) => std::env::temp_dir(),
        }
        .join("bench-counts");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("scratch");
        root
    })
}

/// Bazel passes the paths it built; cargo names them at compile time.
fn tool(variable: &str, built: Option<&'static str>) -> PathBuf {
    match std::env::var(variable) {
        Ok(path) => PathBuf::from(path),
        Err(_) => PathBuf::from(built.unwrap_or_else(|| {
            panic!("neither ${variable} nor a cargo built binary to fall back on")
        })),
    }
}

fn bench_image() -> PathBuf {
    tool("BENCH_IMAGE", option_env!("CARGO_BIN_EXE_bench_image"))
}

fn oci_runtime() -> PathBuf {
    tool("OCI_RUNTIME", option_env!("CARGO_BIN_EXE_oci_runtime"))
}

/// The fixture and its sidecars, built once for every test in this file.
fn fixture() -> &'static (PathBuf, PathBuf) {
    static FIXTURE: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let layout = scratch().join("layout");
        generate(&layout);
        let indexes = scratch().join("indexes");
        let status = Command::new(oci_runtime())
            .arg("index")
            .arg("--layout")
            .arg(&layout)
            .arg("--output")
            .arg(&indexes)
            .status()
            .expect("index");
        assert!(status.success(), "the layout could not be indexed");
        (layout, indexes)
    })
}

fn generate(output: &Path) {
    let result = Command::new(bench_image())
        .arg("--output")
        .arg(output)
        .args(["--profile", "small"])
        .output()
        .expect("bench_image");
    assert!(
        result.status.success(),
        "bench_image failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// The seed is the whole reproduction, so two runs must agree down to the
/// blob digests. Without this every other number here is only as stable as
/// the last person's machine.
#[test]
fn the_same_seed_builds_the_same_image() {
    let first = scratch().join("repeat-a");
    let second = scratch().join("repeat-b");
    generate(&first);
    generate(&second);
    assert_eq!(
        snapshot(&first),
        snapshot(&second),
        "the generator is not reproducible"
    );
}

/// The routes are held to each other elsewhere; what is asserted here is that
/// they place a known number of entries, so a run that quietly did less work
/// cannot pass as a fast one.
#[test]
fn every_route_places_the_same_entries() {
    let (layout, indexes) = fixture();
    let walked = extract("walk", layout, None);
    let spanned = extract("spans", layout, Some(indexes.as_path()));

    assert_eq!(
        entries(&walked.rootfs),
        ENTRIES,
        "the walk placed a different number of entries"
    );
    assert_eq!(
        entries(&spanned.rootfs),
        ENTRIES,
        "the span route placed a different number of entries"
    );
    assert_eq!(
        snapshot(&walked.rootfs),
        snapshot(&spanned.rootfs),
        "the two routes placed different trees"
    );
}

/// The plan's own arithmetic, before anything is written. A change that stops
/// the plan resolving shows up here as a route rather than as a slower run
/// nobody can attribute.
#[test]
fn the_plan_skips_what_later_layers_replace() {
    let (layout, indexes) = fixture();
    let spanned = extract("plan", layout, Some(indexes.as_path()));

    let skipped = number_before(&spanned.log, "files (", "Skipping ");
    assert_eq!(skipped, Some(SKIPPED_FILES), "{}", spanned.log);
    let created = number_before(&spanned.log, "directories", "Created ");
    assert_eq!(created, Some(PLANNED_DIRECTORIES), "{}", spanned.log);
    // Worker count follows the host's core count, so only the units are ours.
    let units = number_before(&spanned.log, "units on", "as ");
    assert_eq!(units, Some(SPAN_UNITS), "{}", spanned.log);
}

/// The metric that does not care what else the host was doing.
#[test]
fn neither_route_asks_the_kernel_for_more_than_it_needs() {
    let Some(_) = which("strace") else {
        eprintln!("strace is not installed, so syscall counts are not checked");
        return;
    };
    let (layout, indexes) = fixture();

    for (name, expected, index) in [
        ("walk", WALK_SYSCALLS, None),
        ("spans", SPAN_SYSCALLS, Some(indexes.as_path())),
    ] {
        let counts = traced(name, layout, index);
        // A sandbox that refuses `ptrace` leaves nothing to compare, and a
        // test that then asserts zeroes reports a regression that is really a
        // missing permission.
        if counts.is_empty() {
            eprintln!("strace counted nothing, so syscall counts are not checked");
            return;
        }
        let mut wrong = Vec::new();
        for (family, want) in expected {
            let members = FAMILIES
                .iter()
                .find(|(named, _)| *named == family)
                .expect("a named family")
                .1;
            let got: u64 = members
                .iter()
                .map(|member| counts.get(*member).copied().unwrap_or(0))
                .sum();
            if got != want {
                wrong.push(format!("{family}: expected {want}, counted {got}"));
            }
        }
        assert!(wrong.is_empty(), "{name} route:\n  {}", wrong.join("\n  "));
    }
}

struct Extraction {
    rootfs: PathBuf,
    log: String,
}

fn extract(name: &str, layout: &Path, indexes: Option<&Path>) -> Extraction {
    let run = scratch().join(name);
    let _ = fs::remove_dir_all(&run);
    fs::create_dir_all(&run).expect("run directory");

    let mut command = Command::new(oci_runtime());
    command
        .arg("run")
        .arg("--layout")
        .arg(layout)
        .args(["--runtime", "/nonexistent/runc"])
        .args(["--keep-bundle", "--verbose", "--strict-xattrs=false"])
        .env("TMPDIR", &run);
    if let Some(indexes) = indexes {
        command.arg("--index").arg(indexes);
    }
    // The runtime is deliberately absent: the bundle is the whole subject, and
    // starting a container needs privileges a test does not have.
    let output = command.output().expect("oci_runtime");
    let log = String::from_utf8_lossy(&output.stderr).into_owned();

    let bundle = fs::read_dir(&run)
        .expect("bundle")
        .next()
        .expect("a bundle was left behind")
        .expect("bundle entry")
        .path();
    Extraction {
        rootfs: bundle.join("rootfs"),
        log,
    }
}

fn traced(name: &str, layout: &Path, indexes: Option<&Path>) -> BTreeMap<String, u64> {
    let run = scratch().join(format!("traced-{name}"));
    let _ = fs::remove_dir_all(&run);
    fs::create_dir_all(&run).expect("run directory");
    let report = scratch().join(format!("strace-{name}.out"));

    let mut command = Command::new("strace");
    command
        .args(["-f", "-c", "-U", "name,calls", "-o"])
        .arg(&report)
        .arg(oci_runtime())
        .arg("run")
        .arg("--layout")
        .arg(layout)
        .args(["--runtime", "/nonexistent/runc"])
        .args(["--keep-bundle", "--strict-xattrs=false"])
        .env("TMPDIR", &run);
    if let Some(indexes) = indexes {
        command.arg("--index").arg(indexes);
    }
    command.output().expect("strace");

    let mut counts = BTreeMap::new();
    let Ok(report) = fs::read_to_string(&report) else {
        return counts;
    };
    for line in report.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 2 || fields[0] == "total" {
            continue;
        }
        if let Ok(count) = fields[1].parse::<u64>() {
            counts.insert(fields[0].to_string(), count);
        }
    }
    counts
}

/// Everything the image left, described well enough that two trees can be
/// compared without reading their bodies twice.
fn snapshot(root: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let mut entries: Vec<PathBuf> = fs::read_dir(&directory)
            .unwrap_or_else(|err| panic!("reading {}: {err}", directory.display()))
            .map(|entry| entry.expect("entry").path())
            .collect();
        entries.sort();
        for path in entries {
            let relative = path.strip_prefix(root).expect("under the root");
            let name = relative.to_string_lossy().into_owned();
            if LAUNCHER_WRITES.contains(&name.as_str()) {
                continue;
            }
            let meta = fs::symlink_metadata(&path).expect("metadata");
            let kind = if meta.is_dir() {
                stack.push(path.clone());
                "d".to_string()
            } else if meta.is_symlink() {
                format!("l {}", fs::read_link(&path).expect("link").display())
            } else {
                // The body, not just its length: a route that wrote the wrong
                // layer's copy leaves a file of the same size.
                format!("f {:x}", fnv(&fs::read(&path).expect("body")))
            };
            lines.push(format!("{name} {:o} {} {kind}", meta.mode(), meta.nlink()));
        }
    }
    lines.sort();
    lines
}

/// FNV-1a. A comparison between two trees this test just built does not need
/// anything stronger, and this file has no dependencies.
fn fnv(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn entries(root: &Path) -> usize {
    snapshot(root).len()
}

/// The number between `after` and `before` on the first line holding both.
fn number_before(log: &str, before: &str, after: &str) -> Option<usize> {
    let line = log
        .lines()
        .find(|line| line.contains(before) && line.contains(after))?;
    let tail = &line[line.find(after)? + after.len()..];
    let number: String = tail.chars().take_while(char::is_ascii_digit).collect();
    number.parse().ok()
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|dir| Path::new(dir).join(program))
        .find(|path| path.is_file())
}
