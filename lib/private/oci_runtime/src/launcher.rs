//! Launcher mode. `runc_binary` symlinks this binary as the target executable
//! and writes a sidecar config beside it, so running an image needs no shell
//! wrapper. Invoked without a sidecar, the binary is an ordinary CLI.

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;

use crate::error::{Error, IoContext, JsonContext, Result};

/// Suffix of the sidecar config, appended to the launcher's own path.
pub const CONFIG_SUFFIX: &str = ".launch.json";

/// Paths are runfiles relative, since the runfiles root is only known at run time.
#[derive(Debug, Deserialize)]
struct Config {
    layout: String,
    runtime: String,
    #[serde(default)]
    args: Vec<String>,
}

/// The `run` command line to parse when invoked through a launcher symlink, or
/// `None` when there is no sidecar config and the real arguments should be used.
pub fn command_line() -> Result<Option<Vec<String>>> {
    let argv: Vec<String> = std::env::args().collect();
    let Some(argv0) = argv.first() else {
        return Ok(None);
    };
    let config_path = Utf8PathBuf::from(format!("{argv0}{CONFIG_SUFFIX}"));
    if !config_path.is_file() {
        return Ok(None);
    }
    let config = read_config(&config_path)?;
    let runfiles = runfiles_dir(argv0)?;
    Ok(Some(config.command_line(argv0, &runfiles, &argv[1..])))
}

fn read_config(path: &Utf8Path) -> Result<Config> {
    let text =
        std::fs::read_to_string(path).io_context(|| format!("reading launcher config {path}"))?;
    serde_json::from_str(&text).json_context(|| format!("parsing launcher config {path}"))
}

/// Bazel exports `RUNFILES_DIR` when it launches the target, and `TEST_SRCDIR`
/// under `bazel test`. Neither is set when the launcher is run straight from
/// `bazel-bin`, where the tree sits next to the executable.
fn runfiles_dir(argv0: &str) -> Result<Utf8PathBuf> {
    let from_env = ["RUNFILES_DIR", "TEST_SRCDIR"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .filter(|value| !value.is_empty())
        .map(Utf8PathBuf::from);
    let candidates = from_env.chain(std::iter::once(Utf8PathBuf::from(format!(
        "{argv0}.runfiles"
    ))));
    for candidate in candidates {
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }
    Err(Error::MissingRunfiles(argv0.to_string()))
}

impl Config {
    /// Rule supplied arguments come first so that user arguments can override them.
    fn command_line(&self, argv0: &str, runfiles: &Utf8Path, user: &[String]) -> Vec<String> {
        let mut argv = vec![
            argv0.to_string(),
            "run".to_string(),
            "--layout".to_string(),
            runfiles.join(&self.layout).into_string(),
            "--runtime".to_string(),
            runfiles.join(&self.runtime).into_string(),
        ];
        argv.extend(self.args.iter().cloned());
        argv.extend(user.iter().cloned());
        argv
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        serde_json::from_str(
            r#"{"layout": "ws/pkg/image", "runtime": "tools/runc", "args": ["--read-only"]}"#,
        )
        .expect("config")
    }

    #[test]
    fn args_default_to_empty() {
        let config: Config =
            serde_json::from_str(r#"{"layout": "l", "runtime": "r"}"#).expect("config");
        assert!(config.args.is_empty());
    }

    #[test]
    fn paths_are_resolved_against_the_runfiles_root() {
        let argv = config().command_line("launcher", Utf8Path::new("/tmp/rf"), &[]);
        assert_eq!(
            argv,
            [
                "launcher",
                "run",
                "--layout",
                "/tmp/rf/ws/pkg/image",
                "--runtime",
                "/tmp/rf/tools/runc",
                "--read-only",
            ]
        );
    }

    #[test]
    fn user_arguments_come_last() {
        let user = ["/bin/echo".to_string(), "hi".to_string()];
        let argv = config().command_line("launcher", Utf8Path::new("/tmp/rf"), &user);
        assert_eq!(&argv[argv.len() - 3..], ["--read-only", "/bin/echo", "hi"]);
    }

    #[test]
    fn the_parsed_command_line_round_trips_through_clap() {
        use clap::Parser;

        let argv = config().command_line(
            "launcher",
            Utf8Path::new("/tmp/rf"),
            &["/bin/echo".to_string(), "--not-a-flag".to_string()],
        );
        let crate::cli::Command::Run(args) = crate::cli::Cli::parse_from(argv).command;
        assert_eq!(args.layout, "/tmp/rf/ws/pkg/image");
        assert_eq!(args.runtime, "/tmp/rf/tools/runc");
        assert!(args.read_only);
        assert_eq!(args.command, ["/bin/echo", "--not-a-flag"]);
    }
}
