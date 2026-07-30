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
        let variant_ok = match (&self.variant, &candidate.variant) {
            (_, None) => true,
            (Some(ours), Some(theirs)) => ours == theirs,
            (None, Some(theirs)) => theirs.is_empty(),
        };
        os_ok && arch_ok && variant_ok
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
}
