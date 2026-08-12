//! Runs launcher binaries against the same image and reports what separates
//! them.
//!
//! The hard part of benchmarking this launcher has never been taking a
//! measurement, it has been believing one. Wall clock on this workload moves
//! several percent between reboots, an A/B of two binaries in a fixed order
//! once invented a 4.6% regression that four binaries and a reversed order
//! made disappear, and a stale sidecar directory silently sent two rounds down
//! the walk while they were being attributed to the span route.
//!
//! So this harness:
//!
//! - builds each binary's sidecars with that binary, and reports the route
//!   every binary actually took, refusing to compare two that disagree;
//! - discards a warmup run, interleaves the rest round robin, and reverses the
//!   order every other round;
//! - reports counts as well as times -- entries placed, syscalls made, faults
//!   taken -- because a count does not care what else the host was doing;
//! - says "within noise" rather than printing a number that cannot be
//!   attributed.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use clap::{Parser, ValueEnum};

/// Below this, a wall clock or CPU difference on this workload is the host,
/// not the change. Measured by running the same binary against itself.
const NOISE_FLOOR: f64 = 0.02;

#[derive(Debug, Parser)]
#[command(
    name = "bench_run",
    about = "Compare launcher binaries on one image, interleaved"
)]
struct Args {
    /// Image layout to extract, as `bench_image` writes one.
    #[arg(long, value_name = "DIR")]
    layout: Utf8PathBuf,

    /// Timed rounds. Each round runs every binary once.
    #[arg(long, default_value_t = 5)]
    rounds: usize,

    /// Which extraction route to put under test.
    #[arg(long, value_enum, default_value_t = Mode::Indexed)]
    mode: Mode,

    /// Scratch space for sidecars and extracted trees. Emptied as it goes.
    #[arg(long, value_name = "DIR", default_value = "/tmp/bench-run")]
    work: Utf8PathBuf,

    /// Also count syscalls under `strace`, in a pass of its own.
    #[arg(long)]
    syscalls: bool,

    /// Also count instructions under `perf stat`, if the host allows it.
    #[arg(long)]
    perf: bool,

    /// Refuse an image whose layers set extended attributes. Generated
    /// fixtures set none; pulled images usually do.
    #[arg(long, value_name = "BOOL", action = clap::ArgAction::Set, default_value_t = false)]
    strict_xattrs: bool,

    /// Launcher binaries to compare. The first is the baseline everything else
    /// is reported against. Passing the same binary twice is the way to see
    /// what this host's noise floor really is.
    #[arg(required = true, value_name = "BINARY")]
    binaries: Vec<Utf8PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// With sidecars, which is what a Bazel built image gets.
    Indexed,
    /// Without them, which is the fallback route.
    Streaming,
}

/// One binary under test, and where its sidecars live.
struct Subject {
    label: String,
    binary: Utf8PathBuf,
    indexes: Option<Utf8PathBuf>,
    route: String,
}

/// What one run cost. Times come from `wait4`, so they are the child's own and
/// not this process's.
#[derive(Default, Clone)]
struct Sample {
    wall: Duration,
    cpu: Duration,
    /// Peak resident set. Very noisy here: the blobs are mapped, so residency
    /// follows page cache pressure rather than the launcher's own appetite.
    maxrss: u64,
    minor_faults: u64,
    major_faults: u64,
    context_switches: u64,
    /// Entries left in the extracted tree, which is how a run that quietly did
    /// less work is caught.
    entries: u64,
}

fn main() {
    let args = Args::parse();
    if let Err(err) = run(&args) {
        eprintln!("bench_run: {err}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> io::Result<()> {
    if args.work.exists() {
        fs::remove_dir_all(&args.work)?;
    }
    fs::create_dir_all(&args.work)?;

    let subjects = prepare(args)?;
    let routes: Vec<&str> = subjects.iter().map(|s| s.route.as_str()).collect();
    if routes.windows(2).any(|pair| pair[0] != pair[1]) {
        eprintln!("bench_run: the binaries took different routes, so this compares nothing:");
        for subject in &subjects {
            eprintln!("  {:<20} {}", subject.label, subject.route);
        }
        std::process::exit(1);
    }
    println!(
        "{} on {}, {} route, {} rounds\n",
        args.layout,
        args.mode_name(),
        subjects[0].route,
        args.rounds
    );
    let paths: std::collections::BTreeSet<&Utf8PathBuf> = args.binaries.iter().collect();
    if paths.len() < args.binaries.len() {
        println!("A binary appears twice: every difference below is this host's floor.\n");
    }

    let mut samples: BTreeMap<String, Vec<Sample>> = BTreeMap::new();
    for round in 0..args.rounds {
        // Reversed every other round. A fixed order is how a difference that
        // is really cache state gets attributed to the second binary.
        let order: Vec<&Subject> = if round % 2 == 0 {
            subjects.iter().collect()
        } else {
            subjects.iter().rev().collect()
        };
        for subject in order {
            let sample = measure(args, subject)?;
            samples
                .entry(subject.label.clone())
                .or_default()
                .push(sample);
        }
    }

    report(&subjects, &samples);

    if args.syscalls {
        syscalls(args, &subjects)?;
    }
    if args.perf {
        perf(args, &subjects)?;
    }

    fs::remove_dir_all(&args.work)?;
    // Sidecars and trees are gone, so nothing here can be reused by a later
    // run without being rebuilt, which is the point.
    Ok(())
}

impl Args {
    fn mode_name(&self) -> &'static str {
        match self.mode {
            Mode::Indexed => "indexed",
            Mode::Streaming => "streaming",
        }
    }

    fn run_dir(&self) -> Utf8PathBuf {
        self.work.join("run")
    }
}

/// Builds each binary's own sidecars and finds out what route it takes, then
/// throws away a run to get the page cache into the state the timed runs will
/// see.
fn prepare(args: &Args) -> io::Result<Vec<Subject>> {
    let mut subjects = Vec::new();
    for (nth, binary) in args.binaries.iter().enumerate() {
        let name = binary.file_name().unwrap_or("binary");
        let label = format!("{}:{name}", nth + 1);

        let indexes = match args.mode {
            Mode::Streaming => None,
            Mode::Indexed => {
                // Its own, never a directory left over from another build:
                // an entry table this binary cannot read resolves no plan and
                // silently drops it onto the walk.
                let indexes = args.work.join(format!("idx-{}", nth + 1));
                let status = Command::new(binary)
                    .args(["index", "--layout"])
                    .arg(&args.layout)
                    .arg("--output")
                    .arg(&indexes)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()?;
                if !status.success() {
                    return Err(io::Error::other(format!(
                        "{binary} could not index the layout"
                    )));
                }
                Some(indexes)
            }
        };

        let mut subject = Subject {
            label,
            binary: binary.clone(),
            indexes,
            route: String::new(),
        };
        subject.route = route_of(args, &subject)?;
        subjects.push(subject);
    }
    Ok(subjects)
}

/// The warmup run, kept for what it says about itself. `--verbose` costs a
/// little, so its timings are thrown away.
fn route_of(args: &Args, subject: &Subject) -> io::Result<String> {
    let run = args.run_dir();
    reset(&run)?;
    let output = launcher(args, subject, &run)
        .arg("--verbose")
        // `output` leaves a stream alone once it has been set, and the
        // launcher's own report is the only way to know which route ran.
        .stderr(Stdio::piped())
        .output()?;
    let log = String::from_utf8_lossy(&output.stderr);
    let route = if log.contains("units on") {
        let units = log
            .lines()
            .find(|line| line.contains("units on"))
            .unwrap_or_default()
            .trim();
        format!("spans ({units})")
    } else if log.contains("Extracting layer") {
        let checkpoints = log
            .lines()
            .filter(|line| line.contains("using") && line.contains("checkpoints"))
            .count();
        format!("walk ({checkpoints} of {} layers resumed)", layers(&log))
    } else {
        return Err(io::Error::other(format!(
            "{} extracted nothing:\n{}",
            subject.binary,
            log.lines().rev().take(3).collect::<Vec<_>>().join("\n")
        )));
    };
    let _ = fs::remove_dir_all(&run);
    Ok(route)
}

fn layers(log: &str) -> usize {
    log.lines()
        .filter(|line| line.starts_with("Extracting layer"))
        .count()
}

fn launcher(args: &Args, subject: &Subject, run: &Utf8Path) -> Command {
    let mut command = Command::new(&subject.binary);
    command
        .args(["run", "--layout"])
        .arg(&args.layout)
        // Extraction is what is being measured, so the container is never
        // started: the bundle is built and left where it can be counted.
        .args(["--runtime", "/nonexistent/runc", "--keep-bundle"])
        .arg(format!("--strict-xattrs={}", args.strict_xattrs))
        .env("TMPDIR", run)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(indexes) = &subject.indexes {
        command.arg("--index").arg(indexes);
    }
    command
}

fn measure(args: &Args, subject: &Subject) -> io::Result<Sample> {
    let run = args.run_dir();
    reset(&run)?;

    let mut command = launcher(args, subject, &run);
    let start = Instant::now();
    let child = command.spawn()?;
    let mut status = 0;
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // Reaping by hand rather than through `Child::wait`, which throws the
    // resource usage away.
    loop {
        // SAFETY: both out pointers are ours and live across the call.
        let reaped = unsafe { libc::wait4(child.id() as libc::pid_t, &mut status, 0, &mut usage) };
        if reaped >= 0 {
            break;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
    let wall = start.elapsed();

    let entries = count_entries(&run)?;
    let sample = Sample {
        wall,
        cpu: timeval(usage.ru_utime) + timeval(usage.ru_stime),
        maxrss: usage.ru_maxrss as u64,
        minor_faults: usage.ru_minflt as u64,
        major_faults: usage.ru_majflt as u64,
        context_switches: (usage.ru_nvcsw + usage.ru_nivcsw) as u64,
        entries,
    };
    let _ = fs::remove_dir_all(&run);
    Ok(sample)
}

fn timeval(time: libc::timeval) -> Duration {
    Duration::new(time.tv_sec as u64, time.tv_usec as u32 * 1_000)
}

fn reset(run: &Utf8Path) -> io::Result<()> {
    if run.exists() {
        fs::remove_dir_all(run)?;
    }
    fs::create_dir_all(run)
}

/// Everything the run left behind, symlinks included and never followed.
fn count_entries(root: &Utf8Path) -> io::Result<u64> {
    let mut count = 0;
    let mut stack = vec![root.as_std_path().to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            count += 1;
            if entry.file_type()?.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(count)
}

fn report(subjects: &[Subject], samples: &BTreeMap<String, Vec<Sample>>) {
    let entries: Vec<u64> = samples
        .values()
        .flatten()
        .map(|sample| sample.entries)
        .collect();
    let agreed: std::collections::BTreeSet<u64> = entries.iter().copied().collect();
    if agreed.len() == 1 {
        println!("entries  {} in every run\n", entries[0]);
    } else {
        println!("entries  DISAGREE {agreed:?} -- a run that placed fewer entries did less work\n");
    }

    let metrics: [(&str, fn(&Sample) -> f64, bool); 6] = [
        ("wall s", |s| s.wall.as_secs_f64(), true),
        ("cpu s", |s| s.cpu.as_secs_f64(), true),
        ("maxrss MiB", |s| s.maxrss as f64 / 1024.0, true),
        ("minor faults", |s| s.minor_faults as f64, false),
        ("major faults", |s| s.major_faults as f64, false),
        ("ctx switches", |s| s.context_switches as f64, false),
    ];

    let baseline = &subjects[0].label;
    println!(
        "{:<14} {:>10} {:>10} {:>10} {:>8}   {}",
        "metric", "min", "median", "max", "spread", "vs baseline (min)"
    );
    for (name, of, timed) in metrics {
        let base = summarise(&samples[baseline].iter().map(of).collect::<Vec<_>>());
        // What the baseline alone did across its own rounds. A difference
        // smaller than that is not a difference: the same binary produced it
        // twice over.
        let floor = NOISE_FLOOR.max(spread(base));
        for subject in subjects {
            let values: Vec<f64> = samples[&subject.label].iter().map(of).collect();
            let (min, median, max) = summarise(&values);
            let against = if &subject.label == baseline {
                "baseline".to_string()
            } else {
                delta(base.0, min, timed.then_some(floor))
            };
            println!(
                "{:<14} {:>10.3} {:>10.3} {:>10.3} {:>7.1}%   {:<12} {}",
                name,
                min,
                median,
                max,
                spread((min, median, max)) * 100.0,
                subject.label,
                against
            );
        }
        println!();
    }
    println!(
        "Spread is one binary against itself. A time within it, or within {:.0}%, is the host.",
        NOISE_FLOOR * 100.0
    );
}

fn summarise(values: &[f64]) -> (f64, f64, f64) {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    (sorted[0], median, sorted[sorted.len() - 1])
}

/// How far one binary's own rounds ranged, as a fraction of its best.
fn spread((min, _, max): (f64, f64, f64)) -> f64 {
    if min == 0.0 { 0.0 } else { (max - min) / min }
}

fn delta(baseline: f64, value: f64, floor: Option<f64>) -> String {
    if baseline == 0.0 {
        return String::new();
    }
    let change = (value - baseline) / baseline;
    let text = format!("{:+.1}%", change * 100.0);
    match floor {
        Some(floor) if change.abs() < floor => format!("{text} (within noise)"),
        _ => text,
    }
}

/// Syscall counts, in a pass of their own: `strace` intercepts every one of
/// them, so a run under it is several times slower and its timings are worth
/// nothing. What the counts are worth is that they do not move when the host
/// is busy, so a difference of one is a difference.
fn syscalls(args: &Args, subjects: &[Subject]) -> io::Result<()> {
    if Command::new("strace")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        println!("\nstrace is not installed, so no syscall counts.");
        return Ok(());
    }

    println!("\nsyscalls (counts, not timings -- strace makes the run several times slower)");
    let mut counted: Vec<(String, BTreeMap<String, u64>, u64)> = Vec::new();
    for subject in subjects {
        let run = args.run_dir();
        reset(&run)?;
        let out = args.work.join("strace.out");
        let mut command = Command::new("strace");
        command
            .args(["-f", "-c", "-U", "name,calls", "-o"])
            .arg(&out)
            .arg(&subject.binary)
            .args(["run", "--layout"])
            .arg(&args.layout)
            .args(["--runtime", "/nonexistent/runc", "--keep-bundle"])
            .arg(format!("--strict-xattrs={}", args.strict_xattrs))
            .env("TMPDIR", &run)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(indexes) = &subject.indexes {
            command.arg("--index").arg(indexes);
        }
        command.status()?;
        let counts = parse_strace(&fs::read_to_string(&out)?);
        counted.push((subject.label.clone(), counts, count_entries(&run)?));
        let _ = fs::remove_dir_all(&run);
    }

    let baseline = &counted[0].1;
    let mut names: Vec<&String> = counted.iter().flat_map(|(_, c, _)| c.keys()).collect();
    names.sort_unstable();
    names.dedup();

    print!("{:<20}", "syscall");
    for (label, _, _) in &counted {
        print!("{label:>16}");
    }
    println!();
    print!("{:<20}", "total");
    for (_, counts, _) in &counted {
        print!("{:>16}", counts.values().sum::<u64>());
    }
    println!();
    print!("{:<20}", "entries placed");
    for (_, _, entries) in &counted {
        print!("{entries:>16}");
    }
    println!();

    let mut moved = 0;
    for name in names {
        let counts: Vec<u64> = counted
            .iter()
            .map(|(_, c, _)| c.get(name).copied().unwrap_or(0))
            .collect();
        if counts.iter().all(|count| *count == counts[0]) {
            continue;
        }
        moved += 1;
        // Only what moved, and only what is worth reading: futex and friends
        // count how the threads happened to interleave, not what was asked of
        // the kernel.
        let noisy = matches!(
            name.as_str(),
            "futex" | "sched_yield" | "epoll_wait" | "restart_syscall" | "clock_nanosleep"
        );
        print!(
            "{:<20}",
            if noisy {
                format!("{name} (noisy)")
            } else {
                name.clone()
            }
        );
        for count in &counts {
            print!("{count:>16}");
        }
        let base = baseline.get(name).copied().unwrap_or(0);
        println!(
            "   {}",
            delta(base as f64, counts[counts.len() - 1] as f64, None)
        );
    }
    if moved == 0 {
        println!(
            "every syscall count identical -- nothing here changed what was asked of the kernel"
        );
    }
    Ok(())
}

/// `strace -c -U name,calls` reports two columns and nothing else. The
/// default report leaves the errors column blank for most syscalls, so its
/// fields cannot be told apart by position.
fn parse_strace(report: &str) -> BTreeMap<String, u64> {
    let mut counts = BTreeMap::new();
    for line in report.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 2 || fields[0] == "total" {
            continue;
        }
        if let Ok(count) = fields[1].parse::<u64>() {
            *counts.entry(fields[0].to_string()).or_insert(0) += count;
        }
    }
    counts
}

/// Instructions retired is the metric that survives a busy host: it counts
/// what the CPU was asked to do rather than how long it took to do it.
/// Containers and VMs often will not allow it, so this is best effort.
fn perf(args: &Args, subjects: &[Subject]) -> io::Result<()> {
    let probe = Command::new("perf")
        .args(["stat", "-e", "instructions", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let allowed = matches!(&probe, Ok(output) if output.status.success());
    if !allowed {
        let why = match &probe {
            Ok(output) => String::from_utf8_lossy(&output.stderr)
                .lines()
                .find(|line| line.contains("not permitted") || line.contains("access"))
                .unwrap_or("perf stat failed")
                .trim()
                .to_string(),
            Err(err) => err.to_string(),
        };
        println!("\nno hardware counters: {why}");
        return Ok(());
    }

    println!("\nperf (instructions and cycles retired, one run each)");
    for subject in subjects {
        let run = args.run_dir();
        reset(&run)?;
        let out = args.work.join("perf.out");
        let mut command = Command::new("perf");
        command
            .args(["stat", "-x,", "-e", "instructions,cycles,page-faults", "-o"])
            .arg(&out)
            .arg("--")
            .arg(&subject.binary)
            .args(["run", "--layout"])
            .arg(&args.layout)
            .args(["--runtime", "/nonexistent/runc", "--keep-bundle"])
            .arg(format!("--strict-xattrs={}", args.strict_xattrs))
            .env("TMPDIR", &run)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(indexes) = &subject.indexes {
            command.arg("--index").arg(indexes);
        }
        command.status()?;
        for line in fs::read_to_string(&out)?.lines() {
            let fields: Vec<&str> = line.split(',').collect();
            if fields.len() > 2 && fields[0].parse::<f64>().is_ok() {
                println!("{:<14} {:>18} {}", subject.label, fields[0], fields[2]);
            }
        }
        let _ = fs::remove_dir_all(&run);
    }
    Ok(())
}
