#![allow(dead_code)]

use super::{
    AssistanceBudget, Document, NavigationIndex, Region, canonical_name, check_navigation_cancel,
};
use pascal_core::conditional::{DirectiveKind, Truth};
use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::AtomicBool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SideEffects {
    None,
    Some(Vec<SideEffectKind>),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SideEffectKind {
    Initialization,
    Finalization,
    UnitBlock,
    ClassConstructor,
    ClassDestructor,
    Resource,
    Exports,
}

pub(crate) type CanonicalUnit = String;
pub(crate) type UsesEntryRef = String;

pub(crate) fn unit_side_effects(
    document: &Document,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<SideEffects, String> {
    check_navigation_cancel(cancel)?;
    if !document.conditionals.complete
        || !document.conditionals.unknown_spans.is_empty()
        || !document.parser_recovery_spans.is_empty()
        || !document.opaque_ranges.is_empty()
    {
        return Ok(SideEffects::Unknown);
    }
    budget.require_bytes(document.source.len(), cancel)?;

    let mut effects = Vec::new();
    let mut cursor = document.tree.walk();
    'tree: loop {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let node = cursor.node();
        let effect = match node.kind() {
            "initialization" => Some(if node_has_child(node, "kBegin") {
                SideEffectKind::UnitBlock
            } else {
                SideEffectKind::Initialization
            }),
            "finalization" => Some(SideEffectKind::Finalization),
            "declExports" => Some(SideEffectKind::Exports),
            "declProc" => class_lifecycle_effect(node),
            _ => None,
        };
        if let Some(effect) = effect {
            record_effect(&mut effects, effect);
        }

        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                break 'tree;
            }
        }
    }

    for directive in &document.conditionals.directives {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        budget.require_bytes(directive.body.len(), cancel)?;
        if directive.activity == Truth::False {
            continue;
        }
        match resource_directive(&directive.body) {
            ResourceDirective::None => {}
            ResourceDirective::Resource if directive.activity == Truth::True => {
                record_effect(&mut effects, SideEffectKind::Resource);
            }
            ResourceDirective::Resource | ResourceDirective::Unknown => {
                return Ok(SideEffects::Unknown);
            }
        }
        if directive.kind == DirectiveKind::Include {
            return Ok(SideEffects::Unknown);
        }
    }

    if effects.is_empty() {
        Ok(SideEffects::None)
    } else {
        Ok(SideEffects::Some(effects))
    }
}

/// Transitive uses closure (interface + implementation) over indexed documents;
/// `None` when any member is unknown/unindexed/incomplete or the budget runs out.
pub(crate) fn uses_closure(
    index: &NavigationIndex,
    document: &Document,
    entries: &[UsesEntryRef],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<BTreeSet<CanonicalUnit>>, String> {
    if !closure_budget(
        budget,
        cancel,
        entries.len(),
        entries
            .len()
            .saturating_mul(std::mem::size_of::<(Option<lsp_types::Url>, CanonicalUnit)>()),
    )? {
        return Ok(None);
    }

    let mut closure = BTreeSet::new();
    let mut frontier: Vec<(CanonicalUnit, Option<lsp_types::Url>)> = Vec::new();
    for entry in entries {
        check_navigation_cancel(cancel)?;
        let unit = canonical_name(entry);
        if unit.is_empty() {
            return Ok(None);
        }
        if !closure_budget(
            budget,
            cancel,
            1,
            unit.len()
                .saturating_add(std::mem::size_of::<(Option<lsp_types::Url>, CanonicalUnit)>()),
        )? {
            return Ok(None);
        }
        frontier.push((unit, None));
    }

    let mut visited_edges = HashSet::new();
    let mut expanded_documents = HashSet::new();
    while let Some((unit, parent_uri)) = frontier.pop() {
        check_navigation_cancel(cancel)?;
        if !closure_budget(
            budget,
            cancel,
            1,
            unit.len()
                .saturating_add(parent_uri.as_ref().map_or(0, |uri| uri.as_str().len()))
                .saturating_add(std::mem::size_of::<(Option<lsp_types::Url>, CanonicalUnit)>()),
        )? {
            return Ok(None);
        }
        let edge = (parent_uri.clone(), unit.clone());
        if !visited_edges.insert(edge) {
            continue;
        }
        let source = match parent_uri.as_ref() {
            Some(uri) => {
                let Some(source) = index.documents.get(uri) else {
                    return Ok(None);
                };
                source
            }
            None => document,
        };
        if source.unknown_imports.contains(&unit) {
            return Ok(None);
        }

        let Some(urls) = budget_result(
            index.unit_urls_for_import_with_budget(source, &unit, cancel, budget),
            budget,
            cancel,
        )?
        else {
            return Ok(None);
        };
        if urls.len() != 1 {
            return Ok(None);
        }
        let provider_uri = urls.into_iter().next().expect("single provider");
        let Some(provider) = index.documents.get(&provider_uri) else {
            return Ok(None);
        };
        let Some(declared_unit) = parsed_unit_name(provider) else {
            return Ok(None);
        };
        if declared_unit != unit || canonical_name(&provider.unit_name) != unit {
            return Ok(None);
        }
        let Some(complete) = budget_result(
            NavigationIndex::document_is_complete_for_proof(provider, cancel, budget),
            budget,
            cancel,
        )?
        else {
            return Ok(None);
        };
        if !complete || !provider.conditionals.complete || !provider.opaque_ranges.is_empty() {
            return Ok(None);
        }
        closure.insert(unit);

        if !expanded_documents.insert(provider_uri.clone()) {
            continue;
        }
        let Some(uses) = budget_result(
            provider.active_uses_with_budget(Region::Other, cancel, budget),
            budget,
            cancel,
        )?
        else {
            return Ok(None);
        };
        for dependency in uses {
            check_navigation_cancel(cancel)?;
            let dependency = canonical_name(dependency);
            if dependency.is_empty() || provider.unknown_imports.contains(&dependency) {
                return Ok(None);
            }
            if !closure_budget(
                budget,
                cancel,
                1,
                dependency
                    .len()
                    .saturating_add(provider_uri.as_str().len())
                    .saturating_add(std::mem::size_of::<(Option<lsp_types::Url>, CanonicalUnit)>()),
            )? {
                return Ok(None);
            }
            frontier.push((dependency, Some(provider_uri.clone())));
        }
    }
    Ok(Some(closure))
}

fn node_has_child(node: tree_sitter::Node<'_>, kind: &str) -> bool {
    (0..node.child_count())
        .filter_map(|index| node.child(index))
        .any(|child| child.kind() == kind)
}

fn parsed_unit_name(document: &Document) -> Option<String> {
    let root = document.tree.root_node();
    let mut root_cursor = root.walk();
    let unit = root
        .named_children(&mut root_cursor)
        .find(|child| child.kind() == "unit")?;
    let mut unit_cursor = unit.walk();
    let module_name = unit
        .named_children(&mut unit_cursor)
        .find(|child| child.kind() == "moduleName")?;
    let name = super::canonical_path(&super::identifier_texts(module_name, &document.source));
    (!name.is_empty()).then_some(name)
}

fn class_lifecycle_effect(node: tree_sitter::Node<'_>) -> Option<SideEffectKind> {
    let mut has_class = false;
    let mut constructor = false;
    let mut destructor = false;
    for index in 0..node.child_count() {
        let Some(child) = node.child(index) else {
            continue;
        };
        match child.kind() {
            "kClass" => has_class = true,
            "kConstructor" => constructor = true,
            "kDestructor" => destructor = true,
            _ => {}
        }
    }
    if !has_class {
        return None;
    }
    if constructor {
        Some(SideEffectKind::ClassConstructor)
    } else if destructor {
        Some(SideEffectKind::ClassDestructor)
    } else {
        None
    }
}

fn record_effect(effects: &mut Vec<SideEffectKind>, effect: SideEffectKind) {
    if !effects.contains(&effect) {
        effects.push(effect);
    }
}

enum ResourceDirective {
    None,
    Resource,
    Unknown,
}

fn resource_directive(body: &str) -> ResourceDirective {
    let body = body.trim_start();
    let keyword_end = body
        .char_indices()
        .find_map(|(index, character)| {
            (!character.is_ascii_alphanumeric() && character != '_').then_some(index)
        })
        .unwrap_or(body.len());
    let keyword = &body[..keyword_end];
    if !keyword.eq_ignore_ascii_case("r") && !keyword.eq_ignore_ascii_case("resource") {
        return ResourceDirective::None;
    }
    let argument = body[keyword_end..]
        .trim_start_matches(|character: char| character.is_ascii_whitespace() || character == ':')
        .trim();
    if argument == "+" || argument == "-" {
        return ResourceDirective::None;
    }
    if argument.is_empty() {
        ResourceDirective::Unknown
    } else {
        ResourceDirective::Resource
    }
}

fn closure_budget(
    budget: &mut AssistanceBudget,
    cancel: &AtomicBool,
    work: usize,
    bytes: usize,
) -> Result<bool, String> {
    if let Err(error) = budget.require_work(work, cancel) {
        return if budget.exhausted_by(&error, cancel) {
            Ok(false)
        } else {
            Err(error)
        };
    }
    if let Err(error) = budget.require_owned_bytes(bytes, cancel) {
        return if budget.exhausted_by(&error, cancel) {
            Ok(false)
        } else {
            Err(error)
        };
    }
    Ok(true)
}

fn budget_result<T>(
    result: Result<T, String>,
    budget: &AssistanceBudget,
    cancel: &AtomicBool,
) -> Result<Option<T>, String> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if budget.exhausted_by(&error, cancel) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigation::ConditionalContext;
    use lsp_types::Url;
    use std::sync::atomic::AtomicBool;

    fn document(source: &str) -> Document {
        let uri = Url::parse("file:///tmp/unit-facts.pas").expect("fixture URI");
        Document::parse_with_cancel(
            uri,
            source.to_owned(),
            &ConditionalContext::default(),
            &AtomicBool::new(false),
            None,
        )
        .expect("fixture parses")
    }

    fn side_effects(source: &str) -> SideEffects {
        let document = document(source);
        let mut budget = AssistanceBudget::new(10_000, 1024 * 1024, "unit facts test");
        unit_side_effects(&document, &AtomicBool::new(false), &mut budget)
            .expect("side-effect analysis succeeds")
    }

    #[test]
    fn unit_with_no_side_effects_is_known_empty() {
        assert_eq!(
            side_effects("unit U; interface implementation end."),
            SideEffects::None
        );
    }

    #[test]
    fn initialization_section_is_a_side_effect() {
        assert_eq!(
            side_effects("unit U; interface implementation initialization end."),
            SideEffects::Some(vec![SideEffectKind::Initialization])
        );
    }

    #[test]
    fn finalization_section_is_a_side_effect() {
        assert_eq!(
            side_effects("unit U; interface implementation finalization end."),
            SideEffects::Some(vec![SideEffectKind::Finalization])
        );
    }

    #[test]
    fn legacy_unit_begin_block_is_a_unit_block_side_effect() {
        let source = "unit U; interface implementation begin end.";
        let document = document(source);
        assert!(
            document.parser_recovery_spans.is_empty(),
            "fixture recovery spans: {:?}\n{}",
            document.parser_recovery_spans,
            document.tree.root_node().to_sexp()
        );
        assert_eq!(
            side_effects(source),
            SideEffects::Some(vec![SideEffectKind::UnitBlock])
        );
    }

    #[test]
    fn class_constructor_is_a_side_effect() {
        assert_eq!(
            side_effects(
                "unit U; interface type TFoo = class class constructor Create; end; implementation end."
            ),
            SideEffects::Some(vec![SideEffectKind::ClassConstructor])
        );
    }

    #[test]
    fn class_destructor_is_a_side_effect() {
        assert_eq!(
            side_effects(
                "unit U; interface type TFoo = class class destructor Destroy; end; implementation end."
            ),
            SideEffects::Some(vec![SideEffectKind::ClassDestructor])
        );
    }

    #[test]
    fn resource_directives_are_side_effects() {
        assert_eq!(
            side_effects("unit U; interface implementation {$R *.res} {$RESOURCE Data.res} end."),
            SideEffects::Some(vec![SideEffectKind::Resource])
        );
    }

    #[test]
    fn range_check_directives_are_not_resource_side_effects() {
        assert_eq!(
            side_effects("unit U; interface implementation {$R+} {$R -} end."),
            SideEffects::None
        );
    }

    #[test]
    fn exports_section_is_a_side_effect() {
        assert_eq!(
            side_effects("unit U; interface implementation exports Run; end."),
            SideEffects::Some(vec![SideEffectKind::Exports])
        );
    }

    #[test]
    fn unknown_conditional_around_initialization_makes_side_effects_unknown() {
        assert_eq!(
            side_effects(
                "unit U; interface implementation {$IFDEF NOT_CONFIGURED} initialization {$ENDIF} end."
            ),
            SideEffects::Unknown
        );
    }

    #[test]
    fn potentially_active_include_makes_side_effects_unknown() {
        assert_eq!(
            side_effects("unit U; interface implementation {$I hidden.inc} end."),
            SideEffects::Unknown
        );
    }

    fn indexed_unit(index: &mut NavigationIndex, uri: &str, source: &str) -> Url {
        let uri = Url::parse(uri).expect("fixture URI");
        index
            .update(uri.clone(), source.to_owned())
            .expect("fixture parses");
        uri
    }

    #[test]
    fn uses_closure_terminates_on_cycles() {
        let mut index = NavigationIndex::new();
        let document_uri = indexed_unit(
            &mut index,
            "file:///tmp/closure/Consumer.pas",
            "unit Consumer; interface uses A; implementation end.",
        );
        indexed_unit(
            &mut index,
            "file:///tmp/closure/A.pas",
            "unit A; interface uses B; implementation uses C; end.",
        );
        indexed_unit(
            &mut index,
            "file:///tmp/closure/B.pas",
            "unit B; interface uses A; implementation end.",
        );
        indexed_unit(
            &mut index,
            "file:///tmp/closure/C.pas",
            "unit C; interface implementation end.",
        );
        let document = index
            .documents
            .get(&document_uri)
            .expect("indexed document");
        let mut budget = AssistanceBudget::new(10_000, 1024 * 1024, "unit closure test");

        let closure = uses_closure(
            &index,
            document,
            &["A".to_owned()],
            &AtomicBool::new(false),
            &mut budget,
        )
        .expect("closure analysis succeeds");

        assert_eq!(
            closure,
            Some(BTreeSet::from([
                "a".to_owned(),
                "b".to_owned(),
                "c".to_owned()
            ]))
        );
    }

    #[test]
    fn uses_closure_fails_closed_for_an_unindexed_unit() {
        let mut index = NavigationIndex::new();
        let document_uri = indexed_unit(
            &mut index,
            "file:///tmp/closure/Consumer.pas",
            "unit Consumer; interface uses Missing; implementation end.",
        );
        let document = index
            .documents
            .get(&document_uri)
            .expect("indexed document");
        let mut budget = AssistanceBudget::new(10_000, 1024 * 1024, "unit closure test");

        assert_eq!(
            uses_closure(
                &index,
                document,
                &["Missing".to_owned()],
                &AtomicBool::new(false),
                &mut budget,
            )
            .expect("closure analysis succeeds"),
            None
        );
    }

    #[test]
    fn uses_closure_fails_closed_for_same_named_fragment() {
        let mut index = NavigationIndex::new();
        let document_uri = indexed_unit(
            &mut index,
            "file:///tmp/closure/Consumer.pas",
            "unit Consumer; interface uses A; implementation end.",
        );
        let fragment_uri = indexed_unit(
            &mut index,
            "file:///tmp/closure/A.pas",
            "type TWidget = class end;",
        );
        let document = index
            .documents
            .get(&document_uri)
            .expect("indexed document");
        let fragment = index
            .documents
            .get(&fragment_uri)
            .expect("indexed fragment");
        assert_eq!(fragment.unit_name, "a", "filename fallback matches import");
        let mut budget = AssistanceBudget::new(10_000, 1024 * 1024, "unit closure test");

        assert_eq!(
            uses_closure(
                &index,
                document,
                &["A".to_owned()],
                &AtomicBool::new(false),
                &mut budget,
            )
            .expect("closure analysis succeeds"),
            None
        );
    }

    #[test]
    fn uses_closure_returns_none_when_budget_is_exhausted() {
        let mut index = NavigationIndex::new();
        let document_uri = indexed_unit(
            &mut index,
            "file:///tmp/closure/Consumer.pas",
            "unit Consumer; interface uses A; implementation end.",
        );
        indexed_unit(
            &mut index,
            "file:///tmp/closure/A.pas",
            "unit A; interface implementation end.",
        );
        let document = index
            .documents
            .get(&document_uri)
            .expect("indexed document");
        let mut budget = AssistanceBudget::new(0, 0, "unit closure test");

        assert_eq!(
            uses_closure(
                &index,
                document,
                &["A".to_owned()],
                &AtomicBool::new(false),
                &mut budget,
            )
            .expect("budget exhaustion is a fail-closed result"),
            None
        );
    }
}
