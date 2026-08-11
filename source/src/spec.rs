//! Generating an OCI runtime configuration (`config.json`), replacing the
//! previous `runc spec --rootless` + `jq` pipeline.

use serde::Serialize;

use crate::error::{Error, Result};
use crate::image::ImageConfig;

/// The runtime-spec version implemented here. Matches runc 1.3.
pub const OCI_VERSION: &str = "1.2.1";

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

const DEFAULT_CAPABILITIES: [&str; 3] = ["CAP_AUDIT_WRITE", "CAP_KILL", "CAP_NET_BIND_SERVICE"];

const MASKED_PATHS: [&str; 10] = [
    "/proc/acpi",
    "/proc/asound",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/proc/sched_debug",
    "/sys/firmware",
    "/proc/scsi",
];

const READONLY_PATHS: [&str; 5] = [
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Spec {
    pub oci_version: String,
    pub process: Process,
    pub root: Root,
    pub hostname: String,
    pub mounts: Vec<Mount>,
    pub linux: Linux,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Process {
    pub terminal: bool,
    pub user: User,
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub capabilities: Capabilities,
    pub rlimits: Vec<Rlimit>,
    pub no_new_privileges: bool,
}

#[derive(Debug, Serialize)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Serialize)]
pub struct Capabilities {
    pub bounding: Vec<String>,
    pub effective: Vec<String>,
    pub permitted: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rlimit {
    pub r#type: String,
    pub hard: u64,
    pub soft: u64,
}

#[derive(Debug, Serialize)]
pub struct Root {
    pub path: String,
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mount {
    pub destination: String,
    pub r#type: String,
    pub source: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Linux {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub uid_mappings: Vec<IdMapping>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gid_mappings: Vec<IdMapping>,
    pub namespaces: Vec<Namespace>,
    pub masked_paths: Vec<String>,
    pub readonly_paths: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct IdMapping {
    #[serde(rename = "containerID")]
    pub container_id: u32,
    #[serde(rename = "hostID")]
    pub host_id: u32,
    pub size: u32,
}

#[derive(Debug, Serialize)]
pub struct Namespace {
    pub r#type: String,
}

/// A `SOURCE:DESTINATION[:OPTIONS]` bind mount request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindMount {
    pub source: String,
    pub destination: String,
    pub options: Vec<String>,
}

impl BindMount {
    pub fn parse(value: &str) -> Result<Self> {
        let mut fields = value.splitn(3, ':');
        let source = fields.next().unwrap_or_default();
        let destination = fields.next().unwrap_or_default();
        let options = fields.next().unwrap_or("rw");

        if source.is_empty() || destination.is_empty() {
            return Err(Error::InvalidMount(value.to_string()));
        }
        if !destination.starts_with('/') {
            return Err(Error::InvalidMount(value.to_string()));
        }
        let options: Vec<String> = options
            .split(',')
            .filter(|option| !option.is_empty())
            .map(str::to_string)
            .collect();
        if options
            .iter()
            .any(|option| option.contains(char::is_whitespace))
        {
            return Err(Error::InvalidMount(value.to_string()));
        }

        Ok(BindMount {
            source: source.to_string(),
            destination: destination.to_string(),
            options: if options.is_empty() {
                vec!["rw".to_string()]
            } else {
                options
            },
        })
    }

    fn to_mount(&self) -> Mount {
        let mut options = vec!["rbind".to_string(), "rprivate".to_string()];
        options.extend(self.options.iter().cloned());
        Mount {
            destination: self.destination.clone(),
            r#type: "none".to_string(),
            source: self.source.clone(),
            options,
        }
    }
}

pub struct SpecOptions<'a> {
    pub rootless: bool,
    pub terminal: bool,
    pub readonly_rootfs: bool,
    pub hostname: &'a str,
    pub uid: u32,
    pub gid: u32,
    pub image: &'a ImageConfig,
    pub extra_env: &'a [String],
    pub bind_mounts: &'a [BindMount],
    /// User supplied command; when non-empty it replaces the image `Cmd`.
    pub command: &'a [String],
    pub workdir: Option<&'a str>,
}

impl Spec {
    pub fn build(options: &SpecOptions<'_>) -> Result<Self> {
        let args = resolve_args(options.image, options.command)?;
        let env = resolve_env(options.image, options.extra_env, options.terminal)?;
        let cwd = options
            .workdir
            .map(str::to_string)
            .or_else(|| {
                options
                    .image
                    .working_dir
                    .as_deref()
                    .filter(|dir| !dir.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "/".to_string());

        let capabilities: Vec<String> =
            DEFAULT_CAPABILITIES.iter().map(|c| c.to_string()).collect();

        let mut mounts = default_mounts(options.rootless);
        mounts.extend(options.bind_mounts.iter().map(BindMount::to_mount));

        let mut namespaces: Vec<Namespace> = ["pid", "ipc", "uts", "mount", "cgroup"]
            .iter()
            .map(|kind| Namespace {
                r#type: kind.to_string(),
            })
            .collect();
        if options.rootless {
            namespaces.push(Namespace {
                r#type: "user".to_string(),
            });
        }

        let (uid_mappings, gid_mappings) = if options.rootless {
            (
                vec![IdMapping {
                    container_id: 0,
                    host_id: options.uid,
                    size: 1,
                }],
                vec![IdMapping {
                    container_id: 0,
                    host_id: options.gid,
                    size: 1,
                }],
            )
        } else {
            (Vec::new(), Vec::new())
        };

        Ok(Spec {
            oci_version: OCI_VERSION.to_string(),
            process: Process {
                terminal: options.terminal,
                user: User { uid: 0, gid: 0 },
                args,
                env,
                cwd,
                capabilities: Capabilities {
                    bounding: capabilities.clone(),
                    effective: capabilities.clone(),
                    permitted: capabilities,
                },
                rlimits: vec![Rlimit {
                    r#type: "RLIMIT_NOFILE".to_string(),
                    hard: 1024,
                    soft: 1024,
                }],
                no_new_privileges: true,
            },
            root: Root {
                path: "rootfs".to_string(),
                readonly: options.readonly_rootfs,
            },
            hostname: options.hostname.to_string(),
            mounts,
            linux: Linux {
                uid_mappings,
                gid_mappings,
                namespaces,
                masked_paths: MASKED_PATHS.iter().map(|p| p.to_string()).collect(),
                readonly_paths: READONLY_PATHS.iter().map(|p| p.to_string()).collect(),
            },
        })
    }
}

/// As with Docker/OCI runtimes, user supplied arguments replace the image `Cmd`.
pub fn resolve_args(image: &ImageConfig, command: &[String]) -> Result<Vec<String>> {
    let mut args = image.entrypoint.clone().unwrap_or_default();
    if command.is_empty() {
        args.extend(image.cmd.clone().unwrap_or_default());
    } else {
        args.extend_from_slice(command);
    }
    if args.is_empty() {
        return Err(Error::NoCommand);
    }
    Ok(args)
}

/// Later definitions of the same variable win, so `--env` overrides the image.
pub fn resolve_env(image: &ImageConfig, extra: &[String], terminal: bool) -> Result<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let mut values: Vec<String> = Vec::new();

    for entry in image.env.iter().flatten() {
        push_env(&mut names, &mut values, entry)?;
    }
    for entry in extra {
        push_env(&mut names, &mut values, entry)?;
    }
    if !names.iter().any(|name| name == "PATH") {
        push_env(&mut names, &mut values, &format!("PATH={DEFAULT_PATH}"))?;
    }
    if terminal && !names.iter().any(|name| name == "TERM") {
        let term = std::env::var("TERM").unwrap_or_else(|_| "xterm".to_string());
        push_env(&mut names, &mut values, &format!("TERM={term}"))?;
    }
    Ok(values)
}

fn push_env(names: &mut Vec<String>, values: &mut Vec<String>, entry: &str) -> Result<()> {
    let (name, _) = entry
        .split_once('=')
        .ok_or_else(|| Error::InvalidEnv(entry.to_string()))?;
    if name.is_empty() || name.contains('\0') {
        return Err(Error::InvalidEnv(entry.to_string()));
    }
    match names.iter().position(|existing| existing == name) {
        Some(index) => values[index] = entry.to_string(),
        None => {
            names.push(name.to_string());
            values.push(entry.to_string());
        }
    }
    Ok(())
}

fn default_mounts(rootless: bool) -> Vec<Mount> {
    let mount = |destination: &str, kind: &str, source: &str, options: &[&str]| Mount {
        destination: destination.to_string(),
        r#type: kind.to_string(),
        source: source.to_string(),
        options: options.iter().map(|o| o.to_string()).collect(),
    };

    let mut mounts = vec![
        mount("/proc", "proc", "proc", &[]),
        mount(
            "/dev",
            "tmpfs",
            "tmpfs",
            &["nosuid", "strictatime", "mode=755", "size=65536k"],
        ),
        mount(
            "/dev/pts",
            "devpts",
            "devpts",
            &[
                "nosuid",
                "noexec",
                "newinstance",
                "ptmxmode=0666",
                "mode=0620",
            ],
        ),
        mount(
            "/dev/shm",
            "tmpfs",
            "shm",
            &["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"],
        ),
        mount(
            "/dev/mqueue",
            "mqueue",
            "mqueue",
            &["nosuid", "noexec", "nodev"],
        ),
    ];

    if rootless {
        // A user namespace cannot mount a fresh sysfs, so bind the host's.
        mounts.push(mount(
            "/sys",
            "none",
            "/sys",
            &["rbind", "nosuid", "noexec", "nodev", "ro"],
        ));
    } else {
        mounts.push(mount(
            "/sys",
            "sysfs",
            "sysfs",
            &["nosuid", "noexec", "nodev", "ro"],
        ));
    }
    mounts.push(mount(
        "/sys/fs/cgroup",
        "cgroup",
        "cgroup",
        &["nosuid", "noexec", "nodev", "relatime", "ro"],
    ));
    mounts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(entrypoint: Option<&[&str]>, cmd: Option<&[&str]>) -> ImageConfig {
        ImageConfig {
            entrypoint: entrypoint.map(|e| e.iter().map(|s| s.to_string()).collect()),
            cmd: cmd.map(|c| c.iter().map(|s| s.to_string()).collect()),
            ..ImageConfig::default()
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn image_cmd_is_used_when_no_command_given() {
        let args = resolve_args(&image(None, Some(&["/bin/sh"])), &[]).expect("args");
        assert_eq!(args, strings(&["/bin/sh"]));
    }

    #[test]
    fn user_command_replaces_image_cmd() {
        let args =
            resolve_args(&image(None, Some(&["/bin/sh"])), &strings(&["/bin/true"])).expect("args");
        assert_eq!(args, strings(&["/bin/true"]));
    }

    #[test]
    fn entrypoint_is_prepended_to_the_user_command() {
        let args = resolve_args(
            &image(Some(&["/entry", "--flag"]), Some(&["default"])),
            &strings(&["override"]),
        )
        .expect("args");
        assert_eq!(args, strings(&["/entry", "--flag", "override"]));
    }

    #[test]
    fn entrypoint_is_combined_with_image_cmd() {
        let args = resolve_args(&image(Some(&["/entry"]), Some(&["default"])), &[]).expect("args");
        assert_eq!(args, strings(&["/entry", "default"]));
    }

    #[test]
    fn images_without_a_command_are_rejected() {
        assert!(matches!(
            resolve_args(&image(None, None), &[]),
            Err(Error::NoCommand)
        ));
    }

    #[test]
    fn image_env_is_preserved_in_order() {
        let config = ImageConfig {
            env: Some(strings(&["PATH=/bin", "A=1", "B=2"])),
            ..ImageConfig::default()
        };
        let env = resolve_env(&config, &[], false).expect("env");
        assert_eq!(env, strings(&["PATH=/bin", "A=1", "B=2"]));
    }

    #[test]
    fn extra_env_overrides_image_env_in_place() {
        let config = ImageConfig {
            env: Some(strings(&["PATH=/bin", "A=1"])),
            ..ImageConfig::default()
        };
        let env = resolve_env(&config, &strings(&["A=2", "C=3"]), false).expect("env");
        assert_eq!(env, strings(&["PATH=/bin", "A=2", "C=3"]));
    }

    #[test]
    fn a_default_path_is_added_when_the_image_has_none() {
        let env = resolve_env(&ImageConfig::default(), &[], false).expect("env");
        assert_eq!(env, strings(&[&format!("PATH={DEFAULT_PATH}")]));
    }

    #[test]
    fn term_is_only_added_for_ttys() {
        let with_tty = resolve_env(&ImageConfig::default(), &[], true).expect("env");
        assert!(with_tty.iter().any(|entry| entry.starts_with("TERM=")));
        let without_tty = resolve_env(&ImageConfig::default(), &[], false).expect("env");
        assert!(!without_tty.iter().any(|entry| entry.starts_with("TERM=")));
    }

    #[test]
    fn explicit_term_is_not_overridden() {
        let env =
            resolve_env(&ImageConfig::default(), &strings(&["TERM=dumb"]), true).expect("env");
        assert!(env.contains(&"TERM=dumb".to_string()));
        assert_eq!(env.iter().filter(|e| e.starts_with("TERM=")).count(), 1);
    }

    #[test]
    fn malformed_env_is_rejected() {
        assert!(matches!(
            resolve_env(&ImageConfig::default(), &strings(&["NOEQUALS"]), false),
            Err(Error::InvalidEnv(_))
        ));
        assert!(matches!(
            resolve_env(&ImageConfig::default(), &strings(&["=value"]), false),
            Err(Error::InvalidEnv(_))
        ));
    }

    #[test]
    fn mounts_default_to_read_write() {
        let mount = BindMount::parse("/src:/dst").expect("mount");
        assert_eq!(mount.source, "/src");
        assert_eq!(mount.destination, "/dst");
        assert_eq!(mount.options, strings(&["rw"]));
    }

    #[test]
    fn mount_options_are_preserved() {
        let mount = BindMount::parse("/src:/dst:ro,noexec").expect("mount");
        assert_eq!(mount.options, strings(&["ro", "noexec"]));
        assert_eq!(
            mount.to_mount().options,
            strings(&["rbind", "rprivate", "ro", "noexec"])
        );
    }

    #[test]
    fn malformed_mounts_are_rejected() {
        for value in [
            "",
            "/src",
            "/src:",
            ":/dst",
            "/src:relative",
            "/src:/dst:a b",
        ] {
            assert!(
                matches!(BindMount::parse(value), Err(Error::InvalidMount(_))),
                "expected {value:?} to be rejected"
            );
        }
    }

    #[test]
    fn rootless_specs_map_the_calling_user_to_root() {
        let config = image(None, Some(&["/bin/sh"]));
        let spec = Spec::build(&SpecOptions {
            rootless: true,
            terminal: false,
            readonly_rootfs: false,
            hostname: "test",
            uid: 1000,
            gid: 1001,
            image: &config,
            extra_env: &[],
            bind_mounts: &[],
            command: &[],
            workdir: None,
        })
        .expect("spec");

        assert_eq!(spec.linux.uid_mappings[0].host_id, 1000);
        assert_eq!(spec.linux.gid_mappings[0].host_id, 1001);
        assert!(spec.linux.namespaces.iter().any(|ns| ns.r#type == "user"));
        assert!(
            spec.linux
                .namespaces
                .iter()
                .all(|ns| ns.r#type != "network")
        );
        assert!(
            spec.mounts
                .iter()
                .any(|m| m.destination == "/sys" && m.options.contains(&"rbind".to_string()))
        );
    }

    #[test]
    fn privileged_specs_omit_the_user_namespace() {
        let config = image(None, Some(&["/bin/sh"]));
        let spec = Spec::build(&SpecOptions {
            rootless: false,
            terminal: false,
            readonly_rootfs: false,
            hostname: "test",
            uid: 0,
            gid: 0,
            image: &config,
            extra_env: &[],
            bind_mounts: &[],
            command: &[],
            workdir: None,
        })
        .expect("spec");

        assert!(spec.linux.uid_mappings.is_empty());
        assert!(spec.linux.namespaces.iter().all(|ns| ns.r#type != "user"));
        assert!(
            spec.mounts
                .iter()
                .any(|m| m.destination == "/sys" && m.r#type == "sysfs")
        );
    }

    #[test]
    fn the_container_never_gets_its_own_network_namespace() {
        for rootless in [true, false] {
            let config = image(None, Some(&["/bin/sh"]));
            let spec = Spec::build(&SpecOptions {
                rootless,
                terminal: false,
                readonly_rootfs: false,
                hostname: "test",
                uid: 0,
                gid: 0,
                image: &config,
                extra_env: &[],
                bind_mounts: &[],
                command: &[],
                workdir: None,
            })
            .expect("spec");
            assert!(
                spec.linux
                    .namespaces
                    .iter()
                    .all(|ns| ns.r#type != "network")
            );
        }
    }

    #[test]
    fn workdir_overrides_the_image_working_directory() {
        let config = ImageConfig {
            cmd: Some(strings(&["/bin/sh"])),
            working_dir: Some("/image".to_string()),
            ..ImageConfig::default()
        };
        let with_override = Spec::build(&SpecOptions {
            rootless: true,
            terminal: false,
            readonly_rootfs: false,
            hostname: "test",
            uid: 0,
            gid: 0,
            image: &config,
            extra_env: &[],
            bind_mounts: &[],
            command: &[],
            workdir: Some("/flag"),
        })
        .expect("spec");
        assert_eq!(with_override.process.cwd, "/flag");

        let without_override = Spec::build(&SpecOptions {
            rootless: true,
            terminal: false,
            readonly_rootfs: false,
            hostname: "test",
            uid: 0,
            gid: 0,
            image: &config,
            extra_env: &[],
            bind_mounts: &[],
            command: &[],
            workdir: None,
        })
        .expect("spec");
        assert_eq!(without_override.process.cwd, "/image");
    }

    #[test]
    fn bind_mounts_are_appended_after_the_defaults() {
        let config = image(None, Some(&["/bin/sh"]));
        let bind = BindMount::parse("/host:/guest:ro").expect("mount");
        let spec = Spec::build(&SpecOptions {
            rootless: true,
            terminal: false,
            readonly_rootfs: false,
            hostname: "test",
            uid: 0,
            gid: 0,
            image: &config,
            extra_env: &[],
            bind_mounts: std::slice::from_ref(&bind),
            command: &[],
            workdir: None,
        })
        .expect("spec");
        let last = spec.mounts.last().expect("mount");
        assert_eq!(last.destination, "/guest");
        assert_eq!(last.source, "/host");
    }

    #[test]
    fn the_spec_serialises_with_runtime_spec_field_names() {
        let config = image(None, Some(&["/bin/sh"]));
        let spec = Spec::build(&SpecOptions {
            rootless: true,
            terminal: false,
            readonly_rootfs: false,
            hostname: "test",
            uid: 0,
            gid: 0,
            image: &config,
            extra_env: &[],
            bind_mounts: &[],
            command: &[],
            workdir: None,
        })
        .expect("spec");
        let json = serde_json::to_value(&spec).expect("json");

        assert_eq!(json["ociVersion"], OCI_VERSION);
        assert_eq!(json["root"]["path"], "rootfs");
        assert_eq!(json["root"]["readonly"], false);
        assert_eq!(json["process"]["noNewPrivileges"], true);
        assert_eq!(json["linux"]["uidMappings"][0]["containerID"], 0);
        assert!(json["linux"]["maskedPaths"].is_array());
        assert!(json["linux"]["readonlyPaths"].is_array());
        assert_eq!(json["process"]["rlimits"][0]["type"], "RLIMIT_NOFILE");
    }
}
