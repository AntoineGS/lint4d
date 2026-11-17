use super::super::dcc_switches::SwitchState;
use super::super::warning_switches::WarningSwitches;
use super::super::{
    AssistanceBudget, Candidate, DccMessage, DccSeverity, Document, NavigationIndex,
    ResolutionState, SemanticDiagnostic, SemanticDiagnosticKind, SourceSpan, SymbolKind,
    check_navigation_cancel, node_text,
};
use super::model::{RoutineFlow, VarOccurrence, collect_nodes};
use super::{Access, LocalKind, VarType};
use cfg_pascal::cfg_core::types::{Cfg, EdgeKind};
use lsp_types::Url;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use tree_sitter::Node;

const USE: u8 = 1;
const DEF: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopKind {
    For,
    While,
    ForIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConstantValue {
    Integer(i128),
    Boolean(bool),
}

#[derive(Debug, Clone, Copy)]
struct LoopFact<'tree> {
    node: Node<'tree>,
    body: Node<'tree>,
    kind: LoopKind,
    zero_iterations: Option<bool>,
    iterator: Option<Node<'tree>>,
    iterator_span: Option<SourceSpan>,
    body_has_break: bool,
    body_has_continue: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn flow_diagnostics(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    flows: &[RoutineFlow<'_>],
    prior_diagnostics: &[SemanticDiagnostic],
    capacity: usize,
    diagnostic_cap_reached: &mut bool,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<SemanticDiagnostic>, String> {
    let mut diagnostics = Vec::new();
    let warning_switches = WarningSwitches::new(document);
    for flow in flows {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if flow.excluded.is_some() || flow.cfg.is_none() {
            continue;
        }
        let loop_facts = collect_loop_facts(index, uri, document, flow, cancel, budget)?;
        let mut active_occurrences = Vec::new();
        for occurrence in &flow.occurrences {
            if !is_inactive(document, occurrence.span, cancel, budget)?
                && !occurrence_in_deleted_loop(occurrence, &loop_facts)
            {
                active_occurrences.push(occurrence);
            }
        }
        let locals = &flow.locals;

        for (local_index, local) in locals.iter().enumerate() {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if !matches!(local.kind, LocalKind::Local | LocalKind::LoopVar)
                || local.address_taken
                || local.captured
                || local.absolute
                || local.possible_with_target
            {
                continue;
            }
            let has_occurrence = active_occurrences
                .iter()
                .any(|occurrence| occurrence.var == local_index);
            if !has_occurrence {
                if diagnostics.len() >= capacity {
                    *diagnostic_cap_reached = true;
                    return Ok(diagnostics);
                }
                let message = DccMessage::H2164.format(&[&local.name, &flow.name]);
                diagnostics.push(SemanticDiagnostic {
                    kind: super::super::SemanticDiagnosticKind::Dcc(DccMessage::H2164),
                    span: local.decl_span,
                    message,
                });
            }
        }

        if routine_has_error(
            flow,
            document,
            prior_diagnostics,
            &warning_switches,
            cancel,
            budget,
        )? {
            continue;
        }

        let compiler_version = document.conditional_context.compiler_version;
        if w1035_w1036_supported(compiler_version) {
            for diagnostic in definite_assignment_diagnostics(
                flow,
                locals,
                &loop_facts,
                &active_occurrences,
                compiler_version,
                cancel,
                budget,
            )? {
                if diagnostics.len() >= capacity {
                    *diagnostic_cap_reached = true;
                    return Ok(diagnostics);
                }
                diagnostics.push(diagnostic);
            }
        }

        append_loop_diagnostics(
            index,
            uri,
            document,
            flow,
            &loop_facts,
            &active_occurrences,
            &mut diagnostics,
            capacity,
            diagnostic_cap_reached,
            cancel,
            budget,
        )?;
        if *diagnostic_cap_reached {
            return Ok(diagnostics);
        }

        let eligible_vars = locals
            .iter()
            .enumerate()
            .filter_map(|(var, local)| {
                (local.kind == LocalKind::Local
                    && local.ty == VarType::Scalar
                    && local.type_resolved
                    && !local.unknown_call_effect
                    && !local.address_taken
                    && !local.captured
                    && !local.absolute
                    && !local.possible_with_target
                    && active_occurrences.iter().any(|occurrence| {
                        occurrence.var == var && occurrence.access == Access::Write
                    }))
                .then_some(var)
            })
            .collect::<Vec<_>>();
        if eligible_vars.is_empty() {
            continue;
        }

        let routine_nodes = collect_nodes(flow.node, cancel, budget)?;
        let inline_reads = inline_argument_reads(
            index,
            uri,
            document,
            &routine_nodes,
            &active_occurrences,
            cancel,
            budget,
        )?;
        let forced_for_exits = definitely_nonempty_for_exits(flow, &loop_facts, cancel, budget)?;
        let dead_writes = dead_write_spans(
            flow.cfg.as_ref().expect("checked CFG"),
            locals.len(),
            &eligible_vars,
            &active_occurrences,
            &inline_reads,
            &forced_for_exits,
            cancel,
            budget,
        )?;
        for (var, span) in dead_writes {
            if diagnostics.len() >= capacity {
                *diagnostic_cap_reached = true;
                return Ok(diagnostics);
            }
            let local = &locals[var];
            diagnostics.push(SemanticDiagnostic {
                kind: super::super::SemanticDiagnosticKind::Dcc(DccMessage::H2077),
                span,
                message: DccMessage::H2077.format(&[&local.name]),
            });
        }
    }
    diagnostics.sort_by_key(|diagnostic| (diagnostic.span.start, diagnostic.span.end));
    Ok(diagnostics)
}

#[derive(Debug)]
struct MustFlowGraph {
    state_blocks: Vec<usize>,
    outgoing: Vec<Vec<(usize, EdgeKind)>>,
    incoming: Vec<Vec<(usize, EdgeKind)>>,
    reachable: Vec<bool>,
    entry: usize,
    exit: usize,
    for_in_binders: HashSet<(usize, usize)>,
    for_in_iterator_occurrences: HashSet<(usize, usize, SourceSpan)>,
    for_in_vars: HashSet<usize>,
}

#[derive(Debug, Default)]
struct DefiniteAssignmentFacts {
    first_uninitialized_read: Option<SourceSpan>,
    normal_exit_uninitialized: bool,
}

fn w1035_w1036_supported(compiler_version: Option<pascal_project::CompilerVersion>) -> bool {
    matches!(
        compiler_version.map(|version| (version.major, version.minor)),
        Some((21, 0) | (24, 0) | (37, 0))
    )
}

#[allow(clippy::too_many_arguments)]
fn definite_assignment_diagnostics(
    flow: &RoutineFlow<'_>,
    locals: &[super::LocalVar],
    loop_facts: &[LoopFact<'_>],
    occurrences: &[&VarOccurrence],
    compiler_version: Option<pascal_project::CompilerVersion>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<SemanticDiagnostic>, String> {
    let Some(cfg) = flow.cfg.as_ref() else {
        return Ok(Vec::new());
    };
    let graph = build_must_flow_graph(flow, loop_facts, occurrences, cancel, budget)?;
    let compiler = compiler_version.map(|version| (version.major, version.minor));
    let mut diagnostics = Vec::new();

    if let Some(result) = locals
        .iter()
        .position(|local| local.kind == LocalKind::Result)
        && local_is_definite_assignment_eligible(&locals[result], result, occurrences)
    {
        let facts = definite_assignment_facts(cfg, &graph, result, occurrences, cancel, budget)?;
        if facts.normal_exit_uninitialized
            && let Some(span) = final_end_span(flow, cancel, budget)?
        {
            budget.require_owned_bytes(std::mem::size_of::<SemanticDiagnostic>(), cancel)?;
            let message = DccMessage::W1035.format(&[&flow.name]);
            diagnostics.push(SemanticDiagnostic {
                kind: super::super::SemanticDiagnosticKind::Dcc(DccMessage::W1035),
                span,
                message,
            });
        }
    }

    for (var, local) in locals.iter().enumerate() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let eligible_kind = if graph.for_in_vars.contains(&var) {
            compiler == Some((37, 0))
        } else {
            local.kind == LocalKind::Local
        };
        if !eligible_kind || !local_is_definite_assignment_eligible(local, var, occurrences) {
            continue;
        }
        let facts = definite_assignment_facts(cfg, &graph, var, occurrences, cancel, budget)?;
        let Some(span) = facts.first_uninitialized_read else {
            continue;
        };
        budget.require_owned_bytes(std::mem::size_of::<SemanticDiagnostic>(), cancel)?;
        diagnostics.push(SemanticDiagnostic {
            kind: super::super::SemanticDiagnosticKind::Dcc(DccMessage::W1036),
            span,
            message: DccMessage::W1036.format(&[&local.name]),
        });
    }
    Ok(diagnostics)
}

fn local_is_definite_assignment_eligible(
    local: &super::LocalVar,
    var: usize,
    occurrences: &[&VarOccurrence],
) -> bool {
    (local.ty == VarType::Scalar || local.class_reference)
        && local.type_resolved
        && !local.unresolved_local_generic_argument
        && !local.unknown_call_effect
        && !local.address_taken
        && !local.captured
        && !local.absolute
        && !local.possible_with_target
        && !occurrences.iter().any(|occurrence| {
            occurrence.var == var && matches!(occurrence.access, Access::OutArg | Access::VarArg)
        })
}

#[allow(clippy::too_many_arguments)]
fn build_must_flow_graph(
    flow: &RoutineFlow<'_>,
    loop_facts: &[LoopFact<'_>],
    occurrences: &[&VarOccurrence],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<MustFlowGraph, String> {
    let cfg = flow.cfg.as_ref().expect("must-flow graph requires a CFG");
    let block_count = cfg.graph.node_count();
    let forced = definitely_nonempty_for_exits(flow, loop_facts, cancel, budget)?;
    let mut forced_blocks = forced
        .into_iter()
        .filter(|block| *block < block_count)
        .collect::<Vec<_>>();
    forced_blocks.sort_unstable();
    let state_count = block_count.saturating_add(forced_blocks.len());
    let graph_bytes = state_count
        .saturating_mul(
            std::mem::size_of::<usize>()
                + std::mem::size_of::<bool>()
                + std::mem::size_of::<Vec<(usize, EdgeKind)>>() * 2,
        )
        .saturating_add(
            cfg.graph
                .edge_count()
                .saturating_mul(4)
                .saturating_mul(std::mem::size_of::<(usize, EdgeKind)>()),
        )
        .saturating_add(occurrences.len().saturating_mul(std::mem::size_of::<(
            usize,
            usize,
            SourceSpan,
        )>()));
    budget.require_owned_bytes(graph_bytes, cancel)?;

    let mut state_blocks = (0..block_count).collect::<Vec<_>>();
    let mut recurrent_state = vec![None; block_count];
    for block in forced_blocks {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        recurrent_state[block] = Some(state_blocks.len());
        state_blocks.push(block);
    }

    let mut for_in_binders = HashSet::new();
    let mut for_in_iterator_occurrences = HashSet::new();
    let mut for_in_vars = HashSet::new();
    for fact in loop_facts {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if fact.kind != LoopKind::ForIn {
            continue;
        }
        let Some(span) = fact.iterator_span else {
            continue;
        };
        for occurrence in occurrences {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if occurrence.span != span {
                continue;
            }
            let block = occurrence.block.index().index();
            for_in_binders.insert((block, occurrence.var));
            for_in_iterator_occurrences.insert((block, occurrence.var, occurrence.span));
            for_in_vars.insert(occurrence.var);
        }
    }

    let mut outgoing = vec![Vec::new(); state_count];
    for edge in cfg.graph.edge_indices() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let Some((from, to)) = cfg.graph.edge_endpoints(edge) else {
            continue;
        };
        let Some(kind) = cfg.graph.edge_weight(edge) else {
            continue;
        };
        let source = from.index();
        let destination = to.index();
        let target = if *kind == EdgeKind::LoopBack {
            recurrent_state[destination].unwrap_or(destination)
        } else {
            destination
        };
        let first_entry_exit = recurrent_state[source].is_some() && *kind == EdgeKind::LoopExit;
        if !first_entry_exit {
            outgoing[source].push((target, kind.clone()));
        }
        if let Some(recurrent) = recurrent_state[source] {
            outgoing[recurrent].push((target, kind.clone()));
        }
    }

    let mut incoming = vec![Vec::new(); state_count];
    for (source, edges) in outgoing.iter().enumerate() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        for (target, kind) in edges {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            incoming[*target].push((source, kind.clone()));
        }
    }

    let entry = cfg.entry.index().index();
    let exit = cfg.exit.index().index();
    let mut reachable = vec![false; state_count];
    if entry < state_count {
        let mut pending = vec![entry];
        reachable[entry] = true;
        while let Some(state) = pending.pop() {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            for (target, _) in &outgoing[state] {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if !reachable[*target] {
                    reachable[*target] = true;
                    pending.push(*target);
                }
            }
        }
    }

    Ok(MustFlowGraph {
        state_blocks,
        outgoing,
        incoming,
        reachable,
        entry,
        exit,
        for_in_binders,
        for_in_iterator_occurrences,
        for_in_vars,
    })
}

#[allow(clippy::too_many_arguments)]
fn definite_assignment_facts(
    cfg: &Cfg,
    graph: &MustFlowGraph,
    var: usize,
    occurrences: &[&VarOccurrence],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<DefiniteAssignmentFacts, String> {
    let block_count = cfg.graph.node_count();
    let state_count = graph.state_blocks.len();
    budget.require_owned_bytes(
        block_count
            .saturating_mul(std::mem::size_of::<Vec<&VarOccurrence>>())
            .saturating_add(state_count.saturating_mul(std::mem::size_of::<bool>() * 2))
            .saturating_add(
                occurrences
                    .len()
                    .saturating_mul(std::mem::size_of::<&VarOccurrence>()),
            ),
        cancel,
    )?;
    let mut by_block = vec![Vec::new(); block_count];
    for &occurrence in occurrences {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if occurrence.var == var {
            let block = occurrence.block.index().index();
            if block < by_block.len() {
                by_block[block].push(occurrence);
            }
        }
    }
    for events in &mut by_block {
        events.sort_by_key(|occurrence| occurrence.order);
    }

    let mut input = vec![true; state_count];
    let mut output = vec![true; state_count];
    loop {
        let mut changed = false;
        for state in 0..state_count {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if !graph.reachable[state] {
                continue;
            }
            let next_input = if state == graph.entry {
                false
            } else {
                let mut has_predecessor = false;
                let mut defined = true;
                for (source, kind) in &graph.incoming[state] {
                    check_navigation_cancel(cancel)?;
                    budget.require_work(1, cancel)?;
                    if !graph.reachable[*source] {
                        continue;
                    }
                    has_predecessor = true;
                    defined &= must_edge_state(graph, *source, kind, var, &input, &output);
                }
                if !has_predecessor {
                    continue;
                }
                defined
            };
            let block = graph.state_blocks[state];
            let mut next_output = next_input;
            for occurrence in &by_block[block] {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if graph
                    .for_in_iterator_occurrences
                    .contains(&(block, var, occurrence.span))
                {
                    continue;
                }
                if access_writes(occurrence.access) {
                    next_output = true;
                }
            }
            if input[state] != next_input || output[state] != next_output {
                input[state] = next_input;
                output[state] = next_output;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut facts = DefiniteAssignmentFacts::default();
    for state in 0..state_count {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if !graph.reachable[state] {
            continue;
        }
        let block = graph.state_blocks[state];
        let mut defined = input[state];
        for occurrence in &by_block[block] {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if graph
                .for_in_iterator_occurrences
                .contains(&(block, var, occurrence.span))
            {
                continue;
            }
            if access_reads(occurrence.access)
                && !defined
                && facts
                    .first_uninitialized_read
                    .is_none_or(|first| occurrence.span.start < first.start)
            {
                facts.first_uninitialized_read = Some(occurrence.span);
            }
            if access_writes(occurrence.access) {
                defined = true;
            }
        }
    }

    for state in 0..state_count {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if !graph.reachable[state] {
            continue;
        }
        for (target, kind) in &graph.outgoing[state] {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if *target != graph.exit || *kind == EdgeKind::ExceptionThrow {
                continue;
            }
            if !must_edge_state(graph, state, kind, var, &input, &output) {
                facts.normal_exit_uninitialized = true;
            }
        }
    }
    Ok(facts)
}

fn must_edge_state(
    graph: &MustFlowGraph,
    source: usize,
    kind: &EdgeKind,
    var: usize,
    input: &[bool],
    output: &[bool],
) -> bool {
    let mut defined = if *kind == EdgeKind::ExceptionThrow {
        input[source]
    } else {
        output[source]
    };
    if *kind == EdgeKind::ConditionalTrue
        && graph
            .for_in_binders
            .contains(&(graph.state_blocks[source], var))
    {
        defined = true;
    }
    defined
}

fn final_end_span(
    flow: &RoutineFlow<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<SourceSpan>, String> {
    let Some(body) = flow.node.child_by_field_name("body") else {
        return Ok(None);
    };
    let nodes = collect_nodes(body, cancel, budget)?;
    Ok(nodes
        .into_iter()
        .filter(|node| node.kind() == "kEnd")
        .max_by_key(|node| node.start_byte())
        .map(|node| SourceSpan {
            start: node.start_byte(),
            end: node.end_byte(),
        }))
}

#[allow(clippy::too_many_arguments)]
fn collect_loop_facts<'tree>(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    flow: &RoutineFlow<'tree>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<LoopFact<'tree>>, String> {
    let Some(body) = flow.node.child_by_field_name("body") else {
        return Ok(Vec::new());
    };
    let mut facts = Vec::new();
    let mut pending = vec![body];
    while let Some(node) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if node.id() != body.id() && matches!(node.kind(), "defProc" | "lambda") {
            continue;
        }
        let kind = match node.kind() {
            "for" => Some(LoopKind::For),
            "while" => Some(LoopKind::While),
            "foreach" => Some(LoopKind::ForIn),
            _ => None,
        };
        if let Some(kind) = kind {
            let Some(loop_body) = node.child_by_field_name("body") else {
                continue;
            };
            let loop_span = SourceSpan {
                start: node.start_byte(),
                end: node.end_byte(),
            };
            if is_inactive(document, loop_span, cancel, budget)? {
                continue;
            }
            let (zero_iterations, iterator) = match kind {
                LoopKind::For => {
                    let start = node
                        .child_by_field_name("start")
                        .and_then(|assignment| assignment.child_by_field_name("rhs"));
                    let end = node.child_by_field_name("end");
                    let zero = match (start, end) {
                        (Some(start), Some(end)) => {
                            let (start, end) = (
                                constant_value(index, uri, document, start, cancel, budget)?,
                                constant_value(index, uri, document, end, cancel, budget)?,
                            );
                            match (start, end) {
                                (
                                    Some(ConstantValue::Integer(start)),
                                    Some(ConstantValue::Integer(end)),
                                ) => Some(if for_is_descending(node) {
                                    start < end
                                } else {
                                    start > end
                                }),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    (
                        zero,
                        node.child_by_field_name("start")
                            .and_then(|assignment| assignment.child_by_field_name("lhs")),
                    )
                }
                LoopKind::While => {
                    let zero = node
                        .child_by_field_name("condition")
                        .map(|condition| {
                            constant_value(index, uri, document, condition, cancel, budget)
                        })
                        .transpose()?
                        .flatten()
                        .and_then(|value| match value {
                            ConstantValue::Boolean(false) => Some(true),
                            ConstantValue::Boolean(true) => Some(false),
                            ConstantValue::Integer(_) => None,
                        });
                    (zero, None)
                }
                LoopKind::ForIn => (
                    None,
                    node.child_by_field_name("iterator")
                        .filter(|iterator| iterator.kind() == "identifier"),
                ),
            };
            let iterator_span = iterator.map(|iterator| SourceSpan {
                start: iterator.start_byte(),
                end: iterator.end_byte(),
            });
            let (body_has_break, body_has_continue) =
                if matches!(kind, LoopKind::For | LoopKind::ForIn) {
                    (
                        loop_body_has_control_transfer(
                            loop_body,
                            &document.source,
                            "break",
                            cancel,
                            budget,
                        )?,
                        loop_body_has_control_transfer(
                            loop_body,
                            &document.source,
                            "continue",
                            cancel,
                            budget,
                        )?,
                    )
                } else {
                    (false, false)
                };
            budget.require_owned_bytes(std::mem::size_of::<LoopFact<'tree>>(), cancel)?;
            facts.push(LoopFact {
                node,
                body: loop_body,
                kind,
                zero_iterations,
                iterator,
                iterator_span,
                body_has_break,
                body_has_continue,
            });
        }
        let mut cursor = node.walk();
        let mut children = Vec::new();
        for child in node.children(&mut cursor) {
            check_navigation_cancel(cancel)?;
            budget.require_owned_bytes(std::mem::size_of::<Node<'tree>>(), cancel)?;
            children.push(child);
        }
        pending.extend(children.into_iter().rev());
    }
    Ok(facts)
}

fn for_is_descending(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| child.kind() == "kDownto")
}

fn loop_body_has_control_transfer(
    body: Node<'_>,
    source: &str,
    transfer: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let mut pending = vec![body];
    while let Some(node) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if node.id() != body.id() && matches!(node.kind(), "for" | "foreach" | "while" | "repeat") {
            continue;
        }
        if node.kind() == "identifier" && node_text(node, source).eq_ignore_ascii_case(transfer) {
            return Ok(true);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            check_navigation_cancel(cancel)?;
            budget.require_owned_bytes(std::mem::size_of::<Node<'_>>(), cancel)?;
            pending.push(child);
        }
    }
    Ok(false)
}

fn occurrence_in_deleted_loop(occurrence: &VarOccurrence, facts: &[LoopFact<'_>]) -> bool {
    facts.iter().any(|fact| {
        fact.zero_iterations == Some(true)
            && (fact.body.start_byte() <= occurrence.span.start
                && occurrence.span.end <= fact.body.end_byte()
                || fact.iterator_span == Some(occurrence.span))
    })
}

#[allow(clippy::too_many_arguments)]
fn append_loop_diagnostics(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    flow: &RoutineFlow<'_>,
    facts: &[LoopFact<'_>],
    occurrences: &[&VarOccurrence],
    diagnostics: &mut Vec<SemanticDiagnostic>,
    capacity: usize,
    diagnostic_cap_reached: &mut bool,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<(), String> {
    for &fact in facts {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if fact.zero_iterations != Some(true) {
            continue;
        }
        let Some(span) = h2135_span(document, fact) else {
            continue;
        };
        if diagnostics.len() >= capacity {
            *diagnostic_cap_reached = true;
            return Ok(());
        }
        diagnostics.push(SemanticDiagnostic {
            kind: super::super::SemanticDiagnosticKind::Dcc(DccMessage::H2135),
            span,
            message: DccMessage::H2135.format(&[]),
        });
    }

    let compiler_version = document.conditional_context.compiler_version;
    let mut reported = HashSet::new();
    for &fact in facts {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if !w1037_supported(fact.kind, compiler_version) || fact.body_has_break {
            continue;
        }
        let Some(iterator) = fact.iterator else {
            continue;
        };
        let Some(variable) =
            resolve_local_loop_variable(index, uri, document, flow, iterator, cancel, budget)?
        else {
            continue;
        };
        let first_access = occurrences
            .iter()
            .filter(|occurrence| {
                occurrence.var == variable && occurrence.span.start > fact.node.end_byte()
            })
            .min_by_key(|occurrence| (occurrence.span.start, occurrence.span.end));
        let Some(first_access) = first_access else {
            continue;
        };
        if !access_reads(first_access.access) || reported.contains(&first_access.span) {
            continue;
        }
        reported.insert(first_access.span);
        let name = &flow.locals[variable].name;
        // Delphi 37.0 reports this same W1037 twice when the loop body
        // contains Continue; preserve that recorded compiler behavior.
        let warning_count = if fact.body_has_continue
            && matches!(
                compiler_version.map(|version| (version.major, version.minor)),
                Some((37, 0))
            ) {
            2
        } else {
            1
        };
        for _ in 0..warning_count {
            if diagnostics.len() >= capacity {
                *diagnostic_cap_reached = true;
                return Ok(());
            }
            budget.require_owned_bytes(std::mem::size_of::<SourceSpan>(), cancel)?;
            diagnostics.push(SemanticDiagnostic {
                kind: super::super::SemanticDiagnosticKind::Dcc(DccMessage::W1037),
                span: first_access.span,
                message: DccMessage::W1037.format(&[name]),
            });
        }
    }
    Ok(())
}

fn w1037_supported(
    kind: LoopKind,
    compiler_version: Option<pascal_project::CompilerVersion>,
) -> bool {
    matches!(
        (
            kind,
            compiler_version.map(|version| (version.major, version.minor))
        ),
        (LoopKind::For, Some((21, 0) | (24, 0) | (37, 0)))
            | (LoopKind::ForIn, Some((21, 0) | (24, 0)))
    )
}

fn h2135_span(document: &Document, fact: LoopFact<'_>) -> Option<SourceSpan> {
    let version = document.conditional_context.compiler_version?;
    let version = (version.major, version.minor);
    let start = match (fact.kind, version) {
        (LoopKind::For, (21, 0) | (24, 0) | (37, 0)) => fact.node.start_byte(),
        (LoopKind::While, (21, 0) | (24, 0)) => fact.body.start_byte(),
        (LoopKind::While, (37, 0)) => fact.node.start_byte(),
        _ => return None,
    };
    Some(SourceSpan { start, end: start })
}

#[allow(clippy::too_many_arguments)]
fn resolve_local_loop_variable(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    flow: &RoutineFlow<'_>,
    identifier: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<usize>, String> {
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
    if candidates.len() != 1
        || state.receiver_resolution_uncertain()
        || state.member_lookup_incomplete
        || state.ambiguous
        || state.inaccessible_candidate
    {
        return Ok(None);
    }
    let candidate = &candidates[0];
    if &candidate.uri != uri {
        return Ok(None);
    }
    let Some(symbol) = document.symbols.get(candidate.index) else {
        return Ok(None);
    };
    if !matches!(symbol.kind, SymbolKind::Variable | SymbolKind::Parameter) {
        return Ok(None);
    }
    let selection_span = SourceSpan {
        start: symbol.span.start,
        end: symbol.span.end,
    };
    Ok(flow
        .locals
        .iter()
        .position(|local| local.decl_span == selection_span))
}

fn constant_value(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    node: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<ConstantValue>, String> {
    constant_value_with_visited(
        index,
        uri,
        document,
        node,
        &mut HashSet::new(),
        cancel,
        budget,
    )
}

#[allow(clippy::too_many_arguments)]
fn constant_value_with_visited(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    node: Node<'_>,
    visited: &mut HashSet<Candidate>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<ConstantValue>, String> {
    check_navigation_cancel(cancel)?;
    budget.require_work(1, cancel)?;
    match node.kind() {
        "literalNumber" => {
            Ok(parse_radix_integer(&node_text(node, &document.source)).map(ConstantValue::Integer))
        }
        "literalChar" => Ok(node_text(node, &document.source)
            .strip_prefix('#')
            .and_then(parse_radix_integer)
            .map(ConstantValue::Integer)),
        "literalString" => Ok(
            parse_single_char_literal(&node_text(node, &document.source))
                .map(|value| ConstantValue::Integer(value as i128)),
        ),
        "kTrue" => Ok(Some(ConstantValue::Boolean(true))),
        "kFalse" => Ok(Some(ConstantValue::Boolean(false))),
        "exprParens" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| !matches!(child.kind(), "kLParen" | "kRParen"))
                .map(|child| {
                    constant_value_with_visited(
                        index, uri, document, child, visited, cancel, budget,
                    )
                })
                .transpose()
                .map(Option::flatten)
        }
        "exprUnary" => {
            let Some(operand) = node.child_by_field_name("operand") else {
                return Ok(None);
            };
            let Some(ConstantValue::Integer(value)) = constant_value_with_visited(
                index, uri, document, operand, visited, cancel, budget,
            )?
            else {
                return Ok(None);
            };
            let operator = node
                .child_by_field_name("operator")
                .map(|operator| node_text(operator, &document.source));
            Ok(match operator.as_deref() {
                Some("+") => Some(ConstantValue::Integer(value)),
                Some("-") => value.checked_neg().map(ConstantValue::Integer),
                _ => None,
            })
        }
        "exprBinary" => {
            let (Some(lhs), Some(rhs), Some(operator)) = (
                node.child_by_field_name("lhs"),
                node.child_by_field_name("rhs"),
                node.child_by_field_name("operator"),
            ) else {
                return Ok(None);
            };
            let (Some(ConstantValue::Integer(lhs)), Some(ConstantValue::Integer(rhs))) = (
                constant_value_with_visited(index, uri, document, lhs, visited, cancel, budget)?,
                constant_value_with_visited(index, uri, document, rhs, visited, cancel, budget)?,
            ) else {
                return Ok(None);
            };
            let operator = node_text(operator, &document.source);
            Ok(match operator.to_ascii_lowercase().as_str() {
                "+" => lhs.checked_add(rhs),
                "-" => lhs.checked_sub(rhs),
                "*" => lhs.checked_mul(rhs),
                "div" if rhs != 0 => lhs.checked_div(rhs),
                "mod" if rhs != 0 => lhs.checked_rem(rhs),
                _ => None,
            }
            .map(ConstantValue::Integer))
        }
        "identifier" => {
            let mut state = ResolutionState::new();
            let candidates = index.resolve_candidates_at_with_state_and_budget(
                uri,
                document,
                node.start_byte(),
                node,
                &mut state,
                0,
                cancel,
                budget,
            )?;
            if candidates.len() != 1
                || state.receiver_resolution_uncertain()
                || state.member_lookup_incomplete
                || state.ambiguous
                || state.inaccessible_candidate
            {
                return Ok(None);
            }
            budget.require_owned_bytes(
                std::mem::size_of::<Candidate>().saturating_add(candidates[0].uri.as_str().len()),
                cancel,
            )?;
            let candidate = candidates[0].clone();
            if !visited.insert(candidate.clone()) {
                return Ok(None);
            }
            let result = if let Some(constant_document) = index.documents.get(&candidate.uri) {
                let Some(symbol) = constant_document.symbols.get(candidate.index) else {
                    visited.remove(&candidate);
                    return Ok(None);
                };
                if symbol.kind != SymbolKind::Constant {
                    None
                } else {
                    let declaration_span = symbol.declaration_span;
                    let Some(declaration) = node_at_range(
                        constant_document.tree.root_node(),
                        declaration_span.start..declaration_span.end,
                        cancel,
                        budget,
                    )?
                    else {
                        visited.remove(&candidate);
                        return Ok(None);
                    };
                    if declaration.kind() != "declConst"
                        || declaration.child_by_field_name("type").is_some()
                    {
                        None
                    } else {
                        declaration
                            .child_by_field_name("defaultValue")
                            .and_then(constant_initializer)
                            .filter(|initializer| is_literal_initializer(*initializer))
                            .map(|initializer| {
                                constant_value_with_visited(
                                    index,
                                    &candidate.uri,
                                    constant_document,
                                    initializer,
                                    visited,
                                    cancel,
                                    budget,
                                )
                            })
                            .transpose()?
                            .flatten()
                    }
                }
            } else {
                None
            };
            visited.remove(&candidate);
            Ok(result)
        }
        _ => Ok(None),
    }
}

fn constant_initializer(default_value: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = default_value.walk();
    default_value
        .named_children(&mut cursor)
        .find(|child| child.kind() != "kEq")
}

fn is_literal_initializer(node: Node<'_>) -> bool {
    match node.kind() {
        "literalNumber" | "literalString" | "literalChar" | "kTrue" | "kFalse" => true,
        "exprParens" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| !matches!(child.kind(), "kLParen" | "kRParen"))
                .is_some_and(is_literal_initializer)
        }
        _ => false,
    }
}

fn node_at_range<'tree>(
    root: Node<'tree>,
    range: std::ops::Range<usize>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Node<'tree>>, String> {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if node.start_byte() == range.start && node.end_byte() == range.end {
            return Ok(Some(node));
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            check_navigation_cancel(cancel)?;
            budget.require_owned_bytes(std::mem::size_of::<Node<'tree>>(), cancel)?;
            pending.push(child);
        }
    }
    Ok(None)
}

fn parse_single_char_literal(text: &str) -> Option<char> {
    let contents = text.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut chars = contents.chars().peekable();
    let mut value = None;
    while let Some(character) = chars.next() {
        if character == '\'' && chars.next_if_eq(&'\'').is_none() {
            return None;
        }
        if value.replace(character).is_some() {
            return None;
        }
    }
    value
}

// DCC withholds H2077 for an erroneous routine; nested-routine errors are
// within the enclosing routine's range and suppress its H2077 findings too.
fn routine_has_error(
    flow: &RoutineFlow<'_>,
    document: &Document,
    diagnostics: &[SemanticDiagnostic],
    warning_switches: &WarningSwitches,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    for diagnostic in diagnostics {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let promoted_warning = match diagnostic.kind {
            SemanticDiagnosticKind::Dcc(message)
                if warning_switches.error_capable(message.code()) =>
            {
                warning_switches.effective_switch(document, message.code(), diagnostic.span.start)
                    == Some(SwitchState::Error)
            }
            _ => false,
        };
        if (diagnostic.kind.severity() == DccSeverity::Error || promoted_warning)
            && flow.node.start_byte() <= diagnostic.span.start
            && diagnostic.span.end <= flow.node.end_byte()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_inactive(
    document: &Document,
    span: SourceSpan,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    for inactive in &document.conditionals.inactive_spans {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if inactive.start <= span.start && span.end <= inactive.end {
            return Ok(true);
        }
    }
    Ok(false)
}

fn inline_argument_reads(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    routine_nodes: &[Node<'_>],
    occurrences: &[&VarOccurrence],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<HashSet<SourceSpan>, String> {
    let reads = occurrences
        .iter()
        .filter(|occurrence| occurrence.access == Access::Read)
        .map(|occurrence| (occurrence.span, occurrence.var))
        .collect::<HashMap<_, _>>();
    if reads.is_empty() {
        return Ok(HashSet::new());
    }

    let mut inline_reads = HashSet::new();
    for &call in routine_nodes {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if call.kind() != "exprCall" || !is_inline_call(index, uri, document, call, cancel, budget)?
        {
            continue;
        }
        let Some(assignment) = call.parent().filter(|parent| parent.kind() == "assignment") else {
            continue;
        };
        let Some(rhs) = assignment.child_by_field_name("rhs") else {
            continue;
        };
        if rhs.start_byte() != call.start_byte() || rhs.end_byte() != call.end_byte() {
            continue;
        }
        let Some(lhs) = assignment
            .child_by_field_name("lhs")
            .filter(|lhs| lhs.kind() == "identifier")
        else {
            continue;
        };
        let lhs_span = SourceSpan {
            start: lhs.start_byte(),
            end: lhs.end_byte(),
        };
        let Some(lhs_var) = occurrences
            .iter()
            .find(|occurrence| occurrence.span == lhs_span && occurrence.access == Access::Write)
            .map(|occurrence| occurrence.var)
        else {
            continue;
        };
        let Some(arguments) = call.child_by_field_name("args") else {
            continue;
        };
        let mut argument_cursor = arguments.walk();
        for argument in arguments.named_children(&mut argument_cursor) {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let expression = if argument.kind() == "namedArgument" {
                argument.child_by_field_name("value").unwrap_or(argument)
            } else {
                argument
            };
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if expression.kind() == "identifier" {
                let span = SourceSpan {
                    start: expression.start_byte(),
                    end: expression.end_byte(),
                };
                if reads.get(&span) == Some(&lhs_var) {
                    inline_reads.insert(span);
                }
            }
        }
    }
    Ok(inline_reads)
}

fn is_inline_call(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    call: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let Some(entity) = call.child_by_field_name("entity") else {
        return Ok(false);
    };
    let mut state = ResolutionState::new();
    let candidates = index.resolve_candidates_at_with_state_and_budget(
        uri,
        document,
        entity.start_byte(),
        entity,
        &mut state,
        0,
        cancel,
        budget,
    )?;
    if candidates.is_empty()
        || state.receiver_resolution_uncertain()
        || state.member_lookup_incomplete
        || state.ambiguous
        || state.inaccessible_candidate
    {
        return Ok(false);
    }
    Ok(candidates.iter().all(|candidate| {
        index
            .documents
            .get(&candidate.uri)
            .and_then(|document| document.symbols.get(candidate.index))
            .is_some_and(|symbol| symbol.routine_directives.inline)
    }))
}

fn definitely_nonempty_for_exits(
    flow: &RoutineFlow<'_>,
    facts: &[LoopFact<'_>],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<HashSet<usize>, String> {
    let Some(cfg) = flow.cfg.as_ref() else {
        return Ok(HashSet::new());
    };
    let mut blocks = HashSet::new();
    for &fact in facts {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if fact.kind != LoopKind::For || fact.zero_iterations != Some(false) {
            continue;
        }
        for block in cfg.graph.node_indices() {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if cfg.graph[block].stmts.iter().any(|statement| {
                statement.node_kind == "for" && statement.byte_range.start == fact.node.start_byte()
            }) {
                blocks.insert(block.index());
                break;
            }
        }
    }
    Ok(blocks)
}

fn parse_radix_integer(text: &str) -> Option<i128> {
    let normalized = text.trim().replace('_', "");
    let (negative, digits) = match normalized.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, normalized.strip_prefix('+').unwrap_or(&normalized)),
    };
    let (radix, digits) = if let Some(hex) = digits.strip_prefix('$') {
        (16, hex)
    } else if let Some(binary) = digits.strip_prefix('%') {
        (2, binary)
    } else {
        (10, digits)
    };
    let value = i128::from_str_radix(digits, radix).ok()?;
    if negative {
        value.checked_neg()
    } else {
        Some(value)
    }
}

#[allow(clippy::too_many_arguments)]
fn dead_write_spans(
    cfg: &Cfg,
    local_count: usize,
    variables: &[usize],
    occurrences: &[&VarOccurrence],
    inline_reads: &HashSet<SourceSpan>,
    forced_for_exits: &HashSet<usize>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<(usize, SourceSpan)>, String> {
    let mut block_count = 0usize;
    for block in cfg.graph.node_indices() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        block_count = block_count.max(block.index().saturating_add(1));
    }
    if variables.is_empty() || block_count == 0 {
        return Ok(Vec::new());
    }
    let mut forced_block_count = 0usize;
    for &block in forced_for_exits {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if block < block_count {
            forced_block_count = forced_block_count.saturating_add(1);
        }
    }
    budget.require_owned_bytes(
        forced_block_count.saturating_mul(std::mem::size_of::<usize>()),
        cancel,
    )?;
    let mut forced_blocks = Vec::with_capacity(forced_block_count);
    for &block in forced_for_exits {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if block < block_count {
            forced_blocks.push(block);
        }
    }
    forced_blocks.sort_unstable();
    let analysis_block_count = block_count.saturating_add(forced_blocks.len());
    let work_len = analysis_block_count.saturating_mul(variables.len());
    budget.require_owned_bytes(
        work_len
            .saturating_mul(std::mem::size_of::<u8>() + std::mem::size_of::<bool>())
            .saturating_add(
                block_count.saturating_mul(std::mem::size_of::<Vec<(usize, &VarOccurrence)>>()),
            )
            .saturating_add(
                analysis_block_count.saturating_mul(std::mem::size_of::<Vec<(usize, EdgeKind)>>()),
            )
            .saturating_add(
                occurrences
                    .len()
                    .saturating_mul(std::mem::size_of::<(usize, &VarOccurrence)>()),
            )
            .saturating_add(local_count.saturating_mul(std::mem::size_of::<Option<usize>>()))
            .saturating_add(
                variables
                    .len()
                    .saturating_mul(std::mem::size_of::<bool>() * 2),
            )
            .saturating_add(
                occurrences
                    .len()
                    .saturating_mul(std::mem::size_of::<((usize, SourceSpan), bool)>()),
            ),
        cancel,
    )?;
    budget.require_owned_bytes(
        cfg.graph
            .edge_count()
            .saturating_mul(2)
            .saturating_mul(std::mem::size_of::<(usize, EdgeKind)>())
            .saturating_add(block_count.saturating_mul(std::mem::size_of::<Option<usize>>()))
            .saturating_add(analysis_block_count.saturating_mul(std::mem::size_of::<usize>())),
        cancel,
    )?;
    let mut recurrent_state = vec![None; block_count];
    let mut state_blocks = (0..block_count).collect::<Vec<_>>();
    for (offset, &block) in forced_blocks.iter().enumerate() {
        recurrent_state[block] = Some(block_count + offset);
        state_blocks.push(block);
    }
    let mut column_for_local = vec![None; local_count];
    for (column, &local) in variables.iter().enumerate() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        column_for_local[local] = Some(column);
    }
    let mut by_block = vec![Vec::new(); block_count];
    for &occurrence in occurrences {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let Some(column) = column_for_local[occurrence.var] else {
            continue;
        };
        let block = occurrence.block.index().index();
        if block < by_block.len() {
            by_block[block].push((column, occurrence));
        }
    }

    let mut outgoing = vec![Vec::<(usize, EdgeKind)>::new(); analysis_block_count];
    for edge in cfg.graph.edge_indices() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let Some((from, to)) = cfg.graph.edge_endpoints(edge) else {
            continue;
        };
        let Some(kind) = cfg.graph.edge_weight(edge) else {
            continue;
        };
        let source = from.index();
        let destination = to.index();
        let target = if *kind == EdgeKind::LoopBack {
            recurrent_state[destination].unwrap_or(destination)
        } else {
            destination
        };
        let first_entry_exit = recurrent_state[source].is_some() && *kind == EdgeKind::LoopExit;
        if !first_entry_exit {
            outgoing[source].push((target, kind.clone()));
        }
        if let Some(recurrent) = recurrent_state[source] {
            outgoing[recurrent].push((target, kind.clone()));
        }
    }

    for events in &mut by_block {
        events.sort_by_key(|(_, occurrence)| occurrence.order);
    }
    let mut facts = vec![0u8; work_len];
    for (state, &block) in state_blocks.iter().enumerate() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let mut defined = vec![false; variables.len()];
        for &(column, occurrence) in &by_block[block] {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let inline_read =
                occurrence.access == Access::Read && inline_reads.contains(&occurrence.span);
            let reads = access_reads(occurrence.access) && !inline_read;
            let writes = access_writes(occurrence.access);
            let fact_index = state * variables.len() + column;
            if reads && !defined[column] {
                facts[fact_index] |= USE;
            }
            if writes {
                facts[fact_index] |= DEF;
                defined[column] = true;
            }
        }
    }

    let mut live_in = vec![false; work_len];
    let mut live_out = vec![false; variables.len()];
    loop {
        let mut changed = false;
        for state in (0..analysis_block_count).rev() {
            check_navigation_cancel(cancel)?;
            live_out.fill(false);
            for (target, _) in &outgoing[state] {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                for (column, live) in live_out.iter_mut().enumerate() {
                    budget.require_work(1, cancel)?;
                    if target * variables.len() + column < live_in.len() {
                        *live |= live_in[target * variables.len() + column];
                    }
                }
            }
            for (column, &live_after) in live_out.iter().enumerate() {
                budget.require_work(1, cancel)?;
                let fact_index = state * variables.len() + column;
                let block_facts = facts[fact_index];
                let next = block_facts & USE != 0 || (live_after && block_facts & DEF == 0);
                if next && !live_in[fact_index] {
                    live_in[fact_index] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    let mut dead_by_span = HashMap::<(usize, SourceSpan), bool>::new();
    let mut live = vec![false; variables.len()];
    for (state, &block_index) in state_blocks.iter().enumerate() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        live.fill(false);
        for (target, _) in &outgoing[state] {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            for (column, value) in live.iter_mut().enumerate() {
                budget.require_work(1, cancel)?;
                if target * variables.len() + column < live_in.len() {
                    *value |= live_in[target * variables.len() + column];
                }
            }
        }
        for &(column, occurrence) in by_block[block_index].iter().rev() {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let inline_read =
                occurrence.access == Access::Read && inline_reads.contains(&occurrence.span);
            match occurrence.access {
                Access::Write => {
                    let unused = !live[column];
                    dead_by_span
                        .entry((occurrence.var, occurrence.span))
                        .and_modify(|all_copies_unused| *all_copies_unused &= unused)
                        .or_insert(unused);
                    live[column] = false;
                }
                access if inline_read && access == Access::Read => {}
                Access::Read | Access::AddressOf => live[column] = true,
                Access::ReadWrite | Access::UnknownCallEffect | Access::VarArg => {
                    live[column] = true
                }
                Access::OutArg => live[column] = false,
            }
        }
    }
    Ok(dead_by_span
        .into_iter()
        .filter_map(|((var, span), unused)| unused.then_some((var, span)))
        .collect())
}

fn access_reads(access: Access) -> bool {
    matches!(
        access,
        Access::Read
            | Access::ReadWrite
            | Access::UnknownCallEffect
            | Access::VarArg
            | Access::AddressOf
    )
}

fn access_writes(access: Access) -> bool {
    matches!(
        access,
        Access::Write
            | Access::ReadWrite
            | Access::UnknownCallEffect
            | Access::VarArg
            | Access::OutArg
    )
}
