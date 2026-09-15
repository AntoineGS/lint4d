use super::{
    AncestryResolutionState, AncestryStatus, AssistanceBudget, BuiltinType, Candidate, Document,
    GenericSubstitution, IntegerKind, NavigationIndex, Origin, ParameterMode, Receiver, Region,
    ResolutionState, ResolvedType, RoutineParameter, Span, Symbol, SymbolKind, TypeIdentity,
    TypeInstance, TypeKind, TypeRef, assistance, canonical_name, check_navigation_cancel,
};
use lsp_types::Url;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::AtomicBool;
use tree_sitter::Node;

const MAX_OVERLOAD_GROUPS: usize = 128;
const MAX_OVERLOAD_ARGUMENTS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct OverloadKey {
    uri: Url,
    routine_key: String,
}

#[derive(Debug, Default)]
pub(super) struct Selection {
    pub(super) selected_group: Option<OverloadKey>,
    pub(super) generic_substitution: Option<GenericSubstitution>,
    pub(super) no_viable_group: bool,
}

#[derive(Debug, Clone)]
struct ArgumentInfo {
    ty: Option<TypeIdentity>,
    assignable: bool,
    nil_literal: bool,
}

#[derive(Debug, Clone)]
struct GroupScore {
    cost: u32,
    uncertain: bool,
    substitution: GenericSubstitution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conversion {
    Cost(u32),
    Unknown,
    Incompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Upcast {
    Distance(u32),
    No,
    Unknown,
}

#[derive(Debug, Clone)]
struct RoutineGroup {
    key: OverloadKey,
    representative: Candidate,
}

pub(super) fn call_for_identifier(identifier: Node<'_>) -> Option<Node<'_>> {
    let identifier_span = Span::from_node(identifier);
    let mut current = identifier.parent();
    while let Some(node) = current {
        if node.kind() == "exprCall"
            && node
                .child_by_field_name("entity")
                .is_some_and(|entity| Span::from_node(entity).contains(identifier_span))
        {
            return Some(node);
        }
        current = node.parent();
    }
    None
}

pub(super) fn key_for_candidate(
    index: &NavigationIndex,
    candidate: &Candidate,
) -> Option<OverloadKey> {
    let symbol = index.symbol(candidate)?;
    (symbol.kind == SymbolKind::Routine)
        .then_some(symbol.routine_key.as_ref())
        .flatten()
        .map(|routine_key| OverloadKey {
            uri: candidate.uri.clone(),
            routine_key: routine_key.clone(),
        })
}

pub(super) fn candidate_in_group(
    index: &NavigationIndex,
    candidate: &Candidate,
    group: &OverloadKey,
) -> bool {
    key_for_candidate(index, candidate).is_some_and(|key| key == *group)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn select(
    index: &NavigationIndex,
    current_uri: &Url,
    current_document: &Document,
    call: Node<'_>,
    candidates: &[Candidate],
    receiver_substitution: &GenericSubstitution,
    owner_instances: &[TypeInstance],
    state: &mut ResolutionState,
    depth: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Selection, String> {
    let groups = routine_groups(index, candidates, cancel, budget)?;
    if groups.is_empty() {
        return Ok(Selection::default());
    }
    if groups.len() > MAX_OVERLOAD_GROUPS {
        return Err(format!(
            "overload selection exceeds the {MAX_OVERLOAD_GROUPS}-group limit"
        ));
    }

    let arguments = call_arguments(call, cancel, budget)?;
    if arguments.len() > MAX_OVERLOAD_ARGUMENTS {
        return Err(format!(
            "overload selection exceeds the {MAX_OVERLOAD_ARGUMENTS}-argument limit"
        ));
    }
    let mut argument_info = Vec::with_capacity(arguments.len());
    for argument in arguments {
        check_navigation_cancel(cancel)?;
        argument_info.push(infer_argument(
            index,
            current_uri,
            current_document,
            argument,
            receiver_substitution,
            state,
            depth,
            cancel,
            budget,
        )?);
    }

    let mut ancestry = AncestryResolutionState::new();
    let mut possible = Vec::new();
    for group in groups {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let Some(symbol) = index.symbol(&group.representative) else {
            continue;
        };
        if let Some(score) = score_group(
            index,
            &group.representative,
            symbol,
            &argument_info,
            call,
            current_uri,
            current_document,
            receiver_substitution,
            owner_instances,
            state,
            &mut ancestry,
            cancel,
            budget,
        )? {
            possible.push((group, score));
        }
    }

    if possible.is_empty() {
        return Ok(Selection {
            no_viable_group: true,
            ..Selection::default()
        });
    }
    if possible.iter().any(|(_, score)| score.uncertain) {
        return Ok(Selection::default());
    }
    if possible.len() == 1 {
        let (group, score) = possible.pop().expect("one possible overload");
        return Ok(Selection {
            selected_group: Some(group.key),
            generic_substitution: Some(score.substitution),
            no_viable_group: false,
        });
    }

    let best_cost = possible
        .iter()
        .map(|(_, score)| score.cost)
        .min()
        .expect("non-empty overload scores");
    let mut best = possible
        .into_iter()
        .filter(|(_, score)| score.cost == best_cost);
    let Some((group, score)) = best.next() else {
        return Ok(Selection::default());
    };
    if best.next().is_some() {
        return Ok(Selection::default());
    }
    Ok(Selection {
        selected_group: Some(group.key),
        generic_substitution: Some(score.substitution),
        no_viable_group: false,
    })
}

fn routine_groups(
    index: &NavigationIndex,
    candidates: &[Candidate],
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<RoutineGroup>, String> {
    let mut groups = HashMap::<OverloadKey, RoutineGroup>::new();
    for candidate in candidates {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let Some(symbol) = index.symbol(candidate) else {
            continue;
        };
        if symbol.kind != SymbolKind::Routine || symbol.unresolved_abbreviated {
            continue;
        }
        let Some(key) = key_for_candidate(index, candidate) else {
            continue;
        };
        let replace = groups.get(&key).is_none_or(|current| {
            candidate_rank(index, candidate) < candidate_rank(index, &current.representative)
        });
        if replace {
            groups.insert(
                key.clone(),
                RoutineGroup {
                    key,
                    representative: candidate.clone(),
                },
            );
        }
    }
    Ok(groups.into_values().collect())
}

fn candidate_rank(index: &NavigationIndex, candidate: &Candidate) -> (u8, u8, usize) {
    let Some(symbol) = index.symbol(candidate) else {
        return (u8::MAX, u8::MAX, usize::MAX);
    };
    let origin = match symbol.origin {
        Origin::Declaration => 0,
        Origin::Definition => 1,
    };
    let region = match symbol.region {
        Region::Interface => 0,
        Region::Implementation => 1,
        Region::Other => 2,
    };
    (origin, region, symbol.declaration_span.start)
}

fn call_arguments<'a>(
    call: Node<'a>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Vec<Node<'a>>, String> {
    let Some(arguments) = call.child_by_field_name("args") else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    budget.require_work(arguments.named_child_count(), cancel)?;
    for index in 0..arguments.named_child_count() {
        check_navigation_cancel(cancel)?;
        let Some(argument) = arguments.named_child(index) else {
            continue;
        };
        if argument.kind() != "legacyFormat" {
            result.push(argument);
        }
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn score_group(
    index: &NavigationIndex,
    candidate: &Candidate,
    symbol: &Symbol,
    arguments: &[ArgumentInfo],
    call: Node<'_>,
    current_uri: &Url,
    current_document: &Document,
    receiver_substitution: &GenericSubstitution,
    owner_instances: &[TypeInstance],
    state: &mut ResolutionState,
    ancestry: &mut AncestryResolutionState,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<GroupScore>, String> {
    let parameters = &symbol.routine_parameters;
    if arguments.len() > parameters.len()
        || parameters[arguments.len()..]
            .iter()
            .any(|parameter| !parameter.has_default)
    {
        return Ok(None);
    }

    let Some(substitution) = generic_substitution_for_group(
        index,
        candidate,
        symbol,
        call,
        arguments,
        current_uri,
        current_document,
        receiver_substitution,
        owner_instances,
        state,
        ancestry,
        cancel,
        budget,
    )?
    else {
        return Ok(None);
    };
    let mut cost: u32 = 0;
    let mut uncertain = false;
    for (argument, parameter) in arguments.iter().zip(parameters) {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if matches!(parameter.mode, ParameterMode::Var | ParameterMode::Out) && !argument.assignable
        {
            return Ok(None);
        }
        let expected = parameter_type(
            index,
            candidate,
            symbol,
            parameter,
            &substitution,
            state,
            cancel,
            budget,
        )?;
        if argument.nil_literal {
            match expected {
                Some(TypeIdentity::Named {
                    kind: TypeKind::Class | TypeKind::Interface,
                    ..
                }) => {
                    cost = cost.saturating_add(1);
                    continue;
                }
                Some(_) => return Ok(None),
                None => {
                    uncertain = true;
                    continue;
                }
            }
        }
        match (&argument.ty, expected) {
            (None, _) | (_, None) => uncertain = true,
            (Some(actual), Some(expected)) => {
                if matches!(parameter.mode, ParameterMode::Var | ParameterMode::Out) {
                    if !exact_type_match(actual, &expected) {
                        return Ok(None);
                    }
                } else {
                    match conversion(index, actual, &expected, ancestry, cancel, budget)? {
                        Conversion::Cost(value) => cost = cost.saturating_add(value),
                        Conversion::Unknown => uncertain = true,
                        Conversion::Incompatible => return Ok(None),
                    }
                }
            }
        }
    }
    Ok(Some(GroupScore {
        cost,
        uncertain,
        substitution,
    }))
}

#[allow(clippy::too_many_arguments)]
fn generic_substitution_for_group(
    index: &NavigationIndex,
    candidate: &Candidate,
    symbol: &Symbol,
    call: Node<'_>,
    arguments: &[ArgumentInfo],
    current_uri: &Url,
    current_document: &Document,
    receiver_substitution: &GenericSubstitution,
    owner_instances: &[TypeInstance],
    state: &mut ResolutionState,
    ancestry: &mut AncestryResolutionState,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<GenericSubstitution>, String> {
    let Some(owner_substitution) = index.routine_owner_substitution_with_budget(
        candidate,
        receiver_substitution,
        owner_instances,
        state,
        cancel,
        budget,
    )?
    else {
        return Ok(None);
    };
    if symbol.generic_parameters.is_empty() {
        return Ok(Some(owner_substitution));
    }

    let mut substitution = owner_substitution;
    for parameter in &symbol.generic_parameters {
        // Routine parameters shadow parameters from an enclosing generic
        // type.  Do not let the owner's substitution silently satisfy a
        // routine parameter that still needs explicit arguments or
        // argument-based inference.
        substitution.remove(&parameter.name);
    }
    if let Some(explicit_arguments) = generic_call_arguments(call) {
        if explicit_arguments.len() != symbol.generic_parameters.len() {
            return Ok(None);
        }
        for (parameter, argument) in symbol.generic_parameters.iter().zip(explicit_arguments) {
            let Some(type_ref) = super::type_ref_from_node(argument, &current_document.source)
            else {
                return Ok(None);
            };
            let receivers = index.type_receivers_for_type_ref_with_budget(
                current_uri,
                current_document,
                type_ref.span.start,
                &type_ref,
                argument,
                None,
                &substitution,
                state,
                cancel,
                budget,
            )?;
            let Some(resolved) = super::resolved_type_from_receivers(receivers) else {
                return Ok(None);
            };
            substitution.insert(&parameter.name, resolved);
        }
    } else {
        let generic_names = symbol
            .generic_parameters
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect::<HashSet<_>>();
        for (argument, parameter) in arguments.iter().zip(&symbol.routine_parameters) {
            let (Some(actual), Some(type_ref)) = (&argument.ty, parameter.type_ref.as_ref()) else {
                continue;
            };
            if !infer_generic_type_ref(index, type_ref, actual, &generic_names, &mut substitution) {
                return Ok(None);
            }
        }
    }

    if symbol
        .generic_parameters
        .iter()
        .any(|parameter| substitution.get(&parameter.name).is_none())
    {
        return Ok(None);
    }

    if !generic_constraints_satisfied(
        index,
        candidate,
        symbol,
        &substitution,
        state,
        ancestry,
        cancel,
        budget,
    )? {
        return Ok(None);
    }
    Ok(Some(substitution))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn generic_constraints_satisfied(
    index: &NavigationIndex,
    candidate: &Candidate,
    symbol: &Symbol,
    substitution: &GenericSubstitution,
    state: &mut ResolutionState,
    ancestry: &mut AncestryResolutionState,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let Some(document) = index.documents.get(&candidate.uri) else {
        return Ok(false);
    };
    for parameter in &symbol.generic_parameters {
        if parameter.constraint_unsupported {
            return Ok(false);
        }
        let Some(constraint) = parameter.constraint.as_ref() else {
            continue;
        };
        let Some(actual) = substitution
            .get(&parameter.name)
            .and_then(super::type_identity_from_resolved_type)
        else {
            return Ok(false);
        };
        if constraint.path.len() == 1 {
            match canonical_name(&constraint.path[0]).as_str() {
                "class" => {
                    if !matches!(
                        actual,
                        TypeIdentity::Named {
                            kind: TypeKind::Class,
                            ..
                        }
                    ) {
                        return Ok(false);
                    }
                    continue;
                }
                "interface" => {
                    if !matches!(
                        actual,
                        TypeIdentity::Named {
                            kind: TypeKind::Interface,
                            ..
                        }
                    ) {
                        return Ok(false);
                    }
                    continue;
                }
                "record" => {
                    if !matches!(
                        actual,
                        TypeIdentity::Named {
                            kind: TypeKind::Record,
                            ..
                        }
                    ) {
                        return Ok(false);
                    }
                    continue;
                }
                "constructor" => {
                    if !has_proven_constructor(index, &actual) {
                        return Ok(false);
                    }
                    continue;
                }
                _ => {}
            }
        }
        let Some(lookup_identifier) = assistance::identifier_at_with_budget(
            document.tree.root_node(),
            constraint.span.start,
            cancel,
            budget,
            "generic constraint",
        )?
        else {
            return Ok(false);
        };
        let receivers = index.type_receivers_for_type_ref_with_budget(
            &candidate.uri,
            document,
            constraint.span.start,
            constraint,
            lookup_identifier,
            Some(symbol.scope),
            substitution,
            state,
            cancel,
            budget,
        )?;
        let Some(expected) = receiver_type(index, receivers)? else {
            return Ok(false);
        };
        if !matches!(
            conversion(index, &actual, &expected, ancestry, cancel, budget)?,
            Conversion::Cost(_)
        ) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn has_proven_constructor(index: &NavigationIndex, identity: &TypeIdentity) -> bool {
    let TypeIdentity::Named { uri, key, .. } = identity else {
        return false;
    };
    let Some(document) = index.documents.get(uri) else {
        return false;
    };
    let Some(indices) = document.member_symbol_indices_by_owner.get(key) else {
        return false;
    };
    indices.iter().any(|index| {
        document.symbols.get(*index).is_some_and(|symbol| {
            symbol.kind == SymbolKind::Routine
                && symbol.routine_kind == super::RoutineKind::Constructor
                && symbol
                    .routine_parameters
                    .iter()
                    .all(|parameter| parameter.has_default)
        })
    })
}

fn generic_call_arguments(call: Node<'_>) -> Option<Vec<Node<'_>>> {
    let entity = call.child_by_field_name("entity")?;
    if entity.kind() != "exprTpl" {
        return None;
    }
    let arguments = entity.child_by_field_name("args")?;
    if matches!(arguments.kind(), "genericArgs" | "typerefArgs" | "exprArgs") {
        Some(
            (0..arguments.named_child_count())
                .filter_map(|index| arguments.named_child(index))
                .collect(),
        )
    } else {
        Some(
            (0..entity.named_child_count())
                .filter_map(|index| entity.named_child(index))
                .filter(|argument| {
                    argument.start_byte() >= arguments.start_byte()
                        && !matches!(argument.kind(), "kLt" | "kGt")
                })
                .collect(),
        )
    }
}

fn infer_generic_type_ref(
    index: &NavigationIndex,
    type_ref: &TypeRef,
    actual: &TypeIdentity,
    generic_names: &HashSet<&str>,
    substitution: &mut GenericSubstitution,
) -> bool {
    if type_ref.path.len() == 1 && type_ref.args.is_empty() {
        let name = type_ref.path[0].as_str();
        if !generic_names.contains(name) {
            return true;
        }
        let Some(resolved) = resolved_type_from_identity(index, actual) else {
            return false;
        };
        if let Some(existing) = substitution.get(name) {
            existing == &resolved
        } else {
            substitution.insert(name, resolved);
            true
        }
    } else if !type_ref.args.is_empty() {
        let TypeIdentity::Named { args, .. } = actual else {
            return true;
        };
        type_ref.args.len() == args.len()
            && type_ref.args.iter().zip(args).all(|(expected, actual)| {
                infer_generic_type_ref(index, expected, actual, generic_names, substitution)
            })
    } else {
        true
    }
}

fn resolved_type_from_identity(
    index: &NavigationIndex,
    identity: &TypeIdentity,
) -> Option<ResolvedType> {
    match identity {
        TypeIdentity::Builtin(builtin) => Some(ResolvedType::Builtin(*builtin)),
        TypeIdentity::IntegerLiteral(value) => Some(ResolvedType::IntegerLiteral(*value)),
        TypeIdentity::Named {
            uri,
            key,
            kind,
            args,
        } => {
            let document = index.documents.get(uri)?;
            let indices = document.type_symbol_indices.get(key)?;
            if indices.len() != 1 {
                return None;
            }
            let symbol = document.symbols.get(*indices.first()?)?;
            if symbol.kind != SymbolKind::Type || symbol.generic_parameters.len() != args.len() {
                return None;
            }
            let parameter_names = symbol
                .generic_parameters
                .iter()
                .map(|parameter| parameter.name.clone())
                .collect::<Vec<_>>();
            let mut substitution = GenericSubstitution::empty();
            for (name, argument) in parameter_names.iter().zip(args) {
                substitution.insert(name, resolved_type_from_identity(index, argument)?);
            }
            Some(ResolvedType::Named(TypeInstance {
                uri: uri.clone(),
                key: key.clone(),
                kind: *kind,
                scope: symbol.scope,
                parameter_names,
                substitution,
            }))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn parameter_type(
    index: &NavigationIndex,
    candidate: &Candidate,
    symbol: &Symbol,
    parameter: &RoutineParameter,
    receiver_substitution: &GenericSubstitution,
    state: &mut ResolutionState,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<TypeIdentity>, String> {
    if parameter.type_ref.is_none() && parameter.type_name.is_none() {
        return Ok(None);
    }
    let Some(document) = index.documents.get(&candidate.uri) else {
        return Ok(None);
    };
    let offset = parameter
        .type_span
        .map_or(symbol.span.start, |span| span.start);
    let Some(lookup_identifier) = assistance::identifier_at_with_budget(
        document.tree.root_node(),
        offset,
        cancel,
        budget,
        "overload selection",
    )?
    else {
        return Ok(parameter
            .type_ref
            .as_ref()
            .map(TypeRef::display)
            .or_else(|| parameter.type_name.clone())
            .and_then(|name| builtin_type(&name).map(TypeIdentity::Builtin)));
    };
    let receivers = if let Some(type_ref) = parameter.type_ref.as_ref() {
        index.type_receivers_for_type_ref_with_budget(
            &candidate.uri,
            document,
            type_ref.span.start,
            type_ref,
            lookup_identifier,
            Some(symbol.scope),
            receiver_substitution,
            state,
            cancel,
            budget,
        )?
    } else {
        index.type_receivers_for_path_with_budget_at_scope(
            &candidate.uri,
            document,
            offset,
            parameter.type_name.as_deref().unwrap_or_default(),
            lookup_identifier,
            Some(symbol.scope),
            state,
            cancel,
            budget,
        )?
    };
    receiver_type(index, receivers)
}

#[allow(clippy::too_many_arguments)]
fn infer_argument(
    index: &NavigationIndex,
    current_uri: &Url,
    current_document: &Document,
    node: Node<'_>,
    receiver_substitution: &GenericSubstitution,
    state: &mut ResolutionState,
    depth: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<ArgumentInfo, String> {
    let mut node = node;
    let mut force_non_assignable = false;
    loop {
        check_navigation_cancel(cancel)?;
        if node.kind() == "exprParens" {
            let Some(operand) = node.named_child(0) else {
                return Ok(unknown_argument());
            };
            budget.require_work(1, cancel)?;
            node = operand;
            continue;
        }
        if node.kind() == "exprUnary" {
            let Some(operand) = node.named_child(0) else {
                return Ok(unknown_argument());
            };
            budget.require_work(1, cancel)?;
            force_non_assignable = true;
            node = operand;
            continue;
        }
        break;
    }
    let kind = node.kind();
    if matches!(kind, "literalNumber") {
        let text = node_text(index, current_document, node, cancel, budget)?;
        let ty = infer_numeric_literal(&text);
        return Ok(ArgumentInfo {
            ty: Some(ty),
            assignable: false,
            nil_literal: false,
        });
    }
    if matches!(kind, "literalString" | "literalChar") {
        let text = node_text(index, current_document, node, cancel, budget)?;
        let is_character = if kind == "literalChar" {
            true
        } else {
            let Some(is_character) = classify_literal_fragments(&text) else {
                return Ok(unknown_argument());
            };
            is_character
        };
        return Ok(ArgumentInfo {
            ty: Some(TypeIdentity::Builtin(if is_character {
                BuiltinType::Character
            } else {
                BuiltinType::String
            })),
            assignable: false,
            nil_literal: false,
        });
    }
    if kind == "kTrue" || kind == "kFalse" {
        return Ok(ArgumentInfo {
            ty: Some(TypeIdentity::Builtin(BuiltinType::Boolean)),
            assignable: false,
            nil_literal: false,
        });
    }
    if kind == "kNil" {
        return Ok(ArgumentInfo {
            ty: None,
            assignable: false,
            nil_literal: true,
        });
    }
    if kind == "exprCall" || kind == "exprAs" {
        let receivers = index.resolve_receivers_with_state_and_budget(
            current_uri,
            current_document,
            node.start_byte(),
            node,
            node,
            state,
            cancel,
            budget,
            depth.saturating_add(1),
        )?;
        return Ok(ArgumentInfo {
            ty: receiver_type(index, receivers)?,
            assignable: false,
            nil_literal: false,
        });
    }

    let Some(identifier) = expression_identifier(node) else {
        return Ok(ArgumentInfo {
            ty: None,
            assignable: false,
            nil_literal: false,
        });
    };
    let candidates = index.resolve_candidates_at_with_state_and_budget(
        current_uri,
        current_document,
        identifier.start_byte(),
        identifier,
        state,
        depth.saturating_add(1),
        cancel,
        budget,
    )?;
    let assignable = !force_non_assignable
        && (candidates
            .iter()
            .any(|candidate| index.symbol(candidate).is_some_and(is_assignable_symbol))
            || node_text(index, current_document, identifier, cancel, budget)?
                .eq_ignore_ascii_case("Result"));

    if candidates
        .iter()
        .any(|candidate| index.candidate_is_conditionally_unknown(candidate))
    {
        return Ok(ArgumentInfo {
            ty: None,
            assignable,
            nil_literal: false,
        });
    }

    let mut identities = HashSet::new();
    let mut unknown = false;
    for candidate in candidates {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let Some(symbol) = index.symbol(&candidate) else {
            unknown = true;
            continue;
        };
        if matches!(
            symbol.kind,
            SymbolKind::Variable | SymbolKind::Parameter | SymbolKind::Field | SymbolKind::Property
        ) {
            match symbol_type(
                index,
                &candidate,
                symbol,
                receiver_substitution,
                state,
                cancel,
                budget,
            )? {
                Some(identity) => {
                    identities.insert(identity);
                }
                None => unknown = true,
            }
        } else {
            unknown = true;
        }
    }
    Ok(ArgumentInfo {
        ty: if !unknown && identities.len() == 1 {
            identities.into_iter().next()
        } else {
            None
        },
        assignable,
        nil_literal: false,
    })
}

fn unknown_argument() -> ArgumentInfo {
    ArgumentInfo {
        ty: None,
        assignable: false,
        nil_literal: false,
    }
}

fn is_assignable_symbol(symbol: &Symbol) -> bool {
    match symbol.kind {
        SymbolKind::Variable | SymbolKind::Field => true,
        SymbolKind::Parameter => matches!(
            symbol.parameter_mode,
            Some(ParameterMode::Value | ParameterMode::Var | ParameterMode::Out)
        ),
        // Properties are deliberately treated as non-assignable.  Their
        // accessor metadata cannot prove that the property has a writable
        // setter, and passing a read-only property to var/out must never
        // select the mutating overload.
        SymbolKind::Property => false,
        _ => false,
    }
}

fn exact_type_match(actual: &TypeIdentity, expected: &TypeIdentity) -> bool {
    actual == expected
}

fn infer_numeric_literal(text: &str) -> TypeIdentity {
    let text = text.trim();
    if is_radix_integer(text) {
        return parse_integer_literal(text)
            .map(TypeIdentity::IntegerLiteral)
            .unwrap_or(TypeIdentity::Builtin(BuiltinType::Integer(
                IntegerKind::Literal,
            )));
    }
    if text.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
        return TypeIdentity::Builtin(BuiltinType::Real);
    }
    parse_integer_literal(text)
        .map(TypeIdentity::IntegerLiteral)
        .unwrap_or(TypeIdentity::Builtin(BuiltinType::Integer(
            IntegerKind::Literal,
        )))
}

fn classify_literal_fragments(text: &str) -> Option<bool> {
    let mut remaining = text.trim();
    let mut fragments = 0usize;
    let mut only_codepoint = false;
    while !remaining.is_empty() {
        remaining = remaining.trim_start();
        if let Some(after_hash) = remaining.strip_prefix('#') {
            let digits = after_hash.strip_prefix('$').unwrap_or(after_hash);
            let length = digits
                .as_bytes()
                .iter()
                .take_while(|byte| {
                    if after_hash.starts_with('$') {
                        byte.is_ascii_hexdigit()
                    } else {
                        byte.is_ascii_digit()
                    }
                })
                .count();
            if length == 0 {
                return None;
            }
            remaining = &digits[length..];
            fragments += 1;
            only_codepoint = true;
            continue;
        }
        if remaining.starts_with('\'') {
            let bytes = remaining.as_bytes();
            let mut index = 1;
            while index < bytes.len() {
                if bytes[index] != b'\'' {
                    index += 1;
                    continue;
                }
                if bytes.get(index + 1) == Some(&b'\'') {
                    index += 2;
                    continue;
                }
                remaining = &remaining[index + 1..];
                fragments += 1;
                only_codepoint = false;
                break;
            }
            if index >= bytes.len() {
                return None;
            }
            continue;
        }
        return None;
    }
    Some(fragments == 1 && only_codepoint)
}

fn is_radix_integer(text: &str) -> bool {
    let text = text
        .strip_prefix('+')
        .or_else(|| text.strip_prefix('-'))
        .unwrap_or(text);
    text.starts_with('$') || text.starts_with('%')
}

fn parse_integer_literal(text: &str) -> Option<i128> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (radix, digits) = if let Some(rest) = digits.strip_prefix('$') {
        (16, rest)
    } else if let Some(rest) = digits.strip_prefix('%') {
        (2, rest)
    } else {
        (10, digits)
    };
    if digits.is_empty() {
        return None;
    }
    let value = u128::from_str_radix(digits, radix).ok()?;
    if negative {
        (value <= (i128::MAX as u128).saturating_add(1)).then(|| {
            if value == (i128::MAX as u128).saturating_add(1) {
                i128::MIN
            } else {
                -(value as i128)
            }
        })
    } else {
        i128::try_from(value).ok()
    }
}

fn expression_identifier(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() == "identifier" {
        return Some(node);
    }
    if matches!(node.kind(), "exprDot" | "genericDot" | "typerefDot") {
        return node
            .child_by_field_name("rhs")
            .filter(|rhs| rhs.kind() == "identifier");
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn symbol_type(
    index: &NavigationIndex,
    candidate: &Candidate,
    symbol: &Symbol,
    receiver_substitution: &GenericSubstitution,
    state: &mut ResolutionState,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<TypeIdentity>, String> {
    if symbol.type_ref.is_none() && symbol.type_name.is_none() {
        return Ok(None);
    }
    let Some(document) = index.documents.get(&candidate.uri) else {
        return Ok(None);
    };
    let Some(lookup_identifier) = assistance::identifier_at_with_budget(
        document.tree.root_node(),
        symbol.span.start,
        cancel,
        budget,
        "overload selection",
    )?
    else {
        return Ok(symbol
            .type_ref
            .as_ref()
            .map(TypeRef::display)
            .or_else(|| symbol.type_name.clone())
            .and_then(|name| builtin_type(&name).map(TypeIdentity::Builtin)));
    };
    let receivers = index.type_receivers_for_symbol_type_with_budget(
        &candidate.uri,
        document,
        symbol,
        lookup_identifier,
        Some(symbol.scope),
        receiver_substitution,
        state,
        cancel,
        budget,
    )?;
    receiver_type(index, receivers)
}

fn receiver_type(
    index: &NavigationIndex,
    receivers: Vec<Receiver>,
) -> Result<Option<TypeIdentity>, String> {
    let mut result = None;
    for receiver in receivers {
        let identity = match receiver {
            Receiver::Builtin(builtin) => TypeIdentity::Builtin(builtin),
            Receiver::IntegerLiteral(value) => TypeIdentity::IntegerLiteral(value),
            Receiver::Type(instance) => {
                let Some(kind) = index.type_kind(&instance.uri, &instance.key) else {
                    return Ok(None);
                };
                TypeIdentity::Named {
                    uri: instance.uri,
                    key: instance.key,
                    kind,
                    args: instance
                        .parameter_names
                        .iter()
                        .filter_map(|name| instance.substitution.get(name))
                        .filter_map(super::type_identity_from_resolved_type)
                        .collect(),
                }
            }
            Receiver::Unit(_) => return Ok(None),
        };
        if result.as_ref().is_some_and(|current| current != &identity) {
            return Ok(None);
        }
        result = Some(identity);
    }
    Ok(result)
}

pub(super) fn builtin_type(name: &str) -> Option<BuiltinType> {
    let parts = name
        .split('.')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let first = parts.first()?;
    let name = parts.last()?;
    if parts.len() > 1 && !first.eq_ignore_ascii_case("System") {
        return None;
    }
    let name = canonical_name(name);
    match name.as_str() {
        "integer" => Some(BuiltinType::Integer(IntegerKind::Integer)),
        "shortint" => Some(BuiltinType::Integer(IntegerKind::ShortInt)),
        "smallint" => Some(BuiltinType::Integer(IntegerKind::SmallInt)),
        "byte" => Some(BuiltinType::Integer(IntegerKind::Byte)),
        "word" => Some(BuiltinType::Integer(IntegerKind::Word)),
        "longint" => Some(BuiltinType::Integer(IntegerKind::Integer)),
        "int64" => Some(BuiltinType::Integer(IntegerKind::Int64)),
        "cardinal" => Some(BuiltinType::Integer(IntegerKind::Cardinal)),
        "longword" => Some(BuiltinType::Integer(IntegerKind::LongWord)),
        "nativeint" => Some(BuiltinType::Integer(IntegerKind::NativeInt)),
        "uint64" => Some(BuiltinType::Integer(IntegerKind::UInt64)),
        "nativeuint" => Some(BuiltinType::Integer(IntegerKind::NativeUInt)),
        "real" | "real48" | "single" | "double" | "extended" | "currency" | "comp" => {
            Some(BuiltinType::Real)
        }
        "string" | "ansistring" | "unicodestring" | "widestring" | "shortstring" => {
            Some(BuiltinType::String)
        }
        "char" | "ansichar" | "widechar" => Some(BuiltinType::Character),
        "boolean" | "bytebool" | "wordbool" | "longbool" => Some(BuiltinType::Boolean),
        _ => None,
    }
}

fn integer_literal_conversion(value: i128, expected: IntegerKind) -> Conversion {
    integer_range(expected).map_or(Conversion::Unknown, |(minimum, maximum)| {
        if (minimum..=maximum).contains(&value) {
            Conversion::Cost(0)
        } else {
            Conversion::Incompatible
        }
    })
}

fn integer_conversion(actual: IntegerKind, expected: IntegerKind) -> Conversion {
    if actual == expected {
        return Conversion::Cost(0);
    }
    match (integer_range(actual), integer_range(expected)) {
        (Some((actual_minimum, actual_maximum)), Some((expected_minimum, expected_maximum)))
            if expected_minimum <= actual_minimum && actual_maximum <= expected_maximum =>
        {
            Conversion::Cost(1)
        }
        (Some(_), Some(_)) => Conversion::Incompatible,
        _ => Conversion::Unknown,
    }
}

fn integer_range(kind: IntegerKind) -> Option<(i128, i128)> {
    match kind {
        IntegerKind::Literal => None,
        IntegerKind::ShortInt => Some((i8::MIN as i128, i8::MAX as i128)),
        IntegerKind::SmallInt => Some((i16::MIN as i128, i16::MAX as i128)),
        IntegerKind::Integer => Some((i32::MIN as i128, i32::MAX as i128)),
        IntegerKind::Byte => Some((u8::MIN as i128, u8::MAX as i128)),
        IntegerKind::Word => Some((u16::MIN as i128, u16::MAX as i128)),
        IntegerKind::Cardinal | IntegerKind::LongWord => Some((u32::MIN as i128, u32::MAX as i128)),
        IntegerKind::Int64 | IntegerKind::NativeInt => Some((i64::MIN as i128, i64::MAX as i128)),
        IntegerKind::UInt64 | IntegerKind::NativeUInt => Some((0, u64::MAX as i128)),
    }
}

#[allow(clippy::too_many_arguments)]
fn conversion(
    index: &NavigationIndex,
    actual: &TypeIdentity,
    expected: &TypeIdentity,
    ancestry: &mut AncestryResolutionState,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Conversion, String> {
    match (actual, expected) {
        (TypeIdentity::IntegerLiteral(value), TypeIdentity::Builtin(expected)) => match expected {
            BuiltinType::Integer(kind) => Ok(integer_literal_conversion(*value, *kind)),
            BuiltinType::Real => Ok(Conversion::Cost(1)),
            _ => Ok(Conversion::Incompatible),
        },
        (TypeIdentity::Builtin(actual), TypeIdentity::Builtin(expected)) => {
            let result = match (actual, expected) {
                (left, right) if left == right => Conversion::Cost(0),
                (BuiltinType::Integer(_), BuiltinType::Real) => Conversion::Cost(1),
                (BuiltinType::Character, BuiltinType::String) => Conversion::Cost(1),
                (BuiltinType::Integer(actual), BuiltinType::Integer(expected)) => {
                    integer_conversion(*actual, *expected)
                }
                _ => Conversion::Incompatible,
            };
            Ok(result)
        }
        (
            TypeIdentity::Named {
                uri: actual_uri,
                key: actual_key,
                args: actual_args,
                ..
            },
            TypeIdentity::Named {
                uri: expected_uri,
                key: expected_key,
                args: expected_args,
                ..
            },
        ) if actual_uri == expected_uri && actual_key == expected_key => {
            if actual_args == expected_args {
                Ok(Conversion::Cost(0))
            } else {
                Ok(Conversion::Incompatible)
            }
        }
        (
            TypeIdentity::Named {
                uri: actual_uri,
                key: actual_key,
                kind: actual_kind,
                ..
            },
            TypeIdentity::Named {
                uri: expected_uri,
                key: expected_key,
                kind: expected_kind,
                ..
            },
        ) => match upcast_distance(
            index,
            actual_uri,
            actual_key,
            *actual_kind,
            expected_uri,
            expected_key,
            *expected_kind,
            ancestry,
            cancel,
            budget,
        )? {
            Upcast::Distance(distance) => Ok(Conversion::Cost(distance)),
            Upcast::No => Ok(Conversion::Incompatible),
            Upcast::Unknown => Ok(Conversion::Unknown),
        },
        (TypeIdentity::Builtin(_), TypeIdentity::Named { kind, .. })
        | (TypeIdentity::Named { kind, .. }, TypeIdentity::Builtin(_))
            if matches!(kind, TypeKind::Other | TypeKind::String) =>
        {
            Ok(Conversion::Unknown)
        }
        (TypeIdentity::Builtin(_), TypeIdentity::Named { .. })
        | (TypeIdentity::Named { .. }, TypeIdentity::Builtin(_)) => Ok(Conversion::Incompatible),
        (TypeIdentity::IntegerLiteral(actual), TypeIdentity::IntegerLiteral(expected))
            if actual == expected =>
        {
            Ok(Conversion::Cost(0))
        }
        (TypeIdentity::IntegerLiteral(_), TypeIdentity::IntegerLiteral(_)) => {
            Ok(Conversion::Unknown)
        }
        (
            TypeIdentity::IntegerLiteral(_),
            TypeIdentity::Named {
                kind: TypeKind::Other | TypeKind::String,
                ..
            },
        ) => Ok(Conversion::Unknown),
        (TypeIdentity::IntegerLiteral(_), TypeIdentity::Named { .. })
        | (TypeIdentity::Named { .. }, TypeIdentity::IntegerLiteral(_))
        | (TypeIdentity::Builtin(_), TypeIdentity::IntegerLiteral(_)) => {
            Ok(Conversion::Incompatible)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn upcast_distance(
    index: &NavigationIndex,
    actual_uri: &Url,
    actual_key: &str,
    actual_kind: TypeKind,
    expected_uri: &Url,
    expected_key: &str,
    expected_kind: TypeKind,
    ancestry: &mut AncestryResolutionState,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Upcast, String> {
    if expected_kind == TypeKind::Interface && actual_kind == TypeKind::Class {
        return Ok(Upcast::Unknown);
    }
    if !matches!(
        (actual_kind, expected_kind),
        (TypeKind::Class, TypeKind::Class) | (TypeKind::Interface, TypeKind::Interface)
    ) {
        return Ok(Upcast::No);
    }

    let target = (expected_uri.clone(), expected_key.to_owned());
    let mut queue = VecDeque::from([(actual_uri.clone(), actual_key.to_owned(), 0u32)]);
    let mut visited = HashSet::new();
    while let Some((uri, key, distance)) = queue.pop_front() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if !visited.insert((uri.clone(), key.clone())) {
            continue;
        }
        let ancestry_result =
            index.resolve_type_ancestry_with_budget(&uri, &key, ancestry, cancel, budget)?;
        if ancestry_result.status == AncestryStatus::Unknown {
            return Ok(Upcast::Unknown);
        }
        for (parent_uri, parent_key) in ancestry_result.parents {
            let next_distance = distance.saturating_add(1);
            if (parent_uri.clone(), parent_key.clone()) == target {
                return Ok(Upcast::Distance(next_distance));
            }
            queue.push_back((parent_uri, parent_key, next_distance));
        }
    }
    Ok(Upcast::No)
}

fn node_text(
    _index: &NavigationIndex,
    document: &Document,
    node: Node<'_>,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<String, String> {
    let span = Span::from_node(node);
    let text = document
        .source
        .get(span.start..span.end)
        .unwrap_or_default();
    budget.require_bytes(text.len(), cancel)?;
    Ok(text.to_owned())
}
