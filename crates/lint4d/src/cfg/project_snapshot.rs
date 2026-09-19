//! Conversion of the shared resolver graph into an immutable CFG project.
//!
//! The resolver owns discovery and source identity.  This module deliberately
//! does not rediscover files or infer import targets: it copies the resolver's
//! explicit decisions into `cfg-pascal`'s caller-owned snapshot contract.

use pascal_core::conditional::{DirectiveKind, Truth};
use pascal_core::resolver::{
    LoadedSource, ResolutionReport, ResolutionTarget, ResolvedImport, ResolvedProject, SourceId,
};
use pascal_project::ConditionalContext;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use tree_sitter::{Node, Parser, Tree};

/// A resolver graph and its validated CFG representation for one target file.
#[derive(Debug, Clone)]
pub struct CfgProjectSnapshot {
    pub snapshot: cfg_pascal::ProjectSnapshot,
    pub target_unit: cfg_pascal::ProjectUnitId,
    pub status: CfgSnapshotStatus,
    pub resolution: ResolutionReport,
    #[allow(dead_code)]
    pub(crate) target_path: PathBuf,
    #[allow(dead_code)]
    pub(crate) target_source_id: SourceId,
    #[allow(dead_code)]
    /// Bytes in the same decoded coordinate space used by the CFG snapshot.
    /// Raw disk bytes remain attached to the resolver source revision.
    pub(crate) target_analysis_bytes: Arc<[u8]>,
}

/// Whether the snapshot is safe to use for cross-unit CFG facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CfgSnapshotStatus {
    Complete,
    Incomplete { reason: String },
}

impl CfgSnapshotStatus {
    pub(crate) fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// Failures constructing the immutable project shell.
#[derive(Debug)]
pub enum CfgSnapshotError {
    Parse { source_id: String, message: String },
    Snapshot(cfg_pascal::ProjectSnapshotError),
    Preparation { source_id: String, message: String },
    MissingTarget(String),
}

impl fmt::Display for CfgSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse { source_id, message } => {
                write!(
                    formatter,
                    "could not parse resolver source {source_id}: {message}"
                )
            }
            Self::Snapshot(error) => write!(formatter, "invalid CFG project snapshot: {error}"),
            Self::Preparation { source_id, message } => {
                write!(
                    formatter,
                    "could not prepare resolver source {source_id}: {message}"
                )
            }
            Self::MissingTarget(source_id) => {
                write!(
                    formatter,
                    "resolver import target {source_id} was not loaded"
                )
            }
        }
    }
}

impl std::error::Error for CfgSnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Snapshot(error) => Some(error),
            _ => None,
        }
    }
}

impl From<cfg_pascal::ProjectSnapshotError> for CfgSnapshotError {
    fn from(error: cfg_pascal::ProjectSnapshotError) -> Self {
        Self::Snapshot(error)
    }
}

/// Options controlling conversion and optional configured-source preparation.
#[derive(Debug, Clone)]
pub struct CfgSnapshotOptions {
    pub prepare_configured_sources: bool,
    pub configuration_id: Option<String>,
    /// Full Task24 conditional context used to project known `IF`/`IFOPT`
    /// expressions into the strict cfg-pascal preparation subset.
    pub conditional_context: ConditionalContext,
    pub preparation_environment: cfg_pascal::PreparationEnvironment,
    pub initial_defined_symbols: Vec<String>,
    pub initial_undefined_symbols: Vec<String>,
    pub preparation_limits: cfg_pascal::PreparationLimits,
}

impl Default for CfgSnapshotOptions {
    fn default() -> Self {
        Self {
            prepare_configured_sources: false,
            configuration_id: None,
            conditional_context: ConditionalContext::default(),
            preparation_environment: cfg_pascal::PreparationEnvironment::Complete,
            initial_defined_symbols: Vec::new(),
            initial_undefined_symbols: Vec::new(),
            preparation_limits: cfg_pascal::PreparationLimits::default(),
        }
    }
}

/// Convert one resolved project into a validated `cfg-pascal` snapshot.
///
/// Raw source inputs are always constructed first.  Preparation is all-or-
/// nothing: any unresolved conditional/include state or source-map mismatch
/// returns the same raw snapshot with an incomplete status instead of mixing
/// precise and conservative inputs.
pub fn to_cfg_project_snapshot(
    project: ResolvedProject,
    options: CfgSnapshotOptions,
) -> Result<CfgProjectSnapshot, CfgSnapshotError> {
    let resolution = project.report.clone();
    let root_source = project.root.source.clone();
    let root_analysis_bytes = analysis_bytes(&root_source);
    let mut status = status_from_resolution(&project, &resolution);
    let mut units = Vec::with_capacity(project.units.len() + 1);
    units.push(project.root.clone());
    units.extend(project.units.clone());

    let (raw_inputs, unit_ids) = parse_raw_inputs(&units)?;
    let root_unit = unit_ids
        .get(&root_source.id)
        .cloned()
        .ok_or_else(|| CfgSnapshotError::MissingTarget(root_source.id.as_str().to_string()))?;

    let raw_imports = import_bindings(&project.imports, &unit_ids)?;
    let raw_snapshot = cfg_pascal::ProjectSnapshot::new(raw_inputs.clone(), raw_imports)?;

    if !options.prepare_configured_sources {
        return Ok(CfgProjectSnapshot {
            snapshot: raw_snapshot,
            target_unit: root_unit,
            status,
            resolution,
            target_path: root_source.path,
            target_source_id: root_source.id,
            target_analysis_bytes: root_analysis_bytes.clone(),
        });
    }

    let Some(configuration_id) = options
        .configuration_id
        .as_deref()
        .filter(|value| !value.is_empty())
    else {
        status = incomplete_status(status, "configured preparation requires a configuration ID");
        return Ok(CfgProjectSnapshot {
            snapshot: raw_snapshot,
            target_unit: root_unit,
            status,
            resolution,
            target_path: root_source.path,
            target_source_id: root_source.id,
            target_analysis_bytes: root_analysis_bytes.clone(),
        });
    };

    if !status.is_complete() {
        return Ok(CfgProjectSnapshot {
            snapshot: raw_snapshot,
            target_unit: root_unit,
            status,
            resolution,
            target_path: root_source.path,
            target_source_id: root_source.id,
            target_analysis_bytes: root_analysis_bytes.clone(),
        });
    }

    let snapshots = source_snapshots(&units, &project.include_sources);
    let projected_snapshots =
        match project_conditional_snapshots(&snapshots, &options.conditional_context) {
            Ok(snapshots) => snapshots,
            Err(reason) => {
                status = incomplete_status(status, reason);
                return Ok(CfgProjectSnapshot {
                    snapshot: raw_snapshot,
                    target_unit: root_unit,
                    status,
                    resolution,
                    target_path: root_source.path,
                    target_source_id: root_source.id,
                    target_analysis_bytes: root_analysis_bytes.clone(),
                });
            }
        };
    let include_bindings = match preparation_include_bindings(&project, &snapshots) {
        Ok(bindings) => bindings,
        Err(reason) => {
            status = incomplete_status(status, reason);
            return Ok(CfgProjectSnapshot {
                snapshot: raw_snapshot,
                target_unit: root_unit,
                status,
                resolution,
                target_path: root_source.path,
                target_source_id: root_source.id,
                target_analysis_bytes: root_analysis_bytes.clone(),
            });
        }
    };

    let prepared_inputs = match prepare_units(
        &units,
        &projected_snapshots,
        &include_bindings,
        configuration_id,
        &options,
    ) {
        Ok(inputs) => inputs,
        Err(reason) => {
            status = incomplete_status(status, reason);
            return Ok(CfgProjectSnapshot {
                snapshot: raw_snapshot,
                target_unit: root_unit,
                status,
                resolution,
                target_path: root_source.path,
                target_source_id: root_source.id,
                target_analysis_bytes: root_analysis_bytes.clone(),
            });
        }
    };

    let prepared_imports =
        match prepared_import_bindings(&project.imports, &prepared_inputs, &unit_ids) {
            Ok(bindings) => bindings,
            Err(reason) => {
                status = incomplete_status(status, reason);
                return Ok(CfgProjectSnapshot {
                    snapshot: raw_snapshot,
                    target_unit: root_unit,
                    status,
                    resolution,
                    target_path: root_source.path,
                    target_source_id: root_source.id,
                    target_analysis_bytes: root_analysis_bytes.clone(),
                });
            }
        };

    let snapshot = match cfg_pascal::ProjectSnapshot::new(prepared_inputs, prepared_imports) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            status = incomplete_status(status, format!("prepared snapshot rejected: {error}"));
            return Ok(CfgProjectSnapshot {
                snapshot: raw_snapshot,
                target_unit: root_unit,
                status,
                resolution,
                target_path: root_source.path,
                target_source_id: root_source.id,
                target_analysis_bytes: root_analysis_bytes.clone(),
            });
        }
    };

    Ok(CfgProjectSnapshot {
        snapshot,
        target_unit: root_unit,
        status,
        resolution,
        target_path: root_source.path,
        target_source_id: root_source.id,
        target_analysis_bytes: root_analysis_bytes,
    })
}

fn status_from_resolution(
    project: &ResolvedProject,
    report: &ResolutionReport,
) -> CfgSnapshotStatus {
    if project.complete && report.complete {
        if project
            .imports
            .iter()
            .any(|import| matches!(import.target, ResolutionTarget::Incomplete))
        {
            return CfgSnapshotStatus::Incomplete {
                reason: "resolver import resolution is incomplete".to_string(),
            };
        }
        if project
            .includes
            .iter()
            .any(|include| matches!(include.target, ResolutionTarget::Incomplete))
        {
            return CfgSnapshotStatus::Incomplete {
                reason: "resolver include resolution is incomplete".to_string(),
            };
        }
        return CfgSnapshotStatus::Complete;
    }

    let reason = report
        .incomplete_reasons
        .first()
        .cloned()
        .unwrap_or_else(|| "resolver project is incomplete".to_string());
    CfgSnapshotStatus::Incomplete { reason }
}

fn incomplete_status(status: CfgSnapshotStatus, reason: impl Into<String>) -> CfgSnapshotStatus {
    match status {
        CfgSnapshotStatus::Complete => CfgSnapshotStatus::Incomplete {
            reason: reason.into(),
        },
        CfgSnapshotStatus::Incomplete { reason: existing } => CfgSnapshotStatus::Incomplete {
            reason: format!("{existing}; {}", reason.into()),
        },
    }
}

fn parse_raw_inputs(
    units: &[pascal_core::resolver::ResolvedUnit],
) -> Result<
    (
        Vec<cfg_pascal::ProjectUnitInput>,
        HashMap<SourceId, cfg_pascal::ProjectUnitId>,
    ),
    CfgSnapshotError,
> {
    let mut inputs = Vec::with_capacity(units.len());
    let mut ids = HashMap::with_capacity(units.len());
    for unit in units {
        let source_id = unit.source.id.clone();
        let unit_id = cfg_pascal::ProjectUnitId::new(source_id.as_str().to_string());
        let source_bytes = unit.source.analysis_bytes();
        let tree = parse_source(&source_id, source_bytes.as_ref())?;
        ids.insert(source_id.clone(), unit_id.clone());
        inputs.push(cfg_pascal::ProjectUnitInput::new(
            unit_id,
            cfg_pascal::ProjectSourceId::new(source_id.as_str().to_string()),
            tree,
            source_bytes.as_ref(),
        ));
    }
    Ok((inputs, ids))
}

fn parse_source(source_id: &SourceId, bytes: &[u8]) -> Result<Tree, CfgSnapshotError> {
    let mut parser = Parser::new();
    parser
        .set_language(&cfg_pascal::LANGUAGE.into())
        .map_err(|error| CfgSnapshotError::Parse {
            source_id: source_id.as_str().to_string(),
            message: error.to_string(),
        })?;
    parser
        .parse(bytes, None)
        .ok_or_else(|| CfgSnapshotError::Parse {
            source_id: source_id.as_str().to_string(),
            message: "parser returned no tree".to_string(),
        })
}

fn import_bindings(
    imports: &[ResolvedImport],
    unit_ids: &HashMap<SourceId, cfg_pascal::ProjectUnitId>,
) -> Result<Vec<cfg_pascal::ImportBinding>, CfgSnapshotError> {
    imports
        .iter()
        .map(|import| {
            let importer = unit_ids.get(&import.importer_source_id).ok_or_else(|| {
                CfgSnapshotError::MissingTarget(import.importer_source_id.as_str().to_string())
            })?;
            let target = match &import.target {
                ResolutionTarget::Found(source_id) => {
                    cfg_pascal::ImportTarget::Loaded(unit_ids.get(source_id).cloned().ok_or_else(
                        || CfgSnapshotError::MissingTarget(source_id.as_str().to_string()),
                    )?)
                }
                ResolutionTarget::Unavailable | ResolutionTarget::Incomplete => {
                    cfg_pascal::ImportTarget::Unavailable
                }
                ResolutionTarget::Ambiguous => cfg_pascal::ImportTarget::Ambiguous,
            };
            Ok(cfg_pascal::ImportBinding::new(
                cfg_pascal::UsesSite::new(importer.clone(), import.site.byte_range.clone()),
                target,
                import.authorized_qualifiers.clone(),
            ))
        })
        .collect()
}

fn source_snapshots(
    units: &[pascal_core::resolver::ResolvedUnit],
    include_sources: &[LoadedSource],
) -> Vec<cfg_pascal::SourceSnapshot> {
    let mut snapshots = Vec::new();
    let mut seen = HashSet::new();
    for source in units
        .iter()
        .map(|unit| &unit.source)
        .chain(include_sources.iter())
    {
        if seen.insert(source.id.clone()) {
            let source_bytes = source.analysis_bytes();
            snapshots.push(cfg_pascal::SourceSnapshot::new(
                cfg_pascal::ProjectSourceId::new(source.id.as_str().to_string()),
                source_bytes.as_ref(),
            ));
        }
    }
    snapshots
}

/// Rewrite only conditions proved by the shared evaluator into the strict
/// `cfg-pascal` expression subset.  Every replacement is byte-length-preserving
/// so source-map ranges still identify the original source coordinates.  An
/// unsupported or unknown condition is left intact and consequently keeps
/// configured CFG preparation incomplete rather than inventing a branch.
fn project_conditional_snapshots(
    snapshots: &[cfg_pascal::SourceSnapshot],
    context: &ConditionalContext,
) -> Result<Vec<cfg_pascal::SourceSnapshot>, String> {
    snapshots
        .iter()
        .map(|snapshot| {
            let source = std::str::from_utf8(snapshot.bytes()).map_err(|_| {
                format!(
                    "conditional CFG projection requires UTF-8 source {}",
                    snapshot.source_id().as_str()
                )
            })?;
            let analysis = pascal_core::conditional::analyze_with_context(source, context);
            if !analysis.complete {
                return Err(format!(
                    "conditional analysis incomplete for {}",
                    snapshot.source_id().as_str()
                ));
            }
            let mut projected = source.as_bytes().to_vec();
            for directive in &analysis.directives {
                let Some(condition) = directive.condition else {
                    continue;
                };
                if !matches!(
                    directive.kind,
                    DirectiveKind::ConditionalStart | DirectiveKind::ConditionalMiddle
                ) {
                    continue;
                }
                let keyword = directive
                    .body
                    .split_ascii_whitespace()
                    .next()
                    .unwrap_or_default();
                let replacement_keyword = match keyword.to_ascii_lowercase().as_str() {
                    "if" => "IF",
                    "elseif" | "elif" => "ELSEIF",
                    "ifopt" => "IF",
                    _ => continue,
                };
                let literal = match condition {
                    Truth::True => "TRUE",
                    Truth::False => "FALSE",
                    Truth::Unknown => continue,
                };
                let replacement = format!("{replacement_keyword} {literal}");
                let body_start = if source.as_bytes().get(directive.start + 1) == Some(&b'$') {
                    directive.start + 2
                } else {
                    directive.start + 3
                };
                let body_end =
                    if source.as_bytes().get(directive.end.saturating_sub(2)) == Some(&b'*') {
                        directive.end.saturating_sub(2)
                    } else {
                        directive.end.saturating_sub(1)
                    };
                if body_start > body_end
                    || body_end > projected.len()
                    || replacement.len() > body_end - body_start
                {
                    continue;
                }
                projected[body_start..body_end].fill(b' ');
                projected[body_start..body_start + replacement.len()]
                    .copy_from_slice(replacement.as_bytes());
            }
            Ok(cfg_pascal::SourceSnapshot::new(
                snapshot.source_id().clone(),
                projected,
            ))
        })
        .collect()
}

fn analysis_bytes(source: &LoadedSource) -> Arc<[u8]> {
    Arc::from(source.analysis_bytes().as_ref())
}

fn preparation_include_bindings(
    project: &ResolvedProject,
    snapshots: &[cfg_pascal::SourceSnapshot],
) -> Result<Vec<cfg_pascal::IncludeBinding>, String> {
    let loaded = snapshots
        .iter()
        .map(|snapshot| snapshot.source_id().as_str())
        .collect::<HashSet<_>>();
    let mut bindings = Vec::new();
    for include in &project.includes {
        let ResolutionTarget::Found(target) = &include.target else {
            if matches!(
                include.target,
                ResolutionTarget::Incomplete | ResolutionTarget::Ambiguous
            ) {
                return Err(format!(
                    "include {} has no precise target",
                    include.requested_name
                ));
            }
            continue;
        };
        if !loaded.contains(include.including_source_id.as_str()) {
            return Err(format!(
                "include owner {} was not loaded",
                include.including_source_id.as_str()
            ));
        }
        if !loaded.contains(target.as_str()) {
            return Err(format!("include target {} was not loaded", target.as_str()));
        }
        bindings.push(cfg_pascal::IncludeBinding::new(
            cfg_pascal::ProjectSourceId::new(include.including_source_id.as_str().to_string()),
            include.byte_range.clone(),
            cfg_pascal::ProjectSourceId::new(target.as_str().to_string()),
        ));
    }
    Ok(bindings)
}

fn prepare_units(
    units: &[pascal_core::resolver::ResolvedUnit],
    snapshots: &[cfg_pascal::SourceSnapshot],
    includes: &[cfg_pascal::IncludeBinding],
    configuration_id: &str,
    options: &CfgSnapshotOptions,
) -> Result<Vec<cfg_pascal::ProjectUnitInput>, String> {
    units
        .iter()
        .map(|unit| {
            let source_id = cfg_pascal::ProjectSourceId::new(unit.source.id.as_str().to_string());
            let prepared_id = cfg_pascal::ProjectSourceId::new(format!(
                "{}#prepared:{}",
                unit.source.id.as_str(),
                configuration_id
            ));
            let preparation_options = cfg_pascal::PrepareSourceOptions::new(
                prepared_id,
                configuration_id.to_string(),
                options.preparation_environment,
            )
            .with_initial_defined_symbols(options.initial_defined_symbols.clone())
            .with_initial_undefined_symbols(options.initial_undefined_symbols.clone())
            .with_limits(options.preparation_limits);
            let prepared =
                cfg_pascal::prepare_source(&source_id, snapshots, includes, preparation_options)
                    .map_err(|error| format!("{}: {error}", unit.source.id.as_str()))?;
            Ok(cfg_pascal::ProjectUnitInput::from_prepared(
                cfg_pascal::ProjectUnitId::new(unit.source.id.as_str().to_string()),
                prepared,
            ))
        })
        .collect()
}

fn prepared_import_bindings(
    imports: &[ResolvedImport],
    prepared_inputs: &[cfg_pascal::ProjectUnitInput],
    unit_ids: &HashMap<SourceId, cfg_pascal::ProjectUnitId>,
) -> Result<Vec<cfg_pascal::ImportBinding>, String> {
    let by_source = prepared_inputs
        .iter()
        .filter_map(|input| {
            input
                .original_sources()
                .iter()
                .find(|source| source.source_id().as_str() == input.id().as_str())
                .map(|source| (source.source_id().as_str().to_string(), input))
                .or_else(|| {
                    input
                        .source_map()
                        .and_then(|map| map.original_sources().first())
                        .map(|source| (source.source_id().as_str().to_string(), input))
                })
        })
        .collect::<HashMap<_, _>>();

    imports.iter().try_fold(Vec::new(), |mut bindings, import| {
        let input = by_source
            .get(import.importer_source_id.as_str())
            .ok_or_else(|| {
                format!(
                    "prepared importer {} was not loaded",
                    import.importer_source_id.as_str()
                )
            })?;
        let mapped = map_import_range(
            input,
            import.importer_source_id.as_str(),
            &import.site.byte_range,
        )
        .map_err(|error| error.to_string())?;
        let Some(range) = mapped else {
            // A masked module name is a known inactive import.  It must not
            // be represented as a UsesSite in the prepared tree.
            return Ok(bindings);
        };
        let importer = unit_ids
            .get(&import.importer_source_id)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "prepared importer {} was not loaded",
                    import.importer_source_id.as_str()
                )
            })?;
        let target = match &import.target {
            ResolutionTarget::Found(source_id) => {
                cfg_pascal::ImportTarget::Loaded(unit_ids.get(source_id).cloned().ok_or_else(
                    || format!("prepared target {} was not loaded", source_id.as_str()),
                )?)
            }
            ResolutionTarget::Unavailable | ResolutionTarget::Incomplete => {
                cfg_pascal::ImportTarget::Unavailable
            }
            ResolutionTarget::Ambiguous => cfg_pascal::ImportTarget::Ambiguous,
        };
        bindings.push(cfg_pascal::ImportBinding::new(
            cfg_pascal::UsesSite::new(importer, range),
            target,
            import.authorized_qualifiers.clone(),
        ));
        Ok(bindings)
    })
}

/// Map one original import occurrence to a single copied module-name node in
/// a prepared tree. `None` means the occurrence was masked as inactive.
fn map_import_range(
    input: &cfg_pascal::ProjectUnitInput,
    source_id: &str,
    original_range: &Range<usize>,
) -> Result<Option<Range<usize>>, MappingFailure> {
    let Some(map) = input.source_map() else {
        return Err(MappingFailure::MissingMap);
    };
    let mut copied = Vec::new();
    let mut masked = false;
    for segment in map.segments() {
        let Some(original) = segment.original() else {
            continue;
        };
        if original.source_id().as_str() != source_id
            || original.byte_range.start > original_range.start
            || original.byte_range.end < original_range.end
        {
            continue;
        }
        let offset = original_range.start - original.byte_range.start;
        let prepared = segment.prepared_range.start + offset
            ..segment.prepared_range.start + offset + original_range.len();
        let mapped = map
            .map_range(prepared.clone())
            .map_err(|_| MappingFailure::MapError)?;
        if mapped.len() != 1 {
            return Err(MappingFailure::Split);
        }
        let mapped = &mapped[0];
        let exact_original = mapped.original().is_some_and(|origin| {
            origin.source_id().as_str() == source_id
                && origin.byte_range() == original_range.clone()
        });
        if !exact_original {
            return Err(MappingFailure::MapError);
        }
        match segment.kind() {
            cfg_pascal::SourceSegmentKind::Copied => copied.push(prepared),
            cfg_pascal::SourceSegmentKind::Masked => masked = true,
            cfg_pascal::SourceSegmentKind::Synthetic => return Err(MappingFailure::Synthetic),
        }
    }
    if copied.len() > 1 {
        return Err(MappingFailure::Split);
    }
    if let Some(range) = copied.into_iter().next() {
        if !contains_module_name(input.tree().root_node(), &range) {
            return Err(MappingFailure::MissingNode);
        }
        return Ok(Some(range));
    }
    if masked {
        Ok(None)
    } else {
        Err(MappingFailure::MissingMap)
    }
}

#[derive(Debug, Clone, Copy)]
enum MappingFailure {
    MissingMap,
    MapError,
    Split,
    Synthetic,
    MissingNode,
}

impl fmt::Display for MappingFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingMap => "import has no validated source-map origin",
            Self::MapError => "import source-map range could not be validated",
            Self::Split => "import source-map range crosses a projection boundary",
            Self::Synthetic => "import source-map range is synthetic",
            Self::MissingNode => "prepared tree has no matching moduleName node",
        })
    }
}

fn contains_module_name(root: Node<'_>, wanted: &Range<usize>) -> bool {
    fn visit(node: Node<'_>, in_uses: bool, wanted: &Range<usize>) -> bool {
        let in_uses = in_uses || node.kind() == "declUses";
        if in_uses && node.kind() == "moduleName" && node.byte_range() == *wanted {
            return true;
        }
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .any(|child| visit(child, in_uses, wanted))
    }
    visit(root, false, wanted)
}
