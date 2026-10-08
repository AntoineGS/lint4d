//! Resumable lexical framing; complete declarations alone go to tree-sitter.
use super::bindings::{Declaration, EnumMember, IndexExpr, TypeExpr};
use super::*;
use crate::conditional::Truth;
use crate::{LoadedSource, SourceId};
use pascal_project::ConditionalContext;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use tree_sitter::Node;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Lex {
    Normal,
    Slash,
    Paren,
    Brace,
    Line,
    StarComment,
    StarEnd,
    String,
    Quote,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Section {
    None,
    Types,
    Vars,
    Consts,
    Routine,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ScopeKind {
    Root,
    Routine,
    Body,
    Block,
    Record,
}
#[derive(Clone, Debug, Hash)]
pub(super) struct Scope {
    pub(super) parent: Option<ScopeId>,
    kind: ScopeKind,
}
#[derive(Clone, Debug, Hash)]
struct Span {
    anchor: SourceAnchor,
    logical_start: usize,
    site: BindingSite,
}

/// Opaque declaration state. Source, occurrence, logical sequence and scope are
/// deliberately independent. No final body reparse repairs earlier queries.
#[derive(Debug)]
pub struct CompileTimeState {
    pub(super) context: ConditionalContext,
    pub(super) scopes: Vec<Scope>,
    stack: Vec<ScopeId>,
    pub(super) declarations: Vec<Declaration>,
    definition_count: usize,
    field_count: usize,
    variant_count: usize,
    pub(super) settings: pascal_project::LayoutSettings,
    pub(super) settings_stack: Vec<Option<pascal_project::LayoutSettings>>,
    pub(super) settings_history: Vec<super::records::LayoutChange>,
    spans: Vec<Span>,
    pub(super) sequence: Sequence,
    logical: usize,
    pending_start: usize,
    pending: String,
    word: String,
    lex: Lex,
    section: Section,
    composite_depth: usize,
    rhs: bool,
    pending_class: bool,
    parens: usize,
    implementation: bool,
    finished: bool,
    root_ended: bool,
    pub(super) failure: Option<IncompleteReason>,
    pub(super) unknown: bool,
}

pub(super) fn limit(
    value: usize,
    maximum: usize,
    name: &'static str,
) -> Result<(), IncompleteReason> {
    if value > maximum {
        Err(IncompleteReason::Limit { name, maximum })
    } else {
        Ok(())
    }
}

impl CompileTimeState {
    pub(super) fn finished_for_directive(&self) -> bool {
        self.finished
    }
    pub(super) fn layout_logical_position(&self) -> usize {
        self.logical
    }
    pub(super) fn node_logical_position(
        &self,
        start: usize,
        prefix: usize,
    ) -> Result<usize, IncompleteReason> {
        start
            .checked_sub(prefix)
            .and_then(|n| self.pending_start.checked_add(n))
            .ok_or(IncompleteReason::MalformedSource)
    }
    pub fn new(
        context: &ConditionalContext,
        root: &LoadedSource,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Self, IncompleteReason> {
        let before = budget.snapshot().retained_bytes;
        let result = Self::new_inner(context, root, budget);
        if result.is_err() {
            // Admission above the previous owner's live payload is not an
            // allocation until the whole context has been admitted.
            budget.release(budget.snapshot().retained_bytes.saturating_sub(before));
        }
        result
    }

    fn new_inner(
        context: &ConditionalContext,
        root: &LoadedSource,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Self, IncompleteReason> {
        limit(1, budget.limits().max_scope_depth, "max_scope_depth")?;
        // Account the complete context before cloning, including its strings.
        budget.charge(
            1,
            root.id.as_str().len(),
            size_of::<Self>() + size_of::<Scope>() + size_of::<ScopeId>(),
        )?;
        let entries = context
            .defines
            .len()
            .checked_add(context.options.len())
            .and_then(|n| n.checked_add(context.constants.len()))
            .ok_or(IncompleteReason::MalformedSource)?;
        let metadata = entries
            .checked_mul(
                size_of::<(String, crate::conditional::ConstantValue)>() + 4 * size_of::<usize>(),
            )
            .ok_or(IncompleteReason::MalformedSource)?;
        budget.charge(entries, 0, metadata)?;
        let mut failure = None;
        context
            .visit_recovery_payload(&mut |n| {
                budget.charge(1, n, n).map_err(|r| {
                    failure = Some(r);
                    "context admission".into()
                })
            })
            .map_err(|_| failure.unwrap_or(IncompleteReason::MalformedSource))?;
        Ok(Self {
            context: context.clone(),
            scopes: vec![Scope {
                parent: None,
                kind: ScopeKind::Root,
            }],
            stack: vec![ScopeId(0)],
            declarations: Vec::new(),
            definition_count: 0,
            field_count: 0,
            variant_count: 0,
            settings: context.layout.defaults.clone(),
            settings_stack: Vec::new(),
            settings_history: Vec::new(),
            spans: Vec::new(),
            sequence: Sequence(0),
            logical: 0,
            pending_start: 0,
            pending: String::new(),
            word: String::new(),
            lex: Lex::Normal,
            section: Section::None,
            composite_depth: 0,
            rhs: false,
            pending_class: false,
            parens: 0,
            implementation: false,
            finished: false,
            root_ended: false,
            failure: None,
            unknown: false,
        })
    }

    fn site(&self) -> BindingSite {
        BindingSite {
            scope: *self.stack.last().expect("root scope"),
            sequence: self.sequence,
        }
    }

    pub fn feed(
        &mut self,
        fragment: SourceFragment<'_>,
        activity: Truth,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        if self.finished {
            return Err(IncompleteReason::MalformedSource);
        }
        if let Some(reason) = &self.failure {
            return Err(reason.clone());
        }
        let result = self.feed_inner(fragment, activity, budget);
        if let Err(reason) = &result {
            self.failure = Some(reason.clone());
        }
        result
    }

    fn feed_inner(
        &mut self,
        fragment: SourceFragment<'_>,
        activity: Truth,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        budget.charge(1, 0, 0)?;
        if activity == Truth::False {
            return Ok(());
        }
        if activity == Truth::Unknown {
            self.unknown = true;
            return Ok(());
        }
        let end = fragment
            .physical_start
            .checked_add(fragment.text.len())
            .ok_or(IncompleteReason::MalformedSource)?;
        for (i, ch) in fragment.text.char_indices() {
            budget.charge(1, ch.len_utf8(), 0)?;
            let offset = fragment.physical_start + i;
            self.map_char(
                fragment.source,
                fragment.occurrence,
                offset,
                ch.len_utf8(),
                budget,
            )?;
            limit(
                self.pending.len().saturating_add(ch.len_utf8()),
                budget.limits().max_pending_bytes,
                "max_pending_bytes",
            )?;
            budget.charge(0, 0, ch.len_utf8())?;
            self.pending.push(ch);
            self.logical = self
                .logical
                .checked_add(ch.len_utf8())
                .ok_or(IncompleteReason::MalformedSource)?;
            self.lex_char(ch, budget)?;
        }
        // Endpoint snapshots are needed even for an empty fragment and for a
        // query immediately after a just-completed declaration.
        self.map_char(fragment.source, fragment.occurrence, end, 0, budget)
    }

    fn map_char(
        &mut self,
        source: &SourceId,
        occurrence: OccurrenceId,
        offset: usize,
        len: usize,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        let site = self.site();
        if let Some(last) = self.spans.last_mut() {
            if last.anchor.source == *source
                && last.anchor.occurrence == occurrence
                && last.anchor.range.end == offset
                && last.site == site
                && last.logical_start + last.anchor.range.len() == self.logical
            {
                last.anchor.range.end += len;
                return Ok(());
            }
        }
        budget.charge(
            1,
            source.as_str().len(),
            size_of::<Span>() + source.as_str().len(),
        )?;
        self.spans.push(Span {
            anchor: SourceAnchor {
                source: source.clone(),
                occurrence,
                range: offset..offset + len,
            },
            logical_start: self.logical,
            site,
        });
        Ok(())
    }

    fn clear_pending(&mut self, budget: &mut CompileTimeBudget<'_>) {
        budget.release(self.pending.len());
        self.pending = String::new();
        self.pending_start = self.logical;
        self.rhs = false;
        self.pending_class = false;
    }

    fn lex_char(
        &mut self,
        ch: char,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        match self.lex {
            Lex::Brace => {
                if ch == '}' {
                    self.lex = Lex::Normal;
                }
                return Ok(());
            }
            Lex::Line => {
                if ch == '\n' {
                    self.lex = Lex::Normal;
                }
                return Ok(());
            }
            Lex::StarComment => {
                if ch == '*' {
                    self.lex = Lex::StarEnd;
                }
                return Ok(());
            }
            Lex::StarEnd => {
                self.lex = if ch == ')' {
                    Lex::Normal
                } else if ch == '*' {
                    Lex::StarEnd
                } else {
                    Lex::StarComment
                };
                return Ok(());
            }
            Lex::String => {
                if ch == '\'' {
                    self.lex = Lex::Quote;
                }
                return Ok(());
            }
            Lex::Quote => {
                if ch == '\'' {
                    self.lex = Lex::String;
                    return Ok(());
                }
                self.lex = Lex::Normal;
            }
            Lex::Slash => {
                self.lex = Lex::Normal;
                if ch == '/' {
                    self.lex = Lex::Line;
                    return Ok(());
                }
            }
            Lex::Paren => {
                self.lex = Lex::Normal;
                if ch == '*' {
                    self.lex = Lex::StarComment;
                    return Ok(());
                }
                limit(
                    self.parens + 1,
                    budget.limits().max_type_depth,
                    "max_type_depth",
                )?;
                self.parens += 1;
            }
            Lex::Normal => {}
        }
        if ch.is_ascii_alphanumeric() || ch == '_' || (!ch.is_ascii() && ch.is_alphanumeric()) {
            limit(
                self.word.len().saturating_add(ch.len_utf8()),
                budget.limits().max_pending_bytes,
                "max_pending_bytes",
            )?;
            budget.charge(0, 0, ch.len_utf8())?;
            self.word.push(ch);
            return Ok(());
        }
        self.flush_word(budget)?;
        if ch == ';' && self.pending_class {
            // A bare composite header is retained as unsupported, not treated
            // as an unterminated body. The parser alone admits its syntax.
            self.composite_depth = self.composite_depth.saturating_sub(1);
            self.stack.pop();
            self.pending_class = false;
        }
        match ch {
            '=' if matches!(self.section, Section::Types | Section::Consts) => self.rhs = true,
            ':' if matches!(self.section, Section::Vars | Section::Consts) => self.rhs = true,
            '{' => self.lex = Lex::Brace,
            '\'' => self.lex = Lex::String,
            '/' => self.lex = Lex::Slash,
            '(' => {
                self.lex = Lex::Paren;
                self.pending_class = false;
            }
            ')' => self.parens = self.parens.saturating_sub(1),
            ';' if self.composite_depth == 0 && self.parens == 0 => {
                self.admit_statement(budget)?;
                self.clear_pending(budget);
            }
            _ => {}
        }
        limit(
            self.parens,
            budget.limits().max_type_depth,
            "max_type_depth",
        )
    }

    fn push_scope(
        &mut self,
        kind: ScopeKind,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        limit(
            self.stack.len() + 1,
            budget.limits().max_scope_depth,
            "max_scope_depth",
        )?;
        budget.charge(1, 0, size_of::<Scope>() + size_of::<ScopeId>())?;
        let id = ScopeId(self.scopes.len() as u64);
        self.scopes.push(Scope {
            parent: self.stack.last().copied(),
            kind,
        });
        self.stack.push(id);
        self.section = Section::None;
        Ok(())
    }

    fn flush_word(&mut self, budget: &mut CompileTimeBudget<'_>) -> Result<(), IncompleteReason> {
        if self.word.is_empty() {
            return Ok(());
        }
        budget.charge(1, self.word.len(), 0)?;
        let mut word = std::mem::take(&mut self.word);
        word.make_ascii_lowercase();
        let result = self.process_word(&word, budget);
        budget.release(word.len());
        result
    }

    fn process_word(
        &mut self,
        word: &str,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        if self.root_ended {
            // A damaged unmatched end cannot turn a later local declaration
            // into a global one. Comments/strings never reach this branch.
            self.unknown = true;
            return Ok(());
        }
        if matches!(
            self.section,
            Section::Types | Section::Vars | Section::Consts
        ) && self.rhs
        {
            if self.pending_class {
                if matches!(word, "abstract" | "sealed") {
                    return Ok(());
                }
                self.pending_class = false;
                if word == "of" {
                    self.composite_depth = self.composite_depth.saturating_sub(1);
                    self.stack.pop();
                    return Ok(());
                }
            }
            if matches!(word, "record" | "object" | "interface" | "dispinterface")
                || (word == "class" && self.composite_depth == 0)
            {
                self.composite_depth += 1;
                self.pending_class = true;
                limit(
                    self.composite_depth,
                    budget.limits().max_type_depth,
                    "max_type_depth",
                )?;
                let section = self.section;
                self.push_scope(ScopeKind::Record, budget)?;
                self.section = section;
            } else if word == "end" && self.composite_depth > 0 {
                self.composite_depth -= 1;
                self.stack.pop();
            }
            return Ok(());
        }
        if self.section == Section::Routine {
            return Ok(());
        }
        match word {
            "program" | "library" => self.implementation = true,
            "unit" => self.implementation = false,
            "implementation" => {
                self.implementation = true;
                self.section = Section::None;
                self.clear_pending(budget);
            }
            "interface" => {
                self.section = Section::None;
                self.clear_pending(budget);
            }
            "type" => {
                self.section = Section::Types;
                self.clear_pending(budget);
            }
            "var" | "threadvar" => {
                self.section = Section::Vars;
                self.clear_pending(budget);
            }
            "const" | "resourcestring" => {
                self.section = Section::Consts;
                self.clear_pending(budget);
            }
            "procedure" | "function" | "constructor" | "destructor" => {
                self.section = Section::Routine;
                // Retain only the routine header, starting at its keyword.
                let start = self.pending.len().saturating_sub(word.len() + 1);
                if start > 0 {
                    self.pending.drain(..start);
                    budget.release(start);
                    self.pending_start += start;
                }
            }
            "begin" | "try" | "case" | "repeat" => {
                let kind = if word == "begin"
                    && self.scopes[self.site().scope.0 as usize].kind == ScopeKind::Routine
                {
                    ScopeKind::Body
                } else {
                    ScopeKind::Block
                };
                self.push_scope(kind, budget)?;
                self.clear_pending(budget);
            }
            "end" | "until" => {
                if self.stack.len() > 1 {
                    let ended = self.stack.pop().unwrap();
                    if self.scopes[ended.0 as usize].kind == ScopeKind::Body && self.stack.len() > 1
                    {
                        self.stack.pop();
                    }
                } else {
                    self.root_ended = true;
                }
                self.section = Section::None;
                self.clear_pending(budget);
            }
            "forward" | "external"
                if self.scopes[self.site().scope.0 as usize].kind == ScopeKind::Routine =>
            {
                self.stack.pop();
                self.section = Section::None;
            }
            _ => {}
        }
        Ok(())
    }

    fn admit_statement(
        &mut self,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        let (prefix, suffix, kind) = match self.section {
            Section::Types => (
                "unit T; interface type ",
                " implementation end.",
                "declType",
            ),
            Section::Vars => ("unit T; interface var ", " implementation end.", "declVar"),
            Section::Consts => (
                "unit T; interface const ",
                " implementation end.",
                "declConst",
            ),
            Section::Routine => ("unit T; interface ", " implementation end.", "declProc"),
            Section::None => return Ok(()),
        };
        let len = prefix
            .len()
            .checked_add(self.pending.len())
            .and_then(|n| n.checked_add(suffix.len()))
            .ok_or(IncompleteReason::MalformedSource)?;
        // Bound parser input and charge before allocating the wrapper or parsing.
        limit(
            self.pending.len(),
            budget.limits().max_pending_bytes,
            "max_pending_bytes",
        )?;
        budget.charge(1, len, len)?;
        let mut source = String::with_capacity(len);
        source.push_str(prefix);
        source.push_str(&self.pending);
        source.push_str(suffix);
        let result = crate::parser::PARSER.with(|cell| {
            let tree = cell
                .borrow_mut()
                .parse(&source, None)
                .ok_or(IncompleteReason::MalformedSource)?;
            let valid = !tree.root_node().has_error();
            let Some(decl) = find_node(tree.root_node(), kind, budget)? else {
                self.unknown = true;
                return Ok(());
            };
            if !valid && self.section == Section::Routine {
                self.unknown = true;
                return Ok(());
            }
            if self.section == Section::Routine {
                self.admit_node(decl, source.as_bytes(), prefix.len(), false, true, budget)?;
                if self.implementation {
                    self.push_scope(ScopeKind::Routine, budget)?;
                    self.admit_arguments(decl, source.as_bytes(), prefix.len(), budget)?;
                }
                self.section = Section::None;
            } else {
                // Recovered declarations establish only a conservative name
                // blocker, never a fact; unrelated later valid names can recover.
                self.admit_node(
                    decl,
                    source.as_bytes(),
                    prefix.len(),
                    self.section == Section::Types,
                    valid,
                    budget,
                )?;
            }
            Ok(())
        });
        budget.release(len);
        result
    }

    fn admit_arguments(
        &mut self,
        node: Node<'_>,
        bytes: &[u8],
        prefix: usize,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        budget.charge(1, 0, 0)?;
        let Some(args) = node.child_by_field_name("args") else {
            return Ok(());
        };
        let mut cursor = args.walk();
        for child in args.named_children(&mut cursor) {
            budget.charge(1, 0, 0)?;
            if child.kind() == "declArg" {
                self.admit_node(child, bytes, prefix, false, true, budget)?;
            }
        }
        Ok(())
    }

    fn admit_node(
        &mut self,
        node: Node<'_>,
        bytes: &[u8],
        prefix: usize,
        is_type: bool,
        valid: bool,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        let mut cursor = node.walk();
        for name in node.children_by_field_name("name", &mut cursor) {
            if !name.is_named() {
                continue;
            }
            if name.kind() != "identifier" || name.is_missing() || name.has_error() {
                // A qualified method implementation belongs to its routine
                // scope, not to the unit's unqualified intrinsic namespace.
                if node.kind() == "declProc" && matches!(name.kind(), "genericDot" | "genericTpl") {
                    continue;
                }
                self.unknown = true;
                continue;
            }
            let mut definition_count = self
                .definition_count
                .checked_add(1)
                .ok_or(IncompleteReason::MalformedSource)?;
            limit(
                definition_count,
                budget.limits().max_definitions,
                "max_definitions",
            )?;
            let name_text = name
                .utf8_text(bytes)
                .map_err(|_| IncompleteReason::MalformedSource)?;
            let mut type_cursor = node.walk();
            let mut type_fields = node.children_by_field_name("type", &mut type_cursor);
            let first = type_fields.next();
            let ty_node = type_fields.last().or(first);
            let strong = first.is_some_and(|n| n.kind() == "kType");
            let ty_text = match (first, ty_node) {
                (Some(a), Some(b)) => {
                    std::str::from_utf8(&bytes[a.start_byte()..b.end_byte()]).unwrap_or("")
                }
                _ => "",
            };
            let ty_text = if valid {
                ty_text
            } else {
                self.pending.as_str()
            };
            let initializer = node
                .child_by_field_name("defaultValue")
                .and_then(|n| n.utf8_text(bytes).ok());
            let initializer_len = initializer.map_or(0, str::len);
            budget.charge(
                1,
                name_text.len() + ty_text.len() + initializer_len,
                size_of::<Declaration>() + name_text.len() + initializer_len,
            )?;
            let mut field_count = self.field_count;
            let mut variant_count = self.variant_count;
            let mut ty = if valid {
                type_expr(
                    self,
                    prefix,
                    ty_node,
                    bytes,
                    ty_text,
                    node.kind() == "declArg",
                    0,
                    &mut definition_count,
                    &mut field_count,
                    &mut variant_count,
                    budget,
                )?
            } else {
                budget.charge(1, ty_text.len(), ty_text.len())?;
                TypeExpr::Unsupported(ty_text.into())
            };
            if strong {
                if let TypeExpr::Named(name) = ty {
                    ty = TypeExpr::StrongAlias(name);
                }
            }
            self.sequence.0 = self
                .sequence
                .0
                .checked_add(1)
                .ok_or(IncompleteReason::MalformedSource)?;
            let site = self.site();
            let logical_name = name
                .start_byte()
                .checked_sub(prefix)
                .and_then(|n| self.pending_start.checked_add(n))
                .ok_or(IncompleteReason::MalformedSource)?;
            let anchor =
                self.anchor_at(logical_name, name.end_byte() - name.start_byte(), budget)?;
            let provenance = self.provenance(self.pending_start, self.logical, budget)?;
            self.declarations.push(Declaration {
                name: name_text.to_ascii_lowercase(),
                key: DefinitionKey {
                    anchor,
                    scope: site.scope,
                },
                site,
                ty,
                is_type,
                // As in the existing conditional scanner, typed constants are
                // lexical blockers, not proven values without coercion evidence.
                is_constant: valid && node.kind() == "declConst" && ty_node.is_none(),
                settings: self.settings_at_logical(logical_name, budget)?.clone(),
                provenance,
                initializer: initializer.map(str::to_owned),
            });
            self.definition_count = definition_count;
            self.field_count = field_count;
            self.variant_count = variant_count;
        }
        Ok(())
    }

    fn anchor_at(
        &self,
        logical: usize,
        len: usize,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<SourceAnchor, IncompleteReason> {
        if let Some(span) = self.spans.get(self.span_index(logical, budget)?) {
            if logical >= span.logical_start
                && logical < span.logical_start + span.anchor.range.len()
            {
                budget.charge(
                    1,
                    span.anchor.source.as_str().len(),
                    size_of::<SourceAnchor>() + span.anchor.source.as_str().len(),
                )?;
                let start = span.anchor.range.start + logical - span.logical_start;
                return Ok(SourceAnchor {
                    source: span.anchor.source.clone(),
                    occurrence: span.anchor.occurrence,
                    range: start..(start + len).min(span.anchor.range.end),
                });
            }
        }
        Err(IncompleteReason::MalformedSource)
    }

    // Logical spans are monotonic even when physical sources/offsets jump at
    // includes. Search only the relevant declaration, not all earlier bodies.
    fn span_index(
        &self,
        logical: usize,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<usize, IncompleteReason> {
        let (mut low, mut high) = (0, self.spans.len());
        while low < high {
            budget.charge(1, 0, 0)?;
            let mid = low + (high - low) / 2;
            let span = &self.spans[mid];
            if span.logical_start + span.anchor.range.len() <= logical {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok(low)
    }

    fn provenance(
        &self,
        start: usize,
        end: usize,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Vec<SourceAnchor>, IncompleteReason> {
        let mut result = Vec::new();
        for span in &self.spans[self.span_index(start, budget)?..] {
            budget.charge(1, 0, 0)?;
            if span.logical_start >= end {
                break;
            }
            let a = start.max(span.logical_start);
            let b = end.min(span.logical_start + span.anchor.range.len());
            if a < b {
                budget.charge(
                    1,
                    span.anchor.source.as_str().len(),
                    size_of::<SourceAnchor>() + span.anchor.source.as_str().len(),
                )?;
                result.push(SourceAnchor {
                    source: span.anchor.source.clone(),
                    occurrence: span.anchor.occurrence,
                    range: span.anchor.range.start + a - span.logical_start
                        ..span.anchor.range.start + b - span.logical_start,
                });
            }
        }
        Ok(result)
    }

    pub fn finish_root(
        &mut self,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        if self.finished {
            return Err(IncompleteReason::MalformedSource);
        }
        if let Some(reason) = &self.failure {
            return Err(reason.clone());
        }
        let result = self.finish_inner(budget);
        if let Err(reason) = &result {
            self.failure = Some(reason.clone());
        }
        result
    }

    fn finish_inner(&mut self, budget: &mut CompileTimeBudget<'_>) -> Result<(), IncompleteReason> {
        self.flush_word(budget)?;
        if !matches!(self.lex, Lex::Normal | Lex::Quote | Lex::Line | Lex::Slash)
            || self.composite_depth != 0
            || self.parens != 0
            || self.stack.len() != 1
        {
            self.failure = Some(IncompleteReason::MalformedSource);
            return Err(IncompleteReason::MalformedSource);
        }
        if matches!(
            self.section,
            Section::Types | Section::Vars | Section::Consts | Section::Routine
        ) && !self.pending.trim().is_empty()
        {
            self.unknown = true;
        }
        self.clear_pending(budget);
        let site = self.site();
        if let Some(last) = self.spans.last() {
            if last.site != site {
                budget.charge(
                    1,
                    last.anchor.source.as_str().len(),
                    size_of::<Span>() + last.anchor.source.as_str().len(),
                )?;
                self.spans.push(Span {
                    anchor: SourceAnchor {
                        source: last.anchor.source.clone(),
                        occurrence: last.anchor.occurrence,
                        range: last.anchor.range.end..last.anchor.range.end,
                    },
                    logical_start: self.logical,
                    site,
                });
            }
        }
        self.finished = true;
        Ok(())
    }

    pub fn site_at(
        &self,
        source: &SourceId,
        occurrence: OccurrenceId,
        offset: usize,
    ) -> Option<BindingSite> {
        self.spans
            .iter()
            .rev()
            .find(|s| {
                s.anchor.source == *source
                    && s.anchor.occurrence == occurrence
                    && s.anchor.range.start <= offset
                    && offset <= s.anchor.range.end
            })
            .map(|s| s.site.clone())
    }

    pub fn layout_name(
        &self,
        site: BindingSite,
        name: &str,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Fact<KnownLayout> {
        super::bindings::layout_name(self, site, name, budget)
    }

    pub fn fingerprint(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.context.hash(&mut h);
        self.scopes.hash(&mut h);
        self.stack.hash(&mut h);
        self.spans.hash(&mut h);
        self.sequence.hash(&mut h);
        self.definition_count.hash(&mut h);
        self.field_count.hash(&mut h);
        self.variant_count.hash(&mut h);
        self.settings.hash(&mut h);
        self.settings_stack.hash(&mut h);
        self.settings_history.hash(&mut h);
        self.logical.hash(&mut h);
        self.pending_start.hash(&mut h);
        self.pending.hash(&mut h);
        self.word.hash(&mut h);
        self.lex.hash(&mut h);
        self.section.hash(&mut h);
        self.composite_depth.hash(&mut h);
        self.parens.hash(&mut h);
        self.implementation.hash(&mut h);
        self.finished.hash(&mut h);
        self.root_ended.hash(&mut h);
        self.unknown.hash(&mut h);
        self.rhs.hash(&mut h);
        self.pending_class.hash(&mut h);
        self.failure.hash(&mut h);
        for d in &self.declarations {
            d.name.hash(&mut h);
            d.key.hash(&mut h);
            d.site.hash(&mut h);
            d.ty.hash(&mut h);
            d.is_type.hash(&mut h);
            d.is_constant.hash(&mut h);
            d.provenance.hash(&mut h);
            d.settings.hash(&mut h);
            d.initializer.hash(&mut h);
        }
        h.finish()
    }

    pub fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        visit(size_of::<Self>())?;
        self.context.visit_recovery_payload(visit)?;
        let entries = self
            .context
            .defines
            .len()
            .checked_add(self.context.options.len())
            .and_then(|n| n.checked_add(self.context.constants.len()))
            .ok_or("context metadata overflow")?;
        visit(
            entries
                .checked_mul(
                    size_of::<(String, crate::conditional::ConstantValue)>()
                        + 4 * size_of::<usize>(),
                )
                .ok_or("context metadata overflow")?,
        )?;
        visit(self.scopes.len() * size_of::<Scope>())?;
        visit(self.stack.len() * size_of::<ScopeId>())?;
        visit(self.settings_stack.len() * size_of::<Option<pascal_project::LayoutSettings>>())?;
        visit(self.settings_history.len() * size_of::<super::records::LayoutChange>())?;
        visit(self.pending.len())?;
        visit(self.word.len())?;
        for s in &self.spans {
            visit(size_of::<Span>())?;
            visit(s.anchor.source.as_str().len())?;
        }
        for d in &self.declarations {
            visit(size_of::<Declaration>())?;
            visit(d.name.len())?;
            visit(d.key.anchor.source.as_str().len())?;
            visit_type_payload(&d.ty, visit)?;
            if let Some(text) = &d.initializer {
                visit(text.len())?;
            }
            for a in &d.provenance {
                visit(size_of::<SourceAnchor>())?;
                visit(a.source.as_str().len())?;
            }
        }
        Ok(())
    }
}

fn find_node<'a>(
    node: Node<'a>,
    kind: &str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Option<Node<'a>>, IncompleteReason> {
    let mut cursor = node.walk();
    loop {
        budget.charge(1, 0, 0)?;
        if cursor.node().kind() == kind {
            return Ok(Some(cursor.node()));
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
                return Ok(None);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn type_expr(
    state: &CompileTimeState,
    prefix: usize,
    node: Option<Node<'_>>,
    bytes: &[u8],
    raw: &str,
    argument: bool,
    depth: usize,
    definition_count: &mut usize,
    field_count: &mut usize,
    variant_count: &mut usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<TypeExpr, IncompleteReason> {
    limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
    budget.charge(1, 0, 0)?;
    let Some(node) = node else {
        return Ok(TypeExpr::Unsupported(retain(raw, budget)?));
    };
    let mut n = node;
    while matches!(n.kind(), "type" | "typeref") && n.named_child_count() == 1 {
        budget.charge(1, 0, 0)?;
        n = n.named_child(0).unwrap();
    }
    if n.kind() == "identifier" {
        let mut name = retain(n.utf8_text(bytes).unwrap_or(""), budget)?;
        name.make_ascii_lowercase();
        Ok(TypeExpr::Named(name))
    } else if n.kind() == "typerefPtr" {
        Ok(TypeExpr::Pointer(retain(raw, budget)?))
    } else if n.kind() == "declString" && n.named_child_count() == 1 {
        Ok(TypeExpr::Named(retain("string", budget)?))
    } else if n.kind() == "declArray" {
        let mut dimensions = Vec::new();
        let mut element = None;
        let mut bracket = false;
        let mut packed = false;
        let mut cursor = n.walk();
        for child in n.children(&mut cursor) {
            budget.charge(1, 0, 0)?;
            match child.kind() {
                "[" => bracket = true,
                "type" => element = Some(child),
                "kPacked" => packed = true,
                "kArray" | "kOf" | "]" | "," => {}
                _ if child.is_named() => {
                    limit(
                        dimensions.len() + 1,
                        budget.limits().max_array_dimensions,
                        "max_array_dimensions",
                    )?;
                    budget.charge(1, 0, size_of::<IndexExpr>())?;
                    dimensions.push(index_expr(child, bytes, budget)?);
                }
                _ => {}
            }
        }
        if bracket && dimensions.is_empty() {
            return Ok(TypeExpr::Unsupported(retain(raw, budget)?));
        }
        budget.charge(1, 0, size_of::<TypeExpr>())?;
        let text = element.and_then(|e| e.utf8_text(bytes).ok()).unwrap_or("");
        let element = Box::new(type_expr(
            state,
            prefix,
            element,
            bytes,
            text,
            false,
            depth + 1,
            definition_count,
            field_count,
            variant_count,
            budget,
        )?);
        Ok(TypeExpr::Array {
            open: argument && !bracket,
            dimensions,
            element,
            packed,
        })
    } else if n.kind() == "declSubRange" {
        Ok(TypeExpr::Range(index_expr(n, bytes, budget)?))
    } else if n.kind() == "declClass" && super::records::is_record(n, budget)? {
        super::records::parse_record(
            state,
            prefix,
            n,
            bytes,
            depth,
            definition_count,
            field_count,
            variant_count,
            budget,
        )
        .map(TypeExpr::Record)
    } else if matches!(n.kind(), "declClass" | "declIntf") {
        Ok(TypeExpr::OpaqueNominal(retain(raw, budget)?))
    } else if n.kind() == "declEnum" {
        let mut members = Vec::new();
        let mut cursor = n.walk();
        for member in n.named_children(&mut cursor) {
            budget.charge(1, 0, 0)?;
            if member.kind() != "declEnumValue" {
                continue;
            }
            *definition_count = definition_count
                .checked_add(1)
                .ok_or(IncompleteReason::MalformedSource)?;
            limit(
                *definition_count,
                budget.limits().max_definitions,
                "max_definitions",
            )?;
            budget.charge(1, 0, size_of::<EnumMember>())?;
            let name_node = member
                .child_by_field_name("name")
                .ok_or(IncompleteReason::MalformedSource)?;
            let name = name_node
                .utf8_text(bytes)
                .map_err(|_| IncompleteReason::MalformedSource)?;
            let logical = name_node
                .start_byte()
                .checked_sub(prefix)
                .and_then(|n| state.pending_start.checked_add(n))
                .ok_or(IncompleteReason::MalformedSource)?;
            let key = DefinitionKey {
                anchor: state.anchor_at(logical, name.len(), budget)?,
                scope: state.site().scope,
            };
            let value = member
                .child_by_field_name("value")
                .and_then(|n| n.utf8_text(bytes).ok())
                .map(|s| {
                    budget.charge(
                        1,
                        s.len()
                            .checked_mul(3)
                            .ok_or(IncompleteReason::MalformedSource)?,
                        0,
                    )?;
                    retain(initializer_expression(s), budget)
                })
                .transpose()?;
            members.push(EnumMember {
                name: retain(name, budget)?,
                value,
                key,
            });
        }
        Ok(TypeExpr::Enum(members))
    } else {
        Ok(TypeExpr::Unsupported(retain(raw, budget)?))
    }
}

fn retain(text: &str, budget: &mut CompileTimeBudget<'_>) -> Result<String, IncompleteReason> {
    budget.charge(1, text.len(), text.len())?;
    Ok(text.to_owned())
}

pub(super) fn initializer_expression(text: &str) -> &str {
    text.trim().strip_prefix('=').unwrap_or(text).trim()
}

fn index_expr(
    node: Node<'_>,
    bytes: &[u8],
    budget: &mut CompileTimeBudget<'_>,
) -> Result<IndexExpr, IncompleteReason> {
    budget.charge(1, 0, 0)?;
    if matches!(node.kind(), "range" | "declSubRange") {
        let lower = node
            .named_child(0)
            .and_then(|n| n.utf8_text(bytes).ok())
            .unwrap_or("");
        let upper = node
            .named_child(1)
            .and_then(|n| n.utf8_text(bytes).ok())
            .unwrap_or("");
        Ok(IndexExpr::Bounds {
            lower: retain(lower, budget)?,
            upper: retain(upper, budget)?,
        })
    } else {
        Ok(IndexExpr::Domain(retain(
            node.utf8_text(bytes).unwrap_or(""),
            budget,
        )?))
    }
}

fn visit_index_payload(
    index: &IndexExpr,
    visit: &mut dyn FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    match index {
        IndexExpr::Domain(s) => visit(s.len()),
        IndexExpr::Bounds { lower, upper } => {
            visit(lower.len())?;
            visit(upper.len())
        }
    }
}

pub(super) fn visit_type_payload(
    ty: &TypeExpr,
    visit: &mut dyn FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    match ty {
        TypeExpr::Record(record) => super::records::visit_payload(record, visit),
        TypeExpr::Named(s)
        | TypeExpr::StrongAlias(s)
        | TypeExpr::Pointer(s)
        | TypeExpr::OpaqueNominal(s)
        | TypeExpr::Unsupported(s) => visit(s.len()),
        TypeExpr::Range(index) => visit_index_payload(index, visit),
        TypeExpr::Array {
            dimensions,
            element,
            ..
        } => {
            for index in dimensions {
                visit(size_of::<IndexExpr>())?;
                visit_index_payload(index, visit)?;
            }
            visit(size_of::<TypeExpr>())?;
            visit_type_payload(element, visit)
        }
        TypeExpr::Enum(members) => {
            for member in members {
                visit(size_of::<EnumMember>())?;
                visit(member.name.len())?;
                visit(member.key.anchor.source.as_str().len())?;
                if let Some(s) = &member.value {
                    visit(s.len())?;
                }
            }
            Ok(())
        }
    }
}
