use crate::directive_fragment_rewrite::{
    DirectivePatch, rewrite_opaque_if_blocks, rewrite_partial_control_flow,
};
use crate::types::{Diagnostic, FileInfo, Severity};
use std::cell::RefCell;
use tree_sitter::{InputEdit, Parser, Point, Tree};

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
    info: &FileInfo,
    source: &[u8],
) -> Result<(tree_sitter::Tree, Vec<Diagnostic>, Vec<DirectivePatch>), String> {
    let parsed = parse_file_with_parser_source(info, source)?;
    Ok((parsed.tree, parsed.diagnostics, parsed.patches))
}

/// Parse Delphi source and also return the exact rewritten bytes consumed by
/// tree-sitter.
///
/// Most callers should use [`parse_file_with_patches`]. This variant is for a
/// caller retaining parse state for a later [`parse_file_incremental`] call.
pub fn parse_file_with_parser_source(
    _info: &FileInfo,
    source: &[u8],
) -> Result<ParseWithParserSourceResult, String> {
    let parsed = parse_fresh(source)?;
    Ok(ParseWithParserSourceResult {
        tree: parsed.tree,
        diagnostics: parsed.diagnostics,
        patches: parsed.patches,
        parser_source: parsed.parser_source,
    })
}

/// A fresh parse together with the rewritten bytes consumed by tree-sitter.
#[derive(Debug)]
pub struct ParseWithParserSourceResult {
    pub tree: Tree,
    pub diagnostics: Vec<Diagnostic>,
    pub patches: Vec<DirectivePatch>,
    pub parser_source: Vec<u8>,
}

/// The result of an incremental parse.
///
/// `parser_source` is the exact byte sequence consumed by tree-sitter after
/// the offset-preserving directive rewrites. Keeping it beside the tree lets a
/// caller describe the next edit in the same coordinate space without sharing
/// a mutable parser or mutating the published tree.
#[derive(Debug)]
pub struct IncrementalParseResult {
    pub tree: Tree,
    pub diagnostics: Vec<Diagnostic>,
    pub patches: Vec<DirectivePatch>,
    pub parser_source: Vec<u8>,
    /// Whether the caller-provided old tree was passed to tree-sitter. This is
    /// useful to instrument callers and tests; it does not claim that every
    /// node was reused by the parser.
    pub used_old_tree: bool,
}

/// Incrementally parse `source` using an old tree and the exact parser bytes
/// that produced that tree.
///
/// The source passed here is the same source accepted by
/// [`parse_file_with_patches`]. The old parser source may differ from it: the
/// function computes one conservative [`InputEdit`] after applying the same
/// directive rewrites to the new source. That keeps byte and `Point`
/// coordinates correct even when a changed edit crosses a rewritten region.
/// If the old tree's extent or a rewrite length invariant is not trustworthy,
/// this function falls back to a fresh parse rather than publishing an unsafe
/// incremental result.
pub fn parse_file_incremental(
    _info: &FileInfo,
    source: &[u8],
    old_parser_source: &[u8],
    old_tree: &Tree,
) -> Result<IncrementalParseResult, String> {
    let (phase1_source, mut patches) = rewrite_partial_control_flow(source);
    let phase1_source = phase1_source.into_owned();
    if phase1_source.len() != source.len() {
        let parsed = parse_fresh(source)?;
        return Ok(IncrementalParseResult {
            tree: parsed.tree,
            diagnostics: parsed.diagnostics,
            patches: parsed.patches,
            parser_source: parsed.parser_source,
            used_old_tree: false,
        });
    }
    let mut used_old_tree = false;

    let mut tree = match incremental_parse_candidate(&phase1_source, old_parser_source, old_tree) {
        Some((tree, reused)) => {
            used_old_tree = reused;
            tree
        }
        None => parse_bytes(&phase1_source, None)?,
    };
    let mut parser_source = phase1_source;

    // Phase 2 — opaque-{$IF} rewrite (Bucket F). Unlike the fresh path, probe
    // this pass even when the incremental tree reports no error: an edited old
    // tree can retain a clean shape for a newly-invalid opaque body. Applying
    // the same offset-preserving rewrite keeps that stale-tree case correct.
    let (phase2_source, patches_f) = rewrite_opaque_if_blocks(&parser_source);
    if !patches_f.is_empty() {
        let phase2_source = phase2_source.into_owned();
        if phase2_source.len() != parser_source.len() {
            let parsed = parse_fresh(source)?;
            return Ok(IncrementalParseResult {
                tree: parsed.tree,
                diagnostics: parsed.diagnostics,
                patches: parsed.patches,
                parser_source: parsed.parser_source,
                used_old_tree: false,
            });
        }
        let phase2_tree = incremental_parse_candidate(&phase2_source, &parser_source, &tree)
            .map(|(tree, _)| tree)
            .or_else(|| parse_bytes(&phase2_source, None).ok())
            .ok_or_else(|| "parser returned no tree".to_string())?;
        tree = phase2_tree;
        parser_source = phase2_source;
        patches.extend(patches_f);
    }

    let diagnostics = collect_parse_errors(&tree, source);
    Ok(IncrementalParseResult {
        tree,
        diagnostics,
        patches,
        parser_source,
        used_old_tree,
    })
}

#[derive(Debug)]
struct FreshParseResult {
    tree: Tree,
    diagnostics: Vec<Diagnostic>,
    patches: Vec<DirectivePatch>,
    parser_source: Vec<u8>,
}

fn parse_fresh(source: &[u8]) -> Result<FreshParseResult, String> {
    // Phase 1 — always-on partial-control-flow rewrite (Bucket C).
    let (phase1_source, mut patches) = rewrite_partial_control_flow(source);
    let mut parser_source = phase1_source.clone().into_owned();
    let mut tree = parse_bytes(&phase1_source, None)?;

    // Phase 2 — lazy opaque-{$IF} rewrite (Bucket F). Runs only when the
    // Phase 1 tree still has real errors.
    if has_real_error(tree.root_node()) {
        let (phase2_source, patches_f) = rewrite_opaque_if_blocks(&phase1_source);
        if !patches_f.is_empty() {
            parser_source = phase2_source.clone().into_owned();
            tree = parse_bytes(&phase2_source, None)?;
            patches.extend(patches_f);
        }
    }

    // Diagnostics are collected against the ORIGINAL source so error
    // messages show the original bytes, not the whitespaced rewrites.
    // Both rewriters preserve byte offsets, so positions stay valid.
    let diagnostics = collect_parse_errors(&tree, source);
    Ok(FreshParseResult {
        tree,
        diagnostics,
        patches,
        parser_source,
    })
}

fn parse_bytes(source: &[u8], old_tree: Option<&Tree>) -> Result<Tree, String> {
    PARSER
        .with(|parser| parser.borrow_mut().parse(source, old_tree))
        .ok_or_else(|| "parser returned no tree".to_string())
}

fn incremental_parse_candidate(
    new_source: &[u8],
    old_source: &[u8],
    old_tree: &Tree,
) -> Option<(Tree, bool)> {
    if old_tree.root_node().end_byte() != old_source.len() {
        return None;
    }
    let mut edited_tree = old_tree.clone();
    if let Some(edit) = input_edit(old_source, new_source) {
        edited_tree.edit(&edit);
    }
    parse_bytes(new_source, Some(&edited_tree))
        .ok()
        .map(|tree| (tree, true))
}

fn input_edit(old_source: &[u8], new_source: &[u8]) -> Option<InputEdit> {
    if old_source == new_source {
        return None;
    }

    let mut start = 0;
    let common_prefix = old_source.len().min(new_source.len());
    while start < common_prefix && old_source[start] == new_source[start] {
        start += 1;
    }

    let mut old_end = old_source.len();
    let mut new_end = new_source.len();
    while old_end > start && new_end > start && old_source[old_end - 1] == new_source[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }

    Some(InputEdit {
        start_byte: start,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: point_at(old_source, start),
        old_end_position: point_at(old_source, old_end),
        new_end_position: point_at(new_source, new_end),
    })
}

fn point_at(source: &[u8], byte: usize) -> Point {
    let mut row = 0;
    let mut line_start = 0;
    for (index, value) in source[..byte].iter().enumerate() {
        if *value == b'\n' {
            row += 1;
            line_start = index + 1;
        }
    }
    Point {
        row,
        column: byte - line_start,
    }
}

/// Walk the tree and emit a `Diagnostic` for every ERROR or MISSING node.
fn collect_parse_errors(tree: &tree_sitter::Tree, source: &[u8]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    visit_node(tree.root_node(), source, &mut diagnostics);
    diagnostics
}

fn visit_node(root: tree_sitter::Node, source: &[u8], out: &mut Vec<Diagnostic>) {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.is_error() || node.is_missing() {
            // Skip bare `raise;` ERROR nodes — tree-sitter-pascal does not
            // recognise standalone `raise` (re-raise) as valid syntax, but
            // it is perfectly legal Delphi. The error node contains a single
            // `kRaise` child.
            if node.is_error() && is_bare_raise_error(node) {
                continue;
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
            continue;
        }

        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
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
fn has_real_error(root: tree_sitter::Node) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.is_error() || node.is_missing() {
            if node.is_error() && is_bare_raise_error(node) {
                continue;
            }
            return true;
        }

        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
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

    #[test]
    fn incremental_parse_reuses_old_tree_for_an_unchanged_prefix() {
        let old_source = b"unit X;\ninterface\nconst Stable = 1;\nimplementation\nprocedure Changed;\nbegin\n  Stable := 2;\nend;\nend.\n";
        let new_source = b"unit X;\ninterface\nconst Stable = 1;\nimplementation\nprocedure Changed;\nbegin\n  Stable := 3;\nend;\nend.\n";
        let (old_tree, _, _) = parse_file_with_patches(&info(), old_source).expect("old parse");
        let fresh = parse_file_with_patches(&info(), new_source).expect("fresh parse");
        let incremental = parse_file_incremental(&info(), new_source, old_source, &old_tree)
            .expect("incremental parse");

        assert!(
            incremental.used_old_tree,
            "the old tree must reach tree-sitter"
        );
        assert_eq!(
            incremental.tree.root_node().to_sexp(),
            fresh.0.root_node().to_sexp(),
            "incremental and fresh trees must be structurally equivalent"
        );
        assert_eq!(
            format!("{:?}", incremental.diagnostics),
            format!("{:?}", fresh.1),
            "incremental and fresh diagnostics must match"
        );
        assert_eq!(incremental.patches, fresh.2);

        let old_stable = old_tree
            .root_node()
            .named_descendant_for_byte_range(5, 6)
            .expect("stable prefix node in old tree");
        let incremental_stable = incremental
            .tree
            .root_node()
            .named_descendant_for_byte_range(5, 6)
            .expect("stable prefix node in incremental tree");
        assert_eq!(
            old_stable.id(),
            incremental_stable.id(),
            "an unchanged prefix node should be reused rather than rebuilt"
        );
    }

    #[test]
    fn incremental_parse_preserves_unicode_crlf_and_directive_coordinates() {
        let old_source = b"unit X;\r\ninterface\r\n// stable \xF0\x9F\x98\x80\r\nconst Stable = 1;\r\nimplementation\r\nif Ready then\r\n{$IFDEF FEATURE}\r\n  DoThing;\r\n{$ENDIF}\r\nend.\r\n";
        let new_source = b"unit X;\r\ninterface\r\n// stable \xF0\x9F\x98\x80\r\nconst Stable = 2;\r\nimplementation\r\nif Ready then\r\n{$IFDEF FEATURE}\r\n  DoThing;\r\n{$ENDIF}\r\nend.\r\n";
        let (old_tree, _, old_patches) =
            parse_file_with_patches(&info(), old_source).expect("old parse");
        let fresh = parse_file_with_patches(&info(), new_source).expect("fresh parse");
        let incremental = parse_file_incremental(&info(), new_source, old_source, &old_tree)
            .expect("incremental parse");

        assert!(incremental.used_old_tree);
        assert_eq!(
            incremental.tree.root_node().to_sexp(),
            fresh.0.root_node().to_sexp()
        );
        assert_eq!(
            format!("{:?}", incremental.diagnostics),
            format!("{:?}", fresh.1)
        );
        assert_eq!(incremental.patches, fresh.2);
        assert_eq!(old_patches, incremental.patches);
        let old_stable = old_tree
            .root_node()
            .named_descendant_for_byte_range(5, 6)
            .expect("stable unit node in old tree");
        let new_stable = incremental
            .tree
            .root_node()
            .named_descendant_for_byte_range(5, 6)
            .expect("stable unit node in new tree");
        assert_eq!(old_stable.id(), new_stable.id());
    }

    #[test]
    fn incremental_parse_reuses_state_across_the_opaque_directive_fallback() {
        let old_source = b"unit X;\ninterface\nimplementation\n{$IF DEFINED(X)}\nrappel: developper en 32 bits pour plus de stabilite\n{$IFEND}\nconst Stable = 1;\nend.\n";
        let new_source = b"unit X;\ninterface\nimplementation\n{$IF DEFINED(X)}\nrappel: developper en 32 bits pour plus de stabilite\n{$IFEND}\nconst Stable = 2;\nend.\n";
        let (old_tree, _, old_patches) =
            parse_file_with_patches(&info(), old_source).expect("old parse");
        assert!(
            old_patches
                .iter()
                .any(|patch| matches!(patch, DirectivePatch::OpaqueBlock(_)))
        );
        let fresh = parse_file_with_patches(&info(), new_source).expect("fresh parse");
        let incremental = parse_file_incremental(&info(), new_source, old_source, &old_tree)
            .expect("incremental parse");

        assert!(incremental.used_old_tree);
        assert_eq!(
            incremental.tree.root_node().to_sexp(),
            fresh.0.root_node().to_sexp()
        );
        assert_eq!(incremental.patches, fresh.2);
    }

    #[test]
    fn incremental_parse_falls_back_when_old_tree_extent_is_untrusted() {
        let old_source = b"unit X;\ninterface\nimplementation\nend.\n";
        let new_source = b"unit X;\ninterface\nimplementation\nconst Added = 1;\nend.\n";
        let (old_tree, _, _) = parse_file_with_patches(&info(), old_source).expect("old parse");
        let fresh = parse_file_with_patches(&info(), new_source).expect("fresh parse");
        let incremental =
            parse_file_incremental(&info(), new_source, b"not-the-old-parser-source", &old_tree)
                .expect("fallback parse");

        assert!(!incremental.used_old_tree);
        assert_eq!(
            incremental.tree.root_node().to_sexp(),
            fresh.0.root_node().to_sexp()
        );
        assert_eq!(incremental.patches, fresh.2);
    }

    #[test]
    fn incremental_parse_matches_fresh_across_insert_delete_and_repair_sequences() {
        let sources: &[&[u8]] = &[
            b"unit X;\ninterface\nconst Stable = 1;\nimplementation\nprocedure Changed;\nbegin\n  Stable := 2;\nend;\nend.\n",
            b"unit X;\ninterface\nconst Stable = 1;\nconst Inserted = 8;\nimplementation\nprocedure Changed;\nbegin\n  Stable := 2;\nend;\nend.\n",
            b"unit X;\ninterface\nconst Stable = 1;\nimplementation\nprocedure Changed;\nbegin\n  Stable := 20;\nend;\nend.\n",
            b"unit X;\ninterface\nconst Stable = 1;\nimplementation\nprocedure Changed;\nvar\n  Local: Integer;\nbegin\n  Local := 3;\n  Stable :=\n    Local + 1;\nend;\nend.\n",
            b"unit X;\ninterface\nconst Stable = ;\nimplementation\nprocedure Changed;\nbegin\n  Stable :=\n    Local + 1;\nend;\nend.\n",
            b"unit X;\ninterface\nconst Stable = 4;\nimplementation\nprocedure Changed;\nvar\n  Local: Integer;\nbegin\n  Local := 3;\n  Stable :=\n    Local + 1;\nend;\nend.\n",
        ];
        let initial = parse_file_with_parser_source(&info(), sources[0]).expect("initial parse");
        let mut old_tree = initial.tree;
        let mut old_parser_source = initial.parser_source;

        for (step, source) in sources.iter().enumerate().skip(1) {
            let fresh = parse_file_with_parser_source(&info(), source).expect("fresh parse");
            let incremental =
                parse_file_incremental(&info(), source, &old_parser_source, &old_tree)
                    .expect("incremental parse");

            assert!(
                incremental.used_old_tree,
                "step {step} should use the old tree"
            );
            assert_eq!(
                incremental.tree.root_node().to_sexp(),
                fresh.tree.root_node().to_sexp(),
                "step {step} tree differs from a fresh parse"
            );
            assert_eq!(
                format!("{:?}", incremental.diagnostics),
                format!("{:?}", fresh.diagnostics),
                "step {step} diagnostics differ from a fresh parse"
            );
            assert_eq!(
                incremental.patches, fresh.patches,
                "step {step} patches differ"
            );
            assert_eq!(
                incremental.parser_source, fresh.parser_source,
                "step {step} parser input differs"
            );

            old_tree = incremental.tree;
            old_parser_source = incremental.parser_source;
        }
    }
}
