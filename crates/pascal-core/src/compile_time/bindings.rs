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
    Record(super::records::Record),
    /// Parser-validated nominal record/class/interface binding, not a layout.
    OpaqueNominal(String),
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
    pub value_logical: Option<usize>,
    pub key: DefinitionKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Declaration {
    pub name: String,
    pub key: DefinitionKey,
    pub site: BindingSite,
    /// Type names bind before a grouped value declaration admits its names.
    pub type_site: BindingSite,
    pub ty: TypeExpr,
    pub is_type: bool,
    pub exported: bool,
    pub is_constant: bool,
    /// Routine result syntax is not a variable's declared static type.
    pub is_routine: bool,
    pub settings: LayoutSettings,
    pub provenance: Vec<SourceAnchor>,
    /// Unevaluated initializer for the later bounded constant-expression owner.
    pub initializer: Option<String>,
    /// Logical start of the original defaultValue node, never a parser-wrapper
    /// offset. Its physical segments live in the state's occurrence span map.
    pub initializer_logical: Option<usize>,
}

/// A provider's graph is retained in its own binding namespace. Declaration
/// keys, captured sites, scalar history and settings are never rewritten as
/// importer-local names. Only proven interface roots are exposed.
#[derive(Debug, Clone)]
pub(super) struct InterfaceFacts {
    pub qualifiers: Vec<String>,
    pub site: BindingSite,
    pub graph: Option<std::sync::Arc<CompileTimeState>>,
}

impl InterfaceFacts {
    pub(super) fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.qualifiers.hash(&mut h);
        self.site.hash(&mut h);
        self.graph.as_ref().map(|g| g.fingerprint()).hash(&mut h);
        h.finish()
    }
    pub(super) fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        visit(size_of::<Self>() + self.qualifiers.len() * size_of::<String>())?;
        for qualifier in &self.qualifiers {
            visit(qualifier.len())?;
        }
        if let Some(graph) = &self.graph {
            graph.visit_recovery_payload(visit)?;
        }
        Ok(())
    }
}

pub(super) fn imported_operand(
    state: &CompileTimeState,
    site: &BindingSite,
    text: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<Fact<KnownLayout>>, IncompleteReason> {
    imported_operand_with_query(state, site, text, state, site, budget)
}

pub(super) fn imported_declaration<'a>(
    state: &'a CompileTimeState,
    site: &BindingSite,
    name: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Fact<(&'a CompileTimeState, &'a Declaration)>, IncompleteReason> {
    let root = name.split('.').next().unwrap_or(name);
    if name.contains('.') && lookup(state, site, root, budget)?.is_some() {
        return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
    }
    let mut result = None;
    for interface in &state.interfaces {
        budget.charge(1, 0, 0)?;
        if interface.site.sequence.0 > site.sequence.0 {
            continue;
        }
        let mut suffix = name;
        let mut qualified = false;
        for qualifier in &interface.qualifiers {
            budget.charge(1, qualifier.len().min(name.len()), 0)?;
            if name.len() > qualifier.len()
                && name.as_bytes().get(qualifier.len()) == Some(&b'.')
                && name
                    .get(..qualifier.len())
                    .is_some_and(|p| same_name(p, qualifier))
            {
                suffix = &name[qualifier.len() + 1..];
                qualified = true;
                break;
            }
        }
        let Some(graph) = &interface.graph else {
            if qualified || !name.contains('.') {
                return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
            }
            continue;
        };
        if !graph.alternatives.is_empty() || graph.failure.is_some() || graph.unknown {
            return Ok(Fact::Unknown(UnknownReason::UnknownActivity));
        }
        let Some(Binding::Declaration(decl)) = lookup(graph, &graph.site(), suffix, budget)? else {
            continue;
        };
        if !decl.exported {
            continue;
        }
        if result.is_some() {
            return Ok(Fact::Unknown(UnknownReason::AmbiguousBinding));
        }
        result = Some((graph.as_ref(), decl));
    }
    Ok(result.map_or(Fact::Unknown(UnknownReason::UnresolvedName), Fact::Known))
}

pub(super) fn imported_operand_with_query(
    state: &CompileTimeState,
    site: &BindingSite,
    text: &str,
    query_state: &CompileTimeState,
    query_site: &BindingSite,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<Fact<KnownLayout>>, IncompleteReason> {
    let root = text.split(['.', '[', '^']).next().unwrap_or(text).trim();
    if text.contains('.') && lookup(state, site, root, budget)?.is_some() {
        return Ok(Some(Fact::Unknown(UnknownReason::UnresolvedName)));
    }
    let mut result = None;
    for interface in &state.interfaces {
        budget.charge(1, 0, 0)?;
        if interface.site.sequence.0 > site.sequence.0 {
            continue;
        }
        let mut suffix = text;
        let mut qualified = false;
        for qualifier in &interface.qualifiers {
            budget.charge(1, qualifier.len().min(text.len()), 0)?;
            if text.len() > qualifier.len()
                && text.as_bytes().get(qualifier.len()) == Some(&b'.')
                && text
                    .get(..qualifier.len())
                    .is_some_and(|p| same_name(p, qualifier))
            {
                suffix = &text[qualifier.len() + 1..];
                qualified = true;
                break;
            }
        }
        let root = suffix
            .split(['.', '[', '^'])
            .next()
            .unwrap_or(suffix)
            .trim();
        let Some(graph) = &interface.graph else {
            if qualified || !text.contains('.') {
                return Ok(Some(Fact::Unknown(UnknownReason::UnresolvedName)));
            }
            continue;
        };
        if !graph.alternatives.is_empty() || graph.failure.is_some() || graph.unknown {
            return Ok(Some(Fact::Unknown(UnknownReason::UnknownActivity)));
        }
        let provider_site = graph.site();
        let exported = match lookup(graph, &provider_site, root, budget)? {
            Some(Binding::Declaration(d)) => d.exported,
            Some(Binding::EnumMember { owner, .. }) => owner.exported,
            None => false,
        };
        if !exported {
            if qualified {
                return Ok(Some(Fact::Unknown(UnknownReason::UnresolvedName)));
            }
            continue;
        }
        if result.is_some() {
            return Ok(Some(Fact::Unknown(UnknownReason::AmbiguousBinding)));
        }
        let before = budget.snapshot().retained_bytes;
        let fact = match parse_sizeof_operand(suffix, budget) {
            Fact::Known(operand) => budget.with_layout_query(|budget| {
                Ok(graph.layout_operand_in_context(
                    provider_site,
                    &operand,
                    query_state,
                    query_site,
                    budget,
                ))
            })?,
            Fact::Unknown(r) => Fact::Unknown(r),
            Fact::Incomplete(r) => Fact::Incomplete(r),
        };
        let keep = if let Fact::Known(layout) = &fact {
            layout_retained(layout)
        } else {
            0
        };
        budget.release(
            budget
                .snapshot()
                .retained_bytes
                .saturating_sub(before)
                .saturating_sub(keep),
        );
        result = Some(fact);
    }
    Ok(result)
}

pub(super) fn layout_retained(layout: &KnownLayout) -> usize {
    let identity = size_of::<TypeIdentity>()
        + match &layout.identity {
            TypeIdentity::Declared(key) => key.anchor.source.as_str().len(),
            TypeIdentity::Intrinsic(name) => name.len(),
        };
    identity
        + layout
            .dependencies
            .iter()
            .map(|dependency| match dependency {
                crate::ResolutionObservation::Payload {
                    source_id,
                    path,
                    revision,
                } => {
                    let mut n = size_of::<crate::ResolutionObservation>()
                        + source_id.as_str().len()
                        + path.as_os_str().len();
                    if let crate::SourceRevision::Disk {
                        read_policy,
                        path_entry,
                        ..
                    } = revision
                    {
                        let mut count = |bytes| {
                            n += bytes;
                            Ok(())
                        };
                        read_policy
                            .visit_recovery_payload(&mut count)
                            .expect("infallible visitor");
                        path_entry
                            .visit_recovery_payload(&mut count)
                            .expect("infallible visitor");
                    }
                    n
                }
                _ => size_of::<crate::ResolutionObservation>(),
            })
            .sum::<usize>()
}

pub(super) fn attach_dependencies(
    state: &CompileTimeState,
    layout: &mut KnownLayout,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<(), IncompleteReason> {
    fn visit(
        state: &CompileTimeState,
        layout: &mut KnownLayout,
        depth: usize,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
        for source in &state.provider_sources {
            budget.charge(layout.dependencies.len() + 1, 0, 0)?;
            if layout.dependencies.iter().any(|d|matches!(d,crate::ResolutionObservation::Payload { source_id,path,revision } if source_id==&source.id && path==&source.path && revision==&source.revision)) { continue; }
            let n = crate::resolver::loaded_source_metadata_retained(source)
                .saturating_sub(size_of::<crate::LoadedSource>())
                + size_of::<crate::ResolutionObservation>();
            budget.charge(1, n, n)?;
            layout.dependencies.reserve_exact(1);
            layout
                .dependencies
                .push(crate::ResolutionObservation::Payload {
                    source_id: source.id.clone(),
                    path: source.path.clone(),
                    revision: source.revision.clone(),
                });
        }
        for interface in &state.interfaces {
            budget.charge(1, 0, 0)?;
            if let Some(graph) = &interface.graph {
                visit(graph, layout, depth + 1, budget)?;
            }
        }
        Ok(())
    }
    let before = budget.snapshot().retained_bytes;
    let result = visit(state, layout, 0, budget);
    if result.is_err() {
        budget.release(budget.snapshot().retained_bytes.saturating_sub(before));
    }
    result
}

/// One source-ordered lexical result for both type and constant consumers.
/// A visible member is a binding/blocker, never permission for intrinsic fallback.
pub(super) enum Binding<'a> {
    Declaration(&'a Declaration),
    EnumMember {
        owner: &'a Declaration,
        index: usize,
    },
}

impl<'a> Binding<'a> {
    pub(super) fn type_declaration(self) -> Option<&'a Declaration> {
        match self {
            Self::Declaration(decl) if decl.is_type => Some(decl),
            _ => None,
        }
    }
}

pub(crate) fn intrinsic(name: &str) -> Option<BuiltinType> {
    use BuiltinType::*;
    // Only the compiler's unqualified intrinsic namespace is admitted here.
    // Qualified names require provider/binding evidence, not suffix matching.
    Some(match name.strip_prefix('&').unwrap_or(name) {
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

/// Delphi's identifier escape changes spelling, not the symbol's identity.
/// Callers charge comparison byte work before using this allocation-free helper.
pub(super) fn same_name(left: &str, right: &str) -> bool {
    left.strip_prefix('&')
        .unwrap_or(left)
        .eq_ignore_ascii_case(right.strip_prefix('&').unwrap_or(right))
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
        if decl.is_none() {
            if let Some(fact) = imported_operand(state, &site, &name, budget)? {
                return Ok(fact);
            }
        }
        let identity = if let Some(binding) = decl {
            let Some(decl) = binding.type_declaration() else {
                return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
            };
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
            state.settings_at_sequence(site.sequence, budget)?,
            &mut Vec::new(),
            0,
            budget,
        )?;
        let mut fact = make_layout(storage, identity);
        if let Fact::Known(layout) = &mut fact {
            attach_dependencies(state, layout, budget)?;
        }
        Ok(fact)
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
) -> Result<Option<Binding<'a>>, IncompleteReason> {
    let mut scope = Some(site.scope);
    while let Some(id) = scope {
        for decl in state.declarations.iter().rev() {
            budget.charge(1, 0, 0)?;
            if decl.site.scope == id && decl.site.sequence.0 <= site.sequence.0 {
                budget.charge(0, decl.name.len().min(name.len()), 0)?;
                if same_name(&decl.name, name) {
                    return Ok(Some(Binding::Declaration(decl)));
                }
                if let (true, TypeExpr::Enum(members)) = (decl.is_type, &decl.ty) {
                    for (index, member) in members.iter().enumerate() {
                        budget.charge(1, member.name.len().min(name.len()), 0)?;
                        if same_name(&member.name, name) {
                            return Ok(Some(Binding::EnumMember { owner: decl, index }));
                        }
                    }
                }
            }
        }
        scope = state.scopes[id.0 as usize].parent;
    }
    // Only a parser-proven current-unit prefix is admitted before the resource
    // provider owner. Never strip a familiar external/System suffix.
    if let Some(unit) = &state.unit_name {
        if name.len() > unit.len() && name.as_bytes().get(unit.len()) == Some(&b'.') {
            let Some(prefix) = name.get(..unit.len()) else {
                return Ok(None);
            };
            budget.charge(1, unit.len(), 0)?;
            if same_name(prefix, unit) {
                let root = prefix.split('.').next().unwrap_or(prefix);
                let suffix = &name[unit.len() + 1..];
                budget.charge(suffix.len(), suffix.len(), 0)?;
                if !suffix.contains('.') && lookup(state, site, root, budget)?.is_none() {
                    let global = BindingSite {
                        scope: ScopeId(0),
                        sequence: site.sequence,
                    };
                    return lookup(state, &global, suffix, budget);
                }
            }
        }
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
    let Some(binding) = lookup(state, site, name, budget)? else {
        if let Some(fact) = imported_operand(state, site, name, budget)? {
            return Ok(match fact {
                Fact::Known(layout) => Fact::Known(layout.storage),
                Fact::Unknown(r) => Fact::Unknown(r),
                Fact::Incomplete(r) => Fact::Incomplete(r),
            });
        }
        return Ok(
            intrinsic(name).map_or(Fact::Unknown(UnknownReason::UnresolvedName), |ty| {
                builtin_layout(ty, &state.context, settings)
            }),
        );
    };
    let Some(decl) = binding.type_declaration() else {
        return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
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
        TypeExpr::Record(record) => {
            super::records::record_layout(state, site, record, settings, visited, depth, budget)
        }
        TypeExpr::OpaqueNominal(_) | TypeExpr::Unsupported(_) => {
            Ok(Fact::Unknown(UnknownReason::UnsupportedLayout))
        }
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
