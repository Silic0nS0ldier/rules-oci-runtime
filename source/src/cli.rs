//! Command line surface. Rule supplied flags come first, then whatever the
//! user passed after `bazel run ... --`.

use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "oci_runtime",
    version,
    about = "Run an OCI image from a Bazel-provided image layout"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Unpack an image layout into a bundle and run it.
    Run(Box<RunArgs>),
    /// Build parallel-decompression checkpoint indexes for gzip blobs.
    Index(IndexArgs),
    /// Check that a recorded profile still describes an image.
    Profile(ProfileArgs),
}

#[derive(Debug, Args)]
pub struct ProfileArgs {
    /// The profile to check.
    #[arg(long, value_name = "PATH")]
    pub profile: Utf8PathBuf,

    /// Image layout the profile is used with.
    #[arg(long, value_name = "DIR")]
    pub layout: Utf8PathBuf,

    /// Directory of the layer sidecars, which is what says what the image
    /// holds without reading it.
    #[arg(long, value_name = "DIR")]
    pub index: Utf8PathBuf,

    /// File to write when the profile passes, for a build that needs an
    /// output to depend on.
    #[arg(long, value_name = "PATH")]
    pub stamp: Option<Utf8PathBuf>,
}

#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Gzip or zstd layer blob to index.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "layout",
        required_unless_present = "layout"
    )]
    pub blob: Option<Utf8PathBuf>,

    /// Image layout whose compressed layers are all indexed, across every
    /// platform.
    #[arg(long, value_name = "DIR")]
    pub layout: Option<Utf8PathBuf>,

    /// Index file to write for `--blob`, or a directory of `<hex>.zinfo`
    /// files for `--layout`.
    #[arg(long, value_name = "PATH")]
    pub output: Utf8PathBuf,

    /// Target uncompressed distance between checkpoints, in bytes.
    #[arg(long, value_name = "BYTES", default_value_t = 4 << 20)]
    pub span: u64,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Directory containing `index.json`, `oci-layout` and `blobs/`.
    #[arg(long, value_name = "DIR")]
    pub layout: Utf8PathBuf,

    /// Path to the OCI runtime binary (runc).
    #[arg(long, value_name = "PATH")]
    pub runtime: Utf8PathBuf,

    /// Whether to serve the root filesystem from the layers, fetching files as
    /// the container opens them, or extract it up front. `auto` serves it
    /// wherever the host and the image allow.
    #[arg(long, value_enum, default_value_t = RootfsMode::Auto)]
    pub rootfs: RootfsMode,

    /// Directory of layer decompression indexes, one `<hex>.zinfo` per
    /// compressed layer.
    #[arg(long, value_name = "DIR")]
    pub index: Option<Utf8PathBuf>,

    /// Profile of what a container read, fetched ahead of this one. Repeatable:
    /// the profile recorded for the image platform in use is the one read.
    #[arg(long = "profile", value_name = "PATH")]
    pub profiles: Vec<Utf8PathBuf>,

    /// Record what the container reads, merging into whatever profile is
    /// there already. Given no `=PATH` it records where the rule said to,
    /// under `$BUILD_WORKSPACE_DIRECTORY`; the platform is appended to the
    /// name either way.
    #[arg(
        long,
        value_name = "PATH",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = ""
    )]
    pub record_profile: Option<Utf8PathBuf>,

    /// How many of a profile's files to fetch before the container starts. The
    /// rest are fetched while it runs.
    #[arg(long, value_name = "COUNT", default_value_t = 1)]
    pub prefetch_barrier: usize,

    /// Where `--record-profile` writes when given no path, relative to
    /// `$BUILD_WORKSPACE_DIRECTORY`. Supplied by the rule.
    #[arg(long, value_name = "PATH")]
    pub profile_base: Option<Utf8PathBuf>,

    /// Additional environment variables, overriding the image.
    #[arg(long = "env", short = 'e', value_name = "NAME=VALUE")]
    pub env: Vec<String>,

    /// Bind mounts. `$VAR` and `${VAR}` are expanded in the source path.
    #[arg(
        long = "mount",
        short = 'v',
        value_name = "SOURCE:DESTINATION[:OPTIONS]"
    )]
    pub mounts: Vec<String>,

    /// Working directory inside the container, overriding the image.
    #[arg(long, value_name = "DIR")]
    pub workdir: Option<String>,

    /// Container hostname.
    #[arg(long, value_name = "NAME")]
    pub hostname: Option<String>,

    /// Image platform to select from a multi-architecture index.
    #[arg(long, value_name = "OS/ARCH")]
    pub platform: Option<String>,

    /// Allocate a pseudo terminal. Defaults to whether stdin is a terminal.
    #[arg(long, value_enum, default_value_t = Toggle::Auto)]
    pub tty: Toggle,

    /// Run in a user namespace. Defaults to whether the caller is unprivileged.
    #[arg(long, value_enum, default_value_t = Toggle::Auto)]
    pub rootless: Toggle,

    /// Mount the root filesystem read-only.
    #[arg(long)]
    pub read_only: bool,

    /// Refuse an image whose layers set extended attributes, which are never
    /// restored. Set false to extract it anyway.
    #[arg(long, value_name = "BOOL", action = clap::ArgAction::Set, default_value_t = true)]
    pub strict_xattrs: bool,

    /// Leave the bundle on disk after the container exits.
    #[arg(long)]
    pub keep_bundle: bool,

    /// Log container setup to stderr.
    #[arg(long)]
    pub verbose: bool,

    /// Command to run, replacing the image `Cmd`.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Toggle {
    Auto,
    True,
    False,
}

/// Where the container's root filesystem comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RootfsMode {
    /// Serve the image where the host and the image both allow it, and extract
    /// it where they do not.
    Auto,
    /// Serve the image, failing the run if it cannot be served.
    Fuse,
    /// Extract the image before the container starts.
    Extract,
}

impl Toggle {
    pub fn resolve(self, detected: bool) -> bool {
        match self {
            Toggle::Auto => detected,
            Toggle::True => true,
            Toggle::False => false,
        }
    }
}

/// Expands `$VAR` and `${VAR}` so rules can refer to values only known at run
/// time, such as `$BUILD_WORKSPACE_DIRECTORY`. Unset variables expand to "".
pub fn expand_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('{') => {
                chars.next();
                let mut name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }
                if closed {
                    out.push_str(&std::env::var(&name).unwrap_or_default());
                } else {
                    out.push_str("${");
                    out.push_str(&name);
                }
            }
            Some(c) if c.is_ascii_alphabetic() || *c == '_' => {
                let mut name = String::new();
                while let Some(c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || *c == '_' {
                        name.push(*c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(&std::env::var(&name).unwrap_or_default());
            }
            _ => out.push('$'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> RunArgs {
        let mut argv = vec!["oci_runtime", "run", "--layout", "/l", "--runtime", "/r"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv).expect("parse").command {
            Command::Run(args) => *args,
            other => panic!("expected `run`, parsed {other:?}"),
        }
    }

    #[test]
    fn required_flags_are_captured() {
        let args = parse(&[]);
        assert_eq!(args.layout, "/l");
        assert_eq!(args.runtime, "/r");
        assert!(args.command.is_empty());
    }

    #[test]
    fn the_trailing_command_keeps_its_flags() {
        let args = parse(&["/bin/sh", "-c", "echo hi"]);
        assert_eq!(args.command, vec!["/bin/sh", "-c", "echo hi"]);
    }

    #[test]
    fn a_double_dash_protects_leading_flags() {
        let args = parse(&["--", "-c", "echo hi"]);
        assert_eq!(args.command, vec!["-c", "echo hi"]);
    }

    #[test]
    fn env_and_mounts_accumulate() {
        let args = parse(&[
            "--env", "A=1", "-e", "B=2", "--mount", "/a:/b", "-v", "/c:/d:ro",
        ]);
        assert_eq!(args.env, vec!["A=1", "B=2"]);
        assert_eq!(args.mounts, vec!["/a:/b", "/c:/d:ro"]);
    }

    #[test]
    fn index_takes_a_blob_or_a_layout_but_not_both() {
        let blob = ["oci_runtime", "index", "--blob", "/b", "--output", "/o"];
        assert!(Cli::try_parse_from(blob).is_ok());

        let layout = ["oci_runtime", "index", "--layout", "/l", "--output", "/o"];
        assert!(Cli::try_parse_from(layout).is_ok());

        let neither = ["oci_runtime", "index", "--output", "/o"];
        assert!(Cli::try_parse_from(neither).is_err());

        let both = [
            "oci_runtime",
            "index",
            "--blob",
            "/b",
            "--layout",
            "/l",
            "--output",
            "/o",
        ];
        assert!(Cli::try_parse_from(both).is_err());
    }

    /// An image asking for something the container will not get is refused
    /// unless the caller says otherwise, so the default is the strict one.
    #[test]
    fn extended_attributes_are_strict_unless_waived() {
        assert!(parse(&[]).strict_xattrs);
        assert!(!parse(&["--strict-xattrs", "false"]).strict_xattrs);
        assert!(parse(&["--strict-xattrs=true"]).strict_xattrs);
    }

    #[test]
    fn toggles_default_to_auto() {
        let args = parse(&[]);
        assert_eq!(args.tty, Toggle::Auto);
        assert_eq!(args.rootless, Toggle::Auto);
        assert!(!args.read_only);
        assert!(!args.verbose);
    }

    #[test]
    fn toggles_can_be_forced() {
        let args = parse(&["--tty", "false", "--rootless", "true"]);
        assert!(!args.tty.resolve(true));
        assert!(args.rootless.resolve(false));
    }

    #[test]
    fn auto_toggles_follow_detection() {
        assert!(Toggle::Auto.resolve(true));
        assert!(!Toggle::Auto.resolve(false));
    }

    #[test]
    fn missing_required_flags_are_an_error() {
        assert!(Cli::try_parse_from(["oci_runtime", "run"]).is_err());
    }

    #[test]
    fn environment_references_are_expanded() {
        unsafe { std::env::set_var("OCI_RUNTIME_TEST_VAR", "/workspace") };
        assert_eq!(expand_env("$OCI_RUNTIME_TEST_VAR:/src"), "/workspace:/src");
        assert_eq!(expand_env("${OCI_RUNTIME_TEST_VAR}/sub"), "/workspace/sub");
        unsafe { std::env::remove_var("OCI_RUNTIME_TEST_VAR") };
    }

    #[test]
    fn unset_variables_expand_to_nothing() {
        assert_eq!(expand_env("${OCI_RUNTIME_UNSET_VAR}/x"), "/x");
    }

    #[test]
    fn literal_dollars_are_left_alone() {
        assert_eq!(expand_env("/a$/b"), "/a$/b");
        assert_eq!(expand_env("/a$"), "/a$");
        assert_eq!(expand_env("${unterminated"), "${unterminated");
        assert_eq!(expand_env("/no/variables"), "/no/variables");
    }
}
