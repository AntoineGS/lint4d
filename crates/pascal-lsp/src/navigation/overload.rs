use super::{
    AncestryResolutionState, AncestryStatus, AssistanceBudget, BuiltinType, Candidate, Document,
    IntegerKind, NavigationIndex, Origin, ParameterMode, Receiver, Region, ResolutionState,
    RoutineParameter, Span, Symbol, SymbolKind, TypeKind, assistance, canonical_name,
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
    pub(super) no_viable_group: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TypeIdentity {
    Builtin(BuiltinType),
    IntegerLiteral(i128),
    Named {
        uri: Url,
        key: String,
        kind: TypeKind,
    },
}

#[derive(Debug, Clone)]
struct ArgumentInfo {
    ty: Option<TypeIdentity>,
    assignable: bool,
    nil_literal: bool,
}

#[derive(Debug, Clone, Copy)]
struct GroupScore {
    cost: u32,
    uncertain: bool,
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
        return Ok(Selection {
            selected_group: Some(possible.pop().expect("one possible overload").0.key),
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
    let Some((group, _)) = best.next() else {
        return Ok(Selection::default());
    };
    if best.next().is_some() {
        return Ok(Selection::default());
    }
    Ok(Selection {
        selected_group: Some(group.key),
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

    let mut cost: u32 = 0;
    let mut uncertain = false;
    for (argument, parameter) in arguments.iter().zip(parameters) {
        check_navigation_cancel(cancel)?;
        budget.require_work(1, cancel)?;
        if matches!(parameter.mode, ParameterMode::Var | ParameterMode::Out) && !argument.assignable
        {
            return Ok(None);
        }
        let expected = parameter_type(index, candidate, symbol, parameter, state, cancel, budget)?;
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
    Ok(Some(GroupScore { cost, uncertain }))
}

#[allow(clippy::too_many_arguments)]
fn parameter_type(
    index: &NavigationIndex,
    candidate: &Candidate,
    symbol: &Symbol,
    parameter: &RoutineParameter,
    state: &mut ResolutionState,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<TypeIdentity>, String> {
    let Some(type_name) = parameter.type_name.as_deref() else {
        return Ok(None);
    };
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
        return Ok(builtin_type(type_name).map(TypeIdentity::Builtin));
    };
    let receivers = index.type_receivers_for_path_with_budget_at_scope(
        &candidate.uri,
        document,
        offset,
        type_name,
        lookup_identifier,
        Some(symbol.scope),
        state,
        cancel,
        budget,
    )?;
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
        let is_character = kind == "literalChar" || text.trim_start().starts_with('#');
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
            match symbol_type(index, &candidate, symbol, state, cancel, budget)? {
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
            Some(ParameterMode::Var | ParameterMode::Out)
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
    match (actual, expected) {
        (TypeIdentity::Builtin(actual), TypeIdentity::Builtin(expected)) => actual == expected,
        (
            TypeIdentity::Named {
                uri: actual_uri,
                key: actual_key,
                ..
            },
            TypeIdentity::Named {
                uri: expected_uri,
                key: expected_key,
                ..
            },
        ) => actual_uri == expected_uri && actual_key == expected_key,
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
    state: &mut ResolutionState,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<Option<TypeIdentity>, String> {
    let Some(type_name) = symbol.type_name.as_deref() else {
        return Ok(None);
    };
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
        return Ok(builtin_type(type_name).map(TypeIdentity::Builtin));
    };
    let receivers = index.type_receivers_for_path_with_budget_at_scope(
        &candidate.uri,
        document,
        symbol.span.start,
        type_name,
        lookup_identifier,
        Some(symbol.scope),
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
            Receiver::Type(uri, key, _) => {
                let Some(kind) = index.type_kind(&uri, &key) else {
                    return Ok(None);
                };
                TypeIdentity::Named { uri, key, kind }
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
        "longint" => Some(BuiltinType::Integer(IntegerKind::LongInt)),
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
        IntegerKind::Integer | IntegerKind::LongInt => Some((i32::MIN as i128, i32::MAX as i128)),
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
                (BuiltinType::Integer(_), BuiltinType::Real)
                | (BuiltinType::Character, BuiltinType::Integer(_)) => Conversion::Cost(1),
                (BuiltinType::Character, BuiltinType::Real) => Conversion::Cost(2),
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
                ..
            },
            TypeIdentity::Named {
                uri: expected_uri,
                key: expected_key,
                kind: _,
            },
        ) if actual_uri == expected_uri && actual_key == expected_key => Ok(Conversion::Cost(0)),
        (
            TypeIdentity::Named {
                uri: actual_uri,
                key: actual_key,
                kind: actual_kind,
            },
            TypeIdentity::Named {
                uri: expected_uri,
                key: expected_key,
                kind: expected_kind,
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
        (TypeIdentity::IntegerLiteral(_), TypeIdentity::IntegerLiteral(_)) => {
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
