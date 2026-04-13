//! Error types for fmt4d.

use std::path::PathBuf;

/// The crate's error type. Returned by every fallible library-level
/// entry point.
#[derive(Debug, thiserror::Error)]
pub enum FmtError {
    /// Tree-sitter parser reported structural errors. The formatter
    /// cannot safely rewrite the file because we don't trust the AST.
    #[error("parse error in {}: {message}", path.display())]
    Parse { path: PathBuf, message: String },

    /// Failed to load or deserialize `.fmt4d.toml`.
    ///
    /// `source` is boxed because `toml::de::Error` is ~120 bytes — without
    /// the box, the whole `FmtError` enum balloons and clippy's
    /// `result_large_err` lint fires on every fallible API.
    #[error("invalid fmt4d config in {}: {source}", path.display())]
    Config {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },

    /// I/O failure on a specific path.
    #[error("io error on {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Catch-all for pre-existing String-typed errors not yet mapped
    /// to a structured variant. Eliminate as conversions land.
    #[error("{0}")]
    Other(String),
}

impl From<String> for FmtError {
    fn from(s: String) -> Self {
        FmtError::Other(s)
    }
}

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, FmtError>;
