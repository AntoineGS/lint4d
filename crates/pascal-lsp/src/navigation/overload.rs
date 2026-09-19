use super::{
    AncestryResolutionState, AncestryStatus, AssistanceBudget, BuiltinType, Candidate, Document,
    GenericSubstitution, IntegerKind, NavigationIndex, Origin, ParameterMode, Receiver, Region,
    ResolutionState, ResolvedType, RoutineKind, RoutineParameter, Span, Symbol, SymbolKind,
    TypeIdentity, TypeInstance, TypeKind, TypeRef, TypeShape, assistance, canonical_name,
    check_navigation_cancel,
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
    span: Span,
    ty: Option<TypeIdentity>,
    writability: Writability,
    nil_literal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Writability {
    Writable,
    NonWritable,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiteralClassification {
    Character,
    String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compatibility {
    Compatible,
    Unknown,
    Incompatible,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ArgumentTypeMismatch {
    pub(super) span: Span,
    pub(super) expected: String,
    pub(super) actual: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CallAnalysis {
    Compatible,
    Incompatible(Vec<ArgumentTypeMismatch>),
    Ambiguous,
    Unsupported,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AssignmentAnalysis {
    Compatible,
    Incompatible { expected: String, actual: String },
    Unsupported,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConstraintOutcome {
    Proven,
    Unknown,
    Contradictory,
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
    Ok(
        match score_group_detailed(
            index,
            candidate,
            symbol,
            arguments,
            call,
            current_uri,
            current_document,
            receiver_substitution,
            owner_instances,
            state,
            ancestry,
            cancel,
            budget,
        )? {
            GroupOutcome::Compatible(score) => Some(score),
            GroupOutcome::Incompatible(_) | GroupOutcome::Incomplete => None,
        },
    )
}

enum GroupOutcome {
    Compatible(GroupScore),
    Incompatible(Vec<ArgumentTypeMismatch>),
    Incomplete,
}

#[allow(clippy::too_many_arguments)]
fn score_group_detailed(
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
) -> Result<GroupOutcome, String> {
    let parameters = &symbol.routine_parameters;
    if arguments.len() > parameters.len()
        || parameters[arguments.len()..]
            .iter()
            .any(|parameter| !parameter.has_default)
    {
        let mismatches = arguments
            .iter()
            .skip(parameters.len())
            .map(|argument| ArgumentTypeMismatch {
                span: argument_span(argument),
                expected: "no parameter".to_owned(),
                actual: argument_actual_label(argument),
            })
            .collect();
        return Ok(GroupOutcome::Incompatible(mismatches));
    }

    let Some((substitution, generic_uncertain)) = generic_substitution_for_group(
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
        return Ok(GroupOutcome::Incomplete);
    };
    let mut cost: u32 = 0;
    let mut uncertain = generic_uncertain;
    let mut mismatches = Vec::new();
    for (argument, parameter) in arguments.iter().zip(parameters) {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
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
        if matches!(parameter.mode, ParameterMode::Var | ParameterMode::Out) {
            match argument.writability {
                Writability::Writable => {}
                Writability::Unknown => return Ok(GroupOutcome::Incomplete),
                Writability::NonWritable => {
                    if uncertain {
                        return Ok(GroupOutcome::Incomplete);
                    }
                    let Some(expected) = expected.as_ref() else {
                        return Ok(GroupOutcome::Incomplete);
                    };
                    mismatches.push(ArgumentTypeMismatch {
                        span: argument_span(argument),
                        expected: type_identity_label(expected),
                        actual: "non-writable expression".to_owned(),
                    });
                    continue;
                }
            }
        }
        if argument.nil_literal {
            let Some(expected) = expected.as_ref() else {
                uncertain = true;
                continue;
            };
            match nil_compatibility(index, expected, cancel, budget)? {
                Compatibility::Compatible => cost = cost.saturating_add(1),
                Compatibility::Unknown => uncertain = true,
                Compatibility::Incompatible => {
                    if uncertain {
                        return Ok(GroupOutcome::Incomplete);
                    }
                    mismatches.push(ArgumentTypeMismatch {
                        span: argument_span(argument),
                        expected: type_identity_label(expected),
                        actual: "nil".to_owned(),
                    });
                }
            }
            continue;
        }
        match (&argument.ty, expected.as_ref()) {
            (None, _) | (_, None) => uncertain = true,
            (Some(actual), Some(expected)) => {
                if matches!(parameter.mode, ParameterMode::Var | ParameterMode::Out) {
                    match byref_type_match(actual, expected) {
                        Compatibility::Compatible => {}
                        Compatibility::Unknown => uncertain = true,
                        Compatibility::Incompatible => {
                            if uncertain {
                                return Ok(GroupOutcome::Incomplete);
                            }
                            mismatches.push(ArgumentTypeMismatch {
                                span: argument_span(argument),
                                expected: type_identity_label(expected),
                                actual: type_identity_label(actual),
                            });
                        }
                    }
                } else {
                    match overload_conversion(
                        index, actual, expected, state, ancestry, cancel, budget,
                    )? {
                        Conversion::Cost(value) => cost = cost.saturating_add(value),
                        Conversion::Unknown => uncertain = true,
                        Conversion::Incompatible => {
                            match assignment_conversion(
                                index, actual, expected, state, ancestry, cancel, budget,
                            )? {
                                Compatibility::Compatible => cost = cost.saturating_add(1),
                                Compatibility::Unknown => uncertain = true,
                                Compatibility::Incompatible => {
                                    if uncertain {
                                        return Ok(GroupOutcome::Incomplete);
                                    }
                                    mismatches.push(ArgumentTypeMismatch {
                                        span: argument_span(argument),
                                        expected: type_identity_label(expected),
                                        actual: type_identity_label(actual),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if uncertain {
        Ok(GroupOutcome::Compatible(GroupScore {
            cost,
            uncertain,
            substitution,
        }))
    } else if mismatches.is_empty() {
        Ok(GroupOutcome::Compatible(GroupScore {
            cost,
            uncertain: false,
            substitution,
        }))
    } else {
        Ok(GroupOutcome::Incompatible(mismatches))
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn analyze_call(
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
) -> Result<CallAnalysis, String> {
    let groups = routine_groups(index, candidates, cancel, budget)?;
    if groups.is_empty() {
        return Ok(CallAnalysis::Unsupported);
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
    for argument in &arguments {
        check_navigation_cancel(cancel)?;
        argument_info.push(infer_argument(
            index,
            current_uri,
            current_document,
            *argument,
            state,
            depth,
            cancel,
            budget,
        )?);
    }

    let mut ancestry = AncestryResolutionState::new();
    let mut possible = Vec::new();
    let mut incompatible = Vec::new();
    let mut incomplete = false;
    for group in groups {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let Some(symbol) = index.symbol(&group.representative) else {
            incomplete = true;
            continue;
        };
        match score_group_detailed(
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
            GroupOutcome::Compatible(score) => possible.push((group, score)),
            GroupOutcome::Incompatible(mismatches) => incompatible.push(mismatches),
            GroupOutcome::Incomplete => incomplete = true,
        }
    }

    if incomplete || possible.iter().any(|(_, score)| score.uncertain) {
        return Ok(CallAnalysis::Incomplete);
    }
    if !possible.is_empty() {
        let best_cost = possible
            .iter()
            .map(|(_, score)| score.cost)
            .min()
            .expect("non-empty overload scores");
        let best_count = possible
            .iter()
            .filter(|(_, score)| score.cost == best_cost)
            .count();
        return Ok(if best_count == 1 {
            CallAnalysis::Compatible
        } else {
            CallAnalysis::Ambiguous
        });
    }
    let Some(first_mismatches) = incompatible.first() else {
        return Ok(CallAnalysis::Incomplete);
    };
    let mut common = Vec::new();
    for mismatch in first_mismatches {
        let matching = incompatible.iter().filter_map(|mismatches| {
            mismatches.iter().find(|candidate| {
                candidate.span == mismatch.span && candidate.actual == mismatch.actual
            })
        });
        let matching = matching.collect::<Vec<_>>();
        if matching.len() != incompatible.len() {
            continue;
        }
        let expected = if matching
            .iter()
            .all(|candidate| candidate.expected == mismatch.expected)
        {
            mismatch.expected.clone()
        } else {
            "a compatible overload parameter".to_owned()
        };
        common.push(ArgumentTypeMismatch {
            span: mismatch.span,
            expected,
            actual: mismatch.actual.clone(),
        });
    }
    Ok(CallAnalysis::Incompatible(common))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn analyze_assignment(
    index: &NavigationIndex,
    current_uri: &Url,
    current_document: &Document,
    lhs: Node<'_>,
    rhs: Node<'_>,
    state: &mut ResolutionState,
    depth: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<AssignmentAnalysis, String> {
    let lhs_info = infer_argument(
        index,
        current_uri,
        current_document,
        lhs,
        state,
        depth,
        cancel,
        budget,
    )?;
    match lhs_info.writability {
        Writability::Writable => {}
        Writability::NonWritable => return Ok(AssignmentAnalysis::Unsupported),
        Writability::Unknown => return Ok(AssignmentAnalysis::Incomplete),
    }
    let Some(expected) = lhs_info.ty else {
        return Ok(AssignmentAnalysis::Incomplete);
    };
    let rhs_info = infer_argument(
        index,
        current_uri,
        current_document,
        rhs,
        state,
        depth,
        cancel,
        budget,
    )?;
    if state_has_uncertainty(state) {
        return Ok(AssignmentAnalysis::Incomplete);
    }
    if rhs_info.nil_literal {
        return Ok(match nil_compatibility(index, &expected, cancel, budget)? {
            Compatibility::Compatible => AssignmentAnalysis::Compatible,
            Compatibility::Unknown => AssignmentAnalysis::Incomplete,
            Compatibility::Incompatible => AssignmentAnalysis::Incompatible {
                expected: type_identity_label(&expected),
                actual: "nil".to_owned(),
            },
        });
    }
    let Some(actual) = rhs_info.ty else {
        return Ok(AssignmentAnalysis::Incomplete);
    };
    let mut ancestry = AncestryResolutionState::new();
    Ok(
        match assignment_conversion(
            index,
            &actual,
            &expected,
            state,
            &mut ancestry,
            cancel,
            budget,
        )? {
            Compatibility::Compatible => AssignmentAnalysis::Compatible,
            Compatibility::Incompatible => AssignmentAnalysis::Incompatible {
                expected: type_identity_label(&expected),
                actual: type_identity_label(&actual),
            },
            Compatibility::Unknown => AssignmentAnalysis::Incomplete,
        },
    )
}

pub(super) fn state_has_uncertainty(state: &ResolutionState) -> bool {
    state.receiver_resolution_uncertain()
        || state.member_lookup_incomplete
        || state.inaccessible_candidate
        || state.ambiguous
}

fn argument_span(argument: &ArgumentInfo) -> Span {
    argument.span
}

fn argument_actual_label(argument: &ArgumentInfo) -> String {
    if argument.nil_literal {
        "nil".to_owned()
    } else {
        argument
            .ty
            .as_ref()
            .map(type_identity_label)
            .unwrap_or_else(|| "unknown".to_owned())
    }
}

fn type_identity_label(identity: &TypeIdentity) -> String {
    match identity {
        TypeIdentity::Builtin(builtin) => match builtin {
            BuiltinType::Integer(kind) => match kind {
                IntegerKind::Literal => "integer literal".to_owned(),
                IntegerKind::ShortInt => "ShortInt".to_owned(),
                IntegerKind::SmallInt => "SmallInt".to_owned(),
                IntegerKind::Integer => "Integer".to_owned(),
                IntegerKind::Byte => "Byte".to_owned(),
                IntegerKind::Word => "Word".to_owned(),
                IntegerKind::Cardinal => "Cardinal".to_owned(),
                IntegerKind::LongWord => "LongWord".to_owned(),
                IntegerKind::Int64 => "Int64".to_owned(),
                IntegerKind::UInt64 => "UInt64".to_owned(),
                IntegerKind::NativeInt => "NativeInt".to_owned(),
                IntegerKind::NativeUInt => "NativeUInt".to_owned(),
            },
            BuiltinType::Real => "Real".to_owned(),
            BuiltinType::String => "String".to_owned(),
            BuiltinType::Character => "Char".to_owned(),
            BuiltinType::Boolean => "Boolean".to_owned(),
        },
        TypeIdentity::IntegerLiteral(_) => "integer literal".to_owned(),
        TypeIdentity::Named { key, args, .. } => {
            if args.is_empty() {
                key.clone()
            } else {
                format!(
                    "{}<{}>",
                    key,
                    args.iter()
                        .map(type_identity_label)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
    }
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
) -> Result<Option<(GenericSubstitution, bool)>, String> {
    let Some(owner_substitution) = index.routine_owner_substitution_with_budget(
        current_uri,
        call.child_by_field_name("entity")
            .map_or(0, |entity| entity.start_byte()),
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
        return Ok(Some((owner_substitution, false)));
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
            // Explicit actual type arguments belong to the lexical call
            // site.  The receiver's owner substitution is only for the
            // selected routine's declaration/result types; applying it here
            // makes a method call such as `Box.Pick<T>()` resolve `T` as the
            // owner's parameter instead of the caller's declaration.
            let receivers = index.type_receivers_for_type_ref_with_budget(
                current_uri,
                current_document,
                type_ref.span.start,
                &type_ref,
                argument,
                None,
                &GenericSubstitution::empty(),
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

    let constraint_outcome = generic_constraints_satisfied(
        index,
        candidate,
        symbol,
        &substitution,
        state,
        ancestry,
        cancel,
        budget,
    )?;
    match constraint_outcome {
        ConstraintOutcome::Contradictory => Ok(None),
        ConstraintOutcome::Proven => Ok(Some((substitution, false))),
        ConstraintOutcome::Unknown => Ok(Some((substitution, true))),
    }
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
) -> Result<ConstraintOutcome, String> {
    let Some(document) = index.documents.get(&candidate.uri) else {
        return Ok(ConstraintOutcome::Contradictory);
    };
    let mut unknown = false;
    for parameter in &symbol.generic_parameters {
        if parameter.constraint_unsupported {
            unknown = true;
            continue;
        }
        let Some(constraint) = parameter.constraint.as_ref() else {
            continue;
        };
        let Some(actual) = substitution
            .get(&parameter.name)
            .and_then(super::type_identity_from_resolved_type)
        else {
            unknown = true;
            continue;
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
                        return Ok(ConstraintOutcome::Contradictory);
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
                        return Ok(ConstraintOutcome::Contradictory);
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
                        return Ok(ConstraintOutcome::Contradictory);
                    }
                    continue;
                }
                "constructor" => {
                    match has_proven_constructor(
                        index,
                        &actual,
                        actual_uri_is_current(&actual, &candidate.uri),
                        cancel,
                        budget,
                    )? {
                        ConstraintOutcome::Proven => {}
                        ConstraintOutcome::Unknown => unknown = true,
                        ConstraintOutcome::Contradictory => {
                            return Ok(ConstraintOutcome::Contradictory);
                        }
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
            unknown = true;
            continue;
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
            unknown = true;
            continue;
        };
        match assignment_conversion(index, &actual, &expected, state, ancestry, cancel, budget)? {
            Compatibility::Compatible => {}
            Compatibility::Unknown => unknown = true,
            Compatibility::Incompatible => return Ok(ConstraintOutcome::Contradictory),
        }
    }
    Ok(if unknown {
        ConstraintOutcome::Unknown
    } else {
        ConstraintOutcome::Proven
    })
}

fn actual_uri_is_current(identity: &TypeIdentity, current_uri: &Url) -> bool {
    matches!(identity, TypeIdentity::Named { uri, .. } if uri == current_uri)
}

fn has_proven_constructor(
    index: &NavigationIndex,
    identity: &TypeIdentity,
    allow_implementation: bool,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<ConstraintOutcome, String> {
    let TypeIdentity::Named { uri, key, .. } = identity else {
        return Ok(ConstraintOutcome::Contradictory);
    };
    let Some(document) = index.documents.get(uri) else {
        return Ok(ConstraintOutcome::Unknown);
    };
    let Some(type_indices) = document.type_symbol_indices.get(key) else {
        return Ok(ConstraintOutcome::Unknown);
    };
    if type_indices.len() != 1 {
        return Ok(ConstraintOutcome::Unknown);
    }
    let Some(type_symbol) = document
        .symbols
        .get(*type_indices.first().expect("non-empty type indices"))
    else {
        return Ok(ConstraintOutcome::Unknown);
    };
    if type_symbol.kind != SymbolKind::Type {
        return Ok(ConstraintOutcome::Unknown);
    }
    let mut ancestry = AncestryResolutionState::new();
    let lookup = index.member_candidates_for_type_with_state_and_budget(
        uri,
        key,
        type_symbol.scope,
        None,
        allow_implementation,
        &mut ancestry,
        cancel,
        budget,
    )?;
    if !lookup.ancestry_known || !lookup.ambiguous_names.is_empty() {
        return Ok(ConstraintOutcome::Unknown);
    }
    let has_constructor = lookup.candidates.iter().any(|candidate| {
        index.symbol(candidate).is_some_and(|symbol| {
            symbol.kind == SymbolKind::Routine
                && symbol.routine_kind == super::RoutineKind::Constructor
                && symbol
                    .routine_parameters
                    .iter()
                    .all(|parameter| parameter.has_default)
        })
    });
    Ok(if has_constructor {
        ConstraintOutcome::Proven
    } else {
        ConstraintOutcome::Unknown
    })
}

fn generic_call_arguments(call: Node<'_>) -> Option<Vec<Node<'_>>> {
    let entity = call.child_by_field_name("entity")?;
    let template = generic_template_for_callable(entity)?;
    let arguments = template.child_by_field_name("args")?;
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
                        && !argument.is_extra()
                        && !matches!(argument.kind(), "kLt" | "kGt")
                })
                .collect(),
        )
    }
}

fn generic_template_for_callable(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "exprTpl" => Some(node),
        "exprDot" | "genericDot" | "typerefDot" => node
            .child_by_field_name("rhs")
            .and_then(generic_template_for_callable),
        _ => None,
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
                helper_owner: None,
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
        return Ok(parameter
            .type_shape
            .as_ref()
            .and_then(|shape| anonymous_shape_identity(&candidate.uri, shape)));
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
            .and_then(|name| builtin_type(&name).map(TypeIdentity::Builtin))
            .or_else(|| {
                parameter
                    .type_shape
                    .as_ref()
                    .and_then(|shape| anonymous_shape_identity(&candidate.uri, shape))
            }));
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
    state: &mut ResolutionState,
    depth: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<ArgumentInfo, String> {
    let argument_span = Span::from_node(node);
    let mut node = node;
    loop {
        check_navigation_cancel(cancel)?;
        if node.kind() == "exprParens" {
            let Some(operand) = node.named_child(0) else {
                return Ok(unknown_argument(argument_span));
            };
            budget.require_work(1, cancel)?;
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
            span: argument_span,
            ty: Some(ty),
            writability: Writability::NonWritable,
            nil_literal: false,
        });
    }
    if matches!(kind, "literalString" | "literalChar") {
        let text = node_text(index, current_document, node, cancel, budget)?;
        let classification = if kind == "literalChar" {
            Some(LiteralClassification::Character)
        } else {
            classify_literal_fragments(&text)
        };
        let Some(classification) = classification else {
            return Ok(unknown_argument(argument_span));
        };
        return Ok(ArgumentInfo {
            span: argument_span,
            ty: Some(TypeIdentity::Builtin(
                if classification == LiteralClassification::Character {
                    BuiltinType::Character
                } else {
                    BuiltinType::String
                },
            )),
            writability: Writability::NonWritable,
            nil_literal: false,
        });
    }
    if kind == "kTrue" || kind == "kFalse" {
        return Ok(ArgumentInfo {
            span: argument_span,
            ty: Some(TypeIdentity::Builtin(BuiltinType::Boolean)),
            writability: Writability::NonWritable,
            nil_literal: false,
        });
    }
    if kind == "kNil" {
        return Ok(ArgumentInfo {
            span: argument_span,
            ty: None,
            writability: Writability::NonWritable,
            nil_literal: true,
        });
    }
    if kind == "exprUnary" {
        let Some(operator) = node.child_by_field_name("operator") else {
            return Ok(unknown_argument(argument_span));
        };
        let operator = node_text(index, current_document, operator, cancel, budget)?;
        if operator != "^" {
            return Ok(unknown_argument(argument_span));
        }
        let Some(operand) = node.child_by_field_name("operand") else {
            return Ok(unknown_argument(argument_span));
        };
        let base = infer_argument(
            index,
            current_uri,
            current_document,
            operand,
            state,
            depth.saturating_add(1),
            cancel,
            budget,
        )?;
        let ty = element_type_for_expression(
            index,
            current_uri,
            current_document,
            operand,
            state,
            depth.saturating_add(1),
            cancel,
            budget,
        )?;
        let writability = if ty.is_some() {
            base.writability
        } else {
            Writability::Unknown
        };
        return Ok(ArgumentInfo {
            span: argument_span,
            ty,
            writability,
            nil_literal: false,
        });
    }
    if kind == "exprSubscript" {
        let Some(entity) = node.child_by_field_name("entity") else {
            return Ok(unknown_argument(argument_span));
        };
        let base = infer_argument(
            index,
            current_uri,
            current_document,
            entity,
            state,
            depth.saturating_add(1),
            cancel,
            budget,
        )?;
        let ty = element_type_for_expression(
            index,
            current_uri,
            current_document,
            entity,
            state,
            depth.saturating_add(1),
            cancel,
            budget,
        )?;
        let writability = if ty.is_some() {
            base.writability
        } else {
            Writability::Unknown
        };
        return Ok(ArgumentInfo {
            span: argument_span,
            ty,
            writability,
            nil_literal: false,
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
        let ty = receiver_type(index, receivers)?;
        return Ok(ArgumentInfo {
            span: argument_span,
            writability: if ty.is_some() {
                Writability::NonWritable
            } else {
                Writability::Unknown
            },
            ty,
            nil_literal: false,
        });
    }

    let Some(identifier) = expression_identifier(node) else {
        return Ok(unknown_argument(argument_span));
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
    if candidates
        .iter()
        .any(|candidate| index.candidate_is_conditionally_unknown(candidate))
    {
        return Ok(ArgumentInfo {
            span: argument_span,
            ty: None,
            writability: Writability::Unknown,
            nil_literal: false,
        });
    }
    if candidates.is_empty() {
        return Ok(ArgumentInfo {
            span: argument_span,
            ty: None,
            writability: Writability::Unknown,
            nil_literal: false,
        });
    }

    let mut identities = HashSet::new();
    let mut unknown = false;
    let mut binding_unknown = false;
    let mut writable = false;
    let mut non_writable = false;
    for candidate in candidates {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let Some(symbol) = index.symbol(&candidate) else {
            unknown = true;
            binding_unknown = true;
            continue;
        };
        if is_assignable_symbol(symbol) {
            writable = true;
        } else if matches!(
            symbol.kind,
            SymbolKind::Constant
                | SymbolKind::EnumValue
                | SymbolKind::Routine
                | SymbolKind::Type
                | SymbolKind::Property
        ) {
            non_writable = true;
        } else {
            binding_unknown = true;
        }
        if matches!(
            symbol.kind,
            SymbolKind::Variable | SymbolKind::Parameter | SymbolKind::Field | SymbolKind::Property
        ) {
            match symbol_type(
                index,
                &candidate,
                symbol,
                &GenericSubstitution::empty(),
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
    let identifier_is_result = node_text(index, current_document, identifier, cancel, budget)?
        .eq_ignore_ascii_case("Result");
    let writability = if binding_unknown || (writable && non_writable) {
        Writability::Unknown
    } else if writable || identifier_is_result {
        Writability::Writable
    } else if non_writable {
        Writability::NonWritable
    } else {
        Writability::Unknown
    };
    Ok(ArgumentInfo {
        span: argument_span,
        ty: if !unknown && identities.len() == 1 {
            identities.into_iter().next()
        } else {
            None
        },
        writability,
        nil_literal: false,
    })
}

fn unknown_argument(span: Span) -> ArgumentInfo {
    ArgumentInfo {
        span,
        ty: None,
        writability: Writability::Unknown,
        nil_literal: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn element_type_for_expression(
    index: &NavigationIndex,
    current_uri: &Url,
    current_document: &Document,
    node: Node<'_>,
    state: &mut ResolutionState,
    depth: usize,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<TypeIdentity>, String> {
    if depth >= super::MAX_RECEIVER_RECURSION_DEPTH {
        return Ok(None);
    }
    let Some(identifier) = expression_identifier(node) else {
        return Ok(None);
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
    let mut result = None;
    for candidate in candidates {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let Some(symbol) = index.symbol(&candidate) else {
            return Ok(None);
        };
        if !matches!(
            symbol.kind,
            SymbolKind::Variable | SymbolKind::Parameter | SymbolKind::Field | SymbolKind::Property
        ) {
            return Ok(None);
        }
        let Some(shape) = symbol.type_shape.as_ref() else {
            return Ok(None);
        };
        let identity = shape_element_identity(
            index,
            &candidate,
            symbol,
            shape,
            state,
            cancel,
            budget,
            &mut HashSet::new(),
        )?;
        let Some(identity) = identity else {
            return Ok(None);
        };
        if result.as_ref().is_some_and(|current| current != &identity) {
            return Ok(None);
        }
        result = Some(identity);
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn shape_element_identity(
    index: &NavigationIndex,
    candidate: &Candidate,
    symbol: &Symbol,
    shape: &TypeShape,
    state: &mut ResolutionState,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
    visited: &mut HashSet<(Url, String)>,
) -> Result<Option<TypeIdentity>, String> {
    match shape {
        TypeShape::Pointer(element) | TypeShape::Array { element, .. } => {
            scalar_identity_from_shape(index, candidate, symbol, element, state, cancel, budget)
        }
        TypeShape::Named(type_ref) => {
            let Some(document) = index.documents.get(&candidate.uri) else {
                return Ok(None);
            };
            let Some(lookup_identifier) = assistance::identifier_at_with_budget(
                document.tree.root_node(),
                type_ref.span.start,
                cancel,
                budget,
                "indexed expression type",
            )?
            else {
                return Ok(None);
            };
            let receivers = index.type_receivers_for_type_ref_with_budget(
                &candidate.uri,
                document,
                type_ref.span.start,
                type_ref,
                lookup_identifier,
                Some(symbol.scope),
                &GenericSubstitution::empty(),
                state,
                cancel,
                budget,
            )?;
            let Some(receiver) = unique_receiver(receivers) else {
                return Ok(None);
            };
            match receiver {
                Receiver::Builtin(builtin) => Ok(Some(TypeIdentity::Builtin(builtin))),
                Receiver::IntegerLiteral(value) => Ok(Some(TypeIdentity::IntegerLiteral(value))),
                Receiver::Type(instance) => {
                    let identity = TypeIdentity::Named {
                        uri: instance.uri.clone(),
                        key: instance.key.clone(),
                        kind: instance.kind,
                        args: instance
                            .parameter_names
                            .iter()
                            .filter_map(|name| instance.substitution.get(name))
                            .filter_map(super::type_identity_from_resolved_type)
                            .collect(),
                    };
                    if !matches!(
                        instance.kind,
                        TypeKind::Pointer
                            | TypeKind::Array
                            | TypeKind::DynamicArray
                            | TypeKind::Callable
                            | TypeKind::Other
                    ) {
                        return Ok(Some(identity));
                    }
                    let key = (instance.uri.clone(), instance.key.clone());
                    if !visited.insert(key) {
                        return Ok(None);
                    }
                    let Some(type_document) = index.documents.get(&instance.uri) else {
                        return Ok(None);
                    };
                    let Some(indices) = type_document.type_symbol_indices.get(&instance.key) else {
                        return Ok(None);
                    };
                    if indices.len() != 1 {
                        return Ok(None);
                    }
                    let type_candidate = Candidate {
                        uri: instance.uri,
                        index: indices[0],
                    };
                    let Some(type_symbol) = index.symbol(&type_candidate) else {
                        return Ok(None);
                    };
                    let Some(type_shape) = type_symbol.type_shape.as_ref() else {
                        return Ok(None);
                    };
                    shape_element_identity(
                        index,
                        &type_candidate,
                        type_symbol,
                        type_shape,
                        state,
                        cancel,
                        budget,
                        visited,
                    )
                }
                Receiver::Unit(_) => Ok(None),
            }
        }
        TypeShape::Callable | TypeShape::Unknown => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
fn scalar_identity_from_shape(
    index: &NavigationIndex,
    candidate: &Candidate,
    symbol: &Symbol,
    shape: &TypeShape,
    state: &mut ResolutionState,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<TypeIdentity>, String> {
    match shape {
        TypeShape::Named(type_ref) => {
            let Some(document) = index.documents.get(&candidate.uri) else {
                return Ok(None);
            };
            let Some(lookup_identifier) = assistance::identifier_at_with_budget(
                document.tree.root_node(),
                type_ref.span.start,
                cancel,
                budget,
                "indexed element type",
            )?
            else {
                return Ok(None);
            };
            let receivers = index.type_receivers_for_type_ref_with_budget(
                &candidate.uri,
                document,
                type_ref.span.start,
                type_ref,
                lookup_identifier,
                Some(symbol.scope),
                &GenericSubstitution::empty(),
                state,
                cancel,
                budget,
            )?;
            let identity = receiver_type(index, receivers)?;
            Ok(identity)
        }
        TypeShape::Pointer(_)
        | TypeShape::Array { .. }
        | TypeShape::Callable
        | TypeShape::Unknown => Ok(None),
    }
}

fn unique_receiver(receivers: Vec<Receiver>) -> Option<Receiver> {
    (receivers.len() == 1)
        .then(|| receivers.into_iter().next())
        .flatten()
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

fn classify_literal_fragments(text: &str) -> Option<LiteralClassification> {
    let mut remaining = text.trim();
    let mut logical_characters = 0usize;
    let mut character_value_is_certain = true;
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
            let value = if let Some(hex_digits) = after_hash.strip_prefix('$') {
                &hex_digits[..length]
            } else {
                &after_hash[..length]
            };
            let radix = if after_hash.starts_with('$') { 16 } else { 10 };
            character_value_is_certain = character_value_is_certain
                && u32::from_str_radix(value, radix).is_ok_and(|value| value <= u8::MAX as u32);
            logical_characters = logical_characters.saturating_add(1);
            continue;
        }
        if remaining.starts_with('\'') {
            let bytes = remaining.as_bytes();
            let mut index = 1;
            while index < bytes.len() {
                if bytes[index] != b'\'' {
                    let character = remaining[index..].chars().next()?;
                    if !character.is_ascii() {
                        character_value_is_certain = false;
                    }
                    logical_characters = logical_characters.saturating_add(1);
                    index += character.len_utf8();
                    continue;
                }
                if bytes.get(index + 1) == Some(&b'\'') {
                    logical_characters = logical_characters.saturating_add(1);
                    index += 2;
                    continue;
                }
                remaining = &remaining[index + 1..];
                break;
            }
            if index >= bytes.len() {
                return None;
            }
            continue;
        }
        return None;
    }
    match logical_characters {
        1 if character_value_is_certain => Some(LiteralClassification::Character),
        1 => None,
        _ => Some(LiteralClassification::String),
    }
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
        return Ok(symbol
            .type_shape
            .as_ref()
            .and_then(|shape| anonymous_shape_identity(&candidate.uri, shape)));
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
            .and_then(|name| builtin_type(&name).map(TypeIdentity::Builtin))
            .or_else(|| {
                symbol
                    .type_shape
                    .as_ref()
                    .and_then(|shape| anonymous_shape_identity(&candidate.uri, shape))
            }));
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

fn anonymous_shape_identity(uri: &Url, shape: &TypeShape) -> Option<TypeIdentity> {
    let kind = match shape {
        TypeShape::Pointer(_) => TypeKind::Pointer,
        TypeShape::Array { dynamic: true, .. } => TypeKind::DynamicArray,
        TypeShape::Array { dynamic: false, .. } => TypeKind::Array,
        TypeShape::Callable => TypeKind::Callable,
        TypeShape::Named(_) | TypeShape::Unknown => return None,
    };
    Some(TypeIdentity::Named {
        uri: uri.clone(),
        key: format!("<anonymous:{}>", shape_label(shape)),
        kind,
        args: Vec::new(),
    })
}

fn shape_label(shape: &TypeShape) -> String {
    match shape {
        TypeShape::Named(type_ref) => type_ref.display(),
        TypeShape::Pointer(element) => format!("pointer<{}>", shape_label(element)),
        TypeShape::Array { element, dynamic } => {
            let kind = if *dynamic { "dynamic-array" } else { "array" };
            format!("{kind}<{}>", shape_label(element))
        }
        TypeShape::Callable => "callable".to_owned(),
        TypeShape::Unknown => "unknown".to_owned(),
    }
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
        // Range narrowing and signedness changes are legal Pascal integer
        // conversions for an unknown runtime value.  They remain more
        // expensive for overload ranking, but are not negative proof for an
        // assignment or value argument.
        (Some(_), Some(_)) => Conversion::Cost(2),
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
fn overload_conversion(
    index: &NavigationIndex,
    actual: &TypeIdentity,
    expected: &TypeIdentity,
    state: &mut ResolutionState,
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
                args: actual_args,
            },
            TypeIdentity::Named {
                uri: expected_uri,
                key: expected_key,
                kind: expected_kind,
                args: expected_args,
            },
        ) => match upcast_distance(
            index,
            actual_uri,
            actual_key,
            *actual_kind,
            actual_args,
            expected_uri,
            expected_key,
            *expected_kind,
            expected_args,
            state,
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
            if !matches!(kind, TypeKind::Class | TypeKind::Interface) =>
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
        (TypeIdentity::IntegerLiteral(_), TypeIdentity::Named { kind, .. })
            if !matches!(kind, TypeKind::Class | TypeKind::Interface) =>
        {
            Ok(Conversion::Unknown)
        }
        (TypeIdentity::IntegerLiteral(_), TypeIdentity::Named { .. })
        | (TypeIdentity::Named { .. }, TypeIdentity::IntegerLiteral(_))
        | (TypeIdentity::Builtin(_), TypeIdentity::IntegerLiteral(_)) => {
            Ok(Conversion::Incompatible)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn assignment_conversion(
    index: &NavigationIndex,
    actual: &TypeIdentity,
    expected: &TypeIdentity,
    state: &mut ResolutionState,
    ancestry: &mut AncestryResolutionState,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Compatibility, String> {
    match (actual, expected) {
        (TypeIdentity::IntegerLiteral(value), TypeIdentity::Builtin(expected)) => {
            Ok(match expected {
                BuiltinType::Integer(kind) => {
                    compatibility_from_conversion(integer_literal_conversion(*value, *kind))
                }
                BuiltinType::Real => Compatibility::Compatible,
                _ => Compatibility::Incompatible,
            })
        }
        (TypeIdentity::Builtin(actual), TypeIdentity::Builtin(expected)) => {
            Ok(match (actual, expected) {
                (left, right) if left == right => Compatibility::Compatible,
                (BuiltinType::Integer(_), BuiltinType::Real)
                | (BuiltinType::Character, BuiltinType::String)
                | (BuiltinType::Integer(_), BuiltinType::Integer(_)) => Compatibility::Compatible,
                _ => Compatibility::Incompatible,
            })
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
            Ok(if actual_args == expected_args {
                Compatibility::Compatible
            } else {
                Compatibility::Incompatible
            })
        }
        (
            TypeIdentity::Named {
                uri: actual_uri,
                key: actual_key,
                kind: actual_kind,
                args: actual_args,
            },
            TypeIdentity::Named {
                uri: expected_uri,
                key: expected_key,
                kind: expected_kind,
                args: expected_args,
            },
        ) => Ok(
            match upcast_distance(
                index,
                actual_uri,
                actual_key,
                *actual_kind,
                actual_args,
                expected_uri,
                expected_key,
                *expected_kind,
                expected_args,
                state,
                ancestry,
                cancel,
                budget,
            )? {
                Upcast::Distance(_) => Compatibility::Compatible,
                Upcast::No => Compatibility::Incompatible,
                Upcast::Unknown => Compatibility::Unknown,
            },
        ),
        (TypeIdentity::Builtin(_), TypeIdentity::Named { kind, .. })
        | (TypeIdentity::Named { kind, .. }, TypeIdentity::Builtin(_))
            if !matches!(kind, TypeKind::Class | TypeKind::Interface) =>
        {
            Ok(Compatibility::Unknown)
        }
        (TypeIdentity::Builtin(_), TypeIdentity::Named { .. })
        | (TypeIdentity::Named { .. }, TypeIdentity::Builtin(_)) => Ok(Compatibility::Incompatible),
        (TypeIdentity::IntegerLiteral(actual), TypeIdentity::IntegerLiteral(expected)) => {
            Ok(if actual == expected {
                Compatibility::Compatible
            } else {
                Compatibility::Unknown
            })
        }
        (TypeIdentity::IntegerLiteral(_), TypeIdentity::Named { kind, .. })
            if !matches!(kind, TypeKind::Class | TypeKind::Interface) =>
        {
            Ok(Compatibility::Unknown)
        }
        (TypeIdentity::IntegerLiteral(_), TypeIdentity::Named { .. })
        | (TypeIdentity::Named { .. }, TypeIdentity::IntegerLiteral(_))
        | (TypeIdentity::Builtin(_), TypeIdentity::IntegerLiteral(_)) => {
            Ok(Compatibility::Incompatible)
        }
    }
}

fn compatibility_from_conversion(conversion: Conversion) -> Compatibility {
    match conversion {
        Conversion::Cost(_) => Compatibility::Compatible,
        Conversion::Unknown => Compatibility::Unknown,
        Conversion::Incompatible => Compatibility::Incompatible,
    }
}

fn nil_compatibility(
    index: &NavigationIndex,
    expected: &TypeIdentity,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Compatibility, String> {
    Ok(match expected {
        TypeIdentity::Named { uri, key, kind, .. } => match kind {
            TypeKind::Class
            | TypeKind::Interface
            | TypeKind::Pointer
            | TypeKind::Callable
            | TypeKind::DynamicArray => Compatibility::Compatible,
            TypeKind::Record => record_nil_compatibility(index, uri, key, cancel, budget)?,
            TypeKind::Other => {
                let Some(document) = index.documents.get(uri) else {
                    return Ok(Compatibility::Unknown);
                };
                let Some(indices) = document.type_symbol_indices.get(key) else {
                    return Ok(Compatibility::Unknown);
                };
                if indices.len() != 1 {
                    return Ok(Compatibility::Unknown);
                }
                let Some(symbol) = document.symbols.get(indices[0]) else {
                    return Ok(Compatibility::Unknown);
                };
                match symbol.type_kind {
                    TypeKind::Pointer | TypeKind::Callable | TypeKind::DynamicArray => {
                        Compatibility::Compatible
                    }
                    TypeKind::Other => Compatibility::Unknown,
                    _ => Compatibility::Incompatible,
                }
            }
            TypeKind::Enum | TypeKind::Array | TypeKind::String | TypeKind::File => {
                Compatibility::Incompatible
            }
        },
        TypeIdentity::Builtin(_) | TypeIdentity::IntegerLiteral(_) => Compatibility::Incompatible,
    })
}

fn record_nil_compatibility(
    index: &NavigationIndex,
    uri: &Url,
    key: &str,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Compatibility, String> {
    let Some(document) = index.documents.get(uri) else {
        return Ok(Compatibility::Unknown);
    };
    let Some(indices) = document.type_symbol_indices.get(key) else {
        return Ok(Compatibility::Unknown);
    };
    if indices.len() != 1 {
        return Ok(Compatibility::Unknown);
    }
    let Some(type_symbol) = document.symbols.get(indices[0]) else {
        return Ok(Compatibility::Unknown);
    };
    if type_symbol.kind != SymbolKind::Type || type_symbol.type_kind != TypeKind::Record {
        return Ok(Compatibility::Unknown);
    }

    budget.require_work(document.symbols.len(), cancel)?;
    if document.symbols.iter().any(|symbol| {
        symbol.kind == SymbolKind::Routine
            && symbol.routine_kind == RoutineKind::Operator
            && symbol.owner_type.as_deref() == Some(key)
    }) {
        Ok(Compatibility::Unknown)
    } else {
        Ok(Compatibility::Incompatible)
    }
}

fn byref_type_match(actual: &TypeIdentity, expected: &TypeIdentity) -> Compatibility {
    if actual == expected {
        return Compatibility::Compatible;
    }
    match (actual, expected) {
        (
            TypeIdentity::Builtin(BuiltinType::Integer(actual)),
            TypeIdentity::Builtin(BuiltinType::Integer(expected)),
        ) => match (
            canonical_integer_kind(*actual),
            canonical_integer_kind(*expected),
        ) {
            (Some(actual), Some(expected)) if actual == expected => Compatibility::Compatible,
            (None, _) | (_, None) => Compatibility::Unknown,
            _ => Compatibility::Incompatible,
        },
        (
            TypeIdentity::Named {
                kind: actual_kind, ..
            },
            TypeIdentity::Named {
                kind: expected_kind,
                ..
            },
        ) if matches!(
            (actual_kind, expected_kind),
            (
                TypeKind::Other | TypeKind::Pointer | TypeKind::Callable | TypeKind::DynamicArray,
                _
            ) | (
                _,
                TypeKind::Other | TypeKind::Pointer | TypeKind::Callable | TypeKind::DynamicArray
            )
        ) =>
        {
            Compatibility::Unknown
        }
        (
            TypeIdentity::Builtin(_),
            TypeIdentity::Named {
                kind: expected_kind,
                ..
            },
        ) if *expected_kind == TypeKind::Other => Compatibility::Unknown,
        (
            TypeIdentity::Named {
                kind: actual_kind, ..
            },
            TypeIdentity::Builtin(_),
        ) if *actual_kind == TypeKind::Other => Compatibility::Unknown,
        _ => Compatibility::Incompatible,
    }
}

fn canonical_integer_kind(kind: IntegerKind) -> Option<IntegerKind> {
    match kind {
        IntegerKind::Cardinal | IntegerKind::LongWord => Some(IntegerKind::Cardinal),
        IntegerKind::Integer => Some(IntegerKind::Integer),
        IntegerKind::NativeInt | IntegerKind::Int64 => None,
        IntegerKind::NativeUInt | IntegerKind::UInt64 => None,
        IntegerKind::Literal => None,
        _ => Some(kind),
    }
}

#[allow(clippy::too_many_arguments)]
fn upcast_distance(
    index: &NavigationIndex,
    actual_uri: &Url,
    actual_key: &str,
    actual_kind: TypeKind,
    actual_args: &[TypeIdentity],
    expected_uri: &Url,
    expected_key: &str,
    expected_kind: TypeKind,
    expected_args: &[TypeIdentity],
    state: &mut ResolutionState,
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
        return Ok(Upcast::Unknown);
    }

    let actual = TypeIdentity::Named {
        uri: actual_uri.clone(),
        key: actual_key.to_owned(),
        kind: actual_kind,
        args: actual_args.to_vec(),
    };
    let mut queue = VecDeque::from([(actual, 0u32)]);
    let mut visited = HashSet::new();
    while let Some((current, distance)) = queue.pop_front() {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        let TypeIdentity::Named { uri, key, args, .. } = &current else {
            return Ok(Upcast::Unknown);
        };
        if !visited.insert(current.clone()) {
            continue;
        }
        if uri == expected_uri && key == expected_key {
            return Ok(if args == expected_args {
                Upcast::Distance(distance)
            } else {
                Upcast::No
            });
        }
        let ancestry_result =
            index.resolve_type_ancestry_with_budget(uri, key, ancestry, cancel, budget)?;
        if ancestry_result.status == AncestryStatus::Unknown {
            return Ok(Upcast::Unknown);
        }
        let Some(document) = index.documents.get(uri) else {
            return Ok(Upcast::Unknown);
        };
        let Some(entries) = document.type_ancestry.get(key) else {
            return Ok(Upcast::Unknown);
        };
        let Some(entry) = entries.first().filter(|_| entries.len() == 1) else {
            return Ok(Upcast::Unknown);
        };
        if entry.kind == TypeKind::Class && !entry.parent_declared {
            let implicit_parent = implicit_tobject_identity(
                index,
                uri,
                document,
                entry.name_span.start,
                state,
                cancel,
                budget,
            )?;
            let expected_is_implicit_root = implicit_parent.as_ref().is_some_and(|parent| {
                matches!(
                    parent,
                    TypeIdentity::Named {
                        uri,
                        key,
                        args,
                        ..
                    } if uri == expected_uri && key == expected_key && args == expected_args
                )
            });
            // A rootless class can only have the implicit TObject ancestor.
            // Consequently it cannot be a distinct rootless class (or any
            // other explicitly declared class) when the expected type is not
            // TObject.  Keep TObject itself unresolved when the selected
            // System domain is unavailable: that relationship remains
            // unknown rather than becoming a false negative proof.
            if expected_kind == TypeKind::Class
                && (uri != expected_uri || key != expected_key)
                && expected_key != "tobject"
            {
                return Ok(Upcast::No);
            }
            if uri == expected_uri && key != expected_key {
                if let Some(expected_document) = index.documents.get(expected_uri) {
                    if !expected_is_implicit_root
                        && expected_document
                            .type_ancestry
                            .get(expected_key)
                            .is_some_and(|entries| {
                                entries.len() == 1 && entries[0].kind == TypeKind::Class
                            })
                    {
                        return Ok(Upcast::No);
                    }
                }
            }
            let Some(parent) = implicit_parent else {
                return Ok(Upcast::Unknown);
            };
            let next_distance = distance.saturating_add(1);
            if parent != current {
                queue.push_back((parent, next_distance));
            }
        }
        let Some(ResolvedType::Named(instance)) = resolved_type_from_identity(index, &current)
        else {
            return Ok(Upcast::Unknown);
        };
        for parent in entry.parents.iter().filter(|parent| {
            matches!(
                (entry.kind, parent.relation),
                (TypeKind::Class, super::ParentRelation::Superclass)
                    | (TypeKind::Interface, super::ParentRelation::InterfaceParent)
            )
        }) {
            let Some(type_ref) = parent.type_ref.as_ref() else {
                return Ok(Upcast::Unknown);
            };
            let Some(lookup_identifier) = assistance::identifier_at_with_budget(
                document.tree.root_node(),
                type_ref.span.start,
                cancel,
                budget,
                "ancestry parent type",
            )?
            else {
                return Ok(Upcast::Unknown);
            };
            let receivers = index.type_receivers_for_type_ref_with_budget(
                uri,
                document,
                type_ref.span.start,
                type_ref,
                lookup_identifier,
                None,
                &instance.substitution,
                state,
                cancel,
                budget,
            )?;
            let Some(parent) = super::resolved_type_from_receivers(receivers)
                .and_then(|resolved| super::type_identity_from_resolved_type(&resolved))
            else {
                return Ok(Upcast::Unknown);
            };
            let next_distance = distance.saturating_add(1);
            queue.push_back((parent, next_distance));
        }
    }
    Ok(Upcast::No)
}

#[allow(clippy::too_many_arguments)]
fn implicit_tobject_identity(
    index: &NavigationIndex,
    current_uri: &Url,
    current_document: &Document,
    offset: usize,
    state: &mut ResolutionState,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<TypeIdentity>, String> {
    let system_urls = index.visible_unit_urls_with_budget(
        current_uri,
        current_document,
        offset,
        "System",
        cancel,
        budget,
    )?;
    let Some(system_uri) = system_urls.first().filter(|_| system_urls.len() == 1) else {
        return Ok(None);
    };
    let candidates =
        index.type_candidates_in_unit_with_budget(system_uri, "TObject", false, cancel, budget)?;
    if candidates.len() != 1 {
        return Ok(None);
    }
    let candidate = &candidates[0];
    if index.candidate_is_conditionally_unknown(candidate) {
        return Ok(None);
    }
    let Some(Receiver::Type(instance)) = index.type_receiver_for_candidate(candidate) else {
        return Ok(None);
    };
    let _ = state;
    Ok(Some(TypeIdentity::Named {
        uri: instance.uri,
        key: instance.key,
        kind: instance.kind,
        args: instance
            .parameter_names
            .iter()
            .filter_map(|name| instance.substitution.get(name))
            .filter_map(super::type_identity_from_resolved_type)
            .collect(),
    }))
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
