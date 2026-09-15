use super::{
    Candidate, Document, NavigationIndex, ROOT_SCOPE, Span, Symbol, SymbolKind, canonical_name,
    collect_nodes_matching, field_identifier_nodes, has_ancestor_kind, identifier_at,
    identifier_nodes, is_ignored_offset, is_right_hand_member, member_expression_at, node_text,
    qualified_type_path_at, routine_name, routine_signature, use_name_at,
};
use crate::text::PositionIndex;
use lsp_types::{PrepareRenameResponse, Range, TextEdit, Url};
#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use tree_sitter::Node;

const MAX_BINDING_LOCATIONS: usize = 10_000;
const CANCELLATION_MESSAGE: &str = "request cancelled";

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

#[derive(Debug)]
struct RenamePlan {
    binding: Binding,
    selected_span: Span,
    occurrences: Vec<Occurrence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BindingGroup {
    Symbol(SymbolId),
    Routine { uri: Url, key: String },
    Parameter(ParameterBindingKey),
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
        self.binding_locations_impl(uri, position, include_declaration, None, true, None)
    }

    pub(crate) fn binding_locations_with_cancel(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        include_declaration: bool,
        cancel: &AtomicBool,
    ) -> Result<Vec<lsp_types::Location>, String> {
        self.binding_locations_impl(uri, position, include_declaration, None, true, Some(cancel))
    }

    pub(crate) fn binding_locations_in_document_with_cancel(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        cancel: &AtomicBool,
    ) -> Result<Vec<lsp_types::Location>, String> {
        self.binding_locations_impl(uri, position, true, Some(uri), false, Some(cancel))
    }

    fn binding_locations_impl(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        include_declaration: bool,
        document_uri: Option<&Url>,
        strict_resolution: bool,
        cancel: Option<&AtomicBool>,
    ) -> Result<Vec<lsp_types::Location>, String> {
        check_cancel(cancel)?;
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
        if !strict_resolution
            && document_uri.is_some()
            && !self.selected_occurrence_is_supported(uri, document, selected_identifier, offset)
        {
            return Ok(Vec::new());
        }

        let (binding, _) = self.binding_plan(uri, position)?;
        let occurrences = self.collect_occurrences_bounded(
            &binding,
            document_uri,
            include_declaration,
            strict_resolution,
            Some(MAX_BINDING_LOCATIONS),
            cancel,
        )?;
        self.locations_for_occurrences(&occurrences, cancel)
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
        let (binding, _) = self.binding_plan_with_cancel(uri, position, Some(cancel))?;
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
        Ok(RenameBindingInfo { local, names })
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

    pub(super) fn owner_has_unknown_class_ancestor(
        &self,
        document: &Document,
        owner_type: &str,
    ) -> bool {
        self.has_unknown_class_ancestor(document, owner_type)
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

    fn check_proposed_name_references(
        &self,
        plan: &RenamePlan,
        new_key: &str,
    ) -> Result<(), String> {
        for (uri, document) in &self.documents {
            let root = document.tree.root_node();
            for identifier in identifier_nodes(root) {
                let span = Span::from_node(identifier);
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
                let candidates = self.resolve_candidates_at(uri, document, span.start, identifier);
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
                ) {
                    return Err(format!(
                        "rename would change the binding of an existing {:?} reference at {uri}:{}",
                        new_key, span.start
                    ));
                }
            }
        }
        Ok(())
    }

    fn proposed_binding_visible_at(
        &self,
        binding: &Binding,
        occurrence_uri: &Url,
        document: &Document,
        offset: usize,
        candidates: &[Candidate],
    ) -> bool {
        let occurrence_scope = document.scope_at(offset);
        let occurrence_owner = document.owner_type_at(offset);

        for member in &binding.members {
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
                    return true;
                }
                if !candidates.iter().any(|candidate| {
                    let Some(candidate_symbol) = self.symbol(candidate) else {
                        return false;
                    };
                    candidate.uri == *occurrence_uri
                        && candidate_symbol.owner_type.is_none()
                        && candidate_symbol.scope != ROOT_SCOPE
                        && scope_is_ancestor(document, candidate_symbol.scope, occurrence_scope)
                }) {
                    return true;
                }
                continue;
            }

            if symbol.scope != ROOT_SCOPE {
                if member.uri != *occurrence_uri
                    || !scope_is_ancestor(document, symbol.scope, occurrence_scope)
                {
                    continue;
                }
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
                    return true;
                }
                continue;
            }

            if symbol.local_only
                || !self.root_binding_visible_at(member, occurrence_uri, document, offset)
            {
                continue;
            }
            let shadowed_by_local = candidates.iter().any(|candidate| {
                let Some(candidate_symbol) = self.symbol(candidate) else {
                    return false;
                };
                candidate.uri == *occurrence_uri
                    && candidate_symbol.owner_type.is_none()
                    && candidate_symbol.scope != ROOT_SCOPE
            });
            if !shadowed_by_local {
                return true;
            }
        }
        false
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
        let (binding, selected_span) = self.binding_plan_with_cancel(uri, position, cancel)?;
        let occurrences =
            self.collect_occurrences_bounded(&binding, None, true, true, None, cancel)?;
        if occurrences.is_empty() {
            return Err("rename binding has no source occurrences".to_string());
        }
        Ok(RenamePlan {
            binding,
            selected_span,
            occurrences,
        })
    }

    fn binding_plan(
        &self,
        uri: &Url,
        position: lsp_types::Position,
    ) -> Result<(Binding, Span), String> {
        self.binding_plan_with_cancel(uri, position, None)
    }

    fn binding_plan_with_cancel(
        &self,
        uri: &Url,
        position: lsp_types::Position,
        cancel: Option<&AtomicBool>,
    ) -> Result<(Binding, Span), String> {
        check_cancel(cancel)?;
        let document = self
            .documents
            .get(uri)
            .ok_or_else(|| format!("document is not indexed: {uri}"))?;
        let offset = super::text::position_to_offset(&document.source, position)
            .ok_or_else(|| "position is outside the source document".to_string())?;
        if is_ignored_offset(document.tree.root_node(), offset) {
            return Err("cannot rename an identifier in a comment or literal".to_string());
        }
        let identifier = identifier_at(document.tree.root_node(), offset)
            .ok_or_else(|| "no renameable identifier at position".to_string())?;
        let candidates = self.resolve_candidates_at(uri, document, offset, identifier);
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Err(
                "rename cannot prove the binding across an unknown conditional branch".to_string(),
            );
        }
        let binding = binding_from_candidates(self, candidates)?;
        check_cancel(cancel)?;
        if binding.kind == SymbolKind::Unit {
            return Err("unit/module rename requires RenameFile support".to_string());
        }
        if binding.kind == SymbolKind::Type && has_forward_class_pair(self, &binding) {
            return Err("forward class/completion type rename is not supported".to_string());
        }
        let selected_span = Span::from_node(identifier);
        Ok((binding, selected_span))
    }

    fn collect_occurrences_bounded(
        &self,
        binding: &Binding,
        document_uri: Option<&Url>,
        include_declaration: bool,
        strict_resolution: bool,
        result_limit: Option<usize>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Vec<Occurrence>, String> {
        let mut documents: Vec<(&Url, &Document)> = self.documents.iter().collect();
        documents.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));

        let mut occurrences = Vec::new();
        let mut seen = HashSet::new();
        let mut unqualified_cache =
            HashMap::<UnqualifiedReferenceCacheKey, (Vec<Candidate>, bool)>::new();
        let binding_has_class_owner = self.has_class_owned_member(binding);
        for (uri, document) in documents {
            check_cancel(cancel)?;
            if document_uri.is_some_and(|requested| requested != uri) {
                continue;
            }
            if strict_resolution
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
            for identifier in identifier_nodes(root) {
                check_occurrence_cancel(cancel)?;
                let span = Span::from_node(identifier);
                let name = canonical_name(&node_text(identifier, &document.source));
                let is_binding_member = binding.contains_span(uri, span);
                if !is_binding_member && !binding.names.contains(&name) {
                    continue;
                }
                if has_ancestor_kind(identifier, "ppDirective") {
                    continue;
                }
                if !document.parser_recovery_spans.is_empty()
                    && document.has_parser_recovery_near(span)
                {
                    if !strict_resolution && !is_binding_member {
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
                    if !strict_resolution && !is_binding_member {
                        continue;
                    }
                    return Err(format!(
                        "rename does not support with/inherited references at {}:{}",
                        uri, span.start
                    ));
                }
                if !include_declaration && is_binding_member {
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
                    && (!document.symbols.iter().any(|symbol| symbol.span == span)
                        || binding.kind == SymbolKind::Routine)
                    && self.has_foreign_class_owner(binding, uri, document, span.start)
                {
                    if !strict_resolution && !is_binding_member {
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
                            let candidates =
                                self.resolve_candidates_at(uri, document, span.start, identifier);
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
                        let candidates =
                            self.resolve_candidates_at(uri, document, span.start, identifier);
                        let unknown_global_fallback = self.is_unknown_global_fallback(
                            document,
                            identifier,
                            span.start,
                            &candidates,
                        );
                        (candidates, unknown_global_fallback)
                    };
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
                    if !strict_resolution && !is_binding_member {
                        continue;
                    }
                    return Err(format!(
                        "rename cannot prove the binding of {:?} at {}:{} because its declaration is in an unknown conditional branch",
                        binding.old_key, uri, span.start
                    ));
                }
                if unknown_global_fallback {
                    if !strict_resolution && !is_binding_member {
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
                if matching > 0 && binding_has_class_owner && !is_direct_declaration {
                    if let Some(dot) = member_expression {
                        if self.member_reference_uses_inherited_class_owner(
                            binding, uri, document, span.start, dot,
                        ) {
                            if !strict_resolution && !is_binding_member {
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
                        if !strict_resolution && !is_binding_member {
                            continue;
                        }
                        return Err(format!(
                            "ambiguous rename reference {:?} at {}:{}",
                            binding.old_key, uri, span.start
                        ));
                    }
                    let occurrence = Occurrence {
                        uri: uri.clone(),
                        span,
                    };
                    if seen.insert((occurrence.uri.clone(), occurrence.span)) {
                        if result_limit.is_some_and(|limit| occurrences.len() >= limit) {
                            return Err(format!(
                                "binding reference result exceeds the {MAX_BINDING_LOCATIONS}-entry limit"
                            ));
                        }
                        occurrences.push(occurrence);
                    }
                } else if strict_resolution
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
    ) -> Result<Vec<lsp_types::Location>, String> {
        let mut seen = HashSet::new();
        let mut locations = Vec::with_capacity(occurrences.len());
        let mut group_start = 0;
        while group_start < occurrences.len() {
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
                    locations.push(lsp_types::Location {
                        uri: occurrence.uri.clone(),
                        range: Range { start, end },
                    });
                }
            }
            group_start = group_end;
        }
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
        let Some(lhs) = dot.child_by_field_name("lhs") else {
            return true;
        };
        let receivers = self.resolve_receivers(occurrence_uri, document, offset, lhs);
        for receiver in receivers {
            let super::Receiver::Type(type_uri, type_key) = receiver else {
                continue;
            };
            let direct = self.direct_member_candidates(
                &type_uri,
                &type_key,
                Some(&binding.old_key),
                type_uri == *occurrence_uri,
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
        for target_id in &binding.members {
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

            for symbol in &document.symbols {
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
            for (uri, document) in &self.documents {
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
        for occurrence in &plan.occurrences {
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
            let candidates = if let Some(dot) = member_expression_at(identifier) {
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
) -> Result<Binding, String> {
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

    if let BindingGroup::Routine { uri, key } = &group {
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
        let Some(target) = members.iter().find_map(|id| {
            document
                .symbols
                .iter()
                .find(|symbol| symbol_id(uri, symbol) == *id)
        }) else {
            return Err("routine binding declaration disappeared".to_string());
        };
        if document.symbols.iter().any(|symbol| {
            symbol.kind == SymbolKind::Routine
                && symbol.key == target.key
                && symbol.scope == target.scope
                && symbol.owner_type == target.owner_type
                && symbol.routine_key.as_deref() != Some(key.as_str())
        }) {
            return Err("overloaded routine bindings are not supported".to_string());
        }
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
