use super::*;
use pascal_core::facts::{IncludeSlotOutcome, include_slots};
use std::collections::HashMap;

type RoutineGroupKey = (Option<String>, String, Option<usize>);
type RedeclarationGroupKey = (Option<String>, usize, String, usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclarationComparison {
    Same,
    Differs,
    Unsupported,
    Unknown,
}

impl NavigationIndex {
    #[allow(clippy::too_many_lines)]
    pub(super) fn declaration_diagnostics_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        capacity: usize,
        diagnostic_cap_reached: &mut bool,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<SemanticDiagnostic>, String> {
        *diagnostic_cap_reached = false;
        let mut declarations_by_name = HashMap::<RoutineGroupKey, Vec<usize>>::new();
        let mut definitions_by_name = HashMap::<RoutineGroupKey, Vec<usize>>::new();
        budget.require_work(document.symbols.len(), cancel)?;
        let group_storage_bytes = document
            .symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Routine)
            .fold(0usize, |bytes, symbol| {
                bytes
                    .saturating_add(std::mem::size_of::<RoutineGroupKey>())
                    .saturating_add(std::mem::size_of::<usize>())
                    .saturating_add(symbol.key.len())
                    .saturating_add(symbol.owner_type.as_ref().map_or(0, String::len))
            });
        budget.require_owned_bytes(group_storage_bytes, cancel)?;
        for (index, symbol) in document.symbols.iter().enumerate() {
            if symbol.kind != SymbolKind::Routine {
                continue;
            }
            let key = routine_group_key(symbol);
            match symbol.origin {
                Origin::Declaration => declarations_by_name.entry(key).or_default().push(index),
                Origin::Definition => definitions_by_name.entry(key).or_default().push(index),
            }
        }
        let oversized_group = declarations_by_name.iter().any(|(key, declarations)| {
            declarations
                .len()
                .saturating_add(definitions_by_name.get(key).map_or(0, Vec::len))
                > overload::MAX_OVERLOAD_GROUPS
        }) || definitions_by_name.iter().any(|(key, definitions)| {
            !declarations_by_name.contains_key(key)
                && definitions.len() > overload::MAX_OVERLOAD_GROUPS
        });
        if oversized_group {
            return Err(
                "routine declaration group exceeds the supported overload bound".to_owned(),
            );
        }

        let proven_end = document
            .parser_recovery_spans
            .iter()
            .map(|span| span.start)
            .min()
            .unwrap_or(usize::MAX);
        let mut diagnostics = Vec::new();

        if document.conditionals.complete
            && !document
                .conditionals
                .unknown_activity_requires_fail_closed()
            && include_expansion_complete(document)
        {
            for (declaration_index, declaration) in document.symbols.iter().enumerate() {
                check_navigation_cancel(cancel)?;
                if declaration.kind != SymbolKind::Routine
                    || declaration.origin != Origin::Declaration
                    || declaration.routine_key.is_none()
                    || declaration.unresolved_abbreviated
                    || !symbol_is_proven(declaration, proven_end)
                    || symbol_conditional_state_is_unknown(document, declaration_index)
                    || declaration.routine_directives.abstract_
                    || declaration.routine_directives.external
                {
                    continue;
                }
                budget.require_work(1, cancel)?;

                let declaration_is_e2065_candidate =
                    if let Some(owner) = declaration.owner_type.as_deref() {
                        if method_resolution_declaration(document, declaration) {
                            false
                        } else {
                            matches!(
                                owner_type_kind(document, owner),
                                Some(TypeKind::Class | TypeKind::Record)
                            )
                        }
                    } else {
                        (declaration.region == Region::Interface && declaration.scope == ROOT_SCOPE)
                            || (declaration.region == Region::Implementation
                                && declaration.routine_directives.forward)
                    };
                if !declaration_is_e2065_candidate {
                    continue;
                }

                let key = routine_group_key(declaration);
                let mut has_matching_definition = false;
                let mut has_uncertain_definition = false;
                if let Some(definition_indices) = definitions_by_name.get(&key) {
                    for &definition_index in definition_indices {
                        budget.require_work(1, cancel)?;
                        let Some(definition) = document.symbols.get(definition_index) else {
                            has_uncertain_definition = true;
                            continue;
                        };
                        if definition.unresolved_abbreviated
                            || !symbol_is_proven(definition, proven_end)
                            || symbol_conditional_state_is_unknown(document, definition_index)
                        {
                            has_uncertain_definition = true;
                            continue;
                        }
                        match self.definition_satisfies_declaration(
                            uri,
                            declaration,
                            definition,
                            cancel,
                            budget,
                        )? {
                            ContractMatch::Yes => has_matching_definition = true,
                            ContractMatch::Unknown => has_uncertain_definition = true,
                            ContractMatch::No => {}
                        }
                    }
                }
                if let Some(candidate_indices) = declarations_by_name.get(&key) {
                    for &candidate_index in candidate_indices {
                        if candidate_index == declaration_index {
                            continue;
                        }
                        let Some(candidate) = document.symbols.get(candidate_index) else {
                            has_uncertain_definition = true;
                            continue;
                        };
                        if candidate.region != Region::Implementation
                            || !candidate.routine_directives.external
                        {
                            continue;
                        }
                        budget.require_work(1, cancel)?;
                        if candidate.unresolved_abbreviated
                            || !symbol_is_proven(candidate, proven_end)
                            || symbol_conditional_state_is_unknown(document, candidate_index)
                        {
                            has_uncertain_definition = true;
                            continue;
                        }
                        match self.definition_satisfies_declaration(
                            uri,
                            declaration,
                            candidate,
                            cancel,
                            budget,
                        )? {
                            ContractMatch::Yes => has_matching_definition = true,
                            ContractMatch::Unknown => has_uncertain_definition = true,
                            ContractMatch::No => {}
                        }
                    }
                }
                if has_matching_definition || has_uncertain_definition {
                    continue;
                }

                let Some(name) = e2065_display_name(document, declaration) else {
                    continue;
                };
                let message = DccMessage::E2065.format(&[&name]);
                if !push_diagnostic(
                    &mut diagnostics,
                    capacity,
                    DccMessage::E2065,
                    declaration.selection_span,
                    message,
                    cancel,
                    budget,
                )? {
                    *diagnostic_cap_reached = true;
                    return Ok(diagnostics);
                }
            }
        }

        if !document.conditionals.complete || !include_expansion_complete(document) {
            return Ok(diagnostics);
        }
        for (definition_index, definition) in document.symbols.iter().enumerate() {
            check_navigation_cancel(cancel)?;
            if definition.kind != SymbolKind::Routine
                || definition.origin != Origin::Definition
                || definition.unresolved_abbreviated
                || !symbol_is_proven(definition, proven_end)
                || symbol_conditional_state_is_unknown(document, definition_index)
            {
                continue;
            }
            budget.require_work(1, cancel)?;
            let key = routine_group_key(definition);
            let Some(declaration_indices) = declarations_by_name.get(&key) else {
                continue;
            };
            if definition.routine_signature.as_deref() == Some("")
                && !declaration_indices.iter().any(|index| {
                    document
                        .symbols
                        .get(*index)
                        .is_some_and(|symbol| symbol.routine_signature.as_deref() == Some(""))
                })
            {
                continue;
            }
            let Some(header_span) = definition.routine_header_span else {
                continue;
            };
            let mut comparisons = Vec::new();
            for &declaration_index in declaration_indices {
                budget.require_work(1, cancel)?;
                let Some(declaration) = document.symbols.get(declaration_index) else {
                    comparisons.push(DeclarationComparison::Unknown);
                    continue;
                };
                if declaration.selection_span.start >= header_span.start {
                    continue;
                }
                if !symbol_is_proven(declaration, proven_end)
                    || symbol_conditional_state_is_unknown(document, declaration_index)
                {
                    comparisons.push(DeclarationComparison::Unknown);
                    continue;
                }
                if declaration.routine_directives.overload {
                    comparisons.push(DeclarationComparison::Unsupported);
                    continue;
                }
                comparisons.push(self.compare_declaration_and_definition(
                    uri,
                    declaration,
                    definition,
                    cancel,
                    budget,
                )?);
            }
            if comparisons.is_empty()
                || comparisons
                    .iter()
                    .any(|comparison| *comparison != DeclarationComparison::Differs)
            {
                continue;
            }

            let message = DccMessage::E2037.format(&[&definition.name]);
            if !push_diagnostic(
                &mut diagnostics,
                capacity,
                DccMessage::E2037,
                definition.selection_span,
                message,
                cancel,
                budget,
            )? {
                *diagnostic_cap_reached = true;
                return Ok(diagnostics);
            }
        }

        if document.conditionals.complete
            && !document
                .conditionals
                .unknown_activity_requires_fail_closed()
            && include_expansion_complete(document)
            && document.parser_recovery_spans.is_empty()
        {
            budget.require_work(document.symbols.len(), cancel)?;
            let candidate_storage_bytes = document.symbols.iter().fold(0usize, |bytes, symbol| {
                if h2219_candidate_kind(symbol)
                    && symbol.owner_type.is_some()
                    && matches!(
                        symbol.visibility,
                        Visibility::Private | Visibility::StrictPrivate
                    )
                {
                    bytes
                        .saturating_add(std::mem::size_of::<usize>() * 3)
                        .saturating_add(symbol.key.len())
                } else {
                    bytes
                }
            });
            budget.require_owned_bytes(candidate_storage_bytes, cancel)?;

            let mut candidates_by_name = HashMap::<String, Vec<usize>>::new();
            let mut candidate_indices = Vec::new();
            let mut definition_headers = HashMap::<usize, Span>::new();
            for (candidate_index, candidate) in document.symbols.iter().enumerate() {
                check_navigation_cancel(cancel)?;
                let Some(owner_type) = candidate.owner_type.as_deref() else {
                    continue;
                };
                if !h2219_candidate_kind(candidate)
                    || candidate.origin != Origin::Declaration
                    || !matches!(
                        candidate.visibility,
                        Visibility::Private | Visibility::StrictPrivate
                    )
                    || !symbol_is_proven(candidate, proven_end)
                    || symbol_conditional_state_is_unknown(document, candidate_index)
                    || !h2219_owner_is_proven_class(document, owner_type, proven_end)
                {
                    continue;
                }

                if candidate.kind == SymbolKind::Routine {
                    if candidate.routine_directives.abstract_
                        || candidate.routine_directives.external
                    {
                        continue;
                    }
                    let Some(routine_key) = candidate.routine_key.as_ref() else {
                        continue;
                    };
                    let Some(routine_indices) = document.routine_symbol_indices.get(routine_key)
                    else {
                        continue;
                    };
                    let mut declarations = 0usize;
                    let mut definition_index = None;
                    let mut definition_count = 0usize;
                    for &routine_index in routine_indices {
                        budget.require_work(1, cancel)?;
                        let Some(routine) = document.symbols.get(routine_index) else {
                            continue;
                        };
                        if routine.owner_type.as_deref() != Some(owner_type)
                            || routine.key != candidate.key
                        {
                            continue;
                        }
                        match routine.origin {
                            Origin::Declaration => declarations += 1,
                            Origin::Definition => {
                                definition_count += 1;
                                definition_index = Some(routine_index);
                            }
                        }
                    }
                    if declarations != 1 || definition_count != 1 {
                        continue;
                    }
                    let Some(definition_index) = definition_index else {
                        continue;
                    };
                    let Some(definition) = document.symbols.get(definition_index) else {
                        continue;
                    };
                    let Some(header) = definition.routine_header_span else {
                        continue;
                    };
                    if definition.body_scope.is_none()
                        || definition.unresolved_abbreviated
                        || !symbol_is_proven(definition, proven_end)
                        || symbol_conditional_state_is_unknown(document, definition_index)
                    {
                        continue;
                    }
                    definition_headers.insert(candidate_index, header);
                }

                candidates_by_name
                    .entry(candidate.key.clone())
                    .or_default()
                    .push(candidate_index);
                candidate_indices.push(candidate_index);
            }

            let identifiers = if candidate_indices.is_empty() {
                Vec::new()
            } else {
                identifier_nodes_with_budget(document.tree.root_node(), cancel, budget)?
            };
            let proof_slots = if candidate_indices.is_empty() {
                0
            } else {
                document.symbols.len()
            };
            budget.require_owned_bytes(
                proof_slots.saturating_mul(std::mem::size_of::<bool>() * 2),
                cancel,
            )?;
            let mut used = vec![false; proof_slots];
            let mut uncertain = vec![false; proof_slots];
            for identifier in identifiers {
                check_navigation_cancel(cancel)?;
                let text = node_text_with_budget(identifier, &document.source, cancel, budget)?;
                budget.require_owned_bytes(text.len(), cancel)?;
                let key = canonical_name(text);
                let Some(matching_candidates) = candidates_by_name.get(&key) else {
                    continue;
                };
                budget.require_work(matching_candidates.len(), cancel)?;
                if has_ancestor_kind_with_budget(identifier, "ppDirective", cancel, budget)? {
                    continue;
                }
                let span = Span::from_node(identifier);
                let has_relevant_candidate = matching_candidates.iter().any(|candidate_index| {
                    let Some(candidate) = document.symbols.get(*candidate_index) else {
                        return false;
                    };
                    !candidate.declaration_span.contains(span)
                        && !definition_headers
                            .get(candidate_index)
                            .is_some_and(|header| header.contains(span))
                });
                if !has_relevant_candidate {
                    continue;
                }
                budget.require_work(document.opaque_ranges.len(), cancel)?;

                if document
                    .opaque_ranges
                    .iter()
                    .any(|opaque| opaque.contains(span))
                {
                    for &candidate_index in matching_candidates {
                        let Some(candidate) = document.symbols.get(candidate_index) else {
                            continue;
                        };
                        if !candidate.declaration_span.contains(span)
                            && !definition_headers
                                .get(&candidate_index)
                                .is_some_and(|header| header.contains(span))
                        {
                            uncertain[candidate_index] = true;
                        }
                    }
                    continue;
                }

                let mut state = ResolutionState::new();
                let targets = self.resolve_candidates_at_with_state_and_budget(
                    uri, document, span.start, identifier, &mut state, 0, cancel, budget,
                )?;
                budget.require_work(targets.len(), cancel)?;
                let resolution_is_uncertain = targets.is_empty()
                    || state.receiver_resolution_uncertain()
                    || state.member_lookup_incomplete
                    || state.implicit_system_namespace_incomplete
                    || state.inaccessible_candidate
                    || state.ambiguous
                    || self.is_unknown_global_fallback(document, identifier, span.start, &targets)
                    || targets.iter().any(|target| {
                        self.candidate_is_conditionally_unknown(target)
                            || self.symbol(target).is_none_or(|symbol| {
                                let Some(target_document) = self.documents.get(&target.uri) else {
                                    return true;
                                };
                                let target_proven_end = target_document
                                    .parser_recovery_spans
                                    .iter()
                                    .map(|recovery| recovery.start)
                                    .min()
                                    .unwrap_or(usize::MAX);
                                symbol.unresolved_abbreviated
                                    || !symbol_is_proven(symbol, target_proven_end)
                            })
                    });
                if resolution_is_uncertain {
                    for &candidate_index in matching_candidates {
                        if document
                            .symbols
                            .get(candidate_index)
                            .is_some_and(|candidate| {
                                !candidate.declaration_span.contains(span)
                                    && !definition_headers
                                        .get(&candidate_index)
                                        .is_some_and(|header| header.contains(span))
                            })
                        {
                            uncertain[candidate_index] = true;
                        }
                    }
                    continue;
                }

                budget.require_work(
                    matching_candidates.len().saturating_mul(targets.len()),
                    cancel,
                )?;
                for &candidate_index in matching_candidates {
                    let Some(candidate) = document.symbols.get(candidate_index) else {
                        continue;
                    };
                    if candidate.declaration_span.contains(span)
                        || definition_headers
                            .get(&candidate_index)
                            .is_some_and(|header| header.contains(span))
                    {
                        continue;
                    }
                    // Delphi counts recursive calls, so only the declaration and header are excluded.
                    let same_member_targets = targets
                        .iter()
                        .filter(|target| {
                            h2219_target_matches_member(
                                uri,
                                candidate_index,
                                candidate,
                                target,
                                self,
                            )
                        })
                        .count();
                    if same_member_targets == targets.len() {
                        used[candidate_index] = true;
                    } else if same_member_targets > 0 {
                        uncertain[candidate_index] = true;
                    }
                }
            }

            for candidate_index in candidate_indices {
                check_navigation_cancel(cancel)?;
                if used[candidate_index] || uncertain[candidate_index] {
                    continue;
                }
                let Some(candidate) = document.symbols.get(candidate_index) else {
                    continue;
                };
                let message = DccMessage::H2219.format(&[&candidate.name]);
                if !push_diagnostic(
                    &mut diagnostics,
                    capacity,
                    DccMessage::H2219,
                    candidate.selection_span,
                    message,
                    cancel,
                    budget,
                )? {
                    *diagnostic_cap_reached = true;
                    return Ok(diagnostics);
                }
            }
        }

        if document.conditionals.complete
            && !document
                .conditionals
                .unknown_activity_requires_fail_closed()
            && include_expansion_complete(document)
        {
            let mut declarations_by_scope = HashMap::<RedeclarationGroupKey, Vec<usize>>::new();
            let mut routines_by_scope = HashMap::<RedeclarationGroupKey, Vec<usize>>::new();
            budget.require_work(document.symbols.len(), cancel)?;
            let group_storage_bytes = document.symbols.iter().fold(0usize, |bytes, symbol| {
                let entry_bytes = std::mem::size_of::<RedeclarationGroupKey>()
                    .saturating_add(std::mem::size_of::<usize>())
                    .saturating_add(symbol.key.len())
                    .saturating_add(symbol.owner_type.as_ref().map_or(0, String::len));
                let declaration_entries = usize::from(is_redeclaration_candidate(symbol));
                let routine_entries = usize::from(symbol.kind == SymbolKind::Routine);
                bytes.saturating_add(
                    entry_bytes.saturating_mul(declaration_entries + routine_entries),
                )
            });
            budget.require_owned_bytes(group_storage_bytes, cancel)?;
            for (index, symbol) in document.symbols.iter().enumerate() {
                check_navigation_cancel(cancel)?;
                let key = redeclaration_group_key(symbol);
                if is_redeclaration_candidate(symbol) {
                    declarations_by_scope
                        .entry(key.clone())
                        .or_default()
                        .push(index);
                }
                if symbol.kind == SymbolKind::Routine {
                    routines_by_scope.entry(key).or_default().push(index);
                }
            }
            if declarations_by_scope
                .values()
                .any(|indices| indices.len() > overload::MAX_OVERLOAD_GROUPS)
                || routines_by_scope
                    .values()
                    .any(|indices| indices.len() > overload::MAX_OVERLOAD_GROUPS)
            {
                return Err("declaration group exceeds the supported overload bound".to_owned());
            }

            let mut declaration_groups = declarations_by_scope.into_values().collect::<Vec<_>>();
            declaration_groups.sort_unstable_by_key(|indices| {
                indices
                    .iter()
                    .filter_map(|index| document.symbols.get(*index))
                    .map(|symbol| symbol.selection_span.start)
                    .min()
                    .unwrap_or(usize::MAX)
            });
            for mut indices in declaration_groups {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if !declaration_group_is_proven(document, &indices, proven_end) {
                    continue;
                }
                indices.sort_unstable_by_key(|index| {
                    document
                        .symbols
                        .get(*index)
                        .map_or(usize::MAX, |symbol| symbol.selection_span.start)
                });
                if has_forward_class_pair(document, &indices) {
                    indices.retain(|index| {
                        document
                            .symbols
                            .get(*index)
                            .is_none_or(|symbol| !is_forward_class_declaration(document, symbol))
                    });
                }
                if indices.len() < 2 {
                    continue;
                }
                let all_routines = indices.iter().all(|index| {
                    document
                        .symbols
                        .get(*index)
                        .is_some_and(|symbol| symbol.kind == SymbolKind::Routine)
                });
                if all_routines {
                    let Some(owner) = indices
                        .first()
                        .and_then(|index| document.symbols.get(*index))
                        .and_then(|symbol| symbol.owner_type.as_deref())
                    else {
                        continue;
                    };
                    if !matches!(
                        owner_type_kind(document, owner),
                        Some(TypeKind::Class | TypeKind::Record)
                    ) {
                        continue;
                    }
                    let mut duplicate_pair = None;
                    'pairs: for second_position in 1..indices.len() {
                        let second_index = indices[second_position];
                        let Some(second) = document.symbols.get(second_index) else {
                            continue;
                        };
                        for &first_index in &indices[..second_position] {
                            budget.require_work(1, cancel)?;
                            let Some(first) = document.symbols.get(first_index) else {
                                continue;
                            };
                            if self.duplicate_routine_parameters_match(
                                uri, first, second, cancel, budget,
                            )? == ContractMatch::Yes
                            {
                                duplicate_pair = Some(second_index);
                                break 'pairs;
                            }
                        }
                    }
                    if let Some(index) = duplicate_pair {
                        let symbol = &document.symbols[index];
                        let message = DccMessage::E2252.format(&[&symbol.name]);
                        if !push_diagnostic(
                            &mut diagnostics,
                            capacity,
                            DccMessage::E2252,
                            symbol.selection_span,
                            message,
                            cancel,
                            budget,
                        )? {
                            *diagnostic_cap_reached = true;
                            return Ok(diagnostics);
                        }
                    }
                    continue;
                }

                let second = &document.symbols[indices[1]];
                let message = DccMessage::E2004.format(&[&second.name]);
                if !push_diagnostic(
                    &mut diagnostics,
                    capacity,
                    DccMessage::E2004,
                    second.selection_span,
                    message,
                    cancel,
                    budget,
                )? {
                    *diagnostic_cap_reached = true;
                    return Ok(diagnostics);
                }
            }

            let mut routine_groups = routines_by_scope.into_values().collect::<Vec<_>>();
            routine_groups.sort_unstable_by_key(|indices| {
                indices
                    .iter()
                    .filter_map(|index| document.symbols.get(*index))
                    .map(|symbol| symbol.selection_span.start)
                    .min()
                    .unwrap_or(usize::MAX)
            });
            for mut indices in routine_groups {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if indices.len() < 2 || !declaration_group_is_proven(document, &indices, proven_end)
                {
                    continue;
                }
                indices.sort_unstable_by_key(|index| {
                    document
                        .symbols
                        .get(*index)
                        .map_or(usize::MAX, |symbol| symbol.selection_span.start)
                });
                for position in 0..indices.len() {
                    check_navigation_cancel(cancel)?;
                    let current_index = indices[position];
                    let Some(current) = document.symbols.get(current_index) else {
                        continue;
                    };
                    if current.routine_directives.overload {
                        continue;
                    }
                    let mut evidence_end = position;
                    if current.origin == Origin::Definition {
                        let mut matching_declaration = None;
                        for (declaration_position, &declaration_index) in
                            indices[..position].iter().enumerate()
                        {
                            let Some(declaration) = document.symbols.get(declaration_index) else {
                                continue;
                            };
                            if declaration.origin != Origin::Declaration {
                                continue;
                            }
                            budget.require_work(1, cancel)?;
                            if self.duplicate_routine_parameters_match(
                                uri,
                                declaration,
                                current,
                                cancel,
                                budget,
                            )? == ContractMatch::Yes
                            {
                                matching_declaration = Some((declaration_position, declaration));
                                break;
                            }
                        }
                        if let Some((declaration_position, declaration)) = matching_declaration {
                            if declaration.routine_directives.overload {
                                continue;
                            }
                            evidence_end = declaration_position;
                        }
                    }
                    let mut missing_overload = false;
                    for &previous_index in &indices[..evidence_end] {
                        budget.require_work(1, cancel)?;
                        let Some(previous) = document.symbols.get(previous_index) else {
                            continue;
                        };
                        if !previous.routine_directives.overload {
                            continue;
                        }
                        if self.duplicate_routine_parameters_match(
                            uri, previous, current, cancel, budget,
                        )? == ContractMatch::No
                        {
                            missing_overload = true;
                            break;
                        }
                    }
                    if !missing_overload {
                        continue;
                    }
                    let message = DccMessage::E2254.format(&[&current.name]);
                    if !push_diagnostic(
                        &mut diagnostics,
                        capacity,
                        DccMessage::E2254,
                        current.selection_span,
                        message,
                        cancel,
                        budget,
                    )? {
                        *diagnostic_cap_reached = true;
                        return Ok(diagnostics);
                    }
                }
            }
        }

        Ok(diagnostics)
    }

    fn duplicate_routine_parameters_match(
        &self,
        uri: &Url,
        left: &Symbol,
        right: &Symbol,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<ContractMatch, String> {
        if left.routine_kind != right.routine_kind
            || left.is_static != right.is_static
            || !left.generic_parameters.is_empty()
            || !right.generic_parameters.is_empty()
        {
            return Ok(ContractMatch::Unknown);
        }
        if left.routine_parameters.len() != right.routine_parameters.len() {
            return Ok(ContractMatch::No);
        }

        let empty = GenericSubstitution::empty();
        let mut unknown = false;
        for (left_parameter, right_parameter) in left
            .routine_parameters
            .iter()
            .zip(&right.routine_parameters)
        {
            if left_parameter.mode != right_parameter.mode {
                return Ok(ContractMatch::No);
            }
            match self.contract_parameter_type_match(
                uri,
                left_parameter.type_ref.as_ref(),
                &empty,
                uri,
                right_parameter.type_ref.as_ref(),
                &empty,
                cancel,
                budget,
            )? {
                ContractMatch::Yes => {}
                ContractMatch::No => return Ok(ContractMatch::No),
                ContractMatch::Unknown => unknown = true,
            }
        }
        Ok(if unknown {
            ContractMatch::Unknown
        } else {
            ContractMatch::Yes
        })
    }

    fn definition_satisfies_declaration(
        &self,
        uri: &Url,
        declaration: &Symbol,
        definition: &Symbol,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<ContractMatch, String> {
        if declaration.routine_kind != definition.routine_kind {
            return Ok(ContractMatch::Unknown);
        }
        if declaration.is_static != definition.is_static {
            return Ok(ContractMatch::No);
        }
        if declaration.routine_key == definition.routine_key {
            return Ok(ContractMatch::Yes);
        }
        if declaration.routine_signature.as_deref() == Some("")
            || definition.routine_signature.as_deref() == Some("")
            || !declaration.generic_parameters.is_empty()
            || !definition.generic_parameters.is_empty()
            || declaration.routine_parameters.len() != definition.routine_parameters.len()
        {
            return Ok(ContractMatch::Unknown);
        }

        let empty = GenericSubstitution::empty();
        let mut unknown = false;
        for (declared, implemented) in declaration
            .routine_parameters
            .iter()
            .zip(&definition.routine_parameters)
        {
            match self.contract_parameter_type_match(
                uri,
                declared.type_ref.as_ref(),
                &empty,
                uri,
                implemented.type_ref.as_ref(),
                &empty,
                cancel,
                budget,
            )? {
                ContractMatch::Yes => {}
                ContractMatch::No => return Ok(ContractMatch::No),
                ContractMatch::Unknown => unknown = true,
            }
        }
        Ok(if unknown {
            ContractMatch::Unknown
        } else {
            ContractMatch::Yes
        })
    }

    fn compare_declaration_and_definition(
        &self,
        uri: &Url,
        declaration: &Symbol,
        definition: &Symbol,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<DeclarationComparison, String> {
        if declaration.routine_kind != definition.routine_kind
            || !declaration.generic_parameters.is_empty()
            || !definition.generic_parameters.is_empty()
            || declaration.routine_directives.calling_convention_unknown
            || definition.routine_directives.calling_convention_unknown
            || declaration.routine_directives.calling_convention
                != definition.routine_directives.calling_convention
        {
            return Ok(DeclarationComparison::Unknown);
        }
        if declaration.routine_parameters.len() != definition.routine_parameters.len() {
            return Ok(DeclarationComparison::Unsupported);
        }

        let empty = GenericSubstitution::empty();
        let mut differs = declaration.is_static != definition.is_static;
        for (declared, implemented) in declaration
            .routine_parameters
            .iter()
            .zip(&definition.routine_parameters)
        {
            if declared.has_default || implemented.has_default {
                return Ok(DeclarationComparison::Unknown);
            }
            match self.contract_parameter_type_match(
                uri,
                declared.type_ref.as_ref(),
                &empty,
                uri,
                implemented.type_ref.as_ref(),
                &empty,
                cancel,
                budget,
            )? {
                ContractMatch::Yes => {}
                ContractMatch::No => return Ok(DeclarationComparison::Unsupported),
                ContractMatch::Unknown => return Ok(DeclarationComparison::Unknown),
            }
            differs |= declared.mode != implemented.mode;
            let declared_name = document_text(uri, declared.span, self)?;
            let implemented_name = document_text(uri, implemented.span, self)?;
            differs |= !declared_name.eq_ignore_ascii_case(&implemented_name);
        }

        match declaration.routine_kind {
            RoutineKind::Function | RoutineKind::Operator => {
                match self.contract_parameter_type_match(
                    uri,
                    declaration.result_type_ref.as_ref(),
                    &empty,
                    uri,
                    definition.implementation_result_type_ref.as_ref(),
                    &empty,
                    cancel,
                    budget,
                )? {
                    ContractMatch::Yes => {}
                    ContractMatch::No => differs = true,
                    ContractMatch::Unknown => return Ok(DeclarationComparison::Unknown),
                }
            }
            RoutineKind::Procedure | RoutineKind::Constructor | RoutineKind::Destructor => {
                if declaration.result_type_ref.is_some() || definition.result_type_ref.is_some() {
                    return Ok(DeclarationComparison::Unknown);
                }
            }
        }

        Ok(if differs {
            DeclarationComparison::Differs
        } else {
            DeclarationComparison::Same
        })
    }
}

fn redeclaration_group_key(symbol: &Symbol) -> RedeclarationGroupKey {
    (
        symbol.owner_type.clone(),
        symbol.scope,
        symbol.key.clone(),
        symbol.generic_parameters.len(),
    )
}

fn is_redeclaration_candidate(symbol: &Symbol) -> bool {
    symbol.origin == Origin::Declaration
        && symbol.generic_parameter.is_none()
        && !symbol.local_only
        && matches!(
            symbol.kind,
            SymbolKind::Type
                | SymbolKind::Routine
                | SymbolKind::Variable
                | SymbolKind::Constant
                | SymbolKind::Parameter
                | SymbolKind::Field
                | SymbolKind::Property
        )
}

fn declaration_group_is_proven(document: &Document, indices: &[usize], proven_end: usize) -> bool {
    indices.iter().all(|index| {
        document.symbols.get(*index).is_some_and(|symbol| {
            !symbol.unresolved_abbreviated
                && symbol_is_proven(symbol, proven_end)
                && !symbol_conditional_state_is_unknown(document, *index)
        })
    })
}

fn has_forward_class_pair(document: &Document, indices: &[usize]) -> bool {
    indices.iter().any(|index| {
        document
            .symbols
            .get(*index)
            .is_some_and(|symbol| is_forward_class_declaration(document, symbol))
    }) && indices.iter().any(|index| {
        document.symbols.get(*index).is_some_and(|symbol| {
            symbol.kind == SymbolKind::Type
                && symbol.type_kind == TypeKind::Class
                && !is_forward_class_declaration(document, symbol)
        })
    })
}

fn is_forward_class_declaration(document: &Document, symbol: &Symbol) -> bool {
    symbol.kind == SymbolKind::Type
        && symbol.type_kind == TypeKind::Class
        && symbol.type_excerpt_end.is_some_and(|end| {
            document.source.get(..end).is_some_and(|prefix| {
                prefix
                    .split_whitespace()
                    .last()
                    .is_some_and(|word| word.eq_ignore_ascii_case("class"))
            }) && document
                .source
                .get(end..symbol.declaration_span.end)
                .is_some_and(|suffix| suffix.trim_start().starts_with(';'))
        })
}

fn routine_group_key(symbol: &Symbol) -> RoutineGroupKey {
    (
        symbol.owner_type.clone(),
        symbol.key.clone(),
        symbol.owner_type.is_none().then_some(symbol.scope),
    )
}

fn include_expansion_complete(document: &Document) -> bool {
    let has_active_include = document.conditionals.directives.iter().any(|directive| {
        directive.kind == pascal_core::conditional::DirectiveKind::Include
            && directive.potentially_active()
    });
    let Some(session) = document.compile_time_session.as_deref() else {
        return !has_active_include;
    };
    let Ok(slots) = include_slots(session) else {
        return false;
    };
    (!has_active_include || !slots.is_empty())
        && slots
            .iter()
            .all(|slot| matches!(slot.outcome, IncludeSlotOutcome::Expanded { .. }))
}

fn h2219_candidate_kind(symbol: &Symbol) -> bool {
    matches!(
        symbol.kind,
        SymbolKind::Field | SymbolKind::Property | SymbolKind::Routine
    )
}

fn h2219_owner_is_proven_class(document: &Document, owner_type: &str, proven_end: usize) -> bool {
    let Some(indices) = document.type_symbol_indices.get(owner_type) else {
        return false;
    };
    let mut owners = indices.iter().filter_map(|index| {
        document
            .symbols
            .get(*index)
            .filter(|symbol| symbol.kind == SymbolKind::Type && symbol.generic_parameter.is_none())
            .map(|symbol| (*index, symbol))
    });
    let Some((owner_index, owner)) = owners.next() else {
        return false;
    };
    owner.type_kind == TypeKind::Class
        && symbol_is_proven(owner, proven_end)
        && !symbol_conditional_state_is_unknown(document, owner_index)
        && owners.next().is_none()
}

fn h2219_target_matches_member(
    current_uri: &Url,
    member_index: usize,
    member: &Symbol,
    target: &Candidate,
    index: &NavigationIndex,
) -> bool {
    if target.uri != *current_uri {
        return false;
    }
    let Some(target_symbol) = index.symbol(target) else {
        return false;
    };
    if member.kind == SymbolKind::Routine {
        target_symbol.kind == SymbolKind::Routine
            && member.routine_key.is_some()
            && member.routine_key == target_symbol.routine_key
            && member.owner_type == target_symbol.owner_type
            && member.key == target_symbol.key
    } else {
        target.index == member_index
    }
}

fn symbol_is_proven(symbol: &Symbol, proven_end: usize) -> bool {
    symbol.selection_span.end <= proven_end
        && symbol.declaration_span.end <= proven_end
        && symbol
            .routine_header_span
            .is_none_or(|header| header.end <= proven_end)
}

fn symbol_conditional_state_is_unknown(document: &Document, index: usize) -> bool {
    document
        .conditional_unknown_symbols
        .get(index)
        .copied()
        .unwrap_or(true)
}

fn owner_type_kind(document: &Document, owner: &str) -> Option<TypeKind> {
    let indices = document.type_symbol_indices.get(owner)?;
    let mut symbols = indices
        .iter()
        .filter_map(|index| document.symbols.get(*index))
        .filter(|symbol| symbol.generic_parameter.is_none());
    let symbol = symbols.next()?;
    symbols.next().is_none().then_some(symbol.type_kind)
}

fn method_resolution_declaration(document: &Document, symbol: &Symbol) -> bool {
    document.method_resolutions.iter().any(|resolution| {
        symbol.owner_type.as_deref() == Some(resolution.interface_owner.as_str())
            && symbol.key == resolution.interface_method
    })
}

fn e2065_display_name(document: &Document, symbol: &Symbol) -> Option<String> {
    let Some(owner) = symbol.owner_type.as_deref() else {
        return Some(symbol.name.clone());
    };
    let owner_name = symbol.owner_type_name.as_deref()?;
    let indices = document.type_symbol_indices.get(owner)?;
    let mut type_symbols = indices
        .iter()
        .filter_map(|index| document.symbols.get(*index))
        .filter(|candidate| candidate.generic_parameter.is_none());
    let type_symbol = type_symbols.next()?;
    if type_symbols.next().is_some() {
        return None;
    }
    if type_symbol.generic_parameters.is_empty() {
        Some(format!("{owner_name}.{}", symbol.name))
    } else {
        let parameters = type_symbol
            .generic_parameters
            .iter()
            .map(|parameter| {
                document
                    .source
                    .get(parameter.span.start..parameter.span.end)
                    .map(str::to_owned)
            })
            .collect::<Option<Vec<_>>>()?
            .join(",");
        Some(format!(
            "{}.{}<{parameters}>.{}",
            document.unit_display_name, owner_name, symbol.name
        ))
    }
}

fn document_text(uri: &Url, span: Span, index: &NavigationIndex) -> Result<String, String> {
    let document = index
        .documents
        .get(uri)
        .ok_or_else(|| format!("document is not indexed: {uri}"))?;
    document
        .source
        .get(span.start..span.end)
        .map(str::to_owned)
        .ok_or_else(|| "routine parameter span is outside its source".to_owned())
}

fn push_diagnostic(
    diagnostics: &mut Vec<SemanticDiagnostic>,
    capacity: usize,
    message_kind: DccMessage,
    span: Span,
    message: String,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    if diagnostics.len() >= capacity {
        return Ok(false);
    }
    budget.require_bytes(message.len(), cancel)?;
    diagnostics.push(SemanticDiagnostic {
        kind: SemanticDiagnosticKind::Dcc(message_kind),
        span: SourceSpan {
            start: span.start,
            end: span.end,
        },
        message,
    });
    Ok(true)
}
