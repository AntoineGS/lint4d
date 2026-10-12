use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::ops::Range;

use pascal_core::node_kind as K;
use tree_sitter::{Node, Tree};

use crate::cfg::analysis::AnalysisContext;
use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::helpers::{byte_offset_to_line_col, freed_identifier, node_text};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};
use cfg_pascal::cfg_core;
use petgraph::visit::EdgeRef;

pub struct UseAfterFreeRule {
    meta: RuleMeta,
}

impl Default for UseAfterFreeRule {
    fn default() -> Self {
        Self::new()
    }
}

impl UseAfterFreeRule {
    pub fn new() -> Self {
        UseAfterFreeRule {
            meta: RuleMeta {
                id: "use-after-free",
                name: "Use After Free",
                category: RuleCategory::ResourceManagement,
                default_severity: Severity::Error,
                description: "Detects variables used after being freed.",
                enabled_by_default: true,
            },
        }
    }
}

impl Rule for UseAfterFreeRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn requires_cfg(&self) -> bool {
        true
    }

    fn check(
        &self,
        _file: &FileInfo,
        _tree: &Tree,
        _source: &[u8],
        _config: &crate::config::Config,
        _ctx: &mut LintContext,
    ) {
        // CFG-based rule; analysis happens in check_cfg.
    }

    fn check_cfg(
        &self,
        _file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        _config: &crate::config::Config,
        analysis: &AnalysisContext<'_>,
        ctx: &mut LintContext,
    ) {
        let mut findings = BTreeSet::new();
        for cfg in analysis.cfgs.values() {
            analyze_cfg(cfg, tree.root_node(), source, &mut findings);
        }
        for finding in findings {
            report(&finding, source, ctx);
        }
    }
}

/// What one CFG statement does to object references, read from the AST.
#[derive(Debug, Default)]
struct Effects {
    frees: Vec<(String, Freed)>,
    assigns: Vec<String>,
    reads: Vec<String>,
    /// Operands of `Assigned(X)` and comparisons with `nil`: harmless on a
    /// variable `FreeAndNil` cleared, a use of a dangling one.
    nil_checks: Vec<String>,
    /// A condition that tests one variable against nil: the variable, and
    /// whether the condition is true when it is non-nil.
    guard: Option<(String, bool)>,
}

/// How a variable was freed: `FreeAndNil` leaves it nil, `.Free` and
/// `.Destroy` leave it dangling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freed {
    Nil,
    Dangling,
}

impl Freed {
    fn join(self, other: Freed) -> Freed {
        if self == other { self } else { Freed::Dangling }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum FindingKind {
    DoubleFree,
    UseAfterFree,
}

/// A finding keyed by statement range and symbol, so a block revisited by
/// the worklist cannot report the same statement twice.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Finding {
    start: usize,
    end: usize,
    symbol: String,
    kind: FindingKind,
}

/// Analyze a single procedure's CFG for use-after-free patterns.
///
/// A forward may-analysis over the set of freed variables (lowercase names):
/// a variable is freed at a block entry if any predecessor path frees it,
/// and dangling if any of those paths left it dangling.  An edge on which a
/// nil test proves a variable non-nil drops it when it can only be nil.
/// Findings are collected into `findings` and emitted by the caller once
/// every CFG has converged.
fn analyze_cfg(
    cfg: &cfg_core::types::Cfg,
    root: Node,
    source: &[u8],
    findings: &mut BTreeSet<Finding>,
) {
    type FreedState = HashMap<String, Freed>;

    let graph = &cfg.graph;
    let mut effects: HashMap<Range<usize>, Effects> = HashMap::new();
    let mut block_entry_state: HashMap<cfg_core::BlockId, FreedState> = HashMap::new();
    let mut worklist: VecDeque<cfg_core::BlockId> = VecDeque::new();

    block_entry_state.insert(cfg.entry, FreedState::new());
    worklist.push_back(cfg.entry);

    while let Some(block_id) = worklist.pop_front() {
        let mut state = block_entry_state
            .get(&block_id)
            .cloned()
            .unwrap_or_default();

        let mut guard = None;
        for stmt in &graph[block_id.index()].stmts {
            let range = stmt.byte_range.start..stmt.byte_range.end.min(source.len());
            if range.is_empty() {
                continue;
            }
            let stmt_effects = effects
                .entry(range.clone())
                .or_insert_with(|| statement_effects(root, source, range.clone()));
            apply_effects(stmt_effects, &range, &mut state, findings);
            guard = stmt_effects.guard.clone();
        }

        for edge in graph.edges(block_id.index()) {
            let successor = cfg_core::BlockId::from(edge.target());
            let mut out = state.clone();
            let taken = match edge.weight() {
                cfg_core::EdgeKind::ConditionalTrue => Some(true),
                cfg_core::EdgeKind::ConditionalFalse => Some(false),
                _ => None,
            };
            if let (Some(taken), Some((symbol, non_nil_when_true))) = (taken, &guard)
                && taken == *non_nil_when_true
                && out.get(symbol) == Some(&Freed::Nil)
            {
                out.remove(symbol);
            }
            let changed = match block_entry_state.get_mut(&successor) {
                Some(existing) => {
                    let mut changed = false;
                    for (symbol, freed) in out {
                        let joined = existing
                            .get(&symbol)
                            .map_or(freed, |current| current.join(freed));
                        if existing.insert(symbol, joined) != Some(joined) {
                            changed = true;
                        }
                    }
                    changed
                }
                None => {
                    block_entry_state.insert(successor, out);
                    true
                }
            };
            if changed {
                worklist.push_back(successor);
            }
        }
    }
}

/// Apply one statement's effects: reads are checked against the state before
/// the statement's own frees and assignments, so `X := X.Next` after a free
/// reports, and `X := nil` clears `X`.
fn apply_effects(
    effects: &Effects,
    range: &Range<usize>,
    state: &mut HashMap<String, Freed>,
    findings: &mut BTreeSet<Finding>,
) {
    let mut finding = |symbol: &str, kind| {
        findings.insert(Finding {
            start: range.start,
            end: range.end,
            symbol: symbol.to_string(),
            kind,
        });
    };
    for symbol in &effects.reads {
        if state.contains_key(symbol) {
            finding(symbol, FindingKind::UseAfterFree);
        }
    }
    for symbol in &effects.nil_checks {
        if state.get(symbol) == Some(&Freed::Dangling) {
            finding(symbol, FindingKind::UseAfterFree);
        }
    }
    for (symbol, freed) in &effects.frees {
        if state.insert(symbol.clone(), *freed).is_some() {
            finding(symbol, FindingKind::DoubleFree);
        }
    }
    for symbol in &effects.assigns {
        state.remove(symbol);
    }
}

/// Compute a statement's effects from the AST nodes inside `range`.
///
/// CFG statements for `if`, `while`, `for`, `case` and `with` cover only the
/// header, so the walk prunes every node outside the range instead of taking
/// the covering node whole.  Comments and string literals contain no
/// identifier nodes, so names inside them are never uses.
fn statement_effects(root: Node, source: &[u8], range: Range<usize>) -> Effects {
    let mut effects = Effects::default();
    let mut skipped: HashSet<usize> = HashSet::new();
    let Some(covering) = root.descendant_for_byte_range(range.start, range.end) else {
        return effects;
    };
    let mut stack = vec![covering];
    while let Some(node) = stack.pop() {
        if node.end_byte() <= range.start || node.start_byte() >= range.end {
            continue;
        }
        let inside = node.start_byte() >= range.start && node.end_byte() <= range.end;
        // The for-in header range ends at the iterable, so the node itself is
        // never inside it.
        if node.kind() == K::FOREACH {
            collect_foreach_effects(node, source, &mut effects, &mut skipped);
        }
        if inside {
            collect_node_effects(node, source, &mut effects, &mut skipped);
        }
        if matches!(node.kind(), K::IF | K::WHILE)
            && let Some(condition) = node.child_by_field_name("condition")
            && condition.start_byte() >= range.start
            && condition.end_byte() <= range.end
        {
            effects.guard = nil_guard(condition, source);
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    effects
}

/// The identifier an assignment or for-in header writes: a plain identifier,
/// or the name declared by an inline `var X`.
fn assign_target(node: Option<Node>) -> Option<Node> {
    let node = node?;
    match node.kind() {
        K::IDENTIFIER => Some(node),
        "varAssignDef" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == K::IDENTIFIER)
        }
        _ => None,
    }
}

/// The for-in variable is assigned by the header on every iteration.
fn collect_foreach_effects(
    node: Node,
    source: &[u8],
    effects: &mut Effects,
    skipped: &mut HashSet<usize>,
) {
    let target = assign_target(node.child_by_field_name("iterator"));
    if let Some(target) = target {
        skipped.insert(target.id());
        effects
            .assigns
            .push(node_text(target, source).to_lowercase());
    }
}

fn collect_node_effects(
    node: Node,
    source: &[u8],
    effects: &mut Effects,
    skipped: &mut HashSet<usize>,
) {
    let symbol = |node: Node| node_text(node, source).to_lowercase();
    match node.kind() {
        K::IDENTIFIER => {
            if !skipped.contains(&node.id()) {
                effects.reads.push(symbol(node));
            }
        }
        K::EXPR_DOT => {
            if let Some(freed) = freed_identifier(node, source) {
                skipped.insert(freed.id());
                effects.frees.push((symbol(freed), Freed::Dangling));
            }
            // Member names are not references to locals of the same name.
            if let Some(rhs) = node.child_by_field_name("rhs")
                && rhs.kind() == K::IDENTIFIER
            {
                skipped.insert(rhs.id());
            }
        }
        K::EXPR_CALL | K::EXPR_BINARY => {
            if let Some(freed) = freed_identifier(node, source) {
                skipped.insert(freed.id());
                effects.frees.push((symbol(freed), Freed::Nil));
            } else if let Some(checked) = nil_check_operand(node, source) {
                skipped.insert(checked.id());
                effects.nil_checks.push(symbol(checked));
            }
        }
        K::INHERITED => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                skipped.insert(child.id());
            }
        }
        K::ASSIGNMENT => {
            let plain = node
                .child_by_field_name("operator")
                .is_some_and(|operator| operator.kind() == K::K_ASSIGN);
            let target = assign_target(node.child_by_field_name("lhs"));
            if let Some(target) = target {
                if plain {
                    skipped.insert(target.id());
                }
                effects.assigns.push(symbol(target));
            }
        }
        _ => {}
    }
}

/// The identifier tested by `Assigned(X)`, `X = nil`, `X <> nil`, `nil = X`
/// or `nil <> X`, looking through parentheses.
fn nil_check_operand<'tree>(node: Node<'tree>, source: &[u8]) -> Option<Node<'tree>> {
    match node.kind() {
        K::EXPR_CALL => {
            let entity = node.child_by_field_name("entity")?;
            if entity.kind() != K::IDENTIFIER
                || !node_text(entity, source).eq_ignore_ascii_case("assigned")
            {
                return None;
            }
            let args = node.child_by_field_name("args")?;
            let mut cursor = args.walk();
            let mut operands = args
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra());
            let operand = operands.next()?;
            operands.next().is_none().then_some(())?;
            plain_identifier(operand)
        }
        K::EXPR_BINARY => {
            let operator = node.child_by_field_name("operator")?;
            if !matches!(operator.kind(), K::K_EQ | "kNeq") {
                return None;
            }
            let lhs = node.child_by_field_name("lhs")?;
            let rhs = node.child_by_field_name("rhs")?;
            match (lhs.kind(), rhs.kind()) {
                (_, "kNil") => plain_identifier(lhs),
                ("kNil", _) => plain_identifier(rhs),
                _ => None,
            }
        }
        _ => None,
    }
}

fn plain_identifier(node: Node) -> Option<Node> {
    match node.kind() {
        K::IDENTIFIER => Some(node),
        K::EXPR_PARENS => {
            let mut cursor = node.walk();
            let mut children = node
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra());
            let inner = children.next()?;
            children.next().is_none().then_some(())?;
            plain_identifier(inner)
        }
        _ => None,
    }
}

/// A condition that is exactly a nil test of one variable, possibly negated
/// or parenthesized: the variable and whether the condition holds when it is
/// non-nil.
fn nil_guard(condition: Node, source: &[u8]) -> Option<(String, bool)> {
    match condition.kind() {
        K::EXPR_PARENS => {
            let mut cursor = condition.walk();
            let mut children = condition
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra());
            let inner = children.next()?;
            children.next().is_none().then_some(())?;
            nil_guard(inner, source)
        }
        K::EXPR_UNARY => {
            let operator = condition.child_by_field_name("operator")?;
            if operator.kind() != K::K_NOT {
                return None;
            }
            let (symbol, non_nil_when_true) =
                nil_guard(condition.child_by_field_name("operand")?, source)?;
            Some((symbol, !non_nil_when_true))
        }
        K::EXPR_CALL | K::EXPR_BINARY => {
            let operand = nil_check_operand(condition, source)?;
            let non_nil_when_true = condition.kind() == K::EXPR_CALL
                || condition
                    .child_by_field_name("operator")
                    .is_some_and(|operator| operator.kind() == "kNeq");
            Some((node_text(operand, source).to_lowercase(), non_nil_when_true))
        }
        _ => None,
    }
}

fn report(finding: &Finding, source: &[u8], ctx: &mut LintContext) {
    let (line, column) = byte_offset_to_line_col(source, finding.start);
    let (end_line, end_column) = byte_offset_to_line_col(source, finding.end);
    let (message, help) = match finding.kind {
        FindingKind::DoubleFree => (
            format!("Double free: '{}' has already been freed", finding.symbol),
            "Remove the duplicate free call or check if the variable was reassigned.",
        ),
        FindingKind::UseAfterFree => (
            format!(
                "Use after free: '{}' is used after being freed",
                finding.symbol
            ),
            "Do not use a variable after it has been freed. \
             Reassign it first or restructure the code.",
        ),
    };
    ctx.report(Diagnostic {
        rule_id: "use-after-free".to_string(),
        severity: Severity::Error,
        message,
        line,
        column,
        end_line,
        end_column,
        help: Some(help.to_string()),
        scope: None,
    });
}
