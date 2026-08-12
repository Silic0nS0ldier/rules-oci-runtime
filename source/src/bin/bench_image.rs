//! Builds a deterministic OCI image layout to benchmark the launcher against.
//!
//! The reference workload has always been a pulled image (`browserless/chromium`),
//! which cannot be reproduced from this repository and does not survive a
//! machine. What matters about it is the *shape* rather than the contents: a
//! long tail of small files with a handful of very large ones carrying most of
//! the bytes, thousands of directories, plenty of symlinks, and far more bodies
//! shadowed by a later layer than survive into the tree. A distribution base
//! image has none of that, and a per entry cache regression that was invisible
//! on alpine cost 5x on chromium.
//!
//! The seed is the whole reproduction: the same seed and profile produce byte
//! identical blobs, so a digest recorded in a test stays valid.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use clap::{Parser, ValueEnum};
use flate2::{Compression, GzBuilder};
use sha2::{Digest, Sha256};

/// Every timestamp is fixed so the layout is reproducible.
const MTIME: u64 = 1_700_000_000;

const GZIP_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const ZSTD_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";

#[derive(Debug, Parser)]
#[command(
    name = "bench_image",
    about = "Generate a deterministic OCI image layout for benchmarking"
)]
struct Args {
    /// Directory to write the layout into. Replaced if it exists.
    #[arg(long, value_name = "DIR")]
    output: Utf8PathBuf,

    /// Size and shape of the generated image.
    #[arg(long, value_enum, default_value_t = Profile::Full)]
    profile: Profile,

    /// Changing this changes every path, size and body in the image.
    #[arg(long, default_value_t = 1)]
    seed: u64,

    /// Layers compressed at once. Each worker holds one whole layer.
    #[arg(long, value_name = "N")]
    workers: Option<usize>,

    /// How the layers are compressed.
    #[arg(long, value_enum, default_value_t = Packing::Gzip)]
    compression: Packing,

    /// Uncompressed bytes per zstd frame. A frame boundary is the only place
    /// a zstd span can start, so this decides how far the layer parallelises:
    /// a layer smaller than this is one frame, as `bsdtar` writes them.
    #[arg(long, value_name = "BYTES", default_value_t = 64 << 20)]
    frame_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Packing {
    Gzip,
    Zstd,
}

impl Packing {
    fn media_type(self) -> &'static str {
        match self {
            Packing::Gzip => GZIP_LAYER,
            Packing::Zstd => ZSTD_LAYER,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Profile {
    /// Roughly the shape of the reference image: ~8k files, ~500 MiB.
    Full,
    /// A tenth of the size, for iterating on the harness itself.
    Medium,
    /// Seconds to build and small enough for a test to extract.
    Small,
}

/// What the profile asks for. Counts are targets, not guarantees: an entry
/// that would collide with something the generator already placed is skipped.
struct Shape {
    layers: usize,
    directories: usize,
    /// Paths written for the first time, across all layers.
    files: usize,
    /// Writes to a path an earlier layer already wrote, whose body the tree
    /// never keeps. The reference image shadows three bodies for every one it
    /// keeps, and the span route exists to skip exactly these.
    rewrites: usize,
    /// Multiplies every body size, so the shape of the distribution is the
    /// same at every profile.
    scale: f64,
}

impl Profile {
    fn shape(self) -> Shape {
        match self {
            Profile::Full => Shape {
                layers: 20,
                directories: 7_000,
                files: 8_000,
                rewrites: 23_000,
                scale: 1.0,
            },
            Profile::Medium => Shape {
                layers: 12,
                directories: 900,
                files: 1_200,
                rewrites: 3_000,
                scale: 0.12,
            },
            Profile::Small => Shape {
                layers: 6,
                directories: 60,
                files: 120,
                rewrites: 240,
                scale: 0.05,
            },
        }
    }
}

/// SplitMix64. Ten lines, no dependency, and the seed is the whole
/// reproduction for any image it produces.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x243f_6a88_85a3_08d3)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.below(from.len())]
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

const WORDS: [&str; 24] = [
    "lib",
    "bin",
    "share",
    "src",
    "include",
    "python3",
    "site",
    "packages",
    "node",
    "modules",
    "chrome",
    "locales",
    "resources",
    "icons",
    "hicolor",
    "fonts",
    "truetype",
    "doc",
    "man",
    "etc",
    "conf",
    "data",
    "cache",
    "build",
];

const EXTENSIONS: [&str; 10] = [
    "so", "py", "js", "json", "png", "txt", "h", "pak", "dat", "xml",
];

const MODES: [u32; 4] = [0o644, 0o644, 0o755, 0o600];

#[derive(Debug, Clone)]
enum Kind {
    Directory,
    /// `seed` reproduces the body without storing it.
    File {
        size: usize,
        mode: u32,
        seed: u64,
    },
    Symlink {
        target: String,
    },
    /// Always names a path this same layer already wrote, which is what a
    /// real archiver emits and what keeps the target resolvable.
    HardLink {
        target: String,
    },
    /// A `.wh.` marker; `path` is already the marker's own path.
    Whiteout,
    /// A `.wh..wh..opq` marker.
    Opaque,
}

struct Entry {
    path: String,
    kind: Kind,
}

/// What the generator decided, reported so a run can be described without
/// unpacking the layout again.
#[derive(Default)]
struct Counts {
    directories: usize,
    files: usize,
    symlinks: usize,
    hard_links: usize,
    whiteouts: usize,
    /// Uncompressed body bytes across every layer, shadowed ones included.
    bytes: u64,
    /// Body bytes a later layer overwrites, so the tree never keeps them.
    shadowed_bytes: u64,
}

fn main() {
    let args = Args::parse();
    if let Err(err) = build(&args) {
        eprintln!("bench_image: {err}");
        std::process::exit(1);
    }
}

fn build(args: &Args) -> io::Result<()> {
    let shape = args.profile.shape();
    let (layers, counts) = plan(args.seed, &shape);

    if args.output.exists() {
        fs::remove_dir_all(&args.output)?;
    }
    fs::create_dir_all(args.output.join("blobs/sha256"))?;

    let workers = args
        .workers
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        })
        .clamp(1, layers.len().max(1))
        // Each worker holds a whole layer, uncompressed and compressed.
        .min(4);

    let built = compress(
        &layers,
        &args.output,
        workers,
        args.compression,
        args.frame_bytes,
    )?;

    let diff_ids: Vec<String> = built.iter().map(|blob| blob.diff_id.clone()).collect();
    let config = serde_json::json!({
        "architecture": host_architecture(),
        "os": "linux",
        "config": {"Cmd": ["/bin/sh"]},
        "rootfs": {"type": "layers", "diff_ids": diff_ids},
    });
    let (config_digest, config_size) = write_blob(&args.output, &serde_json::to_vec(&config)?)?;

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST,
        "config": {
            "mediaType": OCI_CONFIG,
            "digest": config_digest,
            "size": config_size,
        },
        "layers": built.iter().map(|blob| serde_json::json!({
            "mediaType": args.compression.media_type(),
            "digest": blob.digest,
            "size": blob.size,
        })).collect::<Vec<_>>(),
    });
    let (manifest_digest, manifest_size) =
        write_blob(&args.output, &serde_json::to_vec(&manifest)?)?;

    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": OCI_MANIFEST,
            "digest": manifest_digest,
            "size": manifest_size,
            "platform": {"architecture": host_architecture(), "os": "linux"},
        }],
    });
    fs::write(args.output.join("index.json"), serde_json::to_vec(&index)?)?;
    fs::write(
        args.output.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )?;

    // Recorded beside the layout so a harness can report what it measured
    // against without reading every blob back.
    let compressed: u64 = built.iter().map(|blob| blob.size).sum();
    let fixture = serde_json::json!({
        "seed": args.seed,
        "profile": format!("{:?}", args.profile).to_lowercase(),
        "manifest": manifest_digest,
        "layers": built.len(),
        "directories": counts.directories,
        "files": counts.files,
        "symlinks": counts.symlinks,
        "hardLinks": counts.hard_links,
        "whiteouts": counts.whiteouts,
        "uncompressedBytes": counts.bytes,
        "shadowedBytes": counts.shadowed_bytes,
        "compressedBytes": compressed,
    });
    fs::write(
        args.output.join("bench-fixture.json"),
        serde_json::to_vec_pretty(&fixture)?,
    )?;

    eprintln!(
        "{} layers, {} directories, {} files ({} symlinks, {} hard links, {} whiteouts)",
        built.len(),
        counts.directories,
        counts.files,
        counts.symlinks,
        counts.hard_links,
        counts.whiteouts,
    );
    eprintln!(
        "{} of bodies, {} of them shadowed, {} compressed",
        human(counts.bytes),
        human(counts.shadowed_bytes),
        human(compressed),
    );
    println!("{manifest_digest}");
    Ok(())
}

/// Decides every entry of every layer up front. Bodies are not materialised
/// here, so the whole plan for the full profile costs a few megabytes and the
/// layers can then be built in any order.
fn plan(seed: u64, shape: &Shape) -> (Vec<Vec<Entry>>, Counts) {
    let mut rng = Rng::new(seed);
    let mut counts = Counts::default();

    let directories = directories(&mut rng, shape.directories);
    let mut layers: Vec<Vec<Entry>> = (0..shape.layers).map(|_| Vec::new()).collect();

    // The base layer carries most of the tree, as a real image's does.
    let base = directories.len() * 3 / 5;
    let mut visible: Vec<&str> = Vec::new();
    let mut next_directory = 0;

    // Paths holding a body and how big it is, so a rewrite has something to
    // shadow and a whiteout something to remove.
    let mut live: Vec<(String, usize)> = Vec::new();
    let mut taken: BTreeSet<String> = BTreeSet::new();
    // Hard links and the copies they name. The plan refuses an image whose
    // hard link target the final tree does not hold, so nothing may remove
    // one of these.
    let mut pinned: Vec<String> = Vec::new();
    let mut opaque_done = false;

    for layer in 0..shape.layers {
        let entries = &mut layers[layer];

        let wanted = if layer == 0 {
            base
        } else {
            next_directory + (directories.len() - base) / (shape.layers - 1)
        }
        .min(directories.len());
        for directory in &directories[next_directory..wanted] {
            entries.push(Entry {
                path: directory.clone(),
                kind: Kind::Directory,
            });
            visible.push(directory.as_str());
            counts.directories += 1;
        }
        next_directory = wanted;

        // Half the new files land in the base layer, as does most of the tree.
        let new_files = if layer == 0 {
            shape.files / 2
        } else {
            shape.files / 2 / (shape.layers - 1)
        };
        let rewrites = if layer == 0 {
            0
        } else {
            shape.rewrites / (shape.layers - 1)
        };

        for _ in 0..new_files {
            if visible.is_empty() {
                break;
            }
            let directory = *rng.pick(&visible);
            let path = format!("{directory}/{}", file_name(&mut rng));
            if !taken.insert(path.clone()) {
                continue;
            }
            match rng.below(100) {
                // Symlinks are dense in the reference image, and the whole
                // path resolver hangs off them.
                0..=7 => {
                    let target = if live.is_empty() || rng.below(2) == 0 {
                        "../".repeat(rng.below(3)) + &file_name(&mut rng)
                    } else {
                        rng.pick(&live).0.clone()
                    };
                    entries.push(Entry {
                        path,
                        kind: Kind::Symlink { target },
                    });
                    counts.symlinks += 1;
                }
                8..=9 => {
                    // A hard link resolves against the tree as it stands, so
                    // it names something this layer has already written.
                    let target = entries.iter().rev().find_map(|entry| match entry.kind {
                        Kind::File { .. } => Some(entry.path.clone()),
                        _ => None,
                    });
                    match target {
                        Some(target) => {
                            // The plan refuses an image whose hard link names
                            // a copy a later layer replaces or removes, and
                            // that would drop the whole fixture onto the walk.
                            live.retain(|(path, _)| path != &target);
                            pinned.push(target.clone());
                            pinned.push(path.clone());
                            entries.push(Entry {
                                path,
                                kind: Kind::HardLink { target },
                            });
                            counts.hard_links += 1;
                        }
                        None => continue,
                    }
                }
                _ => {
                    let size = body_size(&mut rng, shape.scale, true);
                    entries.push(Entry {
                        path: path.clone(),
                        kind: Kind::File {
                            size,
                            mode: *rng.pick(&MODES),
                            seed: rng.next(),
                        },
                    });
                    counts.bytes += size as u64;
                    counts.files += 1;
                    live.push((path, size));
                }
            }
        }

        // A rewrite is a small file changing between layers. Letting one land
        // on a multi-megabyte file would shadow away most of the image's bytes
        // and leave a tree far smaller than the layers that built it.
        let ceiling = (256_000.0 * shape.scale) as usize;
        for _ in 0..rewrites {
            if live.is_empty() {
                break;
            }
            let mut index = rng.below(live.len());
            for _ in 0..3 {
                if live[index].1 <= ceiling {
                    break;
                }
                index = rng.below(live.len());
            }
            let size = body_size(&mut rng, shape.scale, false);
            entries.push(Entry {
                path: live[index].0.clone(),
                kind: Kind::File {
                    size,
                    mode: *rng.pick(&MODES),
                    seed: rng.next(),
                },
            });
            live[index].1 = size;
            counts.bytes += size as u64;
        }

        if layer > 0 && !live.is_empty() {
            for _ in 0..(live.len() / 200).max(1) {
                let index = rng.below(live.len());
                let (path, _) = live.swap_remove(index);
                entries.push(Entry {
                    path: whiteout_of(&path),
                    kind: Kind::Whiteout,
                });
                counts.whiteouts += 1;
            }
        }

        // One opaque marker, mid image, over a directory with contents to
        // remove. It is the case that has broken the plan twice. Deep enough
        // that it does not take most of the tree with it.
        if !opaque_done && layer == shape.layers / 2 {
            let deep: Vec<&str> = visible
                .iter()
                .copied()
                .filter(|directory| {
                    directory.matches('/').count() >= 3
                        && !pinned
                            .iter()
                            .any(|path| path.starts_with(&format!("{directory}/")))
                })
                .collect();
            if let Some(directory) = deep.first().map(|_| (*rng.pick(&deep)).to_string()) {
                entries.push(Entry {
                    path: format!("{directory}/.wh..wh..opq"),
                    kind: Kind::Opaque,
                });
                let prefix = format!("{directory}/");
                live.retain(|(path, _)| !path.starts_with(&prefix));
                opaque_done = true;
            }
        }
    }

    // A body is shadowed if any later layer writes the same path, which is
    // only known once every layer is planned.
    let mut surviving: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
    for entry in layers.iter().flatten() {
        match &entry.kind {
            Kind::File { size, .. } => {
                surviving.insert(&entry.path, *size as u64);
            }
            _ => {
                surviving.remove(entry.path.as_str());
            }
        }
    }
    counts.shadowed_bytes = counts.bytes - surviving.values().sum::<u64>();

    (layers, counts)
}

/// A directory tree that is deep as well as wide, since resolving a path
/// component by component is on the hot path.
fn directories(rng: &mut Rng, count: usize) -> Vec<String> {
    let mut directories: Vec<String> = vec![
        "usr".into(),
        "usr/lib".into(),
        "usr/share".into(),
        "usr/local".into(),
        "etc".into(),
        "opt".into(),
        "opt/app".into(),
        "var".into(),
        "var/lib".into(),
    ];
    let seeds = directories.len();
    while directories.len() < count.max(seeds) {
        // Biased towards what was just created, which is what makes the tree
        // deep rather than a flat fan out.
        let span = directories.len().min(64);
        let mut parent = directories[directories.len() - 1 - rng.below(span)].clone();
        // Climbing rather than retrying: once the recent window is all at the
        // depth limit, retrying never finds a parent and never terminates.
        if parent.matches('/').count() >= 6 {
            parent = parent.split('/').take(4).collect::<Vec<_>>().join("/");
        }
        let child = format!("{parent}/{}{}", rng.pick(&WORDS), directories.len());
        directories.push(child);
    }
    directories.truncate(count.max(seeds));
    directories
}

fn file_name(rng: &mut Rng) -> String {
    format!(
        "{}{}.{}",
        rng.pick(&WORDS),
        rng.next() % 1_000_000,
        rng.pick(&EXTENSIONS)
    )
}

fn whiteout_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((directory, name)) => format!("{directory}/.wh.{name}"),
        None => format!(".wh.{path}"),
    }
}

/// Body sizes taken from the reference image: median 5.2 KiB, p90 54.6 KiB,
/// p99 361 KiB, and the 0.36% over a megabyte holding two thirds of the
/// bytes. Getting this wrong is how a benchmark ends up measuring per entry
/// overhead when the real cost is throughput, or the reverse.
///
/// Log-uniform within each bucket, which is what puts the median where the
/// percentile says rather than halfway up the range.
fn body_size(rng: &mut Rng, scale: f64, tail: bool) -> usize {
    let (low, high): (f64, f64) = match rng.unit() {
        u if u < 0.50 => (256.0, 5_324.0),
        u if u < 0.90 => (5_324.0, 55_910.0),
        // A rewrite is a small file being patched, not a new blob: the
        // reference shadows 418 MiB across 23,475 of them, some 18 KiB each.
        u if u < 0.99 || !tail => (55_910.0, 369_664.0),
        u if u < 0.9964 => (369_664.0, 1_100_000.0),
        _ => (1_100_000.0, 77_600_000.0),
    };
    let size = low * (high / low).powf(rng.unit()) * scale;
    size.max(1.0) as usize
}

/// Bodies compress like real ones. A layer of real files -- binaries, images,
/// already compressed assets -- gives deflate around two and a half times, so
/// the window here is part dictionary words and part high entropy filler.
/// Plain words would compress five times and make the decompressor look far
/// too good; random bytes would not compress at all and make it look bad.
fn body(seed: u64, size: usize) -> Vec<u8> {
    // One window, repeated. The repeat is 64 KiB apart, well outside
    // deflate's 32 KiB window, so it cannot be matched away.
    const WINDOW: usize = 64 << 10;
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ+/";
    let mut rng = Rng::new(seed);
    let mut window = Vec::with_capacity(WINDOW.min(size) + 64);
    while window.len() < WINDOW.min(size) {
        if rng.below(10) < 7 {
            window.extend_from_slice(rng.pick(&WORDS).as_bytes());
            window.push(if rng.below(8) == 0 { b'\n' } else { b' ' });
        } else {
            for _ in 0..8 {
                window.push(ALPHABET[rng.below(ALPHABET.len())]);
            }
        }
    }

    let mut body = Vec::with_capacity(size);
    let span = window.len();
    while body.len() < size {
        let take = (size - body.len()).min(span);
        body.extend_from_slice(&window[..take]);
        // Rotating keeps consecutive windows from matching each other.
        window.rotate_left((1 + body.len() % 7) % span);
    }
    body.truncate(size);
    body
}

struct LayerBlob {
    digest: String,
    diff_id: String,
    size: u64,
}

fn compress(
    layers: &[Vec<Entry>],
    output: &Utf8Path,
    workers: usize,
    packing: Packing,
    frame_bytes: usize,
) -> io::Result<Vec<LayerBlob>> {
    let built: Mutex<Vec<Option<LayerBlob>>> =
        Mutex::new((0..layers.len()).map(|_| None).collect());
    let next = AtomicUsize::new(0);
    let failure: Mutex<Option<io::Error>> = Mutex::new(None);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= layers.len() {
                        return;
                    }
                    match build_layer(&layers[index], output, packing, frame_bytes) {
                        Ok(blob) => built.lock().expect("built")[index] = Some(blob),
                        Err(err) => {
                            *failure.lock().expect("failure") = Some(err);
                            return;
                        }
                    }
                }
            });
        }
    });

    if let Some(err) = failure.into_inner().expect("failure") {
        return Err(err);
    }
    Ok(built
        .into_inner()
        .expect("built")
        .into_iter()
        .map(|blob| blob.expect("every layer built"))
        .collect())
}

fn build_layer(
    entries: &[Entry],
    output: &Utf8Path,
    packing: Packing,
    frame_bytes: usize,
) -> io::Result<LayerBlob> {
    // Straight to disk rather than into a buffer: the base layer of the full
    // profile is a few hundred megabytes on its own, and several workers hold
    // one each.
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let blobs = output.join("blobs/sha256");
    let staging = blobs.join(format!("partial.{}", NEXT.fetch_add(1, Ordering::Relaxed)));

    let file = io::BufWriter::new(fs::File::create(&staging)?);
    let encoder = match packing {
        Packing::Gzip => Encoder::Gzip(
            GzBuilder::new()
                .mtime(0)
                .write(Hashing::new(file), Compression::new(6)),
        ),
        Packing::Zstd => Encoder::Zstd(FrameWriter::new(Hashing::new(file), frame_bytes)),
    };
    let mut builder = tar::Builder::new(Hashing::new(encoder));
    for entry in entries {
        append(&mut builder, entry)?;
    }

    let plain = builder.into_inner()?;
    let (encoder, diff_id, _) = plain.finish();
    let mut packed = encoder.finish()?;
    io::Write::flush(&mut packed)?;
    let (mut file, digest, size) = packed.finish();
    io::Write::flush(&mut file)?;

    fs::rename(&staging, blobs.join(&digest))?;
    Ok(LayerBlob {
        digest: format!("sha256:{digest}"),
        diff_id: format!("sha256:{diff_id}"),
        size,
    })
}

/// The compressor a layer's tar stream is written through.
enum Encoder<W: io::Write> {
    Gzip(flate2::write::GzEncoder<W>),
    Zstd(FrameWriter<W>),
}

impl<W: io::Write> Encoder<W> {
    fn finish(self) -> io::Result<W> {
        match self {
            Encoder::Gzip(encoder) => encoder.finish(),
            Encoder::Zstd(writer) => writer.finish(),
        }
    }
}

impl<W: io::Write> io::Write for Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Encoder::Gzip(encoder) => encoder.write(buf),
            Encoder::Zstd(writer) => writer.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Encoder::Gzip(encoder) => encoder.flush(),
            Encoder::Zstd(writer) => writer.flush(),
        }
    }
}

/// Writes zstd frames of a bounded uncompressed size, the way `pzstd` and the
/// seekable format do. Each frame is what a span can start at, so this is the
/// knob that decides whether an indexed zstd layer parallelises at all.
struct FrameWriter<W: io::Write> {
    inner: W,
    pending: Vec<u8>,
    limit: usize,
}

impl<W: io::Write> FrameWriter<W> {
    fn new(inner: W, limit: usize) -> Self {
        FrameWriter {
            inner,
            pending: Vec::new(),
            limit: limit.max(1),
        }
    }

    fn emit(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        // `ruzstd` implements only the fastest level, so these frames compress
        // worse than a real layer's. What is being measured is the decoder.
        let frame = ruzstd::encoding::compress_to_vec(
            &self.pending[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        self.pending.clear();
        self.inner.write_all(&frame)
    }

    fn finish(mut self) -> io::Result<W> {
        self.emit()?;
        Ok(self.inner)
    }
}

impl<W: io::Write> io::Write for FrameWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        if self.pending.len() >= self.limit {
            self.emit()?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Hashes and counts everything on its way through.
struct Hashing<W> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: io::Write> Hashing<W> {
    fn new(inner: W) -> Self {
        Hashing {
            inner,
            hasher: Sha256::new(),
            written: 0,
        }
    }

    fn finish(self) -> (W, String, u64) {
        (
            self.inner,
            hex_encode(&self.hasher.finalize()),
            self.written,
        )
    }
}

impl<W: io::Write> io::Write for Hashing<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn append<W: io::Write>(builder: &mut tar::Builder<W>, entry: &Entry) -> io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_mtime(MTIME);
    header.set_uid(0);
    header.set_gid(0);
    match &entry.kind {
        Kind::Directory => {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            builder.append_data(&mut header, format!("{}/", entry.path), io::empty())
        }
        Kind::File { size, mode, seed } => {
            header.set_mode(*mode);
            header.set_size(*size as u64);
            builder.append_data(&mut header, &entry.path, &body(*seed, *size)[..])
        }
        Kind::Symlink { target } => {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            header.set_size(0);
            builder.append_link(&mut header, &entry.path, target.as_str())
        }
        Kind::HardLink { target } => {
            header.set_entry_type(tar::EntryType::Link);
            header.set_mode(0o644);
            header.set_size(0);
            builder.append_link(&mut header, &entry.path, target.as_str())
        }
        Kind::Whiteout | Kind::Opaque => {
            header.set_mode(0o644);
            header.set_size(0);
            builder.append_data(&mut header, &entry.path, io::empty())
        }
    }
}

fn write_blob(output: &Utf8Path, bytes: &[u8]) -> io::Result<(String, u64)> {
    let hex = hex_encode(&Sha256::digest(bytes));
    let path = output.join("blobs/sha256").join(&hex);
    if !path.exists() {
        // A temporary neighbour keeps two workers from seeing a half written
        // blob, which is possible when two layers happen to be identical.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let staging =
            path.with_extension(format!("partial.{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&staging, bytes)?;
        fs::rename(&staging, &path)?;
    }
    Ok((format!("sha256:{hex}"), bytes.len() as u64))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn human(bytes: u64) -> String {
    match bytes {
        bytes if bytes >= 1 << 20 => format!("{:.1} MiB", bytes as f64 / (1u64 << 20) as f64),
        bytes if bytes >= 1 << 10 => format!("{:.1} KiB", bytes as f64 / (1u64 << 10) as f64),
        bytes => format!("{bytes} B"),
    }
}

fn host_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}
