use crate::directive_fragment_rewrite::{
    DirectivePatch, rewrite_opaque_if_blocks, rewrite_partial_control_flow,
};
use crate::types::{Diagnostic, FileInfo, Severity};
use std::borrow::Cow;
use std::cell::RefCell;
use tree_sitter::Parser;

thread_local! {
    pub(crate) static PARSER: RefCell<Parser> = RefCell::new({
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
    // Phase 1 — always-on partial-control-flow rewrite (Bucket C).
    let (phase1_source, mut patches) = rewrite_partial_control_flow(source);

    let mut tree = PARSER
        .with(|parser| {
            let mut parser = parser.borrow_mut();
            parser.parse(&*phase1_source, None)
        })
        .ok_or_else(|| "parser returned no tree".to_string())?;

    // Phase 2 — lazy opaque-{$IF} rewrite (Bucket F). Runs only when the
    // Phase 1 tree still has real errors.
    if has_real_error(tree.root_node()) {
        let (phase2_source, patches_f) = rewrite_opaque_if_blocks(&phase1_source);
        if !patches_f.is_empty() {
            let phase2_owned: Cow<[u8]> = Cow::Owned(phase2_source.into_owned());
            tree = PARSER
                .with(|parser| {
                    let mut parser = parser.borrow_mut();
                    parser.parse(&*phase2_owned, None)
                })
                .ok_or_else(|| "parser returned no tree".to_string())?;
            patches.extend(patches_f);
        }
    }

    // Diagnostics are collected against the ORIGINAL source so error
    // messages show the original bytes, not the whitespaced rewrites.
    // Both rewriters preserve byte offsets, so positions stay valid.
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

/// Returns true iff the tree contains at least one ERROR or MISSING node
/// that is *not* a bare `raise;` false-positive. This is the Phase 2
/// fallback gate in `parse_file_with_patches`: if `has_real_error` returns
/// true, we rerun the source through `rewrite_opaque_if_blocks` and reparse.
fn has_real_error(node: tree_sitter::Node) -> bool {
    if node.is_error() || node.is_missing() {
        if node.is_error() && is_bare_raise_error(node) {
            return false;
        }
        return true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if has_real_error(child) {
            return true;
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

#[cfg(test)]
mod has_real_error_tests {
    use super::*;

    fn parse_raw(source: &[u8]) -> tree_sitter::Tree {
        PARSER
            .with(|p| p.borrow_mut().parse(source, None))
            .expect("parser must produce a tree")
    }

    #[test]
    fn has_real_error_false_for_clean_source() {
        let tree = parse_raw(b"unit X;\ninterface\nimplementation\nend.\n");
        assert!(!has_real_error(tree.root_node()));
    }

    #[test]
    fn has_real_error_true_for_bucket_f_shape() {
        let tree = parse_raw(
            b"unit X;\ninterface\nimplementation\n\
              {$IF DEFINED(X)}\nrappel: developper en 32 bits pour plus de stabilite\n{$IFEND}\nend.\n",
        );
        assert!(has_real_error(tree.root_node()));
    }

    #[test]
    fn has_real_error_false_for_bare_raise_only() {
        // `raise;` is a known false-positive ERROR node that
        // is_bare_raise_error filters out; has_real_error must do the same.
        let tree = parse_raw(
            b"unit X;\ninterface\nimplementation\n\
              procedure P; begin raise; end;\nend.\n",
        );
        assert!(
            !has_real_error(tree.root_node()),
            "bare `raise;` must not count as a real error"
        );
    }
}

#[cfg(test)]
mod parse_with_patches_tests {
    use super::*;
    use crate::types::FileInfo;
    use std::path::PathBuf;

    fn info() -> FileInfo {
        FileInfo::new(PathBuf::from("test.pas"))
    }

    #[test]
    fn clean_source_produces_no_patches() {
        let src = b"unit X;\ninterface\nimplementation\nend.\n";
        let (_tree, diags, patches) = parse_file_with_patches(&info(), src).expect("parse ok");
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
        assert!(patches.is_empty(), "expected no patches, got {patches:?}");
    }

    #[test]
    fn bucket_f_file_parses_cleanly_after_phase_2_rewrite() {
        let src = b"unit X;\ninterface\nimplementation\n\
                    {$IF DEFINED(WIN32) AND NOT DEFINED(UNITTEST)}\n\
                    rappel: developper en 32 bits pour plus de stabilite\n\
                    {$IFEND}\nend.\n";
        let (_tree, diags, patches) = parse_file_with_patches(&info(), src).expect("parse ok");
        assert_eq!(
            diags.len(),
            0,
            "Phase 2 rewrite must clear all diagnostics, got {diags:?}"
        );
        assert_eq!(patches.len(), 1, "one opaque-block patch expected");
        let o = patches[0].expect_opaque();
        assert!(o.text.contains("rappel: developper"));
    }

    #[test]
    fn clean_source_with_valid_if_skips_phase_2() {
        // Source with a valid {$IF} block must not trigger Phase 2.
        // We can't directly observe "Phase 2 didn't run", but we can assert
        // that no OpaqueBlock patches were produced and no Markers either.
        let src = b"unit X;\ninterface\n\
                    {$IF VERSION >= 28}\nconst X = 1;\n{$IFEND}\n\
                    implementation\nend.\n";
        let (_tree, diags, patches) = parse_file_with_patches(&info(), src).expect("parse ok");
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
        assert!(patches.is_empty(), "expected no patches, got {patches:?}");
    }
}
