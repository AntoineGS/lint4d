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
pub(crate) trait ContextSlot: Default {
    fn into_context(self) -> pascal_project::ProjectContext;
    fn from_context(context: pascal_project::ProjectContext) -> Self;
}

#[cfg(test)]
impl ContextSlot for pascal_project::ProjectContext {
    fn into_context(self) -> pascal_project::ProjectContext {
        self
    }

    fn from_context(context: pascal_project::ProjectContext) -> Self {
        context
    }
}

#[cfg(test)]
impl ContextSlot for std::sync::Arc<pascal_project::ProjectContext> {
    fn into_context(self) -> pascal_project::ProjectContext {
        std::sync::Arc::unwrap_or_clone(self)
    }

    fn from_context(context: pascal_project::ProjectContext) -> Self {
        std::sync::Arc::new(context)
    }
}

#[cfg(test)]
pub(crate) fn edit_context<C: ContextSlot, R>(
    context: &mut C,
    edit: impl FnOnce(&mut pascal_project::ProjectContextParts) -> R,
) {
    let mut parts = std::mem::take(context).into_context().into_parts();
    edit(&mut parts);
    *context = C::from_context(pascal_project::ProjectContext::from_parts(parts));
}
