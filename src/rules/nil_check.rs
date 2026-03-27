use std::collections::{HashMap, HashSet, VecDeque};

use cfg_core::types::Cfg;
use cfg_core::BlockId;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use tree_sitter::{Node, Tree};

use crate::cfg::analysis::AnalysisContext;
use crate::dcu::{ProjectContext, TypeKind};
use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::helpers::{
    extract_type_from_decl_arg, extract_type_from_decl_var, extract_uses_clauses, has_out_modifier,
    node_text,
};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

// ─── Rule definition ─────────────────────────────────────────────────────────

pub struct UncheckedNilRule {
    meta: RuleMeta,
}

impl Default for UncheckedNilRule {
    fn default() -> Self {
        Self::new()
    }
}

impl UncheckedNilRule {
    pub fn new() -> Self {
        UncheckedNilRule {
            meta: RuleMeta {
                id: "unchecked-nil",
                name: "Unchecked Nil",
                category: RuleCategory::NullSafety,
                default_severity: Severity::Warning,
                description: "Detects nillable variables used without a preceding nil check.",
                enabled_by_default: false,
            },
        }
    }
}

impl Rule for UncheckedNilRule {
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
        let uses = extract_uses_clauses(tree.root_node(), source);
        for cfg in analysis.cfgs.values() {
            let def_proc = match find_def_proc_at(tree.root_node(), cfg.byte_range.start) {
                Some(n) => n,
                None => continue,
            };
            let nillable_vars = build_nillable_vars(def_proc, source, analysis.project, &uses);
            if nillable_vars.is_empty() {
                continue;
            }
            analyze_cfg(cfg, source, &nillable_vars, ctx);
        }
    }
}

// ─── AST helpers ─────────────────────────────────────────────────────────────

/// Metadata about a tracked nillable variable.
#[derive(Debug, Clone)]
struct VarInfo {
    declared_name: String,
    is_param: bool,
}

/// Check if a type is nillable based on DCU type resolution.
fn is_nillable_type(type_name: &str, project: &ProjectContext, uses: &[String]) -> bool {
    match project.resolve_type(type_name, uses) {
        Some(ty) => matches!(
            ty.kind,
            TypeKind::Class | TypeKind::Interface | TypeKind::Pointer | TypeKind::Procedural
        ),
        None => false,
    }
}

/// Collect all nillable parameters and local variables from a `defProc` node.
fn build_nillable_vars(
    def_proc: Node,
    source: &[u8],
    project: &ProjectContext,
    uses: &[String],
) -> HashMap<String, VarInfo> {
    let mut vars = HashMap::new();

    // Collect parameters from header
    if let Some(header) = def_proc.child_by_field_name("header") {
        let mut header_cursor = header.walk();
        for child in header.children(&mut header_cursor) {
            if child.kind() == "declArgs" {
                let mut args_cursor = child.walk();
                for arg in child.children(&mut args_cursor) {
                    if arg.kind() != "declArg" {
                        continue;
                    }
                    if has_out_modifier(arg) {
                        continue;
                    }
                    let type_name = match extract_type_from_decl_arg(arg, source) {
                        Some(t) => t,
                        None => continue,
                    };
                    if !is_nillable_type(&type_name, project, uses) {
                        continue;
                    }
                    let arg_count = arg.child_count();
                    for i in 0..arg_count {
                        let c = match arg.child(i) {
                            Some(c) => c,
                            None => continue,
                        };
                        if c.kind() == "identifier"
                            && arg.field_name_for_child(i as u32) == Some("name")
                        {
                            let name = node_text(c, source);
                            vars.insert(
                                name.to_lowercase(),
                                VarInfo {
                                    declared_name: name,
                                    is_param: true,
                                },
                            );
                        }
                    }
                }
            }
        }
    }

    // Collect local variables from declVars
    let mut proc_cursor = def_proc.walk();
    for child in def_proc.children(&mut proc_cursor) {
        if child.kind() != "declVars" {
            continue;
        }
        let mut vars_cursor = child.walk();
        for decl_var in child.children(&mut vars_cursor) {
            if decl_var.kind() != "declVar" {
                continue;
            }
            let type_name = match extract_type_from_decl_var(decl_var, source) {
                Some(t) => t,
                None => continue,
            };
            if !is_nillable_type(&type_name, project, uses) {
                continue;
            }
            let dv_count = decl_var.child_count();
            for i in 0..dv_count {
                let c = match decl_var.child(i) {
                    Some(c) => c,
                    None => continue,
                };
                if c.kind() == "identifier"
                    && decl_var.field_name_for_child(i as u32) == Some("name")
                {
                    let name = node_text(c, source);
                    vars.insert(
                        name.to_lowercase(),
                        VarInfo {
                            declared_name: name,
                            is_param: false,
                        },
                    );
                }
            }
        }
    }

    vars
}

/// Find the `defProc` AST node whose start byte matches the given offset.
fn find_def_proc_at(root: Node, start_byte: usize) -> Option<Node> {
    if root.kind() == "defProc" && root.start_byte() == start_byte {
        return Some(root);
    }
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if let Some(found) = find_def_proc_at(child, start_byte) {
            return Some(found);
        }
    }
    None
}

// ─── CFG analysis ────────────────────────────────────────────────────────────

/// Nil state: true = safe (proven non-nil), false = unknown (potentially nil).
type NilState = HashMap<String, bool>;

/// Analyze a single procedure's CFG for unchecked nil uses.
fn analyze_cfg(
    cfg: &Cfg,
    source: &[u8],
    nillable_vars: &HashMap<String, VarInfo>,
    ctx: &mut LintContext,
) {
    let graph = &cfg.graph;
    let entry = cfg.entry;

    let initial_state: NilState = nillable_vars.keys().map(|k| (k.clone(), false)).collect();

    let mut block_entry_state: HashMap<BlockId, NilState> = HashMap::new();
    let mut reported: HashSet<(String, usize)> = HashSet::new();
    let mut worklist: VecDeque<BlockId> = VecDeque::new();

    block_entry_state.insert(entry, initial_state);
    worklist.push_back(entry);

    while let Some(block_id) = worklist.pop_front() {
        let entry_state = block_entry_state
            .get(&block_id)
            .cloned()
            .unwrap_or_default();

        let block = &graph[block_id.index()];
        let mut state = entry_state;

        for stmt in &block.stmts {
            let start = stmt.byte_range.start;
            let end = stmt.byte_range.end.min(source.len());
            if start >= end {
                continue;
            }
            let text = match std::str::from_utf8(&source[start..end]) {
                Ok(t) => t,
                Err(_) => continue,
            };

            process_statement(
                text,
                start,
                source,
                &mut state,
                nillable_vars,
                &mut reported,
                ctx,
            );
        }

        // Detect nil comparison in the last statement for branch-aware propagation.
        let nil_compare = block.stmts.last().and_then(|s| {
            let start = s.byte_range.start;
            let end = s.byte_range.end.min(source.len());
            std::str::from_utf8(&source[start..end])
                .ok()
                .and_then(parse_nil_comparison)
        });

        // Propagate state to successors via edges.
        for edge in graph.edges_directed(block_id.index(), Direction::Outgoing) {
            let successor = BlockId::from(edge.target());
            let edge_kind = edge.weight();

            let mut propagated = state.clone();

            if let Some(ref cmp) = nil_compare {
                use cfg_core::EdgeKind;
                match edge_kind {
                    EdgeKind::ConditionalTrue => {
                        if cmp.is_not_nil {
                            propagated.insert(cmp.var_name.clone(), true);
                        }
                    }
                    EdgeKind::ConditionalFalse => {
                        if !cmp.is_not_nil {
                            propagated.insert(cmp.var_name.clone(), true);
                        }
                    }
                    _ => {}
                }
            }

            let existing = block_entry_state.get(&successor);
            let merged = match existing {
                Some(existing_state) => merge_nil_states(existing_state, &propagated),
                None => propagated,
            };

            let changed = existing.map(|e| e != &merged).unwrap_or(true);
            if changed {
                block_entry_state.insert(successor, merged);
                worklist.push_back(successor);
            }
        }
    }
}

/// Conservative merge: variable is safe only if safe in ALL predecessors.
fn merge_nil_states(a: &NilState, b: &NilState) -> NilState {
    let mut merged = HashMap::new();
    for (var, &safe_a) in a {
        let safe_b = b.get(var).copied().unwrap_or(false);
        merged.insert(var.clone(), safe_a && safe_b);
    }
    for (var, &safe_b) in b {
        if !a.contains_key(var) {
            merged.insert(var.clone(), safe_b);
        }
    }
    merged
}

// ─── Statement processing ────────────────────────────────────────────────────

/// Process a single statement to detect nil-relevant patterns and flag uses.
fn process_statement(
    text: &str,
    byte_offset: usize,
    source: &[u8],
    state: &mut NilState,
    nillable_vars: &HashMap<String, VarInfo>,
    reported: &mut HashSet<(String, usize)>,
    ctx: &mut LintContext,
) {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();

    // 1. RaiseIfNil(var, ...) — marks var as safe
    if let Some(var) = parse_raiseifnil_arg(&lower) {
        if state.contains_key(&var) {
            state.insert(var, true);
        }
        return;
    }

    // 2. FreeAndNil(var) — resets to unknown
    if let Some(var) = parse_freeandnil_arg(&lower) {
        if state.contains_key(&var) {
            state.insert(var, false);
        }
        return;
    }

    // 3. var.Free / var.Destroy — resets to unknown
    if let Some(var) = parse_free_call(&lower) {
        if state.contains_key(&var) {
            state.insert(var, false);
        }
        return;
    }

    // 4. Assignment: var := ...
    if let Some((var, rhs)) = parse_assignment_parts(&lower) {
        if state.contains_key(&var) {
            let rhs_trimmed = rhs.trim().trim_end_matches(';').trim();
            if rhs_trimmed == "nil" {
                state.insert(var.clone(), false);
            } else if is_constructor_pattern(rhs_trimmed) {
                state.insert(var.clone(), true);
            } else {
                // Non-constructor assignment: treat as safe for now.
                // Function return analysis added in Task 10.
                state.insert(var.clone(), true);
            }
        }
        check_uses_in_text(
            &lower,
            byte_offset,
            source,
            state,
            nillable_vars,
            reported,
            ctx,
        );
        return;
    }

    // 5. Check for uses of unchecked variables
    check_uses_in_text(
        &lower,
        byte_offset,
        source,
        state,
        nillable_vars,
        reported,
        ctx,
    );
}

/// Check if text contains a use (dot-access or dereference) of an unchecked variable.
fn check_uses_in_text(
    lower: &str,
    byte_offset: usize,
    source: &[u8],
    state: &NilState,
    nillable_vars: &HashMap<String, VarInfo>,
    reported: &mut HashSet<(String, usize)>,
    ctx: &mut LintContext,
) {
    if parse_nil_comparison(lower).is_some() {
        return;
    }
    if is_safe_call(lower) {
        return;
    }

    for (var_key, &is_safe) in state.iter() {
        if is_safe {
            continue;
        }
        if contains_dot_use(lower, var_key) || contains_deref(lower, var_key) {
            let (line, col) = byte_offset_to_line_col(source, byte_offset);
            let report_key = (var_key.clone(), line);
            if reported.insert(report_key) {
                let info = &nillable_vars[var_key];
                let kind = if info.is_param {
                    "Parameter"
                } else {
                    "Variable"
                };
                ctx.report(Diagnostic {
                    rule_id: "unchecked-nil".to_string(),
                    severity: Severity::Warning,
                    message: format!(
                        "{} '{}' may be nil when accessed. Add a nil check before use.",
                        kind, info.declared_name
                    ),
                    line,
                    column: col,
                    end_line: line,
                    end_column: col + lower.trim().len(),
                    help: Some(format!(
                        "Add 'RaiseIfNil({0}, ''{0}'')' or check 'if {0} <> nil' before use.",
                        info.declared_name
                    )),
                    scope: None,
                });
            }
        }
    }
}

// ─── Pattern matching helpers ────────────────────────────────────────────────

struct NilComparison {
    var_name: String,
    is_not_nil: bool,
}

fn parse_nil_comparison(text: &str) -> Option<NilComparison> {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();

    if let Some(inner) = extract_assigned_arg(&lower) {
        if is_identifier(&inner) {
            let is_negated = lower.trim_start().starts_with("not ");
            return Some(NilComparison {
                var_name: inner,
                is_not_nil: !is_negated,
            });
        }
    }

    if let Some(var) = extract_nil_compare(&lower, "<>") {
        return Some(NilComparison {
            var_name: var,
            is_not_nil: true,
        });
    }

    if let Some(var) = extract_nil_compare(&lower, "=") {
        return Some(NilComparison {
            var_name: var,
            is_not_nil: false,
        });
    }

    None
}

fn extract_assigned_arg(lower: &str) -> Option<String> {
    let s = lower.trim();
    let s = s.strip_prefix("not ").unwrap_or(s).trim();
    let s = s.strip_prefix("assigned(")?;
    let s = s.strip_suffix(')')?;
    let arg = s.trim();
    if arg.is_empty() {
        return None;
    }
    Some(arg.to_string())
}

fn extract_nil_compare(lower: &str, op: &str) -> Option<String> {
    let parts: Vec<&str> = lower.splitn(2, op).collect();
    if parts.len() != 2 {
        return None;
    }
    let lhs = parts[0].trim();
    let rhs = parts[1].trim();

    if rhs == "nil" && is_identifier(lhs) {
        return Some(lhs.to_string());
    }
    if lhs == "nil" && is_identifier(rhs) {
        return Some(rhs.to_string());
    }
    None
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.chars().next().unwrap().is_ascii_digit()
}

fn parse_raiseifnil_arg(lower: &str) -> Option<String> {
    let stripped = lower.trim().trim_end_matches(';').trim();
    let inner = stripped.strip_prefix("raiseifnil(")?;
    let inner = inner.strip_suffix(')')?;
    let first_arg = inner.split(',').next()?.trim();
    if is_identifier(first_arg) {
        Some(first_arg.to_string())
    } else {
        None
    }
}

fn parse_freeandnil_arg(lower: &str) -> Option<String> {
    let stripped = lower.trim().trim_end_matches(';').trim();
    let inner = stripped
        .strip_prefix("freeandnil(")?
        .strip_suffix(')')?
        .trim();
    if is_identifier(inner) {
        Some(inner.to_string())
    } else {
        None
    }
}

fn parse_free_call(lower: &str) -> Option<String> {
    let stripped = lower.trim().trim_end_matches(';').trim();
    let var = stripped
        .strip_suffix(".free")
        .or_else(|| stripped.strip_suffix(".destroy"))?
        .trim();
    if is_identifier(var) {
        Some(var.to_string())
    } else {
        None
    }
}

fn parse_assignment_parts(lower: &str) -> Option<(String, String)> {
    let idx = lower.find(":=")?;
    let var = lower[..idx].trim();
    if !is_identifier(var) {
        return None;
    }
    let rhs = lower[idx + 2..].to_string();
    Some((var.to_string(), rhs))
}

fn is_constructor_pattern(rhs: &str) -> bool {
    let rhs = rhs.trim_end_matches(';').trim();
    if let Some(dot_pos) = rhs.find('.') {
        let after_dot = rhs[dot_pos + 1..].trim();
        let method = after_dot.split('(').next().unwrap_or(after_dot).trim();
        method.eq_ignore_ascii_case("create")
    } else {
        false
    }
}

fn contains_dot_use(lower: &str, var_name: &str) -> bool {
    let pattern = format!("{}.", var_name);
    if let Some(pos) = lower.find(&pattern) {
        if pos > 0 {
            let before = lower.as_bytes()[pos - 1];
            if before.is_ascii_alphanumeric() || before == b'_' {
                return false;
            }
        }
        true
    } else {
        false
    }
}

fn contains_deref(lower: &str, var_name: &str) -> bool {
    let pattern = format!("{}^", var_name);
    if let Some(pos) = lower.find(&pattern) {
        if pos > 0 {
            let before = lower.as_bytes()[pos - 1];
            if before.is_ascii_alphanumeric() || before == b'_' {
                return false;
            }
        }
        true
    } else {
        false
    }
}

fn is_safe_call(lower: &str) -> bool {
    let trimmed = lower.trim().trim_end_matches(';').trim();
    trimmed.starts_with("assigned(")
        || trimmed.starts_with("raiseifnil(")
        || trimmed.starts_with("freeandnil(")
}

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
