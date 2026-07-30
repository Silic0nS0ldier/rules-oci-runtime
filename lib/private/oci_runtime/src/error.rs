use std::fmt;
use std::io;

pub type Result<T> = std::result::Result<T, Error>;

/// Every failure mode this binary can produce.
#[derive(Debug)]
pub enum Error {
    Io {
        context: String,
        source: io::Error,
    },
    Json {
        context: String,
        source: serde_json::Error,
    },
    /// The OCI image layout on disk is missing or malformed.
    Layout(String),
    /// A blob digest was not of the form `<algorithm>:<hex>` or used an unsupported algorithm.
    MalformedDigest(String),
    /// A blob's content did not match the digest recorded in its descriptor.
    DigestMismatch {
        digest: String,
        actual: String,
    },
    /// A blob's size did not match the size recorded in its descriptor.
    SizeMismatch {
        digest: String,
        expected: u64,
        actual: u64,
    },
    /// The image index contains no manifest for the host platform.
    NoMatchingPlatform {
        os: String,
        arch: String,
        available: Vec<String>,
    },
    UnsupportedMediaType(String),
    /// A `--mount` value could not be parsed.
    InvalidMount(String),
    /// A `--env` value could not be parsed.
    InvalidEnv(String),
    /// A tar entry tried to escape the rootfs.
    UnsafeEntry {
        layer: String,
        path: String,
    },
    /// The container had no command: the image sets neither `Entrypoint` nor `Cmd`.
    NoCommand,
    /// The runtime binary could not be started.
    RuntimeSpawn {
        program: String,
        source: io::Error,
    },
}

impl Error {
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    pub fn json(context: impl Into<String>, source: serde_json::Error) -> Self {
        Error::Json {
            context: context.into(),
            source,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { context, source } => write!(f, "{context}: {source}"),
            Error::Json { context, source } => write!(f, "{context}: {source}"),
            Error::Layout(msg) => write!(f, "invalid OCI image layout: {msg}"),
            Error::MalformedDigest(digest) => write!(f, "malformed digest {digest:?}"),
            Error::DigestMismatch { digest, actual } => {
                write!(f, "blob {digest} has content digest sha256:{actual}")
            }
            Error::SizeMismatch {
                digest,
                expected,
                actual,
            } => write!(
                f,
                "blob {digest} is {actual} bytes, descriptor declares {expected}"
            ),
            Error::NoMatchingPlatform {
                os,
                arch,
                available,
            } => write!(
                f,
                "image has no manifest for {os}/{arch} (available: {})",
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            ),
            Error::UnsupportedMediaType(media_type) => {
                write!(f, "unsupported media type {media_type:?}")
            }
            Error::InvalidMount(spec) => write!(
                f,
                "invalid mount {spec:?}, expected SOURCE:DESTINATION[:OPTIONS]"
            ),
            Error::InvalidEnv(value) => write!(f, "invalid environment entry {value:?}, expected NAME=VALUE"),
            Error::UnsafeEntry { layer, path } => write!(
                f,
                "layer {layer} contains entry {path:?} that escapes the root filesystem"
            ),
            Error::NoCommand => write!(
                f,
                "no command to run: the image defines neither Entrypoint nor Cmd, and none was given"
            ),
            Error::RuntimeSpawn { program, source } => {
                write!(f, "failed to start container runtime {program:?}: {source}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            Error::Json { source, .. } => Some(source),
            Error::RuntimeSpawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Attaches human readable context to `io::Error`s without an extra dependency.
pub trait IoContext<T> {
    fn io_context(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for std::result::Result<T, io::Error> {
    fn io_context(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| Error::io(context(), source))
    }
}

/// Attaches human readable context to `serde_json::Error`s.
pub trait JsonContext<T> {
    fn json_context(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> JsonContext<T> for std::result::Result<T, serde_json::Error> {
    fn json_context(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| Error::json(context(), source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_context_is_prefixed() {
        let err: Result<()> = Err(io::Error::new(io::ErrorKind::NotFound, "nope"))
            .io_context(|| "reading index.json".to_string());
        assert_eq!(
            err.unwrap_err().to_string(),
            "reading index.json: nope".to_string()
        );
    }

    #[test]
    fn no_matching_platform_lists_alternatives() {
        let err = Error::NoMatchingPlatform {
            os: "linux".into(),
            arch: "riscv64".into(),
            available: vec!["linux/amd64".into(), "linux/arm64".into()],
        };
        assert_eq!(
            err.to_string(),
            "image has no manifest for linux/riscv64 (available: linux/amd64, linux/arm64)"
        );
    }

    #[test]
    fn no_matching_platform_without_alternatives() {
        let err = Error::NoMatchingPlatform {
            os: "linux".into(),
            arch: "amd64".into(),
            available: Vec::new(),
        };
        assert!(err.to_string().ends_with("(available: none)"));
    }
}
