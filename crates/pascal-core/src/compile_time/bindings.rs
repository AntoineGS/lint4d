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
    /// Complete parser-validated syntax retained for later composite owners.
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Declaration {
    pub name: String,
    pub key: DefinitionKey,
    pub site: BindingSite,
    pub ty: TypeExpr,
    pub is_type: bool,
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
    let mut name = name.to_ascii_lowercase();
    let mut site = site;
    let mut identity = None;
    let mut identity_bytes = 0;
    let mut settings = state.context.layout.defaults.clone();
    let mut visited: Vec<&DefinitionKey> = Vec::new();
    let result = (|| {
        loop {
            limit(
                visited.len(),
                budget.limits().max_type_depth,
                "max_type_depth",
            )?;
            let mut scope = Some(site.scope);
            let mut found = None;
            while let Some(id) = scope {
                for decl in state.declarations.iter().rev() {
                    budget.charge(1, 0, 0)?;
                    if decl.site.scope == id && decl.site.sequence.0 <= site.sequence.0 {
                        budget.charge(0, decl.name.len().min(name.len()), 0)?;
                        if decl.name == name {
                            found = Some(decl);
                            break;
                        }
                    }
                }
                if found.is_some() {
                    break;
                }
                scope = state.scopes[id.0 as usize].parent;
            }
            let Some(decl) = found else {
                let Some(ty) = intrinsic(&name) else {
                    return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
                };
                let storage = builtin_layout(ty, &state.context, &settings);
                if identity.is_none() {
                    let bytes = size_of::<TypeIdentity>() + name.len();
                    budget.charge(1, name.len(), bytes)?;
                    identity_bytes = bytes;
                    identity = Some(TypeIdentity::Intrinsic(name.clone()));
                }
                return Ok(make_layout(storage, identity.take().unwrap()));
            };
            if !decl.is_type {
                return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
            }
            if visited.contains(&&decl.key) {
                return Ok(Fact::Unknown(UnknownReason::CyclicType));
            }
            budget.charge(1, 0, size_of::<&DefinitionKey>())?;
            visited.push(&decl.key);
            if identity.is_none() {
                let bytes = size_of::<TypeIdentity>() + decl.key.anchor.source.as_str().len();
                budget.charge(1, decl.key.anchor.source.as_str().len(), bytes)?;
                identity_bytes = bytes;
                identity = Some(TypeIdentity::Declared(decl.key.clone()));
            }
            match &decl.ty {
                TypeExpr::Named(target) | TypeExpr::StrongAlias(target) => {
                    budget.charge(1, target.len(), target.len())?;
                    budget.release(name.len());
                    name = target.to_ascii_lowercase();
                    site = decl.site.clone();
                    settings = decl.settings.clone();
                }
                TypeExpr::Pointer(_) => {
                    return Ok(make_layout(
                        builtin_layout(BuiltinType::Pointer, &state.context, &decl.settings),
                        identity.take().unwrap(),
                    ));
                }
                TypeExpr::Unsupported(_) => {
                    return Ok(Fact::Unknown(UnknownReason::UnsupportedLayout));
                }
            }
        }
    })();
    budget.release(name.len() + visited.len() * size_of::<&DefinitionKey>());
    if !matches!(result, Ok(Fact::Known(_))) {
        budget.release(identity_bytes);
    }
    result.unwrap_or_else(Fact::Incomplete)
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
