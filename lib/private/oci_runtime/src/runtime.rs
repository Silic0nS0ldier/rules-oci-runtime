//! Handing a prepared bundle to an OCI runtime. Only runc is implemented, but
//! the trait keeps the door open for crun and youki.

use std::process::{Command, Stdio};

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{Error, Result};
use crate::log::log;
use crate::sys;

pub struct RunRequest<'a> {
    pub id: &'a str,
    pub bundle: &'a Utf8Path,
    /// Runtime state lives here so concurrent runs never share `/run/runc`.
    pub state_dir: &'a Utf8Path,
}

pub trait ContainerRuntime {
    fn name(&self) -> &str;

    /// Runs the container in the foreground and returns its exit code.
    fn run(&self, request: &RunRequest<'_>) -> Result<i32>;

    /// Best effort removal of runtime state and any surviving processes.
    fn delete(&self, request: &RunRequest<'_>);
}

pub struct Runc {
    binary: Utf8PathBuf,
}

impl Runc {
    pub fn new(binary: &Utf8Path) -> Self {
        Runc {
            binary: binary.to_owned(),
        }
    }

    fn command(&self, request: &RunRequest<'_>) -> Command {
        let mut command = Command::new(self.binary.as_std_path());
        command.arg("--root").arg(request.state_dir.as_std_path());
        command
    }
}

impl ContainerRuntime for Runc {
    fn name(&self) -> &str {
        "runc"
    }

    fn run(&self, request: &RunRequest<'_>) -> Result<i32> {
        let mut command = self.command(request);
        command
            .arg("run")
            .arg("--bundle")
            .arg(request.bundle.as_std_path())
            .arg(request.id)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        log!("Running container {}", request.id);
        let mut child = command.spawn().map_err(|source| Error::RuntimeSpawn {
            program: self.binary.to_string(),
            source,
        })?;

        let _forwarder = sys::SignalForwarder::install(child.id() as i32);
        let status = child
            .wait()
            .map_err(|source| Error::RuntimeSpawn {
                program: self.binary.to_string(),
                source,
            })?;
        Ok(sys::exit_code(status))
    }

    fn delete(&self, request: &RunRequest<'_>) {
        let mut command = self.command(request);
        let result = command
            .arg("delete")
            .arg("--force")
            .arg(request.id)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Err(err) = result {
            log!("Could not delete container {}: {err}", request.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runc_is_named() {
        assert_eq!(Runc::new(Utf8Path::new("/usr/bin/runc")).name(), "runc");
    }

    #[test]
    fn state_is_kept_out_of_the_shared_runc_root() {
        let runc = Runc::new(Utf8Path::new("/usr/bin/runc"));
        let request = RunRequest {
            id: "test",
            bundle: Utf8Path::new("/tmp/bundle"),
            state_dir: Utf8Path::new("/tmp/state"),
        };
        let command = runc.command(&request);
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, vec!["--root".to_string(), "/tmp/state".to_string()]);
    }

    #[test]
    fn missing_runtimes_report_the_program_path() {
        let runc = Runc::new(Utf8Path::new("/nonexistent/runc"));
        let request = RunRequest {
            id: "test",
            bundle: Utf8Path::new("/tmp/bundle"),
            state_dir: Utf8Path::new("/tmp/state"),
        };
        match runc.run(&request) {
            Err(Error::RuntimeSpawn { program, .. }) => assert_eq!(program, "/nonexistent/runc"),
            other => panic!("expected a spawn failure, got {other:?}"),
        }
    }
}
