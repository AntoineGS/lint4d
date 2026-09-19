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
    // Build per-method CFGs only when a CFG rule will consume them.  Project
    // build failures in that mode are surfaced as one bounded engine
    // diagnostic; file-local-only mode must not manufacture a CFG diagnostic
    // while deliberately withholding uncertain flow conclusions.
    let cfg_map = if run_cfg_rules {
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
                let proc_id = ProcId::new(&unit_name, &cfg.proc_name);
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

        // Skip rules that are off by default and not explicitly enabled in config.
        if !meta.enabled_by_default && config.rule_severity(meta.id).is_none() {
            continue;
        }

        // Skip rules that are explicitly turned off.
        if let Some(RuleSeverityOverride::Off) = config.rule_severity(meta.id) {
            continue;
        }

        // Skip naming rules for .dpr/.dpk files (project/package files).
        if matches!(file.file_type, FileType::Dpr | FileType::Dpk)
            && matches!(meta.category, RuleCategory::NamingConvention)
        {
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
    if let Some(map) = prepared_map {
        if !map_diagnostics_to_original(&mut ctx.diagnostics, &map, source) {
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
    for diagnostic in diagnostics {
        let Some(start) = byte_offset_at_position(
            source_map.prepared_bytes(),
            diagnostic.line,
            diagnostic.column,
        ) else {
            return false;
        };
        let Some(end) = byte_offset_at_position(
            source_map.prepared_bytes(),
            diagnostic.end_line,
            diagnostic.end_column,
        ) else {
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
        let Some((line, column)) = position_at_offset(original_source, original_range.start) else {
            return false;
        };
        let Some((end_line, end_column)) = position_at_offset(original_source, original_range.end)
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

fn byte_offset_at_position(source: &[u8], line: usize, column: usize) -> Option<usize> {
    if line == 0 || column == 0 {
        return None;
    }
    let mut current_line = 1;
    let mut line_start = 0;
    for (index, byte) in source.iter().enumerate() {
        if current_line == line {
            break;
        }
        if *byte == b'\n' {
            current_line += 1;
            line_start = index + 1;
        }
    }
    if current_line != line {
        return None;
    }
    let offset = line_start + column - 1;
    (offset <= source.len()).then_some(offset)
}

fn position_at_offset(source: &[u8], offset: usize) -> Option<(usize, usize)> {
    if offset > source.len() {
        return None;
    }
    let mut line = 1;
    let mut line_start = 0;
    for (index, byte) in source.iter().enumerate().take(offset) {
        if *byte == b'\n' {
            line += 1;
            line_start = index + 1;
        }
    }
    Some((line, offset - line_start + 1))
}

fn paths_equivalent(left: &Path, right: &Path) -> bool {
    lexical_absolute(left) == lexical_absolute(right)
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
    if node.kind() == K::DEF_PROC {
        if let Some(name) = extract_proc_display_name(node, source) {
            out.push(ProcScope {
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                name,
            });
        }
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
