mod context;
pub mod suppress;

pub use context::{Diagnostic, FileInfo, FileType, Severity};

use crate::cfg::analysis::AnalysisContext;
use crate::config::{Config, RuleSeverityOverride};
use crate::dcu::ProjectContext;
use crate::rules::helpers::extract_unit_name;
use crate::rules::{LintContext, RuleCategory, RuleRegistry};
use cfg_core::call_graph::CallGraph;
use cfg_core::summary::ProcId;
use cfg_pascal::build_file_cfgs;
use std::cell::RefCell;
use std::collections::HashMap;
use tree_sitter::Parser;

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        let language = tree_sitter_pascal::LANGUAGE;
        p.set_language(&language.into()).expect("failed to set pascal language");
        p
    });
}

/// Parse Delphi source bytes and collect ERROR/MISSING nodes as diagnostics.
///
/// Returns `Ok((Tree, Vec<Diagnostic>))` on success, or `Err(String)` if the
/// parser fails to initialise or returns no tree.
pub fn parse_file(
    _info: &FileInfo,
    source: &[u8],
) -> Result<(tree_sitter::Tree, Vec<Diagnostic>), String> {
    let tree = PARSER
        .with(|parser| {
            let mut parser = parser.borrow_mut();
            parser.parse(source, None)
        })
        .ok_or_else(|| "parser returned no tree".to_string())?;

    let diagnostics = collect_parse_errors(&tree, source);
    Ok((tree, diagnostics))
}

/// Walk the tree and emit a `Diagnostic` for every ERROR or MISSING node.
fn collect_parse_errors(tree: &tree_sitter::Tree, source: &[u8]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    visit_node(tree.root_node(), source, &mut diagnostics);
    diagnostics
}

fn visit_node(node: tree_sitter::Node, source: &[u8], out: &mut Vec<Diagnostic>) {
    if node.is_error() || node.is_missing() {
        // Skip bare `raise;` ERROR nodes — tree-sitter-pascal does not
        // recognise standalone `raise` (re-raise) as valid syntax, but
        // it is perfectly legal Delphi. The error node contains a single
        // `kRaise` child.
        if node.is_error() && is_bare_raise_error(node) {
            return;
        }

        let start = node.start_position();
        let end = node.end_position();

        let byte_end = node.end_byte().min(node.start_byte() + 40);
        let snippet: String = std::str::from_utf8(&source[node.start_byte()..byte_end])
            .unwrap_or("")
            .chars()
            .take(40)
            .collect();

        let message = if node.is_missing() {
            format!("missing syntax near {:?}", snippet)
        } else {
            format!("unexpected token {:?}", snippet)
        };

        out.push(Diagnostic {
            rule_id: "parse-error".to_string(),
            severity: Severity::Warning,
            message,
            line: start.row + 1,
            column: start.column + 1,
            end_line: end.row + 1,
            end_column: end.column + 1,
            help: None,
        });

        // Don't descend into error nodes to avoid duplicate diagnostics.
        return;
    }

    for child in node.children(&mut node.walk()) {
        visit_node(child, source, out);
    }
}

/// Check whether an ERROR node represents a bare `raise;` statement.
///
/// tree-sitter-pascal does not support standalone `raise` (re-raise the
/// current exception). The ERROR node in this case contains a single
/// `kRaise` child.
fn is_bare_raise_error(node: tree_sitter::Node) -> bool {
    if node.child_count() == 1 {
        if let Some(child) = node.child(0) {
            return child.kind() == "kRaise";
        }
    }
    false
}

/// Run all lint rules on a single file and return sorted, filtered diagnostics.
///
/// This is a convenience wrapper that creates a default [`RuleRegistry`] and
/// delegates to [`run_lint_with_context`] with no project context.
pub fn run_lint(file: &FileInfo, source: &[u8], config: &Config) -> Vec<Diagnostic> {
    let registry = RuleRegistry::new();
    run_lint_with_context(file, source, config, None, None, &registry)
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
    source_ctx: Option<&crate::source_context::SourceContext>,
    registry: &RuleRegistry,
) -> Vec<Diagnostic> {
    let (tree, mut diagnostics) = match parse_file(file, source) {
        Ok(result) => result,
        Err(e) => {
            return vec![Diagnostic {
                rule_id: "lint4d-error".to_string(),
                severity: Severity::Error,
                message: e,
                line: 1,
                column: 1,
                end_line: 1,
                end_column: 1,
                help: None,
            }];
        }
    };

    // Build per-method CFGs from the parsed tree.
    let unit_name = extract_unit_name(tree.root_node(), source).unwrap_or_default();
    let file_cfgs = build_file_cfgs(&tree, source);
    let cfg_map: HashMap<ProcId, _> = file_cfgs
        .into_iter()
        .map(|cfg| {
            let proc_id = ProcId::new(&unit_name, &cfg.proc_name);
            (proc_id, cfg)
        })
        .collect();

    let default_project = ProjectContext::from_units(vec![]);
    let proj_ref = project.unwrap_or(&default_project);
    let analysis = AnalysisContext::new(cfg_map, CallGraph::new(), proj_ref);

    let mut ctx = match source_ctx {
        Some(sc) => LintContext::with_source_ctx(sc),
        None => LintContext::new(),
    };

    for rule in registry.all_rules() {
        let meta = rule.meta();

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
            rule.check_cfg(file, &tree, source, config, &analysis, &mut ctx);
        } else if rule.requires_context() {
            match project {
                Some(proj) => {
                    rule.check_with_context(file, &tree, source, config, proj, &mut ctx);
                }
                None => {
                    // Skip context-dependent rules when no project context is available.
                }
            }
        } else {
            rule.check(file, &tree, source, config, &mut ctx);
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
