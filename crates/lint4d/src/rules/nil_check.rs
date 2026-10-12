use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

use cfg_core::BlockId;
use cfg_core::summary::ProcId;
use cfg_core::types::Cfg;
use cfg_pascal::cfg_core;
use pascal_core::node_kind as K;
use petgraph::Direction;
use petgraph::visit::EdgeRef;
use tree_sitter::{Node, Tree};

use crate::cfg::analysis::AnalysisContext;
use crate::dcu::{ProjectContext, TypeKind};
use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::helpers::{
    byte_offset_to_line_col, extract_type_from_decl_arg, extract_type_from_decl_var,
    extract_uses_clauses, find_def_proc_at, has_out_modifier, node_text,
};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

// ─── Thread-local cache ───────────────────────────────────────────────────────

thread_local! {
    /// Per-function answer of `can_function_return_nil`, `None` meaning
    /// unanalyzable, so every call to a function gets the same answer.
    static FN_RETURN_CACHE: RefCell<HashMap<String, Option<bool>>> = RefCell::new(HashMap::new());
}

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
        let unit_name =
            crate::rules::helpers::extract_unit_name(tree.root_node(), source).unwrap_or_default();

        // Clear function return cache for each file
        FN_RETURN_CACHE.with(|cache| cache.borrow_mut().clear());

        for cfg in analysis.cfgs.values() {
            let def_proc = match find_def_proc_at(tree.root_node(), cfg.byte_range.start) {
                Some(n) => n,
                None => continue,
            };
            let nillable_vars = build_nillable_vars(def_proc, source, analysis.project, &uses);
            if nillable_vars.is_empty() {
                continue;
            }
            analyze_cfg(
                cfg,
                source,
                &nillable_vars,
                tree.root_node(),
                &analysis.cfgs,
                &unit_name,
                ctx,
            );
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
            if child.kind() == K::DECL_ARGS {
                let mut args_cursor = child.walk();
                for arg in child.children(&mut args_cursor) {
                    if arg.kind() != K::DECL_ARG {
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
                        if c.kind() == K::IDENTIFIER
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
        if child.kind() != K::DECL_VARS {
            continue;
        }
        let mut vars_cursor = child.walk();
        for decl_var in child.children(&mut vars_cursor) {
            if decl_var.kind() != K::DECL_VAR {
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
                if c.kind() == K::IDENTIFIER
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

// ─── CFG analysis ────────────────────────────────────────────────────────────

/// Nil state: true = safe (proven non-nil), false = unknown (potentially nil).
type NilState = HashMap<String, bool>;

/// Analyze a single procedure's CFG for unchecked nil uses.
fn analyze_cfg(
    cfg: &Cfg,
    source: &[u8],
    nillable_vars: &HashMap<String, VarInfo>,
    root: Node,
    all_cfgs: &HashMap<ProcId, Cfg>,
    unit_name: &str,
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
            let text = pascal_core::decode_bytes(&source[start..end]);

            process_statement(
                text.as_ref(),
                start,
                source,
                &mut state,
                nillable_vars,
                &mut reported,
                root,
                all_cfgs,
                unit_name,
                ctx,
            );
        }

        // Detect nil comparison in the last statement for branch-aware propagation.
        let nil_compare = block.stmts.last().and_then(|s| {
            let start = s.byte_range.start;
            let end = s.byte_range.end.min(source.len());
            parse_nil_comparison(pascal_core::decode_bytes(&source[start..end]).as_ref())
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
                    EdgeKind::ConditionalFalse if !cmp.is_not_nil => {
                        propagated.insert(cmp.var_name.clone(), true);
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
#[allow(clippy::too_many_arguments)]
fn process_statement(
    text: &str,
    byte_offset: usize,
    source: &[u8],
    state: &mut NilState,
    nillable_vars: &HashMap<String, VarInfo>,
    reported: &mut HashSet<(String, usize)>,
    root: Node,
    all_cfgs: &HashMap<ProcId, Cfg>,
    unit_name: &str,
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
                // Check if RHS is a function call that can return nil
                let func_name = extract_function_name(rhs_trimmed);
                if let Some(ref fname) = func_name {
                    let argc = call_argument_count(rhs_trimmed, fname.len());
                    if let Some(true) =
                        can_function_return_nil(fname, argc, root, source, all_cfgs, unit_name)
                    {
                        state.insert(var.clone(), false); // Function can return nil
                    } else {
                        state.insert(var.clone(), true); // Safe or can't be analyzed
                    }
                } else {
                    state.insert(var.clone(), true);
                }
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

// ─── Function return nil analysis ────────────────────────────────────────────

/// Extract function name from a call expression like `somefunc(args)` or `somefunc`.
fn extract_function_name(rhs: &str) -> Option<String> {
    let rhs = rhs.trim_end_matches(';').trim();
    // Skip constructor calls and dot expressions (method calls)
    if rhs.contains('.') {
        return None;
    }
    let name = rhs.split('(').next()?.trim();
    if is_identifier(name) {
        Some(name.to_string())
    } else {
        None
    }
}

/// The number of arguments of the call starting with a `name_len`-byte name:
/// 0 without parentheses, `None` when the argument list is not balanced.
fn call_argument_count(call: &str, name_len: usize) -> Option<usize> {
    let rest = call.get(name_len..)?.trim_start();
    let Some(args) = rest.strip_prefix('(') else {
        return Some(0);
    };
    let mut depth = 0usize;
    let mut in_string = false;
    let mut commas = 0;
    let mut empty = true;
    for character in args.chars() {
        if in_string {
            in_string = character != '\'';
            continue;
        }
        match character {
            '\'' => in_string = true,
            '(' | '[' => depth += 1,
            ')' if depth == 0 => return Some(if empty { 0 } else { commas + 1 }),
            ')' | ']' => depth = depth.checked_sub(1)?,
            ',' if depth == 0 => commas += 1,
            _ => {}
        }
        if !character.is_whitespace() {
            empty = false;
        }
    }
    None
}

/// Analyze whether a function defined in the current file can return nil
/// when called with `argc` arguments (`None`: unknown, every overload is a
/// candidate).
fn can_function_return_nil(
    func_name: &str,
    argc: Option<usize>,
    root: Node,
    source: &[u8],
    cfgs: &HashMap<ProcId, Cfg>,
    unit_name: &str,
) -> Option<bool> {
    let func_lower = match argc {
        Some(argc) => format!("{}/{argc}", func_name.to_lowercase()),
        None => func_name.to_lowercase(),
    };

    let cached = FN_RETURN_CACHE.with(|cache| cache.borrow().get(&func_lower).copied());
    if let Some(result) = cached {
        return result;
    }

    // Insert sentinel to break recursion (assume can return nil)
    FN_RETURN_CACHE.with(|cache| cache.borrow_mut().insert(func_lower.clone(), Some(true)));

    let result = function_return_nil_uncached(func_name, argc, root, source, cfgs, unit_name);
    FN_RETURN_CACHE.with(|cache| cache.borrow_mut().insert(func_lower, result));
    result
}

fn function_return_nil_uncached(
    func_name: &str,
    argc: Option<usize>,
    root: Node,
    source: &[u8],
    cfgs: &HashMap<ProcId, Cfg>,
    unit_name: &str,
) -> Option<bool> {
    // Find the function's CFGs by matching proc name (case-insensitive)
    // qualified_name may be just "FuncName" or "TClass.MethodName".
    // Overloads are selected by argument count, as dcc's first filter does;
    // the answer is known only when every remaining candidate agrees.
    let mut declarations = Vec::new();
    collect_decl_procs(root, func_name, source, &mut declarations);
    let mut result = None;
    for (pid, cfg) in cfgs {
        // Only consider functions from the same unit
        if !pid.unit_name.eq_ignore_ascii_case(unit_name)
            || !pid.qualified_name.eq_ignore_ascii_case(func_name)
        {
            continue;
        }

        // Verify it's a function (has return type) by checking the AST
        let def_proc = find_def_proc_at(root, cfg.byte_range.start)?;
        if let Some(argc) = argc {
            let arity = def_proc
                .child_by_field_name("header")
                .map(|header| declared_arity(header, &declarations, source))?;
            if !arity.accepts(argc) {
                continue;
            }
        }
        if !is_function_def(def_proc, source) {
            return None; // Procedures don't have return values
        }

        let can_return_nil = analyze_return_nil(cfg, source);
        if result.is_some_and(|known| known != can_return_nil) {
            return None;
        }
        result = Some(can_return_nil);
    }
    result
}

/// Parameter counts of a routine declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Arity {
    /// Parameters without a default value.
    required: usize,
    total: usize,
    varargs: bool,
}

impl Arity {
    fn of(header: Node) -> Arity {
        let mut arity = Arity {
            required: 0,
            total: 0,
            varargs: false,
        };
        let mut cursor = header.walk();
        for attribute in header.children_by_field_name("attribute", &mut cursor) {
            let mut attribute_cursor = attribute.walk();
            arity.varargs |= attribute
                .children(&mut attribute_cursor)
                .any(|child| child.kind() == "kVarargs");
        }
        let Some(args) = header.child_by_field_name("args") else {
            return arity;
        };
        let mut args_cursor = args.walk();
        for arg in args
            .named_children(&mut args_cursor)
            .filter(|arg| arg.kind() == K::DECL_ARG)
        {
            let mut name_cursor = arg.walk();
            let names = arg
                .children_by_field_name("name", &mut name_cursor)
                .filter(|name| name.kind() == K::IDENTIFIER)
                .count();
            arity.total += names;
            if arg.child_by_field_name("defaultValue").is_none() {
                arity.required += names;
            }
        }
        arity
    }

    fn accepts(self, argc: usize) -> bool {
        self.required <= argc && (self.varargs || argc <= self.total)
    }
}

/// Every routine declaration (`declProc`) named `name` in the file.
fn collect_decl_procs<'tree>(
    node: Node<'tree>,
    name: &str,
    source: &[u8],
    found: &mut Vec<Node<'tree>>,
) {
    if node.kind() == K::DECL_PROC
        && node
            .child_by_field_name("name")
            .is_some_and(|ident| node_text(ident, source).eq_ignore_ascii_case(name))
    {
        found.push(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_decl_procs(child, name, source, found);
    }
}

/// The arity of a routine definition's header. Default values are often
/// written only in the interface or forward declaration, so the required
/// count is the smallest among same-named declarations with the same
/// parameter types, or, when none has them, with the same parameter count:
/// a looser bound only keeps more candidates.
fn declared_arity(header: Node, declarations: &[Node], source: &[u8]) -> Arity {
    let mut arity = Arity::of(header);
    let types = parameter_types(header, source);
    let same_types: Vec<_> = declarations
        .iter()
        .filter(|declaration| parameter_types(**declaration, source) == types)
        .collect();
    let matching: Vec<_> = if same_types.len() > 1 {
        same_types
    } else {
        declarations.iter().collect()
    };
    for declaration in matching {
        let other = Arity::of(*declaration);
        if other.total == arity.total {
            arity.required = arity.required.min(other.required);
            arity.varargs |= other.varargs;
        }
    }
    arity
}

/// Each parameter's type text, lowercased without whitespace.
fn parameter_types(header: Node, source: &[u8]) -> Vec<String> {
    let mut types = Vec::new();
    let Some(args) = header.child_by_field_name("args") else {
        return types;
    };
    let mut args_cursor = args.walk();
    for arg in args
        .named_children(&mut args_cursor)
        .filter(|arg| arg.kind() == K::DECL_ARG)
    {
        let type_text: String = arg
            .child_by_field_name("type")
            .map(|node| node_text(node, source))
            .unwrap_or_default()
            .chars()
            .filter(|character| !character.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect();
        let mut name_cursor = arg.walk();
        for _ in arg
            .children_by_field_name("name", &mut name_cursor)
            .filter(|name| name.kind() == K::IDENTIFIER)
        {
            types.push(type_text.clone());
        }
    }
    types
}

/// Check if a `defProc` is a function (has a return type).
fn is_function_def(def_proc: Node, source: &[u8]) -> bool {
    if let Some(header) = def_proc.child_by_field_name("header") {
        let header_text = node_text(header, source).to_lowercase();
        header_text.starts_with("function")
    } else {
        false
    }
}

/// Analyze a function's CFG to determine if it can return nil.
///
/// Returns true if any path to the exit block has Result potentially nil.
fn analyze_return_nil(cfg: &Cfg, source: &[u8]) -> bool {
    let graph = &cfg.graph;
    let entry = cfg.entry;
    let exit = cfg.exit;

    // Track Result state: false = potentially nil, true = assigned non-nil
    let mut block_result_state: HashMap<BlockId, bool> = HashMap::new();
    let mut worklist: VecDeque<BlockId> = VecDeque::new();

    block_result_state.insert(entry, false); // Result starts as nil
    worklist.push_back(entry);

    while let Some(block_id) = worklist.pop_front() {
        let result_safe = block_result_state.get(&block_id).copied().unwrap_or(false);

        let block = &graph[block_id.index()];
        let mut state = result_safe;

        for stmt in &block.stmts {
            let start = stmt.byte_range.start;
            let end = stmt.byte_range.end.min(source.len());
            if start >= end {
                continue;
            }
            let text = pascal_core::decode_bytes(&source[start..end]);
            let lower = text.trim().to_lowercase();

            if let Some((var, rhs)) = parse_assignment_parts(&lower)
                && var == "result"
            {
                let rhs_trimmed = rhs.trim().trim_end_matches(';').trim();
                if rhs_trimmed == "nil" {
                    state = false;
                } else if is_constructor_pattern(rhs_trimmed) {
                    state = true;
                } else {
                    state = true; // Other assignments conservatively non-nil
                }
            }
        }

        for edge in graph.edges_directed(block_id.index(), Direction::Outgoing) {
            let successor = BlockId::from(edge.target());
            // A raising path leaves the function without returning a value
            if successor == exit && *edge.weight() == cfg_core::EdgeKind::ExceptionThrow {
                continue;
            }
            let existing = block_result_state.get(&successor);

            // Conservative: Result is safe only if ALL paths agree
            let merged = match existing {
                Some(&existing_safe) => existing_safe && state,
                None => state,
            };

            let changed = existing.map(|&e| e != merged).unwrap_or(true);
            if changed {
                block_result_state.insert(successor, merged);
                worklist.push_back(successor);
            }
        }
    }

    let exit_safe = block_result_state.get(&exit).copied().unwrap_or(false);
    !exit_safe // Returns true if Result can be nil
}

// ─── Pattern matching helpers ────────────────────────────────────────────────

struct NilComparison {
    var_name: String,
    is_not_nil: bool,
}

fn parse_nil_comparison(text: &str) -> Option<NilComparison> {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();
    let lower = lower
        .strip_prefix("if ")
        .map_or(lower.as_str(), str::trim)
        .trim_end_matches(" then")
        .trim();

    if let Some(inner) = extract_assigned_arg(lower)
        && is_identifier(&inner)
    {
        let is_negated = lower.trim_start().starts_with("not ");
        return Some(NilComparison {
            var_name: inner,
            is_not_nil: !is_negated,
        });
    }

    if let Some(var) = extract_nil_compare(lower, "<>") {
        return Some(NilComparison {
            var_name: var,
            is_not_nil: true,
        });
    }

    if let Some(var) = extract_nil_compare(lower, "=") {
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
