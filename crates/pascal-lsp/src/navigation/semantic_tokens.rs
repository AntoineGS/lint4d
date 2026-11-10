use super::TreeParent as _;
use super::{
    AssistanceBudget, BudgetedLookup, Document, GenericParameterContext, NavigationIndex, Origin,
    OwnerTypeContext, SemanticTokenResolutionMode, Span, Symbol, SymbolKind, TypeKind,
    canonical_name, check_navigation_cancel, collect_nodes, field_identifier_nodes, node_text,
};
use crate::text::{self, PositionIndex};
use lsp_types::{
    Position, Range, SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens,
    SemanticTokensLegend,
};
#[cfg(test)]
use std::cell::Cell;
use std::cmp::{max, min};
use std::collections::HashSet;
use std::ops::Range as ByteRange;
use std::sync::atomic::AtomicBool;
use tree_sitter::Node;

const TOKEN_TYPES: &[&str] = &[
    "namespace",
    "type",
    "class",
    "enum",
    "interface",
    "struct",
    "typeParameter",
    "parameter",
    "variable",
    "property",
    "enumMember",
    "function",
    "method",
    "keyword",
    "modifier",
    "comment",
    "string",
    "number",
    "operator",
];

const TOKEN_MODIFIERS: &[&str] = &["declaration", "definition", "readonly", "static"];

const TOKEN_NAMESPACE: u32 = 0;
const TOKEN_TYPE: u32 = 1;
const TOKEN_CLASS: u32 = 2;
const TOKEN_ENUM: u32 = 3;
const TOKEN_INTERFACE: u32 = 4;
const TOKEN_STRUCT: u32 = 5;
const TOKEN_TYPE_PARAMETER: u32 = 6;
const TOKEN_PARAMETER: u32 = 7;
const TOKEN_VARIABLE: u32 = 8;
const TOKEN_PROPERTY: u32 = 9;
const TOKEN_ENUM_MEMBER: u32 = 10;
const TOKEN_FUNCTION: u32 = 11;
const TOKEN_METHOD: u32 = 12;
const TOKEN_KEYWORD: u32 = 13;
const TOKEN_COMMENT: u32 = 15;
const TOKEN_STRING: u32 = 16;
const TOKEN_NUMBER: u32 = 17;
const TOKEN_OPERATOR: u32 = 18;

const MODIFIER_DECLARATION: u32 = 1 << 0;
const MODIFIER_DEFINITION: u32 = 1 << 1;
const MODIFIER_READONLY: u32 = 1 << 2;
const MODIFIER_STATIC: u32 = 1 << 3;

/// Syntax nodes one response visits; past it the rest of the document gets
/// no tokens. Sized for the largest real units (a 180,000-line, 8 MB unit).
const MAX_SEMANTIC_NODES: usize = 16_000_000;
/// One identifier's resolution, shared with hover. An identifier that needs
/// more is left plain.
pub(super) const MAX_SEMANTIC_RESOLUTION_WORK: usize = 512_000;
pub(super) const MAX_SEMANTIC_RESOLUTION_BYTES: usize = 8 * 1024 * 1024;
/// All identifiers of one response; past it the remaining identifiers are
/// left plain instead of the response being refused. The largest real unit
/// measured (8 MB, 214,000 resolved identifiers) needs about 144M units and
/// 1.3 GB of scanned bytes (l4d-3ia4).
const MAX_SEMANTIC_RESPONSE_WORK: usize = 256 * 1024 * 1024;
/// Cumulative scanned/proved bytes, a work proxy, not memory: what one
/// identifier materializes stays under `MAX_SEMANTIC_RESOLUTION_BYTES` and is
/// dropped before the next.
const MAX_SEMANTIC_RESPONSE_BYTES: usize = if usize::BITS >= 64 {
    (4u64 * 1024 * 1024 * 1024) as usize
} else {
    usize::MAX
};

#[derive(Debug, Clone, Copy)]
struct SemanticTokenLimits {
    nodes: usize,
    identifier_work: usize,
    identifier_bytes: usize,
    total_work: usize,
    total_bytes: usize,
}

const SEMANTIC_TOKEN_LIMITS: SemanticTokenLimits = SemanticTokenLimits {
    nodes: MAX_SEMANTIC_NODES,
    identifier_work: MAX_SEMANTIC_RESOLUTION_WORK,
    identifier_bytes: MAX_SEMANTIC_RESOLUTION_BYTES,
    total_work: MAX_SEMANTIC_RESPONSE_WORK,
    total_bytes: MAX_SEMANTIC_RESPONSE_BYTES,
};

fn semantic_token_limits() -> SemanticTokenLimits {
    #[cfg(test)]
    if let Some(limits) = SEMANTIC_TOKEN_LIMITS_OVERRIDE.with(Cell::get) {
        return limits;
    }
    SEMANTIC_TOKEN_LIMITS
}

#[cfg(test)]
struct SemanticTokenLimitsGuard(Option<SemanticTokenLimits>);

#[cfg(test)]
fn override_semantic_token_limits(limits: SemanticTokenLimits) -> SemanticTokenLimitsGuard {
    SemanticTokenLimitsGuard(SEMANTIC_TOKEN_LIMITS_OVERRIDE.with(|cell| cell.replace(Some(limits))))
}

#[cfg(test)]
impl Drop for SemanticTokenLimitsGuard {
    fn drop(&mut self) {
        SEMANTIC_TOKEN_LIMITS_OVERRIDE.with(|cell| cell.set(self.0.take()));
    }
}

#[cfg(test)]
fn record_semantic_resolution_work(work: usize) {
    SEMANTIC_RESOLUTION_WORK_USED.with(|used| used.set(used.get().saturating_add(work)));
}

#[cfg(test)]
fn take_semantic_resolution_work() -> usize {
    SEMANTIC_RESOLUTION_WORK_USED.with(|used| used.replace(0))
}

#[cfg(test)]
thread_local! {
    static SEMANTIC_TOKEN_LIMITS_OVERRIDE: Cell<Option<SemanticTokenLimits>> =
        const { Cell::new(None) };
    static SEMANTIC_RESOLUTION_WORK_USED: Cell<usize> = const { Cell::new(0) };
    static LAST_SEMANTIC_TOKEN_DEGRADATION: Cell<SemanticTokenDegradation> =
        const { Cell::new(SemanticTokenDegradation { plain_identifiers: 0, node_bound_reached: false }) };
    static SPAN_QUERY_COMPARISONS: Cell<usize> = const { Cell::new(0) };
    static GENERIC_PARAMETER_NAME_COMPARISONS: Cell<usize> = const { Cell::new(0) };
    static SEMANTIC_TOKEN_CANCEL_AFTER_CHECKS: Cell<Option<usize>> = const { Cell::new(None) };
}

#[cfg(test)]
struct SemanticTokenCancellationGuard(Option<usize>);

#[cfg(test)]
fn cancel_semantic_tokens_after_checks(checks: usize) -> SemanticTokenCancellationGuard {
    let previous = SEMANTIC_TOKEN_CANCEL_AFTER_CHECKS.with(|remaining| {
        let previous = remaining.get();
        remaining.set(Some(checks));
        previous
    });
    SemanticTokenCancellationGuard(previous)
}

#[cfg(test)]
impl Drop for SemanticTokenCancellationGuard {
    fn drop(&mut self) {
        SEMANTIC_TOKEN_CANCEL_AFTER_CHECKS.with(|remaining| remaining.set(self.0.take()));
    }
}

fn check_semantic_token_cancel(cancel: &AtomicBool) -> Result<(), String> {
    #[cfg(test)]
    let force_cancel = SEMANTIC_TOKEN_CANCEL_AFTER_CHECKS.with(|remaining| match remaining.get() {
        Some(0) => {
            remaining.set(None);
            true
        }
        Some(count) => {
            remaining.set(Some(count.saturating_sub(1)));
            false
        }
        None => false,
    });
    #[cfg(test)]
    if force_cancel {
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    check_navigation_cancel(cancel)
}

#[derive(Debug, Clone, Copy)]
struct RawToken {
    span: Span,
    token_type: u32,
    modifiers: u32,
    priority: u8,
}

#[derive(Debug, Clone, Copy)]
struct AbsoluteToken {
    position: Position,
    length: u32,
    token_type: u32,
    modifiers: u32,
}

#[derive(Debug)]
struct SpanIndex {
    spans: Vec<Span>,
    prefix_max_end: Vec<usize>,
}

impl SpanIndex {
    fn new(mut spans: Vec<Span>) -> Self {
        spans.sort_unstable_by_key(|span| (span.start, span.end));
        let mut prefix_max_end = Vec::with_capacity(spans.len());
        let mut max_end = 0;
        for span in &spans {
            max_end = max(max_end, span.end);
            prefix_max_end.push(max_end);
        }
        Self {
            spans,
            prefix_max_end,
        }
    }

    fn overlaps(&self, span: Span) -> bool {
        if span.start >= span.end {
            return false;
        }
        let end = self.lower_bound_start(span.end);
        end > 0 && self.prefix_max_end[end - 1] > span.start
    }

    fn contains(&self, span: Span) -> bool {
        let end = self.upper_bound_start(span.start);
        end > 0 && self.prefix_max_end[end - 1] >= span.end
    }

    fn contains_offset(&self, offset: usize) -> bool {
        let end = self.upper_bound_start(offset);
        end > 0 && self.prefix_max_end[end - 1] > offset
    }

    fn upper_bound_start(&self, start: usize) -> usize {
        let mut low = 0;
        let mut high = self.spans.len();
        while low < high {
            let middle = low + (high - low) / 2;
            #[cfg(test)]
            SPAN_QUERY_COMPARISONS.with(|comparisons| {
                comparisons.set(comparisons.get().saturating_add(1));
            });
            if self.spans[middle].start <= start {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    fn lower_bound_start(&self, start: usize) -> usize {
        let mut low = 0;
        let mut high = self.spans.len();
        while low < high {
            let middle = low + (high - low) / 2;
            #[cfg(test)]
            SPAN_QUERY_COMPARISONS.with(|comparisons| {
                comparisons.set(comparisons.get().saturating_add(1));
            });
            if self.spans[middle].start < start {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }
}

pub(crate) fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES
            .iter()
            .map(|token_type| SemanticTokenType::from(*token_type))
            .collect(),
        token_modifiers: TOKEN_MODIFIERS
            .iter()
            .map(|modifier| SemanticTokenModifier::from(*modifier))
            .collect(),
    }
}

#[cfg(test)]
pub(crate) fn semantic_tokens(
    index: &NavigationIndex,
    uri: &lsp_types::Url,
    range: Option<&Range>,
    cancel: &AtomicBool,
) -> Result<SemanticTokens, String> {
    semantic_tokens_with_mode(index, uri, range, cancel, SemanticTokenResolutionMode::Full)
}

pub(crate) fn semantic_tokens_with_mode(
    index: &NavigationIndex,
    uri: &lsp_types::Url,
    range: Option<&Range>,
    cancel: &AtomicBool,
    mode: SemanticTokenResolutionMode,
) -> Result<SemanticTokens, String> {
    semantic_tokens_with_projection(index, uri, range, cancel, mode, None)
}

/// Maps byte spans of an include-expanded document onto the physical file the
/// client requested tokens for. Spans from included files map to nothing.
pub(crate) struct PhysicalProjection<'a> {
    pub(crate) source: &'a str,
    pub(crate) map: &'a mut dyn FnMut(ByteRange<usize>) -> Result<Vec<ByteRange<usize>>, String>,
}

pub(crate) fn semantic_tokens_with_projection(
    index: &NavigationIndex,
    uri: &lsp_types::Url,
    range: Option<&Range>,
    cancel: &AtomicBool,
    mode: SemanticTokenResolutionMode,
    mut projection: Option<PhysicalProjection<'_>>,
) -> Result<SemanticTokens, String> {
    check_semantic_token_cancel(cancel)?;
    let Some(document) = index.documents.get(uri) else {
        return Ok(SemanticTokens::default());
    };
    let encoded_source = projection
        .as_ref()
        .map_or(&*document.source, |projection| projection.source);

    let range = if let Some(range) = range {
        let Some(start) = text::position_to_offset_clamped(encoded_source, range.start) else {
            return Ok(SemanticTokens::default());
        };
        let Some(end) = text::position_to_offset_clamped(encoded_source, range.end) else {
            return Ok(SemanticTokens::default());
        };
        if start >= end {
            return Ok(SemanticTokens::default());
        }
        Some((start, end))
    } else {
        None
    };
    // A physical range has no single virtual counterpart, so projected
    // documents are traversed whole and clipped after mapping.
    let traversal_range = if projection.is_some() { None } else { range };

    let mut raw_tokens = Vec::new();
    // Each pending node carries whether a `with` or `inherited` node is
    // among its ancestors, so identifiers need no parent walk to know it.
    let mut pending = vec![(document.tree.root_node(), false)];
    let parser_recovery = SpanIndex::new(document.parser_recovery_spans.clone());
    let conditional_unknown = SpanIndex::new(
        document
            .conditionals
            .unknown_spans
            .iter()
            .map(|span| Span {
                start: span.start,
                end: span.end,
            })
            .collect(),
    );
    let opaque_ranges = SpanIndex::new(document.opaque_ranges.clone());
    let mut visited = 0usize;
    let limits = semantic_token_limits();
    let mut response = ResponseBudget {
        work: limits.total_work,
        bytes: limits.total_bytes,
        plain_identifiers: 0,
        node_bound_reached: false,
    };

    while let Some((node, under_with_or_inherited)) = pending.pop() {
        check_semantic_token_cancel(cancel)?;
        #[cfg(test)]
        super::test_record_semantic_node_visit();
        if traversal_range
            .is_some_and(|(start, end)| node.end_byte() <= start || node.start_byte() >= end)
        {
            continue;
        }
        visited = visited.saturating_add(1);
        if visited > limits.nodes {
            response.node_bound_reached = true;
            break;
        }

        let span = Span::from_node(node);
        if let Some(token_type) = lexical_token_type(node.kind()) {
            push_raw_token(&mut raw_tokens, span, token_type, 0, 0, &document.source);
        }

        if mode == SemanticTokenResolutionMode::Full
            && node.kind() == "identifier"
            && !conditional_unknown.contains_offset(node.start_byte())
            && !parser_recovery.overlaps(span)
            && !opaque_ranges.contains(span)
            && let Some((token_type, modifiers)) = bounded_identifier_type(
                index,
                uri,
                document,
                node,
                under_with_or_inherited,
                cancel,
                limits,
                &mut response,
            )?
        {
            push_raw_token(
                &mut raw_tokens,
                span,
                token_type,
                modifiers,
                1,
                &document.source,
            );
        }

        let children_under_with_or_inherited =
            under_with_or_inherited || matches!(node.kind(), "with" | "inherited");
        let start_len = pending.len();
        if let Some((start, end)) = traversal_range {
            pending.extend(
                children_overlapping_range(node, start, end)
                    .into_iter()
                    .map(|child| (child, children_under_with_or_inherited)),
            );
        } else {
            let mut cursor = node.walk();
            pending.extend(
                node.children(&mut cursor)
                    .map(|child| (child, children_under_with_or_inherited)),
            );
        }
        // Reversed so nodes pop in source order: a response cut short by a
        // bound keeps the start of the document.
        pending[start_len..].reverse();
    }

    response.report_degradation(uri, limits);

    if let Some(projection) = projection.as_mut() {
        raw_tokens = project_raw_tokens(raw_tokens, projection, cancel)?;
    }
    encode_tokens(encoded_source, raw_tokens, range, cancel)
}

/// What is left of the response-wide resolution bound, and how the response
/// was cut short.
struct ResponseBudget {
    work: usize,
    bytes: usize,
    /// Identifiers left plain because a resolution bound ran out.
    plain_identifiers: usize,
    /// Whether the traversal stopped at the node bound.
    node_bound_reached: bool,
}

/// How the last semantic-token response on this thread was cut short.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SemanticTokenDegradation {
    plain_identifiers: usize,
    node_bound_reached: bool,
}

#[cfg(test)]
fn take_semantic_token_degradation() -> SemanticTokenDegradation {
    LAST_SEMANTIC_TOKEN_DEGRADATION.with(|last| last.replace(SemanticTokenDegradation::default()))
}

impl ResponseBudget {
    /// Logs a response cut short by a bound, so a document partly left plain
    /// can be explained from the server log.
    fn report_degradation(&self, uri: &lsp_types::Url, limits: SemanticTokenLimits) {
        #[cfg(test)]
        LAST_SEMANTIC_TOKEN_DEGRADATION.with(|last| {
            last.set(SemanticTokenDegradation {
                plain_identifiers: self.plain_identifiers,
                node_bound_reached: self.node_bound_reached,
            });
        });
        if self.node_bound_reached {
            eprintln!(
                "pascal-lsp: warning: semantic tokens for {uri} stop at the {}-node bound; the rest of the document has none",
                limits.nodes
            );
        }
        if self.plain_identifiers > 0 {
            eprintln!(
                "pascal-lsp: warning: semantic tokens for {uri} leave {} identifiers plain: their resolution exceeded the per-identifier or per-response bound",
                self.plain_identifiers
            );
        }
    }
}

/// Resolves one identifier under its own budget, charged to the response.
/// Read-only features answer best effort (D16): an identifier whose
/// resolution runs out of either bound is left plain rather than the whole
/// response being refused.
#[allow(clippy::too_many_arguments)]
fn bounded_identifier_type(
    index: &NavigationIndex,
    uri: &lsp_types::Url,
    document: &Document,
    identifier: Node<'_>,
    under_with_or_inherited: bool,
    cancel: &AtomicBool,
    limits: SemanticTokenLimits,
    response: &mut ResponseBudget,
) -> Result<Option<(u32, u32)>, String> {
    let work = min(limits.identifier_work, response.work);
    let bytes = min(limits.identifier_bytes, response.bytes);
    let mut budget = AssistanceBudget::new(work, bytes, "semantic token resolution");
    let resolved = resolved_identifier_type(
        index,
        uri,
        document,
        identifier,
        under_with_or_inherited,
        cancel,
        &mut budget,
    );
    let used_work = work.saturating_sub(budget.remaining_work);
    response.work = response.work.saturating_sub(used_work);
    response.bytes = response
        .bytes
        .saturating_sub(bytes.saturating_sub(budget.remaining_bytes));
    #[cfg(test)]
    record_semantic_resolution_work(used_work);
    match resolved {
        Ok(classification) if !budget.exhausted() => Ok(classification),
        Ok(_) => {
            response.plain_identifiers += 1;
            Ok(None)
        }
        Err(error) if budget.exhausted_by(&error, cancel) => {
            response.plain_identifiers += 1;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn project_raw_tokens(
    raw_tokens: Vec<RawToken>,
    projection: &mut PhysicalProjection<'_>,
    cancel: &AtomicBool,
) -> Result<Vec<RawToken>, String> {
    let mut projected = Vec::with_capacity(raw_tokens.len());
    for token in raw_tokens {
        check_semantic_token_cancel(cancel)?;
        for physical in (projection.map)(token.span.start..token.span.end)? {
            push_raw_token(
                &mut projected,
                Span {
                    start: physical.start,
                    end: physical.end,
                },
                token.token_type,
                token.modifiers,
                token.priority,
                projection.source,
            );
        }
    }
    Ok(projected)
}

fn lexical_token_type(kind: &str) -> Option<u32> {
    match kind {
        "comment" => Some(TOKEN_COMMENT),
        "literalString" | "literalChar" => Some(TOKEN_STRING),
        "literalNumber" => Some(TOKEN_NUMBER),
        "ppIf" | "ppElse" | "ppEndIf" | "ppDirective" => Some(TOKEN_KEYWORD),
        "kDot" | "kAdd" | "kSub" | "kMul" | "kFdiv" | "kAssign" | "kAssignAdd" | "kAssignSub"
        | "kAssignMul" | "kAssignDiv" | "kEq" | "kLt" | "kLte" | "kGt" | "kGte" | "kNeq"
        | "kAt" | "kHat" | "kOr" | "kXor" | "kDiv" | "kMod" | "kAnd" | "kShl" | "kShr" | "kNot"
        | "kIs" | "kAs" | "kIn" => Some(TOKEN_OPERATOR),
        "kEndDot" => None,
        kind if kind.starts_with('k') => Some(TOKEN_KEYWORD),
        _ => None,
    }
}

fn resolved_identifier_type(
    index: &NavigationIndex,
    uri: &lsp_types::Url,
    document: &Document,
    identifier: Node<'_>,
    under_with_or_inherited: bool,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<(u32, u32)>, String> {
    if is_generic_parameter_declaration(identifier) {
        return Ok(Some((TOKEN_TYPE_PARAMETER, MODIFIER_DECLARATION)));
    }
    let span = Span::from_node(identifier);
    if let Some(direct_indices) = document.direct_symbol_indices.get(&span) {
        let mut classification = None;
        for symbol_index in direct_indices {
            if document
                .conditional_unknown_symbols
                .get(*symbol_index)
                .copied()
                .unwrap_or(true)
            {
                return Ok(None);
            }
            let Some(symbol) = document.symbols.get(*symbol_index) else {
                return Ok(None);
            };
            let Some(candidate_classification) = symbol_classification(symbol) else {
                return Ok(None);
            };
            if classification.is_some_and(|existing| existing != candidate_classification) {
                return Ok(None);
            }
            classification = Some(candidate_classification);
        }
        return Ok(classification);
    }
    if under_with_or_inherited {
        return Ok(None);
    }
    match is_generic_parameter_reference(document, identifier, cancel, budget)? {
        Some(true) => return Ok(Some((TOKEN_TYPE_PARAMETER, 0))),
        None => return Ok(None),
        Some(false) => {}
    }
    let candidates = index.resolve_candidates_at_with_state_and_budget(
        uri,
        document,
        identifier.start_byte(),
        identifier,
        &mut super::ResolutionState::new(),
        0,
        cancel,
        budget,
    )?;
    if candidates.is_empty()
        || candidates
            .iter()
            .any(|candidate| index.candidate_is_conditionally_unknown(candidate))
    {
        return Ok(None);
    }

    let mut classification = None;
    for candidate in candidates {
        let Some(symbol) = index.symbol(&candidate) else {
            return Ok(None);
        };
        let Some(candidate_classification) = symbol_classification(symbol) else {
            return Ok(None);
        };
        let candidate_classification = (
            candidate_classification.0,
            candidate_classification.1 & (MODIFIER_READONLY | MODIFIER_STATIC),
        );
        if classification.is_some_and(|existing| existing != candidate_classification) {
            return Ok(None);
        }
        classification = Some(candidate_classification);
    }
    Ok(classification)
}

fn symbol_classification(symbol: &Symbol) -> Option<(u32, u32)> {
    let token_type = match symbol.kind {
        SymbolKind::Unit => TOKEN_NAMESPACE,
        SymbolKind::Type => match symbol.type_kind {
            TypeKind::Class => TOKEN_CLASS,
            TypeKind::Enum => TOKEN_ENUM,
            TypeKind::Interface => TOKEN_INTERFACE,
            TypeKind::Record => TOKEN_STRUCT,
            _ => TOKEN_TYPE,
        },
        SymbolKind::Routine => {
            if symbol.owner_type.is_some() {
                TOKEN_METHOD
            } else {
                TOKEN_FUNCTION
            }
        }
        SymbolKind::Parameter => TOKEN_PARAMETER,
        SymbolKind::Variable | SymbolKind::Field | SymbolKind::Label => TOKEN_VARIABLE,
        SymbolKind::Constant => TOKEN_VARIABLE,
        SymbolKind::Property => TOKEN_PROPERTY,
        SymbolKind::EnumValue => TOKEN_ENUM_MEMBER,
    };

    let mut modifiers = match symbol.origin {
        Origin::Declaration => MODIFIER_DECLARATION,
        Origin::Definition => MODIFIER_DEFINITION,
    };
    if matches!(symbol.kind, SymbolKind::Constant | SymbolKind::EnumValue) {
        modifiers |= MODIFIER_READONLY;
    }
    if symbol.is_static {
        modifiers |= MODIFIER_STATIC;
    }
    Some((token_type, modifiers))
}

fn is_generic_parameter_declaration(identifier: Node<'_>) -> bool {
    let Some(parent) = identifier.tree_parent() else {
        return false;
    };
    parent.kind() == "genericArg"
        && parent
            .child_by_field_name("name")
            .is_some_and(|name| Span::from_node(name) == Span::from_node(identifier))
}

pub(super) fn generic_parameter_contexts(
    root: Node<'_>,
    source: &str,
) -> Vec<GenericParameterContext> {
    let mut contexts: Vec<GenericParameterContext> = Vec::new();
    collect_nodes(root, &mut |node| {
        let (declaration, span) = match node.kind() {
            "declType" | "declProc" => (Some(node), Span::from_node(node)),
            "defProc" => (node.child_by_field_name("header"), Span::from_node(node)),
            _ => return,
        };
        let Some(declaration) = declaration else {
            return;
        };
        let Some(name) = declaration.child_by_field_name("name") else {
            return;
        };
        if name.kind() != "genericTpl" {
            return;
        }
        let Some(arguments) = name.child_by_field_name("args") else {
            return;
        };
        let mut names = HashSet::new();
        let mut cursor = arguments.walk();
        for argument in arguments
            .children(&mut cursor)
            .filter(|node| node.kind() == "genericArg")
        {
            for identifier in field_identifier_nodes(argument, "name") {
                #[cfg(test)]
                GENERIC_PARAMETER_NAME_COMPARISONS.with(|comparisons| {
                    comparisons.set(comparisons.get().saturating_add(1));
                });
                names.insert(canonical_name(&node_text(identifier, source)));
            }
        }
        if !names.is_empty() {
            contexts.push(GenericParameterContext {
                span,
                names,
                parent: None,
            });
        }
    });

    let mut order = (0..contexts.len()).collect::<Vec<_>>();
    order.sort_unstable_by_key(|index| {
        (
            contexts[*index].span.start,
            std::cmp::Reverse(contexts[*index].span.end),
        )
    });
    let mut open: Vec<usize> = Vec::new();
    for index in order {
        while let Some(parent) = open.last().copied() {
            if contexts[parent].span.contains(contexts[index].span)
                && contexts[parent].span != contexts[index].span
            {
                break;
            }
            open.pop();
        }
        contexts[index].parent = open.last().copied();
        open.push(index);
    }
    contexts
}

pub(super) fn owner_type_contexts(root: Node<'_>, source: &str) -> Vec<OwnerTypeContext> {
    let mut contexts: Vec<OwnerTypeContext> = Vec::new();
    collect_nodes(root, &mut |node| {
        if node.kind() != "declType" {
            return;
        }
        let Some(type_node) = node.child_by_field_name("type") else {
            return;
        };
        let Some(owner_type) = field_identifier_nodes(node, "name")
            .last()
            .map(|identifier| canonical_name(&node_text(*identifier, source)))
        else {
            return;
        };
        if owner_type.is_empty() {
            return;
        }
        contexts.push(OwnerTypeContext {
            span: Span::from_node(type_node),
            owner_type,
        });
    });
    contexts
}

fn is_generic_parameter_reference(
    document: &Document,
    identifier: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<bool>, String> {
    match is_right_hand_qualified_identifier_with_budget(identifier, cancel, budget)? {
        Some(true) => return Ok(Some(false)),
        None => return Ok(None),
        Some(false) => {}
    }
    let span = Span::from_node(identifier);
    if !budget.take_bytes(span.end.saturating_sub(span.start), cancel)? {
        return Ok(None);
    }
    let key = canonical_name(&node_text(identifier, &document.source));
    let context = match document
        .generic_parameter_intervals
        .at_with_budget_or_unknown(identifier.start_byte(), cancel, budget)?
    {
        BudgetedLookup::Value(context) => context,
        BudgetedLookup::Exhausted => return Ok(None),
    };
    let Some(mut context) = context else {
        return Ok(Some(false));
    };

    loop {
        #[cfg(test)]
        super::TEST_SEMANTIC_GENERIC_CONTEXT_CHECKS.with(|value| {
            value.set(value.get().saturating_add(1));
        });
        if !budget.take_work(1, cancel)? {
            return Ok(None);
        }
        let generic_context = &document.generic_parameter_contexts[context];
        if generic_context.names.contains(&key) {
            let scope = match document.scope_at_with_budget_or_unknown(
                identifier.start_byte(),
                cancel,
                budget,
            )? {
                BudgetedLookup::Value(scope) => scope,
                BudgetedLookup::Exhausted => return Ok(None),
            };
            let owner_type = match document.owner_type_at_identifier_with_budget_or_unknown(
                identifier, scope, cancel, budget,
            )? {
                BudgetedLookup::Value(owner_type) => owner_type,
                BudgetedLookup::Exhausted => return Ok(None),
            };
            return Ok(
                has_nearer_binding(document, scope, &key, owner_type, cancel, budget)?
                    .map(|nearer| !nearer),
            );
        }
        let Some(parent) = generic_context.parent else {
            return Ok(Some(false));
        };
        context = parent;
    }
}

fn is_right_hand_qualified_identifier_with_budget(
    identifier: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<bool>, String> {
    let target = Span::from_node(identifier);
    let mut current = identifier.tree_parent();
    while let Some(node) = current {
        if !budget.take_work(1, cancel)? {
            return Ok(None);
        }
        if matches!(node.kind(), "exprDot" | "genericDot" | "typerefDot")
            && node
                .child_by_field_name("rhs")
                .is_some_and(|rhs| Span::from_node(rhs).contains(target))
        {
            return Ok(Some(true));
        }
        current = node.tree_parent();
    }
    Ok(Some(false))
}

fn has_nearer_binding(
    document: &Document,
    scope: usize,
    key: &str,
    owner_type: Option<&str>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<bool>, String> {
    if !budget.take_work(1, cancel)? {
        return Ok(None);
    }
    #[cfg(test)]
    super::TEST_SEMANTIC_SHADOW_CHECKS.with(|value| {
        value.set(value.get().saturating_add(1));
    });
    if document
        .scope_binding_keys
        .get(scope)
        .is_some_and(|keys| keys.contains(key))
    {
        return Ok(Some(true));
    }

    let Some(owner_type) = owner_type else {
        return Ok(Some(false));
    };
    if !budget.take_work(1, cancel)? {
        return Ok(None);
    }
    #[cfg(test)]
    super::TEST_SEMANTIC_SHADOW_CHECKS.with(|value| {
        value.set(value.get().saturating_add(1));
    });
    Ok(Some(
        document
            .member_binding_keys_by_owner
            .get(owner_type)
            .is_some_and(|keys| keys.contains(key)),
    ))
}

fn push_raw_token(
    tokens: &mut Vec<RawToken>,
    span: Span,
    token_type: u32,
    modifiers: u32,
    priority: u8,
    source: &str,
) {
    if span.start < span.end
        && span.end <= source.len()
        && source.is_char_boundary(span.start)
        && source.is_char_boundary(span.end)
    {
        tokens.push(RawToken {
            span,
            token_type,
            modifiers,
            priority,
        });
    }
}

fn encode_tokens(
    source: &str,
    mut raw_tokens: Vec<RawToken>,
    range: Option<(usize, usize)>,
    cancel: &AtomicBool,
) -> Result<SemanticTokens, String> {
    raw_tokens.sort_by(|left, right| {
        left.span
            .start
            .cmp(&right.span.start)
            .then_with(|| right.priority.cmp(&left.priority))
            .then_with(|| left.span.end.cmp(&right.span.end))
            .then_with(|| left.token_type.cmp(&right.token_type))
    });

    let mut non_overlapping = Vec::with_capacity(raw_tokens.len());
    let mut last_end = 0usize;
    for token in raw_tokens {
        check_semantic_token_cancel(cancel)?;
        if token.span.start < last_end {
            continue;
        }
        last_end = token.span.end;
        non_overlapping.push(token);
    }

    let positions = PositionIndex::new_with_cancel(source, cancel)
        .map_err(|()| "request cancelled".to_string())?;
    let mut absolute = Vec::new();
    for token in non_overlapping {
        check_semantic_token_cancel(cancel)?;
        let (start, end) = if let Some((range_start, range_end)) = range {
            (
                max(token.span.start, range_start),
                min(token.span.end, range_end),
            )
        } else {
            (token.span.start, token.span.end)
        };
        if start >= end {
            continue;
        }
        for (segment_start, segment_end) in split_lines(source, start, end) {
            check_semantic_token_cancel(cancel)?;
            let Some(position) = positions.offset_to_position(source, segment_start) else {
                continue;
            };
            let Some(length) = source
                .get(segment_start..segment_end)
                .and_then(|text| u32::try_from(text.encode_utf16().count()).ok())
            else {
                continue;
            };
            if length == 0 {
                continue;
            }
            absolute.push(AbsoluteToken {
                position,
                length,
                token_type: token.token_type,
                modifiers: token.modifiers,
            });
        }
    }

    absolute.sort_by(|left, right| {
        left.position
            .line
            .cmp(&right.position.line)
            .then_with(|| left.position.character.cmp(&right.position.character))
            .then_with(|| left.length.cmp(&right.length))
            .then_with(|| left.token_type.cmp(&right.token_type))
    });
    absolute.dedup_by(|left, right| {
        left.position == right.position
            && left.length == right.length
            && left.token_type == right.token_type
            && left.modifiers == right.modifiers
    });

    let mut data = Vec::with_capacity(absolute.len());
    let mut previous_line = 0;
    let mut previous_character = 0;
    for token in absolute {
        let delta_line = token.position.line.saturating_sub(previous_line);
        let delta_start = if delta_line == 0 {
            token.position.character.saturating_sub(previous_character)
        } else {
            token.position.character
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length: token.length,
            token_type: token.token_type,
            token_modifiers_bitset: token.modifiers,
        });
        previous_line = token.position.line;
        previous_character = token.position.character;
    }

    Ok(SemanticTokens {
        result_id: None,
        data,
    })
}

fn split_lines(source: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut segments = Vec::new();
    let mut cursor = start;
    let bytes = source.as_bytes();
    while cursor < end {
        let mut line_end = cursor;
        while line_end < end && !matches!(bytes[line_end], b'\r' | b'\n') {
            line_end += 1;
        }
        if cursor < line_end {
            segments.push((cursor, line_end));
        }
        if line_end == end {
            break;
        }
        cursor = line_end
            + usize::from(
                bytes[line_end] == b'\r' && line_end + 1 < end && bytes[line_end + 1] == b'\n',
            );
        cursor += 1;
    }
    segments
}

fn children_overlapping_range(node: Node<'_>, start: usize, end: usize) -> Vec<Node<'_>> {
    let mut children = Vec::new();
    let mut current = node.first_child_for_byte(start);
    while let Some(child) = current {
        if child.start_byte() >= end {
            break;
        }
        if child.end_byte() > start {
            children.push(child);
        }
        current = child.next_sibling();
    }
    children
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Url;

    fn absolute_tokens(tokens: &SemanticTokens) -> Vec<(Position, u32, u32, u32)> {
        let mut line = 0;
        let mut character = 0;
        tokens
            .data
            .iter()
            .map(|token| {
                line += token.delta_line;
                character = if token.delta_line == 0 {
                    character + token.delta_start
                } else {
                    token.delta_start
                };
                (
                    Position::new(line, character),
                    token.length,
                    token.token_type,
                    token.token_modifiers_bitset,
                )
            })
            .collect()
    }

    #[test]
    fn semantic_tokens_from_index_supports_range_queries() {
        let uri = Url::parse("file:///RangeTokens.pas").expect("test URI");
        let source = "unit RangeTokens;\ninterface\nprocedure Run;\nimplementation\nprocedure Run;\nvar\n  Value: Integer;\nbegin\n  Value := 42;\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_string())
            .expect("index source");
        let cancel = AtomicBool::new(false);
        let full = semantic_tokens(&index, &uri, None, &cancel).expect("full tokens");
        let range = semantic_tokens(
            &index,
            &uri,
            Some(&Range {
                start: Position::new(8, 3),
                end: Position::new(8, 7),
            }),
            &cancel,
        )
        .expect("range tokens");
        assert!(!full.data.is_empty());
        assert_eq!(
            range.data,
            vec![SemanticToken {
                delta_line: 8,
                delta_start: 3,
                length: 4,
                token_type: TOKEN_VARIABLE,
                token_modifiers_bitset: 0,
            }]
        );
    }

    #[test]
    fn semantic_tokens_return_empty_results_for_missing_documents_and_empty_ranges() {
        let uri = Url::parse("file:///EmptyTokens.pas").expect("test URI");
        let missing_uri = Url::parse("file:///MissingTokens.pas").expect("missing URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), "unit EmptyTokens;\nend.\n".to_string())
            .expect("index source");
        let cancel = AtomicBool::new(false);

        assert_eq!(
            semantic_tokens(&index, &missing_uri, None, &cancel).expect("missing document"),
            SemanticTokens::default()
        );
        assert_eq!(
            semantic_tokens(
                &index,
                &uri,
                Some(&Range {
                    start: Position::new(1, 0),
                    end: Position::new(1, 0),
                }),
                &cancel,
            )
            .expect("empty range"),
            SemanticTokens::default()
        );
        assert_eq!(
            semantic_tokens(
                &index,
                &uri,
                Some(&Range {
                    start: Position::new(99, 0),
                    end: Position::new(100, 0),
                }),
                &cancel,
            )
            .expect("out-of-bounds range"),
            SemanticTokens::default()
        );
    }

    #[test]
    fn encoding_splits_crlf_and_multiline_spans_using_utf16_positions() {
        let source = "😀abc\r\nxy\n";
        let cancel = AtomicBool::new(false);
        let tokens = encode_tokens(
            source,
            vec![
                RawToken {
                    span: Span { start: 0, end: 4 },
                    token_type: TOKEN_NUMBER,
                    modifiers: 0,
                    priority: 0,
                },
                RawToken {
                    span: Span { start: 4, end: 7 },
                    token_type: TOKEN_STRING,
                    modifiers: 0,
                    priority: 0,
                },
                RawToken {
                    span: Span { start: 9, end: 12 },
                    token_type: TOKEN_COMMENT,
                    modifiers: 0,
                    priority: 0,
                },
            ],
            None,
            &cancel,
        )
        .expect("encode tokens");
        assert_eq!(
            tokens.data,
            vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 2,
                    token_type: TOKEN_NUMBER,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 2,
                    length: 3,
                    token_type: TOKEN_STRING,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 2,
                    token_type: TOKEN_COMMENT,
                    token_modifiers_bitset: 0,
                },
            ]
        );
    }

    #[test]
    fn encoding_clips_to_a_range_and_prefers_semantic_tokens_on_overlap() {
        let source = "abcdef";
        let cancel = AtomicBool::new(false);
        let tokens = encode_tokens(
            source,
            vec![
                RawToken {
                    span: Span { start: 0, end: 6 },
                    token_type: TOKEN_COMMENT,
                    modifiers: 0,
                    priority: 0,
                },
                RawToken {
                    span: Span { start: 0, end: 6 },
                    token_type: TOKEN_VARIABLE,
                    modifiers: 0,
                    priority: 1,
                },
            ],
            Some((2, 4)),
            &cancel,
        )
        .expect("encode tokens");
        assert_eq!(
            tokens.data,
            vec![SemanticToken {
                delta_line: 0,
                delta_start: 2,
                length: 2,
                token_type: TOKEN_VARIABLE,
                token_modifiers_bitset: 0,
            }]
        );
    }

    #[test]
    fn semantic_tokens_stop_before_work_when_cancelled() {
        let uri = Url::parse("file:///Cancelled.pas").expect("test URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), "unit Cancelled;\nend.\n".to_string())
            .expect("index source");
        let cancel = AtomicBool::new(true);
        assert_eq!(
            semantic_tokens(&index, &uri, None, &cancel),
            Err("request cancelled".to_string())
        );
    }

    #[test]
    fn semantic_tokens_honor_cancellation_during_collection_and_encoding() {
        let uri = Url::parse("file:///CancelledDuringTokens.pas").expect("test URI");
        let source = "unit CancelledDuringTokens;\ninterface\nconst Value = 1;\nimplementation\nprocedure Run;\nbegin\n  Value := 2;\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_string())
            .expect("index source");
        let cancel = AtomicBool::new(false);
        let _guard = cancel_semantic_tokens_after_checks(8);

        assert_eq!(
            semantic_tokens(&index, &uri, None, &cancel),
            Err("request cancelled".to_string())
        );
        assert!(cancel.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn span_index_queries_are_logarithmic_for_large_interval_sets() {
        let count = 65_536usize;
        let spans = (0..count)
            .map(|index| Span {
                start: index * 4,
                end: index * 4 + 1,
            })
            .collect();
        let index = SpanIndex::new(spans);
        SPAN_QUERY_COMPARISONS.with(|comparisons| comparisons.set(0));

        for value in 0..count {
            let start = value * 4;
            assert!(index.overlaps(Span {
                start,
                end: start + 1,
            }));
            assert!(index.contains(Span {
                start,
                end: start + 1,
            }));
            assert!(index.contains_offset(start));
        }

        let comparisons = SPAN_QUERY_COMPARISONS.with(Cell::get);
        assert!(
            comparisons <= count * 52,
            "indexed interval queries performed {comparisons} comparisons for {count} spans"
        );
    }

    #[test]
    fn span_index_does_not_treat_touching_intervals_as_overlapping() {
        let index = SpanIndex::new(vec![Span { start: 10, end: 20 }]);

        assert!(!index.overlaps(Span { start: 0, end: 10 }));
        assert!(!index.overlaps(Span { start: 20, end: 30 }));
        assert!(index.overlaps(Span { start: 19, end: 21 }));
    }

    #[test]
    fn semantic_tokens_keep_interval_query_work_bounded_for_large_mixed_sources() {
        use std::fmt::Write as _;

        const DECLARATION_COUNT: usize = 65_536;
        let uri = Url::parse("file:///LargeMixedTokens.pas").expect("test URI");
        let mut source = String::from("unit LargeMixedTokens;\ninterface\nconst\n");
        for index in 0..DECLARATION_COUNT {
            writeln!(source, "  Broken{index} = ;").expect("append malformed declaration");
        }
        source.push_str("const\n");
        for index in 0..DECLARATION_COUNT {
            writeln!(source, "  Valid{index} = 1;").expect("append valid declaration");
        }
        source.push_str("implementation\nend.\n");

        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source)
            .expect("index mixed source");
        SPAN_QUERY_COMPARISONS.with(|comparisons| comparisons.set(0));
        let cancel = AtomicBool::new(false);
        semantic_tokens(&index, &uri, None, &cancel).expect("semantic tokens");

        let comparisons = SPAN_QUERY_COMPARISONS.with(Cell::get);
        assert!(
            comparisons < 12_000_000,
            "interval query work grew beyond the bounded indexed budget: {comparisons}"
        );
    }

    #[test]
    fn semantic_tokens_bound_generic_parameter_lookup_work_per_scope() {
        use std::fmt::Write as _;

        const PARAMETER_COUNT: usize = 4_000;
        let uri = Url::parse("file:///GenericWork.pas").expect("test URI");
        let mut source = String::from("unit GenericWork;\ninterface\ntype\n  TBox<");
        for index in 0..PARAMETER_COUNT {
            if index > 0 {
                source.push_str(", ");
            }
            write!(source, "P{index}").expect("append generic parameter");
        }
        source.push_str("> = class\n");
        for index in 0..PARAMETER_COUNT {
            writeln!(source, "    F{index}: P{index};").expect("append field");
        }
        source.push_str("  end;\nimplementation\nend.\n");

        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.clone())
            .expect("index generic source");
        let cancel = AtomicBool::new(false);

        super::super::test_reset_semantic_token_work_counters();
        semantic_tokens(&index, &uri, None, &cancel).expect("semantic tokens");

        let full_work = super::super::test_semantic_token_work_counters();
        assert_eq!(
            full_work.0, 0,
            "generic resolution must not scan every scope: {full_work:?}"
        );
        assert_eq!(
            full_work.1, 0,
            "generic resolution must not rebuild a scope chain per reference: {full_work:?}"
        );
        assert!(
            full_work.3 <= PARAMETER_COUNT * 64,
            "generic owner/header traversal grew beyond the linear bound: {full_work:?}"
        );
        assert!(
            full_work.4 <= PARAMETER_COUNT * 8,
            "generic context checks grew beyond the linear bound: {full_work:?}"
        );
        assert!(
            full_work.5 <= PARAMETER_COUNT * 32,
            "semantic-token node visits grew beyond the linear bound: {full_work:?}"
        );
        assert!(
            full_work.6 <= PARAMETER_COUNT * 32,
            "indexed scope/context query work grew beyond the linear bound: {full_work:?}"
        );
        assert!(
            full_work.7 <= PARAMETER_COUNT * 4,
            "nearer-binding checks grew beyond the linear bound: {full_work:?}"
        );

        let tokens = semantic_tokens(&index, &uri, None, &cancel).expect("semantic tokens");
        let tokens = absolute_tokens(&tokens);
        assert!(
            tokens.iter().any(|(position, _, token_type, _)| {
                *position == Position::new(4, 8) && *token_type == TOKEN_TYPE_PARAMETER
            }),
            "the first field type must remain a generic parameter"
        );
        assert!(
            tokens.iter().any(|(position, _, token_type, _)| {
                *position == Position::new(4 + PARAMETER_COUNT as u32 - 1, 11)
                    && *token_type == TOKEN_TYPE_PARAMETER
            }),
            "the last field type must remain a generic parameter"
        );

        super::super::test_reset_semantic_token_work_counters();
        semantic_tokens(
            &index,
            &uri,
            Some(&Range {
                start: Position::new(4, 0),
                end: Position::new(4, 100),
            }),
            &cancel,
        )
        .expect("range semantic tokens");
        let range_work = super::super::test_semantic_token_work_counters();
        assert_eq!(
            range_work.3, 0,
            "a range-pruned query must not rescan the generic parameter list: {range_work:?}"
        );
        assert!(
            range_work.5 <= 32,
            "a range-pruned query must visit only the selected syntax context: {range_work:?}"
        );
        assert!(
            range_work.6 <= 16 && range_work.7 <= 4,
            "a range-pruned query must keep indexed resolution work bounded: {range_work:?}"
        );
    }

    #[test]
    fn semantic_generic_resolution_returns_unknown_before_unbudgeted_owner_work() {
        let uri = Url::parse("file:///BudgetedGenericTokens.pas").expect("test URI");
        let source = "unit BudgetedGenericTokens;\ninterface\ntype\n  TBox<T> = class\n    Value: T;\n  end;\nimplementation\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_string())
            .expect("index generic source");
        let document = index.documents.get(&uri).expect("indexed document");
        let offset = source.find("T;").expect("generic reference");
        let identifier = super::super::identifier_at(document.tree.root_node(), offset)
            .expect("generic reference identifier");
        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(0, MAX_SEMANTIC_RESOLUTION_BYTES, "semantic tokens");

        super::super::test_reset_semantic_token_work_counters();
        assert_eq!(
            is_generic_parameter_reference(document, identifier, &cancel, &mut budget),
            Ok(None)
        );
        let work = super::super::test_semantic_token_work_counters();
        assert_eq!(
            work.3, 0,
            "budget exhaustion must happen before any owner/header traversal: {work:?}"
        );
    }

    #[test]
    fn semantic_tokens_clamp_range_endpoints_beyond_line_content() {
        let uri = Url::parse("file:///ClampTokens.pas").expect("test URI");
        let source = "unit ClampTokens;\r\ninterface\r\nconst Value = 1;\r\nend.\r\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_string())
            .expect("index source");
        let cancel = AtomicBool::new(false);

        let tokens = semantic_tokens(
            &index,
            &uri,
            Some(&Range {
                start: Position::new(2, 0),
                end: Position::new(2, 10_000),
            }),
            &cancel,
        )
        .expect("clamped range tokens");

        assert!(!tokens.data.is_empty());
    }

    #[test]
    fn semantic_tokens_omit_unsupported_with_and_inherited_bindings() {
        let uri = Url::parse("file:///UnsupportedTokens.pas").expect("test URI");
        let source = "unit UnsupportedTokens;\ninterface\ntype\n  TRecord = record\n    Value: Integer;\n  end;\n  TRunner = class\n    procedure Run;\n  end;\nconst\n  Value = 1;\nimplementation\nprocedure TRunner.Run;\nvar\n  R: TRecord;\nbegin\n  with R do\n    Value := 42;\n  inherited Run;\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_string())
            .expect("index source");
        let cancel = AtomicBool::new(false);
        let tokens = semantic_tokens(&index, &uri, None, &cancel).expect("semantic tokens");
        let tokens = absolute_tokens(&tokens);

        assert!(
            !tokens.iter().any(|(position, length, _, _)| {
                *position == Position::new(17, 4) && *length == 5
            }),
            "with member references must not fall back to a global binding"
        );
        assert!(
            !tokens.iter().any(|(position, length, _, _)| {
                *position == Position::new(18, 12) && *length == 3
            }),
            "inherited references must not be resolved as ordinary members"
        );
    }

    #[test]
    fn semantic_tokens_resolve_generic_parameter_references_lexically() {
        let uri = Url::parse("file:///GenericTokens.pas").expect("test URI");
        let source = "unit GenericTokens;\ninterface\ntype\n  T = class\n  end;\n  TBox<T> = class\n    Value: T;\n  end;\nimplementation\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_string())
            .expect("index source");
        let cancel = AtomicBool::new(false);
        let tokens = semantic_tokens(&index, &uri, None, &cancel).expect("semantic tokens");
        let tokens = absolute_tokens(&tokens);

        assert!(
            tokens.iter().any(|(position, length, token_type, _)| {
                *position == Position::new(6, 11)
                    && *length == 1
                    && *token_type == TOKEN_TYPE_PARAMETER
            }),
            "a generic parameter reference must not resolve to a same-named global class"
        );
    }

    #[test]
    fn semantic_tokens_do_not_treat_qualified_types_as_generic_parameters() {
        let provider_uri = Url::parse("file:///Provider.pas").expect("provider URI");
        let provider_source =
            "unit Provider;\ninterface\ntype\n  T = class\n  end;\nimplementation\nend.\n";
        let uri = Url::parse("file:///GenericQualified.pas").expect("test URI");
        let source = "unit GenericQualified;\ninterface\nuses Provider;\ntype\n  T = class\n  end;\n  TBox<T> = class\n    Value: T;\n    Qualified: Provider.T;\n  end;\nimplementation\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(provider_uri, provider_source.to_string())
            .expect("index provider source");
        index
            .update(uri.clone(), source.to_string())
            .expect("index generic source");
        let cancel = AtomicBool::new(false);
        let tokens = semantic_tokens(&index, &uri, None, &cancel).expect("semantic tokens");
        let tokens = absolute_tokens(&tokens);

        assert!(
            tokens.iter().any(|(position, length, token_type, _)| {
                *position == Position::new(8, 24) && *length == 1 && *token_type == TOKEN_CLASS
            }),
            "the final identifier in Provider.T must resolve as the qualified provider type"
        );
        assert!(!tokens.iter().any(|(position, length, token_type, _)| {
            *position == Position::new(8, 24) && *length == 1 && *token_type == TOKEN_TYPE_PARAMETER
        }));
    }

    #[test]
    fn semantic_tokens_prioritize_a_nearer_local_binding_over_a_generic_parameter() {
        let uri = Url::parse("file:///GenericShadow.pas").expect("test URI");
        let source = "unit GenericShadow;\ninterface\ntype\n  TBox<T> = class\n    procedure Run;\n  end;\nimplementation\nprocedure TBox<T>.Run;\nvar\n  T: Integer;\nbegin\n  T := 1;\nend;\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_string())
            .expect("index generic source");
        let cancel = AtomicBool::new(false);
        let tokens = semantic_tokens(&index, &uri, None, &cancel).expect("semantic tokens");
        let tokens = absolute_tokens(&tokens);

        assert!(
            tokens.iter().any(|(position, length, token_type, _)| {
                *position == Position::new(11, 2) && *length == 1 && *token_type == TOKEN_VARIABLE
            }),
            "a local T binding must take precedence over the enclosing generic parameter"
        );
        assert!(!tokens.iter().any(|(position, length, token_type, _)| {
            *position == Position::new(11, 2) && *length == 1 && *token_type == TOKEN_TYPE_PARAMETER
        }));
    }

    /// A unit with `constants` global constants and a routine whose body is
    /// `lines` lines of `X := X + 1;`, and the line of the first body line.
    fn identifier_heavy_unit(lines: usize, constants: usize) -> (String, u32) {
        use std::fmt::Write as _;

        let mut source = String::from("unit Heavy;\ninterface\nconst\n  C = 0;\n");
        for index in 0..constants {
            writeln!(source, "  C{index} = {index};").expect("append constant");
        }
        source.push_str("implementation\nprocedure Run;\nvar\n  X: Integer;\nbegin\n");
        let first_body_line = u32::try_from(source.matches('\n').count()).expect("line count");
        source.push_str(&"  X := X + 1;\n".repeat(lines));
        source.push_str("end;\nend.\n");
        (source, first_body_line)
    }

    fn body_tokens(tokens: &SemanticTokens, body: std::ops::Range<u32>, token_type: u32) -> usize {
        absolute_tokens(tokens)
            .iter()
            .filter(|(position, _, kind, _)| body.contains(&position.line) && *kind == token_type)
            .count()
    }

    #[test]
    fn semantic_tokens_resolve_identifier_heavy_units_with_linear_work() {
        const LINES: u32 = 4_200;
        let (source, first) = identifier_heavy_unit(LINES as usize, 2_000);
        let uri = Url::parse("file:///Heavy.pas").expect("test URI");
        let mut index = NavigationIndex::new();
        index.update(uri.clone(), source).expect("index source");
        let cancel = AtomicBool::new(false);
        let references = 2 * LINES as usize;

        take_semantic_resolution_work();
        let full = semantic_tokens(&index, &uri, None, &cancel)
            .expect("an identifier-heavy unit must get semantic tokens");
        let full_work = take_semantic_resolution_work();
        assert_eq!(
            take_semantic_token_degradation(),
            SemanticTokenDegradation::default(),
            "nothing is cut short"
        );
        assert_eq!(
            body_tokens(&full, first..first + LINES, TOKEN_VARIABLE),
            references,
            "every reference to X must be classified"
        );
        assert!(
            full_work <= references * 64,
            "resolution work must not grow with the unit's symbol count: {full_work} for {references} references"
        );
        assert!(
            full_work.saturating_mul(4) <= SEMANTIC_TOKEN_LIMITS.total_work,
            "the response bound must leave room for units many times larger: {full_work}"
        );

        let range = semantic_tokens(
            &index,
            &uri,
            Some(&Range {
                start: Position::new(first + 2_000, 0),
                end: Position::new(first + 2_100, 0),
            }),
            &cancel,
        )
        .expect("range tokens");
        let range_work = take_semantic_resolution_work();
        assert_eq!(
            body_tokens(&range, first + 2_000..first + 2_100, TOKEN_VARIABLE),
            200
        );
        assert!(
            range_work <= 200 * 64,
            "a range request must pay only for its range: {range_work}"
        );
    }

    #[test]
    fn semantic_tokens_degrade_instead_of_refusing_when_the_response_bound_runs_out() {
        const LINES: u32 = 200;
        let (source, first) = identifier_heavy_unit(LINES as usize, 0);
        let uri = Url::parse("file:///Degraded.pas").expect("test URI");
        let mut index = NavigationIndex::new();
        index.update(uri.clone(), source).expect("index source");
        let cancel = AtomicBool::new(false);
        let body = first..first + LINES;

        let _limits = override_semantic_token_limits(SemanticTokenLimits {
            total_work: 2_000,
            ..SEMANTIC_TOKEN_LIMITS
        });
        let tokens = semantic_tokens(&index, &uri, None, &cancel)
            .expect("an exhausted response bound must degrade, not refuse");
        let classified = body_tokens(&tokens, body.clone(), TOKEN_VARIABLE);
        assert!(
            classified > 0 && classified < 2 * LINES as usize,
            "references resolved before the bound keep their class, the rest stay plain: {classified}"
        );
        assert_eq!(
            take_semantic_token_degradation(),
            SemanticTokenDegradation {
                plain_identifiers: 2 * LINES as usize - classified,
                node_bound_reached: false,
            },
            "the response reports every reference it left plain"
        );
        assert_eq!(
            body_tokens(&tokens, body.clone(), TOKEN_OPERATOR),
            2 * LINES as usize,
            "lexical tokens are still returned for the whole document"
        );
        let plain = absolute_tokens(&tokens);
        let first_unclassified = (first..first + LINES)
            .find(|line| {
                !plain
                    .iter()
                    .any(|(position, _, kind, _)| position.line == *line && *kind == TOKEN_VARIABLE)
            })
            .expect("some line is left plain");
        assert!(
            (first_unclassified..first + LINES).all(|line| !plain
                .iter()
                .any(|(position, _, kind, _)| position.line == line && *kind == TOKEN_VARIABLE)),
            "resolution proceeds in source order, so the classified prefix is contiguous"
        );
    }

    #[test]
    fn semantic_tokens_leave_identifiers_plain_when_their_own_bound_runs_out() {
        const LINES: u32 = 20;
        let (source, first) = identifier_heavy_unit(LINES as usize, 0);
        let uri = Url::parse("file:///IdentifierBound.pas").expect("test URI");
        let mut index = NavigationIndex::new();
        index.update(uri.clone(), source).expect("index source");
        let cancel = AtomicBool::new(false);

        let _limits = override_semantic_token_limits(SemanticTokenLimits {
            identifier_work: 1,
            ..SEMANTIC_TOKEN_LIMITS
        });
        let tokens = semantic_tokens(&index, &uri, None, &cancel)
            .expect("an identifier over its own bound must not refuse the response");
        assert_eq!(
            body_tokens(&tokens, first..first + LINES, TOKEN_VARIABLE),
            0,
            "an identifier whose resolution needs more than its bound stays plain"
        );
        let degradation = take_semantic_token_degradation();
        assert!(
            degradation.plain_identifiers >= 2 * LINES as usize && !degradation.node_bound_reached,
            "{degradation:?}"
        );
        assert_eq!(
            body_tokens(&tokens, first..first + LINES, TOKEN_OPERATOR),
            2 * LINES as usize
        );
        assert!(
            absolute_tokens(&tokens)
                .iter()
                .any(|(position, _, kind, modifiers)| position.line == first - 2
                    && *kind == TOKEN_VARIABLE
                    && modifiers & MODIFIER_DECLARATION != 0),
            "declarations need no resolution and keep their class"
        );
    }

    #[test]
    fn semantic_tokens_return_the_leading_tokens_when_the_node_bound_runs_out() {
        let (source, _) = identifier_heavy_unit(200, 0);
        let uri = Url::parse("file:///NodeBound.pas").expect("test URI");
        let mut index = NavigationIndex::new();
        index.update(uri.clone(), source).expect("index source");
        let cancel = AtomicBool::new(false);

        let _limits = override_semantic_token_limits(SemanticTokenLimits {
            nodes: 64,
            ..SEMANTIC_TOKEN_LIMITS
        });
        let tokens = semantic_tokens(&index, &uri, None, &cancel)
            .expect("an exhausted node bound must degrade, not refuse");
        assert!(take_semantic_token_degradation().node_bound_reached);
        let tokens = absolute_tokens(&tokens);
        assert!(
            tokens
                .first()
                .is_some_and(|(position, _, kind, _)| *position == Position::new(0, 0)
                    && *kind == TOKEN_KEYWORD),
            "the traversal covers the document from its start: {tokens:?}"
        );
        assert!(tokens.len() < 64, "the traversal stopped at the bound");
    }
}
