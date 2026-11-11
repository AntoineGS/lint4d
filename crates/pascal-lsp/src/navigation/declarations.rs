use super::*;
use pascal_core::facts::{IncludeSlotOutcome, include_slots};
use std::collections::HashMap;

type RoutineGroupKey = (Option<String>, String, Option<usize>);

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

        Ok(diagnostics)
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
