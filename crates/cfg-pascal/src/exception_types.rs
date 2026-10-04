use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

use tree_sitter::Node;

use crate::prepared::{is_preprocessor_kind, is_preprocessor_node};
use crate::project::{import_site_spans, ImportTarget, ProjectSnapshot, ProjectUnitId};

/// Stable identity for a class type in one project snapshot.
///
/// The caller-assigned unit identity is part of the key, so equal declarations
/// in different units never compare equal and identity does not depend on the
/// order in which the caller supplied the units.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct UnitKey(String);

impl UnitKey {
    fn singleton() -> Self {
        Self("<singleton>".to_string())
    }

    pub(crate) fn from_project(id: &ProjectUnitId) -> Self {
        Self(id.as_str().to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TypeId {
    /// Ordinal assigned by sorting the caller's stable unit IDs.
    unit: usize,
    local: usize,
}

/// The semantic fact carried by an exceptional transfer.
///
/// `Known` is an exact class produced by a proven constructor. `SubtypeOf` is
/// a conservative bound used when re-raising from a typed handler: the
/// original exception is that class or one of its descendants, but the exact
/// runtime class is not known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ExceptionTypeFact {
    Known(TypeId),
    SubtypeOf(TypeId),
    Unknown,
}

/// Result of comparing a raised class with one typed handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeMatch {
    Yes,
    No,
    Unknown,
}

/// A private, owned index of the type information that is safe to use for
/// exception dispatch in one file or in an explicitly connected project
/// snapshot.
///
/// This deliberately does not try to model the Pascal type system in full.
/// Only complete, non-generic classes and transparent aliases are resolved.
/// The unit-local collector handles same-file declarations while the wrapper
/// adds only explicitly selected project imports. Missing, shadowed,
/// conditional, malformed, generic, `with`-implicit, and otherwise unsupported
/// information stays unknown so CFG edges are never removed on an unproven
/// assumption.
///
/// The declaration collector lives in [`UnitTypeIndex`]. This project-level
/// wrapper supplies only the cross-unit namespace and uses-site resolution;
/// it intentionally does not duplicate the collector. Every modern `pp*`
/// node is treated as an
/// unresolved file-wide barrier because the grammar may expose directives,
/// conditional blocks, or preprocessor fragments as siblings rather than as
/// ancestors of the declarations they affect; ordinary comments are not
/// barriers.
#[derive(Debug)]
pub(crate) struct ExceptionTypeIndex {
    units: HashMap<UnitKey, UnitTypeIndex>,
    unit_keys: Vec<UnitKey>,
    imports: Vec<ImportSelection>,
}

#[derive(Debug)]
struct UnitTypeIndex {
    unit_index: usize,
    module_name: Option<String>,
    root_scope: LexicalScopeId,
    scopes: Vec<LexicalScope>,
    types: Vec<TypeDeclaration>,
    unsupported_ranges: Vec<Range<usize>>,
    /// A `with` body has implicit member bindings that are not represented by
    /// the lexical tree.  Unqualified names in it cannot safely fall through
    /// to the file's global type namespace.
    with_ranges: Vec<Range<usize>>,
    /// The grammar exposes preprocessor directives as extra sibling nodes,
    /// rather than wrapping the declarations they affect.  A range check is
    /// therefore not sufficient to decide whether a type relationship is
    /// active; seeing an unresolved directive makes the whole file
    /// conservative.
    has_preprocessor_barrier: bool,
    pending_method_owners: Vec<PendingMethodOwner>,
    interface_range: Option<Range<usize>>,
    parser_incomplete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportSection {
    /// A program/library uses clause, or an unusual uses clause without a
    /// containing unit section.
    Module,
    Interface,
    Implementation,
}

#[derive(Debug, Clone)]
enum ImportSelectionTarget {
    Loaded(UnitKey),
    Unavailable,
    Ambiguous,
}

#[derive(Debug, Clone)]
struct ImportSelection {
    importer: UnitKey,
    start: usize,
    end: usize,
    section: ImportSection,
    section_start: usize,
    target: ImportSelectionTarget,
    authorized_qualifiers: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct LexicalScopeId(usize);

#[derive(Debug)]
struct LexicalScope {
    parent: Option<LexicalScopeId>,
    start: usize,
    end: usize,
    bindings: Vec<Binding>,
    /// A routine implementation whose owner resolved to a class. The owner
    /// may live in another project unit, so it is stored as a type identity
    /// rather than by borrowing that unit's lexical scope.
    owner_type: Option<TypeId>,
    /// The scope belongs to a method whose syntactic owner could not be
    /// resolved. Its missing parent must not be mistaken for the file root.
    unresolved_owner: bool,
}

#[derive(Debug)]
struct Binding {
    name: String,
    start: usize,
    kind: BindingKind,
    visibility: Visibility,
}

#[derive(Debug, Clone, Copy)]
enum BindingKind {
    Type(TypeId),
    Value,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Default,
    Private,
    StrictPrivate,
    Protected,
    StrictProtected,
    Public,
    Published,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreateMember {
    None,
    Constructor(Visibility),
    NonConstructor(Visibility),
    Ambiguous,
    Unknown,
}

#[derive(Debug)]
struct TypeDeclaration {
    definition: TypeDefinition,
}

#[derive(Debug)]
enum TypeDefinition {
    Class {
        class_scope: LexicalScopeId,
        parent: ParentType,
        create_member: CreateMember,
        incomplete: bool,
    },
    Alias {
        target: Option<TypeReference>,
    },
    Unsupported,
}

#[derive(Debug)]
enum ParentType {
    None,
    Reference(TypeReference),
    Unsupported,
}

#[derive(Debug)]
struct TypeReference {
    parts: Vec<String>,
    scope: LexicalScopeId,
    offset: usize,
}

#[derive(Debug)]
struct PendingMethodOwner {
    routine_scope: LexicalScopeId,
    /// `None` records a syntactically present but unsupported owner, such as
    /// a generic instantiation.  It is important not to leave the routine
    /// attached to its lexical root in that case: doing so would make an
    /// unqualified member name fall through to an unrelated global type.
    owner_parts: Option<Vec<String>>,
    enclosing_scope: LexicalScopeId,
    offset: usize,
}

#[derive(Debug, Clone, Copy)]
enum Lookup {
    Type(TypeId),
    Value,
    Unknown,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Knowledge {
    Yes,
    No,
    Unknown,
}

impl ExceptionTypeIndex {
    pub(crate) fn build(root: Node, source: &[u8]) -> Self {
        let unit_key = UnitKey::singleton();
        let unit = UnitTypeIndex::build(0, root, source);
        let mut index = Self {
            units: HashMap::from([(unit_key, unit)]),
            unit_keys: vec![UnitKey::singleton()],
            imports: Vec::new(),
        };
        index.resolve_pending_method_owners();
        index
    }

    pub(crate) fn build_project(snapshot: &ProjectSnapshot) -> Self {
        let mut inputs: Vec<_> = snapshot.units().iter().collect();
        inputs.sort_by(|left, right| left.id().cmp(right.id()));

        let mut units = HashMap::new();
        let mut unit_keys = Vec::with_capacity(inputs.len());
        for (unit_index, input) in inputs.iter().enumerate() {
            let unit_key = UnitKey::from_project(input.id());
            let unit = UnitTypeIndex::build(unit_index, input.tree().root_node(), input.source());
            unit_keys.push(unit_key.clone());
            units.insert(unit_key, unit);
        }

        let mut index = Self {
            units,
            unit_keys,
            imports: Vec::new(),
        };
        let mut selected_sites = HashSet::new();
        for binding in snapshot.imports() {
            let importer_key = UnitKey::from_project(binding.site().unit_id());
            let Some(importer) = snapshot.unit(binding.site().unit_id()) else {
                continue;
            };
            let byte_range = binding.site().byte_range();
            selected_sites.insert((importer_key.clone(), byte_range.start, byte_range.end));
            let (section, section_start) =
                import_section(importer.tree().root_node(), byte_range.start);
            let target = match binding.target() {
                ImportTarget::Loaded(target) => {
                    ImportSelectionTarget::Loaded(UnitKey::from_project(target))
                }
                ImportTarget::Unavailable => ImportSelectionTarget::Unavailable,
                ImportTarget::Ambiguous => ImportSelectionTarget::Ambiguous,
            };
            index.imports.push(ImportSelection {
                importer: importer_key,
                start: byte_range.start,
                end: byte_range.end,
                section,
                section_start,
                target,
                authorized_qualifiers: binding
                    .authorized_qualifiers()
                    .iter()
                    .map(|qualifier| {
                        qualifier
                            .split('.')
                            .map(|part| canonical(part.to_string()))
                            .collect()
                    })
                    .collect(),
            });
        }
        for input in snapshot.units() {
            let importer = UnitKey::from_project(input.id());
            for (start, end) in import_site_spans(input.tree().root_node()) {
                if selected_sites.contains(&(importer.clone(), start, end)) {
                    continue;
                }
                let (section, section_start) = import_section(input.tree().root_node(), start);
                index.imports.push(ImportSelection {
                    importer: importer.clone(),
                    start,
                    end,
                    section,
                    section_start,
                    target: ImportSelectionTarget::Unavailable,
                    authorized_qualifiers: Vec::new(),
                });
            }
        }
        index
            .imports
            .sort_by_key(|import| std::cmp::Reverse(import.start));
        index.resolve_pending_method_owners();
        index
    }

    pub(crate) fn singleton_unit_key() -> UnitKey {
        UnitKey::singleton()
    }

    /// Resolve the type produced by a syntactic `raise TException.Create`
    /// expression.  The returned fact is authoritative only when it is
    /// `Known`; all other forms remain conservative.
    pub(crate) fn raised_fact(
        &self,
        unit_key: &UnitKey,
        raise: Node,
        source: &[u8],
    ) -> ExceptionTypeFact {
        let Some(unit) = self.units.get(unit_key) else {
            return ExceptionTypeFact::Unknown;
        };
        let Some(parts) = constructor_parts(raise, source) else {
            return ExceptionTypeFact::Unknown;
        };
        if unit.has_preprocessor_barrier || unit.is_unsupported_at(raise.start_byte()) {
            return ExceptionTypeFact::Unknown;
        }
        if unit.is_with_implicit_reference(&parts, raise.start_byte()) {
            return ExceptionTypeFact::Unknown;
        }

        let scope = unit.scope_at(raise.start_byte());
        let Some(type_id) = self.resolve_parts(
            &parts,
            unit_key,
            scope,
            raise.start_byte(),
            &mut HashMap::new(),
        ) else {
            return ExceptionTypeFact::Unknown;
        };

        let knowledge = self.constructor_knowledge(type_id, unit_key);
        match knowledge {
            Knowledge::Yes => ExceptionTypeFact::Known(type_id),
            Knowledge::No | Knowledge::Unknown => ExceptionTypeFact::Unknown,
        }
    }

    /// Resolve the type named by a typed `except` handler.
    pub(crate) fn handler_fact(
        &self,
        unit_key: &UnitKey,
        handler: Node,
        source: &[u8],
    ) -> ExceptionTypeFact {
        let Some(unit) = self.units.get(unit_key) else {
            return ExceptionTypeFact::Unknown;
        };
        let Some(exception) = handler.child_by_field_name("exception") else {
            return ExceptionTypeFact::Unknown;
        };
        if unit.has_preprocessor_barrier || unit.is_unsupported_at(exception.start_byte()) {
            return ExceptionTypeFact::Unknown;
        }
        let Some(parts) = type_reference_parts(exception, source) else {
            return ExceptionTypeFact::Unknown;
        };
        if unit.is_with_implicit_reference(&parts, handler.start_byte()) {
            return ExceptionTypeFact::Unknown;
        }
        let scope = unit.scope_at(handler.start_byte());
        let resolved = self.resolve_parts(
            &parts,
            unit_key,
            scope,
            exception.start_byte(),
            &mut HashMap::new(),
        );
        resolved
            .map(ExceptionTypeFact::Known)
            .unwrap_or(ExceptionTypeFact::Unknown)
    }

    pub(crate) fn match_handler(
        &self,
        raised: ExceptionTypeFact,
        handler: ExceptionTypeFact,
    ) -> TypeMatch {
        match (raised, handler) {
            (ExceptionTypeFact::Known(raised), ExceptionTypeFact::Known(handler)) => {
                self.is_subtype(raised, handler, &mut HashMap::new())
            }
            (ExceptionTypeFact::SubtypeOf(bound), ExceptionTypeFact::Known(handler)) => {
                self.match_subtype_bound(bound, handler)
            }
            _ => TypeMatch::Unknown,
        }
    }

    /// Compute the fact available to a typed handler body. An exact fact is
    /// retained only when every incoming path that can reach the handler is
    /// the same proven class. Otherwise the handler declaration provides the
    /// safe subtype bound.
    pub(crate) fn handler_context(
        &self,
        handler: ExceptionTypeFact,
        incoming: &[ExceptionTypeFact],
    ) -> ExceptionTypeFact {
        let ExceptionTypeFact::Known(handler_type) = handler else {
            return ExceptionTypeFact::Unknown;
        };

        let mut exact = None;
        for &raised in incoming {
            if matches!(self.match_handler(raised, handler), TypeMatch::No) {
                continue;
            }
            let ExceptionTypeFact::Known(raised_type) = raised else {
                return ExceptionTypeFact::SubtypeOf(handler_type);
            };
            if let Some(previous) = exact {
                if previous != raised_type {
                    return ExceptionTypeFact::SubtypeOf(handler_type);
                }
            } else {
                exact = Some(raised_type);
            }
        }

        exact
            .map(ExceptionTypeFact::Known)
            .unwrap_or(ExceptionTypeFact::SubtypeOf(handler_type))
    }

    fn resolve_pending_method_owners(&mut self) {
        let mut unit_keys: Vec<_> = self.units.keys().cloned().collect();
        unit_keys.sort_by(|left, right| left.0.cmp(&right.0));

        for unit_key in unit_keys {
            let pending = self
                .units
                .get_mut(&unit_key)
                .map(|unit| std::mem::take(&mut unit.pending_method_owners))
                .unwrap_or_default();

            for owner in pending {
                let Some(owner_parts) = owner.owner_parts else {
                    self.mark_unresolved_owner(&unit_key, owner.routine_scope);
                    continue;
                };
                let Some(type_id) = self.resolve_parts(
                    &owner_parts,
                    &unit_key,
                    owner.enclosing_scope,
                    owner.offset,
                    &mut HashMap::new(),
                ) else {
                    self.mark_unresolved_owner(&unit_key, owner.routine_scope);
                    continue;
                };
                if self.class_scope(type_id).is_none() {
                    self.mark_unresolved_owner(&unit_key, owner.routine_scope);
                    continue;
                }
                if let Some(unit) = self.units.get_mut(&unit_key) {
                    unit.scopes[owner.routine_scope.0].owner_type = Some(type_id);
                }
            }
        }
    }

    fn mark_unresolved_owner(&mut self, unit_key: &UnitKey, routine_scope: LexicalScopeId) {
        if let Some(unit) = self.units.get_mut(unit_key) {
            let scope = &mut unit.scopes[routine_scope.0];
            scope.parent = None;
            scope.owner_type = None;
            scope.unresolved_owner = true;
        }
    }

    fn resolve_parts(
        &self,
        parts: &[String],
        unit_key: &UnitKey,
        scope: LexicalScopeId,
        offset: usize,
        seen: &mut HashMap<TypeId, ()>,
    ) -> Option<TypeId> {
        if parts.is_empty() {
            return None;
        }

        self.units.get(unit_key)?;
        if let Some((root_scope, remaining)) =
            self.module_qualified_scope(unit_key, parts, scope, offset)
        {
            return self.resolve_local_parts(remaining, unit_key, root_scope, offset, seen);
        }

        match self.lookup(unit_key, scope, &parts[0], offset) {
            Lookup::Type(type_id) => {
                let resolved = self.resolve_type_id(type_id, seen)?;
                self.resolve_member_parts(&parts[1..], resolved, unit_key, offset, seen)
            }
            Lookup::Value | Lookup::Unknown => None,
            Lookup::Absent => {
                if parts.len() > 1
                    && self.imports.iter().any(|import| {
                        self.qualifier_match(import, parts, unit_key, offset)
                            .is_some()
                    })
                {
                    self.resolve_imported_parts(parts, unit_key, offset, true, seen)
                } else {
                    self.resolve_imported_parts(parts, unit_key, offset, false, seen)
                }
            }
        }
    }

    fn resolve_local_parts(
        &self,
        parts: &[String],
        unit_key: &UnitKey,
        scope: LexicalScopeId,
        offset: usize,
        seen: &mut HashMap<TypeId, ()>,
    ) -> Option<TypeId> {
        let first = match self.lookup(unit_key, scope, &parts[0], offset) {
            Lookup::Type(type_id) => self.resolve_type_id(type_id, seen)?,
            Lookup::Value | Lookup::Unknown | Lookup::Absent => return None,
        };
        self.resolve_member_parts(&parts[1..], first, unit_key, offset, seen)
    }

    fn resolve_member_parts(
        &self,
        parts: &[String],
        mut resolved: TypeId,
        access_unit: &UnitKey,
        offset: usize,
        seen: &mut HashMap<TypeId, ()>,
    ) -> Option<TypeId> {
        for part in parts {
            resolved = match self.lookup_class_member(
                resolved,
                part,
                access_unit,
                offset,
                &mut HashMap::new(),
            ) {
                Lookup::Type(type_id) => self.resolve_type_id(type_id, seen)?,
                Lookup::Value | Lookup::Unknown | Lookup::Absent => return None,
            };
        }
        Some(resolved)
    }

    fn resolve_imported_parts(
        &self,
        parts: &[String],
        unit_key: &UnitKey,
        offset: usize,
        qualified_only: bool,
        seen: &mut HashMap<TypeId, ()>,
    ) -> Option<TypeId> {
        for import in self.visible_imports(unit_key, offset) {
            let prefix_len = if qualified_only {
                if self.import_can_shadow_qualifier(import, &parts[0]) {
                    return None;
                }
                let Some(prefix_len) = self.qualifier_match(import, parts, unit_key, offset) else {
                    continue;
                };
                prefix_len
            } else {
                0
            };
            if prefix_len >= parts.len() {
                continue;
            }
            let target = match &import.target {
                ImportSelectionTarget::Loaded(target) => target,
                ImportSelectionTarget::Unavailable | ImportSelectionTarget::Ambiguous => {
                    // An unresolved higher-priority namespace is a blocker,
                    // not evidence that a lower-priority unit is absent.
                    return None;
                }
            };
            let lookup = self.lookup_export(target, &parts[prefix_len]);
            match lookup {
                Lookup::Type(type_id) => {
                    let resolved = self.resolve_type_id(type_id, seen)?;
                    return self.resolve_member_parts(
                        &parts[prefix_len + 1..],
                        resolved,
                        unit_key,
                        offset,
                        seen,
                    );
                }
                Lookup::Value | Lookup::Unknown => return None,
                Lookup::Absent => continue,
            }
        }
        None
    }

    fn visible_imports<'a>(
        &'a self,
        unit_key: &UnitKey,
        offset: usize,
    ) -> impl Iterator<Item = &'a ImportSelection> + 'a {
        let unit_key = unit_key.clone();
        self.imports.iter().filter(move |import| {
            import.importer == unit_key
                && import.end <= offset
                && match import.section {
                    ImportSection::Module | ImportSection::Interface => true,
                    ImportSection::Implementation => offset >= import.section_start,
                }
        })
    }

    fn qualifier_match(
        &self,
        import: &ImportSelection,
        parts: &[String],
        unit_key: &UnitKey,
        offset: usize,
    ) -> Option<usize> {
        if &import.importer != unit_key || !self.import_visible(import, offset) {
            return None;
        }
        import
            .authorized_qualifiers
            .iter()
            .filter(|qualifier| qualifier.len() < parts.len() && parts.starts_with(qualifier))
            .map(Vec::len)
            .max()
    }

    fn import_can_shadow_qualifier(&self, import: &ImportSelection, qualifier: &str) -> bool {
        match &import.target {
            ImportSelectionTarget::Unavailable | ImportSelectionTarget::Ambiguous => true,
            ImportSelectionTarget::Loaded(target) => {
                !matches!(self.lookup_export(target, qualifier), Lookup::Absent)
            }
        }
    }

    fn import_visible(&self, import: &ImportSelection, offset: usize) -> bool {
        if import.end > offset {
            return false;
        }
        match import.section {
            ImportSection::Module | ImportSection::Interface => true,
            ImportSection::Implementation => offset >= import.section_start,
        }
    }

    fn lookup_export(&self, unit_key: &UnitKey, name: &str) -> Lookup {
        let Some(unit) = self.units.get(unit_key) else {
            return Lookup::Unknown;
        };
        if unit.parser_incomplete || unit.has_preprocessor_barrier {
            return Lookup::Unknown;
        }
        let Some(interface_range) = &unit.interface_range else {
            return Lookup::Absent;
        };

        let mut matches: Vec<&Binding> = unit.scopes[unit.root_scope.0]
            .bindings
            .iter()
            .filter(|binding| {
                binding.name == name
                    && interface_range.start <= binding.start
                    && binding.start < interface_range.end
            })
            .collect();
        let Some(latest_start) = matches.iter().map(|binding| binding.start).max() else {
            return Lookup::Absent;
        };
        matches.retain(|binding| binding.start == latest_start);
        if matches.len() != 1 {
            return Lookup::Unknown;
        }
        match matches[0].kind {
            BindingKind::Type(type_id) => Lookup::Type(type_id),
            BindingKind::Value => Lookup::Value,
            BindingKind::Unsupported => Lookup::Unknown,
        }
    }

    fn module_qualified_scope<'a>(
        &self,
        unit_key: &UnitKey,
        parts: &'a [String],
        scope: LexicalScopeId,
        offset: usize,
    ) -> Option<(LexicalScopeId, &'a [String])> {
        let unit = self.units.get(unit_key)?;
        let module_parts: Vec<_> = unit
            .module_name
            .as_deref()?
            .split('.')
            .map(str::to_ascii_lowercase)
            .collect();
        if parts.len() <= module_parts.len()
            || !parts.starts_with(&module_parts)
            || self.qualifier_is_shadowed(unit_key, scope, &parts[0], offset)
            || self.has_unresolved_owner(unit_key, scope)
        {
            return None;
        }
        Some((unit.root_scope, &parts[module_parts.len()..]))
    }

    fn has_unresolved_owner(&self, unit_key: &UnitKey, scope: LexicalScopeId) -> bool {
        let Some(unit) = self.units.get(unit_key) else {
            return true;
        };
        let mut current = Some(scope);
        while let Some(scope) = current {
            if unit.scopes[scope.0].unresolved_owner {
                return true;
            }
            current = unit.scopes[scope.0].parent;
        }
        false
    }

    fn qualifier_is_shadowed(
        &self,
        unit_key: &UnitKey,
        scope: LexicalScopeId,
        name: &str,
        offset: usize,
    ) -> bool {
        let mut current = Some(scope);
        while let Some(scope) = current {
            let Some(unit) = self.units.get(unit_key) else {
                return true;
            };
            if self.has_binding(unit_key, scope, name, offset) {
                return true;
            }
            if let Some(type_id) = self.class_type_for_scope(unit_key, scope) {
                if !matches!(
                    self.lookup_class_member(type_id, name, unit_key, offset, &mut HashMap::new(),),
                    Lookup::Absent
                ) {
                    return true;
                }
            }
            if let Some(owner_type) = unit.scopes[scope.0].owner_type {
                if !matches!(
                    self.lookup_class_member(
                        owner_type,
                        name,
                        unit_key,
                        offset,
                        &mut HashMap::new(),
                    ),
                    Lookup::Absent
                ) {
                    return true;
                }
            }
            current = unit.scopes[scope.0].parent;
        }
        false
    }

    fn lookup_class_member(
        &self,
        type_id: TypeId,
        name: &str,
        access_unit: &UnitKey,
        offset: usize,
        seen: &mut HashMap<TypeId, ()>,
    ) -> Lookup {
        if seen.insert(type_id, ()).is_some() {
            return Lookup::Unknown;
        }
        let Some(unit) = self.unit_for_type(type_id) else {
            seen.remove(&type_id);
            return Lookup::Unknown;
        };
        let TypeDefinition::Class {
            class_scope,
            parent,
            incomplete,
            ..
        } = &unit.types[type_id.local].definition
        else {
            seen.remove(&type_id);
            return Lookup::Unknown;
        };
        if *incomplete || unit.parser_incomplete || unit.has_preprocessor_barrier {
            seen.remove(&type_id);
            return Lookup::Unknown;
        }

        let defining_unit = type_id_unit_key(self, type_id);
        let member_offset = if &defining_unit == access_unit {
            offset
        } else {
            usize::MAX
        };
        let direct = self.lookup_direct_member(
            unit,
            *class_scope,
            name,
            &defining_unit,
            access_unit,
            member_offset,
        );
        if !matches!(direct, Lookup::Absent) {
            seen.remove(&type_id);
            return direct;
        }
        let result = match parent {
            ParentType::None => Lookup::Absent,
            ParentType::Unsupported => Lookup::Unknown,
            ParentType::Reference(parent) => {
                let Some(parent_id) = self.resolve_parts(
                    &parent.parts,
                    &type_id_unit_key(self, type_id),
                    parent.scope,
                    parent.offset,
                    seen,
                ) else {
                    seen.remove(&type_id);
                    return Lookup::Unknown;
                };
                self.lookup_class_member(parent_id, name, access_unit, offset, seen)
            }
        };
        seen.remove(&type_id);
        result
    }

    fn lookup_direct_member(
        &self,
        unit: &UnitTypeIndex,
        scope: LexicalScopeId,
        name: &str,
        defining_unit: &UnitKey,
        access_unit: &UnitKey,
        offset: usize,
    ) -> Lookup {
        let mut bindings: Vec<&Binding> = unit.scopes[scope.0]
            .bindings
            .iter()
            .filter(|binding| binding.name == name && binding.start <= offset)
            .collect();
        let Some(latest_start) = bindings.iter().map(|binding| binding.start).max() else {
            return Lookup::Absent;
        };
        bindings.retain(|binding| binding.start == latest_start);
        if bindings.len() != 1 {
            return Lookup::Unknown;
        }
        if !member_visible(bindings[0].visibility, defining_unit, access_unit) {
            return Lookup::Unknown;
        }
        match bindings[0].kind {
            BindingKind::Type(type_id) => Lookup::Type(type_id),
            BindingKind::Value => Lookup::Value,
            BindingKind::Unsupported => Lookup::Unknown,
        }
    }

    fn class_type_for_scope(&self, unit_key: &UnitKey, scope: LexicalScopeId) -> Option<TypeId> {
        let unit = self.units.get(unit_key)?;
        unit.types
            .iter()
            .enumerate()
            .find_map(|(local, declaration)| {
                let TypeDefinition::Class { class_scope, .. } = &declaration.definition else {
                    return None;
                };
                (*class_scope == scope).then_some(TypeId {
                    unit: unit.unit_index,
                    local,
                })
            })
    }

    fn resolve_type_id(&self, type_id: TypeId, seen: &mut HashMap<TypeId, ()>) -> Option<TypeId> {
        if seen.insert(type_id, ()).is_some() {
            return None;
        }
        let unit_key = type_id_unit_key(self, type_id);
        let unit = self.unit_for_type(type_id)?;
        let resolved = match &unit.types[type_id.local].definition {
            TypeDefinition::Class {
                incomplete: true, ..
            } => None,
            TypeDefinition::Class {
                incomplete: false, ..
            } if !unit.parser_incomplete && !unit.has_preprocessor_barrier => Some(type_id),
            TypeDefinition::Alias {
                target: Some(target),
            } => self.resolve_parts(&target.parts, &unit_key, target.scope, target.offset, seen),
            TypeDefinition::Class { .. }
            | TypeDefinition::Alias { target: None }
            | TypeDefinition::Unsupported => None,
        };
        seen.remove(&type_id);
        resolved
    }

    fn constructor_knowledge(&self, type_id: TypeId, access_unit: &UnitKey) -> Knowledge {
        self.constructor_knowledge_with_seen(type_id, access_unit, &mut HashMap::new())
    }

    fn constructor_knowledge_with_seen(
        &self,
        type_id: TypeId,
        access_unit: &UnitKey,
        seen: &mut HashMap<TypeId, ()>,
    ) -> Knowledge {
        if seen.insert(type_id, ()).is_some() {
            return Knowledge::Unknown;
        }
        let Some(unit) = self.unit_for_type(type_id) else {
            seen.remove(&type_id);
            return Knowledge::Unknown;
        };
        let TypeDefinition::Class {
            parent,
            create_member,
            incomplete,
            ..
        } = &unit.types[type_id.local].definition
        else {
            seen.remove(&type_id);
            return Knowledge::Unknown;
        };
        let result = if *incomplete || unit.parser_incomplete || unit.has_preprocessor_barrier {
            Knowledge::Unknown
        } else {
            match create_member {
                CreateMember::Constructor(visibility)
                    if member_visible(
                        *visibility,
                        &type_id_unit_key(self, type_id),
                        access_unit,
                    ) =>
                {
                    Knowledge::Yes
                }
                CreateMember::Constructor(_)
                | CreateMember::NonConstructor(_)
                | CreateMember::Ambiguous
                | CreateMember::Unknown => Knowledge::Unknown,
                CreateMember::None => match parent {
                    ParentType::None => Knowledge::No,
                    ParentType::Unsupported => Knowledge::Unknown,
                    ParentType::Reference(parent) => {
                        let Some(parent_id) = self.resolve_parts(
                            &parent.parts,
                            &type_id_unit_key(self, type_id),
                            parent.scope,
                            parent.offset,
                            seen,
                        ) else {
                            seen.remove(&type_id);
                            return Knowledge::Unknown;
                        };
                        self.constructor_knowledge_with_seen(parent_id, access_unit, seen)
                    }
                },
            }
        };
        seen.remove(&type_id);
        result
    }

    fn is_subtype(
        &self,
        raised: TypeId,
        handler: TypeId,
        seen: &mut HashMap<TypeId, ()>,
    ) -> TypeMatch {
        let mut current = raised;
        loop {
            if current == handler {
                return TypeMatch::Yes;
            }
            if seen.insert(current, ()).is_some() {
                return TypeMatch::Unknown;
            }
            let Some(unit) = self.unit_for_type(current) else {
                return TypeMatch::Unknown;
            };
            let TypeDefinition::Class {
                parent, incomplete, ..
            } = &unit.types[current.local].definition
            else {
                return TypeMatch::Unknown;
            };
            if *incomplete || unit.parser_incomplete || unit.has_preprocessor_barrier {
                return TypeMatch::Unknown;
            }
            let ParentType::Reference(parent) = parent else {
                return match parent {
                    ParentType::None => TypeMatch::No,
                    ParentType::Unsupported => TypeMatch::Unknown,
                    ParentType::Reference(_) => unreachable!(),
                };
            };
            let Some(parent_id) = self.resolve_parts(
                &parent.parts,
                &type_id_unit_key(self, current),
                parent.scope,
                parent.offset,
                seen,
            ) else {
                return TypeMatch::Unknown;
            };
            current = parent_id;
        }
    }

    fn match_subtype_bound(&self, bound: TypeId, handler: TypeId) -> TypeMatch {
        match self.is_subtype(bound, handler, &mut HashMap::new()) {
            TypeMatch::Yes => TypeMatch::Yes,
            TypeMatch::No => match self.is_subtype(handler, bound, &mut HashMap::new()) {
                TypeMatch::Yes => TypeMatch::Unknown,
                TypeMatch::No => TypeMatch::No,
                TypeMatch::Unknown => TypeMatch::Unknown,
            },
            TypeMatch::Unknown => TypeMatch::Unknown,
        }
    }

    fn class_scope(&self, type_id: TypeId) -> Option<LexicalScopeId> {
        let unit = self.unit_for_type(type_id)?;
        match &unit.types[type_id.local].definition {
            TypeDefinition::Class {
                class_scope,
                incomplete: false,
                ..
            } if !unit.parser_incomplete && !unit.has_preprocessor_barrier => Some(*class_scope),
            TypeDefinition::Class { .. }
            | TypeDefinition::Alias { .. }
            | TypeDefinition::Unsupported => None,
        }
    }

    fn lookup(
        &self,
        unit_key: &UnitKey,
        scope: LexicalScopeId,
        name: &str,
        offset: usize,
    ) -> Lookup {
        let mut current = Some(scope);
        while let Some(scope) = current {
            let Some(unit) = self.units.get(unit_key) else {
                return Lookup::Unknown;
            };
            if unit.has_binding(scope, name, offset) {
                return unit.lookup_direct_before(scope, name, offset);
            }
            if unit.scopes[scope.0].unresolved_owner {
                return Lookup::Unknown;
            }
            if let Some(owner_type) = unit.scopes[scope.0].owner_type {
                let owner_lookup = self.lookup_class_member(
                    owner_type,
                    name,
                    unit_key,
                    offset,
                    &mut HashMap::new(),
                );
                if !matches!(owner_lookup, Lookup::Absent) {
                    return owner_lookup;
                }
            }
            if let Some(type_id) = self.class_type_for_scope(unit_key, scope) {
                let member_lookup =
                    self.lookup_class_member(type_id, name, unit_key, offset, &mut HashMap::new());
                if !matches!(member_lookup, Lookup::Absent) {
                    return member_lookup;
                }
            }
            current = unit.scopes[scope.0].parent;
        }
        Lookup::Absent
    }

    fn has_binding(
        &self,
        unit_key: &UnitKey,
        scope: LexicalScopeId,
        name: &str,
        offset: usize,
    ) -> bool {
        self.units
            .get(unit_key)
            .is_some_and(|unit| unit.has_binding(scope, name, offset))
    }

    fn unit_for_type(&self, type_id: TypeId) -> Option<&UnitTypeIndex> {
        let unit_key = self.unit_keys.get(type_id.unit)?;
        self.units.get(unit_key)
    }
}

fn type_id_unit_key(index: &ExceptionTypeIndex, type_id: TypeId) -> UnitKey {
    index
        .unit_keys
        .get(type_id.unit)
        .cloned()
        .expect("type identity has a valid unit ordinal")
}

impl UnitTypeIndex {
    fn build(unit_index: usize, root: Node, source: &[u8]) -> Self {
        let mut index = Self {
            unit_index,
            module_name: extract_module_name(root, source),
            root_scope: LexicalScopeId(0),
            scopes: Vec::new(),
            types: Vec::new(),
            unsupported_ranges: Vec::new(),
            with_ranges: Vec::new(),
            has_preprocessor_barrier: contains_preprocessor_directive(root, source),
            pending_method_owners: Vec::new(),
            interface_range: find_module_section(root, "interface"),
            parser_incomplete: root.has_error(),
        };

        index.root_scope = index.new_scope(None, 0, source.len());
        index.collect_node(root, index.root_scope, source, false);
        index
    }

    fn collect_node(
        &mut self,
        node: Node,
        scope: LexicalScopeId,
        source: &[u8],
        conditional: bool,
    ) {
        let conditional = conditional || is_preprocessor_node(node, source);
        if is_preprocessor_node(node, source) {
            self.has_preprocessor_barrier = true;
            self.unsupported_ranges
                .push(node.start_byte()..node.end_byte());
        }
        if node.kind() == "with" {
            self.with_ranges.push(node.start_byte()..node.end_byte());
        }

        match node.kind() {
            "defProc" => self.collect_routine(node, scope, source, conditional),
            "lambda" => self.collect_lambda(node, scope, source, conditional),
            "for" | "foreach" => self.collect_loop(node, scope, source, conditional),
            "declTypes" => {
                self.collect_type_section(node, scope, source, conditional, Visibility::Default);
            }
            "declVars" | "declConsts" => {
                self.collect_value_section(node, scope, source, Visibility::Default);
            }
            "varDef" | "varAssignDef" => self.collect_inline_value_binding(node, scope, source),
            "declProc" => self.collect_proc_binding(node, scope, source, conditional),
            "exceptionHandler" => self.collect_exception_handler(node, scope, source, conditional),
            _ => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.collect_node(child, scope, source, conditional);
                }
            }
        }
    }

    fn collect_type_section(
        &mut self,
        node: Node,
        scope: LexicalScopeId,
        source: &[u8],
        conditional: bool,
        visibility: Visibility,
    ) -> CreateMember {
        let mut create_member = CreateMember::None;
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "declType" {
                if let Some(name) = child.child_by_field_name("name") {
                    let member = if name_parts(name, source)
                        .is_some_and(|parts| parts.len() == 1 && parts[0] == "create")
                    {
                        if conditional || self.has_generic_syntax(child) {
                            CreateMember::Unknown
                        } else {
                            CreateMember::NonConstructor(visibility)
                        }
                    } else {
                        CreateMember::None
                    };
                    merge_create_member(&mut create_member, member);
                }
                self.collect_type_declaration(child, scope, source, conditional, visibility);
            } else if child.kind() == "ppBlock" {
                merge_create_member(&mut create_member, CreateMember::Unknown);
            }
        }
        create_member
    }

    fn collect_value_section(
        &mut self,
        node: Node,
        scope: LexicalScopeId,
        source: &[u8],
        visibility: Visibility,
    ) -> CreateMember {
        let mut create_member = CreateMember::None;
        let mut cursor = node.walk();
        for declaration in node.named_children(&mut cursor) {
            if declaration.kind() == "ppBlock" {
                merge_create_member(&mut create_member, CreateMember::Unknown);
                continue;
            }
            if !matches!(declaration.kind(), "declVar" | "declConst") {
                continue;
            }
            for name_node in field_named_children(declaration, "name") {
                let name = canonical(node_text(name_node, source));
                self.add_binding_with_visibility(
                    scope,
                    name.clone(),
                    name_node.start_byte(),
                    BindingKind::Value,
                    visibility,
                );
                if name == "create" {
                    merge_create_member(
                        &mut create_member,
                        CreateMember::NonConstructor(visibility),
                    );
                }
            }
        }
        create_member
    }

    fn collect_inline_value_binding(&mut self, node: Node, scope: LexicalScopeId, source: &[u8]) {
        let Some(name) = direct_named_child(node, "identifier") else {
            return;
        };
        self.add_binding(
            scope,
            canonical(node_text(name, source)),
            name.start_byte(),
            BindingKind::Value,
        );
    }

    fn collect_type_declaration(
        &mut self,
        declaration: Node,
        scope: LexicalScopeId,
        source: &[u8],
        conditional: bool,
        visibility: Visibility,
    ) {
        let Some(name_node) = declaration.child_by_field_name("name") else {
            return;
        };

        let type_id = TypeId {
            unit: self.unit_index,
            local: self.types.len(),
        };
        self.types.push(TypeDeclaration {
            definition: TypeDefinition::Unsupported,
        });

        let simple_name =
            (name_node.kind() == "identifier").then(|| canonical(node_text(name_node, source)));
        let binding_name = simple_name.clone().or_else(|| {
            first_identifier(name_node).map(|identifier| canonical(node_text(identifier, source)))
        });
        self.add_binding_with_visibility(
            scope,
            binding_name.unwrap_or_default(),
            declaration.start_byte(),
            if conditional || simple_name.is_none() {
                BindingKind::Unsupported
            } else {
                BindingKind::Type(type_id)
            },
            visibility,
        );

        if conditional || simple_name.is_none() || self.has_generic_syntax(declaration) {
            return;
        }

        let Some(type_node) = field_named_children(declaration, "type")
            .into_iter()
            .next()
            .and_then(unwrap_type_node)
        else {
            return;
        };

        let definition = match type_node.kind() {
            "declClass" if direct_named_child(type_node, "kClass").is_some() => {
                let class_scope =
                    self.new_scope(Some(scope), type_node.start_byte(), type_node.end_byte());
                let incomplete = direct_named_child(type_node, "kEnd").is_none();
                let parent = self.class_parent(type_node, scope, source);
                let create_member = self.collect_class_members(type_node, class_scope, source);
                TypeDefinition::Class {
                    class_scope,
                    parent,
                    create_member,
                    incomplete,
                }
            }
            "declClass" | "declIntf" | "declHelper" | "declMetaClass" => {
                TypeDefinition::Unsupported
            }
            "typeref" | "typerefDot" | "typerefPtr" => TypeDefinition::Alias {
                target: type_reference_parts(type_node, source).map(|parts| TypeReference {
                    parts,
                    scope,
                    offset: type_node.start_byte(),
                }),
            },
            _ => TypeDefinition::Unsupported,
        };

        self.types[type_id.local].definition = definition;
    }

    fn class_parent(&self, class_node: Node, scope: LexicalScopeId, source: &[u8]) -> ParentType {
        let parents = field_named_children(class_node, "parent");
        let Some(parent) = parents.into_iter().next() else {
            return ParentType::None;
        };
        if field_named_children(class_node, "parent").len() > 1 {
            return ParentType::Unsupported;
        }
        type_reference_parts(parent, source)
            .map(|parts| {
                ParentType::Reference(TypeReference {
                    parts,
                    scope,
                    offset: parent.start_byte(),
                })
            })
            .unwrap_or(ParentType::Unsupported)
    }

    fn collect_class_members(
        &mut self,
        class_node: Node,
        scope: LexicalScopeId,
        source: &[u8],
    ) -> CreateMember {
        self.collect_class_member_children(class_node, scope, source, Visibility::Default)
    }

    fn collect_class_member_children(
        &mut self,
        node: Node,
        scope: LexicalScopeId,
        source: &[u8],
        visibility: Visibility,
    ) -> CreateMember {
        let mut create_member = CreateMember::None;
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "declTypes" => {
                    let nested = self.collect_type_section(child, scope, source, false, visibility);
                    merge_create_member(&mut create_member, nested);
                }
                "declVars" | "declConsts" => {
                    let nested = self.collect_value_section(child, scope, source, visibility);
                    merge_create_member(&mut create_member, nested);
                }
                "declField" | "declProp" => {
                    for name_node in field_named_children(child, "name") {
                        let name = canonical(node_text(name_node, source));
                        self.add_binding_with_visibility(
                            scope,
                            name.clone(),
                            name_node.start_byte(),
                            BindingKind::Value,
                            visibility,
                        );
                        if name == "create" {
                            merge_create_member(
                                &mut create_member,
                                CreateMember::NonConstructor(visibility),
                            );
                        }
                    }
                }
                "declProc" => {
                    let is_create =
                        name_parts(child.child_by_field_name("name").unwrap_or(child), source)
                            .is_some_and(|parts| parts.len() == 1 && parts[0] == "create");
                    self.collect_proc_binding_with_visibility(
                        child, scope, source, false, visibility,
                    );
                    if is_create {
                        let member = if is_create_constructor(child, source) {
                            CreateMember::Constructor(visibility)
                        } else {
                            CreateMember::NonConstructor(visibility)
                        };
                        merge_create_member(&mut create_member, member);
                    }
                }
                "declSection" => {
                    let section_visibility = visibility_of_section(child);
                    let nested = self.collect_class_member_children(
                        child,
                        scope,
                        source,
                        section_visibility,
                    );
                    merge_create_member(&mut create_member, nested);
                }
                "ppBlock" | "ppDeclSection" => {
                    merge_create_member(&mut create_member, CreateMember::Unknown)
                }
                _ => continue,
            }
        }
        create_member
    }

    fn collect_proc_binding(
        &mut self,
        declaration: Node,
        scope: LexicalScopeId,
        source: &[u8],
        unsupported: bool,
    ) {
        self.collect_proc_binding_with_visibility(
            declaration,
            scope,
            source,
            unsupported,
            Visibility::Default,
        );
    }

    fn collect_proc_binding_with_visibility(
        &mut self,
        declaration: Node,
        scope: LexicalScopeId,
        source: &[u8],
        unsupported: bool,
        visibility: Visibility,
    ) {
        let Some(name_node) = declaration.child_by_field_name("name") else {
            return;
        };
        let Some(parts) = name_parts(name_node, source) else {
            return;
        };
        if parts.len() == 1 {
            self.add_binding_with_visibility(
                scope,
                parts[0].clone(),
                declaration.start_byte(),
                if unsupported {
                    BindingKind::Unsupported
                } else {
                    BindingKind::Value
                },
                visibility,
            );
        }
    }

    fn collect_routine(
        &mut self,
        routine: Node,
        enclosing_scope: LexicalScopeId,
        source: &[u8],
        conditional: bool,
    ) {
        let Some(header) = routine
            .child_by_field_name("header")
            .or_else(|| direct_named_child(routine, "declProc"))
        else {
            return;
        };

        self.collect_proc_binding(header, enclosing_scope, source, conditional);

        let routine_scope = self.new_scope(
            Some(enclosing_scope),
            routine.start_byte(),
            routine.end_byte(),
        );
        if let Some(args) = field_named_children(header, "args").into_iter().next() {
            let mut cursor = args.walk();
            for argument in args.named_children(&mut cursor) {
                if argument.kind() != "declArg" {
                    continue;
                }
                for name in field_named_children(argument, "name") {
                    self.add_binding(
                        routine_scope,
                        canonical(node_text(name, source)),
                        name.start_byte(),
                        BindingKind::Value,
                    );
                }
            }
        }

        let Some(name_node) = header.child_by_field_name("name") else {
            return;
        };

        let owner_parts = name_parts(name_node, source)
            .and_then(|parts| (parts.len() > 1).then(|| parts[..parts.len() - 1].to_vec()));
        let has_method_owner = owner_parts.is_some()
            || (name_node.kind() == "genericDot" && name_parts(name_node, source).is_none());
        if has_method_owner {
            self.pending_method_owners.push(PendingMethodOwner {
                routine_scope,
                owner_parts: if owner_parts.is_some() {
                    owner_parts
                } else {
                    None
                },
                enclosing_scope,
                offset: routine.start_byte(),
            });
            self.add_binding(
                routine_scope,
                "self".to_string(),
                routine.start_byte(),
                BindingKind::Value,
            );
        }

        if direct_named_child(header, "kFunction").is_some() {
            self.add_binding(
                routine_scope,
                "result".to_string(),
                routine.start_byte(),
                BindingKind::Value,
            );
            if let Some(parts) = name_parts(name_node, source) {
                if let Some(result_name) = parts.last() {
                    self.add_binding(
                        routine_scope,
                        result_name.clone(),
                        routine.start_byte(),
                        BindingKind::Value,
                    );
                }
            }
        }

        for local in field_named_children(routine, "local") {
            self.collect_node(local, routine_scope, source, conditional);
        }

        if let Some(body) = routine.child_by_field_name("body") {
            self.collect_node(body, routine_scope, source, conditional);
        }
    }

    fn collect_lambda(
        &mut self,
        lambda: Node,
        enclosing_scope: LexicalScopeId,
        source: &[u8],
        conditional: bool,
    ) {
        let lambda_scope = self.new_scope(
            Some(enclosing_scope),
            lambda.start_byte(),
            lambda.end_byte(),
        );

        if let Some(args) = field_named_children(lambda, "args").into_iter().next() {
            let mut cursor = args.walk();
            for argument in args.named_children(&mut cursor) {
                if argument.kind() != "declArg" {
                    continue;
                }
                for name in field_named_children(argument, "name") {
                    self.add_binding(
                        lambda_scope,
                        canonical(node_text(name, source)),
                        name.start_byte(),
                        BindingKind::Value,
                    );
                }
            }
        }

        for local in field_named_children(lambda, "local") {
            self.collect_node(local, lambda_scope, source, conditional);
        }

        if let Some(body) = lambda.child_by_field_name("body") {
            self.collect_node(body, lambda_scope, source, conditional);
        }
    }

    fn collect_loop(
        &mut self,
        loop_node: Node,
        enclosing_scope: LexicalScopeId,
        source: &[u8],
        conditional: bool,
    ) {
        let loop_scope = self.new_scope(
            Some(enclosing_scope),
            loop_node.start_byte(),
            loop_node.end_byte(),
        );
        let mut cursor = loop_node.walk();
        for child in loop_node.named_children(&mut cursor) {
            self.collect_node(child, loop_scope, source, conditional);
        }
    }

    fn collect_exception_handler(
        &mut self,
        handler: Node,
        enclosing_scope: LexicalScopeId,
        source: &[u8],
        conditional: bool,
    ) {
        let Some(body) = handler.child_by_field_name("body") else {
            return;
        };
        let handler_scope =
            self.new_scope(Some(enclosing_scope), body.start_byte(), handler.end_byte());

        if let Some(variable) = handler.child_by_field_name("variable") {
            if variable.kind() == "identifier" {
                self.add_binding(
                    handler_scope,
                    canonical(node_text(variable, source)),
                    body.start_byte(),
                    if conditional {
                        BindingKind::Unsupported
                    } else {
                        BindingKind::Value
                    },
                );
            }
        }

        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            self.collect_node(child, handler_scope, source, conditional);
        }
    }

    fn new_scope(
        &mut self,
        parent: Option<LexicalScopeId>,
        start: usize,
        end: usize,
    ) -> LexicalScopeId {
        let id = LexicalScopeId(self.scopes.len());
        self.scopes.push(LexicalScope {
            parent,
            start,
            end,
            bindings: Vec::new(),
            owner_type: None,
            unresolved_owner: false,
        });
        id
    }

    fn add_binding(
        &mut self,
        scope: LexicalScopeId,
        name: String,
        start: usize,
        kind: BindingKind,
    ) {
        self.add_binding_with_visibility(scope, name, start, kind, Visibility::Default);
    }

    fn add_binding_with_visibility(
        &mut self,
        scope: LexicalScopeId,
        name: String,
        start: usize,
        kind: BindingKind,
        visibility: Visibility,
    ) {
        if name.is_empty() {
            return;
        }
        self.scopes[scope.0].bindings.push(Binding {
            name,
            start,
            kind,
            visibility,
        });
    }

    fn scope_at(&self, offset: usize) -> LexicalScopeId {
        let mut selected = self.root_scope;
        for (index, scope) in self.scopes.iter().enumerate() {
            if scope.start > offset || offset >= scope.end {
                continue;
            }
            let selected_scope = &self.scopes[selected.0];
            if scope.start > selected_scope.start
                || (scope.start == selected_scope.start && scope.end < selected_scope.end)
            {
                selected = LexicalScopeId(index);
            }
        }
        selected
    }

    fn lookup_direct_before(&self, scope: LexicalScopeId, name: &str, offset: usize) -> Lookup {
        let bindings: Vec<&Binding> = self.scopes[scope.0]
            .bindings
            .iter()
            .filter(|binding| binding.name == name && binding.start <= offset)
            .collect();
        let Some(latest_start) = bindings.iter().map(|binding| binding.start).max() else {
            return Lookup::Unknown;
        };
        let latest: Vec<&Binding> = bindings
            .into_iter()
            .filter(|binding| binding.start == latest_start)
            .collect();
        if latest.len() != 1 {
            return Lookup::Unknown;
        }
        match latest[0].kind {
            BindingKind::Type(type_id) => Lookup::Type(type_id),
            BindingKind::Value | BindingKind::Unsupported => Lookup::Value,
        }
    }

    fn has_binding(&self, scope: LexicalScopeId, name: &str, offset: usize) -> bool {
        self.scopes[scope.0]
            .bindings
            .iter()
            .any(|binding| binding.name == name && binding.start <= offset)
    }

    fn is_unsupported_at(&self, offset: usize) -> bool {
        self.unsupported_ranges
            .iter()
            .any(|range| range.start <= offset && offset < range.end)
    }

    /// Dotted names can still begin with an implicit member of a `with`
    /// receiver, so their length does not establish explicit qualification.
    fn is_with_implicit_reference(&self, parts: &[String], offset: usize) -> bool {
        !parts.is_empty()
            && self
                .with_ranges
                .iter()
                .any(|range| range.start <= offset && offset < range.end)
    }

    fn has_generic_syntax(&self, node: Node) -> bool {
        if matches!(node.kind(), "genericTpl" | "typerefTpl" | "kGeneric")
            || is_preprocessor_kind(node.kind())
        {
            return true;
        }
        let mut cursor = node.walk();
        let result = node
            .named_children(&mut cursor)
            .any(|child| self.has_generic_syntax(child));
        result
    }
}

fn extract_module_name(root: Node, source: &[u8]) -> Option<String> {
    let module = direct_named_child_any(root, &["unit", "program", "library"])?;
    let module_name = direct_named_child_any(module, &["moduleName"])?;
    Some(canonical(node_text(module_name, source)))
}

fn find_module_section(root: Node, kind: &str) -> Option<Range<usize>> {
    let module = direct_named_child_any(root, &["unit", "program", "library"])?;
    let section = direct_named_child(module, kind)?;
    Some(section.start_byte()..section.end_byte())
}

fn import_section(root: Node, offset: usize) -> (ImportSection, usize) {
    let Some(mut node) = root.descendant_for_byte_range(offset, offset.saturating_add(1)) else {
        return (ImportSection::Module, root.start_byte());
    };

    loop {
        if node.kind() == "declUses" {
            let mut ancestor = node.parent();
            while let Some(parent) = ancestor {
                match parent.kind() {
                    "interface" => {
                        return (ImportSection::Interface, parent.start_byte());
                    }
                    "implementation" => {
                        return (ImportSection::Implementation, parent.start_byte());
                    }
                    "unit" | "program" | "library" | "root" => break,
                    _ => ancestor = parent.parent(),
                }
            }
            return (ImportSection::Module, node.start_byte());
        }
        let Some(parent) = node.parent() else {
            break;
        };
        node = parent;
    }

    (ImportSection::Module, root.start_byte())
}

fn contains_preprocessor_directive(node: Node, source: &[u8]) -> bool {
    if is_preprocessor_node(node, source) {
        return true;
    }
    let mut cursor = node.walk();
    let result = node
        .children(&mut cursor)
        .any(|child| contains_preprocessor_directive(child, source));
    result
}

fn constructor_parts(node: Node, source: &[u8]) -> Option<Vec<String>> {
    let exception = node.child_by_field_name("exception")?;
    match exception.kind() {
        "exprCall" => constructor_call_parts(exception, source),
        "exprDot" => constructor_parts_from_entity(exception, source),
        "exprParens" => {
            let mut cursor = exception.walk();
            let parts =
                exception
                    .named_children(&mut cursor)
                    .find_map(|child| match child.kind() {
                        "exprCall" => constructor_call_parts(child, source),
                        "exprDot" => constructor_parts_from_entity(child, source),
                        _ => None,
                    });
            parts
        }
        _ => None,
    }
}

fn constructor_call_parts(node: Node, source: &[u8]) -> Option<Vec<String>> {
    let entity = node.child_by_field_name("entity")?;
    constructor_parts_from_entity(entity, source)
}

fn constructor_parts_from_entity(node: Node, source: &[u8]) -> Option<Vec<String>> {
    let mut parts = name_parts(node, source)?;
    let constructor = parts.pop()?;
    constructor.eq_ignore_ascii_case("create").then_some(parts)
}

fn type_reference_parts(node: Node, source: &[u8]) -> Option<Vec<String>> {
    match node.kind() {
        "typeref" | "type" => {
            let mut cursor = node.walk();
            let children: Vec<Node> = node.named_children(&mut cursor).collect();
            if children.len() != 1 {
                return None;
            }
            type_reference_parts(children[0], source)
        }
        "typerefDot" => {
            let lhs = node.child_by_field_name("lhs")?;
            let rhs = node.child_by_field_name("rhs")?;
            let mut parts = type_reference_parts(lhs, source)?;
            parts.extend(type_reference_parts(rhs, source)?);
            Some(parts)
        }
        "typerefPtr" | "typerefTpl" | "genericDot" | "genericTpl" => None,
        "identifier" => Some(vec![canonical(node_text(node, source))]),
        _ => None,
    }
}

fn name_parts(node: Node, source: &[u8]) -> Option<Vec<String>> {
    match node.kind() {
        "identifier" => Some(vec![canonical(node_text(node, source))]),
        "exprDot" | "genericDot" => {
            let lhs = node.child_by_field_name("lhs")?;
            let rhs = node.child_by_field_name("rhs")?;
            let mut parts = name_parts(lhs, source)?;
            parts.extend(name_parts(rhs, source)?);
            Some(parts)
        }
        "genericTpl" => None,
        _ => None,
    }
}

fn is_create_constructor(node: Node, source: &[u8]) -> bool {
    direct_named_child(node, "kConstructor").is_some()
        && node
            .child_by_field_name("name")
            .and_then(|name| name_parts(name, source))
            .is_some_and(|parts| parts.len() == 1 && parts[0] == "create")
}

fn visibility_of_section(node: Node) -> Visibility {
    let mut strict = false;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "kStrict" => strict = true,
            "kPrivate" => {
                return if strict {
                    Visibility::StrictPrivate
                } else {
                    Visibility::Private
                }
            }
            "kProtected" => {
                return if strict {
                    Visibility::StrictProtected
                } else {
                    Visibility::Protected
                }
            }
            "kPublic" => return Visibility::Public,
            "kPublished" => return Visibility::Published,
            _ => continue,
        }
    }
    Visibility::Unknown
}

fn merge_create_member(current: &mut CreateMember, incoming: CreateMember) {
    *current = match (*current, incoming) {
        (CreateMember::Unknown, _) | (_, CreateMember::Unknown) => CreateMember::Unknown,
        (CreateMember::Ambiguous, _) | (_, CreateMember::Ambiguous) => CreateMember::Ambiguous,
        (CreateMember::None, member) | (member, CreateMember::None) => member,
        (CreateMember::Constructor(left), CreateMember::Constructor(right)) if left == right => {
            CreateMember::Constructor(left)
        }
        (CreateMember::NonConstructor(left), CreateMember::NonConstructor(right))
            if left == right =>
        {
            CreateMember::NonConstructor(left)
        }
        _ => CreateMember::Ambiguous,
    };
}

fn member_visible(visibility: Visibility, defining_unit: &UnitKey, access_unit: &UnitKey) -> bool {
    match visibility {
        Visibility::Default | Visibility::Public | Visibility::Published => true,
        // Delphi's ordinary private and protected visibility is unit-scoped.
        // Strict visibility and cross-unit protected access require
        // class-context reasoning that this intentionally conservative index
        // does not model.
        Visibility::Private | Visibility::Protected => defining_unit == access_unit,
        Visibility::StrictPrivate | Visibility::StrictProtected | Visibility::Unknown => false,
    }
}

fn unwrap_type_node<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    if node.kind() != "type" {
        return Some(node);
    }
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    (children.len() == 1).then_some(children[0])
}

fn field_named_children<'tree>(node: Node<'tree>, field: &str) -> Vec<Node<'tree>> {
    let mut cursor = node.walk();
    node.children_by_field_name(field, &mut cursor)
        .filter(|child| child.is_named())
        .collect()
}

fn direct_named_child<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    direct_named_child_any(node, &[kind])
}

fn direct_named_child_any<'tree>(node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let result = node
        .named_children(&mut cursor)
        .find(|child| kinds.contains(&child.kind()));
    result
}

fn first_identifier<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    if node.kind() == "identifier" {
        return Some(node);
    }
    let mut cursor = node.walk();
    let result = node.named_children(&mut cursor).find_map(first_identifier);
    result
}

fn node_text(node: Node, source: &[u8]) -> String {
    std::str::from_utf8(&source[node.start_byte()..node.end_byte()])
        .unwrap_or("")
        .to_string()
}

fn canonical(name: String) -> String {
    name.to_ascii_lowercase()
}
