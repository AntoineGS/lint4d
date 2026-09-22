use super::{
    AssistanceBudget, Candidate, Document, GenericSubstitution, NavigationIndex, ROOT_SCOPE,
    ResolutionState, Span, Symbol, SymbolKind, canonical_name, collect_nodes_matching,
    field_identifier_nodes, has_ancestor_kind, identifier_at, identifier_nodes,
    identifier_nodes_with_budget, is_ignored_offset, is_right_hand_member,
    is_unit_declaration_identifier, member_expression_at, node_text, qualified_type_path_at,
    qualified_type_path_at_with_budget, routine_name, routine_signature, use_name_at,
};
use crate::text::PositionIndex;
use lsp_types::{
    DocumentHighlight, DocumentHighlightKind, Position, PrepareRenameResponse, Range, TextEdit, Url,
};
#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use tree_sitter::Node;

const MAX_BINDING_LOCATIONS: usize = 10_000;
const MAX_HIGHLIGHT_WORK: usize = 1_000_000;
const MAX_HIGHLIGHT_BYTES: usize = 8 * 1024 * 1024;
const CANCELLATION_MESSAGE: &str = "request cancelled";

/// Shared work accounting for snapshot queries that combine several virtual
/// include contexts.  This budget bounds scanning/resolution work; callers
/// enforce the response-size limit separately after physical mapping and
/// deduplication.
#[derive(Debug)]
pub(crate) struct BindingWorkBudget {
    work: usize,
    max_work: usize,
}

impl BindingWorkBudget {
    pub(crate) fn new(max_work: usize) -> Self {
        Self { work: 0, max_work }
    }

    pub(crate) fn charge(&mut self) -> Result<(), String> {
        self.work = self
            .work
            .checked_add(1)
            .ok_or_else(|| "binding resolution work accounting overflowed".to_string())?;
        if self.work > self.max_work {
            return Err(format!(
                "binding resolution work limit ({}) reached",
                self.max_work
            ));
        }
        Ok(())
    }

    fn charge_with_cancel(&mut self, cancel: Option<&AtomicBool>) -> Result<(), String> {
        if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        self.charge()?;
        Ok(())
    }
}

fn charge_binding_work(
    work_budget: &mut Option<&mut BindingWorkBudget>,
    shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    if let Some(shared_work_budget) = shared_work_budget.as_deref_mut() {
        let fallback_cancel = AtomicBool::new(false);
        shared_work_budget.require_work(1, cancel.unwrap_or(&fallback_cancel))?;
    }
    if let Some(work_budget) = work_budget.as_deref_mut() {
        work_budget.charge_with_cancel(cancel)?;
    }
    Ok(())
}

fn charge_shared_work(
    shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
    cancel: Option<&AtomicBool>,
    amount: usize,
) -> Result<(), String> {
    if let Some(shared_work_budget) = shared_work_budget.as_deref_mut() {
        let fallback_cancel = AtomicBool::new(false);
        shared_work_budget.require_work(amount, cancel.unwrap_or(&fallback_cancel))?;
    }
    Ok(())
}

fn charge_shared_bytes(
    shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
    cancel: Option<&AtomicBool>,
    amount: usize,
) -> Result<(), String> {
    if let Some(shared_work_budget) = shared_work_budget.as_deref_mut() {
        let fallback_cancel = AtomicBool::new(false);
        shared_work_budget.require_bytes(amount, cancel.unwrap_or(&fallback_cancel))?;
    }
    Ok(())
}

fn candidate_materialization_bytes(candidates: &[Candidate]) -> usize {
    candidates.iter().fold(
        candidates
            .len()
            .saturating_mul(std::mem::size_of::<Candidate>()),
        |bytes, candidate| bytes.saturating_add(candidate.uri.as_str().len()),
    )
}

fn comparison_sort_work(length: usize) -> usize {
    if length < 2 {
        return 0;
    }
    let mut remaining = length;
    let mut depth = 0;
    while remaining > 1 {
        depth += 1;
        remaining = remaining.saturating_add(1) / 2;
    }
    length.saturating_mul(depth)
}

#[cfg(test)]
thread_local! {
    static TEST_CANCEL_AFTER_CHECKS: Cell<Option<usize>> = const { Cell::new(None) };
    static TEST_CANCEL_PHASE: Cell<Option<TestCancellationPhase>> = const { Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct TestCancellationGuard(Option<usize>);

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestCancellationPhase {
    OccurrenceCollection,
    LocationConversion,
}

#[cfg(test)]
pub(crate) struct TestCancellationPhaseGuard(Option<TestCancellationPhase>);

#[cfg(test)]
pub(crate) fn test_cancel_after_checks(checks: usize) -> TestCancellationGuard {
    let previous = TEST_CANCEL_AFTER_CHECKS.with(|budget| {
        let previous = budget.get();
        budget.set(Some(checks));
        previous
    });
    TestCancellationGuard(previous)
}

#[cfg(test)]
impl Drop for TestCancellationGuard {
    fn drop(&mut self) {
        TEST_CANCEL_AFTER_CHECKS.with(|budget| budget.set(self.0.take()));
    }
}

#[cfg(test)]
pub(crate) fn test_cancel_in_phase(phase: TestCancellationPhase) -> TestCancellationPhaseGuard {
    let previous = TEST_CANCEL_PHASE.with(|current| {
        let previous = current.get();
        current.set(Some(phase));
        previous
    });
    TestCancellationPhaseGuard(previous)
}

#[cfg(test)]
impl Drop for TestCancellationPhaseGuard {
    fn drop(&mut self) {
        TEST_CANCEL_PHASE.with(|current| current.set(self.0.take()));
    }
}

fn check_cancel(cancel: Option<&AtomicBool>) -> Result<(), String> {
    #[cfg(test)]
    let force_cancel = cancel.is_some()
        && TEST_CANCEL_AFTER_CHECKS.with(|budget| match budget.get() {
            Some(0) => {
                budget.set(None);
                true
            }
            Some(remaining) => {
                budget.set(Some(remaining.saturating_sub(1)));
                false
            }
            None => false,
        });
    #[cfg(test)]
    if force_cancel {
        if let Some(cancel) = cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }
    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        Err(CANCELLATION_MESSAGE.to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn check_cancel_at_phase(
    cancel: Option<&AtomicBool>,
    phase: TestCancellationPhase,
) -> Result<(), String> {
    if cancel.is_some() && TEST_CANCEL_PHASE.with(|current| current.get() == Some(phase)) {
        if let Some(cancel) = cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }
    check_cancel(cancel)
}

fn check_occurrence_cancel(cancel: Option<&AtomicBool>) -> Result<(), String> {
    #[cfg(test)]
    {
        check_cancel_at_phase(cancel, TestCancellationPhase::OccurrenceCollection)
    }
    #[cfg(not(test))]
    {
        check_cancel(cancel)
    }
}

fn check_location_cancel(cancel: Option<&AtomicBool>) -> Result<(), String> {
    #[cfg(test)]
    {
        check_cancel_at_phase(cancel, TestCancellationPhase::LocationConversion)
    }
    #[cfg(not(test))]
    {
        check_cancel(cancel)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SymbolId {
    uri: Url,
    span: Span,
    scope: usize,
    kind: SymbolKind,
    origin: super::Origin,
    owner_type: Option<String>,
}

#[derive(Debug, Clone)]
struct Binding {
    old_key: String,
    kind: SymbolKind,
    members: HashSet<SymbolId>,
    names: HashSet<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RenameBindingInfo {
    pub(crate) local: bool,
    pub(crate) unit: bool,
    pub(crate) names: Vec<String>,
}

#[derive(Debug, Clone)]
struct Occurrence {
    uri: Url,
    span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UnqualifiedReferenceCacheKey {
    uri: Url,
    name: String,
    scope: usize,
    region: super::Region,
    owner_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UnresolvedCallTargetCacheKey {
    uri: Url,
    name: String,
    scope: usize,
    region: super::Region,
    owner_type: Option<String>,
}

#[derive(Debug)]
struct RenamePlan {
    binding: Binding,
    selected_span: Span,
    occurrences: Vec<Occurrence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HighlightRole {
    Text,
    Read,
    Write,
}

impl HighlightRole {
    fn document_kind(self) -> DocumentHighlightKind {
        match self {
            Self::Text => DocumentHighlightKind::TEXT,
            Self::Read => DocumentHighlightKind::READ,
            Self::Write => DocumentHighlightKind::WRITE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BindingGroup {
    Symbol(SymbolId),
    Routine { uri: Url, key: String },
    Parameter(ParameterBindingKey),
}

struct BindingLocationOptions<'a> {
    document_uri: Option<&'a Url>,
    strict_resolution: bool,
    allow_unit: bool,
    cancel: Option<&'a AtomicBool>,
    result_limit: Option<usize>,
    work_budget: Option<&'a mut BindingWorkBudget>,
    shared_work_budget: Option<&'a mut super::AssistanceBudget>,
}

struct OccurrenceCollectionOptions<'a> {
    document_uri: Option<&'a Url>,
    include_declaration: bool,
    strict_resolution: bool,
    result_limit: Option<usize>,
    cancel: Option<&'a AtomicBool>,
    work_budget: Option<&'a mut BindingWorkBudget>,
    shared_work_budget: Option<&'a mut super::AssistanceBudget>,
}

impl NavigationIndex {
    /// Return deduplicated, binding-resolved source locations for an
    /// identifier, optionally including every declaration site.
    pub fn binding_locations(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        include_declaration: bool,
    ) -> Result<Vec<lsp_types::Location>, String> {
        let mut semantic_budget = super::AssistanceBudget::new(
            MAX_HIGHLIGHT_WORK,
            MAX_HIGHLIGHT_BYTES,
            "binding references",
        );
        self.binding_locations_impl(
            uri,
            position,
            include_declaration,
            BindingLocationOptions {
                document_uri: None,
                strict_resolution: true,
                allow_unit: true,
                cancel: None,
                result_limit: Some(MAX_BINDING_LOCATIONS),
                work_budget: None,
                shared_work_budget: Some(&mut semantic_budget),
            },
        )
    }

    /// Resolve one virtual context without applying the public physical result
    /// cap.  Include expansion can produce repeated virtual occurrences that
    /// map back to one physical range; the workspace wrapper applies the
    /// response limit only after that exact mapping and deduplication.
    pub(crate) fn binding_locations_with_cancel_and_work_budget(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        include_declaration: bool,
        cancel: &AtomicBool,
        work_budget: &mut BindingWorkBudget,
        shared_work_budget: &mut super::AssistanceBudget,
    ) -> Result<Vec<lsp_types::Location>, String> {
        self.binding_locations_impl(
            uri,
            position,
            include_declaration,
            BindingLocationOptions {
                document_uri: None,
                strict_resolution: true,
                allow_unit: true,
                cancel: Some(cancel),
                result_limit: None,
                work_budget: Some(work_budget),
                shared_work_budget: Some(shared_work_budget),
            },
        )
    }

    /// Return binding-resolved highlights for one document.  The occurrence
    /// walk is shared with references; only the final role classification is
    /// specific to highlights.
    #[allow(dead_code)]
    pub(crate) fn binding_highlights_in_document_with_cancel_and_work_budget(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        cancel: &AtomicBool,
        work_budget: &mut BindingWorkBudget,
    ) -> Result<Vec<DocumentHighlight>, String> {
        let mut semantic_budget = super::AssistanceBudget::new(
            MAX_HIGHLIGHT_WORK,
            MAX_HIGHLIGHT_BYTES,
            "document highlights",
        );
        self.binding_highlights_in_document_with_cancel_and_work_budget_and_shared_budget(
            uri,
            position,
            cancel,
            work_budget,
            &mut semantic_budget,
        )
    }

    pub(crate) fn binding_highlights_in_document_with_cancel_and_work_budget_and_shared_budget(
        &self,
        uri: &Url,
        position: Position,
        cancel: &AtomicBool,
        work_budget: &mut BindingWorkBudget,
        semantic_budget: &mut super::AssistanceBudget,
    ) -> Result<Vec<DocumentHighlight>, String> {
        check_cancel(Some(cancel))?;
        let document = self
            .documents
            .get(uri)
            .ok_or_else(|| format!("document is not indexed: {uri}"))?;
        let offset = super::text::position_to_offset(&document.source, position)
            .ok_or_else(|| "position is outside the source document".to_string())?;
        if document.conditionals.is_unknown_at(offset)
            || is_ignored_offset(document.tree.root_node(), offset)
            || identifier_at(document.tree.root_node(), offset).is_none()
        {
            return Ok(Vec::new());
        }

        semantic_budget.require_work(1, cancel)?;
        let (binding, _) = self.binding_plan_with_cancel_and_budget(
            uri,
            position,
            Some(cancel),
            Some(semantic_budget),
            true,
        )?;
        let mut unresolved_call_targets = HashSet::new();
        let mut occurrence_options = OccurrenceCollectionOptions {
            document_uri: Some(uri),
            include_declaration: true,
            strict_resolution: false,
            result_limit: None,
            cancel: Some(cancel),
            work_budget: Some(work_budget),
            shared_work_budget: Some(semantic_budget),
        };
        let occurrences = self.collect_occurrences_bounded(&binding, &mut occurrence_options)?;
        let position_index = PositionIndex::new_with_cancel(&document.source, cancel)
            .map_err(|()| CANCELLATION_MESSAGE.to_string())?;
        let identifiers = super::identifier_nodes_with_budget(
            document.tree.root_node(),
            cancel,
            semantic_budget,
        )?;
        semantic_budget.require_bytes(
            identifiers.len().saturating_mul(
                std::mem::size_of::<Span>() + std::mem::size_of::<tree_sitter::Node<'_>>() + 32,
            ),
            cancel,
        )?;
        let mut identifiers_by_span = HashMap::with_capacity(identifiers.len());
        for identifier in identifiers {
            check_cancel(Some(cancel))?;
            identifiers_by_span.insert(Span::from_node(identifier), identifier);
        }
        let binding_writable = if binding.kind == SymbolKind::Unit {
            false
        } else {
            binding_is_writable_with_budget(self, &binding, cancel, semantic_budget)?
        };
        let mut highlights = Vec::with_capacity(occurrences.len());
        for occurrence in occurrences {
            check_location_cancel(Some(cancel))?;
            work_budget.charge()?;
            if occurrence.uri != *uri {
                continue;
            }
            let role = self.highlight_role_for_occurrence(
                &binding,
                uri,
                document,
                occurrence.span,
                cancel,
                semantic_budget,
                &mut unresolved_call_targets,
                &identifiers_by_span,
                binding_writable,
            )?;
            let start = position_index
                .offset_to_position(&document.source, occurrence.span.start)
                .ok_or_else(|| "cannot map highlight start to an LSP position".to_string())?;
            let end = position_index
                .offset_to_position(&document.source, occurrence.span.end)
                .ok_or_else(|| "cannot map highlight end to an LSP position".to_string())?;
            highlights.push(DocumentHighlight {
                range: Range { start, end },
                kind: Some(role.document_kind()),
            });
        }
        highlights.sort_by_key(|highlight| {
            (
                highlight.range.start.line,
                highlight.range.start.character,
                highlight.range.end.line,
                highlight.range.end.character,
            )
        });
        Ok(highlights)
    }

    #[allow(clippy::too_many_arguments)]
    fn highlight_role_for_occurrence(
        &self,
        binding: &Binding,
        current_uri: &Url,
        document: &Document,
        span: Span,
        cancel: &AtomicBool,
        budget: &mut super::AssistanceBudget,
        unresolved_call_targets: &mut HashSet<UnresolvedCallTargetCacheKey>,
        identifiers_by_span: &HashMap<Span, Node<'_>>,
        binding_writable: bool,
    ) -> Result<HighlightRole, String> {
        check_cancel(Some(cancel))?;
        budget.require_work(1, cancel)?;
        if binding.kind == SymbolKind::Unit {
            return Ok(HighlightRole::Text);
        }
        let Some(identifier) = identifiers_by_span.get(&span).copied() else {
            return Ok(HighlightRole::Text);
        };
        let non_value = super::is_non_value_identifier_with_budget_and_declaration(
            identifier,
            &document.source,
            cancel,
            budget,
            binding.contains_span(current_uri, span),
        )?;
        if Span::from_node(identifier) != span || non_value {
            return Ok(HighlightRole::Text);
        }

        if let Some((call, argument_index, argument)) =
            call_argument_for_identifier_with_budget(identifier, cancel, budget)?
        {
            let mode = match super::overload::parameter_mode_for_argument(
                self,
                current_uri,
                document,
                call,
                argument_index,
                cancel,
                budget,
            )? {
                super::overload::ParameterModeResolution::Resolved(mode)
                | super::overload::ParameterModeResolution::Intrinsic(mode) => mode,
                super::overload::ParameterModeResolution::Unresolved => {
                    let cache_key = unresolved_call_target_cache_key_with_budget(
                        current_uri,
                        document,
                        call,
                        cancel,
                        budget,
                    )?;
                    if cache_key
                        .as_ref()
                        .is_some_and(|key| unresolved_call_targets.contains(key))
                    {
                        None
                    } else {
                        if let Some(key) = cache_key {
                            budget
                                .require_bytes(unresolved_call_target_cache_bytes(&key), cancel)?;
                            unresolved_call_targets.insert(key);
                        }
                        None
                    }
                }
            };
            let Some(mode) = mode else {
                return Ok(HighlightRole::Text);
            };
            return Ok(match mode {
                super::ParameterMode::Var | super::ParameterMode::Out => {
                    if lvalue_terminal_for_identifier_with_budget(
                        argument, identifier, cancel, budget,
                    )? {
                        if binding_writable {
                            HighlightRole::Write
                        } else {
                            HighlightRole::Text
                        }
                    } else {
                        HighlightRole::Read
                    }
                }
                super::ParameterMode::Value
                | super::ParameterMode::Const
                | super::ParameterMode::ConstRef => HighlightRole::Read,
            });
        }

        if let Some((assignment, lhs)) =
            assignment_lhs_for_identifier_with_budget(identifier, cancel, budget)?
        {
            let operator = assignment
                .child_by_field_name("operator")
                .map(|operator| {
                    super::node_text_with_budget(operator, &document.source, cancel, budget)
                })
                .transpose()?
                .unwrap_or_default();
            if Span::from_node(lhs).contains(span) {
                if !lvalue_terminal_for_identifier_with_budget(lhs, identifier, cancel, budget)? {
                    return Ok(HighlightRole::Read);
                }
                return Ok(if binding_writable {
                    HighlightRole::Write
                } else if operator == ":=" {
                    HighlightRole::Text
                } else {
                    // A compound assignment is still a storage operation, but
                    // the selected binding must prove that it is writable.
                    HighlightRole::Text
                });
            }
        }
        if identifier_is_foreach_iterator_with_budget(identifier, cancel, budget)? {
            return Ok(if binding_writable {
                HighlightRole::Write
            } else {
                HighlightRole::Text
            });
        }
        if is_address_operand_with_budget(identifier, cancel, budget)? {
            return Ok(HighlightRole::Text);
        }

        Ok(match binding.kind {
            SymbolKind::Variable
            | SymbolKind::Parameter
            | SymbolKind::Field
            | SymbolKind::Property
            | SymbolKind::Constant
            | SymbolKind::EnumValue => HighlightRole::Read,
            _ => HighlightRole::Text,
        })
    }

    fn binding_locations_impl(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        include_declaration: bool,
        options: BindingLocationOptions<'_>,
    ) -> Result<Vec<lsp_types::Location>, String> {
        check_cancel(options.cancel)?;
        let document = self
            .documents
            .get(uri)
            .ok_or_else(|| format!("document is not indexed: {uri}"))?;
        let offset = super::text::position_to_offset(&document.source, position)
            .ok_or_else(|| "position is outside the source document".to_string())?;
        if document.conditionals.is_unknown_at(offset) {
            return Ok(Vec::new());
        }
        if is_ignored_offset(document.tree.root_node(), offset)
            || identifier_at(document.tree.root_node(), offset).is_none()
        {
            return Ok(Vec::new());
        }

        let selected_identifier = identifier_at(document.tree.root_node(), offset)
            .expect("identifier presence checked above");
        if !options.strict_resolution
            && options.document_uri.is_some()
            && !self.selected_occurrence_is_supported(uri, document, selected_identifier, offset)
        {
            return Ok(Vec::new());
        }

        let mut shared_work_budget = options.shared_work_budget;
        let (binding, _) = self.binding_plan_with_cancel_and_budget(
            uri,
            position,
            options.cancel,
            shared_work_budget.as_deref_mut(),
            options.allow_unit,
        )?;
        let mut occurrence_options = OccurrenceCollectionOptions {
            document_uri: options.document_uri,
            include_declaration,
            strict_resolution: options.strict_resolution,
            result_limit: options.result_limit,
            cancel: options.cancel,
            work_budget: options.work_budget,
            shared_work_budget,
        };
        let occurrences = self.collect_occurrences_bounded(&binding, &mut occurrence_options)?;
        self.locations_for_occurrences(
            &occurrences,
            occurrence_options.cancel,
            &mut occurrence_options.work_budget,
            &mut occurrence_options.shared_work_budget,
        )
    }

    fn selected_occurrence_is_supported(
        &self,
        uri: &Url,
        document: &Document,
        identifier: Node<'_>,
        offset: usize,
    ) -> bool {
        let span = Span::from_node(identifier);
        if document
            .opaque_ranges
            .iter()
            .any(|range| range.contains(span))
            || document.has_parser_recovery_near(span)
            || has_ancestor_kind(identifier, "ppDirective")
            || has_ancestor_kind(identifier, "with")
            || has_ancestor_kind(identifier, "inherited")
            || document.conditionals.is_unknown_at(offset)
        {
            return false;
        }
        let candidates = self.resolve_candidates_at(uri, document, offset, identifier);
        !candidates.is_empty()
            && !self.is_unknown_global_fallback(document, identifier, offset, &candidates)
    }

    pub(crate) fn rename_binding_info_with_cancel(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        cancel: &AtomicBool,
    ) -> Result<RenameBindingInfo, String> {
        check_cancel(Some(cancel))?;
        let (binding, _) = self.binding_plan_with_cancel(uri, position, Some(cancel), true)?;
        let mut names = binding.names.iter().cloned().collect::<Vec<_>>();
        names.sort_by_key(|name| canonical_name(name));
        names.dedup_by(|left, right| canonical_name(left) == canonical_name(right));
        let local = !binding.members.is_empty()
            && binding.members.iter().all(|member| {
                member.uri == *uri
                    && member.scope != ROOT_SCOPE
                    && member.owner_type.is_none()
                    && matches!(
                        member.kind,
                        SymbolKind::Variable
                            | SymbolKind::Constant
                            | SymbolKind::Parameter
                            | SymbolKind::Label
                    )
            });
        check_cancel(Some(cancel))?;
        Ok(RenameBindingInfo {
            local,
            unit: binding.kind == SymbolKind::Unit,
            names,
        })
    }

    /// Prove that the rename target can be resolved without any imported
    /// documents. This is deliberately narrower than classifying a binding as
    /// local: public bindings still require the workspace snapshot and its
    /// reverse-reference checks.
    pub(crate) fn self_contained_rename_binding_with_cancel(
        &mut self,
        uri: &Url,
        position: lsp_types::Position,
        additional_names: &[String],
        cancel: &AtomicBool,
    ) -> bool {
        if check_cancel(Some(cancel)).is_err() {
            return false;
        }
        self.bind_imports(uri, std::iter::empty::<(String, Url)>());
        let Ok(plan) = self.rename_plan_with_cancel(uri, position, Some(cancel)) else {
            return false;
        };
        let Some(document) = self.documents.get(uri) else {
            return false;
        };

        let binding_names = plan
            .binding
            .names
            .iter()
            .map(|name| canonical_name(name))
            .collect::<HashSet<_>>();
        let candidate_names = binding_names
            .iter()
            .cloned()
            .chain(additional_names.iter().map(|name| canonical_name(name)))
            .collect::<HashSet<_>>();

        for identifier in identifier_nodes(document.tree.root_node()) {
            if check_cancel(Some(cancel)).is_err() {
                return false;
            }
            let span = Span::from_node(identifier);
            let name = canonical_name(&node_text(identifier, &document.source));
            if !candidate_names.contains(&name) {
                continue;
            }
            if is_ignored_offset(document.tree.root_node(), span.start)
                || has_ancestor_kind(identifier, "ppDirective")
            {
                continue;
            }
            if document
                .opaque_ranges
                .iter()
                .any(|range| range.contains(span))
                || document.has_parser_recovery_near(span)
                || has_ancestor_kind(identifier, "with")
                || has_ancestor_kind(identifier, "inherited")
            {
                return false;
            }

            let candidates = self.resolve_candidates_at(uri, document, span.start, identifier);
            if candidates.len() != 1 {
                return false;
            }
            if self.candidate_is_conditionally_unknown(&candidates[0]) {
                return false;
            }
            let candidate = &candidates[0];
            if self.is_unknown_global_fallback(document, identifier, span.start, &candidates) {
                return false;
            }
            if candidate.uri != *uri
                || (binding_names.contains(&name)
                    && !plan.binding.matches_candidate(self, candidate))
            {
                return false;
            }
        }

        check_cancel(Some(cancel)).is_ok()
    }

    pub(super) fn is_unknown_global_fallback(
        &self,
        document: &Document,
        identifier: Node<'_>,
        offset: usize,
        candidates: &[Candidate],
    ) -> bool {
        let owner_type = document.owner_type_at(offset);
        self.is_unknown_global_fallback_for_owner(
            document,
            identifier,
            owner_type.as_deref(),
            candidates,
        )
    }

    pub(super) fn is_unknown_global_fallback_for_owner(
        &self,
        document: &Document,
        identifier: Node<'_>,
        owner_type: Option<&str>,
        candidates: &[Candidate],
    ) -> bool {
        let type_reference = has_ancestor_kind(identifier, "typeref");
        owner_type.is_some_and(|owner| self.has_unknown_class_ancestor(document, owner))
            && member_expression_at(identifier).is_none()
            && qualified_type_path_at(identifier, &document.source).is_none()
            && use_name_at(identifier, &document.source).is_none()
            && candidates.iter().any(|candidate| {
                self.candidate_is_global(candidate)
                    && (!type_reference
                        || self
                            .symbol(candidate)
                            .is_none_or(|symbol| symbol.kind != SymbolKind::Type))
            })
    }

    fn candidate_is_global(&self, candidate: &Candidate) -> bool {
        self.symbol(candidate).is_some_and(|symbol| {
            symbol.scope == ROOT_SCOPE
                && symbol.owner_type.is_none()
                && !symbol.local_only
                && matches!(
                    symbol.kind,
                    SymbolKind::Variable | SymbolKind::Constant | SymbolKind::Routine
                )
        })
    }

    fn has_unknown_class_ancestor(&self, document: &Document, owner_type: &str) -> bool {
        // Task 01 does not establish a complete override/rename model. Even
        // a source-visible parent is not proof that every inherited binding
        // and override family can be edited safely, so keep rename/reference
        // operations conservative until that model is implemented.
        document.unknown_class_owners.contains(owner_type)
            || !document.known_non_class_owners.contains(owner_type)
    }

    /// Check whether the identifier at `position` can be renamed in the
    /// current in-memory snapshot and return the selected source range.
    pub fn prepare_rename(
        &self,
        uri: &Url,
        position: lsp_types::Position,
    ) -> Result<PrepareRenameResponse, String> {
        let plan = self.rename_plan(uri, position)?;
        let range = self.range_for_occurrence(uri, plan.selected_span)?;
        Ok(PrepareRenameResponse::Range(range))
    }

    /// Plan a binding-based rename using only documents currently retained by
    /// this index. No filesystem discovery or document mutation is performed.
    pub fn rename_edits(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        new_name: &str,
    ) -> Result<HashMap<Url, Vec<TextEdit>>, String> {
        let plan = self.rename_plan(uri, position)?;
        validate_new_name(new_name)?;
        let new_key = canonical_name(new_name);
        self.check_proposed_name_references(&plan, &new_key)?;
        self.check_declaration_collisions(&plan.binding, &new_key)?;
        self.check_reference_capture(&plan, new_name)?;

        let mut edits: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for occurrence in plan.occurrences {
            let range = self.range_for_occurrence(&occurrence.uri, occurrence.span)?;
            edits
                .entry(occurrence.uri)
                .or_default()
                .push(TextEdit::new(range, new_name.to_owned()));
        }
        for document_edits in edits.values_mut() {
            document_edits.sort_by_key(|edit| {
                (
                    edit.range.start.line,
                    edit.range.start.character,
                    edit.range.end.line,
                    edit.range.end.character,
                )
            });
        }
        Ok(edits)
    }

    /// Plan a rename while charging occurrence collection and range
    /// materialization to the caller's request-wide assistance budget.
    ///
    /// The ordinary public rename API intentionally keeps its historical
    /// unbounded behavior. Source fix-all is a bounded assistance operation,
    /// so it uses this companion instead of repeatedly invoking that API.
    pub(crate) fn rename_edits_with_cancel_and_work_budget(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        new_name: &str,
        cancel: &AtomicBool,
        budget: &mut super::AssistanceBudget,
    ) -> Result<HashMap<Url, Vec<TextEdit>>, String> {
        check_cancel(Some(cancel))?;
        let mut binding_budget = BindingWorkBudget::new(MAX_BINDING_LOCATIONS);
        budget.require_work(1, cancel)?;
        let (binding, selected_span) = self.binding_plan_with_cancel_and_budget(
            uri,
            position,
            Some(cancel),
            Some(budget),
            false,
        )?;
        let mut occurrence_options = OccurrenceCollectionOptions {
            document_uri: None,
            include_declaration: true,
            strict_resolution: true,
            result_limit: None,
            cancel: Some(cancel),
            work_budget: Some(&mut binding_budget),
            shared_work_budget: Some(budget),
        };
        let occurrences = self.collect_occurrences_bounded(&binding, &mut occurrence_options)?;
        if occurrences.is_empty() {
            return Err("rename binding has no source occurrences".to_string());
        }
        check_cancel(Some(cancel))?;
        validate_new_name(new_name)?;
        let new_key = canonical_name(new_name);
        // These checks are retained from the established rename proof. Charge
        // their complete phase as one request unit before and after; the
        // occurrence-sensitive capture check itself is additionally polled by
        // the bounded occurrence loop above.
        binding_budget.charge_with_cancel(Some(cancel))?;
        self.check_proposed_name_references_bounded(
            &RenamePlan {
                binding: binding.clone(),
                selected_span,
                occurrences: occurrences.clone(),
            },
            &new_key,
            Some(cancel),
            Some(budget),
        )?;
        binding_budget.charge_with_cancel(Some(cancel))?;
        self.check_declaration_collisions_bounded(&binding, &new_key, Some(cancel), Some(budget))?;
        binding_budget.charge_with_cancel(Some(cancel))?;
        self.check_reference_capture_bounded(
            &RenamePlan {
                binding,
                selected_span,
                occurrences: occurrences.clone(),
            },
            new_name,
            Some(cancel),
            Some(budget),
        )?;

        let mut edits: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for occurrence in occurrences {
            check_cancel(Some(cancel))?;
            binding_budget.charge_with_cancel(Some(cancel))?;
            let range = self.range_for_occurrence(&occurrence.uri, occurrence.span)?;
            budget.require_bytes(new_name.len(), cancel)?;
            edits
                .entry(occurrence.uri)
                .or_default()
                .push(TextEdit::new(range, new_name.to_owned()));
        }
        for document_edits in edits.values_mut() {
            check_cancel(Some(cancel))?;
            budget.require_work(comparison_sort_work(document_edits.len()), cancel)?;
            document_edits.sort_by_key(|edit| {
                (
                    edit.range.start.line,
                    edit.range.start.character,
                    edit.range.end.line,
                    edit.range.end.character,
                )
            });
        }
        Ok(edits)
    }

    fn check_proposed_name_references(
        &self,
        plan: &RenamePlan,
        new_key: &str,
    ) -> Result<(), String> {
        self.check_proposed_name_references_bounded(plan, new_key, None, None)
    }

    fn check_proposed_name_references_bounded(
        &self,
        plan: &RenamePlan,
        new_key: &str,
        cancel: Option<&AtomicBool>,
        shared_work_budget: Option<&mut super::AssistanceBudget>,
    ) -> Result<(), String> {
        let mut shared_work_budget = shared_work_budget;
        let mut documents = self.documents.iter().collect::<Vec<_>>();
        charge_shared_work(
            &mut shared_work_budget,
            cancel,
            comparison_sort_work(documents.len()),
        )?;
        charge_shared_bytes(
            &mut shared_work_budget,
            cancel,
            documents
                .len()
                .saturating_mul(std::mem::size_of::<(&Url, &Document)>()),
        )?;
        documents.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
        for (uri, document) in documents {
            charge_shared_work(&mut shared_work_budget, cancel, 1)?;
            charge_shared_bytes(&mut shared_work_budget, cancel, document.source.len())?;
            let root = document.tree.root_node();
            let identifiers = identifier_nodes(root);
            charge_shared_work(&mut shared_work_budget, cancel, identifiers.len())?;
            for identifier in identifiers {
                check_cancel(cancel)?;
                let span = Span::from_node(identifier);
                charge_shared_bytes(
                    &mut shared_work_budget,
                    cancel,
                    span.end.saturating_sub(span.start),
                )?;
                if canonical_name(&node_text(identifier, &document.source)) != new_key
                    || is_ignored_offset(root, span.start)
                    || has_ancestor_kind(identifier, "ppDirective")
                    || document.symbols.iter().any(|symbol| symbol.span == span)
                {
                    continue;
                }
                if self.has_class_owned_member(&plan.binding)
                    && (has_ancestor_kind(identifier, "with")
                        || has_ancestor_kind(identifier, "inherited"))
                {
                    return Err(format!(
                        "rename does not support inherited/with lookup for proposed name at {uri}:{}",
                        span.start
                    ));
                }
                let candidates = self.resolve_candidates_at_with_shared_budget(
                    uri,
                    document,
                    span.start,
                    identifier,
                    cancel,
                    &mut shared_work_budget,
                )?;
                charge_shared_work(
                    &mut shared_work_budget,
                    cancel,
                    candidates.len().saturating_add(1),
                )?;
                let matching = candidates
                    .iter()
                    .filter(|candidate| plan.binding.matches_candidate(self, candidate))
                    .count();
                if !candidates.is_empty() && matching == candidates.len() {
                    continue;
                }
                if matching > 0 {
                    return Err(format!(
                        "rename would make an existing {:?} reference ambiguous at {uri}:{}",
                        new_key, span.start
                    ));
                }
                if self.proposed_binding_visible_at(
                    &plan.binding,
                    uri,
                    document,
                    span.start,
                    &candidates,
                    cancel,
                    &mut shared_work_budget,
                )? {
                    return Err(format!(
                        "rename would change the binding of an existing {:?} reference at {uri}:{}",
                        new_key, span.start
                    ));
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn proposed_binding_visible_at(
        &self,
        binding: &Binding,
        occurrence_uri: &Url,
        document: &Document,
        offset: usize,
        candidates: &[Candidate],
        cancel: Option<&AtomicBool>,
        shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
    ) -> Result<bool, String> {
        let occurrence_scope = document.scope_at(offset);
        let occurrence_owner = document.owner_type_at(offset);

        for member in &binding.members {
            charge_shared_work(shared_work_budget, cancel, 1)?;
            let Some(member_document) = self.documents.get(&member.uri) else {
                continue;
            };
            let Some(symbol) = member_document
                .symbols
                .iter()
                .find(|symbol| symbol_id(&member.uri, symbol) == *member)
            else {
                continue;
            };

            if let Some(owner_type) = &symbol.owner_type {
                if member.uri != *occurrence_uri
                    || occurrence_owner.as_deref() != Some(owner_type.as_str())
                {
                    charge_shared_work(shared_work_budget, cancel, candidates.len())?;
                    if candidates.iter().any(|candidate| {
                        let Some(candidate_symbol) = self.symbol(candidate) else {
                            return false;
                        };
                        candidate.uri == *occurrence_uri
                            && candidate_symbol.owner_type.is_none()
                            && candidate_symbol.scope != ROOT_SCOPE
                            && scope_is_ancestor(document, candidate_symbol.scope, occurrence_scope)
                    }) {
                        continue;
                    }
                    return Ok(true);
                }
                charge_shared_work(shared_work_budget, cancel, candidates.len())?;
                if !candidates.iter().any(|candidate| {
                    let Some(candidate_symbol) = self.symbol(candidate) else {
                        return false;
                    };
                    candidate.uri == *occurrence_uri
                        && candidate_symbol.owner_type.is_none()
                        && candidate_symbol.scope != ROOT_SCOPE
                        && scope_is_ancestor(document, candidate_symbol.scope, occurrence_scope)
                }) {
                    return Ok(true);
                }
                continue;
            }

            if symbol.scope != ROOT_SCOPE {
                if member.uri != *occurrence_uri
                    || !scope_is_ancestor(document, symbol.scope, occurrence_scope)
                {
                    continue;
                }
                charge_shared_work(shared_work_budget, cancel, candidates.len())?;
                let shadowed_by_nearer_local = candidates.iter().any(|candidate| {
                    let Some(candidate_symbol) = self.symbol(candidate) else {
                        return false;
                    };
                    candidate.uri == *occurrence_uri
                        && candidate_symbol.owner_type.is_none()
                        && candidate_symbol.scope != ROOT_SCOPE
                        && candidate_symbol.scope != symbol.scope
                        && scope_is_ancestor(document, symbol.scope, candidate_symbol.scope)
                });
                if !shadowed_by_nearer_local {
                    return Ok(true);
                }
                continue;
            }

            if symbol.local_only
                || !self.root_binding_visible_at(member, occurrence_uri, document, offset)
            {
                continue;
            }
            charge_shared_work(shared_work_budget, cancel, candidates.len())?;
            let shadowed_by_local = candidates.iter().any(|candidate| {
                let Some(candidate_symbol) = self.symbol(candidate) else {
                    return false;
                };
                candidate.uri == *occurrence_uri
                    && candidate_symbol.owner_type.is_none()
                    && candidate_symbol.scope != ROOT_SCOPE
            });
            if !shadowed_by_local {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn has_class_owned_member(&self, binding: &Binding) -> bool {
        binding.members.iter().any(|member| {
            let Some(document) = self.documents.get(&member.uri) else {
                return false;
            };
            document
                .symbols
                .iter()
                .find(|symbol| symbol_id(&member.uri, symbol) == *member)
                .is_some_and(|symbol| symbol.owner_type.is_some())
        })
    }

    fn root_binding_visible_at(
        &self,
        member: &SymbolId,
        occurrence_uri: &Url,
        document: &Document,
        offset: usize,
    ) -> bool {
        if member.uri == *occurrence_uri {
            return true;
        }
        let region = document.region_at(offset);
        document.active_uses(region).iter().any(|used| {
            self.unit_urls_for_import(document, used)
                .iter()
                .any(|uri| uri == &member.uri)
        })
    }

    fn rename_plan(&self, uri: &Url, position: lsp_types::Position) -> Result<RenamePlan, String> {
        self.rename_plan_with_cancel(uri, position, None)
    }

    fn rename_plan_with_cancel(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        cancel: Option<&AtomicBool>,
    ) -> Result<RenamePlan, String> {
        check_cancel(cancel)?;
        let (binding, selected_span) =
            self.binding_plan_with_cancel(uri, position, cancel, false)?;
        let mut occurrence_options = OccurrenceCollectionOptions {
            document_uri: None,
            include_declaration: true,
            strict_resolution: true,
            result_limit: None,
            cancel,
            work_budget: None,
            shared_work_budget: None,
        };
        let occurrences = self.collect_occurrences_bounded(&binding, &mut occurrence_options)?;
        if occurrences.is_empty() {
            return Err("rename binding has no source occurrences".to_string());
        }
        Ok(RenamePlan {
            binding,
            selected_span,
            occurrences,
        })
    }

    fn binding_plan_with_cancel(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        cancel: Option<&AtomicBool>,
        allow_unit: bool,
    ) -> Result<(Binding, Span), String> {
        self.binding_plan_with_cancel_and_budget(uri, position, cancel, None, allow_unit)
    }

    fn binding_plan_with_cancel_and_budget(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        cancel: Option<&AtomicBool>,
        shared_work_budget: Option<&mut super::AssistanceBudget>,
        allow_unit: bool,
    ) -> Result<(Binding, Span), String> {
        let mut shared_work_budget = shared_work_budget;
        check_cancel(cancel)?;
        charge_shared_work(&mut shared_work_budget, cancel, 1)?;
        let document = self
            .documents
            .get(uri)
            .ok_or_else(|| format!("document is not indexed: {uri}"))?;
        charge_shared_bytes(&mut shared_work_budget, cancel, document.source.len())?;
        let offset = super::text::position_to_offset(&document.source, position)
            .ok_or_else(|| "position is outside the source document".to_string())?;
        charge_shared_work(&mut shared_work_budget, cancel, 1)?;
        if is_ignored_offset(document.tree.root_node(), offset) {
            return Err("cannot rename an identifier in a comment or literal".to_string());
        }
        let identifier = identifier_at(document.tree.root_node(), offset)
            .ok_or_else(|| "no renameable identifier at position".to_string())?;
        let candidates = self.resolve_candidates_at_with_shared_budget(
            uri,
            document,
            offset,
            identifier,
            cancel,
            &mut shared_work_budget,
        )?;
        let candidates = self.select_rename_overload_candidates(
            uri,
            document,
            identifier,
            candidates,
            cancel,
            &mut shared_work_budget,
        )?;
        charge_shared_work(
            &mut shared_work_budget,
            cancel,
            candidates.len().saturating_add(1),
        )?;
        charge_shared_bytes(
            &mut shared_work_budget,
            cancel,
            candidate_materialization_bytes(&candidates),
        )?;
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Err(
                "rename cannot prove the binding across an unknown conditional branch".to_string(),
            );
        }
        let binding = binding_from_candidates(self, candidates, cancel, &mut shared_work_budget)?;
        charge_shared_work(
            &mut shared_work_budget,
            cancel,
            binding.members.len().saturating_add(binding.names.len()),
        )?;
        charge_shared_bytes(
            &mut shared_work_budget,
            cancel,
            binding
                .members
                .iter()
                .fold(
                    binding.old_key.len().saturating_add(
                        binding
                            .members
                            .len()
                            .saturating_mul(std::mem::size_of::<SymbolId>()),
                    ),
                    |bytes, member| bytes.saturating_add(member.uri.as_str().len()),
                )
                .saturating_add(binding.names.iter().map(String::len).sum::<usize>()),
        )?;
        check_cancel(cancel)?;
        if binding.kind == SymbolKind::Unit && !allow_unit {
            return Err("unit/module rename requires RenameFile support".to_string());
        }
        if binding.members.iter().any(|member| {
            self.documents
                .get(&member.uri)
                .and_then(|document| {
                    document
                        .symbols
                        .iter()
                        .find(|symbol| symbol_id(&member.uri, symbol) == *member)
                })
                .is_some_and(|symbol| symbol.generic_parameter.is_some())
        }) {
            return Err("generic parameter rename is not supported".to_string());
        }
        charge_shared_work(&mut shared_work_budget, cancel, 1)?;
        if binding.kind == SymbolKind::Type && has_forward_class_pair(self, &binding) {
            return Err("forward class/completion type rename is not supported".to_string());
        }
        let selected_span = Span::from_node(identifier);
        Ok((binding, selected_span))
    }

    fn resolve_candidates_at_with_shared_budget(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        identifier: Node<'_>,
        cancel: Option<&AtomicBool>,
        shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
    ) -> Result<Vec<Candidate>, String> {
        if let Some(shared_work_budget) = shared_work_budget.as_deref_mut() {
            let fallback_cancel = AtomicBool::new(false);
            let cancel = cancel.unwrap_or(&fallback_cancel);
            let mut state = ResolutionState::new();
            self.resolve_candidates_at_with_state_and_budget(
                uri,
                document,
                offset,
                identifier,
                &mut state,
                0,
                cancel,
                shared_work_budget,
            )
        } else {
            Ok(self.resolve_candidates_at(uri, document, offset, identifier))
        }
    }

    /// Apply the same call-site overload proof used by navigation before
    /// rename compares occurrence candidates. A raw member lookup contains
    /// every same-named overload; retaining that set makes a proven call look
    /// ambiguous and either rejects or widens an exact slot.
    fn select_rename_overload_candidates(
        &self,
        uri: &Url,
        document: &Document,
        identifier: Node<'_>,
        mut candidates: Vec<Candidate>,
        cancel: Option<&AtomicBool>,
        shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
    ) -> Result<Vec<Candidate>, String> {
        let Some(call) = super::overload::call_for_identifier(identifier) else {
            return Ok(candidates);
        };
        let fallback_cancel = AtomicBool::new(false);
        let cancel = cancel.unwrap_or(&fallback_cancel);
        let entity = call.child_by_field_name("entity");
        let explicit_owner = entity.and_then(super::callable_owner_node);

        let mut select = |budget: &mut super::AssistanceBudget| {
            let mut state = ResolutionState::new();
            let mut implicit_candidates = None;
            let (owner_receivers, implicit_self) = if let Some(owner) = explicit_owner {
                (
                    self.resolve_receivers_with_state_and_budget(
                        uri,
                        document,
                        identifier.start_byte(),
                        owner,
                        owner,
                        &mut state,
                        cancel,
                        budget,
                        0,
                    )?,
                    false,
                )
            } else {
                if document.has_with_context_at(identifier.start_byte()) {
                    // An unqualified call inside `with` can have a receiver
                    // other than lexical Self. The ordinary navigation
                    // resolver owns that proof; do not guess one here.
                    return Ok::<(), String>(());
                }
                let scope =
                    self.budgeted_scope_at(document, identifier.start_byte(), cancel, budget)?;
                let owner_type = document
                    .owner_type_at_identifier_with_budget(identifier, scope, cancel, budget)?;
                if owner_type.is_none() {
                    // An unqualified global call has no lexical receiver proof;
                    // keep the conservative behavior used before this helper.
                    return Ok::<(), String>(());
                }
                let name = identifier
                    .utf8_text(document.source.as_bytes())
                    .map_err(|_| "implicit-self call name is not valid UTF-8".to_string())?;
                let lexical_candidates = self.unqualified_references_with_budget_and_state(
                    uri,
                    document,
                    identifier.start_byte(),
                    name,
                    identifier,
                    &mut state,
                    cancel,
                    budget,
                )?;
                if lexical_candidates.is_empty()
                    || lexical_candidates.iter().any(|candidate| {
                        self.symbol(candidate).is_none_or(|symbol| {
                            symbol.kind != SymbolKind::Routine
                                || self.candidate_is_conditionally_unknown(candidate)
                        })
                    })
                {
                    // A local/parameter shadow, an unknown conditional, or a
                    // missing lexical binding is not proof of an implicit
                    // method call.
                    return Ok::<(), String>(());
                }
                candidates.retain(|candidate| lexical_candidates.contains(candidate));
                if candidates.is_empty() {
                    return Ok::<(), String>(());
                }
                implicit_candidates = Some(lexical_candidates);
                (
                    self.resolve_identifier_receiver_with_budget(
                        uri,
                        document,
                        identifier.start_byte(),
                        "Self",
                        identifier,
                        &mut state,
                        cancel,
                        budget,
                    )?,
                    true,
                )
            };
            let owner_instances = owner_receivers
                .iter()
                .filter_map(|receiver| match receiver {
                    super::Receiver::Type(instance) => Some(instance.clone()),
                    super::Receiver::Unit(_)
                    | super::Receiver::Builtin(_)
                    | super::Receiver::IntegerLiteral(_) => None,
                })
                .collect::<Vec<_>>();
            if let Some(lexical_candidates) = implicit_candidates.as_ref() {
                if !lexical_candidates.iter().any(|candidate| {
                    candidates.contains(candidate)
                        && self
                            .symbol(candidate)
                            .is_some_and(|symbol| symbol.kind == SymbolKind::Routine)
                }) {
                    return Ok::<(), String>(());
                }
            }
            let selection = super::overload::select(
                self,
                uri,
                document,
                call,
                &candidates,
                &super::GenericSubstitution::empty(),
                &owner_instances,
                &mut state,
                0,
                cancel,
                budget,
            )?;
            if let Some(group) = selection.selected_group {
                if implicit_self
                    && !candidates.iter().any(|candidate| {
                        if !super::overload::candidate_in_group(self, candidate, &group) {
                            return false;
                        }
                        let Some(symbol) = self.symbol(candidate) else {
                            return false;
                        };
                        owner_instances
                            .iter()
                            .any(|owner| symbol.owner_type.as_deref() == Some(owner.key.as_str()))
                    })
                {
                    // A lexical class context alone must not turn a global,
                    // with-bound, or otherwise unrelated routine into a self
                    // member. Only narrow when the selected group contains a
                    // candidate owned by the proven implicit receiver.
                    return Ok::<(), String>(());
                }
                candidates.retain(|candidate| {
                    super::overload::candidate_in_group(self, candidate, &group)
                });
            } else if selection.no_viable_group {
                candidates.retain(|candidate| {
                    super::overload::key_for_candidate(self, candidate).is_none()
                });
            }
            Ok::<(), String>(())
        };

        if let Some(budget) = shared_work_budget.as_deref_mut() {
            select(budget)?;
        } else {
            let mut budget = super::AssistanceBudget::new(
                super::MAX_NAVIGATION_OVERLOAD_WORK,
                super::MAX_NAVIGATION_OVERLOAD_BYTES,
                "rename overload selection",
            );
            select(&mut budget)?;
        }
        Ok(candidates)
    }

    fn collect_occurrences_bounded(
        &self,
        binding: &Binding,
        options: &mut OccurrenceCollectionOptions<'_>,
    ) -> Result<Vec<Occurrence>, String> {
        let mut documents: Vec<(&Url, &Document)> = self.documents.iter().collect();
        charge_shared_work(
            &mut options.shared_work_budget,
            options.cancel,
            comparison_sort_work(documents.len()),
        )?;
        charge_shared_bytes(
            &mut options.shared_work_budget,
            options.cancel,
            documents
                .len()
                .saturating_mul(std::mem::size_of::<(&Url, &Document)>()),
        )?;
        documents.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));

        let mut occurrences = Vec::new();
        let mut seen = HashSet::new();
        let mut unqualified_cache =
            HashMap::<UnqualifiedReferenceCacheKey, (Vec<Candidate>, bool)>::new();
        let binding_has_class_owner = self.has_class_owned_member(binding);
        for (uri, document) in documents {
            check_cancel(options.cancel)?;
            charge_binding_work(
                &mut options.work_budget,
                &mut options.shared_work_budget,
                options.cancel,
            )?;
            if options
                .document_uri
                .is_some_and(|requested| requested != uri)
            {
                continue;
            }
            if options.strict_resolution
                && document.opaque_ranges.iter().any(|range| {
                    binding
                        .names
                        .iter()
                        .any(|name| contains_identifier(&document.source, *range, name))
                })
            {
                return Err(format!(
                    "rename is incomplete: {:?} occurs in an opaque compiler-directive block in {uri}",
                    binding.old_key
                ));
            }

            let root = document.tree.root_node();
            let unit_spans = if binding.kind == SymbolKind::Unit {
                if let Some(shared_work_budget) = options.shared_work_budget.as_deref_mut() {
                    let fallback_cancel = AtomicBool::new(false);
                    let cancel = options.cancel.unwrap_or(&fallback_cancel);
                    self.unit_occurrence_spans_with_budget(
                        uri,
                        document,
                        cancel,
                        shared_work_budget,
                    )?
                } else {
                    HashMap::new()
                }
            } else {
                HashMap::new()
            };
            let identifiers =
                if let Some(shared_work_budget) = options.shared_work_budget.as_deref_mut() {
                    let fallback_cancel = AtomicBool::new(false);
                    let cancel = options.cancel.unwrap_or(&fallback_cancel);
                    identifier_nodes_with_budget(root, cancel, shared_work_budget)?
                } else {
                    identifier_nodes(root)
                };
            for identifier in identifiers {
                check_occurrence_cancel(options.cancel)?;
                charge_binding_work(
                    &mut options.work_budget,
                    &mut options.shared_work_budget,
                    options.cancel,
                )?;
                let span = Span::from_node(identifier);
                let name = canonical_name(&node_text(identifier, &document.source));
                let is_binding_member = binding.contains_span(uri, span);
                let unit_span = if binding.kind == SymbolKind::Unit {
                    if options.shared_work_budget.is_some() {
                        unit_spans.get(&span).copied()
                    } else {
                        self.unit_occurrence_span(
                            uri,
                            document,
                            identifier,
                            span.start,
                            options.cancel,
                        )
                    }
                } else {
                    None
                };
                if !is_binding_member && !binding.names.contains(&name) && unit_span.is_none() {
                    continue;
                }
                if has_ancestor_kind(identifier, "ppDirective") {
                    continue;
                }
                if !document.parser_recovery_spans.is_empty()
                    && document.has_parser_recovery_near(span)
                {
                    if !options.strict_resolution && !is_binding_member {
                        continue;
                    }
                    return Err(format!(
                        "rename cannot prove the binding of {:?} at {}:{} because parser recovery affects the identifier",
                        binding.old_key, uri, span.start
                    ));
                }

                if has_ancestor_kind(identifier, "with")
                    || has_ancestor_kind(identifier, "inherited")
                {
                    if !options.strict_resolution && !is_binding_member {
                        continue;
                    }
                    if has_ancestor_kind(identifier, "inherited") || binding_has_class_owner {
                        return Err(format!(
                            "rename does not support with/inherited references at {}:{}",
                            uri, span.start
                        ));
                    }
                }
                let is_unit_declaration = binding.kind == SymbolKind::Unit
                    && unit_span.is_some()
                    && is_unit_declaration_identifier(identifier);
                if !options.include_declaration && (is_binding_member || is_unit_declaration) {
                    continue;
                }
                let is_direct_declaration =
                    document.symbols.iter().any(|symbol| symbol.span == span);
                let member_expression = member_expression_at(identifier);
                let qualified_type_path = qualified_type_path_at(identifier, &document.source);
                let use_name = use_name_at(identifier, &document.source);
                if binding_has_class_owner
                    && member_expression.is_none()
                    && qualified_type_path.is_none()
                    && !document.symbols.iter().any(|symbol| symbol.span == span)
                    && self.has_foreign_class_owner(binding, uri, document, span.start)
                {
                    if !options.strict_resolution && !is_binding_member {
                        continue;
                    }
                    return Err(format!(
                        "rename does not support inherited class lookup at {}:{}",
                        uri, span.start
                    ));
                }

                let simple_reference = !is_binding_member
                    && !is_direct_declaration
                    && member_expression.is_none()
                    && qualified_type_path.is_none()
                    && use_name.is_none();
                let scope = document.scope_at(span.start);
                let (candidates, unknown_global_fallback) =
                    if simple_reference && cacheable_unqualified_use(document, identifier, scope) {
                        let cache_key = UnqualifiedReferenceCacheKey {
                            uri: uri.clone(),
                            name: name.clone(),
                            scope,
                            region: document.region_at(span.start),
                            owner_type: document.owner_type_at_identifier(identifier, scope),
                        };
                        if let Some(cached) = unqualified_cache.get(&cache_key) {
                            cached.clone()
                        } else {
                            let candidates = self.resolve_candidates_at_with_shared_budget(
                                uri,
                                document,
                                span.start,
                                identifier,
                                options.cancel,
                                &mut options.shared_work_budget,
                            )?;
                            let unknown_global_fallback = self.is_unknown_global_fallback(
                                document,
                                identifier,
                                span.start,
                                &candidates,
                            );
                            unqualified_cache
                                .insert(cache_key, (candidates.clone(), unknown_global_fallback));
                            (candidates, unknown_global_fallback)
                        }
                    } else {
                        let candidates = self.resolve_candidates_at_with_shared_budget(
                            uri,
                            document,
                            span.start,
                            identifier,
                            options.cancel,
                            &mut options.shared_work_budget,
                        )?;
                        let unknown_global_fallback = self.is_unknown_global_fallback(
                            document,
                            identifier,
                            span.start,
                            &candidates,
                        );
                        (candidates, unknown_global_fallback)
                    };
                let candidates = self.select_rename_overload_candidates(
                    uri,
                    document,
                    identifier,
                    candidates,
                    options.cancel,
                    &mut options.shared_work_budget,
                )?;
                charge_shared_work(
                    &mut options.shared_work_budget,
                    options.cancel,
                    candidates.len(),
                )?;
                charge_shared_bytes(
                    &mut options.shared_work_budget,
                    options.cancel,
                    candidate_materialization_bytes(&candidates),
                )?;
                let in_unknown_branch = document
                    .conditionals
                    .unknown_spans
                    .iter()
                    .any(|range| range.start <= span.start && span.end <= range.end);
                if in_unknown_branch
                    && candidates.iter().any(|candidate| {
                        binding.matches_candidate(self, candidate)
                            && self.candidate_is_conditionally_unknown(candidate)
                    })
                {
                    if !options.strict_resolution && !is_binding_member {
                        continue;
                    }
                    return Err(format!(
                        "rename cannot prove the binding of {:?} at {}:{} because its declaration is in an unknown conditional branch",
                        binding.old_key, uri, span.start
                    ));
                }
                if unknown_global_fallback {
                    if !options.strict_resolution && !is_binding_member {
                        continue;
                    }
                    return Err(format!(
                        "rename cannot prove the binding of {:?} at {}:{} because the class ancestor is unknown",
                        binding.old_key, uri, span.start
                    ));
                }
                let matching = candidates
                    .iter()
                    .filter(|candidate| binding.matches_candidate(self, candidate))
                    .count();
                if has_ancestor_kind(identifier, "with")
                    && !binding_has_class_owner
                    && !is_binding_member
                {
                    if candidates.is_empty() {
                        if !options.strict_resolution {
                            continue;
                        }
                        return Err(format!(
                            "rename cannot prove the local binding through with at {}:{}",
                            uri, span.start
                        ));
                    }
                    if matching == 0 {
                        continue;
                    }
                    if matching != candidates.len() {
                        if !options.strict_resolution {
                            continue;
                        }
                        return Err(format!(
                            "rename cannot prove the local binding through with at {}:{}",
                            uri, span.start
                        ));
                    }
                    if !options.strict_resolution {
                        continue;
                    }
                }
                if matching > 0 && binding_has_class_owner && !is_direct_declaration {
                    if let Some(dot) = member_expression {
                        if self.member_reference_uses_inherited_class_owner(
                            binding, uri, document, span.start, dot,
                        ) {
                            if !options.strict_resolution && !is_binding_member {
                                continue;
                            }
                            return Err(format!(
                                "rename does not support inherited class lookup at {}:{}",
                                uri, span.start
                            ));
                        }
                    }
                }
                if matching > 0 {
                    if matching != candidates.len() {
                        if !options.strict_resolution && !is_binding_member {
                            continue;
                        }
                        return Err(format!(
                            "ambiguous rename reference {:?} at {}:{}",
                            binding.old_key, uri, span.start
                        ));
                    }
                    let occurrence = Occurrence {
                        uri: uri.clone(),
                        span: unit_span.unwrap_or(span),
                    };
                    if seen.insert((occurrence.uri.clone(), occurrence.span)) {
                        if options
                            .result_limit
                            .is_some_and(|limit| occurrences.len() >= limit)
                        {
                            return Err(format!(
                                "binding reference result exceeds the {MAX_BINDING_LOCATIONS}-entry limit"
                            ));
                        }
                        occurrences.push(occurrence);
                    }
                } else if options.strict_resolution
                    && candidates.is_empty()
                    && (!has_ancestor_kind(identifier, "moduleName")
                        || binding.kind == SymbolKind::Unit)
                {
                    return Err(format!(
                        "unresolved rename reference {:?} at {}:{}",
                        binding.old_key, uri, span.start
                    ));
                }
            }
        }
        Ok(occurrences)
    }

    fn locations_for_occurrences(
        &self,
        occurrences: &[Occurrence],
        cancel: Option<&AtomicBool>,
        work_budget: &mut Option<&mut BindingWorkBudget>,
        shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
    ) -> Result<Vec<lsp_types::Location>, String> {
        let mut seen = HashSet::new();
        charge_shared_bytes(
            shared_work_budget,
            cancel,
            occurrences
                .len()
                .saturating_mul(std::mem::size_of::<lsp_types::Location>()),
        )?;
        let mut locations = Vec::with_capacity(occurrences.len());
        let mut group_start = 0;
        while group_start < occurrences.len() {
            charge_shared_bytes(
                shared_work_budget,
                cancel,
                occurrences[group_start].uri.as_str().len(),
            )?;
            let group_uri = occurrences[group_start].uri.clone();
            let mut group_end = group_start + 1;
            while group_end < occurrences.len() && occurrences[group_end].uri == group_uri {
                group_end += 1;
            }

            // Occurrences are collected in URI order. Keep this index scoped to
            // one contiguous document group so a large workspace response does
            // not retain one UTF-16 table for every consumer at once.
            {
                let document = self
                    .documents
                    .get(&group_uri)
                    .ok_or_else(|| format!("document is not indexed: {group_uri}"))?;
                let position_index = match cancel {
                    Some(cancel) => PositionIndex::new_with_cancel(&document.source, cancel)
                        .map_err(|()| CANCELLATION_MESSAGE.to_string())?,
                    None => PositionIndex::new(&document.source),
                };
                for occurrence in &occurrences[group_start..group_end] {
                    check_location_cancel(cancel)?;
                    charge_binding_work(work_budget, shared_work_budget, cancel)?;
                    if !seen.insert((occurrence.uri.clone(), occurrence.span)) {
                        continue;
                    }
                    let start = position_index
                        .offset_to_position(&document.source, occurrence.span.start)
                        .ok_or_else(|| {
                            "cannot map reference start to an LSP position".to_string()
                        })?;
                    let end = position_index
                        .offset_to_position(&document.source, occurrence.span.end)
                        .ok_or_else(|| "cannot map reference end to an LSP position".to_string())?;
                    charge_shared_bytes(
                        shared_work_budget,
                        cancel,
                        std::mem::size_of::<lsp_types::Location>()
                            .saturating_add(occurrence.uri.as_str().len()),
                    )?;
                    locations.push(lsp_types::Location {
                        uri: occurrence.uri.clone(),
                        range: Range { start, end },
                    });
                }
            }
            group_start = group_end;
        }
        charge_shared_work(
            shared_work_budget,
            cancel,
            comparison_sort_work(locations.len()),
        )?;
        locations.sort_by(|left, right| {
            left.uri
                .as_str()
                .cmp(right.uri.as_str())
                .then_with(|| left.range.start.line.cmp(&right.range.start.line))
                .then_with(|| left.range.start.character.cmp(&right.range.start.character))
                .then_with(|| left.range.end.line.cmp(&right.range.end.line))
                .then_with(|| left.range.end.character.cmp(&right.range.end.character))
        });
        Ok(locations)
    }

    fn has_foreign_class_owner(
        &self,
        binding: &Binding,
        occurrence_uri: &Url,
        document: &Document,
        offset: usize,
    ) -> bool {
        let owners: HashSet<(Url, String)> = binding
            .members
            .iter()
            .filter_map(|member| {
                let member_document = self.documents.get(&member.uri)?;
                member_document
                    .symbols
                    .iter()
                    .find(|symbol| symbol_id(&member.uri, symbol) == *member)
                    .and_then(|symbol| {
                        symbol
                            .owner_type
                            .clone()
                            .map(|owner| (member.uri.clone(), owner))
                    })
            })
            .collect();
        if owners.is_empty() {
            return false;
        }
        document
            .owner_type_at(offset)
            .is_some_and(|owner| !owners.contains(&(occurrence_uri.clone(), owner)))
    }

    fn member_reference_uses_inherited_class_owner(
        &self,
        binding: &Binding,
        occurrence_uri: &Url,
        document: &Document,
        offset: usize,
        dot: Node<'_>,
    ) -> bool {
        if binding.members.iter().any(|member| {
            self.documents
                .get(&member.uri)
                .and_then(|document| {
                    document
                        .symbols
                        .iter()
                        .enumerate()
                        .find(|(_, symbol)| symbol_id(&member.uri, symbol) == *member)
                        .map(|(index, _)| Candidate {
                            uri: member.uri.clone(),
                            index,
                        })
                })
                .is_some_and(|candidate| self.candidate_is_helper_member(&candidate))
        }) {
            return false;
        }
        let Some(lhs) = dot.child_by_field_name("lhs") else {
            return true;
        };
        let receivers = self.resolve_receivers(occurrence_uri, document, offset, lhs);
        for receiver in receivers {
            let super::Receiver::Type(instance) = receiver else {
                continue;
            };
            let direct = self.direct_member_candidates(
                &instance.uri,
                &instance.key,
                instance.scope,
                Some(&binding.old_key),
                instance.uri == *occurrence_uri,
            );
            if direct
                .iter()
                .any(|candidate| binding.matches_candidate(self, candidate))
            {
                return false;
            }
        }
        true
    }

    fn check_declaration_collisions(&self, binding: &Binding, new_key: &str) -> Result<(), String> {
        self.check_declaration_collisions_bounded(binding, new_key, None, None)
    }

    fn check_declaration_collisions_bounded(
        &self,
        binding: &Binding,
        new_key: &str,
        cancel: Option<&AtomicBool>,
        shared_work_budget: Option<&mut super::AssistanceBudget>,
    ) -> Result<(), String> {
        let mut shared_work_budget = shared_work_budget;
        let mut target_ids = binding.members.iter().collect::<Vec<_>>();
        charge_shared_work(
            &mut shared_work_budget,
            cancel,
            comparison_sort_work(target_ids.len()),
        )?;
        charge_shared_bytes(
            &mut shared_work_budget,
            cancel,
            target_ids
                .len()
                .saturating_mul(std::mem::size_of::<&SymbolId>()),
        )?;
        target_ids.sort_by(|left, right| {
            left.uri
                .as_str()
                .cmp(right.uri.as_str())
                .then_with(|| left.span.start.cmp(&right.span.start))
                .then_with(|| left.span.end.cmp(&right.span.end))
        });
        for target_id in target_ids {
            charge_shared_work(&mut shared_work_budget, cancel, 1)?;
            let Some(document) = self.documents.get(&target_id.uri) else {
                return Err("rename declaration disappeared from the index".to_string());
            };
            let Some(target) = document
                .symbols
                .iter()
                .find(|symbol| symbol_id(&target_id.uri, symbol) == *target_id)
            else {
                return Err("rename declaration disappeared from the index".to_string());
            };

            charge_shared_work(&mut shared_work_budget, cancel, document.symbols.len())?;
            for symbol in &document.symbols {
                check_cancel(cancel)?;
                let other_id = symbol_id(&target_id.uri, symbol);
                if binding.members.contains(&other_id) || symbol.key != new_key {
                    continue;
                }
                if target.kind == SymbolKind::Unit && symbol.kind == SymbolKind::Unit {
                    return Err(format!(
                        "rename collides with unit declaration in {}",
                        target_id.uri
                    ));
                }
                let same_scope = if target.local_only && target.kind == SymbolKind::Parameter {
                    symbol.local_only
                        && symbol.kind == SymbolKind::Parameter
                        && same_parameter_routine(document, &target_id.uri, target, symbol)
                } else {
                    target.scope == symbol.scope && target.owner_type == symbol.owner_type
                };
                if same_scope {
                    return Err(format!(
                        "rename collides with declaration at {}:{}",
                        target_id.uri, symbol.span.start
                    ));
                }
            }
        }

        if binding.kind == SymbolKind::Unit {
            let mut documents = self.documents.iter().collect::<Vec<_>>();
            charge_shared_work(
                &mut shared_work_budget,
                cancel,
                comparison_sort_work(documents.len()),
            )?;
            charge_shared_bytes(
                &mut shared_work_budget,
                cancel,
                documents
                    .len()
                    .saturating_mul(std::mem::size_of::<(&Url, &Document)>()),
            )?;
            documents.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
            for (uri, document) in documents {
                charge_shared_work(&mut shared_work_budget, cancel, document.symbols.len())?;
                if document.symbols.iter().any(|symbol| {
                    symbol.kind == SymbolKind::Unit
                        && symbol.key == new_key
                        && !binding.members.contains(&symbol_id(uri, symbol))
                }) {
                    return Err(format!("rename collides with unit declaration in {uri}"));
                }
            }
        }
        Ok(())
    }

    fn check_reference_capture(&self, plan: &RenamePlan, new_name: &str) -> Result<(), String> {
        self.check_reference_capture_bounded(plan, new_name, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn proposed_name_candidates_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        identifier: Node<'_>,
        offset: usize,
        new_name: &str,
        binding_kind: SymbolKind,
        cancel: &AtomicBool,
        budget: &mut super::AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        budget.require_bytes(new_name.len(), cancel)?;
        if let Some(dot) = member_expression_at(identifier) {
            budget.require_work(1, cancel)?;
            let mut state = ResolutionState::new();
            if is_right_hand_member(dot, identifier) {
                return self.member_references_with_state_and_budget(
                    uri, document, offset, dot, new_name, identifier, &mut state, 0, cancel, budget,
                );
            }
            return self.unqualified_references_with_budget_and_state(
                uri, document, offset, new_name, identifier, &mut state, cancel, budget,
            );
        }
        if let Some((mut path, cursor_index)) =
            qualified_type_path_at_with_budget(identifier, &document.source, cancel, budget)?
        {
            budget.require_bytes(new_name.len(), cancel)?;
            if let Some(part) = path.get_mut(cursor_index) {
                *part = new_name.to_owned();
            }
            return self.type_reference_candidates_with_budget(
                uri,
                document,
                offset,
                identifier,
                &path,
                cursor_index,
                cancel,
                budget,
            );
        }
        if super::use_name_at(identifier, &document.source).is_some() {
            if binding_kind != SymbolKind::Unit {
                return Ok(Vec::new());
            }
            let key = canonical_name(new_name);
            let mut documents = self.documents.iter().collect::<Vec<_>>();
            documents.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
            let mut result = Vec::new();
            for (candidate_uri, candidate_document) in documents {
                budget.require_work(1, cancel)?;
                budget.require_work(candidate_document.symbols.len(), cancel)?;
                budget.require_bytes(
                    candidate_uri.as_str().len()
                        + candidate_document
                            .symbols
                            .len()
                            .saturating_mul(std::mem::size_of::<Symbol>()),
                    cancel,
                )?;
                for (index, symbol) in candidate_document.symbols.iter().enumerate() {
                    if symbol.kind == SymbolKind::Unit && symbol.key == key {
                        result.push(Candidate {
                            uri: candidate_uri.clone(),
                            index,
                        });
                    }
                }
            }
            return Ok(result);
        }
        let mut state = ResolutionState::new();
        self.unqualified_references_with_budget_and_state(
            uri, document, offset, new_name, identifier, &mut state, cancel, budget,
        )
    }

    fn check_reference_capture_bounded(
        &self,
        plan: &RenamePlan,
        new_name: &str,
        cancel: Option<&AtomicBool>,
        shared_work_budget: Option<&mut super::AssistanceBudget>,
    ) -> Result<(), String> {
        let mut shared_work_budget = shared_work_budget;
        for occurrence in &plan.occurrences {
            check_cancel(cancel)?;
            charge_shared_work(&mut shared_work_budget, cancel, 1)?;
            charge_shared_bytes(&mut shared_work_budget, cancel, new_name.len())?;
            let Some(document) = self.documents.get(&occurrence.uri) else {
                return Err("rename occurrence disappeared from the index".to_string());
            };
            let Some(identifier) = identifier_at(document.tree.root_node(), occurrence.span.start)
            else {
                return Err("rename occurrence is not present in the source tree".to_string());
            };
            if plan.binding.contains_span(&occurrence.uri, occurrence.span) {
                continue;
            }

            let offset = occurrence.span.start;
            let fallback_cancel = AtomicBool::new(false);
            let candidates = if let Some(budget) = shared_work_budget.as_deref_mut() {
                self.proposed_name_candidates_with_budget(
                    &occurrence.uri,
                    document,
                    identifier,
                    offset,
                    new_name,
                    plan.binding.kind,
                    cancel.unwrap_or(&fallback_cancel),
                    budget,
                )?
            } else if let Some(dot) = member_expression_at(identifier) {
                if is_right_hand_member(dot, identifier) {
                    self.member_references(&occurrence.uri, document, offset, dot, new_name)
                } else {
                    self.unqualified_references(&occurrence.uri, document, offset, new_name)
                }
            } else if let Some((mut path, cursor_index)) =
                qualified_type_path_at(identifier, &document.source)
            {
                if let Some(part) = path.get_mut(cursor_index) {
                    *part = new_name.to_owned();
                }
                self.type_reference_candidates(
                    &occurrence.uri,
                    document,
                    offset,
                    &path,
                    cursor_index,
                )
            } else if super::use_name_at(identifier, &document.source).is_some() {
                if plan.binding.kind == SymbolKind::Unit {
                    self.unit_candidates_for_key(new_name)
                } else {
                    Vec::new()
                }
            } else {
                self.unqualified_references(&occurrence.uri, document, offset, new_name)
            };

            charge_shared_work(
                &mut shared_work_budget,
                cancel,
                candidates.len().saturating_add(1),
            )?;

            if candidates.iter().any(|candidate| {
                !plan.binding.matches_candidate(self, candidate)
                    && !plan
                        .binding
                        .shadows_candidate(self, &occurrence.uri, candidate)
            }) {
                return Err(format!(
                    "rename would capture a reference at {}:{}",
                    occurrence.uri, occurrence.span.start
                ));
            }
        }

        if plan.binding.kind == SymbolKind::Unit {
            let new_key = canonical_name(new_name);
            let mut symbol_count = 0usize;
            for document in self.documents.values() {
                symbol_count = symbol_count.saturating_add(document.symbols.len());
            }
            charge_shared_work(&mut shared_work_budget, cancel, symbol_count)?;
            if self
                .documents
                .values()
                .flat_map(|document| document.symbols.iter())
                .any(|symbol| symbol.kind == SymbolKind::Unit && symbol.key == new_key)
                && plan.binding.old_key != new_key
            {
                return Err("rename would capture a unit reference".to_string());
            }
        }
        Ok(())
    }

    fn unit_candidates_for_key(&self, name: &str) -> Vec<Candidate> {
        let key = canonical_name(name);
        self.documents
            .iter()
            .flat_map(|(uri, document)| {
                document
                    .symbols
                    .iter()
                    .enumerate()
                    .filter(|(_, symbol)| symbol.kind == SymbolKind::Unit && symbol.key == key)
                    .map(|(index, _)| Candidate {
                        uri: uri.clone(),
                        index,
                    })
            })
            .collect()
    }

    fn range_for_occurrence(&self, uri: &Url, span: Span) -> Result<Range, String> {
        let document = self
            .documents
            .get(uri)
            .ok_or_else(|| format!("document is not indexed: {uri}"))?;
        let start = super::text::offset_to_position(&document.source, span.start)
            .ok_or_else(|| "cannot map rename start to an LSP position".to_string())?;
        let end = super::text::offset_to_position(&document.source, span.end)
            .ok_or_else(|| "cannot map rename end to an LSP position".to_string())?;
        Ok(Range { start, end })
    }
}

fn call_argument_for_identifier_with_budget<'a>(
    identifier: Node<'a>,
    cancel: &AtomicBool,
    budget: &mut super::AssistanceBudget,
) -> Result<Option<(Node<'a>, usize, Node<'a>)>, String> {
    let span = Span::from_node(identifier);
    let mut current = identifier.parent();
    while let Some(node) = current {
        check_cancel(Some(cancel))?;
        budget.require_work(1, cancel)?;
        if node.kind() == "exprCall" {
            let Some(arguments) = node.child_by_field_name("args") else {
                return Ok(None);
            };
            if !Span::from_node(arguments).contains(span) {
                return Ok(None);
            }
            let mut argument_index = 0usize;
            for index in 0..arguments.named_child_count() {
                check_cancel(Some(cancel))?;
                budget.require_work(1, cancel)?;
                let Some(argument) = arguments.named_child(index) else {
                    continue;
                };
                if argument.kind() == "legacyFormat" {
                    continue;
                }
                if Span::from_node(argument).contains(span) {
                    return Ok(Some((node, argument_index, argument)));
                }
                argument_index = argument_index.saturating_add(1);
            }
            return Ok(None);
        }
        current = node.parent();
    }
    Ok(None)
}

fn unresolved_call_target_cache_key_with_budget(
    current_uri: &Url,
    document: &Document,
    call: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut super::AssistanceBudget,
) -> Result<Option<UnresolvedCallTargetCacheKey>, String> {
    budget.require_work(1, cancel)?;
    let Some(entity) = call.child_by_field_name("entity") else {
        return Ok(None);
    };
    if entity.kind() != "identifier" {
        return Ok(None);
    }
    let identifier = super::callable_lookup_identifier(entity);
    if identifier.kind() != "identifier" {
        return Ok(None);
    }
    let offset = identifier.start_byte();
    let scope = document.scope_at(offset);
    if !cacheable_unqualified_use_with_budget(document, identifier, scope, cancel, budget)? {
        return Ok(None);
    }
    let name = super::canonical_name(super::node_text_with_budget(
        identifier,
        &document.source,
        cancel,
        budget,
    )?);
    let owner_type = document.owner_type_at_identifier(identifier, scope);
    budget.require_owned_bytes(
        current_uri
            .as_str()
            .len()
            .saturating_add(name.len())
            .saturating_add(owner_type.as_deref().map_or(0, str::len)),
        cancel,
    )?;
    Ok(Some(UnresolvedCallTargetCacheKey {
        uri: current_uri.clone(),
        name,
        scope,
        region: document.region_at(offset),
        owner_type,
    }))
}

fn unresolved_call_target_cache_bytes(key: &UnresolvedCallTargetCacheKey) -> usize {
    std::mem::size_of::<UnresolvedCallTargetCacheKey>()
        .saturating_add(64)
        .saturating_add(key.uri.as_str().len())
        .saturating_add(key.name.len())
        .saturating_add(key.owner_type.as_deref().map_or(0, str::len))
}

fn assignment_lhs_for_identifier_with_budget<'a>(
    identifier: Node<'a>,
    cancel: &AtomicBool,
    budget: &mut super::AssistanceBudget,
) -> Result<Option<(Node<'a>, Node<'a>)>, String> {
    let span = Span::from_node(identifier);
    let mut current = identifier.parent();
    while let Some(node) = current {
        check_cancel(Some(cancel))?;
        budget.require_work(1, cancel)?;
        if node.kind() == "assignment" {
            let Some(lhs) = node.child_by_field_name("lhs") else {
                return Ok(None);
            };
            return Ok(Span::from_node(lhs).contains(span).then_some((node, lhs)));
        }
        current = node.parent();
    }
    Ok(None)
}

fn lvalue_terminal_for_identifier_with_budget(
    lhs: Node<'_>,
    identifier: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut super::AssistanceBudget,
) -> Result<bool, String> {
    let target = Span::from_node(identifier);
    let mut current = lhs;
    loop {
        check_cancel(Some(cancel))?;
        budget.require_work(1, cancel)?;
        match current.kind() {
            "identifier" => return Ok(Span::from_node(current) == target),
            "exprParens" => {
                let Some(operand) = current.named_child(0) else {
                    return Ok(false);
                };
                current = operand;
            }
            // The receiver and every index expression are read in order to
            // locate the assigned element; the element itself has no separate
            // identifier node in these forms.
            "exprSubscript" | "exprUnary" => return Ok(false),
            "exprDot" | "genericDot" => {
                let Some(rhs) = current.child_by_field_name("rhs") else {
                    return Ok(false);
                };
                if !Span::from_node(rhs).contains(target) {
                    return Ok(false);
                }
                current = rhs;
            }
            _ => return Ok(false),
        }
    }
}

fn identifier_is_foreach_iterator_with_budget(
    identifier: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut super::AssistanceBudget,
) -> Result<bool, String> {
    let span = Span::from_node(identifier);
    let mut current = identifier.parent();
    while let Some(node) = current {
        check_cancel(Some(cancel))?;
        budget.require_work(1, cancel)?;
        if node.kind() == "foreach"
            && node
                .child_by_field_name("iterator")
                .is_some_and(|iterator| Span::from_node(iterator).contains(span))
        {
            return Ok(true);
        }
        current = node.parent();
    }
    Ok(false)
}

fn is_address_operand_with_budget(
    identifier: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut super::AssistanceBudget,
) -> Result<bool, String> {
    let mut current = identifier.parent();
    while let Some(node) = current {
        check_cancel(Some(cancel))?;
        budget.require_work(1, cancel)?;
        if node.kind() == "exprUnary"
            && node
                .child_by_field_name("operator")
                .is_some_and(|operator| operator.kind() == "kAt")
        {
            return Ok(true);
        }
        current = node.parent();
    }
    Ok(false)
}

fn binding_is_writable_with_budget(
    index: &NavigationIndex,
    binding: &Binding,
    cancel: &AtomicBool,
    budget: &mut super::AssistanceBudget,
) -> Result<bool, String> {
    if binding.members.is_empty() {
        budget.require_work(1, cancel)?;
        return Ok(false);
    }
    for member in &binding.members {
        check_cancel(Some(cancel))?;
        let Some(document) = index.documents.get(&member.uri) else {
            return Ok(false);
        };
        budget.require_work(document.symbols.len().saturating_add(1), cancel)?;
        budget.require_bytes(
            member.uri.as_str().len().saturating_add(
                document
                    .symbols
                    .len()
                    .saturating_mul(std::mem::size_of::<Symbol>()),
            ),
            cancel,
        )?;
        let mut symbol = None;
        for candidate in &document.symbols {
            check_cancel(Some(cancel))?;
            if symbol_id(&member.uri, candidate) == *member {
                symbol = Some(candidate);
                break;
            }
        }
        let Some(symbol) = symbol else {
            return Ok(false);
        };
        let writable = match symbol.kind {
            SymbolKind::Variable | SymbolKind::Field => true,
            SymbolKind::Parameter => matches!(
                symbol.parameter_mode,
                Some(
                    super::ParameterMode::Value
                        | super::ParameterMode::Var
                        | super::ParameterMode::Out
                )
            ),
            _ => false,
        };
        if !writable {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Binding {
    fn matches_candidate(&self, index: &NavigationIndex, candidate: &Candidate) -> bool {
        let Some(symbol) = index.symbol(candidate) else {
            return false;
        };
        self.members.contains(&symbol_id(&candidate.uri, symbol))
    }

    fn shadows_candidate(
        &self,
        index: &NavigationIndex,
        occurrence_uri: &Url,
        candidate: &Candidate,
    ) -> bool {
        let Some(document) = index.documents.get(occurrence_uri) else {
            return false;
        };
        let target_scopes = self
            .members
            .iter()
            .filter(|member| member.uri == *occurrence_uri)
            .filter_map(|member| {
                let target = document
                    .symbols
                    .iter()
                    .find(|symbol| symbol_id(occurrence_uri, symbol) == *member)?;
                (target.owner_type.is_none() && target.scope != super::ROOT_SCOPE)
                    .then_some(target.scope)
            })
            .collect::<HashSet<_>>();
        if target_scopes.is_empty() {
            return false;
        }
        let Some(candidate_document) = index.documents.get(&candidate.uri) else {
            return false;
        };
        if candidate.uri != *occurrence_uri {
            return true;
        }
        let Some(candidate_symbol) = index.symbol(candidate) else {
            return false;
        };
        target_scopes.iter().any(|target_scope| {
            candidate_symbol.scope != *target_scope
                && scope_is_ancestor(candidate_document, candidate_symbol.scope, *target_scope)
        })
    }
}

impl Binding {
    fn contains_span(&self, uri: &Url, span: Span) -> bool {
        self.members
            .iter()
            .any(|member| member.uri == *uri && member.span == span)
    }
}

fn scope_is_ancestor(document: &Document, ancestor: usize, descendant: usize) -> bool {
    let mut current = Some(descendant);
    while let Some(scope) = current {
        if scope == ancestor {
            return true;
        }
        current = document.scopes.get(scope).and_then(|scope| scope.parent);
    }
    false
}

fn binding_from_candidates(
    index: &NavigationIndex,
    candidates: Vec<Candidate>,
    cancel: Option<&AtomicBool>,
    shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
) -> Result<Binding, String> {
    let fallback_cancel = AtomicBool::new(false);
    let cancel = cancel.unwrap_or(&fallback_cancel);
    let mut groups: HashMap<BindingGroup, Vec<SymbolId>> = HashMap::new();
    let mut kind = None;
    let mut old_key = None;
    for candidate in candidates {
        let Some(symbol) = index.symbol(&candidate) else {
            continue;
        };
        let id = symbol_id(&candidate.uri, symbol);
        let group = match symbol.kind {
            SymbolKind::Routine => {
                let Some(routine_key) = symbol.routine_key.clone() else {
                    return Err("routine has no stable binding key".to_string());
                };
                BindingGroup::Routine {
                    uri: candidate.uri.clone(),
                    key: routine_key,
                }
            }
            SymbolKind::Parameter => index
                .documents
                .get(&candidate.uri)
                .and_then(|document| parameter_binding_key(&candidate.uri, document, symbol))
                .map_or_else(
                    || BindingGroup::Symbol(id.clone()),
                    |(key, _)| BindingGroup::Parameter(key),
                ),
            _ => BindingGroup::Symbol(id.clone()),
        };
        groups.entry(group).or_default().push(id);
        kind.get_or_insert(symbol.kind);
        old_key.get_or_insert_with(|| symbol.key.clone());
    }

    if groups.len() != 1 {
        return Err("rename target is unresolved or ambiguous".to_string());
    }
    let (group, ids) = groups.into_iter().next().expect("group count checked");
    let mut members: HashSet<SymbolId> = ids.into_iter().collect();
    let kind = kind.ok_or_else(|| "rename target is unresolved".to_string())?;
    let old_key = old_key.ok_or_else(|| "rename target is unresolved".to_string())?;

    if let BindingGroup::Routine { uri, .. } = &group {
        let Some(document) = index.documents.get(uri) else {
            return Err("routine binding document disappeared".to_string());
        };
        let mut declarations = 0;
        let mut definitions = 0;
        for id in &members {
            let Some(symbol) = document
                .symbols
                .iter()
                .find(|symbol| symbol_id(uri, symbol) == *id)
            else {
                return Err("routine binding declaration disappeared".to_string());
            };
            match symbol.origin {
                super::Origin::Declaration => declarations += 1,
                super::Origin::Definition => definitions += 1,
            }
        }
        if declarations > 1 || definitions > 1 {
            return Err("routine binding has multiple declarations and definitions".to_string());
        }
        // A virtual/dynamic routine is a slot rather than an ordinary
        // same-named routine.  Expand only an explicitly proven override
        // family here.  In particular, do not widen by name: reintroduced
        // methods, hiding methods, and independent overloads remain separate
        // bindings.  The expansion is performed before occurrence collection
        // so declarations, implementations, and calls all use one identity.
        members = expand_override_family(index, members, cancel, shared_work_budget)?;
    }

    if kind == SymbolKind::Parameter {
        members = pair_parameter_members(index, members)?;
    }

    let names = members
        .iter()
        .filter_map(|member| {
            let document = index.documents.get(&member.uri)?;
            let symbol = document
                .symbols
                .iter()
                .find(|symbol| symbol_id(&member.uri, symbol) == *member)?;
            Some(symbol.key.clone())
        })
        .collect();

    Ok(Binding {
        old_key,
        kind,
        members,
        names,
    })
}

/// Expand one routine binding to the source-backed virtual slot it belongs to.
///
/// This deliberately uses declaration identity, exact parameter/result shape,
/// static-ness, and proven class ancestry.  A matching spelling is never
/// enough: a method must explicitly be `override` (or be the selected virtual
/// root), and an unknown ancestry result is an error rather than permission to
/// guess.  This is the small semantic family needed by references/rename;
/// ordinary overloads and `reintroduce` methods remain independent bindings.
fn expand_override_family(
    index: &NavigationIndex,
    initial_members: HashSet<SymbolId>,
    cancel: &AtomicBool,
    shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
) -> Result<HashSet<SymbolId>, String> {
    check_cancel(Some(cancel))?;
    let Some(selected) = initial_members.iter().next().cloned() else {
        return Err("routine binding is empty".to_string());
    };
    let Some(selected_document) = index.documents.get(&selected.uri) else {
        return Err("routine binding document disappeared".to_string());
    };
    let Some(selected_symbol) = selected_document
        .symbols
        .iter()
        .find(|symbol| symbol_id(&selected.uri, symbol) == selected)
    else {
        return Err("routine binding declaration disappeared".to_string());
    };

    let Some(selected_owner) = selected_symbol.owner_type.clone() else {
        return Ok(initial_members);
    };
    if selected_symbol.is_static {
        if selected_symbol.routine_directives.virtual_
            || selected_symbol.routine_directives.dynamic
            || selected_symbol.routine_directives.override_
        {
            return Err("static virtual override family is unsupported".to_string());
        }
        return Ok(initial_members);
    }
    if selected_symbol
        .routine_directives
        .calling_convention_unknown
        && (selected_symbol.routine_directives.virtual_
            || selected_symbol.routine_directives.dynamic
            || selected_symbol.routine_directives.override_)
    {
        return Err("override family calling convention is unknown".to_string());
    }
    if !selected_symbol.routine_directives.virtual_
        && !selected_symbol.routine_directives.dynamic
        && !selected_symbol.routine_directives.override_
    {
        return Ok(initial_members);
    }

    // A selected virtual/override routine is itself an ancestry-sensitive
    // proof anchor.  Validate the complete selected owner ancestry even when
    // no explicit override candidate is currently indexed; an unavailable
    // parent may carry another slot or an interface obligation.
    let mut selected_ancestry = super::AncestryResolutionState::new();
    let selected_ancestry_result =
        index.resolve_type_ancestry(&selected.uri, &selected_owner, &mut selected_ancestry);
    if selected_ancestry_result.status != super::AncestryStatus::Complete {
        return Err("override family selected root ancestry is unknown".to_string());
    }

    let selected_name = selected_symbol.key.clone();
    let selected_kind = selected_symbol.routine_kind;

    let mut family = initial_members;
    let mut candidates = Vec::<(Url, usize, SymbolId)>::new();
    for (uri, document) in &index.documents {
        for (symbol_index, symbol) in document.symbols.iter().enumerate() {
            check_cancel(Some(cancel))?;
            charge_shared_work(shared_work_budget, Some(cancel), 1)?;
            if symbol.kind != SymbolKind::Routine
                || symbol.key != selected_name
                || symbol.owner_type.is_none()
                || symbol.is_static
                || symbol.routine_kind != selected_kind
            {
                continue;
            }
            let id = symbol_id(uri, symbol);
            charge_shared_bytes(
                shared_work_budget,
                Some(cancel),
                uri.as_str()
                    .len()
                    .saturating_add(std::mem::size_of::<SymbolId>()),
            )?;
            candidates.push((uri.clone(), symbol_index, id));
        }
    }

    let anchor_id = if selected_symbol.routine_directives.override_ {
        let mut roots = Vec::new();
        for (candidate_uri, candidate_index, candidate_id) in &candidates {
            check_cancel(Some(cancel))?;
            charge_shared_work(shared_work_budget, Some(cancel), 1)?;
            let Some(candidate_symbol) = index
                .documents
                .get(candidate_uri)
                .and_then(|document| document.symbols.get(*candidate_index))
            else {
                return Err("override family member disappeared".to_string());
            };
            let Some(candidate_owner) = candidate_symbol.owner_type.as_deref() else {
                continue;
            };
            if candidate_symbol.routine_directives.override_
                || (!candidate_symbol.routine_directives.virtual_
                    && !candidate_symbol.routine_directives.dynamic)
                || candidate_uri == &selected.uri && candidate_owner == selected_owner.as_str()
            {
                continue;
            }
            let Some(distance) = proven_type_distance(
                index,
                &selected.uri,
                selected_owner.as_str(),
                candidate_uri,
                candidate_owner,
                cancel,
                shared_work_budget,
            )?
            else {
                continue;
            };
            if !proven_override_slot_path(
                index,
                selected_symbol,
                &selected.uri,
                selected_owner.as_str(),
                candidate_uri,
                candidate_owner,
                cancel,
                shared_work_budget,
            )? {
                continue;
            }
            match override_contract_match(
                index,
                selected_symbol,
                &selected.uri,
                selected_owner.as_str(),
                candidate_symbol,
                candidate_uri,
                candidate_owner,
                cancel,
                shared_work_budget,
            )? {
                super::ContractMatch::Yes => {}
                super::ContractMatch::No => continue,
                super::ContractMatch::Unknown => {
                    return Err("override family root signature is unknown".to_string());
                }
            }
            let Some(routine_key) = candidate_symbol.routine_key.clone() else {
                return Err("override family routine has no stable identity".to_string());
            };
            if !roots
                .iter()
                .any(|(_, uri, key, _)| uri == candidate_uri && key == &routine_key)
            {
                roots.push((
                    distance,
                    candidate_uri.clone(),
                    routine_key,
                    candidate_id.clone(),
                ));
            }
        }
        roots.sort_by_key(|(distance, _, _, _)| *distance);
        let Some((distance, _, _, root)) = roots.first().cloned() else {
            return Err("override family has no proven virtual root".to_string());
        };
        if roots
            .get(1)
            .is_some_and(|(other, _, _, _)| *other == distance)
        {
            return Err("override family has ambiguous virtual roots".to_string());
        }
        Some(root)
    } else {
        None
    };
    let (anchor_uri, anchor_owner, anchor_symbol) = if let Some(anchor_id) = anchor_id {
        let anchor_uri = anchor_id.uri.clone();
        let document = index
            .documents
            .get(&anchor_uri)
            .ok_or_else(|| "override family member document disappeared".to_string())?;
        let symbol = document
            .symbols
            .iter()
            .find(|symbol| symbol_id(&anchor_uri, symbol) == anchor_id)
            .ok_or_else(|| "override family member disappeared".to_string())?;
        let owner = symbol
            .owner_type
            .clone()
            .ok_or_else(|| "override family root has no owner".to_string())?;
        (anchor_uri, owner, symbol)
    } else {
        (
            selected.uri.clone(),
            selected_owner.clone(),
            selected_symbol,
        )
    };

    // The selected routine is itself a proof anchor.  A source-backed virtual
    // root may have no override directive, while a selected override proves
    // that its corresponding ancestor slot must be included.
    let selected_is_root =
        selected_symbol.routine_directives.virtual_ || selected_symbol.routine_directives.dynamic;
    if !selected_is_root && !selected_symbol.routine_directives.override_ {
        return Ok(family);
    }

    for (candidate_uri, candidate_index, candidate_id) in candidates {
        check_cancel(Some(cancel))?;
        charge_shared_work(shared_work_budget, Some(cancel), 1)?;
        let Some(candidate_symbol) = index
            .documents
            .get(&candidate_uri)
            .and_then(|document| document.symbols.get(candidate_index))
        else {
            return Err("override family member disappeared".to_string());
        };
        let Some(candidate_owner) = candidate_symbol.owner_type.as_deref() else {
            continue;
        };
        if candidate_uri == selected.uri && candidate_owner == selected_owner {
            if candidate_symbol.routine_key.as_deref() == selected_symbol.routine_key.as_deref() {
                family.insert(candidate_id);
            }
            continue;
        }
        if candidate_uri == anchor_uri && candidate_owner == anchor_owner {
            if candidate_symbol.routine_key.as_deref() == anchor_symbol.routine_key.as_deref() {
                family.insert(candidate_id);
            }
            continue;
        }

        // A same-named routine is eligible only when it is an explicit
        // override.  `reintroduce` and a plain same-signature declaration are
        // intentionally excluded even when the classes are related.
        if candidate_symbol.routine_directives.reintroduce
            || !candidate_symbol.routine_directives.override_
        {
            continue;
        }

        let descendant_relation = proven_type_descendant(
            index,
            &candidate_uri,
            candidate_owner,
            &anchor_uri,
            anchor_owner.as_str(),
            cancel,
            shared_work_budget,
        )?;
        if !descendant_relation {
            continue;
        }
        if descendant_relation
            && !proven_override_slot_path(
                index,
                candidate_symbol,
                &candidate_uri,
                candidate_owner,
                &anchor_uri,
                anchor_owner.as_str(),
                cancel,
                shared_work_budget,
            )?
        {
            continue;
        }

        // Compare the instantiated signatures, not their source spelling.
        // For `TBase<T>.Work(T)` overridden by `TChild.Work(Integer)`, the
        // parent substitution is part of the proof.  Unknown type identity or
        // generic constraints fail closed instead of widening by name.
        match override_contract_match(
            index,
            candidate_symbol,
            &candidate_uri,
            candidate_owner,
            anchor_symbol,
            &anchor_uri,
            anchor_owner.as_str(),
            cancel,
            shared_work_budget,
        )? {
            // An independent overload slot is not part of this exact family.
            // It must not make a proven selected slot fail or get merged.
            super::ContractMatch::No => continue,
            super::ContractMatch::Unknown => {
                return Err("override family signature is unknown".to_string());
            }
            super::ContractMatch::Yes => family.insert(candidate_id),
        };
    }

    // Ensure every source-backed family routine has at most one declaration
    // and definition.  A missing implementation is allowed only for an
    // abstract member; otherwise a rename would silently leave a required
    // source spelling behind.
    let mut sites: HashMap<(Url, String), (usize, usize, bool)> = HashMap::new();
    for member in &family {
        let Some(document) = index.documents.get(&member.uri) else {
            return Err("override family member document disappeared".to_string());
        };
        let Some(symbol) = document
            .symbols
            .iter()
            .find(|symbol| symbol_id(&member.uri, symbol) == *member)
        else {
            return Err("override family member disappeared".to_string());
        };
        let Some(owner) = symbol.owner_type.clone() else {
            continue;
        };
        let entry = sites.entry((member.uri.clone(), owner)).or_default();
        match symbol.origin {
            super::Origin::Declaration => entry.0 += 1,
            super::Origin::Definition => entry.1 += 1,
        }
        entry.2 |= symbol.routine_directives.abstract_;
    }
    if sites
        .values()
        .any(|(declarations, definitions, abstract_)| {
            *declarations > 1
                || *definitions > 1
                || *declarations != 1
                || (!*abstract_ && *definitions != 1)
        })
    {
        return Err("override family has incomplete or duplicate source members".to_string());
    }
    Ok(family)
}

#[allow(clippy::too_many_arguments)]
fn override_contract_match(
    index: &NavigationIndex,
    descendant_symbol: &Symbol,
    descendant_uri: &Url,
    descendant_owner: &str,
    ancestor_symbol: &Symbol,
    ancestor_uri: &Url,
    ancestor_owner: &str,
    cancel: &AtomicBool,
    shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
) -> Result<super::ContractMatch, String> {
    check_cancel(Some(cancel))?;
    let descendant_substitution = index
        .owner_type_substitution(descendant_uri, descendant_owner)
        .unwrap_or_else(GenericSubstitution::empty);
    let mut state = ResolutionState::new();
    let ancestor_substitution = index
        .member_owner_substitution(
            descendant_uri,
            descendant_owner,
            &descendant_substitution,
            ancestor_uri,
            ancestor_owner,
            &mut state,
        )
        .ok_or_else(|| "override family generic substitution is unknown".to_string())?;
    if let Some(budget) = shared_work_budget.as_deref_mut() {
        index.routines_contract_match(
            descendant_symbol,
            descendant_uri,
            &descendant_substitution,
            ancestor_symbol,
            ancestor_uri,
            &ancestor_substitution,
            cancel,
            budget,
        )
    } else {
        let mut budget =
            AssistanceBudget::new(16_384, 2 * 1024 * 1024, "override family signature proof");
        index.routines_contract_match(
            descendant_symbol,
            descendant_uri,
            &descendant_substitution,
            ancestor_symbol,
            ancestor_uri,
            &ancestor_substitution,
            cancel,
            &mut budget,
        )
    }
}

fn proven_type_distance(
    index: &NavigationIndex,
    descendant_uri: &Url,
    descendant: &str,
    ancestor_uri: &Url,
    ancestor: &str,
    cancel: &AtomicBool,
    shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
) -> Result<Option<usize>, String> {
    if descendant_uri == ancestor_uri && descendant == ancestor {
        return Ok(Some(0));
    }
    let mut state = super::AncestryResolutionState::new();
    let mut active = HashSet::new();

    #[allow(clippy::too_many_arguments)]
    fn visit(
        index: &NavigationIndex,
        current_uri: &Url,
        current: &str,
        ancestor_uri: &Url,
        ancestor: &str,
        depth: usize,
        state: &mut super::AncestryResolutionState,
        active: &mut HashSet<(Url, String)>,
        cancel: &AtomicBool,
        shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
    ) -> Result<Option<usize>, String> {
        check_cancel(Some(cancel))?;
        charge_shared_work(shared_work_budget, Some(cancel), 1)?;
        if current_uri == ancestor_uri && current == ancestor {
            return Ok(Some(depth));
        }
        let identity = (current_uri.clone(), current.to_owned());
        if !active.insert(identity.clone()) {
            return Err("override family ancestry is cyclic".to_string());
        }
        if !state.take_work() {
            active.remove(&identity);
            return Err("override family ancestry work limit reached".to_string());
        }
        let ancestry = index.resolve_type_ancestry(current_uri, current, state);
        if ancestry.status != super::AncestryStatus::Complete {
            active.remove(&identity);
            return Err("override family ancestry is unknown".to_string());
        }
        let mut found = None;
        for (parent_uri, parent_key) in ancestry.parents {
            if let Some(distance) = visit(
                index,
                &parent_uri,
                &parent_key,
                ancestor_uri,
                ancestor,
                depth + 1,
                state,
                active,
                cancel,
                shared_work_budget,
            )? {
                if found.is_some() {
                    active.remove(&identity);
                    return Err("override family ancestry is ambiguous".to_string());
                }
                found = Some(distance);
            }
        }
        active.remove(&identity);
        Ok(found)
    }

    visit(
        index,
        descendant_uri,
        descendant,
        ancestor_uri,
        ancestor,
        0,
        &mut state,
        &mut active,
        cancel,
        shared_work_budget,
    )
}

/// Return whether `descendant_uri::descendant` is a proven subclass of
/// `ancestor_uri::ancestor`.  The resolver retains URI identity and rejects
/// ambiguous/unknown ancestry, so same-name classes in separate units cannot
/// accidentally form a family.
fn proven_type_descendant(
    index: &NavigationIndex,
    descendant_uri: &Url,
    descendant: &str,
    ancestor_uri: &Url,
    ancestor: &str,
    cancel: &AtomicBool,
    shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
) -> Result<bool, String> {
    Ok(proven_type_distance(
        index,
        descendant_uri,
        descendant,
        ancestor_uri,
        ancestor,
        cancel,
        shared_work_budget,
    )?
    .is_some())
}

/// Prove that every same-signature declaration between a descendant and an
/// ancestor preserves the same virtual slot.  A virtual redeclaration or a
/// `reintroduce` boundary starts a new slot; a transitive ancestry check alone
/// must not connect the descendant back to the older root.
#[allow(clippy::too_many_arguments)]
fn proven_override_slot_path(
    index: &NavigationIndex,
    descendant_symbol: &Symbol,
    descendant_uri: &Url,
    descendant_owner: &str,
    ancestor_uri: &Url,
    ancestor_owner: &str,
    cancel: &AtomicBool,
    shared_work_budget: &mut Option<&mut super::AssistanceBudget>,
) -> Result<bool, String> {
    let mut current_uri = descendant_uri.clone();
    let mut current_owner = descendant_owner.to_owned();
    let mut state = super::AncestryResolutionState::new();
    let mut active = HashSet::new();
    loop {
        check_cancel(Some(cancel))?;
        charge_shared_work(shared_work_budget, Some(cancel), 1)?;
        if current_uri == *ancestor_uri && current_owner == ancestor_owner {
            return Ok(true);
        }
        let identity = (current_uri.clone(), current_owner.clone());
        if !active.insert(identity.clone()) {
            return Err("override family ancestry is cyclic".to_string());
        }
        if !state.take_work() {
            return Err("override family ancestry work limit reached".to_string());
        }
        let ancestry = index.resolve_type_ancestry(&current_uri, &current_owner, &mut state);
        if ancestry.status != super::AncestryStatus::Complete {
            return Err("override family ancestry is unknown".to_string());
        }
        let Some((parent_uri, parent_owner)) = ancestry
            .parents
            .into_iter()
            .find(|(_, key)| !key.is_empty())
        else {
            return Ok(false);
        };
        if parent_uri == *ancestor_uri && parent_owner == ancestor_owner {
            return Ok(true);
        }

        let Some(parent_document) = index.documents.get(&parent_uri) else {
            return Err("override family ancestor document disappeared".to_string());
        };
        let mut matching_parent = None;
        let mut seen_parent_routines = HashSet::new();
        for parent_candidate in parent_document.symbols.iter().filter(|symbol| {
            symbol.kind == SymbolKind::Routine
                && symbol.key == descendant_symbol.key
                && symbol.owner_type.as_deref() == Some(parent_owner.as_str())
                && !symbol.is_static
                && symbol.routine_kind == descendant_symbol.routine_kind
        }) {
            let Some(routine_key) = parent_candidate.routine_key.as_deref() else {
                return Err("override family ancestor has no stable routine identity".to_string());
            };
            if !seen_parent_routines.insert(routine_key.to_owned()) {
                continue;
            }
            check_cancel(Some(cancel))?;
            charge_shared_work(shared_work_budget, Some(cancel), 1)?;
            match override_contract_match(
                index,
                descendant_symbol,
                descendant_uri,
                descendant_owner,
                parent_candidate,
                &parent_uri,
                &parent_owner,
                cancel,
                shared_work_budget,
            )? {
                super::ContractMatch::Yes => {}
                super::ContractMatch::No => continue,
                super::ContractMatch::Unknown => {
                    return Err("override family intermediate signature is unknown".to_string());
                }
            }
            if matching_parent.is_some() {
                return Err("override family ancestry has ambiguous slot declarations".to_string());
            }
            matching_parent = Some(parent_candidate);
        }
        if let Some(parent_symbol) = matching_parent {
            if !parent_symbol.routine_directives.override_ {
                return Ok(false);
            }
        }
        active.remove(&identity);
        current_uri = parent_uri;
        current_owner = parent_owner;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ParameterBindingKey {
    uri: Url,
    routine_key: String,
    ordinal: usize,
}

fn pair_parameter_members(
    index: &NavigationIndex,
    initial_members: HashSet<SymbolId>,
) -> Result<HashSet<SymbolId>, String> {
    let Some(selected) = initial_members.iter().next() else {
        return Err("parameter binding is empty".to_string());
    };
    let Some(selected_document) = index.documents.get(&selected.uri) else {
        return Err("parameter binding document disappeared".to_string());
    };
    let Some(selected_symbol) = selected_document
        .symbols
        .iter()
        .find(|symbol| symbol_id(&selected.uri, symbol) == *selected)
    else {
        return Err("parameter binding declaration disappeared".to_string());
    };
    let Some((key, _)) = parameter_binding_key(&selected.uri, selected_document, selected_symbol)
    else {
        return Ok(initial_members);
    };
    let Some(routine) = selected_document.symbols.iter().find(|symbol| {
        symbol.kind == SymbolKind::Routine
            && symbol.routine_key.as_deref() == Some(key.routine_key.as_str())
    }) else {
        return Ok(initial_members);
    };
    if selected_document.symbols.iter().any(|symbol| {
        symbol.kind == SymbolKind::Routine
            && symbol.key == routine.key
            && symbol.scope == routine.scope
            && symbol.owner_type == routine.owner_type
            && symbol.routine_key.as_deref() != Some(key.routine_key.as_str())
    }) {
        return Err("overloaded routine parameter bindings are not supported".to_string());
    }

    let mut members = HashSet::new();
    let mut sites: HashMap<Span, bool> = HashMap::new();
    for (uri, document) in &index.documents {
        for symbol in &document.symbols {
            if symbol.kind != SymbolKind::Parameter {
                continue;
            }
            let Some((candidate_key, is_definition)) = parameter_binding_key(uri, document, symbol)
            else {
                continue;
            };
            if candidate_key != key {
                continue;
            }
            sites
                .entry(candidate_key_site(document, symbol))
                .or_insert(is_definition);
            members.insert(symbol_id(uri, symbol));
        }
    }

    let declaration_sites = sites
        .values()
        .filter(|is_definition| !**is_definition)
        .count();
    let definition_sites = sites
        .values()
        .filter(|is_definition| **is_definition)
        .count();
    if declaration_sites > 1 || definition_sites > 1 {
        return Err("parameter binding has multiple declaration sites".to_string());
    }
    if members.is_empty() {
        return Ok(initial_members);
    }
    Ok(members)
}

fn parameter_binding_key(
    uri: &Url,
    document: &Document,
    symbol: &Symbol,
) -> Option<(ParameterBindingKey, bool)> {
    let identifier = identifier_at(document.tree.root_node(), symbol.span.start)?;
    let mut current = Some(identifier);
    let mut argument = None;
    while let Some(node) = current {
        if node.kind() == "declArg" {
            argument = Some(node);
            break;
        }
        current = node.parent();
    }
    let argument = argument?;

    let mut routine_node = argument.parent();
    while let Some(node) = routine_node {
        if matches!(node.kind(), "declProc" | "defProc") {
            break;
        }
        routine_node = node.parent();
    }
    let routine_node = routine_node?;
    let header = if routine_node.kind() == "defProc" {
        routine_node.child_by_field_name("header")?
    } else {
        routine_node
    };
    let (_, routine_span, owner_type) = routine_name(header, &document.source)?;
    let signature = routine_signature(header, &document.source);
    let routine_symbol = document.symbols.iter().find(|candidate| {
        candidate.kind == SymbolKind::Routine
            && candidate.span == routine_span
            && candidate.owner_type == owner_type
            && candidate.routine_signature.as_deref() == Some(signature.as_str())
    })?;
    let routine_key = routine_symbol.routine_key.clone()?;

    let arguments = header.child_by_field_name("args")?;
    let mut ordinal = 0;
    let mut found = false;
    for candidate in collect_nodes_matching(arguments, "declArg") {
        for name in field_identifier_nodes(candidate, "name") {
            if Span::from_node(name) == symbol.span {
                found = true;
                break;
            }
            ordinal += 1;
        }
        if found {
            break;
        }
    }
    found.then_some((
        ParameterBindingKey {
            uri: uri.clone(),
            routine_key,
            ordinal,
        },
        routine_symbol.region == super::Region::Implementation,
    ))
}

fn same_parameter_routine(document: &Document, uri: &Url, left: &Symbol, right: &Symbol) -> bool {
    let Some((left_key, _)) = parameter_binding_key(uri, document, left) else {
        return false;
    };
    let Some((right_key, _)) = parameter_binding_key(uri, document, right) else {
        return false;
    };
    left_key.routine_key == right_key.routine_key
}

fn candidate_key_site(document: &Document, symbol: &Symbol) -> Span {
    let identifier = identifier_at(document.tree.root_node(), symbol.span.start);
    let Some(identifier) = identifier else {
        return symbol.span;
    };
    let mut current = Some(identifier);
    while let Some(node) = current {
        if matches!(node.kind(), "declProc" | "defProc") {
            return Span::from_node(node);
        }
        current = node.parent();
    }
    symbol.span
}

fn symbol_id(uri: &Url, symbol: &Symbol) -> SymbolId {
    SymbolId {
        uri: uri.clone(),
        span: symbol.span,
        scope: symbol.scope,
        kind: symbol.kind,
        origin: symbol.origin,
        owner_type: symbol.owner_type.clone(),
    }
}

fn has_forward_class_pair(index: &NavigationIndex, binding: &Binding) -> bool {
    if binding.kind != SymbolKind::Type {
        return false;
    }

    binding.members.iter().any(|member| {
        let Some(document) = index.documents.get(&member.uri) else {
            return false;
        };
        let Some(target) = declaration_node_for_span(document.tree.root_node(), member.span) else {
            return false;
        };
        if target.kind() != "declType" {
            return false;
        }
        let Some(target_name) = field_identifier_nodes(target, "name")
            .last()
            .map(|identifier| canonical_name(&node_text(*identifier, &document.source)))
        else {
            return false;
        };

        let mut has_forward = false;
        let mut has_completion = false;
        for declaration in collect_nodes_matching(document.tree.root_node(), "declType") {
            let Some(name) = field_identifier_nodes(declaration, "name")
                .last()
                .map(|identifier| canonical_name(&node_text(*identifier, &document.source)))
            else {
                continue;
            };
            if name != target_name {
                continue;
            }
            let Some(type_node) = declaration.child_by_field_name("type") else {
                continue;
            };
            if type_node.kind() != "declClass" {
                continue;
            }
            if contains_node_kind(type_node, "kEnd") {
                has_completion = true;
            } else {
                has_forward = true;
            }
        }
        has_forward && has_completion
    })
}

fn declaration_node_for_span(root: Node<'_>, span: Span) -> Option<Node<'_>> {
    let identifier = identifier_at(root, span.start)?;
    let mut current = Some(identifier);
    while let Some(node) = current {
        if node.kind() == "declType"
            && field_identifier_nodes(node, "name")
                .iter()
                .any(|name| Span::from_node(*name) == span)
        {
            return Some(node);
        }
        current = node.parent();
    }
    None
}

fn contains_node_kind(node: Node<'_>, kind: &str) -> bool {
    collect_nodes_matching(node, kind)
        .into_iter()
        .next()
        .is_some()
}

fn cacheable_unqualified_use(document: &Document, identifier: Node<'_>, scope: usize) -> bool {
    // Lambda bodies are not represented in `build_scopes`, so a root scope key
    // would incorrectly merge their captures with globals.
    if has_ancestor_kind(identifier, "lambda") {
        return false;
    }

    // Inline declarations (`varDef` and `varAssignDef`) change visibility, but
    // the current scope model does not record their boundaries. Their
    // containing scope was marked while the document was parsed, so this is a
    // constant-time eligibility check for every occurrence.
    !document.cache_unsafe_scopes.contains(&scope)
}

fn cacheable_unqualified_use_with_budget(
    document: &Document,
    identifier: Node<'_>,
    scope: usize,
    cancel: &AtomicBool,
    budget: &mut super::AssistanceBudget,
) -> Result<bool, String> {
    let mut current = Some(identifier);
    while let Some(node) = current {
        check_cancel(Some(cancel))?;
        budget.require_work(1, cancel)?;
        if node.kind() == "lambda" {
            return Ok(false);
        }
        current = node.parent();
    }
    Ok(!document.cache_unsafe_scopes.contains(&scope))
}

fn validate_new_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let escaped = chars.next() == Some('&');
    if escaped && name.len() == 1 {
        return Err("new name must be a Pascal identifier".to_string());
    }
    let first = if escaped {
        chars.next()
    } else {
        name.chars().next()
    };
    let Some(first) = first else {
        return Err("new name must not be empty".to_string());
    };
    if !is_identifier_start(first) {
        return Err("new name must start with a letter or underscore".to_string());
    }
    if chars.any(|character| !is_identifier_continue(character)) {
        return Err("new name contains invalid Pascal identifier characters".to_string());
    }

    let key = canonical_name(name);
    if !escaped && is_keyword(&key) {
        return Err("new name must not be a Pascal keyword".to_string());
    }
    Ok(())
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character.is_alphabetic()
}

fn is_identifier_continue(character: char) -> bool {
    is_identifier_start(character) || character.is_numeric()
}

fn is_keyword(name: &str) -> bool {
    matches!(
        name,
        "absolute"
            | "and"
            | "array"
            | "as"
            | "asm"
            | "abstract"
            | "begin"
            | "case"
            | "class"
            | "const"
            | "constructor"
            | "destructor"
            | "dispinterface"
            | "div"
            | "do"
            | "downto"
            | "else"
            | "end"
            | "except"
            | "exports"
            | "file"
            | "finalization"
            | "finally"
            | "for"
            | "function"
            | "goto"
            | "if"
            | "implementation"
            | "in"
            | "inherited"
            | "initialization"
            | "inline"
            | "interface"
            | "is"
            | "label"
            | "library"
            | "mod"
            | "nil"
            | "not"
            | "object"
            | "of"
            | "on"
            | "operator"
            | "or"
            | "out"
            | "packed"
            | "private"
            | "procedure"
            | "program"
            | "property"
            | "protected"
            | "public"
            | "published"
            | "raise"
            | "record"
            | "read"
            | "reintroduce"
            | "repeat"
            | "resourcestring"
            | "set"
            | "shl"
            | "shr"
            | "strict"
            | "string"
            | "stored"
            | "then"
            | "threadvar"
            | "to"
            | "try"
            | "type"
            | "unit"
            | "until"
            | "uses"
            | "var"
            | "while"
            | "with"
            | "write"
            | "xor"
            | "default"
            | "index"
            | "nodefault"
            | "self"
            | "result"
    )
}

fn contains_identifier(source: &str, span: Span, key: &str) -> bool {
    let Some(block) = source.get(span.start..span.end) else {
        return true;
    };
    let mut start = None;
    for (offset, character) in block.char_indices() {
        if start.is_none() {
            if character == '&' || is_identifier_start(character) {
                start = Some(offset);
            }
            continue;
        }
        if is_identifier_continue(character) {
            continue;
        }
        let token_start = start.take().expect("token start exists");
        if canonical_name(&block[token_start..offset]) == key {
            return true;
        }
        if character == '&' || is_identifier_start(character) {
            start = Some(offset);
        }
    }
    start.is_some_and(|token_start| canonical_name(&block[token_start..]) == key)
}
