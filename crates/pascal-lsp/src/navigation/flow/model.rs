use super::super::{
    AssistanceBudget, Candidate, Document, NavigationIndex, Origin, ParameterMode, ResolutionState,
    RoutineKind, SourceSpan, Symbol, SymbolKind, TypeKind, TypeRef, TypeShape, canonical_name,
    check_navigation_cancel, node_text,
};
use cfg_pascal::cfg_core::types::{BlockId, Cfg, EdgeKind};
use lsp_types::Url;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use tree_sitter::Node;

pub(super) type StatementLocations = Vec<(BlockId, Range<usize>)>;

#[derive(Debug)]
pub(super) struct StatementLocationIndex {
    ranges: Vec<IndexedStatementRange>,
}

#[derive(Debug)]
struct IndexedStatementRange {
    range: Range<usize>,
    blocks: Vec<BlockId>,
    parent: Option<usize>,
}

impl StatementLocationIndex {
    pub(super) fn new(
        cfg: &Cfg,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<Self>, String> {
        let mut by_range = HashMap::<(usize, usize), Vec<BlockId>>::new();
        for node_index in cfg.graph.node_indices() {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let block = BlockId::from(node_index);
            for statement in &cfg.graph[node_index].stmts {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if statement.byte_range.is_empty() {
                    continue;
                }
                let range = (statement.byte_range.start, statement.byte_range.end);
                if !by_range.contains_key(&range) {
                    budget.require_owned_bytes(
                        std::mem::size_of::<((usize, usize), Vec<BlockId>)>(),
                        cancel,
                    )?;
                }
                let blocks = by_range.entry(range).or_default();
                if !blocks.contains(&block) {
                    budget.require_work(1, cancel)?;
                    budget.require_owned_bytes(std::mem::size_of::<BlockId>(), cancel)?;
                    blocks.push(block);
                }
            }
        }
        let mut ranges = by_range
            .into_iter()
            .map(|((start, end), mut blocks)| {
                blocks.sort_unstable();
                IndexedStatementRange {
                    range: start..end,
                    blocks,
                    parent: None,
                }
            })
            .collect::<Vec<_>>();
        ranges.sort_by(|left, right| {
            left.range
                .start
                .cmp(&right.range.start)
                .then_with(|| right.range.end.cmp(&left.range.end))
        });

        let mut nesting = Vec::<usize>::new();
        for index in 0..ranges.len() {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            while let Some(&parent_index) = nesting.last() {
                let parent = &ranges[parent_index].range;
                let current = &ranges[index].range;
                if parent.end >= current.end {
                    ranges[index].parent = Some(parent_index);
                    break;
                }
                if parent.end > current.start {
                    // Partially overlapping statement ranges are not a
                    // containment forest, so retain the conservative scan.
                    return Ok(None);
                }
                nesting.pop();
            }
            nesting.push(index);
        }
        Ok(Some(Self { ranges }))
    }

    pub(super) fn locations_for_node(
        &self,
        node: Node<'_>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<StatementLocations>, String> {
        self.locations_for_span(
            SourceSpan {
                start: node.start_byte(),
                end: node.end_byte(),
            },
            cancel,
            budget,
        )
    }

    fn locations_for_span(
        &self,
        span: SourceSpan,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<StatementLocations>, String> {
        let prefix_len = self
            .ranges
            .partition_point(|entry| entry.range.start <= span.start);
        let Some(mut index) = prefix_len.checked_sub(1) else {
            return Ok(None);
        };
        loop {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let entry = &self.ranges[index];
            if span.end <= entry.range.end {
                budget.require_owned_bytes(
                    entry
                        .blocks
                        .len()
                        .saturating_mul(std::mem::size_of::<(BlockId, Range<usize>)>()),
                    cancel,
                )?;
                return Ok(Some(
                    entry
                        .blocks
                        .iter()
                        .copied()
                        .map(|block| (block, entry.range.clone()))
                        .collect(),
                ));
            }
            let Some(parent) = entry.parent else {
                return Ok(None);
            };
            index = parent;
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_CFG_BUILDER_CALL_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn reset_test_cfg_builder_call_count() {
    TEST_CFG_BUILDER_CALL_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(super) fn test_cfg_builder_call_count() -> usize {
    TEST_CFG_BUILDER_CALL_COUNT.with(std::cell::Cell::get)
}

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
    ExceptionHandler,
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
    UnknownCallEffect,
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
    pub(crate) class_reference: bool,
    pub(crate) type_resolved: bool,
    pub(crate) unresolved_local_generic_argument: bool,
    pub(crate) unknown_call_effect: bool,
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
    pub(crate) cfg: Option<Cfg>,
    pub(crate) locals: Vec<LocalVar>,
    pub(crate) occurrences: Vec<VarOccurrence>,
    pub(crate) excluded: Option<FlowExclusion>,
    pub(crate) node: Node<'a>,
}

#[derive(Debug, Default)]
struct CallArgumentAccesses {
    by_span: HashMap<(usize, usize), Access>,
    target_uncertain: bool,
}

struct CollectedRoutineNodes<'a> {
    all_nodes: Vec<Node<'a>>,
    body_nodes: Vec<Node<'a>>,
    nested_ranges: Vec<Range<usize>>,
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
    budget.require_owned_bytes(document.source.len(), cancel)?;

    let (document_node_count, routines) =
        collect_document_routines(root, &document.source, cancel, budget)?;
    let flow_symbols = FlowSymbolIndex::new(document, &routines, cancel, budget)?;
    let flow_bytes = routines
        .len()
        .saturating_mul(std::mem::size_of::<RoutineFlow<'a>>());
    budget.require_owned_bytes(flow_bytes, cancel)?;
    if !budget.take_work(document_node_count, cancel)? {
        return Ok(routines
            .into_iter()
            .map(|(node, name)| RoutineFlow {
                name,
                cfg: None,
                locals: Vec::new(),
                occurrences: Vec::new(),
                excluded: Some(FlowExclusion::Budget),
                node,
            })
            .collect());
    }

    let mut budget_excluded = HashMap::new();
    let mut build_error = None;
    #[cfg(test)]
    TEST_CFG_BUILDER_CALL_COUNT.with(|count| count.set(count.get() + 1));
    let mut cfgs = cfg_pascal::build_routine_cfgs_with_filter_and_cancel(
        &document.tree,
        document.source.as_bytes(),
        || cancel.load(Ordering::Relaxed),
        |proc_name, node| {
            if let Err(error) = check_navigation_cancel(cancel) {
                build_error = Some(error);
                return false;
            }
            let node_count = node.descendant_count().saturating_add(1);
            match budget.take_work(node_count, cancel) {
                Ok(true) => {
                    if let Err(error) = check_navigation_cancel(cancel) {
                        build_error = Some(error);
                        false
                    } else {
                        true
                    }
                }
                Ok(false) => {
                    if let Err(error) = budget.require_owned_bytes(proc_name.len(), cancel) {
                        build_error = Some(error);
                        return false;
                    }
                    budget_excluded
                        .insert((node.start_byte(), node.end_byte()), proc_name.to_owned());
                    false
                }
                Err(error) => {
                    build_error = Some(error);
                    false
                }
            }
        },
    )?;
    if let Some(error) = build_error {
        return Err(error);
    }
    check_navigation_cancel(cancel)?;
    budget.require_owned_bytes(
        cfgs.len()
            .saturating_mul(std::mem::size_of::<Cfg>() + std::mem::size_of::<usize>() * 2),
        cancel,
    )?;
    let mut cfgs_by_range = HashMap::with_capacity(cfgs.len());
    for cfg in cfgs.drain(..) {
        check_navigation_cancel(cancel)?;
        cfgs_by_range.insert((cfg.byte_range.start, cfg.byte_range.end), cfg);
    }

    let mut flows = Vec::with_capacity(routines.len());
    for (node, _) in routines {
        check_navigation_cancel(cancel)?;
        let range = (node.start_byte(), node.end_byte());
        if let Some(name) = budget_excluded.remove(&range) {
            flows.push(RoutineFlow {
                name,
                cfg: None,
                locals: Vec::new(),
                occurrences: Vec::new(),
                excluded: Some(FlowExclusion::Budget),
                node,
            });
            continue;
        }
        let Some(cfg) = cfgs_by_range.remove(&(node.start_byte(), node.end_byte())) else {
            continue;
        };
        if !budget.take_work(1, cancel)? {
            flows.push(RoutineFlow {
                name: cfg.proc_name.clone(),
                cfg: Some(cfg),
                locals: Vec::new(),
                occurrences: Vec::new(),
                excluded: Some(FlowExclusion::Budget),
                node,
            });
            continue;
        }
        flows.push(build_routine_flow(
            index,
            uri,
            document,
            node,
            cfg,
            &flow_symbols,
            cancel,
            budget,
        )?);
    }
    Ok(flows)
}

fn collect_document_routines<'a>(
    root: Node<'a>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<(usize, Vec<(Node<'a>, String)>), String> {
    let mut node_count = 0usize;
    let mut routines = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        budget.require_owned_bytes(std::mem::size_of::<Node<'a>>(), cancel)?;
        node_count = node_count.saturating_add(1);
        if node.kind() == "defProc" && node.child_by_field_name("body").is_some() {
            let name = node
                .child_by_field_name("header")
                .and_then(|header| header.child_by_field_name("name"))
                .map(|name_node| node_text(name_node, source))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| format!("<routine@{}>", node.start_byte()));
            budget.require_owned_bytes(name.len(), cancel)?;
            routines.push((node, name));
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            check_navigation_cancel(cancel)?;
            budget.require_owned_bytes(std::mem::size_of::<Node<'a>>(), cancel)?;
            pending.push(child);
        }
    }
    Ok((node_count, routines))
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

#[derive(Debug, Default)]
struct FlowSymbolIndex {
    routine_by_range: HashMap<(usize, usize), usize>,
    routine_scope_by_range: HashMap<(usize, usize), usize>,
    function_ranges: HashSet<(usize, usize)>,
    local_symbols_by_routine_scope: HashMap<usize, Vec<usize>>,
}

#[derive(Debug)]
struct CachedLocalResolution {
    candidates: Vec<Candidate>,
    receiver_uncertain: bool,
    member_lookup_incomplete: bool,
    compiler_surface_resolved: bool,
    ambiguous: bool,
    inaccessible_candidate: bool,
}

impl FlowSymbolIndex {
    fn new(
        document: &Document,
        routines: &[(Node<'_>, String)],
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Self, String> {
        let mut result = Self::default();
        for (symbol_index, symbol) in document.symbols.iter().enumerate() {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if symbol.kind != SymbolKind::Routine || symbol.origin != Origin::Definition {
                continue;
            }
            let range = (symbol.declaration_span.start, symbol.declaration_span.end);
            budget.require_owned_bytes(std::mem::size_of::<((usize, usize), usize)>(), cancel)?;
            result.routine_by_range.entry(range).or_insert(symbol_index);
            if symbol.routine_kind == RoutineKind::Function {
                budget.require_owned_bytes(std::mem::size_of::<(usize, usize)>(), cancel)?;
                result.function_ranges.insert(range);
            }
        }

        let mut scopes_by_range = HashMap::new();
        for (scope_index, scope) in document.scopes.iter().enumerate() {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            budget.require_owned_bytes(std::mem::size_of::<((usize, usize), usize)>(), cancel)?;
            scopes_by_range
                .entry((scope.start, scope.end))
                .or_insert(scope_index);
        }

        let mut routine_scopes = HashSet::new();
        for (node, _) in routines {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let range = (node.start_byte(), node.end_byte());
            let routine_scope = result
                .routine_by_range
                .get(&range)
                .and_then(|symbol_index| document.symbols.get(*symbol_index))
                .and_then(|symbol| symbol.body_scope)
                .or_else(|| scopes_by_range.get(&range).copied());
            if let Some(routine_scope) = routine_scope {
                budget.require_owned_bytes(
                    std::mem::size_of::<((usize, usize), usize)>() + std::mem::size_of::<usize>(),
                    cancel,
                )?;
                result.routine_scope_by_range.insert(range, routine_scope);
                routine_scopes.insert(routine_scope);
            }
        }

        for (symbol_index, symbol) in document.symbols.iter().enumerate() {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if !matches!(symbol.kind, SymbolKind::Variable | SymbolKind::Parameter) {
                continue;
            }
            let mut current_scope = Some(symbol.scope);
            let mut owner_routine_scope = None;
            while let Some(scope) = current_scope {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if routine_scopes.contains(&scope) {
                    owner_routine_scope = Some(scope);
                    break;
                }
                current_scope = document.scopes.get(scope).and_then(|scope| scope.parent);
            }
            if let Some(routine_scope) = owner_routine_scope {
                budget.require_owned_bytes(std::mem::size_of::<usize>(), cancel)?;
                result
                    .local_symbols_by_routine_scope
                    .entry(routine_scope)
                    .or_default()
                    .push(symbol_index);
            }
        }
        Ok(result)
    }
}

#[allow(clippy::too_many_arguments)]
fn build_routine_flow<'a>(
    index: &NavigationIndex,
    uri: &Url,
    document: &'a Document,
    node: Node<'a>,
    cfg: Cfg,
    flow_symbols: &FlowSymbolIndex,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<RoutineFlow<'a>, String> {
    let body = node
        .child_by_field_name("body")
        .expect("only executable routine nodes have a body");
    let CollectedRoutineNodes {
        all_nodes,
        body_nodes,
        nested_ranges,
    } = collect_routine_nodes(node, body, cancel, budget)?;
    check_navigation_cancel(cancel)?;
    budget.require_work(1, cancel)?;
    let range = (node.start_byte(), node.end_byte());
    let routine_symbol_index = flow_symbols.routine_by_range.get(&range).copied();
    let routine_scope = flow_symbols.routine_scope_by_range.get(&range).copied();
    let local_symbol_indices = routine_scope
        .and_then(|scope| flow_symbols.local_symbols_by_routine_scope.get(&scope))
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut excluded = exclusion_for_routine(
        document,
        node,
        &body_nodes,
        &nested_ranges,
        &cfg,
        cancel,
        budget,
    )?;
    let statement_index = StatementLocationIndex::new(&cfg, cancel, budget)?;
    let CollectedLocals {
        mut locals,
        bindings,
        routine_scope,
    } = collect_locals(
        document,
        uri,
        routine_symbol_index,
        routine_scope,
        local_symbol_indices,
        &nested_ranges,
        &all_nodes,
        cancel,
        budget,
    )?;
    if routine_scope.is_none() {
        note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
    }

    let mut local_names = HashSet::new();
    for local in &locals {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        budget.require_owned_bytes(local.name.len(), cancel)?;
        if !local_names.insert(canonical_name(&local.name)) {
            note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
        }
        if local.unresolved_local_generic_argument {
            note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
        }
    }
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
    if !mark_possible_with_targets(
        document,
        &body_nodes,
        &mut locals,
        &local_names,
        &mut excluded,
        cancel,
        budget,
    )? {
        return Ok(RoutineFlow {
            name: cfg.proc_name.clone(),
            cfg: Some(cfg),
            locals,
            occurrences: Vec::new(),
            excluded,
            node,
        });
    }
    mark_captured_locals(
        index,
        uri,
        document,
        node,
        &nested_ranges,
        &mut locals,
        &bindings,
        &flow_symbols.function_ranges,
        &mut excluded,
        cancel,
        budget,
    )?;

    let call_effects = call_argument_accesses(
        index,
        uri,
        document,
        &body_nodes,
        &local_names,
        cancel,
        budget,
    )?;
    if call_effects.target_uncertain {
        note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
        return Ok(RoutineFlow {
            name: cfg.proc_name.clone(),
            cfg: Some(cfg),
            locals,
            occurrences: Vec::new(),
            excluded,
            node,
        });
    }
    let call_argument_accesses = call_effects.by_span;
    let mut pending = Vec::new();
    let result_local = locals
        .iter()
        .position(|local| local.kind == LocalKind::Result);
    let routine_candidate = routine_symbol_index.map(|index| Candidate {
        uri: uri.clone(),
        index,
    });
    let mut local_resolution_cache =
        HashMap::<usize, HashMap<String, CachedLocalResolution>>::new();
    for identifier in body_nodes.iter().copied() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if identifier.kind() != "identifier" {
            continue;
        }
        if is_control_transfer_identifier(identifier, &document.source) {
            if let (Some(result), Some(exit_call)) = (
                result_local,
                exit_call_with_argument(identifier, &document.source),
            ) {
                let span = SourceSpan {
                    start: exit_call.start_byte(),
                    end: exit_call.end_byte(),
                };
                let Some(locations) = statement_locations_for_node(
                    &cfg,
                    statement_index.as_ref(),
                    exit_call,
                    cancel,
                    budget,
                )?
                else {
                    note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
                    continue;
                };
                for (block, statement) in locations {
                    pending.push(PendingOccurrence {
                        var: result,
                        span,
                        access: Access::Write,
                        block,
                        statement_start: statement.start,
                        semantic_order: 2,
                    });
                }
            }
            continue;
        }
        let name = node_text(identifier, &document.source);
        let key = canonical_name(&name);
        let is_result_pseudo = result_local.is_some()
            && !locals.iter().enumerate().any(|(local_index, local)| {
                local_index != result_local.unwrap() && canonical_name(&local.name) == key
            })
            && key == "result"
            && !is_right_hand_member(identifier)
            && !is_call_entity(identifier);
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
            let Some(locations) = statement_locations_for_node(
                &cfg,
                statement_index.as_ref(),
                identifier,
                cancel,
                budget,
            )?
            else {
                note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
                continue;
            };
            let access =
                classify_access(identifier, span, &document.source, &call_argument_accesses);
            for (block, statement) in locations {
                pending.push(PendingOccurrence {
                    var: result_local.unwrap(),
                    span,
                    access,
                    block,
                    statement_start: statement.start,
                    semantic_order: assignment_order(identifier),
                });
            }
            continue;
        }
        if is_declaration_name(identifier) {
            continue;
        }
        if key == "self"
            && routine_candidate
                .as_ref()
                .and_then(|candidate| index.symbol(candidate))
                .is_some_and(|symbol| symbol.owner_type.is_some())
        {
            continue;
        }
        let is_local_occurrence =
            (local_names.contains(&key) || is_result_name) && !is_right_hand_member(identifier);
        if !is_local_occurrence && is_call_entity(identifier) {
            continue;
        }

        // Incomplete imports do not excuse an unresolved bare identifier: a
        // resolver candidate is the evidence that the name is declared and
        // visible; otherwise the routine is excluded below.
        let offset = identifier.start_byte();
        let (
            candidates,
            receiver_uncertain,
            member_lookup_incomplete,
            compiler_surface_resolved,
            ambiguous,
            inaccessible,
        ) = if is_local_occurrence && !document.has_with_context_at(offset) {
            let scope = document.scope_at(offset);
            let cached = local_resolution_cache
                .get(&scope)
                .and_then(|by_name| by_name.get(&key));
            if cached.is_none() {
                let mut state = ResolutionState::new();
                let candidates = index.resolve_candidates_at_with_state_and_budget(
                    uri, document, offset, identifier, &mut state, 0, cancel, budget,
                )?;
                budget.require_work(1, cancel)?;
                budget.require_owned_bytes(
                    key.len() + std::mem::size_of::<(String, CachedLocalResolution)>(),
                    cancel,
                )?;
                local_resolution_cache.entry(scope).or_default().insert(
                    key.clone(),
                    CachedLocalResolution {
                        candidates,
                        receiver_uncertain: state.receiver_resolution_uncertain(),
                        member_lookup_incomplete: state.member_lookup_incomplete,
                        compiler_surface_resolved: state.compiler_surface_resolved,
                        ambiguous: state.ambiguous,
                        inaccessible_candidate: state.inaccessible_candidate,
                    },
                );
            } else {
                budget.require_work(1, cancel)?;
            }
            let cached = local_resolution_cache
                .get(&scope)
                .and_then(|by_name| by_name.get(&key))
                .expect("local resolution was cached");
            (
                std::borrow::Cow::Borrowed(cached.candidates.as_slice()),
                cached.receiver_uncertain,
                cached.member_lookup_incomplete,
                cached.compiler_surface_resolved,
                cached.ambiguous,
                cached.inaccessible_candidate,
            )
        } else {
            let mut state = ResolutionState::new();
            let candidates = index.resolve_candidates_at_with_state_and_budget(
                uri, document, offset, identifier, &mut state, 0, cancel, budget,
            )?;
            (
                std::borrow::Cow::Owned(candidates),
                state.receiver_resolution_uncertain(),
                state.member_lookup_incomplete,
                state.compiler_surface_resolved,
                state.ambiguous,
                state.inaccessible_candidate,
            )
        };
        let external_candidate_uncertain = if !is_local_occurrence && !candidates.is_empty() {
            if super::super::overload::candidate_target_count_after_overload_grouping(
                index,
                &candidates,
                cancel,
                budget,
            )? != 1
            {
                true
            } else if is_right_hand_member(identifier) {
                false
            } else {
                index.flow_candidate_has_shadowing_uncertainty(
                    uri,
                    document,
                    offset,
                    &key,
                    &candidates[0],
                    cancel,
                    budget,
                )?
            }
        } else {
            false
        };
        let local_resolution_uncertain = candidates.len() != 1
            || receiver_uncertain
            || member_lookup_incomplete
            || ambiguous
            || inaccessible;
        let compiler_surface_only_resolution = candidates.is_empty()
            && compiler_surface_resolved
            && is_right_hand_member(identifier)
            && !receiver_uncertain
            && !inaccessible;
        let external_resolution_uncertain = !compiler_surface_only_resolution
            && (candidates.is_empty() && !compiler_surface_resolved
                || receiver_uncertain
                || member_lookup_incomplete
                || inaccessible
                || ambiguous
                || external_candidate_uncertain);
        if is_local_occurrence && local_resolution_uncertain
            || !is_local_occurrence && external_resolution_uncertain
        {
            let inside_with = is_inside_with_body(identifier, &body_nodes, cancel, budget)?;
            if is_local_occurrence && !inside_with || !is_local_occurrence {
                note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
            }
            continue;
        }
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
                let Some(locations) = statement_locations_for_node(
                    &cfg,
                    statement_index.as_ref(),
                    identifier,
                    cancel,
                    budget,
                )?
                else {
                    note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
                    continue;
                };
                let access =
                    classify_access(identifier, span, &document.source, &call_argument_accesses);
                if access == Access::AddressOf {
                    locals[binding.var].address_taken = true;
                }
                for (block, statement) in locations {
                    pending.push(PendingOccurrence {
                        var: binding.var,
                        span,
                        access,
                        block,
                        statement_start: statement.start,
                        semantic_order: assignment_order(identifier),
                    });
                }
                continue;
            }
            if is_result_name && routine_candidate.as_ref() == Some(candidate) {
                let var = result_local.expect("result variable exists for functions");
                let span = SourceSpan {
                    start: identifier.start_byte(),
                    end: identifier.end_byte(),
                };
                let Some(locations) = statement_locations_for_node(
                    &cfg,
                    statement_index.as_ref(),
                    identifier,
                    cancel,
                    budget,
                )?
                else {
                    note_exclusion(&mut excluded, FlowExclusion::UnresolvedIdentity);
                    continue;
                };
                let access =
                    classify_access(identifier, span, &document.source, &call_argument_accesses);
                for (block, statement) in locations {
                    pending.push(PendingOccurrence {
                        var,
                        span,
                        access,
                        block,
                        statement_start: statement.start,
                        semantic_order: assignment_order(identifier),
                    });
                }
            }
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
    for occurrence in &pending {
        if occurrence.access == Access::UnknownCallEffect {
            locals[occurrence.var].unknown_call_effect = true;
        }
    }
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
        cfg: Some(cfg),
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
    routine_symbol_index: Option<usize>,
    routine_scope: Option<usize>,
    local_symbol_indices: &[usize],
    nested_ranges: &[Range<usize>],
    routine_nodes: &[Node<'_>],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<CollectedLocals, String> {
    let routine_symbol = routine_symbol_index.and_then(|index| document.symbols.get(index));
    let Some(routine_scope) = routine_scope else {
        return Ok(CollectedLocals {
            locals: Vec::new(),
            bindings: Vec::new(),
            routine_scope: None,
        });
    };
    let mut locals = Vec::new();
    let mut bindings = Vec::new();
    for &symbol_index in local_symbol_indices {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let Some(symbol) = document.symbols.get(symbol_index) else {
            continue;
        };
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
        let kind = if declaration_node
            .is_some_and(|declaration| declaration.kind() == "exceptionHandler")
        {
            LocalKind::ExceptionHandler
        } else {
            mode.map_or(LocalKind::Local, LocalKind::Parameter)
        };
        let ty = classify_symbol_type(document, symbol);
        let class_reference = symbol_is_class_reference(document, symbol);
        let type_resolved = symbol.type_shape.as_ref().is_some_and(|shape| {
            type_shape_is_resolved(document, symbol.scope, shape, &mut HashSet::new())
        });
        let unresolved_local_generic_argument = symbol.type_shape.as_ref().is_some_and(|shape| {
            type_shape_has_unresolved_local_generic_argument(document, symbol.scope, shape)
        });
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
            class_reference,
            type_resolved,
            unresolved_local_generic_argument,
            unknown_call_effect: false,
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
        let type_resolved = routine_symbol
            .implementation_result_type_ref
            .as_ref()
            .or(routine_symbol.result_type_ref.as_ref())
            .is_some_and(|type_ref| {
                type_ref_is_resolved(document, routine_scope, type_ref, &mut HashSet::new())
            });
        let unresolved_local_generic_argument = routine_symbol
            .implementation_result_type_ref
            .as_ref()
            .or(routine_symbol.result_type_ref.as_ref())
            .is_some_and(|type_ref| {
                type_ref_has_unresolved_local_generic_argument(document, routine_scope, type_ref)
            });
        locals.push(LocalVar {
            name: "Result".to_owned(),
            decl_span: SourceSpan {
                start: routine_symbol.span.start,
                end: routine_symbol.span.end,
            },
            kind: LocalKind::Result,
            ty,
            class_reference: false,
            type_resolved,
            unresolved_local_generic_argument,
            unknown_call_effect: false,
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

fn symbol_is_class_reference(document: &Document, symbol: &Symbol) -> bool {
    symbol.type_shape.as_ref().is_some_and(|shape| {
        type_shape_is_class_reference(document, symbol.scope, shape, &mut HashSet::new())
    })
}

fn type_shape_is_class_reference(
    document: &Document,
    scope: usize,
    shape: &TypeShape,
    visited: &mut HashSet<(usize, String)>,
) -> bool {
    match shape {
        TypeShape::Named(type_ref) => {
            type_ref_is_class_reference(document, scope, type_ref, visited)
        }
        TypeShape::Pointer(_)
        | TypeShape::Array { .. }
        | TypeShape::Callable
        | TypeShape::Unknown => false,
    }
}

fn type_ref_is_class_reference(
    document: &Document,
    scope: usize,
    type_ref: &TypeRef,
    visited: &mut HashSet<(usize, String)>,
) -> bool {
    let Some(name) = type_ref.path.last() else {
        return false;
    };
    let key = canonical_name(name);
    if key == "tobject" {
        return true;
    }
    if !visited.insert((scope, key.clone())) {
        return false;
    }
    let result = document
        .symbols
        .iter()
        .filter(|candidate| {
            candidate.kind == SymbolKind::Type
                && candidate.key == key
                && scope_is_within(document, scope, candidate.scope)
        })
        .max_by_key(|candidate| scope_depth(document, candidate.scope))
        .is_some_and(|candidate| {
            candidate.type_kind == TypeKind::Class
                || candidate.type_shape.as_ref().is_some_and(|shape| {
                    type_shape_is_class_reference(document, candidate.scope, shape, visited)
                })
                || candidate.type_ref.as_ref().is_some_and(|underlying| {
                    type_ref_is_class_reference(document, candidate.scope, underlying, visited)
                })
        });
    visited.remove(&(scope, key));
    result
}

fn type_shape_is_resolved(
    document: &Document,
    scope: usize,
    shape: &TypeShape,
    visited: &mut HashSet<(usize, String)>,
) -> bool {
    match shape {
        TypeShape::Named(type_ref) => type_ref_is_resolved(document, scope, type_ref, visited),
        TypeShape::Pointer(inner) | TypeShape::Array { element: inner, .. } => {
            type_shape_is_resolved(document, scope, inner, visited)
        }
        TypeShape::Callable => true,
        TypeShape::Unknown => false,
    }
}

fn type_ref_is_resolved(
    document: &Document,
    scope: usize,
    type_ref: &TypeRef,
    visited: &mut HashSet<(usize, String)>,
) -> bool {
    if type_ref
        .args
        .iter()
        .any(|argument| !type_ref_is_resolved(document, scope, argument, visited))
    {
        return false;
    }
    let Some(name) = type_ref.path.last() else {
        return false;
    };
    let key = canonical_name(name);
    if is_builtin_type_name(&key) {
        return true;
    }
    if !visited.insert((scope, key.clone())) {
        return false;
    }
    let resolved = document
        .symbols
        .iter()
        .filter(|candidate| {
            candidate.kind == SymbolKind::Type
                && candidate.key == key
                && scope_is_within(document, scope, candidate.scope)
        })
        .max_by_key(|candidate| scope_depth(document, candidate.scope))
        .is_some_and(|candidate| {
            if matches!(
                candidate.type_kind,
                TypeKind::Class
                    | TypeKind::Record
                    | TypeKind::Interface
                    | TypeKind::Enum
                    | TypeKind::File
                    | TypeKind::String
            ) {
                return true;
            }
            candidate
                .type_shape
                .as_ref()
                .map(|shape| type_shape_is_resolved(document, candidate.scope, shape, visited))
                .or_else(|| {
                    candidate.type_ref.as_ref().map(|underlying| {
                        type_ref_is_resolved(document, candidate.scope, underlying, visited)
                    })
                })
                .unwrap_or(true)
        });
    visited.remove(&(scope, key));
    resolved
}

fn type_shape_has_unresolved_local_generic_argument(
    document: &Document,
    scope: usize,
    shape: &TypeShape,
) -> bool {
    match shape {
        TypeShape::Named(type_ref) => {
            type_ref_has_unresolved_local_generic_argument(document, scope, type_ref)
        }
        TypeShape::Pointer(inner) | TypeShape::Array { element: inner, .. } => {
            type_shape_has_unresolved_local_generic_argument(document, scope, inner)
        }
        TypeShape::Callable | TypeShape::Unknown => false,
    }
}

fn type_ref_has_unresolved_local_generic_argument(
    document: &Document,
    scope: usize,
    type_ref: &TypeRef,
) -> bool {
    let Some(name) = type_ref.path.last() else {
        return false;
    };
    let key = canonical_name(name);
    let local_type = document
        .symbols
        .iter()
        .filter(|candidate| {
            candidate.kind == SymbolKind::Type
                && candidate.key == key
                && scope_is_within(document, scope, candidate.scope)
        })
        .max_by_key(|candidate| scope_depth(document, candidate.scope));
    let has_unresolved_argument = local_type.is_some_and(|candidate| {
        !candidate.generic_parameters.is_empty()
            && type_ref.args.iter().any(|argument| {
                !type_ref_is_resolved(document, scope, argument, &mut HashSet::new())
                    && !type_ref_is_generic_parameter(document, argument)
            })
    });
    has_unresolved_argument
        || type_ref.args.iter().any(|argument| {
            type_ref_has_unresolved_local_generic_argument(document, scope, argument)
        })
}

fn type_ref_is_generic_parameter(document: &Document, type_ref: &TypeRef) -> bool {
    let Some(name) = type_ref.path.last() else {
        return false;
    };
    let key = canonical_name(name);
    document
        .generic_parameter_contexts
        .iter()
        .any(|context| context.span.contains(type_ref.span) && context.names.contains(&key))
}

fn is_builtin_type_name(key: &str) -> bool {
    matches!(
        key,
        "integer"
            | "smallint"
            | "shortint"
            | "byte"
            | "word"
            | "longword"
            | "cardinal"
            | "longint"
            | "int64"
            | "uint64"
            | "nativeint"
            | "nativeuint"
            | "boolean"
            | "bytebool"
            | "wordbool"
            | "longbool"
            | "char"
            | "widechar"
            | "ansichar"
            | "real"
            | "single"
            | "double"
            | "extended"
            | "currency"
            | "comp"
            | "string"
            | "ansistring"
            | "widestring"
            | "unicodestring"
            | "shortstring"
            | "pointer"
            | "variant"
            | "olevariant"
            | "tobject"
    )
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
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<FlowExclusion>, String> {
    let mut excluded = None;
    let mut unknown_conditional = !document.conditionals.complete;
    for span in &document.conditionals.unknown_spans {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if source_span_affects_routine(span.start, span.end, node, nested_ranges, cancel, budget)? {
            unknown_conditional = true;
            break;
        }
    }
    if unknown_conditional {
        note_exclusion(&mut excluded, FlowExclusion::UnknownConditional);
    }
    for span in &document.parser_recovery_spans {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if source_span_affects_routine(span.start, span.end, node, nested_ranges, cancel, budget)? {
            note_exclusion(&mut excluded, FlowExclusion::Recovery);
            break;
        }
    }
    for candidate in body_nodes {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if candidate.kind() == "asm" {
            note_exclusion(&mut excluded, FlowExclusion::Asm);
            break;
        }
    }
    let mut contains_goto = false;
    for candidate in body_nodes {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if matches!(
            candidate.kind(),
            "goto" | "label" | "labeledStatement" | "labeledStatementTr"
        ) {
            contains_goto = true;
            break;
        }
    }
    if !contains_goto {
        for edge in cfg.graph.edge_weights() {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if *edge == EdgeKind::Goto {
                contains_goto = true;
                break;
            }
        }
    }
    if contains_goto {
        note_exclusion(&mut excluded, FlowExclusion::Goto);
    }
    for candidate in body_nodes {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if matches!(candidate.kind(), "varDef" | "varAssignDef") {
            note_exclusion(&mut excluded, FlowExclusion::InlineVariable);
            break;
        }
    }
    Ok(excluded)
}

fn source_span_affects_routine(
    start: usize,
    end: usize,
    routine: Node<'_>,
    nested_ranges: &[Range<usize>],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let intersects = if start == end {
        routine.start_byte() <= start && start <= routine.end_byte()
    } else {
        start < routine.end_byte() && routine.start_byte() < end
    };
    if !intersects {
        return Ok(false);
    }
    for range in nested_ranges {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if range.start <= start && end <= range.end {
            return Ok(false);
        }
    }
    Ok(true)
}

fn note_exclusion(excluded: &mut Option<FlowExclusion>, reason: FlowExclusion) {
    if excluded.is_none() {
        *excluded = Some(reason);
    }
}

pub(super) fn collect_nodes<'a>(
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
        let mut children = Vec::new();
        for child in node.children(&mut cursor) {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            children.push(child);
        }
        pending.extend(children.into_iter().rev());
    }
    Ok(nodes)
}

fn collect_routine_nodes<'a>(
    root: Node<'a>,
    body: Node<'a>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<CollectedRoutineNodes<'a>, String> {
    let mut owned_nodes = Vec::new();
    let mut body_nodes = Vec::new();
    let mut ranges = Vec::new();
    let mut pending = vec![(root, true)];
    while let Some((node, owned)) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if owned {
            budget.require_owned_bytes(std::mem::size_of::<Node<'a>>(), cancel)?;
            owned_nodes.push(node);
            if node.start_byte() >= body.start_byte() && node.end_byte() <= body.end_byte() {
                budget.require_owned_bytes(std::mem::size_of::<Node<'a>>(), cancel)?;
                body_nodes.push(node);
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let nested = matches!(child.kind(), "defProc" | "lambda");
            if nested {
                budget.require_owned_bytes(std::mem::size_of::<Range<usize>>(), cancel)?;
                ranges.push(child.start_byte()..child.end_byte());
            }
            pending.push((child, owned && !nested));
        }
    }
    Ok(CollectedRoutineNodes {
        all_nodes: owned_nodes,
        body_nodes,
        nested_ranges: ranges,
    })
}

fn collect_child_routines<'a>(
    root: Node<'a>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<Node<'a>>, String> {
    let mut routines = Vec::new();
    let mut cursor = root.walk();
    let mut pending = root.children(&mut cursor).collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if matches!(node.kind(), "defProc" | "lambda") {
            budget.require_owned_bytes(std::mem::size_of::<Node<'a>>(), cancel)?;
            routines.push(node);
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    Ok(routines)
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
    for &node in body_nodes {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if node.kind() != "for" {
            continue;
        }
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

pub(super) fn mark_possible_with_targets(
    document: &Document,
    body_nodes: &[Node<'_>],
    locals: &mut [LocalVar],
    local_names: &HashSet<String>,
    excluded: &mut Option<FlowExclusion>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    for &with_node in body_nodes {
        check_navigation_cancel(cancel)?;
        if !budget.take_work(1, cancel)? {
            note_exclusion(excluded, FlowExclusion::Budget);
            return Ok(false);
        }
        check_navigation_cancel(cancel)?;
        if with_node.kind() != "with" {
            continue;
        }
        let Some(body) = with_node.child_by_field_name("body") else {
            continue;
        };
        for &identifier in body_nodes {
            check_navigation_cancel(cancel)?;
            if !budget.take_work(1, cancel)? {
                note_exclusion(excluded, FlowExclusion::Budget);
                return Ok(false);
            }
            check_navigation_cancel(cancel)?;
            if identifier.kind() != "identifier"
                || body.start_byte() > identifier.start_byte()
                || identifier.end_byte() > body.end_byte()
                || is_right_hand_member(identifier)
            {
                continue;
            }
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
    Ok(true)
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
    function_ranges: &HashSet<(usize, usize)>,
    excluded: &mut Option<FlowExclusion>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<(), String> {
    if bindings.is_empty() || nested_ranges.is_empty() {
        return Ok(());
    }
    let mut local_names = locals
        .iter()
        .map(|local| canonical_name(&local.name))
        .collect::<HashSet<_>>();
    let mut result_names = HashSet::new();
    if let Some(result) = locals
        .iter()
        .position(|local| local.kind == LocalKind::Result)
    {
        result_names.insert(canonical_name(&locals[result].name));
    }
    for binding in bindings.iter().filter(|binding| binding.synthetic_result) {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if let Some(symbol) = index
            .documents
            .get(&binding.candidate.uri)
            .and_then(|document| document.symbols.get(binding.candidate.index))
        {
            budget.require_owned_bytes(symbol.name.len(), cancel)?;
            let name = canonical_name(&symbol.name);
            local_names.insert(name.clone());
            result_names.insert(name);
        }
    }
    let mut pending = vec![routine_node];
    while let Some(node) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let is_nested =
            node.id() != routine_node.id() && matches!(node.kind(), "defProc" | "lambda");
        if is_nested {
            let nested_has_own_result = node.kind() == "defProc"
                && routine_has_function_result(function_ranges, node, cancel, budget)?;
            let child_routines = collect_child_routines(node, cancel, budget)?;
            pending.extend(child_routines);
            let nested_nodes = collect_owned_nodes(node, node.id(), cancel, budget)?;
            for identifier in nested_nodes {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if identifier.kind() != "identifier" {
                    continue;
                }
                let key = canonical_name(&node_text(identifier, &document.source));
                if !local_names.contains(&key) || is_declaration_name(identifier) {
                    continue;
                }
                let candidates =
                    resolve_identifier(index, uri, document, identifier, cancel, budget)?;
                if candidates.len() == 1
                    && let Some(binding) = bindings
                        .iter()
                        .find(|binding| binding.candidate == candidates[0])
                {
                    locals[binding.var].captured = true;
                } else if candidates.iter().any(|candidate| {
                    bindings
                        .iter()
                        .any(|binding| binding.candidate == *candidate)
                }) {
                    for binding in bindings.iter().filter(|binding| {
                        candidates
                            .iter()
                            .any(|candidate| candidate == &binding.candidate)
                    }) {
                        locals[binding.var].captured = true;
                    }
                } else if candidates.is_empty() {
                    let is_result_pseudo = key == "result"
                        && !nested_has_own_result
                        && !is_right_hand_member(identifier)
                        && !is_call_entity(identifier);
                    let is_result_name = key != "result"
                        && result_names.contains(&key)
                        && !is_right_hand_member(identifier)
                        && !is_call_entity(identifier);
                    if let Some(result) = locals
                        .iter_mut()
                        .find(|local| local.kind == LocalKind::Result)
                        && (is_result_pseudo || is_result_name)
                    {
                        result.captured = true;
                    } else {
                        note_exclusion(excluded, FlowExclusion::UnresolvedIdentity);
                    }
                }
            }
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    Ok(())
}

fn routine_has_function_result(
    function_ranges: &HashSet<(usize, usize)>,
    node: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    check_navigation_cancel(cancel)?;
    budget.require_work(1, cancel)?;
    Ok(function_ranges.contains(&(node.start_byte(), node.end_byte())))
}

fn call_argument_accesses(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    body_nodes: &[Node<'_>],
    local_names: &HashSet<String>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<CallArgumentAccesses, String> {
    let mut result = CallArgumentAccesses::default();
    for call in body_nodes.iter().copied() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if call.kind() != "exprCall" {
            continue;
        }
        let Some(arguments) = call.child_by_field_name("args") else {
            continue;
        };
        let entity = call.child_by_field_name("entity");
        let callee_name = entity.map(|entity| canonical_name(&node_text(entity, &document.source)));
        let mut cursor = arguments.walk();
        let mut argument_index = 0;
        for argument in arguments.named_children(&mut cursor) {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if argument.kind() == "legacyFormat" {
                continue;
            }
            let expression = if argument.kind() == "namedArgument" {
                argument.child_by_field_name("value").unwrap_or(argument)
            } else {
                argument
            };
            let lvalue_base =
                call_argument_lvalue_base(expression, &document.source, cancel, budget)?;
            let target_is_local = lvalue_base.is_some_and(|target| {
                target.kind() == "identifier"
                    && local_names.contains(&canonical_name(&node_text(target, &document.source)))
            });
            let fallback_targets = if lvalue_base.is_none() {
                local_identifiers_in_expression(
                    expression,
                    &document.source,
                    local_names,
                    cancel,
                    budget,
                )?
            } else {
                Vec::new()
            };
            if !target_is_local && fallback_targets.is_empty() {
                argument_index += 1;
                continue;
            }
            let mode = super::super::overload::parameter_mode_for_argument(
                index,
                uri,
                document,
                call,
                argument_index,
                cancel,
                budget,
            )?;
            result.target_uncertain |= matches!(
                mode,
                super::super::overload::ParameterModeResolution::UncertainTarget
                    | super::super::overload::ParameterModeResolution::Resolved(None)
            );
            let access = if matches!(callee_name.as_deref(), Some("inc" | "dec"))
                && mode
                    == super::super::overload::ParameterModeResolution::Intrinsic(Some(
                        ParameterMode::Var,
                    )) {
                Access::ReadWrite
            } else if callee_name.as_deref() == Some("addr")
                && mode == super::super::overload::ParameterModeResolution::Unresolved
            {
                Access::AddressOf
            } else {
                call_argument_access_for_mode(mode)
            };

            if access != Access::Read {
                if target_is_local {
                    let target = lvalue_base.expect("a local call target is an lvalue base");
                    insert_call_argument_access(
                        &mut result.by_span,
                        (target.start_byte(), target.end_byte()),
                        access,
                    );
                } else {
                    // The argument is not a recognized variable lvalue. Preserve
                    // soundness by assigning a conservative variable-level effect
                    // to every local it references rather than silently treating
                    // the whole expression as a read.
                    for identifier in fallback_targets {
                        let fallback_access = if access == Access::UnknownCallEffect {
                            Access::UnknownCallEffect
                        } else {
                            Access::ReadWrite
                        };
                        insert_call_argument_access(
                            &mut result.by_span,
                            (identifier.start_byte(), identifier.end_byte()),
                            fallback_access,
                        );
                    }
                }
            }
            argument_index += 1;
        }
    }
    Ok(result)
}

fn call_argument_access_for_mode(mode: super::super::overload::ParameterModeResolution) -> Access {
    match mode {
        super::super::overload::ParameterModeResolution::Resolved(Some(ParameterMode::Var))
        | super::super::overload::ParameterModeResolution::Intrinsic(Some(ParameterMode::Var)) => {
            Access::VarArg
        }
        super::super::overload::ParameterModeResolution::Resolved(Some(ParameterMode::Out))
        | super::super::overload::ParameterModeResolution::Intrinsic(Some(ParameterMode::Out)) => {
            Access::OutArg
        }
        super::super::overload::ParameterModeResolution::Unresolved
        | super::super::overload::ParameterModeResolution::UncertainTarget
        | super::super::overload::ParameterModeResolution::Resolved(None)
        | super::super::overload::ParameterModeResolution::Intrinsic(None) => {
            Access::UnknownCallEffect
        }
        _ => Access::Read,
    }
}

fn insert_call_argument_access(
    accesses: &mut HashMap<(usize, usize), Access>,
    span: (usize, usize),
    access: Access,
) {
    accesses
        .entry(span)
        .and_modify(|existing| {
            if *existing != Access::UnknownCallEffect || access == Access::UnknownCallEffect {
                *existing = access;
            }
        })
        .or_insert(access);
}

fn call_argument_lvalue_base<'tree>(
    mut expression: Node<'tree>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Node<'tree>>, String> {
    loop {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        match expression.kind() {
            "exprParens" if expression.named_child_count() == 1 => {
                let Some(child) = expression.named_child(0) else {
                    return Ok(None);
                };
                expression = child;
            }
            "exprDot" | "genericDot" => {
                let Some(lhs) = expression.child_by_field_name("lhs") else {
                    return Ok(None);
                };
                expression = lhs;
            }
            "exprSubscript" => {
                let Some(entity) = expression.child_by_field_name("entity") else {
                    return Ok(None);
                };
                expression = entity;
            }
            "exprUnary"
                if expression
                    .child_by_field_name("operator")
                    .is_some_and(|operator| node_text(operator, source) == "^") =>
            {
                let Some(operand) = expression.child_by_field_name("operand") else {
                    return Ok(None);
                };
                expression = operand;
            }
            "identifier" => return Ok(Some(expression)),
            _ => return Ok(None),
        }
    }
}

fn local_identifiers_in_expression<'tree>(
    expression: Node<'tree>,
    source: &str,
    local_names: &HashSet<String>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<Node<'tree>>, String> {
    let mut identifiers = Vec::new();
    let mut pending = vec![expression];
    while let Some(candidate) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if candidate.kind() == "identifier"
            && local_names.contains(&canonical_name(&node_text(candidate, source)))
            && !is_declaration_name(candidate)
            && !is_right_hand_member(candidate)
            && !is_call_entity(candidate)
        {
            budget.require_owned_bytes(std::mem::size_of::<Node<'tree>>(), cancel)?;
            identifiers.push(candidate);
        }
        let mut cursor = candidate.walk();
        pending.extend(candidate.named_children(&mut cursor));
    }
    Ok(identifiers)
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

fn is_control_transfer_identifier(identifier: Node<'_>, source: &str) -> bool {
    let name = node_text(identifier, source);
    ["exit", "break", "continue", "raise"]
        .iter()
        .any(|keyword| name.eq_ignore_ascii_case(keyword))
}

fn exit_call_with_argument<'tree>(identifier: Node<'tree>, source: &str) -> Option<Node<'tree>> {
    if !node_text(identifier, source).eq_ignore_ascii_case("exit") {
        return None;
    }
    let call = identifier
        .parent()
        .filter(|parent| parent.kind() == "exprCall")?;
    let entity = call.child_by_field_name("entity")?;
    let args = call.child_by_field_name("args")?;
    (entity.id() == identifier.id() && args.named_child_count() > 0).then_some(call)
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
    parent
        .child_by_field_name("rhs")
        .or_else(|| parent.child_by_field_name("right"))
        .is_some_and(|right| {
            right.start_byte() <= identifier.start_byte()
                && identifier.end_byte() <= right.end_byte()
        })
}

fn is_inside_with_body(
    identifier: Node<'_>,
    body_nodes: &[Node<'_>],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    for &node in body_nodes {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if node.kind() == "with"
            && node.child_by_field_name("body").is_some_and(|body| {
                body.start_byte() <= identifier.start_byte()
                    && identifier.end_byte() <= body.end_byte()
            })
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn statement_locations(
    cfg: &Cfg,
    span: SourceSpan,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<StatementLocations>, String> {
    let mut matches = Vec::new();
    for node_index in cfg.graph.node_indices() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let block = &cfg.graph[node_index];
        for statement in &block.stmts {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if statement.byte_range.start <= span.start && span.end <= statement.byte_range.end {
                budget
                    .require_owned_bytes(std::mem::size_of::<(BlockId, Range<usize>)>(), cancel)?;
                matches.push((
                    BlockId::from(node_index),
                    statement.byte_range.clone(),
                    statement.byte_range.end - statement.byte_range.start,
                ));
            }
        }
    }
    let Some(shortest) = matches.iter().map(|(_, _, length)| *length).min() else {
        return Ok(None);
    };
    matches.retain(|(_, _, length)| *length == shortest);
    matches.sort_by_key(|(block, _, _)| block.index().index());
    matches.dedup_by_key(|(block, _, _)| *block);
    Ok(Some(
        matches
            .into_iter()
            .map(|(block, range, _)| (block, range))
            .collect(),
    ))
}

fn statement_locations_for_node(
    cfg: &Cfg,
    index: Option<&StatementLocationIndex>,
    node: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<StatementLocations>, String> {
    if let Some(index) = index {
        index.locations_for_node(node, cancel, budget)
    } else {
        statement_locations(
            cfg,
            SourceSpan {
                start: node.start_byte(),
                end: node.end_byte(),
            },
            cancel,
            budget,
        )
    }
}
