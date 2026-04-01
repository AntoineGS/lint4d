use crate::types::{Diagnostic, FileInfo, Severity};
use std::cell::RefCell;
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
            scope: None,
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
            return child.kind() == crate::node_kind::K_RAISE;
        }
    }
    false
}

/// Extract the UTF-8 text of a tree-sitter node from the source bytes.
pub fn node_text(node: tree_sitter::Node, source: &[u8]) -> String {
    std::str::from_utf8(&source[node.start_byte()..node.end_byte()])
        .unwrap_or("")
        .to_string()
}
