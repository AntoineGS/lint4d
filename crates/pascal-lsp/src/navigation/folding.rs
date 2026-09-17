use super::{Document, NavigationIndex, Span};
use crate::conditional::Truth;
use crate::text::PositionIndex;
use lsp_types::{FoldingRange, FoldingRangeKind, Url};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use tree_sitter::Node;

pub(crate) const FOLDING_KIND_COMMENT: u8 = 1;
pub(crate) const FOLDING_KIND_IMPORTS: u8 = 1 << 1;
pub(crate) const FOLDING_KIND_REGION: u8 = 1 << 2;

const MAX_FOLDING_TRAVERSAL_NODES: usize = 100_000;
const MAX_FOLDING_CANDIDATES: usize = 50_000;
const MAX_FOLDING_DEPTH: usize = 128;
const MAX_REGION_DEPTH: usize = 128;
const CANCELLATION_MESSAGE: &str = "request cancelled";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FoldingRangeOptions {
    pub(crate) range_limit: Option<usize>,
    pub(crate) line_folding_only: bool,
    /// `None` means the client did not advertise a value set. `Some(0)` means
    /// it advertised an empty value set and therefore accepts no tagged kind.
    pub(crate) kind_value_set: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateCategory {
    Section,
    Routine,
    Type,
    Declaration,
    Block,
    Comment,
    Region,
    Imports,
}

impl CandidateCategory {
    fn selection_rank(self) -> u8 {
        match self {
            Self::Section | Self::Routine | Self::Type => 0,
            Self::Declaration | Self::Comment | Self::Region | Self::Imports => 1,
            Self::Block => 2,
        }
    }

    fn kind(self) -> Option<FoldingRangeKind> {
        match self {
            Self::Comment => Some(FoldingRangeKind::Comment),
            Self::Region => Some(FoldingRangeKind::Region),
            Self::Imports => Some(FoldingRangeKind::Imports),
            Self::Section | Self::Routine | Self::Type | Self::Declaration | Self::Block => None,
        }
    }

    fn kind_bit(self) -> Option<u8> {
        match self {
            Self::Comment => Some(FOLDING_KIND_COMMENT),
            Self::Region => Some(FOLDING_KIND_REGION),
            Self::Imports => Some(FOLDING_KIND_IMPORTS),
            Self::Section | Self::Routine | Self::Type | Self::Declaration | Self::Block => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    span: Span,
    category: CandidateCategory,
    source_order: usize,
}

#[derive(Debug, Clone)]
struct VisibleRange {
    candidate: Candidate,
    range: FoldingRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct RangeKey {
    start_line: u32,
    start_character: Option<u32>,
    end_line: u32,
    end_character: Option<u32>,
}

pub(super) fn folding_ranges(
    index: &NavigationIndex,
    uri: &Url,
) -> Result<Vec<FoldingRange>, String> {
    let cancel = AtomicBool::new(false);
    folding_ranges_with_cancel(index, uri, FoldingRangeOptions::default(), &cancel)
}

pub(super) fn folding_ranges_with_cancel(
    index: &NavigationIndex,
    uri: &Url,
    options: FoldingRangeOptions,
    cancel: &AtomicBool,
) -> Result<Vec<FoldingRange>, String> {
    check_cancel(cancel)?;
    let document = index
        .documents
        .get(uri)
        .ok_or_else(|| format!("document is not indexed: {uri}"))?;
    if options.range_limit == Some(0) {
        return Ok(Vec::new());
    }

    let candidates = collect_candidates(document, cancel)?;
    let positions = PositionIndex::new_with_cancel(&document.source, cancel)
        .map_err(|()| CANCELLATION_MESSAGE.to_string())?;
    let mut ranges = visible_ranges(document, &positions, candidates, options, cancel)?;
    select_ranges(&mut ranges, options.range_limit);
    ranges.sort_by(compare_output_ranges);
    Ok(ranges.into_iter().map(|visible| visible.range).collect())
}

fn collect_candidates(document: &Document, cancel: &AtomicBool) -> Result<Vec<Candidate>, String> {
    let mut candidates = Vec::new();
    let mut comments = Vec::new();
    let mut pending = vec![(document.tree.root_node(), 0_usize)];
    let mut visited = 0_usize;
    let mut source_order = 0_usize;

    while let Some((node, depth)) = pending.pop() {
        check_cancel(cancel)?;
        visited = visited.saturating_add(1);
        if visited > MAX_FOLDING_TRAVERSAL_NODES {
            return Err(format!(
                "folding syntax traversal exceeds the {MAX_FOLDING_TRAVERSAL_NODES}-node limit"
            ));
        }
        if depth > MAX_FOLDING_DEPTH {
            return Err(format!(
                "folding syntax hierarchy exceeds the {MAX_FOLDING_DEPTH}-level depth limit"
            ));
        }

        if node.kind() == "comment" {
            comments.push(Span::from_node(node));
        } else if let Some(category) = category_for_node(node) {
            push_candidate(
                &mut candidates,
                Candidate {
                    span: Span::from_node(node),
                    category,
                    source_order,
                },
            )?;
            source_order = source_order.saturating_add(1);
        }

        let mut cursor = node.walk();
        let children = node.named_children(&mut cursor).collect::<Vec<_>>();
        pending.extend(children.into_iter().rev().map(|child| (child, depth + 1)));
    }

    collect_comment_candidates(
        document.source.as_ref(),
        comments,
        &mut candidates,
        &mut source_order,
    )?;
    collect_region_candidates(document, &mut candidates, &mut source_order, cancel)?;
    Ok(candidates)
}

fn category_for_node(node: Node<'_>) -> Option<CandidateCategory> {
    match node.kind() {
        "interface" | "implementation" | "initialization" | "finalization" => {
            Some(CandidateCategory::Section)
        }
        "declTypes" | "declVars" | "declConsts" | "declLabels" | "declExports" => {
            Some(CandidateCategory::Section)
        }
        "defProc" => Some(CandidateCategory::Routine),
        "declClass" | "declIntf" | "declHelper" | "declVariant" => Some(CandidateCategory::Type),
        "declType" => {
            let type_node = node.child_by_field_name("type");
            (!type_node.is_some_and(|type_node| {
                matches!(type_node.kind(), "declClass" | "declIntf" | "declHelper")
            }))
            .then_some(CandidateCategory::Type)
        }
        "declProc" | "declProp" => Some(CandidateCategory::Declaration),
        "declUses" => Some(CandidateCategory::Imports),
        "if" | "ifElse" | "while" | "repeat" | "for" | "foreach" | "try" | "case" | "block"
        | "with" | "asm" => Some(CandidateCategory::Block),
        _ => None,
    }
}

fn push_candidate(candidates: &mut Vec<Candidate>, candidate: Candidate) -> Result<(), String> {
    if candidates.len() >= MAX_FOLDING_CANDIDATES {
        return Err(format!(
            "folding candidate count exceeds the {MAX_FOLDING_CANDIDATES}-range limit"
        ));
    }
    candidates.push(candidate);
    Ok(())
}

fn collect_comment_candidates(
    source: &str,
    mut comments: Vec<Span>,
    candidates: &mut Vec<Candidate>,
    source_order: &mut usize,
) -> Result<(), String> {
    comments.sort_by_key(|span| (span.start, span.end));
    let mut line_group = None;
    for span in comments {
        let is_line_comment = source
            .get(span.start..span.end)
            .is_some_and(|comment| comment.starts_with("//"));
        if !is_line_comment {
            flush_line_comment_group(candidates, &mut line_group, source_order)?;
            if source
                .get(span.start..span.end)
                .is_some_and(|comment| comment.contains(['\n', '\r']))
            {
                push_candidate(
                    candidates,
                    Candidate {
                        span,
                        category: CandidateCategory::Comment,
                        source_order: *source_order,
                    },
                )?;
                *source_order = source_order.saturating_add(1);
            }
            continue;
        }

        let can_extend = line_group
            .as_ref()
            .is_some_and(|current| are_adjacent_comment_lines(source, *current, span));
        if can_extend {
            line_group.as_mut().expect("line comment group").end = span.end;
        } else {
            flush_line_comment_group(candidates, &mut line_group, source_order)?;
            line_group = Some(span);
        }
    }
    flush_line_comment_group(candidates, &mut line_group, source_order)
}

fn flush_line_comment_group(
    candidates: &mut Vec<Candidate>,
    group: &mut Option<Span>,
    source_order: &mut usize,
) -> Result<(), String> {
    let Some(span) = group.take() else {
        return Ok(());
    };
    push_candidate(
        candidates,
        Candidate {
            span,
            category: CandidateCategory::Comment,
            source_order: *source_order,
        },
    )?;
    *source_order = source_order.saturating_add(1);
    Ok(())
}

fn are_adjacent_comment_lines(source: &str, previous: Span, next: Span) -> bool {
    if previous.end > next.start {
        return false;
    }
    let Some(between) = source.get(previous.end..next.start) else {
        return false;
    };
    between.chars().all(char::is_whitespace) && line_break_count(between) == 1
}

fn line_break_count(source: &str) -> usize {
    let bytes = source.as_bytes();
    let mut count: usize = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                count = count.saturating_add(1);
                if bytes.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
            }
            b'\n' => count = count.saturating_add(1),
            _ => {}
        }
        index += 1;
    }
    count
}

fn collect_region_candidates(
    document: &Document,
    candidates: &mut Vec<Candidate>,
    source_order: &mut usize,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut open_regions = Vec::new();
    for directive in &document.conditionals.directives {
        check_cancel(cancel)?;
        if directive.activity != Truth::True {
            continue;
        }
        let keyword = directive
            .body
            .trim_start()
            .split(|character: char| character.is_ascii_whitespace() || character == ':')
            .next()
            .unwrap_or_default();
        if keyword.eq_ignore_ascii_case("region") {
            if open_regions.len() >= MAX_REGION_DEPTH {
                return Err(format!(
                    "folding region nesting exceeds the {MAX_REGION_DEPTH}-level depth limit"
                ));
            }
            open_regions.push(Span {
                start: directive.start,
                end: directive.end,
            });
        } else if keyword.eq_ignore_ascii_case("endregion") {
            if let Some(open) = open_regions.pop() {
                push_candidate(
                    candidates,
                    Candidate {
                        span: Span {
                            start: open.start,
                            end: directive.end,
                        },
                        category: CandidateCategory::Region,
                        source_order: *source_order,
                    },
                )?;
                *source_order = source_order.saturating_add(1);
            }
        }
    }
    Ok(())
}

fn visible_ranges(
    document: &Document,
    positions: &PositionIndex,
    candidates: Vec<Candidate>,
    options: FoldingRangeOptions,
    cancel: &AtomicBool,
) -> Result<Vec<VisibleRange>, String> {
    let mut unique: HashMap<RangeKey, VisibleRange> = HashMap::new();
    for candidate in candidates {
        check_cancel(cancel)?;
        if !safe_span(document, candidate.span) {
            continue;
        }
        if let Some(mask) = options.kind_value_set {
            if candidate
                .category
                .kind_bit()
                .is_some_and(|kind| mask & kind == 0)
            {
                continue;
            }
        }
        let Some(start) = positions.offset_to_position(&document.source, candidate.span.start)
        else {
            continue;
        };
        let Some(end) = positions.offset_to_position(&document.source, candidate.span.end) else {
            continue;
        };
        if start.line >= end.line {
            continue;
        }
        let (start_character, end_character, end_line) = if options.line_folding_only {
            let end_line = line_only_end_line(document.source.as_ref(), candidate.span, end.line);
            (None, None, end_line)
        } else {
            (Some(start.character), Some(end.character), end.line)
        };
        if start.line >= end_line {
            continue;
        }
        let range = FoldingRange {
            start_line: start.line,
            start_character,
            end_line,
            end_character,
            kind: candidate.category.kind(),
            collapsed_text: None,
        };
        let key = RangeKey {
            start_line: range.start_line,
            start_character: range.start_character,
            end_line: range.end_line,
            end_character: range.end_character,
        };
        let visible = VisibleRange { candidate, range };
        match unique.get(&key).cloned() {
            Some(previous) if !prefer_candidate(visible.candidate, previous.candidate) => {}
            _ => {
                unique.insert(key, visible);
            }
        }
    }
    Ok(unique.into_values().collect())
}

fn safe_span(document: &Document, span: Span) -> bool {
    if span.start >= span.end || span.end > document.source.len() {
        return false;
    }
    if document
        .parser_recovery_spans
        .iter()
        .any(|excluded| spans_overlap(span, *excluded))
        || document.conditionals.inactive_spans.iter().any(|excluded| {
            spans_overlap(
                span,
                Span {
                    start: excluded.start,
                    end: excluded.end,
                },
            )
        })
        || document.conditionals.unknown_spans.iter().any(|excluded| {
            spans_overlap(
                span,
                Span {
                    start: excluded.start,
                    end: excluded.end,
                },
            )
        })
    {
        return false;
    }
    !document
        .opaque_ranges
        .iter()
        .any(|excluded| spans_overlap(span, *excluded))
}

fn spans_overlap(left: Span, right: Span) -> bool {
    if right.start == right.end {
        return left.start <= right.start && right.start <= left.end;
    }
    left.start < right.end && right.start < left.end
}

fn line_only_end_line(source: &str, span: Span, end_line: u32) -> u32 {
    let after_end = source
        .get(span.end..)
        .and_then(|suffix| suffix.split(['\r', '\n']).next())
        .unwrap_or_default();
    if after_end
        .chars()
        .any(|character| !character.is_whitespace())
    {
        end_line.saturating_sub(1)
    } else {
        end_line
    }
}

fn prefer_candidate(left: Candidate, right: Candidate) -> bool {
    (
        left.category.selection_rank(),
        left.span.start,
        left.source_order,
    ) < (
        right.category.selection_rank(),
        right.span.start,
        right.source_order,
    )
}

fn select_ranges(ranges: &mut Vec<VisibleRange>, range_limit: Option<usize>) {
    let Some(limit) = range_limit else {
        return;
    };
    ranges.sort_by(|left, right| {
        left.candidate
            .category
            .selection_rank()
            .cmp(&right.candidate.category.selection_rank())
            .then_with(|| {
                let left_length = left.range.end_line.saturating_sub(left.range.start_line);
                let right_length = right.range.end_line.saturating_sub(right.range.start_line);
                right_length.cmp(&left_length)
            })
            .then_with(|| left.range.start_line.cmp(&right.range.start_line))
            .then_with(|| right.range.end_line.cmp(&left.range.end_line))
            .then_with(|| {
                left.candidate
                    .source_order
                    .cmp(&right.candidate.source_order)
            })
    });
    ranges.truncate(limit);
}

fn compare_output_ranges(left: &VisibleRange, right: &VisibleRange) -> std::cmp::Ordering {
    left.range
        .start_line
        .cmp(&right.range.start_line)
        .then_with(|| {
            left.range
                .start_character
                .unwrap_or_default()
                .cmp(&right.range.start_character.unwrap_or_default())
        })
        .then_with(|| right.range.end_line.cmp(&left.range.end_line))
        .then_with(|| {
            left.range
                .end_character
                .unwrap_or_default()
                .cmp(&right.range.end_character.unwrap_or_default())
        })
        .then_with(|| {
            left.candidate
                .source_order
                .cmp(&right.candidate.source_order)
        })
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(CANCELLATION_MESSAGE.to_string())
    } else {
        Ok(())
    }
}
