use super::{
    AncestryResolutionState, AssistanceBudget, NavigationIndex, Span, Symbol, SymbolKind, TypeKind,
};
use crate::text::{self, PositionIndex};
use lsp_types::{Position, Range, TypeHierarchyItem, Url};
use std::collections::HashSet;
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
    let positions = position_index(document, cancel)?;
    let Some(item) = item_for_symbol(uri, document, &positions, symbol) else {
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
    let key = canonical(&symbol.name);
    let mut cycle_check = TypeHierarchyCycleCheck::new(index, cancel, &mut state, &mut budget);
    let ancestry = cycle_check.direct_ancestry(&uri, &key)?;
    if ancestry.status != super::AncestryStatus::Complete {
        return Ok(None);
    }
    if !cycle_check.has_acyclic_known_ancestry(&uri, &key)? {
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
        let positions = position_index(document, cancel)?;
        let Some(parent) = item_for_symbol(
            &parent_uri,
            document,
            &positions,
            &document.symbols[indices[0]],
        ) else {
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
    let mut cycle_check = TypeHierarchyCycleCheck::new(index, cancel, &mut state, &mut budget);
    if !cycle_check.has_acyclic_known_ancestry(&target_uri, &target_key)? {
        return Ok(None);
    }
    for (uri, document) in &index.documents {
        let mut positions = None;
        for symbol in document.symbols.iter().filter(|symbol| {
            symbol.kind == SymbolKind::Type
                && matches!(symbol.type_kind, TypeKind::Class | TypeKind::Interface)
        }) {
            check_cancel(cancel)?;
            candidates = candidates.saturating_add(1);
            if candidates > MAX_CANDIDATES {
                return Err("type hierarchy candidate limit exceeded".into());
            }
            cycle_check.start_candidate();
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
            let key = canonical(&symbol.name);
            let ancestry = cycle_check.direct_ancestry(uri, &key)?;
            if ancestry.status != super::AncestryStatus::Complete {
                continue;
            }
            if !cycle_check.has_acyclic_known_ancestry(uri, &key)? {
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
            let positions = match positions.as_mut() {
                Some(positions) => positions,
                None => positions.insert(position_index(document, cancel)?),
            };
            let Some(child) = item_for_symbol(uri, document, positions, symbol) else {
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

struct TypeHierarchyCycleCheck<'a> {
    index: &'a NavigationIndex,
    cancel: &'a AtomicBool,
    state: &'a mut AncestryResolutionState,
    budget: &'a mut AssistanceBudget,
    active: HashSet<(Url, String)>,
    known_acyclic: HashSet<(Url, String)>,
}

impl<'a> TypeHierarchyCycleCheck<'a> {
    fn new(
        index: &'a NavigationIndex,
        cancel: &'a AtomicBool,
        state: &'a mut AncestryResolutionState,
        budget: &'a mut AssistanceBudget,
    ) -> Self {
        Self {
            index,
            cancel,
            state,
            budget,
            active: HashSet::new(),
            known_acyclic: HashSet::new(),
        }
    }

    /// Bound each subtype candidate's ancestry walk independently; the shared
    /// budget and the candidate cap bound the scan as a whole.
    fn start_candidate(&mut self) {
        *self.state = AncestryResolutionState::new();
    }

    fn direct_ancestry(
        &mut self,
        uri: &Url,
        key: &str,
    ) -> Result<super::TypeAncestryResolution, String> {
        self.index.resolve_direct_type_ancestry_with_budget(
            uri,
            key,
            self.state,
            self.cancel,
            self.budget,
        )
    }

    fn has_acyclic_known_ancestry(&mut self, uri: &Url, key: &str) -> Result<bool, String> {
        check_cancel(self.cancel)?;
        let identity_bytes = std::mem::size_of::<(Url, String)>()
            .saturating_add(uri.as_str().len())
            .saturating_add(key.len());
        self.budget
            .require_owned_bytes(identity_bytes, self.cancel)?;
        let identity = (uri.clone(), key.to_owned());
        if self.active.contains(&identity) {
            return Ok(false);
        }
        if self.known_acyclic.contains(&identity) {
            return Ok(true);
        }
        self.budget
            .require_owned_bytes(identity_bytes, self.cancel)?;
        self.active.insert(identity.clone());
        let result = self.direct_ancestry(uri, key)?;
        let mut acyclic = true;
        for (parent_uri, parent_key) in result.parents {
            if !self.has_acyclic_known_ancestry(&parent_uri, &parent_key)? {
                acyclic = false;
                break;
            }
        }
        self.active.remove(&identity);
        if acyclic && result.status == super::AncestryStatus::Complete {
            self.known_acyclic.insert(identity);
        }
        Ok(acyclic)
    }
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
    let mut positions = None;
    for (i, symbol) in document.symbols.iter().enumerate().filter(|(_, s)| {
        s.kind == SymbolKind::Type
            && matches!(s.type_kind, TypeKind::Class | TypeKind::Interface)
            && s.name == item.name
    }) {
        let positions = positions.get_or_insert_with(|| PositionIndex::new(&document.source));
        let Some(candidate) = item_for_symbol(&item.uri, document, positions, symbol) else {
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

#[cfg(test)]
thread_local! {
    static ITEM_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn item_for_symbol(
    uri: &Url,
    document: &super::ParsedDocument,
    positions: &PositionIndex,
    symbol: &Symbol,
) -> Option<TypeHierarchyItem> {
    #[cfg(test)]
    ITEM_BUILDS.with(|builds| builds.set(builds.get().saturating_add(1)));
    let range = byte_range(positions, &document.source, symbol.declaration_span)?;
    let selection_range = byte_range(positions, &document.source, symbol.selection_span)?;
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

fn byte_range(positions: &PositionIndex, source: &str, span: Span) -> Option<Range> {
    Some(Range {
        start: positions.offset_to_position(source, span.start)?,
        end: positions.offset_to_position(source, span.end)?,
    })
}

fn position_index(
    document: &super::ParsedDocument,
    cancel: &AtomicBool,
) -> Result<PositionIndex, String> {
    PositionIndex::new_with_cancel(&document.source, cancel)
        .map_err(|()| "type hierarchy cancelled".to_string())
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

#[cfg(test)]
mod tests {
    use super::{ITEM_BUILDS, prepare, subtypes, validate_item};
    use crate::NavigationIndex;
    use crate::text::{SOURCE_SCAN_CONVERSIONS, offset_to_position};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn item_validation_builds_only_same_named_candidates() {
        let mut source = String::from("unit ManyTypes; interface type TBase = class end;\n");
        for index in 0..64 {
            source.push_str(&format!("TChild{index} = class(TBase) end;\n"));
        }
        source.push_str("implementation end.");
        let uri = lsp_types::Url::parse("file:///ManyTypes.pas").expect("type hierarchy URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.clone())
            .expect("type hierarchy source parses");
        let position = offset_to_position(&source, source.find("TBase").expect("base type"))
            .expect("base position");
        let item = prepare(&index, &uri, position, &AtomicBool::new(false))
            .expect("prepare type hierarchy")
            .expect("base item")
            .remove(0);

        ITEM_BUILDS.with(|builds| builds.set(0));
        let (validated_uri, _) = validate_item(&index, &item).expect("validated item");

        assert_eq!(validated_uri, uri);
        assert_eq!(ITEM_BUILDS.with(std::cell::Cell::get), 1);
    }

    #[test]
    fn subtype_items_do_not_rescan_the_source_per_child() {
        let mut source = String::from("unit ManyTypes; interface type TBase = class end;\n");
        for index in 0..64 {
            source.push_str(&format!("TChild{index} = class(TBase) end;\n"));
        }
        source.push_str("implementation end.");
        let uri = lsp_types::Url::parse("file:///ManyTypes.pas").expect("type hierarchy URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.clone())
            .expect("type hierarchy source parses");
        let cancel = AtomicBool::new(false);
        let position = offset_to_position(&source, source.find("TBase").expect("base type"))
            .expect("base position");
        let item = prepare(&index, &uri, position, &cancel)
            .expect("prepare type hierarchy")
            .expect("base item")
            .remove(0);

        SOURCE_SCAN_CONVERSIONS.with(|scans| scans.set(0));
        let children = subtypes(&index, &item, &cancel)
            .expect("subtypes")
            .expect("proven subtypes");

        assert_eq!(children.len(), 64);
        assert_eq!(SOURCE_SCAN_CONVERSIONS.with(std::cell::Cell::get), 0);
    }

    #[test]
    fn subtypes_are_found_among_hundreds_of_unrelated_classes() {
        let mut source = String::from("unit Wide; interface type TBase = class end;\n");
        for index in 0..300 {
            source.push_str(&format!("TOther{index} = class end;\n"));
        }
        source.push_str("TChild = class(TBase) end;\nimplementation end.");
        let uri = lsp_types::Url::parse("file:///Wide.pas").expect("type hierarchy URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.clone())
            .expect("type hierarchy source parses");
        let cancel = AtomicBool::new(false);
        let position = offset_to_position(&source, source.find("TBase").expect("base type"))
            .expect("base position");
        let item = prepare(&index, &uri, position, &cancel)
            .expect("prepare type hierarchy")
            .expect("base item")
            .remove(0);

        let children = subtypes(&index, &item, &cancel)
            .expect("subtypes within the candidate limit")
            .expect("proven subtypes");

        assert_eq!(
            children
                .iter()
                .map(|child| child.name.as_str())
                .collect::<Vec<_>>(),
            ["TChild"]
        );
    }
}
