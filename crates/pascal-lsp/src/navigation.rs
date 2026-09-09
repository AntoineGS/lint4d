use crate::text;
use lsp_types::{Location, Position, Range, Url};
use pascal_core::{FileInfo, parser};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use tree_sitter::{Node, Tree};

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

impl NavigationIndex {
    /// Construct an empty navigation index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse and replace one document.
    ///
    /// Parsing happens before the old document is replaced, so a parser error
    /// leaves the last known-good overlay available to the caller.
    pub fn update(&mut self, uri: Url, source: String) -> Result<(), String> {
        let document = Document::parse(uri.clone(), source)?;
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
        if is_ignored_offset(document.tree.root_node(), offset) {
            return Vec::new();
        }
        let Some(identifier) = identifier_at(document.tree.root_node(), offset) else {
            return Vec::new();
        };

        let name = node_text(identifier, &document.source);
        let references = if let Some(unit_name) = use_name_at(identifier, &document.source) {
            self.unit_references(&unit_name)
        } else if let Some(direct) = self.direct_symbol_references(uri, identifier) {
            direct
        } else if let Some((path, cursor_index)) =
            qualified_type_path_at(identifier, &document.source)
        {
            self.type_reference_candidates(uri, document, offset, &path, cursor_index)
        } else if let Some(dot) = member_expression_at(identifier) {
            if is_right_hand_member(dot, identifier) {
                self.member_references(uri, document, offset, dot, &name)
            } else {
                self.unqualified_references(uri, document, offset, &name)
            }
        } else {
            self.unqualified_references(uri, document, offset, &name)
        };

        self.locations_for(references, target)
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
        }
        self.type_receivers_for_parts(current_uri, current_document, offset, parts, &mut state)
            .into_iter()
            .flat_map(|receiver| match receiver {
                Receiver::Type(type_uri, type_key) => self
                    .documents
                    .get(&type_uri)
                    .into_iter()
                    .flat_map(|document| document.symbols.iter().enumerate())
                    .filter(move |(_, symbol)| {
                        symbol.kind == SymbolKind::Type && symbol.key == type_key
                    })
                    .map(move |(index, _)| Candidate {
                        uri: type_uri.clone(),
                        index,
                    })
                    .collect::<Vec<_>>(),
                Receiver::Unit(_) => Vec::new(),
            })
            .collect()
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
        let direct: Vec<usize> = document
            .symbols
            .iter()
            .enumerate()
            .filter_map(|(index, symbol)| (symbol.span == span).then_some(index))
            .collect();
        if direct.is_empty() {
            return None;
        }

        let mut references = Vec::new();
        for index in direct {
            let symbol = &document.symbols[index];
            if symbol.kind == SymbolKind::Routine {
                if let Some(routine_key) = &symbol.routine_key {
                    for (candidate_index, candidate) in document.symbols.iter().enumerate() {
                        if candidate.kind == SymbolKind::Routine
                            && candidate.routine_key.as_ref() == Some(routine_key)
                        {
                            references.push(Candidate {
                                uri: uri.clone(),
                                index: candidate_index,
                            });
                        }
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

    fn unit_references(&self, name: &str) -> Vec<Candidate> {
        let key = canonical_name(name);
        self.units
            .get(&key)
            .into_iter()
            .flat_map(|uris| uris.iter())
            .filter_map(|uri| {
                let document = self.documents.get(uri)?;
                let index = document
                    .symbols
                    .iter()
                    .position(|symbol| symbol.kind == SymbolKind::Unit)?;
                Some(Candidate {
                    uri: uri.clone(),
                    index,
                })
            })
            .collect()
    }

    fn unqualified_references(
        &self,
        uri: &Url,
        document: &Document,
        offset: usize,
        name: &str,
    ) -> Vec<Candidate> {
        let key = canonical_name(name);

        // Search lexical scopes from the innermost outward. A local symbol
        // shadows both the unit's declarations and imported declarations.
        for scope_id in document.scope_chain(offset) {
            let local: Vec<Candidate> = document
                .symbols
                .iter()
                .enumerate()
                .filter(|(_, symbol)| {
                    symbol.scope == scope_id
                        && symbol.scope != ROOT_SCOPE
                        && !symbol.local_only
                        && symbol.owner_type.is_none()
                        && symbol.key == key
                        && symbol.kind != SymbolKind::Unit
                        && !symbol.unresolved_abbreviated
                })
                .map(|(index, _)| Candidate {
                    uri: uri.clone(),
                    index,
                })
                .collect();
            if !local.is_empty() {
                return local;
            }
        }

        // Class members are the next lexical scope in Delphi. They must be
        // considered before unit globals and imported declarations, and the
        // owner lookup also covers nested procedures inside a class method.
        if let Some(owner_type) = document.owner_type_at(offset) {
            let members = self.member_references_for_type(uri, &owner_type, &key, true);
            if !members.is_empty() {
                return members;
            }
        }

        let region = document.region_at(offset);
        let current: Vec<Candidate> = document
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| {
                symbol.scope == ROOT_SCOPE
                    && !symbol.local_only
                    && symbol.owner_type.is_none()
                    && symbol.key == key
                    && symbol.kind != SymbolKind::Unit
                    && !symbol.unresolved_abbreviated
                    && symbol_visible_in_region(symbol, region)
            })
            .map(|(index, _)| Candidate {
                uri: uri.clone(),
                index,
            })
            .collect();
        if !current.is_empty() {
            return current;
        }

        // Only the uses clauses active at this source position contribute
        // imported names. Imported implementation-only routines are excluded
        // by exported_references_for_document.
        let mut imported = Vec::new();
        for unit in document.active_uses(region) {
            for unit_uri in self.units.get(unit).into_iter().flatten() {
                imported.extend(
                    self.exported_references_for_document(unit_uri)
                        .into_iter()
                        .filter(|candidate| {
                            self.symbol(candidate)
                                .is_some_and(|symbol| symbol.key == key)
                        }),
                );
            }
        }
        if !imported.is_empty() {
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
        let Some(lhs) = dot.child_by_field_name("lhs") else {
            return Vec::new();
        };
        let key = canonical_name(member_name);
        let mut references = Vec::new();
        for receiver in self.resolve_receivers(current_uri, current_document, offset, lhs) {
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
                Receiver::Type(type_uri, type_key) => {
                    references.extend(self.member_references_for_type(
                        &type_uri,
                        &type_key,
                        &key,
                        type_uri == *current_uri,
                    ))
                }
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

    fn resolve_receivers_with_state(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        node: Node<'_>,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        if !state.take_receiver_work() {
            return Vec::new();
        }
        match node.kind() {
            "identifier" => self.resolve_identifier_receiver(
                current_uri,
                current_document,
                offset,
                &node_text(node, &current_document.source),
                state,
            ),
            "exprParens" => first_named_child(node)
                .map(|operand| {
                    self.resolve_receivers_with_state(
                        current_uri,
                        current_document,
                        offset,
                        operand,
                        state,
                    )
                })
                .unwrap_or_default(),
            "exprDot" | "genericDot" | "typerefDot" => {
                if let Some(parts) = qualified_name_parts(&node, &current_document.source) {
                    return self.resolve_qualified_receiver_path(
                        current_uri,
                        current_document,
                        offset,
                        &parts,
                        state,
                    );
                }
                let Some(lhs) = node.child_by_field_name("lhs") else {
                    return Vec::new();
                };
                let Some(rhs) = node.child_by_field_name("rhs") else {
                    return Vec::new();
                };
                let Some(rhs_name) = qualified_name_parts(&rhs, &current_document.source)
                    .and_then(|parts| parts.last().cloned())
                else {
                    return Vec::new();
                };
                let mut result = Vec::new();
                for receiver in self.resolve_receivers_with_state(
                    current_uri,
                    current_document,
                    offset,
                    lhs,
                    state,
                ) {
                    match receiver {
                        Receiver::Unit(unit_uri) => {
                            result.extend(self.type_receivers_in_unit(
                                &unit_uri,
                                &rhs_name,
                                unit_uri == *current_uri,
                            ));
                        }
                        Receiver::Type(type_uri, type_key) => {
                            result.extend(self.member_type_receivers(
                                &type_uri,
                                &type_key,
                                &rhs_name,
                                type_uri == *current_uri,
                                state,
                            ));
                        }
                    }
                }
                result
            }
            _ => Vec::new(),
        }
    }

    fn resolve_qualified_receiver_path(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        if parts.len() < 2 || parts.len() > MAX_RECEIVER_WORK {
            return Vec::new();
        }

        let Some(first) = parts.first() else {
            return Vec::new();
        };
        let first_is_bound = !self
            .unqualified_references(current_uri, current_document, offset, first)
            .is_empty();
        if first.eq_ignore_ascii_case("Self") || first_is_bound {
            let mut receivers = self.resolve_identifier_receiver(
                current_uri,
                current_document,
                offset,
                first,
                state,
            );
            for member_name in &parts[1..] {
                receivers = receivers
                    .into_iter()
                    .flat_map(|receiver| match receiver {
                        Receiver::Type(type_uri, type_key) => self.member_type_receivers(
                            &type_uri,
                            &type_key,
                            member_name,
                            type_uri == *current_uri,
                            state,
                        ),
                        Receiver::Unit(unit_uri) => self.type_receivers_in_unit(
                            &unit_uri,
                            member_name,
                            unit_uri == *current_uri,
                        ),
                    })
                    .collect();
            }
            return receivers;
        }

        if let Some(unit_uris) =
            self.visible_unit_urls_for_path(current_uri, current_document, offset, parts)
        {
            return unit_uris.into_iter().map(Receiver::Unit).collect();
        }

        if let Some((prefix_len, unit_uris)) =
            self.longest_visible_unit_prefix(current_uri, current_document, offset, parts)
        {
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
                        Receiver::Type(type_uri, type_key) => self.member_type_receivers(
                            &type_uri,
                            &type_key,
                            member_name,
                            type_uri == *current_uri,
                            state,
                        ),
                        Receiver::Unit(_) => Vec::new(),
                    })
                    .collect();
            }
            return receivers;
        }

        let mut receivers =
            self.resolve_identifier_receiver(current_uri, current_document, offset, first, state);
        for member_name in &parts[1..] {
            receivers = receivers
                .into_iter()
                .flat_map(|receiver| match receiver {
                    Receiver::Type(type_uri, type_key) => self.member_type_receivers(
                        &type_uri,
                        &type_key,
                        member_name,
                        type_uri == *current_uri,
                        state,
                    ),
                    Receiver::Unit(unit_uri) => self.type_receivers_in_unit(
                        &unit_uri,
                        member_name,
                        unit_uri == *current_uri,
                    ),
                })
                .collect();
        }
        receivers
    }

    fn resolve_identifier_receiver(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        name: &str,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        if name.eq_ignore_ascii_case("Self") {
            return current_document
                .owner_type_at(offset)
                .map(|owner_type| vec![Receiver::Type(current_uri.clone(), owner_type)])
                .unwrap_or_default();
        }

        let references = self.unqualified_references(current_uri, current_document, offset, name);
        if !references.is_empty() {
            let mut result = Vec::new();
            for reference in references {
                let Some(symbol) = self.symbol(&reference) else {
                    continue;
                };
                match symbol.kind {
                    SymbolKind::Type => {
                        result.push(Receiver::Type(reference.uri.clone(), symbol.key.clone()));
                    }
                    SymbolKind::Variable
                    | SymbolKind::Parameter
                    | SymbolKind::Field
                    | SymbolKind::Property => {
                        if let Some(type_name) = &symbol.type_name {
                            let Some(declaration_document) = self.documents.get(&reference.uri)
                            else {
                                continue;
                            };
                            result.extend(self.type_receivers_for_path(
                                &reference.uri,
                                declaration_document,
                                symbol.span.start,
                                type_name,
                                state,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            // A visible local/imported binding shadows a unit with the same
            // spelling even when its type is currently unknown.
            return result;
        }

        self.visible_unit_urls(current_uri, current_document, offset, name)
            .into_iter()
            .map(Receiver::Unit)
            .collect()
    }

    fn type_receivers_for_path(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        path: &str,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        let parts = path
            .split('.')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        self.type_receivers_for_parts(current_uri, current_document, offset, &parts, state)
    }

    fn type_receivers_for_parts(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        if parts.is_empty() || !state.take_type_work(parts.len()) {
            return Vec::new();
        }

        if parts.len() == 1 {
            return self
                .unqualified_references(current_uri, current_document, offset, &parts[0])
                .into_iter()
                .filter_map(|candidate| {
                    let symbol = self.symbol(&candidate)?;
                    (symbol.kind == SymbolKind::Type)
                        .then(|| Receiver::Type(candidate.uri.clone(), symbol.key.clone()))
                })
                .collect();
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
                    Receiver::Type(type_uri, type_key) => self.member_type_receivers(
                        &type_uri,
                        &type_key,
                        member_name,
                        type_uri == *current_uri,
                        state,
                    ),
                    Receiver::Unit(_) => Vec::new(),
                })
                .collect();
        }
        receivers
    }

    fn visible_unit_urls(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        name: &str,
    ) -> Vec<Url> {
        let parts = vec![name.to_owned()];
        self.visible_unit_urls_for_path(current_uri, current_document, offset, &parts)
            .unwrap_or_default()
    }

    fn visible_unit_urls_for_path(
        &self,
        current_uri: &Url,
        current_document: &Document,
        offset: usize,
        parts: &[String],
    ) -> Option<Vec<Url>> {
        let key = canonical_path(parts);
        if current_document.unit_name == key {
            return Some(vec![current_uri.clone()]);
        }
        let region = current_document.region_at(offset);
        if !current_document
            .active_uses(region)
            .iter()
            .any(|used| used.as_str() == key)
        {
            return None;
        }
        self.units
            .get(&key)
            .cloned()
            .filter(|urls| !urls.is_empty())
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
            if let Some(unit_uris) = self.units.get(used) {
                consider(used, unit_uris.clone());
            }
        }
        best
    }

    fn type_receivers_in_unit(
        &self,
        unit_uri: &Url,
        name: &str,
        allow_implementation: bool,
    ) -> Vec<Receiver> {
        let key = canonical_name(name);
        let Some(document) = self.documents.get(unit_uri) else {
            return Vec::new();
        };
        document
            .symbols
            .iter()
            .filter(|symbol| {
                symbol.kind == SymbolKind::Type
                    && symbol.owner_type.is_none()
                    && !symbol.local_only
                    && symbol.key == key
                    && (symbol.region == Region::Interface
                        || (allow_implementation && symbol.region == Region::Implementation))
            })
            .map(|symbol| Receiver::Type(unit_uri.clone(), symbol.key.clone()))
            .collect()
    }

    fn member_type_receivers(
        &self,
        type_uri: &Url,
        type_key: &str,
        name: &str,
        allow_implementation: bool,
        state: &mut ResolutionState,
    ) -> Vec<Receiver> {
        let Some(document) = self.documents.get(type_uri) else {
            return Vec::new();
        };
        let member_key = canonical_name(name);
        let resolution_key = (type_uri.clone(), type_key.to_owned(), member_key.clone());
        if !state.active_members.insert(resolution_key.clone()) {
            return Vec::new();
        }
        let result = self
            .member_references_for_type(type_uri, type_key, &member_key, allow_implementation)
            .into_iter()
            .filter_map(|candidate| {
                let symbol = self.symbol(&candidate)?;
                let type_name = symbol.type_name.as_deref()?;
                self.type_receivers_for_path(
                    type_uri,
                    document,
                    symbol.span.start,
                    type_name,
                    state,
                )
                .into_iter()
                .next()
            })
            .collect();
        state.active_members.remove(&resolution_key);
        result
    }

    fn member_references_for_type(
        &self,
        type_uri: &Url,
        type_key: &str,
        member_key: &str,
        allow_implementation: bool,
    ) -> Vec<Candidate> {
        let Some(document) = self.documents.get(type_uri) else {
            return Vec::new();
        };
        let interface_routine_keys: HashSet<String> = document
            .symbols
            .iter()
            .filter_map(|symbol| {
                (symbol.owner_type.as_deref() == Some(type_key)
                    && symbol.kind == SymbolKind::Routine
                    && symbol.origin == Origin::Declaration
                    && (symbol.region == Region::Interface
                        || (allow_implementation && symbol.region == Region::Implementation)))
                    .then(|| symbol.routine_key.clone())
                    .flatten()
            })
            .collect();

        document
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| {
                symbol.owner_type.as_deref() == Some(type_key)
                    && symbol.key == member_key
                    && !symbol.local_only
                    && match symbol.kind {
                        SymbolKind::Routine => {
                            (symbol.origin == Origin::Declaration
                                && (symbol.region == Region::Interface
                                    || (allow_implementation
                                        && symbol.region == Region::Implementation)))
                                || (symbol.origin == Origin::Definition
                                    && symbol
                                        .routine_key
                                        .as_ref()
                                        .is_some_and(|key| interface_routine_keys.contains(key)))
                        }
                        _ => {
                            symbol.region == Region::Interface
                                || (allow_implementation && symbol.region == Region::Implementation)
                        }
                    }
            })
            .map(|(index, _)| Candidate {
                uri: type_uri.clone(),
                index,
            })
            .collect()
    }

    fn exported_references_for_document(&self, uri: &Url) -> Vec<Candidate> {
        let Some(document) = self.documents.get(uri) else {
            return Vec::new();
        };
        let interface_routines: HashSet<String> = document
            .symbols
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

        document
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| {
                if symbol.owner_type.is_some()
                    || symbol.kind == SymbolKind::Unit
                    || symbol.local_only
                {
                    return false;
                }
                symbol.region == Region::Interface
                    || (symbol.kind == SymbolKind::Routine
                        && symbol.origin == Origin::Definition
                        && symbol
                            .routine_key
                            .as_ref()
                            .is_some_and(|key| interface_routines.contains(key)))
            })
            .map(|(index, _)| Candidate {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    Interface,
    Implementation,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Declaration,
    Definition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
struct Symbol {
    span: Span,
    key: String,
    kind: SymbolKind,
    scope: usize,
    owner_type: Option<String>,
    type_name: Option<String>,
    region: Region,
    origin: Origin,
    local_only: bool,
    routine_key: Option<String>,
    routine_signature: Option<String>,
    body_scope: Option<usize>,
    unresolved_abbreviated: bool,
}

#[derive(Debug, Clone)]
struct Candidate {
    uri: Url,
    index: usize,
}

enum Receiver {
    Unit(Url),
    Type(Url, String),
}

const MAX_RECEIVER_WORK: usize = 256;
const MAX_TYPE_RESOLUTION_WORK: usize = 256;

struct ResolutionState {
    receiver_work: usize,
    type_work: usize,
    active_members: HashSet<(Url, String, String)>,
}

impl ResolutionState {
    fn new() -> Self {
        Self {
            receiver_work: MAX_RECEIVER_WORK,
            type_work: MAX_TYPE_RESOLUTION_WORK,
            active_members: HashSet::new(),
        }
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
    unit_name: String,
    interface_range: Option<Span>,
    implementation_range: Option<Span>,
    interface_uses: Vec<String>,
    implementation_uses: Vec<String>,
    scopes: Vec<Scope>,
    symbols: Vec<Symbol>,
}

impl Document {
    fn parse(uri: Url, source: String) -> Result<Self, String> {
        let path = uri
            .to_file_path()
            .unwrap_or_else(|_| PathBuf::from(uri.path()));
        let info = FileInfo::new(path);
        let (tree, _diagnostics) = parser::parse_file(&info, source.as_bytes())?;
        let root = tree.root_node();

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

        let interface_range = sections
            .iter()
            .find_map(|(region, span)| (*region == Region::Interface).then_some(*span));
        let implementation_range = sections
            .iter()
            .find_map(|(region, span)| (*region == Region::Implementation).then_some(*span));

        let mut interface_uses = Vec::new();
        let mut implementation_uses = Vec::new();
        for module_name in &module_names {
            if !has_ancestor_kind(*module_name, "declUses") {
                continue;
            }
            let name = canonical_path(&identifier_texts(*module_name, &source));
            if name.is_empty() {
                continue;
            }
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

        let definitions = collect_nodes_matching(root, "defProc");
        let (scopes, scope_by_span) = build_scopes(source.len(), &definitions, &source);
        let mut symbols = Vec::new();

        if let Some(module_name) = unit_module {
            symbols.push(Symbol {
                span: Span::from_node(module_name),
                key: unit_name.clone(),
                kind: SymbolKind::Unit,
                scope: ROOT_SCOPE,
                owner_type: None,
                type_name: None,
                region: Region::Other,
                origin: Origin::Declaration,
                local_only: false,
                routine_key: None,
                routine_signature: None,
                body_scope: None,
                unresolved_abbreviated: false,
            });
        }

        collect_symbols(root, &source, &scopes, &scope_by_span, &mut symbols);
        pair_abbreviated_definitions(root, &source, &scope_by_span, &mut symbols);

        Ok(Self {
            source,
            tree,
            unit_name,
            interface_range,
            implementation_range,
            interface_uses,
            implementation_uses,
            scopes,
            symbols,
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
        let mut result = Vec::new();
        let mut current = self.scope_at(offset);
        loop {
            result.push(current);
            let Some(parent) = self.scopes[current].parent else {
                break;
            };
            current = parent;
        }
        result
    }

    fn owner_type_at(&self, offset: usize) -> Option<String> {
        let mut scope = self.scope_at(offset);
        loop {
            if let Some(owner_type) = &self.scopes[scope].owner_type {
                return Some(owner_type.clone());
            }
            let Some(parent) = self.scopes[scope].parent else {
                break;
            };
            scope = parent;
        }

        // Class declarations do not create routine scopes, but member names
        // used by property accessors still resolve in the enclosing type.
        identifier_at(self.tree.root_node(), offset)
            .and_then(|identifier| enclosing_type(identifier, &self.source))
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
        for identifier in field_identifier_nodes(node, "name") {
            let name = node_text(identifier, source);
            if name.is_empty() {
                continue;
            }
            symbols.push(Symbol {
                span: Span::from_node(identifier),
                key: canonical_name(&name),
                kind: SymbolKind::Parameter,
                scope: body_scope,
                owner_type: None,
                type_name: type_name.clone(),
                region: Region::Implementation,
                origin: Origin::Declaration,
                local_only: false,
                routine_key: None,
                routine_signature: None,
                body_scope: None,
                unresolved_abbreviated: false,
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
    let own_scope = scope_by_span
        .get(&Span::from_node(node))
        .copied()
        .unwrap_or(ROOT_SCOPE);
    let scope = scopes[own_scope].parent.unwrap_or(ROOT_SCOPE);
    let signature = routine_signature(header, source);
    symbols.push(Symbol {
        span,
        key: canonical_name(&name),
        kind: SymbolKind::Routine,
        scope,
        owner_type: owner_type.clone(),
        type_name: None,
        region: region_for_node(node),
        origin: Origin::Definition,
        local_only: false,
        routine_key: Some(routine_key_with_owner(
            owner_type.as_deref(),
            &name,
            &signature,
            scope,
        )),
        routine_signature: Some(signature),
        body_scope: Some(own_scope),
        unresolved_abbreviated: false,
    });
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
    let scope = scope_for_declaration(node, scope_by_span);
    let signature = routine_signature(node, source);
    symbols.push(Symbol {
        span,
        key: canonical_name(&name),
        kind: SymbolKind::Routine,
        scope,
        owner_type: owner_type.clone(),
        type_name: None,
        region: region_for_node(node),
        origin: Origin::Declaration,
        local_only: false,
        routine_key: Some(routine_key_with_owner(
            owner_type.as_deref(),
            &name,
            &signature,
            scope,
        )),
        routine_signature: Some(signature),
        body_scope: None,
        unresolved_abbreviated: false,
    });
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
    let scope = scope_for_declaration(node, scope_by_span);
    let local_only = kind == SymbolKind::Parameter && scope == ROOT_SCOPE;
    let identifiers = field_identifier_nodes(node, "name");
    for identifier in identifiers {
        let name = node_text(identifier, source);
        if name.is_empty() {
            continue;
        }
        symbols.push(Symbol {
            span: Span::from_node(identifier),
            key: canonical_name(&name),
            kind,
            scope,
            owner_type: owner_type.clone(),
            type_name: type_name.clone(),
            region: region_for_node(node),
            origin: Origin::Declaration,
            local_only,
            routine_key: None,
            routine_signature: None,
            body_scope: None,
            unresolved_abbreviated: false,
        });
    }
}

fn routine_name(node: Node<'_>, source: &str) -> Option<(String, Span, Option<String>)> {
    let identifiers = field_identifier_nodes(node, "name");
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

fn routine_signature(node: Node<'_>, source: &str) -> String {
    let Some(arguments) = node.child_by_field_name("args") else {
        return String::new();
    };
    let mut types = Vec::new();
    collect_nodes(arguments, &mut |child| {
        if child.kind() != "declArg" {
            return;
        }
        let count = field_identifier_nodes(child, "name").len().max(1);
        let type_name = child
            .child_by_field_name("type")
            .and_then(|type_node| simple_type_path(type_node, source))
            .unwrap_or_else(|| "?".to_string());
        for _ in 0..count {
            types.push(type_name.clone());
        }
    });
    types.join(",")
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
                let identifiers = field_identifier_nodes(parent, "name");
                return identifiers
                    .last()
                    .map(|identifier| canonical_name(&node_text(*identifier, source)));
            }
        }
        current = parent.parent();
    }
    None
}

fn simple_type_path(node: Node<'_>, source: &str) -> Option<String> {
    let identifiers = identifier_texts(node, source);
    (!identifiers.is_empty()).then(|| canonical_path(&identifiers))
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

fn identifier_nodes<'a>(node: Node<'a>) -> Vec<Node<'a>> {
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

fn identifier_texts<'a>(node: Node<'a>, source: &str) -> Vec<String> {
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

fn identifier_at<'a>(root: Node<'a>, offset: usize) -> Option<Node<'a>> {
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

fn member_expression_at<'a>(identifier: Node<'a>) -> Option<Node<'a>> {
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
