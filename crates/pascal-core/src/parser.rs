use crate::directive_fragment_rewrite::{DirectivePatch, rewrite_partial_control_flow};
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
///
/// This function runs the directive-fragment rewrite pass before parsing
/// but discards the patches. Callers that need the patches should use
/// [`parse_file_with_patches`] instead.
pub fn parse_file(
    info: &FileInfo,
    source: &[u8],
) -> Result<(tree_sitter::Tree, Vec<Diagnostic>), String> {
    let (tree, diagnostics, _patches) = parse_file_with_patches(info, source)?;
    Ok((tree, diagnostics))
}

/// Parse Delphi source bytes, running the directive-fragment rewrite pass
/// first. Returns the parsed tree, diagnostics, and the list of directive
/// patches that were rewritten.
///
/// Callers that don't need patches should use [`parse_file`] instead.
pub fn parse_file_with_patches(
    _info: &FileInfo,
    source: &[u8],
) -> Result<(tree_sitter::Tree, Vec<Diagnostic>, Vec<DirectivePatch>), String> {
    let (rewritten, patches) = rewrite_partial_control_flow(source);
    let tree = PARSER
        .with(|parser| {
            let mut parser = parser.borrow_mut();
            parser.parse(&*rewritten, None)
        })
        .ok_or_else(|| "parser returned no tree".to_string())?;

    // Diagnostics are collected against the ORIGINAL source so error messages
    // show the original bytes, not the whitespaced rewrite.
    let diagnostics = collect_parse_errors(&tree, source);
    Ok((tree, diagnostics, patches))
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
        let snippet: String = crate::text::decode_bytes(&source[node.start_byte()..byte_end])
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

/// Extract the text of a tree-sitter node from the source bytes.
///
/// Tolerates non-UTF-8 source by decoding the slice as Latin-1 / ISO-8859-1
/// (see [`crate::text::decode_bytes`]). Legacy Delphi codebases are commonly
/// Windows-1252 encoded; this keeps accented identifiers, comments and string
/// literals from silently becoming empty.
pub fn node_text(node: tree_sitter::Node, source: &[u8]) -> String {
    crate::text::decode_bytes(&source[node.start_byte()..node.end_byte()]).into_owned()
}
