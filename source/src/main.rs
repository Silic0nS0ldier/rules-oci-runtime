mod bundle;
mod cli;
mod entries;
mod error;
mod extract;
mod fsutil;
mod image;
mod launcher;
mod log;
mod runtime;
mod sidecar;
mod spec;
mod sys;
mod zinfo;

use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;

use crate::bundle::Bundle;
use crate::cli::{Cli, Command, IndexArgs, RunArgs};
use crate::error::{Error, Result};
use crate::extract::RootfsExtractor;
use crate::image::{Layout, Platform};
use crate::log::log;
use crate::runtime::{ContainerRuntime, RunRequest, Runc};
use crate::spec::{BindMount, Spec, SpecOptions};

/// Indexing holds a whole blob and its index in memory per worker, so the
/// build action stays within a sensible footprint on a many core machine.
const MAX_INDEX_WORKERS: usize = 4;

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    if let Some(path) = bundle::remover_target(&argv) {
        bundle::remove_staged(path);
        return std::process::ExitCode::SUCCESS;
    }

    let code = launcher::command_line().and_then(|argv| {
        let cli = match argv {
            Some(argv) => Cli::parse_from(argv),
            None => Cli::parse(),
        };
        match cli.command {
            Command::Run(args) => run(*args),
            Command::Index(args) => index(args),
        }
    });
    match code {
        Ok(code) => std::process::ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(err) => {
            eprintln!("oci_runtime: {err}");
            std::process::ExitCode::from(1)
        }
    }
}

fn run(args: RunArgs) -> Result<i32> {
    log::init(args.verbose);

    let layout = Layout::open(&args.layout)?;
    let platform = parse_platform(args.platform.as_deref())?;
    log!("Reading image {} for {platform}", layout.root());

    let manifest = layout.resolve_manifest(&platform)?;
    let image_config = layout.read_image_config(&manifest)?;
    if let Some(user) = image_config.user.as_deref()
        && !matches!(user, "" | "0" | "root" | "0:0" | "root:root")
    {
        log::warn(format!(
            "image requests user {user:?}, but this runtime only maps the calling user to root"
        ));
    }

    let id = format!("rules-oci-runtime-{}", sys::random_hex(8)?);
    let temp_dir = Utf8PathBuf::from_path_buf(std::env::temp_dir()).map_err(|path| {
        Error::io(
            format!("temporary directory {} is not valid UTF-8", path.display()),
            std::io::Error::from(std::io::ErrorKind::InvalidData),
        )
    })?;
    let bundle = Bundle::create(&temp_dir, &id, args.keep_bundle)?;
    log!("Using {} for the container bundle", bundle.dir());

    let rootfs = bundle.rootfs();
    let mut extractor = RootfsExtractor::new(&rootfs, args.index.as_deref(), args.strict_xattrs)?;
    extractor.plan(&manifest.layers)?;
    extractor.apply(&layout, &manifest.layers)?;

    let hostname = args
        .hostname
        .clone()
        .unwrap_or_else(|| "container".to_string());
    bundle::install_network_files(&rootfs, &hostname)?;
    extractor.finish()?;

    let terminal = args.tty.resolve(sys::stdin_is_tty());
    let rootless = args.rootless.resolve(sys::euid() != 0);
    let bind_mounts = args
        .mounts
        .iter()
        .map(|value| BindMount::parse(&cli::expand_env(value)))
        .collect::<Result<Vec<_>>>()?;
    for mount in &bind_mounts {
        if !std::path::Path::new(&mount.source).exists() {
            return Err(Error::io(
                format!("mount source {} does not exist", mount.source),
                std::io::Error::from(std::io::ErrorKind::NotFound),
            ));
        }
    }

    let spec = Spec::build(&SpecOptions {
        rootless,
        terminal,
        readonly_rootfs: args.read_only,
        hostname: &hostname,
        uid: sys::euid(),
        gid: sys::egid(),
        image: &image_config,
        extra_env: &args.env,
        bind_mounts: &bind_mounts,
        command: &args.command,
        workdir: args.workdir.as_deref(),
    })?;
    bundle.write_config(&spec)?;

    let state_dir = bundle.state_dir();
    let request = RunRequest {
        id: &id,
        bundle: bundle.dir(),
        state_dir: &state_dir,
    };
    let runc = Runc::new(&args.runtime);
    log!("Handing bundle {} to {}", bundle.dir(), runc.name());
    let result = runc.run(&request);
    runc.delete(&request);
    log!("Container has exited, cleaning up...");
    result
}

fn index(args: IndexArgs) -> Result<i32> {
    if let Some(blob) = &args.blob {
        index_blob(
            blob,
            &args.output,
            &Utf8PathBuf::from(format!("{}.entries", args.output)),
            args.span,
            None,
        )?;
    } else if let Some(layout) = &args.layout {
        index_layout(layout, &args.output, args.span)?;
    }
    Ok(0)
}

/// Records a blob's checkpoints and, where the tar stream can be walked, its
/// entries.
///
/// `media_type` is what the manifest calls the layer. A blob indexed on its
/// own has no manifest to ask, so its format is taken from the bytes.
fn index_blob(
    blob: &Utf8Path,
    checkpoints: &Utf8Path,
    entries: &Utf8Path,
    span: u64,
    media_type: Option<&str>,
) -> Result<()> {
    let file =
        std::fs::File::open(blob).map_err(|source| Error::io(format!("opening {blob}"), source))?;
    let bytes =
        sys::Blob::of(&file).map_err(|source| Error::io(format!("reading {blob}"), source))?;
    let bytes: &[u8] = &bytes;
    // Which blob failed matters more than usual here: this runs as a build
    // action over every layer of an image at once.
    let named = |source| match source {
        Error::Io { context, source } => Error::io(format!("{context} {blob}"), source),
        other => other,
    };

    let compression = match media_type {
        Some(media_type) => extract::compression_of(media_type)
            .ok_or_else(|| Error::UnsupportedMediaType(media_type.to_string()))?,
        None => sniff(bytes).ok_or_else(|| {
            Error::io(
                format!("{blob} is not gzip or zstd"),
                std::io::Error::from(std::io::ErrorKind::InvalidData),
            )
        })?,
    };
    let flavor = match compression {
        extract::Compression::Gzip => zinfo::Flavor::Gzip,
        extract::Compression::Zstd => zinfo::Flavor::Zstd,
        // Nothing to resume from, so the layer is walked at run time.
        extract::Compression::None => return Ok(()),
    };

    let index = zinfo::Index::build(flavor, bytes, span).map_err(named)?;
    write_sidecar(checkpoints, |writer| index.write_to(writer))?;

    // A second pass rather than a second job for the decompressor: the entry
    // walk wants the tar stream in order, and the checkpoint pass keeps none
    // of it.
    //
    // A layer the walk cannot follow still extracts, because extraction reads
    // the stream itself; it just does not get planned. So the table is left
    // out rather than failing the build over it.
    match entries::Table::build(extract::decompressed(compression, bytes)) {
        Ok(table) => write_sidecar(entries, |writer| table.write_to(writer)),
        Err(err) => {
            log::warn(format!("not recording the entries of {blob}: {err}"));
            Ok(())
        }
    }
}

/// The compression a blob's first bytes announce.
fn sniff(bytes: &[u8]) -> Option<extract::Compression> {
    match bytes {
        [0x1f, 0x8b, ..] => Some(extract::Compression::Gzip),
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Some(extract::Compression::Zstd),
        _ if zinfo::skippable_frame_len(bytes).is_some() => Some(extract::Compression::Zstd),
        _ => None,
    }
}

fn write_sidecar(
    path: &Utf8Path,
    write: impl FnOnce(std::io::BufWriter<std::fs::File>) -> std::io::Result<()>,
) -> Result<()> {
    let file = std::fs::File::create(path)
        .map_err(|source| Error::io(format!("creating {path}"), source))?;
    write(std::io::BufWriter::new(file))
        .map_err(|source| Error::io(format!("writing {path}"), source))
}

/// Indexes every compressed layer of every manifest in the layout, so a
/// multi-architecture image gets indexes for whichever platform runs it.
///
/// Blobs are independent, so they are indexed concurrently. This runs as a
/// build action once per image, where the whole point is to spend time now
/// instead of at every container start.
fn index_layout(layout: &Utf8Path, output: &Utf8Path, span: u64) -> Result<()> {
    let layout = Layout::open(layout)?;
    std::fs::create_dir_all(output)
        .map_err(|source| Error::io(format!("creating {output}"), source))?;

    let mut indexed = std::collections::HashSet::new();
    let mut work = Vec::new();
    for manifest in layout.all_manifests()? {
        for layer in &manifest.layers {
            if extract::flavor_of(&layer.media_type).is_none() {
                continue;
            }
            let hex = image::parse_digest(&layer.digest)?.hex;
            if !indexed.insert(hex.clone()) {
                continue;
            }
            let blob = layout.blob_path(&layer.digest)?;
            work.push((
                blob,
                sidecar::checkpoints_at(output, &hex),
                sidecar::entries_at(output, &hex),
                layer.media_type.clone(),
            ));
        }
    }

    // Each worker holds one blob and the index it is building, so the width is
    // capped rather than following the core count.
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(work.len())
        .min(MAX_INDEX_WORKERS);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<Result<()>>>> =
        work.iter().map(|_| std::sync::Mutex::new(None)).collect();

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let (work, next, results) = (&work, &next, &results);
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((blob, checkpoints, entries, media_type)) = work.get(i) else {
                        break;
                    };
                    let result = index_blob(blob, checkpoints, entries, span, Some(media_type));
                    *results[i].lock().expect("index result") = Some(result);
                }
            });
        }
    });

    // Reported in layout order, so the same broken layout always fails the
    // same way however the work happened to be scheduled.
    for result in results {
        if let Some(result) = result.into_inner().expect("index result") {
            result?;
        }
    }
    Ok(())
}

fn parse_platform(value: Option<&str>) -> Result<Platform> {
    let Some(value) = value else {
        return Ok(Platform::host());
    };
    let mut fields = value.split('/');
    let os = fields.next().unwrap_or_default();
    let architecture = fields.next().unwrap_or_default();
    let variant = fields.next().map(str::to_string);
    if os.is_empty() || architecture.is_empty() || fields.next().is_some() {
        return Err(Error::io(
            format!("invalid platform {value:?}, expected OS/ARCH[/VARIANT]"),
            std::io::Error::from(std::io::ErrorKind::InvalidInput),
        ));
    }
    Ok(Platform {
        architecture: architecture.to_string(),
        os: os.to_string(),
        variant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_platform_is_used_by_default() {
        let platform = parse_platform(None).expect("platform");
        assert_eq!(platform, Platform::host());
    }

    #[test]
    fn explicit_platforms_are_parsed() {
        let platform = parse_platform(Some("linux/arm64")).expect("platform");
        assert_eq!(platform.os, "linux");
        assert_eq!(platform.architecture, "arm64");
        assert_eq!(platform.variant, None);
    }

    #[test]
    fn platform_variants_are_parsed() {
        let platform = parse_platform(Some("linux/arm/v7")).expect("platform");
        assert_eq!(platform.variant.as_deref(), Some("v7"));
    }

    #[test]
    fn malformed_platforms_are_rejected() {
        for value in ["", "linux", "linux/", "/amd64", "linux/amd64/v8/extra"] {
            assert!(
                parse_platform(Some(value)).is_err(),
                "expected {value:?} to be rejected"
            );
        }
    }

    mod indexing {
        use std::io::Write;

        use sha2::{Digest, Sha256};

        use super::super::*;

        fn scratch(name: &str) -> Utf8PathBuf {
            let dir = Utf8PathBuf::from(std::env::temp_dir().to_str().expect("utf-8 tmpdir"))
                .join(format!("oci-runtime-index-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("blobs/sha256")).expect("create layout");
            std::fs::write(dir.join("oci-layout"), "{}").expect("oci-layout");
            dir
        }

        /// Stores `bytes` under its digest and returns a descriptor JSON fragment.
        fn install_blob(root: &Utf8Path, media_type: &str, bytes: &[u8]) -> String {
            let hex = image::hex_encode(&Sha256::digest(bytes));
            std::fs::write(root.join("blobs/sha256").join(&hex), bytes).expect("write blob");
            format!(
                r#"{{"mediaType": "{media_type}", "digest": "sha256:{hex}", "size": {}}}"#,
                bytes.len()
            )
        }

        fn gzip(bytes: &[u8]) -> Vec<u8> {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(bytes).expect("compress");
            encoder.finish().expect("finish")
        }

        fn zstd(bytes: &[u8]) -> Vec<u8> {
            ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
        }

        #[test]
        fn a_layout_gets_one_index_per_compressed_layer() {
            let root = scratch("layout");
            let gzip_layer = gzip(b"pretend this is a tar");
            let gzip_hex = image::hex_encode(&Sha256::digest(&gzip_layer));
            let zstd_layer = zstd(b"pretend this is another tar");
            let zstd_hex = image::hex_encode(&Sha256::digest(&zstd_layer));

            let gzip_descriptor = install_blob(
                &root,
                "application/vnd.oci.image.layer.v1.tar+gzip",
                &gzip_layer,
            );
            let zstd_descriptor = install_blob(
                &root,
                "application/vnd.oci.image.layer.v1.tar+zstd",
                &zstd_layer,
            );
            let plain_descriptor = install_blob(
                &root,
                "application/vnd.oci.image.layer.v1.tar",
                b"uncompressed tar",
            );
            let config_descriptor =
                install_blob(&root, "application/vnd.oci.image.config.v1+json", b"{}");
            // The gzip layer appears twice, as in a multi-platform image
            // sharing a base layer: it must be indexed once.
            let manifest = format!(
                r#"{{"config": {config_descriptor}, "layers": [{gzip_descriptor}, {plain_descriptor}, {gzip_descriptor}, {zstd_descriptor}]}}"#,
            );
            let manifest_descriptor = install_blob(
                &root,
                "application/vnd.oci.image.manifest.v1+json",
                manifest.as_bytes(),
            );
            std::fs::write(
                root.join("index.json"),
                format!(r#"{{"manifests": [{manifest_descriptor}]}}"#),
            )
            .expect("index.json");

            let output = root.join("indexes");
            index_layout(&root, &output, 4 << 20).expect("index the layout");

            let mut entries: Vec<String> = std::fs::read_dir(&output)
                .expect("read output")
                .map(|entry| {
                    entry
                        .expect("entry")
                        .file_name()
                        .into_string()
                        .expect("utf-8")
                })
                .collect();
            entries.sort();
            let mut expected = [format!("{gzip_hex}.zinfo"), format!("{zstd_hex}.zinfo")];
            expected.sort();
            assert_eq!(entries, expected);

            for (name, flavor) in [
                (format!("{gzip_hex}.zinfo"), zinfo::Flavor::Gzip),
                (format!("{zstd_hex}.zinfo"), zinfo::Flavor::Zstd),
            ] {
                let file = std::fs::File::open(output.join(name)).expect("open index");
                let index = zinfo::Index::read_from(std::io::BufReader::new(file))
                    .expect("a well-formed index");
                assert_eq!(index.flavor, flavor);
            }

            std::fs::remove_dir_all(&root).expect("cleanup");
        }

        /// Blobs are indexed concurrently, so which failure surfaces must come
        /// from the layout rather than from whichever worker finished first.
        #[test]
        fn the_first_broken_layer_in_the_layout_is_the_one_reported() {
            let root = scratch("broken");
            // Both are truncated gzip streams, so both fail to index.
            let first = install_blob(
                &root,
                "application/vnd.oci.image.layer.v1.tar+gzip",
                &gzip(b"first")[..12],
            );
            let second = install_blob(
                &root,
                "application/vnd.oci.image.layer.v1.tar+gzip",
                &gzip(b"second")[..12],
            );
            let config_descriptor =
                install_blob(&root, "application/vnd.oci.image.config.v1+json", b"{}");
            assert_ne!(first, second, "the two layers must be distinct blobs");
            let manifest =
                format!(r#"{{"config": {config_descriptor}, "layers": [{first}, {second}]}}"#);
            let manifest_descriptor = install_blob(
                &root,
                "application/vnd.oci.image.manifest.v1+json",
                manifest.as_bytes(),
            );
            std::fs::write(
                root.join("index.json"),
                format!(r#"{{"manifests": [{manifest_descriptor}]}}"#),
            )
            .expect("index.json");

            let first_hex = image::hex_encode(&Sha256::digest(&gzip(b"first")[..12]));
            for _ in 0..8 {
                let err = index_layout(&root, &root.join("indexes"), 4 << 20)
                    .expect_err("a truncated blob cannot be indexed");
                assert!(
                    err.to_string().contains(&first_hex),
                    "expected the first layer to be reported, got {err}"
                );
            }

            std::fs::remove_dir_all(&root).expect("cleanup");
        }
    }
}
