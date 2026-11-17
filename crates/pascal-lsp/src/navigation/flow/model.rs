use super::super::{
    AssistanceBudget, Candidate, Document, NavigationIndex, Origin, ParameterMode, ResolutionState,
    RoutineKind, SourceSpan, Span, Symbol, SymbolKind, TypeKind, TypeRef, TypeShape,
    canonical_name, check_navigation_cancel, node_text,
};
use cfg_pascal::cfg_core::types::{BlockId, Cfg, EdgeKind};
use lsp_types::Url;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::AtomicBool;
use tree_sitter::Node;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalVarParameterMode {
    Value,
    Var,
    Out,
    Const,
    ConstRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalKind {
    Local,
    Result,
    Parameter(LocalVarParameterMode),
    LoopVar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VarType {
    Scalar,
    Managed,
    Record,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Write,
    ReadWrite,
    OutArg,
    VarArg,
    AddressOf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlowExclusion {
    Recovery,
    UnknownConditional,
    Asm,
    Goto,
    UnresolvedIdentity,
    Budget,
    InlineVariable,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalVar {
    pub(crate) name: String,
    pub(crate) decl_span: SourceSpan,
    pub(crate) kind: LocalKind,
    pub(crate) ty: VarType,
    pub(crate) address_taken: bool,
    pub(crate) captured: bool,
    pub(crate) absolute: bool,
    pub(crate) possible_with_target: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct VarOccurrence {
    pub(crate) var: usize,
    pub(crate) span: SourceSpan,
    pub(crate) access: Access,
    pub(crate) block: BlockId,
    pub(crate) order: usize,
}

#[derive(Debug)]
pub(crate) struct RoutineFlow<'a> {
    pub(crate) name: String,
    pub(crate) cfg: Cfg,
    pub(crate) locals: Vec<LocalVar>,
    pub(crate) occurrences: Vec<VarOccurrence>,
    pub(crate) excluded: Option<FlowExclusion>,
    pub(crate) node: Node<'a>,
}

pub(crate) fn routine_flows<'a>(
    index: &NavigationIndex,
    uri: &Url,
    document: &'a Document,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<RoutineFlow<'a>>, String> {
    check_navigation_cancel(cancel)?;
    let root = document.tree.root_node();
    budget.require_work(root.descendant_count().saturating_add(1), cancel)?;
    budget.require_owned_bytes(document.source.len(), cancel)?;

    let mut cfgs = cfg_pascal::build_file_cfgs(&document.tree, document.source.as_bytes());
    budget.require_work(cfgs.len(), cancel)?;
    budget.require_owned_bytes(
        cfgs.len()
            .saturating_mul(std::mem::size_of::<Cfg>() + std::mem::size_of::<usize>() * 2),
        cancel,
    )?;
    let mut cfgs_by_range = HashMap::with_capacity(cfgs.len());
    for cfg in cfgs.drain(..) {
        cfgs_by_range.insert((cfg.byte_range.start, cfg.byte_range.end), cfg);
    }

    let routines = collect_nodes(root, cancel, budget)?
        .into_iter()
        .filter(|node| node.kind() == "defProc" && node.child_by_field_name("body").is_some())
        .collect::<Vec<_>>();
    let mut flows = Vec::with_capacity(routines.len());
    for node in routines {
        check_navigation_cancel(cancel)?;
        let Some(cfg) = cfgs_by_range.remove(&(node.start_byte(), node.end_byte())) else {
            continue;
        };
        budget.require_work(1, cancel)?;
        flows.push(build_routine_flow(
            index, uri, document, node, cfg, cancel, budget,
        )?);
    }
    Ok(flows)
}

#[derive(Debug, Clone)]
struct LocalBinding {
    var: usize,
    candidate: Candidate,
    symbol_scope: usize,
    synthetic_result: bool,
}

#[derive(Debug)]
struct PendingOccurrence {
    var: usize,
    span: SourceSpan,
    access: Access,
    block: BlockId,
    statement_start: usize,
    semantic_order: u8,
}

#[derive(Debug)]
struct CollectedLocals {
    locals: Vec<LocalVar>,
    bindings: Vec<LocalBinding>,
    routine_scope: Option<usize>,
}

fn build_routine_flow<'a>(
    index: &NavigationIndex,
    uri: &Url,
    document: &'a Document,
    node: Node<'a>,
    cfg: Cfg,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<RoutineFlow<'a>, String> {
    let body = node
        .child_by_field_name("body")
        .expect("only executable routine nodes have a body");
    let all_nodes = collect_owned_nodes(node, node.id(), cancel, budget)?;
    let body_nodes = all_nodes
        .iter()
        .copied()
        .filter(|candidate| {
            candidate.start_byte() >= body.start_byte() && candidate.end_byte() <= body.end_byte()
        })
        .collect::<Vec<_>>();
    let nested_ranges = collect_nested_ranges(node, cancel, budget)?;
    let routine_symbol_index = document.symbols.iter().position(|symbol| {
        symbol.kind == SymbolKind::Routine
            && symbol.origin == Origin::Definition
            && symbol.declaration_span == Span::from_node(node)
    });
    let mut excluded = exclusion_for_routine(document, node, &body_nodes, &nested_ranges, &cfg);
    let CollectedLocals {
        mut locals,
        bindings,
        routine_scope,
    } = collect_locals(
        document,
        uri,
        node,
        routine_symbol_index,
        &nested_ranges,
        &all_nodes,
        cancel,
        budget,
    )?;
    if routine_scope.is_none() {
        note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
    }

    let local_names = locals
        .iter()
        .map(|local| canonical_name(&local.name))
        .collect::<HashSet<_>>();
    mark_loop_variables(
        index,
        uri,
        document,
        &body_nodes,
        &mut locals,
        &bindings,
        &mut excluded,
        cancel,
        budget,
    )?;
    mark_possible_with_targets(document, &body_nodes, &mut locals, &local_names);
    mark_captured_locals(
        index,
        uri,
        document,
        node,
        &nested_ranges,
        &mut locals,
        &bindings,
        &mut excluded,
        cancel,
        budget,
    )?;

    let call_argument_accesses = call_argument_accesses(
        index,
        uri,
        document,
        &body_nodes,
        &local_names,
        cancel,
        budget,
    )?;
    let mut pending = Vec::new();
    let result_local = locals
        .iter()
        .position(|local| local.kind == LocalKind::Result);
    let routine_candidate = routine_symbol_index.map(|index| Candidate {
        uri: uri.clone(),
        index,
    });
    for identifier in body_nodes
        .iter()
        .copied()
        .filter(|candidate| candidate.kind() == "identifier")
    {
        check_navigation_cancel(cancel)?;
        let name = node_text(identifier, &document.source);
        let key = canonical_name(&name);
        let is_result_pseudo = result_local.is_some()
            && !locals.iter().enumerate().any(|(local_index, local)| {
                local_index != result_local.unwrap() && canonical_name(&local.name) == key
            })
            && key == "result";
        let is_result_name = result_local.is_some()
            && routine_candidate.is_some()
            && routine_symbol_index
                .and_then(|symbol_index| document.symbols.get(symbol_index))
                .is_some_and(|symbol| canonical_name(&symbol.name) == key)
            && !is_call_entity(identifier);
        if is_result_pseudo {
            let span = SourceSpan {
                start: identifier.start_byte(),
                end: identifier.end_byte(),
            };
            let Some((block, statement)) = statement_location(&cfg, span) else {
                note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
                continue;
            };
            let access =
                classify_access(identifier, span, &document.source, &call_argument_accesses);
            pending.push(PendingOccurrence {
                var: result_local.unwrap(),
                span,
                access,
                block,
                statement_start: statement.start,
                semantic_order: assignment_order(identifier),
            });
            continue;
        }
        if !local_names.contains(&key) && !is_result_name {
            continue;
        }
        if is_declaration_name(identifier) {
            continue;
        }

        let mut state = ResolutionState::new();
        let candidates = index.resolve_candidates_at_with_state_and_budget(
            uri,
            document,
            identifier.start_byte(),
            identifier,
            &mut state,
            0,
            cancel,
            budget,
        )?;
        if candidates.len() == 1 {
            let candidate = &candidates[0];
            if let Some(binding) = bindings
                .iter()
                .find(|binding| binding.candidate == *candidate)
            {
                if binding.synthetic_result && !is_result_name {
                    continue;
                }
                let span = SourceSpan {
                    start: identifier.start_byte(),
                    end: identifier.end_byte(),
                };
                let Some((block, statement)) = statement_location(&cfg, span) else {
                    note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
                    continue;
                };
                let access =
                    classify_access(identifier, span, &document.source, &call_argument_accesses);
                if access == Access::AddressOf {
                    locals[binding.var].address_taken = true;
                }
                pending.push(PendingOccurrence {
                    var: binding.var,
                    span,
                    access,
                    block,
                    statement_start: statement.start,
                    semantic_order: assignment_order(identifier),
                });
                continue;
            }
            if is_result_name && routine_candidate.as_ref() == Some(candidate) {
                let var = result_local.expect("result variable exists for functions");
                let span = SourceSpan {
                    start: identifier.start_byte(),
                    end: identifier.end_byte(),
                };
                let Some((block, statement)) = statement_location(&cfg, span) else {
                    note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
                    continue;
                };
                let access =
                    classify_access(identifier, span, &document.source, &call_argument_accesses);
                pending.push(PendingOccurrence {
                    var,
                    span,
                    access,
                    block,
                    statement_start: statement.start,
                    semantic_order: assignment_order(identifier),
                });
            }
        } else if (is_result_name
            || local_names.contains(&key)
                && !is_right_hand_member(identifier)
                && !is_inside_with_body(identifier, &body_nodes))
            && !state.receiver_resolution_uncertain()
            && !state.member_lookup_incomplete
        {
            note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
        }
    }

    pending.sort_by_key(|occurrence| {
        (
            occurrence.block.index().index(),
            occurrence.statement_start,
            occurrence.semantic_order,
            occurrence.span.start,
            occurrence.span.end,
        )
    });
    let mut orders_by_block = HashMap::<BlockId, usize>::new();
    let occurrences = pending
        .into_iter()
        .map(|occurrence| {
            let order = orders_by_block.entry(occurrence.block).or_default();
            let current = *order;
            *order = order.saturating_add(1);
            VarOccurrence {
                var: occurrence.var,
                span: occurrence.span,
                access: occurrence.access,
                block: occurrence.block,
                order: current,
            }
        })
        .collect();

    Ok(RoutineFlow {
        name: cfg.proc_name.clone(),
        cfg,
        locals,
        occurrences,
        excluded,
        node,
    })
}

#[allow(clippy::too_many_arguments)]
fn collect_locals(
    document: &Document,
    uri: &Url,
    routine_node: Node<'_>,
    routine_symbol_index: Option<usize>,
    nested_ranges: &[Range<usize>],
    routine_nodes: &[Node<'_>],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<CollectedLocals, String> {
    let routine_symbol = routine_symbol_index.and_then(|index| document.symbols.get(index));
    let routine_scope = routine_symbol
        .and_then(|symbol| symbol.body_scope)
        .or_else(|| {
            document.scopes.iter().position(|scope| {
                scope.start == routine_node.start_byte() && scope.end == routine_node.end_byte()
            })
        });
    let Some(routine_scope) = routine_scope else {
        return Ok(CollectedLocals {
            locals: Vec::new(),
            bindings: Vec::new(),
            routine_scope: None,
        });
    };
    let mut locals = Vec::new();
    let mut bindings = Vec::new();
    for (symbol_index, symbol) in document.symbols.iter().enumerate() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if !matches!(symbol.kind, SymbolKind::Variable | SymbolKind::Parameter)
            || !scope_is_within(document, symbol.scope, routine_scope)
            || nested_ranges.iter().any(|range| {
                range.start <= symbol.declaration_span.start
                    && symbol.declaration_span.end <= range.end
            })
        {
            continue;
        }
        let declaration_node = routine_nodes.iter().copied().find(|candidate| {
            candidate.start_byte() == symbol.declaration_span.start
                && candidate.end_byte() == symbol.declaration_span.end
        });
        let absolute = declaration_node.is_some_and(|decl| {
            routine_nodes.iter().any(|candidate| {
                candidate.start_byte() >= decl.start_byte()
                    && candidate.end_byte() <= decl.end_byte()
                    && candidate.kind() == "kAbsolute"
            })
        });
        let mode = symbol.parameter_mode.map(local_parameter_mode);
        let kind = mode.map_or(LocalKind::Local, LocalKind::Parameter);
        let ty = classify_symbol_type(document, symbol);
        let local_index = locals.len();
        budget.require_owned_bytes(symbol.name.len(), cancel)?;
        budget.require_owned_bytes(
            std::mem::size_of::<LocalVar>() + std::mem::size_of::<LocalBinding>(),
            cancel,
        )?;
        locals.push(LocalVar {
            name: symbol.name.clone(),
            decl_span: SourceSpan {
                start: symbol.span.start,
                end: symbol.span.end,
            },
            kind,
            ty,
            address_taken: false,
            captured: false,
            absolute,
            possible_with_target: false,
        });
        bindings.push(LocalBinding {
            var: local_index,
            candidate: Candidate {
                uri: uri.clone(),
                index: symbol_index,
            },
            symbol_scope: symbol.scope,
            synthetic_result: false,
        });
    }
    if let (Some(routine_symbol_index), Some(routine_symbol)) =
        (routine_symbol_index, routine_symbol)
        && routine_symbol.routine_kind == RoutineKind::Function
    {
        budget.require_work(1, cancel)?;
        budget.require_owned_bytes(
            "Result".len() + std::mem::size_of::<LocalVar>() + std::mem::size_of::<LocalBinding>(),
            cancel,
        )?;
        let local_index = locals.len();
        let ty = routine_symbol
            .implementation_result_type_ref
            .as_ref()
            .or(routine_symbol.result_type_ref.as_ref())
            .map(|type_ref| {
                classify_type_ref(document, routine_scope, type_ref, &mut HashSet::new())
            })
            .unwrap_or(VarType::Unknown);
        locals.push(LocalVar {
            name: "Result".to_owned(),
            decl_span: SourceSpan {
                start: routine_symbol.span.start,
                end: routine_symbol.span.end,
            },
            kind: LocalKind::Result,
            ty,
            address_taken: false,
            captured: false,
            absolute: false,
            possible_with_target: false,
        });
        bindings.push(LocalBinding {
            var: local_index,
            candidate: Candidate {
                uri: uri.clone(),
                index: routine_symbol_index,
            },
            symbol_scope: routine_symbol.scope,
            synthetic_result: true,
        });
    }
    Ok(CollectedLocals {
        locals,
        bindings,
        routine_scope: Some(routine_scope),
    })
}

fn local_parameter_mode(mode: ParameterMode) -> LocalVarParameterMode {
    match mode {
        ParameterMode::Value => LocalVarParameterMode::Value,
        ParameterMode::Var => LocalVarParameterMode::Var,
        ParameterMode::Out => LocalVarParameterMode::Out,
        ParameterMode::Const => LocalVarParameterMode::Const,
        ParameterMode::ConstRef => LocalVarParameterMode::ConstRef,
    }
}

fn scope_is_within(document: &Document, scope: usize, ancestor: usize) -> bool {
    let mut current = Some(scope);
    while let Some(index) = current {
        if index == ancestor {
            return true;
        }
        current = document.scopes.get(index).and_then(|scope| scope.parent);
    }
    false
}

fn classify_symbol_type(document: &Document, symbol: &Symbol) -> VarType {
    symbol
        .type_shape
        .as_ref()
        .map(|shape| classify_type_shape(document, symbol.scope, shape, &mut HashSet::new()))
        .unwrap_or(VarType::Unknown)
}

fn classify_type_shape(
    document: &Document,
    scope: usize,
    shape: &TypeShape,
    visited: &mut HashSet<(usize, String)>,
) -> VarType {
    match shape {
        TypeShape::Named(type_ref) => classify_type_ref(document, scope, type_ref, visited),
        TypeShape::Pointer(_)
        | TypeShape::Array { .. }
        | TypeShape::Callable
        | TypeShape::Unknown => VarType::Unknown,
    }
}

fn classify_type_ref(
    document: &Document,
    scope: usize,
    type_ref: &TypeRef,
    visited: &mut HashSet<(usize, String)>,
) -> VarType {
    let Some(name) = type_ref.path.last() else {
        return VarType::Unknown;
    };
    match canonical_name(name).as_str() {
        "string" | "ansistring" | "widestring" | "unicodestring" | "shortstring" => {
            return VarType::Managed;
        }
        "integer" | "smallint" | "shortint" | "byte" | "word" | "longword" | "cardinal"
        | "longint" | "int64" | "uint64" | "nativeint" | "nativeuint" | "boolean" | "bytebool"
        | "wordbool" | "longbool" | "char" | "widechar" | "ansichar" | "real" | "single"
        | "double" | "extended" | "currency" | "comp" => {
            return VarType::Scalar;
        }
        _ => {}
    }
    let key = canonical_name(name);
    if !visited.insert((scope, key.clone())) {
        return VarType::Unknown;
    }
    let candidates = document
        .symbols
        .iter()
        .filter(|candidate| {
            candidate.kind == SymbolKind::Type
                && candidate.key == key
                && scope_is_within(document, scope, candidate.scope)
        })
        .collect::<Vec<_>>();
    let Some(candidate) = candidates
        .iter()
        .max_by_key(|candidate| scope_depth(document, candidate.scope))
    else {
        return VarType::Unknown;
    };
    match candidate.type_kind {
        TypeKind::Record => VarType::Record,
        TypeKind::String | TypeKind::DynamicArray => VarType::Managed,
        _ => candidate
            .type_shape
            .as_ref()
            .map(|shape| classify_type_shape(document, candidate.scope, shape, visited))
            .unwrap_or(VarType::Unknown),
    }
}

fn scope_depth(document: &Document, scope: usize) -> usize {
    let mut depth = 0;
    let mut current = Some(scope);
    while let Some(index) = current {
        depth += 1;
        current = document.scopes.get(index).and_then(|scope| scope.parent);
    }
    depth
}

fn exclusion_for_routine(
    document: &Document,
    node: Node<'_>,
    body_nodes: &[Node<'_>],
    nested_ranges: &[Range<usize>],
    cfg: &Cfg,
) -> Option<FlowExclusion> {
    let mut excluded = None;
    if !document.conditionals.complete
        || document
            .conditionals
            .unknown_spans
            .iter()
            .any(|span| source_span_affects_routine(span.start, span.end, node, nested_ranges))
    {
        note_exclusion(&mut excluded, FlowExclusion::UnknownConditional);
    }
    if document
        .parser_recovery_spans
        .iter()
        .any(|span| source_span_affects_routine(span.start, span.end, node, nested_ranges))
    {
        note_exclusion(&mut excluded, FlowExclusion::Recovery);
    }
    if body_nodes.iter().any(|candidate| candidate.kind() == "asm") {
        note_exclusion(&mut excluded, FlowExclusion::Asm);
    }
    if body_nodes.iter().any(|candidate| {
        matches!(
            candidate.kind(),
            "goto" | "label" | "labeledStatement" | "labeledStatementTr"
        )
    }) || cfg.graph.edge_weights().any(|edge| *edge == EdgeKind::Goto)
    {
        note_exclusion(&mut excluded, FlowExclusion::Goto);
    }
    if body_nodes
        .iter()
        .any(|candidate| matches!(candidate.kind(), "varDef" | "varAssignDef"))
    {
        note_exclusion(&mut excluded, FlowExclusion::InlineVariable);
    }
    excluded
}

fn source_span_affects_routine(
    start: usize,
    end: usize,
    routine: Node<'_>,
    nested_ranges: &[Range<usize>],
) -> bool {
    let intersects = if start == end {
        routine.start_byte() <= start && start <= routine.end_byte()
    } else {
        start < routine.end_byte() && routine.start_byte() < end
    };
    intersects
        && !nested_ranges
            .iter()
            .any(|range| range.start <= start && end <= range.end)
}

fn note_exclusion(excluded: &mut Option<FlowExclusion>, reason: FlowExclusion) {
    if excluded.is_none() {
        *excluded = Some(reason);
    }
}

fn collect_nodes<'a>(
    root: Node<'a>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<Node<'a>>, String> {
    collect_nodes_with_boundary(root, None, cancel, budget)
}

fn collect_owned_nodes<'a>(
    root: Node<'a>,
    root_id: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<Node<'a>>, String> {
    collect_nodes_with_boundary(root, Some(root_id), cancel, budget)
}

fn collect_nodes_with_boundary<'a>(
    root: Node<'a>,
    root_id: Option<usize>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<Node<'a>>, String> {
    let mut nodes = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        budget.require_owned_bytes(std::mem::size_of::<Node<'a>>(), cancel)?;
        if root_id.is_some_and(|root_id| {
            node.id() != root_id && matches!(node.kind(), "defProc" | "lambda")
        }) {
            continue;
        }
        nodes.push(node);
        let mut cursor = node.walk();
        pending.extend(
            node.children(&mut cursor)
                .collect::<Vec<_>>()
                .into_iter()
                .rev(),
        );
    }
    Ok(nodes)
}

fn collect_nested_ranges(
    root: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<Range<usize>>, String> {
    let mut ranges = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.id() != root.id() && matches!(child.kind(), "defProc" | "lambda") {
                ranges.push(child.start_byte()..child.end_byte());
            }
            pending.push(child);
        }
    }
    Ok(ranges)
}

#[allow(clippy::too_many_arguments)]
fn mark_loop_variables(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    body_nodes: &[Node<'_>],
    locals: &mut [LocalVar],
    bindings: &[LocalBinding],
    excluded: &mut Option<FlowExclusion>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<(), String> {
    for node in body_nodes
        .iter()
        .copied()
        .filter(|node| node.kind() == "for")
    {
        check_navigation_cancel(cancel)?;
        let Some(start) = node.child_by_field_name("start") else {
            continue;
        };
        let Some(lhs) = start.child_by_field_name("lhs") else {
            continue;
        };
        if lhs.kind() != "identifier" {
            continue;
        }
        let candidates = resolve_identifier(index, uri, document, lhs, cancel, budget)?;
        if candidates.len() == 1
            && let Some(binding) = bindings
                .iter()
                .find(|binding| binding.candidate == candidates[0] && !binding.synthetic_result)
        {
            locals[binding.var].kind = LocalKind::LoopVar;
        } else if locals.iter().any(|local| {
            canonical_name(&local.name) == canonical_name(&node_text(lhs, &document.source))
        }) {
            note_exclusion(excluded, FlowExclusion::UnresolvedIdentity);
        }
    }
    Ok(())
}

fn mark_possible_with_targets(
    document: &Document,
    body_nodes: &[Node<'_>],
    locals: &mut [LocalVar],
    local_names: &HashSet<String>,
) {
    let with_nodes = body_nodes
        .iter()
        .copied()
        .filter(|node| node.kind() == "with")
        .collect::<Vec<_>>();
    for with_node in with_nodes {
        let Some(body) = with_node.child_by_field_name("body") else {
            continue;
        };
        for identifier in body_nodes.iter().copied().filter(|node| {
            node.kind() == "identifier"
                && body.start_byte() <= node.start_byte()
                && node.end_byte() <= body.end_byte()
                && !is_right_hand_member(*node)
        }) {
            let key = canonical_name(&node_text(identifier, &document.source));
            if local_names.contains(&key) {
                for local in locals
                    .iter_mut()
                    .filter(|local| canonical_name(&local.name) == key)
                {
                    local.possible_with_target = true;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn mark_captured_locals(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    routine_node: Node<'_>,
    nested_ranges: &[Range<usize>],
    locals: &mut [LocalVar],
    bindings: &[LocalBinding],
    excluded: &mut Option<FlowExclusion>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<(), String> {
    if bindings.is_empty() || nested_ranges.is_empty() {
        return Ok(());
    }
    let local_names = locals
        .iter()
        .map(|local| canonical_name(&local.name))
        .collect::<HashSet<_>>();
    let mut pending = vec![routine_node];
    while let Some(node) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let is_nested =
            node.id() != routine_node.id() && matches!(node.kind(), "defProc" | "lambda");
        if is_nested {
            let nested_nodes = collect_nodes(node, cancel, budget)?;
            for identifier in nested_nodes
                .into_iter()
                .filter(|candidate| candidate.kind() == "identifier")
            {
                let key = canonical_name(&node_text(identifier, &document.source));
                if !local_names.contains(&key) || is_declaration_name(identifier) {
                    continue;
                }
                let candidates =
                    resolve_identifier(index, uri, document, identifier, cancel, budget)?;
                if candidates.len() == 1
                    && let Some(binding) = bindings.iter().find(|binding| {
                        binding.candidate == candidates[0] && !binding.synthetic_result
                    })
                {
                    locals[binding.var].captured = true;
                } else if candidates.iter().any(|candidate| {
                    bindings
                        .iter()
                        .any(|binding| binding.candidate == *candidate && !binding.synthetic_result)
                }) {
                    for binding in bindings.iter().filter(|binding| {
                        !binding.synthetic_result
                            && candidates
                                .iter()
                                .any(|candidate| candidate == &binding.candidate)
                    }) {
                        locals[binding.var].captured = true;
                    }
                } else if candidates.is_empty() {
                    note_exclusion(excluded, FlowExclusion::UnresolvedIdentity);
                }
            }
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    Ok(())
}

fn call_argument_accesses(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    body_nodes: &[Node<'_>],
    local_names: &HashSet<String>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<HashMap<(usize, usize), Access>, String> {
    let mut accesses = HashMap::new();
    for call in body_nodes
        .iter()
        .copied()
        .filter(|node| node.kind() == "exprCall")
    {
        check_navigation_cancel(cancel)?;
        let Some(arguments) = call.child_by_field_name("args") else {
            continue;
        };
        let entity = call.child_by_field_name("entity");
        let callee_name = entity.map(|entity| canonical_name(&node_text(entity, &document.source)));
        let mut cursor = arguments.walk();
        let mut argument_index = 0;
        for argument in arguments.named_children(&mut cursor) {
            if argument.kind() == "legacyFormat" {
                continue;
            }
            let expression = if argument.kind() == "namedArgument" {
                argument.child_by_field_name("value").unwrap_or(argument)
            } else {
                argument
            };
            if expression.kind() == "identifier"
                && local_names.contains(&canonical_name(&node_text(expression, &document.source)))
            {
                let access = if matches!(callee_name.as_deref(), Some("inc" | "dec")) {
                    Access::ReadWrite
                } else if callee_name.as_deref() == Some("addr") {
                    Access::AddressOf
                } else {
                    match super::super::overload::parameter_mode_for_argument(
                        index,
                        uri,
                        document,
                        call,
                        argument_index,
                        cancel,
                        budget,
                    )? {
                        super::super::overload::ParameterModeResolution::Resolved(Some(
                            ParameterMode::Var,
                        ))
                        | super::super::overload::ParameterModeResolution::Intrinsic(Some(
                            ParameterMode::Var,
                        )) => Access::VarArg,
                        super::super::overload::ParameterModeResolution::Resolved(Some(
                            ParameterMode::Out,
                        ))
                        | super::super::overload::ParameterModeResolution::Intrinsic(Some(
                            ParameterMode::Out,
                        )) => Access::OutArg,
                        super::super::overload::ParameterModeResolution::Unresolved
                        | super::super::overload::ParameterModeResolution::Resolved(None)
                        | super::super::overload::ParameterModeResolution::Intrinsic(None) => {
                            Access::ReadWrite
                        }
                        _ => Access::Read,
                    }
                };
                accesses.insert((expression.start_byte(), expression.end_byte()), access);
            }
            argument_index += 1;
        }
    }
    Ok(accesses)
}

fn resolve_identifier(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    identifier: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<Candidate>, String> {
    let mut state = ResolutionState::new();
    index
        .resolve_candidates_at_with_state_and_budget(
            uri,
            document,
            identifier.start_byte(),
            identifier,
            &mut state,
            0,
            cancel,
            budget,
        )
        .map(|candidates| {
            if state.receiver_resolution_uncertain()
                || state.member_lookup_incomplete
                || state.ambiguous
                || state.inaccessible_candidate
            {
                Vec::new()
            } else {
                candidates
            }
        })
}

fn classify_access(
    identifier: Node<'_>,
    span: SourceSpan,
    source: &str,
    call_argument_accesses: &HashMap<(usize, usize), Access>,
) -> Access {
    if is_address_of_expression(identifier) {
        return Access::AddressOf;
    }
    if let Some(access) = call_argument_accesses.get(&(span.start, span.end)) {
        return *access;
    }
    let mut current = identifier;
    while let Some(parent) = current.parent() {
        if parent.kind() == "assignment" {
            let lhs = parent.child_by_field_name("lhs");
            if lhs.is_some_and(|lhs| lhs.start_byte() <= span.start && span.end <= lhs.end_byte()) {
                let compound = parent
                    .child_by_field_name("operator")
                    .is_some_and(|operator| node_text(operator, source) != ":=");
                let direct_lvalue =
                    lhs.is_some_and(|lhs| is_plain_identifier_lvalue(lhs, identifier));
                return if direct_lvalue && compound {
                    Access::ReadWrite
                } else if direct_lvalue {
                    Access::Write
                } else {
                    Access::Read
                };
            }
            break;
        }
        current = parent;
    }
    Access::Read
}

fn assignment_order(identifier: Node<'_>) -> u8 {
    let mut current = identifier;
    while let Some(parent) = current.parent() {
        if parent.kind() == "assignment" {
            return if parent.child_by_field_name("lhs").is_some_and(|lhs| {
                lhs.start_byte() <= identifier.start_byte()
                    && identifier.end_byte() <= lhs.end_byte()
            }) {
                1
            } else {
                0
            };
        }
        current = parent;
    }
    0
}

fn is_plain_identifier_lvalue(lhs: Node<'_>, identifier: Node<'_>) -> bool {
    let mut current = lhs;
    while current.kind() == "exprParens" && current.named_child_count() == 1 {
        let Some(child) = current.named_child(0) else {
            break;
        };
        current = child;
    }
    current.id() == identifier.id()
}

fn is_address_of_expression(identifier: Node<'_>) -> bool {
    let mut current = identifier;
    while let Some(parent) = current.parent() {
        if parent.kind() == "exprUnary" {
            let mut cursor = parent.walk();
            return parent
                .children(&mut cursor)
                .any(|child| child.kind() == "kAt");
        }
        if matches!(
            parent.kind(),
            "assignment" | "exprCall" | "exprBinary" | "if" | "while"
        ) {
            return false;
        }
        current = parent;
    }
    false
}

fn is_call_entity(identifier: Node<'_>) -> bool {
    let mut current = identifier;
    while let Some(parent) = current.parent() {
        if parent.kind() == "exprCall" {
            return parent.child_by_field_name("entity").is_some_and(|entity| {
                entity.start_byte() <= identifier.start_byte()
                    && identifier.end_byte() <= entity.end_byte()
            });
        }
        if matches!(parent.kind(), "assignment" | "statement" | "if" | "while") {
            return false;
        }
        current = parent;
    }
    false
}

fn is_declaration_name(identifier: Node<'_>) -> bool {
    let mut current = identifier;
    while let Some(parent) = current.parent() {
        if matches!(
            parent.kind(),
            "declVar" | "declArg" | "varDef" | "varAssignDef" | "exceptionHandler"
        ) {
            return true;
        }
        if matches!(
            parent.kind(),
            "assignment" | "exprCall" | "statement" | "block" | "statements"
        ) {
            return false;
        }
        current = parent;
    }
    false
}

fn is_right_hand_member(identifier: Node<'_>) -> bool {
    let Some(parent) = identifier.parent() else {
        return false;
    };
    if !matches!(parent.kind(), "exprDot" | "genericDot") {
        return false;
    }
    parent.child_by_field_name("right").is_some_and(|right| {
        right.start_byte() <= identifier.start_byte() && identifier.end_byte() <= right.end_byte()
    })
}

fn is_inside_with_body(identifier: Node<'_>, body_nodes: &[Node<'_>]) -> bool {
    body_nodes.iter().copied().any(|node| {
        node.kind() == "with"
            && node.child_by_field_name("body").is_some_and(|body| {
                body.start_byte() <= identifier.start_byte()
                    && identifier.end_byte() <= body.end_byte()
            })
    })
}

fn statement_location(cfg: &Cfg, span: SourceSpan) -> Option<(BlockId, Range<usize>)> {
    cfg.graph
        .node_indices()
        .flat_map(|node_index| {
            let block = &cfg.graph[node_index];
            block.stmts.iter().filter_map(move |statement| {
                (statement.byte_range.start <= span.start && span.end <= statement.byte_range.end)
                    .then_some((
                        BlockId::from(node_index),
                        statement.byte_range.clone(),
                        statement.byte_range.end - statement.byte_range.start,
                    ))
            })
        })
        .min_by_key(|(block, _, length)| (*length, block.index().index()))
        .map(|(block, range, _)| (block, range))
}
