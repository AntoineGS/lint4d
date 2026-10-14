// Re-export shared types from pascal-core so existing `use crate::engine::{...}` paths work
pub use pascal_core::parser::parse_file;
pub use pascal_core::{Diagnostic, FileInfo, FileType, Severity};

// Re-export suppress module for backward compatibility
pub mod suppress {
    pub use pascal_core::directives::*;
}

use pascal_core::node_kind as K;

use crate::cfg::analysis::AnalysisContext;
use crate::cfg::project_snapshot::{CfgProjectSnapshot, CfgSnapshotStatus};
use crate::config::{Config, RuleSeverityOverride};
use crate::dcu::ProjectContext;
use crate::rules::helpers::extract_unit_name;
use crate::rules::{LintContext, RuleCategory, RuleRegistry};
use cfg_core::call_graph::CallGraph;
use cfg_core::summary::ProcId;
use cfg_pascal::cfg_core;
use cfg_pascal::{build_file_cfgs, build_file_cfgs_in_project};
use pascal_core::parser::node_text;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

#[cfg(test)]
std::thread_local! {
    static CFG_BUILDER_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_cfg_builder_call() {
    CFG_BUILDER_CALLS.with(|calls| calls.set(calls.get() + 1));
}

/// Run all lint rules on a single file and return sorted, filtered diagnostics.
///
/// This is a convenience wrapper that creates a default [`RuleRegistry`] and
/// delegates to [`run_lint_with_context`] with no project context.
pub fn run_lint(file: &FileInfo, source: &[u8], config: &Config) -> Vec<Diagnostic> {
    let registry = RuleRegistry::new();
    run_lint_with_cfg_project(file, source, config, None, None, &registry)
}

/// Run all lint rules on a single file with an optional project context.
///
/// This function:
/// 1. Parses the file and collects parse-error diagnostics
/// 2. Runs all enabled rules (respecting config overrides and file-type skipping)
/// 3. When `project` is `Some`, dispatches via `check_with_context`; otherwise
///    skips rules that `requires_context()` and dispatches via `check`
/// 4. Applies severity overrides from config
/// 5. Filters out suppressed diagnostics
/// 6. Sorts results by line, then column
pub fn run_lint_with_context(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
    project: Option<&crate::dcu::ProjectContext>,
    registry: &RuleRegistry,
) -> Vec<Diagnostic> {
    run_lint_with_cfg_project(file, source, config, project, None, registry)
}

/// Run lint rules with an optional resolver-backed CFG project.
///
/// The rule, suppression, severity, scope, and ordering pipeline is shared by
/// the legacy file-local and project-aware entry points.  A complete snapshot
/// is the only mode allowed to contribute cross-unit CFG facts; incomplete
/// snapshots deliberately use the established file-local builder.
pub fn run_lint_with_cfg_project(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
    project: Option<&crate::dcu::ProjectContext>,
    cfg_project: Option<&CfgProjectSnapshot>,
    registry: &RuleRegistry,
) -> Vec<Diagnostic> {
    run_lint_with_cfg_project_mode(file, source, config, project, cfg_project, registry, true)
}

/// Run only rules whose results do not depend on a project CFG.
///
/// The source may still be a proven include-expanded buffer.  This mode keeps
/// physical-source local rules available while withholding CFG conclusions
/// when project resolution is incomplete or otherwise untrusted.
pub fn run_lint_file_local_rules(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
    registry: &RuleRegistry,
) -> Vec<Diagnostic> {
    run_lint_with_cfg_project_mode(file, source, config, None, None, registry, false)
}

fn run_lint_with_cfg_project_mode(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
    project: Option<&crate::dcu::ProjectContext>,
    cfg_project: Option<&CfgProjectSnapshot>,
    registry: &RuleRegistry,
    run_cfg_rules: bool,
) -> Vec<Diagnostic> {
    let (tree, mut diagnostics) = match parse_file(file, source) {
        Ok(result) => result,
        Err(e) => {
            return vec![error_diagnostic(e)];
        }
    };
    let raw_scopes = collect_proc_scopes(tree.root_node(), source);

    let (analysis_tree, analysis_source, file_cfgs, prepared_map) =
        match select_cfg_inputs(file, source, tree, cfg_project) {
            Ok(inputs) => inputs,
            Err(error) => return vec![error_diagnostic(error)],
        };

    let unit_name =
        extract_unit_name(analysis_tree.root_node(), &analysis_source).unwrap_or_default();
    // Build per-method CFGs only when an eligible CFG rule will consume them. Project
    // build failures in that mode are surfaced as one bounded engine
    // diagnostic; file-local-only mode must not manufacture a CFG diagnostic
    // while deliberately withholding uncertain flow conclusions.
    let has_enabled_cfg_consumer = run_cfg_rules
        && registry.all_rules().iter().any(|rule| {
            rule.requires_cfg() && rule_is_eligible_for_dispatch(rule.meta(), file, config)
        });
    let cfg_map = if has_enabled_cfg_consumer {
        #[cfg(test)]
        record_cfg_builder_call();
        let file_cfgs = match file_cfgs {
            CfgInputs::FileLocal { tree, source } => build_file_cfgs(&tree, &source),
            CfgInputs::Project {
                snapshot,
                target_unit,
            } => match build_file_cfgs_in_project(&snapshot, &target_unit) {
                Ok(cfgs) => cfgs,
                Err(error) => return vec![error_diagnostic(error.to_string())],
            },
        };
        file_cfgs
            .into_iter()
            .map(|cfg| {
                let proc_id = ProcId::new(&unit_name, &cfg.proc_name, cfg.byte_range.clone());
                (proc_id, cfg)
            })
            .collect()
    } else {
        HashMap::new()
    };

    let default_project = ProjectContext::from_units(vec![]);
    let proj_ref = project.unwrap_or(&default_project);
    let analysis = AnalysisContext::new(cfg_map, CallGraph::new(), proj_ref);

    let mut ctx = LintContext::new();

    for rule in registry.all_rules() {
        let meta = rule.meta();

        if !rule_is_eligible_for_dispatch(meta, file, config) {
            continue;
        }

        if rule.requires_cfg() {
            if !run_cfg_rules {
                continue;
            }
            rule.check_cfg(
                file,
                &analysis_tree,
                &analysis_source,
                config,
                &analysis,
                &mut ctx,
            );
        } else if rule.requires_context() {
            match project {
                Some(proj) => {
                    rule.check_with_context(
                        file,
                        &analysis_tree,
                        &analysis_source,
                        config,
                        proj,
                        &mut ctx,
                    );
                }
                None => {
                    // Skip context-dependent rules when no project context is available.
                }
            }
        } else {
            rule.check(file, &analysis_tree, &analysis_source, config, &mut ctx);
        }
    }

    // Apply severity overrides from config.
    for diag in &mut ctx.diagnostics {
        if let Some(RuleSeverityOverride::Severity(s)) = config.rule_severity(&diag.rule_id) {
            diag.severity = s;
        }
    }

    // Skip parse-error diagnostics for .dpr/.dpk files.
    // The tree-sitter-pascal grammar does not support the `in 'path'` clause
    // used in project/package uses sections, which produces numerous spurious
    // parse errors on otherwise valid code.
    if matches!(file.file_type, FileType::Dpr | FileType::Dpk) {
        diagnostics.retain(|d| d.rule_id != "parse-error");
    }

    // Parse diagnostics are produced from the original source, while rule
    // diagnostics are produced from the selected analysis source.  Enrich
    // each set in its own coordinate space before any prepared-source mapping.
    for diag in &mut diagnostics {
        if diag.scope.is_none() {
            diag.scope = find_enclosing_scope(&raw_scopes, diag.line);
        }
    }
    let analysis_scopes = collect_proc_scopes(analysis_tree.root_node(), &analysis_source);
    for diag in &mut ctx.diagnostics {
        if diag.scope.is_none() {
            diag.scope = find_enclosing_scope(&analysis_scopes, diag.line);
        }
    }

    // Prepared diagnostics are initially in prepared coordinates.  Only a
    // single-file, origin-bearing mapping is safe to publish.  Anything that
    // crosses an include boundary, points at synthetic bytes, or belongs to a
    // different source reruns through the raw file-local path.
    if let Some(map) = prepared_map
        && !map_diagnostics_to_original(&mut ctx.diagnostics, &map, source)
    {
        return run_lint_with_cfg_project_mode(
            file,
            source,
            config,
            project,
            None,
            registry,
            run_cfg_rules,
        );
    }

    // Merge parse-error diagnostics with rule diagnostics.
    diagnostics.append(&mut ctx.diagnostics);

    // Filter out suppressed diagnostics.
    let suppressions = suppress::parse_suppressions(source);
    diagnostics.retain(|diag| {
        !suppressions
            .iter()
            .any(|s| s.matches(&diag.rule_id, diag.line))
    });

    // Sort by line, then column.
    diagnostics.sort_by(|a, b| a.line.cmp(&b.line).then(a.column.cmp(&b.column)));

    diagnostics
}

fn rule_is_eligible_for_dispatch(
    meta: &crate::rules::RuleMeta,
    file: &FileInfo,
    config: &Config,
) -> bool {
    let enabled = match config.rule_severity(meta.id) {
        Some(RuleSeverityOverride::Off) => false,
        Some(RuleSeverityOverride::Severity(_)) => true,
        None => meta.enabled_by_default,
    };

    enabled
        && !(matches!(file.file_type, FileType::Dpr | FileType::Dpk)
            && matches!(meta.category, RuleCategory::NamingConvention))
}

#[derive(Debug)]
enum CfgInputs {
    FileLocal {
        tree: tree_sitter::Tree,
        source: Vec<u8>,
    },
    Project {
        snapshot: cfg_pascal::ProjectSnapshot,
        target_unit: cfg_pascal::ProjectUnitId,
    },
}

fn select_cfg_inputs(
    file: &FileInfo,
    source: &[u8],
    parsed_tree: tree_sitter::Tree,
    cfg_project: Option<&CfgProjectSnapshot>,
) -> Result<
    (
        tree_sitter::Tree,
        Vec<u8>,
        CfgInputs,
        Option<cfg_pascal::SourceMap>,
    ),
    String,
> {
    let Some(cfg_project) = cfg_project else {
        return Ok((
            parsed_tree.clone(),
            source.to_vec(),
            CfgInputs::FileLocal {
                tree: parsed_tree,
                source: source.to_vec(),
            },
            None,
        ));
    };

    if !paths_equivalent(&file.path, &cfg_project.target_path) {
        return Err(format!(
            "CFG target path {} does not match lint file {}",
            cfg_project.target_path.display(),
            file.path.display()
        ));
    }
    if cfg_project.target_analysis_bytes.as_ref() != source {
        return Err(format!(
            "CFG target source {} does not match the caller's analysis bytes",
            cfg_project.target_source_id.as_str()
        ));
    }

    let Some(target) = cfg_project.snapshot.unit(&cfg_project.target_unit) else {
        return Err(format!(
            "CFG project is missing target unit {}",
            cfg_project.target_unit.as_str()
        ));
    };
    if !matches!(cfg_project.status, CfgSnapshotStatus::Complete) {
        return Ok((
            parsed_tree.clone(),
            source.to_vec(),
            CfgInputs::FileLocal {
                tree: parsed_tree,
                source: source.to_vec(),
            },
            None,
        ));
    }

    let target_source = target.source().to_vec();
    let target_tree = target.tree().clone();
    let source_map = target.source_map().cloned();
    Ok((
        target_tree.clone(),
        target_source.clone(),
        CfgInputs::Project {
            snapshot: cfg_project.snapshot.clone(),
            target_unit: cfg_project.target_unit.clone(),
        },
        source_map,
    ))
}

fn map_diagnostics_to_original(
    diagnostics: &mut [Diagnostic],
    source_map: &cfg_pascal::SourceMap,
    original_source: &[u8],
) -> bool {
    if diagnostics.is_empty() {
        return true;
    }

    let mut prepared_positions = LineStartIndex::new(source_map.prepared_bytes());
    let mut original_positions = LineStartIndex::new(original_source);

    for diagnostic in diagnostics {
        let Some(start) =
            prepared_positions.byte_offset_at_position(diagnostic.line, diagnostic.column)
        else {
            return false;
        };
        let Some(end) =
            prepared_positions.byte_offset_at_position(diagnostic.end_line, diagnostic.end_column)
        else {
            return false;
        };
        if start >= end {
            return false;
        }
        let Ok(mapped) = source_map.map_range(start..end) else {
            return false;
        };
        if mapped.len() != 1 {
            return false;
        }
        let Some(original) = mapped[0].original() else {
            return false;
        };
        if !matches!(mapped[0].kind(), cfg_pascal::SourceSegmentKind::Copied)
            || original.source_id().as_str()
                != source_map
                    .original_sources()
                    .first()
                    .map(|source| source.source_id().as_str())
                    .unwrap_or_default()
        {
            return false;
        }
        let original_range = original.byte_range();
        let Some((line, column)) = original_positions.position_at_offset(original_range.start)
        else {
            return false;
        };
        let Some((end_line, end_column)) =
            original_positions.position_at_offset(original_range.end)
        else {
            return false;
        };
        diagnostic.line = line;
        diagnostic.column = column;
        diagnostic.end_line = end_line;
        diagnostic.end_column = end_column;
    }
    true
}

/// Private LF-only byte-coordinate index used while mapping prepared
/// diagnostics. It borrows the source and extends a monotonic prefix only as
/// coordinates require; cached starts serve direct line lookup and binary
/// search.
struct LineStartIndex<'source> {
    source: &'source [u8],
    line_starts: Vec<usize>,
    scanned_to: usize,
}

impl<'source> LineStartIndex<'source> {
    fn new(source: &'source [u8]) -> Self {
        #[cfg(test)]
        record_line_start_index_build();

        Self {
            source,
            line_starts: vec![0],
            scanned_to: 0,
        }
    }

    fn byte_offset_at_position(&mut self, line: usize, column: usize) -> Option<usize> {
        if line == 0 || column == 0 {
            return None;
        }
        self.extend_through_line(line);
        let line_start = *self.line_starts.get(line - 1)?;
        // Keep the original usize arithmetic order: debug overflow still
        // panics, and release overflow still wraps before the bounds check.
        let offset = line_start + column - 1;
        (offset <= self.source.len()).then_some(offset)
    }

    fn position_at_offset(&mut self, offset: usize) -> Option<(usize, usize)> {
        if offset > self.source.len() {
            return None;
        }
        self.extend_through_offset(offset);

        #[cfg(test)]
        let mut search_probes = 0;
        let line_index = self.line_starts.partition_point(|line_start| {
            #[cfg(test)]
            {
                search_probes += 1;
            }
            *line_start <= offset
        }) - 1;
        #[cfg(test)]
        record_line_start_search_probes(search_probes);

        let line_start = self.line_starts[line_index];
        Some((line_index + 1, offset - line_start + 1))
    }

    fn extend_through_line(&mut self, line: usize) {
        #[cfg(test)]
        let previous_scanned_to = self.scanned_to;
        #[cfg(test)]
        let previous_line_starts = self.line_starts.len();
        while self.line_starts.len() < line && self.scanned_to < self.source.len() {
            self.scan_next_byte();
        }
        #[cfg(test)]
        self.record_scan_extension(previous_scanned_to, previous_line_starts);
    }

    fn extend_through_offset(&mut self, offset: usize) {
        #[cfg(test)]
        let previous_scanned_to = self.scanned_to;
        #[cfg(test)]
        let previous_line_starts = self.line_starts.len();
        while self.scanned_to < offset {
            self.scan_next_byte();
        }
        #[cfg(test)]
        self.record_scan_extension(previous_scanned_to, previous_line_starts);
    }

    fn scan_next_byte(&mut self) {
        if self.source[self.scanned_to] == b'\n' {
            self.line_starts.push(self.scanned_to + 1);
        }
        self.scanned_to += 1;
    }

    #[cfg(test)]
    fn record_scan_extension(&self, previous_scanned_to: usize, previous_line_starts: usize) {
        record_line_start_scan(
            self.scanned_to - previous_scanned_to,
            self.line_starts.len() - previous_line_starts,
        );
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Default)]
struct LineStartIndexWork {
    builds: usize,
    bytes_scanned: usize,
    line_starts_added: usize,
    offset_search_probes: usize,
}

#[cfg(test)]
std::thread_local! {
    static LINE_START_INDEX_WORK: std::cell::Cell<LineStartIndexWork> = const {
        std::cell::Cell::new(LineStartIndexWork {
            builds: 0,
            bytes_scanned: 0,
            line_starts_added: 0,
            offset_search_probes: 0,
        })
    };
}

#[cfg(test)]
fn record_line_start_index_build() {
    LINE_START_INDEX_WORK.with(|counter| {
        let mut work = counter.get();
        work.builds += 1;
        counter.set(work);
    });
}

#[cfg(test)]
fn record_line_start_scan(bytes_scanned: usize, line_starts_added: usize) {
    LINE_START_INDEX_WORK.with(|counter| {
        let mut work = counter.get();
        work.bytes_scanned += bytes_scanned;
        work.line_starts_added += line_starts_added;
        counter.set(work);
    });
}

#[cfg(test)]
fn record_line_start_search_probes(probes: usize) {
    LINE_START_INDEX_WORK.with(|counter| {
        let mut work = counter.get();
        work.offset_search_probes += probes;
        counter.set(work);
    });
}

#[cfg(test)]
fn reset_line_start_index_work() {
    LINE_START_INDEX_WORK.with(|work| work.set(LineStartIndexWork::default()));
}

#[cfg(test)]
fn line_start_index_work() -> LineStartIndexWork {
    LINE_START_INDEX_WORK.with(std::cell::Cell::get)
}

fn paths_equivalent(left: &Path, right: &Path) -> bool {
    use pascal_project::path_identity::{on_disk_spelling, paths_equal};
    // The CFG target carries the on-disk spelling; the lint file may be named
    // through 8.3 short names or another letter case.
    let spelled = |path: &Path| {
        let path = lexical_absolute(path);
        on_disk_spelling(&path).unwrap_or(path)
    };
    paths_equal(&spelled(left), &spelled(right))
}

fn lexical_absolute(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            Component::RootDir | Component::Prefix(_) => result.push(component.as_os_str()),
            Component::Normal(value) => result.push(value),
        }
    }
    result
}

fn error_diagnostic(message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        rule_id: "lint4d-error".to_string(),
        severity: Severity::Error,
        message: message.into(),
        line: 1,
        column: 1,
        end_line: 1,
        end_column: 1,
        help: None,
        scope: None,
    }
}

// ---- Scope enrichment -------------------------------------------------------

struct ProcScope {
    start_line: usize,
    end_line: usize,
    name: String,
}

/// Walk the AST and collect all `defProc` nodes with their line ranges and names.
fn collect_proc_scopes(root: tree_sitter::Node, source: &[u8]) -> Vec<ProcScope> {
    let mut scopes = Vec::new();
    collect_proc_scopes_recursive(root, source, &mut scopes);
    scopes.sort_by_key(|s| s.start_line);
    scopes
}

fn collect_proc_scopes_recursive(node: tree_sitter::Node, source: &[u8], out: &mut Vec<ProcScope>) {
    if node.kind() == K::DEF_PROC
        && let Some(name) = extract_proc_display_name(node, source)
    {
        out.push(ProcScope {
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            name,
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_proc_scopes_recursive(child, source, out);
    }
}

/// Extract display name from a `defProc` node.
///
/// Methods: `TClassName.MethodName` (from `genericDot`).
/// Standalone: just the identifier name.
fn extract_proc_display_name(def_proc: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut cursor = def_proc.walk();
    let decl_proc = def_proc
        .children(&mut cursor)
        .find(|c| c.kind() == K::DECL_PROC)?;

    let mut decl_cursor = decl_proc.walk();
    if let Some(generic_dot) = decl_proc
        .children(&mut decl_cursor)
        .find(|c| c.kind() == K::GENERIC_DOT)
    {
        let idents: Vec<tree_sitter::Node> = generic_dot
            .children(&mut generic_dot.walk())
            .filter(|c| c.kind() == K::IDENTIFIER)
            .collect();
        if idents.len() >= 2 {
            return Some(format!(
                "{}.{}",
                node_text(idents[0], source),
                node_text(idents[1], source)
            ));
        }
        if !idents.is_empty() {
            return Some(node_text(idents[0], source));
        }
    }

    if let Some(name_node) = decl_proc.child_by_field_name("name") {
        return Some(node_text(name_node, source));
    }

    let mut cursor2 = decl_proc.walk();
    for child in decl_proc.children(&mut cursor2) {
        if child.kind() == K::IDENTIFIER {
            return Some(node_text(child, source));
        }
    }

    None
}

/// Find the innermost enclosing procedure scope for a 1-based line number.
fn find_enclosing_scope(scopes: &[ProcScope], line: usize) -> Option<String> {
    let mut best: Option<&ProcScope> = None;
    for scope in scopes {
        if line >= scope.start_line && line <= scope.end_line {
            match best {
                Some(prev) if scope.start_line > prev.start_line => best = Some(scope),
                None => best = Some(scope),
                _ => {}
            }
        }
    }
    best.map(|s| s.name.clone())
}

#[cfg(test)]
mod tests {
    use super::{CFG_BUILDER_CALLS, FileInfo, RuleRegistry, run_lint, run_lint_file_local_rules};
    use crate::config::Config;
    use std::path::PathBuf;

    const NO_CFG_RULES_CONFIG: &str = r#"
version = 1
[rules]
resource-leak-unprotected = "off"
resource-leak-no-try = "off"
field-not-freed = "off"
field-reassign-leak = "off"
bare-except = "off"
empty-except = "off"
raise-in-destructor = "off"
type-prefix = "off"
interface-prefix = "off"
constant-naming = "off"
identifier-casing = "off"
inherited-order = "off"
inherited-missing = "off"
use-after-free = "off"
transaction = "off"
"#;

    fn reset_cfg_builder_calls() {
        CFG_BUILDER_CALLS.with(|calls| calls.set(0));
    }

    fn cfg_builder_calls() -> usize {
        CFG_BUILDER_CALLS.with(std::cell::Cell::get)
    }

    #[test]
    fn naming_and_with_rules_run_without_building_cfgs_when_cfg_consumers_are_off() {
        reset_cfg_builder_calls();
        let source = b"unit Gate;\ninterface\nimplementation\nprocedure Run;\nvar badName: Integer; Obj: TObject;\nbegin\n  with Obj do badName := 1;\nend;\nend.\n";
        let file = FileInfo::new(PathBuf::from("Gate.pas"));
        let config = NO_CFG_RULES_CONFIG.parse::<Config>().unwrap();

        let diagnostics = run_lint(&file, source, &config);

        assert_eq!(cfg_builder_calls(), 0);
        assert!(
            diagnostics
                .iter()
                .any(|diag| diag.rule_id == "with-statement")
        );
        assert!(
            diagnostics
                .iter()
                .any(|diag| diag.rule_id == "local-variable-naming")
        );
        assert!(diagnostics.iter().all(|diag| diag.rule_id != "parse-error"));
    }

    #[test]
    fn enabled_cfg_consumer_builds_cfg_and_keeps_its_diagnostic() {
        reset_cfg_builder_calls();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/resource_leak/bad_unprotected.pas");
        let source = std::fs::read(&path).unwrap();
        let file = FileInfo::new(path);
        let config = "version = 1".parse::<Config>().unwrap();

        let diagnostics = run_lint(&file, &source, &config);

        assert_eq!(cfg_builder_calls(), 1);
        assert!(
            diagnostics
                .iter()
                .any(|diag| diag.rule_id == "resource-leak-unprotected")
        );
    }

    #[test]
    fn explicitly_enabled_default_off_cfg_consumer_triggers_build() {
        reset_cfg_builder_calls();
        let file = FileInfo::new(PathBuf::from("Gate.pas"));
        let source = b"unit Gate;\ninterface\nimplementation\nend.\n";
        let config = format!("{NO_CFG_RULES_CONFIG}\nunchecked-nil = \"warning\"\n")
            .parse::<Config>()
            .unwrap();

        let _ = run_lint(&file, source, &config);

        assert_eq!(cfg_builder_calls(), 1);
    }

    #[test]
    fn dpr_and_dpk_naming_exclusions_match_dispatch_and_do_not_build_cfgs() {
        for extension in ["dpr", "dpk"] {
            reset_cfg_builder_calls();
            let file = FileInfo::new(PathBuf::from(format!("Gate.{extension}")));
            let source = b"unit Gate;\ninterface\nimplementation\nprocedure Run;\nvar badName: Integer;\nbegin\n  badName := 1;\nend;\nend.\n";
            let config = NO_CFG_RULES_CONFIG.parse::<Config>().unwrap();

            let diagnostics = run_lint(&file, source, &config);

            assert_eq!(cfg_builder_calls(), 0);
            assert!(
                diagnostics
                    .iter()
                    .all(|diag| diag.rule_id != "local-variable-naming")
            );
        }
    }

    #[test]
    fn default_off_cfg_consumer_does_not_trigger_build_until_enabled() {
        reset_cfg_builder_calls();
        let file = FileInfo::new(PathBuf::from("Gate.pas"));
        let source = b"unit Gate;\ninterface\nimplementation\nend.\n";
        let config = NO_CFG_RULES_CONFIG.parse::<Config>().unwrap();

        let _ = run_lint(&file, source, &config);

        assert_eq!(cfg_builder_calls(), 0);
    }

    #[test]
    fn file_local_mode_never_builds_cfgs() {
        reset_cfg_builder_calls();
        let file = FileInfo::new(PathBuf::from("Gate.pas"));
        let source = b"unit Gate;\ninterface\nimplementation\nend.\n";
        let config = "version = 1".parse::<Config>().unwrap();

        let _ = run_lint_file_local_rules(&file, source, &config, &RuleRegistry::new());

        assert_eq!(cfg_builder_calls(), 0);
    }
}

#[cfg(test)]
mod coordinate_tests {
    use super::{
        Diagnostic, LineStartIndex, Severity, line_start_index_work, map_diagnostics_to_original,
        reset_line_start_index_work,
    };
    use cfg_pascal::{ExpansionId, ProjectSourceId, SourceMap, SourceMapSegment, SourceSnapshot};

    fn byte_offset_at_position(source: &[u8], line: usize, column: usize) -> Option<usize> {
        LineStartIndex::new(source).byte_offset_at_position(line, column)
    }

    fn position_at_offset(source: &[u8], offset: usize) -> Option<(usize, usize)> {
        LineStartIndex::new(source).position_at_offset(offset)
    }

    #[test]
    fn byte_positions_are_lf_only_raw_byte_columns_and_may_cross_lines() {
        let source = b"a\nb\n";
        for (line, column, expected) in [
            (1, 1, Some(0)),
            (1, 2, Some(1)),
            (1, 3, Some(2)), // The old conversion does not clamp to line 1.
            (2, 2, Some(3)),
            (3, 1, Some(4)), // The trailing LF creates a final empty line.
        ] {
            assert_eq!(byte_offset_at_position(source, line, column), expected);
        }

        let crlf = b"a\r\nb";
        assert_eq!(byte_offset_at_position(crlf, 1, 2), Some(1)); // CR
        assert_eq!(byte_offset_at_position(crlf, 1, 3), Some(2)); // LF
        assert_eq!(byte_offset_at_position(crlf, 1, 4), Some(3)); // Crosses LF.
        assert_eq!(byte_offset_at_position(crlf, 2, 1), Some(3));

        let utf8 = "éx".as_bytes();
        assert_eq!(byte_offset_at_position(utf8, 1, 2), Some(1));
        assert_eq!(byte_offset_at_position(utf8, 1, 3), Some(2));

        let invalid_utf8 = b"\xff\x80\n";
        assert_eq!(byte_offset_at_position(invalid_utf8, 1, 3), Some(2));
        assert_eq!(byte_offset_at_position(invalid_utf8, 2, 1), Some(3));
    }

    #[test]
    fn byte_offsets_keep_lf_ownership_and_eof_line_semantics() {
        assert_eq!(position_at_offset(b"", 0), Some((1, 1)));

        let source = b"a\nb\n";
        for (offset, expected) in [
            (0, Some((1, 1))),
            (1, Some((1, 2))), // The LF belongs to the preceding line.
            (2, Some((2, 1))),
            (3, Some((2, 2))),
            (4, Some((3, 1))), // EOF after trailing LF is the final empty line.
        ] {
            assert_eq!(position_at_offset(source, offset), expected);
        }

        let crlf = b"a\r\nb";
        assert_eq!(position_at_offset(crlf, 1), Some((1, 2)));
        assert_eq!(position_at_offset(crlf, 2), Some((1, 3)));
        assert_eq!(position_at_offset(crlf, 3), Some((2, 1)));

        assert_eq!(position_at_offset("é".as_bytes(), 1), Some((1, 2)));
        assert_eq!(position_at_offset("é".as_bytes(), 2), Some((1, 3)));
        assert_eq!(position_at_offset(b"\xff\xfe\n", 2), Some((1, 3)));
        assert_eq!(position_at_offset(b"\xff\xfe\n", 3), Some((2, 1)));
    }

    #[test]
    fn byte_coordinate_conversions_reject_zero_and_out_of_range_values() {
        assert_eq!(byte_offset_at_position(b"", 0, 1), None);
        assert_eq!(byte_offset_at_position(b"", 1, 0), None);
        assert_eq!(byte_offset_at_position(b"", 1, 2), None);
        assert_eq!(byte_offset_at_position(b"a\n", 3, 1), None);
        assert_eq!(byte_offset_at_position(b"a\n", 1, 4), None);
        assert_eq!(position_at_offset(b"", 1), None);
        assert_eq!(position_at_offset(b"a", 2), None);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn byte_position_addition_keeps_debug_overflow_panic() {
        let _ = byte_offset_at_position(b"\n", 2, usize::MAX);
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn byte_position_addition_keeps_release_wrapping_result() {
        assert_eq!(byte_offset_at_position(b"\n", 2, usize::MAX), None);
    }

    fn diagnostic(line: usize, column: usize, end_line: usize, end_column: usize) -> Diagnostic {
        Diagnostic {
            rule_id: "test-rule".to_string(),
            severity: Severity::Warning,
            message: "test diagnostic".to_string(),
            line,
            column,
            end_line,
            end_column,
            help: None,
            scope: None,
        }
    }

    fn two_mib_source_with_dense_prefix_lines() -> Vec<u8> {
        let mut source = vec![b'x'; 2 * 1024 * 1024];
        for line in 1..=8_192 {
            source[line * 2 - 1] = b'\n';
        }
        source
    }

    fn identity_source_map(source: &[u8]) -> SourceMap {
        SourceMap::identity(SourceSnapshot::new(
            ProjectSourceId::new("large.pas"),
            source,
        ))
        .expect("identity source map")
    }

    #[test]
    fn empty_mapped_diagnostics_do_not_build_or_scan_coordinate_indices() {
        let source = two_mib_source_with_dense_prefix_lines();
        let source_map = identity_source_map(&source);
        let mut diagnostics = Vec::new();

        reset_line_start_index_work();
        assert!(map_diagnostics_to_original(
            &mut diagnostics,
            &source_map,
            &source,
        ));
        let work = line_start_index_work();

        assert_eq!(work.builds, 0);
        assert_eq!(work.bytes_scanned, 0);
        assert_eq!(work.offset_search_probes, 0);
    }

    #[test]
    fn sparse_out_of_order_diagnostics_reuse_only_the_needed_line_prefix() {
        const SOURCE_LEN: usize = 2 * 1024 * 1024;
        let mut source = vec![b'x'; SOURCE_LEN];
        source[..8].copy_from_slice(b"a\nb\nc\nd\n");
        let source_map = identity_source_map(&source);
        let mut diagnostics = vec![
            diagnostic(4, 1, 4, 2),
            diagnostic(2, 1, 2, 2),
            diagnostic(3, 1, 3, 2),
        ];

        reset_line_start_index_work();
        let started = std::time::Instant::now();
        assert!(map_diagnostics_to_original(
            &mut diagnostics,
            &source_map,
            &source,
        ));
        let elapsed = started.elapsed();
        let work = line_start_index_work();

        assert_eq!(work.builds, 2);
        // The first (line 4) query scans prepared bytes through its line
        // start (6); original offsets 6 and 7 extend the other prefix to 7.
        // Later line 2/3 queries move backward and reuse both cached prefixes.
        assert_eq!(work.bytes_scanned, 6 + 7);
        assert_eq!(work.line_starts_added, 3 + 3);
        assert!(work.offset_search_probes > 0);
        assert_eq!(
            diagnostics
                .iter()
                .map(|diag| (diag.line, diag.column, diag.end_line, diag.end_column))
                .collect::<Vec<_>>(),
            [(4, 1, 4, 2), (2, 1, 2, 2), (3, 1, 3, 2)]
        );
        eprintln!(
            "2 MiB sparse out-of-order SourceMap: mapped 3 diagnostics in {elapsed:?}; index builds={}, prefix bytes scanned={}, line starts added={}, offset binary-search probes={}",
            work.builds, work.bytes_scanned, work.line_starts_added, work.offset_search_probes,
        );
    }

    #[test]
    fn end_of_source_position_query_extends_the_prefix_to_eof() {
        const SOURCE_LEN: usize = 2 * 1024 * 1024;
        let source = two_mib_source_with_dense_prefix_lines();
        let source_map = identity_source_map(&source);
        // Deliberately use a line-1 column that crosses the intervening lines:
        // the old coordinate contract allows it when the total offset is valid.
        let mut diagnostics = vec![diagnostic(1, SOURCE_LEN, 1, SOURCE_LEN + 1)];

        reset_line_start_index_work();
        let started = std::time::Instant::now();
        assert!(map_diagnostics_to_original(
            &mut diagnostics,
            &source_map,
            &source,
        ));
        let elapsed = started.elapsed();
        let work = line_start_index_work();

        assert_eq!(work.builds, 2);
        // Position-to-byte on line 1 needs no scan; the original end offset is
        // EOF, so byte-to-position must scan all original bytes to find its line.
        assert_eq!(work.bytes_scanned, SOURCE_LEN);
        assert_eq!(work.line_starts_added, 8_192);
        assert_eq!(diagnostics[0].line, 8_193);
        assert_eq!(diagnostics[0].column, SOURCE_LEN - 16_384);
        assert_eq!(diagnostics[0].end_line, 8_193);
        assert_eq!(diagnostics[0].end_column, SOURCE_LEN - 16_384 + 1);
        eprintln!(
            "2 MiB EOF SourceMap: mapped 1 diagnostic in {elapsed:?}; index builds={}, prefix bytes scanned={}, line starts added={}, offset binary-search probes={}",
            work.builds, work.bytes_scanned, work.line_starts_added, work.offset_search_probes,
        );
    }

    #[test]
    fn source_map_failure_preserves_old_partial_mutation_order_for_fallback() {
        let source_id = ProjectSourceId::new("origin.pas");
        let source_map = SourceMap::new(
            b"abc\n",
            vec![SourceSnapshot::new(source_id.clone(), b"prefix abc\n")],
            vec![SourceMapSegment::copied(
                0..4,
                source_id,
                7..11,
                ExpansionId::root(),
            )],
        )
        .expect("valid copied source map");
        let mut diagnostics = vec![
            diagnostic(1, 1, 1, 2),
            diagnostic(99, 1, 99, 2),
            diagnostic(1, 2, 1, 3),
        ];

        assert!(!map_diagnostics_to_original(
            &mut diagnostics,
            &source_map,
            b"prefix abc\n"
        ));

        // The first diagnostic was mapped before the second rejected the
        // prepared coordinates. The caller discards this partial vector and
        // reruns against the original source; later diagnostics are untouched.
        assert_eq!(
            (
                diagnostics[0].line,
                diagnostics[0].column,
                diagnostics[0].end_line,
                diagnostics[0].end_column,
            ),
            (1, 8, 1, 9)
        );
        assert_eq!(
            (
                diagnostics[1].line,
                diagnostics[1].column,
                diagnostics[1].end_line,
                diagnostics[1].end_column,
            ),
            (99, 1, 99, 2)
        );
        assert_eq!(
            (
                diagnostics[2].line,
                diagnostics[2].column,
                diagnostics[2].end_line,
                diagnostics[2].end_column,
            ),
            (1, 2, 1, 3)
        );
    }

    #[test]
    fn maps_ten_thousand_diagnostics_through_a_two_mib_source_map() {
        const DIAGNOSTIC_COUNT: usize = 10_000;

        let prepared = two_mib_source_with_dense_prefix_lines();
        let source_map = identity_source_map(&prepared);
        let mut diagnostics = Vec::with_capacity(DIAGNOSTIC_COUNT);
        let mut legacy_coordinate_scan_bytes = 0usize;
        for index in 0..DIAGNOSTIC_COUNT {
            let line = 4_097 + index % 4_096;
            let line_start = (line - 1) * 2;
            // The old conversion loops visit line_start + 1 bytes for each
            // position-to-offset conversion, then offset bytes for each
            // offset-to-position conversion: exactly 4 * line_start + 3.
            legacy_coordinate_scan_bytes += 4 * line_start + 3;
            diagnostics.push(diagnostic(line, 1, line, 2));
        }

        reset_line_start_index_work();
        let started = std::time::Instant::now();
        assert!(map_diagnostics_to_original(
            &mut diagnostics,
            &source_map,
            prepared.as_slice(),
        ));
        let elapsed = started.elapsed();
        let work = line_start_index_work();

        assert_eq!(diagnostics.len(), DIAGNOSTIC_COUNT);
        assert_eq!(work.builds, 2, "prepared and original indices are reused");
        assert_eq!(work.bytes_scanned, (8_192 - 1) * 2 + ((8_192 - 1) * 2 + 1));
        assert_eq!(work.line_starts_added, 2 * (8_192 - 1));
        assert!(work.offset_search_probes > 0);
        // Rust 1.99's std probe measured 15 predicates for 8,193 entries;
        // keep a conservative bound rather than assuming 14.
        assert!(
            work.offset_search_probes <= DIAGNOSTIC_COUNT * 2 * 16,
            "each of two offset lookups per diagnostic should use at most 16 binary-search probes; observed {}",
            work.offset_search_probes,
        );
        assert!(diagnostics.iter().enumerate().all(|(index, diagnostic)| {
            diagnostic.line == 4_097 + index % 4_096
                && diagnostic.column == 1
                && diagnostic.end_line == diagnostic.line
                && diagnostic.end_column == 2
        }));
        eprintln!(
            "2 MiB identity SourceMap: mapped {DIAGNOSTIC_COUNT} diagnostics in {elapsed:?}; index builds={}, prefix bytes scanned={}, line starts added={}, offset binary-search probes={}, legacy conversion loop model visits {} bytes",
            work.builds,
            work.bytes_scanned,
            work.line_starts_added,
            work.offset_search_probes,
            legacy_coordinate_scan_bytes,
        );
    }
}
