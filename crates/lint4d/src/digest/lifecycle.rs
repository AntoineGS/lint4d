//! Object lifecycle verdicts from CFG paths (spec §6.3).

use std::collections::{HashSet, VecDeque};
use std::ops::Range;

use cfg_pascal::calls::{CallKind, classify_call};
use cfg_pascal::cfg_core::{BlockId, Cfg, EdgeKind};
use pascal_core::parser::node_text;
use petgraph::visit::EdgeRef;
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
    let nested: Vec<Node<'_>> = if scope.kind() == "defProc" {
        field_children(scope, "local")
    } else {
        Vec::new()
    };
    let facts = Facts::collect(body, &nested, text);
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
    /// Frees, assignments and creations inside a `with` body, where a name
    /// may denote a member of the `with` subject instead of the local.
    with_names: Vec<(String, Range<usize>)>,
    exits: Vec<Range<usize>>,
    inline_vars: HashSet<String>,
}

impl Facts {
    fn collect(body: Node<'_>, nested: &[Node<'_>], text: &[u8]) -> Self {
        let mut facts = Self::default();
        facts.walk(body, false, text);
        for node in nested {
            facts.walk(*node, true, text);
        }
        facts
    }

    /// Walk `root`. Inside lambdas and nested routines (`opaque`) only
    /// assignments are recorded: they can reassign a captured variable, but
    /// their creations, frees and exits are not on this routine's paths.
    /// Inside a `with` body, frees and assignments are recorded only as
    /// ambiguous names.
    fn walk(&mut self, root: Node<'_>, opaque: bool, text: &[u8]) {
        let mut stack = vec![(root, opaque, false)];
        while let Some((node, opaque, in_with)) = stack.pop() {
            let opaque = opaque || matches!(node.kind(), "lambda" | "defProc");
            match node.kind() {
                "assignment" if in_with => self.with_name(node, text),
                "assignment" if opaque => self.reassignment(node, text),
                "assignment" => self.assignment(node, text),
                "statement" if !opaque && is_exit_statement(node, text) => {
                    self.exits.push(node.byte_range())
                }
                _ => {}
            }
            if !opaque && let Some(var) = freed_var(node, text) {
                let names = if in_with {
                    &mut self.with_names
                } else {
                    &mut self.frees
                };
                names.push((key(&var), node.byte_range()));
            }
            let with_body = (node.kind() == "with")
                .then(|| node.child_by_field_name("body"))
                .flatten();
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
            stack.extend(
                children
                    .into_iter()
                    .rev()
                    .map(|child| (child, opaque, in_with || Some(child) == with_body)),
            );
        }
    }

    /// An assignment inside a `with` body: a creation is still listed, but
    /// its verdict is unknown.
    fn with_name(&mut self, node: Node<'_>, text: &[u8]) {
        if let Some(key) = self.creation(node, text) {
            self.with_names.push((key, node.byte_range()));
        }
    }

    fn reassignment(&mut self, node: Node<'_>, text: &[u8]) {
        if let Some(lhs) = node.child_by_field_name("lhs") {
            self.assignments
                .push((key(&target_name(lhs, text)), node.byte_range()));
        }
    }

    fn assignment(&mut self, node: Node<'_>, text: &[u8]) {
        if let Some(key) = self.creation(node, text) {
            self.assignments.push((key, node.byte_range()));
        }
    }

    /// Records a constructor assignment as a creation; returns the target key.
    fn creation(&mut self, node: Node<'_>, text: &[u8]) -> Option<String> {
        let lhs = node.child_by_field_name("lhs")?;
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
        Some(key)
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
    if let Some((_, range)) = facts
        .with_names
        .iter()
        .find(|(key, _)| *key == creation.key)
    {
        return Verdict::Unknown {
            why: format!("named inside with at L{}", lines.line(range.start)),
        };
    }
    if let Some(block) = context.hidden.iter().min_by_key(|block| block.start) {
        return Verdict::Unknown {
            why: format!("hidden directive block at L{}", lines.line(block.start)),
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
    if cfg.blocks_for_stmt(creation.range.clone()).is_empty() {
        return Verdict::Unknown {
            why: "creation not in CFG".to_string(),
        };
    }
    let free_blocks: HashSet<BlockId> = frees
        .iter()
        .flat_map(|range| cfg.blocks_for_stmt((*range).clone()))
        .collect();
    escapes(cfg, creation, &free_blocks, &facts.exits, lines)
}

/// Every statement where a path from the creation leaves the routine
/// without entering a free. Exceptions raised by the creating statement
/// itself are ignored (nothing to free yet).
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
        let mut seen = HashSet::from([start]);
        let mut queue = VecDeque::from([start]);
        while let Some(block) = queue.pop_front() {
            let last_line = || {
                cfg.graph[block.index()].stmts.last().map_or_else(
                    || lines.line(cfg.byte_range.end.saturating_sub(1)),
                    |stmt| lines.line(stmt.byte_range.start),
                )
            };
            let holds_exit = cfg.graph[block.index()].stmts.iter().any(|stmt| {
                exits.iter().any(|exit| {
                    exit.start <= stmt.byte_range.start && stmt.byte_range.end <= exit.end
                })
            });
            let mut reaches_exit = false;
            for edge in cfg.graph.edges(block.index()) {
                let target = BlockId::from(edge.target());
                if free_blocks.contains(&target) {
                    continue;
                }
                if *edge.weight() == EdgeKind::ExceptionThrow {
                    let leaves = target == cfg.exit
                        || cfg
                            .path_avoiding(target, cfg.exit, free_blocks, &[])
                            .is_some();
                    if block != start && leaves {
                        found.push(Escape {
                            line: last_line(),
                            how: EscapeKind::Exception,
                        });
                    }
                    continue;
                }
                reaches_exit |= target == cfg.exit;
                if seen.insert(target) {
                    queue.push_back(target);
                }
            }
            if reaches_exit {
                let how = if holds_exit {
                    EscapeKind::Exit
                } else {
                    EscapeKind::FallsOffEnd
                };
                found.push(Escape {
                    line: last_line(),
                    how,
                });
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
