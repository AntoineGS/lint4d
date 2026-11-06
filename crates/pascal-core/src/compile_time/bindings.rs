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
    /// Declared under `{$SCOPEDENUMS ON}`: only `Type.Value` names it.
    pub scoped: bool,
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
    /// What a class, record, interface or helper body could declare, for
    /// the member scope of its methods (l4d-wwmg).
    pub shape: Option<std::sync::Arc<ClassShape>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ShapeKind {
    Class,
    /// An old-style `object`: no implicit ancestor.
    Object,
    Record,
    Interface,
    Helper,
}

/// The members a composite type body could declare. `names` holds every
/// identifier its active body text spells (D46's lexical rule applied to
/// one body); `None` when that text could not be read as a proof. The
/// heritage list is kept as spelled; a helper also keeps the type it helps.
#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct ClassShape {
    pub kind: ShapeKind,
    pub names: Option<super::InterfaceNames>,
    pub parents: Vec<String>,
    pub helped: Option<String>,
}

impl ClassShape {
    pub(super) fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            + self
                .names
                .as_ref()
                .map_or(0, super::InterfaceNames::retained_bytes)
            + self
                .parents
                .iter()
                .chain(self.helped.iter())
                .map(|p| size_of::<String>() + p.len())
                .sum::<usize>()
    }

    fn may_declare(&self, name: &str) -> bool {
        self.names
            .as_ref()
            .is_none_or(|names| names.may_declare(name))
    }
}

/// A provider's graph is retained in its own binding namespace. Declaration
/// keys, captured sites, scalar history and settings are never rewritten as
/// importer-local names. Only proven interface roots are exposed.
#[derive(Debug, Clone)]
pub(super) struct InterfaceFacts {
    pub qualifiers: Vec<String>,
    pub site: BindingSite,
    pub graph: Option<super::session::AdmittedInterfaceGraph>,
    /// The session's occurrence of `graph`, whose memoized graph has the
    /// most names prepared (l4d-4nbs D29).
    pub occurrence: Option<super::OccurrenceId>,
    /// What looking the provider up reported, for the interface compiled
    /// from it to carry (l4d-4nbs D44).
    pub lookup: Option<std::sync::Arc<super::session::LookupReports>>,
    pub pending: PendingProvider,
    pub attempted: bool,
    pub provided: Option<super::session::AdmittedProviderSource>,
    pub failure: Option<UnknownReason>,
    /// The identifiers an unknown or partly unknown interface could declare
    /// (l4d-onnm, D46): a name outside them is not declared there.
    pub names: Option<std::sync::Arc<super::InterfaceNames>>,
}

#[derive(Debug, Clone, Hash)]
pub(super) struct ImportIdentity {
    pub name: String,
    pub site: BindingSite,
    pub opener: SourceAnchor,
}

#[derive(Debug, Clone, Hash)]
pub(super) struct PendingProvider {
    pub identity: ImportIdentity,
    pub importer_path: std::path::PathBuf,
    pub authorized: bool,
}

impl InterfaceFacts {
    pub(super) fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.qualifiers.hash(&mut h);
        self.site.hash(&mut h);
        self.pending.hash(&mut h);
        self.attempted.hash(&mut h);
        self.failure
            .as_ref()
            .map(std::mem::discriminant)
            .hash(&mut h);
        if let Some(provider) = &self.provided {
            provider.unit.requested_name.hash(&mut h);
            provider.unit.declared_name.hash(&mut h);
            provider.context.hash(&mut h);
            provider.authorized_qualifiers.hash(&mut h);
            provider.unit.source.id.hash(&mut h);
            provider.unit.source.path.hash(&mut h);
            match &provider.unit.source.revision {
                crate::SourceRevision::Overlay {
                    version,
                    content_hash,
                } => {
                    version.hash(&mut h);
                    content_hash.hash(&mut h);
                }
                crate::SourceRevision::Disk {
                    stamp,
                    content_hash,
                    read_policy,
                    path_entry,
                } => {
                    stamp.hash(&mut h);
                    content_hash.hash(&mut h);
                    read_policy.hash(&mut h);
                    path_entry.hash(&mut h);
                }
            }
        }
        self.graph.as_ref().map(|g| g.fingerprint()).hash(&mut h);
        self.names.hash(&mut h);
        h.finish()
    }
    pub(super) fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        self.visit_payload(visit, true)
    }

    pub(super) fn visit_snapshot_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        self.visit_payload(visit, false)
    }

    fn visit_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
        recovery: bool,
    ) -> Result<(), String> {
        visit(size_of::<Self>() + self.qualifiers.len() * size_of::<String>())?;
        for qualifier in &self.qualifiers {
            visit(qualifier.len())?;
        }
        visit(
            self.pending.identity.name.len()
                + self.pending.identity.opener.source.as_str().len()
                + self.pending.importer_path.as_os_str().len(),
        )?;
        if recovery && let Some(provider) = &self.provided {
            visit(
                crate::resolver::loaded_source_metadata_retained(&provider.unit.source)
                    + provider.unit.requested_name.len()
                    + provider.unit.declared_name.len(),
            )?;
            provider.context.visit_recovery_payload(visit)?;
            visit(
                (provider.context.defines.len()
                    + provider.context.options.len()
                    + provider.context.constants.len())
                    * (size_of::<(String, crate::conditional::ConstantValue)>()
                        + 4 * size_of::<usize>()),
            )?;
            for q in &provider.authorized_qualifiers {
                visit(size_of::<String>() + q.len())?;
            }
        }
        if recovery && let Some(graph) = &self.graph {
            graph.visit_recovery_payload(visit)?;
        }
        if recovery && let Some(names) = &self.names {
            visit(names.retained_bytes())?;
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
) -> Result<Option<Fact<(&'a CompileTimeState, &'a Declaration)>>, IncompleteReason> {
    // None proves absence in all participating providers. Unknown is a binding
    // barrier, not permission to consult compiler-context or DEFINE fallbacks.
    let root = name.split('.').next().unwrap_or(name);
    if name.contains('.') && lookup(state, site, root, budget)?.is_some() {
        return Ok(Some(Fact::Unknown(UnknownReason::UnresolvedName)));
    }
    let explicitly_qualified = has_authorized_provider_qualifier(state, site, name, budget)?;
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
        if explicitly_qualified && !qualified {
            continue;
        }
        // Unknown-activity imports cannot be read, but are not proven absent.
        // After qualified exclusion, they prevent an unqualified identity proof.
        if !interface.pending.authorized {
            return Ok(Some(Fact::Unknown(UnknownReason::UnknownActivity)));
        }
        if !qualified && proven_undeclared(interface, root, budget)? {
            continue;
        }
        let Some(graph) = &interface.graph else {
            if qualified || !name.contains('.') {
                return Ok(Some(Fact::Unknown(UnknownReason::UnresolvedName)));
            }
            continue;
        };
        if !graph.alternatives.is_empty() || graph.failure.is_some() || graph.unknown {
            return Ok(Some(Fact::Unknown(UnknownReason::UnknownActivity)));
        }
        let decl = match lookup(graph, &graph.site(), suffix, budget)? {
            Some(Binding::Declaration(decl)) => decl,
            Some(Binding::EnumMember { owner, .. }) if owner.exported => {
                // Unsupported imported member values are still source bindings.
                // Their real parent owns export visibility; never manufacture a
                // type declaration or prove absence for scalar/callee fallback.
                return Ok(Some(Fact::Unknown(UnknownReason::UnresolvedName)));
            }
            _ => continue,
        };
        if !decl.exported {
            continue;
        }
        if result.is_some() {
            return Ok(Some(Fact::Unknown(UnknownReason::AmbiguousBinding)));
        }
        result = Some((&***graph, decl));
    }
    Ok(result.map(Fact::Known))
}

pub(super) fn imported_operand_with_query(
    state: &CompileTimeState,
    site: &BindingSite,
    text: &str,
    query_state: &CompileTimeState,
    query_site: &BindingSite,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<Fact<KnownLayout>>, IncompleteReason> {
    let text = text.trim();
    let root = text.split(['.', '[', '^']).next().unwrap_or(text).trim();
    if text.contains('.') && lookup(state, site, root, budget)?.is_some() {
        return Ok(Some(Fact::Unknown(UnknownReason::UnresolvedName)));
    }
    let explicitly_qualified = has_authorized_provider_qualifier(state, site, text, budget)?;
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
        if explicitly_qualified && !qualified {
            continue;
        }
        if !interface.pending.authorized {
            return Ok(Some(Fact::Unknown(UnknownReason::UnknownActivity)));
        }
        let root = suffix
            .split(['.', '[', '^'])
            .next()
            .unwrap_or(suffix)
            .trim();
        if !qualified && proven_undeclared(interface, root, budget)? {
            continue;
        }
        let Some(graph) = &interface.graph else {
            if qualified || !text.contains('.') {
                return Ok(Some(Fact::Unknown(
                    interface
                        .failure
                        .clone()
                        .unwrap_or(UnknownReason::UnresolvedName),
                )));
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
        let before = budget.manual_retained();
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
                .manual_retained()
                .saturating_sub(before)
                .saturating_sub(keep),
        );
        result = Some(fact);
    }
    Ok(result)
}

/// An interface that is unknown, or partly unknown, still cannot declare a
/// name that never occurs as an identifier in its interface text or the
/// includes it could expand there (l4d-onnm, D46). A known interface is
/// read instead.
fn proven_undeclared(
    interface: &InterfaceFacts,
    name: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    let Some(names) = &interface.names else {
        return Ok(false);
    };
    let unknown = interface.graph.as_ref().is_none_or(|graph| {
        !graph.alternatives.is_empty() || graph.failure.is_some() || graph.unknown
    });
    if !unknown {
        return Ok(false);
    }
    budget.charge(names.len().max(1).ilog2() as usize + 1, name.len(), 0)?;
    Ok(!names.may_declare(name))
}

/// Match the preparation path's authorized, source-visible provider spellings.
/// A dot alone is not qualification: imported record members may be unqualified.
fn has_authorized_provider_qualifier(
    state: &CompileTimeState,
    site: &BindingSite,
    name: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    for interface in &state.interfaces {
        budget.charge(1, 0, 0)?;
        if interface.site.sequence.0 > site.sequence.0 || !interface.pending.authorized {
            continue;
        }
        for qualifier in &interface.qualifiers {
            budget.charge(1, qualifier.len().min(name.len()), 0)?;
            if name.len() > qualifier.len()
                && name.as_bytes().get(qualifier.len()) == Some(&b'.')
                && name
                    .get(..qualifier.len())
                    .is_some_and(|p| same_name(p, qualifier))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
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
    let before = budget.manual_retained();
    let result = visit(state, layout, 0, budget);
    if result.is_err() {
        budget.release(budget.manual_retained().saturating_sub(before));
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
    match state.hidden_at(&site, budget) {
        Ok(true) => return Fact::Unknown(UnknownReason::UnknownActivity),
        Ok(false) => {}
        Err(reason) => return Fact::Incomplete(reason),
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
        if decl.is_none()
            && let Some(fact) = imported_operand(state, &site, &name, budget)?
        {
            return Ok(fact);
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

/// What a name that a member of the method's type could declare binds to:
/// a declaration that is neither a constant, a type nor a routine, so no
/// consumer proves anything from it (l4d-wwmg).
static MEMBER_BLOCKER: std::sync::LazyLock<Declaration> = std::sync::LazyLock::new(|| {
    let site = BindingSite {
        scope: ScopeId(0),
        sequence: Sequence(0),
    };
    Declaration {
        name: String::new(),
        key: DefinitionKey {
            anchor: SourceAnchor {
                source: crate::SourceId::new(String::new()),
                occurrence: OccurrenceId(0),
                range: 0..0,
            },
            scope: ScopeId(0),
        },
        site: site.clone(),
        type_site: site,
        ty: TypeExpr::Unsupported(String::new()),
        is_type: false,
        exported: false,
        is_constant: false,
        is_routine: false,
        settings: LayoutSettings::default(),
        provenance: Vec::new(),
        initializer: None,
        initializer_logical: None,
        shape: None,
    }
});

/// The binding stands for a possible member of a method's type.
pub(super) fn is_member_blocker(decl: &Declaration) -> bool {
    std::ptr::eq(decl, &*MEMBER_BLOCKER)
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
                        if !member.scoped && same_name(&member.name, name) {
                            return Ok(Some(Binding::EnumMember { owner: decl, index }));
                        }
                    }
                }
            }
        }
        // A method's parameters and locals come first, then the members of
        // its type, its ancestors and the helpers in scope (dcc 37.0 C1).
        if let Some(owner) = state.scopes[id.0 as usize].owner.as_deref()
            && member_may_bind(state, site, owner, name, budget)?
        {
            return Ok(Some(Binding::Declaration(&MEMBER_BLOCKER)));
        }
        scope = state.scopes[id.0 as usize].parent;
    }
    // Only a parser-proven current-unit prefix is admitted before the resource
    // provider owner. Never strip a familiar external/System suffix.
    if let Some(unit) = &state.unit_name
        && name.len() > unit.len()
        && name.as_bytes().get(unit.len()) == Some(&b'.')
    {
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
    Ok(None)
}

/// The members the `TObject` declarations of System.pas declare, in all
/// their conditional branches: RAD Studio 7.0 and 10.0, Studio 37.0 (the
/// names `member_names` would collect from them). A class without a
/// heritage list derives from System's TObject whatever `TObject` names in
/// its scope, and System declares no class helper.
const TOBJECT_MEMBERS: &[&str] = &[
    "__markdestroying",
    "__objaddref",
    "__objrelease",
    "__setdisposed",
    "afterconstruction",
    "beforedestruction",
    "checkdisposed",
    "classinfo",
    "classname",
    "classnameis",
    "classparent",
    "classtype",
    "cleanupinstance",
    "cpp_abi_1",
    "cpp_abi_2",
    "cpp_abi_3",
    "create",
    "defaulthandler",
    "destroy",
    "dispatch",
    "disposed",
    "disposeof",
    "equals",
    "fieldaddress",
    "free",
    "freeinstance",
    "frefcount",
    "getdisposed",
    "gethashcode",
    "getinterface",
    "getinterfaceentry",
    "getinterfacetable",
    "inheritsfrom",
    "initinstance",
    "instancesize",
    "methodaddress",
    "methodname",
    "newinstance",
    "objdestroyingflag",
    "objdisposedflag",
    "qualifiedclassname",
    "refcount",
    "safecallexception",
    "tostring",
    "unitname",
    "unitscope",
];

/// The members System.pas's `TInterfacedObject` declares, in all branches
/// (AUTOREFCOUNT): RAD Studio 7.0 lines 518-529, 10.0 lines 813-828,
/// Studio 37.0 lines 1064-1085. Its ancestor is TObject.
const TINTERFACEDOBJECT_MEMBERS: &[&str] = &[
    "__markdestroying",
    "_addref",
    "_release",
    "afterconstruction",
    "beforedestruction",
    "frefcount",
    "getrefcount",
    "newinstance",
    "objdestroyingflag",
    "queryinterface",
    "refcount",
];

/// The members System.pas's `TAggregatedObject` declares: RAD Studio 7.0
/// lines 562-574, 10.0 lines 863-875, Studio 37.0 lines 1120-1131. Its
/// ancestor is TObject.
const TAGGREGATEDOBJECT_MEMBERS: &[&str] = &[
    "_addref",
    "_release",
    "controller",
    "create",
    "fcontroller",
    "getcontroller",
    "queryinterface",
];

/// The members System.pas's `TContainedObject` declares: RAD Studio 7.0
/// lines 590-594, 10.0 lines 893-897, Studio 37.0 lines 1149-1153. Its
/// ancestor is TAggregatedObject.
const TCONTAINEDOBJECT_MEMBERS: &[&str] = &["queryinterface"];

fn members_may_declare(
    members: &[&str],
    name: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    budget.charge(members.len().max(1).ilog2() as usize + 1, name.len(), 0)?;
    let name = name.strip_prefix('&').unwrap_or(name);
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Ok(true);
    }
    Ok(members
        .binary_search_by(|probe| {
            probe
                .bytes()
                .cmp(name.bytes().map(|b| b.to_ascii_lowercase()))
        })
        .is_ok())
}

fn tobject_may_declare(
    name: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    members_may_declare(TOBJECT_MEMBERS, name, budget)
}

/// A type System declares that a heritage list commonly names. System and
/// SysInit declare no class helper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SystemType {
    Object,
    /// IInterface, IUnknown, IDispatch, IInvokable (RAD Studio 7.0 lines
    /// 455-503, 10.0 lines 746-796, Studio 37.0 lines 994-1047).
    Interface,
    InterfacedObject,
    AggregatedObject,
    ContainedObject,
}

impl SystemType {
    fn named(name: &str) -> Option<Self> {
        let name = name.strip_prefix('&').unwrap_or(name);
        [
            ("tobject", Self::Object),
            ("iinterface", Self::Interface),
            ("iunknown", Self::Interface),
            ("idispatch", Self::Interface),
            ("iinvokable", Self::Interface),
            ("tinterfacedobject", Self::InterfacedObject),
            ("taggregatedobject", Self::AggregatedObject),
            ("tcontainedobject", Self::ContainedObject),
        ]
        .into_iter()
        .find_map(|(system, ty)| name.eq_ignore_ascii_case(system).then_some(ty))
    }

    /// Whether a member of this class or of its System ancestors could
    /// declare `name`. An interface adds no member to a class's scope.
    fn may_declare(
        self,
        name: &str,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<bool, IncompleteReason> {
        let own = match self {
            Self::Interface => return Ok(false),
            Self::Object => return tobject_may_declare(name, budget),
            Self::InterfacedObject => TINTERFACEDOBJECT_MEMBERS,
            Self::AggregatedObject => TAGGREGATEDOBJECT_MEMBERS,
            Self::ContainedObject => TCONTAINEDOBJECT_MEMBERS,
        };
        if members_may_declare(own, name, budget)? {
            return Ok(true);
        }
        let parent = match self {
            Self::ContainedObject => Self::AggregatedObject,
            _ => Self::Object,
        };
        parent.may_declare(name, budget)
    }
}

/// A composite type a heritage list or a method heading names.
enum Resolved<'a> {
    Type(&'a CompileTimeState, &'a Declaration),
    /// A type of System, which no unit in scope redeclares.
    System(SystemType),
    /// Not proven: unresolved, ambiguous, not a composite type body, or
    /// declared where it cannot be read.
    Unproven,
}

fn resolve_type<'a>(
    state: &'a CompileTimeState,
    site: &BindingSite,
    name: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Resolved<'a>, IncompleteReason> {
    budget.charge(1, name.len(), 0)?;
    if name.is_empty() || name.contains(['.', '<']) {
        return Ok(Resolved::Unproven);
    }
    if state.unknown || !state.alternatives.is_empty() || state.hidden_at(site, budget)? {
        return Ok(Resolved::Unproven);
    }
    let (owner, decl) = match lookup(state, site, name, budget)? {
        Some(Binding::Declaration(decl)) => (state, decl),
        Some(Binding::EnumMember { .. }) => return Ok(Resolved::Unproven),
        None => match imported_declaration(state, site, name, budget)? {
            Some(Fact::Known(found)) => found,
            Some(_) => return Ok(Resolved::Unproven),
            None => {
                return Ok(SystemType::named(name).map_or(Resolved::Unproven, Resolved::System));
            }
        },
    };
    Ok(if decl.is_type && decl.shape.is_some() {
        Resolved::Type(owner, decl)
    } else {
        Resolved::Unproven
    })
}

/// Whether a member of `decl`'s type, of its ancestors or, for a helper, of
/// the helped type could declare `name`.
fn type_may_declare(
    state: &CompileTimeState,
    decl: &Declaration,
    name: &str,
    depth: usize,
    visited: &mut Vec<*const Declaration>,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    budget.charge(visited.len() + 1, 0, 0)?;
    let Some(shape) = &decl.shape else {
        return Ok(true);
    };
    if visited.contains(&(decl as *const Declaration)) {
        // A cyclic heritage list does not compile; it proves nothing.
        return Ok(true);
    }
    budget.charge(
        shape
            .names
            .as_ref()
            .map_or(1, |n| n.len().max(1).ilog2() as usize + 1),
        name.len(),
        0,
    )?;
    if shape.may_declare(name) {
        return Ok(true);
    }
    visited.push(decl);
    let site = decl.site.clone();
    let result = (|| {
        let mut class_parent = false;
        let helped = shape
            .helped
            .iter()
            .filter(|_| shape.kind == ShapeKind::Helper);
        // Only the first entry of a class's heritage list can be its
        // ancestor; the others are implemented interfaces, which add no
        // member to its scope (dcc C3), resolved or not.
        let parents = if shape.kind == ShapeKind::Class {
            &shape.parents[..shape.parents.len().min(1)]
        } else {
            &shape.parents[..]
        };
        for parent in parents.iter().chain(helped) {
            match resolve_type(state, &site, parent, budget)? {
                Resolved::Type(owner, parent) => {
                    let interface = parent
                        .shape
                        .as_ref()
                        .is_some_and(|s| s.kind == ShapeKind::Interface);
                    class_parent |= !interface;
                    // An interface's members are not in a class's scope.
                    if !interface
                        && type_may_declare(owner, parent, name, depth + 1, visited, budget)?
                    {
                        return Ok(true);
                    }
                }
                Resolved::System(system) => {
                    class_parent |= system != SystemType::Interface;
                    if system.may_declare(name, budget)? {
                        return Ok(true);
                    }
                }
                Resolved::Unproven => return Ok(true),
            }
        }
        if shape.kind == ShapeKind::Class && !class_parent {
            return tobject_may_declare(name, budget);
        }
        Ok(false)
    })();
    visited.pop();
    result
}

/// Whether a helper or one of its ancestor helpers could declare `name`.
fn helper_may_declare(
    state: &CompileTimeState,
    decl: &Declaration,
    name: &str,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    let Some(shape) = &decl.shape else {
        return Ok(true);
    };
    budget.charge(
        shape
            .names
            .as_ref()
            .map_or(1, |n| n.len().max(1).ilog2() as usize + 1),
        name.len(),
        0,
    )?;
    if shape.may_declare(name) {
        return Ok(true);
    }
    for parent in &shape.parents {
        match resolve_type(state, &decl.site, parent, budget)? {
            Resolved::Type(owner, parent) => {
                if helper_may_declare(owner, parent, name, depth + 1, budget)? {
                    return Ok(true);
                }
            }
            _ => return Ok(true),
        }
    }
    Ok(false)
}

/// Whether a class or record helper in scope at `site` could declare
/// `name`. Which helper is active for which type is not modelled: every
/// helper the unit declares or imports counts, with its ancestor helpers.
/// An imported interface that is not known and never spells `helper`
/// declares no helper (D46); one that does may declare any member.
fn helpers_may_declare(
    state: &CompileTimeState,
    site: &BindingSite,
    name: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    fn declared(
        state: &CompileTimeState,
        site: &BindingSite,
        name: &str,
        exported: bool,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<bool, IncompleteReason> {
        for decl in &state.declarations {
            budget.charge(1, 0, 0)?;
            if decl.site.sequence.0 > site.sequence.0
                || (exported && !decl.exported)
                || decl
                    .shape
                    .as_ref()
                    .is_none_or(|shape| shape.kind != ShapeKind::Helper)
            {
                continue;
            }
            if helper_may_declare(state, decl, name, 0, budget)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    if declared(state, site, name, false, budget)? {
        return Ok(true);
    }
    for interface in &state.interfaces {
        budget.charge(1, 0, 0)?;
        if interface.site.sequence.0 > site.sequence.0 {
            continue;
        }
        if !interface.pending.authorized {
            return Ok(true);
        }
        let known = interface.graph.as_ref().filter(|graph| {
            graph.alternatives.is_empty() && graph.failure.is_none() && !graph.unknown
        });
        if let Some(graph) = known {
            if declared(graph, &graph.site(), name, true, budget)? {
                return Ok(true);
            }
            continue;
        }
        let Some(names) = &interface.names else {
            return Ok(true);
        };
        budget.charge(2 * (names.len().max(1).ilog2() as usize + 1), name.len(), 0)?;
        // A helper there may inherit the name from an ancestor helper of
        // another unit, which its own text never spells (review P1, dcc
        // 37.0): a unit that may declare a helper at all proves nothing.
        if names.may_declare("helper") || names.may_declare(name) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether, in a method of `owner`, a member could bind `name` before the
/// scopes enclosing the method do.
fn member_may_bind(
    state: &CompileTimeState,
    site: &BindingSite,
    owner: &str,
    name: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    let root = name.split('.').next().unwrap_or(name);
    let global = BindingSite {
        scope: ScopeId(0),
        sequence: site.sequence,
    };
    let mut visited = Vec::new();
    match resolve_type(state, &global, owner, budget)? {
        Resolved::Type(owner_state, decl)
            if decl
                .shape
                .as_ref()
                .is_some_and(|shape| shape.kind != ShapeKind::Interface) =>
        {
            if type_may_declare(owner_state, decl, root, 0, &mut visited, budget)? {
                return Ok(true);
            }
        }
        _ => return Ok(true),
    }
    helpers_may_declare(state, &global, root, budget)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_members_are_sorted_lowercase_identifiers() {
        for members in [
            TOBJECT_MEMBERS,
            TINTERFACEDOBJECT_MEMBERS,
            TAGGREGATEDOBJECT_MEMBERS,
            TCONTAINEDOBJECT_MEMBERS,
        ] {
            assert!(members.windows(2).all(|w| w[0] < w[1]));
            assert!(members.iter().all(|name| {
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            }));
        }
        let mut budget =
            CompileTimeBudget::new(CompileTimeLimits::default(), &crate::NoCancellation);
        for name in ["Free", "CLASSNAME", "&Create", "Unit.Name"] {
            assert!(tobject_may_declare(name, &mut budget).unwrap(), "{name}");
        }
        for name in ["CompilerVersion", "Integer", "TObject"] {
            assert!(!tobject_may_declare(name, &mut budget).unwrap(), "{name}");
        }
    }
}
