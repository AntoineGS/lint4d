//! fmt4d — an opinionated Delphi / Object Pascal code formatter.
//!
//! # Quick start (library)
//!
//! ```rust,no_run
//! use fmt4d::{format_source, FmtConfig};
//! use pascal_core::FileInfo;
//! use std::collections::HashSet;
//! use std::path::PathBuf;
//!
//! let source = b"unit Foo; interface end.";
//! let info = FileInfo::new(PathBuf::from("Foo.pas"));
//! let config = FmtConfig::default();
//! let formatted = format_source(source, &info, &config, &HashSet::new())?;
//! # Ok::<_, fmt4d::FmtError>(())
//! ```
//!
//! For disk I/O where encoding must be preserved, use
//! [`format_bytes`] instead — it detects UTF-8/UTF-8-BOM/Latin-1
//! and round-trips losslessly.
//!
//! # Pipeline
//!
//! `bytes → tree-sitter parse → Doc builder → Doc IR → renderer → EOL`
//!
//! # Configuration
//!
//! Configuration is read from `.fmt4d.toml` via
//! [`FmtConfig::discover`]. A malformed config causes
//! [`FmtError::Config`] rather than silently falling back to
//! defaults.

// ── Internal modules (not part of the public API) ────────────────────
pub(crate) mod blank_lines;
pub(crate) mod comments;
pub(crate) mod directive_map;
pub(crate) mod doc;
pub(crate) mod doc_builder;
pub(crate) mod doc_builder_alignment;
pub(crate) mod doc_builder_blocks;
pub(crate) mod doc_builder_control_flow;
pub(crate) mod doc_builder_decls;
pub(crate) mod doc_builder_expressions;
pub(crate) mod doc_builder_sections;
pub(crate) mod doc_builder_types;
pub(crate) mod indent;
pub(crate) mod line_break;
pub(crate) mod renderer;
pub(crate) mod spacing;

// ── Public modules ───────────────────────────────────────────────────
pub mod config;
pub mod error;
pub mod formatter;
pub mod uses;

// ── Curated public API (re-exports) ──────────────────────────────────
pub use config::{EndOfLine, FmtConfig};
pub use error::{FmtError, Result};
pub use formatter::{format_bytes, format_source};
