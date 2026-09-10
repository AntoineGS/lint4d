//! Native Linux language-server building blocks for Delphi/Object Pascal.

pub mod navigation;
pub mod project;
pub mod server;
pub mod text;
pub mod workspace;

pub use navigation::{NavigationIndex, NavigationTarget};
pub use project::{ProjectContext, ProjectOptions, discover};
