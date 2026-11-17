use super::super::{
    AssistanceBudget, DccMessage, DccSeverity, Document, NavigationIndex, ResolutionState,
    SemanticDiagnostic, SourceSpan, check_navigation_cancel, node_text,
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

#[allow(clippy::too_many_arguments)]
pub(super) fn flow_diagnostics(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    flows: &[RoutineFlow<'_>],
    prior_diagnostics: &[SemanticDiagnostic],
    capacity: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<SemanticDiagnostic>, String> {
    let mut diagnostics = Vec::new();
    for flow in flows {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if flow.excluded.is_some() || flow.cfg.is_none() {
            continue;
        }
        let mut active_occurrences = Vec::new();
        for occurrence in &flow.occurrences {
            if !is_inactive(document, occurrence.span, cancel, budget)? {
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

        if routine_has_error(flow, prior_diagnostics, cancel, budget)? {
            continue;
        }

        let eligible_vars = locals
            .iter()
            .enumerate()
            .filter_map(|(var, local)| {
                (local.kind == LocalKind::Local
                    && local.ty == VarType::Scalar
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
        let forced_for_exits =
            definitely_nonempty_for_exits(flow, document, &routine_nodes, cancel, budget)?;
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

// DCC withholds H2077 for an erroneous routine; nested-routine errors are
// within the enclosing routine's range and suppress its H2077 findings too.
fn routine_has_error(
    flow: &RoutineFlow<'_>,
    diagnostics: &[SemanticDiagnostic],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    for diagnostic in diagnostics {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if diagnostic.kind.severity() == DccSeverity::Error
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
        .map(|occurrence| occurrence.span)
        .collect::<HashSet<_>>();
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
            let mut pending = vec![expression];
            while let Some(node) = pending.pop() {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if node.kind() == "identifier" {
                    let span = SourceSpan {
                        start: node.start_byte(),
                        end: node.end_byte(),
                    };
                    if reads.contains(&span) {
                        inline_reads.insert(span);
                    }
                }
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor));
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
    document: &Document,
    routine_nodes: &[Node<'_>],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<HashSet<usize>, String> {
    let Some(cfg) = flow.cfg.as_ref() else {
        return Ok(HashSet::new());
    };
    let mut blocks = HashSet::new();
    for &node in routine_nodes {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if node.kind() != "for"
            || is_inactive(
                document,
                SourceSpan {
                    start: node.start_byte(),
                    end: node.end_byte(),
                },
                cancel,
                budget,
            )?
        {
            continue;
        }
        let Some(start) = node.child_by_field_name("start") else {
            continue;
        };
        let Some(start_value) = start.child_by_field_name("rhs") else {
            continue;
        };
        let Some(end_value) = node.child_by_field_name("end") else {
            continue;
        };
        let (Some(start), Some(end)) = (
            constant_integer(start_value, &document.source, cancel, budget)?,
            constant_integer(end_value, &document.source, cancel, budget)?,
        ) else {
            continue;
        };
        let mut cursor = node.walk();
        let descending = node
            .children(&mut cursor)
            .any(|child| child.kind() == "kDownto");
        if (descending && start < end) || (!descending && start > end) {
            continue;
        }
        for block in cfg.graph.node_indices() {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if cfg.graph[block].stmts.iter().any(|statement| {
                statement.node_kind == "for" && statement.byte_range.start == node.start_byte()
            }) {
                blocks.insert(block.index());
                break;
            }
        }
    }
    Ok(blocks)
}

fn constant_integer(
    node: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<i128>, String> {
    check_navigation_cancel(cancel)?;
    budget.require_work(1, cancel)?;
    match node.kind() {
        "literalNumber" => Ok(parse_radix_integer(&node_text(node, source))),
        "exprParens" => node
            .named_child(0)
            .map(|child| constant_integer(child, source, cancel, budget))
            .unwrap_or(Ok(None)),
        "exprUnary" => {
            let Some(operand) = node.child_by_field_name("operand") else {
                return Ok(None);
            };
            let Some(value) = constant_integer(operand, source, cancel, budget)? else {
                return Ok(None);
            };
            let operator = node
                .child_by_field_name("operator")
                .map(|operator| node_text(operator, source));
            Ok(match operator.as_deref() {
                Some("+") => Some(value),
                Some("-") => value.checked_neg(),
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
            let (Some(lhs), Some(rhs)) = (
                constant_integer(lhs, source, cancel, budget)?,
                constant_integer(rhs, source, cancel, budget)?,
            ) else {
                return Ok(None);
            };
            let operator = node_text(operator, source);
            Ok(match operator.to_ascii_lowercase().as_str() {
                "+" => lhs.checked_add(rhs),
                "-" => lhs.checked_sub(rhs),
                "*" => lhs.checked_mul(rhs),
                "div" if rhs != 0 => lhs.checked_div(rhs),
                "mod" if rhs != 0 => lhs.checked_rem(rhs),
                _ => None,
            })
        }
        _ => Ok(None),
    }
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
                Access::ReadWrite | Access::VarArg => live[column] = true,
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
        Access::Read | Access::ReadWrite | Access::VarArg | Access::AddressOf
    )
}

fn access_writes(access: Access) -> bool {
    matches!(
        access,
        Access::Write | Access::ReadWrite | Access::VarArg | Access::OutArg
    )
}
