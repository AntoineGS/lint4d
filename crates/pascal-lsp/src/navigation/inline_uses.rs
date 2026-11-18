use super::*;

#[derive(Debug)]
struct ReferencedUnit {
    uri: Url,
    name: String,
}

impl NavigationIndex {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn inline_uses_diagnostics_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        calls: &[Node<'_>],
        identifiers: &[Node<'_>],
        capacity: usize,
        diagnostic_cap_reached: &mut bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<SemanticDiagnostic>, String> {
        *diagnostic_cap_reached = false;
        if !document.conditionals.complete
            || document
                .conditionals
                .unknown_activity_requires_fail_closed()
            || !Self::document_is_complete_for_proof(document, cancel, budget)?
            || !declarations::include_expansion_complete(document)
        {
            return Ok(Vec::new());
        }
        let Some(imported_units) = self.complete_imported_units(document, cancel, budget)? else {
            return Ok(Vec::new());
        };

        let mut diagnostics = Vec::new();
        for call in calls.iter().copied().chain(
            identifiers
                .iter()
                .copied()
                .filter_map(bare_callable_expression),
        ) {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            if document.conditionals.is_unknown_at(call.start_byte())
                || document
                    .opaque_ranges
                    .iter()
                    .any(|range| range.contains(Span::from_node(call)))
                || !self.imports_are_complete_for_proof(
                    document,
                    call.start_byte(),
                    cancel,
                    budget,
                )?
            {
                continue;
            }
            let Some(selected) =
                self.selected_call_candidates(uri, document, call, true, false, cancel, budget)?
            else {
                continue;
            };
            let Some(candidate) = selected.first() else {
                continue;
            };
            let Some(target) = self.documents.get(&candidate.uri) else {
                continue;
            };
            if target.unit_name == document.unit_name
                || !target.conditionals.complete
                || target.conditionals.unknown_activity_requires_fail_closed()
                || !Self::document_is_complete_for_proof(target, cancel, budget)?
                || !declarations::include_expansion_complete(target)
            {
                continue;
            }
            let Some((declaration, definition)) =
                self.inline_routine_pair(target, candidate, cancel, budget)?
            else {
                continue;
            };
            let Some(definition_node) =
                routine_definition_node(target, definition, cancel, budget)?
            else {
                continue;
            };
            let Some(body) = definition_node.child_by_field_name("body") else {
                continue;
            };
            if body.kind() == "asm"
                || target
                    .opaque_ranges
                    .iter()
                    .any(|range| range.contains(Span::from_node(body)))
                || target.conditionals.unknown_spans.iter().any(|span| {
                    span.start < definition_node.end_byte()
                        && definition_node.start_byte() < span.end
                })
            {
                continue;
            }
            let Some(references) = self.inline_body_references(
                &candidate.uri,
                target,
                definition_node,
                cancel,
                budget,
            )?
            else {
                continue;
            };
            let caller_imports = &imported_units;
            let Some(missing) = references.into_iter().find(|referenced| {
                referenced.uri != candidate.uri
                    && referenced.uri != *uri
                    && !matches!(
                        canonical_name(&referenced.name).as_str(),
                        "system" | "sysinit"
                    )
                    && !caller_imports.contains(&referenced.uri)
            }) else {
                continue;
            };
            let Some(function_name) = inline_display_name(declaration) else {
                continue;
            };
            let Some(entity) = call
                .child_by_field_name("entity")
                .or_else(|| (call.kind() != "exprCall").then_some(call))
            else {
                continue;
            };
            let call_name = callable_lookup_identifier(entity);
            if diagnostics.len() >= capacity {
                *diagnostic_cap_reached = true;
                break;
            }
            let missing_unit_name = h2443_missing_unit_name(document, &missing.name);
            let message = DccMessage::H2443.format(&[&function_name, &missing_unit_name]);
            budget.require_owned_bytes(
                std::mem::size_of::<SemanticDiagnostic>().saturating_add(message.len()),
                cancel,
            )?;
            diagnostics.push(SemanticDiagnostic {
                kind: SemanticDiagnosticKind::Dcc(DccMessage::H2443),
                span: SourceSpan {
                    start: call_name.start_byte(),
                    end: call_name.end_byte(),
                },
                message,
            });
        }
        Ok(diagnostics)
    }

    fn complete_imported_units(
        &self,
        document: &Document,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<HashSet<Url>>, String> {
        budget.require_work(document.imports.len(), cancel)?;
        budget.require_owned_bytes(
            document
                .imports
                .len()
                .saturating_mul(std::mem::size_of::<Url>()),
            cancel,
        )?;
        let mut imported = HashSet::new();
        for import in &document.imports {
            check_navigation_cancel(cancel)?;
            let key = canonical_name(&import.name);
            if document.unknown_imports.contains(&key) {
                return Ok(None);
            }
            let urls = self.unit_urls_for_import_with_budget(document, &key, cancel, budget)?;
            if urls.len() != 1 || !self.documents.contains_key(&urls[0]) {
                return Ok(None);
            }
            imported.insert(urls[0].clone());
        }
        Ok(Some(imported))
    }

    #[allow(clippy::too_many_arguments)]
    fn selected_call_candidates(
        &self,
        uri: &Url,
        document: &Document,
        call: Node<'_>,
        allow_class_global_fallback: bool,
        allow_unique_unselected: bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<Vec<Candidate>>, String> {
        let Some(entity) = call
            .child_by_field_name("entity")
            .or_else(|| (call.kind() != "exprCall").then_some(call))
        else {
            return Ok(None);
        };
        let lookup_identifier = callable_lookup_identifier(entity);
        let mut state = ResolutionState::new();
        state.allow_class_global_fallback = allow_class_global_fallback;
        let mut candidates = self.resolve_candidates_at_with_state_and_budget(
            uri,
            document,
            lookup_identifier.start_byte(),
            lookup_identifier,
            &mut state,
            0,
            cancel,
            budget,
        )?;
        if overload::state_has_uncertainty(&state)
            || candidates
                .iter()
                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
        {
            return Ok(None);
        }
        candidates.retain(|candidate| {
            self.symbol(candidate).is_some_and(|symbol| {
                symbol.kind == SymbolKind::Routine
                    && !symbol.unresolved_abbreviated
                    && symbol.generic_parameters.is_empty()
            })
        });
        if candidates.is_empty() {
            return Ok(None);
        }
        for candidate in &candidates {
            let Some(candidate_document) = self.documents.get(&candidate.uri) else {
                return Ok(None);
            };
            if !Self::document_is_complete_for_proof(candidate_document, cancel, budget)?
                || !candidate_document.conditionals.complete
            {
                return Ok(None);
            }
        }
        let owner_instances = callable_owner_node(entity)
            .map(|owner| {
                self.resolve_receivers_with_state_and_budget(
                    uri,
                    document,
                    entity.start_byte(),
                    owner,
                    owner,
                    &mut state,
                    cancel,
                    budget,
                    0,
                )
            })
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .filter_map(|receiver| match receiver {
                Receiver::Type(instance) => Some(instance),
                Receiver::Unit(_) | Receiver::Builtin(_) | Receiver::IntegerLiteral(_) => None,
            })
            .collect::<Vec<_>>();
        if overload::state_has_uncertainty(&state) {
            return Ok(None);
        }
        let selection = overload::select(
            self,
            uri,
            document,
            call,
            &candidates,
            &GenericSubstitution::empty(),
            &owner_instances,
            &mut state,
            0,
            cancel,
            budget,
        )?;
        if overload::state_has_uncertainty(&state) {
            return Ok(None);
        }
        let Some(group) = selection.selected_group else {
            if allow_unique_unselected && candidates_identify_one_routine(self, &candidates) {
                return Ok(Some(candidates));
            }
            return Ok(None);
        };
        candidates.retain(|candidate| overload::candidate_in_group(self, candidate, &group));
        if candidates.is_empty() {
            return Ok(None);
        }
        Ok(Some(candidates))
    }

    fn inline_routine_pair<'a>(
        &self,
        document: &'a Document,
        candidate: &Candidate,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<(&'a Symbol, &'a Symbol)>, String> {
        let Some(symbol) = self.symbol(candidate) else {
            return Ok(None);
        };
        let Some(routine_key) = symbol.routine_key.as_deref() else {
            return Ok(None);
        };
        let Some(indices) = document.routine_symbol_indices.get(routine_key) else {
            return Ok(None);
        };
        let mut declaration = None;
        let mut definition = None;
        for &index in indices {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(candidate) = document.symbols.get(index) else {
                return Ok(None);
            };
            match candidate.origin {
                Origin::Declaration => {
                    if declaration.replace(candidate).is_some() {
                        return Ok(None);
                    }
                }
                Origin::Definition => {
                    if definition.replace(candidate).is_some() {
                        return Ok(None);
                    }
                }
            }
        }
        let (Some(declaration), Some(definition)) = (declaration, definition) else {
            return Ok(None);
        };
        // Generic routines and signature-only dependencies are unsupported:
        // H2443 proves unit references from a concrete routine's locals/body,
        // not from parameter or result types in its header.
        if !declaration.routine_directives.inline
            || declaration.unresolved_abbreviated
            || definition.unresolved_abbreviated
            || definition.body_scope.is_none()
            || !declaration.generic_parameters.is_empty()
            || !definition.generic_parameters.is_empty()
        {
            return Ok(None);
        }
        Ok(Some((declaration, definition)))
    }

    fn inline_body_references(
        &self,
        uri: &Url,
        document: &Document,
        definition: Node<'_>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<Vec<ReferencedUnit>>, String> {
        let mut references = Vec::new();
        for root in [
            definition.child_by_field_name("local"),
            definition.child_by_field_name("body"),
        ]
        .into_iter()
        .flatten()
        {
            let identifiers = identifier_nodes_with_budget(root, cancel, budget)?;
            for identifier in identifiers {
                check_navigation_cancel(cancel)?;
                let span = Span::from_node(identifier);
                let unknown_conditional = document.conditionals.is_unknown_at(span.start);
                let opaque = document
                    .opaque_ranges
                    .iter()
                    .any(|range| range.contains(span));
                let in_directive =
                    has_ancestor_kind_with_budget(identifier, "ppDirective", cancel, budget)?;
                if unknown_conditional || opaque || in_directive {
                    return Ok(None);
                }
                let name = node_text_with_budget(identifier, &document.source, cancel, budget)?;
                let candidates = if let Some(call) = unique_callable_identifier_call(identifier) {
                    if let Some(candidates) = self
                        .selected_call_candidates(uri, document, call, true, true, cancel, budget)?
                    {
                        candidates
                    } else {
                        let mut state = ResolutionState::new();
                        state.allow_class_global_fallback = true;
                        let candidates = self.resolve_candidates_at_with_state_and_budget(
                            uri, document, span.start, identifier, &mut state, 0, cancel, budget,
                        )?;
                        if overload::state_has_uncertainty(&state)
                            || candidates
                                .iter()
                                .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
                        {
                            return Ok(None);
                        }
                        if !candidates.is_empty()
                            && candidates.iter().all(|candidate| {
                                self.symbol(candidate)
                                    .is_some_and(|symbol| symbol.kind == SymbolKind::Type)
                            })
                        {
                            candidates
                        } else if candidates.is_empty()
                            && is_known_compiler_typecast_or_intrinsic(name)
                        {
                            continue;
                        } else {
                            return Ok(None);
                        }
                    }
                } else {
                    let mut state = ResolutionState::new();
                    state.allow_class_global_fallback = true;
                    let resolution = self.resolve_candidates_at_with_state_and_budget(
                        uri, document, span.start, identifier, &mut state, 0, cancel, budget,
                    );
                    let candidates = resolution?;
                    if candidates.is_empty()
                        && matches!(canonical_name(name).as_str(), "result" | "self")
                    {
                        continue;
                    }
                    if overload::state_has_uncertainty(&state)
                        || candidates
                            .iter()
                            .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
                    {
                        return Ok(None);
                    }
                    if candidates.is_empty() {
                        if state.compiler_surface_resolved {
                            // A compiler-surface hit cannot disambiguate a
                            // same-named identifier from an unindexed import.
                            if !self.imports_are_complete_for_proof(
                                document, span.start, cancel, budget,
                            )? {
                                return Ok(None);
                            }
                            continue;
                        }
                        return Ok(None);
                    }
                    if candidates.iter().any(|candidate| {
                        self.symbol(candidate).is_none_or(|symbol| {
                            symbol.kind == SymbolKind::Routine
                                && symbol.routine_key.as_ref().is_some_and(|key| {
                                    candidates.iter().any(|other| {
                                        self.symbol(other)
                                            .and_then(|other| other.routine_key.as_ref())
                                            .is_some_and(|other_key| other_key != key)
                                    })
                                })
                        })
                    }) {
                        return Ok(None);
                    }
                    candidates
                };
                for candidate in candidates {
                    check_navigation_cancel(cancel)?;
                    if self.symbol(&candidate).is_none() {
                        return Ok(None);
                    }
                    let Some(referenced_document) = self.documents.get(&candidate.uri) else {
                        return Ok(None);
                    };
                    if !Self::document_is_complete_for_proof(referenced_document, cancel, budget)?
                        || !referenced_document.conditionals.complete
                        || referenced_document
                            .conditionals
                            .unknown_activity_requires_fail_closed()
                    {
                        return Ok(None);
                    }
                    if !references
                        .iter()
                        .any(|reference: &ReferencedUnit| reference.uri == candidate.uri)
                    {
                        budget.require_owned_bytes(
                            std::mem::size_of::<ReferencedUnit>()
                                .saturating_add(candidate.uri.as_str().len())
                                .saturating_add(referenced_document.unit_display_name.len()),
                            cancel,
                        )?;
                        references.push(ReferencedUnit {
                            uri: candidate.uri.clone(),
                            name: referenced_document.unit_display_name.clone(),
                        });
                    }
                }
            }
        }
        Ok(Some(references))
    }
}

fn candidates_identify_one_routine(index: &NavigationIndex, candidates: &[Candidate]) -> bool {
    let Some(first) = candidates.first() else {
        return false;
    };
    let Some(routine_key) = index
        .symbol(first)
        .and_then(|symbol| symbol.routine_key.as_deref())
    else {
        return false;
    };
    candidates.iter().all(|candidate| {
        candidate.uri == first.uri
            && index
                .symbol(candidate)
                .is_some_and(|symbol| symbol.routine_key.as_deref() == Some(routine_key))
    })
}

fn is_known_compiler_typecast_or_intrinsic(name: &str) -> bool {
    // System.pas exposes these names to the compiler without declarations the
    // source index can resolve; Length is compiler intrinsic rather than RTL.
    overload::builtin_type(name).is_some()
        || matches!(
            canonical_name(name).as_str(),
            "length" | "olevariant" | "pansichar" | "pchar" | "pointer" | "pwidechar" | "variant"
        )
}

fn h2443_missing_unit_name(caller: &Document, source_name: &str) -> String {
    // Recorded dcc surfaces spell this default scope as Winapi.Windows from
    // compiler 24.0 onward, while 21.0 reports the source unit name.
    let namespaced_windows = caller
        .conditional_context
        .compiler_version
        .is_some_and(|version| version.major >= 24);
    if namespaced_windows && canonical_name(source_name) == "windows" {
        "Winapi.Windows".to_owned()
    } else {
        source_name.to_owned()
    }
}

fn unique_callable_identifier_call(identifier: Node<'_>) -> Option<Node<'_>> {
    let call = overload::call_for_identifier(identifier)?;
    let entity = call.child_by_field_name("entity")?;
    (Span::from_node(callable_lookup_identifier(entity)) == Span::from_node(identifier))
        .then_some(call)
}

fn bare_callable_expression(identifier: Node<'_>) -> Option<Node<'_>> {
    if overload::call_for_identifier(identifier).is_some() {
        return None;
    }
    let identifier_span = Span::from_node(identifier);
    let mut entity = identifier;
    while let Some(parent) = entity.tree_parent() {
        let contains_identifier = match parent.kind() {
            "exprDot" | "genericDot" | "typerefDot" => {
                Span::from_node(parent).contains(identifier_span)
            }
            "exprTpl" => parent
                .child_by_field_name("entity")
                .is_some_and(|base| Span::from_node(base).contains(identifier_span)),
            _ => false,
        };
        if !contains_identifier {
            break;
        }
        entity = parent;
    }
    (Span::from_node(callable_lookup_identifier(entity)) == identifier_span).then_some(entity)
}

fn inline_display_name(symbol: &Symbol) -> Option<String> {
    match symbol.owner_type_name.as_deref() {
        Some(owner) => Some(format!("{owner}.{}", symbol.name)),
        None => Some(symbol.name.clone()),
    }
}

fn routine_definition_node<'tree>(
    document: &'tree Document,
    definition: &Symbol,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<Node<'tree>>, String> {
    let Some(scope_index) = definition.body_scope else {
        return Ok(None);
    };
    let Some(scope) = document.scopes.get(scope_index) else {
        return Ok(None);
    };
    let Some(node) = document
        .tree
        .root_node()
        .named_descendant_for_byte_range(scope.start, scope.end)
    else {
        return Ok(None);
    };
    check_navigation_cancel(cancel)?;
    budget.require_work(1, cancel)?;
    let Some(header_span) = definition.routine_header_span else {
        return Ok(None);
    };
    if node.kind() != "defProc"
        || node.start_byte() != scope.start
        || node.end_byte() != scope.end
        || node
            .child_by_field_name("header")
            .is_none_or(|header| Span::from_node(header) != header_span)
    {
        return Ok(None);
    }
    Ok(Some(node))
}

#[cfg(test)]
mod tests {
    use super::super::*;

    const HELPER: &str = concat!(
        "unit InlineHelper;\n",
        "interface\n",
        "function HelperValue: Integer;\n",
        "implementation\n",
        "function HelperValue: Integer; begin Result := 1; end;\n",
        "end.\n",
    );

    const WINDOWS: &str = concat!(
        "unit Windows;\n",
        "interface\n",
        "function WindowsValue: Integer;\n",
        "implementation\n",
        "function WindowsValue: Integer; begin Result := 1; end;\n",
        "end.\n",
    );

    const PROVIDER: &str = concat!(
        "unit InlineProvider;\n",
        "interface\n",
        "function UsesHelper: Integer; inline;\n",
        "implementation\n",
        "uses InlineHelper;\n",
        "function UsesHelper: Integer;\n",
        "begin Result := HelperValue; end;\n",
        "end.\n",
    );

    const CALLER: &str = concat!(
        "unit InlineCaller;\n",
        "interface\n",
        "implementation\n",
        "uses InlineProvider;\n",
        "procedure P;\n",
        "var I: Integer;\n",
        "begin I := UsesHelper; end;\n",
        "end.\n",
    );

    fn indexed_diagnostics(units: &[(&str, &str)], caller_name: &str) -> Vec<SemanticDiagnostic> {
        indexed_diagnostics_with_context(units, caller_name, &ConditionalContext::default())
    }

    fn indexed_diagnostics_with_context(
        units: &[(&str, &str)],
        caller_name: &str,
        context: &ConditionalContext,
    ) -> Vec<SemanticDiagnostic> {
        let mut index = NavigationIndex::new();
        let mut caller_uri = None;
        let cancel = AtomicBool::new(false);
        for (name, source) in units {
            let uri = Url::parse(&format!("file:///tmp/{name}.pas")).expect("test URI");
            index
                .update_with_context_with_cancel(
                    uri.clone(),
                    (*source).to_owned(),
                    context,
                    &cancel,
                )
                .expect("test unit parses");
            if *name == caller_name {
                caller_uri = Some(uri);
            }
        }
        index
            .semantic_diagnostics_with_cancel(
                &caller_uri.expect("caller indexed"),
                &AtomicBool::new(false),
            )
            .expect("semantic diagnostics complete")
    }

    fn h2443(diagnostics: &[SemanticDiagnostic]) -> Vec<&SemanticDiagnostic> {
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.kind.code() == "H2443")
            .collect()
    }

    #[test]
    fn inline_use_and_hint_passes_share_the_semantic_diagnostic_cap() {
        let provider = concat!(
            "unit InlineProvider;\n",
            "interface\n",
            "function UsesHelper: Integer; inline; deprecated 'use a replacement';\n",
            "implementation\n",
            "uses InlineHelper;\n",
            "function UsesHelper: Integer;\n",
            "begin Result := HelperValue; end;\n",
            "end.\n",
        );
        let caller = concat!(
            "unit InlineCaller;\n",
            "interface\n",
            "implementation\n",
            "uses InlineProvider;\n",
            "procedure P;\n",
            "var I: Integer;\n",
            "begin\n",
            "  I := UsesHelper;\n",
            "  I := UsesHelper;\n",
            "end;\n",
            "end.\n",
        );
        let units = [
            ("InlineHelper", HELPER),
            ("InlineProvider", provider),
            ("InlineCaller", caller),
        ];
        let small_diagnostics = indexed_diagnostics(&units, "InlineCaller");
        assert!(
            small_diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind.code() == "W1000"),
            "the hint-directive pass must produce a warning for the inline call"
        );
        assert_eq!(h2443(&small_diagnostics).len(), 2);

        let mut index = NavigationIndex::new();
        let mut caller_uri = None;
        for (name, source) in units {
            let uri = Url::parse(&format!("file:///tmp/{name}.pas")).expect("test URI");
            index
                .update(uri.clone(), source.to_owned())
                .expect("test unit parses");
            if name == "InlineCaller" {
                caller_uri = Some(uri);
            }
        }
        let caller_uri = caller_uri.expect("caller indexed");
        let document = index.documents.get(&caller_uri).expect("caller indexed");
        let mut pending = vec![document.tree.root_node()];
        let mut identifiers = Vec::new();
        let mut calls = Vec::new();
        while let Some(node) = pending.pop() {
            if node.kind() == "identifier" {
                identifiers.push(node);
                continue;
            }
            if node.kind() == "exprCall" {
                calls.push(node);
            }
            let mut cursor = node.walk();
            pending.extend(
                node.children(&mut cursor)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            );
        }

        let mut report = SemanticDiagnosticReport::default();
        for _ in 0..MAX_SEMANTIC_DIAGNOSTICS - 1 {
            report.diagnostics.push(SemanticDiagnostic {
                kind: SemanticDiagnosticKind::TypeMismatch,
                span: SourceSpan { start: 0, end: 0 },
                message: String::new(),
            });
        }
        let cancel = AtomicBool::new(false);
        let mut budget = AssistanceBudget::new(100_000, 8 * 1024 * 1024, "cap regression test");
        let capacity = MAX_SEMANTIC_DIAGNOSTICS - report.diagnostics.len();
        index
            .append_inline_and_hint_directive_diagnostics_with_budget(
                &caller_uri,
                document,
                &calls,
                &identifiers,
                capacity,
                &mut report,
                &cancel,
                &mut budget,
            )
            .expect("final semantic diagnostics complete");

        assert_eq!(report.diagnostics.len(), MAX_SEMANTIC_DIAGNOSTICS);
        assert_eq!(h2443(&report.diagnostics).len(), 1);
        assert!(
            report
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.kind.code() != "W1000"),
            "once the inline-use pass fills the cap, the later pass must not append findings"
        );
    }

    #[test]
    fn reports_h2443_at_a_call_to_an_inline_routine_using_an_implementation_unit() {
        let diagnostics = indexed_diagnostics(
            &[
                ("InlineHelper", HELPER),
                ("InlineProvider", PROVIDER),
                ("InlineCaller", CALLER),
            ],
            "InlineCaller",
        );

        let findings = h2443(&diagnostics);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].message,
            "Inline function 'UsesHelper' has not been expanded because unit 'InlineHelper' is not specified in USES list"
        );
        assert_eq!(
            findings[0].span,
            SourceSpan {
                start: CALLER.find("UsesHelper").expect("call location"),
                end: CALLER.find("UsesHelper").expect("call location") + "UsesHelper".len(),
            }
        );
    }

    #[test]
    fn h2443_uses_the_compiler_scoped_name_for_windows() {
        let provider = PROVIDER
            .replace("uses InlineHelper;", "uses Windows;")
            .replace("HelperValue", "WindowsValue");

        for (version, expected_name) in [(21, "Windows"), (24, "Winapi.Windows")] {
            let context = ConditionalContext::default()
                .with_compiler_version(CompilerVersion::new(version, 0));
            let diagnostics = indexed_diagnostics_with_context(
                &[
                    ("Windows", WINDOWS),
                    ("InlineProvider", &provider),
                    ("InlineCaller", CALLER),
                ],
                "InlineCaller",
                &context,
            );
            let finding = h2443(&diagnostics).into_iter().next().unwrap();

            assert!(
                finding.message.contains(&format!("unit '{expected_name}'")),
                "version {version}: {}",
                finding.message
            );
        }
    }

    #[test]
    fn compiler_pointer_typecast_does_not_hide_inline_unit_reference() {
        let provider =
            PROVIDER.replace("Result := HelperValue", "Result := PAnsiChar(HelperValue)");
        let diagnostics = indexed_diagnostics(
            &[
                ("InlineHelper", HELPER),
                ("InlineProvider", &provider),
                ("InlineCaller", CALLER),
            ],
            "InlineCaller",
        );

        assert_eq!(h2443(&diagnostics).len(), 1);
    }

    #[test]
    fn resolved_typecast_does_not_hide_inline_unit_reference() {
        let provider = PROVIDER
            .replace(
                "function UsesHelper: Integer; inline;",
                "type TInlineCast = Integer;\nfunction UsesHelper: Integer; inline;",
            )
            .replace(
                "Result := HelperValue",
                "Result := TInlineCast(HelperValue)",
            );
        let diagnostics = indexed_diagnostics(
            &[
                ("InlineHelper", HELPER),
                ("InlineProvider", &provider),
                ("InlineCaller", CALLER),
            ],
            "InlineCaller",
        );

        assert_eq!(h2443(&diagnostics).len(), 1);
    }

    #[test]
    fn compiler_intrinsic_call_does_not_hide_inline_unit_reference() {
        let provider = PROVIDER.replace(
            "Result := HelperValue",
            "Result := Integer(Length(HelperValue))",
        );
        let diagnostics = indexed_diagnostics(
            &[
                ("InlineHelper", HELPER),
                ("InlineProvider", &provider),
                ("InlineCaller", CALLER),
            ],
            "InlineCaller",
        );

        assert_eq!(h2443(&diagnostics).len(), 1);
    }

    #[test]
    fn unresolved_call_does_not_produce_h2443() {
        let caller = CALLER.replace("UsesHelper", "UnresolvedRoutine");
        let diagnostics = indexed_diagnostics(
            &[
                ("InlineHelper", HELPER),
                ("InlineProvider", PROVIDER),
                ("InlineCaller", &caller),
            ],
            "InlineCaller",
        );

        assert!(h2443(&diagnostics).is_empty());
    }

    #[test]
    fn ambiguous_provider_lookup_does_not_produce_h2443() {
        let provider_a = PROVIDER.replace("InlineProvider", "InlineProviderA");
        let provider_b = PROVIDER
            .replace("InlineProvider", "InlineProviderB")
            .replace("InlineHelper", "InlineHelperB");
        let helper_b = HELPER.replace("InlineHelper", "InlineHelperB");
        let caller = CALLER.replace("InlineProvider;", "InlineProviderA, InlineProviderB;");
        let diagnostics = indexed_diagnostics(
            &[
                ("InlineHelper", HELPER),
                ("InlineHelperB", &helper_b),
                ("InlineProviderA", &provider_a),
                ("InlineProviderB", &provider_b),
                ("InlineCaller", &caller),
            ],
            "InlineCaller",
        );

        assert!(h2443(&diagnostics).is_empty());
    }

    #[test]
    fn incomplete_provider_does_not_produce_h2443() {
        let incomplete_provider = PROVIDER.trim_end_matches("end.\n");
        let diagnostics = indexed_diagnostics(
            &[
                ("InlineHelper", HELPER),
                ("InlineProvider", incomplete_provider),
                ("InlineCaller", CALLER),
            ],
            "InlineCaller",
        );

        assert!(h2443(&diagnostics).is_empty());
    }

    #[test]
    fn compiler_surface_miss_with_an_incomplete_provider_import_is_silent() {
        // MissingProviderImport is deliberately unindexed and may export a
        // TObject that shadows the compiler-surface name; HelperValue resolves
        // through indexed InlineHelper and must not leak a claim.
        let provider = PROVIDER
            .replace(
                "uses InlineHelper;",
                "uses MissingProviderImport, InlineHelper;",
            )
            .replace(
                "Result := HelperValue",
                "if TObject = nil then Result := HelperValue",
            );
        let key = super::super::dcc_surface::SurfaceKey {
            major: 37,
            minor: 0,
            platform: super::super::dcc_surface::SurfacePlatform::Win32,
        };
        let table = super::super::dcc_surface::table_for(&key).unwrap();
        let context = super::super::dcc_surface::verified_system_context(&key, &table);
        let diagnostics = indexed_diagnostics_with_context(
            &[
                ("InlineHelper", HELPER),
                ("InlineProvider", &provider),
                ("InlineCaller", CALLER),
            ],
            "InlineCaller",
            &context,
        );

        assert!(h2443(&diagnostics).is_empty());
    }

    #[test]
    fn complete_provider_imports_allow_compiler_surface_misses() {
        let provider = PROVIDER.replace(
            "Result := HelperValue",
            "if TObject = nil then Result := HelperValue",
        );
        let key = super::super::dcc_surface::SurfaceKey {
            major: 37,
            minor: 0,
            platform: super::super::dcc_surface::SurfacePlatform::Win32,
        };
        let table = super::super::dcc_surface::table_for(&key).unwrap();
        let context = super::super::dcc_surface::verified_system_context(&key, &table);
        let diagnostics = indexed_diagnostics_with_context(
            &[
                ("InlineHelper", HELPER),
                ("InlineProvider", &provider),
                ("InlineCaller", CALLER),
            ],
            "InlineCaller",
            &context,
        );

        assert_eq!(h2443(&diagnostics).len(), 1);
    }

    #[test]
    fn parameter_type_only_dependency_does_not_produce_h2443() {
        let helper = HELPER.replace(
            "interface\nfunction HelperValue",
            "interface\ntype THelperArgument = Integer;\nfunction HelperValue",
        );
        let provider = concat!(
            "unit InlineProvider;\n",
            "interface\nuses InlineHelper;\n",
            "procedure UsesHelper(Value: THelperArgument); inline;\n",
            "implementation\n",
            "procedure UsesHelper(Value: THelperArgument); begin end;\n",
            "end.\n",
        );
        let caller = concat!(
            "unit InlineCaller;\n",
            "interface\nimplementation\nuses InlineProvider;\n",
            "procedure P;\nbegin UsesHelper(1); end;\nend.\n",
        );
        let diagnostics = indexed_diagnostics(
            &[
                ("InlineHelper", &helper),
                ("InlineProvider", provider),
                ("InlineCaller", caller),
            ],
            "InlineCaller",
        );

        assert!(h2443(&diagnostics).is_empty());
    }

    #[test]
    fn generic_inline_routines_do_not_produce_h2443() {
        let provider = concat!(
            "unit InlineProvider;\n",
            "interface\n",
            "function UsesHelper<T>(Value: T): Integer; inline;\n",
            "implementation\n",
            "uses InlineHelper;\n",
            "function UsesHelper<T>(Value: T): Integer;\n",
            "begin Result := HelperValue; end;\n",
            "end.\n",
        );
        let caller = concat!(
            "unit InlineCaller;\n",
            "interface\nimplementation\nuses InlineProvider;\n",
            "procedure P;\nvar I: Integer;\nbegin I := UsesHelper<Integer>(1); end;\nend.\n",
        );
        let diagnostics = indexed_diagnostics(
            &[
                ("InlineHelper", HELPER),
                ("InlineProvider", provider),
                ("InlineCaller", caller),
            ],
            "InlineCaller",
        );

        assert!(h2443(&diagnostics).is_empty());
    }

    #[test]
    fn unknown_conditional_in_inline_body_does_not_produce_h2443() {
        let provider = PROVIDER.replace(
            "begin Result := HelperValue; end;",
            concat!(
                "begin\n",
                "{$IFDEF H2443_UNKNOWN}\n",
                "  Result := HelperValue;\n",
                "{$ELSE}\n",
                "  Result := 0;\n",
                "{$ENDIF}\n",
                "end;",
            ),
        );
        let diagnostics = indexed_diagnostics(
            &[
                ("InlineHelper", HELPER),
                ("InlineProvider", &provider),
                ("InlineCaller", CALLER),
            ],
            "InlineCaller",
        );

        assert!(h2443(&diagnostics).is_empty());
    }

    #[test]
    fn asm_inline_body_does_not_produce_h2443() {
        let provider = PROVIDER.replace(
            "begin Result := HelperValue; end;",
            "asm\n  mov eax, 1\nend;",
        );
        let diagnostics = indexed_diagnostics(
            &[
                ("InlineHelper", HELPER),
                ("InlineProvider", &provider),
                ("InlineCaller", CALLER),
            ],
            "InlineCaller",
        );

        assert!(h2443(&diagnostics).is_empty());
    }
}
