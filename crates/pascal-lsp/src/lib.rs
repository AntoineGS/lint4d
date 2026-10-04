//! Native Linux language-server building blocks for Delphi/Object Pascal.
//!
//! Filesystem-only project discovery and configuration evaluation live in
//! [`pascal_project`]. This crate retains workspace orchestration, overlays,
//! package/index lookup, and the stateful include/rename resolver.

pub(crate) mod configuration;
pub(crate) mod coverage;
pub(crate) mod file_watch;
pub(crate) mod include_expansion;
pub mod navigation;
mod project_cache;
pub mod server;
pub mod text;
pub(crate) mod warmer;
pub mod workspace;

pub use navigation::{NavigationIndex, NavigationTarget};
/// Compatibility module for the former `pascal_lsp::project` API.
pub use pascal_project as project;
pub use pascal_project::{ProjectContext, ProjectOptions, discover};
