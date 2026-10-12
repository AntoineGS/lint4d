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
    /// First parse-error line inside the routine, when there is one.
    pub first_error_line: Option<usize>,
    /// Last line of the routine.
    pub end_line: usize,
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
        .map(|creation| {
            let line = lines.line(creation.range.start);
            let (verdict, reference) = verdict(creation, &facts, &locals, cfg, lines, context);
            ObjectVerdict {
                var: creation.target.clone(),
                class: creation.class.clone(),
                line,
                verify_lines: verify_lines(&verdict, line, reference, context.end_line),
                verdict,
            }
        })
        .collect()
}

/// The lines to read to confirm a verdict that is not a proven fact.
/// `reference` is the line an unknown verdict names, when it names one.
fn verify_lines(
    verdict: &Verdict,
    line: usize,
    reference: Option<usize>,
    end_line: usize,
) -> Option<[usize; 2]> {
    match verdict {
        Verdict::Escapes { via, .. } => {
            let lines = via.iter().map(|escape| escape.line).chain([line]);
            let first = lines.clone().min().unwrap_or(line);
            let last = lines.max().unwrap_or(line);
            Some([first, last])
        }
        Verdict::Unknown { .. } => Some(match reference {
            Some(reference) => [line.min(reference), line.max(reference)],
            None => [line, end_line],
        }),
        Verdict::NotFreedHere {
            why: NotFreedReason::NeverFreed,
        } => Some([line, end_line]),
        Verdict::FreedAllPaths | Verdict::NotFreedHere { .. } => None,
    }
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
    /// Assignments of `nil`, by range.
    nil_assignments: HashSet<Range<usize>>,
    /// `if`/`while` conditions that test one variable against nil.
    guards: Vec<Guard>,
    exits: Vec<Range<usize>>,
    inline_vars: HashSet<String>,
}

/// A condition such as `Assigned(L)` or `L = nil`: the variable, the
/// condition's range, and whether it holds when the variable is non-nil.
struct Guard {
    key: String,
    condition: Range<usize>,
    non_nil_when_true: bool,
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
                "if" | "ifElse" | "while" if !opaque && !in_with => {
                    if let Some(condition) = node.child_by_field_name("condition")
                        && let Some((var, non_nil_when_true)) = nil_guard(condition, text)
                    {
                        self.guards.push(Guard {
                            key: key(&var),
                            condition: condition.byte_range(),
                            non_nil_when_true,
                        });
                    }
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
            if node
                .child_by_field_name("rhs")
                .is_some_and(|rhs| rhs.kind() == "kNil")
            {
                self.nil_assignments.insert(node.byte_range());
            }
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

/// A condition that is exactly `Assigned(X)`, `X = nil`, `X <> nil` (either
/// operand order), possibly negated or parenthesized: the variable and
/// whether the condition holds when it is non-nil.
fn nil_guard(condition: Node<'_>, text: &[u8]) -> Option<(String, bool)> {
    let variable = |node: Node<'_>| {
        let node = if node.kind() == "exprParens" {
            only_child(node)?
        } else {
            node
        };
        (node.kind() == "identifier").then(|| node_text(node, text).to_string())
    };
    match condition.kind() {
        "exprParens" => nil_guard(only_child(condition)?, text),
        "exprUnary" => {
            let operator = condition.child_by_field_name("operator")?;
            if operator.kind() != "kNot" {
                return None;
            }
            let (var, non_nil_when_true) =
                nil_guard(condition.child_by_field_name("operand")?, text)?;
            Some((var, !non_nil_when_true))
        }
        "exprCall" => {
            let entity = condition.child_by_field_name("entity")?;
            if entity.kind() != "identifier"
                || !node_text(entity, text).eq_ignore_ascii_case("assigned")
            {
                return None;
            }
            Some((
                variable(only_child(condition.child_by_field_name("args")?)?)?,
                true,
            ))
        }
        "exprBinary" => {
            let non_nil_when_true = match condition.child_by_field_name("operator")?.kind() {
                "kNeq" => true,
                "kEq" => false,
                _ => return None,
            };
            let lhs = condition.child_by_field_name("lhs")?;
            let rhs = condition.child_by_field_name("rhs")?;
            let var = match (lhs.kind(), rhs.kind()) {
                (_, "kNil") => variable(lhs)?,
                ("kNil", _) => variable(rhs)?,
                _ => return None,
            };
            Some((var, non_nil_when_true))
        }
        _ => None,
    }
}

fn only_child(node: Node<'_>) -> Option<Node<'_>> {
    let children = named_children(node);
    (children.len() == 1).then(|| children[0])
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
) -> (Verdict, Option<usize>) {
    if context.parse_errors {
        let why = "parse errors".to_string();
        return (Verdict::Unknown { why }, context.first_error_line);
    }
    if creation.key == "result" {
        let why = NotFreedReason::Result;
        return (Verdict::NotFreedHere { why }, None);
    }
    let frees: Vec<&Range<usize>> = facts
        .frees
        .iter()
        .filter(|(key, _)| *key == creation.key)
        .map(|(_, range)| range)
        .collect();
    let free_blocks: HashSet<BlockId> = frees
        .iter()
        .flat_map(|range| cfg.blocks_for_stmt((*range).clone()))
        .collect();
    // `L := nil` cannot lose the object when no path from the creation
    // reaches it without a free (initialization before a `try`, or clearing
    // after the free).
    if let Some((_, range)) = facts.assignments.iter().find(|(key, range)| {
        *key == creation.key
            && *range != creation.range
            && !(facts.nil_assignments.contains(range)
                && !reachable_from_creation(cfg, creation, &free_blocks, range))
    }) {
        let line = lines.line(range.start);
        let why = format!("reassigned at L{line}");
        return (Verdict::Unknown { why }, Some(line));
    }
    if let Some((_, range)) = facts
        .with_names
        .iter()
        .find(|(key, _)| *key == creation.key)
    {
        let line = lines.line(range.start);
        let why = format!("named inside with at L{line}");
        return (Verdict::Unknown { why }, Some(line));
    }
    if let Some(block) = context.hidden.iter().min_by_key(|block| block.start) {
        let line = lines.line(block.start);
        let why = format!("hidden directive block at L{line}");
        return (Verdict::Unknown { why }, Some(line));
    }
    if frees.is_empty() {
        let why = if locals.contains(&creation.key) {
            NotFreedReason::NeverFreed
        } else {
            NotFreedReason::NonLocal
        };
        return (Verdict::NotFreedHere { why }, None);
    }
    if cfg.blocks_for_stmt(creation.range.clone()).is_empty() {
        let why = "creation not in CFG".to_string();
        return (Verdict::Unknown { why }, None);
    }
    // A nil test of a local only reads a variable, so it cannot raise.
    let guards: Vec<(&Guard, bool)> = facts
        .guards
        .iter()
        .filter(|guard| guard.key == creation.key)
        .map(|guard| (guard, locals.contains(&guard.key)))
        .collect();
    let verdict = escapes(cfg, creation, &free_blocks, &facts.exits, &guards, lines);
    (verdict, None)
}

/// Whether a path from the creation reaches `range` without passing a free.
/// Exceptions raised by the creating statement are ignored (nothing was
/// created). An unknown position counts as reachable.
fn reachable_from_creation(
    cfg: &Cfg,
    creation: &Creation,
    free_blocks: &HashSet<BlockId>,
    range: &Range<usize>,
) -> bool {
    let starts = cfg.blocks_for_stmt(creation.range.clone());
    let targets = cfg.blocks_for_stmt(range.clone());
    if starts.is_empty() || targets.is_empty() {
        return true;
    }
    starts.iter().any(|&start| {
        targets.iter().any(|&target| {
            (target == start && range.start > creation.range.start)
                || cfg
                    .path_avoiding(start, target, free_blocks, &[EdgeKind::ExceptionThrow])
                    .is_some()
        })
    })
}

/// Edges out of `block` that are never taken when it ends with a guard on
/// the created variable: after the creation the variable is non-nil (other
/// assignments make the verdict unknown), so the nil branch is infeasible,
/// and a guard on a local (`true` in `guards`) cannot raise.
fn infeasible_edges(cfg: &Cfg, block: BlockId, guards: &[(&Guard, bool)]) -> Vec<EdgeKind> {
    let Some(stmt) = cfg.graph[block.index()].stmts.last() else {
        return Vec::new();
    };
    let Some((guard, local)) = guards.iter().find(|(guard, _)| {
        stmt.byte_range.start <= guard.condition.start && guard.condition.end <= stmt.byte_range.end
    }) else {
        return Vec::new();
    };
    let mut edges = vec![if guard.non_nil_when_true {
        EdgeKind::ConditionalFalse
    } else {
        EdgeKind::ConditionalTrue
    }];
    if *local {
        edges.push(EdgeKind::ExceptionThrow);
    }
    edges
}

/// Whether Exit is reachable from `from` without entering a free or taking
/// an infeasible guard branch.
fn reaches_exit_avoiding(
    cfg: &Cfg,
    from: BlockId,
    free_blocks: &HashSet<BlockId>,
    guards: &[(&Guard, bool)],
) -> bool {
    let mut seen = HashSet::from([from]);
    let mut queue = VecDeque::from([from]);
    while let Some(block) = queue.pop_front() {
        let infeasible = infeasible_edges(cfg, block, guards);
        for edge in cfg.graph.edges(block.index()) {
            let target = BlockId::from(edge.target());
            if free_blocks.contains(&target) || infeasible.contains(edge.weight()) {
                continue;
            }
            if target == cfg.exit {
                return true;
            }
            if seen.insert(target) {
                queue.push_back(target);
            }
        }
    }
    false
}

/// Every statement where a path from the creation leaves the routine
/// without entering a free. Exceptions raised by the creating statement
/// itself are ignored (nothing to free yet).
fn escapes(
    cfg: &Cfg,
    creation: &Creation,
    free_blocks: &HashSet<BlockId>,
    exits: &[Range<usize>],
    guards: &[(&Guard, bool)],
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
            let infeasible = infeasible_edges(cfg, block, guards);
            let mut reaches_exit = false;
            for edge in cfg.graph.edges(block.index()) {
                let target = BlockId::from(edge.target());
                if free_blocks.contains(&target) || infeasible.contains(edge.weight()) {
                    continue;
                }
                if *edge.weight() == EdgeKind::ExceptionThrow {
                    let leaves = target == cfg.exit
                        || reaches_exit_avoiding(cfg, target, free_blocks, guards);
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
