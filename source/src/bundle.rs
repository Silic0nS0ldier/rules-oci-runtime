//! Assembling an OCI bundle: rootfs, network files and `config.json`.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{Error, IoContext, Result};
use crate::fsutil;
use crate::log::{log, warning};
use crate::spec::Spec;

/// Hidden first argument of the detached process that removes a bundle.
const REMOVE_ARG: &str = "__remove";

/// Suffix given to a bundle once it has been renamed out of the way.
const REMOVE_SUFFIX: &str = ".removing";

/// The bundle a detached remover was asked to delete, or `None` for an ordinary
/// invocation. Checked before the launcher sidecar so a bundle path can never
/// be mistaken for a command line.
pub fn remover_target(argv: &[String]) -> Option<&str> {
    match argv {
        [_, flag, path] if flag == REMOVE_ARG => Some(path.as_str()),
        _ => None,
    }
}

/// A temporary directory holding the bundle and the runtime's state, removed
/// when this value is dropped.
pub struct Bundle {
    root: Utf8PathBuf,
    keep: bool,
}

impl Bundle {
    pub fn create(parent: &Utf8Path, id: &str, keep: bool) -> Result<Self> {
        let root = parent.join(id);
        fs::create_dir_all(&root).io_context(|| format!("creating {root}"))?;
        // The bundle can contain host paths under a shared /tmp.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .io_context(|| format!("securing {root}"))?;
        let bundle = Bundle { root, keep };
        fs::create_dir_all(bundle.rootfs())
            .io_context(|| format!("creating {}", bundle.rootfs()))?;
        fs::create_dir_all(bundle.state_dir())
            .io_context(|| format!("creating {}", bundle.state_dir()))?;
        Ok(bundle)
    }

    pub fn dir(&self) -> &Utf8Path {
        &self.root
    }

    pub fn rootfs(&self) -> Utf8PathBuf {
        self.root.join("rootfs")
    }

    pub fn state_dir(&self) -> Utf8PathBuf {
        self.root.join("state")
    }

    /// Where a served rootfs keeps the bytes of the files something opened.
    /// Inside the bundle, so it is taken away with everything else.
    pub fn backing_dir(&self) -> Utf8PathBuf {
        self.root.join("backing")
    }

    pub fn write_config(&self, spec: &Spec) -> Result<()> {
        let path = self.root.join("config.json");
        let json = serde_json::to_vec_pretty(spec)
            .map_err(|source| Error::json("serialising config.json", source))?;
        fs::write(&path, json).io_context(|| format!("writing {path}"))?;
        log!("Wrote {path}");
        Ok(())
    }
}

impl Drop for Bundle {
    fn drop(&mut self) {
        if self.keep {
            crate::log::warn(format!("keeping bundle at {}", self.root));
            return;
        }
        // Renaming first means the bundle is gone the moment this returns, even
        // though deleting a large rootfs takes far longer than the run itself.
        let staged = match self.stage_for_removal() {
            Some(staged) => staged,
            None => self.root.clone(),
        };
        if spawn_remover(&staged) {
            return;
        }
        if let Err(err) = fsutil::force_remove_dir_all(staged.as_std_path()) {
            warning!("could not clean up {staged}: {err}");
        }
    }
}

impl Bundle {
    /// The bundle identifier is random, so the staged name cannot collide.
    fn stage_for_removal(&self) -> Option<Utf8PathBuf> {
        let staged = Utf8PathBuf::from(format!("{}{REMOVE_SUFFIX}", self.root));
        match fs::rename(&self.root, &staged) {
            Ok(()) => Some(staged),
            Err(err) => {
                log!("Could not stage {} for removal: {err}", self.root);
                None
            }
        }
    }
}

/// Hands the tree to a copy of this binary that outlives it. Detaching from the
/// session keeps a terminal signal from orphaning a half deleted tree, and null
/// standard streams keep the child from holding a caller's pipe open.
fn spawn_remover(path: &Utf8Path) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut command = Command::new(exe);
    command
        .arg(REMOVE_ARG)
        .arg(path.as_std_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches nothing this process owns.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match command.spawn() {
        Ok(_) => true,
        Err(err) => {
            log!("Could not spawn a background remover for {path}: {err}");
            false
        }
    }
}

/// Deletes a staged bundle. The exit code is ignored by the parent, so failures
/// are silent by design: the tree lives under a temporary directory either way.
pub fn remove_staged(path: &str) {
    let _ = fsutil::force_remove_dir_all(Path::new(path));
}

/// Copies the host resolver configuration and writes `/etc/hosts` and
/// `/etc/hostname`. The container shares the host network namespace, so host
/// resolver settings are the correct ones to use.
pub fn install_network_files(rootfs: &Utf8Path, hostname: &str) -> Result<()> {
    let etc = rootfs.join("etc");
    match fs::create_dir_all(&etc) {
        Ok(()) => {}
        Err(err) => {
            warning!("could not create {etc}: {err}, skipping network configuration");
            return Ok(());
        }
    }
    ensure_writable(&etc);

    copy_host_file("/etc/resolv.conf", &etc.join("resolv.conf"))?;

    let hosts = etc.join("hosts");
    if !hosts.exists() {
        let contents = format!(
            "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n127.0.1.1\t{hostname}\n"
        );
        write_container_file(&hosts, contents.as_bytes())?;
    }

    write_container_file(&etc.join("hostname"), format!("{hostname}\n").as_bytes())
}

fn copy_host_file(source: &str, destination: &Utf8Path) -> Result<()> {
    match fs::read(source) {
        Ok(contents) => {
            log!("Copying host {source} into the container");
            write_container_file(destination, &contents)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            warning!("host {source} not found, containers may not resolve names");
            Ok(())
        }
        Err(err) => {
            warning!("could not read host {source}: {err}");
            Ok(())
        }
    }
}

fn write_container_file(path: &Utf8Path, contents: &[u8]) -> Result<()> {
    // Layers may ship these paths as symlinks or read-only files.
    fsutil::remove_any(path.as_std_path())?;
    match fs::write(path, contents) {
        Ok(()) => {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
            Ok(())
        }
        Err(err) => {
            warning!("could not write {path}: {err}");
            Ok(())
        }
    }
}

fn ensure_writable(path: &Utf8Path) {
    if let Ok(metadata) = fs::metadata(path) {
        let mode = metadata.permissions().mode();
        if mode & 0o700 != 0o700 {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> Utf8PathBuf {
        let dir = Utf8PathBuf::from(std::env::temp_dir().to_string_lossy().into_owned())
            .join(format!("oci-runtime-test-{name}-{}", std::process::id()));
        let _ = fsutil::force_remove_dir_all(dir.as_std_path());
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn bundles_create_their_subdirectories() {
        let parent = scratch("bundle-create");
        let bundle = Bundle::create(&parent, "abc", false).expect("bundle");
        assert!(bundle.rootfs().is_dir());
        assert!(bundle.state_dir().is_dir());
        assert_eq!(bundle.dir(), parent.join("abc"));
        let _ = fsutil::force_remove_dir_all(parent.as_std_path());
    }

    #[test]
    fn bundles_are_private_to_the_owner() {
        let parent = scratch("bundle-perms");
        let bundle = Bundle::create(&parent, "abc", false).expect("bundle");
        let mode = fs::metadata(bundle.dir())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let _ = fsutil::force_remove_dir_all(parent.as_std_path());
    }

    #[test]
    fn dropping_a_bundle_removes_it() {
        let parent = scratch("bundle-drop");
        let path = {
            let bundle = Bundle::create(&parent, "abc", false).expect("bundle");
            // A read-only directory must not defeat cleanup.
            let locked = bundle.rootfs().join("locked");
            fs::create_dir_all(&locked).expect("dir");
            fs::write(locked.join("file"), b"x").expect("file");
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("chmod");
            bundle.dir().to_owned()
        };
        assert!(!path.exists());
        // Under `cargo test` the spawned remover is the test binary, which
        // deletes nothing, so the staged tree is cleared here instead.
        let _ = fsutil::force_remove_dir_all(parent.as_std_path());
    }

    #[test]
    fn removers_are_recognised_by_their_argument_list() {
        let argv = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(
            remover_target(&argv(&["oci_runtime", REMOVE_ARG, "/tmp/bundle"])),
            Some("/tmp/bundle")
        );
        assert_eq!(remover_target(&argv(&["oci_runtime", REMOVE_ARG])), None);
        assert_eq!(
            remover_target(&argv(&["oci_runtime", "run", "--layout"])),
            None
        );
        assert_eq!(
            remover_target(&argv(&["oci_runtime", REMOVE_ARG, "/tmp/bundle", "extra"])),
            None
        );
    }

    #[test]
    fn staging_moves_the_bundle_aside() {
        let parent = scratch("bundle-stage");
        let bundle = Bundle::create(&parent, "abc", true).expect("bundle");
        let staged = bundle.stage_for_removal().expect("staged");
        assert_eq!(staged, parent.join(format!("abc{REMOVE_SUFFIX}")));
        assert!(!bundle.dir().exists());
        assert!(staged.is_dir());
        let _ = fsutil::force_remove_dir_all(parent.as_std_path());
    }

    #[test]
    fn keeping_a_bundle_leaves_it_behind() {
        let parent = scratch("bundle-keep");
        let path = {
            let bundle = Bundle::create(&parent, "abc", true).expect("bundle");
            bundle.dir().to_owned()
        };
        assert!(path.exists());
        let _ = fsutil::force_remove_dir_all(parent.as_std_path());
    }

    #[test]
    fn network_files_replace_read_only_layer_content() {
        let parent = scratch("network-files");
        let rootfs = parent.join("rootfs");
        let etc = rootfs.join("etc");
        fs::create_dir_all(&etc).expect("etc");
        fs::write(etc.join("hostname"), b"from-layer").expect("hostname");
        fs::set_permissions(etc.join("hostname"), fs::Permissions::from_mode(0o444))
            .expect("chmod");

        install_network_files(&rootfs, "container-1").expect("network files");

        assert_eq!(
            fs::read_to_string(etc.join("hostname")).expect("hostname"),
            "container-1\n"
        );
        let hosts = fs::read_to_string(etc.join("hosts")).expect("hosts");
        assert!(hosts.contains("127.0.0.1\tlocalhost"));
        assert!(hosts.contains("container-1"));
        let _ = fsutil::force_remove_dir_all(parent.as_std_path());
    }

    #[test]
    fn existing_hosts_files_are_preserved() {
        let parent = scratch("network-hosts");
        let rootfs = parent.join("rootfs");
        let etc = rootfs.join("etc");
        fs::create_dir_all(&etc).expect("etc");
        fs::write(etc.join("hosts"), b"# from layer\n").expect("hosts");

        install_network_files(&rootfs, "container-1").expect("network files");

        assert_eq!(
            fs::read_to_string(etc.join("hosts")).expect("hosts"),
            "# from layer\n"
        );
        let _ = fsutil::force_remove_dir_all(parent.as_std_path());
    }

    #[test]
    fn network_files_are_created_when_etc_is_missing() {
        let parent = scratch("network-no-etc");
        let rootfs = parent.join("rootfs");
        fs::create_dir_all(&rootfs).expect("rootfs");

        install_network_files(&rootfs, "container-1").expect("network files");

        assert!(rootfs.join("etc/hostname").is_file());
        assert!(rootfs.join("etc/hosts").is_file());
        let _ = fsutil::force_remove_dir_all(parent.as_std_path());
    }
}
