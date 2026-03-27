use std::collections::HashMap;
use std::ops::Range;

use tree_sitter::{Node, Tree};

use cfg_core::types::Cfg;
use cfg_pascal::calls::{lookup_transaction_method, TransactionCallKind, TransactionOp};

use crate::cfg::analysis::AnalysisContext;
use crate::dcu::ProjectContext;
use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::helpers::{build_var_type_map, extract_uses_clauses, node_text};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

// ═══════════════════════════════════════════════════════════════
// Data types
// ═══════════════════════════════════════════════════════════════

/// Immutable context threaded through the recursive AST walkers.
struct AnalysisCtx<'a> {
    source: &'a [u8],
    var_types: &'a HashMap<String, String>,
    project: &'a ProjectContext,
    uses: &'a [String],
    guards: &'a [GuardBinding],
    protected_ranges: &'a [Range<usize>],
}

/// A transaction operation found in the procedure body.
struct TrxOp {
    op: TransactionOp,
    receiver: String,
    byte_range: Range<usize>,
    /// True if the operation is inside an `if guardVar then` block.
    is_guarded: bool,
    /// True if the operation is inside an except or finally handler block.
    in_protected_handler: bool,
}

/// A guard binding: `guardVar := not receiver.InTransaction`.
struct GuardBinding {
    guard_var: String,
    _receiver: String,
}

/// A diagnostic finding from transaction analysis.
struct TrxFinding {
    rule_id: &'static str,
    message: String,
    byte_range: Range<usize>,
    severity: Severity,
    help: String,
}

// ═══════════════════════════════════════════════════════════════
// Transaction call classification (type-checked)
// ═══════════════════════════════════════════════════════════════

/// Classify a dot-call AST node as a transaction operation.
///
/// Resolves the receiver's declared type from `var_types`, then checks the
/// framework dictionary. Uses `ProjectContext` to walk the inheritance chain
/// for subclass matching. Returns `None` if the type is unresolvable or does
/// not match any known transaction type.
fn classify_transaction_call(
    node: Node,
    source: &[u8],
    var_types: &HashMap<String, String>,
    project: &ProjectContext,
    uses: &[String],
) -> Option<TransactionCallKind> {
    let (receiver, method) = extract_dot_call(node, source)?;
    let entries = lookup_transaction_method(&method);
    if entries.is_empty() {
        return None;
    }
    let declared_type = var_types.get(&receiver.to_lowercase())?;
    for entry in &entries {
        if declared_type.eq_ignore_ascii_case(entry.type_name) {
            return Some(make_call_kind(entry.op, receiver.clone()));
        }
        if let Some(true) = project.descends_from(declared_type, entry.type_name, uses) {
            return Some(make_call_kind(entry.op, receiver.clone()));
        }
    }
    None
}

fn extract_dot_call(node: Node, source: &[u8]) -> Option<(String, String)> {
    match node.kind() {
        "exprCall" => {
            let entity = node.child_by_field_name("entity")?;
            if entity.kind() == "exprDot" {
                let lhs = entity.child_by_field_name("lhs")?;
                let rhs = entity.child_by_field_name("rhs")?;
                Some((node_text(lhs, source), node_text(rhs, source)))
            } else {
                None
            }
        }
        "exprDot" => {
            let lhs = node.child_by_field_name("lhs")?;
            let rhs = node.child_by_field_name("rhs")?;
            Some((node_text(lhs, source), node_text(rhs, source)))
        }
        _ => None,
    }
}

fn make_call_kind(op: TransactionOp, receiver: String) -> TransactionCallKind {
    match op {
        TransactionOp::Start => TransactionCallKind::Start { receiver },
        TransactionOp::Commit => TransactionCallKind::Commit { receiver },
        TransactionOp::Rollback => TransactionCallKind::Rollback { receiver },
        TransactionOp::InTransaction => TransactionCallKind::InTransaction { receiver },
    }
}

// ═══════════════════════════════════════════════════════════════
// Guard detection
// ═══════════════════════════════════════════════════════════════

/// Scan an AST subtree for `guardVar := not receiver.InTransaction` patterns.
fn collect_guard_bindings(
    node: Node,
    source: &[u8],
    var_types: &HashMap<String, String>,
    project: &ProjectContext,
    uses: &[String],
) -> Vec<GuardBinding> {
    let mut guards = Vec::new();
    collect_guards_recursive(node, source, var_types, project, uses, &mut guards);
    guards
}

fn collect_guards_recursive(
    node: Node,
    source: &[u8],
    var_types: &HashMap<String, String>,
    project: &ProjectContext,
    uses: &[String],
    out: &mut Vec<GuardBinding>,
) {
    // Look for assignment: guardVar := not receiver.InTransaction
    if node.kind() == "assignment" {
        if let Some(binding) = try_parse_guard_assignment(node, source, var_types, project, uses) {
            out.push(binding);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_guards_recursive(child, source, var_types, project, uses, out);
    }
}

/// Try to parse `lhs := not receiver.InTransaction` from an assignment node.
fn try_parse_guard_assignment(
    node: Node,
    source: &[u8],
    var_types: &HashMap<String, String>,
    project: &ProjectContext,
    uses: &[String],
) -> Option<GuardBinding> {
    let lhs = node.child_by_field_name("lhs")?;
    let rhs = node.child_by_field_name("rhs")?;

    let guard_var = node_text(lhs, source);

    // RHS must be `not <expr>` — check for exprUnary with `not` or `kNot`
    if rhs.kind() != "exprUnary" {
        return None;
    }
    let mut rhs_cursor = rhs.walk();
    let mut found_not = false;
    let mut inner_expr: Option<Node> = None;
    for child in rhs.children(&mut rhs_cursor) {
        let kind = child.kind();
        let text_lower = node_text(child, source).to_lowercase();
        if kind == "kNot" || text_lower == "not" {
            found_not = true;
        } else if child.is_named() {
            inner_expr = Some(child);
        }
    }

    if !found_not {
        return None;
    }
    let inner = inner_expr?;

    // Inner must classify as InTransaction
    let call = classify_transaction_call(inner, source, var_types, project, uses)?;
    match call {
        TransactionCallKind::InTransaction { receiver } => Some(GuardBinding {
            guard_var: guard_var.to_lowercase(),
            _receiver: receiver,
        }),
        _ => None,
    }
}

/// Check whether a condition node references a known guard variable
/// OR is an inline `not receiver.InTransaction` expression.
fn condition_is_guard(
    node: Node,
    source: &[u8],
    guards: &[GuardBinding],
    var_types: &HashMap<String, String>,
    project: &ProjectContext,
    uses: &[String],
) -> bool {
    // Check named guard variables
    let text = node_text(node, source).to_lowercase().trim().to_string();
    if guards.iter().any(|g| g.guard_var == text) {
        return true;
    }
    // Check inline `not receiver.InTransaction` pattern
    if node.kind() == "exprUnary" {
        let mut cursor = node.walk();
        let mut found_not = false;
        let mut inner_expr: Option<Node> = None;
        for child in node.children(&mut cursor) {
            let kind = child.kind();
            let text_lower = node_text(child, source).to_lowercase();
            if kind == "kNot" || text_lower == "not" {
                found_not = true;
            } else if child.is_named() {
                inner_expr = Some(child);
            }
        }
        if found_not {
            if let Some(inner) = inner_expr {
                if let Some(TransactionCallKind::InTransaction { .. }) =
                    classify_transaction_call(inner, source, var_types, project, uses)
                {
                    return true;
                }
            }
        }
    }
    false
}

// ═══════════════════════════════════════════════════════════════
// Transaction operation collection
// ═══════════════════════════════════════════════════════════════

/// Collect all byte ranges that represent protected handler sections in the AST.
///
/// Walks the tree looking for `try` nodes and collects the byte ranges of:
/// - `except` sections (between `kExcept` and `kEnd`) — exception path only
/// - `finally` sections (between `kFinally` and `kEnd`) — both success and exception paths
///
/// A rollback in either section satisfies the "has rollback protection" check
/// because both guarantee the rollback will run on failure.
fn collect_protected_ranges(proc_node: Node) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    collect_protected_ranges_recursive(proc_node, &mut ranges);
    ranges
}

fn collect_protected_ranges_recursive(node: Node, out: &mut Vec<Range<usize>>) {
    if node.kind() == "try" {
        // Collect ranges for both `except` and `finally` sections within this try node.
        // Each section starts at the keyword token (`kExcept` or `kFinally`) and extends
        // to the closing `kEnd` token of this try block.
        let mut section_start: Option<usize> = None;
        let mut in_section = false;
        let mut end_byte: Option<usize> = None;

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "kExcept" | "kFinally" => {
                    // Flush any previous section that didn't find its end yet
                    if in_section {
                        if let (Some(start), Some(end)) = (section_start, end_byte) {
                            out.push(start..end);
                        }
                    }
                    section_start = Some(child.start_byte());
                    in_section = true;
                    end_byte = None;
                }
                "kEnd" if in_section => {
                    end_byte = Some(child.end_byte());
                }
                _ => {}
            }
        }

        // Push the last section found (except or finally before kEnd)
        if let (Some(start), Some(end)) = (section_start, end_byte) {
            out.push(start..end);
        }
    }

    // Recurse into all children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_protected_ranges_recursive(child, out);
    }
}

/// Collect all transaction operations from a procedure, annotated with
/// guard status and protected handler context.
fn collect_transaction_ops(
    proc_node: Node,
    source: &[u8],
    var_types: &HashMap<String, String>,
    project: &ProjectContext,
    uses: &[String],
    guards: &[GuardBinding],
) -> Vec<TrxOp> {
    let protected_ranges = collect_protected_ranges(proc_node);
    let ctx = AnalysisCtx {
        source,
        var_types,
        project,
        uses,
        guards,
        protected_ranges: &protected_ranges,
    };
    let mut ops = Vec::new();
    collect_ops_recursive(proc_node, false, &ctx, &mut ops);
    ops
}

fn collect_ops_recursive(node: Node, is_guarded: bool, ctx: &AnalysisCtx<'_>, out: &mut Vec<TrxOp>) {
    // Check if this is a transaction call
    if let Some(call) =
        classify_transaction_call(node, ctx.source, ctx.var_types, ctx.project, ctx.uses)
    {
        let (op, receiver) = match call {
            TransactionCallKind::Start { receiver } => (TransactionOp::Start, receiver),
            TransactionCallKind::Commit { receiver } => (TransactionOp::Commit, receiver),
            TransactionCallKind::Rollback { receiver } => (TransactionOp::Rollback, receiver),
            TransactionCallKind::InTransaction { .. } => {
                // InTransaction calls are tracked via guard bindings, not as ops
                return;
            }
        };

        let byte_range = node.start_byte()..node.end_byte();
        let in_protected_handler = is_in_protected_handler(&byte_range, ctx.protected_ranges);

        out.push(TrxOp {
            op,
            receiver,
            byte_range,
            is_guarded,
            in_protected_handler,
        });
        return;
    }

    // Check for `if guardVar then` — children on the then-branch are guarded
    if node.kind() == "ifElse" || node.kind() == "if" {
        if let Some(cond) = node
            .child_by_field_name("cond")
            .or_else(|| node.child_by_field_name("condition"))
        {
            let cond_is_guard =
                condition_is_guard(cond, ctx.source, ctx.guards, ctx.var_types, ctx.project, ctx.uses);

            let mut cursor = node.walk();
            let mut saw_cond = false;
            let mut in_then = false;
            for child in node.children(&mut cursor) {
                if child.id() == cond.id() {
                    saw_cond = true;
                    continue;
                }
                if saw_cond && child.kind() == "kThen" {
                    in_then = true;
                    continue;
                }
                if child.kind() == "kElse" {
                    in_then = false;
                }
                let child_guarded = if cond_is_guard && in_then {
                    true
                } else {
                    is_guarded
                };
                collect_ops_recursive(child, child_guarded, ctx, out);
            }
            return;
        }
    }

    // Default: recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_ops_recursive(child, is_guarded, ctx, out);
    }
}

/// Check whether a byte range falls within any protected handler section (except or finally).
fn is_in_protected_handler(byte_range: &Range<usize>, protected_ranges: &[Range<usize>]) -> bool {
    protected_ranges
        .iter()
        .any(|r| r.start <= byte_range.start && byte_range.end <= r.end)
}

// ═══════════════════════════════════════════════════════════════
// Rule checks
// ═══════════════════════════════════════════════════════════════

fn check_no_rollback(ops: &[TrxOp], findings: &mut Vec<TrxFinding>) {
    // Group ops by receiver
    let receivers = unique_receivers(ops);
    for recv in &receivers {
        let starts: Vec<_> = ops
            .iter()
            .filter(|o| o.receiver.eq_ignore_ascii_case(recv) && o.op == TransactionOp::Start)
            .collect();
        let has_rollback_in_handler = ops.iter().any(|o| {
            o.receiver.eq_ignore_ascii_case(recv)
                && o.op == TransactionOp::Rollback
                && o.in_protected_handler
        });

        if !starts.is_empty() && !has_rollback_in_handler {
            for start in &starts {
                findings.push(TrxFinding {
                    rule_id: "transaction-no-rollback",
                    message: format!(
                        "Transaction started on '{}' but no rollback in exception handler",
                        recv
                    ),
                    byte_range: start.byte_range.clone(),
                    severity: Severity::Error,
                    help: "Wrap transaction work in try/except and rollback in the except block."
                        .to_string(),
                });
            }
        }
    }
}

fn check_ownership_violation(ops: &[TrxOp], findings: &mut Vec<TrxFinding>) {
    let receivers = unique_receivers(ops);
    for recv in &receivers {
        let starts: Vec<_> = ops
            .iter()
            .filter(|o| o.receiver.eq_ignore_ascii_case(recv) && o.op == TransactionOp::Start)
            .collect();
        let start_is_guarded = starts.iter().all(|s| s.is_guarded);
        let has_no_start = starts.is_empty();

        // Commits/rollbacks that are unguarded when start is guarded or absent
        for op in ops {
            if !op.receiver.eq_ignore_ascii_case(recv) {
                continue;
            }
            if op.op != TransactionOp::Commit && op.op != TransactionOp::Rollback {
                continue;
            }
            if !op.is_guarded && (start_is_guarded || has_no_start) {
                let op_name = if op.op == TransactionOp::Commit {
                    "Commit"
                } else {
                    "Rollback"
                };
                findings.push(TrxFinding {
                    rule_id: "transaction-ownership-violation",
                    message: format!(
                        "{} on '{}' without owning the transaction (start is {} in this scope)",
                        op_name,
                        recv,
                        if has_no_start {
                            "absent"
                        } else {
                            "conditional"
                        }
                    ),
                    byte_range: op.byte_range.clone(),
                    severity: Severity::Error,
                    help: "Only commit/rollback transactions you started. Guard with the same boolean used for StartTransaction.".to_string(),
                });
            }
        }
    }
}

fn check_no_commit(ops: &[TrxOp], findings: &mut Vec<TrxFinding>) {
    let receivers = unique_receivers(ops);
    for recv in &receivers {
        let has_start = ops.iter().any(|o| {
            o.receiver.eq_ignore_ascii_case(recv) && o.op == TransactionOp::Start
        });
        let has_commit_outside_handler = ops.iter().any(|o| {
            o.receiver.eq_ignore_ascii_case(recv)
                && o.op == TransactionOp::Commit
                && !o.in_protected_handler
        });

        if has_start && !has_commit_outside_handler {
            let start = ops
                .iter()
                .find(|o| {
                    o.receiver.eq_ignore_ascii_case(recv) && o.op == TransactionOp::Start
                })
                .unwrap();
            findings.push(TrxFinding {
                rule_id: "transaction-no-commit",
                message: format!(
                    "Transaction started on '{}' but never committed on the normal path",
                    recv
                ),
                byte_range: start.byte_range.clone(),
                severity: Severity::Warning,
                help: "Add a commit call on the success path (typically inside the try block)."
                    .to_string(),
            });
        }
    }
}

fn check_nested_start(ops: &[TrxOp], findings: &mut Vec<TrxFinding>) {
    let receivers = unique_receivers(ops);
    for recv in &receivers {
        let starts: Vec<_> = ops
            .iter()
            .filter(|o| o.receiver.eq_ignore_ascii_case(recv) && o.op == TransactionOp::Start)
            .collect();
        // If there are multiple unguarded starts for the same receiver, flag duplicates
        let unguarded_starts: Vec<_> = starts.iter().filter(|s| !s.is_guarded).collect();
        if unguarded_starts.len() > 1 {
            // Flag all but the first
            for start in &unguarded_starts[1..] {
                findings.push(TrxFinding {
                    rule_id: "transaction-nested-start",
                    message: format!(
                        "Nested StartTransaction on '{}' without InTransaction guard",
                        recv
                    ),
                    byte_range: start.byte_range.clone(),
                    severity: Severity::Warning,
                    help: "Check InTransaction before starting to avoid nested transaction errors."
                        .to_string(),
                });
            }
        }
    }
}

fn unique_receivers(ops: &[TrxOp]) -> Vec<String> {
    let mut seen = Vec::new();
    for op in ops {
        let lower = op.receiver.to_lowercase();
        if !seen.iter().any(|s: &String| s == &lower) {
            seen.push(lower);
        }
    }
    seen
}

// ═══════════════════════════════════════════════════════════════
// Main analysis entry point
// ═══════════════════════════════════════════════════════════════

/// Analyze a procedure for transaction issues.
///
/// Walks the AST to collect transaction operations and guard bindings,
/// uses AST structure for exception path context, then checks all four rules.
fn analyze_procedure(
    proc_node: Node,
    source: &[u8],
    project: &ProjectContext,
    uses: &[String],
) -> Vec<TrxFinding> {
    let var_types = build_var_type_map(proc_node, source);
    let guards = collect_guard_bindings(proc_node, source, &var_types, project, uses);
    let ops = collect_transaction_ops(proc_node, source, &var_types, project, uses, &guards);

    if ops.is_empty() {
        return Vec::new();
    }

    let mut findings = Vec::new();
    check_no_rollback(&ops, &mut findings);
    check_ownership_violation(&ops, &mut findings);
    check_no_commit(&ops, &mut findings);
    check_nested_start(&ops, &mut findings);
    findings
}

/// Extract the qualified proc name from a `defProc` node.
///
/// AST structure: `defProc header:(declProc name:(identifier))`
/// For methods: `defProc header:(declProc (genericDot (identifier) (identifier)))`
fn extract_defproc_name(def_proc: Node, source: &[u8]) -> Option<String> {
    // Get the `header` field (which is a `declProc` node)
    let decl_proc = if let Some(h) = def_proc.child_by_field_name("header") {
        h
    } else {
        // Fallback: find the first `declProc` child
        let mut cursor = def_proc.walk();
        let found = def_proc
            .children(&mut cursor)
            .find(|c| c.kind() == "declProc");
        drop(cursor);
        found?
    };

    // Try `genericDot` first (for method implementations like `TClass.Method`)
    let mut cursor = decl_proc.walk();
    if let Some(generic_dot) = decl_proc
        .children(&mut cursor)
        .find(|c| c.kind() == "genericDot")
    {
        let idents: Vec<Node> = generic_dot
            .children(&mut generic_dot.walk())
            .filter(|c| c.kind() == "identifier")
            .collect();
        if idents.len() >= 2 {
            return Some(format!(
                "{}.{}",
                node_text(idents[0], source),
                node_text(idents[1], source)
            ));
        }
        if !idents.is_empty() {
            return Some(node_text(idents[0], source));
        }
    }

    // Try the `name` field on `declProc`
    if let Some(name_node) = decl_proc.child_by_field_name("name") {
        return Some(node_text(name_node, source));
    }

    // Fallback: first direct `identifier` child of `declProc`
    let mut cursor2 = decl_proc.walk();
    for child in decl_proc.children(&mut cursor2) {
        if child.kind() == "identifier" {
            return Some(node_text(child, source));
        }
    }

    None
}

/// Find defProc nodes in the AST and match them to CFGs by proc_name.
fn find_proc_node_for_cfg<'a>(
    tree: &'a Tree,
    source: &[u8],
    cfg: &Cfg,
) -> Option<Node<'a>> {
    fn walk<'a>(node: Node<'a>, source: &[u8], target_name: &str) -> Option<Node<'a>> {
        if node.kind() == "defProc" {
            if let Some(name) = extract_defproc_name(node, source) {
                if name.eq_ignore_ascii_case(target_name) {
                    return Some(node);
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(found) = walk(child, source, target_name) {
                return Some(found);
            }
        }
        None
    }

    // The proc_name in the CFG may be qualified (e.g. "TFoo.Bar").
    // Try matching the full name first, then just the method part.
    let result = walk(tree.root_node(), source, &cfg.proc_name);
    if result.is_some() {
        return result;
    }
    if let Some(dot_pos) = cfg.proc_name.rfind('.') {
        let method_name = &cfg.proc_name[dot_pos + 1..];
        return walk(tree.root_node(), source, method_name);
    }
    None
}

/// Shared implementation: run analysis for all CFGs and collect findings.
fn run_transaction_analysis(
    tree: &Tree,
    source: &[u8],
    analysis: &AnalysisContext<'_>,
) -> Vec<TrxFinding> {
    let uses = extract_uses_clauses(tree.root_node(), source);
    let mut all_findings = Vec::new();

    for cfg in analysis.cfgs.values() {
        let proc_node = match find_proc_node_for_cfg(tree, source, cfg) {
            Some(n) => n,
            None => continue,
        };
        let findings = analyze_procedure(proc_node, source, analysis.project, &uses);
        all_findings.extend(findings);
    }

    all_findings
}

/// Convert a byte offset to 1-based (line, column).
fn byte_offset_to_line_col(source: &[u8], offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let mut line = 1;
    let mut last_newline = 0;
    for (i, &b) in source[..offset].iter().enumerate() {
        if b == b'\n' {
            line += 1;
            last_newline = i + 1;
        }
    }
    let col = offset - last_newline + 1;
    (line, col)
}

// ═══════════════════════════════════════════════════════════════
// Umbrella rule: transaction
// ═══════════════════════════════════════════════════════════════

pub struct TransactionRule {
    meta: RuleMeta,
}

impl Default for TransactionRule {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionRule {
    pub fn new() -> Self {
        Self {
            meta: RuleMeta {
                id: "transaction",
                name: "Transaction Management",
                category: RuleCategory::ResourceManagement,
                default_severity: Severity::Error,
                description: "Detects incorrect transaction management patterns.",
            },
        }
    }
}

impl Rule for TransactionRule {
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
        let findings = run_transaction_analysis(tree, source, analysis);
        for finding in &findings {
            let (line, col) = byte_offset_to_line_col(source, finding.byte_range.start);
            let (end_line, end_col) = byte_offset_to_line_col(source, finding.byte_range.end);
            ctx.report(Diagnostic {
                rule_id: finding.rule_id.to_string(),
                severity: finding.severity,
                message: finding.message.clone(),
                line,
                column: col,
                end_line,
                end_column: end_col,
                help: Some(finding.help.clone()),
            });
        }
    }
}
