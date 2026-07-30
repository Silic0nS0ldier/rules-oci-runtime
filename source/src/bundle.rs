//! Assembling an OCI bundle: rootfs, network files and `config.json`.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{Error, IoContext, Result};
use crate::fsutil;
use crate::log::{log, warning};
use crate::spec::Spec;

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
        fs::create_dir_all(bundle.rootfs()).io_context(|| format!("creating {}", bundle.rootfs()))?;
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
        if let Err(err) = fsutil::force_remove_dir_all(self.root.as_std_path()) {
            warning!("could not clean up {}: {err}", self.root);
        }
    }
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
        let mode = fs::metadata(bundle.dir()).expect("metadata").permissions().mode();
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
