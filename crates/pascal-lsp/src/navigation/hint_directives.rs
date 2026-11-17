use super::*;

impl NavigationIndex {
    pub(super) fn append_hint_directive_diagnostics_with_budget(
        &self,
        uri: &Url,
        document: &Document,
        capacity: usize,
        report: &mut SemanticDiagnosticReport,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<(), String> {
        let proven_end = document
            .parser_recovery_spans
            .iter()
            .map(|span| span.start)
            .min()
            .unwrap_or(usize::MAX);
        let mut pending = vec![document.tree.root_node()];
        let mut visited = 0usize;
        let mut added = 0usize;
        while let Some(node) = pending.pop() {
            check_navigation_cancel(cancel)?;
            visited = visited.saturating_add(1);
            if visited > MAX_SEMANTIC_DIAGNOSTIC_NODES {
                report.note(SemanticCoverageGap::Budget);
                return Ok(());
            }
            if let Err(error) = budget.require_work(1, cancel) {
                if error == "request cancelled" {
                    return Err(error);
                }
                report.note(SemanticCoverageGap::Budget);
                return Ok(());
            }
            if node.start_byte() >= proven_end {
                continue;
            }
            if node.kind() == "identifier" {
                if node.end_byte() <= proven_end
                    && !is_declaration_identifier(node)
                    && !document.conditionals.is_unknown_at(node.start_byte())
                    && !document
                        .opaque_ranges
                        .iter()
                        .any(|range| range.contains_offset(node.start_byte()))
                {
                    let complete_imports = report.checked_candidate(
                        self.imports_are_complete_for_proof(
                            document,
                            node.start_byte(),
                            cancel,
                            budget,
                        ),
                        budget,
                    )?;
                    let Some(true) = complete_imports else {
                        if budget.exhausted() {
                            return Ok(());
                        }
                        continue;
                    };

                    let mut state = ResolutionState::new();
                    let candidates = report.checked_candidate(
                        self.resolve_candidates_at_with_state_and_budget(
                            uri,
                            document,
                            node.start_byte(),
                            node,
                            &mut state,
                            0,
                            cancel,
                            budget,
                        ),
                        budget,
                    )?;
                    let Some(candidates) = candidates else {
                        if budget.exhausted() {
                            return Ok(());
                        }
                        continue;
                    };
                    if candidates.is_empty()
                        || state.ambiguous
                        || state.member_lookup_incomplete
                        || state.receiver_resolution_uncertain()
                        || state.inaccessible_candidate
                        || state.implicit_system_namespace_incomplete
                    {
                        continue;
                    }

                    let findings = report.checked_candidate(
                        hint_diagnostics_for_candidates(
                            self,
                            &candidates,
                            Span::from_node(node),
                            cancel,
                            budget,
                        ),
                        budget,
                    )?;
                    let Some(findings) = findings else {
                        if budget.exhausted() {
                            return Ok(());
                        }
                        continue;
                    };
                    for finding in findings {
                        if added >= capacity {
                            report.note(SemanticCoverageGap::DiagnosticCap);
                            return Ok(());
                        }
                        if let Err(error) = budget.require_bytes(finding.message.len(), cancel) {
                            if error == "request cancelled" {
                                return Err(error);
                            }
                            report.note(SemanticCoverageGap::Budget);
                            return Ok(());
                        }
                        report.diagnostics.push(finding);
                        added += 1;
                    }
                }
                continue;
            }
            let mut cursor = node.walk();
            pending.extend(
                node.children(&mut cursor)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            );
        }
        Ok(())
    }
}

fn hint_diagnostics_for_candidates(
    index: &NavigationIndex,
    candidates: &[Candidate],
    span: Span,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<SemanticDiagnostic>, String> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    budget.require_owned_bytes(
        candidates
            .len()
            .saturating_mul(std::mem::size_of::<(&Document, &Symbol)>()),
        cancel,
    )?;
    let mut symbols = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let Some(document) = index.documents.get(&candidate.uri) else {
            return Ok(Vec::new());
        };
        let Some(symbol) = document.symbols.get(candidate.index) else {
            return Ok(Vec::new());
        };
        let proven_end = document
            .parser_recovery_spans
            .iter()
            .map(|span| span.start)
            .min()
            .unwrap_or(usize::MAX);
        if symbol.kind == SymbolKind::Unit
            || symbol.unresolved_abbreviated
            || symbol.selection_span.end > proven_end
            || symbol.declaration_span.end > proven_end
            || symbol
                .routine_header_span
                .is_some_and(|header| header.end > proven_end)
            || !document.conditionals.complete
            || index.candidate_is_conditionally_unknown(candidate)
            || symbol.hint_directives.unknown
        {
            return Ok(Vec::new());
        }
        symbols.push((document, symbol));
    }

    let Some((_, first_symbol)) = symbols.first() else {
        return Ok(Vec::new());
    };
    let mut name = first_symbol.name.clone();
    if !first_symbol.generic_parameters.is_empty() {
        let arity = first_symbol.generic_parameters.len().to_string();
        budget.require_owned_bytes(arity.len().saturating_add(1), cancel)?;
        name.push('`');
        name.push_str(&arity);
    } else {
        budget.require_owned_bytes(name.len(), cancel)?;
    }

    let mut diagnostics = Vec::new();
    let mut push = |code: DccMessage, suffix: Option<&str>| -> Result<(), String> {
        if !symbols.iter().all(|(_, symbol)| {
            symbol.name == first_symbol.name
                && symbol.generic_parameters.len() == first_symbol.generic_parameters.len()
                && carries_same_hint(symbol, code, suffix)
        }) {
            return Ok(());
        }
        let formatted_suffix = suffix.map_or_else(String::new, |raw| format!(": {raw}"));
        let args: Vec<&str> = if code == DccMessage::W1000 {
            vec![&name, &formatted_suffix]
        } else {
            vec![&name]
        };
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
            message: code.format(&args),
        });
        Ok(())
    };

    let deprecated_raw = valid_deprecated_raw(&first_symbol.hint_directives);
    if let Some(raw) = deprecated_raw {
        push(DccMessage::W1000, raw)?;
    }
    if first_symbol.hint_directives.library {
        push(DccMessage::W1001, None)?;
    }
    if first_symbol.hint_directives.platform {
        push(DccMessage::W1002, None)?;
    }
    if first_symbol.hint_directives.experimental {
        push(DccMessage::W1003, None)?;
    }
    Ok(diagnostics)
}

fn valid_deprecated_raw(directives: &HintDirectives) -> Option<Option<&str>> {
    match (
        directives.deprecated.as_ref(),
        directives.deprecated_raw.as_deref(),
    ) {
        (Some(None), None) => Some(None),
        (Some(Some(_)), Some(raw)) => Some(Some(raw)),
        _ => None,
    }
}

fn carries_same_hint(symbol: &Symbol, code: DccMessage, deprecated_raw: Option<&str>) -> bool {
    match code {
        DccMessage::W1000 => valid_deprecated_raw(&symbol.hint_directives) == Some(deprecated_raw),
        DccMessage::W1001 => symbol.hint_directives.library,
        DccMessage::W1002 => symbol.hint_directives.platform,
        DccMessage::W1003 => symbol.hint_directives.experimental,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use pascal_project::PropertyValue;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn diagnostics(source: &str) -> Vec<SemanticDiagnostic> {
        let uri = Url::parse("file:///tmp/hint-directives-test.pas").expect("fixture URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_owned())
            .expect("hint-directives fixture parses");
        index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete")
    }

    fn diagnostics_with_warning_properties(
        source: &str,
        warning_ids: &[&str],
    ) -> Vec<SemanticDiagnostic> {
        let uri = Url::parse("file:///tmp/hint-directives-test.pas").expect("fixture URI");
        let warning_properties = warning_ids
            .iter()
            .map(|id| {
                (
                    format!("DCC_{id}"),
                    PropertyValue::Known("error".to_owned()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut index = NavigationIndex::new();
        index
            .update_with_session(
                uri.clone(),
                source.to_owned(),
                &ConditionalContext::default(),
                None,
                &AtomicBool::new(false),
                None,
                Some(Arc::new(warning_properties)),
            )
            .expect("hint-directives fixture parses");
        index
            .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
            .expect("semantic diagnostics complete")
    }

    fn hint_codes(diagnostics: &[SemanticDiagnostic]) -> Vec<&str> {
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.kind.code())
            .filter(|code| matches!(*code, "W1000" | "W1001" | "W1002" | "W1003"))
            .collect()
    }

    #[test]
    fn deprecated_routine_use_reports_its_message_at_the_use() {
        let source = concat!(
            "unit HintDiagnostic;\n",
            "interface\n",
            "procedure OldRoutine; deprecated 'use NextRoutine';\n",
            "procedure UseOldRoutine;\n",
            "implementation\n",
            "procedure OldRoutine; begin end;\n",
            "procedure UseOldRoutine; begin OldRoutine; end;\n",
            "end.\n",
        );

        let diagnostics = diagnostics(source);
        let findings = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.kind.code() == "W1000")
            .collect::<Vec<_>>();
        let start = source.rfind("OldRoutine").expect("call occurrence");

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            findings[0].span,
            SourceSpan {
                start,
                end: start + "OldRoutine".len(),
            }
        );
        assert_eq!(
            findings[0].message,
            "Symbol 'OldRoutine' is deprecated: 'use NextRoutine'"
        );
    }

    #[test]
    fn mixed_deprecated_and_plain_overloads_are_withheld() {
        let source = concat!(
            "unit MixedHintOverloads;\n",
            "interface\n",
            "procedure Pick(Value: Integer); overload; deprecated;\n",
            "procedure Pick(Value: string); overload;\n",
            "procedure UseOverloads;\n",
            "implementation\n",
            "procedure Pick(Value: Integer); begin end;\n",
            "procedure Pick(Value: string); begin end;\n",
            "procedure UseOverloads; begin Pick(1); Pick('a'); end;\n",
            "end.\n",
        );

        assert!(hint_codes(&diagnostics(source)).is_empty());
    }

    #[test]
    fn overloads_with_different_deprecated_messages_are_withheld() {
        let source = concat!(
            "unit DifferentHintOverloads;\n",
            "interface\n",
            "procedure Pick(Value: Integer); overload; deprecated 'integer';\n",
            "procedure Pick(Value: string); overload; deprecated 'string';\n",
            "procedure UseOverloads;\n",
            "implementation\n",
            "procedure Pick(Value: Integer); begin end;\n",
            "procedure Pick(Value: string); begin end;\n",
            "procedure UseOverloads; begin Pick(1); end;\n",
            "end.\n",
        );

        assert!(hint_codes(&diagnostics(source)).is_empty());
    }

    #[test]
    fn unknown_conditional_hint_state_is_withheld() {
        let source = concat!(
            "unit UnknownHintUse;\n",
            "interface\n",
            "{$IFDEF MAYBE}\n",
            "procedure ConditionalRoutine; deprecated;\n",
            "{$ENDIF}\n",
            "procedure UseConditionalRoutine;\n",
            "implementation\n",
            "procedure UseConditionalRoutine; begin ConditionalRoutine; end;\n",
            "end.\n",
        );

        assert!(hint_codes(&diagnostics(source)).is_empty());
    }

    #[test]
    fn unresolved_reference_does_not_produce_hint_findings() {
        let source = concat!(
            "unit UnresolvedHintUse;\n",
            "interface\n",
            "uses MissingHintProvider;\n",
            "procedure UseMissingRoutine;\n",
            "implementation\n",
            "procedure UseMissingRoutine; begin MissingRoutine; end;\n",
            "end.\n",
        );

        assert!(hint_codes(&diagnostics(source)).is_empty());
    }

    #[test]
    fn hint_warning_error_project_properties_publish_promoted_error_codes() {
        let source = concat!(
            "unit HintErrorPromotion;\n",
            "interface\n",
            "procedure DeprecatedRoutine; deprecated;\n",
            "var DeprecatedLibraryVariable: Integer library;\n",
            "procedure PlatformRoutine; platform;\n",
            "procedure ExperimentalRoutine; experimental;\n",
            "procedure UseHints;\n",
            "implementation\n",
            "procedure DeprecatedRoutine; begin end;\n",
            "procedure PlatformRoutine; begin end;\n",
            "procedure ExperimentalRoutine; begin end;\n",
            "procedure UseHints;\n",
            "var Value: Integer;\n",
            "begin\n",
            "  DeprecatedRoutine;\n",
            "  Value := DeprecatedLibraryVariable;\n",
            "  PlatformRoutine;\n",
            "  ExperimentalRoutine;\n",
            "end;\n",
            "end.\n",
        );

        let all_diagnostics = diagnostics_with_warning_properties(
            source,
            &[
                "SYMBOL_DEPRECATED",
                "SYMBOL_LIBRARY",
                "SYMBOL_PLATFORM",
                "SYMBOL_EXPERIMENTAL",
            ],
        );
        let findings = all_diagnostics
            .iter()
            .filter(|diagnostic| {
                matches!(
                    diagnostic.kind.code(),
                    "W1000" | "W1001" | "W1002" | "W1003" | "E1000" | "E1001" | "E1002" | "E1003"
                )
            })
            .map(|diagnostic| (diagnostic.kind.code(), diagnostic.kind.severity()))
            .collect::<Vec<_>>();

        assert_eq!(
            findings,
            vec![
                ("E1000", DccSeverity::Error),
                ("E1001", DccSeverity::Error),
                ("E1002", DccSeverity::Error),
                ("E1003", DccSeverity::Error),
            ],
            "all diagnostics: {all_diagnostics:#?}"
        );
    }
}
