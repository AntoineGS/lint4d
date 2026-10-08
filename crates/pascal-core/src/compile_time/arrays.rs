//! Checked ordinal domains and array storage. A domain is not an enum's ABI width.
use super::bindings::{
    Binding, EnumMember, IndexExpr, TypeExpr, intrinsic, lookup, same_name, storage_type,
};
use super::declarations::{initializer_expression, limit};
use super::*;
use crate::conditional::{
    BoundValue, ConstantValue, MAX_EXPRESSION_TOKENS, evaluate_bound_expression,
};
use pascal_project::{LayoutPlatform, LayoutSettings};
use std::mem::size_of;

type Domain = (i64, i64);

enum OrdinalKind {
    Integer,
    Boolean,
    Character(BuiltinType),
    Enum(DefinitionKey),
}
impl OrdinalKind {
    fn compatible(
        &self,
        other: &Self,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<bool, IncompleteReason> {
        budget.charge(1, 0, 0)?;
        Ok(match (self, other) {
            (Self::Integer, Self::Integer) | (Self::Boolean, Self::Boolean) => true,
            (Self::Character(a), Self::Character(b)) => a == b,
            (Self::Enum(a), Self::Enum(b)) => {
                budget.charge(
                    1,
                    a.anchor
                        .source
                        .as_str()
                        .len()
                        .min(b.anchor.source.as_str().len()),
                    0,
                )?;
                a == b
            }
            _ => false,
        })
    }
    fn release(&self, budget: &mut CompileTimeBudget<'_>) {
        if let Self::Enum(key) = self {
            budget.release(size_of::<DefinitionKey>());
            budget.release(key.anchor.source.as_str().len());
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn array_layout<'a>(
    state: &'a CompileTimeState,
    site: &BindingSite,
    dimensions: &[IndexExpr],
    element: &TypeExpr,
    open: bool,
    settings: &LayoutSettings,
    visited: &mut Vec<&'a DefinitionKey>,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Fact<StorageLayout>, IncompleteReason> {
    if open {
        return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
    }
    if dimensions.is_empty() {
        // The descriptor retains its element independently; SizeOf never queries
        // the heap elements to establish the variable's reference storage.
        if !element_valid(state, site, element, visited, depth + 1, budget)? {
            return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
        }
        return Ok(builtin_layout(
            BuiltinType::DynamicArrayReference,
            &state.context,
            settings,
        ));
    }
    let count = match array_count(state, site, dimensions, depth, budget)? {
        Fact::Known(count) => count,
        Fact::Unknown(reason) => return Ok(Fact::Unknown(reason)),
        Fact::Incomplete(reason) => return Ok(Fact::Incomplete(reason)),
    };
    match storage_type(state, site, element, settings, visited, depth + 1, budget)? {
        Fact::Known(element) => {
            budget.charge(1, 0, 0)?;
            Ok(element.size.checked_mul(count).map_or(
                Fact::Unknown(UnknownReason::Overflow),
                |size| {
                    Fact::Known(StorageLayout {
                        size,
                        alignment: element.alignment,
                    })
                },
            ))
        }
        Fact::Unknown(r) => Ok(Fact::Unknown(r)),
        Fact::Incomplete(r) => Ok(Fact::Incomplete(r)),
    }
}

/// Cardinality proof is shared by storage and element-type validity. Neither
/// may accept overflowing bounds/products merely because stride is not needed.
fn array_count(
    state: &CompileTimeState,
    site: &BindingSite,
    dimensions: &[IndexExpr],
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Fact<u64>, IncompleteReason> {
    limit(
        dimensions.len(),
        budget.limits().max_array_dimensions,
        "max_array_dimensions",
    )?;
    let mut count = 1u64;
    for dimension in dimensions {
        budget.charge(1, 0, 0)?;
        let Some((lower, upper)) = index_domain(state, site, dimension, depth + 1, budget)? else {
            return Ok(Fact::Unknown(UnknownReason::UnsupportedLayout));
        };
        if upper < lower {
            return Ok(Fact::Unknown(UnknownReason::UnsupportedLayout));
        }
        let n = upper
            .checked_sub(lower)
            .and_then(|n| n.checked_add(1))
            .and_then(|n| u64::try_from(n).ok());
        let Some(product) = n.and_then(|n| count.checked_mul(n)) else {
            return Ok(Fact::Unknown(UnknownReason::Overflow));
        };
        count = product;
    }
    Ok(Fact::Known(count))
}

/// Establish a valid element binding/shape, not its size or record stride.
pub(super) fn element_valid<'a>(
    state: &'a CompileTimeState,
    site: &BindingSite,
    ty: &TypeExpr,
    visited: &mut Vec<&'a DefinitionKey>,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    budget.charge(1, 0, 0)?;
    match ty {
        TypeExpr::Named(name) | TypeExpr::StrongAlias(name) => {
            let Some(binding) = lookup(state, site, name, budget)? else {
                return Ok(intrinsic(name).is_some());
            };
            let Some(decl) = binding.type_declaration() else {
                return Ok(false);
            };
            for key in visited.iter() {
                budget.charge(
                    1,
                    key.anchor
                        .source
                        .as_str()
                        .len()
                        .min(decl.key.anchor.source.as_str().len()),
                    0,
                )?;
                if *key == &decl.key {
                    return Ok(false);
                }
            }
            budget.charge(1, 0, size_of::<&DefinitionKey>())?;
            visited.push(&decl.key);
            let result = element_valid(state, &decl.site, &decl.ty, visited, depth + 1, budget);
            visited.pop();
            budget.release(size_of::<&DefinitionKey>());
            result
        }
        TypeExpr::OpaqueNominal(_) | TypeExpr::Pointer(_) => Ok(true),
        TypeExpr::Record(record) => {
            super::records::record_element_valid(state, record, visited, depth + 1, budget)
        }
        TypeExpr::Array {
            dimensions,
            element,
            open,
            ..
        } => {
            if *open {
                return Ok(false);
            }
            if !dimensions.is_empty()
                && !matches!(
                    array_count(state, site, dimensions, depth, budget)?,
                    Fact::Known(_)
                )
            {
                return Ok(false);
            }
            element_valid(state, site, element, visited, depth + 1, budget)
        }
        TypeExpr::Range(_) | TypeExpr::Enum(_) => {
            Ok(type_domain(state, site, ty, depth + 1, budget)?
                .is_some_and(|(lower, upper)| upper >= lower))
        }
        TypeExpr::Unsupported(_) => Ok(false),
    }
}

/// Char is AnsiChar before Delphi 2009, and a UTF-16 WideChar afterwards.
/// Reuse verified target/compiler profiles. A supplementary scalar occupies two
/// code units, not one Char. Legacy non-ASCII bytes need code-page evidence.
fn character_ordinal(
    state: &CompileTimeState,
    text: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<(i64, OrdinalKind)>, IncompleteReason> {
    budget.charge(1, text.len(), 0)?;
    if !matches!(
        state.context.layout.platform,
        Some(LayoutPlatform::Win32 | LayoutPlatform::Win64)
    ) {
        return Ok(None);
    }
    let Some(storage) = builtin_layout(
        BuiltinType::Char,
        &state.context,
        &state.context.layout.defaults,
    )
    .known() else {
        return Ok(None);
    };
    Ok(match storage.size {
        1 => match text.as_bytes() {
            [value] if value.is_ascii() => Some((
                i64::from(*value),
                OrdinalKind::Character(BuiltinType::AnsiChar),
            )),
            _ => None,
        },
        2 => {
            let mut units = text.encode_utf16();
            match (units.next(), units.next()) {
                (Some(value), None) => Some((
                    i64::from(value),
                    OrdinalKind::Character(BuiltinType::WideChar),
                )),
                _ => None,
            }
        }
        _ => None,
    })
}

pub(super) fn ordinal_layout(
    state: &CompileTimeState,
    site: &BindingSite,
    ty: &TypeExpr,
    settings: &LayoutSettings,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Fact<StorageLayout>, IncompleteReason> {
    let Some((lower, upper)) = type_domain(state, site, ty, depth + 1, budget)? else {
        return Ok(Fact::Unknown(UnknownReason::UnsupportedLayout));
    };
    if upper < lower {
        return Ok(Fact::Unknown(UnknownReason::UnsupportedLayout));
    }
    let enum_based = match ty {
        TypeExpr::Enum(_) => true,
        TypeExpr::Range(IndexExpr::Bounds { lower, .. }) => {
            let value = ordinal_expression(state, site, lower, depth + 1, budget)?;
            let enum_based = value
                .as_ref()
                .is_some_and(|(_, kind)| matches!(kind, OrdinalKind::Enum(_)));
            if let Some((_, kind)) = value {
                kind.release(budget);
            }
            enum_based
        }
        _ => false,
    };
    let builtin = if enum_based {
        let Some(minimum @ (1 | 2 | 4)) = settings.minimum_enum_size else {
            return Ok(Fact::Unknown(UnknownReason::UnsupportedLayout));
        };
        if lower < 0 || upper > i64::from(i32::MAX) {
            return Ok(Fact::Unknown(UnknownReason::UnsupportedLayout));
        }
        match (minimum, upper) {
            (1, 0..=255) => BuiltinType::Byte,
            (1 | 2, 0..=65535) => BuiltinType::Word,
            _ => BuiltinType::Integer,
        }
    } else if lower >= 0 && upper <= 255 {
        BuiltinType::Byte
    } else if lower >= -128 && upper <= 127 {
        BuiltinType::ShortInt
    } else if lower >= 0 && upper <= 65535 {
        BuiltinType::Word
    } else if lower >= -32768 && upper <= 32767 {
        BuiltinType::SmallInt
    } else if lower >= i64::from(i32::MIN) && upper <= i64::from(i32::MAX) {
        BuiltinType::Integer
    } else {
        return Ok(Fact::Unknown(UnknownReason::UnsupportedLayout));
    };
    Ok(builtin_layout(builtin, &state.context, settings))
}

fn index_domain(
    state: &CompileTimeState,
    site: &BindingSite,
    index: &IndexExpr,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<Domain>, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    match index {
        IndexExpr::Bounds { lower, upper } => {
            let lower = ordinal_expression(state, site, lower, depth + 1, budget)?;
            let result = (|| {
                let upper = ordinal_expression(state, site, upper, depth + 1, budget)?;
                let result = match (&lower, &upper) {
                    (Some((lower, lower_kind)), Some((upper, upper_kind))) => lower_kind
                        .compatible(upper_kind, budget)
                        .map(|valid| valid.then_some((*lower, *upper))),
                    _ => Ok(None),
                };
                if let Some((_, kind)) = upper {
                    kind.release(budget);
                }
                result
            })();
            if let Some((_, kind)) = lower {
                kind.release(budget);
            }
            result
        }
        IndexExpr::Domain(name) => {
            if let Some(binding) = lookup(state, site, name, budget)? {
                let Some(decl) = binding.type_declaration() else {
                    return Ok(None);
                };
                return type_domain(state, &decl.site, &decl.ty, depth + 1, budget);
            }
            budget.charge(1, name.len(), name.len())?;
            let canonical = name.to_ascii_lowercase();
            let builtin = intrinsic(&canonical);
            budget.release(name.len());
            let domain = match builtin {
                Some(BuiltinType::Boolean) => (0, 1),
                Some(BuiltinType::Byte | BuiltinType::AnsiChar) => (0, 255),
                Some(BuiltinType::ShortInt) => (-128, 127),
                Some(BuiltinType::Word | BuiltinType::WideChar) => (0, 65535),
                Some(BuiltinType::SmallInt) => (-32768, 32767),
                Some(BuiltinType::Integer | BuiltinType::LongInt) => {
                    (i64::from(i32::MIN), i64::from(i32::MAX))
                }
                Some(BuiltinType::Cardinal | BuiltinType::LongWord) => (0, i64::from(u32::MAX)),
                Some(BuiltinType::Int64) => (i64::MIN, i64::MAX),
                Some(BuiltinType::Char | BuiltinType::NativeInt | BuiltinType::NativeUInt) => {
                    let ty = builtin.unwrap();
                    let Some(storage) =
                        builtin_layout(ty, &state.context, &state.context.layout.defaults).known()
                    else {
                        return Ok(None);
                    };
                    match (ty, storage.size) {
                        (BuiltinType::Char, 1) => (0, 255),
                        (BuiltinType::Char, 2) => (0, 65535),
                        (BuiltinType::NativeInt, 4) => (i64::from(i32::MIN), i64::from(i32::MAX)),
                        (BuiltinType::NativeInt, 8) => (i64::MIN, i64::MAX),
                        (BuiltinType::NativeUInt, 4) => (0, i64::from(u32::MAX)),
                        _ => return Ok(None),
                    }
                }
                _ => return Ok(None),
            };
            Ok(Some(domain))
        }
    }
}

pub(super) fn type_domain(
    state: &CompileTimeState,
    site: &BindingSite,
    ty: &TypeExpr,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<Domain>, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    budget.charge(1, 0, 0)?;
    match ty {
        TypeExpr::Named(name) | TypeExpr::StrongAlias(name) => {
            index_domain_name(state, site, name, depth + 1, budget)
        }
        TypeExpr::Range(index) => index_domain(state, site, index, depth + 1, budget),
        TypeExpr::Enum(members) => {
            enum_values(state, site, members, members.len(), depth + 1, budget)
        }
        _ => Ok(None),
    }
}

fn index_domain_name(
    state: &CompileTimeState,
    site: &BindingSite,
    name: &str,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<Domain>, IncompleteReason> {
    // Resolve aliases at their definition sites, independently of storage width.
    if let Some(binding) = lookup(state, site, name, budget)? {
        let Some(decl) = binding.type_declaration() else {
            return Ok(None);
        };
        type_domain(state, &decl.site, &decl.ty, depth + 1, budget)
    } else {
        budget.charge(1, name.len(), size_of::<IndexExpr>() + name.len())?;
        let index = IndexExpr::Domain(name.to_owned());
        let result = index_domain(state, site, &index, depth + 1, budget);
        budget.release(size_of::<IndexExpr>() + name.len());
        result
    }
}

fn ordinal_expression(
    state: &CompileTimeState,
    site: &BindingSite,
    text: &str,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<(i64, OrdinalKind)>, IncompleteReason> {
    let value = constant_expression(state, site, text, depth, budget)?;
    Ok(match value {
        Some(BoundValue::Constant(ConstantValue::Integer(n))) => Some((n, OrdinalKind::Integer)),
        Some(BoundValue::EnumOrdinal(n, identity)) => Some((n, OrdinalKind::Enum(identity))),
        Some(BoundValue::Constant(ConstantValue::Boolean(b))) => {
            Some((i64::from(b), OrdinalKind::Boolean))
        }
        Some(BoundValue::Constant(ConstantValue::String(s))) => {
            let result = character_ordinal(state, &s, budget);
            budget.release(s.len());
            result?
        }
        _ => None,
    })
}

/// Shared typed ordinal admission for record labels. Resolve nominal enum
/// identity through definition-site aliases; never compare enum values alone.
fn selector_kind(
    state: &CompileTimeState,
    site: &BindingSite,
    ty: &TypeExpr,
    owner: Option<&DefinitionKey>,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<OrdinalKind>, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    budget.charge(1, 0, 0)?;
    match ty {
        TypeExpr::Named(name) | TypeExpr::StrongAlias(name) => {
            if let Some(binding) = lookup(state, site, name, budget)? {
                let Some(decl) = binding.type_declaration() else {
                    return Ok(None);
                };
                return selector_kind(
                    state,
                    &decl.site,
                    &decl.ty,
                    Some(&decl.key),
                    depth + 1,
                    budget,
                );
            }
            Ok(match intrinsic(name) {
                Some(BuiltinType::Boolean) => Some(OrdinalKind::Boolean),
                Some(BuiltinType::AnsiChar) => Some(OrdinalKind::Character(BuiltinType::AnsiChar)),
                Some(BuiltinType::WideChar) => Some(OrdinalKind::Character(BuiltinType::WideChar)),
                Some(BuiltinType::Char) => builtin_layout(
                    BuiltinType::Char,
                    &state.context,
                    &state.context.layout.defaults,
                )
                .known()
                .and_then(|s| match s.size {
                    1 => Some(OrdinalKind::Character(BuiltinType::AnsiChar)),
                    2 => Some(OrdinalKind::Character(BuiltinType::WideChar)),
                    _ => None,
                }),
                // The separate domain admission determines which integer intrinsics
                // are actually supported; this does not invent their bounds.
                Some(_) => Some(OrdinalKind::Integer),
                None => None,
            })
        }
        TypeExpr::Enum(_) => {
            let Some(key) = owner else {
                return Ok(None);
            };
            budget.charge(
                1,
                key.anchor.source.as_str().len(),
                size_of::<DefinitionKey>() + key.anchor.source.as_str().len(),
            )?;
            Ok(Some(OrdinalKind::Enum(key.clone())))
        }
        TypeExpr::Range(IndexExpr::Bounds { lower, .. }) => {
            Ok(ordinal_expression(state, site, lower, depth + 1, budget)?.map(|(_, kind)| kind))
        }
        _ => Ok(None),
    }
}

fn label_interval(
    state: &CompileTimeState,
    site: &BindingSite,
    label: &IndexExpr,
    expected: &OrdinalKind,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<Domain>, IncompleteReason> {
    let (lower, upper) = match label {
        IndexExpr::Domain(text) => (text.as_str(), None),
        IndexExpr::Bounds { lower, upper } => (lower.as_str(), Some(upper.as_str())),
    };
    let Some((lower, kind)) = ordinal_expression(state, site, lower, depth + 1, budget)? else {
        return Ok(None);
    };
    let result = (|| {
        if !expected.compatible(&kind, budget)? {
            return Ok(None);
        }
        let Some(text) = upper else {
            return Ok(Some((lower, lower)));
        };
        let Some((upper, upper_kind)) = ordinal_expression(state, site, text, depth + 1, budget)?
        else {
            return Ok(None);
        };
        let result = expected
            .compatible(&upper_kind, budget)
            .map(|valid| (valid && lower <= upper).then_some((lower, upper)));
        upper_kind.release(budget);
        result
    })();
    kind.release(budget);
    result
}

pub(super) fn validate_variant_labels<'a>(
    state: &CompileTimeState,
    site: &BindingSite,
    selector: &TypeExpr,
    labels: impl Iterator<Item = &'a IndexExpr>,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    let Some((minimum, maximum)) = type_domain(state, site, selector, depth + 1, budget)? else {
        return Ok(false);
    };
    if minimum > maximum {
        return Ok(false);
    }
    let Some(kind) = selector_kind(state, site, selector, None, depth + 1, budget)? else {
        return Ok(false);
    };
    let mut intervals: Vec<Domain> = Vec::new();
    let result = (|| {
        for label in labels {
            budget.charge(1, 0, 0)?;
            let Some((lower, upper)) =
                label_interval(state, site, label, &kind, depth + 1, budget)?
            else {
                return Ok(false);
            };
            if lower < minimum || upper > maximum {
                return Ok(false);
            }
            for (a, b) in &intervals {
                budget.charge(1, 0, 0)?;
                if lower <= *b && *a <= upper {
                    return Ok(false);
                }
            }
            // Retain intervals, never enumerate values of a possibly huge range.
            budget.charge(1, 0, size_of::<Domain>())?;
            intervals.push((lower, upper));
        }
        Ok(!intervals.is_empty())
    })();
    kind.release(budget);
    budget.release(intervals.len() * size_of::<Domain>());
    result
}

fn constant_expression(
    state: &CompileTimeState,
    site: &BindingSite,
    text: &str,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<BoundValue>, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    evaluate_bound_expression(text, budget, &mut |name, budget| {
        constant_name(state, site, name, depth + 1, budget)
    })
}

fn constant_name(
    state: &CompileTimeState,
    site: &BindingSite,
    name: &str,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<BoundValue>, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    if let Some(binding) = lookup(state, site, name, budget)? {
        match binding {
            Binding::Declaration(decl) => {
                if !decl.is_constant {
                    return Ok(None);
                }
                let prior = BindingSite {
                    scope: decl.site.scope,
                    sequence: Sequence(
                        decl.site
                            .sequence
                            .0
                            .checked_sub(1)
                            .ok_or(IncompleteReason::MalformedSource)?,
                    ),
                };
                return decl
                    .initializer
                    .as_deref()
                    .map(|s| {
                        budget.charge(
                            1,
                            s.len()
                                .checked_mul(3)
                                .ok_or(IncompleteReason::MalformedSource)?,
                            0,
                        )?;
                        constant_expression(
                            state,
                            &prior,
                            initializer_expression(s),
                            depth + 1,
                            budget,
                        )
                    })
                    .unwrap_or(Ok(None));
            }
            Binding::EnumMember { owner, index } => {
                let TypeExpr::Enum(members) = &owner.ty else {
                    return Err(IncompleteReason::MalformedSource);
                };
                // Preserve a member's nominal kind only after its complete
                // owning declaration establishes a valid domain, not its prefix.
                if enum_values(
                    state,
                    &owner.site,
                    members,
                    members.len(),
                    depth + 1,
                    budget,
                )?
                .is_none()
                {
                    return Ok(None);
                }
                let Some((_, value)) =
                    enum_values(state, &owner.site, members, index + 1, depth + 1, budget)?
                else {
                    return Ok(None);
                };
                let bytes = size_of::<DefinitionKey>()
                    .checked_add(owner.key.anchor.source.as_str().len())
                    .ok_or(IncompleteReason::MalformedSource)?;
                budget.charge(1, owner.key.anchor.source.as_str().len(), bytes)?;
                return Ok(Some(BoundValue::EnumOrdinal(value, owner.key.clone())));
            }
        }
    }
    // Immutable explicit context facts are admitted only after lexical blockers.
    let mut found = None;
    for (key, value) in &state.context.constants {
        budget.charge(1, key.len().min(name.len()), 0)?;
        if same_name(key, name) {
            if let Some(previous) = found {
                let bytes = match value {
                    ConstantValue::String(s) => s.len(),
                    _ => 0,
                };
                budget.charge(1, bytes, 0)?;
                if previous != value {
                    return Ok(None);
                }
            }
            found = Some(value);
        }
    }
    let Some(value) = found else {
        return Ok(None);
    };
    let bytes = match value {
        ConstantValue::String(s) => s.len(),
        _ => 0,
    };
    let byte_work = bytes
        .checked_mul(MAX_EXPRESSION_TOKENS)
        .ok_or(IncompleteReason::MalformedSource)?;
    budget.charge(1, byte_work, bytes)?;
    Ok(Some(BoundValue::Constant(value.clone())))
}

fn enum_values(
    state: &CompileTimeState,
    site: &BindingSite,
    members: &[EnumMember],
    take: usize,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<Domain>, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    let bytes = take
        .checked_mul(size_of::<i64>())
        .ok_or(IncompleteReason::MalformedSource)?;
    budget.charge(take, 0, bytes)?;
    let mut values: Vec<i64> = Vec::with_capacity(take);
    let result = (|| {
        for member in &members[..take] {
            let n = if let Some(text) = &member.value {
                let value = evaluate_bound_expression(text, budget, &mut |name, budget| {
                    for (previous, value) in members.iter().zip(&values) {
                        budget.charge(1, previous.name.len().min(name.len()), 0)?;
                        if same_name(&previous.name, name) {
                            return Ok(Some(BoundValue::Constant(ConstantValue::Integer(*value))));
                        }
                    }
                    let prior = BindingSite {
                        scope: site.scope,
                        sequence: Sequence(site.sequence.0 - 1),
                    };
                    constant_name(state, &prior, name, depth + 1, budget)
                })?;
                match value {
                    Some(BoundValue::Constant(ConstantValue::Integer(n))) => n,
                    Some(BoundValue::EnumOrdinal(_, identity)) => {
                        OrdinalKind::Enum(identity).release(budget);
                        return Ok(false);
                    }
                    Some(BoundValue::Constant(ConstantValue::String(s))) => {
                        budget.release(s.len());
                        return Ok(false);
                    }
                    _ => return Ok(false),
                }
            } else {
                let Some(n) = values.last().map_or(Some(0), |n| n.checked_add(1)) else {
                    return Ok(false);
                };
                n
            };
            if n < 0
                || n > i64::from(i32::MAX)
                || values.last().is_some_and(|previous| n <= *previous)
            {
                return Ok(false);
            }
            values.push(n);
        }
        Ok(true)
    })();
    // These values are query scratch only; callers consume them immediately.
    budget.release(bytes);
    result.map(|valid| {
        if valid {
            values.first().copied().zip(values.last().copied())
        } else {
            None
        }
    })
}
