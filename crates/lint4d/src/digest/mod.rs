//! Per-routine control-flow digests for coding agents (l4d-7mw.1).

pub mod lifecycle;
pub mod model;
pub mod render;
pub mod select;
pub mod tree;

pub use model::*;

use std::ops::Range;
use std::path::Path;

use cfg_pascal::build_file_cfgs;
use cfg_pascal::cfg_core::Cfg;
use pascal_core::FileInfo;
use pascal_core::directive_fragment_rewrite::DirectivePatch;
use pascal_core::parser::{node_text, parse_file_with_parser_source};
use tree_sitter::Node;

/// Parse `source` the way lint does and digest every routine.
///
/// Byte offsets of the rewritten parser source equal the original ones, so
/// line numbers refer to the file as written.
pub fn digest_source(path: &Path, source: &[u8]) -> Result<FileDigest, String> {
    let parsed = parse_file_with_parser_source(&FileInfo::new(path.to_path_buf()), source)?;
    let text = parsed.parser_source.as_slice();
    let lines = LineIndex::new(source);
    let hidden: Vec<Range<usize>> = parsed
        .patches
        .iter()
        .filter_map(|patch| match patch {
            DirectivePatch::OpaqueBlock(block) => Some(block.start..block.end),
            DirectivePatch::Markers(_) => None,
        })
        .collect();
    let root = parsed.tree.root_node();
    let routines = build_file_cfgs(&parsed.tree, text)
        .iter()
        .map(|cfg| digest_routine(root, cfg, text, &lines, &hidden))
        .collect();
    Ok(FileDigest {
        path: path.display().to_string(),
        routines,
    })
}

fn digest_routine(
    root: Node<'_>,
    cfg: &Cfg,
    text: &[u8],
    lines: &LineIndex,
    hidden: &[Range<usize>],
) -> RoutineDigest {
    let range = cfg.byte_range.clone();
    let scope = scope_node(root, &range);
    let mut degraded = Vec::new();
    let errors = scope
        .map(|node| error_lines(node, lines))
        .unwrap_or_default();
    let parse_errors = !errors.is_empty();
    let first_error_line = errors.first().copied();
    if parse_errors {
        degraded.push(Degradation::ParseErrors { lines: errors });
    }
    let (body, frequent_calls, stats) = match scope {
        Some(node) => tree::build(node, cfg, text, lines),
        None => (Vec::new(), Vec::new(), tree::Stats::default()),
    };
    if stats.conditional_regions > 0 {
        degraded.push(Degradation::Conditional {
            regions: stats.conditional_regions,
        });
    }
    let hidden: Vec<Range<usize>> = hidden
        .iter()
        .filter(|block| block.start < range.end && range.start < block.end)
        .cloned()
        .collect();
    degraded.extend(hidden.iter().map(|block| Degradation::HiddenDirective {
        line: lines.line(block.start),
    }));
    let end_line = lines.line(range.end.saturating_sub(1).max(range.start));
    let context = lifecycle::Context {
        parse_errors,
        hidden: &hidden,
        first_error_line,
        end_line,
    };
    let objects = scope
        .map(|node| lifecycle::verdicts(node, cfg, text, lines, &context))
        .unwrap_or_default();
    if stats.lambdas > 0 {
        degraded.push(Degradation::AnonymousMethods);
    }
    RoutineDigest {
        name: cfg.proc_name.clone(),
        start_line: lines.line(range.start),
        end_line,
        byte_range: range,
        objects,
        frequent_calls,
        body,
        degraded,
    }
}

/// The node whose byte range is exactly the CFG's routine range: a
/// `defProc`, a program `block`, or an `initialization`/`finalization` section.
fn scope_node<'t>(root: Node<'t>, range: &Range<usize>) -> Option<Node<'t>> {
    let mut node = root.descendant_for_byte_range(range.start, range.end)?;
    loop {
        if node.byte_range() == *range {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// Lines of the first five ERROR/MISSING nodes inside `scope`.
fn error_lines(scope: Node<'_>, lines: &LineIndex) -> Vec<usize> {
    let mut found = Vec::new();
    let mut stack = vec![scope];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            let line = lines.line(node.start_byte());
            if !found.contains(&line) {
                found.push(line);
            }
            continue;
        }
        if node.has_error() {
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
    }
    found.sort_unstable();
    found.truncate(5);
    found
}

/// Named children that are not extras (comments).
pub fn named_children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !child.is_extra())
        .collect()
}

pub fn field_children<'t>(node: Node<'t>, field: &str) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.children_by_field_name(field, &mut cursor).collect()
}

/// Keyword tokens such as `kBegin`, `kElse` are named nodes in this grammar.
pub fn is_keyword(node: Node<'_>) -> bool {
    let kind = node.kind().as_bytes();
    kind.len() > 1 && kind[0] == b'k' && kind[1].is_ascii_uppercase()
}

/// `Exit;` or `Exit(value);`.
pub fn is_exit_statement(node: Node<'_>, source: &[u8]) -> bool {
    if node.kind() != "statement" {
        return false;
    }
    let children = named_children(node);
    let [child] = children.as_slice() else {
        return false;
    };
    let callee = match child.kind() {
        "identifier" => *child,
        "exprCall" => match child.child_by_field_name("entity") {
            Some(entity) if entity.kind() == "identifier" => entity,
            _ => return false,
        },
        _ => return false,
    };
    node_text(callee, source).eq_ignore_ascii_case("exit")
}

/// Display text of an assignment target; an inline `var X` yields `X`.
pub fn target_name(lhs: Node<'_>, source: &[u8]) -> String {
    let node = if lhs.kind() == "varAssignDef" {
        named_children(lhs)
            .into_iter()
            .find(|child| child.kind() == "identifier")
            .unwrap_or(lhs)
    } else {
        lhs
    };
    normalize(&node_text(node, source))
}

/// 1-based line lookup over source bytes. CRLF and LF both end at `\n`.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(source: &[u8]) -> Self {
        let mut starts = vec![0];
        starts.extend(
            source
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'\n')
                .map(|(index, _)| index + 1),
        );
        Self { starts }
    }

    pub fn line(&self, byte: usize) -> usize {
        match self.starts.binary_search(&byte) {
            Ok(index) => index + 1,
            Err(index) => index,
        }
    }
}

/// Collapse every whitespace run (including CR and LF) to one space.
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut `text` to `max` characters followed by `…`.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index_is_one_based_for_lf_and_crlf() {
        let lf = LineIndex::new(b"a\nbc\nd");
        assert_eq!(
            (lf.line(0), lf.line(2), lf.line(3), lf.line(5)),
            (1, 2, 2, 3)
        );
        let crlf = LineIndex::new(b"a\r\nbc\r\nd");
        assert_eq!((crlf.line(0), crlf.line(3), crlf.line(7)), (1, 2, 3));
    }

    #[test]
    fn truncate_counts_characters() {
        assert_eq!(truncate("été", 3), "été");
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(normalize(" a\r\n  b\t c "), "a b c");
    }
}
