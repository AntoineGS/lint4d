//! Native Linux language-server building blocks for Delphi/Object Pascal.
//!
//! Filesystem-only project discovery and configuration evaluation live in
//! [`pascal_project`]. This crate retains workspace orchestration, overlays,
//! package/index lookup, and the stateful include/rename resolver.

pub mod analysis_metrics;
pub(crate) mod configuration;
pub(crate) mod coverage;
pub(crate) mod file_watch;
pub(crate) mod include_expansion;
pub mod navigation;
mod project_cache;
pub mod server;
pub mod text;
pub(crate) mod trace;
pub(crate) mod warmer;
pub mod workspace;

pub use navigation::{NavigationIndex, NavigationTarget};
/// Compatibility module for the former `pascal_lsp::project` API.
pub use pascal_project as project;
pub use pascal_project::{ProjectContext, ProjectOptions, discover};

#[cfg(test)]
pub(crate) fn edit_context<R>(
    context: &mut pascal_project::ProjectContext,
    edit: impl FnOnce(&mut pascal_project::ProjectContextParts) -> R,
) {
    let mut parts = std::mem::take(context).into_parts();
    edit(&mut parts);
    *context = pascal_project::ProjectContext::from_parts(parts);
}
