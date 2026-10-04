pub use cfg_core;
pub use tree_sitter_pascal::LANGUAGE;

pub mod calls;
pub(crate) mod constructs;
mod exception_types;
pub mod factory;
mod pascal_builder;
pub mod prepared;
pub mod project;
pub mod source_map;

pub use pascal_builder::{build_file_cfgs, build_file_cfgs_in_project};
pub use prepared::{
    prepare_source, prepare_source_with_options, EnvironmentCompleteness, IncludeBinding,
    PreparationBudget, PreparationEnvironment, PreparationFidelity, PreparationLimits,
    PreparationOptions, PreparationProvenance, PrepareSourceError, PrepareSourceOptions,
    PreparedSource, PreparedSourceError, ResolvedIncludeBinding,
};
pub use project::{
    ImportBinding, ImportTarget, ProjectBuildError, ProjectSnapshot, ProjectSnapshotError,
    ProjectSourceId, ProjectUnitId, ProjectUnitInput, UsesSite,
};
pub use source_map::{
    ExpansionId, MappedSourceSpan, MappedSpan, SourceMap, SourceMapError, SourceMapSegment,
    SourceSegment, SourceSegmentKind, SourceSnapshot, SourceSnapshotError, SourceSpan,
};
