use crate::conditional::{self, ConditionalAnalysis};
use crate::text;
use lsp_types::{Location, Position, Range, Url};
use pascal_core::directive_fragment_rewrite::DirectivePatch;
use pascal_core::{FileInfo, parser};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tree_sitter::{Node, Tree};

mod assistance;
mod overload;
mod rename;
mod symbols;
pub(crate) use rename::RenameBindingInfo;
#[cfg(test)]
pub(crate) use rename::test_cancel_after_checks;
#[cfg(test)]
pub(crate) use rename::{TestCancellationPhase, test_cancel_in_phase};

#[cfg(test)]
thread_local! {
    static OWNER_TYPE_ROOT_LOOKUPS: Cell<usize> = const { Cell::new(0) };
    static TEST_UNIT_URL_VECTOR_MATERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
    static TEST_EXPORTED_INDEX_VECTOR_MATERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
    static TEST_TYPE_INDEX_VECTOR_MATERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
    static TEST_MEMBER_INDEX_VECTOR_MATERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
    static TEST_NODE_SIBLING_FRONTIER_ENTRIES: Cell<usize> = const { Cell::new(0) };
    static TEST_LEGACY_EXPORTED_MATERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn test_record_materialization(counter: &'static std::thread::LocalKey<Cell<usize>>) {
    counter.with(|value| value.set(value.get().saturating_add(1)));
}

#[cfg(test)]
fn test_materialization_count(counter: &'static std::thread::LocalKey<Cell<usize>>) -> usize {
    counter.with(Cell::get)
}

#[cfg(test)]
fn test_reset_materialization_counters() {
    TEST_UNIT_URL_VECTOR_MATERIALIZATIONS.with(|value| value.set(0));
    TEST_EXPORTED_INDEX_VECTOR_MATERIALIZATIONS.with(|value| value.set(0));
    TEST_TYPE_INDEX_VECTOR_MATERIALIZATIONS.with(|value| value.set(0));
    TEST_MEMBER_INDEX_VECTOR_MATERIALIZATIONS.with(|value| value.set(0));
    TEST_LEGACY_EXPORTED_MATERIALIZATIONS.with(|value| value.set(0));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum IntegerKind {
    Literal,
    ShortInt,
    SmallInt,
    Integer,
    Byte,
    Word,
    Cardinal,
    LongWord,
    Int64,
    UInt64,
    NativeInt,
    NativeUInt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum BuiltinType {
    Integer(IntegerKind),
    Real,
    String,
    Character,
    Boolean,
}

/// The navigation operation requested by an LSP client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NavigationTarget {
    /// The declaration visible from the current lexical/import scope.
    Declaration,
    /// The implementation body when one is available.
    Definition,
    /// The implementation body, explicitly requested by the client.
    Implementation,
}

/// An in-memory, incrementally replaceable index of Pascal source documents.
///
/// The index deliberately has no filesystem policy: callers decide which
/// documents belong to a workspace and feed disk or unsaved-buffer contents to
/// [`NavigationIndex::update`].
#[derive(Default)]
pub struct NavigationIndex {
    documents: HashMap<Url, Document>,
    units: HashMap<String, Vec<Url>>,
}

/// A byte span in a parsed Pascal source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    pub start: usize,
    pub end: usize,
}

/// Metadata for one unit imported by a document's `uses` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportMetadata {
    pub name: String,
    pub span: SourceSpan,
}

impl NavigationIndex {
    /// Construct an empty navigation index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the source-accurate outline for one indexed document.
    pub fn document_symbols(&self, uri: &Url) -> Result<Vec<lsp_types::DocumentSymbol>, String> {
        symbols::document_symbols(self, uri)
    }

    pub(crate) fn document_symbols_with_cancel(
        &self,
        uri: &Url,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<lsp_types::DocumentSymbol>, String> {
        symbols::document_symbols_with_cancel(self, uri, cancel)
    }

    /// Return fully-resolved declarations from the indexed documents whose
    /// names contain `query`, case-insensitively.
    pub fn workspace_symbols(
        &self,
        query: &str,
    ) -> Result<Vec<lsp_types::SymbolInformation>, String> {
        symbols::workspace_symbols(self, query)
    }

    pub(crate) fn workspace_symbols_with_cancel(
        &self,
        query: &str,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<lsp_types::SymbolInformation>, String> {
        symbols::workspace_symbols_with_cancel(self, query, cancel)
    }

    pub(crate) fn flatten_document_symbols(
        uri: &Url,
        symbols: Vec<lsp_types::DocumentSymbol>,
    ) -> Vec<lsp_types::SymbolInformation> {
        symbols::flatten_document_symbols(uri, symbols)
    }

    /// Parse and replace one document.
    ///
    /// Parsing happens before the old document is replaced, so a parser error
    /// leaves the last known-good overlay available to the caller.
    pub fn update(&mut self, uri: Url, source: String) -> Result<(), String> {
        self.update_with_defines(uri, source, &[])
    }

    /// Parse and replace one document using the selected project's positive
    /// conditional-compilation facts.
    pub(crate) fn update_with_defines(
        &mut self,
        uri: Url,
        source: String,
        defines: &[String],
    ) -> Result<(), String> {
        let cancel = AtomicBool::new(false);
        self.update_with_defines_with_cancel(uri, source, defines, &cancel)
    }

    pub(crate) fn update_with_defines_with_cancel(
        &mut self,
        uri: Url,
        source: String,
        defines: &[String],
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let document = Document::parse_with_cancel(uri.clone(), source, defines, cancel)?;
        let old_unit = self
            .documents
            .get(&uri)
            .map(|document| document.unit_name.clone());
        let new_unit = document.unit_name.clone();
        self.documents.insert(uri.clone(), document);
        if let Some(old_unit) = old_unit {
            self.remove_uri_from_unit(&old_unit, &uri);
        }
        let urls = self.units.entry(new_unit).or_default();
        urls.push(uri);
        urls.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        urls.dedup();
        Ok(())
    }

    /// Remove a document and all symbols contributed by it.
    pub fn remove(&mut self, uri: &Url) {
        if let Some(document) = self.documents.remove(uri) {
            self.remove_uri_from_unit(&document.unit_name, uri);
        }
    }

    /// Return the parsed `uses` entries for a document.
    pub fn imports(&self, uri: &Url) -> Vec<ImportMetadata> {
        self.documents
            .get(uri)
            .map(|document| document.imports.clone())
            .unwrap_or_default()
    }

    pub(crate) fn source_text(&self, uri: &Url) -> Option<&str> {
        self.documents
            .get(uri)
            .map(|document| document.source.as_str())
    }

    /// Bind a document's imports to the workspace-selected unit documents.
    ///
    /// Supplying an empty iterator is intentional: once a workspace owns an
    /// index, an unresolved import must not fall back to a same-named unit
    /// contributed by another project. A plain `NavigationIndex` remains
    /// global and implicit when this method has not been called for a document.
    pub fn bind_imports<I>(&mut self, uri: &Url, bindings: I)
    where
        I: IntoIterator<Item = (String, Url)>,
    {
        let bindings = bindings
            .into_iter()
            .map(|(name, uri)| (canonical_name(&name), uri))
            .collect();
        if let Some(document) = self.documents.get_mut(uri) {
            document.import_bindings = Some(bindings);
        }
    }

    /// Clear a document's resolved imports while retaining an explicit
    /// workspace-owned empty binding map.
    pub fn clear_import_bindings(&mut self, uri: &Url) {
        if let Some(document) = self.documents.get_mut(uri) {
            document.import_bindings = Some(HashMap::new());
        }
    }

    /// Return the unit name declared by a parsed document.
    pub fn unit_name(&self, uri: &Url) -> Option<String> {
        self.documents
            .get(uri)
            .map(|document| document.unit_name.clone())
    }

    /// Return whether this URI has a parsed document in the index.
    pub fn contains(&self, uri: &Url) -> bool {
        self.documents.contains_key(uri)
    }

    pub(crate) fn conditional_analysis(&self, uri: &Url) -> Option<&ConditionalAnalysis> {
        self.documents
            .get(uri)
            .map(|document| &document.conditionals)
    }

    pub(crate) fn conditional_unknown_at(&self, uri: &Url, offset: usize) -> bool {
        self.conditional_analysis(uri)
            .is_some_and(|analysis| analysis.is_unknown_at(offset))
    }

    pub(crate) fn conditional_unknown_contains_identifier(&self, uri: &Url, name: &str) -> bool {
        let Some(document) = self.documents.get(uri) else {
            return false;
        };
        document
            .conditionals
            .unknown_contains_identifier(&document.source, name)
    }

    fn candidate_is_conditionally_unknown(&self, candidate: &Candidate) -> bool {
        let Some(document) = self.documents.get(&candidate.uri) else {
            return true;
        };
        document
            .conditional_unknown_symbols
            .get(candidate.index)
            .copied()
            .unwrap_or(true)
    }

    pub(crate) fn position_is_ignored_or_empty(
        &self,
        uri: &Url,
        position: Position,
    ) -> Result<bool, String> {
        let document = self
            .documents
            .get(uri)
            .ok_or_else(|| format!("document is not indexed: {uri}"))?;
        let Some(offset) = text::position_to_offset(&document.source, position) else {
            return Ok(true);
        };
        Ok(is_ignored_offset(document.tree.root_node(), offset)
            || identifier_at(document.tree.root_node(), offset).is_none())
    }

    /// Number of retained parsed documents.
    pub fn document_count(&self) -> usize {
        self.documents.len()
    }

    /// Resolve the identifier at `position` in `uri`.
    ///
    /// Unknown names and unknown receivers return an empty vector. In
    /// particular, this method never performs an unrelated workspace-wide
    /// name search merely because an expression could not be typed.
    pub fn navigate(
        &self,
        uri: &Url,
        position: Position,
        target: NavigationTarget,
    ) -> Vec<Location> {
        let Some(document) = self.documents.get(uri) else {
            return Vec::new();
        };
        let Some(offset) = text::position_to_offset(&document.source, position) else {
            return Vec::new();
        };
        if document.conditionals.is_unknown_at(offset) {
            return Vec::new();
        }
        if is_ignored_offset(document.tree.root_node(), offset) {
            return Vec::new();
        }
        let Some(identifier) = identifier_at(document.tree.root_node(), offset) else {
            return Vec::new();
        };

        let mut state = ResolutionState::new();
        let mut references =
            self.resolve_candidates_at_with_state(uri, document, offset, identifier, &mut state);
        if references
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Vec::new();
        }

        if let Some(call) = overload::call_for_identifier(identifier) {
            let cancel = AtomicBool::new(false);
            let mut budget = AssistanceBudget::new(
                MAX_NAVIGATION_OVERLOAD_WORK,
                MAX_NAVIGATION_OVERLOAD_BYTES,
                "navigation overload selection",
            );
            let owner_receivers = call
                .child_by_field_name("entity")
                .and_then(callable_owner_node)
                .map(|owner| {
                    self.resolve_receivers_with_state_and_budget(
                        uri,
                        document,
                        offset,
                        owner,
                        owner,
                        &mut state,
                        &cancel,
                        &mut budget,
                        0,
                    )
                })
                .transpose()
                .unwrap_or_default()
                .unwrap_or_default();
            let owner_instances = owner_receivers
                .iter()
                .filter_map(|receiver| match receiver {
                    Receiver::Type(instance) => Some(instance.clone()),
                    Receiver::Unit(_) | Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => None,
                })
                .collect::<Vec<_>>();
            let selection = overload::select(
                self,
                uri,
                document,
                call,
                &references,
                &GenericSubstitution::empty(),
                &owner_instances,
                &mut state,
                0,
                &cancel,
                &mut budget,
            )
            .unwrap_or_default();
            if let Some(group) = selection.selected_group {
                references
                    .retain(|candidate| overload::candidate_in_group(self, candidate, &group));
            } else if selection.no_viable_group {
                references
                    .retain(|candidate| overload::key_for_candidate(self, candidate).is_none());
            }
        }

        self.locations_for(self.expand_property_candidates(references, target), target)
    }

    fn resolve_candidates_at(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        identifier: Node<'_>,
    ) -> Vec<Candidate> {
        let mut state = ResolutionState::new();
        self.resolve_candidates_at_with_state(uri, document, offset, identifier, &mut state)
    }

    fn resolve_candidates_at_with_state(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        identifier: Node<'_>,
        state: &mut ResolutionState,
    ) -> Vec<Candidate> {
        let name = node_text(identifier, &document.source);
        if let Some(unit_name) = use_name_at(identifier, &document.source) {
            self.unit_references(document, &unit_name)
        } else if let Some(direct) = self.direct_symbol_references(uri, identifier) {
            direct
        } else if let Some(generic) = self.generic_parameter_references(uri, document, identifier) {
            generic
        } else if let Some((path, cursor_index)) =
            qualified_type_path_at(identifier, &document.source)
        {
            self.type_reference_candidates(uri, document, offset, &path, cursor_index)
        } else if let Some(dot) = member_expression_at(identifier) {
            if is_right_hand_member(dot, identifier) {
                self.member_references_with_state(uri, document, offset, dot, &name, state)
            } else {
                self.unqualified_references(uri, document, offset, &name)
            }
        } else {
            self.unqualified_references(uri, document, offset, &name)
        }
    }

    fn resolve_candidates_at_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        identifier: Node<'_>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let mut state = ResolutionState::new();
        self.resolve_candidates_at_with_state_and_budget(
            uri, document, offset, identifier, &mut state, 0, cancel, budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_candidates_at_with_state_and_budget(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        identifier: Node<'_>,
        state: &mut ResolutionState,
        depth: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let name = node_text_with_budget(identifier, &document.source, cancel, budget)?;
        if let Some(unit_name) =
            use_name_at_with_budget(identifier, &document.source, cancel, budget)?
        {
            self.unit_references_with_budget(document, &unit_name, cancel, budget)
        } else if let Some(direct) =
            self.direct_symbol_references_with_budget(uri, identifier, cancel, budget)?
        {
            Ok(direct)
        } else if let Some(generic) = self.generic_parameter_references(uri, document, identifier) {
            budget.require_work(generic.len(), cancel)?;
            budget.require_bytes(uri.as_str().len().saturating_mul(generic.len()), cancel)?;
            Ok(generic)
        } else if let Some((path, cursor_index)) =
            qualified_type_path_at_with_budget(identifier, &document.source, cancel, budget)?
        {
            self.type_reference_candidates_with_budget(
                uri,
                document,
                offset,
                identifier,
                &path,
                cursor_index,
                cancel,
                budget,
            )
        } else if let Some(dot) = member_expression_at(identifier) {
            if is_right_hand_member(dot, identifier) {
                self.member_references_with_state_and_budget(
                    uri, document, offset, dot, name, identifier, state, depth, cancel, budget,
                )
            } else {
                self.unqualified_references_with_budget(
                    uri, document, offset, name, identifier, cancel, budget,
                )
            }
        } else {
            self.unqualified_references_with_budget(
                uri, document, offset, name, identifier, cancel, budget,
            )
        }
    }

    fn direct_symbol_references_with_budget(
        &self,
        uri: &Url,
        identifier: Node<'_>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<Vec<Candidate>>, String> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(None);
        };
        let span = Span::from_node(identifier);
        let Some(direct) = document.direct_symbol_indices.get(&span) else {
            return Ok(None);
        };
        let mut result_capacity = 0usize;
        for index in direct {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(*index) else {
                continue;
            };
            if symbol.kind == SymbolKind::Routine {
                if let Some(routine_key) = &symbol.routine_key {
                    result_capacity = result_capacity.saturating_add(
                        document
                            .routine_symbol_indices
                            .get(routine_key)
                            .map_or(0, Vec::len),
                    );
                } else {
                    result_capacity = result_capacity.saturating_add(1);
                }
            } else {
                result_capacity = result_capacity.saturating_add(1);
            }
        }
        budget.require_work(result_capacity, cancel)?;
        budget.require_bytes(uri.as_str().len().saturating_mul(result_capacity), cancel)?;
        let mut references = Vec::with_capacity(result_capacity);
        for index in direct {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(*index) else {
                continue;
            };
            if symbol.kind == SymbolKind::Routine {
                if let Some(routine_key) = &symbol.routine_key {
                    if let Some(candidate_indices) =
                        document.routine_symbol_indices.get(routine_key)
                    {
                        for candidate_index in candidate_indices {
                            check_navigation_cancel(cancel)?;
                            references.push(Candidate {
                                uri: uri.clone(),
                                index: *candidate_index,
                            });
                        }
                    }
                }
            } else {
                references.push(Candidate {
                    uri: uri.clone(),
                    index: *index,
                });
            }
        }
        Ok(Some(references))
    }

    fn unit_references_with_budget(
        &self,
        current_document: &Document,
        name: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        budget.require_bytes(name.len(), cancel)?;
        let key = canonical_name(name);
        let urls = self.unit_urls_for_import_with_budget(current_document, &key, cancel, budget)?;
        let mut result_capacity = 0usize;
        let mut result_uri_bytes = 0usize;
        for uri in &urls {
            let Some(document) = self.documents.get(uri) else {
                continue;
            };
            budget.require_bytes(document.unit_name.len(), cancel)?;
            let Some(indices) = document
                .symbol_indices_by_scope_key
                .get(&(ROOT_SCOPE, document.unit_name.clone()))
            else {
                continue;
            };
            result_capacity = result_capacity.saturating_add(indices.len());
            result_uri_bytes =
                result_uri_bytes.saturating_add(uri.as_str().len().saturating_mul(indices.len()));
        }
        budget.require_work(result_capacity, cancel)?;
        budget.require_bytes(result_uri_bytes, cancel)?;
        let mut result = Vec::with_capacity(result_capacity);
        for uri in urls {
            let Some(document) = self.documents.get(&uri) else {
                continue;
            };
            budget.require_bytes(document.unit_name.len(), cancel)?;
            let Some(indices) = document
                .symbol_indices_by_scope_key
                .get(&(ROOT_SCOPE, document.unit_name.clone()))
            else {
                continue;
            };
            for index in indices {
                check_navigation_cancel(cancel)?;
                if document
                    .symbols
                    .get(*index)
                    .is_some_and(|symbol| symbol.kind == SymbolKind::Unit)
                {
                    result.push(Candidate {
                        uri: uri.clone(),
                        index: *index,
                    });
                    break;
                }
            }
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn unqualified_references_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        name: &str,
        identifier: Node<'_>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        if let Some(generic) = self.generic_parameter_references(uri, document, identifier) {
            budget.require_work(generic.len(), cancel)?;
            budget.require_bytes(uri.as_str().len().saturating_mul(generic.len()), cancel)?;
            return Ok(generic);
        }
        let scope = document.scope_at(offset);
        self.unqualified_references_with_budget_at_scope(
            uri, document, offset, name, identifier, scope, cancel, budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn unqualified_references_with_budget_at_scope(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        name: &str,
        identifier: Node<'_>,
        scope: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        budget.require_bytes(name.len(), cancel)?;
        let key = canonical_name(name);
        if let Some(generic) = self.generic_parameter_references(uri, document, identifier) {
            budget.require_work(generic.len(), cancel)?;
            budget.require_bytes(uri.as_str().len().saturating_mul(generic.len()), cancel)?;
            return Ok(generic
                .into_iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.key == key)
                })
                .collect());
        }
        budget.require_work(document.scopes.len().saturating_add(1), cancel)?;
        let owner_type = document.owner_type_at_identifier(identifier, scope);

        let scope_chain = document.scope_chain_from(scope);
        budget.require_bytes(
            key.len()
                .saturating_mul(scope_chain.len().saturating_add(1)),
            cancel,
        )?;
        for scope_id in scope_chain {
            let Some(indices) = document
                .symbol_indices_by_scope_key
                .get(&(scope_id, key.clone()))
            else {
                continue;
            };
            budget.require_work(indices.len(), cancel)?;
            budget.require_bytes(uri.as_str().len().saturating_mul(indices.len()), cancel)?;
            let mut local = Vec::with_capacity(indices.len());
            for index in indices {
                check_navigation_cancel(cancel)?;
                let Some(symbol) = document.symbols.get(*index) else {
                    continue;
                };
                if scope_id != ROOT_SCOPE
                    && !symbol.local_only
                    && symbol.owner_type.is_none()
                    && symbol.generic_parameter.is_none()
                    && symbol.kind != SymbolKind::Unit
                    && !symbol.unresolved_abbreviated
                {
                    local.push(Candidate {
                        uri: uri.clone(),
                        index: *index,
                    });
                }
            }
            if !local.is_empty() {
                return Ok(local);
            }
        }

        if let Some(owner_type) = owner_type.as_deref() {
            let members = self.member_references_for_type_with_budget(
                uri, owner_type, ROOT_SCOPE, &key, true, cancel, budget,
            )?;
            if !members.is_empty() {
                return Ok(members);
            }
        }

        let region = document.region_at(offset);
        let Some(root_indices) = document
            .symbol_indices_by_scope_key
            .get(&(ROOT_SCOPE, key.clone()))
        else {
            return self.imported_references_with_budget(
                document,
                region,
                &key,
                identifier,
                owner_type.as_deref(),
                cancel,
                budget,
            );
        };
        budget.require_work(root_indices.len(), cancel)?;
        budget.require_bytes(
            uri.as_str().len().saturating_mul(root_indices.len()),
            cancel,
        )?;
        let mut current = Vec::with_capacity(root_indices.len());
        for index in root_indices {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(*index) else {
                continue;
            };
            if !symbol.local_only
                && symbol.owner_type.is_none()
                && symbol.generic_parameter.is_none()
                && symbol.kind != SymbolKind::Unit
                && !symbol.unresolved_abbreviated
                && symbol_visible_in_region(symbol, region)
            {
                current.push(Candidate {
                    uri: uri.clone(),
                    index: *index,
                });
            }
        }
        if !current.is_empty() {
            if self.is_unknown_global_fallback_for_owner(
                document,
                identifier,
                owner_type.as_deref(),
                &current,
            ) {
                return Ok(Vec::new());
            }
            return Ok(current);
        }

        self.imported_references_with_budget(
            document,
            region,
            &key,
            identifier,
            owner_type.as_deref(),
            cancel,
            budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn imported_references_with_budget(
        &self,
        document: &Document,
        region: Region,
        key: &str,
        identifier: Node<'_>,
        owner_type: Option<&str>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let active_uses = document.active_uses_with_budget(region, cancel, budget)?;
        let mut imported = Vec::new();
        for unit in active_uses {
            check_navigation_cancel(cancel)?;
            if document.unknown_imports.contains(unit.as_str()) {
                return Ok(Vec::new());
            }
            for unit_uri in self.unit_urls_for_import_with_budget(document, unit, cancel, budget)? {
                imported.extend(
                    self.exported_references_for_key_with_budget(&unit_uri, key, cancel, budget)?,
                );
            }
        }
        if !imported.is_empty() {
            if self
                .is_unknown_global_fallback_for_owner(document, identifier, owner_type, &imported)
            {
                return Ok(Vec::new());
            }
            return Ok(imported);
        }

        Ok(Vec::new())
    }

    fn exported_references_for_key_with_budget(
        &self,
        uri: &Url,
        key: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(Vec::new());
        };
        budget.require_bytes(key.len(), cancel)?;
        let key = key.to_owned();
        let Some(indices) = document.symbol_indices_by_scope_key.get(&(ROOT_SCOPE, key)) else {
            return Ok(Vec::new());
        };
        budget.require_work(indices.len(), cancel)?;
        budget.require_bytes(uri.as_str().len().saturating_mul(indices.len()), cancel)?;
        #[cfg(test)]
        test_record_materialization(&TEST_EXPORTED_INDEX_VECTOR_MATERIALIZATIONS);
        let mut result = Vec::with_capacity(indices.len());
        for index in indices {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(*index) else {
                continue;
            };
            if symbol.owner_type.is_none()
                && symbol.generic_parameter.is_none()
                && symbol.kind != SymbolKind::Unit
                && !symbol.local_only
                && (symbol.region == Region::Interface
                    || (symbol.kind == SymbolKind::Routine
                        && symbol.origin == Origin::Definition
                        && symbol.routine_key.as_ref().is_some_and(|routine_key| {
                            document.interface_routine_keys.contains(routine_key)
                        })))
            {
                result.push(Candidate {
                    uri: uri.clone(),
                    index: *index,
                });
            }
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn member_references_with_state_and_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        dot: Node<'_>,
        member_name: &str,
        lookup_identifier: Node<'_>,
        state: &mut ResolutionState,
        depth: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let Some(lhs) = dot.child_by_field_name("lhs") else {
            return Ok(Vec::new());
        };
        budget.require_bytes(member_name.len(), cancel)?;
        let key = canonical_name(member_name);
        let receivers = self.resolve_receivers_with_state_and_budget(
            current_uri,
            current_document,
            offset,
            lhs,
            lookup_identifier,
            state,
            cancel,
            budget,
            depth.saturating_add(1),
        )?;
        let mut references = Vec::new();
        for receiver in receivers {
            budget.require_work(1, cancel)?;
            match receiver {
                Receiver::Unit(unit_uri) => {
                    references.extend(self.exported_references_for_key_with_budget(
                        &unit_uri, &key, cancel, budget,
                    )?);
                }
                Receiver::Type(instance) => {
                    references.extend(self.member_references_for_type_with_budget(
                        &instance.uri,
                        &instance.key,
                        instance.scope,
                        &key,
                        instance.uri == *current_uri,
                        cancel,
                        budget,
                    )?)
                }
                Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => {}
            }
        }
        Ok(references)
    }

    fn type_reference_candidates(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        cursor_index: usize,
    ) -> Vec<Candidate> {
        let mut state = ResolutionState::new();
        if let Some((prefix_len, unit_uris)) =
            self.longest_visible_unit_prefix(current_uri, current_document, offset, parts)
        {
            if cursor_index < prefix_len {
                return self.unit_candidates(unit_uris);
            }
            if cursor_index == prefix_len && parts.len() == prefix_len + 1 {
                let Some(type_name) = parts.get(prefix_len) else {
                    return Vec::new();
                };
                let allow_implementation = unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
                return unit_uris
                    .iter()
                    .flat_map(|unit_uri| {
                        self.type_candidates_in_unit(unit_uri, type_name, allow_implementation)
                    })
                    .collect();
            }
        }
        self.type_receivers_for_parts(
            current_uri,
            current_document,
            offset,
            parts,
            None,
            &mut state,
        )
        .into_iter()
        .flat_map(|receiver| match receiver {
            Receiver::Type(instance) => self.type_candidates_in_unit(
                &instance.uri,
                &instance.key,
                instance.uri == *current_uri,
            ),
            Receiver::Unit(_) => Vec::new(),
            Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Vec::new(),
        })
        .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn type_reference_candidates_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        lookup_identifier: Node<'_>,
        parts: &[String],
        cursor_index: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        budget.require_work(parts.len(), cancel)?;
        if parts.len() == 1 {
            let candidates = self.unqualified_references_with_budget(
                current_uri,
                current_document,
                offset,
                &parts[0],
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
            return Ok(candidates
                .into_iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.kind == SymbolKind::Type)
                })
                .collect());
        }
        let mut state = ResolutionState::new();
        if let Some((prefix_len, unit_uris)) = self.longest_visible_unit_prefix_with_budget(
            current_uri,
            current_document,
            offset,
            parts,
            cancel,
            budget,
        )? {
            if cursor_index < prefix_len {
                return self.unit_candidates_with_budget(unit_uris, cancel, budget);
            }
            if cursor_index == prefix_len && parts.len() == prefix_len + 1 {
                let Some(type_name) = parts.get(prefix_len) else {
                    return Ok(Vec::new());
                };
                let allow_implementation = unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
                let mut result = Vec::new();
                for unit_uri in unit_uris {
                    result.extend(self.type_candidates_in_unit_with_budget(
                        &unit_uri,
                        type_name,
                        allow_implementation,
                        cancel,
                        budget,
                    )?);
                }
                return Ok(result);
            }
        }
        let receivers = self.resolve_qualified_receiver_path_with_budget(
            current_uri,
            current_document,
            offset,
            parts,
            lookup_identifier,
            &mut state,
            cancel,
            budget,
        )?;
        let mut result = Vec::new();
        for receiver in receivers {
            budget.require_work(1, cancel)?;
            if let Receiver::Type(instance) = receiver {
                result.extend(self.type_candidates_in_unit_with_budget(
                    &instance.uri,
                    &instance.key,
                    instance.uri == *current_uri,
                    cancel,
                    budget,
                )?);
            }
        }
        Ok(result)
    }

    fn unit_candidates_with_budget(
        &self,
        unit_uris: Vec<Url>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let mut result = Vec::new();
        for uri in unit_uris {
            budget.require_work(1, cancel)?;
            let Some(document) = self.documents.get(&uri) else {
                continue;
            };
            let Some(indices) = document
                .symbol_indices_by_scope_key
                .get(&(ROOT_SCOPE, document.unit_name.clone()))
            else {
                continue;
            };
            for index in indices {
                budget.require_work(1, cancel)?;
                if document
                    .symbols
                    .get(*index)
                    .is_some_and(|symbol| symbol.kind == SymbolKind::Unit)
                {
                    result.push(Candidate {
                        uri: uri.clone(),
                        index: *index,
                    });
                    break;
                }
            }
        }
        Ok(result)
    }

    fn unit_candidates(&self, unit_uris: Vec<Url>) -> Vec<Candidate> {
        unit_uris
            .into_iter()
            .filter_map(|uri| {
                let document = self.documents.get(&uri)?;
                let index = document
                    .symbols
                    .iter()
                    .position(|symbol| symbol.kind == SymbolKind::Unit)?;
                Some(Candidate { uri, index })
            })
            .collect()
    }

    fn remove_uri_from_unit(&mut self, unit_name: &str, uri: &Url) {
        let mut remove_unit = false;
        if let Some(urls) = self.units.get_mut(unit_name) {
            urls.retain(|candidate| candidate != uri);
            remove_unit = urls.is_empty();
        }
        if remove_unit {
            self.units.remove(unit_name);
        }
    }

    fn direct_symbol_references(&self, uri: &Url, identifier: Node<'_>) -> Option<Vec<Candidate>> {
        let document = self.documents.get(uri)?;
        let span = Span::from_node(identifier);
        let direct = document.direct_symbol_indices.get(&span)?.clone();
        if direct.is_empty() {
            return None;
        }

        let mut references = Vec::new();
        for index in direct {
            let symbol = &document.symbols[index];
            if symbol.kind == SymbolKind::Routine {
                if let Some(routine_key) = &symbol.routine_key {
                    for candidate_index in document
                        .routine_symbol_indices
                        .get(routine_key)
                        .into_iter()
                        .flatten()
                        .copied()
                    {
                        references.push(Candidate {
                            uri: uri.clone(),
                            index: candidate_index,
                        });
                    }
                }
            } else {
                references.push(Candidate {
                    uri: uri.clone(),
                    index,
                });
            }
        }
        Some(references)
    }

    fn unit_references(&self, current_document: &Document, name: &str) -> Vec<Candidate> {
        let key = canonical_name(name);
        self.unit_urls_for_import(current_document, &key)
            .into_iter()
            .filter_map(|uri| {
                let document = self.documents.get(&uri)?;
                let index = document
                    .symbol_indices_by_scope_key
                    .get(&(ROOT_SCOPE, document.unit_name.clone()))?
                    .iter()
                    .copied()
                    .find(|index| {
                        document
                            .symbols
                            .get(*index)
                            .is_some_and(|symbol| symbol.kind == SymbolKind::Unit)
                    })?;
                Some(Candidate {
                    uri: uri.clone(),
                    index,
                })
            })
            .collect()
    }

    fn unit_urls_for_import(&self, current_document: &Document, name: &str) -> Vec<Url> {
        if current_document.unknown_imports.contains(name) {
            return Vec::new();
        }
        if let Some(bindings) = &current_document.import_bindings {
            return bindings.get(name).cloned().into_iter().collect();
        }
        self.units.get(name).cloned().unwrap_or_default()
    }

    fn unit_urls_for_import_with_budget(
        &self,
        current_document: &Document,
        name: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Url>, String> {
        if current_document.unknown_imports.contains(name) {
            return Ok(Vec::new());
        }
        if let Some(bindings) = &current_document.import_bindings {
            let Some(uri) = bindings.get(name) else {
                return Ok(Vec::new());
            };
            budget.require_work(1, cancel)?;
            budget.require_bytes(uri.as_str().len(), cancel)?;
            #[cfg(test)]
            test_record_materialization(&TEST_UNIT_URL_VECTOR_MATERIALIZATIONS);
            return Ok(vec![uri.clone()]);
        }
        let Some(urls) = self.units.get(name) else {
            return Ok(Vec::new());
        };
        budget.require_work(urls.len(), cancel)?;
        if urls.is_empty() {
            return Ok(Vec::new());
        }
        budget.require_bytes(
            urls.iter().map(|url| url.as_str().len()).sum::<usize>(),
            cancel,
        )?;
        #[cfg(test)]
        test_record_materialization(&TEST_UNIT_URL_VECTOR_MATERIALIZATIONS);
        Ok(urls.clone())
    }

    fn generic_parameter_references(
        &self,
        uri: &Url,
        document: &Document,
        identifier: Node<'_>,
    ) -> Option<Vec<Candidate>> {
        let span = Span::from_node(identifier);
        let key = canonical_name(&node_text(identifier, &document.source));
        let routine_keys = document
            .symbols
            .iter()
            .filter(|symbol| {
                symbol.kind == SymbolKind::Routine
                    && !symbol.generic_parameters.is_empty()
                    && symbol.declaration_span.contains(span)
                    && symbol
                        .generic_parameters
                        .iter()
                        .any(|parameter| parameter.name == key)
            })
            .filter_map(|symbol| symbol.routine_key.as_ref())
            .collect::<HashSet<_>>();
        if routine_keys.is_empty() {
            return None;
        }
        let candidates = document
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| {
                symbol.kind == SymbolKind::Type
                    && symbol.generic_parameter.as_deref() == Some(key.as_str())
                    && symbol
                        .routine_key
                        .as_ref()
                        .is_some_and(|routine_key| routine_keys.contains(routine_key))
            })
            .map(|(index, _)| Candidate {
                uri: uri.clone(),
                index,
            })
            .collect::<Vec<_>>();
        (!candidates.is_empty()).then_some(candidates)
    }

    fn unqualified_references(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        name: &str,
    ) -> Vec<Candidate> {
        let Some(identifier) = identifier_at(document.tree.root_node(), offset) else {
            return Vec::new();
        };
        if let Some(generic) = self.generic_parameter_references(uri, document, identifier) {
            return generic;
        }
        self.unqualified_references_at_scope(
            uri,
            document,
            offset,
            name,
            identifier,
            document.scope_at(offset),
        )
    }

    fn unqualified_references_at_scope(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        name: &str,
        identifier: Node<'_>,
        scope: usize,
    ) -> Vec<Candidate> {
        let key = canonical_name(name);
        if let Some(generic) = self.generic_parameter_references(uri, document, identifier) {
            return generic
                .into_iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.key == key)
                })
                .collect();
        }

        // Search lexical scopes from the innermost outward. A local symbol
        // shadows both the unit's declarations and imported declarations.
        for scope_id in document.scope_chain_from(scope) {
            let local = document
                .symbol_indices_by_scope_key
                .get(&(scope_id, key.clone()))
                .into_iter()
                .flatten()
                .copied()
                .filter(|index| {
                    document.symbols.get(*index).is_some_and(|symbol| {
                        scope_id != ROOT_SCOPE
                            && !symbol.local_only
                            && symbol.owner_type.is_none()
                            && symbol.generic_parameter.is_none()
                            && symbol.kind != SymbolKind::Unit
                            && !symbol.unresolved_abbreviated
                    })
                })
                .map(|index| Candidate {
                    uri: uri.clone(),
                    index,
                })
                .collect::<Vec<_>>();
            if !local.is_empty() {
                return local;
            }
        }

        // Class members are the next lexical scope in Delphi. They must be
        // considered before unit globals and imported declarations, and the
        // owner lookup also covers nested procedures inside a class method.
        if let Some(owner_type) = document.owner_type_at(offset) {
            let members = self.member_references_for_type(uri, &owner_type, ROOT_SCOPE, &key, true);
            if !members.is_empty() {
                return members;
            }
        }

        let region = document.region_at(offset);
        let current = document
            .symbol_indices_by_scope_key
            .get(&(ROOT_SCOPE, key.clone()))
            .into_iter()
            .flatten()
            .copied()
            .filter(|index| {
                document.symbols.get(*index).is_some_and(|symbol| {
                    !symbol.local_only
                        && symbol.owner_type.is_none()
                        && symbol.generic_parameter.is_none()
                        && symbol.kind != SymbolKind::Unit
                        && !symbol.unresolved_abbreviated
                        && symbol_visible_in_region(symbol, region)
                })
            })
            .map(|index| Candidate {
                uri: uri.clone(),
                index,
            })
            .collect::<Vec<_>>();
        if !current.is_empty() {
            if self.is_unknown_global_fallback(document, identifier, offset, &current) {
                return Vec::new();
            }
            return current;
        }

        // Only the uses clauses active at this source position contribute
        // imported names. Imported implementation-only routines are excluded
        // by exported_references_for_document.
        let active_uses = document.active_uses(region);
        if active_uses
            .iter()
            .any(|unit| document.unknown_imports.contains(unit.as_str()))
        {
            return Vec::new();
        }
        let mut imported = Vec::new();
        for unit in active_uses {
            for unit_uri in self.unit_urls_for_import(document, unit) {
                imported.extend(
                    self.exported_references_for_document(&unit_uri)
                        .into_iter()
                        .filter(|candidate| {
                            self.symbol(candidate)
                                .is_some_and(|symbol| symbol.key == key)
                        }),
                );
            }
        }
        if !imported.is_empty() {
            if self.is_unknown_global_fallback(document, identifier, offset, &imported) {
                return Vec::new();
            }
            return imported;
        }

        // No class scope is guessed for an ordinary free procedure.
        Vec::new()
    }

    fn member_references(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        dot: Node<'_>,
        member_name: &str,
    ) -> Vec<Candidate> {
        let mut state = ResolutionState::new();
        self.member_references_with_state(
            current_uri,
            current_document,
            offset,
            dot,
            member_name,
            &mut state,
        )
    }

    fn member_references_with_state(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        dot: Node<'_>,
        member_name: &str,
        state: &mut ResolutionState,
    ) -> Vec<Candidate> {
        let Some(lhs) = dot.child_by_field_name("lhs") else {
            return Vec::new();
        };
        let key = canonical_name(member_name);
        let mut references = Vec::new();
        let receivers =
            self.resolve_receivers_with_state(current_uri, current_document, offset, lhs, state);
        for receiver in receivers {
            match receiver {
                Receiver::Unit(unit_uri) => references.extend(
                    self.exported_references_for_document(&unit_uri)
                        .into_iter()
                        .filter(|candidate| {
                            self.symbol(candidate).is_some_and(|symbol| {
                                symbol.owner_type.is_none() && symbol.key == key
                            })
                        }),
                ),
                Receiver::Type(instance) => {
                    let members = self.member_references_for_type(
                        &instance.uri,
                        &instance.key,
                        instance.scope,
                        &key,
                        instance.uri == *current_uri,
                    );
                    references.extend(members)
                }
                Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => {}
            }
        }
        references
    }

    fn resolve_receivers(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        node: Node<'_>,
    ) -> Vec<Receiver> {
        let mut state = ResolutionState::new();
        self.resolve_receivers_with_state(current_uri, current_document, offset, node, &mut state)
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_receivers_with_state_and_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        node: Node<'_>,
        lookup_identifier: Node<'_>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
        depth: usize,
    ) -> Result<Vec<Receiver>, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        if depth >= MAX_RECEIVER_RECURSION_DEPTH {
            state.mark_receiver_uncertain();
            return Ok(Vec::new());
        }
        budget.require_work(1, cancel)?;
        if !state.take_receiver_work() {
            state.mark_receiver_uncertain();
            return Ok(Vec::new());
        }
        match node.kind() {
            "identifier" => self.resolve_identifier_receiver_with_budget(
                current_uri,
                current_document,
                offset,
                node_text_with_budget(node, &current_document.source, cancel, budget)?,
                lookup_identifier,
                state,
                cancel,
                budget,
            ),
            "exprParens" => first_named_child(node)
                .map(|operand| {
                    self.resolve_receivers_with_state_and_budget(
                        current_uri,
                        current_document,
                        offset,
                        operand,
                        lookup_identifier,
                        state,
                        cancel,
                        budget,
                        depth.saturating_add(1),
                    )
                })
                .unwrap_or_else(|| Ok(Vec::new())),
            "exprCall" => self.resolve_call_receivers_with_budget(
                current_uri,
                current_document,
                offset,
                node,
                lookup_identifier,
                state,
                depth,
                cancel,
                budget,
            ),
            "exprTpl" | "typerefTpl" => {
                let Some(type_ref) = type_ref_from_node(node, &current_document.source) else {
                    return Ok(Vec::new());
                };
                self.type_receivers_for_type_ref_with_budget(
                    current_uri,
                    current_document,
                    type_ref.span.start,
                    &type_ref,
                    lookup_identifier,
                    None,
                    &GenericSubstitution::empty(),
                    state,
                    cancel,
                    budget,
                )
            }
            "exprBinary" | "exprAs" => {
                let Some(operator) = node.child_by_field_name("operator") else {
                    return Ok(Vec::new());
                };
                if node_text_with_budget(operator, &current_document.source, cancel, budget)?
                    .eq_ignore_ascii_case("as")
                {
                    node.child_by_field_name("rhs")
                        .map(|rhs| {
                            self.resolve_cast_receivers_with_state_and_budget(
                                current_uri,
                                current_document,
                                offset,
                                rhs,
                                state,
                                cancel,
                                budget,
                            )
                        })
                        .unwrap_or_else(|| Ok(Vec::new()))
                } else {
                    Ok(Vec::new())
                }
            }
            "exprDot" | "genericDot" | "typerefDot" => {
                if let Some(parts) = qualified_name_parts_with_budget(
                    node,
                    &current_document.source,
                    cancel,
                    budget,
                )? {
                    budget.require_work(parts.len(), cancel)?;
                    return self.resolve_qualified_receiver_path_with_budget(
                        current_uri,
                        current_document,
                        offset,
                        &parts,
                        lookup_identifier,
                        state,
                        cancel,
                        budget,
                    );
                }
                let Some(lhs) = node.child_by_field_name("lhs") else {
                    return Ok(Vec::new());
                };
                let Some(rhs) = node.child_by_field_name("rhs") else {
                    return Ok(Vec::new());
                };
                let Some(rhs_name) = qualified_name_parts_with_budget(
                    rhs,
                    &current_document.source,
                    cancel,
                    budget,
                )?
                .and_then(|parts| parts.last().cloned()) else {
                    return Ok(Vec::new());
                };
                let receivers = self.resolve_receivers_with_state_and_budget(
                    current_uri,
                    current_document,
                    offset,
                    lhs,
                    lookup_identifier,
                    state,
                    cancel,
                    budget,
                    depth.saturating_add(1),
                )?;
                let mut result = Vec::new();
                for receiver in receivers {
                    budget.require_work(1, cancel)?;
                    match receiver {
                        Receiver::Unit(unit_uri) => {
                            result.extend(self.type_receivers_in_unit_with_budget(
                                &unit_uri,
                                &rhs_name,
                                unit_uri == *current_uri,
                                cancel,
                                budget,
                            )?)
                        }
                        Receiver::Type(instance) => {
                            result.extend(self.member_type_receivers_with_budget(
                                &instance.uri,
                                &instance.key,
                                &rhs_name,
                                instance.uri == *current_uri,
                                instance.scope,
                                &instance.substitution,
                                lookup_identifier,
                                state,
                                cancel,
                                budget,
                            )?)
                        }
                        Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => {}
                    }
                }
                Ok(result)
            }
            _ => Ok(Vec::new()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_cast_receivers_with_state_and_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_node: Node<'_>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        let Some(parts) =
            qualified_name_parts_with_budget(type_node, &current_document.source, cancel, budget)?
        else {
            return Ok(Vec::new());
        };
        let receivers = self.type_receivers_for_parts_with_budget(
            current_uri,
            current_document,
            offset,
            &parts,
            type_node,
            None,
            state,
            cancel,
            budget,
        )?;
        if receivers.is_empty() {
            if let Some(builtin) = overload::builtin_type(&parts.join(".")) {
                return Ok(vec![Receiver::Builtin(builtin)]);
            }
        }
        Ok(receivers)
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_call_receivers_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        call: Node<'_>,
        _lookup_identifier: Node<'_>,
        state: &mut ResolutionState,
        depth: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        let Some(entity) = call.child_by_field_name("entity") else {
            return Ok(Vec::new());
        };
        let owner_receivers = callable_owner_node(entity)
            .map(|lhs| {
                self.resolve_receivers_with_state_and_budget(
                    current_uri,
                    current_document,
                    offset,
                    lhs,
                    lhs,
                    state,
                    cancel,
                    budget,
                    depth.saturating_add(1),
                )
            })
            .transpose()?
            .unwrap_or_default();
        let owner_instances = owner_receivers
            .iter()
            .filter_map(|receiver| match receiver {
                Receiver::Type(instance) => Some(instance.clone()),
                Receiver::Unit(_) | Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => None,
            })
            .collect::<Vec<_>>();
        let owner_substitution =
            substitution_from_receivers(owner_receivers).unwrap_or_else(GenericSubstitution::empty);
        let callable_identifier = callable_lookup_identifier(entity);
        let candidates = self.resolve_candidates_at_with_state_and_budget(
            current_uri,
            current_document,
            entity.start_byte(),
            callable_identifier,
            state,
            depth.saturating_add(1),
            cancel,
            budget,
        )?;
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Ok(Vec::new());
        }

        let mut type_candidates = Vec::new();
        let mut routine_candidates = Vec::new();
        for candidate in candidates {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            match symbol.kind {
                SymbolKind::Type => type_candidates.push(candidate),
                SymbolKind::Routine if !symbol.unresolved_abbreviated => {
                    routine_candidates.push(candidate)
                }
                _ => {}
            }
        }

        if !type_candidates.is_empty() {
            if !routine_candidates.is_empty() {
                state.mark_receiver_uncertain();
                return Ok(Vec::new());
            }
            return Ok(type_candidates
                .into_iter()
                .filter_map(|candidate| self.type_receiver_for_candidate(&candidate))
                .collect());
        }
        if routine_candidates.is_empty() {
            return Ok(Vec::new());
        }

        let selection = overload::select(
            self,
            current_uri,
            current_document,
            call,
            &routine_candidates,
            &owner_substitution,
            &owner_instances,
            state,
            depth,
            cancel,
            budget,
        )?;
        let Some(selected_group) = selection.selected_group else {
            state.mark_receiver_uncertain();
            return Ok(Vec::new());
        };
        let result_substitution = selection
            .generic_substitution
            .unwrap_or_else(|| owner_substitution.clone());
        let routine_candidates = routine_candidates
            .into_iter()
            .filter(|candidate| overload::candidate_in_group(self, candidate, &selected_group))
            .collect::<Vec<_>>();

        let mut result_type_source: Option<(Url, ResultTypeAnnotation)> = None;
        let mut constructor = false;
        for candidate in routine_candidates {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            constructor |= symbol.routine_kind == RoutineKind::Constructor;
            if let Some(annotation) = symbol.result_type_annotation() {
                if result_type_source
                    .as_ref()
                    .is_some_and(|(_, current)| current.name != annotation.name)
                {
                    state.mark_receiver_uncertain();
                    return Ok(Vec::new());
                }
                result_type_source = Some((candidate.uri.clone(), annotation));
            }
        }

        let Some((result_uri, annotation)) = result_type_source else {
            if !constructor {
                return Ok(Vec::new());
            }
            let Some(lhs) = entity.child_by_field_name("lhs") else {
                return Ok(Vec::new());
            };
            return self.resolve_receivers_with_state_and_budget(
                current_uri,
                current_document,
                offset,
                lhs,
                callable_identifier,
                state,
                cancel,
                budget,
                depth.saturating_add(1),
            );
        };
        let Some(result_document) = self.documents.get(&result_uri) else {
            return Ok(Vec::new());
        };
        self.result_type_receivers_with_budget(
            &result_uri,
            result_document,
            &annotation,
            &result_substitution,
            state,
            cancel,
            budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_qualified_receiver_path_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        lookup_identifier: Node<'_>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        if parts.len() < 2 || parts.len() > MAX_RECEIVER_WORK {
            return Ok(Vec::new());
        }
        budget.require_work(parts.len(), cancel)?;
        let Some(first) = parts.first() else {
            return Ok(Vec::new());
        };
        let first_is_bound = !self
            .unqualified_references_with_budget(
                current_uri,
                current_document,
                offset,
                first,
                lookup_identifier,
                cancel,
                budget,
            )?
            .is_empty();
        if first.eq_ignore_ascii_case("Self") || first_is_bound {
            let mut receivers = self.resolve_identifier_receiver_with_budget(
                current_uri,
                current_document,
                offset,
                first,
                lookup_identifier,
                state,
                cancel,
                budget,
            )?;
            for member_name in &parts[1..] {
                budget.require_work(1, cancel)?;
                receivers = receivers
                    .into_iter()
                    .map(|receiver| match receiver {
                        Receiver::Type(instance) => self.member_type_receivers_with_budget(
                            &instance.uri,
                            &instance.key,
                            member_name,
                            instance.uri == *current_uri,
                            instance.scope,
                            &instance.substitution,
                            lookup_identifier,
                            state,
                            cancel,
                            budget,
                        ),
                        Receiver::Unit(unit_uri) => self.type_receivers_in_unit_with_budget(
                            &unit_uri,
                            member_name,
                            unit_uri == *current_uri,
                            cancel,
                            budget,
                        ),
                        Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Ok(Vec::new()),
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect();
            }
            return Ok(receivers);
        }

        if let Some(unit_uris) = self.visible_unit_urls_for_path_with_budget(
            current_uri,
            current_document,
            offset,
            parts,
            cancel,
            budget,
        )? {
            return Ok(unit_uris.into_iter().map(Receiver::Unit).collect());
        }

        if let Some((prefix_len, unit_uris)) = self.longest_visible_unit_prefix_with_budget(
            current_uri,
            current_document,
            offset,
            parts,
            cancel,
            budget,
        )? {
            let Some(type_name) = parts.get(prefix_len) else {
                return Ok(Vec::new());
            };
            let allow_implementation = unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
            let mut receivers = Vec::new();
            for unit_uri in unit_uris {
                receivers.extend(self.type_receivers_in_unit_with_budget(
                    &unit_uri,
                    type_name,
                    allow_implementation,
                    cancel,
                    budget,
                )?);
            }
            for member_name in &parts[prefix_len + 1..] {
                budget.require_work(1, cancel)?;
                receivers = receivers
                    .into_iter()
                    .map(|receiver| match receiver {
                        Receiver::Type(instance) => self.member_type_receivers_with_budget(
                            &instance.uri,
                            &instance.key,
                            member_name,
                            instance.uri == *current_uri,
                            instance.scope,
                            &instance.substitution,
                            lookup_identifier,
                            state,
                            cancel,
                            budget,
                        ),
                        Receiver::Unit(_) => Ok(Vec::new()),
                        Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Ok(Vec::new()),
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect();
            }
            return Ok(receivers);
        }

        let mut receivers = self.resolve_identifier_receiver_with_budget(
            current_uri,
            current_document,
            offset,
            first,
            lookup_identifier,
            state,
            cancel,
            budget,
        )?;
        for member_name in &parts[1..] {
            budget.require_work(1, cancel)?;
            receivers = receivers
                .into_iter()
                .map(|receiver| match receiver {
                    Receiver::Type(instance) => self.member_type_receivers_with_budget(
                        &instance.uri,
                        &instance.key,
                        member_name,
                        instance.uri == *current_uri,
                        instance.scope,
                        &instance.substitution,
                        lookup_identifier,
                        state,
                        cancel,
                        budget,
                    ),
                    Receiver::Unit(unit_uri) => self.type_receivers_in_unit_with_budget(
                        &unit_uri,
                        member_name,
                        unit_uri == *current_uri,
                        cancel,
                        budget,
                    ),
                    Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Ok(Vec::new()),
                })
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten()
                .collect();
        }
        Ok(receivers)
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_identifier_receiver_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        name: &str,
        lookup_identifier: Node<'_>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        if name.eq_ignore_ascii_case("Self") {
            let scope = self.budgeted_scope_at(current_document, offset, cancel, budget)?;
            return Ok(current_document
                .owner_type_at_identifier(lookup_identifier, scope)
                .map(|owner_type| {
                    vec![Receiver::Type(TypeInstance {
                        uri: current_uri.clone(),
                        key: owner_type,
                        kind: TypeKind::Other,
                        scope: ROOT_SCOPE,
                        parameter_names: Vec::new(),
                        substitution: GenericSubstitution::empty(),
                    })]
                })
                .unwrap_or_default());
        }
        if name.eq_ignore_ascii_case("Result") {
            let scope = self.budgeted_scope_at(current_document, offset, cancel, budget)?;
            if let Some(annotation) = current_document.result_type_annotation_for_body_scope(scope)
            {
                return self.result_type_receivers_with_budget(
                    current_uri,
                    current_document,
                    &annotation,
                    &GenericSubstitution::empty(),
                    state,
                    cancel,
                    budget,
                );
            }
        }
        let references = self.unqualified_references_with_budget(
            current_uri,
            current_document,
            offset,
            name,
            lookup_identifier,
            cancel,
            budget,
        )?;
        if !references.is_empty() {
            if references
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return Ok(Vec::new());
            }
            let mut result = Vec::new();
            for reference in references {
                budget.require_work(1, cancel)?;
                let Some(symbol) = self.symbol(&reference) else {
                    continue;
                };
                match symbol.kind {
                    SymbolKind::Type => {
                        if let Some(receiver) = self.type_receiver_for_candidate(&reference) {
                            result.push(receiver);
                        }
                    }
                    SymbolKind::Variable
                    | SymbolKind::Parameter
                    | SymbolKind::Field
                    | SymbolKind::Property => {
                        if let Some(declaration_document) = self.documents.get(&reference.uri) {
                            result.extend(self.type_receivers_for_symbol_type_with_budget(
                                &reference.uri,
                                declaration_document,
                                symbol,
                                lookup_identifier,
                                None,
                                &GenericSubstitution::empty(),
                                state,
                                cancel,
                                budget,
                            )?);
                        }
                    }
                    _ => {}
                }
            }
            return Ok(result);
        }
        let urls = self.visible_unit_urls_with_budget(
            current_uri,
            current_document,
            offset,
            name,
            cancel,
            budget,
        )?;
        Ok(urls.into_iter().map(Receiver::Unit).collect())
    }

    #[allow(clippy::too_many_arguments)]
    fn result_type_receivers_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        annotation: &ResultTypeAnnotation,
        substitution: &GenericSubstitution,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        let Some(lookup_identifier) = assistance::identifier_at_with_budget(
            current_document.tree.root_node(),
            annotation.offset,
            cancel,
            budget,
            "receiver result type",
        )?
        else {
            return Ok(overload::builtin_type(&annotation.name)
                .map(|builtin| vec![Receiver::Builtin(builtin)])
                .unwrap_or_default());
        };
        self.type_receivers_for_type_ref_with_budget(
            current_uri,
            current_document,
            annotation.type_ref.span.start,
            &annotation.type_ref,
            lookup_identifier,
            Some(annotation.scope),
            substitution,
            state,
            cancel,
            budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn type_receivers_for_path_with_budget_at_scope(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        path: &str,
        lookup_identifier: Node<'_>,
        scope_override: Option<usize>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        budget.require_work(
            path.split('.').filter(|part| !part.is_empty()).count(),
            cancel,
        )?;
        budget.require_bytes(path.len(), cancel)?;
        let parts = path
            .split('.')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        budget.require_work(parts.len(), cancel)?;
        let receivers = self.type_receivers_for_parts_with_budget(
            current_uri,
            current_document,
            offset,
            &parts,
            lookup_identifier,
            scope_override,
            state,
            cancel,
            budget,
        )?;
        if receivers.is_empty() {
            if let Some(builtin) = overload::builtin_type(path) {
                return Ok(vec![Receiver::Builtin(builtin)]);
            }
        }
        Ok(receivers)
    }

    #[allow(clippy::too_many_arguments)]
    fn type_receivers_for_type_ref_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_ref: &TypeRef,
        lookup_identifier: Node<'_>,
        scope_override: Option<usize>,
        substitution: &GenericSubstitution,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        self.type_receivers_for_type_ref_with_budget_at_depth(
            current_uri,
            current_document,
            offset,
            type_ref,
            lookup_identifier,
            scope_override,
            substitution,
            state,
            cancel,
            budget,
            0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn type_receivers_for_type_ref_with_budget_at_depth(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_ref: &TypeRef,
        lookup_identifier: Node<'_>,
        scope_override: Option<usize>,
        substitution: &GenericSubstitution,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
        depth: usize,
    ) -> Result<Vec<Receiver>, String> {
        if depth >= MAX_TYPE_REF_RECURSION_DEPTH {
            state.mark_receiver_uncertain();
            return Ok(Vec::new());
        }
        budget.require_work(
            type_ref.path.len().saturating_add(type_ref.args.len()),
            cancel,
        )?;
        budget.require_bytes(type_ref.display().len(), cancel)?;
        if type_ref.path.len() == 1 {
            if let Some(resolved) = substitution.get(&type_ref.path[0]) {
                return Ok(resolved.clone().into_receiver().into_iter().collect());
            }
        }
        let bases = self.type_receivers_for_parts_with_budget(
            current_uri,
            current_document,
            offset,
            &type_ref.path,
            lookup_identifier,
            scope_override,
            state,
            cancel,
            budget,
        )?;
        let mut result = Vec::new();
        for base in bases {
            let Receiver::Type(mut instance) = base else {
                if type_ref.args.is_empty() {
                    result.push(base);
                }
                continue;
            };
            if instance.parameter_names.is_empty() {
                if !type_ref.args.is_empty() {
                    continue;
                }
                result.push(Receiver::Type(instance));
                continue;
            }
            if type_ref.args.len() != instance.parameter_names.len() {
                continue;
            }
            let mut actuals = Vec::with_capacity(type_ref.args.len());
            let mut valid = true;
            for argument in &type_ref.args {
                let receivers = self.type_receivers_for_type_ref_with_budget_at_depth(
                    current_uri,
                    current_document,
                    argument.span.start,
                    argument,
                    lookup_identifier,
                    scope_override,
                    substitution,
                    state,
                    cancel,
                    budget,
                    depth.saturating_add(1),
                )?;
                let Some(resolved) = resolved_type_from_receivers(receivers) else {
                    valid = false;
                    break;
                };
                actuals.push(resolved);
            }
            if !valid {
                continue;
            }
            for (name, actual) in instance.parameter_names.iter().zip(actuals) {
                instance.substitution.insert(name, actual);
            }
            if !self
                .generic_type_constraints_satisfied_with_budget(&instance, state, cancel, budget)?
            {
                continue;
            }
            result.push(Receiver::Type(instance));
        }
        if result.is_empty() && type_ref.args.is_empty() {
            if let Some(builtin) = overload::builtin_type(&type_ref.display()) {
                result.push(Receiver::Builtin(builtin));
            }
        }
        Ok(result)
    }

    fn generic_type_constraints_satisfied_with_budget(
        &self,
        instance: &TypeInstance,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<bool, String> {
        let Some(candidate) = self.type_symbol_candidate(instance) else {
            return Ok(false);
        };
        let Some(symbol) = self.symbol(&candidate) else {
            return Ok(false);
        };
        if symbol.generic_parameters.is_empty() {
            return Ok(true);
        }
        // A constraint can expand its own parameter into a larger generic
        // argument on every recursive pass (for example
        // `TNode<T: TNode<TWrap<T>>>`).  Including the substitution in the
        // active key therefore makes every pass look new and lets the call
        // stack grow until the process overflows.  Re-entering the same
        // source generic declaration while proving one specialization is
        // already enough to classify the relationship as unknown.
        let resolution_key = (instance.uri.clone(), instance.key.clone());
        if !state
            .active_generic_constraints
            .insert(resolution_key.clone())
        {
            state.mark_receiver_uncertain();
            return Ok(false);
        }
        let mut ancestry = AncestryResolutionState::new();
        let result = overload::generic_constraints_satisfied(
            self,
            &candidate,
            symbol,
            &instance.substitution,
            state,
            &mut ancestry,
            cancel,
            budget,
        );
        state.active_generic_constraints.remove(&resolution_key);
        match result? {
            overload::ConstraintOutcome::Proven => Ok(true),
            overload::ConstraintOutcome::Unknown => {
                state.mark_receiver_uncertain();
                Ok(false)
            }
            overload::ConstraintOutcome::Contradictory => Ok(false),
        }
    }

    fn generic_type_constraints_satisfied(
        &self,
        instance: &TypeInstance,
        state: &mut ResolutionState,
    ) -> bool {
        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(
            MAX_NAVIGATION_OVERLOAD_WORK,
            MAX_NAVIGATION_OVERLOAD_BYTES,
            "generic type constraint",
        );
        self.generic_type_constraints_satisfied_with_budget(instance, state, &cancel, &mut budget)
            .unwrap_or(false)
    }

    fn type_symbol_candidate(&self, instance: &TypeInstance) -> Option<Candidate> {
        let document = self.documents.get(&instance.uri)?;
        let indices = document.type_symbol_indices.get(&instance.key)?;
        if indices.len() != 1 {
            return None;
        }
        Some(Candidate {
            uri: instance.uri.clone(),
            index: *indices.first()?,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn type_receivers_for_type_ref(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        type_ref: &TypeRef,
        scope_override: Option<usize>,
        substitution: &GenericSubstitution,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        if type_ref.path.len() == 1 {
            if let Some(resolved) = substitution.get(&type_ref.path[0]) {
                return resolved.clone().into_receiver().into_iter().collect();
            }
        }
        let bases = self.type_receivers_for_parts(
            current_uri,
            current_document,
            offset,
            &type_ref.path,
            scope_override,
            state,
        );
        let mut result = Vec::new();
        for base in bases {
            let Receiver::Type(mut instance) = base else {
                if type_ref.args.is_empty() {
                    result.push(base);
                }
                continue;
            };
            if instance.parameter_names.is_empty() {
                if type_ref.args.is_empty() {
                    result.push(Receiver::Type(instance));
                }
                continue;
            }
            if type_ref.args.len() != instance.parameter_names.len() {
                continue;
            }
            let mut actuals = Vec::with_capacity(type_ref.args.len());
            let mut valid = true;
            for argument in &type_ref.args {
                let receivers = self.type_receivers_for_type_ref(
                    current_uri,
                    current_document,
                    argument.span.start,
                    argument,
                    scope_override,
                    substitution,
                    state,
                );
                let Some(resolved) = resolved_type_from_receivers(receivers) else {
                    valid = false;
                    break;
                };
                actuals.push(resolved);
            }
            if !valid {
                continue;
            }
            for (name, actual) in instance.parameter_names.iter().zip(actuals) {
                instance.substitution.insert(name, actual);
            }
            if !self.generic_type_constraints_satisfied(&instance, state) {
                continue;
            }
            result.push(Receiver::Type(instance));
        }
        if result.is_empty() && type_ref.args.is_empty() {
            if let Some(builtin) = overload::builtin_type(&type_ref.display()) {
                result.push(Receiver::Builtin(builtin));
            }
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn type_receivers_for_symbol_type_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        symbol: &Symbol,
        lookup_identifier: Node<'_>,
        scope_override: Option<usize>,
        substitution: &GenericSubstitution,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        if let Some(type_ref) = symbol.type_ref.as_ref() {
            return self.type_receivers_for_type_ref_with_budget(
                current_uri,
                current_document,
                type_ref.span.start,
                type_ref,
                lookup_identifier,
                scope_override,
                substitution,
                state,
                cancel,
                budget,
            );
        }
        let Some(type_name) = symbol.type_name.as_deref() else {
            return Ok(Vec::new());
        };
        self.type_receivers_for_path_with_budget_at_scope(
            current_uri,
            current_document,
            symbol.span.start,
            type_name,
            lookup_identifier,
            scope_override,
            state,
            cancel,
            budget,
        )
    }

    fn type_receivers_for_symbol_type(
        &self,
        current_uri: &Url,
        current_document: &Document,
        symbol: &Symbol,
        scope_override: Option<usize>,
        substitution: &GenericSubstitution,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        if let Some(type_ref) = symbol.type_ref.as_ref() {
            return self.type_receivers_for_type_ref(
                current_uri,
                current_document,
                type_ref.span.start,
                type_ref,
                scope_override,
                substitution,
                state,
            );
        }
        let Some(type_name) = symbol.type_name.as_deref() else {
            return Vec::new();
        };
        self.type_receivers_for_path_at_scope(
            current_uri,
            current_document,
            symbol.span.start,
            type_name,
            scope_override,
            state,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn type_receivers_for_parts_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        lookup_identifier: Node<'_>,
        scope_override: Option<usize>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        if parts.is_empty() {
            return Ok(Vec::new());
        }
        budget.require_work(parts.len(), cancel)?;
        if parts.len() == 1 {
            let candidates = if let Some(scope) = scope_override {
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
            };
            if candidates
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return Ok(Vec::new());
            }
            let mut result = Vec::new();
            for candidate in candidates {
                budget.require_work(1, cancel)?;
                let Some(symbol) = self.symbol(&candidate) else {
                    continue;
                };
                if symbol.kind == SymbolKind::Type {
                    if let Some(receiver) = self.type_receiver_for_candidate(&candidate) {
                        result.push(receiver);
                    }
                }
            }
            return Ok(result);
        }

        let root_candidates = if let Some(scope) = scope_override {
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
        };
        if !root_candidates.is_empty() {
            if root_candidates
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return Ok(Vec::new());
            }
            let mut receivers = Vec::new();
            for candidate in root_candidates {
                budget.require_work(1, cancel)?;
                let Some(symbol) = self.symbol(&candidate) else {
                    continue;
                };
                if symbol.kind != SymbolKind::Type {
                    return Ok(Vec::new());
                }
                let Some(receiver) = self.type_receiver_for_candidate(&candidate) else {
                    return Ok(Vec::new());
                };
                receivers.push(receiver);
            }
            for member_name in &parts[1..] {
                budget.require_work(1, cancel)?;
                receivers = receivers
                    .into_iter()
                    .map(|receiver| match receiver {
                        Receiver::Type(instance) => self.member_type_receivers_with_budget(
                            &instance.uri,
                            &instance.key,
                            member_name,
                            instance.uri == *current_uri,
                            instance.scope,
                            &instance.substitution,
                            lookup_identifier,
                            state,
                            cancel,
                            budget,
                        ),
                        Receiver::Unit(_) => Ok(Vec::new()),
                        Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Ok(Vec::new()),
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect();
            }
            return Ok(receivers);
        }

        let Some((prefix_len, unit_uris)) = self.longest_visible_unit_prefix_with_budget(
            current_uri,
            current_document,
            offset,
            parts,
            cancel,
            budget,
        )?
        else {
            return Ok(Vec::new());
        };
        let Some(type_name) = parts.get(prefix_len) else {
            return Ok(Vec::new());
        };
        let allow_implementation = unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
        let mut receivers = Vec::new();
        for unit_uri in unit_uris {
            receivers.extend(self.type_receivers_in_unit_with_budget(
                &unit_uri,
                type_name,
                allow_implementation,
                cancel,
                budget,
            )?);
        }
        for member_name in &parts[prefix_len + 1..] {
            budget.require_work(1, cancel)?;
            receivers = receivers
                .into_iter()
                .map(|receiver| match receiver {
                    Receiver::Type(instance) => self.member_type_receivers_with_budget(
                        &instance.uri,
                        &instance.key,
                        member_name,
                        instance.uri == *current_uri,
                        instance.scope,
                        &instance.substitution,
                        lookup_identifier,
                        state,
                        cancel,
                        budget,
                    ),
                    Receiver::Unit(_) => Ok(Vec::new()),
                    Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Ok(Vec::new()),
                })
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten()
                .collect();
        }
        Ok(receivers)
    }

    fn type_receivers_in_unit_with_budget(
        &self,
        unit_uri: &Url,
        name: &str,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        let candidates = self.type_candidates_in_unit_with_budget(
            unit_uri,
            name,
            allow_implementation,
            cancel,
            budget,
        )?;
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for candidate in candidates {
            budget.require_work(1, cancel)?;
            let Some(_symbol) = self.symbol(&candidate) else {
                continue;
            };
            if let Some(receiver) = self.type_receiver_for_candidate(&candidate) {
                result.push(receiver);
            }
        }
        Ok(result)
    }

    fn budgeted_scope_at(
        &self,
        document: &Document,
        offset: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<usize, String> {
        budget.require_work(document.scopes.len().saturating_add(1), cancel)?;
        Ok(document.scope_at(offset))
    }

    fn scope_chain_with_budget(
        &self,
        document: &Document,
        offset: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<usize>, String> {
        budget.require_work(document.scopes.len().saturating_add(1), cancel)?;
        Ok(document.scope_chain(offset))
    }

    fn member_owner_substitution(
        &self,
        type_uri: &Url,
        type_key: &str,
        substitution: &GenericSubstitution,
        owner_uri: &Url,
        owner_key: &str,
        state: &mut ResolutionState,
    ) -> Option<GenericSubstitution> {
        let mut active = HashSet::new();
        self.member_owner_substitution_inner(
            type_uri,
            type_key,
            substitution,
            owner_uri,
            owner_key,
            state,
            &mut active,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn routine_owner_substitution_with_budget(
        &self,
        candidate: &Candidate,
        fallback: &GenericSubstitution,
        owner_instances: &[TypeInstance],
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<GenericSubstitution>, String> {
        let Some(symbol) = self.symbol(candidate) else {
            return Ok(None);
        };
        let Some(owner_key) = symbol.owner_type.as_deref() else {
            return Ok(Some(fallback.clone()));
        };
        let mut result = None;
        for instance in owner_instances {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(substitution) = self.member_owner_substitution_with_budget(
                &instance.uri,
                &instance.key,
                &instance.substitution,
                &candidate.uri,
                owner_key,
                state,
                cancel,
                budget,
            )?
            else {
                continue;
            };
            if result
                .as_ref()
                .is_some_and(|current: &GenericSubstitution| current != &substitution)
            {
                return Ok(None);
            }
            result = Some(substitution);
        }
        Ok(result.or_else(|| Some(fallback.clone())))
    }

    #[allow(clippy::too_many_arguments)]
    fn member_owner_substitution_inner(
        &self,
        type_uri: &Url,
        type_key: &str,
        substitution: &GenericSubstitution,
        owner_uri: &Url,
        owner_key: &str,
        state: &mut ResolutionState,
        active: &mut HashSet<(Url, String)>,
    ) -> Option<GenericSubstitution> {
        if type_uri == owner_uri && type_key == owner_key {
            return Some(substitution.clone());
        }
        if !active.insert((type_uri.clone(), type_key.to_owned())) {
            return None;
        }
        let result = (|| {
            let document = self.documents.get(type_uri)?;
            let entries = document.type_ancestry.get(type_key)?;
            if entries.len() != 1 {
                return None;
            }
            let entry = &entries[0];
            for parent in entry.parents.iter().filter(|parent| {
                matches!(
                    (entry.kind, parent.relation),
                    (TypeKind::Class, ParentRelation::Superclass)
                        | (TypeKind::Interface, ParentRelation::InterfaceParent)
                )
            }) {
                let type_ref = parent.type_ref.as_ref()?;
                let receivers = self.type_receivers_for_type_ref(
                    type_uri,
                    document,
                    type_ref.span.start,
                    type_ref,
                    None,
                    substitution,
                    state,
                );
                let parent_instance = unique_type_instance(receivers)?;
                if let Some(result) = self.member_owner_substitution_inner(
                    &parent_instance.uri,
                    &parent_instance.key,
                    &parent_instance.substitution,
                    owner_uri,
                    owner_key,
                    state,
                    active,
                ) {
                    return Some(result);
                }
            }
            None
        })();
        active.remove(&(type_uri.clone(), type_key.to_owned()));
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn member_owner_substitution_with_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        substitution: &GenericSubstitution,
        owner_uri: &Url,
        owner_key: &str,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<GenericSubstitution>, String> {
        let mut active = HashSet::new();
        self.member_owner_substitution_with_budget_inner(
            type_uri,
            type_key,
            substitution,
            owner_uri,
            owner_key,
            state,
            &mut active,
            cancel,
            budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn member_owner_substitution_with_budget_inner(
        &self,
        type_uri: &Url,
        type_key: &str,
        substitution: &GenericSubstitution,
        owner_uri: &Url,
        owner_key: &str,
        state: &mut ResolutionState,
        active: &mut HashSet<(Url, String)>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<GenericSubstitution>, String> {
        if type_uri == owner_uri && type_key == owner_key {
            return Ok(Some(substitution.clone()));
        }
        if !active.insert((type_uri.clone(), type_key.to_owned())) {
            return Ok(None);
        }
        let result = (|| {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(document) = self.documents.get(type_uri) else {
                return Ok(None);
            };
            let Some(entries) = document.type_ancestry.get(type_key) else {
                return Ok(None);
            };
            if entries.len() != 1 {
                return Ok(None);
            }
            let entry = &entries[0];
            for parent in entry.parents.iter().filter(|parent| {
                matches!(
                    (entry.kind, parent.relation),
                    (TypeKind::Class, ParentRelation::Superclass)
                        | (TypeKind::Interface, ParentRelation::InterfaceParent)
                )
            }) {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                let Some(type_ref) = parent.type_ref.as_ref() else {
                    return Ok(None);
                };
                let lookup_identifier = assistance::identifier_at_with_budget(
                    document.tree.root_node(),
                    type_ref.span.start,
                    cancel,
                    budget,
                    "generic parent substitution",
                )?
                .unwrap_or_else(|| document.tree.root_node());
                let receivers = self.type_receivers_for_type_ref_with_budget(
                    type_uri,
                    document,
                    type_ref.span.start,
                    type_ref,
                    lookup_identifier,
                    None,
                    substitution,
                    state,
                    cancel,
                    budget,
                )?;
                let Some(parent_instance) = unique_type_instance(receivers) else {
                    return Ok(None);
                };
                if let Some(result) = self.member_owner_substitution_with_budget_inner(
                    &parent_instance.uri,
                    &parent_instance.key,
                    &parent_instance.substitution,
                    owner_uri,
                    owner_key,
                    state,
                    active,
                    cancel,
                    budget,
                )? {
                    return Ok(Some(result));
                }
            }
            Ok(None)
        })();
        active.remove(&(type_uri.clone(), type_key.to_owned()));
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn member_type_receivers_with_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        name: &str,
        allow_implementation: bool,
        type_scope: usize,
        substitution: &GenericSubstitution,
        lookup_identifier: Node<'_>,
        state: &mut ResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Receiver>, String> {
        let member_key = canonical_name(name);
        let resolution_key = (
            type_uri.clone(),
            type_key.to_owned(),
            member_key.clone(),
            type_scope,
            substitution.clone(),
        );
        if !state.active_members.insert(resolution_key.clone()) {
            return Ok(Vec::new());
        }
        let candidates = self.member_references_for_type_with_budget(
            type_uri,
            type_key,
            type_scope,
            &member_key,
            allow_implementation,
            cancel,
            budget,
        )?;
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            state.active_members.remove(&resolution_key);
            return Ok(Vec::new());
        }
        let constructor_keys = candidates
            .iter()
            .filter_map(|candidate| {
                let symbol = self.symbol(candidate)?;
                (symbol.kind == SymbolKind::Routine
                    && symbol.routine_kind == RoutineKind::Constructor)
                    .then(|| {
                        symbol
                            .routine_key
                            .clone()
                            .map(|key| (candidate.uri.clone(), key))
                    })
                    .flatten()
            })
            .collect::<HashSet<_>>();
        let constructor_is_unique = constructor_keys.len() == 1;
        let mut result = Vec::new();
        for candidate in candidates {
            budget.require_work(1, cancel)?;
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            if symbol.kind == SymbolKind::Routine && symbol.routine_kind == RoutineKind::Constructor
            {
                if constructor_is_unique {
                    result.push(Receiver::Type(TypeInstance {
                        uri: type_uri.clone(),
                        key: type_key.to_owned(),
                        kind: TypeKind::Other,
                        scope: type_scope,
                        parameter_names: Vec::new(),
                        substitution: substitution.clone(),
                    }));
                }
                continue;
            }
            let owner_key = symbol.owner_type.as_deref().unwrap_or(type_key);
            let Some(member_substitution) = self.member_owner_substitution_with_budget(
                type_uri,
                type_key,
                substitution,
                &candidate.uri,
                owner_key,
                state,
                cancel,
                budget,
            )?
            else {
                continue;
            };
            let Some(document) = self.documents.get(&candidate.uri) else {
                continue;
            };
            result.extend(self.type_receivers_for_symbol_type_with_budget(
                &candidate.uri,
                document,
                symbol,
                lookup_identifier,
                None,
                &member_substitution,
                state,
                cancel,
                budget,
            )?);
        }
        state.active_members.remove(&resolution_key);
        Ok(result)
    }

    fn resolve_receivers_with_state(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        node: Node<'_>,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(
            MAX_NAVIGATION_OVERLOAD_WORK,
            MAX_NAVIGATION_OVERLOAD_BYTES,
            "receiver resolution",
        );
        self.resolve_receivers_with_state_and_budget(
            current_uri,
            current_document,
            offset,
            node,
            node,
            state,
            &cancel,
            &mut budget,
            0,
        )
        .unwrap_or_default()
    }

    fn type_receivers_for_path_at_scope(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        path: &str,
        scope_override: Option<usize>,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        let parts = path
            .split('.')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let receivers = self.type_receivers_for_parts(
            current_uri,
            current_document,
            offset,
            &parts,
            scope_override,
            state,
        );
        if receivers.is_empty() {
            if let Some(builtin) = overload::builtin_type(path) {
                return vec![Receiver::Builtin(builtin)];
            }
        }
        receivers
    }

    fn type_receivers_for_parts(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        scope_override: Option<usize>,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        if parts.is_empty() {
            return Vec::new();
        }
        if !state.take_type_work(parts.len()) {
            state.mark_receiver_uncertain();
            return Vec::new();
        }

        if parts.len() == 1 {
            let candidates = if let Some(scope) = scope_override {
                let Some(identifier) = identifier_at(current_document.tree.root_node(), offset)
                else {
                    return Vec::new();
                };
                self.unqualified_references_at_scope(
                    current_uri,
                    current_document,
                    offset,
                    &parts[0],
                    identifier,
                    scope,
                )
            } else {
                self.unqualified_references(current_uri, current_document, offset, &parts[0])
            };
            if candidates
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return Vec::new();
            }
            return candidates
                .into_iter()
                .filter_map(|candidate| self.type_receiver_for_candidate(&candidate))
                .collect();
        }

        let root_candidates = if let Some(scope) = scope_override {
            let Some(identifier) = identifier_at(current_document.tree.root_node(), offset) else {
                return Vec::new();
            };
            self.unqualified_references_at_scope(
                current_uri,
                current_document,
                offset,
                &parts[0],
                identifier,
                scope,
            )
        } else {
            self.unqualified_references(current_uri, current_document, offset, &parts[0])
        };
        if !root_candidates.is_empty() {
            if root_candidates
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return Vec::new();
            }
            if root_candidates.iter().any(|candidate| {
                self.symbol(candidate)
                    .is_none_or(|symbol| symbol.kind != SymbolKind::Type)
            }) {
                return Vec::new();
            }
            let mut receivers = root_candidates
                .into_iter()
                .filter_map(|candidate| self.type_receiver_for_candidate(&candidate))
                .collect::<Vec<_>>();
            for member_name in &parts[1..] {
                receivers = receivers
                    .into_iter()
                    .flat_map(|receiver| match receiver {
                        Receiver::Type(instance) => self.member_type_receivers(
                            &instance.uri,
                            &instance.key,
                            member_name,
                            instance.uri == *current_uri,
                            instance.scope,
                            &instance.substitution,
                            state,
                        ),
                        Receiver::Unit(_) => Vec::new(),
                        Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Vec::new(),
                    })
                    .collect();
            }
            return receivers;
        }

        let Some((prefix_len, unit_uris)) =
            self.longest_visible_unit_prefix(current_uri, current_document, offset, parts)
        else {
            // A qualified type name must not fall back to an unqualified type
            // with the same final component.
            return Vec::new();
        };
        let Some(type_name) = parts.get(prefix_len) else {
            return Vec::new();
        };
        let allow_implementation = unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
        let mut receivers = unit_uris
            .iter()
            .flat_map(|unit_uri| {
                self.type_receivers_in_unit(unit_uri, type_name, allow_implementation)
            })
            .collect::<Vec<_>>();
        for member_name in &parts[prefix_len + 1..] {
            receivers = receivers
                .into_iter()
                .flat_map(|receiver| match receiver {
                    Receiver::Type(instance) => self.member_type_receivers(
                        &instance.uri,
                        &instance.key,
                        member_name,
                        instance.uri == *current_uri,
                        instance.scope,
                        &instance.substitution,
                        state,
                    ),
                    Receiver::Unit(_) => Vec::new(),
                    Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => Vec::new(),
                })
                .collect();
        }
        receivers
    }

    fn longest_visible_unit_prefix(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
    ) -> Option<(usize, Vec<Url>)> {
        if parts.len() > MAX_RECEIVER_WORK {
            return None;
        }

        let region = current_document.region_at(offset);
        let mut best = None;
        let mut consider = |unit_name: &str, unit_uris: Vec<Url>| {
            let unit_parts = unit_name.split('.').collect::<Vec<_>>();
            if unit_parts.len() >= parts.len()
                || !unit_parts.iter().enumerate().all(|(index, unit_part)| {
                    canonical_name(unit_part) == canonical_name(&parts[index])
                })
            {
                return;
            }
            if best
                .as_ref()
                .is_none_or(|(prefix_len, _)| *prefix_len < unit_parts.len())
            {
                best = Some((unit_parts.len(), unit_uris));
            }
        };

        consider(
            current_document.unit_name.as_str(),
            vec![current_uri.clone()],
        );
        for used in current_document.active_uses(region) {
            let unit_uris = self.unit_urls_for_import(current_document, used);
            if !unit_uris.is_empty() {
                consider(used, unit_uris);
            }
        }
        best
    }

    fn visible_unit_urls_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        name: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Url>, String> {
        budget.require_bytes(name.len(), cancel)?;
        let key = canonical_name(name);
        if current_document.unit_name == key {
            budget.require_work(1, cancel)?;
            budget.require_bytes(current_uri.as_str().len(), cancel)?;
            return Ok(vec![current_uri.clone()]);
        }

        let region = current_document.region_at(offset);
        let active_uses = current_document.active_uses_with_budget(region, cancel, budget)?;
        if !active_uses.iter().any(|used| used.as_str() == key) {
            return Ok(Vec::new());
        }
        self.unit_urls_for_import_with_budget(current_document, &key, cancel, budget)
    }

    fn visible_unit_urls_for_path_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<Vec<Url>>, String> {
        budget.require_work(parts.len().saturating_add(1), cancel)?;
        let path_bytes = parts
            .iter()
            .map(String::len)
            .sum::<usize>()
            .saturating_add(parts.len().saturating_sub(1));
        budget.require_bytes(path_bytes, cancel)?;
        let key = canonical_path(parts);
        if current_document.unit_name == key {
            budget.require_bytes(current_uri.as_str().len(), cancel)?;
            return Ok(Some(vec![current_uri.clone()]));
        }

        let region = current_document.region_at(offset);
        let active_uses = current_document.active_uses_with_budget(region, cancel, budget)?;
        if !active_uses.iter().any(|used| used.as_str() == key) {
            return Ok(None);
        }
        let urls = self.unit_urls_for_import_with_budget(current_document, &key, cancel, budget)?;
        Ok((!urls.is_empty()).then_some(urls))
    }

    fn longest_visible_unit_prefix_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<(usize, Vec<Url>)>, String> {
        if parts.len() > MAX_RECEIVER_WORK {
            return Ok(None);
        }
        budget.require_work(parts.len().saturating_add(1), cancel)?;

        let region = current_document.region_at(offset);
        let mut best = None;
        if let Some(prefix_len) = matching_unit_prefix_len(&current_document.unit_name, parts) {
            budget.require_work(1, cancel)?;
            budget.require_bytes(current_uri.as_str().len(), cancel)?;
            best = Some((prefix_len, vec![current_uri.clone()]));
        }

        let active_uses = current_document.active_uses_with_budget(region, cancel, budget)?;
        for used in active_uses {
            check_navigation_cancel(cancel)?;
            let Some(prefix_len) = matching_unit_prefix_len(used, parts) else {
                continue;
            };
            if best
                .as_ref()
                .is_some_and(|(best_prefix_len, _)| *best_prefix_len >= prefix_len)
            {
                continue;
            }
            let unit_uris =
                self.unit_urls_for_import_with_budget(current_document, used, cancel, budget)?;
            if !unit_uris.is_empty() {
                best = Some((prefix_len, unit_uris));
            }
        }
        Ok(best)
    }

    fn type_receivers_in_unit(
        &self,
        unit_uri: &Url,
        name: &str,
        allow_implementation: bool,
    ) -> Vec<Receiver> {
        let candidates = self.type_candidates_in_unit(unit_uri, name, allow_implementation);
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Vec::new();
        }
        candidates
            .into_iter()
            .filter_map(|candidate| self.type_receiver_for_candidate(&candidate))
            .collect()
    }

    fn type_receiver_for_candidate(&self, candidate: &Candidate) -> Option<Receiver> {
        let symbol = self.symbol(candidate)?;
        (symbol.kind == SymbolKind::Type && symbol.generic_parameter.is_none())
            .then(|| Receiver::Type(type_instance_from_symbol(candidate, symbol)))
    }

    fn type_candidates_in_unit(
        &self,
        unit_uri: &Url,
        name: &str,
        allow_implementation: bool,
    ) -> Vec<Candidate> {
        let key = canonical_name(name);
        let Some(document) = self.documents.get(unit_uri) else {
            return Vec::new();
        };
        document
            .type_symbol_indices
            .get(&key)
            .into_iter()
            .flatten()
            .copied()
            .filter(|index| {
                document.symbols.get(*index).is_some_and(|symbol| {
                    !symbol.local_only
                        && (symbol.region == Region::Interface
                            || (allow_implementation && symbol.region == Region::Implementation))
                })
            })
            .map(|index| Candidate {
                uri: unit_uri.clone(),
                index,
            })
            .collect()
    }

    fn type_candidates_in_unit_with_budget(
        &self,
        unit_uri: &Url,
        name: &str,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        budget.require_bytes(name.len(), cancel)?;
        let key = canonical_name(name);
        let Some(document) = self.documents.get(unit_uri) else {
            return Ok(Vec::new());
        };
        let Some(indices) = document.type_symbol_indices.get(&key) else {
            return Ok(Vec::new());
        };
        budget.require_work(indices.len(), cancel)?;
        budget.require_bytes(
            unit_uri.as_str().len().saturating_mul(indices.len()),
            cancel,
        )?;
        #[cfg(test)]
        test_record_materialization(&TEST_TYPE_INDEX_VECTOR_MATERIALIZATIONS);
        let mut result = Vec::with_capacity(indices.len());
        for index in indices {
            check_navigation_cancel(cancel)?;
            if document.symbols.get(*index).is_some_and(|symbol| {
                !symbol.local_only
                    && (symbol.region == Region::Interface
                        || (allow_implementation && symbol.region == Region::Implementation))
            }) {
                result.push(Candidate {
                    uri: unit_uri.clone(),
                    index: *index,
                });
            }
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn member_type_receivers(
        &self,
        type_uri: &Url,
        type_key: &str,
        name: &str,
        allow_implementation: bool,
        type_scope: usize,
        substitution: &GenericSubstitution,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        let member_key = canonical_name(name);
        let resolution_key = (
            type_uri.clone(),
            type_key.to_owned(),
            member_key.clone(),
            type_scope,
            substitution.clone(),
        );
        if !state.active_members.insert(resolution_key.clone()) {
            return Vec::new();
        }
        let candidates = self.member_references_for_type(
            type_uri,
            type_key,
            type_scope,
            &member_key,
            allow_implementation,
        );
        if candidates
            .iter()
            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            state.active_members.remove(&resolution_key);
            return Vec::new();
        }
        let constructor_keys = candidates
            .iter()
            .filter_map(|candidate| {
                let symbol = self.symbol(candidate)?;
                (symbol.kind == SymbolKind::Routine
                    && symbol.routine_kind == RoutineKind::Constructor)
                    .then(|| {
                        symbol
                            .routine_key
                            .clone()
                            .map(|key| (candidate.uri.clone(), key))
                    })
                    .flatten()
            })
            .collect::<HashSet<_>>();
        let constructor_is_unique = constructor_keys.len() == 1;
        let mut result = Vec::new();
        for candidate in candidates {
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            if symbol.kind == SymbolKind::Routine && symbol.routine_kind == RoutineKind::Constructor
            {
                if constructor_is_unique {
                    result.push(Receiver::Type(TypeInstance {
                        uri: type_uri.clone(),
                        key: type_key.to_owned(),
                        kind: TypeKind::Other,
                        scope: type_scope,
                        parameter_names: Vec::new(),
                        substitution: substitution.clone(),
                    }));
                }
                continue;
            }
            let owner_key = symbol.owner_type.as_deref().unwrap_or(type_key);
            let Some(member_substitution) = self.member_owner_substitution(
                type_uri,
                type_key,
                substitution,
                &candidate.uri,
                owner_key,
                state,
            ) else {
                continue;
            };
            let Some(document) = self.documents.get(&candidate.uri) else {
                continue;
            };
            result.extend(self.type_receivers_for_symbol_type(
                &candidate.uri,
                document,
                symbol,
                None,
                &member_substitution,
                state,
            ));
        }
        state.active_members.remove(&resolution_key);
        result
    }

    fn member_references_for_type(
        &self,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        member_key: &str,
        allow_implementation: bool,
    ) -> Vec<Candidate> {
        let mut state = AncestryResolutionState::new();
        self.member_candidates_for_type_with_state(
            type_uri,
            type_key,
            type_scope,
            Some(member_key),
            allow_implementation,
            &mut state,
        )
        .candidates
    }

    #[allow(clippy::too_many_arguments)]
    fn member_references_for_type_with_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        member_key: &str,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let mut state = AncestryResolutionState::new();
        Ok(self
            .member_candidates_for_type_with_state_and_budget(
                type_uri,
                type_key,
                type_scope,
                Some(member_key),
                allow_implementation,
                &mut state,
                cancel,
                budget,
            )?
            .candidates)
    }

    fn member_candidates_for_completion_with_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<MemberLookup, String> {
        let mut state = AncestryResolutionState::new();
        self.member_candidates_for_type_with_state_and_budget(
            type_uri,
            type_key,
            type_scope,
            None,
            allow_implementation,
            &mut state,
            cancel,
            budget,
        )
    }

    fn member_candidates_for_type_with_state(
        &self,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        member_key: Option<&str>,
        allow_implementation: bool,
        state: &mut AncestryResolutionState,
    ) -> MemberLookup {
        let identity = (
            type_uri.clone(),
            type_key.to_owned(),
            type_scope,
            member_key.map(str::to_owned),
            allow_implementation,
        );
        if let Some(resolved) = state.resolved_members.get(&identity) {
            return resolved.clone();
        }
        if !state.active_members.insert(identity.clone()) {
            return MemberLookup::unknown(Vec::new());
        }

        let result = if !state.take_work() {
            MemberLookup::unknown(Vec::new())
        } else {
            let direct = self.direct_member_candidates(
                type_uri,
                type_key,
                type_scope,
                member_key,
                allow_implementation,
            );
            let ancestry = if self.type_requires_ancestry(type_uri, type_key) {
                self.resolve_type_ancestry(type_uri, type_key, state)
            } else {
                complete_ancestry(Vec::new())
            };
            if ancestry.status != AncestryStatus::Complete {
                MemberLookup::unknown(direct)
            } else {
                let mut parent_lookups = Vec::with_capacity(ancestry.parents.len());
                let mut ancestry_known = true;
                for (parent_uri, parent_key) in ancestry.parents {
                    let lookup = self.member_candidates_for_type_with_state(
                        &parent_uri,
                        &parent_key,
                        ROOT_SCOPE,
                        member_key,
                        allow_implementation,
                        state,
                    );
                    if !lookup.ancestry_known {
                        ancestry_known = false;
                        break;
                    }
                    parent_lookups.push(lookup);
                }
                if ancestry_known {
                    self.merge_member_candidates(direct, parent_lookups, member_key)
                } else {
                    MemberLookup::unknown(direct)
                }
            }
        };
        state.active_members.remove(&identity);
        state.resolved_members.insert(identity, result.clone());
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn member_candidates_for_type_with_state_and_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        member_key: Option<&str>,
        allow_implementation: bool,
        state: &mut AncestryResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<MemberLookup, String> {
        let identity = (
            type_uri.clone(),
            type_key.to_owned(),
            type_scope,
            member_key.map(str::to_owned),
            allow_implementation,
        );
        if let Some(resolved) = state.resolved_members.get(&identity) {
            return Ok(resolved.clone());
        }
        if !state.active_members.insert(identity.clone()) {
            return Ok(MemberLookup::unknown(Vec::new()));
        }

        let result = (|| {
            check_navigation_cancel(cancel)?;
            if !state.take_work() {
                return Ok(MemberLookup::unknown(Vec::new()));
            }
            let direct = self.direct_member_candidates_with_budget(
                type_uri,
                type_key,
                type_scope,
                member_key,
                allow_implementation,
                cancel,
                budget,
            )?;
            let ancestry = if self.type_requires_ancestry(type_uri, type_key) {
                self.resolve_type_ancestry_with_budget(type_uri, type_key, state, cancel, budget)?
            } else {
                complete_ancestry(Vec::new())
            };
            if ancestry.status != AncestryStatus::Complete {
                return Ok(MemberLookup::unknown(direct));
            }

            let mut parent_lookups = Vec::with_capacity(ancestry.parents.len());
            let mut ancestry_known = true;
            for (parent_uri, parent_key) in ancestry.parents {
                let lookup = self.member_candidates_for_type_with_state_and_budget(
                    &parent_uri,
                    &parent_key,
                    ROOT_SCOPE,
                    member_key,
                    allow_implementation,
                    state,
                    cancel,
                    budget,
                )?;
                if !lookup.ancestry_known {
                    ancestry_known = false;
                    break;
                }
                parent_lookups.push(lookup);
            }
            Ok(if ancestry_known {
                self.merge_member_candidates(direct, parent_lookups, member_key)
            } else {
                MemberLookup::unknown(direct)
            })
        })();
        state.active_members.remove(&identity);
        if let Ok(resolved) = &result {
            state.resolved_members.insert(identity, resolved.clone());
        }
        result
    }

    fn direct_member_candidates(
        &self,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        member_key: Option<&str>,
        allow_implementation: bool,
    ) -> Vec<Candidate> {
        let Some(document) = self.documents.get(type_uri) else {
            return Vec::new();
        };
        let indices = match member_key {
            Some(member_key) => document
                .member_symbol_indices
                .get(&(type_key.to_owned(), member_key.to_owned())),
            None => document.member_symbol_indices_by_owner.get(type_key),
        };
        let Some(indices) = indices else {
            return Vec::new();
        };
        indices
            .iter()
            .filter_map(|index| {
                let symbol = document.symbols.get(*index)?;
                (member_symbol_is_visible(document, symbol, type_key, allow_implementation)
                    && (type_scope == ROOT_SCOPE || symbol.scope == type_scope))
                    .then(|| Candidate {
                        uri: type_uri.clone(),
                        index: *index,
                    })
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn direct_member_candidates_with_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        type_scope: usize,
        member_key: Option<&str>,
        allow_implementation: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        budget.require_bytes(
            type_key
                .len()
                .saturating_add(member_key.map_or(0, str::len)),
            cancel,
        )?;
        let Some(document) = self.documents.get(type_uri) else {
            return Ok(Vec::new());
        };
        let indices = match member_key {
            Some(member_key) => document
                .member_symbol_indices
                .get(&(type_key.to_owned(), member_key.to_owned())),
            None => document.member_symbol_indices_by_owner.get(type_key),
        };
        let Some(indices) = indices else {
            return Ok(Vec::new());
        };
        budget.require_work(indices.len(), cancel)?;
        budget.require_bytes(
            type_uri.as_str().len().saturating_mul(indices.len()),
            cancel,
        )?;
        #[cfg(test)]
        test_record_materialization(&TEST_MEMBER_INDEX_VECTOR_MATERIALIZATIONS);
        let mut result = Vec::with_capacity(indices.len());
        for index in indices {
            check_navigation_cancel(cancel)?;
            let Some(symbol) = document.symbols.get(*index) else {
                continue;
            };
            if member_symbol_is_visible(document, symbol, type_key, allow_implementation)
                && (type_scope == ROOT_SCOPE || symbol.scope == type_scope)
            {
                result.push(Candidate {
                    uri: type_uri.clone(),
                    index: *index,
                });
            }
        }
        Ok(result)
    }

    fn type_requires_ancestry(&self, type_uri: &Url, type_key: &str) -> bool {
        let Some(document) = self.documents.get(type_uri) else {
            return true;
        };
        let Some(entries) = document.type_ancestry.get(type_key) else {
            return true;
        };
        entries.len() != 1 || entries[0].parent_declared
    }

    fn resolve_type_ancestry(
        &self,
        type_uri: &Url,
        type_key: &str,
        state: &mut AncestryResolutionState,
    ) -> TypeAncestryResolution {
        let identity = (type_uri.clone(), type_key.to_owned());
        if let Some(resolved) = state.resolved_types.get(&identity) {
            return resolved.clone();
        }
        if state.active_types.contains(&identity) {
            return unknown_ancestry();
        }
        let Some(document) = self.documents.get(type_uri) else {
            return unknown_ancestry();
        };
        let Some(entries) = document.type_ancestry.get(type_key) else {
            return unknown_ancestry();
        };
        if !state.take_work() {
            return unknown_ancestry();
        }
        state.active_types.insert(identity.clone());
        let mut result = if entries.len() == 1 {
            self.resolve_type_ancestry_entry(type_uri, type_key, &entries[0], document)
        } else {
            unknown_ancestry()
        };
        if result.status == AncestryStatus::Complete {
            for (parent_uri, parent_key) in result.parents.clone() {
                if self
                    .resolve_type_ancestry(&parent_uri, &parent_key, state)
                    .status
                    != AncestryStatus::Complete
                {
                    result = unknown_ancestry();
                    break;
                }
            }
        }
        state.active_types.remove(&identity);
        state.resolved_types.insert(identity, result.clone());
        result
    }

    fn resolve_type_ancestry_entry(
        &self,
        type_uri: &Url,
        type_key: &str,
        entry: &TypeAncestry,
        document: &Document,
    ) -> TypeAncestryResolution {
        let Some(type_indices) = document.type_symbol_indices.get(type_key) else {
            return unknown_ancestry();
        };
        if type_indices.len() != 1 {
            return unknown_ancestry();
        }
        let type_candidate = Candidate {
            uri: type_uri.clone(),
            index: type_indices[0],
        };
        if self.candidate_is_conditionally_unknown(&type_candidate) {
            return unknown_ancestry();
        }
        let Some(type_symbol) = self.symbol(&type_candidate) else {
            return unknown_ancestry();
        };
        if type_symbol.kind != SymbolKind::Type || type_symbol.type_kind != entry.kind {
            return unknown_ancestry();
        }
        if !matches!(entry.kind, TypeKind::Class | TypeKind::Interface) {
            return complete_ancestry(Vec::new());
        }
        if entry.parent_declared && entry.parents.is_empty() {
            return unknown_ancestry();
        }

        let mut parents = Vec::with_capacity(entry.parents.len());
        for parent in entry.parents.iter().filter(|parent| {
            matches!(
                (entry.kind, parent.relation),
                (TypeKind::Class, ParentRelation::Superclass)
                    | (TypeKind::Interface, ParentRelation::InterfaceParent)
            )
        }) {
            if parent.path.is_empty() {
                return unknown_ancestry();
            }
            let candidates = dedup_candidates(self.type_parent_candidates(
                type_uri,
                document,
                entry.name_span.start,
                &parent.path,
            ));
            if candidates.len() != 1
                || candidates
                    .iter()
                    .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return unknown_ancestry();
            }
            let candidate = &candidates[0];
            let Some(symbol) = self.symbol(candidate) else {
                return unknown_ancestry();
            };
            if symbol.kind != SymbolKind::Type {
                return unknown_ancestry();
            }
            if entry.kind == TypeKind::Class && symbol.type_kind == TypeKind::Interface {
                continue;
            }
            let expected_kind = match entry.kind {
                TypeKind::Class => TypeKind::Class,
                TypeKind::Interface => TypeKind::Interface,
                _ => continue,
            };
            if symbol.type_kind != expected_kind {
                return unknown_ancestry();
            }
            parents.push((candidate.uri.clone(), symbol.key.clone()));
        }
        complete_ancestry(parents)
    }

    fn type_parent_candidates(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        path: &[String],
    ) -> Vec<Candidate> {
        let Some(first) = path.first() else {
            return Vec::new();
        };
        let region = current_document.region_at(offset);
        if path.len() == 1 {
            let local = self.type_candidates_in_unit(
                current_uri,
                first,
                matches!(region, Region::Implementation | Region::Other),
            );
            if !local.is_empty() {
                return local;
            }
            let active_uses = current_document.active_uses(region);
            if active_uses
                .iter()
                .any(|unit| current_document.unknown_imports.contains(unit.as_str()))
            {
                return Vec::new();
            }
            return dedup_candidates(
                active_uses
                    .into_iter()
                    .flat_map(|unit| {
                        self.unit_urls_for_import(current_document, unit)
                            .into_iter()
                            .flat_map(|unit_uri| {
                                self.type_candidates_in_unit(&unit_uri, first, false)
                            })
                    })
                    .collect(),
            );
        }

        let Some((prefix_len, unit_uris)) =
            self.longest_visible_unit_prefix(current_uri, current_document, offset, path)
        else {
            return Vec::new();
        };
        if prefix_len.saturating_add(1) != path.len() {
            return Vec::new();
        }
        let Some(type_name) = path.get(prefix_len) else {
            return Vec::new();
        };
        let allow_implementation = matches!(region, Region::Implementation | Region::Other)
            && unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
        dedup_candidates(
            unit_uris
                .into_iter()
                .flat_map(|unit_uri| {
                    self.type_candidates_in_unit(&unit_uri, type_name, allow_implementation)
                })
                .collect(),
        )
    }

    fn resolve_type_ancestry_with_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        state: &mut AncestryResolutionState,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<TypeAncestryResolution, String> {
        let identity = (type_uri.clone(), type_key.to_owned());
        if let Some(resolved) = state.resolved_types.get(&identity) {
            return Ok(resolved.clone());
        }
        if state.active_types.contains(&identity) {
            return Ok(unknown_ancestry());
        }
        let Some(document) = self.documents.get(type_uri) else {
            return Ok(unknown_ancestry());
        };
        let Some(entries) = document.type_ancestry.get(type_key) else {
            return Ok(unknown_ancestry());
        };
        if !state.take_work() {
            return Ok(unknown_ancestry());
        }
        budget.require_work(1, cancel)?;
        budget.require_bytes(
            type_uri.as_str().len().saturating_add(type_key.len()),
            cancel,
        )?;
        state.active_types.insert(identity.clone());
        let result = if entries.len() == 1 {
            self.resolve_type_ancestry_entry_with_budget(
                type_uri,
                type_key,
                &entries[0],
                document,
                cancel,
                budget,
            )
        } else {
            Ok(unknown_ancestry())
        };
        let mut result = match result {
            Ok(result) => result,
            Err(error) => {
                state.active_types.remove(&identity);
                return Err(error);
            }
        };
        if result.status == AncestryStatus::Complete {
            for (parent_uri, parent_key) in result.parents.clone() {
                if self
                    .resolve_type_ancestry_with_budget(
                        &parent_uri,
                        &parent_key,
                        state,
                        cancel,
                        budget,
                    )?
                    .status
                    != AncestryStatus::Complete
                {
                    result = unknown_ancestry();
                    break;
                }
            }
        }
        state.active_types.remove(&identity);
        state.resolved_types.insert(identity, result.clone());
        Ok(result)
    }

    fn resolve_type_ancestry_entry_with_budget(
        &self,
        type_uri: &Url,
        type_key: &str,
        entry: &TypeAncestry,
        document: &Document,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<TypeAncestryResolution, String> {
        let Some(type_indices) = document.type_symbol_indices.get(type_key) else {
            return Ok(unknown_ancestry());
        };
        if type_indices.len() != 1 {
            return Ok(unknown_ancestry());
        }
        let type_candidate = Candidate {
            uri: type_uri.clone(),
            index: type_indices[0],
        };
        budget.require_work(1, cancel)?;
        if self.candidate_is_conditionally_unknown(&type_candidate) {
            return Ok(unknown_ancestry());
        }
        let Some(type_symbol) = self.symbol(&type_candidate) else {
            return Ok(unknown_ancestry());
        };
        if type_symbol.kind != SymbolKind::Type || type_symbol.type_kind != entry.kind {
            return Ok(unknown_ancestry());
        }
        if !matches!(entry.kind, TypeKind::Class | TypeKind::Interface) {
            return Ok(complete_ancestry(Vec::new()));
        }
        if entry.parent_declared && entry.parents.is_empty() {
            return Ok(unknown_ancestry());
        }

        let mut parents = Vec::with_capacity(entry.parents.len());
        for parent in entry.parents.iter().filter(|parent| {
            matches!(
                (entry.kind, parent.relation),
                (TypeKind::Class, ParentRelation::Superclass)
                    | (TypeKind::Interface, ParentRelation::InterfaceParent)
            )
        }) {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if parent.path.is_empty() {
                return Ok(unknown_ancestry());
            }
            let candidates = dedup_candidates(self.type_parent_candidates_with_budget(
                type_uri,
                document,
                entry.name_span.start,
                &parent.path,
                cancel,
                budget,
            )?);
            budget.require_work(candidates.len(), cancel)?;
            if candidates.len() != 1
                || candidates
                    .iter()
                    .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return Ok(unknown_ancestry());
            }
            let candidate = &candidates[0];
            let Some(symbol) = self.symbol(candidate) else {
                return Ok(unknown_ancestry());
            };
            if symbol.kind != SymbolKind::Type {
                return Ok(unknown_ancestry());
            }
            if entry.kind == TypeKind::Class && symbol.type_kind == TypeKind::Interface {
                continue;
            }
            let expected_kind = match entry.kind {
                TypeKind::Class => TypeKind::Class,
                TypeKind::Interface => TypeKind::Interface,
                _ => continue,
            };
            if symbol.type_kind != expected_kind {
                return Ok(unknown_ancestry());
            }
            parents.push((candidate.uri.clone(), symbol.key.clone()));
        }
        Ok(complete_ancestry(parents))
    }

    fn type_parent_candidates_with_budget(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        path: &[String],
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<Candidate>, String> {
        let Some(first) = path.first() else {
            return Ok(Vec::new());
        };
        budget.require_work(path.len().saturating_add(1), cancel)?;
        budget.require_bytes(
            path.iter()
                .map(String::len)
                .sum::<usize>()
                .saturating_add(path.len().saturating_sub(1)),
            cancel,
        )?;
        let region = current_document.region_at(offset);
        if path.len() == 1 {
            let local = self.type_candidates_in_unit_with_budget(
                current_uri,
                first,
                matches!(region, Region::Implementation | Region::Other),
                cancel,
                budget,
            )?;
            if !local.is_empty() {
                return Ok(local);
            }
            let active_uses = current_document.active_uses_with_budget(region, cancel, budget)?;
            if active_uses
                .iter()
                .any(|unit| current_document.unknown_imports.contains(unit.as_str()))
            {
                return Ok(Vec::new());
            }
            let mut imported = Vec::new();
            for unit in active_uses {
                for unit_uri in
                    self.unit_urls_for_import_with_budget(current_document, unit, cancel, budget)?
                {
                    imported.extend(self.type_candidates_in_unit_with_budget(
                        &unit_uri, first, false, cancel, budget,
                    )?);
                }
            }
            return Ok(dedup_candidates(imported));
        }

        let Some((prefix_len, unit_uris)) = self.longest_visible_unit_prefix_with_budget(
            current_uri,
            current_document,
            offset,
            path,
            cancel,
            budget,
        )?
        else {
            return Ok(Vec::new());
        };
        if prefix_len.saturating_add(1) != path.len() {
            return Ok(Vec::new());
        }
        let Some(type_name) = path.get(prefix_len) else {
            return Ok(Vec::new());
        };
        let allow_implementation = matches!(region, Region::Implementation | Region::Other)
            && unit_uris.iter().any(|unit_uri| unit_uri == current_uri);
        let mut result = Vec::new();
        for unit_uri in unit_uris {
            result.extend(self.type_candidates_in_unit_with_budget(
                &unit_uri,
                type_name,
                allow_implementation,
                cancel,
                budget,
            )?);
        }
        Ok(dedup_candidates(result))
    }

    fn merge_member_candidates(
        &self,
        direct: Vec<Candidate>,
        parent_lookups: Vec<MemberLookup>,
        member_key: Option<&str>,
    ) -> MemberLookup {
        if member_key.is_some() {
            let direct_has_routine = direct.iter().any(|candidate| {
                self.symbol(candidate)
                    .is_some_and(|symbol| symbol.kind == SymbolKind::Routine)
            });
            if !direct.is_empty() && !direct_has_routine {
                return MemberLookup::known(direct);
            }
            let direct_routines = direct
                .iter()
                .filter_map(|candidate| {
                    self.symbol(candidate)
                        .filter(|symbol| symbol.kind == SymbolKind::Routine)
                })
                .collect::<Vec<_>>();
            let direct_overrides = direct_routines
                .iter()
                .filter(|symbol| symbol.routine_directives.override_)
                .filter_map(|symbol| symbol.routine_signature.clone())
                .collect::<HashSet<_>>();
            let direct_overload_set = !direct_routines.is_empty()
                && direct_routines
                    .iter()
                    .all(|symbol| symbol.routine_directives.overload);
            let mut selected: Option<Vec<Candidate>> = None;
            for lookup in parent_lookups {
                let candidates = dedup_candidates(
                    lookup
                        .candidates
                        .into_iter()
                        .filter(|candidate| {
                            if direct_routines.is_empty() {
                                return true;
                            }
                            if !direct_overload_set {
                                return false;
                            }
                            self.symbol(candidate).is_some_and(|symbol| {
                                if symbol.kind != SymbolKind::Routine
                                    || !symbol.routine_directives.overload
                                {
                                    return false;
                                }
                                let overridden =
                                    symbol.routine_signature.as_ref().is_some_and(|signature| {
                                        direct_overrides.contains(signature)
                                            && (symbol.routine_directives.virtual_
                                                || symbol.routine_directives.dynamic)
                                    });
                                !overridden
                            })
                        })
                        .collect(),
                );
                if candidates.is_empty() {
                    continue;
                }
                if selected
                    .as_ref()
                    .is_some_and(|current| !candidate_sets_equal(current, &candidates))
                {
                    return MemberLookup::unknown(direct);
                }
                selected = Some(candidates);
            }
            let mut result = direct;
            if let Some(selected) = selected {
                result.extend(selected);
            }
            return MemberLookup::known(result);
        }

        let direct_keys: HashSet<String> = direct
            .iter()
            .filter_map(|candidate| self.symbol(candidate).map(|symbol| symbol.key.clone()))
            .collect();
        let mut key_order = Vec::new();
        let mut seen_keys = HashSet::new();
        let mut ambiguous_names = parent_lookups
            .iter()
            .flat_map(|lookup| lookup.ambiguous_names.iter().cloned())
            .collect::<HashSet<_>>();
        for key in &direct_keys {
            ambiguous_names.remove(key);
        }
        let branch_maps: Vec<HashMap<String, Vec<Candidate>>> = parent_lookups
            .into_iter()
            .map(|lookup| {
                let mut by_key = HashMap::new();
                for candidate in dedup_candidates(lookup.candidates) {
                    let Some(symbol) = self.symbol(&candidate) else {
                        continue;
                    };
                    if seen_keys.insert(symbol.key.clone()) {
                        key_order.push(symbol.key.clone());
                    }
                    push_unique_candidate(
                        by_key.entry(symbol.key.clone()).or_insert_with(Vec::new),
                        candidate,
                    );
                }
                by_key
            })
            .collect();

        let mut result = direct;
        for key in key_order {
            if direct_keys.contains(&key) {
                continue;
            }
            let mut selected: Option<Vec<Candidate>> = None;
            let mut ambiguous = false;
            for branch in &branch_maps {
                let Some(candidates) = branch.get(&key) else {
                    continue;
                };
                if selected
                    .as_ref()
                    .is_some_and(|current| !candidate_sets_equal(current, candidates))
                {
                    ambiguous = true;
                    break;
                }
                selected = Some(candidates.clone());
            }
            if ambiguous {
                ambiguous_names.insert(key);
            } else if !ambiguous_names.contains(&key) {
                if let Some(selected) = selected {
                    result.extend(selected);
                }
            }
        }
        if ambiguous_names.is_empty() {
            MemberLookup::known(result)
        } else {
            MemberLookup::ambiguous(result, ambiguous_names)
        }
    }

    fn exported_references_for_document(&self, uri: &Url) -> Vec<Candidate> {
        let Some(document) = self.documents.get(uri) else {
            return Vec::new();
        };
        #[cfg(test)]
        TEST_LEGACY_EXPORTED_MATERIALIZATIONS.with(|count| {
            count.set(
                count
                    .get()
                    .saturating_add(document.exported_symbol_indices.len()),
            );
        });
        document
            .exported_symbol_indices
            .iter()
            .filter_map(|index| document.symbols.get(*index).map(|_| *index))
            .map(|index| Candidate {
                uri: uri.clone(),
                index,
            })
            .collect()
    }

    fn symbol(&self, candidate: &Candidate) -> Option<&Symbol> {
        self.documents
            .get(&candidate.uri)
            .and_then(|document| document.symbols.get(candidate.index))
    }

    fn type_kind(&self, uri: &Url, key: &str) -> Option<TypeKind> {
        let document = self.documents.get(uri)?;
        let indices = document.type_symbol_indices.get(key)?;
        if indices.len() != 1 {
            return None;
        }
        let symbol = document.symbols.get(*indices.first()?)?;
        (symbol.kind == SymbolKind::Type).then_some(symbol.type_kind)
    }

    fn expand_property_candidates(
        &self,
        references: Vec<Candidate>,
        target: NavigationTarget,
    ) -> Vec<Candidate> {
        if target == NavigationTarget::Declaration {
            return references;
        }

        references
            .into_iter()
            .flat_map(|candidate| {
                let Some(property) = self.symbol(&candidate) else {
                    return vec![candidate];
                };
                if property.kind != SymbolKind::Property {
                    return vec![candidate];
                }
                let Some(accessor) = property.accessor.as_deref() else {
                    return vec![candidate];
                };
                let Some(owner_type) = property.owner_type.as_deref() else {
                    return vec![candidate];
                };
                let key = canonical_name(accessor);
                let Some(document) = self.documents.get(&candidate.uri) else {
                    return vec![candidate];
                };
                let accessors: Vec<Candidate> = document
                    .symbols
                    .iter()
                    .enumerate()
                    .filter(|(_, symbol)| {
                        symbol.owner_type.as_deref() == Some(owner_type)
                            && symbol.key == key
                            && matches!(symbol.kind, SymbolKind::Field | SymbolKind::Routine)
                    })
                    .map(|(index, _)| Candidate {
                        uri: candidate.uri.clone(),
                        index,
                    })
                    .collect();
                if accessors.is_empty() {
                    vec![candidate]
                } else {
                    accessors
                }
            })
            .collect()
    }

    fn locations_for(&self, references: Vec<Candidate>, target: NavigationTarget) -> Vec<Location> {
        let mut routine_groups: BTreeMap<(String, String), Vec<Candidate>> = BTreeMap::new();
        let mut non_routines = Vec::new();
        for candidate in references {
            let Some(symbol) = self.symbol(&candidate) else {
                continue;
            };
            if let (SymbolKind::Routine, Some(routine_key)) = (symbol.kind, &symbol.routine_key) {
                routine_groups
                    .entry((candidate.uri.to_string(), routine_key.clone()))
                    .or_default()
                    .push(candidate);
            } else {
                non_routines.push(candidate);
            }
        }

        let mut selected = non_routines;
        for candidates in routine_groups.into_values() {
            let declarations: Vec<Candidate> = candidates
                .iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.origin == Origin::Declaration)
                })
                .cloned()
                .collect();
            let interface_declarations: Vec<Candidate> = declarations
                .iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.region == Region::Interface)
                })
                .cloned()
                .collect();
            let definitions: Vec<Candidate> = candidates
                .iter()
                .filter(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.origin == Origin::Definition)
                })
                .cloned()
                .collect();

            match target {
                NavigationTarget::Declaration => {
                    if !interface_declarations.is_empty() {
                        selected.extend(interface_declarations);
                    } else if !declarations.is_empty() {
                        selected.extend(declarations);
                    } else {
                        selected.extend(definitions);
                    }
                }
                NavigationTarget::Definition | NavigationTarget::Implementation => {
                    if !definitions.is_empty() {
                        selected.extend(definitions);
                    } else {
                        selected.extend(declarations);
                    }
                }
            }
        }

        let mut locations = Vec::new();
        let mut seen = HashSet::new();
        for candidate in selected {
            let Some(document) = self.documents.get(&candidate.uri) else {
                continue;
            };
            let Some(symbol) = document.symbols.get(candidate.index) else {
                continue;
            };
            let Some(location) = location_for_span(&candidate.uri, &document.source, symbol.span)
            else {
                continue;
            };
            let deduplication_key = (
                location.uri.to_string(),
                location.range.start.line,
                location.range.start.character,
                location.range.end.line,
                location.range.end.character,
            );
            if seen.insert(deduplication_key) {
                locations.push(location);
            }
        }
        locations.sort_by(|left, right| {
            left.uri
                .as_str()
                .cmp(right.uri.as_str())
                .then_with(|| left.range.start.line.cmp(&right.range.start.line))
                .then_with(|| left.range.start.character.cmp(&right.range.start.character))
        });
        locations
    }
}

const ROOT_SCOPE: usize = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Region {
    Interface,
    Implementation,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Origin {
    Declaration,
    Definition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SymbolKind {
    Unit,
    Type,
    Routine,
    Variable,
    Constant,
    Parameter,
    Field,
    Property,
    EnumValue,
    Label,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum TypeKind {
    Class,
    Record,
    Interface,
    Enum,
    Array,
    Callable,
    String,
    File,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum TypeIdentity {
    Builtin(BuiltinType),
    IntegerLiteral(i128),
    Named {
        uri: Url,
        key: String,
        kind: TypeKind,
        args: Vec<TypeIdentity>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TypeRef {
    path: Vec<String>,
    args: Vec<TypeRef>,
    span: Span,
}

impl TypeRef {
    fn display(&self) -> String {
        let path = self.path.join(".");
        if self.args.is_empty() {
            return path;
        }
        format!(
            "{path}<{}>",
            self.args
                .iter()
                .map(TypeRef::display)
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GenericParameter {
    name: String,
    span: Span,
    constraint: Option<TypeRef>,
    constraint_unsupported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GenericSubstitution(BTreeMap<String, ResolvedType>);

impl GenericSubstitution {
    fn empty() -> Self {
        Self(BTreeMap::new())
    }

    fn get(&self, name: &str) -> Option<&ResolvedType> {
        self.0.get(&canonical_name(name))
    }

    fn insert(&mut self, name: &str, value: ResolvedType) {
        self.0.insert(canonical_name(name), value);
    }

    fn remove(&mut self, name: &str) {
        self.0.remove(&canonical_name(name));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TypeInstance {
    uri: Url,
    key: String,
    kind: TypeKind,
    scope: usize,
    parameter_names: Vec<String>,
    substitution: GenericSubstitution,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ResolvedType {
    Builtin(BuiltinType),
    IntegerLiteral(i128),
    Named(TypeInstance),
}

impl ResolvedType {
    fn into_receiver(self) -> Option<Receiver> {
        Some(match self {
            Self::Builtin(builtin) => Receiver::Builtin(builtin),
            Self::IntegerLiteral(value) => Receiver::IntegerLiteral(value),
            Self::Named(instance) => Receiver::Type(instance),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RoutineKind {
    Procedure,
    Function,
    Constructor,
    Destructor,
    Operator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Span {
    start: usize,
    end: usize,
}

impl Span {
    fn from_node(node: Node<'_>) -> Self {
        Self {
            start: node.start_byte(),
            end: node.end_byte(),
        }
    }

    fn contains(self, other: Span) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    fn contains_offset(self, offset: usize) -> bool {
        self.start <= offset && (offset < self.end || self.start == self.end)
    }
}

#[derive(Debug)]
struct Scope {
    start: usize,
    end: usize,
    parent: Option<usize>,
    owner_type: Option<String>,
}

#[derive(Debug)]
pub(super) struct AssistanceBudget {
    remaining_work: usize,
    remaining_bytes: usize,
    work_limit: usize,
    byte_limit: usize,
    operation: &'static str,
}

fn check_navigation_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("request cancelled".to_string())
    } else {
        Ok(())
    }
}

impl AssistanceBudget {
    pub(super) fn new(work_limit: usize, byte_limit: usize, operation: &'static str) -> Self {
        Self {
            remaining_work: work_limit,
            remaining_bytes: byte_limit,
            work_limit,
            byte_limit,
            operation,
        }
    }

    pub(super) fn take_work(&mut self, amount: usize, cancel: &AtomicBool) -> Result<bool, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        if amount > self.remaining_work {
            self.remaining_work = 0;
            return Ok(false);
        }
        self.remaining_work -= amount;
        Ok(true)
    }

    pub(super) fn take_bytes(
        &mut self,
        amount: usize,
        cancel: &AtomicBool,
    ) -> Result<bool, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        if amount > self.remaining_bytes {
            self.remaining_bytes = 0;
            return Ok(false);
        }
        self.remaining_bytes -= amount;
        Ok(true)
    }

    pub(super) fn require_work(
        &mut self,
        amount: usize,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if self.take_work(amount, cancel)? {
            Ok(())
        } else {
            Err(format!(
                "{} exceeds the {}-node traversal limit",
                self.operation, self.work_limit
            ))
        }
    }

    pub(super) fn require_bytes(
        &mut self,
        amount: usize,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if self.take_bytes(amount, cancel)? {
            Ok(())
        } else {
            Err(format!(
                "{} exceeds the {}-byte scan limit",
                self.operation, self.byte_limit
            ))
        }
    }
}

#[derive(Debug)]
struct Symbol {
    span: Span,
    declaration_span: Span,
    selection_span: Span,
    name: String,
    key: String,
    kind: SymbolKind,
    type_kind: TypeKind,
    routine_kind: RoutineKind,
    routine_directives: RoutineDirectives,
    parameter_mode: Option<ParameterMode>,
    scope: usize,
    owner_type: Option<String>,
    owner_type_name: Option<String>,
    generic_parameters: Vec<GenericParameter>,
    generic_parameter: Option<String>,
    type_name: Option<String>,
    type_ref: Option<TypeRef>,
    result_type_name: Option<String>,
    result_type_ref: Option<TypeRef>,
    result_type_span: Option<Span>,
    region: Region,
    origin: Origin,
    local_only: bool,
    routine_key: Option<String>,
    routine_signature: Option<String>,
    routine_header_span: Option<Span>,
    routine_parameter_spans: Vec<Span>,
    routine_parameters: Vec<RoutineParameter>,
    type_excerpt_end: Option<usize>,
    body_scope: Option<usize>,
    unresolved_abbreviated: bool,
    accessor: Option<String>,
}

#[derive(Debug, Clone)]
struct RoutineParameter {
    span: Span,
    type_span: Option<Span>,
    type_name: Option<String>,
    type_ref: Option<TypeRef>,
    mode: ParameterMode,
    has_default: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct RoutineDirectives {
    overload: bool,
    virtual_: bool,
    dynamic: bool,
    override_: bool,
    reintroduce: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParameterMode {
    Value,
    Var,
    Out,
    Const,
    ConstRef,
}

#[derive(Debug, Clone)]
struct ResultTypeAnnotation {
    name: String,
    type_ref: TypeRef,
    offset: usize,
    scope: usize,
}

impl Symbol {
    fn result_type_annotation(&self) -> Option<ResultTypeAnnotation> {
        self.result_type_name
            .as_ref()
            .map(|name| ResultTypeAnnotation {
                name: name.clone(),
                type_ref: self.result_type_ref.clone().unwrap_or_else(|| TypeRef {
                    path: name
                        .split('.')
                        .filter(|part| !part.is_empty())
                        .map(str::to_owned)
                        .collect(),
                    args: Vec::new(),
                    span: self.result_type_span.unwrap_or(self.span),
                }),
                offset: self
                    .result_type_span
                    .map_or(self.span.start, |span| span.start),
                scope: self.scope,
            })
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    uri: Url,
    index: usize,
}

#[derive(Debug, Clone)]
struct ParentType {
    path: Vec<String>,
    type_ref: Option<TypeRef>,
    relation: ParentRelation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentRelation {
    Superclass,
    ImplementedInterface,
    InterfaceParent,
}

#[derive(Debug, Clone)]
struct TypeAncestry {
    kind: TypeKind,
    name_span: Span,
    parent_declared: bool,
    parents: Vec<ParentType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AncestryStatus {
    Complete,
    Unknown,
}

#[derive(Debug, Clone)]
struct TypeAncestryResolution {
    status: AncestryStatus,
    parents: Vec<(Url, String)>,
}

#[derive(Debug, Clone)]
struct MemberLookup {
    candidates: Vec<Candidate>,
    ancestry_known: bool,
    ambiguous_names: HashSet<String>,
}

impl MemberLookup {
    fn known(candidates: Vec<Candidate>) -> Self {
        Self {
            candidates,
            ancestry_known: true,
            ambiguous_names: HashSet::new(),
        }
    }

    fn unknown(candidates: Vec<Candidate>) -> Self {
        Self {
            candidates,
            ancestry_known: false,
            ambiguous_names: HashSet::new(),
        }
    }

    fn ambiguous(candidates: Vec<Candidate>, ambiguous_names: HashSet<String>) -> Self {
        Self {
            candidates,
            ancestry_known: true,
            ambiguous_names,
        }
    }
}

struct AncestryResolutionState {
    remaining_work: usize,
    active_types: HashSet<(Url, String)>,
    resolved_types: HashMap<(Url, String), TypeAncestryResolution>,
    active_members: HashSet<(Url, String, usize, Option<String>, bool)>,
    resolved_members: HashMap<(Url, String, usize, Option<String>, bool), MemberLookup>,
}

impl AncestryResolutionState {
    fn new() -> Self {
        Self {
            remaining_work: MAX_ANCESTRY_WORK,
            active_types: HashSet::new(),
            resolved_types: HashMap::new(),
            active_members: HashSet::new(),
            resolved_members: HashMap::new(),
        }
    }

    fn take_work(&mut self) -> bool {
        let available = self.remaining_work > 0;
        if available {
            self.remaining_work -= 1;
        }
        available
    }
}

enum Receiver {
    Unit(Url),
    Type(TypeInstance),
    Builtin(BuiltinType),
    IntegerLiteral(i128),
}

fn type_instance_from_symbol(candidate: &Candidate, symbol: &Symbol) -> TypeInstance {
    TypeInstance {
        uri: candidate.uri.clone(),
        key: symbol.key.clone(),
        kind: symbol.type_kind,
        scope: symbol.scope,
        parameter_names: symbol
            .generic_parameters
            .iter()
            .map(|parameter| parameter.name.clone())
            .collect(),
        substitution: GenericSubstitution::empty(),
    }
}

fn type_identity_from_resolved_type(resolved: &ResolvedType) -> Option<TypeIdentity> {
    match resolved {
        ResolvedType::Builtin(builtin) => Some(TypeIdentity::Builtin(*builtin)),
        ResolvedType::IntegerLiteral(value) => Some(TypeIdentity::IntegerLiteral(*value)),
        ResolvedType::Named(instance) => Some(TypeIdentity::Named {
            uri: instance.uri.clone(),
            key: instance.key.clone(),
            kind: instance.kind,
            args: instance
                .parameter_names
                .iter()
                .map(|name| instance.substitution.get(name))
                .map(|value| value.and_then(type_identity_from_resolved_type))
                .collect::<Option<Vec<_>>>()?,
        }),
    }
}

fn resolved_type_from_receivers(receivers: Vec<Receiver>) -> Option<ResolvedType> {
    let mut result = None;
    for receiver in receivers {
        let resolved = match receiver {
            Receiver::Builtin(builtin) => ResolvedType::Builtin(builtin),
            Receiver::IntegerLiteral(value) => ResolvedType::IntegerLiteral(value),
            Receiver::Type(instance) => {
                let resolved = ResolvedType::Named(instance);
                type_identity_from_resolved_type(&resolved)?;
                resolved
            }
            Receiver::Unit(_) => return None,
        };
        if result.as_ref().is_some_and(|current| current != &resolved) {
            return None;
        }
        result = Some(resolved);
    }
    result
}

fn substitution_from_receivers(receivers: Vec<Receiver>) -> Option<GenericSubstitution> {
    let mut result = None;
    for receiver in receivers {
        let Receiver::Type(instance) = receiver else {
            return None;
        };
        if result
            .as_ref()
            .is_some_and(|current: &GenericSubstitution| current != &instance.substitution)
        {
            return None;
        }
        result = Some(instance.substitution);
    }
    result
}

fn unique_type_instance(receivers: Vec<Receiver>) -> Option<TypeInstance> {
    let mut result = None;
    for receiver in receivers {
        let Receiver::Type(instance) = receiver else {
            return None;
        };
        if result
            .as_ref()
            .is_some_and(|current: &TypeInstance| current != &instance)
        {
            return None;
        }
        result = Some(instance);
    }
    result
}

const MAX_RECEIVER_WORK: usize = 256;
const MAX_TYPE_RESOLUTION_WORK: usize = 256;
const MAX_RECEIVER_RECURSION_DEPTH: usize = 64;
const MAX_TYPE_REF_RECURSION_DEPTH: usize = 64;
const MAX_ANCESTRY_WORK: usize = 256;
const MAX_NAVIGATION_OVERLOAD_WORK: usize = 100_000;
const MAX_NAVIGATION_OVERLOAD_BYTES: usize = 8 * 1024 * 1024;

struct ResolutionState {
    receiver_work: usize,
    type_work: usize,
    active_members: HashSet<(Url, String, String, usize, GenericSubstitution)>,
    active_generic_constraints: HashSet<(Url, String)>,
    receiver_uncertain: bool,
}

impl ResolutionState {
    fn new() -> Self {
        Self {
            receiver_work: MAX_RECEIVER_WORK,
            type_work: MAX_TYPE_RESOLUTION_WORK,
            active_members: HashSet::new(),
            active_generic_constraints: HashSet::new(),
            receiver_uncertain: false,
        }
    }

    fn mark_receiver_uncertain(&mut self) {
        self.receiver_uncertain = true;
    }

    fn receiver_resolution_uncertain(&self) -> bool {
        self.receiver_uncertain
    }

    fn take_receiver_work(&mut self) -> bool {
        let available = self.receiver_work > 0;
        if available {
            self.receiver_work -= 1;
        }
        available
    }

    fn take_type_work(&mut self, amount: usize) -> bool {
        if amount > self.type_work {
            return false;
        }
        self.type_work -= amount;
        true
    }
}

struct Document {
    source: String,
    tree: Tree,
    parser_recovery_spans: Vec<Span>,
    unit_name: String,
    unit_display_name: String,
    interface_range: Option<Span>,
    implementation_range: Option<Span>,
    interface_uses: Vec<String>,
    implementation_uses: Vec<String>,
    imports: Vec<ImportMetadata>,
    unknown_imports: HashSet<String>,
    import_bindings: Option<HashMap<String, Url>>,
    interface_routine_keys: HashSet<String>,
    scopes: Vec<Scope>,
    cache_unsafe_scopes: HashSet<usize>,
    symbols: Vec<Symbol>,
    opaque_ranges: Vec<Span>,
    conditionals: ConditionalAnalysis,
    conditional_unknown_symbols: Vec<bool>,
    unknown_class_owners: HashSet<String>,
    known_non_class_owners: HashSet<String>,
    symbol_indices_by_scope_key: HashMap<(usize, String), Vec<usize>>,
    scope_symbol_indices: HashMap<usize, Vec<usize>>,
    member_symbol_indices: HashMap<(String, String), Vec<usize>>,
    member_symbol_indices_by_owner: HashMap<String, Vec<usize>>,
    type_ancestry: HashMap<String, Vec<TypeAncestry>>,
    type_symbol_indices: HashMap<String, Vec<usize>>,
    direct_symbol_indices: HashMap<Span, Vec<usize>>,
    routine_symbol_indices: HashMap<String, Vec<usize>>,
    routine_symbol_indices_by_body_scope: HashMap<usize, Vec<usize>>,
    exported_symbol_indices: Vec<usize>,
    routine_declaration_spans: HashMap<String, Span>,
    interface_member_routine_keys: HashMap<String, HashSet<String>>,
}

impl Document {
    fn parse_with_cancel(
        uri: Url,
        source: String,
        defines: &[String],
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        let path = uri
            .to_file_path()
            .unwrap_or_else(|_| PathBuf::from(uri.path()));
        let info = FileInfo::new(path);
        let conditionals = conditional::analyze_with_cancel(&source, defines, cancel);
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled".to_string());
        }
        let (tree, _diagnostics, patches) =
            parser::parse_file_with_patches(&info, conditionals.projected_source.as_bytes())?;
        let root = tree.root_node();
        let parser_recovery_spans = collect_parser_recovery_spans(root);
        let opaque_ranges = patches
            .into_iter()
            .filter_map(|patch| match patch {
                DirectivePatch::OpaqueBlock(block) => Some(Span {
                    start: block.start,
                    end: block.end,
                }),
                DirectivePatch::Markers(_) => None,
            })
            .collect();

        let mut module_names = Vec::new();
        let mut sections = Vec::new();
        collect_nodes(root, &mut |node| {
            if node.kind() == "moduleName" {
                module_names.push(node);
            } else if node.kind() == "interface" {
                sections.push((Region::Interface, Span::from_node(node)));
            } else if node.kind() == "implementation" {
                sections.push((Region::Implementation, Span::from_node(node)));
            }
        });

        let unit_module = module_names
            .iter()
            .copied()
            .find(|node| !has_ancestor_kind(*node, "declUses"));
        let unit_name = unit_module
            .map(|node| canonical_path(&identifier_texts(node, &source)))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| fallback_unit_name(&uri));
        let unit_display_name = unit_module
            .map(|node| identifier_texts(node, &source).join("."))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| unit_name.clone());

        let interface_range = sections
            .iter()
            .find_map(|(region, span)| (*region == Region::Interface).then_some(*span));
        let implementation_range = sections
            .iter()
            .find_map(|(region, span)| (*region == Region::Implementation).then_some(*span));

        let mut interface_uses = Vec::new();
        let mut implementation_uses = Vec::new();
        let mut imports = Vec::new();
        for module_name in &module_names {
            if !has_ancestor_kind(*module_name, "declUses") {
                continue;
            }
            let name = canonical_path(&identifier_texts(*module_name, &source));
            if name.is_empty() {
                continue;
            }
            imports.push(ImportMetadata {
                name: name.clone(),
                span: SourceSpan {
                    start: module_name.start_byte(),
                    end: module_name.end_byte(),
                },
            });
            match region_for_node(*module_name) {
                Region::Interface => interface_uses.push(name),
                Region::Implementation => implementation_uses.push(name),
                Region::Other => implementation_uses.push(name),
            }
        }
        interface_uses.sort();
        interface_uses.dedup();
        implementation_uses.sort();
        implementation_uses.dedup();
        let unknown_imports = imports
            .iter()
            .filter(|import| {
                conditionals
                    .unknown_spans
                    .iter()
                    .any(|span| span.start <= import.span.start && import.span.end <= span.end)
            })
            .map(|import| canonical_name(&import.name))
            .collect();

        let definitions = collect_nodes_matching(root, "defProc");
        let (scopes, scope_by_span) = build_scopes(source.len(), &definitions, &source);
        let cache_unsafe_scopes = cache_unsafe_scopes(root, &scope_by_span);
        let mut symbols = Vec::new();

        if let Some(module_name) = unit_module {
            let span = Span::from_node(module_name);
            let selection_span = identifier_nodes(module_name)
                .last()
                .map(|node| Span::from_node(*node))
                .unwrap_or(span);
            symbols.push(Symbol {
                span,
                declaration_span: Span::from_node(root),
                selection_span,
                name: node_text(module_name, &source),
                key: unit_name.clone(),
                kind: SymbolKind::Unit,
                type_kind: TypeKind::Other,
                routine_kind: RoutineKind::Procedure,
                routine_directives: RoutineDirectives::default(),
                parameter_mode: None,
                scope: ROOT_SCOPE,
                owner_type: None,
                owner_type_name: None,
                generic_parameters: Vec::new(),
                generic_parameter: None,
                type_name: None,
                type_ref: None,
                result_type_name: None,
                result_type_ref: None,
                result_type_span: None,
                region: Region::Other,
                origin: Origin::Declaration,
                local_only: false,
                routine_key: None,
                routine_signature: None,
                routine_header_span: None,
                routine_parameter_spans: Vec::new(),
                routine_parameters: Vec::new(),
                type_excerpt_end: None,
                body_scope: None,
                unresolved_abbreviated: false,
                accessor: None,
            });
        }

        collect_symbols(root, &source, &scopes, &scope_by_span, &mut symbols);
        pair_abbreviated_definitions(root, &source, &scope_by_span, &mut symbols);
        let conditional_unknown_symbols =
            conditional_unknown_symbols(root, &conditionals, &symbols);
        let type_ancestry = collect_type_ancestry(root, &source);
        let unknown_class_owners = symbols
            .iter()
            .filter(|symbol| {
                symbol.kind == SymbolKind::Type
                    && symbol.owner_type.is_none()
                    && symbol.generic_parameter.is_none()
                    && symbol.type_kind == TypeKind::Class
            })
            .map(|symbol| symbol.key.clone())
            .collect();
        let known_non_class_owners = symbols
            .iter()
            .enumerate()
            .filter(|(index, symbol)| {
                symbol.kind == SymbolKind::Type
                    && symbol.owner_type.is_none()
                    && symbol.generic_parameter.is_none()
                    && symbol.type_kind != TypeKind::Class
                    && !conditional_unknown_symbols[*index]
            })
            .map(|(_, symbol)| symbol.key.clone())
            .collect();
        let interface_routine_keys: HashSet<String> = symbols
            .iter()
            .filter_map(|symbol| {
                (symbol.kind == SymbolKind::Routine
                    && symbol.owner_type.is_none()
                    && !symbol.local_only
                    && symbol.origin == Origin::Declaration
                    && symbol.region == Region::Interface)
                    .then(|| symbol.routine_key.clone())
                    .flatten()
            })
            .collect();
        let exported_symbol_indices = symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| {
                if symbol.owner_type.is_some()
                    || symbol.kind == SymbolKind::Unit
                    || symbol.local_only
                    || symbol.generic_parameter.is_some()
                {
                    return false;
                }
                symbol.region == Region::Interface
                    || (symbol.kind == SymbolKind::Routine
                        && symbol.origin == Origin::Definition
                        && symbol
                            .routine_key
                            .as_ref()
                            .is_some_and(|key| interface_routine_keys.contains(key)))
            })
            .map(|(index, _)| index)
            .collect();
        let mut symbol_indices_by_scope_key = HashMap::new();
        let mut scope_symbol_indices = HashMap::new();
        let mut member_symbol_indices = HashMap::new();
        let mut member_symbol_indices_by_owner = HashMap::new();
        let mut type_symbol_indices = HashMap::new();
        let mut direct_symbol_indices = HashMap::new();
        let mut routine_symbol_indices = HashMap::new();
        let mut routine_symbol_indices_by_body_scope = HashMap::new();
        let mut routine_declaration_spans = HashMap::new();
        let mut interface_member_routine_keys = HashMap::<String, HashSet<String>>::new();
        for (index, symbol) in symbols.iter().enumerate() {
            symbol_indices_by_scope_key
                .entry((symbol.scope, symbol.key.clone()))
                .or_insert_with(Vec::new)
                .push(index);
            scope_symbol_indices
                .entry(symbol.scope)
                .or_insert_with(Vec::new)
                .push(index);
            direct_symbol_indices
                .entry(symbol.span)
                .or_insert_with(Vec::new)
                .push(index);
            if let Some(owner_type) = &symbol.owner_type {
                member_symbol_indices
                    .entry((owner_type.clone(), symbol.key.clone()))
                    .or_insert_with(Vec::new)
                    .push(index);
                member_symbol_indices_by_owner
                    .entry(owner_type.clone())
                    .or_insert_with(Vec::new)
                    .push(index);
                if symbol.kind == SymbolKind::Routine
                    && symbol.origin == Origin::Declaration
                    && symbol.region == Region::Interface
                {
                    if let Some(routine_key) = &symbol.routine_key {
                        interface_member_routine_keys
                            .entry(owner_type.clone())
                            .or_default()
                            .insert(routine_key.clone());
                    }
                }
            }
            if symbol.kind == SymbolKind::Type
                && symbol.scope == ROOT_SCOPE
                && symbol.owner_type.is_none()
                && symbol.generic_parameter.is_none()
            {
                type_symbol_indices
                    .entry(symbol.key.clone())
                    .or_insert_with(Vec::new)
                    .push(index);
            }
            if symbol.kind == SymbolKind::Routine {
                if let Some(routine_key) = &symbol.routine_key {
                    routine_symbol_indices
                        .entry(routine_key.clone())
                        .or_insert_with(Vec::new)
                        .push(index);
                    if symbol.origin == Origin::Declaration {
                        routine_declaration_spans
                            .entry(routine_key.clone())
                            .or_insert(symbol.declaration_span);
                    }
                }
                if let Some(body_scope) = symbol.body_scope {
                    routine_symbol_indices_by_body_scope
                        .entry(body_scope)
                        .or_insert_with(Vec::new)
                        .push(index);
                }
            }
        }

        Ok(Self {
            source,
            tree,
            parser_recovery_spans,
            unit_name,
            unit_display_name,
            interface_range,
            implementation_range,
            interface_uses,
            implementation_uses,
            imports,
            unknown_imports,
            import_bindings: None,
            interface_routine_keys,
            scopes,
            cache_unsafe_scopes,
            symbols,
            opaque_ranges,
            conditionals,
            conditional_unknown_symbols,
            unknown_class_owners,
            known_non_class_owners,
            symbol_indices_by_scope_key,
            scope_symbol_indices,
            member_symbol_indices,
            member_symbol_indices_by_owner,
            type_ancestry,
            type_symbol_indices,
            direct_symbol_indices,
            routine_symbol_indices,
            routine_symbol_indices_by_body_scope,
            exported_symbol_indices,
            routine_declaration_spans,
            interface_member_routine_keys,
        })
    }

    fn region_at(&self, offset: usize) -> Region {
        if self
            .interface_range
            .is_some_and(|range| range.contains_offset(offset))
        {
            Region::Interface
        } else if self
            .implementation_range
            .is_some_and(|range| range.contains_offset(offset))
        {
            Region::Implementation
        } else {
            Region::Other
        }
    }

    fn active_uses(&self, region: Region) -> Vec<&String> {
        match region {
            Region::Interface => self.interface_uses.iter().collect(),
            Region::Implementation | Region::Other => self
                .interface_uses
                .iter()
                .chain(self.implementation_uses.iter())
                .collect(),
        }
    }

    fn active_uses_with_budget<'a>(
        &'a self,
        region: Region,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<&'a String>, String> {
        let count = match region {
            Region::Interface => self.interface_uses.len(),
            Region::Implementation | Region::Other => self
                .interface_uses
                .len()
                .saturating_add(self.implementation_uses.len()),
        };
        budget.require_work(count, cancel)?;
        Ok(match region {
            Region::Interface => self.interface_uses.iter().collect(),
            Region::Implementation | Region::Other => self
                .interface_uses
                .iter()
                .chain(self.implementation_uses.iter())
                .collect(),
        })
    }

    fn scope_at(&self, offset: usize) -> usize {
        self.scopes
            .iter()
            .enumerate()
            .filter(|(_, scope)| {
                Span {
                    start: scope.start,
                    end: scope.end,
                }
                .contains_offset(offset)
            })
            .min_by_key(|(_, scope)| scope.end.saturating_sub(scope.start))
            .map_or(ROOT_SCOPE, |(index, _)| index)
    }

    fn scope_chain(&self, offset: usize) -> Vec<usize> {
        self.scope_chain_from(self.scope_at(offset))
    }

    fn scope_chain_from(&self, mut current: usize) -> Vec<usize> {
        let mut result = Vec::new();
        loop {
            result.push(current);
            let Some(parent) = self.scopes[current].parent else {
                break;
            };
            current = parent;
        }
        result
    }

    fn result_type_annotation_for_body_scope(&self, scope: usize) -> Option<ResultTypeAnnotation> {
        self.routine_symbol_indices_by_body_scope
            .get(&scope)
            .into_iter()
            .flatten()
            .find_map(|index| self.symbols.get(*index)?.result_type_annotation())
    }

    fn owner_type_at(&self, offset: usize) -> Option<String> {
        let scope = self.scope_at(offset);
        if let Some(owner_type) = self.owner_type_for_scope(scope) {
            return Some(owner_type);
        }
        #[cfg(test)]
        OWNER_TYPE_ROOT_LOOKUPS.with(|lookups| lookups.set(lookups.get() + 1));
        let identifier = identifier_at(self.tree.root_node(), offset)?;
        enclosing_type(identifier, &self.source)
    }

    fn owner_type_at_identifier(&self, identifier: Node<'_>, scope: usize) -> Option<String> {
        self.owner_type_for_scope(scope)
            .or_else(|| enclosing_type(identifier, &self.source))
    }

    fn owner_type_for_scope(&self, mut scope: usize) -> Option<String> {
        loop {
            if let Some(owner_type) = &self.scopes[scope].owner_type {
                return Some(owner_type.clone());
            }
            let Some(parent) = self.scopes[scope].parent else {
                break;
            };
            scope = parent;
        }
        None
    }

    fn has_parser_recovery_near(&self, span: Span) -> bool {
        self.parser_recovery_spans
            .iter()
            .any(|recovery| recovery.start <= span.end && recovery.end >= span.start)
    }
}

fn symbol_visible_in_region(symbol: &Symbol, region: Region) -> bool {
    match region {
        Region::Interface => symbol.region == Region::Interface,
        Region::Implementation => {
            symbol.region == Region::Interface || symbol.region == Region::Implementation
        }
        Region::Other => true,
    }
}

fn member_symbol_is_visible(
    document: &Document,
    symbol: &Symbol,
    type_key: &str,
    allow_implementation: bool,
) -> bool {
    if symbol.local_only || symbol.owner_type.as_deref() != Some(type_key) {
        return false;
    }
    match symbol.kind {
        SymbolKind::Routine => {
            (symbol.origin == Origin::Declaration
                && (symbol.region == Region::Interface
                    || (allow_implementation && symbol.region == Region::Implementation)))
                || (symbol.origin == Origin::Definition
                    && symbol.routine_key.as_ref().is_some_and(|key| {
                        document
                            .interface_member_routine_keys
                            .get(type_key)
                            .is_some_and(|keys| keys.contains(key))
                    }))
        }
        _ => {
            symbol.region == Region::Interface
                || (allow_implementation && symbol.region == Region::Implementation)
        }
    }
}

fn complete_ancestry(parents: Vec<(Url, String)>) -> TypeAncestryResolution {
    TypeAncestryResolution {
        status: AncestryStatus::Complete,
        parents,
    }
}

fn unknown_ancestry() -> TypeAncestryResolution {
    TypeAncestryResolution {
        status: AncestryStatus::Unknown,
        parents: Vec::new(),
    }
}

fn push_unique_candidate(candidates: &mut Vec<Candidate>, candidate: Candidate) {
    if !candidates
        .iter()
        .any(|current| current.uri == candidate.uri && current.index == candidate.index)
    {
        candidates.push(candidate);
    }
}

fn dedup_candidates(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut result = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        push_unique_candidate(&mut result, candidate);
    }
    result
}

fn candidate_sets_equal(left: &[Candidate], right: &[Candidate]) -> bool {
    left.len() == right.len()
        && left.iter().all(|candidate| {
            right
                .iter()
                .any(|other| candidate.uri == other.uri && candidate.index == other.index)
        })
}

fn collect_symbols(
    root: Node<'_>,
    source: &str,
    scopes: &[Scope],
    scope_by_span: &HashMap<Span, usize>,
    symbols: &mut Vec<Symbol>,
) {
    collect_nodes(root, &mut |node| match node.kind() {
        "defProc" => add_definition_symbol(node, source, scopes, scope_by_span, symbols),
        "declProc" if !is_definition_header(node) => {
            add_routine_symbol(node, source, scope_by_span, symbols);
        }
        "declType" => {
            add_named_symbol(node, source, scope_by_span, symbols, SymbolKind::Type, None)
        }
        "declVar" => {
            let type_name = node
                .child_by_field_name("type")
                .and_then(|type_node| simple_type_path(type_node, source));
            add_named_symbol(
                node,
                source,
                scope_by_span,
                symbols,
                SymbolKind::Variable,
                type_name,
            );
        }
        "declConst" => add_named_symbol(
            node,
            source,
            scope_by_span,
            symbols,
            SymbolKind::Constant,
            None,
        ),
        "declField" => {
            let type_name = node
                .child_by_field_name("type")
                .and_then(|type_node| simple_type_path(type_node, source));
            add_named_symbol(
                node,
                source,
                scope_by_span,
                symbols,
                SymbolKind::Field,
                type_name,
            );
        }
        "declProp" => {
            let type_name = node
                .child_by_field_name("type")
                .and_then(|type_node| simple_type_path(type_node, source));
            add_named_symbol(
                node,
                source,
                scope_by_span,
                symbols,
                SymbolKind::Property,
                type_name,
            );
        }
        "declArg" => {
            let type_name = node
                .child_by_field_name("type")
                .and_then(|type_node| simple_type_path(type_node, source));
            add_named_symbol(
                node,
                source,
                scope_by_span,
                symbols,
                SymbolKind::Parameter,
                type_name,
            );
        }
        "declEnumValue" => add_named_symbol(
            node,
            source,
            scope_by_span,
            symbols,
            SymbolKind::EnumValue,
            None,
        ),
        "declLabel" => add_named_symbol(
            node,
            source,
            scope_by_span,
            symbols,
            SymbolKind::Label,
            None,
        ),
        _ => {}
    });
}

fn pair_abbreviated_definitions(
    root: Node<'_>,
    source: &str,
    scope_by_span: &HashMap<Span, usize>,
    symbols: &mut Vec<Symbol>,
) {
    #[derive(Default)]
    struct DeclarationGroup {
        has_exact: bool,
        matching_count: usize,
        unique_matching: Option<usize>,
    }

    let mut declaration_nodes_by_key: HashMap<String, Vec<Node<'_>>> = HashMap::new();
    for node in collect_nodes_matching(root, "declProc") {
        if is_definition_header(node) || !is_routine_declaration_region(region_for_node(node)) {
            continue;
        }
        let Some((name, _, owner_type)) = routine_name(node, source) else {
            continue;
        };
        let signature = routine_signature(node, source);
        let routine_key = routine_key_with_owner(
            owner_type.as_deref(),
            &name,
            &signature,
            scope_for_declaration(node, scope_by_span),
        );
        declaration_nodes_by_key
            .entry(routine_key)
            .or_default()
            .push(node);
    }

    let mut declaration_groups: HashMap<(usize, String, Option<String>), DeclarationGroup> =
        HashMap::new();
    for (index, symbol) in symbols.iter().enumerate() {
        if symbol.kind == SymbolKind::Routine
            && symbol.origin == Origin::Declaration
            && is_routine_declaration_region(symbol.region)
        {
            let group = declaration_groups
                .entry((symbol.scope, symbol.key.clone(), symbol.owner_type.clone()))
                .or_default();
            if symbol.routine_signature.as_deref() == Some("") {
                group.has_exact = true;
            } else if symbol.routine_signature.is_some() {
                group.matching_count += 1;
                if group.matching_count == 1 {
                    group.unique_matching = Some(index);
                }
            }
        }
    }

    let abbreviated_definitions: Vec<usize> = symbols
        .iter()
        .enumerate()
        .filter_map(|(index, symbol)| {
            (symbol.kind == SymbolKind::Routine
                && symbol.origin == Origin::Definition
                && symbol.routine_signature.as_deref() == Some(""))
            .then_some(index)
        })
        .collect();

    for definition_index in abbreviated_definitions {
        let definition_scope = symbols[definition_index].scope;
        let definition_key = symbols[definition_index].key.clone();
        let definition_owner = symbols[definition_index].owner_type.clone();
        let group_key = (definition_scope, definition_key, definition_owner);
        let Some(group) = declaration_groups.get(&group_key) else {
            continue;
        };
        if group.has_exact {
            continue;
        }
        if group.matching_count != 1 {
            if group.matching_count > 1 {
                symbols[definition_index].unresolved_abbreviated = true;
            }
            continue;
        }
        let Some(declaration_index) = group.unique_matching else {
            continue;
        };

        let Some(routine_key) = symbols[declaration_index].routine_key.clone() else {
            continue;
        };
        let body_scope = symbols[definition_index].body_scope;
        symbols[definition_index].routine_key = Some(routine_key.clone());
        symbols[definition_index].routine_directives =
            symbols[declaration_index].routine_directives;
        symbols[definition_index].result_type_name =
            symbols[declaration_index].result_type_name.clone();
        symbols[definition_index].result_type_ref =
            symbols[declaration_index].result_type_ref.clone();
        symbols[definition_index].result_type_span = symbols[declaration_index].result_type_span;
        symbols[definition_index].generic_parameters =
            symbols[declaration_index].generic_parameters.clone();
        let declaration_node = declaration_nodes_by_key
            .get(&routine_key)
            .and_then(|nodes| nodes.first().copied());
        let Some(body_scope) = body_scope else {
            continue;
        };
        if let Some(declaration_node) = declaration_node {
            inject_abbreviated_parameters(declaration_node, body_scope, source, symbols);
        }
    }

    let mut declarations_by_routine_key: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, symbol) in symbols.iter().enumerate() {
        if symbol.kind == SymbolKind::Routine && symbol.origin == Origin::Declaration {
            if let Some(routine_key) = symbol.routine_key.as_ref() {
                declarations_by_routine_key
                    .entry(routine_key.clone())
                    .or_default()
                    .push(index);
            }
        }
    }
    let definition_indices: Vec<usize> = symbols
        .iter()
        .enumerate()
        .filter_map(|(index, symbol)| {
            (symbol.kind == SymbolKind::Routine && symbol.origin == Origin::Definition)
                .then_some(index)
        })
        .collect();
    for definition_index in definition_indices {
        let Some(routine_key) = symbols[definition_index].routine_key.as_ref() else {
            continue;
        };
        let Some(declarations) = declarations_by_routine_key.get(routine_key) else {
            continue;
        };
        let Some(&declaration_index) = declarations.first().filter(|_| declarations.len() == 1)
        else {
            continue;
        };
        symbols[definition_index].result_type_name =
            symbols[declaration_index].result_type_name.clone();
        symbols[definition_index].result_type_ref =
            symbols[declaration_index].result_type_ref.clone();
        symbols[definition_index].result_type_span = symbols[declaration_index].result_type_span;
        symbols[definition_index].generic_parameters =
            symbols[declaration_index].generic_parameters.clone();
        symbols[definition_index].routine_directives =
            symbols[declaration_index].routine_directives;
    }
}

fn inject_abbreviated_parameters(
    declaration: Node<'_>,
    body_scope: usize,
    source: &str,
    symbols: &mut Vec<Symbol>,
) {
    let Some(arguments) = declaration.child_by_field_name("args") else {
        return;
    };
    collect_nodes(arguments, &mut |node| {
        if node.kind() != "declArg" {
            return;
        }
        let type_name = node
            .child_by_field_name("type")
            .and_then(|type_node| simple_type_path(type_node, source));
        let type_ref = node
            .child_by_field_name("type")
            .and_then(|type_node| type_ref_from_node(type_node, source));
        for identifier in field_identifier_nodes(node, "name") {
            let name = node_text(identifier, source);
            if name.is_empty() {
                continue;
            }
            symbols.push(Symbol {
                span: Span::from_node(identifier),
                declaration_span: Span::from_node(node),
                selection_span: Span::from_node(identifier),
                name: name.clone(),
                key: canonical_name(&name),
                kind: SymbolKind::Parameter,
                type_kind: TypeKind::Other,
                routine_kind: RoutineKind::Procedure,
                routine_directives: RoutineDirectives::default(),
                parameter_mode: Some(parameter_mode(node)),
                scope: body_scope,
                owner_type: None,
                owner_type_name: None,
                generic_parameters: Vec::new(),
                generic_parameter: None,
                type_name: type_name.clone(),
                type_ref: type_ref.clone(),
                result_type_name: None,
                result_type_ref: None,
                result_type_span: None,
                region: Region::Implementation,
                origin: Origin::Declaration,
                local_only: false,
                routine_key: None,
                routine_signature: None,
                routine_header_span: None,
                routine_parameter_spans: Vec::new(),
                routine_parameters: Vec::new(),
                type_excerpt_end: None,
                body_scope: None,
                unresolved_abbreviated: false,
                accessor: None,
            });
        }
    });
}

fn add_definition_symbol(
    node: Node<'_>,
    source: &str,
    scopes: &[Scope],
    scope_by_span: &HashMap<Span, usize>,
    symbols: &mut Vec<Symbol>,
) {
    let Some(header) = node.child_by_field_name("header") else {
        return;
    };
    let Some((name, span, owner_type)) = routine_name(header, source) else {
        return;
    };
    let owner_type_name = routine_owner_name(header, source);
    let own_scope = scope_by_span
        .get(&Span::from_node(node))
        .copied()
        .unwrap_or(ROOT_SCOPE);
    let scope = scopes[own_scope].parent.unwrap_or(ROOT_SCOPE);
    let signature = routine_signature(header, source);
    let result_type = routine_result_type(header, source);
    let result_type_ref = header
        .child_by_field_name("type")
        .and_then(|type_node| type_ref_from_node(type_node, source));
    let generic_parameters = generic_parameters_for_node(header, source);
    let routine_parameters = direct_routine_parameters(header, source);
    let routine_key = routine_key_with_owner(owner_type.as_deref(), &name, &signature, scope);
    symbols.push(Symbol {
        span,
        declaration_span: Span::from_node(node),
        selection_span: span,
        name: name.clone(),
        key: canonical_name(&name),
        kind: SymbolKind::Routine,
        type_kind: TypeKind::Other,
        routine_kind: routine_kind(header),
        routine_directives: routine_directives(header),
        parameter_mode: None,
        scope,
        owner_type: owner_type.clone(),
        owner_type_name,
        generic_parameters: generic_parameters.clone(),
        generic_parameter: None,
        type_name: None,
        type_ref: None,
        result_type_name: result_type.as_ref().map(|(name, _)| name.clone()),
        result_type_ref,
        result_type_span: result_type.map(|(_, span)| span),
        region: region_for_node(node),
        origin: Origin::Definition,
        local_only: false,
        routine_key: Some(routine_key.clone()),
        routine_signature: Some(signature),
        routine_header_span: Some(Span::from_node(header)),
        routine_parameter_spans: routine_parameters
            .iter()
            .map(|parameter| parameter.span)
            .collect(),
        routine_parameters,
        type_excerpt_end: None,
        body_scope: Some(own_scope),
        unresolved_abbreviated: false,
        accessor: None,
    });
    push_routine_generic_parameter_symbols(
        symbols,
        &generic_parameters,
        &routine_key,
        scope,
        region_for_node(node),
    );
}

fn add_routine_symbol(
    node: Node<'_>,
    source: &str,
    scope_by_span: &HashMap<Span, usize>,
    symbols: &mut Vec<Symbol>,
) {
    let Some((name, span, owner_type)) = routine_name(node, source) else {
        return;
    };
    let owner_type_name = routine_owner_name(node, source);
    let scope = scope_for_declaration(node, scope_by_span);
    let signature = routine_signature(node, source);
    let result_type = routine_result_type(node, source);
    let result_type_ref = node
        .child_by_field_name("type")
        .and_then(|type_node| type_ref_from_node(type_node, source));
    let generic_parameters = generic_parameters_for_node(node, source);
    let routine_parameters = direct_routine_parameters(node, source);
    let routine_key = routine_key_with_owner(owner_type.as_deref(), &name, &signature, scope);
    symbols.push(Symbol {
        span,
        declaration_span: Span::from_node(node),
        selection_span: span,
        name: name.clone(),
        key: canonical_name(&name),
        kind: SymbolKind::Routine,
        type_kind: TypeKind::Other,
        routine_kind: routine_kind(node),
        routine_directives: routine_directives(node),
        parameter_mode: None,
        scope,
        owner_type: owner_type.clone(),
        owner_type_name,
        generic_parameters: generic_parameters.clone(),
        generic_parameter: None,
        type_name: None,
        type_ref: None,
        result_type_name: result_type.as_ref().map(|(name, _)| name.clone()),
        result_type_ref,
        result_type_span: result_type.map(|(_, span)| span),
        region: region_for_node(node),
        origin: Origin::Declaration,
        local_only: false,
        routine_key: Some(routine_key.clone()),
        routine_signature: Some(signature),
        routine_header_span: Some(Span::from_node(node)),
        routine_parameter_spans: routine_parameters
            .iter()
            .map(|parameter| parameter.span)
            .collect(),
        routine_parameters,
        type_excerpt_end: None,
        body_scope: None,
        unresolved_abbreviated: false,
        accessor: None,
    });
    push_routine_generic_parameter_symbols(
        symbols,
        &generic_parameters,
        &routine_key,
        scope,
        region_for_node(node),
    );
}

fn push_routine_generic_parameter_symbols(
    symbols: &mut Vec<Symbol>,
    parameters: &[GenericParameter],
    routine_key: &str,
    scope: usize,
    region: Region,
) {
    for parameter in parameters {
        symbols.push(Symbol {
            span: parameter.span,
            declaration_span: parameter.span,
            selection_span: parameter.span,
            name: parameter.name.clone(),
            key: parameter.name.clone(),
            kind: SymbolKind::Type,
            type_kind: TypeKind::Other,
            routine_kind: RoutineKind::Procedure,
            routine_directives: RoutineDirectives::default(),
            parameter_mode: None,
            scope,
            owner_type: None,
            owner_type_name: None,
            generic_parameters: Vec::new(),
            generic_parameter: Some(parameter.name.clone()),
            type_name: parameter.constraint.as_ref().map(TypeRef::display),
            type_ref: parameter.constraint.clone(),
            result_type_name: None,
            result_type_ref: None,
            result_type_span: None,
            region,
            origin: Origin::Declaration,
            local_only: false,
            routine_key: Some(routine_key.to_owned()),
            routine_signature: None,
            routine_header_span: None,
            routine_parameter_spans: Vec::new(),
            routine_parameters: Vec::new(),
            type_excerpt_end: None,
            body_scope: None,
            unresolved_abbreviated: false,
            accessor: None,
        });
    }
}

fn add_named_symbol(
    node: Node<'_>,
    source: &str,
    scope_by_span: &HashMap<Span, usize>,
    symbols: &mut Vec<Symbol>,
    kind: SymbolKind,
    type_name: Option<String>,
) {
    let owner_type = enclosing_type(node, source);
    let owner_type_name = enclosing_type_name(node, source);
    let scope = scope_for_declaration(node, scope_by_span);
    let local_only = kind == SymbolKind::Parameter && scope == ROOT_SCOPE;
    let type_kind = if kind == SymbolKind::Type {
        type_kind_for_declaration(node)
    } else {
        TypeKind::Other
    };
    let type_excerpt_end = if kind == SymbolKind::Type {
        type_declaration_excerpt_end(node, type_kind)
    } else {
        None
    };
    let accessor = if kind == SymbolKind::Property {
        property_accessor(node, source)
    } else {
        None
    };
    let generic_parameters = if kind == SymbolKind::Type {
        generic_parameters_for_node(node, source)
    } else {
        Vec::new()
    };
    let type_ref = node
        .child_by_field_name("type")
        .and_then(|type_node| type_ref_from_node(type_node, source));
    let identifiers = if kind == SymbolKind::Type {
        declaration_name_identifiers(node)
    } else {
        field_identifier_nodes(node, "name")
    };
    let declared_type_key = (kind == SymbolKind::Type)
        .then(|| identifiers.last())
        .flatten()
        .map(|identifier| canonical_name(&node_text(*identifier, source)));
    for identifier in identifiers {
        let name = node_text(identifier, source);
        if name.is_empty() {
            continue;
        }
        symbols.push(Symbol {
            span: Span::from_node(identifier),
            declaration_span: Span::from_node(node),
            selection_span: Span::from_node(identifier),
            name: name.clone(),
            key: canonical_name(&name),
            kind,
            type_kind,
            routine_kind: RoutineKind::Procedure,
            routine_directives: RoutineDirectives::default(),
            parameter_mode: (kind == SymbolKind::Parameter).then(|| parameter_mode(node)),
            scope,
            owner_type: owner_type.clone(),
            owner_type_name: owner_type_name.clone(),
            generic_parameters: generic_parameters.clone(),
            generic_parameter: None,
            type_name: type_name.clone(),
            type_ref: type_ref.clone(),
            result_type_name: None,
            result_type_ref: None,
            result_type_span: None,
            region: region_for_node(node),
            origin: Origin::Declaration,
            local_only,
            routine_key: None,
            routine_signature: None,
            routine_header_span: None,
            routine_parameter_spans: Vec::new(),
            routine_parameters: Vec::new(),
            type_excerpt_end,
            body_scope: None,
            unresolved_abbreviated: false,
            accessor: accessor.clone(),
        });
    }

    if kind == SymbolKind::Type {
        for parameter in generic_parameters {
            symbols.push(Symbol {
                span: parameter.span,
                declaration_span: parameter.span,
                selection_span: parameter.span,
                name: parameter.name.clone(),
                key: parameter.name.clone(),
                kind: SymbolKind::Type,
                type_kind: TypeKind::Other,
                routine_kind: RoutineKind::Procedure,
                routine_directives: RoutineDirectives::default(),
                parameter_mode: None,
                scope,
                owner_type: declared_type_key.clone(),
                owner_type_name: declared_type_key.clone(),
                generic_parameters: Vec::new(),
                generic_parameter: Some(parameter.name),
                type_name: parameter.constraint.as_ref().map(TypeRef::display),
                type_ref: parameter.constraint,
                result_type_name: None,
                result_type_ref: None,
                result_type_span: None,
                region: region_for_node(node),
                origin: Origin::Declaration,
                local_only: false,
                routine_key: None,
                routine_signature: None,
                routine_header_span: None,
                routine_parameter_spans: Vec::new(),
                routine_parameters: Vec::new(),
                type_excerpt_end: None,
                body_scope: None,
                unresolved_abbreviated: false,
                accessor: None,
            });
        }
    }
}

fn type_declaration_excerpt_end(node: Node<'_>, type_kind: TypeKind) -> Option<usize> {
    if !matches!(
        type_kind,
        TypeKind::Class | TypeKind::Record | TypeKind::Interface
    ) {
        return None;
    }
    let type_node = node.child_by_field_name("type")?;
    let mut body_start: Option<usize> = None;
    collect_nodes(type_node, &mut |child| {
        if matches!(
            child.kind(),
            "declSection"
                | "declField"
                | "declProc"
                | "declProp"
                | "declTypes"
                | "declVariant"
                | "declEnum"
        ) {
            body_start =
                Some(body_start.map_or(child.start_byte(), |start| start.min(child.start_byte())));
        }
    });
    Some(body_start.unwrap_or_else(|| type_node.end_byte()))
}

fn property_accessor(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("getter")
        .or_else(|| node.child_by_field_name("setter"))
        .map(|accessor| node_text(accessor, source))
        .filter(|accessor| !accessor.is_empty())
}

fn type_kind_for_declaration(node: Node<'_>) -> TypeKind {
    let Some(type_node) = node.child_by_field_name("type") else {
        return TypeKind::Other;
    };
    let mut pending = vec![type_node];
    while let Some(current) = pending.pop() {
        match current.kind() {
            "declIntf" => return TypeKind::Interface,
            "declEnum" => return TypeKind::Enum,
            "declRecord" => return TypeKind::Record,
            "declArray" | "kArray" | "declSet" => return TypeKind::Array,
            "declProcRef" | "kProcedure" | "kFunction" => return TypeKind::Callable,
            "declString" | "kString" => return TypeKind::String,
            "declFile" | "kFile" => return TypeKind::File,
            "declMetaClass" => return TypeKind::Class,
            "declClass" => {
                let mut cursor = current.walk();
                return current
                    .children(&mut cursor)
                    .find_map(|child| match child.kind() {
                        "kRecord" => Some(TypeKind::Record),
                        "kClass" | "kObject" => Some(TypeKind::Class),
                        _ => None,
                    })
                    .unwrap_or(TypeKind::Class);
            }
            _ => {
                let mut cursor = current.walk();
                let children: Vec<_> = current.children(&mut cursor).collect();
                pending.extend(children.into_iter().rev());
            }
        }
    }
    TypeKind::Other
}

fn collect_type_ancestry(root: Node<'_>, source: &str) -> HashMap<String, Vec<TypeAncestry>> {
    let mut ancestry = HashMap::new();
    for declaration in collect_nodes_matching(root, "declType") {
        if enclosing_type(declaration, source).is_some() {
            continue;
        }
        let Some(name) = declaration_name_identifiers(declaration).last().copied() else {
            continue;
        };
        let type_kind = type_kind_for_declaration(declaration);
        let Some(type_node) = declaration.child_by_field_name("type") else {
            continue;
        };
        let Some(shape) = type_shape_node(type_node, type_kind) else {
            ancestry
                .entry(canonical_name(&node_text(name, source)))
                .or_insert_with(Vec::new)
                .push(TypeAncestry {
                    kind: type_kind,
                    name_span: Span::from_node(name),
                    parent_declared: false,
                    parents: Vec::new(),
                });
            continue;
        };

        let parent_fields = {
            let mut cursor = shape.walk();
            shape
                .children_by_field_name("parent", &mut cursor)
                .collect::<Vec<_>>()
        };
        let parent_declared = !parent_fields.is_empty();
        let mut parent_spans = HashSet::new();
        let mut parents = Vec::new();
        let mut parent_position = 0usize;
        for field in parent_fields {
            let fields = if field.kind() == "typeref" {
                vec![field]
            } else {
                let mut cursor = field.walk();
                field
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "typeref")
                    .collect()
            };
            for parent in fields {
                let relation = match type_kind {
                    TypeKind::Class => {
                        if parent_position == 0 {
                            ParentRelation::Superclass
                        } else {
                            ParentRelation::ImplementedInterface
                        }
                    }
                    TypeKind::Interface => ParentRelation::InterfaceParent,
                    _ => ParentRelation::ImplementedInterface,
                };
                parent_position = parent_position.saturating_add(1);
                let span = Span::from_node(parent);
                if !parent_spans.insert(span) {
                    continue;
                }
                let type_ref = type_ref_from_node(parent, source);
                let path = type_ref
                    .as_ref()
                    .map(|type_ref| type_ref.path.clone())
                    .unwrap_or_default();
                parents.push(ParentType {
                    path,
                    type_ref,
                    relation,
                });
            }
        }

        ancestry
            .entry(canonical_name(&node_text(name, source)))
            .or_insert_with(Vec::new)
            .push(TypeAncestry {
                kind: type_kind,
                name_span: Span::from_node(name),
                parent_declared,
                parents,
            });
    }
    ancestry
}

fn type_shape_node(node: Node<'_>, type_kind: TypeKind) -> Option<Node<'_>> {
    let wanted = match type_kind {
        TypeKind::Class | TypeKind::Record => "declClass",
        TypeKind::Interface => "declIntf",
        _ => return None,
    };
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        if current.kind() == wanted {
            return Some(current);
        }
        let mut cursor = current.walk();
        let children: Vec<_> = current.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    None
}

fn routine_kind(node: Node<'_>) -> RoutineKind {
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        match current.kind() {
            "kFunction" => return RoutineKind::Function,
            "kConstructor" => return RoutineKind::Constructor,
            "kDestructor" => return RoutineKind::Destructor,
            "kOperator" => return RoutineKind::Operator,
            "kProcedure" => return RoutineKind::Procedure,
            _ => {
                let mut cursor = current.walk();
                let children: Vec<_> = current.children(&mut cursor).collect();
                pending.extend(children.into_iter().rev());
            }
        }
    }
    RoutineKind::Procedure
}

fn routine_directives(node: Node<'_>) -> RoutineDirectives {
    let mut directives = RoutineDirectives::default();
    collect_nodes(node, &mut |child| match child.kind() {
        "kOverload" => directives.overload = true,
        "kVirtual" => directives.virtual_ = true,
        "kDynamic" => directives.dynamic = true,
        "kOverride" => directives.override_ = true,
        "kReintroduce" => directives.reintroduce = true,
        _ => {}
    });
    directives
}

fn routine_result_type(node: Node<'_>, source: &str) -> Option<(String, Span)> {
    node.child_by_field_name("type").and_then(|type_node| {
        simple_type_path(type_node, source).map(|name| (name, Span::from_node(type_node)))
    })
}

fn routine_name(node: Node<'_>, source: &str) -> Option<(String, Span, Option<String>)> {
    let identifiers = declaration_name_identifiers(node);
    let last = identifiers.last().copied()?;
    let name = node_text(last, source);
    let owner_type = if identifiers.len() > 1 {
        identifiers
            .get(identifiers.len().saturating_sub(2))
            .map(|identifier| canonical_name(&node_text(*identifier, source)))
    } else {
        enclosing_type(node, source)
    };
    Some((name, Span::from_node(last), owner_type))
}

fn routine_owner_name(node: Node<'_>, source: &str) -> Option<String> {
    let identifiers = declaration_name_identifiers(node);
    if identifiers.len() > 1 {
        return identifiers
            .get(identifiers.len().saturating_sub(2))
            .map(|identifier| node_text(*identifier, source));
    }
    enclosing_type_name(node, source)
}

fn routine_signature(node: Node<'_>, source: &str) -> String {
    let Some(arguments) = node.child_by_field_name("args") else {
        return String::new();
    };
    let mut types = Vec::new();
    for child in direct_routine_argument_groups(arguments) {
        let count = field_identifier_nodes(child, "name").len().max(1);
        let mode = match parameter_mode(child) {
            ParameterMode::Value => "",
            ParameterMode::Var => "var ",
            ParameterMode::Out => "out ",
            ParameterMode::Const => "const ",
            ParameterMode::ConstRef => "constref ",
        };
        let type_name = child
            .child_by_field_name("type")
            .and_then(|type_node| simple_type_path(type_node, source))
            .unwrap_or_else(|| "?".to_string());
        for _ in 0..count {
            types.push(format!("{mode}{type_name}"));
        }
    }
    types.join(",")
}

fn direct_routine_argument_groups(arguments: Node<'_>) -> Vec<Node<'_>> {
    (0..arguments.named_child_count())
        .filter_map(|index| arguments.named_child(index))
        .filter(|child| child.kind() == "declArg")
        .collect()
}

fn direct_routine_parameters(node: Node<'_>, source: &str) -> Vec<RoutineParameter> {
    let Some(arguments) = node.child_by_field_name("args") else {
        return Vec::new();
    };
    direct_routine_argument_groups(arguments)
        .into_iter()
        .flat_map(|group| {
            let type_node = group.child_by_field_name("type");
            let type_span = type_node.map(Span::from_node);
            let type_name = type_node.and_then(|node| simple_type_path(node, source));
            let type_ref = type_node.and_then(|node| type_ref_from_node(node, source));
            let mode = parameter_mode(group);
            let has_default = group.child_by_field_name("defaultValue").is_some();
            field_identifier_nodes(group, "name")
                .into_iter()
                .map(move |identifier| RoutineParameter {
                    span: Span::from_node(identifier),
                    type_span,
                    type_name: type_name.clone(),
                    type_ref: type_ref.clone(),
                    mode,
                    has_default,
                })
        })
        .collect()
}

fn parameter_mode(node: Node<'_>) -> ParameterMode {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "kVar" => return ParameterMode::Var,
            "kOut" => return ParameterMode::Out,
            "kConstref" => return ParameterMode::ConstRef,
            "kConst" => return ParameterMode::Const,
            _ => {}
        }
    }
    ParameterMode::Value
}

fn routine_key_with_owner(
    owner_type: Option<&str>,
    name: &str,
    signature: &str,
    scope: usize,
) -> String {
    format!(
        "{}::{}({})@{}",
        owner_type.unwrap_or_default(),
        canonical_name(name),
        signature,
        scope,
    )
}

fn enclosing_type(node: Node<'_>, source: &str) -> Option<String> {
    let node_span = Span::from_node(node);
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "declType" {
            let Some(type_node) = parent.child_by_field_name("type") else {
                current = parent.parent();
                continue;
            };
            if Span::from_node(type_node).contains(node_span) {
                let identifiers = declaration_name_identifiers(parent);
                return identifiers
                    .last()
                    .map(|identifier| canonical_name(&node_text(*identifier, source)));
            }
        }
        current = parent.parent();
    }
    None
}

fn enclosing_type_name(node: Node<'_>, source: &str) -> Option<String> {
    let node_span = Span::from_node(node);
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "declType"
            && parent
                .child_by_field_name("type")
                .is_some_and(|type_node| Span::from_node(type_node).contains(node_span))
        {
            return declaration_name_identifiers(parent)
                .last()
                .map(|identifier| node_text(*identifier, source));
        }
        current = parent.parent();
    }
    None
}

fn simple_type_path(node: Node<'_>, source: &str) -> Option<String> {
    type_ref_from_node(node, source).map(|type_ref| type_ref.display())
}

fn type_ref_from_node(node: Node<'_>, source: &str) -> Option<TypeRef> {
    type_ref_from_node_at_depth(node, source, 0)
}

fn type_ref_from_node_at_depth(node: Node<'_>, source: &str, depth: usize) -> Option<TypeRef> {
    if depth >= MAX_TYPE_REF_RECURSION_DEPTH {
        return None;
    }
    let mut type_node = node;
    while matches!(type_node.kind(), "type" | "typeref") {
        type_node = first_named_child(type_node)?;
    }
    match type_node.kind() {
        "identifier" => Some(TypeRef {
            path: vec![canonical_name(&node_text(type_node, source))],
            args: Vec::new(),
            span: Span::from_node(node),
        }),
        "typerefDot" => {
            let lhs = type_node.child_by_field_name("lhs")?;
            let rhs = type_node.child_by_field_name("rhs")?;
            let next_depth = depth.saturating_add(1);
            let mut left = type_ref_from_node_at_depth(lhs, source, next_depth)?;
            let right = type_ref_from_node_at_depth(rhs, source, next_depth)?;
            if !right.args.is_empty() {
                left.args = right.args;
            }
            left.path.extend(right.path);
            left.span = Span::from_node(node);
            Some(left)
        }
        "typerefTpl" | "exprTpl" => {
            let entity = type_node.child_by_field_name("entity")?;
            let next_depth = depth.saturating_add(1);
            let mut result = type_ref_from_node_at_depth(entity, source, next_depth)?;
            let args_node = type_node.child_by_field_name("args")?;
            let args = if matches!(args_node.kind(), "genericArgs" | "typerefArgs" | "exprArgs") {
                (0..args_node.named_child_count())
                    .filter_map(|index| args_node.named_child(index))
                    .filter_map(|arg| type_ref_from_node_at_depth(arg, source, next_depth))
                    .collect::<Vec<_>>()
            } else {
                type_ref_from_node_at_depth(args_node, source, next_depth)
                    .into_iter()
                    .collect()
            };
            if args.is_empty() {
                return None;
            }
            result.args = args;
            result.span = Span::from_node(node);
            Some(result)
        }
        "declString" => Some(TypeRef {
            path: vec!["string".to_owned()],
            args: Vec::new(),
            span: Span::from_node(node),
        }),
        _ => None,
    }
}

fn declaration_name_identifiers(node: Node<'_>) -> Vec<Node<'_>> {
    let Some(name) = node.child_by_field_name("name") else {
        return Vec::new();
    };
    let mut result = Vec::new();
    collect_name_identifiers(name, &mut result);
    result
}

fn collect_name_identifiers<'a>(node: Node<'a>, result: &mut Vec<Node<'a>>) {
    match node.kind() {
        "identifier" => result.push(node),
        "genericTpl" => {
            if let Some(entity) = node.child_by_field_name("entity") {
                collect_name_identifiers(entity, result);
            }
        }
        "genericDot" => {
            if let Some(lhs) = node.child_by_field_name("lhs") {
                collect_name_identifiers(lhs, result);
            }
            if let Some(rhs) = node.child_by_field_name("rhs") {
                collect_name_identifiers(rhs, result);
            }
        }
        _ => {}
    }
}

fn generic_template_for_name(node: Node<'_>) -> Option<Node<'_>> {
    let name = node.child_by_field_name("name")?;
    generic_template_in_name(name)
}

fn generic_template_in_name(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "genericTpl" => Some(node),
        "genericDot" => node
            .child_by_field_name("rhs")
            .and_then(generic_template_in_name),
        _ => None,
    }
}

fn generic_parameters_for_node(node: Node<'_>, source: &str) -> Vec<GenericParameter> {
    let Some(template) = generic_template_for_name(node) else {
        return Vec::new();
    };
    let Some(arguments) = template.child_by_field_name("args") else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for index in 0..arguments.named_child_count() {
        let Some(argument) = arguments.named_child(index) else {
            continue;
        };
        if argument.kind() != "genericArgs" && argument.kind() != "genericArg" {
            continue;
        }
        let groups = if argument.kind() == "genericArgs" {
            (0..argument.named_child_count())
                .filter_map(|child_index| argument.named_child(child_index))
                .collect::<Vec<_>>()
        } else {
            vec![argument]
        };
        for group in groups {
            if group.kind() != "genericArg" {
                continue;
            }
            let constraint_node = generic_constraint_type_node(group);
            let constraint =
                constraint_node.and_then(|type_node| type_ref_from_node(type_node, source));
            let constraint_unsupported = constraint_node.is_some() && constraint.is_none();
            for identifier in field_identifier_nodes(group, "name") {
                result.push(GenericParameter {
                    name: canonical_name(&node_text(identifier, source)),
                    span: Span::from_node(identifier),
                    constraint: constraint.clone(),
                    constraint_unsupported,
                });
            }
        }
    }
    result
}

fn generic_constraint_type_node(node: Node<'_>) -> Option<Node<'_>> {
    if let Some(type_node) = node
        .child_by_field_name("type")
        .filter(|type_node| type_node.kind() != ":")
    {
        return Some(type_node);
    }
    (0..node.named_child_count())
        .filter_map(|index| node.named_child(index))
        .find(|child| {
            matches!(
                child.kind(),
                "typeref" | "typerefDot" | "typerefTpl" | "typerefPtr" | "ppFragmentExpr"
            )
        })
}

fn build_scopes(
    source_len: usize,
    definitions: &[Node<'_>],
    source: &str,
) -> (Vec<Scope>, HashMap<Span, usize>) {
    let mut seeds: Vec<(Span, Option<String>)> = definitions
        .iter()
        .map(|node| {
            let owner_type = node
                .child_by_field_name("header")
                .and_then(|header| routine_name(header, source))
                .and_then(|(_, _, owner)| owner);
            (Span::from_node(*node), owner_type)
        })
        .collect();
    seeds.sort_by_key(|(span, _)| (span.start, std::cmp::Reverse(span.end)));

    let mut scopes = vec![Scope {
        start: 0,
        end: source_len,
        parent: None,
        owner_type: None,
    }];
    let mut open_scopes = vec![ROOT_SCOPE];
    let mut scope_by_span = HashMap::new();
    for (span, owner_type) in seeds {
        while let Some(&open_scope) = open_scopes.last() {
            let open_span = Span {
                start: scopes[open_scope].start,
                end: scopes[open_scope].end,
            };
            if open_span.contains(span) && open_span != span {
                break;
            }
            open_scopes.pop();
        }
        let parent = open_scopes.last().copied().unwrap_or(ROOT_SCOPE);
        let scope = scopes.len();
        scopes.push(Scope {
            start: span.start,
            end: span.end,
            parent: Some(parent),
            owner_type,
        });
        scope_by_span.insert(span, scope);
        open_scopes.push(scope);
    }
    (scopes, scope_by_span)
}

fn cache_unsafe_scopes(root: Node<'_>, scope_by_span: &HashMap<Span, usize>) -> HashSet<usize> {
    let mut inline_declarations = collect_nodes_matching(root, "varDef");
    inline_declarations.extend(collect_nodes_matching(root, "varAssignDef"));

    let mut unsafe_scopes = HashSet::new();
    for declaration in inline_declarations {
        let mut current = Some(declaration);
        let mut marked = false;
        while let Some(node) = current {
            match node.kind() {
                "defProc" => {
                    unsafe_scopes.insert(
                        scope_by_span
                            .get(&Span::from_node(node))
                            .copied()
                            .unwrap_or(ROOT_SCOPE),
                    );
                    marked = true;
                    break;
                }
                "lambda" => {
                    // Lambda occurrences are rejected by the per-use check;
                    // they must not disable caching for unrelated root uses.
                    marked = true;
                    break;
                }
                _ => current = node.parent(),
            }
        }
        if !marked {
            // Inline declarations in a program/unit-level block have no
            // distinct scope in the current model, so disable root caching.
            unsafe_scopes.insert(ROOT_SCOPE);
        }
    }
    unsafe_scopes
}

fn scope_for_declaration(node: Node<'_>, scope_by_span: &HashMap<Span, usize>) -> usize {
    let mut current = Some(node);
    while let Some(item) = current {
        if item.kind() == "defProc" {
            return scope_by_span
                .get(&Span::from_node(item))
                .copied()
                .unwrap_or(ROOT_SCOPE);
        }
        current = item.parent();
    }
    ROOT_SCOPE
}

fn region_for_node(node: Node<'_>) -> Region {
    let mut current = Some(node);
    while let Some(item) = current {
        match item.kind() {
            "interface" => return Region::Interface,
            "implementation" => return Region::Implementation,
            _ => current = item.parent(),
        }
    }
    Region::Other
}

fn is_routine_declaration_region(region: Region) -> bool {
    matches!(region, Region::Interface | Region::Implementation)
}

fn is_definition_header(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "defProc"
            && parent
                .child_by_field_name("header")
                .is_some_and(|header| Span::from_node(header).contains(Span::from_node(node)))
    })
}

fn conditional_unknown_symbols(
    root: Node<'_>,
    conditionals: &ConditionalAnalysis,
    symbols: &[Symbol],
) -> Vec<bool> {
    let mut declaration_type_spans = HashMap::new();
    collect_nodes(root, &mut |node| {
        if matches!(
            node.kind(),
            "declVar" | "declArg" | "declField" | "declProp"
        ) {
            if let Some(type_node) = node.child_by_field_name("type") {
                declaration_type_spans.insert(Span::from_node(node), Span::from_node(type_node));
            }
        }
    });

    symbols
        .iter()
        .map(|symbol| {
            let symbol_unknown = conditionals.unknown_spans.iter().any(|unknown| {
                unknown.start <= symbol.span.start && symbol.span.end <= unknown.end
            });
            let type_unknown = declaration_type_spans
                .get(&symbol.declaration_span)
                .is_some_and(|type_span| {
                    conditionals.unknown_spans.iter().any(|unknown| {
                        unknown.start < type_span.end && type_span.start < unknown.end
                    })
                });
            let header_unknown = symbol.routine_header_span.is_some_and(|header| {
                conditionals
                    .unknown_spans
                    .iter()
                    .any(|unknown| unknown.start < header.end && header.start < unknown.end)
            });
            symbol_unknown || type_unknown || header_unknown
        })
        .collect()
}

fn collect_nodes_matching<'a>(root: Node<'a>, kind: &str) -> Vec<Node<'a>> {
    let mut result = Vec::new();
    collect_nodes(root, &mut |node| {
        if node.kind() == kind {
            result.push(node);
        }
    });
    result
}

fn collect_nodes<'a>(root: Node<'a>, callback: &mut impl FnMut(Node<'a>)) {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        callback(node);
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
}

fn collect_parser_recovery_spans(root: Node<'_>) -> Vec<Span> {
    let mut spans = Vec::new();
    collect_nodes(root, &mut |node| {
        if node.is_error() || node.is_missing() {
            spans.push(Span::from_node(node));
        }
    });
    spans
}

fn identifier_nodes(node: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    collect_nodes(node, &mut |child| {
        if child.kind() == "identifier" {
            result.push(child);
        }
    });
    result
}

fn field_identifier_nodes<'a>(node: Node<'a>, field: &str) -> Vec<Node<'a>> {
    let mut result = Vec::new();
    let mut cursor = node.walk();
    for field_node in node.children_by_field_name(field, &mut cursor) {
        result.extend(identifier_nodes(field_node));
    }
    result
}

fn identifier_texts(node: Node<'_>, source: &str) -> Vec<String> {
    identifier_nodes(node)
        .into_iter()
        .map(|identifier| node_text(identifier, source))
        .collect()
}

fn qualified_name_parts(node: &Node<'_>, source: &str) -> Option<Vec<String>> {
    let mut pending = vec![*node];
    let mut parts = Vec::new();
    while let Some(current) = pending.pop() {
        match current.kind() {
            "identifier" => parts.push(node_text(current, source)),
            "exprDot" | "genericDot" | "typerefDot" => {
                let lhs = current.child_by_field_name("lhs")?;
                let rhs = current.child_by_field_name("rhs")?;
                pending.push(rhs);
                pending.push(lhs);
            }
            _ => return None,
        }
    }
    Some(parts)
}

fn callable_lookup_identifier(node: Node<'_>) -> Node<'_> {
    let mut current = node;
    loop {
        match current.kind() {
            "identifier" => return current,
            "exprDot" | "genericDot" | "typerefDot" => {
                let Some(rhs) = current.child_by_field_name("rhs") else {
                    return node;
                };
                current = rhs;
            }
            "exprTpl" => {
                let Some(entity) = current.child_by_field_name("entity") else {
                    return node;
                };
                current = entity;
            }
            _ => return node,
        }
    }
}

fn callable_owner_node(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node;
    loop {
        match current.kind() {
            "exprTpl" => current = current.child_by_field_name("entity")?,
            "exprDot" | "genericDot" | "typerefDot" => {
                return current.child_by_field_name("lhs");
            }
            _ => return None,
        }
    }
}

fn qualified_name_parts_with_budget(
    node: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Vec<String>>, String> {
    budget.require_work(1, cancel)?;
    let mut pending = vec![node];
    let mut parts = Vec::new();
    while let Some(current) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        match current.kind() {
            "identifier" => {
                let text = node_text_with_budget(current, source, cancel, budget)?;
                parts.push(text.to_owned());
            }
            "exprDot" | "genericDot" | "typerefDot" => {
                let Some(lhs) = current.child_by_field_name("lhs") else {
                    return Ok(None);
                };
                let Some(rhs) = current.child_by_field_name("rhs") else {
                    return Ok(None);
                };
                pending.push(rhs);
                pending.push(lhs);
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(parts))
}

fn matching_unit_prefix_len(unit_name: &str, parts: &[String]) -> Option<usize> {
    let unit_parts = unit_name.split('.').filter(|part| !part.is_empty());
    let unit_part_count = unit_parts.clone().count();
    if unit_part_count >= parts.len()
        || !unit_parts
            .zip(parts)
            .all(|(unit_part, path_part)| canonical_name_eq(unit_part, path_part))
    {
        return None;
    }
    Some(unit_part_count)
}

fn canonical_name_eq(left: &str, right: &str) -> bool {
    left.trim_start_matches('&')
        .eq_ignore_ascii_case(right.trim_start_matches('&'))
}

fn node_text_with_budget<'a>(
    node: Node<'_>,
    source: &'a str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<&'a str, String> {
    let text = source
        .get(node.start_byte()..node.end_byte())
        .unwrap_or_default();
    budget.require_bytes(text.len(), cancel)?;
    Ok(text)
}

fn use_name_at_with_budget(
    identifier: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<String>, String> {
    let mut current = Some(identifier);
    while let Some(node) = current {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if node.kind() == "moduleName"
            && has_ancestor_kind_with_budget(node, "declUses", cancel, budget)?
        {
            let span = Span::from_node(node);
            let node_count = count_nodes_with_budget(node, cancel, budget)?;
            budget.require_work(node_count.saturating_mul(3).saturating_add(1), cancel)?;
            budget.require_bytes(
                span.end.saturating_sub(span.start).saturating_mul(2),
                cancel,
            )?;
            return Ok(Some(canonical_path(&identifier_texts(node, source))));
        }
        current = node.parent();
    }
    Ok(None)
}

fn has_ancestor_kind_with_budget(
    node: Node<'_>,
    kind: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if ancestor.kind() == kind {
            return Ok(true);
        }
        current = ancestor.parent();
    }
    Ok(false)
}

fn count_nodes_with_budget(
    node: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<usize, String> {
    let mut cursor = node.walk();
    let mut count = 0usize;
    loop {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        count = count.saturating_add(1);

        if cursor.goto_first_child() {
            #[cfg(test)]
            TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(|entries| {
                entries.set(entries.get().saturating_add(1));
            });
            continue;
        }

        loop {
            if cursor.goto_next_sibling() {
                #[cfg(test)]
                TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(|entries| {
                    entries.set(entries.get().saturating_add(1));
                });
                break;
            }
            if !cursor.goto_parent() {
                return Ok(count);
            }
        }
    }
}

fn qualified_type_path_at_with_budget(
    identifier: Node<'_>,
    source: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<(Vec<String>, usize)>, String> {
    let mut current = identifier.parent();
    let mut qualified_node = None;
    while let Some(node) = current {
        check_navigation_cancel(cancel)?;
        if matches!(node.kind(), "typerefDot" | "genericDot") {
            if let Some(parts) = qualified_name_parts_with_budget(node, source, cancel, budget)? {
                if parts.len() > 1 {
                    qualified_node = Some((node, parts));
                }
            }
        }
        current = node.parent();
    }
    let Some((node, parts)) = qualified_node else {
        return Ok(None);
    };
    let Some(cursor_index) =
        qualified_identifier_index_with_budget(node, identifier, cancel, budget)?
    else {
        return Ok(None);
    };
    Ok(Some((parts, cursor_index)))
}

fn qualified_identifier_index_with_budget(
    node: Node<'_>,
    target: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<usize>, String> {
    let target_span = Span::from_node(target);
    let mut pending = vec![node];
    let mut index = 0usize;
    while let Some(current) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        match current.kind() {
            "identifier" => {
                if Span::from_node(current) == target_span {
                    return Ok(Some(index));
                }
                index = index.saturating_add(1);
            }
            "exprDot" | "genericDot" | "typerefDot" => {
                let Some(lhs) = current.child_by_field_name("lhs") else {
                    return Ok(None);
                };
                let Some(rhs) = current.child_by_field_name("rhs") else {
                    return Ok(None);
                };
                pending.push(rhs);
                pending.push(lhs);
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

fn qualified_type_path_at(identifier: Node<'_>, source: &str) -> Option<(Vec<String>, usize)> {
    let mut current = identifier.parent();
    let mut qualified_node = None;
    while let Some(node) = current {
        if matches!(node.kind(), "typerefDot" | "genericDot") {
            let parts = qualified_name_parts(&node, source)?;
            if parts.len() > 1 {
                qualified_node = Some(node);
            }
        }
        current = node.parent();
    }
    let node = qualified_node?;
    let parts = qualified_name_parts(&node, source)?;
    let cursor_index = identifier_nodes(node)
        .into_iter()
        .position(|candidate| Span::from_node(candidate) == Span::from_node(identifier))?;
    Some((parts, cursor_index))
}

fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    (0..node.named_child_count()).find_map(|index| node.named_child(index))
}

fn canonical_name(name: &str) -> String {
    name.trim_start_matches('&').to_ascii_lowercase()
}

fn canonical_path(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| canonical_name(part))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(".")
}

fn node_text(node: Node<'_>, source: &str) -> String {
    source
        .get(node.start_byte()..node.end_byte())
        .unwrap_or_default()
        .to_string()
}

fn fallback_unit_name(uri: &Url) -> String {
    uri.to_file_path()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
        .or_else(|| {
            uri.path_segments()
                .and_then(|mut segments| segments.next_back())
                .map(|name| name.trim_end_matches(".pas").to_owned())
        })
        .map_or_else(|| "<anonymous>".to_string(), |name| canonical_name(&name))
}

fn has_ancestor_kind(node: Node<'_>, kind: &str) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == kind {
            return true;
        }
        current = parent.parent();
    }
    false
}

fn identifier_at(root: Node<'_>, offset: usize) -> Option<Node<'_>> {
    if !Span::from_node(root).contains_offset(offset) {
        return None;
    }

    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if !Span::from_node(node).contains_offset(offset) {
            continue;
        }
        if node.kind() == "identifier" {
            return Some(node);
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    None
}

fn is_ignored_offset(root: Node<'_>, offset: usize) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if !Span::from_node(node).contains_offset(offset) {
            continue;
        }
        if matches!(node.kind(), "comment" | "literalString" | "literalChar") {
            return true;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    false
}

fn use_name_at(identifier: Node<'_>, source: &str) -> Option<String> {
    let mut current = Some(identifier);
    while let Some(node) = current {
        if node.kind() == "moduleName" && has_ancestor_kind(node, "declUses") {
            return Some(canonical_path(&identifier_texts(node, source)));
        }
        current = node.parent();
    }
    None
}

fn member_expression_at(identifier: Node<'_>) -> Option<Node<'_>> {
    let mut current = identifier.parent();
    while let Some(node) = current {
        if node.kind() == "exprDot" {
            return Some(node);
        }
        if matches!(node.kind(), "assignment" | "block" | "statements") {
            return None;
        }
        current = node.parent();
    }
    None
}

fn is_right_hand_member(dot: Node<'_>, identifier: Node<'_>) -> bool {
    dot.child_by_field_name("rhs")
        .is_some_and(|rhs| Span::from_node(rhs).contains(Span::from_node(identifier)))
}

fn location_for_span(uri: &Url, source: &str, span: Span) -> Option<Location> {
    let start = text::offset_to_position(source, span.start)?;
    let end = text::offset_to_position(source, span.end)?;
    Some(Location {
        uri: uri.clone(),
        range: Range { start, end },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    fn repeated_class_field_source(repetitions: usize) -> String {
        let mut source = String::from(
            "unit RepeatedClassField;\ninterface\ntype\n  TFirst = class\n    FValue: Integer;\n    procedure Touch;\n  end;\nimplementation\nprocedure TFirst.Touch;\nbegin\n",
        );
        for index in 0..repetitions {
            writeln!(&mut source, "  FValue := {index};").expect("write fixture source");
        }
        source.push_str("end;\nend.\n");
        source
    }

    fn query_repeated_class_field(repetitions: usize) -> (usize, usize) {
        let source = repeated_class_field_source(repetitions);
        let uri = Url::parse("file:///tmp/repeated-class-field.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source)
            .expect("class-field fixture parses");

        OWNER_TYPE_ROOT_LOOKUPS.with(|lookups| lookups.set(0));
        let locations = index
            .binding_locations(&uri, Position::new(4, 4), true)
            .expect("class-field references resolve");
        let root_lookups = OWNER_TYPE_ROOT_LOOKUPS.with(Cell::get);
        (locations.len(), root_lookups)
    }

    #[test]
    fn class_field_binding_work_does_not_repeat_root_lookup_per_occurrence() {
        let (small_count, small_root_lookups) = query_repeated_class_field(1);
        let (large_count, large_root_lookups) = query_repeated_class_field(128);

        assert_eq!(small_count, 2);
        assert_eq!(large_count, 129);
        assert_eq!(
            large_root_lookups, small_root_lookups,
            "known class owners must not restart root lookup for each field use"
        );
    }

    #[test]
    fn budgeted_uses_lookup_charges_ast_traversal_before_identifier_materialization() {
        let source =
            "unit UsesBudgetConsumer;\ninterface\nuses BudgetProvider;\nimplementation\nend.\n";
        let uri = Url::parse("file:///tmp/uses-budget-consumer.pas").expect("fixture URI");
        let cancel = AtomicBool::new(false);
        let document = Document::parse_with_cancel(uri, source.to_owned(), &[], &cancel)
            .expect("uses-budget fixture parses");
        let module_name = collect_nodes_matching(document.tree.root_node(), "moduleName")
            .into_iter()
            .find(|node| has_ancestor_kind(*node, "declUses"))
            .expect("uses module name");

        let mut count_budget = AssistanceBudget::new(4096, 4096, "uses traversal");
        let node_count = count_nodes_with_budget(module_name, &cancel, &mut count_budget)
            .expect("count uses nodes");
        let exact_work = node_count
            .saturating_add(node_count.saturating_mul(3))
            .saturating_add(3);
        let mut exact_budget = AssistanceBudget::new(exact_work, 4096, "uses lookup");
        assert_eq!(
            use_name_at_with_budget(module_name, &document.source, &cancel, &mut exact_budget,)
                .expect("exact uses lookup budget"),
            Some("budgetprovider".to_owned())
        );
        assert_eq!(exact_budget.remaining_work, 0);
    }

    #[test]
    fn wide_uses_lookup_charges_each_child_before_materializing_a_frontier() {
        let parts = (0..64)
            .map(|index| format!("UnitPart{index}"))
            .collect::<Vec<_>>();
        let source = format!(
            "unit WideUsesBudget;\ninterface\nuses {};\nimplementation\nend.\n",
            parts.join(".")
        );
        let uri = Url::parse("file:///tmp/wide-uses-budget.pas").expect("fixture URI");
        let cancel = AtomicBool::new(false);
        let document = Document::parse_with_cancel(uri, source, &[], &cancel)
            .expect("wide uses fixture parses");
        let module_name = collect_nodes_matching(document.tree.root_node(), "moduleName")
            .into_iter()
            .find(|node| has_ancestor_kind(*node, "declUses"))
            .expect("wide uses module name");
        assert!(
            module_name.named_child_count() >= 8,
            "wide module name was not parsed"
        );

        TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(|entries| entries.set(0));
        let mut zero_budget = AssistanceBudget::new(0, 4096, "wide uses zero budget");
        assert!(count_nodes_with_budget(module_name, &cancel, &mut zero_budget).is_err());
        assert_eq!(
            TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(Cell::get),
            0,
            "zero remaining work must reject before entering the module children"
        );

        TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(|entries| entries.set(0));
        let mut one_budget = AssistanceBudget::new(1, 4096, "wide uses one budget");
        assert!(count_nodes_with_budget(module_name, &cancel, &mut one_budget).is_err());
        assert!(
            TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(Cell::get) <= 1,
            "one remaining unit must not materialize the whole sibling frontier"
        );

        let mut count_budget = AssistanceBudget::new(4096, 4096, "wide uses count");
        let node_count = count_nodes_with_budget(module_name, &cancel, &mut count_budget)
            .expect("count wide uses nodes");
        TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(|entries| entries.set(0));
        let mut exact_budget = AssistanceBudget::new(node_count, 4096, "wide uses exact budget");
        assert_eq!(
            count_nodes_with_budget(module_name, &cancel, &mut exact_budget)
                .expect("exact wide uses budget"),
            node_count
        );
        assert_eq!(exact_budget.remaining_work, 0);

        let cancelled = AtomicBool::new(true);
        TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(|entries| entries.set(0));
        let mut cancelled_budget = AssistanceBudget::new(4096, 4096, "wide uses cancelled");
        assert!(count_nodes_with_budget(module_name, &cancelled, &mut cancelled_budget).is_err());
        assert_eq!(
            TEST_NODE_SIBLING_FRONTIER_ENTRIES.with(Cell::get),
            0,
            "cancelled traversal must not enter a child frontier"
        );
    }

    #[test]
    fn conditional_unknown_status_is_precomputed_per_symbol() {
        let source = "unit CachedConditionalStatus;\ninterface\nvar\n{$IFDEF MAYBE}\n  Hidden: Integer;\n{$ENDIF}\n  Visible: Integer;\nimplementation\nend.\n";
        let uri = Url::parse("file:///tmp/cached-conditional-status.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("conditional symbol fixture parses");
        let document = index.documents.get(&uri).expect("indexed document");
        let hidden = document
            .symbols
            .iter()
            .position(|symbol| symbol.name.eq_ignore_ascii_case("Hidden"))
            .expect("hidden symbol");
        let visible = document
            .symbols
            .iter()
            .position(|symbol| symbol.name.eq_ignore_ascii_case("Visible"))
            .expect("visible symbol");

        assert_eq!(
            document.conditional_unknown_symbols.len(),
            document.symbols.len()
        );
        assert!(document.conditional_unknown_symbols[hidden]);
        assert!(!document.conditional_unknown_symbols[visible]);
        assert!(index.candidate_is_conditionally_unknown(&Candidate {
            uri: uri.clone(),
            index: hidden,
        }));
        assert!(!index.candidate_is_conditionally_unknown(&Candidate {
            uri,
            index: visible
        }));
    }

    #[test]
    fn unknown_class_owner_cache_survives_a_large_method_body() {
        let mut source = String::from(
            "unit CachedUnknownOwner;\ninterface\ntype\n  TObj = class(TUnknown)\n    procedure Caller;\n  end;\nconst\n  GlobalName = 1;\nimplementation\nprocedure TObj.Caller;\nbegin\n  Glo;\n",
        );
        for _ in 0..120_000 {
            source.push_str("  WriteLn(1);\n");
        }
        source.push_str("end;\nend.\n");
        let uri = Url::parse("file:///tmp/cached-unknown-owner.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.clone())
            .expect("large unknown-owner fixture parses");
        let document = index.documents.get(&uri).expect("indexed document");
        let offset = source.find("  Glo;").expect("global prefix") + "  Glo;".len();
        assert_eq!(
            document.owner_type_at(offset).as_deref(),
            Some("tobj"),
            "scopes: {:?}",
            document.scopes
        );
        assert!(document.unknown_class_owners.contains("tobj"));
    }

    #[test]
    fn inherited_member_dag_uses_bounded_memoized_expansion() {
        const DAG_WIDTH: usize = 37;
        let mut source = String::from(
            "unit InheritedMemberDag;\ninterface\ntype\n  I0 = interface\n    procedure Hit;\n  end;\n",
        );
        for index in 1..DAG_WIDTH {
            if index == 1 {
                writeln!(&mut source, "  I{index} = interface(I0)\n  end;")
                    .expect("write DAG node");
            } else {
                writeln!(
                    &mut source,
                    "  I{index} = interface(I{}, I{})\n  end;",
                    index - 1,
                    index - 2
                )
                .expect("write DAG node");
            }
        }
        source.push_str(
            "implementation\nprocedure Caller;\nvar\n  Obj: I36;\nbegin\n  Obj.Hit;\nend;\nend.\n",
        );
        let uri = Url::parse("file:///tmp/inherited-member-dag.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source)
            .expect("inherited member DAG parses");

        let mut state = AncestryResolutionState::new();
        assert_eq!(
            index.resolve_type_ancestry(&uri, "i36", &mut state).status,
            AncestryStatus::Complete
        );
        let lookup = index.member_candidates_for_type_with_state(
            &uri,
            "i36",
            ROOT_SCOPE,
            Some("hit"),
            false,
            &mut state,
        );
        assert!(lookup.ancestry_known);
        assert_eq!(lookup.candidates.len(), 1);
        assert_eq!(
            index
                .symbol(&lookup.candidates[0])
                .map(|symbol| symbol.name.as_str()),
            Some("Hit")
        );
        assert_eq!(
            state.remaining_work,
            MAX_ANCESTRY_WORK.saturating_sub(DAG_WIDTH.saturating_mul(2)),
            "each type and type/member pair must consume one bounded expansion"
        );
    }
}
