use crate::ProjectPathProvenance;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectPathIssueKind {
    MissingReference,
    MissingMainSource,
    MissingDirectory,
    UnresolvedProperty,
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPathIssue {
    pub kind: ProjectPathIssueKind,
    pub source_file: PathBuf,
    pub property: String,
    pub raw: String,
    pub path: Option<PathBuf>,
    pub unit_name: Option<String>,
    pub provenance: ProjectPathProvenance,
}
