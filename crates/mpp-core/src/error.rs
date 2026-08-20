//! Crate-wide error type: every fallible operation reports the file it was
//! working on plus either the underlying I/O error or a human-readable reason.

use std::path::PathBuf;

/// Crate-wide result alias over [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// The error type for all mpp-core operations.
///
/// Deliberately small: an I/O failure on a file, or a malformed/unsupported
/// file. New variants are added only when a caller needs to distinguish them.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An operating-system I/O failure on `path`.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The file or directory the operation was working on.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// `path` exists but its contents are unsupported, malformed, or
    /// inconsistent with what the operation requires; `reason` says how.
    #[error("unsupported or malformed file {path}: {reason}")]
    Format {
        /// The offending file.
        path: PathBuf,
        /// Human-readable explanation of what is wrong.
        reason: String,
    },
}

impl Error {
    /// Wrap an [`std::io::Error`] with the path it occurred on.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    /// Build an [`Error::Format`] from a path and a reason.
    pub fn format(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Error::Format {
            path: path.into(),
            reason: reason.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_carry_path_and_reason() {
        let e = Error::format("/tmp/x.xisf", "bad signature");
        assert_eq!(
            e.to_string(),
            "unsupported or malformed file /tmp/x.xisf: bad signature"
        );
        let e = Error::io(
            "/tmp/y.tif",
            std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
        );
        assert!(e.to_string().contains("/tmp/y.tif"));
    }
}
