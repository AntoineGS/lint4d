use super::*;
use pascal_core::conditional::{DirectiveKind, Truth};
use std::collections::BTreeSet;

const IMPLICIT_SIDE_EFFECT_UNITS: &[&str] = &["system", "sysinit"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormFileRequirement {
    None,
    Read(&'static str),
    Unknown,
}

impl NavigationIndex {
    pub(crate) fn unused_unit_diagnostics_with_report(
        &self,
        uri: &Url,
        report: &SemanticDiagnosticReport,
        form_file_bytes: Option<&[u8]>,
        cancel: &AtomicBool,
    ) -> Result<Vec<SemanticDiagnostic>, String> {
        let mut budget = AssistanceBudget::new(
            MAX_SEMANTIC_DIAGNOSTIC_WORK,
            MAX_SEMANTIC_DIAGNOSTIC_BYTES,
            "unused-unit analysis",
        );
        match self.unused_unit_diagnostics_with_budget(
            uri,
            report,
            form_file_bytes,
            cancel,
            &mut budget,
        ) {
            Err(error) if error == "request cancelled" => Err(error),
            Err(_) => Ok(Vec::new()),
            Ok(diagnostics) => Ok(diagnostics),
        }
    }

    pub(crate) fn form_file_requirement(&self, uri: &Url) -> Option<FormFileRequirement> {
        let document = self.documents.get(uri)?;
        Some(form_file_requirement(document))
    }

    fn unused_unit_diagnostics_with_budget(
        &self,
        uri: &Url,
        report: &SemanticDiagnosticReport,
        form_file_bytes: Option<&[u8]>,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Vec<SemanticDiagnostic>, String> {
        check_navigation_cancel(cancel)?;
        let Some(document) = self.documents.get(uri) else {
            return Ok(Vec::new());
        };
        if !report.is_complete()
            || !document.conditionals.complete
            || !document.conditionals.unknown_spans.is_empty()
            || document
                .conditionals
                .unknown_activity_requires_fail_closed()
            || !document.parser_recovery_spans.is_empty()
            || !document.opaque_ranges.is_empty()
            || !declarations::include_expansion_complete(document)
            || !compiler_tables_available(document)
        {
            return Ok(Vec::new());
        }

        let form_classes = match form_file_requirement(document) {
            FormFileRequirement::None => None,
            FormFileRequirement::Unknown => return Ok(Vec::new()),
            FormFileRequirement::Read(_) => {
                let Some(bytes) = form_file_bytes else {
                    return Ok(Vec::new());
                };
                let Some(classes) = form_file::form_object_classes(bytes) else {
                    return Ok(Vec::new());
                };
                Some(classes)
            }
        };

        let identifiers =
            match identifier_nodes_with_budget(document.tree.root_node(), cancel, budget) {
                Ok(identifiers) => identifiers,
                Err(error) if error == "request cancelled" => return Err(error),
                Err(_) => return Ok(Vec::new()),
            };
        let Some(inline_dependencies) =
            self.inline_body_dependencies_with_budget(uri, document, &identifiers, cancel, budget)?
        else {
            return Ok(Vec::new());
        };
        let mut used_units = BTreeSet::new();
        for identifier in identifiers {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let generic_constraint_identifier =
                is_generic_constraint_identifier(identifier, cancel, budget)?;
            if (is_declaration_identifier(identifier) && !generic_constraint_identifier)
                || is_unit_declaration_identifier(identifier)
                || has_ancestor_kind_with_budget(identifier, "declUses", cancel, budget)?
            {
                continue;
            }
            let name = node_text_with_budget(identifier, &document.source, cancel, budget)?;
            if is_implicit_or_intrinsic_name(&canonical_name(name)) {
                continue;
            }
            let non_value = is_non_value_identifier_with_budget_and_declaration(
                identifier,
                &document.source,
                cancel,
                budget,
                false,
            )?;
            if !non_value {
                match self.semantic_proof_at_with_budget(
                    uri,
                    document,
                    identifier.start_byte(),
                    identifier,
                    cancel,
                    budget,
                )? {
                    SemanticProofStatus::Resolved => {}
                    SemanticProofStatus::ProvenAbsent
                    | SemanticProofStatus::Ambiguous
                    | SemanticProofStatus::Incomplete => return Ok(Vec::new()),
                }
            }
            if !self.imports_are_complete_for_proof(
                document,
                identifier.start_byte(),
                cancel,
                budget,
            )? {
                return Ok(Vec::new());
            }
            let mut state = ResolutionState::new();
            let mut candidates = self.resolve_candidates_at_with_state_and_budget(
                uri,
                document,
                identifier.start_byte(),
                identifier,
                &mut state,
                0,
                cancel,
                budget,
            )?;
            if candidates.is_empty() && is_bare_attribute_name(identifier) {
                let attribute_name = format!("{name}Attribute");
                let mut attribute_state = ResolutionState::new();
                candidates = self.unqualified_references_with_budget_and_state(
                    uri,
                    document,
                    identifier.start_byte(),
                    &attribute_name,
                    identifier,
                    &mut attribute_state,
                    cancel,
                    budget,
                )?;
                candidates.retain(|candidate| {
                    self.symbol(candidate)
                        .is_some_and(|symbol| symbol.kind == SymbolKind::Type)
                });
                state = attribute_state;
            }
            if state.ambiguous
                || state.member_lookup_incomplete
                || state.receiver_resolution_uncertain()
                || state.inaccessible_candidate
                || state.implicit_system_namespace_incomplete
                || candidates
                    .iter()
                    .any(|candidate| self.candidate_is_conditionally_unknown(candidate))
            {
                return Ok(Vec::new());
            }
            if non_value && candidates.len() > 1 {
                return Ok(Vec::new());
            }
            if candidates.is_empty() && !state.compiler_surface_resolved {
                // A non-value occurrence (for example a type, attribute, or
                // generic constraint) still needs a positive source binding.
                // The ordinary semantic proof handles compiler-surface values.
                if non_value && overload::builtin_type(name).is_none() {
                    return Ok(Vec::new());
                }
                if !non_value {
                    return Ok(Vec::new());
                }
            }
            for candidate in candidates {
                check_navigation_cancel(cancel)?;
                let Some(target) = self.documents.get(&candidate.uri) else {
                    return Ok(Vec::new());
                };
                if !Self::document_is_complete_for_proof(target, cancel, budget)?
                    || !target.conditionals.complete
                    || !target.conditionals.unknown_spans.is_empty()
                    || target.conditionals.unknown_activity_requires_fail_closed()
                    || !declarations::include_expansion_complete(target)
                {
                    return Ok(Vec::new());
                }
                if candidate.uri != *uri {
                    used_units.insert(canonical_name(&target.unit_name));
                }
            }
        }

        let mut diagnostics = Vec::new();
        for import in &document.imports {
            check_navigation_cancel(cancel)?;
            budget.require_work(1, cancel)?;
            let unit_name = canonical_name(&import.name);
            if unit_name.is_empty()
                || used_units.contains(&unit_name)
                || import_is_project_path(uri, document, import)
                || import_is_duplicated(document, &unit_name)
            {
                continue;
            }
            let Some(provider_uri) =
                self.unique_import_provider(document, &unit_name, cancel, budget)?
            else {
                continue;
            };
            let Some(provider) = self.documents.get(&provider_uri) else {
                continue;
            };
            if inline_dependencies.contains(&provider_uri) {
                continue;
            }
            let Some(Some(unit_closure)) = budget_result(
                unit_facts::uses_closure(
                    self,
                    document,
                    std::slice::from_ref(&unit_name),
                    cancel,
                    budget,
                ),
                budget,
                cancel,
            )?
            else {
                continue;
            };
            let effects = match budget_result(
                unit_facts::unit_side_effects(provider, cancel, budget),
                budget,
                cancel,
            )? {
                Some(effects) => effects,
                None => continue,
            };
            if effects != unit_facts::SideEffects::None {
                continue;
            }

            let remaining_entries = document
                .imports
                .iter()
                .filter(|other| canonical_name(&other.name) != unit_name)
                .map(|other| canonical_name(&other.name))
                .collect::<Vec<_>>();
            let Some(Some(other_closure)) = budget_result(
                unit_facts::uses_closure(self, document, &remaining_entries, cancel, budget),
                budget,
                cancel,
            )?
            else {
                continue;
            };
            let mut covered_side_effects = other_closure;
            covered_side_effects.extend(
                IMPLICIT_SIDE_EFFECT_UNITS
                    .iter()
                    .map(|name| (*name).to_owned()),
            );
            let mut safe = true;
            for member in &unit_closure {
                check_navigation_cancel(cancel)?;
                let Some(member_uri) = self.unique_indexed_unit_provider(member, cancel, budget)?
                else {
                    safe = false;
                    break;
                };
                let Some(member_document) = self.documents.get(&member_uri) else {
                    safe = false;
                    break;
                };
                match budget_result(
                    unit_facts::unit_side_effects(member_document, cancel, budget),
                    budget,
                    cancel,
                )? {
                    Some(unit_facts::SideEffects::None) => {}
                    Some(unit_facts::SideEffects::Some(_))
                        if covered_side_effects.contains(member) => {}
                    Some(unit_facts::SideEffects::Some(_))
                    | Some(unit_facts::SideEffects::Unknown)
                    | None => {
                        safe = false;
                        break;
                    }
                }
            }
            if !safe {
                continue;
            }
            if form_classes.as_ref().is_some_and(|classes| {
                classes.iter().any(|class_name| {
                    class_name.rsplit('.').next().is_some_and(|class| {
                        provider.symbols.iter().any(|symbol| {
                            symbol.kind == SymbolKind::Type && symbol.key == canonical_name(class)
                        })
                    })
                })
            }) {
                continue;
            }
            let display_name = document
                .source
                .get(import.span.start..import.span.end)
                .unwrap_or(&import.name);
            let message = format!("Unit '{display_name}' is not used");
            budget.require_owned_bytes(
                std::mem::size_of::<SemanticDiagnostic>().saturating_add(message.len()),
                cancel,
            )?;
            diagnostics.push(SemanticDiagnostic {
                kind: SemanticDiagnosticKind::UnusedUnit,
                span: import.span,
                message,
            });
        }
        Ok(diagnostics)
    }

    fn unique_import_provider(
        &self,
        document: &Document,
        unit_name: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<Url>, String> {
        let urls = self.unit_urls_for_import_with_budget(document, unit_name, cancel, budget)?;
        if urls.len() != 1 || !self.documents.contains_key(&urls[0]) {
            return Ok(None);
        }
        Ok(urls.into_iter().next())
    }

    fn unique_indexed_unit_provider(
        &self,
        unit_name: &str,
        cancel: &AtomicBool,
        budget: &mut AssistanceBudget,
    ) -> Result<Option<Url>, String> {
        check_navigation_cancel(cancel)?;
        let Some(urls) = self.units.get(unit_name) else {
            return Ok(None);
        };
        budget.require_work(urls.len(), cancel)?;
        if urls.len() != 1 {
            return Ok(None);
        }
        let uri = urls[0].clone();
        budget.require_bytes(uri.as_str().len(), cancel)?;
        Ok(self.documents.contains_key(&uri).then_some(uri))
    }
}

fn is_generic_constraint_identifier(
    identifier: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let span = Span::from_node(identifier);
    let mut current = Some(identifier);
    while let Some(node) = current {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let constraint_type = if node.kind() == "genericArg" {
            node.child_by_field_name("type")
                .filter(|type_node| type_node.kind() != ":")
                .or_else(|| {
                    (0..node.named_child_count())
                        .filter_map(|index| node.named_child(index))
                        .find(|child| {
                            matches!(
                                child.kind(),
                                "typeref"
                                    | "typerefDot"
                                    | "typerefTpl"
                                    | "typerefPtr"
                                    | "ppFragmentExpr"
                            )
                        })
                })
        } else {
            None
        };
        if constraint_type.is_some_and(|constraint| {
            constraint.start_byte() <= span.start && span.end <= constraint.end_byte()
        }) {
            return Ok(true);
        }
        current = node.parent();
    }
    Ok(false)
}

fn is_bare_attribute_name(identifier: Node<'_>) -> bool {
    identifier
        .parent()
        .is_some_and(|parent| parent.kind() == "rttiAttributes")
}

fn compiler_tables_available(document: &Document) -> bool {
    let Some(key) = dcc_surface::SurfaceKey::from_context(&document.conditional_context) else {
        return false;
    };
    let Some(version) = document.conditional_context.compiler_version else {
        return false;
    };
    dcc_surface::table_for(&key).is_some()
        && dcc_switches::switch_table(version.major, version.minor).is_some()
}

fn budget_result<T>(
    result: Result<T, String>,
    budget: &AssistanceBudget,
    cancel: &AtomicBool,
) -> Result<Option<T>, String> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error == "request cancelled" => Err(error),
        Err(error) if budget.exhausted_by(&error, cancel) => Ok(None),
        Err(error) => Err(error),
    }
}

fn import_is_duplicated(document: &Document, unit_name: &str) -> bool {
    document
        .imports
        .iter()
        .filter(|import| canonical_name(&import.name) == unit_name)
        .take(2)
        .count()
        > 1
}

fn import_is_project_path(uri: &Url, document: &Document, import: &ImportMetadata) -> bool {
    let is_project_file = uri
        .to_file_path()
        .ok()
        .and_then(|path| {
            path.extension()
                .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        })
        .is_some_and(|extension| matches!(extension.as_str(), "dpr" | "dpk"));
    if !is_project_file {
        return false;
    }
    let Some(module_name) = find_node_by_span(document.tree.root_node(), "moduleName", import.span)
    else {
        return true;
    };
    let Some(parent) = module_name.parent() else {
        return true;
    };
    let mut after_target = false;
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.id() == module_name.id() {
            after_target = true;
            continue;
        }
        if !after_target {
            continue;
        }
        match child.kind() {
            "kIn" => return true,
            "moduleName" | "," | ";" => return false,
            _ => {}
        }
    }
    false
}

fn find_node_by_span<'tree>(
    root: Node<'tree>,
    kind: &str,
    span: SourceSpan,
) -> Option<Node<'tree>> {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.kind() == kind && node.start_byte() == span.start && node.end_byte() == span.end {
            return Some(node);
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return None;
            }
        }
    }
}

fn form_file_requirement(document: &Document) -> FormFileRequirement {
    if !document.conditionals.complete
        || !document.conditionals.unknown_spans.is_empty()
        || document
            .conditionals
            .unknown_activity_requires_fail_closed()
    {
        return FormFileRequirement::Unknown;
    }
    let mut found = None;
    for directive in &document.conditionals.directives {
        if directive.activity == Truth::False || directive.kind == DirectiveKind::Include {
            continue;
        }
        let Some(extension) = form_resource_extension(&directive.body) else {
            continue;
        };
        if found.is_some_and(|prior| prior != extension) {
            return FormFileRequirement::Unknown;
        }
        found = Some(extension);
    }
    found.map_or(FormFileRequirement::None, FormFileRequirement::Read)
}

fn form_resource_extension(body: &str) -> Option<&'static str> {
    let body = body.trim_start();
    let keyword_end = body
        .char_indices()
        .find_map(|(index, character)| {
            (!character.is_ascii_alphanumeric() && character != '_').then_some(index)
        })
        .unwrap_or(body.len());
    let keyword = &body[..keyword_end];
    if !keyword.eq_ignore_ascii_case("r") && !keyword.eq_ignore_ascii_case("resource") {
        return None;
    }
    let argument = body[keyword_end..]
        .trim_start_matches(|character: char| character.is_ascii_whitespace() || character == ':')
        .trim()
        .trim_matches(['\'', '"']);
    if argument.eq_ignore_ascii_case("*.dfm") {
        Some("dfm")
    } else if argument.eq_ignore_ascii_case("*.fmx") {
        Some("fmx")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pascal_project::{CompilerVersion, LayoutContext, TargetPlatform};

    fn context() -> ConditionalContext {
        let version = CompilerVersion::new(24, 0);
        ConditionalContext {
            compiler_version: Some(version),
            layout: LayoutContext::for_target(Some(version), Some(&TargetPlatform::Win32)),
            ..ConditionalContext::default()
        }
    }

    fn add_unit(index: &mut NavigationIndex, name: &str, source: &str) -> Url {
        let uri = Url::parse(&format!("file:///tmp/unused-unit/{name}.pas")).unwrap();
        index
            .update_with_context_with_cancel(
                uri.clone(),
                source.to_owned(),
                &context(),
                &AtomicBool::new(false),
            )
            .unwrap();
        uri
    }

    fn index_with_unused_unit() -> (NavigationIndex, Url) {
        let mut index = NavigationIndex::new();
        add_unit(
            &mut index,
            "System",
            "unit System; interface implementation end.",
        );
        add_unit(
            &mut index,
            "SysInit",
            "unit SysInit; interface implementation end.",
        );
        add_unit(&mut index, "U", "unit U; interface implementation end.");
        let uri = add_unit(
            &mut index,
            "D",
            "unit D; interface uses U; implementation end.",
        );
        (index, uri)
    }

    fn index_with_sources(
        sources: &[(&str, &str)],
        consumer_name: &str,
        consumer: &str,
    ) -> (NavigationIndex, Url) {
        let mut index = NavigationIndex::new();
        add_unit(
            &mut index,
            "System",
            "unit System; interface implementation end.",
        );
        add_unit(
            &mut index,
            "SysInit",
            "unit SysInit; interface implementation end.",
        );
        for (name, source) in sources {
            add_unit(&mut index, name, source);
        }
        let uri = add_unit(&mut index, consumer_name, consumer);
        (index, uri)
    }

    fn unused_diagnostics(
        index: &NavigationIndex,
        uri: &Url,
        form_file_bytes: Option<&[u8]>,
    ) -> Vec<SemanticDiagnostic> {
        let cancel = AtomicBool::new(false);
        let report = index
            .semantic_diagnostic_report_with_cancel(uri, &cancel)
            .unwrap();
        assert!(report.is_complete(), "semantic report: {report:?}");
        index
            .unused_unit_diagnostics_with_report(uri, &report, form_file_bytes, &cancel)
            .unwrap()
    }

    #[test]
    fn reports_a_proven_unused_side_effect_free_unit() {
        let (index, uri) = index_with_unused_unit();
        let cancel = AtomicBool::new(false);
        let report = index
            .semantic_diagnostic_report_with_cancel(&uri, &cancel)
            .unwrap();
        assert!(report.is_complete(), "semantic report: {report:?}");

        let diagnostics = index
            .unused_unit_diagnostics_with_report(&uri, &report, None, &cancel)
            .unwrap();

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].message, "Unit 'U' is not used",
            "an eligible uses entry gets the lint4d wording"
        );
        assert_eq!(
            diagnostics[0].span,
            SourceSpan { start: 23, end: 24 },
            "the hint anchors the unit name"
        );
    }

    #[test]
    fn does_not_report_when_the_document_compiler_tables_are_unknown() {
        let mut index = NavigationIndex::new();
        for (name, source) in [
            ("System", "unit System; interface implementation end."),
            ("SysInit", "unit SysInit; interface implementation end."),
            ("U", "unit U; interface implementation end."),
        ] {
            index
                .update(
                    Url::parse(&format!("file:///tmp/unused-unit/{name}.pas")).unwrap(),
                    source.into(),
                )
                .unwrap();
        }
        let uri = Url::parse("file:///tmp/unused-unit/D.pas").unwrap();
        index
            .update(
                uri.clone(),
                "unit D; interface uses U; implementation end.".into(),
            )
            .unwrap();

        assert!(unused_diagnostics(&index, &uri, None).is_empty());
    }

    #[test]
    fn does_not_report_when_the_semantic_pass_has_a_coverage_gap() {
        let (index, uri) = index_with_unused_unit();
        let cancel = AtomicBool::new(false);
        let mut report = index
            .semantic_diagnostic_report_with_cancel(&uri, &cancel)
            .unwrap();
        report.note(SemanticCoverageGap::Budget);

        assert!(
            index
                .unused_unit_diagnostics_with_report(&uri, &report, None, &cancel)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn does_not_report_after_parser_recovery_or_an_unknown_conditional() {
        for source in [
            "unit D; interface uses U implementation end.",
            "unit D; interface {$IFDEF UNKNOWN} uses U; {$ENDIF} implementation end.",
        ] {
            let (index, uri) = index_with_sources(
                &[("U", "unit U; interface implementation end.")],
                "D",
                source,
            );
            let cancel = AtomicBool::new(false);
            let report = index
                .semantic_diagnostic_report_with_cancel(&uri, &cancel)
                .unwrap();
            assert!(
                index
                    .unused_unit_diagnostics_with_report(&uri, &report, None, &cancel)
                    .unwrap()
                    .is_empty(),
                "source: {source}; report: {report:?}"
            );
        }
    }

    #[test]
    fn does_not_report_when_an_active_include_was_not_expanded() {
        let (index, uri) = index_with_sources(
            &[("U", "unit U; interface implementation end.")],
            "D",
            "unit D; {$I Missing.inc} interface uses U; implementation end.",
        );

        assert!(unused_diagnostics(&index, &uri, None).is_empty());
    }

    #[test]
    fn all_known_unit_side_effects_make_the_import_ineligible() {
        for source in [
            "unit U; interface implementation initialization end.",
            "unit U; interface implementation finalization end.",
            "unit U; interface implementation begin end.",
            "unit U; interface type T = class class constructor Create; end; implementation end.",
            "unit U; interface type T = class class destructor Destroy; end; implementation end.",
            "unit U; interface implementation {$R Data.res} end.",
            "unit U; interface implementation exports Run; end.",
        ] {
            let (index, uri) = index_with_sources(
                &[("U", source)],
                "D",
                "unit D; interface uses U; implementation end.",
            );
            assert!(
                unused_diagnostics(&index, &uri, None).is_empty(),
                "side-effect source: {source}"
            );
        }
    }

    #[test]
    fn a_transitive_side_effect_already_in_other_uses_closure_is_preserved() {
        let (index, uri) = index_with_sources(
            &[
                ("V", "unit V; interface implementation initialization end."),
                ("U", "unit U; interface uses V; implementation end."),
            ],
            "D",
            "unit D; interface uses U, V; implementation end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'U' is not used"]
        );
    }

    #[test]
    fn a_transitive_side_effect_not_preserved_by_other_uses_blocks_the_hint() {
        let (index, uri) = index_with_sources(
            &[
                ("V", "unit V; interface implementation initialization end."),
                ("U", "unit U; interface uses V; implementation end."),
            ],
            "D",
            "unit D; interface uses U; implementation end.",
        );

        assert!(unused_diagnostics(&index, &uri, None).is_empty());
    }

    #[test]
    fn an_unindexed_transitive_unit_blocks_the_hint() {
        let (index, uri) = index_with_sources(
            &[("U", "unit U; interface uses Missing; implementation end.")],
            "D",
            "unit D; interface uses U; implementation end.",
        );

        assert!(unused_diagnostics(&index, &uri, None).is_empty());
    }

    #[test]
    fn helper_member_reference_keeps_its_declaring_unit_used() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "Base",
                    "unit Base; interface type TBase = class end; implementation end.",
                ),
                (
                    "U",
                    "unit U; interface uses Base; type TBaseHelper = class helper for TBase procedure Touch; end; implementation procedure TBaseHelper.Touch; begin end; end.",
                ),
                ("V", "unit V; interface implementation end."),
            ],
            "D",
            "unit D; interface uses Base, U, V; implementation procedure Work; var item: TBase; begin item.Touch; end; end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'V' is not used"]
        );
    }

    #[test]
    fn attribute_short_name_resolves_to_the_attribute_suffix() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface type TMarkerAttribute = class end; implementation end.",
                ),
                ("V", "unit V; interface implementation end."),
            ],
            "D",
            "unit D; interface uses U, V; type [TMarker] TAnnotated = class end; implementation end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'V' is not used"]
        );
    }

    #[test]
    fn generic_constraint_reference_keeps_its_declaring_unit_used() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface type IMarker = interface end; implementation end.",
                ),
                ("V", "unit V; interface implementation end."),
            ],
            "D",
            "unit D; interface uses U, V; type TBox<T: U.IMarker> = class end; implementation end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'V' is not used"]
        );
    }

    #[test]
    fn field_and_parameter_type_references_keep_their_unit_used() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface type TExternal = class end; implementation end.",
                ),
                ("V", "unit V; interface implementation end."),
            ],
            "D",
            "unit D; interface uses U, V; type TLocal = class F: TExternal; end; procedure Work(Value: U.TExternal); implementation end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'V' is not used"]
        );
    }

    #[test]
    fn ambiguous_type_reference_blocks_all_unused_unit_hints() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface type TShared = class end; implementation end.",
                ),
                (
                    "V",
                    "unit V; interface type TShared = class end; implementation end.",
                ),
                ("W", "unit W; interface implementation end."),
            ],
            "D",
            "unit D; interface uses U, V, W; type TLocal = TShared; implementation end.",
        );

        assert!(unused_diagnostics(&index, &uri, None).is_empty());
    }

    #[test]
    fn form_file_class_from_the_candidate_unit_keeps_that_unit_used() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface type TStreamed = class end; implementation end.",
                ),
                ("V", "unit V; interface implementation end."),
            ],
            "D",
            "unit D; {$R *.dfm} interface uses U, V; implementation end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, Some(b"object Form: TStreamed\nend\n"));

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'V' is not used"]
        );
    }

    #[test]
    fn missing_binary_or_malformed_form_data_blocks_every_hint() {
        let (index, uri) = index_with_sources(
            &[
                ("U", "unit U; interface implementation end."),
                ("V", "unit V; interface implementation end."),
            ],
            "D",
            "unit D; {$R *.dfm} interface uses U, V; implementation end.",
        );

        for bytes in [
            None,
            Some(&b"TPF0\x00\x01"[..]),
            Some(&b"object Form TBroken\nend\n"[..]),
        ] {
            assert!(
                unused_diagnostics(&index, &uri, bytes).is_empty(),
                "form bytes: {bytes:?}"
            );
        }
    }

    #[test]
    fn form_class_from_another_unit_does_not_block_the_hint() {
        let (index, uri) = index_with_sources(
            &[
                ("U", "unit U; interface implementation end."),
                (
                    "V",
                    "unit V; interface type TStreamed = class end; implementation end.",
                ),
            ],
            "D",
            "unit D; {$R *.dfm} interface uses U, V; implementation end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, Some(b"object Form: TStreamed\nend\n"));

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'U' is not used"]
        );
    }

    #[test]
    fn inline_body_dependency_keeps_its_unit_used() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface function Needed: Integer; implementation function Needed: Integer; begin Result := 1; end; end.",
                ),
                (
                    "InlineProvider",
                    "unit InlineProvider; interface function InlineValue: Integer; inline; implementation uses U; function InlineValue: Integer; begin Result := Needed; end; end.",
                ),
            ],
            "D",
            "unit D; interface uses U, InlineProvider; implementation procedure Work; var I: Integer; begin I := InlineValue; end; end.",
        );

        assert!(
            unused_diagnostics(&index, &uri, None).is_empty(),
            "a call to an inline routine keeps its body dependencies"
        );
    }

    #[test]
    fn unit_qualified_reference_keeps_the_qualified_unit_used() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface type TExternal = class end; implementation end.",
                ),
                ("V", "unit V; interface implementation end."),
            ],
            "D",
            "unit D; interface uses U, V; type TLocal = U.TExternal; implementation end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'V' is not used"]
        );
    }

    #[test]
    fn project_uses_in_path_entries_are_not_reported() {
        let mut index = NavigationIndex::new();
        for (name, source) in [
            ("System", "unit System; interface implementation end."),
            ("SysInit", "unit SysInit; interface implementation end."),
            ("U", "unit U; interface implementation end."),
            ("V", "unit V; interface implementation end."),
        ] {
            add_unit(&mut index, name, source);
        }
        let uri = Url::parse("file:///tmp/unused-unit/App.dpr").unwrap();
        index
            .update_with_context_with_cancel(
                uri.clone(),
                "program App; uses U in 'U.pas', V; begin end.".to_owned(),
                &context(),
                &AtomicBool::new(false),
            )
            .unwrap();

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'V' is not used"],
            "only the project unit selected with an explicit path is exempt"
        );
    }

    #[test]
    fn package_contains_entries_are_not_reported() {
        let mut index = NavigationIndex::new();
        for (name, source) in [
            ("System", "unit System; interface implementation end."),
            ("SysInit", "unit SysInit; interface implementation end."),
            ("U", "unit U; interface implementation end."),
        ] {
            add_unit(&mut index, name, source);
        }
        let uri = Url::parse("file:///tmp/unused-unit/Package.dpk").unwrap();
        index
            .update_with_context_with_cancel(
                uri.clone(),
                "package Package; contains U; end.".to_owned(),
                &context(),
                &AtomicBool::new(false),
            )
            .unwrap();

        assert!(unused_diagnostics(&index, &uri, None).is_empty());
    }
}
