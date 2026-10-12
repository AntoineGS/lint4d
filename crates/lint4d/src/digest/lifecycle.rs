//! Object lifecycle verdicts from CFG paths (spec §6.3).

use std::collections::HashSet;
use std::ops::Range;

use cfg_pascal::calls::{CallKind, classify_call};
use cfg_pascal::cfg_core::{BlockId, Cfg, EdgeKind, PathEdge};
use pascal_core::parser::node_text;
use tree_sitter::Node;

use super::model::{Escape, EscapeKind, NotFreedReason, ObjectVerdict, Verdict};
use super::{LineIndex, field_children, is_exit_statement, named_children, normalize, target_name};

const MAX_ESCAPES: usize = 3;

/// Conditions that make every verdict of a routine unknown or suspect.
pub struct Context<'a> {
    pub parse_errors: bool,
    /// Opaque `{$IF}…{$IFEND}` blocks inside the routine, blanked by the parser rewrite.
    pub hidden: &'a [Range<usize>],
}

pub fn verdicts(
    scope: Node<'_>,
    cfg: &Cfg,
    text: &[u8],
    lines: &LineIndex,
    context: &Context<'_>,
) -> Vec<ObjectVerdict> {
    let body = if scope.kind() == "defProc" {
        scope.child_by_field_name("body")
    } else {
        Some(scope)
    };
    let Some(body) = body else {
        return Vec::new();
    };
    let facts = Facts::collect(body, text);
    let locals = local_names(scope, &facts, text);
    facts
        .creations
        .iter()
        .map(|creation| ObjectVerdict {
            var: creation.target.clone(),
            class: creation.class.clone(),
            line: lines.line(creation.range.start),
            verdict: verdict(creation, &facts, &locals, cfg, lines, context),
        })
        .collect()
}

struct Creation {
    key: String,
    target: String,
    class: String,
    range: Range<usize>,
}

#[derive(Default)]
struct Facts {
    creations: Vec<Creation>,
    assignments: Vec<(String, Range<usize>)>,
    frees: Vec<(String, Range<usize>)>,
    exits: Vec<Range<usize>>,
    inline_vars: HashSet<String>,
}

impl Facts {
    fn collect(body: Node<'_>, text: &[u8]) -> Self {
        let mut facts = Self::default();
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "lambda" | "defProc" => continue,
                "assignment" => facts.assignment(node, text),
                "statement" if is_exit_statement(node, text) => facts.exits.push(node.byte_range()),
                _ => {}
            }
            if let Some(var) = freed_var(node, text) {
                facts.frees.push((key(&var), node.byte_range()));
            }
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        facts
    }

    fn assignment(&mut self, node: Node<'_>, text: &[u8]) {
        let Some(lhs) = node.child_by_field_name("lhs") else {
            return;
        };
        let target = target_name(lhs, text);
        let key = key(&target);
        if lhs.kind() == "varAssignDef" {
            self.inline_vars.insert(key.clone());
        }
        if let Some(CallKind::Constructor { class_name }) = node
            .child_by_field_name("rhs")
            .and_then(|rhs| classify_call(rhs, text))
        {
            self.creations.push(Creation {
                key: key.clone(),
                target,
                class: class_name,
                range: node.byte_range(),
            });
        }
        self.assignments.push((key, node.byte_range()));
    }
}

fn key(name: &str) -> String {
    normalize(name).to_ascii_lowercase()
}

fn freed_var(node: Node<'_>, text: &[u8]) -> Option<String> {
    if !matches!(node.kind(), "exprCall" | "exprDot" | "statement") {
        return None;
    }
    let parent = node.parent();
    if node.kind() == "exprDot"
        && parent.is_some_and(|p| {
            p.kind() == "exprCall" && p.child_by_field_name("entity") == Some(node)
        })
    {
        return None;
    }
    match classify_call(node, text)? {
        CallKind::Free { var_name }
        | CallKind::Destroy { var_name }
        | CallKind::FreeAndNil { var_name } => Some(var_name),
        _ => None,
    }
}

fn local_names(scope: Node<'_>, facts: &Facts, text: &[u8]) -> HashSet<String> {
    let mut names = facts.inline_vars.clone();
    if scope.kind() == "defProc" {
        for section in field_children(scope, "local") {
            if section.kind() != "declVars" {
                continue;
            }
            for decl in named_children(section)
                .into_iter()
                .filter(|d| d.kind() == "declVar")
            {
                for name in field_children(decl, "name") {
                    names.insert(key(&node_text(name, text)));
                }
            }
        }
    }
    names
}

fn verdict(
    creation: &Creation,
    facts: &Facts,
    locals: &HashSet<String>,
    cfg: &Cfg,
    lines: &LineIndex,
    context: &Context<'_>,
) -> Verdict {
    if context.parse_errors {
        return Verdict::Unknown {
            why: "parse errors".to_string(),
        };
    }
    if creation.key == "result" {
        return Verdict::NotFreedHere {
            why: NotFreedReason::Result,
        };
    }
    if let Some((_, range)) = facts
        .assignments
        .iter()
        .find(|(key, range)| *key == creation.key && *range != creation.range)
    {
        return Verdict::Unknown {
            why: format!("reassigned at L{}", lines.line(range.start)),
        };
    }
    let frees: Vec<&Range<usize>> = facts
        .frees
        .iter()
        .filter(|(key, _)| *key == creation.key)
        .map(|(_, range)| range)
        .collect();
    if frees.is_empty() {
        let why = if locals.contains(&creation.key) {
            NotFreedReason::NeverFreed
        } else {
            NotFreedReason::NonLocal
        };
        return Verdict::NotFreedHere { why };
    }
    if let Some(block) = context
        .hidden
        .iter()
        .find(|block| block.start > creation.range.start)
    {
        return Verdict::Unknown {
            why: format!("hidden directive block at L{}", lines.line(block.start)),
        };
    }
    let free_blocks: HashSet<BlockId> = frees
        .iter()
        .flat_map(|range| cfg.blocks_for_stmt((*range).clone()))
        .collect();
    escapes(cfg, creation, &free_blocks, &facts.exits, lines)
}

/// Walk forward from the creation without entering a free; ignore
/// exceptions raised by the creating statement itself (nothing to free yet).
fn escapes(
    cfg: &Cfg,
    creation: &Creation,
    free_blocks: &HashSet<BlockId>,
    exits: &[Range<usize>],
    lines: &LineIndex,
) -> Verdict {
    let mut found: Vec<Escape> = Vec::new();
    for start in cfg.blocks_for_stmt(creation.range.clone()) {
        if cfg
            .path_avoiding(start, start, free_blocks, &[EdgeKind::ExceptionThrow])
            .is_some()
        {
            found.push(Escape {
                line: lines.line(creation.range.start),
                how: EscapeKind::ReCreated,
            });
        }
        let mut avoid = free_blocks.clone();
        for _ in 0..=MAX_ESCAPES {
            let Some(path) =
                cfg.path_avoiding(start, cfg.exit, &avoid, &[EdgeKind::ExceptionThrow])
            else {
                break;
            };
            let (block, escape) = escape_point(cfg, &path, exits, lines);
            if !found.contains(&escape) {
                found.push(escape);
            }
            if block == start || !avoid.insert(block) {
                break;
            }
        }
    }
    found.sort();
    found.dedup();
    if found.is_empty() {
        return Verdict::FreedAllPaths;
    }
    let more = found.len() > MAX_ESCAPES;
    found.truncate(MAX_ESCAPES);
    Verdict::Escapes { via: found, more }
}

/// Where `path` leaves for good: the first exception edge, else an `Exit`
/// statement, else the last statement before the routine ends.
fn escape_point(
    cfg: &Cfg,
    path: &[PathEdge],
    exits: &[Range<usize>],
    lines: &LineIndex,
) -> (BlockId, Escape) {
    let has_stmts = |block: &BlockId| !cfg.graph[block.index()].stmts.is_empty();
    let last_line = |block: BlockId| {
        cfg.graph[block.index()]
            .stmts
            .last()
            .map_or(0, |stmt| lines.line(stmt.byte_range.start))
    };
    if let Some(index) = path
        .iter()
        .position(|edge| edge.kind == EdgeKind::ExceptionThrow)
        && let Some(block) = path[..=index]
            .iter()
            .rev()
            .map(|edge| edge.from)
            .find(has_stmts)
    {
        return (
            block,
            Escape {
                line: last_line(block),
                how: EscapeKind::Exception,
            },
        );
    }
    let is_exit = |block: &BlockId| {
        cfg.graph[block.index()].stmts.iter().any(|stmt| {
            exits
                .iter()
                .any(|exit| exit.start <= stmt.byte_range.start && stmt.byte_range.end <= exit.end)
        })
    };
    if let Some(block) = path.iter().map(|edge| edge.from).find(is_exit) {
        return (
            block,
            Escape {
                line: last_line(block),
                how: EscapeKind::Exit,
            },
        );
    }
    match path.iter().rev().map(|edge| edge.from).find(has_stmts) {
        Some(block) => (
            block,
            Escape {
                line: last_line(block),
                how: EscapeKind::FallsOffEnd,
            },
        ),
        None => (
            path[0].from,
            Escape {
                line: lines.line(cfg.byte_range.end.saturating_sub(1)),
                how: EscapeKind::FallsOffEnd,
            },
        ),
    }
}
