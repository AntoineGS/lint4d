mod context;
pub mod suppress;

pub use context::{Diagnostic, FileInfo, FileType, Severity};

use crate::config::{Config, RuleSeverityOverride};
use crate::rules::{LintContext, RuleCategory, RuleRegistry};
use tree_sitter::Parser;

/// Parse Delphi source bytes and collect ERROR/MISSING nodes as diagnostics.
///
/// Returns `Ok((Tree, Vec<Diagnostic>))` on success, or `Err(String)` if the
/// parser fails to initialise or returns no tree.
pub fn parse_file(
    _info: &FileInfo,
    source: &[u8],
) -> Result<(tree_sitter::Tree, Vec<Diagnostic>), String> {
    let mut parser = Parser::new();
    let language = tree_sitter_pascal::LANGUAGE;
    parser
        .set_language(&language.into())
        .map_err(|e| format!("failed to set language: {e}"))?;

    let tree = parser
        .parse(source, None)
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

/// Run all lint rules on a single file and return sorted, filtered diagnostics.
///
/// This function:
/// 1. Parses the file and collects parse-error diagnostics
/// 2. Runs all enabled rules (respecting config overrides and file-type skipping)
/// 3. Applies severity overrides from config
/// 4. Filters out suppressed diagnostics
/// 5. Sorts results by line, then column
pub fn run_lint(file: &FileInfo, source: &[u8], config: &Config) -> Vec<Diagnostic> {
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

    let registry = RuleRegistry::new();
    let mut ctx = LintContext::new();

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

        rule.check(file, &tree, source, &mut ctx);
    }

    // Apply severity overrides from config.
    for diag in &mut ctx.diagnostics {
        if let Some(RuleSeverityOverride::Severity(s)) = config.rule_severity(&diag.rule_id) {
            diag.severity = s;
        }
    }

    // Merge parse-error diagnostics with rule diagnostics.
    diagnostics.append(&mut ctx.diagnostics);

    // Filter out suppressed diagnostics.
    let suppressions = suppress::parse_suppressions(source);
    diagnostics.retain(|diag| !suppressions.iter().any(|s| s.matches(&diag.rule_id, diag.line)));

    // Sort by line, then column.
    diagnostics.sort_by(|a, b| a.line.cmp(&b.line).then(a.column.cmp(&b.column)));

    diagnostics
}
