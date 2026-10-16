//! Lazy, filesystem-only Delphi project context discovery.
//!
//! This crate resolves bounded project metadata and resolution paths from
//! `.dproj`, `.dpr`, `.dpk`, and option-set files. It records defines, aliases,
//! unit/include search paths, project references, provenance, and freshness
//! observations without recursively scanning a workspace.
//!
//! It does not expand or look up Pascal include files, and it is not a full
//! MSBuild evaluator. The existing basic CLI source discovery and generic
//! MSBuild-facing discovery helpers intentionally remain in `pascal-core`.
//! `pascal-lsp` owns stateful workspace orchestration, overlays, indexes, and
//! the include/rename resolver that consumes this crate's bounded results.

pub mod analysis_metrics;
pub mod build_selection;
pub mod compiler_defines;
pub mod conditional;
pub mod conditional_closure;
pub mod configuration;
pub mod delphi_overrides;
pub mod installation_config;
#[allow(dead_code)]
pub mod installations;
pub mod layout;
pub mod path_identity;
pub mod path_issues;
pub mod rtl_constants;

pub use build_selection::{BuildCandidates, BuildChoice, BuildSelection, BuildSelectionMode};
pub use compiler_defines::TargetPlatform;
pub use conditional::{
    CompilerVersion, ConditionalContext, ConditionalFact, ConstantValue, canonical_option_name,
};
pub use conditional_closure::{ConditionalClosure, OpenReason, SourceOrigin};
pub use configuration::{ConfigRead, config_directories, read_config};
pub use installations::{InstallationOrigin, InstallationSelection};
pub use layout::{LayoutContext, LayoutPlatform, LayoutSettings};
pub use path_issues::{ProjectPathIssue, ProjectPathIssueKind};

use crate::delphi_overrides::{
    EffectiveOverrides, LOCAL_CONFIG_NAME, OverrideSession, PathMapping, ResolvedPath,
};
use crate::installations::{
    IdePaths, InstallationEvidence, evaluate_ide_paths, load_installation, select_installation,
};
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::ops::Range;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

const MAX_PROJECT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_IMPORT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MAIN_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PACKAGE_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_LOCAL_PROJECT_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_LOCAL_PROJECT_CONFIG_CANDIDATES: usize = 128;
const MAX_IMPORT_COUNT: usize = 64;
const MAX_METADATA_FILES: usize = MAX_IMPORT_COUNT + 1;
const MAX_EXPANDED_VALUE_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_PROPERTY_BYTES: usize = 16 * 1024 * 1024;
const MAX_OWNERSHIP_CANDIDATES: usize = 32;
const MAX_PROJECT_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_OWNERSHIP_METADATA_FILES: usize = 512;
const MAX_OWNERSHIP_SOURCE_FILES: usize = 256;
const MAX_OWNERSHIP_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OWNERSHIP_WARNING_BYTES: usize = 256 * 1024;
const UNRESOLVED_MARKER: char = '\u{1}';

pub use authorization::{
    ProjectPathEntry, ProjectPathProvenance, ReadPolicy, ReadPolicyView, ReadRootView,
};
mod authorization;
pub use reads::{
    MetadataObservation, ProjectDiscovery, ProjectReadObservation, ProjectReadStamp,
    ProjectWorkBudget, SystemPasSearchObservation, content_hash_bytes, metadata_payload_is_current,
    path_stamp_result,
};
#[cfg(any(test, feature = "test-support"))]
pub use reads::{
    TestAfterProjectReadAtGuard, TestAfterProjectReadGuard, TestBeforeProjectReadAtGuard,
    test_after_project_read, test_after_project_read_at, test_before_project_read_at,
};
mod reads;
pub use discovery_cache::{ProjectDiscoveryCacheScope, ProjectDiscoveryCacheStats};
#[cfg(any(test, feature = "test-support"))]
pub use discovery_cache::{
    TestProjectDirectoryAdvanceGuard, TestProjectScanCancellationGuard,
    test_before_project_directory_advance, test_cancel_project_scan_after_checks,
    test_project_directory_listing_counts, test_project_discovery_counts,
    test_reset_project_directory_listing_counts, test_reset_project_discovery_counts,
};
mod discovery_cache;
#[cfg(any(test, feature = "test-support"))]
pub use context::ProjectContextParts;
pub use context::{
    ProjectCandidateMembership, ProjectCandidates, ProjectContext, ProjectOptions,
    ProjectSelections,
};
mod context;
pub use observations::{DiscoveredProject, ProjectObservations};
mod observations;
pub use selection::{
    discover, discover_with_selections,
    discover_with_selections_and_observations_with_cancel_and_overrides,
    discover_with_selections_and_observations_with_overrides,
    discover_with_selections_and_observations_with_overrides_and_deleted_paths,
    discover_with_selections_and_observations_with_work_budget,
    discover_with_selections_and_observations_with_work_budget_and_deleted_paths,
    discover_with_selections_and_observations_with_work_budget_and_optional_cancel_and_deleted_paths,
    has_invalid_project_selection, project_candidate_membership,
    project_candidate_membership_with_deleted_paths,
    project_candidate_membership_with_deleted_paths_and_budget, project_candidates,
    project_candidates_with_cancel, project_candidates_with_work_budget,
    project_candidates_with_work_budget_and_deleted_paths, runtime_project_selection,
    selected_project_is_current, selected_project_is_current_with_budget_and_deleted_paths,
    selected_project_is_current_with_cancel, selected_project_is_current_with_work_budget,
};
mod selection;
pub use assembly::{add_metadata_observation, system_pas_candidate_in_directory};
mod assembly;
pub use packages::{
    PackageMetadata, PackageMetadataRead, read_package_metadata,
    read_package_metadata_with_observations,
    read_package_metadata_with_observations_and_work_budget,
};
mod membership;
mod msbuild_eval;
mod packages;
use assembly::add_unique_path;
use authorization::{ExistingPathStatus, resolve_existing_path_status};
use msbuild_eval::{ProjectBuilder, local_name, xml_attributes, xml_reference_text};
use reads::{ProjectReadTracker, read_payload_with_tracker};
use selection::{project_path_starts_with, project_paths_equal};
