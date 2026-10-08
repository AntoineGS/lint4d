//! Checked ordinal domains and array storage. A domain is not an enum's ABI width.
use super::bindings::{EnumMember, IndexExpr, TypeExpr, intrinsic, lookup, storage_type};
use super::declarations::{initializer_expression, limit};
use super::*;
use crate::conditional::{ConstantValue, MAX_EXPRESSION_TOKENS, evaluate_bound_expression};
use pascal_project::LayoutSettings;
use std::mem::size_of;

type Domain = (i64, i64);

enum OrdinalKind<'a> {
    Integer,
    Boolean,
    Enum(&'a DefinitionKey),
}
impl OrdinalKind<'_> {
    fn compatible(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Integer, Self::Integer) | (Self::Boolean, Self::Boolean) => true,
            (Self::Enum(a), Self::Enum(b)) => std::ptr::eq(*a, *b),
            _ => false,
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
        return Ok(builtin_layout(
            BuiltinType::DynamicArrayReference,
            &state.context,
            settings,
        ));
    }
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
            enum_owner(state, site, lower, depth + 1, budget)?.is_some()
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
            let upper = ordinal_expression(state, site, upper, depth + 1, budget)?;
            Ok(lower
                .zip(upper)
                .and_then(|((lower, lower_kind), (upper, upper_kind))| {
                    lower_kind.compatible(&upper_kind).then_some((lower, upper))
                }))
        }
        IndexExpr::Domain(name) => {
            if let Some(decl) = lookup(state, site, name, budget)? {
                if !decl.is_type {
                    return Ok(None);
                }
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

fn type_domain(
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
    if let Some(decl) = lookup(state, site, name, budget)? {
        if !decl.is_type {
            return Ok(None);
        }
        type_domain(state, &decl.site, &decl.ty, depth + 1, budget)
    } else {
        budget.charge(1, name.len(), size_of::<IndexExpr>() + name.len())?;
        let index = IndexExpr::Domain(name.to_owned());
        let result = index_domain(state, site, &index, depth + 1, budget);
        budget.release(size_of::<IndexExpr>() + name.len());
        result
    }
}

fn ordinal_expression<'a>(
    state: &'a CompileTimeState,
    site: &BindingSite,
    text: &str,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<(i64, OrdinalKind<'a>)>, IncompleteReason> {
    let value = constant_expression(state, site, text, depth, budget)?;
    Ok(match value {
        Some(ConstantValue::Integer(n)) => Some((
            n,
            enum_owner(state, site, text, depth + 1, budget)?
                .map_or(OrdinalKind::Integer, OrdinalKind::Enum),
        )),
        Some(ConstantValue::Boolean(b)) => Some((i64::from(b), OrdinalKind::Boolean)),
        Some(ConstantValue::String(s)) => {
            budget.release(s.len());
            None
        }
        _ => None,
    })
}

fn constant_expression(
    state: &CompileTimeState,
    site: &BindingSite,
    text: &str,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<ConstantValue>, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    evaluate_bound_expression(text, budget, &mut |name, budget| {
        // The existing evaluator carries integer values, not enum arithmetic
        // rules. Bare enum constants are supported with nominal identity;
        // arithmetic on them must remain unknown rather than erase that type.
        if !text.trim().eq_ignore_ascii_case(name)
            && enum_owner(state, site, name, depth + 1, budget)?.is_some()
        {
            return Ok(None);
        }
        constant_name(state, site, name, depth + 1, budget)
    })
}

fn enum_owner<'a>(
    state: &'a CompileTimeState,
    site: &BindingSite,
    name: &str,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<&'a DefinitionKey>, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    budget.charge(1, name.len(), 0)?;
    let name = name.trim();
    let mut scope = Some(site.scope);
    while let Some(id) = scope {
        for decl in state.declarations.iter().rev() {
            budget.charge(1, 0, 0)?;
            if decl.site.scope != id || decl.site.sequence.0 > site.sequence.0 {
                continue;
            }
            budget.charge(1, name.len().min(decl.name.len()), 0)?;
            if decl.name.eq_ignore_ascii_case(name) {
                if !decl.is_constant {
                    return Ok(None);
                }
                let prior = BindingSite {
                    scope: decl.site.scope,
                    sequence: Sequence(decl.site.sequence.0 - 1),
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
                        enum_owner(state, &prior, initializer_expression(s), depth + 1, budget)
                    })
                    .unwrap_or(Ok(None));
            }
            if let (true, TypeExpr::Enum(members)) = (decl.is_type, &decl.ty) {
                for member in members {
                    budget.charge(1, name.len().min(member.name.len()), 0)?;
                    if member.name.eq_ignore_ascii_case(name) {
                        return Ok(Some(&decl.key));
                    }
                }
            }
        }
        scope = state.scopes[id.0 as usize].parent;
    }
    Ok(None)
}

fn constant_name(
    state: &CompileTimeState,
    site: &BindingSite,
    name: &str,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<ConstantValue>, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    let mut scope = Some(site.scope);
    while let Some(id) = scope {
        for decl in state.declarations.iter().rev() {
            budget.charge(1, 0, 0)?;
            if decl.site.scope != id || decl.site.sequence.0 > site.sequence.0 {
                continue;
            }
            budget.charge(1, name.len().min(decl.name.len()), 0)?;
            if decl.name.eq_ignore_ascii_case(name) {
                if !decl.is_constant {
                    return Ok(None);
                }
                let prior = BindingSite {
                    scope: decl.site.scope,
                    sequence: Sequence(decl.site.sequence.0 - 1),
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
            if let (true, TypeExpr::Enum(members)) = (decl.is_type, &decl.ty) {
                for (index, member) in members.iter().enumerate() {
                    budget.charge(1, name.len().min(member.name.len()), 0)?;
                    if member.name.eq_ignore_ascii_case(name) {
                        return Ok(enum_values(
                            state,
                            &decl.site,
                            members,
                            index + 1,
                            depth + 1,
                            budget,
                        )?
                        .map(|(_, last)| ConstantValue::Integer(last)));
                    }
                }
            }
        }
        scope = state.scopes[id.0 as usize].parent;
    }
    // Immutable explicit context facts are admitted only after lexical blockers.
    let mut found = None;
    for (key, value) in &state.context.constants {
        budget.charge(1, key.len().min(name.len()), 0)?;
        if key.eq_ignore_ascii_case(name) {
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
    Ok(Some(value.clone()))
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
                        if previous.name.eq_ignore_ascii_case(name) {
                            return Ok(Some(ConstantValue::Integer(*value)));
                        }
                    }
                    let prior = BindingSite {
                        scope: site.scope,
                        sequence: Sequence(site.sequence.0 - 1),
                    };
                    constant_name(state, &prior, name, depth + 1, budget)
                })?;
                match value {
                    Some(ConstantValue::Integer(n)) => n,
                    Some(ConstantValue::String(s)) => {
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
