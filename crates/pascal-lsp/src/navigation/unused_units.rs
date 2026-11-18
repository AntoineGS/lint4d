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
        let document_has_indexing = document_has_indexing_expression(document, cancel, budget)?;
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
            if !name.starts_with('&') && is_implicit_or_intrinsic_name(&canonical_name(name)) {
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
                || report.diagnostics.iter().any(|diagnostic| {
                    diagnostic.span.start < import.span.end
                        && import.span.start < diagnostic.span.end
                })
                || import_is_project_path(uri, document, import)
                || import_is_duplicated(document, &unit_name)
                || import_is_in_conditional_uses_clause(document, import.span, cancel, budget)?
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
            let hazards = match budget_result(
                unit_lookup_hazards(provider, cancel, budget),
                budget,
                cancel,
            )? {
                Some(hazards) => hazards,
                None => continue,
            };
            if hazards.operator_or_helper
                || (document_has_indexing && hazards.default_indexed_property)
            {
                continue;
            }
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

#[derive(Debug, Default)]
struct UnitLookupHazards {
    operator_or_helper: bool,
    default_indexed_property: bool,
}

fn unit_lookup_hazards(
    document: &Document,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<UnitLookupHazards, String> {
    let mut hazards = UnitLookupHazards::default();
    let mut pending = vec![document.tree.root_node()];
    budget.require_owned_bytes(std::mem::size_of::<Node<'_>>(), cancel)?;
    while let Some(node) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        match node.kind() {
            "declHelper" => hazards.operator_or_helper = true,
            "declProc"
                if has_direct_child(node, "kClass") && has_direct_child(node, "kOperator") =>
            {
                hazards.operator_or_helper = true;
            }
            "declProp"
                if has_direct_child(node, "declPropArgs")
                    && has_direct_child_with_child(node, "procAttribute", "kDefault") =>
            {
                hazards.default_indexed_property = true;
            }
            _ => {}
        }
        if hazards.operator_or_helper && hazards.default_indexed_property {
            break;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            budget.require_owned_bytes(std::mem::size_of::<Node<'_>>(), cancel)?;
            pending.push(child);
        }
    }
    Ok(hazards)
}

fn has_direct_child(node: Node<'_>, kind: &str) -> bool {
    (0..node.child_count())
        .filter_map(|index| node.child(index))
        .any(|child| child.kind() == kind)
}

fn has_direct_child_with_child(node: Node<'_>, parent_kind: &str, child_kind: &str) -> bool {
    (0..node.child_count())
        .filter_map(|index| node.child(index))
        .filter(|child| child.kind() == parent_kind)
        .any(|parent| has_direct_child(parent, child_kind))
}

fn document_has_indexing_expression(
    document: &Document,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let mut pending = vec![document.tree.root_node()];
    budget.require_owned_bytes(std::mem::size_of::<Node<'_>>(), cancel)?;
    while let Some(node) = pending.pop() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if node.kind() == "exprSubscript" {
            return Ok(true);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            budget.require_owned_bytes(std::mem::size_of::<Node<'_>>(), cancel)?;
            pending.push(child);
        }
    }
    Ok(false)
}

fn import_is_in_conditional_uses_clause(
    document: &Document,
    import_span: SourceSpan,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let root = document.tree.root_node();
    let Some(mut module_name) =
        root.named_descendant_for_byte_range(import_span.start, import_span.end)
    else {
        return Ok(true);
    };
    loop {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if module_name.kind() == "moduleName"
            && module_name.start_byte() == import_span.start
            && module_name.end_byte() == import_span.end
        {
            break;
        }
        if module_name.start_byte() > import_span.start || import_span.end > module_name.end_byte()
        {
            return Ok(true);
        }
        let Some(parent) = module_name.parent() else {
            return Ok(true);
        };
        module_name = parent;
    }
    let mut parent = module_name.parent();
    let clause = loop {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let Some(current) = parent else {
            return Ok(true);
        };
        parent = current.parent();
        if current.kind() == "declUses" {
            break current;
        }
    };

    for directive in &document.conditionals.directives {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if directive.start < clause.end_byte() && clause.start_byte() < directive.end {
            return Ok(true);
        }
    }

    let mut conditional_depth = 0usize;
    for directive in &document.conditionals.directives {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if directive.start >= import_span.start {
            break;
        }
        match directive.kind {
            DirectiveKind::ConditionalStart => {
                conditional_depth = conditional_depth.saturating_add(1);
            }
            DirectiveKind::ConditionalEnd => {
                conditional_depth = conditional_depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    Ok(conditional_depth > 0)
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
    fn escaped_intrinsic_spelling_resolves_to_an_imported_declaration() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface procedure &Exit; implementation procedure &Exit; begin end; end.",
                ),
                ("V", "unit V; interface implementation end."),
            ],
            "D",
            "unit D; interface uses U, V; implementation procedure Work; begin &Exit; end; end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'V' is not used"],
            "escaping an intrinsic spelling makes it an ordinary identifier"
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
    fn dcc_unit_warning_at_a_uses_entry_blocks_an_unused_unit_hint() {
        for (directive, code) in [
            ("library", "W1004"),
            ("platform", "W1005"),
            ("deprecated 'use another unit instead'", "W1006"),
            ("experimental", "W1007"),
        ] {
            let provider = format!("unit U {directive}; interface implementation end.");
            let (index, uri) = index_with_sources(
                &[("U", provider.as_str())],
                "D",
                "unit D; interface uses U; implementation end.",
            );
            let cancel = AtomicBool::new(false);
            let report = index
                .semantic_diagnostic_report_with_cancel(&uri, &cancel)
                .unwrap();
            assert!(
                report
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.kind.code() == code),
                "unit declaration should produce {code}: {:?}",
                report.diagnostics
            );

            let unused = index
                .unused_unit_diagnostics_with_report(&uri, &report, None, &cancel)
                .unwrap();
            assert!(
                unused.is_empty(),
                "removing the entry would remove {code}: {unused:?}"
            );
        }
    }

    #[test]
    fn class_operator_declarations_in_direct_or_nested_types_block_unused_hints() {
        let cases = [
            (
                "implicit",
                "unit U; interface type TNumber = record class operator Implicit(Value: Integer): TNumber; end; implementation end.",
            ),
            (
                "explicit",
                "unit U; interface type TNumber = record class operator Explicit(Value: Integer): TNumber; end; implementation end.",
            ),
            (
                "add",
                "unit U; interface type TNumber = record class operator Add(Left, Right: TNumber): TNumber; end; implementation end.",
            ),
            (
                "equal",
                "unit U; interface type TNumber = record class operator Equal(Left, Right: TNumber): Boolean; end; implementation end.",
            ),
            (
                "nested",
                "unit U; interface type TOuter = class type TNested = record class operator Implicit(Value: Integer): TNested; end; end; implementation end.",
            ),
        ];

        for (case, unit_source) in cases {
            let (index, uri) = index_with_sources(
                &[
                    ("U", unit_source),
                    ("V", "unit V; interface implementation end."),
                ],
                "D",
                "unit D; interface uses U, V; implementation end.",
            );
            let diagnostics = unused_diagnostics(&index, &uri, None);
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic.message != "Unit 'U' is not used"),
                "{case} operator should make U ineligible: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn implicit_conversion_into_an_imported_overload_keeps_operator_unit_used() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface type TNumber = record class operator Implicit(Value: Integer): TNumber; end; implementation end.",
                ),
                (
                    "V",
                    "unit V; interface uses U; function Consume(Value: TNumber): Integer; implementation function Consume(Value: TNumber): Integer; begin Result := Value; end; end.",
                ),
            ],
            "D",
            "unit D; interface uses U, V; implementation procedure Work; begin Consume(1); end; end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.message != "Unit 'U' is not used"),
            "implicit conversions participate in the imported overload: {diagnostics:?}"
        );
    }

    #[test]
    fn record_and_class_helpers_block_unused_unit_hints() {
        for (kind, unit_source) in [
            (
                "record helper",
                "unit U; interface type TIntHelper = record helper for Integer function Twice: Integer; end; implementation end.",
            ),
            (
                "class helper",
                "unit U; interface type TObjectHelper = class helper for TObject procedure Touch; end; implementation end.",
            ),
        ] {
            let (index, uri) = index_with_sources(
                &[
                    ("U", unit_source),
                    ("V", "unit V; interface implementation end."),
                ],
                "D",
                "unit D; interface uses U, V; implementation end.",
            );
            let diagnostics = unused_diagnostics(&index, &uri, None);
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic.message != "Unit 'U' is not used"),
                "{kind} changes member lookup and must make U ineligible: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn indexing_with_a_default_array_property_keeps_its_declaring_unit() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface type TIndexed = class function GetItem(Index: Integer): Integer; property Items[Index: Integer]: Integer read GetItem; default; end; implementation function TIndexed.GetItem(Index: Integer): Integer; begin Result := Index; end; end.",
                ),
                (
                    "V",
                    "unit V; interface uses U; type TChild = class(TIndexed) end; implementation end.",
                ),
            ],
            "D",
            "unit D; interface uses U, V; implementation procedure Work; var Item: TChild; Value: Integer; begin Value := Item[0]; end; end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.message != "Unit 'U' is not used"),
            "indexing can invoke U's default property without naming it: {diagnostics:?}"
        );
    }

    #[test]
    fn transitive_resourcestring_side_effect_blocks_unused_import_hint() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface uses SideEffects; implementation end.",
                ),
                (
                    "SideEffects",
                    "unit SideEffects; interface resourcestring DisplayName = 'side effect'; implementation end.",
                ),
                ("V", "unit V; interface implementation end."),
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
            ["Unit 'V' is not used"],
            "removing U would remove a transitive resourcestring initializer"
        );
    }

    #[test]
    fn transitive_message_side_effect_blocks_unused_import_hint() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface uses SideEffects; implementation end.",
                ),
                (
                    "SideEffects",
                    "unit SideEffects; interface implementation {$MESSAGE WARN 'side effect'} end.",
                ),
                ("V", "unit V; interface implementation end."),
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
            ["Unit 'V' is not used"],
            "removing U would remove a transitive MESSAGE directive"
        );
    }

    #[test]
    fn conditional_uses_clauses_withhold_unused_unit_hints() {
        let cases = [
            (
                "conditional entry",
                "unit D; interface {$DEFINE FEATURE} uses {$IFDEF FEATURE} U, {$ENDIF} V; implementation end.",
            ),
            (
                "unconditional entry beside conditional entry",
                "unit D; interface {$DEFINE FEATURE} uses U, {$IFDEF FEATURE} V {$ENDIF}; implementation end.",
            ),
            (
                "whole uses clause inside known conditional region",
                "unit D; interface {$DEFINE FEATURE} {$IFDEF FEATURE} uses U; {$ENDIF} implementation end.",
            ),
        ];

        for (case, consumer) in cases {
            let (index, uri) = index_with_sources(
                &[
                    ("U", "unit U; interface implementation end."),
                    ("V", "unit V; interface implementation end."),
                ],
                "D",
                consumer,
            );
            let document = index.documents.get(&uri).expect("consumer document");
            assert!(
                document.parser_recovery_spans.is_empty(),
                "{case} fixture recovered: {:?}",
                document.parser_recovery_spans
            );
            let diagnostics = unused_diagnostics(&index, &uri, None);
            assert!(
                diagnostics.is_empty(),
                "{case} must withhold findings for directive-bearing uses: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn overload_candidate_from_an_unselected_import_keeps_its_unit_used() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface function Choose(Value: Integer): Integer; overload; implementation function Choose(Value: Integer): Integer; begin Result := Value; end; end.",
                ),
                (
                    "V",
                    "unit V; interface function Choose(Value: string): Integer; overload; implementation function Choose(Value: string): Integer; begin Result := Length(Value); end; end.",
                ),
                ("W", "unit W; interface implementation end."),
            ],
            "D",
            "unit D; interface uses U, V, W; implementation procedure Work; var Value: Integer; begin Value := Choose('selected from V'); end; end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'W' is not used"],
            "U remains in the overload candidate set although V's overload is selected"
        );
    }

    #[test]
    fn published_property_and_field_types_keep_their_units_used() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface type TExternal = class end; implementation end.",
                ),
                ("V", "unit V; interface implementation end."),
            ],
            "D",
            "unit D; interface uses U, V; type TLocal = class published FExternal: U.TExternal; property External: U.TExternal read FExternal; end; implementation end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'V' is not used"],
            "published fields and property signatures expose U's type"
        );
    }

    #[test]
    fn interface_type_used_only_as_supports_guid_keeps_its_unit_used() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface type IFoo = interface ['{00000000-0000-0000-0000-000000000001}'] end; implementation end.",
                ),
                ("V", "unit V; interface implementation end."),
                (
                    "SysUtils",
                    "unit SysUtils; interface function Supports(Instance: IInterface; IID: TGUID): Boolean; implementation function Supports(Instance: IInterface; IID: TGUID): Boolean; begin Result := False; end; end.",
                ),
            ],
            "D",
            "unit D; interface uses U, V, SysUtils; implementation procedure Work(Value: IInterface); begin if Supports(Value, IFoo) then Value := Value; end; end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'V' is not used"],
            "the Supports IID argument binds the imported interface type"
        );
    }

    #[test]
    fn guid_constant_used_only_as_supports_argument_keeps_its_unit_used() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface const IID_IFoo: TGUID = '{00000000-0000-0000-0000-000000000001}'; implementation end.",
                ),
                ("V", "unit V; interface implementation end."),
                (
                    "SysUtils",
                    "unit SysUtils; interface function Supports(Instance: IInterface; IID: TGUID): Boolean; implementation function Supports(Instance: IInterface; IID: TGUID): Boolean; begin Result := False; end; end.",
                ),
            ],
            "D",
            "unit D; interface uses U, V, SysUtils; implementation procedure Work(Value: IInterface); begin if Supports(Value, IID_IFoo) then Value := Value; end; end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'V' is not used"],
            "the Supports GUID argument binds the imported constant"
        );
    }

    #[test]
    fn known_active_conditional_code_usage_keeps_its_unit_used() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface function Needed: Integer; implementation function Needed: Integer; begin Result := 1; end; end.",
                ),
                ("V", "unit V; interface implementation end."),
            ],
            "D",
            "unit D; interface uses U, V; implementation procedure Work; var Value: Integer; begin {$DEFINE FEATURE} {$IFDEF FEATURE} Value := Needed; {$ENDIF} end; end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'V' is not used"],
            "a closed, known-active branch contributes its binding"
        );
    }

    #[test]
    fn inactive_conditional_code_usage_does_not_keep_its_unit_used() {
        let (index, uri) = index_with_sources(
            &[
                (
                    "U",
                    "unit U; interface function Needed: Integer; implementation function Needed: Integer; begin Result := 1; end; end.",
                ),
                ("V", "unit V; interface implementation end."),
            ],
            "D",
            "unit D; interface uses U, V; implementation procedure Work; var Value: Integer; begin {$UNDEF NEVER_DEFINED} {$IFDEF NEVER_DEFINED} Value := Needed; {$ENDIF} end; end.",
        );

        let diagnostics = unused_diagnostics(&index, &uri, None);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unit 'U' is not used", "Unit 'V' is not used"],
            "inactive branch identifiers do not count as unit use"
        );
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
