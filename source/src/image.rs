//! Reading an OCI image layout (`index.json` + `blobs/<algorithm>/<hex>`) from disk.

use std::fs::File;
use std::io::Read;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{Error, IoContext, JsonContext, Result};

pub const MEDIA_TYPE_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MEDIA_TYPE_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const MEDIA_TYPE_DOCKER_LIST: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";
pub const MEDIA_TYPE_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// Guards against a hostile index pointing at an unbounded blob.
const MAX_METADATA_BLOB_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    #[serde(default)]
    pub media_type: String,
    pub digest: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub platform: Option<Platform>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Platform {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub variant: Option<String>,
}

impl Platform {
    /// The platform the current process is running on, in OCI (Go) spelling.
    pub fn host() -> Self {
        Platform {
            architecture: host_architecture().to_string(),
            os: host_os().to_string(),
            variant: None,
        }
    }

    /// Descriptor platforms are matched loosely: a missing field means "any".
    pub fn matches(&self, candidate: &Platform) -> bool {
        let os_ok = candidate.os.is_empty() || candidate.os == self.os;
        let arch_ok = candidate.architecture.is_empty() || candidate.architecture == self.architecture;
        let ours = default_variant_removed(&self.architecture, self.variant.as_deref());
        let theirs = default_variant_removed(&candidate.architecture, candidate.variant.as_deref());
        let variant_ok = match theirs {
            None => true,
            Some(theirs) => ours.as_deref() == Some(theirs.as_str()),
        };
        os_ok && arch_ok && variant_ok
    }
}

/// `arm64/v8` and `amd64/v1` name the same platforms as bare `arm64` and `amd64`,
/// so registries (and `docker buildx`) spell them either way.
fn default_variant_removed(architecture: &str, variant: Option<&str>) -> Option<String> {
    let variant = variant
        .map(str::to_ascii_lowercase)
        .filter(|variant| !variant.is_empty())?;
    match (architecture.to_ascii_lowercase().as_str(), variant.as_str()) {
        ("arm64" | "aarch64", "v8" | "8") => None,
        ("amd64" | "x86_64", "v1") => None,
        _ => Some(variant),
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)?;
        match &self.variant {
            Some(variant) if !variant.is_empty() => write!(f, "/{variant}"),
            _ => Ok(()),
        }
    }
}

fn host_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "arm" => "arm",
        "powerpc64" => "ppc64le",
        other => other,
    }
}

fn host_os() -> &'static str {
    std::env::consts::OS
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Index {
    #[serde(default)]
    pub manifests: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub config: Descriptor,
    #[serde(default)]
    pub layers: Vec<Descriptor>,
}

/// The `config` object inside an image configuration blob. Field names are
/// capitalised in the OCI spec for historical (Docker) reasons.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageConfig {
    #[serde(default, rename = "Env")]
    pub env: Option<Vec<String>>,
    #[serde(default, rename = "Entrypoint")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default, rename = "Cmd")]
    pub cmd: Option<Vec<String>>,
    #[serde(default, rename = "WorkingDir")]
    pub working_dir: Option<String>,
    #[serde(default, rename = "User")]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageConfigBlob {
    #[serde(default)]
    pub config: ImageConfig,
}

/// A digest split into its algorithm and hex encoded value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDigest {
    pub algorithm: String,
    pub hex: String,
}

/// Rejects anything that could escape `blobs/` when used as a path component.
pub fn parse_digest(digest: &str) -> Result<ParsedDigest> {
    let (algorithm, hex) = digest
        .split_once(':')
        .ok_or_else(|| Error::MalformedDigest(digest.to_string()))?;

    let algorithm_ok = !algorithm.is_empty()
        && algorithm
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let hex_ok = !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit());
    if !algorithm_ok || !hex_ok {
        return Err(Error::MalformedDigest(digest.to_string()));
    }
    if algorithm != "sha256" {
        return Err(Error::UnsupportedMediaType(format!(
            "digest algorithm {algorithm}"
        )));
    }
    Ok(ParsedDigest {
        algorithm: algorithm.to_string(),
        hex: hex.to_string(),
    })
}

pub struct Layout {
    root: Utf8PathBuf,
}

impl Layout {
    pub fn open(root: &Utf8Path) -> Result<Self> {
        if !root.join("index.json").is_file() {
            return Err(Error::Layout(format!("{root}/index.json does not exist")));
        }
        if !root.join("oci-layout").is_file() {
            return Err(Error::Layout(format!("{root}/oci-layout does not exist")));
        }
        Ok(Layout {
            root: root.to_owned(),
        })
    }

    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    pub fn blob_path(&self, digest: &str) -> Result<Utf8PathBuf> {
        let parsed = parse_digest(digest)?;
        Ok(self
            .root
            .join("blobs")
            .join(parsed.algorithm)
            .join(parsed.hex))
    }

    /// Opens a blob for streaming. The caller is responsible for verifying the digest.
    pub fn open_blob(&self, descriptor: &Descriptor) -> Result<File> {
        let path = self.blob_path(&descriptor.digest)?;
        File::open(&path).io_context(|| format!("opening blob {path}"))
    }

    /// Reads a small (metadata) blob and verifies its size and digest.
    pub fn read_metadata_blob(&self, descriptor: &Descriptor) -> Result<Vec<u8>> {
        let path = self.blob_path(&descriptor.digest)?;
        let mut file = File::open(&path).io_context(|| format!("opening blob {path}"))?;

        let limit = if descriptor.size == 0 {
            MAX_METADATA_BLOB_BYTES
        } else {
            descriptor.size.min(MAX_METADATA_BLOB_BYTES)
        };
        let mut bytes = Vec::new();
        file.by_ref()
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .io_context(|| format!("reading blob {path}"))?;
        if bytes.len() as u64 > limit {
            return Err(Error::Layout(format!(
                "blob {} is larger than its descriptor allows",
                descriptor.digest
            )));
        }
        verify(descriptor, &bytes)?;
        Ok(bytes)
    }

    pub fn read_index(&self) -> Result<Index> {
        let path = self.root.join("index.json");
        let bytes = std::fs::read(&path).io_context(|| format!("reading {path}"))?;
        serde_json::from_slice(&bytes).json_context(|| format!("parsing {path}"))
    }

    /// Walks `index.json`, descending through nested indexes, to the manifest for `platform`.
    pub fn resolve_manifest(&self, platform: &Platform) -> Result<Manifest> {
        let descriptor = self.resolve_manifest_descriptor(platform)?;
        let bytes = self.read_metadata_blob(&descriptor)?;
        serde_json::from_slice(&bytes)
            .json_context(|| format!("parsing manifest {}", descriptor.digest))
    }

    /// Every manifest reachable from `index.json`, regardless of platform.
    pub fn all_manifests(&self) -> Result<Vec<Manifest>> {
        let mut queue = vec![self.read_index()?.manifests];
        let mut manifests = Vec::new();
        let mut depth = 0usize;

        while let Some(descriptors) = queue.pop() {
            depth += 1;
            if depth > 8 {
                return Err(Error::Layout("image index nested too deeply".to_string()));
            }
            for descriptor in descriptors {
                match descriptor.media_type.as_str() {
                    MEDIA_TYPE_OCI_INDEX | MEDIA_TYPE_DOCKER_LIST => {
                        let bytes = self.read_metadata_blob(&descriptor)?;
                        let index: Index = serde_json::from_slice(&bytes)
                            .json_context(|| format!("parsing index {}", descriptor.digest))?;
                        queue.push(index.manifests);
                    }
                    MEDIA_TYPE_OCI_MANIFEST | MEDIA_TYPE_DOCKER_MANIFEST | "" => {
                        let bytes = self.read_metadata_blob(&descriptor)?;
                        manifests.push(
                            serde_json::from_slice(&bytes).json_context(|| {
                                format!("parsing manifest {}", descriptor.digest)
                            })?,
                        );
                    }
                    other => {
                        crate::log::log!("ignoring descriptor with media type {other}");
                    }
                }
            }
        }
        Ok(manifests)
    }

    fn resolve_manifest_descriptor(&self, platform: &Platform) -> Result<Descriptor> {
        let mut queue = vec![self.read_index()?.manifests];
        let mut available = Vec::new();
        let mut depth = 0usize;

        while let Some(descriptors) = queue.pop() {
            depth += 1;
            if depth > 8 {
                return Err(Error::Layout("image index nested too deeply".to_string()));
            }
            let mut nested = Vec::new();
            for descriptor in descriptors {
                match descriptor.media_type.as_str() {
                    MEDIA_TYPE_OCI_INDEX | MEDIA_TYPE_DOCKER_LIST => {
                        if descriptor
                            .platform
                            .as_ref()
                            .is_none_or(|p| platform.matches(p))
                        {
                            nested.push(descriptor);
                        }
                    }
                    MEDIA_TYPE_OCI_MANIFEST | MEDIA_TYPE_DOCKER_MANIFEST | "" => {
                        match &descriptor.platform {
                            None => return Ok(descriptor),
                            Some(candidate) if platform.matches(candidate) => {
                                return Ok(descriptor);
                            }
                            Some(candidate) => available.push(candidate.to_string()),
                        }
                    }
                    other => {
                        crate::log::log!("ignoring descriptor with media type {other}");
                    }
                }
            }
            for descriptor in nested {
                let bytes = self.read_metadata_blob(&descriptor)?;
                let index: Index = serde_json::from_slice(&bytes)
                    .json_context(|| format!("parsing index {}", descriptor.digest))?;
                queue.push(index.manifests);
            }
        }

        available.sort();
        available.dedup();
        Err(Error::NoMatchingPlatform {
            os: platform.os.clone(),
            arch: platform.architecture.clone(),
            available,
        })
    }

    pub fn read_image_config(&self, manifest: &Manifest) -> Result<ImageConfig> {
        let bytes = self.read_metadata_blob(&manifest.config)?;
        let blob: ImageConfigBlob = serde_json::from_slice(&bytes)
            .json_context(|| format!("parsing image config {}", manifest.config.digest))?;
        Ok(blob.config)
    }
}

/// Checks in-memory bytes against the size and digest recorded in a descriptor.
pub fn verify(descriptor: &Descriptor, bytes: &[u8]) -> Result<()> {
    if descriptor.size != 0 && descriptor.size != bytes.len() as u64 {
        return Err(Error::SizeMismatch {
            digest: descriptor.digest.clone(),
            expected: descriptor.size,
            actual: bytes.len() as u64,
        });
    }
    let parsed = parse_digest(&descriptor.digest)?;
    let actual = hex_encode(&Sha256::digest(bytes));
    if actual != parsed.hex {
        return Err(Error::DigestMismatch {
            digest: descriptor.digest.clone(),
            actual,
        });
    }
    Ok(())
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0xf) as u32, 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(media_type: &str, os: &str, arch: &str) -> Descriptor {
        Descriptor {
            media_type: media_type.to_string(),
            digest: "sha256:00".to_string(),
            size: 0,
            platform: Some(Platform {
                architecture: arch.to_string(),
                os: os.to_string(),
                variant: None,
            }),
        }
    }

    #[test]
    fn digests_are_parsed() {
        let parsed = parse_digest("sha256:abc123").expect("valid digest");
        assert_eq!(parsed.algorithm, "sha256");
        assert_eq!(parsed.hex, "abc123");
    }

    #[test]
    fn traversal_digests_are_rejected() {
        for digest in [
            "sha256:../../etc/passwd",
            "sha256:",
            ":abc",
            "abc",
            "sha256:zz",
            "../sha256:ab",
        ] {
            assert!(
                matches!(parse_digest(digest), Err(Error::MalformedDigest(_))),
                "expected {digest} to be rejected"
            );
        }
    }

    #[test]
    fn unsupported_algorithms_are_rejected() {
        assert!(matches!(
            parse_digest("sha512:ab"),
            Err(Error::UnsupportedMediaType(_))
        ));
    }

    #[test]
    fn platform_matching_treats_empty_fields_as_wildcards() {
        let host = Platform {
            architecture: "amd64".into(),
            os: "linux".into(),
            variant: None,
        };
        assert!(host.matches(&Platform {
            architecture: "amd64".into(),
            os: "linux".into(),
            variant: None
        }));
        assert!(host.matches(&Platform::default()));
        assert!(!host.matches(&Platform {
            architecture: "arm64".into(),
            os: "linux".into(),
            variant: None
        }));
        assert!(!host.matches(&Platform {
            architecture: "amd64".into(),
            os: "windows".into(),
            variant: None
        }));
    }

    #[test]
    fn platform_variant_must_match_when_requested() {
        let host = Platform {
            architecture: "arm".into(),
            os: "linux".into(),
            variant: Some("v7".into()),
        };
        assert!(host.matches(&Platform {
            architecture: "arm".into(),
            os: "linux".into(),
            variant: Some("v7".into())
        }));
        assert!(!host.matches(&Platform {
            architecture: "arm".into(),
            os: "linux".into(),
            variant: Some("v6".into())
        }));
    }

    #[test]
    fn default_variants_match_the_bare_architecture() {
        let host = Platform {
            architecture: "arm64".into(),
            os: "linux".into(),
            variant: None,
        };
        assert!(host.matches(&Platform {
            architecture: "arm64".into(),
            os: "linux".into(),
            variant: Some("v8".into())
        }));
        assert!(!host.matches(&Platform {
            architecture: "arm64".into(),
            os: "linux".into(),
            variant: Some("v9".into())
        }));

        let requested = Platform {
            architecture: "arm64".into(),
            os: "linux".into(),
            variant: Some("v8".into()),
        };
        assert!(requested.matches(&Platform {
            architecture: "arm64".into(),
            os: "linux".into(),
            variant: None
        }));

        let amd64 = Platform {
            architecture: "amd64".into(),
            os: "linux".into(),
            variant: None,
        };
        assert!(amd64.matches(&Platform {
            architecture: "amd64".into(),
            os: "linux".into(),
            variant: Some("v1".into())
        }));
        assert!(!amd64.matches(&Platform {
            architecture: "amd64".into(),
            os: "linux".into(),
            variant: Some("v3".into())
        }));
    }

    #[test]
    fn platform_is_displayed_with_variant() {
        assert_eq!(
            Platform {
                architecture: "arm".into(),
                os: "linux".into(),
                variant: Some("v7".into())
            }
            .to_string(),
            "linux/arm/v7"
        );
        assert_eq!(
            Platform {
                architecture: "amd64".into(),
                os: "linux".into(),
                variant: None
            }
            .to_string(),
            "linux/amd64"
        );
    }

    #[test]
    fn host_platform_uses_oci_architecture_names() {
        let host = Platform::host();
        assert!(!host.architecture.is_empty());
        assert_ne!(host.architecture, "x86_64");
        assert_ne!(host.architecture, "aarch64");
    }

    #[test]
    fn hex_encoding_is_lowercase_and_padded() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn descriptors_deserialize_with_defaults() {
        let descriptor: Descriptor =
            serde_json::from_str(r#"{"digest":"sha256:ab"}"#).expect("minimal descriptor");
        assert_eq!(descriptor.media_type, "");
        assert_eq!(descriptor.size, 0);
        assert!(descriptor.platform.is_none());
    }

    #[test]
    fn image_config_uses_capitalised_field_names() {
        let blob: ImageConfigBlob = serde_json::from_str(
            r#"{"architecture":"amd64","config":{"Env":["A=1"],"Cmd":["/bin/sh"],"WorkingDir":"/w"}}"#,
        )
        .expect("image config");
        assert_eq!(blob.config.env.as_deref(), Some(&["A=1".to_string()][..]));
        assert_eq!(blob.config.cmd.as_deref(), Some(&["/bin/sh".to_string()][..]));
        assert_eq!(blob.config.working_dir.as_deref(), Some("/w"));
        assert!(blob.config.entrypoint.is_none());
    }

    #[test]
    fn descriptor_media_types_are_recognised() {
        let index = descriptor(MEDIA_TYPE_OCI_INDEX, "linux", "amd64");
        assert_eq!(index.media_type, MEDIA_TYPE_OCI_INDEX);
    }

    #[test]
    fn verify_detects_size_mismatch() {
        let descriptor = Descriptor {
            media_type: String::new(),
            digest: "sha256:00".into(),
            size: 99,
            platform: None,
        };
        assert!(matches!(
            verify(&descriptor, b"hello"),
            Err(Error::SizeMismatch { .. })
        ));
    }

    #[test]
    fn verify_detects_digest_mismatch() {
        let descriptor = Descriptor {
            media_type: String::new(),
            digest: "sha256:00".into(),
            size: 5,
            platform: None,
        };
        assert!(matches!(
            verify(&descriptor, b"hello"),
            Err(Error::DigestMismatch { .. })
        ));
    }

    #[test]
    fn verify_accepts_matching_content() {
        let content = b"hello";
        let digest = format!("sha256:{}", hex_encode(&Sha256::digest(content)));
        let descriptor = Descriptor {
            media_type: String::new(),
            digest,
            size: content.len() as u64,
            platform: None,
        };
        assert!(verify(&descriptor, content).is_ok());
    }

    mod resolution {
        use super::*;

        struct Scratch(Utf8PathBuf);

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn scratch(name: &str) -> Scratch {
            let dir = Utf8PathBuf::from(std::env::temp_dir().to_str().expect("utf-8 tmpdir"))
                .join(format!("oci-runtime-resolve-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("blobs/sha256")).expect("create layout");
            std::fs::write(dir.join("oci-layout"), "{}").expect("oci-layout");
            Scratch(dir)
        }

        /// Stores `bytes` under its digest and returns a descriptor JSON fragment.
        fn install_blob(
            root: &Utf8Path,
            media_type: &str,
            bytes: &[u8],
            platform: Option<&str>,
        ) -> String {
            let hex = hex_encode(&Sha256::digest(bytes));
            std::fs::write(root.join("blobs/sha256").join(&hex), bytes).expect("write blob");
            let platform = match platform {
                None => String::new(),
                Some(platform) => {
                    let mut fields = platform.split('/');
                    let os = fields.next().unwrap_or_default();
                    let architecture = fields.next().unwrap_or_default();
                    let variant = match fields.next() {
                        Some(variant) => format!(r#","variant":"{variant}""#),
                        None => String::new(),
                    };
                    format!(
                        r#","platform":{{"os":"{os}","architecture":"{architecture}"{variant}}}"#
                    )
                }
            };
            format!(
                r#"{{"mediaType":"{media_type}","digest":"sha256:{hex}","size":{}{platform}}}"#,
                bytes.len()
            )
        }

        /// A manifest whose image config records the platform it was built for,
        /// so a test can tell which one was resolved.
        fn manifest_for(root: &Utf8Path, platform: &str) -> String {
            let config = install_blob(
                root,
                "application/vnd.oci.image.config.v1+json",
                format!(r#"{{"config":{{"Cmd":["{platform}"]}}}}"#).as_bytes(),
                None,
            );
            install_blob(
                root,
                MEDIA_TYPE_OCI_MANIFEST,
                format!(r#"{{"config":{config},"layers":[]}}"#).as_bytes(),
                Some(platform),
            )
        }

        /// The shape registries publish: the arm64 manifest carries the default
        /// `v8` variant, the amd64 one carries none, and 32-bit arm carries a
        /// variant that really does discriminate.
        fn multi_platform_layout(name: &str) -> Scratch {
            let root = scratch(name);
            let amd64 = manifest_for(&root.0, "linux/amd64");
            let arm64 = manifest_for(&root.0, "linux/arm64/v8");
            let arm = manifest_for(&root.0, "linux/arm/v7");
            std::fs::write(
                root.0.join("index.json"),
                format!(r#"{{"manifests":[{amd64},{arm64},{arm}]}}"#),
            )
            .expect("index.json");
            root
        }

        /// The `Cmd` of the resolved manifest, which names its platform.
        fn resolved_platform(layout: &Layout, platform: &str) -> String {
            let platform = crate::parse_platform(Some(platform)).expect("platform");
            let manifest = layout.resolve_manifest(&platform).expect("a manifest");
            let config = layout.read_image_config(&manifest).expect("image config");
            config.cmd.expect("Cmd").join(" ")
        }

        #[test]
        fn a_manifest_declaring_the_default_variant_is_resolved() {
            let root = multi_platform_layout("default-variant");
            let layout = Layout::open(&root.0).expect("layout");

            assert_eq!(resolved_platform(&layout, "linux/arm64"), "linux/arm64/v8");
            assert_eq!(
                resolved_platform(&layout, "linux/arm64/v8"),
                "linux/arm64/v8"
            );
            assert_eq!(resolved_platform(&layout, "linux/amd64"), "linux/amd64");
            assert_eq!(resolved_platform(&layout, "linux/amd64/v1"), "linux/amd64");
            assert_eq!(resolved_platform(&layout, "linux/arm/v7"), "linux/arm/v7");
        }

        #[test]
        fn a_nested_index_declaring_the_default_variant_is_descended_into() {
            let root = scratch("nested-variant");
            let arm64 = manifest_for(&root.0, "linux/arm64/v8");
            let nested = install_blob(
                &root.0,
                MEDIA_TYPE_OCI_INDEX,
                format!(r#"{{"manifests":[{arm64}]}}"#).as_bytes(),
                Some("linux/arm64/v8"),
            );
            std::fs::write(
                root.0.join("index.json"),
                format!(r#"{{"manifests":[{nested}]}}"#),
            )
            .expect("index.json");

            let layout = Layout::open(&root.0).expect("layout");
            assert_eq!(resolved_platform(&layout, "linux/arm64"), "linux/arm64/v8");
        }

        #[test]
        fn a_platform_the_image_lacks_reports_what_it_has() {
            let root = multi_platform_layout("no-match");
            let layout = Layout::open(&root.0).expect("layout");

            let platform = crate::parse_platform(Some("linux/riscv64")).expect("platform");
            let error = layout.resolve_manifest(&platform).expect_err("no manifest");
            assert_eq!(
                error.to_string(),
                "image has no manifest for linux/riscv64 \
                 (available: linux/amd64, linux/arm/v7, linux/arm64/v8)"
            );

            // A variant that is not the architecture default still discriminates.
            let platform = crate::parse_platform(Some("linux/arm/v6")).expect("platform");
            let error = layout.resolve_manifest(&platform).expect_err("no manifest");
            assert!(matches!(error, Error::NoMatchingPlatform { .. }));
        }
    }
}
