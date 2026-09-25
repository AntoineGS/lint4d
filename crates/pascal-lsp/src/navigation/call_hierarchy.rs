use super::{NavigationIndex, Origin, RoutineKind, Span, Symbol, SymbolKind};
use crate::text;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, Range, Url,
};
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::AtomicBool;
use tree_sitter::Node;

const MAX_NODE_VISITS: usize = 200_000;
const MAX_CALLS: usize = 4_096;
const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;

pub(super) fn prepare(
    index: &NavigationIndex,
    uri: &Url,
    position: lsp_types::Position,
    cancel: &AtomicBool,
) -> Result<Option<Vec<CallHierarchyItem>>, String> {
    check_cancel(cancel)?;
    let Some(document) = index.documents.get(uri) else {
        return Ok(None);
    };
    let Some(offset) = text::position_to_offset(&document.source, position) else {
        return Ok(None);
    };
    if document.conditionals.is_unknown_at(offset)
        || document.has_parser_recovery_near(Span {
            start: offset,
            end: offset,
        })
    {
        return Ok(None);
    }
    let mut candidates = document.symbols.iter().filter(|symbol| {
        symbol.kind == SymbolKind::Routine
            && symbol.selection_span.start <= offset
            && offset < symbol.selection_span.end
    });
    let Some(candidate) = candidates.next() else {
        return Ok(None);
    };
    if candidates.next().is_some() || document.has_parser_recovery_near(candidate.declaration_span)
    {
        return Ok(None);
    }
    let Some(key) = candidate.routine_key.as_ref() else {
        return Ok(None);
    };
    let mut definitions = index
        .documents
        .iter()
        .flat_map(|(target_uri, target_document)| {
            target_document.symbols.iter().filter_map(move |symbol| {
                (symbol.kind == SymbolKind::Routine
                    && symbol.origin == Origin::Definition
                    && target_document.unit_name == document.unit_name
                    && symbol.routine_key.as_ref() == Some(key))
                .then_some((target_uri, target_document, symbol))
            })
        });
    let Some((target_uri, target_document, symbol)) = definitions.next() else {
        return Ok(None);
    };
    if definitions.next().is_some() {
        return Ok(None);
    }
    let Some(item) = item_for_symbol(target_uri, target_document, symbol) else {
        return Ok(None);
    };
    Ok(Some(vec![item]))
}

pub(super) fn incoming(
    index: &NavigationIndex,
    item: &CallHierarchyItem,
    cancel: &AtomicBool,
) -> Result<Vec<CallHierarchyIncomingCall>, String> {
    let (target_uri, target_index) = validate_item(index, item)?;
    let target_document = index
        .documents
        .get(&target_uri)
        .ok_or_else(|| "hierarchy target source is unavailable".to_string())?;
    let target_symbol = &target_document.symbols[target_index];
    let mut grouped: BTreeMap<(String, usize, usize), (CallHierarchyItem, Vec<Range>)> =
        BTreeMap::new();
    let mut budget = ScanBudget::default();
    let mut binding_budget = super::AssistanceBudget::new(
        super::MAX_NAVIGATION_OVERLOAD_WORK,
        super::MAX_NAVIGATION_OVERLOAD_BYTES,
        "call hierarchy incoming binding",
    );
    let mut result_budget = ResultBudget::default();
    for (uri, document) in &index.documents {
        let root = document.tree.root_node();
        scan_calls(root, &mut budget, cancel, &mut |call| {
            let span = Span::from_node(call);
            if document.conditionals.is_unknown_at(span.start)
                || document.has_parser_recovery_near(span)
            {
                return Ok(());
            }
            let Some((caller_index, caller)) = containing_caller(document, span) else {
                return Ok(());
            };
            if document.has_parser_recovery_near(caller.declaration_span) {
                return Ok(());
            }
            let Some((callee_uri, callee)) =
                resolve_call(index, uri, document, call, cancel, &mut binding_budget)?
            else {
                return Ok(());
            };
            if has_dynamic_dispatch(callee) {
                return Ok(());
            }
            if callee_uri != target_uri
                || callee.selection_span != target_symbol.selection_span
                || callee.routine_signature != target_symbol.routine_signature
            {
                return Ok(());
            }
            let entity = call
                .child_by_field_name("entity")
                .ok_or_else(|| "call has no entity".to_string())?;
            let Some(range) = byte_range(&document.source, entity.start_byte(), entity.end_byte())
            else {
                return Ok(());
            };
            result_budget.charge(
                uri.as_str()
                    .len()
                    .saturating_add(3 * std::mem::size_of::<usize>()),
            )?;
            let key = (uri.to_string(), caller_index, caller.selection_span.start);
            if let Some((_, ranges)) = grouped.get_mut(&key) {
                result_budget.charge_range()?;
                ranges.push(range);
            } else {
                result_budget.charge_item(uri, caller)?;
                result_budget.charge_range()?;
                let caller_item = item_for_symbol(uri, document, caller)
                    .ok_or_else(|| "could not form caller identity".to_string())?;
                grouped.insert(key, (caller_item, vec![range]));
            }
            Ok(())
        })?;
    }
    grouped
        .into_values()
        .map(|(from, mut from_ranges)| {
            from_ranges.sort_by_key(|range| {
                (
                    range.start.line,
                    range.start.character,
                    range.end.line,
                    range.end.character,
                )
            });
            from_ranges.dedup();
            Ok(CallHierarchyIncomingCall { from, from_ranges })
        })
        .collect()
}

pub(super) fn outgoing(
    index: &NavigationIndex,
    item: &CallHierarchyItem,
    cancel: &AtomicBool,
) -> Result<Vec<CallHierarchyOutgoingCall>, String> {
    let (caller_uri, caller_index) = validate_item(index, item)?;
    let document = index
        .documents
        .get(&caller_uri)
        .ok_or_else(|| "hierarchy item source is unavailable".to_string())?;
    let caller = &document.symbols[caller_index];
    let mut grouped: BTreeMap<(String, usize, usize), (CallHierarchyItem, Vec<Range>)> =
        BTreeMap::new();
    let mut budget = ScanBudget::default();
    let mut binding_budget = super::AssistanceBudget::new(
        super::MAX_NAVIGATION_OVERLOAD_WORK,
        super::MAX_NAVIGATION_OVERLOAD_BYTES,
        "call hierarchy outgoing binding",
    );
    let mut result_budget = ResultBudget::default();
    scan_calls(
        document.tree.root_node(),
        &mut budget,
        cancel,
        &mut |call| {
            let span = Span::from_node(call);
            if !caller.declaration_span.contains(span)
                || document.conditionals.is_unknown_at(span.start)
                || document.has_parser_recovery_near(span)
                || document.has_parser_recovery_near(caller.declaration_span)
            {
                return Ok(());
            }
            let Some((callee_uri, callee)) = resolve_call(
                index,
                &caller_uri,
                document,
                call,
                cancel,
                &mut binding_budget,
            )?
            else {
                return Ok(());
            };
            if has_dynamic_dispatch(callee) {
                return Ok(());
            }
            let callee_document = index
                .documents
                .get(&callee_uri)
                .ok_or_else(|| "resolved call target source is unavailable".to_string())?;
            let entity = call
                .child_by_field_name("entity")
                .ok_or_else(|| "call has no entity".to_string())?;
            let Some(range) = byte_range(&document.source, entity.start_byte(), entity.end_byte())
            else {
                return Ok(());
            };
            result_budget.charge(
                callee_uri
                    .as_str()
                    .len()
                    .saturating_add(3 * std::mem::size_of::<usize>()),
            )?;
            let key = (
                callee_uri.to_string(),
                callee.selection_span.start,
                callee.selection_span.end,
            );
            if let Some((_, ranges)) = grouped.get_mut(&key) {
                result_budget.charge_range()?;
                ranges.push(range);
            } else {
                result_budget.charge_item(&callee_uri, callee)?;
                result_budget.charge_range()?;
                let callee_item = item_for_symbol(&callee_uri, callee_document, callee)
                    .ok_or_else(|| "could not form callee identity".to_string())?;
                grouped.insert(key, (callee_item, vec![range]));
            }
            Ok(())
        },
    )?;
    grouped
        .into_values()
        .map(|(to, mut from_ranges)| {
            from_ranges.sort_by_key(|range| {
                (
                    range.start.line,
                    range.start.character,
                    range.end.line,
                    range.end.character,
                )
            });
            from_ranges.dedup();
            Ok(CallHierarchyOutgoingCall { to, from_ranges })
        })
        .collect()
}

fn validate_item(
    index: &NavigationIndex,
    item: &CallHierarchyItem,
) -> Result<(Url, usize), String> {
    let data = item
        .data
        .as_ref()
        .ok_or_else(|| "call hierarchy item has no identity data".to_string())?;
    let document = index
        .documents
        .get(&item.uri)
        .ok_or_else(|| "call hierarchy item source is not selected".to_string())?;
    let mut found = None;
    for (i, symbol) in document
        .symbols
        .iter()
        .enumerate()
        .filter(|(_, symbol)| symbol.kind == SymbolKind::Routine)
    {
        let Some(candidate) = item_for_symbol(&item.uri, document, symbol) else {
            continue;
        };
        if candidate.name == item.name
            && candidate.kind == item.kind
            && candidate.range == item.range
            && candidate.selection_range == item.selection_range
            && candidate.data.as_ref() == Some(data)
        {
            if found.is_some() {
                return Err("call hierarchy item identity is ambiguous".to_string());
            }
            found = Some(i);
        }
    }
    found
        .map(|i| (item.uri.clone(), i))
        .ok_or_else(|| "stale or forged call hierarchy item".to_string())
}

fn item_for_symbol(
    uri: &Url,
    document: &super::ParsedDocument,
    symbol: &Symbol,
) -> Option<CallHierarchyItem> {
    let range = byte_range(
        &document.source,
        symbol.declaration_span.start,
        symbol.declaration_span.end,
    )?;
    let selection_range = byte_range(
        &document.source,
        symbol.selection_span.start,
        symbol.selection_span.end,
    )?;
    let kind = match symbol.routine_kind {
        RoutineKind::Constructor => lsp_types::SymbolKind::CONSTRUCTOR,
        RoutineKind::Operator => lsp_types::SymbolKind::OPERATOR,
        RoutineKind::Function if symbol.owner_type_name.is_none() => {
            lsp_types::SymbolKind::FUNCTION
        }
        _ if symbol.owner_type_name.is_some() => lsp_types::SymbolKind::METHOD,
        _ => lsp_types::SymbolKind::FUNCTION,
    };
    let source = document
        .source
        .get(symbol.declaration_span.start..symbol.declaration_span.end)?;
    let mut fingerprint = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut fingerprint);
    symbol.routine_signature.hash(&mut fingerprint);
    let source_fingerprint = fingerprint.finish();
    let identity = serde_json::json!({ "uri": uri, "name": symbol.name, "kind": kind, "range": range, "selectionRange": selection_range, "sourceFingerprint": source_fingerprint });
    Some(CallHierarchyItem {
        name: symbol.name.clone(),
        kind,
        tags: None,
        detail: symbol.routine_signature.clone(),
        uri: uri.clone(),
        range,
        selection_range,
        data: Some(identity),
    })
}

fn resolve_call<'a>(
    index: &'a NavigationIndex,
    uri: &Url,
    document: &super::ParsedDocument,
    call: Node<'_>,
    cancel: &AtomicBool,
    binding_budget: &mut super::AssistanceBudget,
) -> Result<Option<(Url, &'a Symbol)>, String> {
    check_cancel(cancel)?;
    let Some(entity) = call.child_by_field_name("entity") else {
        return Ok(None);
    };
    if !is_direct_callee_entity(entity) {
        return Ok(None);
    }
    let identifier = super::callable_lookup_identifier(entity);
    let Some(position) = text::offset_to_position(&document.source, identifier.start_byte()) else {
        return Ok(None);
    };
    let locations = index.navigate_with_cancel_and_budget(
        uri,
        position,
        crate::NavigationTarget::Definition,
        cancel,
        binding_budget,
    )?;
    if locations.len() != 1 {
        return Ok(None);
    }
    let location = &locations[0];
    let Some(target_doc) = index.documents.get(&location.uri) else {
        return Ok(None);
    };
    let Some(offset) = text::position_to_offset(&target_doc.source, location.range.start) else {
        return Ok(None);
    };
    let mut selected = target_doc.symbols.iter().filter(|symbol| {
        symbol.kind == SymbolKind::Routine
            && symbol.selection_span.start == offset
            && symbol.origin == Origin::Definition
    });
    let Some(symbol) = selected.next() else {
        return Ok(None);
    };
    if selected.next().is_some() {
        return Ok(None);
    }
    if target_doc
        .conditionals
        .is_unknown_at(symbol.selection_span.start)
        || target_doc.has_parser_recovery_near(symbol.declaration_span)
    {
        return Ok(None);
    }
    Ok(Some((location.uri.clone(), symbol)))
}

fn containing_caller(document: &super::ParsedDocument, span: Span) -> Option<(usize, &Symbol)> {
    document
        .symbols
        .iter()
        .enumerate()
        .filter(|(_, symbol)| {
            symbol.kind == SymbolKind::Routine
                && symbol.body_scope.is_some()
                && symbol.declaration_span.contains(span)
        })
        .min_by_key(|(_, symbol)| {
            symbol
                .declaration_span
                .end
                .saturating_sub(symbol.declaration_span.start)
        })
}

fn scan_calls(
    node: Node<'_>,
    budget: &mut ScanBudget,
    cancel: &AtomicBool,
    visit: &mut impl FnMut(Node<'_>) -> Result<(), String>,
) -> Result<(), String> {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        check_cancel(cancel)?;
        budget.visits += 1;
        if budget.visits > MAX_NODE_VISITS {
            return Err("call hierarchy AST scan work limit exceeded".to_string());
        }
        if node.kind() == "exprCall" {
            budget.calls += 1;
            if budget.calls > MAX_CALLS {
                return Err("call hierarchy call limit exceeded".to_string());
            }
            visit(node)?;
        }
        for index in (0..node.child_count()).rev() {
            if let Some(child) = node.child(index) {
                stack.push(child);
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct ScanBudget {
    visits: usize,
    calls: usize,
}

fn byte_range(source: &str, start: usize, end: usize) -> Option<Range> {
    Some(Range {
        start: text::offset_to_position(source, start)?,
        end: text::offset_to_position(source, end)?,
    })
}

#[derive(Default)]
struct ResultBudget {
    bytes: usize,
}

impl ResultBudget {
    fn charge_item(&mut self, uri: &Url, symbol: &Symbol) -> Result<(), String> {
        // Reserve conservatively for the map node/key, CallHierarchyItem,
        // serialized identity data (which repeats URI/name), and cloned detail
        // before constructing or inserting any of those allocations.
        let bytes = uri
            .as_str()
            .len()
            .saturating_mul(3)
            .saturating_add(symbol.name.len().saturating_mul(3))
            .saturating_add(symbol.routine_signature.as_ref().map_or(0, String::len))
            .saturating_add(1_024);
        self.charge(bytes)
    }

    fn charge_range(&mut self) -> Result<(), String> {
        // Includes the range plus amortized Vec capacity and map bookkeeping.
        self.charge(
            std::mem::size_of::<Range>()
                .saturating_mul(2)
                .saturating_add(64),
        )
    }

    fn charge(&mut self, bytes: usize) -> Result<(), String> {
        let next = self.bytes.saturating_add(bytes);
        if next > MAX_RESULT_BYTES {
            return Err("call hierarchy result byte limit exceeded".to_string());
        }
        self.bytes = next;
        Ok(())
    }
}

fn has_dynamic_dispatch(symbol: &Symbol) -> bool {
    symbol.routine_directives.dynamic
        || symbol.routine_directives.virtual_
        || symbol.routine_directives.override_
}

fn is_direct_callee_entity(entity: Node<'_>) -> bool {
    if !matches!(
        entity.kind(),
        "identifier" | "exprDot" | "genericDot" | "typerefDot" | "exprTpl"
    ) {
        return false;
    }
    let mut stack = vec![entity];
    while let Some(node) = stack.pop() {
        if node.kind() == "exprCall" {
            return false;
        }
        for index in 0..node.child_count() {
            if let Some(child) = node.child(index) {
                stack.push(child);
            }
        }
    }
    true
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        Err("request cancelled".to_string())
    } else {
        Ok(())
    }
}
