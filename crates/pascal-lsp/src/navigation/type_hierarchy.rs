use super::{
    AncestryResolutionState, AssistanceBudget, NavigationIndex, Span, Symbol, SymbolKind, TypeKind,
};
use crate::text;
use lsp_types::{Position, Range, TypeHierarchyItem, Url};
use std::hash::{Hash, Hasher};
use std::sync::atomic::AtomicBool;

const MAX_CANDIDATES: usize = 4096;
const MAX_RESULTS: usize = 2048;
const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;

pub(super) fn prepare(
    index: &NavigationIndex,
    uri: &Url,
    position: Position,
    cancel: &AtomicBool,
) -> Result<Option<Vec<TypeHierarchyItem>>, String> {
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
    let mut found = document.symbols.iter().filter(|symbol| {
        symbol.kind == SymbolKind::Type
            && matches!(symbol.type_kind, TypeKind::Class | TypeKind::Interface)
            && symbol.selection_span.start <= offset
            && offset < symbol.selection_span.end
    });
    let Some(symbol) = found.next() else {
        return Ok(None);
    };
    if found.next().is_some() || document.has_parser_recovery_near(symbol.declaration_span) {
        return Ok(None);
    }
    let key = canonical(&symbol.name);
    let Some(entries) = document.type_ancestry.get(&key) else {
        return Ok(None);
    };
    if entries.len() != 1 || entries[0].kind != symbol.type_kind {
        return Ok(None);
    }
    let Some(item) = item_for_symbol(uri, document, symbol) else {
        return Ok(None);
    };
    Ok(Some(vec![item]))
}

pub(super) fn supertypes(
    index: &NavigationIndex,
    item: &TypeHierarchyItem,
    cancel: &AtomicBool,
) -> Result<Option<Vec<TypeHierarchyItem>>, String> {
    let (uri, symbol_index) = validate_item(index, item)?;
    let symbol = &index.documents[&uri].symbols[symbol_index];
    if has_generic_ancestry(index, &uri, symbol) {
        return Ok(None);
    }
    let mut budget = hierarchy_budget();
    let mut state = AncestryResolutionState::new();
    let ancestry = index.resolve_direct_type_ancestry_with_budget(
        &uri,
        &canonical(&symbol.name),
        &mut state,
        cancel,
        &mut budget,
    )?;
    if ancestry.status != super::AncestryStatus::Complete {
        return Ok(None);
    }
    if ancestry.parents.iter().any(|(parent_uri, parent_key)| {
        index
            .documents
            .get(parent_uri)
            .and_then(|document| document.type_symbol_indices.get(parent_key))
            .is_some_and(|indices| {
                indices.len() != 1
                    || !index.documents[parent_uri].symbols[indices[0]]
                        .generic_parameters
                        .is_empty()
            })
    }) {
        return Ok(None);
    }
    let mut output = Vec::new();
    let mut bytes = 0usize;
    for (parent_uri, parent_key) in ancestry.parents {
        check_cancel(cancel)?;
        let Some(document) = index.documents.get(&parent_uri) else {
            return Ok(None);
        };
        let Some(indices) = document.type_symbol_indices.get(&parent_key) else {
            return Ok(None);
        };
        if indices.len() != 1 {
            return Ok(None);
        }
        let Some(parent) = item_for_symbol(&parent_uri, document, &document.symbols[indices[0]])
        else {
            return Ok(None);
        };
        charge_result(&mut bytes, &parent)?;
        output.push(parent);
        if output.len() > MAX_RESULTS {
            return Err("type hierarchy result limit exceeded".into());
        }
    }
    output.sort_by(|a, b| {
        (&a.uri, &a.name, a.selection_range.start).cmp(&(&b.uri, &b.name, b.selection_range.start))
    });
    output.dedup_by(|a, b| a.uri == b.uri && a.selection_range == b.selection_range);
    Ok(Some(output))
}

pub(super) fn subtypes(
    index: &NavigationIndex,
    item: &TypeHierarchyItem,
    cancel: &AtomicBool,
) -> Result<Option<Vec<TypeHierarchyItem>>, String> {
    let (target_uri, target_index) = validate_item(index, item)?;
    let target = &index.documents[&target_uri].symbols[target_index];
    if !target.generic_parameters.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let target_key = canonical(&target.name);
    let mut output = Vec::new();
    let mut bytes = 0usize;
    let mut candidates = 0usize;
    let mut state = AncestryResolutionState::new();
    let mut budget = hierarchy_budget();
    for (uri, document) in &index.documents {
        for symbol in document.symbols.iter().filter(|symbol| {
            symbol.kind == SymbolKind::Type
                && matches!(symbol.type_kind, TypeKind::Class | TypeKind::Interface)
        }) {
            check_cancel(cancel)?;
            candidates = candidates.saturating_add(1);
            if candidates > MAX_CANDIDATES {
                return Err("type hierarchy candidate limit exceeded".into());
            }
            if document
                .conditionals
                .is_unknown_at(symbol.selection_span.start)
                || document.has_parser_recovery_near(symbol.declaration_span)
            {
                continue;
            }
            if has_generic_ancestry(index, uri, symbol) {
                continue;
            }
            let ancestry = index.resolve_direct_type_ancestry_with_budget(
                uri,
                &canonical(&symbol.name),
                &mut state,
                cancel,
                &mut budget,
            )?;
            if ancestry.status != super::AncestryStatus::Complete {
                continue;
            }
            if ancestry.parents.iter().any(|(parent_uri, parent_key)| {
                index
                    .documents
                    .get(parent_uri)
                    .and_then(|parent_document| parent_document.type_symbol_indices.get(parent_key))
                    .is_some_and(|indices| {
                        indices.len() != 1
                            || !index.documents[parent_uri].symbols[indices[0]]
                                .generic_parameters
                                .is_empty()
                    })
            }) {
                continue;
            }
            if !ancestry.parents.iter().any(|(parent_uri, parent_key)| {
                parent_uri == &target_uri && parent_key == &target_key
            }) {
                continue;
            }
            let Some(child) = item_for_symbol(uri, document, symbol) else {
                continue;
            };
            charge_result(&mut bytes, &child)?;
            output.push(child);
            if output.len() > MAX_RESULTS {
                return Err("type hierarchy result limit exceeded".into());
            }
        }
    }
    output.sort_by(|a, b| {
        (&a.uri, &a.name, a.selection_range.start).cmp(&(&b.uri, &b.name, b.selection_range.start))
    });
    output.dedup_by(|a, b| a.uri == b.uri && a.selection_range == b.selection_range);
    Ok(Some(output))
}

fn hierarchy_budget() -> AssistanceBudget {
    AssistanceBudget::new(
        super::MAX_NAVIGATION_OVERLOAD_WORK,
        super::MAX_NAVIGATION_OVERLOAD_BYTES,
        "type hierarchy",
    )
}

fn has_generic_ancestry(index: &NavigationIndex, uri: &Url, symbol: &Symbol) -> bool {
    if !symbol.generic_parameters.is_empty() {
        return true;
    }
    match index
        .documents
        .get(uri)
        .and_then(|document| document.type_ancestry.get(&canonical(&symbol.name)))
    {
        Some(entries) if entries.len() == 1 => entries[0].parents.iter().any(|parent| {
            parent
                .type_ref
                .as_ref()
                .is_some_and(|reference| !reference.args.is_empty())
        }),
        _ => true,
    }
}

fn validate_item(
    index: &NavigationIndex,
    item: &TypeHierarchyItem,
) -> Result<(Url, usize), String> {
    let data = item
        .data
        .as_ref()
        .ok_or_else(|| "type hierarchy item has no identity data".to_string())?;
    let document = index
        .documents
        .get(&item.uri)
        .ok_or_else(|| "type hierarchy source is not selected".to_string())?;
    let mut found = None;
    for (i, symbol) in document.symbols.iter().enumerate().filter(|(_, s)| {
        s.kind == SymbolKind::Type && matches!(s.type_kind, TypeKind::Class | TypeKind::Interface)
    }) {
        let Some(candidate) = item_for_symbol(&item.uri, document, symbol) else {
            continue;
        };
        let matches = candidate.name == item.name
            && candidate.kind == item.kind
            && candidate.range == item.range
            && candidate.selection_range == item.selection_range
            && candidate.data.as_ref() == Some(data);
        if matches && found.is_some() {
            return Err("type hierarchy item identity is ambiguous".into());
        }
        if matches {
            found = Some(i);
        }
    }
    found
        .map(|i| (item.uri.clone(), i))
        .ok_or_else(|| "stale or forged type hierarchy item".into())
}

fn item_for_symbol(
    uri: &Url,
    document: &super::ParsedDocument,
    symbol: &Symbol,
) -> Option<TypeHierarchyItem> {
    let range = byte_range(&document.source, symbol.declaration_span)?;
    let selection_range = byte_range(&document.source, symbol.selection_span)?;
    let kind = match symbol.type_kind {
        TypeKind::Class => lsp_types::SymbolKind::CLASS,
        TypeKind::Interface => lsp_types::SymbolKind::INTERFACE,
        _ => return None,
    };
    let source = document
        .source
        .get(symbol.declaration_span.start..symbol.declaration_span.end)?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hash);
    symbol.type_kind.hash(&mut hash);
    symbol.name.hash(&mut hash);
    let data = serde_json::json!({"uri": uri, "name": symbol.name, "kind": kind, "range": range, "selectionRange": selection_range, "fingerprint": hash.finish()});
    Some(TypeHierarchyItem {
        name: symbol.name.clone(),
        kind,
        tags: None,
        detail: None,
        uri: uri.clone(),
        range,
        selection_range,
        data: Some(data),
    })
}

fn byte_range(source: &str, span: Span) -> Option<Range> {
    Some(Range {
        start: text::offset_to_position(source, span.start)?,
        end: text::offset_to_position(source, span.end)?,
    })
}

fn canonical(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn charge_result(bytes: &mut usize, item: &TypeHierarchyItem) -> Result<(), String> {
    let estimate = item
        .uri
        .as_str()
        .len()
        .saturating_add(item.name.len())
        .saturating_add(256);
    *bytes = bytes.saturating_add(estimate);
    if *bytes > MAX_RESULT_BYTES {
        Err("type hierarchy result byte limit exceeded".into())
    } else {
        Ok(())
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        Err("type hierarchy cancelled".into())
    } else {
        Ok(())
    }
}
