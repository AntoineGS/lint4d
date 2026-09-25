use super::{AssistanceBudget, Document, NavigationIndex, Span, canonical_name};
use crate::text::PositionIndex;
use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Position, Range, Url};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use tree_sitter::Node;

const MAX_INLAY_TRAVERSAL_NODES: usize = 100_000;
const MAX_INLAY_CALLS: usize = 2_000;
const MAX_INLAY_HINTS: usize = 512;
const MAX_INLAY_LABEL_BYTES: usize = 16 * 1024;
const MAX_INLAY_RANGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_INLAY_EXCLUSIONS: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InlayHintOptions {
    pub(crate) parameter_names: bool,
    pub(crate) inferred_types: bool,
    pub(crate) range_limit: Option<usize>,
}

impl Default for InlayHintOptions {
    fn default() -> Self {
        Self {
            parameter_names: true,
            inferred_types: true,
            range_limit: None,
        }
    }
}

pub(super) fn inlay_hints_with_cancel(
    index: &NavigationIndex,
    uri: &Url,
    range: Range,
    options: InlayHintOptions,
    cancel: &AtomicBool,
) -> Result<Vec<InlayHint>, String> {
    check_cancel(cancel)?;
    let Some(document) = index.documents.get(uri) else {
        return Err(format!("document is not indexed: {uri}"));
    };
    if range.start > range.end || options.range_limit == Some(0) {
        return Ok(Vec::new());
    }
    let positions = PositionIndex::new_with_cancel(&document.source, cancel)
        .map_err(|()| "request cancelled".to_string())?;
    let (Some(range_start), Some(range_end)) = (
        positions.position_to_offset(&document.source, range.start),
        positions.position_to_offset(&document.source, range.end),
    ) else {
        return Ok(Vec::new());
    };
    if range_end.saturating_sub(range_start) > MAX_INLAY_RANGE_BYTES {
        return Err(format!(
            "inlay hint range exceeds the {MAX_INLAY_RANGE_BYTES}-byte limit"
        ));
    }
    let excluded_spans = exclusion_spans(document, cancel)?;

    let mut budget = AssistanceBudget::new(250_000, 16 * 1024 * 1024, "inlay hints");
    let mut pending = vec![document.tree.root_node()];
    let mut visited = 0_usize;
    let mut calls = 0_usize;
    let mut hints = Vec::new();
    let mut unique = HashSet::new();
    let mut label_bytes = 0_usize;
    while let Some(node) = pending.pop() {
        check_cancel(cancel)?;
        visited = visited.saturating_add(1);
        if visited > MAX_INLAY_TRAVERSAL_NODES {
            return Err(format!(
                "inlay syntax traversal exceeds the {MAX_INLAY_TRAVERSAL_NODES}-node limit"
            ));
        }
        let span = Span::from_node(node);
        if span.end < range_start || span.start > range_end {
            continue;
        }
        if options.parameter_names && node.kind() == "exprCall" && !excluded(&excluded_spans, span)
        {
            calls = calls.saturating_add(1);
            if calls > MAX_INLAY_CALLS {
                return Err(format!(
                    "inlay call count exceeds the {MAX_INLAY_CALLS}-call limit"
                ));
            }
            collect_call_hints(
                index,
                uri,
                document,
                node,
                range,
                &positions,
                &excluded_spans,
                cancel,
                &mut budget,
                &mut hints,
                &mut unique,
                &mut label_bytes,
            )?;
        }
        if options.inferred_types && node.kind() == "declConst" && !excluded(&excluded_spans, span)
        {
            collect_boolean_constant_hint(
                document,
                node,
                range,
                &positions,
                cancel,
                &mut hints,
                &mut unique,
                &mut label_bytes,
            )?;
        }
        let mut cursor = node.walk();
        if node.named_child_count() > MAX_INLAY_TRAVERSAL_NODES.saturating_sub(visited) {
            return Err(format!(
                "inlay syntax traversal exceeds the {MAX_INLAY_TRAVERSAL_NODES}-node limit"
            ));
        }
        let children = node.named_children(&mut cursor).collect::<Vec<_>>();
        pending.extend(children.into_iter().rev());
    }
    hints.sort_by_key(|hint| {
        (
            hint.position.line,
            hint.position.character,
            hint.kind
                .map(|kind| kind == InlayHintKind::TYPE)
                .unwrap_or(false),
        )
    });
    hints.dedup_by(|left, right| {
        left.position == right.position && label_text(&left.label) == label_text(&right.label)
    });
    if options.range_limit.is_some_and(|limit| hints.len() > limit) {
        hints.truncate(options.range_limit.unwrap_or_default());
    }
    Ok(hints)
}

#[allow(clippy::too_many_arguments)]
fn collect_call_hints(
    index: &NavigationIndex,
    uri: &Url,
    document: &Document,
    call: Node<'_>,
    range: Range,
    positions: &PositionIndex,
    excluded_spans: &[Span],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
    hints: &mut Vec<InlayHint>,
    unique: &mut HashSet<(Position, String)>,
    label_bytes: &mut usize,
) -> Result<(), String> {
    let Some(args) = call.child_by_field_name("args") else {
        return Ok(());
    };
    let Some(names) =
        super::overload::parameter_names_for_call(index, uri, document, call, cancel, budget)?
    else {
        return Ok(());
    };
    let mut cursor = args.walk();
    let arguments = args
        .named_children(&mut cursor)
        .filter(|argument| argument.kind() != "legacyFormat")
        .collect::<Vec<_>>();
    if arguments.iter().any(|argument| {
        argument.kind().to_ascii_lowercase().contains("named")
            || document
                .source
                .get(argument.start_byte()..argument.end_byte())
                .is_some_and(|text| text.contains(":="))
    }) {
        return Ok(());
    }
    let mut ordinal = 0_usize;
    for argument in arguments {
        check_cancel(cancel)?;
        let Some(name) = names.get(ordinal) else {
            return Ok(());
        };
        ordinal = ordinal.saturating_add(1);
        let argument_span = Span::from_node(argument);
        if excluded(excluded_spans, argument_span) {
            continue;
        }
        let text = document
            .source
            .get(argument_span.start..argument_span.end)
            .unwrap_or_default()
            .trim();
        if argument.kind() == "identifier" && canonical_name(text) == canonical_name(name) {
            continue;
        }
        let Some(position) = positions.offset_to_position(&document.source, argument_span.end)
        else {
            continue;
        };
        if position < range.start || position > range.end {
            continue;
        }
        let label = format!("{name}:");
        push_hint(
            hints,
            unique,
            label_bytes,
            position,
            label,
            InlayHintKind::PARAMETER,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn collect_boolean_constant_hint(
    document: &Document,
    declaration: Node<'_>,
    range: Range,
    positions: &PositionIndex,
    cancel: &AtomicBool,
    hints: &mut Vec<InlayHint>,
    unique: &mut HashSet<(Position, String)>,
    label_bytes: &mut usize,
) -> Result<(), String> {
    if declaration.child_by_field_name("type").is_some() {
        return Ok(());
    }
    let Some(initializer) = declaration.child_by_field_name("defaultValue") else {
        return Ok(());
    };
    check_cancel(cancel)?;
    // `defaultValue` is the grammar's declaration-initializer wrapper; do not
    // unwrap arbitrary expression nodes (e.g. unary operators or parentheses).
    let mut cursor = initializer.walk();
    let children = initializer
        .named_children(&mut cursor)
        .filter(|child| child.kind() != "kEq")
        .collect::<Vec<_>>();
    if children.len() != 1 || !matches!(children[0].kind(), "kTrue" | "kFalse") {
        return Ok(());
    }
    let Some(name) = declaration.child_by_field_name("name") else {
        return Ok(());
    };
    let Some(position) = positions.offset_to_position(&document.source, name.end_byte()) else {
        return Ok(());
    };
    if position < range.start || position > range.end {
        return Ok(());
    }
    push_hint(
        hints,
        unique,
        label_bytes,
        position,
        ": Boolean".to_owned(),
        InlayHintKind::TYPE,
    )
}

fn push_hint(
    hints: &mut Vec<InlayHint>,
    unique: &mut HashSet<(Position, String)>,
    label_bytes: &mut usize,
    position: Position,
    label: String,
    kind: InlayHintKind,
) -> Result<(), String> {
    if hints.len() >= MAX_INLAY_HINTS {
        return Err(format!(
            "inlay hint output exceeds the {MAX_INLAY_HINTS}-hint limit"
        ));
    }
    *label_bytes = label_bytes.saturating_add(label.len());
    if *label_bytes > MAX_INLAY_LABEL_BYTES {
        return Err(format!(
            "inlay hint labels exceed the {MAX_INLAY_LABEL_BYTES}-byte limit"
        ));
    }
    if unique.insert((position, label.clone())) {
        hints.push(InlayHint {
            position,
            label: InlayHintLabel::String(label),
            kind: Some(kind),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: None,
        });
    }
    Ok(())
}

fn exclusion_spans(document: &Document, cancel: &AtomicBool) -> Result<Vec<Span>, String> {
    let count = document
        .parser_recovery_spans
        .len()
        .saturating_add(document.conditionals.inactive_spans.len())
        .saturating_add(document.conditionals.unknown_spans.len());
    if count > MAX_INLAY_EXCLUSIONS {
        return Err(format!(
            "inlay conditional/recovery exclusions exceed the {MAX_INLAY_EXCLUSIONS}-span limit"
        ));
    }
    let mut spans = Vec::with_capacity(count);
    for span in &document.parser_recovery_spans {
        check_cancel(cancel)?;
        spans.push(*span);
    }
    for span in document
        .conditionals
        .inactive_spans
        .iter()
        .chain(&document.conditionals.unknown_spans)
    {
        check_cancel(cancel)?;
        spans.push(Span {
            start: span.start,
            end: span.end,
        });
    }
    spans.sort_unstable_by_key(|span| (span.start, span.end));
    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        check_cancel(cancel)?;
        if let Some(previous) = merged.last_mut() {
            if span.start <= previous.end {
                previous.end = previous.end.max(span.end);
                continue;
            }
        }
        merged.push(span);
    }
    Ok(merged)
}

fn excluded(excluded_spans: &[Span], span: Span) -> bool {
    let index = excluded_spans.partition_point(|other| other.end <= span.start);
    excluded_spans
        .get(index)
        .is_some_and(|other| overlaps(span, *other))
}

fn overlaps(left: Span, right: Span) -> bool {
    left.start < right.end && right.start < left.end
}
fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("request cancelled".to_owned())
    } else {
        Ok(())
    }
}
fn label_text(label: &InlayHintLabel) -> &str {
    match label {
        InlayHintLabel::String(value) => value,
        InlayHintLabel::LabelParts(_) => "",
    }
}
