//! Lexical binding is separate from syntax admission and ABI layout.
use super::declarations::limit;
use super::*;
use pascal_project::LayoutSettings;
use std::mem::size_of;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TypeExpr {
    Named(String),
    StrongAlias(String),
    Pointer(String),
    Array {
        dimensions: Vec<IndexExpr>,
        element: Box<TypeExpr>,
        open: bool,
        packed: bool,
    },
    Range(IndexExpr),
    Enum(Vec<EnumMember>),
    /// Complete parser-validated syntax retained for later composite owners.
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum IndexExpr {
    Domain(String),
    Bounds { lower: String, upper: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct EnumMember {
    pub name: String,
    pub value: Option<String>,
    pub key: DefinitionKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Declaration {
    pub name: String,
    pub key: DefinitionKey,
    pub site: BindingSite,
    pub ty: TypeExpr,
    pub is_type: bool,
    pub is_constant: bool,
    pub settings: LayoutSettings,
    pub provenance: Vec<SourceAnchor>,
    /// Unevaluated initializer for the later bounded constant-expression owner.
    pub initializer: Option<String>,
}

pub(crate) fn intrinsic(name: &str) -> Option<BuiltinType> {
    use BuiltinType::*;
    // Only the compiler's unqualified intrinsic namespace is admitted here.
    // Qualified names require provider/binding evidence, not suffix matching.
    Some(match name {
        "byte" => Byte,
        "shortint" => ShortInt,
        "ansichar" => AnsiChar,
        "boolean" => Boolean,
        "word" => Word,
        "smallint" => SmallInt,
        "widechar" => WideChar,
        "integer" => Integer,
        "longint" => LongInt,
        "cardinal" => Cardinal,
        "longword" => LongWord,
        "single" => Single,
        "int64" => Int64,
        "uint64" => UInt64,
        "double" => Double,
        "currency" => Currency,
        "pointer" => Pointer,
        "nativeint" => NativeInt,
        "nativeuint" => NativeUInt,
        "extended" => Extended,
        "char" => Char,
        "string" => String,
        "shortstring" => ShortString,
        "bytebool" => ByteBool,
        "wordbool" => WordBool,
        "longbool" => LongBool,
        "real" => Real,
        "real48" => Real48,
        "ansistring" => AnsiString,
        "unicodestring" => UnicodeString,
        "widestring" => WideString,
        _ => return None,
    })
}

pub(super) fn layout_name(
    state: &CompileTimeState,
    site: BindingSite,
    name: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Fact<KnownLayout> {
    if let Err(reason) = budget.charge(1, 0, 0) {
        return Fact::Incomplete(reason);
    }
    if let Some(reason) = &state.failure {
        return Fact::Incomplete(reason.clone());
    }
    if state.unknown {
        return Fact::Unknown(UnknownReason::UnknownActivity);
    }
    if site.scope.0 as usize >= state.scopes.len() || site.sequence.0 > state.sequence.0 {
        return Fact::Unknown(UnknownReason::UnresolvedName);
    }
    if let Err(reason) = budget.charge(1, name.len(), name.len()) {
        return Fact::Incomplete(reason);
    }
    let name = name.to_ascii_lowercase();
    let mut identity_bytes = 0;
    let result = (|| {
        let decl = lookup(state, &site, &name, budget)?;
        let identity = if let Some(decl) = decl {
            if !decl.is_type {
                return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
            }
            let bytes = size_of::<TypeIdentity>() + decl.key.anchor.source.as_str().len();
            budget.charge(1, decl.key.anchor.source.as_str().len(), bytes)?;
            identity_bytes = bytes;
            TypeIdentity::Declared(decl.key.clone())
        } else {
            let bytes = size_of::<TypeIdentity>() + name.len();
            budget.charge(1, name.len(), bytes)?;
            identity_bytes = bytes;
            TypeIdentity::Intrinsic(name.clone())
        };
        let storage = storage_name(
            state,
            &site,
            &name,
            &state.context.layout.defaults,
            &mut Vec::new(),
            0,
            budget,
        )?;
        Ok(make_layout(storage, identity))
    })();
    budget.release(name.len());
    if !matches!(result, Ok(Fact::Known(_))) {
        budget.release(identity_bytes);
    }
    result.unwrap_or_else(Fact::Incomplete)
}

pub(super) fn lookup<'a>(
    state: &'a CompileTimeState,
    site: &BindingSite,
    name: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<&'a Declaration>, IncompleteReason> {
    let mut scope = Some(site.scope);
    while let Some(id) = scope {
        for decl in state.declarations.iter().rev() {
            budget.charge(1, 0, 0)?;
            if decl.site.scope == id && decl.site.sequence.0 <= site.sequence.0 {
                budget.charge(0, decl.name.len().min(name.len()), 0)?;
                if decl.name.eq_ignore_ascii_case(name) {
                    return Ok(Some(decl));
                }
            }
        }
        scope = state.scopes[id.0 as usize].parent;
    }
    Ok(None)
}

pub(super) fn storage_name<'a>(
    state: &'a CompileTimeState,
    site: &BindingSite,
    name: &str,
    settings: &LayoutSettings,
    visited: &mut Vec<&'a DefinitionKey>,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Fact<StorageLayout>, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    let Some(decl) = lookup(state, site, name, budget)? else {
        return Ok(
            intrinsic(name).map_or(Fact::Unknown(UnknownReason::UnresolvedName), |ty| {
                builtin_layout(ty, &state.context, settings)
            }),
        );
    };
    if !decl.is_type {
        return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
    }
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
            return Ok(Fact::Unknown(UnknownReason::CyclicType));
        }
    }
    budget.charge(1, 0, size_of::<&DefinitionKey>())?;
    visited.push(&decl.key);
    let result = storage_type(
        state,
        &decl.site,
        &decl.ty,
        &decl.settings,
        visited,
        depth + 1,
        budget,
    );
    visited.pop();
    budget.release(size_of::<&DefinitionKey>());
    result
}

pub(super) fn storage_type<'a>(
    state: &'a CompileTimeState,
    site: &BindingSite,
    ty: &TypeExpr,
    settings: &LayoutSettings,
    visited: &mut Vec<&'a DefinitionKey>,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Fact<StorageLayout>, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    budget.charge(1, 0, 0)?;
    match ty {
        TypeExpr::Named(name) | TypeExpr::StrongAlias(name) => {
            storage_name(state, site, name, settings, visited, depth, budget)
        }
        TypeExpr::Pointer(_) => Ok(builtin_layout(
            BuiltinType::Pointer,
            &state.context,
            settings,
        )),
        TypeExpr::Array {
            dimensions,
            element,
            open,
            packed: false,
        } => super::arrays::array_layout(
            state, site, dimensions, element, *open, settings, visited, depth, budget,
        ),
        TypeExpr::Array { .. } => Ok(Fact::Unknown(UnknownReason::UnsupportedLayout)),
        TypeExpr::Range(_) | TypeExpr::Enum(_) => {
            super::arrays::ordinal_layout(state, site, ty, settings, depth, budget)
        }
        TypeExpr::Unsupported(_) => Ok(Fact::Unknown(UnknownReason::UnsupportedLayout)),
    }
}

fn make_layout(fact: Fact<StorageLayout>, identity: TypeIdentity) -> Fact<KnownLayout> {
    match fact {
        Fact::Known(storage) => Fact::Known(KnownLayout {
            storage,
            identity,
            dependencies: Vec::new(),
        }),
        Fact::Unknown(r) => Fact::Unknown(r),
        Fact::Incomplete(r) => Fact::Incomplete(r),
    }
}
