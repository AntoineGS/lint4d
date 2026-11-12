use super::*;

impl NavigationIndex {
    #[allow(clippy::too_many_lines)]
    pub(super) fn ancestry_diagnostics_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        capacity: usize,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<SemanticDiagnostic>, String> {
        let mut diagnostics = Vec::new();
        let mut ancestry = ContractAncestryState::new();
        if !document.conditionals.complete
            || !self.contract_document_complete_with_budget(uri, &mut ancestry, cancel, budget)?
        {
            return Ok(diagnostics);
        }
        let proven_end = document
            .parser_recovery_spans
            .iter()
            .map(|span| span.start)
            .min()
            .unwrap_or(usize::MAX);

        for (symbol_index, symbol) in document.symbols.iter().enumerate() {
            check_navigation_cancel(cancel)?;
            if symbol.kind != SymbolKind::Routine
                || symbol.origin != Origin::Declaration
                || symbol.owner_type.is_none()
                || symbol.routine_directives.override_
                || symbol.routine_directives.reintroduce
                || symbol.routine_directives.overload
                || symbol.routine_directives.abstract_
                || symbol.routine_directives.external
                || symbol.routine_directives.forward
                || symbol.routine_directives.message
                || symbol.unresolved_abbreviated
                || !symbol_is_proven(symbol, proven_end)
                || symbol_conditional_state_is_unknown(document, symbol_index)
            {
                continue;
            }
            budget.require_work(1, cancel)?;
            let Some(owner_key) = symbol.owner_type.as_deref() else {
                continue;
            };
            let Some(owner_instance) = self.contract_type_instance(uri, owner_key) else {
                continue;
            };
            if owner_instance.kind != TypeKind::Class
                || !contract_substitution_is_complete(&owner_instance)
                || self.contract_routine_signature_status(
                    &Candidate {
                        uri: uri.clone(),
                        index: symbol_index,
                    },
                    &owner_instance.substitution,
                    cancel,
                    budget,
                )? == ContractMatch::Unknown
            {
                continue;
            }

            let mut inherited = Vec::new();
            if self.collect_superclass_routines_with_budget(
                &owner_instance,
                &mut ancestry,
                &mut inherited,
                cancel,
                budget,
            )? == AncestryStatus::Unknown
            {
                continue;
            }

            let mut hidden_virtual = None;
            let mut uncertain = false;
            for candidate in inherited
                .iter()
                .filter(|candidate| candidate.symbol_key == symbol.key)
            {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if candidate.conditional_unknown(self) {
                    uncertain = true;
                    continue;
                }
                let Some(ancestor_symbol) = self.symbol(&candidate.candidate) else {
                    uncertain = true;
                    continue;
                };
                if ancestor_symbol.origin != Origin::Declaration {
                    continue;
                }
                if !candidate_symbol_is_proven(self, &candidate.candidate)
                    || matches!(
                        ancestor_symbol.visibility,
                        Visibility::Private | Visibility::StrictPrivate
                    )
                    || ancestor_symbol.routine_directives.overload
                    || ancestor_symbol.routine_directives.abstract_
                    || ancestor_symbol.routine_directives.external
                    || ancestor_symbol.routine_directives.forward
                    || ancestor_symbol.routine_directives.message
                {
                    uncertain = true;
                    continue;
                }
                match self.routines_contract_match(
                    symbol,
                    uri,
                    &owner_instance.substitution,
                    ancestor_symbol,
                    &candidate.candidate.uri,
                    &candidate.substitution,
                    cancel,
                    budget,
                )? {
                    ContractMatch::Yes
                        if ancestor_symbol.routine_directives.virtual_
                            || ancestor_symbol.routine_directives.dynamic =>
                    {
                        if let Some(base_name) = ancestor_symbol.owner_type_name.as_deref() {
                            hidden_virtual.get_or_insert((base_name, symbol.name.as_str()));
                        } else {
                            uncertain = true;
                        }
                    }
                    ContractMatch::Yes | ContractMatch::No => {}
                    ContractMatch::Unknown => uncertain = true,
                }
            }
            if uncertain {
                continue;
            }
            if let Some((base_name, method_name)) = hidden_virtual {
                push_ancestry_diagnostic(
                    &mut diagnostics,
                    capacity,
                    DccMessage::W1010,
                    symbol.selection_span,
                    &[method_name, base_name],
                    cancel,
                    budget,
                )?;
            }
        }

        for (class_symbol_index, class_symbol) in document.symbols.iter().enumerate() {
            check_navigation_cancel(cancel)?;
            if class_symbol.kind != SymbolKind::Type
                || class_symbol.type_kind != TypeKind::Class
                || class_symbol.owner_type.is_some()
                || class_symbol.generic_parameter.is_some()
                || !symbol_is_proven(class_symbol, proven_end)
                || symbol_conditional_state_is_unknown(document, class_symbol_index)
            {
                continue;
            }
            budget.require_work(1, cancel)?;
            let Some(class_instance) = self.contract_type_instance(uri, &class_symbol.key) else {
                continue;
            };
            if !contract_substitution_is_complete(&class_instance) {
                continue;
            }

            let Some((missing, surface)) = self.missing_interface_contracts_for_class_with_budget(
                &class_instance,
                &mut ancestry,
                cancel,
                budget,
            )?
            else {
                continue;
            };
            for obligation in missing {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if obligation.implementation != ContractMatch::No
                    || !self.interface_signature_mismatch_with_budget(
                        &class_instance,
                        &obligation,
                        &surface,
                        cancel,
                        budget,
                    )?
                {
                    continue;
                }
                let Some(interface_method) = self.symbol(&obligation.requirement.candidate) else {
                    continue;
                };
                let Some(interface_symbol) =
                    self.contract_interface_symbol(&obligation.requirement.interface)
                else {
                    continue;
                };
                push_ancestry_diagnostic(
                    &mut diagnostics,
                    capacity,
                    DccMessage::E2211,
                    class_symbol.selection_span,
                    &[&interface_method.name, &interface_symbol.name],
                    cancel,
                    budget,
                )?;
            }

            let Some(member_indices) = document
                .member_symbol_indices_by_owner
                .get(&class_symbol.key)
            else {
                continue;
            };
            let mut has_properties = false;
            for index in member_indices {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if document
                    .symbols
                    .get(*index)
                    .is_some_and(|member| member.kind == SymbolKind::Property)
                {
                    has_properties = true;
                    break;
                }
            }
            if !has_properties {
                continue;
            }
            let mut surface = ContractClassSurface::default();
            if self.collect_class_surface_with_budget(
                &class_instance,
                &mut ancestry,
                &mut surface,
                cancel,
                budget,
            )? == AncestryStatus::Unknown
                || surface.implicit_compiler_root_is_open
            {
                continue;
            }
            let class_resolution = self.resolve_contract_type_with_budget(
                &class_instance,
                &mut ancestry,
                cancel,
                budget,
            )?;
            if class_resolution.status == AncestryStatus::Unknown {
                continue;
            }
            let mut parent = class_resolution.superclass;
            let mut ancestor_fields = HashSet::new();
            let mut uncertain_fields = HashSet::new();
            let mut ancestry_unknown = false;
            while let Some(parent_instance) = parent {
                check_navigation_cancel(cancel)?;
                budget.require_work(1, cancel)?;
                if !contract_substitution_is_complete(&parent_instance) {
                    ancestry_unknown = true;
                    break;
                }
                let Some(parent_document) = self.documents.get(&parent_instance.uri) else {
                    ancestry_unknown = true;
                    break;
                };
                if let Some(indices) = parent_document
                    .member_symbol_indices_by_owner
                    .get(&parent_instance.key)
                {
                    budget.require_work(indices.len(), cancel)?;
                    for index in indices {
                        check_navigation_cancel(cancel)?;
                        let Some(member) = parent_document.symbols.get(*index) else {
                            continue;
                        };
                        if member.kind != SymbolKind::Field
                            || member.owner_type.as_deref() != Some(parent_instance.key.as_str())
                        {
                            continue;
                        }
                        if !symbol_is_proven(member, source_proven_end(parent_document))
                            || symbol_conditional_state_is_unknown(parent_document, *index)
                            || matches!(
                                member.visibility,
                                Visibility::Private | Visibility::StrictPrivate
                            )
                        {
                            budget.require_owned_bytes(
                                std::mem::size_of::<String>()
                                    .saturating_add(3 * std::mem::size_of::<usize>())
                                    .saturating_add(member.key.len()),
                                cancel,
                            )?;
                            uncertain_fields.insert(member.key.clone());
                            continue;
                        }
                        budget.require_owned_bytes(
                            std::mem::size_of::<String>()
                                .saturating_add(3 * std::mem::size_of::<usize>())
                                .saturating_add(member.key.len()),
                            cancel,
                        )?;
                        ancestor_fields.insert(member.key.clone());
                    }
                }
                let parent_resolution = self.resolve_contract_type_with_budget(
                    &parent_instance,
                    &mut ancestry,
                    cancel,
                    budget,
                )?;
                if parent_resolution.status == AncestryStatus::Unknown {
                    ancestry_unknown = true;
                    break;
                }
                parent = parent_resolution.superclass;
            }
            if ancestry_unknown {
                continue;
            }
            for index in member_indices {
                check_navigation_cancel(cancel)?;
                let Some(property) = document.symbols.get(*index) else {
                    continue;
                };
                if property.kind != SymbolKind::Property
                    || property.owner_type.as_deref() != Some(class_instance.key.as_str())
                    || property.origin != Origin::Declaration
                    || property.unresolved_abbreviated
                    || !symbol_is_proven(property, proven_end)
                    || symbol_conditional_state_is_unknown(document, *index)
                    || uncertain_fields.contains(&property.key)
                    || !ancestor_fields.contains(&property.key)
                {
                    continue;
                }
                push_ancestry_diagnostic(
                    &mut diagnostics,
                    capacity,
                    DccMessage::W1009,
                    property.selection_span,
                    &[&property.name],
                    cancel,
                    budget,
                )?;
            }
        }

        Ok(diagnostics)
    }

    pub(super) fn interface_signature_mismatch_with_budget(
        &self,
        class_instance: &TypeInstance,
        obligation: &ContractMissingInterfaceMethod,
        surface: &ContractClassSurface,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<bool, String> {
        if obligation.implementation != ContractMatch::No {
            return Ok(false);
        }
        let Some(interface_method) = self.symbol(&obligation.requirement.candidate) else {
            return Ok(false);
        };
        if obligation.method_name != interface_method.key
            || surface.method_resolutions.iter().any(|resolution| {
                resolution.interface_owner == obligation.requirement.interface.key
                    && resolution.interface_method == interface_method.key
            })
        {
            return Ok(false);
        }

        let mut mismatch = false;
        let mut uncertain = false;
        let mut direct_candidates = 0usize;
        for candidate in surface.routines.iter().filter(|candidate| {
            candidate.symbol_key == obligation.method_name
                && candidate.candidate.uri == class_instance.uri
        }) {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let Some(candidate_symbol) = self.symbol(&candidate.candidate) else {
                uncertain = true;
                continue;
            };
            if candidate_symbol.owner_type.as_deref() != Some(class_instance.key.as_str())
                || candidate_symbol.origin != Origin::Declaration
            {
                continue;
            }
            direct_candidates = direct_candidates.saturating_add(1);
            let Some(candidate_document) = self.documents.get(&candidate.candidate.uri) else {
                uncertain = true;
                continue;
            };
            if !symbol_is_proven(candidate_symbol, source_proven_end(candidate_document))
                || symbol_conditional_state_is_unknown(
                    candidate_document,
                    candidate.candidate.index,
                )
                || matches!(
                    candidate_symbol.visibility,
                    Visibility::Private | Visibility::StrictPrivate
                )
            {
                uncertain = true;
                continue;
            }
            if self.routines_contract_match(
                interface_method,
                &obligation.requirement.candidate.uri,
                &obligation.requirement.substitution,
                candidate_symbol,
                &candidate.candidate.uri,
                &candidate.substitution,
                cancel,
                budget,
            )? == ContractMatch::Yes
            {
                return Ok(false);
            }
            if interface_method.routine_kind != RoutineKind::Procedure
                || candidate_symbol.routine_kind != RoutineKind::Procedure
                || interface_method.is_static
                || candidate_symbol.is_static
                || interface_method.result_type_ref.is_some()
                || candidate_symbol.result_type_ref.is_some()
                || !interface_method.generic_parameters.is_empty()
                || !candidate_symbol.generic_parameters.is_empty()
                || candidate_symbol.routine_directives.overload
                || candidate_symbol.routine_directives.abstract_
                || candidate_symbol.routine_directives.external
                || candidate_symbol.routine_directives.forward
                || candidate_symbol.routine_directives.message
                || interface_method
                    .routine_directives
                    .calling_convention_unknown
                || candidate_symbol
                    .routine_directives
                    .calling_convention_unknown
                || Self::calling_conventions_contract_match(
                    interface_method.routine_directives.calling_convention,
                    candidate_symbol.routine_directives.calling_convention,
                ) != ContractMatch::Yes
                || interface_method.routine_parameters.len()
                    != candidate_symbol.routine_parameters.len()
                || interface_method.routine_parameters.len() != 1
            {
                uncertain = true;
                continue;
            }
            let mut candidate_mismatch = false;
            for (interface_parameter, candidate_parameter) in interface_method
                .routine_parameters
                .iter()
                .zip(&candidate_symbol.routine_parameters)
            {
                if interface_parameter.mode != candidate_parameter.mode {
                    uncertain = true;
                    break;
                }
                match self.contract_parameter_type_match(
                    &obligation.requirement.candidate.uri,
                    interface_parameter.type_ref.as_ref(),
                    &obligation.requirement.substitution,
                    &candidate.candidate.uri,
                    candidate_parameter.type_ref.as_ref(),
                    &candidate.substitution,
                    cancel,
                    budget,
                )? {
                    ContractMatch::Yes => {}
                    ContractMatch::No => candidate_mismatch = true,
                    ContractMatch::Unknown => uncertain = true,
                }
            }
            if candidate_mismatch {
                mismatch = true;
            }
        }
        Ok(mismatch && !uncertain && direct_candidates == 1)
    }

    fn contract_interface_symbol(&self, instance: &TypeInstance) -> Option<&Symbol> {
        let document = self.documents.get(&instance.uri)?;
        let indices = document.type_symbol_indices.get(&instance.key)?;
        let [index] = indices.as_slice() else {
            return None;
        };
        document.symbols.get(*index).filter(|symbol| {
            symbol.kind == SymbolKind::Type && symbol.type_kind == TypeKind::Interface
        })
    }
}

fn source_proven_end(document: &Document) -> usize {
    document
        .parser_recovery_spans
        .iter()
        .map(|span| span.start)
        .min()
        .unwrap_or(usize::MAX)
}

fn symbol_is_proven(symbol: &Symbol, proven_end: usize) -> bool {
    !symbol.unresolved_abbreviated
        && symbol.selection_span.end <= proven_end
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

fn candidate_symbol_is_proven(index: &NavigationIndex, candidate: &Candidate) -> bool {
    let Some(document) = index.documents.get(&candidate.uri) else {
        return false;
    };
    let Some(symbol) = document.symbols.get(candidate.index) else {
        return false;
    };
    symbol_is_proven(symbol, source_proven_end(document))
        && !symbol_conditional_state_is_unknown(document, candidate.index)
}

fn push_ancestry_diagnostic(
    diagnostics: &mut Vec<SemanticDiagnostic>,
    capacity: usize,
    code: DccMessage,
    span: Span,
    args: &[&str],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<(), String> {
    if diagnostics.len() >= capacity {
        return Err(SEMANTIC_DIAGNOSTICS_LIMIT.to_owned());
    }
    let bytes = code
        .template()
        .len()
        .saturating_add(args.iter().map(|arg| arg.len()).sum::<usize>());
    budget.require_bytes(bytes.saturating_mul(2), cancel)?;
    diagnostics.push(SemanticDiagnostic {
        kind: SemanticDiagnosticKind::Dcc(code),
        span: SourceSpan {
            start: span.start,
            end: span.end,
        },
        message: code.format(args),
    });
    Ok(())
}
