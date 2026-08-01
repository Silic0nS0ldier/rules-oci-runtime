mod bundle;
mod cli;
mod error;
mod extract;
mod fsutil;
mod image;
mod launcher;
mod log;
mod runtime;
mod spec;
mod sys;

use camino::Utf8PathBuf;
use clap::Parser;

use crate::bundle::Bundle;
use crate::cli::{Cli, Command, RunArgs};
use crate::error::{Error, Result};
use crate::extract::RootfsExtractor;
use crate::image::{Layout, Platform};
use crate::log::log;
use crate::runtime::{ContainerRuntime, RunRequest, Runc};
use crate::spec::{BindMount, Spec, SpecOptions};

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
    let mut extractor = RootfsExtractor::new(&rootfs)?;
    for layer in &manifest.layers {
        extractor.apply_layer(&layout, layer)?;
    }

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
}
