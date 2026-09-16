use super::{
    Document, NavigationIndex, ROOT_SCOPE, RoutineKind, Span, Symbol, SymbolKind, TypeKind,
};
use crate::text::PositionIndex;
use lsp_types::{
    DocumentSymbol, Location, Range, SymbolInformation, SymbolKind as LspSymbolKind, Url,
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_SYMBOL_RESULTS: usize = 10_000;
const MAX_DOCUMENT_SYMBOL_DEPTH: usize = 32;
const CANCELLATION_MESSAGE: &str = "request cancelled";

#[derive(Debug, Clone)]
struct OutlineEntry {
    name: String,
    kind: LspSymbolKind,
    declaration_span: Span,
    selection_span: Span,
    parent: Option<usize>,
    children: Vec<usize>,
    source_order: usize,
}

struct Outline {
    entries: Vec<OutlineEntry>,
    roots: Vec<usize>,
}

pub(super) fn document_symbols(
    index: &NavigationIndex,
    uri: &Url,
) -> Result<Vec<DocumentSymbol>, String> {
    document_symbols_impl(index, uri, None)
}

pub(super) fn document_symbols_with_cancel(
    index: &NavigationIndex,
    uri: &Url,
    cancel: &AtomicBool,
) -> Result<Vec<DocumentSymbol>, String> {
    document_symbols_impl(index, uri, Some(cancel))
}

fn document_symbols_impl(
    index: &NavigationIndex,
    uri: &Url,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<DocumentSymbol>, String> {
    check_cancel(cancel)?;
    let document = index
        .documents
        .get(uri)
        .ok_or_else(|| format!("document is not indexed: {uri}"))?;
    let outline = build_outline(document, cancel, Some(MAX_SYMBOL_RESULTS))?;
    validate_hierarchy_depth(&outline, cancel)?;
    let positions = position_index(document, cancel)?;
    let mut result = Vec::with_capacity(outline.roots.len());
    for root in &outline.roots {
        check_cancel(cancel)?;
        result.push(document_symbol(
            &outline, *root, document, &positions, cancel,
        )?);
    }
    Ok(result)
}

pub(super) fn workspace_symbols(
    index: &NavigationIndex,
    query: &str,
) -> Result<Vec<SymbolInformation>, String> {
    workspace_symbols_impl(index, query, None)
}

pub(super) fn workspace_symbols_with_cancel(
    index: &NavigationIndex,
    query: &str,
    cancel: &AtomicBool,
) -> Result<Vec<SymbolInformation>, String> {
    workspace_symbols_impl(index, query, Some(cancel))
}

fn workspace_symbols_impl(
    index: &NavigationIndex,
    query: &str,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<SymbolInformation>, String> {
    check_cancel(cancel)?;
    let query = query.to_lowercase();
    let mut result = Vec::new();
    let mut seen_selections = HashSet::new();

    let mut documents: Vec<_> = index.documents.iter().collect();
    documents.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));

    for (uri, document) in documents {
        check_cancel(cancel)?;
        let unit_name = document
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Unit)
            .map(|symbol| symbol.name.as_str())
            .unwrap_or(document.unit_name.as_str());
        let mut matches = Vec::new();
        for (symbol_index, symbol) in document.symbols.iter().enumerate() {
            check_cancel(cancel)?;
            if !workspace_eligible(symbol) || !symbol.name.to_lowercase().contains(&query) {
                continue;
            }
            if !seen_selections.insert(((*uri).clone(), symbol.selection_span)) {
                continue;
            }
            if result.len() + matches.len() >= MAX_SYMBOL_RESULTS {
                return Err(format!(
                    "workspace symbol query returned more than {MAX_SYMBOL_RESULTS} results; narrow the query"
                ));
            }
            matches.push((symbol_index, symbol));
        }

        matches.sort_by(|(left_index, left), (right_index, right)| {
            left.declaration_span
                .start
                .cmp(&right.declaration_span.start)
                .then_with(|| right.declaration_span.end.cmp(&left.declaration_span.end))
                .then_with(|| left.selection_span.start.cmp(&right.selection_span.start))
                .then_with(|| left_index.cmp(right_index))
        });

        if !matches.is_empty() {
            let positions = position_index(document, cancel)?;
            let mut projection_cancel =
                |_| cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed));
            result.extend(project_workspace_symbols(
                uri,
                unit_name,
                &document.source,
                &positions,
                matches,
                &mut projection_cancel,
            )?);
        }
    }
    Ok(result)
}

pub(super) fn flatten_document_symbols(
    uri: &Url,
    symbols: Vec<DocumentSymbol>,
) -> Vec<SymbolInformation> {
    let mut result = Vec::new();
    for symbol in symbols {
        flatten_symbol(symbol, uri, None, &mut result);
    }
    result
}

fn build_outline(
    document: &Document,
    cancel: Option<&AtomicBool>,
    limit: Option<usize>,
) -> Result<Outline, String> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for (source_order, symbol) in document.symbols.iter().enumerate() {
        check_cancel(cancel)?;
        if !outline_eligible(symbol) {
            continue;
        }
        let key = (
            symbol.name.clone(),
            symbol.kind,
            symbol.declaration_span,
            symbol.selection_span,
        );
        if !seen.insert(key) {
            continue;
        }
        if limit.is_some_and(|limit| entries.len() >= limit) {
            return Err(format!(
                "document symbol result exceeds the {MAX_SYMBOL_RESULTS}-entry limit"
            ));
        }
        entries.push(OutlineEntry {
            name: symbol.name.clone(),
            kind: lsp_kind(symbol),
            declaration_span: symbol.declaration_span,
            selection_span: symbol.selection_span,
            parent: None,
            children: Vec::new(),
            source_order,
        });
    }

    entries.sort_by(|left, right| {
        left.declaration_span
            .start
            .cmp(&right.declaration_span.start)
            .then_with(|| right.declaration_span.end.cmp(&left.declaration_span.end))
            .then_with(|| left.selection_span.start.cmp(&right.selection_span.start))
            .then_with(|| left.source_order.cmp(&right.source_order))
    });

    let mut containing: Vec<usize> = Vec::new();
    for child_index in 0..entries.len() {
        check_cancel(cancel)?;
        let child_span = entries[child_index].declaration_span;
        while containing.last().is_some_and(|parent| {
            let parent_span = entries[*parent].declaration_span;
            parent_span == child_span || !parent_span.contains(child_span)
        }) {
            containing.pop();
        }
        entries[child_index].parent = containing.last().copied();
        containing.push(child_index);
    }

    let mut roots = Vec::new();
    for index in 0..entries.len() {
        check_cancel(cancel)?;
        if let Some(parent) = entries[index].parent {
            entries[parent].children.push(index);
        } else {
            roots.push(index);
        }
    }
    Ok(Outline { entries, roots })
}

fn validate_hierarchy_depth(outline: &Outline, cancel: Option<&AtomicBool>) -> Result<(), String> {
    let mut pending = outline
        .roots
        .iter()
        .rev()
        .map(|root| (*root, 1))
        .collect::<Vec<_>>();
    while let Some((index, depth)) = pending.pop() {
        check_cancel(cancel)?;
        if depth > MAX_DOCUMENT_SYMBOL_DEPTH {
            return Err(format!(
                "document symbol hierarchy exceeds the {MAX_DOCUMENT_SYMBOL_DEPTH}-level depth limit"
            ));
        }
        for child in outline.entries[index].children.iter().rev() {
            pending.push((*child, depth.saturating_add(1)));
        }
    }
    Ok(())
}

fn outline_eligible(symbol: &Symbol) -> bool {
    matches!(
        symbol.kind,
        SymbolKind::Unit
            | SymbolKind::Type
            | SymbolKind::Routine
            | SymbolKind::Variable
            | SymbolKind::Constant
            | SymbolKind::Field
            | SymbolKind::Property
            | SymbolKind::EnumValue
    )
}

fn workspace_eligible(symbol: &Symbol) -> bool {
    if !outline_eligible(symbol) {
        return false;
    }
    match symbol.kind {
        SymbolKind::Variable | SymbolKind::Constant | SymbolKind::Parameter => {
            symbol.scope == ROOT_SCOPE
        }
        _ => true,
    }
}

#[allow(deprecated)]
fn document_symbol(
    outline: &Outline,
    index: usize,
    document: &Document,
    positions: &PositionIndex,
    cancel: Option<&AtomicBool>,
) -> Result<DocumentSymbol, String> {
    check_cancel(cancel)?;
    let entry = &outline.entries[index];
    let children = if entry.children.is_empty() {
        None
    } else {
        let mut children = Vec::with_capacity(entry.children.len());
        for child in &entry.children {
            check_cancel(cancel)?;
            children.push(document_symbol(
                outline, *child, document, positions, cancel,
            )?);
        }
        Some(children)
    };
    Ok(DocumentSymbol {
        name: entry.name.clone(),
        detail: None,
        kind: entry.kind,
        tags: None,
        deprecated: None,
        range: range_for_span(positions, &document.source, entry.declaration_span)?,
        selection_range: range_for_span(positions, &document.source, entry.selection_span)?,
        children,
    })
}

fn workspace_container_name(unit_name: &str, symbol: &Symbol) -> Option<String> {
    if symbol.kind == SymbolKind::Unit {
        None
    } else {
        symbol
            .owner_type_name
            .clone()
            .or_else(|| Some(unit_name.to_owned()))
    }
}

#[allow(deprecated)]
fn project_workspace_symbols<'a, I, F>(
    uri: &Url,
    unit_name: &str,
    source: &str,
    positions: &PositionIndex,
    matches: I,
    is_cancelled: &mut F,
) -> Result<Vec<SymbolInformation>, String>
where
    I: IntoIterator<Item = (usize, &'a Symbol)>,
    F: FnMut(usize) -> bool,
{
    let mut result = Vec::new();
    for (projected, (_, symbol)) in matches.into_iter().enumerate() {
        if is_cancelled(projected) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let range = range_for_span(positions, source, symbol.declaration_span)?;
        result.push(SymbolInformation {
            name: symbol.name.clone(),
            kind: lsp_kind(symbol),
            tags: None,
            deprecated: None,
            location: Location {
                uri: uri.clone(),
                range,
            },
            container_name: workspace_container_name(unit_name, symbol),
        });
    }
    Ok(result)
}

#[allow(deprecated)]
fn flatten_symbol(
    symbol: DocumentSymbol,
    uri: &Url,
    parent: Option<&str>,
    result: &mut Vec<SymbolInformation>,
) {
    let mut pending = vec![(symbol, parent.map(str::to_owned))];
    while let Some((symbol, parent)) = pending.pop() {
        let DocumentSymbol {
            name,
            kind,
            tags,
            deprecated,
            range,
            selection_range: _,
            children,
            detail: _,
        } = symbol;
        result.push(SymbolInformation {
            name: name.clone(),
            kind,
            tags,
            #[allow(deprecated)]
            deprecated,
            location: Location {
                uri: uri.clone(),
                range,
            },
            container_name: parent,
        });
        if let Some(children) = children {
            for child in children.into_iter().rev() {
                pending.push((child, Some(name.clone())));
            }
        }
    }
}

fn range_for_span(positions: &PositionIndex, source: &str, span: Span) -> Result<Range, String> {
    if span.start > span.end {
        return Err("symbol span has an inverted range".to_string());
    }
    let start = positions
        .offset_to_position(source, span.start)
        .ok_or_else(|| "symbol start is not a valid UTF-8/UTF-16 boundary".to_string())?;
    let end = positions
        .offset_to_position(source, span.end)
        .ok_or_else(|| "symbol end is not a valid UTF-8/UTF-16 boundary".to_string())?;
    Ok(Range { start, end })
}

fn position_index(
    document: &Document,
    cancel: Option<&AtomicBool>,
) -> Result<PositionIndex, String> {
    match cancel {
        Some(cancel) => PositionIndex::new_with_cancel(&document.source, cancel)
            .map_err(|()| CANCELLATION_MESSAGE.to_string()),
        None => Ok(PositionIndex::new(&document.source)),
    }
}

fn check_cancel(cancel: Option<&AtomicBool>) -> Result<(), String> {
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
        Err(CANCELLATION_MESSAGE.to_string())
    } else {
        Ok(())
    }
}

fn lsp_kind(symbol: &Symbol) -> LspSymbolKind {
    match symbol.kind {
        SymbolKind::Unit => LspSymbolKind::MODULE,
        SymbolKind::Type => match symbol.type_kind {
            TypeKind::Class => LspSymbolKind::CLASS,
            TypeKind::Record => LspSymbolKind::STRUCT,
            TypeKind::Interface => LspSymbolKind::INTERFACE,
            TypeKind::Enum => LspSymbolKind::ENUM,
            TypeKind::Array => LspSymbolKind::ARRAY,
            TypeKind::Callable => LspSymbolKind::FUNCTION,
            TypeKind::String => LspSymbolKind::STRING,
            TypeKind::File => LspSymbolKind::FILE,
            TypeKind::Other => LspSymbolKind::TYPE_PARAMETER,
        },
        SymbolKind::Routine => match symbol.routine_kind {
            RoutineKind::Constructor => LspSymbolKind::CONSTRUCTOR,
            RoutineKind::Function if symbol.owner_type_name.is_none() => LspSymbolKind::FUNCTION,
            RoutineKind::Operator => LspSymbolKind::OPERATOR,
            _ if symbol.owner_type_name.is_some() => LspSymbolKind::METHOD,
            _ => LspSymbolKind::FUNCTION,
        },
        SymbolKind::Variable => LspSymbolKind::VARIABLE,
        SymbolKind::Constant => LspSymbolKind::CONSTANT,
        SymbolKind::Field => LspSymbolKind::FIELD,
        SymbolKind::Property => LspSymbolKind::PROPERTY,
        SymbolKind::EnumValue => LspSymbolKind::ENUM_MEMBER,
        SymbolKind::Parameter | SymbolKind::Label => LspSymbolKind::KEY,
    }
}

#[cfg(test)]
mod tests {
    use super::super::NavigationIndex;
    use super::{PositionIndex, project_workspace_symbols};
    use lsp_types::Url;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn cancellable_workspace_symbol_queries_honor_cancellation() {
        let uri = Url::parse("file:///workspace/Cancellable.pas").expect("test URI");
        let mut index = NavigationIndex::new();
        index
            .update(
                uri,
                "unit Cancellable; interface procedure Visible; implementation end.".to_string(),
            )
            .expect("source parses");
        let cancel = AtomicBool::new(true);

        assert_eq!(
            index
                .workspace_symbols_with_cancel("Visible", &cancel)
                .expect_err("cancelled workspace query must fail"),
            "request cancelled"
        );
    }

    #[test]
    fn workspace_symbol_projection_honors_cancellation_after_projected_entries() {
        let uri = Url::parse("file:///workspace/ProjectionCancellation.pas").expect("test URI");
        let source = "unit ProjectionCancellation; interface procedure Visible0; procedure Visible1; procedure Visible2; procedure Visible3; implementation end.";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_string())
            .expect("source parses");
        let document = index.documents.get(&uri).expect("indexed document");
        let positions = PositionIndex::new(&document.source);
        let matches = document
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| symbol.name.starts_with("Visible"))
            .take(4)
            .collect::<Vec<_>>();
        assert_eq!(
            matches.len(),
            4,
            "projection fixture must have four matches"
        );

        let mut no_cancellation = |_| false;
        let projected = project_workspace_symbols(
            &uri,
            &document.unit_name,
            &document.source,
            &positions,
            matches.clone(),
            &mut no_cancellation,
        )
        .expect("the projection control must emit every match");
        assert_eq!(
            projected
                .iter()
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>(),
            ["Visible0", "Visible1", "Visible2", "Visible3"]
        );

        let mut cancellation_checks = Vec::new();
        let error = project_workspace_symbols(
            &uri,
            &document.unit_name,
            &document.source,
            &positions,
            matches,
            &mut |projected| {
                cancellation_checks.push(projected);
                projected >= 3
            },
        )
        .expect_err("cancellation must interrupt projection after three entries");
        assert_eq!(cancellation_checks, [0, 1, 2, 3]);
        assert_eq!(error, "request cancelled");
    }
}
