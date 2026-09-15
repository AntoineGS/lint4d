use super::{
    AssistanceBudget, Candidate, Document, NavigationIndex, Origin, Region, RoutineKind, Span,
    Symbol, SymbolKind, canonical_name, location_for_span, node_text, symbol_visible_in_region,
};
use crate::text;
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, CompletionTextEdit, Hover, HoverContents,
    Location, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, Position, Range,
    SignatureHelp, SignatureInformation, TextEdit, Url,
};
#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use tree_sitter::Node;

const CANCELLATION_MESSAGE: &str = "request cancelled";
const MAX_HOVER_CANDIDATES: usize = 128;
const MAX_HOVER_EXCERPT_BYTES: usize = 16 * 1024;
const MAX_HOVER_VALUE_BYTES: usize = 64 * 1024;
const MAX_COMPLETION_ITEMS: usize = 256;
const MAX_COMPLETION_SCANNED_SYMBOLS: usize = 100_000;
const MAX_COMPLETION_CONTEXT_NODES: usize = 100_000;
const MAX_SIGNATURES: usize = 128;
const MAX_SIGNATURE_SCAN_BYTES: usize = 64 * 1024;
const MAX_SIGNATURE_NESTING: usize = 256;
const MAX_SIGNATURE_NODES: usize = 100_000;
const MAX_SIGNATURE_LABEL_BYTES: usize = 128 * 1024;
const MAX_SIGNATURE_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_SIGNATURE_PARAMETERS: usize = 4096;
const MAX_TRAILING_MEMBER_DOT_GAP_BYTES: usize = 256;

#[cfg(test)]
static COMPLETION_SYMBOL_VISITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
thread_local! {
    static PARAMETER_INFORMATION_HELPER_ENTRIES: Cell<usize> = const { Cell::new(0) };
    static PARAMETER_UTF16_SCAN_PASSES: Cell<usize> = const { Cell::new(0) };
    static SOURCE_SCAN_HELPER_ENTRIES: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn record_assistance_helper_entry(counter: &'static std::thread::LocalKey<Cell<usize>>) {
    counter.with(|value| value.set(value.get().saturating_add(1)));
}

#[derive(Debug)]
struct CompletionAccumulator<'a> {
    prefix: String,
    candidates: HashMap<String, (Candidate, usize)>,
    uncertain: HashMap<String, usize>,
    scanned: usize,
    exhausted: bool,
    is_incomplete: bool,
    budget: &'a mut AssistanceBudget,
}

enum CompletionMember<'a> {
    Unqualified,
    Node(Node<'a>),
    Expression(Node<'a>),
    Bare { path: &'a str, end: usize },
    Unsupported,
}

enum TrailingMemberDot {
    Absent,
    Present(usize),
    GapExceeded,
}

impl<'a> CompletionAccumulator<'a> {
    fn new(prefix: &str, budget: &'a mut AssistanceBudget) -> Self {
        Self {
            prefix: canonical_name(prefix),
            candidates: HashMap::new(),
            uncertain: HashMap::new(),
            scanned: 0,
            exhausted: false,
            is_incomplete: false,
            budget,
        }
    }

    fn matches(&self, symbol: &Symbol) -> bool {
        canonical_name(&symbol.name).starts_with(&self.prefix)
    }

    fn take_scan_slot(&mut self, cancel: &AtomicBool) -> Result<bool, String> {
        if self.scanned >= MAX_COMPLETION_SCANNED_SYMBOLS || !self.budget.take_work(1, cancel)? {
            self.exhausted = true;
            self.is_incomplete = true;
            return Ok(false);
        }
        self.scanned += 1;
        #[cfg(test)]
        COMPLETION_SYMBOL_VISITS.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    fn insert(&mut self, candidate: Candidate, symbol: &Symbol, precedence: usize) {
        if let Some(uncertain_precedence) = self.uncertain.get(&symbol.key).copied() {
            if uncertain_precedence <= precedence {
                return;
            }
            self.uncertain.remove(&symbol.key);
        }
        let replace = self
            .candidates
            .get(&symbol.key)
            .is_none_or(|(_, current_precedence)| precedence < *current_precedence);
        if replace {
            self.candidates
                .insert(symbol.key.clone(), (candidate, precedence));
        }
    }

    fn mark_uncertain(&mut self, key: &str, precedence: usize) {
        if self
            .candidates
            .get(key)
            .is_some_and(|(_, current_precedence)| *current_precedence < precedence)
        {
            return;
        }
        self.candidates.remove(key);
        self.uncertain
            .entry(key.to_owned())
            .and_modify(|current_precedence| {
                *current_precedence = (*current_precedence).min(precedence);
            })
            .or_insert(precedence);
    }
}

/// A bounded, source-derived declaration projection.
///
/// The projection intentionally keeps only a declaration header.  Keeping it
/// separate from the LSP rendering makes the same source representation
/// reusable by later completion and signature-help work without coupling those
/// features to Markdown formatting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeclarationDisplay {
    pub(crate) context: String,
    pub(crate) excerpt: String,
    pub(crate) source_uri: Url,
    pub(crate) source_start: usize,
}

impl NavigationIndex {
    /// Return bounded, source-based completion items for the identifier or
    /// member expression at `position`.
    pub fn completion(&self, uri: &Url, position: Position) -> Result<CompletionList, String> {
        let cancel = AtomicBool::new(false);
        self.completion_with_cancel(uri, position, &cancel)
    }

    pub(crate) fn completion_with_cancel(
        &self,
        uri: &Url,
        position: Position,
        cancel: &AtomicBool,
    ) -> Result<CompletionList, String> {
        check_cancel(cancel)?;
        let mut budget = AssistanceBudget::new(
            MAX_COMPLETION_CONTEXT_NODES + MAX_COMPLETION_SCANNED_SYMBOLS,
            MAX_HOVER_VALUE_BYTES,
            "completion",
        );
        let Some(document) = self.documents.get(uri) else {
            return Ok(CompletionList::default());
        };
        let Some(offset) = text::position_to_offset(&document.source, position) else {
            return Ok(CompletionList::default());
        };
        let anchor = offset.saturating_sub(1);
        if completion_position_is_conditionally_unknown(document, offset, anchor) {
            return Ok(CompletionList {
                is_incomplete: true,
                items: Vec::new(),
            });
        }
        if completion_position_is_ignored_with_budget(
            document,
            offset,
            anchor,
            cancel,
            &mut budget,
        )? {
            return Ok(CompletionList::default());
        }
        if unsupported_context_at(
            document,
            offset,
            cancel,
            &mut budget,
            MAX_COMPLETION_CONTEXT_NODES,
            "completion",
        )? {
            return Ok(CompletionList::default());
        }

        let prefix_start =
            identifier_prefix_start_with_budget(&document.source, offset, cancel, &mut budget)?;
        let prefix = document
            .source
            .get(prefix_start..offset)
            .unwrap_or_default();
        let identifier = identifier_at_with_budget(
            document.tree.root_node(),
            anchor,
            cancel,
            &mut budget,
            "completion",
        )?
        .or(identifier_at_with_budget(
            document.tree.root_node(),
            offset,
            cancel,
            &mut budget,
            "completion",
        )?);
        let replacement_end = identifier
            .filter(|identifier| {
                identifier.start_byte() <= offset && offset <= identifier.end_byte()
            })
            .map_or(offset, |identifier| identifier.end_byte());
        if let Some(identifier) = identifier {
            if unsupported_hover_context_with_budget(document, identifier, cancel, &mut budget)? {
                return Ok(CompletionList::default());
            }
        }
        let member = member_expression_for_completion(
            document,
            offset,
            prefix_start,
            identifier,
            cancel,
            &mut budget,
        )?;
        if matches!(member, CompletionMember::Unsupported) {
            return Ok(CompletionList::default());
        }
        let (candidates, mut is_incomplete) = self.completion_candidates(
            uri,
            document,
            anchor,
            prefix,
            member,
            identifier,
            cancel,
            &mut budget,
        )?;
        let range = Range {
            start: text::offset_to_position(&document.source, prefix_start)
                .ok_or_else(|| "completion prefix start is not a UTF-16 boundary".to_string())?,
            end: text::offset_to_position(&document.source, replacement_end)
                .ok_or_else(|| "completion cursor is not a UTF-16 boundary".to_string())?,
        };
        let response_truncated = candidates.len() > MAX_COMPLETION_ITEMS;
        let mut items = Vec::with_capacity(candidates.len().min(MAX_COMPLETION_ITEMS));
        for candidate in candidates.into_iter().take(MAX_COMPLETION_ITEMS) {
            check_cancel(cancel)?;
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            budget.require_bytes(symbol.name.len().saturating_mul(2), cancel)?;
            items.push(CompletionItem {
                label: symbol.name.clone(),
                kind: Some(completion_kind(symbol.kind)),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(
                    range,
                    symbol.name.clone(),
                ))),
                ..CompletionItem::default()
            });
        }
        if response_truncated {
            is_incomplete = true;
        }
        Ok(CompletionList {
            is_incomplete,
            items,
        })
    }

    /// Return source-declared callable signatures for the call containing
    /// `position`. Argument selection is syntactic and deliberately does not
    /// infer argument types or choose an overload winner.
    pub fn signature_help(
        &self,
        uri: &Url,
        position: Position,
    ) -> Result<Option<SignatureHelp>, String> {
        let cancel = AtomicBool::new(false);
        self.signature_help_with_cancel(uri, position, &cancel)
    }

    pub(crate) fn signature_help_with_cancel(
        &self,
        uri: &Url,
        position: Position,
        cancel: &AtomicBool,
    ) -> Result<Option<SignatureHelp>, String> {
        check_cancel(cancel)?;
        let mut budget = AssistanceBudget::new(
            MAX_SIGNATURE_NODES * 4 + MAX_SIGNATURES + MAX_SIGNATURE_PARAMETERS,
            MAX_SIGNATURE_SCAN_BYTES * 4
                + MAX_SIGNATURE_LABEL_BYTES * MAX_SIGNATURES
                + MAX_SIGNATURE_RESPONSE_BYTES,
            "signature help",
        );
        let Some(document) = self.documents.get(uri) else {
            return Ok(None);
        };
        let Some(offset) = text::position_to_offset(&document.source, position) else {
            return Ok(None);
        };
        let anchor = offset.saturating_sub(1);
        if completion_position_is_ignored_with_budget(
            document,
            offset,
            anchor,
            cancel,
            &mut budget,
        )? {
            return Ok(None);
        }
        if unsupported_context_at(
            document,
            offset,
            cancel,
            &mut budget,
            MAX_SIGNATURE_NODES,
            "signature help",
        )? {
            return Ok(None);
        }
        let Some(call) = call_at_offset(document, offset, cancel, &mut budget)? else {
            return Ok(None);
        };
        let Some(entity) = call.child_by_field_name("entity") else {
            return Ok(None);
        };
        let Some(open) = source_open_paren(document, entity, call, cancel, &mut budget)? else {
            return Ok(None);
        };
        if conditional_argument_is_unknown(document, open, offset) {
            return Ok(None);
        }
        let Some(active_parameter) = argument_index(
            &document.conditionals.projected_source,
            open,
            offset,
            cancel,
            &mut budget,
        )?
        else {
            return Ok(None);
        };

        let candidates =
            self.callable_candidates(uri, document, call, entity, cancel, &mut budget)?;
        if candidates.is_empty() {
            return Ok(None);
        }
        let mut selected = BTreeMap::<(String, String), Candidate>::new();
        for candidate in candidates {
            check_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            if symbol.kind != SymbolKind::Routine
                || self.candidate_is_conditionally_unknown(&candidate)
            {
                return Ok(None);
            }
            let key = (
                candidate.uri.to_string(),
                symbol.routine_key.clone().unwrap_or_else(|| {
                    format!(
                        "{}:{}",
                        symbol.declaration_span.start, symbol.declaration_span.end
                    )
                }),
            );
            let replace = selected
                .get(&key)
                .and_then(|current| self.symbol(current))
                .is_none_or(|current| candidate_rank(symbol) < candidate_rank(current));
            if replace {
                selected.insert(key, candidate);
            }
            if selected.len() > MAX_SIGNATURES {
                return Err(format!(
                    "signature help exceeds the {MAX_SIGNATURES}-signature limit"
                ));
            }
        }

        let mut signatures = Vec::with_capacity(selected.len());
        let mut response_bytes = 0usize;
        let mut response_parameters = 0usize;
        for candidate in selected.into_values() {
            check_cancel(cancel)?;
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            let Some(document) = self.documents.get(&candidate.uri) else {
                continue;
            };
            let Some((label, label_start)) =
                routine_signature_label(document, symbol, cancel, &mut budget)?
            else {
                continue;
            };
            if symbol.routine_parameter_spans.len() > MAX_SIGNATURE_PARAMETERS {
                return Err(format!(
                    "signature help exceeds the {MAX_SIGNATURE_PARAMETERS}-parameter limit"
                ));
            }
            let parameter_count = symbol.routine_parameter_spans.len();
            let signature_bytes = label
                .len()
                .saturating_add(parameter_count.saturating_mul(std::mem::size_of::<u32>() * 2));
            response_bytes = response_bytes.saturating_add(signature_bytes);
            if response_bytes > MAX_SIGNATURE_RESPONSE_BYTES {
                return Err(format!(
                    "signature help exceeds the {MAX_SIGNATURE_RESPONSE_BYTES}-byte response limit"
                ));
            }
            budget.require_bytes(signature_bytes, cancel)?;
            let Some(parameters) = parameter_information_with_budget(
                symbol,
                &label,
                label_start,
                cancel,
                &mut budget,
            )?
            else {
                continue;
            };
            response_parameters = response_parameters.saturating_add(parameters.len());
            if response_parameters > MAX_SIGNATURE_PARAMETERS {
                return Err(format!(
                    "signature help exceeds the {MAX_SIGNATURE_PARAMETERS}-parameter limit"
                ));
            }
            signatures.push(SignatureInformation {
                label,
                documentation: None,
                parameters: Some(parameters),
                active_parameter: None,
            });
        }
        signatures.sort_by(|left, right| left.label.cmp(&right.label));
        if signatures.is_empty() {
            return Ok(None);
        }
        Ok(Some(SignatureHelp {
            signatures,
            // There may be several source overloads and no argument-type
            // inference is performed, so selecting one would be misleading.
            active_signature: None,
            active_parameter: Some(active_parameter as u32),
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn completion_candidates(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        prefix: &str,
        member: CompletionMember<'_>,
        lookup_identifier: Option<Node<'_>>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<(Vec<Candidate>, bool), String> {
        let mut accumulator = CompletionAccumulator::new(prefix, budget);
        let mut private_spans = HashMap::new();
        let unqualified = matches!(&member, CompletionMember::Unqualified);
        let mut suppress_unqualified_globals = false;
        let member_receivers = match member {
            CompletionMember::Unqualified => None,
            CompletionMember::Node(dot) => {
                let Some(lhs) = dot.child_by_field_name("lhs") else {
                    return Ok((Vec::new(), false));
                };
                let mut state = super::ResolutionState::new();
                let receivers = self.resolve_receivers_with_state_and_budget(
                    current_uri,
                    current_document,
                    offset,
                    lhs,
                    lhs,
                    &mut state,
                    cancel,
                    accumulator.budget,
                    0,
                )?;
                if state.receiver_resolution_uncertain() {
                    accumulator.is_incomplete = true;
                }
                Some(receivers)
            }
            CompletionMember::Expression(receiver) => {
                let mut state = super::ResolutionState::new();
                let receivers = self.resolve_receivers_with_state_and_budget(
                    current_uri,
                    current_document,
                    offset,
                    receiver,
                    receiver,
                    &mut state,
                    cancel,
                    accumulator.budget,
                    0,
                )?;
                if state.receiver_resolution_uncertain() {
                    accumulator.is_incomplete = true;
                }
                Some(receivers)
            }
            CompletionMember::Bare { path, end } => {
                let lookup_identifier = lookup_identifier.or(identifier_at_with_budget(
                    current_document.tree.root_node(),
                    end.saturating_sub(1),
                    cancel,
                    accumulator.budget,
                    "completion",
                )?);
                let (receivers, uncertain) = self.resolve_completion_receivers(
                    current_uri,
                    current_document,
                    end.saturating_sub(1),
                    path,
                    lookup_identifier,
                    cancel,
                    accumulator.budget,
                )?;
                if uncertain {
                    accumulator.is_incomplete = true;
                }
                Some(receivers)
            }
            CompletionMember::Unsupported => return Ok((Vec::new(), false)),
        };
        if let Some(receivers) = member_receivers {
            if receivers.is_empty() {
                return Ok((Vec::new(), accumulator.is_incomplete));
            }
            if receivers.len() > 1 {
                accumulator.is_incomplete = true;
            }
            for receiver in receivers {
                check_cancel(cancel)?;
                match receiver {
                    super::Receiver::Unit(unit_uri) => {
                        self.add_exported_completion_candidates(
                            &mut accumulator,
                            &unit_uri,
                            current_uri,
                            false,
                            &mut private_spans,
                            0,
                            cancel,
                        )?;
                    }
                    super::Receiver::Type(type_uri, type_key, type_scope) => {
                        let (ancestry_known, has_ambiguous_names) = self
                            .add_member_completion_candidates(
                                &mut accumulator,
                                &type_uri,
                                &type_key,
                                type_scope,
                                current_uri,
                                &mut private_spans,
                                0,
                                cancel,
                            )?;
                        if !ancestry_known || has_ambiguous_names {
                            accumulator.is_incomplete = true;
                        }
                    }
                }
                if accumulator.exhausted {
                    break;
                }
            }
        } else {
            let mut precedence = 0;
            for scope in
                self.scope_chain_with_budget(current_document, offset, cancel, accumulator.budget)?
            {
                if scope == super::ROOT_SCOPE {
                    continue;
                }
                self.add_symbols_for_completion_scope(
                    &mut accumulator,
                    current_uri,
                    current_document,
                    scope,
                    precedence,
                    &mut private_spans,
                    cancel,
                )?;
                if accumulator.exhausted {
                    break;
                }
                precedence += 1;
            }

            if !accumulator.exhausted {
                if let Some(owner_type) = current_document.owner_type_at(offset) {
                    let (ancestry_known, has_ambiguous_names) = self
                        .add_member_completion_candidates(
                            &mut accumulator,
                            current_uri,
                            &owner_type,
                            super::ROOT_SCOPE,
                            current_uri,
                            &mut private_spans,
                            precedence,
                            cancel,
                        )?;
                    if !ancestry_known || has_ambiguous_names {
                        accumulator.is_incomplete = true;
                        if unqualified {
                            suppress_unqualified_globals = true;
                        }
                    }
                    precedence += 1;
                }
            }

            if !suppress_unqualified_globals {
                let region = current_document.region_at(offset);
                for index in current_document
                    .scope_symbol_indices
                    .get(&super::ROOT_SCOPE)
                    .into_iter()
                    .flatten()
                {
                    check_cancel(cancel)?;
                    if !accumulator.take_scan_slot(cancel)? {
                        break;
                    }
                    let Some(symbol) = current_document.symbols.get(*index) else {
                        continue;
                    };
                    if symbol.scope != super::ROOT_SCOPE
                        || symbol.owner_type.is_some()
                        || symbol.local_only
                        || !symbol_visible_in_region(symbol, region)
                    {
                        continue;
                    }
                    self.add_completion_candidate(
                        &mut accumulator,
                        Candidate {
                            uri: current_uri.clone(),
                            index: *index,
                        },
                        current_uri,
                        false,
                        &mut private_spans,
                        precedence,
                        cancel,
                    )?;
                }

                for unit in current_document.active_uses(region) {
                    if accumulator.exhausted {
                        break;
                    }
                    check_cancel(cancel)?;
                    if current_document.unknown_imports.contains(unit.as_str()) {
                        continue;
                    }
                    for unit_uri in self.unit_urls_for_import(current_document, unit) {
                        self.add_exported_completion_candidates(
                            &mut accumulator,
                            &unit_uri,
                            current_uri,
                            false,
                            &mut private_spans,
                            precedence.saturating_add(1),
                            cancel,
                        )?;
                        if let Some(document) = self.documents.get(&unit_uri) {
                            if let Some(index) = document
                                .symbol_indices_by_scope_key
                                .get(&(super::ROOT_SCOPE, document.unit_name.clone()))
                                .into_iter()
                                .flatten()
                                .copied()
                                .find(|index| {
                                    document
                                        .symbols
                                        .get(*index)
                                        .is_some_and(|symbol| symbol.kind == SymbolKind::Unit)
                                })
                            {
                                if !accumulator.take_scan_slot(cancel)? {
                                    break;
                                }
                                self.add_completion_candidate(
                                    &mut accumulator,
                                    Candidate {
                                        uri: unit_uri.clone(),
                                        index,
                                    },
                                    current_uri,
                                    false,
                                    &mut private_spans,
                                    precedence.saturating_add(1),
                                    cancel,
                                )?;
                            }
                        }
                        if accumulator.exhausted {
                            break;
                        }
                    }
                }
            }
        }

        if !accumulator.uncertain.is_empty() {
            accumulator.is_incomplete = true;
        }
        let mut candidates = accumulator
            .candidates
            .into_values()
            .map(|(candidate, _)| candidate)
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            let left_symbol = self.symbol(left);
            let right_symbol = self.symbol(right);
            left_symbol
                .map(|symbol| canonical_name(&symbol.name))
                .cmp(&right_symbol.map(|symbol| canonical_name(&symbol.name)))
                .then_with(|| {
                    left_symbol
                        .map(|symbol| symbol.name.as_str())
                        .cmp(&right_symbol.map(|symbol| symbol.name.as_str()))
                })
                .then_with(|| left.uri.as_str().cmp(right.uri.as_str()))
                .then_with(|| left.index.cmp(&right.index))
        });
        Ok((candidates, accumulator.is_incomplete))
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_completion_receivers(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        path: &str,
        lookup_identifier: Option<Node<'_>>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<(Vec<super::Receiver>, bool), String> {
        budget.require_bytes(path.len(), cancel)?;
        let parts = path
            .split('.')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if parts.is_empty() || parts.len() > super::MAX_RECEIVER_WORK {
            return Ok((Vec::new(), false));
        }
        let Some(lookup_identifier) = lookup_identifier else {
            return Ok((Vec::new(), false));
        };
        let mut state = super::ResolutionState::new();
        let receivers = if parts.len() == 1 {
            self.resolve_identifier_receiver_with_budget(
                current_uri,
                current_document,
                offset,
                &parts[0],
                lookup_identifier,
                &mut state,
                cancel,
                budget,
            )
        } else {
            self.resolve_qualified_receiver_path_with_budget(
                current_uri,
                current_document,
                offset,
                &parts,
                lookup_identifier,
                &mut state,
                cancel,
                budget,
            )
        }?;
        Ok((receivers, state.receiver_resolution_uncertain()))
    }

    #[allow(clippy::too_many_arguments)]
    fn add_symbols_for_completion_scope(
        &self,
        accumulator: &mut CompletionAccumulator,
        uri: &Url,
        document: &Document,
        scope: usize,
        precedence: usize,
        private_spans: &mut HashMap<Url, HashSet<Span>>,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let indices = document
            .scope_symbol_indices
            .get(&scope)
            .into_iter()
            .flatten();
        for index in indices {
            check_cancel(cancel)?;
            if !accumulator.take_scan_slot(cancel)? {
                break;
            }
            let Some(symbol) = document.symbols.get(*index) else {
                continue;
            };
            if symbol.scope != scope
                || symbol.owner_type.is_some()
                || symbol.local_only
                || symbol.kind == SymbolKind::Unit
            {
                continue;
            }
            self.add_completion_candidate(
                accumulator,
                Candidate {
                    uri: uri.clone(),
                    index: *index,
                },
                uri,
                false,
                private_spans,
                precedence,
                cancel,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_exported_completion_candidates(
        &self,
        accumulator: &mut CompletionAccumulator,
        uri: &Url,
        current_uri: &Url,
        member_access: bool,
        private_spans: &mut HashMap<Url, HashSet<Span>>,
        precedence: usize,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(());
        };
        for index in &document.exported_symbol_indices {
            check_cancel(cancel)?;
            if !accumulator.take_scan_slot(cancel)? {
                break;
            }
            let Some(symbol) = document.symbols.get(*index) else {
                continue;
            };
            if symbol.owner_type.is_some()
                || symbol.kind == SymbolKind::Unit
                || symbol.local_only
                || !(symbol.region == Region::Interface
                    || (symbol.kind == SymbolKind::Routine
                        && symbol.origin == Origin::Definition
                        && symbol
                            .routine_key
                            .as_ref()
                            .is_some_and(|key| document.interface_routine_keys.contains(key))))
            {
                continue;
            }
            self.add_completion_candidate(
                accumulator,
                Candidate {
                    uri: uri.clone(),
                    index: *index,
                },
                current_uri,
                member_access,
                private_spans,
                precedence,
                cancel,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_member_completion_candidates(
        &self,
        accumulator: &mut CompletionAccumulator,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        current_uri: &Url,
        private_spans: &mut HashMap<Url, HashSet<Span>>,
        precedence: usize,
        cancel: &AtomicBool,
    ) -> Result<(bool, bool), String> {
        let lookup = self.member_candidates_for_completion_with_budget(
            type_uri,
            type_key,
            type_scope,
            type_uri == current_uri,
            cancel,
            accumulator.budget,
        )?;
        let status = (lookup.ancestry_known, !lookup.ambiguous_names.is_empty());
        let mut routine_keys = HashSet::new();
        for candidate in lookup.candidates {
            check_cancel(cancel)?;
            if !accumulator.take_scan_slot(cancel)? {
                break;
            }
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            if symbol.local_only {
                continue;
            }
            if symbol.kind == SymbolKind::Routine
                && symbol.origin == Origin::Definition
                && symbol
                    .routine_key
                    .as_ref()
                    .is_some_and(|key| routine_keys.contains(key))
            {
                continue;
            }
            if symbol.kind == SymbolKind::Routine {
                if let Some(key) = &symbol.routine_key {
                    routine_keys.insert(key.clone());
                }
            }
            self.add_completion_candidate(
                accumulator,
                Candidate {
                    uri: candidate.uri.clone(),
                    index: candidate.index,
                },
                current_uri,
                candidate.uri != *current_uri,
                private_spans,
                precedence,
                cancel,
            )?;
        }
        Ok(status)
    }

    #[allow(clippy::too_many_arguments)]
    fn add_completion_candidate(
        &self,
        accumulator: &mut CompletionAccumulator,
        candidate: Candidate,
        current_uri: &Url,
        member_access: bool,
        private_spans: &mut HashMap<Url, HashSet<Span>>,
        precedence: usize,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        check_cancel(cancel)?;
        let Some(symbol) = self.symbol(&candidate) else {
            return Ok(());
        };
        if !completion_symbol_kind_supported(symbol.kind)
            || !accumulator.matches(symbol)
            || symbol.unresolved_abbreviated
        {
            return Ok(());
        }
        if member_access
            && candidate.uri != *current_uri
            && self.symbol_is_private_or_protected(
                &candidate,
                private_spans,
                accumulator.budget,
                cancel,
            )?
        {
            return Ok(());
        }
        if self.candidate_is_conditionally_unknown(&candidate) {
            accumulator.mark_uncertain(&symbol.key, precedence);
            return Ok(());
        }
        accumulator.insert(candidate, symbol, precedence);
        Ok(())
    }

    fn symbol_is_private_or_protected(
        &self,
        candidate: &Candidate,
        private_spans: &mut HashMap<Url, HashSet<Span>>,
        budget: &mut AssistanceBudget,
        cancel: &AtomicBool,
    ) -> Result<bool, String> {
        let Some(document) = self.documents.get(&candidate.uri) else {
            return Ok(true);
        };
        let Some(symbol) = document.symbols.get(candidate.index) else {
            return Ok(true);
        };
        let declaration_span = if symbol.origin == Origin::Definition {
            symbol
                .routine_key
                .as_ref()
                .and_then(|key| document.routine_declaration_spans.get(key).copied())
                .unwrap_or(symbol.declaration_span)
        } else {
            symbol.declaration_span
        };
        if !private_spans.contains_key(&candidate.uri) {
            let spans = private_declaration_spans(document, budget, cancel)?;
            private_spans.insert(candidate.uri.clone(), spans);
        }
        Ok(private_spans
            .get(&candidate.uri)
            .is_some_and(|spans| spans.contains(&declaration_span)))
    }

    fn callable_candidates(
        &self,
        current_uri: &Url,
        document: &Document,
        call: Node<'_>,
        entity: Node<'_>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let lookup_identifier = callable_lookup_identifier(entity);
        let mut candidates = self.resolve_candidates_at_with_budget(
            current_uri,
            document,
            lookup_identifier.start_byte(),
            lookup_identifier,
            cancel,
            budget,
        )?;
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Ok(Vec::new());
        }
        candidates.retain(|candidate| {
            self.symbol(candidate).is_some_and(|symbol| {
                symbol.kind == SymbolKind::Routine && !symbol.unresolved_abbreviated
            })
        });
        if entity.kind() == "exprDot" {
            let mut private_spans = HashMap::new();
            let mut visible = Vec::with_capacity(candidates.len());
            for candidate in candidates {
                check_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if candidate.uri == *current_uri
                    || !self.symbol_is_private_or_protected(
                        &candidate,
                        &mut private_spans,
                        budget,
                        cancel,
                    )?
                {
                    visible.push(candidate);
                }
            }
            candidates = visible;
        }
        check_cancel(cancel)?;
        let _ = call;
        Ok(candidates)
    }

    /// Return source declarations for the named type of the identifier at
    /// `position`.
    ///
    /// Type aliases deliberately retain their own source identity.  This is a
    /// conservative source-based policy: the server does not chase aliases,
    /// infer anonymous or built-in types, or guess through an ambiguous
    /// workspace-wide spelling match.
    pub fn type_definitions(&self, uri: &Url, position: Position) -> Vec<Location> {
        let cancel = AtomicBool::new(false);
        self.type_definitions_with_cancel(uri, position, &cancel)
            .unwrap_or_default()
    }

    pub(crate) fn type_definitions_with_cancel(
        &self,
        uri: &Url,
        position: Position,
        cancel: &AtomicBool,
    ) -> Result<Vec<Location>, String> {
        let mut budget = AssistanceBudget::new(
            MAX_COMPLETION_CONTEXT_NODES + MAX_HOVER_CANDIDATES,
            MAX_HOVER_VALUE_BYTES + MAX_HOVER_EXCERPT_BYTES,
            "type definition",
        );
        self.type_definitions_with_budget(uri, position, cancel, &mut budget)
    }

    #[allow(clippy::too_many_arguments)]
    fn type_definitions_with_budget(
        &self,
        uri: &Url,
        position: Position,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Location>, String> {
        check_cancel(cancel)?;
        let Some(document) = self.documents.get(uri) else {
            return Ok(Vec::new());
        };
        let Some(offset) = text::position_to_offset(&document.source, position) else {
            return Ok(Vec::new());
        };
        if document.conditionals.is_unknown_at(offset) {
            return Ok(Vec::new());
        }
        if ignored_offset_with_budget(document, offset, cancel, budget, "type definition")? {
            return Ok(Vec::new());
        }
        let Some(identifier) = identifier_at_with_budget(
            document.tree.root_node(),
            offset,
            cancel,
            budget,
            "type definition",
        )?
        else {
            return Ok(Vec::new());
        };
        if unsupported_hover_context_with_budget(document, identifier, cancel, budget)? {
            return Ok(Vec::new());
        }

        check_cancel(cancel)?;
        let result_annotation =
            if node_text(identifier, &document.source).eq_ignore_ascii_case("Result") {
                let scope = self.budgeted_scope_at(document, offset, cancel, budget)?;
                document.result_type_annotation_for_body_scope(scope)
            } else {
                None
            };
        let references = if result_annotation.is_some() {
            Vec::new()
        } else {
            self.resolve_candidates_at_with_budget(
                uri, document, offset, identifier, cancel, budget,
            )?
        };
        if references
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Ok(Vec::new());
        }
        let mut targets = Vec::new();
        let mut state = super::ResolutionState::new();
        if let Some(annotation) = result_annotation {
            let Some(lookup_identifier) = identifier_at_with_budget(
                document.tree.root_node(),
                annotation.offset,
                cancel,
                budget,
                "type definition",
            )?
            else {
                return Ok(Vec::new());
            };
            targets.extend(self.type_declaration_candidates_with_budget(
                uri,
                document,
                annotation.offset,
                &annotation.name,
                lookup_identifier,
                Some(annotation.scope),
                &mut state,
                cancel,
                budget,
            )?);
        }
        for reference in references {
            check_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(symbol) = self.symbol(&reference) else {
                continue;
            };
            match symbol.kind {
                SymbolKind::Type => targets.push(reference),
                SymbolKind::Variable
                | SymbolKind::Parameter
                | SymbolKind::Field
                | SymbolKind::Property => {
                    let Some(declaration_document) = self.documents.get(&reference.uri) else {
                        continue;
                    };
                    let Some(type_name) = named_type_path_for_symbol(declaration_document, symbol)
                    else {
                        continue;
                    };
                    let Some(lookup_identifier) = identifier_at_with_budget(
                        declaration_document.tree.root_node(),
                        symbol.span.start,
                        cancel,
                        budget,
                        "type definition",
                    )?
                    else {
                        continue;
                    };
                    targets.extend(self.type_declaration_candidates_with_budget(
                        &reference.uri,
                        declaration_document,
                        symbol.span.start,
                        &type_name,
                        lookup_identifier,
                        None,
                        &mut state,
                        cancel,
                        budget,
                    )?);
                }
                SymbolKind::Routine => {
                    let is_constructor = symbol.routine_kind == RoutineKind::Constructor;
                    let type_name = if is_constructor {
                        symbol.owner_type_name.as_deref()
                    } else {
                        symbol.result_type_name.as_deref()
                    };
                    let Some(type_name) = type_name else {
                        continue;
                    };
                    let Some(declaration_document) = self.documents.get(&reference.uri) else {
                        continue;
                    };
                    if is_constructor {
                        if let Some(dot) = super::member_expression_at(identifier)
                            .filter(|dot| super::is_right_hand_member(*dot, identifier))
                        {
                            if let Some(lhs) = dot.child_by_field_name("lhs") {
                                let receivers = self.resolve_receivers_with_state_and_budget(
                                    uri, document, offset, lhs, lhs, &mut state, cancel, budget, 0,
                                )?;
                                let mut constructed_targets = Vec::new();
                                for receiver in receivers {
                                    let super::Receiver::Type(type_uri, type_key, _) = receiver
                                    else {
                                        continue;
                                    };
                                    constructed_targets.extend(
                                        self.type_candidates_in_unit_with_budget(
                                            &type_uri,
                                            &type_key,
                                            type_uri == *uri,
                                            cancel,
                                            budget,
                                        )?,
                                    );
                                }
                                if !constructed_targets.is_empty() {
                                    targets.extend(constructed_targets);
                                    continue;
                                }
                            }
                        }
                    }
                    let type_offset = if is_constructor {
                        symbol.span.start
                    } else {
                        symbol
                            .result_type_span
                            .map_or(symbol.span.start, |span| span.start)
                    };
                    let Some(lookup_identifier) = identifier_at_with_budget(
                        declaration_document.tree.root_node(),
                        type_offset,
                        cancel,
                        budget,
                        "type definition",
                    )?
                    else {
                        continue;
                    };
                    targets.extend(self.type_declaration_candidates_with_budget(
                        &reference.uri,
                        declaration_document,
                        type_offset,
                        type_name,
                        lookup_identifier,
                        Some(symbol.scope),
                        &mut state,
                        cancel,
                        budget,
                    )?);
                }
                _ => {}
            }
        }
        if targets
            .iter()
            .any(|target| self.candidate_is_conditionally_unknown(target))
        {
            return Ok(Vec::new());
        }

        let mut seen = HashSet::new();
        let mut locations = Vec::new();
        for target in targets {
            check_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if !seen.insert((target.uri.clone(), target.index)) {
                continue;
            }
            let Some(target_document) = self.documents.get(&target.uri) else {
                continue;
            };
            let Some(symbol) = target_document.symbols.get(target.index) else {
                continue;
            };
            let Some(location) =
                location_for_span(&target.uri, &target_document.source, symbol.span)
            else {
                continue;
            };
            locations.push(location);
        }
        locations.sort_by(|left, right| {
            left.uri
                .as_str()
                .cmp(right.uri.as_str())
                .then_with(|| left.range.start.line.cmp(&right.range.start.line))
                .then_with(|| left.range.start.character.cmp(&right.range.start.character))
        });
        Ok(locations)
    }

    #[allow(clippy::too_many_arguments)]
    fn type_declaration_candidates_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_name: &str,
        lookup_identifier: Node<'_>,
        scope_override: Option<usize>,
        state: &mut super::ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let parts = type_name
            .split('.')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if parts.is_empty() || !state.take_type_work(parts.len()) {
            return Ok(Vec::new());
        }
        if parts.len() == 1 {
            let references = if let Some(scope) = scope_override {
                self.unqualified_references_with_budget_at_scope(
                    current_uri,
                    current_document,
                    offset,
                    &parts[0],
                    lookup_identifier,
                    scope,
                    cancel,
                    budget,
                )?
            } else {
                self.unqualified_references_with_budget(
                    current_uri,
                    current_document,
                    offset,
                    &parts[0],
                    lookup_identifier,
                    cancel,
                    budget,
                )?
            }
            .into_iter()
            .filter(|candidate| {
                self.symbol(candidate)
                    .is_some_and(|symbol| symbol.kind == SymbolKind::Type)
            })
            .collect();
            return Ok(references);
        }

        self.type_reference_candidates_with_budget(
            current_uri,
            current_document,
            offset,
            lookup_identifier,
            &parts,
            parts.len().saturating_sub(1),
            cancel,
            budget,
        )
    }

    /// Return source-based hover information for the identifier at `position`.
    ///
    /// The direct index API uses Markdown because it has no client capability
    /// negotiation context. Protocol workers call [`hover_with_cancel`] with
    /// the negotiated format instead.
    pub fn hover(&self, uri: &Url, position: Position) -> Option<Hover> {
        let cancel = AtomicBool::new(false);
        self.hover_with_cancel(uri, position, MarkupKind::Markdown, &cancel)
            .ok()
            .flatten()
    }

    pub(crate) fn hover_with_cancel(
        &self,
        uri: &Url,
        position: Position,
        format: MarkupKind,
        cancel: &AtomicBool,
    ) -> Result<Option<Hover>, String> {
        check_cancel(cancel)?;
        let mut budget = AssistanceBudget::new(
            MAX_COMPLETION_CONTEXT_NODES + MAX_HOVER_CANDIDATES,
            MAX_HOVER_VALUE_BYTES + MAX_HOVER_EXCERPT_BYTES,
            "hover",
        );
        let Some(document) = self.documents.get(uri) else {
            return Ok(None);
        };
        let Some(offset) = text::position_to_offset(&document.source, position) else {
            return Ok(None);
        };
        if document.conditionals.is_unknown_at(offset) {
            return Ok(None);
        }
        if ignored_offset_with_budget(document, offset, cancel, &mut budget, "hover")? {
            return Ok(None);
        }
        let Some(identifier) = identifier_at_with_budget(
            document.tree.root_node(),
            offset,
            cancel,
            &mut budget,
            "hover",
        )?
        else {
            return Ok(None);
        };
        if unsupported_hover_context_with_budget(document, identifier, cancel, &mut budget)? {
            return Ok(None);
        }

        check_cancel(cancel)?;
        let candidates = self.resolve_candidates_at_with_budget(
            uri,
            document,
            offset,
            identifier,
            cancel,
            &mut budget,
        )?;
        if candidates.is_empty() {
            return Ok(None);
        }
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Ok(None);
        }
        let scope = self.budgeted_scope_at(document, offset, cancel, &mut budget)?;
        if self.is_unknown_global_fallback_for_owner(
            document,
            identifier,
            document.owner_type_for_scope(scope).as_deref(),
            &candidates,
        ) {
            return Ok(None);
        }
        if candidates.len() > MAX_HOVER_CANDIDATES {
            return Err(format!(
                "hover result exceeds the {MAX_HOVER_CANDIDATES}-candidate limit"
            ));
        }

        let selected = self.select_display_candidates(candidates);
        let mut displays = Vec::with_capacity(selected.len());
        let mut seen = HashSet::new();
        for candidate in selected {
            check_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(display) = self.declaration_display(&candidate, cancel, &mut budget)? else {
                continue;
            };
            let key = (
                display.source_uri.clone(),
                display.source_start,
                display.context.clone(),
                display.excerpt.clone(),
            );
            if seen.insert(key) {
                displays.push(display);
            }
        }
        if displays.is_empty() {
            return Ok(None);
        }
        displays.sort_by(|left, right| {
            left.source_uri
                .as_str()
                .cmp(right.source_uri.as_str())
                .then_with(|| left.source_start.cmp(&right.source_start))
                .then_with(|| left.excerpt.cmp(&right.excerpt))
        });

        let Some(location) = location_for_span(uri, &document.source, Span::from_node(identifier))
        else {
            return Ok(None);
        };
        let value = render_displays_with_budget(&displays, format.clone(), cancel, &mut budget)?;
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: format,
                value,
            }),
            range: Some(location.range),
        }))
    }

    fn select_display_candidates(&self, candidates: Vec<Candidate>) -> Vec<Candidate> {
        let mut routines: BTreeMap<(String, String), Candidate> = BTreeMap::new();
        let mut other_candidates: BTreeMap<(String, usize), Candidate> = BTreeMap::new();

        for candidate in candidates {
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            if symbol.kind == SymbolKind::Routine {
                let key = symbol.routine_key.clone().unwrap_or_else(|| {
                    format!(
                        "unpaired:{}:{}",
                        symbol.declaration_span.start, symbol.declaration_span.end
                    )
                });
                let map_key = (candidate.uri.to_string(), key);
                let replace = routines
                    .get(&map_key)
                    .and_then(|current| self.symbol(current))
                    .is_none_or(|current| candidate_rank(symbol) < candidate_rank(current));
                if replace {
                    routines.insert(map_key, candidate);
                }
            } else {
                other_candidates
                    .entry((candidate.uri.to_string(), candidate.index))
                    .or_insert(candidate);
            }
        }

        routines
            .into_values()
            .chain(other_candidates.into_values())
            .collect()
    }

    fn declaration_display(
        &self,
        candidate: &Candidate,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<DeclarationDisplay>, String> {
        let Some(document) = self.documents.get(&candidate.uri) else {
            return Ok(None);
        };
        let Some(symbol) = document.symbols.get(candidate.index) else {
            return Ok(None);
        };
        let Some(excerpt) = declaration_excerpt(document, symbol, cancel, budget)? else {
            return Ok(None);
        };
        let unit_name = display_unit_name(document);
        let context = symbol
            .owner_type_name
            .as_ref()
            .map_or_else(|| unit_name.clone(), |owner| format!("{unit_name}.{owner}"));
        Ok(Some(DeclarationDisplay {
            context,
            excerpt,
            source_uri: candidate.uri.clone(),
            source_start: symbol.declaration_span.start,
        }))
    }
}

fn callable_lookup_identifier(entity: Node<'_>) -> Node<'_> {
    let mut current = entity;
    loop {
        match current.kind() {
            "identifier" => return current,
            "exprDot" | "genericDot" | "typerefDot" => {
                let Some(rhs) = current.child_by_field_name("rhs") else {
                    return entity;
                };
                current = rhs;
            }
            _ => return entity,
        }
    }
}

fn named_type_path_for_symbol(_document: &Document, symbol: &Symbol) -> Option<String> {
    matches!(
        symbol.kind,
        SymbolKind::Variable | SymbolKind::Parameter | SymbolKind::Field | SymbolKind::Property
    )
    .then(|| symbol.type_name.clone())
    .flatten()
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(CANCELLATION_MESSAGE.to_string())
    } else {
        Ok(())
    }
}

fn completion_position_is_ignored_with_budget(
    document: &Document,
    offset: usize,
    anchor: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    if completion_position_is_conditionally_unknown(document, offset, anchor) {
        return Ok(true);
    }
    budget.require_work(
        document
            .conditionals
            .inactive_spans
            .len()
            .saturating_add(document.opaque_ranges.len()),
        cancel,
    )?;
    Ok(document
        .conditionals
        .inactive_spans
        .iter()
        .any(|span| span.start <= anchor && anchor < span.end)
        || ignored_offset_with_budget(document, anchor, cancel, budget, "completion")?
        || document
            .opaque_ranges
            .iter()
            .any(|range| range.contains_offset(anchor)))
}

fn completion_position_is_conditionally_unknown(
    document: &Document,
    offset: usize,
    anchor: usize,
) -> bool {
    document.conditionals.is_unknown_at(anchor) || document.conditionals.is_unknown_at(offset)
}

fn identifier_prefix_start_with_budget(
    source: &str,
    offset: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<usize, String> {
    let mut start = offset;
    while start > 0 {
        let Some((candidate, character)) = source[..start].char_indices().next_back() else {
            break;
        };
        if is_identifier_continue(character) {
            budget.require_bytes(start.saturating_sub(candidate), cancel)?;
            start = candidate;
        } else {
            break;
        }
    }
    if start > 0 && source.as_bytes().get(start - 1) == Some(&b'&') {
        budget.require_bytes(1, cancel)?;
        start -= 1;
    }
    Ok(start)
}

fn is_identifier_continue(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '$')
}

fn member_expression_for_completion<'a>(
    document: &'a Document,
    offset: usize,
    prefix_start: usize,
    identifier: Option<Node<'a>>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<CompletionMember<'a>, String> {
    if let Some(identifier) = identifier {
        if let Some(dot) = super::member_expression_at(identifier) {
            if super::is_right_hand_member(dot, identifier) {
                return Ok(CompletionMember::Node(dot));
            }
        }
    }

    if let Some((path, end)) =
        bare_member_path_with_budget(&document.source, offset, cancel, budget)?
    {
        return Ok(CompletionMember::Bare { path, end });
    }
    let dot_offset =
        match trailing_member_dot_with_budget(&document.source, offset, cancel, budget)? {
            TrailingMemberDot::Absent => return Ok(CompletionMember::Unqualified),
            TrailingMemberDot::Present(dot_offset) => dot_offset,
            TrailingMemberDot::GapExceeded => return Ok(CompletionMember::Unsupported),
        };

    // The parser may omit the RHS identifier while the user is typing `Obj.`.
    // In that case locate an expression whose dot is immediately before the
    // completion prefix (allowing source whitespace between the two).
    let mut result: Option<CompletionMember<'a>> = None;
    let mut cursor = document.tree.root_node().walk();
    loop {
        check_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let node = cursor.node();
        if result.is_none()
            && node.end_byte() == dot_offset
            && node.start_byte() < node.end_byte()
            && matches!(
                node.kind(),
                "identifier" | "exprCall" | "exprParens" | "exprAs" | "exprDot" | "genericDot"
            )
        {
            result = Some(CompletionMember::Expression(node));
        }
        if result.is_none() && matches!(node.kind(), "exprDot" | "genericDot") {
            if let Some(operator) = node.child_by_field_name("operator") {
                if operator.end_byte() <= prefix_start {
                    let between = document
                        .source
                        .get(operator.end_byte()..prefix_start)
                        .unwrap_or_default();
                    if between.chars().all(char::is_whitespace) {
                        let end = node
                            .child_by_field_name("rhs")
                            .map_or(operator.end_byte(), |rhs| rhs.end_byte());
                        if prefix_start >= operator.end_byte() && offset >= end {
                            result = Some(CompletionMember::Node(node));
                        }
                    }
                }
            }
        }
        if result.is_none()
            && node.kind() == "ERROR"
            && node.end_byte() == prefix_start
            && document
                .source
                .get(node.start_byte()..node.end_byte())
                .is_some_and(|text| text == ".")
        {
            if let Some(parent) = node.parent() {
                budget.require_work(parent.named_child_count(), cancel)?;
                let mut receiver = None;
                for index in 0..parent.named_child_count() {
                    let Some(candidate) = parent.named_child(index) else {
                        continue;
                    };
                    if candidate.end_byte() <= node.start_byte() {
                        receiver = Some(candidate);
                    }
                }
                if let Some(receiver) = receiver {
                    result = Some(CompletionMember::Expression(receiver));
                }
            }
        }
        if result.is_some() {
            break;
        }
        if cursor.goto_first_child() {
            continue;
        }
        if !advance_cursor(&mut cursor) {
            break;
        }
    }
    Ok(result.unwrap_or(CompletionMember::Unsupported))
}

fn trailing_member_dot_with_budget(
    source: &str,
    offset: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<TrailingMemberDot, String> {
    let mut cursor = offset.min(source.len());
    let mut gap_bytes = 0usize;
    while cursor > 0 && source.as_bytes()[cursor - 1].is_ascii_whitespace() {
        budget.require_bytes(1, cancel)?;
        gap_bytes = gap_bytes.saturating_add(1);
        if gap_bytes > MAX_TRAILING_MEMBER_DOT_GAP_BYTES {
            return Ok(TrailingMemberDot::GapExceeded);
        }
        cursor -= 1;
    }
    if cursor > 0 {
        budget.require_bytes(1, cancel)?;
    }
    Ok(
        if cursor > 0 && source.as_bytes().get(cursor - 1) == Some(&b'.') {
            TrailingMemberDot::Present(cursor - 1)
        } else {
            TrailingMemberDot::Absent
        },
    )
}

fn bare_member_path_with_budget<'a>(
    source: &'a str,
    offset: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<(&'a str, usize)>, String> {
    let TrailingMemberDot::Present(dot) =
        trailing_member_dot_with_budget(source, offset, cancel, budget)?
    else {
        return Ok(None);
    };
    let mut start = dot;
    while start > 0 {
        budget.require_bytes(1, cancel)?;
        let byte = source.as_bytes()[start - 1];
        if is_identifier_byte(byte) || byte == b'.' {
            start -= 1;
        } else {
            break;
        }
    }
    let Some(path) = source.get(start..dot) else {
        return Ok(None);
    };
    if path.is_empty()
        || path
            .split('.')
            .any(|part| part.is_empty() || !part.bytes().all(is_identifier_byte))
    {
        return Ok(None);
    }
    Ok(Some((path, dot)))
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn completion_kind(kind: SymbolKind) -> CompletionItemKind {
    match kind {
        SymbolKind::Unit => CompletionItemKind::MODULE,
        SymbolKind::Type => CompletionItemKind::CLASS,
        SymbolKind::Routine => CompletionItemKind::FUNCTION,
        SymbolKind::Variable | SymbolKind::Parameter => CompletionItemKind::VARIABLE,
        SymbolKind::Constant => CompletionItemKind::CONSTANT,
        SymbolKind::Field => CompletionItemKind::FIELD,
        SymbolKind::Property => CompletionItemKind::PROPERTY,
        SymbolKind::EnumValue => CompletionItemKind::ENUM_MEMBER,
        SymbolKind::Label => CompletionItemKind::TEXT,
    }
}

fn completion_symbol_kind_supported(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Unit
            | SymbolKind::Type
            | SymbolKind::Routine
            | SymbolKind::Variable
            | SymbolKind::Constant
            | SymbolKind::Parameter
            | SymbolKind::Field
            | SymbolKind::Property
            | SymbolKind::EnumValue
            | SymbolKind::Label
    )
}

fn private_declaration_spans(
    document: &Document,
    budget: &mut AssistanceBudget,
    cancel: &AtomicBool,
) -> Result<HashSet<Span>, String> {
    let mut spans = HashSet::new();
    let mut cursor = document.tree.root_node().walk();
    loop {
        check_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let node = cursor.node();
        if !matches!(node.kind(), "declField" | "declProc" | "declProp") {
            if cursor.goto_first_child() {
                continue;
            }
            if !advance_cursor(&mut cursor) {
                break;
            }
            continue;
        }
        let mut parent = node.parent();
        while let Some(section) = parent {
            check_cancel(cancel)?;
            if matches!(section.kind(), "declSection" | "ppDeclSection") {
                let mut restricted = false;
                for index in 0..section.named_child_count() {
                    check_cancel(cancel)?;
                    budget.require_work(1, cancel)?;
                    if section
                        .named_child(index)
                        .is_some_and(|child| matches!(child.kind(), "kPrivate" | "kProtected"))
                    {
                        restricted = true;
                        break;
                    }
                }
                if restricted {
                    spans.insert(Span::from_node(node));
                }
                break;
            }
            if matches!(section.kind(), "declType" | "defProc") {
                break;
            }
            parent = section.parent();
        }
        if cursor.goto_first_child() {
            continue;
        }
        if !advance_cursor(&mut cursor) {
            break;
        }
    }
    Ok(spans)
}

fn call_at_offset<'a>(
    document: &'a Document,
    offset: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Node<'a>>, String> {
    let mut calls = Vec::new();
    let mut visited = 0usize;
    let mut current = document.tree.root_node();
    loop {
        check_cancel(cancel)?;
        visited = visited.saturating_add(1);
        if visited > MAX_SIGNATURE_NODES {
            return Err(format!(
                "signature help exceeds the {MAX_SIGNATURE_NODES}-node traversal limit"
            ));
        }
        budget.require_work(1, cancel)?;
        if !node_contains_cursor(current, offset) {
            break;
        }
        if current.kind() == "exprCall" {
            if let Some(entity) = current.child_by_field_name("entity") {
                if let Some(open) = source_open_paren(document, entity, current, cancel, budget)? {
                    if offset >= open.saturating_add(1) {
                        calls.push((current, open));
                    }
                }
            }
        }

        let mut child = None;
        let mut cursor = current.walk();
        for candidate in current.named_children(&mut cursor) {
            check_cancel(cancel)?;
            visited = visited.saturating_add(1);
            if visited > MAX_SIGNATURE_NODES {
                return Err(format!(
                    "signature help exceeds the {MAX_SIGNATURE_NODES}-node traversal limit"
                ));
            }
            budget.require_work(1, cancel)?;
            if node_contains_cursor(candidate, offset) {
                child = Some(candidate);
                break;
            }
        }
        let Some(next) = child else {
            break;
        };
        current = next;
    }

    for (call, open) in calls.into_iter().rev() {
        if !call_has_closed_before_offset(document, open, offset, cancel, budget)? {
            return Ok(Some(call));
        }
    }
    Ok(None)
}

fn node_contains_cursor(node: Node<'_>, offset: usize) -> bool {
    Span::from_node(node).contains_offset(offset)
        || (node.kind() == "exprCall" && node.end_byte() == offset)
}

fn advance_cursor(cursor: &mut tree_sitter::TreeCursor<'_>) -> bool {
    loop {
        if cursor.goto_next_sibling() {
            return true;
        }
        if !cursor.goto_parent() {
            return false;
        }
    }
}

fn source_open_paren(
    document: &Document,
    entity: Node<'_>,
    call: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<usize>, String> {
    let start = entity.end_byte();
    let end = call
        .end_byte()
        .min(document.conditionals.projected_source.len());
    let scan_end = end.min(start.saturating_add(MAX_SIGNATURE_SCAN_BYTES));
    let Some(bytes) = document
        .conditionals
        .projected_source
        .as_bytes()
        .get(start..scan_end)
    else {
        return Ok(None);
    };
    budget.require_bytes(bytes.len(), cancel)?;
    #[cfg(test)]
    record_assistance_helper_entry(&SOURCE_SCAN_HELPER_ENTRIES);
    let mut index = 0;
    while index < bytes.len() {
        check_cancel(cancel)?;
        match bytes[index] {
            b'\'' => index = skip_pascal_string(bytes, index),
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = skip_line_comment(bytes, index + 2);
            }
            b'{' => index = skip_brace_comment(bytes, index + 1),
            b'(' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_paren_star_comment(bytes, index + 2);
            }
            b'(' => return Ok(Some(start + index)),
            _ => index += 1,
        }
    }
    if scan_end < end {
        return Err(format!(
            "signature help exceeds the {MAX_SIGNATURE_SCAN_BYTES}-byte scan limit"
        ));
    }
    Ok(None)
}

fn call_has_closed_before_offset(
    document: &Document,
    open: usize,
    offset: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    if open >= offset || offset > document.conditionals.projected_source.len() {
        return Ok(false);
    }
    if offset.saturating_sub(open) > MAX_SIGNATURE_SCAN_BYTES {
        return Err(format!(
            "signature help exceeds the {MAX_SIGNATURE_SCAN_BYTES}-byte scan limit"
        ));
    }

    let bytes = document.conditionals.projected_source.as_bytes();
    let scan_start = open.saturating_add(1);
    budget.require_bytes(offset.saturating_sub(scan_start), cancel)?;
    #[cfg(test)]
    record_assistance_helper_entry(&SOURCE_SCAN_HELPER_ENTRIES);
    let mut index = open.saturating_add(1);
    let mut parentheses = 0usize;
    while index < offset {
        check_cancel(cancel)?;
        match bytes[index] {
            b'\'' => index = skip_pascal_string(bytes, index),
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = skip_line_comment(bytes, index + 2);
            }
            b'{' => index = skip_brace_comment(bytes, index + 1),
            b'(' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_paren_star_comment(bytes, index + 2);
            }
            b'(' => {
                parentheses = parentheses.saturating_add(1);
                if parentheses > MAX_SIGNATURE_NESTING {
                    return Err(format!(
                        "signature help exceeds the {MAX_SIGNATURE_NESTING}-level nesting limit"
                    ));
                }
                index += 1;
            }
            b')' => {
                if parentheses == 0 {
                    return Ok(true);
                }
                parentheses -= 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    Ok(false)
}

fn conditional_argument_is_unknown(document: &Document, open: usize, offset: usize) -> bool {
    let start = open.saturating_add(1);
    document
        .conditionals
        .unknown_spans
        .iter()
        .any(|span| span.start < offset && span.end > start)
}

fn argument_index(
    source: &str,
    open: usize,
    offset: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<usize>, String> {
    if open >= offset || offset > source.len() {
        return Ok(None);
    }
    if offset.saturating_sub(open) > MAX_SIGNATURE_SCAN_BYTES {
        return Err(format!(
            "signature help exceeds the {MAX_SIGNATURE_SCAN_BYTES}-byte scan limit"
        ));
    }

    let bytes = source.as_bytes();
    let scan_start = open.saturating_add(1);
    budget.require_bytes(offset.saturating_sub(scan_start), cancel)?;
    #[cfg(test)]
    record_assistance_helper_entry(&SOURCE_SCAN_HELPER_ENTRIES);
    let mut index = open + 1;
    let mut parentheses = 0usize;
    let mut brackets = 0usize;
    let mut parameter = 0usize;
    while index < offset {
        check_cancel(cancel)?;
        let byte = bytes[index];
        match byte {
            b'\'' => {
                index = skip_pascal_string(bytes, index);
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = skip_line_comment(bytes, index + 2);
            }
            b'{' => {
                index = skip_brace_comment(bytes, index + 1);
            }
            b'(' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_paren_star_comment(bytes, index + 2);
            }
            b'(' => {
                parentheses = parentheses.saturating_add(1);
                if parentheses > MAX_SIGNATURE_NESTING {
                    return Err(format!(
                        "signature help exceeds the {MAX_SIGNATURE_NESTING}-level nesting limit"
                    ));
                }
                index += 1;
            }
            b')' => {
                if parentheses == 0 {
                    return Ok(Some(parameter));
                }
                parentheses -= 1;
                index += 1;
            }
            b'[' => {
                brackets = brackets.saturating_add(1);
                if brackets > MAX_SIGNATURE_NESTING {
                    return Err(format!(
                        "signature help exceeds the {MAX_SIGNATURE_NESTING}-level nesting limit"
                    ));
                }
                index += 1;
            }
            b']' => {
                brackets = brackets.saturating_sub(1);
                index += 1;
            }
            b',' if parentheses == 0 && brackets == 0 => {
                parameter = parameter.saturating_add(1);
                index += 1;
            }
            _ => index += 1,
        }
    }
    Ok(Some(parameter))
}

fn skip_pascal_string(bytes: &[u8], mut index: usize) -> usize {
    index += 1;
    while index < bytes.len() {
        if bytes[index] != b'\'' {
            index += 1;
            continue;
        }
        if bytes.get(index + 1) == Some(&b'\'') {
            index += 2;
        } else {
            return index + 1;
        }
    }
    bytes.len()
}

fn skip_line_comment(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index] != b'\n' && bytes[index] != b'\r' {
        index += 1;
    }
    index
}

fn skip_brace_comment(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() {
        if bytes[index] == b'}' {
            return index + 1;
        }
        index += 1;
    }
    bytes.len()
}

fn skip_paren_star_comment(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() {
        if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b')') {
            return index + 2;
        }
        index += 1;
    }
    bytes.len()
}

fn routine_signature_label(
    document: &Document,
    symbol: &Symbol,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<(String, usize)>, String> {
    let Some(header_span) = symbol.routine_header_span else {
        return Ok(None);
    };
    let Some(raw_header) = document.source.get(header_span.start..header_span.end) else {
        return Ok(None);
    };
    if raw_header.len() > MAX_SIGNATURE_LABEL_BYTES {
        return Err(format!(
            "signature help exceeds the {MAX_SIGNATURE_LABEL_BYTES}-byte label limit"
        ));
    }
    let Some(end) =
        routine_header_end_with_cancel(document, header_span, Some(cancel), Some(budget))?
    else {
        return Ok(None);
    };
    let Some(raw) = document.source.get(header_span.start..end) else {
        return Ok(None);
    };
    let label = raw.trim();
    if label.len() > MAX_SIGNATURE_LABEL_BYTES {
        return Err(format!(
            "signature help exceeds the {MAX_SIGNATURE_LABEL_BYTES}-byte label limit"
        ));
    }
    let label_start = header_span
        .start
        .saturating_add(raw.len() - raw.trim_start().len());
    Ok(Some((label.to_owned(), label_start)))
}

fn parameter_information_with_budget(
    symbol: &Symbol,
    label: &str,
    label_start: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Vec<ParameterInformation>>, String> {
    let parameter_count = symbol.routine_parameter_spans.len();
    budget.require_work(parameter_count, cancel)?;
    budget.require_bytes(
        label
            .len()
            .saturating_mul(2)
            .saturating_mul(parameter_count),
        cancel,
    )?;
    #[cfg(test)]
    record_assistance_helper_entry(&PARAMETER_INFORMATION_HELPER_ENTRIES);

    let mut ranges = Vec::with_capacity(parameter_count);
    for span in &symbol.routine_parameter_spans {
        check_cancel(cancel)?;
        let Some(start) = span.start.checked_sub(label_start) else {
            return Ok(None);
        };
        let Some(end) = span.end.checked_sub(label_start) else {
            return Ok(None);
        };
        let Some(parameter) = label.get(start..end) else {
            return Ok(None);
        };
        if parameter.is_empty() {
            return Ok(None);
        }
        if ranges
            .last()
            .is_some_and(|(_, previous_end)| start < *previous_end)
        {
            return Ok(None);
        }
        ranges.push((start, end));
    }

    let mut offsets = vec![[0usize; 2]; parameter_count];
    let mut next_start = 0usize;
    let mut next_end = 0usize;
    let mut utf16_offset = 0usize;
    #[cfg(test)]
    record_assistance_helper_entry(&PARAMETER_UTF16_SCAN_PASSES);
    for (byte_offset, character) in label.char_indices() {
        check_cancel(cancel)?;
        while next_start < parameter_count && ranges[next_start].0 == byte_offset {
            offsets[next_start][0] = utf16_offset;
            next_start += 1;
        }
        while next_end < parameter_count && ranges[next_end].1 == byte_offset {
            offsets[next_end][1] = utf16_offset;
            next_end += 1;
        }
        utf16_offset = utf16_offset.saturating_add(character.len_utf16());
    }
    while next_start < parameter_count && ranges[next_start].0 == label.len() {
        offsets[next_start][0] = utf16_offset;
        next_start += 1;
    }
    while next_end < parameter_count && ranges[next_end].1 == label.len() {
        offsets[next_end][1] = utf16_offset;
        next_end += 1;
    }
    if next_start != parameter_count || next_end != parameter_count {
        return Ok(None);
    }

    let mut result = Vec::with_capacity(parameter_count);
    for [start_utf16, end_utf16] in offsets {
        result.push(ParameterInformation {
            label: ParameterLabel::LabelOffsets([
                u32::try_from(start_utf16).ok().ok_or_else(|| {
                    "signature parameter UTF-16 offset exceeds the u32 limit".to_string()
                })?,
                u32::try_from(end_utf16).ok().ok_or_else(|| {
                    "signature parameter UTF-16 offset exceeds the u32 limit".to_string()
                })?,
            ]),
            documentation: None,
        });
    }
    Ok(Some(result))
}

fn candidate_rank(symbol: &Symbol) -> (u8, u8, usize) {
    let origin = match symbol.origin {
        Origin::Declaration => 0,
        Origin::Definition => 1,
    };
    let region = match symbol.region {
        Region::Interface => 0,
        Region::Implementation => 1,
        Region::Other => 2,
    };
    (origin, region, symbol.declaration_span.start)
}

fn unsupported_hover_context_with_budget(
    document: &Document,
    identifier: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let span = Span::from_node(identifier);
    budget.require_work(
        document
            .opaque_ranges
            .len()
            .saturating_add(document.parser_recovery_spans.len()),
        cancel,
    )?;
    if document
        .opaque_ranges
        .iter()
        .any(|range| range.contains(span))
        || document
            .parser_recovery_spans
            .iter()
            .any(|recovery| recovery.start <= span.end && recovery.end >= span.start)
    {
        return Ok(true);
    }
    for ancestor in Ancestors::new(identifier) {
        check_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if matches!(ancestor.kind(), "ppDirective" | "with" | "inherited") {
            return Ok(true);
        }
    }
    Ok(false)
}

struct Ancestors<'a> {
    current: Option<Node<'a>>,
}

impl<'a> Ancestors<'a> {
    fn new(node: Node<'a>) -> Self {
        Self {
            current: node.parent(),
        }
    }
}

impl<'a> Iterator for Ancestors<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.current?;
        self.current = current.parent();
        Some(current)
    }
}

fn unsupported_context_at(
    document: &Document,
    offset: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
    max_nodes: usize,
    operation: &str,
) -> Result<bool, String> {
    let Some(node) = node_at_offset(
        document.tree.root_node(),
        offset,
        cancel,
        budget,
        max_nodes,
        operation,
    )?
    else {
        return Ok(false);
    };
    budget.require_work(
        document
            .opaque_ranges
            .len()
            .saturating_add(document.parser_recovery_spans.len()),
        cancel,
    )?;
    if document
        .opaque_ranges
        .iter()
        .any(|range| range.contains(Span::from_node(node)))
    {
        return Ok(true);
    }
    for ancestor in Ancestors::new(node) {
        check_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if matches!(ancestor.kind(), "ppDirective" | "with" | "inherited") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn node_at_offset<'a>(
    root: Node<'a>,
    offset: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
    max_nodes: usize,
    operation: &str,
) -> Result<Option<Node<'a>>, String> {
    if !Span::from_node(root).contains_offset(offset) {
        return Ok(None);
    }
    let mut current = root;
    let mut visited = 0usize;
    loop {
        check_cancel(cancel)?;
        let mut cursor = current.walk();
        let mut child = None;
        for candidate in current.named_children(&mut cursor) {
            check_cancel(cancel)?;
            visited = visited.saturating_add(1);
            if visited > max_nodes {
                return Err(format!(
                    "{operation} exceeds the {max_nodes}-node traversal limit"
                ));
            }
            budget.require_work(1, cancel)?;
            if Span::from_node(candidate).contains_offset(offset) {
                child = Some(candidate);
                break;
            }
        }
        let Some(child) = child else {
            return Ok(Some(current));
        };
        current = child;
    }
}

pub(super) fn identifier_at_with_budget<'a>(
    root: Node<'a>,
    offset: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
    _operation: &str,
) -> Result<Option<Node<'a>>, String> {
    let max_nodes = if _operation == "signature help" {
        MAX_SIGNATURE_NODES
    } else {
        MAX_COMPLETION_CONTEXT_NODES
    };
    Ok(
        node_at_offset(root, offset, cancel, budget, max_nodes, _operation)?
            .filter(|node| node.kind() == "identifier"),
    )
}

fn ignored_offset_with_budget(
    document: &Document,
    offset: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
    _operation: &str,
) -> Result<bool, String> {
    let max_nodes = if _operation == "signature help" {
        MAX_SIGNATURE_NODES
    } else {
        MAX_COMPLETION_CONTEXT_NODES
    };
    let Some(node) = node_at_offset(
        document.tree.root_node(),
        offset,
        cancel,
        budget,
        max_nodes,
        _operation,
    )?
    else {
        return Ok(false);
    };
    Ok(matches!(
        node.kind(),
        "comment" | "literalString" | "literalChar"
    ))
}

fn declaration_excerpt(
    document: &Document,
    symbol: &Symbol,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<String>, String> {
    if symbol.kind == SymbolKind::Unit {
        let excerpt = format!("unit {};", display_unit_name(document));
        budget.require_bytes(excerpt.len(), cancel)?;
        return Ok(Some(excerpt));
    }
    let Some(raw) = (match symbol.kind {
        SymbolKind::Routine => routine_excerpt(document, symbol, cancel, budget),
        SymbolKind::Type => type_excerpt(document, symbol),
        _ => source_excerpt(document, symbol.declaration_span),
    }) else {
        return Ok(None);
    };
    let Some(excerpt) = bounded_source(raw) else {
        return Ok(None);
    };
    budget.require_bytes(excerpt.len(), cancel)?;
    if symbol.kind == SymbolKind::Type && !starts_with_keyword(&excerpt, "type") {
        let value = format!("type {excerpt}");
        budget.require_bytes(value.len(), cancel)?;
        Ok(bounded_string(value))
    } else {
        Ok(Some(excerpt))
    }
}

fn routine_excerpt<'a>(
    document: &'a Document,
    symbol: &Symbol,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Option<&'a str> {
    let mut span = symbol
        .routine_header_span
        .unwrap_or(symbol.declaration_span);
    span.end = routine_header_end_with_cancel(document, span, Some(cancel), Some(budget))
        .ok()
        .flatten()?;
    source_excerpt(document, span)
}

fn routine_header_end_with_cancel(
    document: &Document,
    span: Span,
    cancel: Option<&AtomicBool>,
    budget: Option<&mut AssistanceBudget>,
) -> Result<Option<usize>, String> {
    let Some(source) = document.source.as_bytes().get(span.start..span.end) else {
        return Ok(None);
    };
    if source.len() > MAX_SIGNATURE_LABEL_BYTES {
        return Ok(None);
    }
    let mut budget = budget;
    let no_cancel = AtomicBool::new(false);
    if let Some(budget) = budget.as_mut() {
        budget.require_bytes(source.len(), cancel.unwrap_or(&no_cancel))?;
    }
    #[cfg(test)]
    record_assistance_helper_entry(&SOURCE_SCAN_HELPER_ENTRIES);
    let mut index = 0;
    let mut parentheses = 0usize;
    let mut brackets = 0usize;
    while index < source.len() {
        if let Some(cancel) = cancel {
            check_cancel(cancel)?;
        }
        match source[index] {
            b'\'' => index = skip_pascal_string(source, index),
            b'/' if source.get(index + 1) == Some(&b'/') => {
                index = skip_line_comment(source, index + 2);
            }
            b'{' => index = skip_brace_comment(source, index + 1),
            b'(' if source.get(index + 1) == Some(&b'*') => {
                index = skip_paren_star_comment(source, index + 2);
            }
            b'(' => {
                parentheses = parentheses.saturating_add(1);
                index += 1;
            }
            b')' => {
                parentheses = parentheses.saturating_sub(1);
                index += 1;
            }
            b'[' => {
                brackets = brackets.saturating_add(1);
                index += 1;
            }
            b']' => {
                brackets = brackets.saturating_sub(1);
                index += 1;
            }
            b';' if parentheses == 0 && brackets == 0 => {
                return Ok(Some(span.start.saturating_add(index).saturating_add(1)));
            }
            _ => index += 1,
        }
    }
    Ok(None)
}

fn type_excerpt<'a>(document: &'a Document, symbol: &Symbol) -> Option<&'a str> {
    let end = symbol
        .type_excerpt_end
        .unwrap_or(symbol.declaration_span.end);
    source_excerpt(
        document,
        Span {
            start: symbol.declaration_span.start,
            end,
        },
    )
}

fn source_excerpt(document: &Document, span: Span) -> Option<&str> {
    document.source.get(span.start..span.end)
}

fn bounded_source(source: &str) -> Option<String> {
    let source = source.trim();
    (source.len() <= MAX_HOVER_EXCERPT_BYTES).then(|| source.to_owned())
}

fn bounded_string(value: String) -> Option<String> {
    (value.len() <= MAX_HOVER_EXCERPT_BYTES).then_some(value)
}

#[cfg(test)]
fn render_displays(displays: &[DeclarationDisplay], format: MarkupKind) -> Result<String, String> {
    let mut value = String::new();
    for (index, display) in displays.iter().enumerate() {
        if index != 0 {
            value.push_str("\n\n");
        }
        match format {
            MarkupKind::Markdown => {
                let fence = markdown_fence(&display.excerpt);
                value.push('`');
                value.push_str(&display.context);
                value.push_str("`\n\n");
                value.push_str(&fence);
                value.push_str("pascal\n");
                value.push_str(&display.excerpt);
                value.push('\n');
                value.push_str(&fence);
            }
            MarkupKind::PlainText => {
                value.push_str(&display.context);
                value.push('\n');
                value.push_str(&display.excerpt);
            }
        }
        if value.len() > MAX_HOVER_VALUE_BYTES {
            return Err(format!(
                "hover result exceeds the {MAX_HOVER_VALUE_BYTES}-byte limit"
            ));
        }
    }
    Ok(value)
}

fn render_displays_with_budget(
    displays: &[DeclarationDisplay],
    format: MarkupKind,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<String, String> {
    let mut value = String::new();
    for (index, display) in displays.iter().enumerate() {
        check_cancel(cancel)?;
        let mut rendered = String::new();
        if index != 0 {
            rendered.push_str("\n\n");
        }
        match format {
            MarkupKind::Markdown => {
                let fence = markdown_fence(&display.excerpt);
                rendered.push('`');
                rendered.push_str(&display.context);
                rendered.push_str("`\n\n");
                rendered.push_str(&fence);
                rendered.push_str("pascal\n");
                rendered.push_str(&display.excerpt);
                rendered.push('\n');
                rendered.push_str(&fence);
            }
            MarkupKind::PlainText => {
                rendered.push_str(&display.context);
                rendered.push('\n');
                rendered.push_str(&display.excerpt);
            }
        }
        budget.require_bytes(rendered.len(), cancel)?;
        value.push_str(&rendered);
        if value.len() > MAX_HOVER_VALUE_BYTES {
            return Err(format!(
                "hover result exceeds the {MAX_HOVER_VALUE_BYTES}-byte limit"
            ));
        }
    }
    Ok(value)
}

fn markdown_fence(excerpt: &str) -> String {
    let mut longest: usize = 0;
    let mut current: usize = 0;
    for byte in excerpt.bytes() {
        if byte == b'`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat(longest.saturating_add(1).max(3))
}

fn starts_with_keyword(value: &str, keyword: &str) -> bool {
    value
        .split_whitespace()
        .next()
        .is_some_and(|first| first.eq_ignore_ascii_case(keyword))
}

fn display_unit_name(document: &Document) -> String {
    document.unit_display_name.clone()
}

#[cfg(test)]
mod tests {
    use super::{
        AssistanceBudget, COMPLETION_SYMBOL_VISITS, Candidate, CompletionAccumulator,
        DeclarationDisplay, MAX_COMPLETION_SCANNED_SYMBOLS, MAX_HOVER_VALUE_BYTES, MarkupKind,
        NavigationIndex, Origin, PARAMETER_INFORMATION_HELPER_ENTRIES, PARAMETER_UTF16_SCAN_PASSES,
        ParameterLabel, Region, SOURCE_SCAN_HELPER_ENTRIES, Span, Symbol, SymbolKind,
        identifier_at_with_budget, named_type_path_for_symbol, parameter_information_with_budget,
        render_displays, routine_signature_label,
    };
    use crate::navigation::{
        ROOT_SCOPE, RoutineKind, TEST_EXPORTED_INDEX_VECTOR_MATERIALIZATIONS,
        TEST_LEGACY_EXPORTED_MATERIALIZATIONS, TEST_MEMBER_INDEX_VECTOR_MATERIALIZATIONS,
        TEST_TYPE_INDEX_VECTOR_MATERIALIZATIONS, TEST_UNIT_URL_VECTOR_MATERIALIZATIONS, TypeKind,
        test_materialization_count, test_reset_materialization_counters,
    };
    use crate::text;
    use lsp_types::Url;
    use std::cell::Cell;
    use std::fmt::Write as _;

    fn display(excerpt: String) -> DeclarationDisplay {
        DeclarationDisplay {
            context: "U".to_owned(),
            excerpt,
            source_uri: Url::parse("file:///U.pas").expect("test URI"),
            source_start: 0,
        }
    }

    #[test]
    fn markdown_fence_is_longer_than_backtick_runs_in_the_excerpt() {
        let rendered = render_displays(
            &[display("before\n```\nafter".to_owned())],
            MarkupKind::Markdown,
        )
        .expect("bounded Markdown hover");
        let opening = rendered
            .lines()
            .find(|line| line.ends_with("pascal"))
            .expect("Pascal code fence");
        let fence = opening.strip_suffix("pascal").expect("fence prefix");
        assert_eq!(fence.len(), 4);
        assert_eq!(rendered.lines().last(), Some(fence));
    }

    #[test]
    fn markdown_fence_overhead_is_included_in_the_combined_bound() {
        let fence_len = 4;
        let overhead = "U".len() + 12 + 2 * fence_len;
        let exact_excerpt_len = MAX_HOVER_VALUE_BYTES - overhead;
        let exact_excerpt = format!("{}\n```", "x".repeat(exact_excerpt_len.saturating_sub(4)));
        let exact = render_displays(&[display(exact_excerpt)], MarkupKind::Markdown)
            .expect("exactly bounded Markdown hover");
        assert_eq!(exact.len(), MAX_HOVER_VALUE_BYTES);

        let overflowing_excerpt_len = exact_excerpt_len + 1;
        let overflowing_excerpt = format!(
            "{}\n```",
            "x".repeat(overflowing_excerpt_len.saturating_sub(4))
        );
        assert!(
            render_displays(&[display(overflowing_excerpt)], MarkupKind::Markdown).is_err(),
            "Markdown delimiter overhead must not exceed the combined bound"
        );
    }

    fn completion_symbol(key: &str) -> Symbol {
        let span = Span { start: 0, end: 1 };
        Symbol {
            span,
            declaration_span: span,
            selection_span: span,
            name: key.to_owned(),
            key: key.to_owned(),
            kind: SymbolKind::Variable,
            type_kind: TypeKind::Other,
            routine_kind: RoutineKind::Procedure,
            scope: 0,
            owner_type: None,
            owner_type_name: None,
            type_name: None,
            result_type_name: None,
            result_type_span: None,
            region: Region::Interface,
            origin: Origin::Declaration,
            local_only: false,
            routine_key: None,
            routine_signature: None,
            routine_header_span: None,
            routine_parameter_spans: Vec::new(),
            type_excerpt_end: None,
            body_scope: None,
            unresolved_abbreviated: false,
            accessor: None,
        }
    }

    fn budget_lookup_index() -> (NavigationIndex, Url, Url) {
        let provider_uri = Url::parse("file:///BudgetProvider.pas").expect("provider URI");
        let consumer_uri = Url::parse("file:///BudgetConsumer.pas").expect("consumer URI");
        let provider = "unit BudgetProvider;\ninterface\ntype\n  TWidget = class\n    procedure Member;\n  end;\nconst\n  Exported = 1;\nimplementation\nprocedure TWidget.Member;\nbegin\nend;\nend.\n";
        let consumer =
            "unit BudgetConsumer;\ninterface\nuses BudgetProvider;\nimplementation\nend.\n";
        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.to_owned())
            .expect("budget provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("budget consumer parses");
        index.bind_imports(
            &consumer_uri,
            [("BudgetProvider".to_owned(), provider_uri.clone())],
        );
        (index, provider_uri, consumer_uri)
    }

    #[test]
    fn cached_lookup_budget_is_checked_before_vector_materialization() {
        let (index, provider_uri, consumer_uri) = budget_lookup_index();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let provider = index
            .documents
            .get(&provider_uri)
            .expect("provider document");
        let exported_len = provider
            .symbol_indices_by_scope_key
            .get(&(ROOT_SCOPE, "exported".to_owned()))
            .expect("exported index bucket")
            .len();
        let type_len = provider
            .type_symbol_indices
            .get("twidget")
            .expect("type index bucket")
            .len();
        let member_len = provider
            .member_symbol_indices
            .get(&("twidget".to_owned(), "member".to_owned()))
            .expect("member index bucket")
            .len();
        assert_eq!(exported_len, 1);
        assert_eq!(type_len, 1);
        assert!(member_len > 0);

        test_reset_materialization_counters();
        let mut exported_budget = AssistanceBudget::new(0, 4096, "exported lookup");
        assert!(
            index
                .exported_references_for_key_with_budget(
                    &provider_uri,
                    "exported",
                    &cancel,
                    &mut exported_budget,
                )
                .is_err()
        );
        assert_eq!(
            test_materialization_count(&TEST_EXPORTED_INDEX_VECTOR_MATERIALIZATIONS),
            0,
            "zero remaining work must reject before copying the exported bucket",
        );

        let mut type_budget = AssistanceBudget::new(0, 4096, "type lookup");
        assert!(
            index
                .type_candidates_in_unit_with_budget(
                    &provider_uri,
                    "TWidget",
                    true,
                    &cancel,
                    &mut type_budget,
                )
                .is_err()
        );
        assert_eq!(
            test_materialization_count(&TEST_TYPE_INDEX_VECTOR_MATERIALIZATIONS),
            0,
            "zero remaining work must reject before copying the type bucket",
        );

        let mut member_budget = AssistanceBudget::new(0, 4096, "member lookup");
        assert!(
            index
                .member_references_for_type_with_budget(
                    &provider_uri,
                    "twidget",
                    ROOT_SCOPE,
                    "member",
                    true,
                    &cancel,
                    &mut member_budget,
                )
                .is_err()
        );
        assert_eq!(
            test_materialization_count(&TEST_MEMBER_INDEX_VECTOR_MATERIALIZATIONS),
            0,
            "zero remaining work must reject before copying the member bucket",
        );

        let consumer = index
            .documents
            .get(&consumer_uri)
            .expect("consumer document");
        let mut unit_budget = AssistanceBudget::new(0, 4096, "unit lookup");
        assert!(
            index
                .unit_references_with_budget(consumer, "BudgetProvider", &cancel, &mut unit_budget)
                .is_err()
        );
        assert_eq!(
            test_materialization_count(&TEST_UNIT_URL_VECTOR_MATERIALIZATIONS),
            0,
            "zero remaining work must reject before copying imported unit URLs",
        );
    }

    #[test]
    fn cached_lookup_budget_accepts_exact_bounds_and_rejects_exhausted_remaining_work() {
        let (index, provider_uri, consumer_uri) = budget_lookup_index();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let provider = index
            .documents
            .get(&provider_uri)
            .expect("provider document");
        let exported_len = provider
            .symbol_indices_by_scope_key
            .get(&(ROOT_SCOPE, "exported".to_owned()))
            .expect("exported index bucket")
            .len();
        let type_len = provider
            .type_symbol_indices
            .get("twidget")
            .expect("type index bucket")
            .len();
        let member_len = provider
            .member_symbol_indices
            .get(&("twidget".to_owned(), "member".to_owned()))
            .expect("member index bucket")
            .len();
        let consumer = index
            .documents
            .get(&consumer_uri)
            .expect("consumer document");

        test_reset_materialization_counters();
        let mut exported_budget = AssistanceBudget::new(exported_len, 4096, "exported lookup");
        assert_eq!(
            index
                .exported_references_for_key_with_budget(
                    &provider_uri,
                    "exported",
                    &cancel,
                    &mut exported_budget,
                )
                .expect("exact exported bound")
                .len(),
            exported_len,
        );
        assert_eq!(
            test_materialization_count(&TEST_EXPORTED_INDEX_VECTOR_MATERIALIZATIONS),
            1,
            "the exported vector may be materialized only after its bound is charged",
        );

        test_reset_materialization_counters();
        let mut type_budget = AssistanceBudget::new(type_len, 4096, "type lookup");
        assert_eq!(
            index
                .type_candidates_in_unit_with_budget(
                    &provider_uri,
                    "TWidget",
                    true,
                    &cancel,
                    &mut type_budget,
                )
                .expect("exact type bound")
                .len(),
            type_len,
        );
        assert_eq!(
            test_materialization_count(&TEST_TYPE_INDEX_VECTOR_MATERIALIZATIONS),
            1,
            "the type vector may be materialized only after its bound is charged",
        );

        test_reset_materialization_counters();
        let mut member_budget = AssistanceBudget::new(member_len, 4096, "member lookup");
        assert_eq!(
            index
                .member_references_for_type_with_budget(
                    &provider_uri,
                    "twidget",
                    ROOT_SCOPE,
                    "member",
                    true,
                    &cancel,
                    &mut member_budget,
                )
                .expect("exact member bound")
                .len(),
            member_len,
        );
        assert_eq!(
            test_materialization_count(&TEST_MEMBER_INDEX_VECTOR_MATERIALIZATIONS),
            1,
            "the member vector may be materialized only after its bound is charged",
        );

        test_reset_materialization_counters();
        let mut unit_budget = AssistanceBudget::new(1, 4096, "unit lookup");
        unit_budget
            .require_work(1, &cancel)
            .expect("consume the unit dispatch allowance");
        assert!(
            index
                .unit_references_with_budget(consumer, "BudgetProvider", &cancel, &mut unit_budget)
                .is_err()
        );
        assert_eq!(
            test_materialization_count(&TEST_UNIT_URL_VECTOR_MATERIALIZATIONS),
            0,
            "an exhausted remaining budget must reject before imported URL materialization",
        );
    }

    #[test]
    fn type_definition_named_type_lookup_uses_the_budgeted_type_bucket() {
        let provider_uri = Url::parse("file:///LargeTypeProvider.pas").expect("provider URI");
        let consumer_uri = Url::parse("file:///LargeTypeConsumer.pas").expect("consumer URI");
        let mut provider = String::from(
            "unit LargeTypeProvider;\ninterface\ntype\n  TWidget = class end;\nconst\n",
        );
        for index in 0..4_096 {
            writeln!(&mut provider, "UnrelatedExport{index} = {index};")
                .expect("write unrelated export");
        }
        provider.push_str("implementation\nend.\n");
        let consumer = "unit LargeTypeConsumer;\ninterface\nuses LargeTypeProvider;\nimplementation\nprocedure Caller;\nvar\n  Item: TWidget;\nbegin\n  Item := Item;\nend;\nend.\n";

        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider)
            .expect("large type provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("large type consumer parses");
        index.bind_imports(
            &consumer_uri,
            [("LargeTypeProvider".to_owned(), provider_uri.clone())],
        );

        let cancel = std::sync::atomic::AtomicBool::new(false);
        TEST_LEGACY_EXPORTED_MATERIALIZATIONS.with(|count| count.set(0));
        let item_offset = consumer.find("  Item :=").expect("Item use") + 2;
        let locations = index
            .type_definitions_with_cancel(
                &consumer_uri,
                text::offset_to_position(consumer, item_offset).expect("Item position"),
                &cancel,
            )
            .expect("type definition request");
        assert_eq!(locations.len(), 1, "named type must still resolve");
        assert_eq!(locations[0].uri, provider_uri);
        assert_eq!(
            TEST_LEGACY_EXPORTED_MATERIALIZATIONS.with(Cell::get),
            0,
            "type definition must not materialize unrelated exported declarations"
        );

        let consumer_document = index
            .documents
            .get(&consumer_uri)
            .expect("consumer document");
        let item_symbol = consumer_document
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Item")
            .expect("Item declaration");
        let item_type =
            named_type_path_for_symbol(consumer_document, item_symbol).expect("Item named type");
        let mut lookup_budget = AssistanceBudget::new(64, 4096, "test identifier");
        let lookup_identifier = identifier_at_with_budget(
            consumer_document.tree.root_node(),
            item_symbol.span.start,
            &cancel,
            &mut lookup_budget,
            "type definition",
        )
        .expect("Item identifier lookup")
        .expect("Item identifier");

        TEST_LEGACY_EXPORTED_MATERIALIZATIONS.with(|count| count.set(0));
        let mut exhausted_budget = AssistanceBudget::new(0, 4096, "type definition");
        let mut state = super::super::ResolutionState::new();
        assert!(
            index
                .type_declaration_candidates_with_budget(
                    &consumer_uri,
                    consumer_document,
                    item_symbol.span.start,
                    &item_type,
                    lookup_identifier,
                    None,
                    &mut state,
                    &cancel,
                    &mut exhausted_budget,
                )
                .is_err(),
            "an exhausted request budget must reject the second named-type lookup"
        );
        assert_eq!(
            TEST_LEGACY_EXPORTED_MATERIALIZATIONS.with(Cell::get),
            0,
            "an exhausted request budget must reject before provider materialization"
        );

        TEST_LEGACY_EXPORTED_MATERIALIZATIONS.with(|count| count.set(0));
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let mut cancellation_budget = AssistanceBudget::new(64, 4096, "type definition");
        let mut state = super::super::ResolutionState::new();
        assert_eq!(
            index
                .type_declaration_candidates_with_budget(
                    &consumer_uri,
                    consumer_document,
                    item_symbol.span.start,
                    &item_type,
                    lookup_identifier,
                    None,
                    &mut state,
                    &cancelled,
                    &mut cancellation_budget,
                )
                .expect_err("cancelled named-type lookup"),
            "request cancelled"
        );
        assert_eq!(
            TEST_LEGACY_EXPORTED_MATERIALIZATIONS.with(Cell::get),
            0,
            "cancellation must be observed before provider materialization"
        );
    }

    #[test]
    fn signature_budget_checks_scan_and_parameter_helpers_before_entry() {
        let (index, provider_uri, _) = budget_lookup_index();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let provider = index
            .documents
            .get(&provider_uri)
            .expect("provider document");
        let symbol = provider
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Member")
            .expect("routine symbol");
        let header_span = symbol.routine_header_span.expect("routine header span");
        let header_len = header_span.end.saturating_sub(header_span.start);

        SOURCE_SCAN_HELPER_ENTRIES.with(|value| value.set(0));
        let mut zero_scan_budget =
            AssistanceBudget::new(64, header_len.saturating_sub(1), "header");
        assert!(
            routine_signature_label(provider, symbol, &cancel, &mut zero_scan_budget,).is_err()
        );
        assert_eq!(
            SOURCE_SCAN_HELPER_ENTRIES.with(|value| value.get()),
            0,
            "a short byte budget must reject before entering the header scanner",
        );

        SOURCE_SCAN_HELPER_ENTRIES.with(|value| value.set(0));
        let mut exact_scan_budget = AssistanceBudget::new(64, header_len, "header");
        assert!(
            routine_signature_label(provider, symbol, &cancel, &mut exact_scan_budget,)
                .expect("exact header scan bound")
                .is_some()
        );
        assert_eq!(
            SOURCE_SCAN_HELPER_ENTRIES.with(|value| value.get()),
            1,
            "the header scanner may enter only after its full bound is charged",
        );

        let label = "procedure Run(é: string; B: Integer);";
        let mut parameter_symbol = completion_symbol("Run");
        parameter_symbol.routine_parameter_spans = ["é: string", "B: Integer"]
            .into_iter()
            .map(|parameter| {
                let start = label.find(parameter).expect("parameter in label");
                Span {
                    start,
                    end: start + parameter.len(),
                }
            })
            .collect();
        let parameter_count = parameter_symbol.routine_parameter_spans.len();
        let utf16_scan_bound = label
            .len()
            .saturating_mul(2)
            .saturating_mul(parameter_count);

        PARAMETER_INFORMATION_HELPER_ENTRIES.with(|value| value.set(0));
        let mut zero_parameter_budget = AssistanceBudget::new(
            parameter_count.saturating_sub(1),
            utf16_scan_bound,
            "parameters",
        );
        assert!(
            parameter_information_with_budget(
                &parameter_symbol,
                label,
                0,
                &cancel,
                &mut zero_parameter_budget,
            )
            .is_err()
        );
        assert_eq!(
            PARAMETER_INFORMATION_HELPER_ENTRIES.with(|value| value.get()),
            0,
            "an exhausted remaining work budget must reject before parameter projection",
        );

        PARAMETER_INFORMATION_HELPER_ENTRIES.with(|value| value.set(0));
        PARAMETER_UTF16_SCAN_PASSES.with(|value| value.set(0));
        let mut exact_parameter_budget =
            AssistanceBudget::new(parameter_count, utf16_scan_bound, "parameters");
        let parameters = parameter_information_with_budget(
            &parameter_symbol,
            label,
            0,
            &cancel,
            &mut exact_parameter_budget,
        )
        .expect("exact parameter projection bound")
        .expect("valid parameter spans");
        assert_eq!(parameters.len(), parameter_count);
        assert_eq!(
            PARAMETER_INFORMATION_HELPER_ENTRIES.with(|value| value.get()),
            1,
            "parameter projection may enter only after work and UTF-16 scan bounds are charged",
        );
        assert_eq!(
            PARAMETER_UTF16_SCAN_PASSES.with(|value| value.get()),
            1,
            "all parameter UTF-16 offsets must use one bounded label projection",
        );
        assert!(matches!(
            parameters[0].label,
            ParameterLabel::LabelOffsets(_)
        ));
    }

    #[test]
    fn completion_uncertainty_respects_lexical_precedence() {
        let uri = Url::parse("file:///completion-accumulator.pas").expect("test URI");
        let symbol = completion_symbol("SameName");
        let candidate = Candidate { uri, index: 0 };

        let mut same_scope_budget = AssistanceBudget::new(16, 16, "test");
        let mut same_scope = CompletionAccumulator::new("same", &mut same_scope_budget);
        same_scope.mark_uncertain(&symbol.key, 2);
        same_scope.insert(candidate.clone(), &symbol, 2);
        assert!(same_scope.candidates.is_empty());
        assert_eq!(same_scope.uncertain.get(&symbol.key), Some(&2));

        let mut shadowed_budget = AssistanceBudget::new(16, 16, "test");
        let mut shadowed = CompletionAccumulator::new("same", &mut shadowed_budget);
        shadowed.insert(candidate, &symbol, 0);
        shadowed.mark_uncertain(&symbol.key, 1);
        assert!(shadowed.candidates.contains_key(&symbol.key));
        assert!(shadowed.uncertain.is_empty());
    }

    #[test]
    fn signature_rendering_uses_cached_header_spans() {
        let provider_uri = Url::parse("file:///SignatureWalkProvider.pas").expect("provider URI");
        let consumer_uri = Url::parse("file:///SignatureWalkConsumer.pas").expect("consumer URI");
        let mut provider = String::from("unit SignatureWalkProvider;\ninterface\n");
        for index in 0..16 {
            writeln!(&mut provider, "procedure Run(X: T{index}); overload;")
                .expect("write signature declaration");
        }
        provider.push_str("const\n");
        for index in 0..6_000 {
            writeln!(&mut provider, "Filler{index} = 1;").expect("write provider filler");
        }
        provider.push_str("implementation\nend.\n");
        let consumer = "unit SignatureWalkConsumer;\ninterface\nuses SignatureWalkProvider;\nimplementation\nprocedure Caller;\nbegin\n  Run(1);\nend;\nend.\n";

        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider)
            .expect("signature walk provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("signature walk consumer parses");

        let provider_document = index
            .documents
            .get(&provider_uri)
            .expect("provider document");
        assert_eq!(
            provider_document
                .symbols
                .iter()
                .filter(|symbol| symbol.kind == SymbolKind::Routine)
                .filter(|symbol| symbol.routine_header_span.is_some())
                .count(),
            16
        );
        let help = index
            .signature_help(
                &consumer_uri,
                text::offset_to_position(consumer, consumer.find("Run(1").unwrap() + 4)
                    .expect("signature walk position"),
            )
            .expect("signature walk projection")
            .expect("signature walk call");
        assert_eq!(help.signatures.len(), 16);
    }

    #[test]
    fn completion_symbol_traversal_stops_at_the_scan_limit() {
        let uri = Url::parse("file:///CompletionScanLimit.pas").expect("completion URI");
        let mut source = String::from("unit CompletionScanLimit;\ninterface\nconst\n");
        for index in 0..MAX_COMPLETION_SCANNED_SYMBOLS + 5 {
            writeln!(&mut source, "Name{index} = {index};").expect("write completion symbol");
        }
        source.push_str("implementation\nprocedure Caller;\nbegin\n  Nam;\nend;\nend.\n");

        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.clone())
            .expect("completion scan source parses");
        COMPLETION_SYMBOL_VISITS.store(0, std::sync::atomic::Ordering::Relaxed);
        let completion = index
            .completion(
                &uri,
                text::offset_to_position(&source, source.find("  Nam").unwrap() + 6)
                    .expect("completion scan position"),
            )
            .expect("completion scan projection");
        assert!(completion.is_incomplete);
        assert!(
            COMPLETION_SYMBOL_VISITS.load(std::sync::atomic::Ordering::Relaxed)
                <= MAX_COMPLETION_SCANNED_SYMBOLS,
            "completion continued enumerating symbols after its scan budget"
        );
    }
}
