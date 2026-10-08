//! Delphi record field storage (not class instance layouts).
//! See ABI fixture references `record_fields`, `record_variants`, `layout_options`.
use super::bindings::{IndexExpr, TypeExpr, same_name, storage_type};
use super::declarations::{index_expr, limit, type_expr, visit_index_payload, visit_type_payload};
use super::*;
use crate::conditional::Truth;
use pascal_project::{CompilerVersion, ConditionalFact, LayoutPlatform, LayoutSettings};
use std::mem::size_of;
use tree_sitter::Node;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Record {
    packed: bool,
    valid: bool,
    settings: LayoutSettings,
    site: BindingSite,
    provenance: Vec<SourceAnchor>,
    parts: Vec<Part>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Part {
    Field {
        name: String,
        ty: TypeExpr,
        grouped: bool,
        settings: LayoutSettings,
    },
    Variant {
        name: Option<String>,
        ty: TypeExpr,
        settings: LayoutSettings,
        alternatives: Vec<Alternative>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Alternative {
    labels: Vec<IndexExpr>,
    parts: Vec<Part>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct LayoutChange {
    pub(super) logical: usize,
    pub(super) sequence: Sequence,
    pub(super) settings: LayoutSettings,
}

pub(super) fn is_record(
    node: Node<'_>,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        budget.charge(1, 0, 0)?;
        if child.kind() == "kRecord" {
            return Ok(true);
        }
    }
    Ok(false)
}

struct Admission<'a, 'b, 'c> {
    state: &'a CompileTimeState,
    prefix: usize,
    bytes: &'b [u8],
    definitions: &'c mut usize,
    fields: &'c mut usize,
    variants: &'c mut usize,
    names: Vec<&'b str>,
    valid: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn parse_record(
    state: &CompileTimeState,
    prefix: usize,
    node: Node<'_>,
    bytes: &[u8],
    depth: usize,
    definitions: &mut usize,
    fields: &mut usize,
    variants: &mut usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Record, IncompleteReason> {
    let mut a = Admission {
        state,
        prefix,
        bytes,
        definitions,
        fields,
        variants,
        names: Vec::new(),
        valid: !node.has_error(),
    };
    let mut packed = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        budget.charge(1, 0, 0)?;
        packed |= child.kind() == "kPacked";
    }
    let result = a.parts(node, depth + 1, budget);
    budget.release(a.names.len() * size_of::<&str>());
    let parts = result?;
    let logical = state.node_logical_position(node.start_byte(), prefix)?;
    let settings = state.settings_at_logical(logical, budget)?.clone();
    let anchor = state.anchor_at(logical, 1, budget)?;
    let mut site = state
        .site_at(&anchor.source, anchor.occurrence, anchor.range.start)
        .ok_or(IncompleteReason::MalformedSource)?;
    // The enclosing declaration is admitted immediately after this parse. Include
    // it for recursive-by-value cycle detection, never later declarations.
    site.sequence = Sequence(
        state
            .sequence
            .0
            .checked_add(1)
            .ok_or(IncompleteReason::MalformedSource)?,
    );
    budget.release(size_of::<SourceAnchor>() + anchor.source.as_str().len());
    let end = logical
        .checked_add(node.end_byte() - node.start_byte())
        .ok_or(IncompleteReason::MalformedSource)?;
    let provenance = state.provenance(logical, end, budget)?;
    Ok(Record {
        packed,
        valid: a.valid,
        settings,
        site,
        provenance,
        parts,
    })
}

impl Admission<'_, '_, '_> {
    fn settings(
        &self,
        node: Node<'_>,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<LayoutSettings, IncompleteReason> {
        let logical = self
            .state
            .node_logical_position(node.start_byte(), self.prefix)?;
        Ok(self.state.settings_at_logical(logical, budget)?.clone())
    }
    fn name(
        &mut self,
        node: Node<'_>,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<String, IncompleteReason> {
        *self.fields = self
            .fields
            .checked_add(1)
            .ok_or(IncompleteReason::MalformedSource)?;
        limit(*self.fields, budget.limits().max_fields, "max_fields")?;
        let text = node
            .utf8_text(self.bytes)
            .map_err(|_| IncompleteReason::MalformedSource)?;
        if node.kind() != "identifier" || node.is_missing() || node.has_error() {
            self.valid = false;
        }
        for name in &self.names {
            budget.charge(1, name.len().min(text.len()), 0)?;
            if same_name(name, text) {
                self.valid = false;
            }
        }
        budget.charge(1, text.len(), text.len() + size_of::<&str>())?;
        self.names.push(text);
        Ok(text.to_ascii_lowercase())
    }

    fn ty(
        &mut self,
        node: Node<'_>,
        depth: usize,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<TypeExpr, IncompleteReason> {
        let n = node.child_by_field_name("type");
        let raw = n.and_then(|n| n.utf8_text(self.bytes).ok()).unwrap_or("");
        type_expr(
            self.state,
            self.prefix,
            n,
            self.bytes,
            raw,
            false,
            depth,
            self.definitions,
            self.fields,
            self.variants,
            budget,
        )
    }

    fn parts(
        &mut self,
        node: Node<'_>,
        depth: usize,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Vec<Part>, IncompleteReason> {
        limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
        let mut parts = Vec::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            budget.charge(1, 0, 0)?;
            match child.kind() {
                "declField" => {
                    let mut c = child.walk();
                    let mut count = 0;
                    for name in child.children_by_field_name("name", &mut c) {
                        budget.charge(1, 0, 0)?;
                        if name.is_named() {
                            count += 1;
                        }
                    }
                    if count == 0 {
                        self.valid = false;
                    }
                    let mut c = child.walk();
                    for name in child.children_by_field_name("name", &mut c) {
                        budget.charge(1, 0, 0)?;
                        if !name.is_named() {
                            continue;
                        }
                        budget.charge(1, 0, size_of::<Part>())?;
                        let name = self.name(name, budget)?;
                        let ty = self.ty(child, depth + 1, budget)?;
                        parts.push(Part::Field {
                            name,
                            ty,
                            grouped: count > 1,
                            settings: self.settings(child, budget)?,
                        });
                    }
                }
                "declVariant" => {
                    budget.charge(1, 0, size_of::<Part>())?;
                    let name = child
                        .child_by_field_name("name")
                        .map(|n| self.name(n, budget))
                        .transpose()?;
                    let ty = self.ty(child, depth + 1, budget)?;
                    let mut alternatives = Vec::new();
                    let mut c = child.walk();
                    for clause in child.named_children(&mut c) {
                        budget.charge(1, 0, 0)?;
                        if clause.kind() != "declVariantClause" {
                            continue;
                        }
                        *self.variants = self
                            .variants
                            .checked_add(1)
                            .ok_or(IncompleteReason::MalformedSource)?;
                        limit(
                            *self.variants,
                            budget.limits().max_variant_alternatives,
                            "max_variant_alternatives",
                        )?;
                        let mut labels = Vec::new();
                        let mut lc = clause.walk();
                        for n in clause.named_children(&mut lc) {
                            budget.charge(1, 0, 0)?;
                            if n.kind() == "caseLabel" {
                                let mut cursor = n.walk();
                                for label in n.named_children(&mut cursor) {
                                    budget.charge(1, 0, size_of::<IndexExpr>())?;
                                    if label.kind() == "comment" {
                                        budget.release(size_of::<IndexExpr>());
                                        continue;
                                    }
                                    if label.kind().starts_with("pp") {
                                        self.valid = false;
                                    }
                                    labels.push(index_expr(label, self.bytes, budget)?);
                                }
                            }
                        }
                        if labels.is_empty() {
                            self.valid = false;
                        }
                        budget.charge(1, 0, size_of::<Alternative>())?;
                        alternatives.push(Alternative {
                            labels,
                            parts: self.parts(clause, depth + 1, budget)?,
                        });
                    }
                    if alternatives.is_empty() {
                        self.valid = false;
                    }
                    parts.push(Part::Variant {
                        name,
                        ty,
                        alternatives,
                        settings: self.settings(child, budget)?,
                    });
                }
                "declSection" => {
                    let more = self.parts(child, depth + 1, budget)?;
                    // Account the destination before extending; release old vector's slots.
                    budget.charge(1, 0, more.len() * size_of::<Part>())?;
                    let bytes = more.len() * size_of::<Part>();
                    parts.extend(more);
                    budget.release(bytes);
                }
                // Only parser-identified non-instance declarations may be ignored.
                "declVars" => {
                    if !proven_static_section(child, budget)? {
                        self.valid = false;
                    }
                }
                "declProc" | "declProp" | "comment" | "caseLabel" => {}
                kind if kind.starts_with('k') => {}
                _ => self.valid = false,
            }
        }
        Ok(parts)
    }
}

/// Nominal element validity is distinct from storage/stride proof. Reuse the
/// array graph's shape validation so invalid labels cannot hide behind a dynamic
/// reference, including nested anonymous records and definition-site aliases.
pub(super) fn record_element_valid<'a>(
    state: &'a CompileTimeState,
    record: &Record,
    visited: &mut Vec<&'a DefinitionKey>,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    fn parts<'a>(
        state: &'a CompileTimeState,
        site: &BindingSite,
        items: &[Part],
        visited: &mut Vec<&'a DefinitionKey>,
        depth: usize,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<bool, IncompleteReason> {
        limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
        for part in items {
            budget.charge(1, 0, 0)?;
            match part {
                Part::Field { ty, .. } => {
                    if !super::arrays::element_valid(state, site, ty, visited, depth + 1, budget)? {
                        return Ok(false);
                    }
                }
                Part::Variant {
                    ty, alternatives, ..
                } => {
                    if !super::arrays::validate_variant_labels(
                        state,
                        site,
                        ty,
                        alternatives.iter().flat_map(|a| a.labels.iter()),
                        depth + 1,
                        budget,
                    )? {
                        return Ok(false);
                    }
                    for alt in alternatives {
                        if !parts(state, site, &alt.parts, visited, depth + 1, budget)? {
                            return Ok(false);
                        }
                    }
                }
            }
        }
        Ok(true)
    }
    if !record.valid {
        return Ok(false);
    }
    parts(state, &record.site, &record.parts, visited, depth, budget)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn record_layout<'a>(
    state: &'a CompileTimeState,
    _site: &BindingSite,
    record: &Record,
    _settings: &LayoutSettings,
    visited: &mut Vec<&'a DefinitionKey>,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Fact<StorageLayout>, IncompleteReason> {
    let settings = &record.settings;
    let site = &record.site;
    if !record.valid || record.parts.is_empty() {
        return Ok(Fact::Unknown(UnknownReason::UnsupportedLayout));
    }
    // Reuse the scalar owner's exact target/version admission; never host ABI.
    match builtin_layout(BuiltinType::Pointer, &state.context, settings) {
        Fact::Known(_) => {}
        other => return Ok(other),
    }
    let cap = if record.packed {
        1
    } else {
        match settings.record_alignment {
            Some(cap @ (1 | 2 | 4 | 8)) => u32::from(cap),
            _ => return Ok(Fact::Unknown(UnknownReason::UnsupportedLayout)),
        }
    };
    if !record.packed
        && !uniform_alignment(&record.parts, settings.record_alignment, depth + 1, budget)?
    {
        return Ok(Fact::Unknown(UnknownReason::UnsupportedLayout));
    }
    layout_parts(state, site, &record.parts, cap, visited, depth + 1, budget)
}

fn uniform_alignment(
    parts: &[Part],
    expected: Option<u8>,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    for part in parts {
        budget.charge(1, 0, 0)?;
        match part {
            Part::Field { settings, .. } if settings.record_alignment != expected => {
                return Ok(false);
            }
            Part::Variant {
                settings,
                alternatives,
                ..
            } => {
                if settings.record_alignment != expected {
                    return Ok(false);
                }
                for alt in alternatives {
                    if !uniform_alignment(&alt.parts, expected, depth + 1, budget)? {
                        return Ok(false);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn layout_parts<'a>(
    state: &'a CompileTimeState,
    site: &BindingSite,
    parts: &[Part],
    cap: u32,
    visited: &mut Vec<&'a DefinitionKey>,
    depth: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Fact<StorageLayout>, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    let mut offset = 0u64;
    let mut alignment = 1;
    for part in parts {
        budget.charge(1, 0, 0)?;
        let storage = match part {
            Part::Field {
                ty,
                grouped,
                settings,
                ..
            } => {
                let fact = storage_type(state, site, ty, settings, visited, depth + 1, budget)?;
                let Fact::Known(storage) = fact else {
                    return Ok(fact);
                };
                // Legacy grouped-field packing is not modeled without verified final-
                // alignment evidence. It cannot affect singly declared/byte fields.
                if *grouped
                    && cap > 1
                    && storage.alignment > 1
                    && settings.old_type_layout != ConditionalFact::False
                {
                    return Ok(Fact::Unknown(UnknownReason::UnsupportedLayout));
                }
                storage
            }
            Part::Variant {
                name,
                ty,
                alternatives,
                settings,
            } => {
                if !super::arrays::validate_variant_labels(
                    state,
                    site,
                    ty,
                    alternatives.iter().flat_map(|a| a.labels.iter()),
                    depth + 1,
                    budget,
                )? {
                    return Ok(Fact::Unknown(UnknownReason::UnsupportedLayout));
                }
                if name.is_some() {
                    let fact = storage_type(state, site, ty, settings, visited, depth + 1, budget)?;
                    let Fact::Known(tag) = fact else {
                        return Ok(fact);
                    };
                    let a = tag.alignment.min(cap);
                    let Some(end) =
                        checked_align_up(offset, a).and_then(|n| n.checked_add(tag.size))
                    else {
                        return Ok(Fact::Unknown(UnknownReason::Overflow));
                    };
                    offset = end;
                    alignment = alignment.max(a);
                }
                let mut union = StorageLayout {
                    size: 0,
                    alignment: 1,
                };
                for alt in alternatives {
                    let fact =
                        layout_parts(state, site, &alt.parts, cap, visited, depth + 1, budget)?;
                    let Fact::Known(storage) = fact else {
                        return Ok(fact);
                    };
                    union.size = union.size.max(storage.size);
                    union.alignment = union.alignment.max(storage.alignment);
                }
                union
            }
        };
        let a = storage.alignment.min(cap);
        let Some(end) = checked_align_up(offset, a).and_then(|n| n.checked_add(storage.size))
        else {
            return Ok(Fact::Unknown(UnknownReason::Overflow));
        };
        offset = end;
        alignment = alignment.max(a);
    }
    Ok(match checked_align_up(offset, alignment) {
        Some(size) => Fact::Known(StorageLayout { size, alignment }),
        None => Fact::Unknown(UnknownReason::Overflow),
    })
}

pub(super) fn visit_payload(
    record: &Record,
    visit: &mut dyn FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    fn visit_parts(
        parts: &[Part],
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        for part in parts {
            visit(size_of::<Part>())?;
            match part {
                Part::Field { name, ty, .. } => {
                    visit(name.len())?;
                    visit_type_payload(ty, visit)?;
                }
                Part::Variant {
                    name,
                    ty,
                    alternatives,
                    ..
                } => {
                    if let Some(name) = name {
                        visit(name.len())?;
                    }
                    visit_type_payload(ty, visit)?;
                    for alt in alternatives {
                        visit(size_of::<Alternative>())?;
                        for label in &alt.labels {
                            visit(size_of::<IndexExpr>())?;
                            visit_index_payload(label, visit)?;
                        }
                        visit_parts(&alt.parts, visit)?;
                    }
                }
            }
        }
        Ok(())
    }
    for anchor in &record.provenance {
        visit(size_of::<SourceAnchor>())?;
        visit(anchor.source.as_str().len())?;
    }
    visit_parts(&record.parts, visit)
}

fn proven_static_section(
    node: Node<'_>,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    let mut class = false;
    let mut var = false;
    let mut declarations = 0;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        budget.charge(1, 0, 0)?;
        if child.has_error() || child.is_missing() {
            return Ok(false);
        }
        match child.kind() {
            "kClass" => class = true,
            "kVar" => var = true,
            "declVar" => {
                if child.child_by_field_name("type").is_none()
                    || !unconditional_shape(child, budget)?
                {
                    return Ok(false);
                }
                let mut c = child.walk();
                let mut names = 0;
                for name in child.children_by_field_name("name", &mut c) {
                    budget.charge(1, 0, 0)?;
                    if !name.is_named() {
                        continue;
                    }
                    if name.kind() != "identifier" || name.is_missing() || name.has_error() {
                        return Ok(false);
                    }
                    names += 1;
                }
                if names == 0 {
                    return Ok(false);
                }
                declarations += 1;
            }
            "comment" => {}
            _ => return Ok(false),
        }
    }
    Ok(class && var && declarations > 0)
}

/// A class marker does not prove a conditional/recovered variable declaration.
/// Traverse without a second parser or an allocation proportional to nesting.
fn unconditional_shape(
    node: Node<'_>,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<bool, IncompleteReason> {
    let mut cursor = node.walk();
    loop {
        budget.charge(1, 0, 0)?;
        let node = cursor.node();
        if node.is_missing() || node.has_error() || node.kind().starts_with("pp") {
            return Ok(false);
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            budget.charge(1, 0, 0)?;
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(true);
            }
        }
    }
}

impl CompileTimeState {
    pub(super) fn settings_at_sequence(
        &self,
        sequence: Sequence,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<&LayoutSettings, IncompleteReason> {
        budget.charge(self.settings_history.len(), 0, 0)?;
        Ok(self
            .settings_history
            .iter()
            .rev()
            .find(|c| c.sequence.0 <= sequence.0)
            .map_or(&self.context.layout.defaults, |c| &c.settings))
    }
    pub(super) fn settings_at_logical(
        &self,
        logical: usize,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<&LayoutSettings, IncompleteReason> {
        budget.charge(self.settings_history.len(), 0, 0)?;
        Ok(self
            .settings_history
            .iter()
            .rev()
            .find(|c| c.logical <= logical)
            .map_or(&self.context.layout.defaults, |c| &c.settings))
    }
    /// Apply an already extracted directive body. Conditional/include orchestration
    /// remains the caller's responsibility. No implicit compiler defaults are added.
    pub fn apply_layout_directive(
        &mut self,
        body: &str,
        activity: Truth,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        if self.finished_for_directive() {
            return Err(IncompleteReason::MalformedSource);
        }
        if let Some(reason) = &self.failure {
            return Err(reason.clone());
        }
        let result = self.layout_directive_inner(body, activity, budget);
        if let Err(reason) = &result {
            self.failure = Some(reason.clone());
        }
        result
    }

    fn layout_directive_inner(
        &mut self,
        body: &str,
        activity: Truth,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        budget.charge(1, body.len(), 0)?;
        if activity == Truth::False {
            return Ok(());
        }
        budget.charge(1, 0, size_of::<LayoutChange>())?;
        let sequence = Sequence(
            self.sequence
                .0
                .checked_add(1)
                .ok_or(IncompleteReason::MalformedSource)?,
        );
        let body = body.trim();
        let body = body.strip_prefix('$').unwrap_or(body);
        let split = body
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(body.len());
        let (key, value) = body.split_at(split);
        let value = value.trim();
        let supported = matches!(
            self.context.layout.platform,
            Some(LayoutPlatform::Win32 | LayoutPlatform::Win64)
        ) && self.context.compiler_version.is_some_and(|v| {
            (20..=37).any(|n| v == CompilerVersion::new(n, 0)) || v == CompilerVersion::new(18, 5)
        });
        let certain = supported && activity == Truth::True;
        let short_boolean = key.eq_ignore_ascii_case("H");
        let fact = if !certain {
            ConditionalFact::Unknown
        } else if (short_boolean && value == "+")
            || (!short_boolean && value.eq_ignore_ascii_case("ON"))
        {
            ConditionalFact::True
        } else if (short_boolean && value == "-")
            || (!short_boolean && value.eq_ignore_ascii_case("OFF"))
        {
            ConditionalFact::False
        } else {
            ConditionalFact::Unknown
        };
        if key.eq_ignore_ascii_case("A") || key.eq_ignore_ascii_case("ALIGN") {
            self.settings.record_alignment = if certain {
                match value {
                    "1" => Some(1),
                    "-" if key.eq_ignore_ascii_case("A") => Some(1),
                    "2" => Some(2),
                    "4" => Some(4),
                    "8" => Some(8),
                    "+" if key.eq_ignore_ascii_case("A") => Some(8),
                    v if key.eq_ignore_ascii_case("ALIGN") && v.eq_ignore_ascii_case("ON") => {
                        Some(8)
                    }
                    v if key.eq_ignore_ascii_case("ALIGN") && v.eq_ignore_ascii_case("OFF") => {
                        Some(1)
                    }
                    _ => None,
                }
            } else {
                None
            };
        } else if key.eq_ignore_ascii_case("Z") || key.eq_ignore_ascii_case("MINENUMSIZE") {
            self.settings.minimum_enum_size = if certain {
                match value {
                    "1" => Some(1),
                    "2" => Some(2),
                    "4" => Some(4),
                    _ => None,
                }
            } else {
                None
            };
        } else if key.eq_ignore_ascii_case("H") || key.eq_ignore_ascii_case("LONGSTRINGS") {
            self.settings.long_strings = fact;
        } else if key.eq_ignore_ascii_case("OLDTYPELAYOUT") {
            self.settings.old_type_layout = fact;
        } else if key.eq_ignore_ascii_case("REALCOMPATIBILITY") {
            self.settings.real_compatibility = fact;
        } else if key.eq_ignore_ascii_case("PUSHOPT") {
            limit(
                self.settings_stack.len() + 1,
                budget.limits().max_scope_depth,
                "max_scope_depth",
            )?;
            budget.charge(1, 0, size_of::<Option<LayoutSettings>>())?;
            let proven = certain
                && value.is_empty()
                && self.context.compiler_version == Some(CompilerVersion::new(37, 0));
            if !proven {
                self.poison_settings_stack(budget)?;
            }
            self.settings_stack
                .push(proven.then(|| self.settings.clone()));
        } else if key.eq_ignore_ascii_case("POPOPT") {
            let saved = self.settings_stack.pop();
            if saved.is_some() {
                budget.release(size_of::<Option<LayoutSettings>>());
            }
            if certain
                && value.is_empty()
                && self.context.compiler_version == Some(CompilerVersion::new(37, 0))
            {
                if let Some(Some(saved)) = saved {
                    // LONGSTRINGS is absent from the documented eligible options list.
                    let h = self.settings.long_strings;
                    self.settings = saved;
                    self.settings.long_strings = h;
                } else {
                    self.poison_settings_stack(budget)?;
                    self.settings = LayoutSettings::default();
                }
            } else {
                self.poison_settings_stack(budget)?;
                self.settings = LayoutSettings::default();
            }
        } else {
            // The shared entry point admits only layout families. Unsupported
            // save/restore spellings or unknown layout directives cannot be harmless.
            self.settings = LayoutSettings::default();
            self.poison_settings_stack(budget)?;
        }
        self.sequence = sequence;
        self.settings_history.push(LayoutChange {
            logical: self.layout_logical_position(),
            sequence,
            settings: self.settings.clone(),
        });
        Ok(())
    }

    fn poison_settings_stack(
        &mut self,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        budget.charge(self.settings_stack.len(), 0, 0)?;
        self.settings_stack.fill(None);
        Ok(())
    }
}
