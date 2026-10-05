use crate::comments::CommentMap;
use crate::config::UsesConfig;
use crate::directive_map::DirectiveMap;
use crate::doc_builder::{first_leaf, last_leaf};
use pascal_core::node_kind as K;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitSection {
    Core,
    External,
    Project,
}

/// An item in a uses clause — either a sortable unit or a pinned directive.
#[derive(Debug, Clone)]
pub enum UsesItem {
    /// A regular unit name — participates in sorting/grouping. Its comments
    /// move with it: `leading` ones sit on their own lines above it,
    /// `trailing` ones (including those around its `,` or `;`) follow the
    /// punctuation on its line.
    Unit {
        name: String,
        leading: Vec<String>,
        trailing: Vec<String>,
        /// A `,` followed it in the source.
        comma_after: bool,
    },
    /// An {$IFDEF}...{$ENDIF} block — pinned in position, contents untouched.
    IfDefBlock(IfDefBlock),
    /// A standalone directive ({$I ...}, {$HINTS OFF}, etc.) — pinned in position.
    Directive {
        text: String,
        /// A `,` followed it in the source.
        comma_after: bool,
    },
    /// A comment with no unit to attach to — pinned in position.
    Comment(String),
}

impl UsesItem {
    /// A unit without comments.
    pub fn unit(name: impl Into<String>) -> Self {
        UsesItem::Unit {
            name: name.into(),
            leading: Vec::new(),
            trailing: Vec::new(),
            comma_after: false,
        }
    }

    /// A directive with no `,` after it.
    pub fn directive(text: impl Into<String>) -> Self {
        UsesItem::Directive {
            text: text.into(),
            comma_after: false,
        }
    }

    /// Whether a `,` followed the item in the source.
    fn comma_after(&self) -> bool {
        match self {
            UsesItem::Unit { comma_after, .. } | UsesItem::Directive { comma_after, .. } => {
                *comma_after
            }
            UsesItem::IfDefBlock(block) => block.comma_after,
            UsesItem::Comment(_) => false,
        }
    }

    fn set_comma_after(&mut self) {
        match self {
            UsesItem::Unit { comma_after, .. } | UsesItem::Directive { comma_after, .. } => {
                *comma_after = true;
            }
            UsesItem::IfDefBlock(block) => block.comma_after = true,
            UsesItem::Comment(_) => {}
        }
    }
}

/// An `{$I file}` or `{$INCLUDE file}` directive (not the `{$I+}` switch),
/// also spelled `(*$I file*)`, whose file is read in its place.
fn is_include_directive(text: &str) -> bool {
    let Some(body) = text.strip_prefix("{$").or_else(|| text.strip_prefix("(*$")) else {
        return false;
    };
    let name_end = body
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(body.len());
    let (name, rest) = body.split_at(name_end);
    (name.eq_ignore_ascii_case("I") || name.eq_ignore_ascii_case("INCLUDE"))
        && rest.starts_with(|c: char| c.is_whitespace() || c == '\'' || c == '"')
}

/// Whether `items` hold an include, in any branch.
fn has_include(items: &[UsesItem]) -> bool {
    items.iter().any(|item| match item {
        UsesItem::Directive { text, .. } => is_include_directive(text),
        UsesItem::IfDefBlock(block) => {
            has_include(&block.if_branch.items)
                || block
                    .else_if_branches
                    .iter()
                    .any(|branch| has_include(&branch.items))
                || block.else_branch.as_deref().is_some_and(has_include)
        }
        UsesItem::Unit { .. } | UsesItem::Comment(_) => false,
    })
}

/// Records where the `,` of a list were in the source while its children
/// are read in order: on the item each follows, or before the first item.
#[derive(Default)]
struct SourceCommas {
    /// The item read last, while only comments have followed it.
    last: Option<usize>,
}

impl SourceCommas {
    /// Read the next child of the list holding `items`, of kind `kind`.
    /// Returns whether it is a `,` before the list's first item.
    fn read(&mut self, kind: &str, items: &mut [UsesItem]) -> bool {
        if kind == K::COMMENT {
            return false;
        }
        let last = self.last.take();
        if kind != K::COMMA {
            return false;
        }
        match last.and_then(|idx| items.get_mut(idx)) {
            Some(item) => {
                item.set_comma_after();
                false
            }
            None => items
                .iter()
                .all(|item| matches!(item, UsesItem::Comment(_))),
        }
    }

    /// The item just read was pushed last in `items`.
    fn pushed(&mut self, items: &[UsesItem]) {
        self.last = items.len().checked_sub(1);
    }
}

/// A complete {$IFDEF}...{$ENDIF} conditional block.
#[derive(Debug, Clone, Default)]
pub struct IfDefBlock {
    /// The opening condition branch.
    pub if_branch: CondBranch,
    /// Zero or more {$ELSEIF ...} branches.
    pub else_if_branches: Vec<CondBranch>,
    /// Optional {$ELSE} fallback branch.
    pub else_branch: Option<Vec<UsesItem>>,
    /// The {$ELSE} directive text.
    pub else_directive: String,
    /// Comments on the `{$ELSE}` line.
    pub else_trailing: Vec<String>,
    /// The closing directive text, e.g. "{$ENDIF}".
    pub endif: String,
    /// Comments on the `{$ENDIF}` line, after any `;`.
    pub trailing: Vec<String>,
    /// Each branch ends with the clause's `;` (a `ppUsesBlockWithSemi`).
    pub terminated: bool,
    /// The {$ELSE} branch started with a `,` in the source.
    pub else_lead: bool,
    /// A `,` followed the `{$ENDIF}` in the source.
    pub comma_after: bool,
}

/// A conditional branch with its directive text and items.
#[derive(Debug, Clone, Default)]
pub struct CondBranch {
    /// The directive text: "{$IFDEF DELPHI_XE6_UP}", "{$ELSEIF expr}", etc.
    pub directive: String,
    /// Comments on the directive's line.
    pub trailing: Vec<String>,
    /// Items in this branch (order preserved, not sorted). Recursive — can
    /// contain nested IfDefBlocks.
    pub items: Vec<UsesItem>,
    /// The branch started with a `,` in the source.
    pub lead: bool,
}

const CORE_PREFIXES: &[&str] = &[
    "System",
    "Vcl",
    "Fmx",
    "Data",
    "Datasnap",
    "FireDAC",
    "IBX",
    "REST",
    "Soap",
    "Web",
    "Xml",
    "Winapi",
    "Posix",
    "Macapi",
    "iOSapi",
    "Androidapi",
    "Bde",
    "Box2D",
    "EMS",
    "EMSHosting",
    "Generics",
    "Linuxapi",
    "MetropolisUI",
    "RSConfig",
    "RSConsole",
    "RSSetUp",
    "ToolsAPI",
];

fn is_core_prefix(prefix: &str) -> bool {
    CORE_PREFIXES.iter().any(|p| p.eq_ignore_ascii_case(prefix))
}

pub fn classify_unit(
    unit_name: &str,
    config: &UsesConfig,
    external_units: &HashSet<String>,
) -> UnitSection {
    // 1. Core: dotted prefix matches CORE_PREFIXES
    if let Some(dot_pos) = unit_name.find('.') {
        let prefix = &unit_name[..dot_pos];
        if is_core_prefix(prefix) {
            return UnitSection::Core;
        }
    } else if legacy_namespace(unit_name).is_some() {
        // Non-dotted legacy unit maps to a known core namespace
        return UnitSection::Core;
    }

    // 2. External: in scanned file set or matches external prefix
    if external_units.contains(&unit_name.to_lowercase()) {
        return UnitSection::External;
    }
    if let Some(dot_pos) = unit_name.find('.') {
        let prefix = &unit_name[..dot_pos];
        for ext_prefix in &config.external_prefixes {
            if prefix.eq_ignore_ascii_case(ext_prefix) {
                return UnitSection::External;
            }
        }
    }

    // 3. Everything else is project
    UnitSection::Project
}

fn legacy_namespace(name: &str) -> Option<&'static str> {
    // Delphi unit names are case-insensitive, so normalise before matching.
    match name.to_ascii_lowercase().as_str() {
        "sysutils" | "classes" | "types" | "variants" | "sysconst" | "math" | "strutils"
        | "dateutils" | "ioutils" | "regularexpressions" | "syncobjs" | "rtti" | "typinfo"
        | "contnrs" | "xsbuiltins" => Some("System"),

        "forms" | "controls" | "stdctrls" | "extctrls" | "comctrls" | "dialogs" | "graphics"
        | "menus" | "actnlist" | "grids" | "buttons" | "imglist" | "toolwin" | "appevnts" => {
            Some("Vcl")
        }

        "db" | "dbclient" | "provider" | "dbgrids" | "dbctrls" | "sqlexpr" => Some("Data"),

        "windows" | "messages" | "shellapi" | "activex" | "commctrl" | "shlobj" => Some("Winapi"),

        "ibdatabase" | "ibsql" | "ibquery" | "ibtable" | "ibupdatesql" | "ibevents"
        | "ibcustomdataset" | "ibstoredproc" | "ibdatabaseinfo" => Some("IBX"),

        _ => None,
    }
}

/// Recursively collect all unit names from a list of items.
fn collect_items_units(items: &[UsesItem], out: &mut Vec<String>) {
    for item in items {
        match item {
            UsesItem::Unit { name, .. } => out.push(name.clone()),
            UsesItem::IfDefBlock(block) => collect_ifdef_units(block, out),
            UsesItem::Directive { .. } | UsesItem::Comment(_) => {}
        }
    }
}

/// Recursively collect all unit names from an IfDefBlock.
fn collect_ifdef_units(block: &IfDefBlock, out: &mut Vec<String>) {
    collect_items_units(&block.if_branch.items, out);
    for branch in &block.else_if_branches {
        collect_items_units(&branch.items, out);
    }
    if let Some(else_items) = &block.else_branch {
        collect_items_units(else_items, out);
    }
}

/// If every unit in the block belongs to the same section, return that section.
fn classify_ifdef_block(
    block: &IfDefBlock,
    config: &UsesConfig,
    external_units: &HashSet<String>,
) -> Option<UnitSection> {
    let mut units = Vec::new();
    collect_ifdef_units(block, &mut units);
    if units.is_empty() {
        return None;
    }
    let section = classify_unit(&units[0], config, external_units);
    if units[1..]
        .iter()
        .all(|u| classify_unit(u, config, external_units) == section)
    {
        Some(section)
    } else {
        None
    }
}

/// Like `group_units` but returns each group tagged with its section.
fn group_units_tagged(
    units: &[String],
    config: &UsesConfig,
    external_units: &HashSet<String>,
) -> Vec<(UnitSection, Vec<String>)> {
    if !config.group {
        let mut sorted = units.to_vec();
        if config.sort {
            sorted.sort_by_key(|a: &String| a.to_lowercase());
        }
        return vec![(UnitSection::Core, sorted)];
    }

    let mut core: Vec<String> = Vec::new();
    let mut external: Vec<String> = Vec::new();
    let mut project: Vec<String> = Vec::new();

    for unit in units {
        match classify_unit(unit, config, external_units) {
            UnitSection::Core => core.push(unit.clone()),
            UnitSection::External => external.push(unit.clone()),
            UnitSection::Project => project.push(unit.clone()),
        }
    }

    if config.sort {
        core.sort_by_key(|a: &String| a.to_lowercase());
        external.sort_by_key(|a: &String| a.to_lowercase());
        project.sort_by_key(|a: &String| a.to_lowercase());
    }

    let mut result = Vec::new();
    if !core.is_empty() {
        result.push((UnitSection::Core, core));
    }
    if !external.is_empty() {
        result.push((UnitSection::External, external));
    }
    if !project.is_empty() {
        result.push((UnitSection::Project, project));
    }
    result
}

pub fn group_units(
    units: &[String],
    config: &UsesConfig,
    external_units: &HashSet<String>,
) -> Vec<Vec<String>> {
    group_units_tagged(units, config, external_units)
        .into_iter()
        .map(|(_, units)| units)
        .collect()
}

fn node_text(node: tree_sitter::Node, source: &[u8]) -> String {
    pascal_core::decode_bytes(&source[node.start_byte()..node.end_byte()]).replace('\r', "")
}

/// Which conditional branch of a {$IFDEF}/{$ELSEIF}/{$ELSE}/{$ENDIF}
/// block is currently being parsed.
#[derive(Debug, Clone, Copy, PartialEq)]
enum BranchState {
    If,
    ElseIf,
    Else,
}

/// The item list of the branch `state` points at.
fn branch_items(block: &mut IfDefBlock, state: BranchState) -> &mut Vec<UsesItem> {
    match state {
        BranchState::ElseIf if !block.else_if_branches.is_empty() => {
            let last = block.else_if_branches.len() - 1;
            &mut block.else_if_branches[last].items
        }
        BranchState::Else => block.else_branch.get_or_insert_with(Vec::new),
        _ => &mut block.if_branch.items,
    }
}

/// Texts of the comments leading `node`'s first leaf.
fn leading_texts(node: tree_sitter::Node, comments: &CommentMap) -> Vec<String> {
    comments
        .leading_comments(first_leaf(node).id())
        .iter()
        .map(|c| c.text.clone())
        .collect()
}

/// Texts of the comments trailing `node`'s last leaf.
fn trailing_texts(node: tree_sitter::Node, comments: &CommentMap) -> Vec<String> {
    comments
        .trailing_comments(last_leaf(node).id())
        .iter()
        .map(|c| c.text.clone())
        .collect()
}

/// Texts of the comments on either side of a `,` or `;`.
fn punctuation_texts(node: tree_sitter::Node, comments: &CommentMap) -> Vec<String> {
    let mut texts = leading_texts(node, comments);
    texts.extend(trailing_texts(node, comments));
    texts
}

/// Give comments found after an item (around its `,` or `;`) to that item,
/// or pin them in place when it cannot carry comments.
fn attach_after(items: &mut Vec<UsesItem>, texts: Vec<String>) {
    match items.last_mut() {
        Some(UsesItem::Unit { trailing, .. }) => trailing.extend(texts),
        Some(UsesItem::IfDefBlock(block)) => block.trailing.extend(texts),
        _ => items.extend(texts.into_iter().map(UsesItem::Comment)),
    }
}

/// Build a unit item from a `moduleName` node, with the comments that
/// lead its first leaf and trail its last one.
fn unit_item(node: tree_sitter::Node, source: &[u8], comments: &CommentMap) -> Option<UsesItem> {
    let name = node_text(node, source);
    (!name.is_empty()).then(|| UsesItem::Unit {
        name,
        leading: leading_texts(node, comments),
        trailing: trailing_texts(node, comments),
        comma_after: false,
    })
}

/// Whether the branch `state` points at started with a `,`.
fn branch_lead(block: &mut IfDefBlock, state: BranchState) -> &mut bool {
    match state {
        BranchState::ElseIf if !block.else_if_branches.is_empty() => {
            let last = block.else_if_branches.len() - 1;
            &mut block.else_if_branches[last].lead
        }
        BranchState::Else => &mut block.else_lead,
        _ => &mut block.if_branch.lead,
    }
}

/// Walk children of a `ppUsesBlock` node and return an `IfDefBlock`.
///
/// Comments leading the block's own `{$IFDEF}` belong to the caller.
fn parse_pp_uses_block(
    node: tree_sitter::Node,
    source: &[u8],
    comments: &CommentMap,
) -> IfDefBlock {
    let mut block = IfDefBlock::default();
    // Which conditional branch we are currently appending items into.
    let mut state = BranchState::If;
    let mut ended = false;
    let mut commas = SourceCommas::default();

    for child in node.children(&mut node.walk()) {
        if !ended && commas.read(child.kind(), branch_items(&mut block, state)) {
            *branch_lead(&mut block, state) = true;
        }
        match child.kind() {
            K::PP_IF => {
                block.if_branch.directive = node_text(child, source);
                block.if_branch.trailing = trailing_texts(child, comments);
            }
            K::PP_ELSE => {
                let leading = leading_texts(child, comments);
                branch_items(&mut block, state).extend(leading.into_iter().map(UsesItem::Comment));
                let text = node_text(child, source);
                let trailing = trailing_texts(child, comments);
                if text.to_lowercase().contains("elseif") {
                    block.else_if_branches.push(CondBranch {
                        directive: text,
                        trailing,
                        items: Vec::new(),
                        lead: false,
                    });
                    state = BranchState::ElseIf;
                } else {
                    // It's a plain {$ELSE}
                    block.else_branch = Some(Vec::new());
                    block.else_directive = text;
                    block.else_trailing = trailing;
                    state = BranchState::Else;
                }
            }
            K::PP_END_IF => {
                let leading = leading_texts(child, comments);
                branch_items(&mut block, state).extend(leading.into_iter().map(UsesItem::Comment));
                block.endif = node_text(child, source);
                block.trailing = trailing_texts(child, comments);
                ended = true;
            }
            K::MODULE_NAME => {
                if let Some(item) = unit_item(child, source, comments) {
                    let items = branch_items(&mut block, state);
                    items.push(item);
                    commas.pushed(items);
                }
            }
            K::PP_USES_BLOCK => {
                let items = branch_items(&mut block, state);
                items.extend(
                    leading_texts(child, comments)
                        .into_iter()
                        .map(UsesItem::Comment),
                );
                items.push(UsesItem::IfDefBlock(parse_pp_uses_block(
                    child, source, comments,
                )));
                commas.pushed(items);
            }
            K::PP_DIRECTIVE => {
                let items = branch_items(&mut block, state);
                items.push(UsesItem::directive(node_text(child, source)));
                commas.pushed(items);
            }
            K::COMMA | K::SEMICOLON => {
                let texts = punctuation_texts(child, comments);
                if ended {
                    block.trailing.extend(texts);
                    block.comma_after |= child.kind() == K::COMMA;
                } else {
                    attach_after(branch_items(&mut block, state), texts);
                }
            }
            _ => {} // skip comments (taken from `comments`), etc.
        }
    }

    block
}

/// The contents of a `declUses` node.
#[derive(Debug, Default)]
pub(crate) struct UsesClause {
    pub items: Vec<UsesItem>,
    /// Comments and directives after the clause's end on its line, from
    /// the first directive on. They lie outside the clause, so they stay
    /// after its end whatever moves inside it.
    pub after: Vec<String>,
}

/// The trivia trailing `end`, the clause's last leaf, from its first
/// directive on, in source order, and how many of them are comments.
fn after_clause_trivia(
    end: tree_sitter::Node,
    comments: &CommentMap,
    directives: &DirectiveMap,
) -> (Vec<String>, usize) {
    if end.is_missing() || !matches!(end.kind(), K::SEMICOLON | K::PP_END_IF) {
        return (Vec::new(), 0);
    }
    let directives = directives.trailing_directives(end.id());
    let Some(cut) = directives.iter().map(|d| d.span.start).min() else {
        return (Vec::new(), 0);
    };
    let later_comments: Vec<_> = comments
        .trailing_comments(end.id())
        .iter()
        .filter(|c| c.span.start > cut)
        .map(|c| (c.span.start, c.text.clone()))
        .collect();
    let count = later_comments.len();
    let mut trivia: Vec<(usize, String)> = directives
        .iter()
        .map(|d| (d.span.start, d.text.clone()))
        .chain(later_comments)
        .collect();
    trivia.sort_by_key(|(start, _)| *start);
    (trivia.into_iter().map(|(_, text)| text).collect(), count)
}

/// Extract the items of a `declUses` node, and the trivia after its end.
///
/// Comments come from `comments`; those attached to the `uses` keyword
/// are left to the caller.
pub(crate) fn extract_uses_items(
    node: tree_sitter::Node,
    source: &[u8],
    comments: &CommentMap,
    directives: &DirectiveMap,
) -> UsesClause {
    let end = last_leaf(node);
    let (after, after_comments) = after_clause_trivia(end, comments, directives);
    // The comments in `after` also trail `end`, last: leave them out.
    let without_after = |mut texts: Vec<String>| {
        texts.truncate(texts.len().saturating_sub(after_comments));
        texts
    };
    let mut items = Vec::new();
    let mut commas = SourceCommas::default();
    for child in node.children(&mut node.walk()) {
        commas.read(child.kind(), &mut items);
        match child.kind() {
            K::MODULE_NAME => {
                if let Some(item) = unit_item(child, source, comments) {
                    items.push(item);
                    commas.pushed(&items);
                }
            }
            K::PP_USES_BLOCK | K::PP_USES_BLOCK_WITH_SEMI => {
                items.extend(
                    leading_texts(child, comments)
                        .into_iter()
                        .map(UsesItem::Comment),
                );
                let mut block = parse_pp_uses_block(child, source, comments);
                block.terminated = child.kind() == K::PP_USES_BLOCK_WITH_SEMI;
                if last_leaf(child).id() == end.id() {
                    block.trailing = without_after(block.trailing);
                }
                items.push(UsesItem::IfDefBlock(block));
                commas.pushed(&items);
            }
            K::PP_DIRECTIVE => {
                let text = node_text(child, source);
                if !text.is_empty() {
                    items.push(UsesItem::directive(text));
                    commas.pushed(&items);
                }
            }
            K::COMMA | K::SEMICOLON => {
                let mut texts = punctuation_texts(child, comments);
                if child.id() == end.id() {
                    texts = without_after(texts);
                }
                attach_after(&mut items, texts);
            }
            _ => {} // skip kUses keyword, comments, etc.
        }
    }
    UsesClause { items, after }
}

/// Format a list of `UsesItem`s with anchor-based pinning for directives/ifdef blocks.
///
/// Units are sorted/grouped according to `config`; pinned items are re-inserted
/// after their anchor unit (the unit that immediately preceded them in the
/// original list), preserving their relative order. With sorting, a block
/// is never left after the last unit: it moves in front of that unit
/// instead. Without sorting it is written comma-first.
///
/// When grouping is enabled, an `{$IFDEF}` block whose units all belong to the
/// same section is placed at the end of that section instead of being pinned
/// to its anchor unit.
pub fn format_uses_items(
    items: &[UsesItem],
    config: &UsesConfig,
    indent: &str,
    external_units: &HashSet<String>,
) -> String {
    layout_uses_items(items, &[], config, indent, external_units)
        .into_iter()
        .map(|line| line + "\n")
        .collect()
}

/// A unit's leading and trailing comments.
type UnitComments<'a> = (&'a [String], &'a [String]);

/// Like [`format_uses_items`], but return the output lines without their
/// newlines, with `after` (see [`UsesClause::after`]) following the
/// clause's end. An empty line separates groups; a line holding a
/// multi-line block comment contains its inner newlines.
///
/// A clause holding an include is written as in the source, its items in
/// their order and with their own punctuation: what the include lists is
/// unknown, so nothing around it can be sorted, grouped or re-punctuated.
pub(crate) fn layout_uses_items(
    items: &[UsesItem],
    after: &[String],
    config: &UsesConfig,
    indent: &str,
    external_units: &HashSet<String>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if has_include(items) {
        let items: Vec<Option<&UsesItem>> = items.iter().map(Some).collect();
        let style = ListStyle::Source { lead: false };
        emit_list(&items, indent, ItemEnd::Semicolon, style, after, &mut lines);
        return lines;
    }
    // Separate plain units from pinned items, recording the anchor (preceding unit name).
    // When grouping is enabled, ifdef blocks whose units all belong to one section
    // are placed in that section rather than pinned.
    let mut plain_units: Vec<String> = Vec::new();
    // Comments of each unit, queued per name in source order (sorting is
    // stable, so duplicates keep their relative order).
    let mut unit_comments: HashMap<&str, VecDeque<UnitComments>> = HashMap::new();
    // pinned: (anchor: Option<String>, item)
    // anchor is None when the pinned item appears before any unit.
    let mut pinned: Vec<(Option<String>, UsesItem)> = Vec::new();
    // section_blocks: ifdef blocks placed into a specific section.
    let mut section_blocks: Vec<(UnitSection, UsesItem)> = Vec::new();
    let mut last_unit: Option<String> = None;

    for item in items {
        match item {
            UsesItem::Unit {
                name,
                leading,
                trailing,
                ..
            } => {
                plain_units.push(name.clone());
                unit_comments
                    .entry(name.as_str())
                    .or_default()
                    .push_back((leading, trailing));
                last_unit = Some(name.clone());
            }
            UsesItem::IfDefBlock(block) if config.group => {
                if let Some(section) = classify_ifdef_block(block, config, external_units) {
                    section_blocks.push((section, item.clone()));
                } else {
                    pinned.push((last_unit.clone(), item.clone()));
                }
            }
            UsesItem::IfDefBlock(_) | UsesItem::Directive { .. } | UsesItem::Comment(_) => {
                pinned.push((last_unit.clone(), item.clone()));
            }
        }
    }

    // Sort/group plain units with section tags.
    let tagged_groups = group_units_tagged(&plain_units, config, external_units);

    // Build a flat ordered list of units (with group separators tracked via index).
    // We'll insert pinned items after we build the structure.
    // Represent the final output as a Vec of "slots": either a unit name or a pinned item.
    #[derive(Debug)]
    enum Slot {
        Unit { name: String },
        Pinned(Box<UsesItem>),
        GroupSep,
    }

    let mut slots: Vec<Slot> = Vec::new();
    let section_order = [
        UnitSection::Core,
        UnitSection::External,
        UnitSection::Project,
    ];
    let mut first_section = true;

    for &section in &section_order {
        let group = tagged_groups.iter().find(|(s, _)| *s == section);
        let blocks: Vec<_> = section_blocks
            .iter()
            .filter(|(s, _)| *s == section)
            .collect();

        if group.is_none() && blocks.is_empty() {
            continue;
        }

        if !first_section {
            slots.push(Slot::GroupSep);
        }
        first_section = false;

        if let Some((_, units)) = group {
            for name in units {
                slots.push(Slot::Unit { name: name.clone() });
            }
        }
        for (_, block_item) in &blocks {
            slots.push(Slot::Pinned(Box::new(block_item.clone())));
        }
    }

    // Re-insert pinned items after their anchor unit.
    // We iterate pinned in original order to preserve relative order for same anchor.
    // For each pinned item, find the last occurrence of the anchor unit in slots and
    // insert after it. If anchor is None, insert at the very beginning.
    let mut none_insert_pos: usize = 0;
    for (anchor, pinned_item) in pinned {
        match anchor {
            None => {
                // Insert at none_insert_pos and advance it so the next None-anchor
                // item is placed after the previous one, preserving original order.
                slots.insert(none_insert_pos, Slot::Pinned(Box::new(pinned_item)));
                none_insert_pos += 1;
            }
            Some(anchor_name) => {
                // Find the last position of the anchor unit in slots
                let pos = slots.iter().rposition(|s| match s {
                    Slot::Unit { name } => name == &anchor_name,
                    _ => false,
                });
                match pos {
                    Some(idx) => {
                        // Find the insertion point: after the anchor, but also after any
                        // already-inserted pinned items that follow it.
                        let mut insert_at = idx + 1;
                        while insert_at < slots.len() {
                            if matches!(slots[insert_at], Slot::Pinned(_)) {
                                insert_at += 1;
                            } else {
                                break;
                            }
                        }
                        slots.insert(insert_at, Slot::Pinned(Box::new(pinned_item)));
                    }
                    None => {
                        // Anchor unit was not in the sorted list (e.g. it was inside an
                        // IfDefBlock). Append at the end.
                        slots.push(Slot::Pinned(Box::new(pinned_item)));
                    }
                }
            }
        }
    }

    // Blocks after the last unit would have to be written comma-first (see
    // `list_puncts`). When sorting already reorders the units, move the
    // pinned items up to the last such block in front of that unit instead,
    // keeping one unit per line; the sections they leave behind are empty.
    // Without sorting the units keep their source order.
    if config.sort
        && let Some(last_unit) = slots.iter().rposition(|s| matches!(s, Slot::Unit { .. }))
        && let Some(last_block) = slots.iter().rposition(
            |s| matches!(s, Slot::Pinned(item) if matches!(**item, UsesItem::IfDefBlock(_))),
        )
        && last_block > last_unit
    {
        let moved: Vec<Slot> = slots
            .drain(last_unit + 1..=last_block)
            .filter(|s| !matches!(s, Slot::GroupSep))
            .collect();
        slots.splice(last_unit..last_unit, moved);
    }

    // Give the units back their comments; `None` separates groups.
    let ordered: Vec<Option<UsesItem>> = slots
        .into_iter()
        .map(|slot| match slot {
            Slot::GroupSep => None,
            Slot::Unit { name } => {
                let (leading, trailing) = unit_comments
                    .get_mut(name.as_str())
                    .and_then(VecDeque::pop_front)
                    .unwrap_or((&[], &[]));
                Some(UsesItem::Unit {
                    leading: leading.to_vec(),
                    trailing: trailing.to_vec(),
                    name,
                    comma_after: false,
                })
            }
            Slot::Pinned(item) => Some(*item),
        })
        .collect();
    let ordered: Vec<Option<&UsesItem>> = ordered.iter().map(Option::as_ref).collect();

    let style = ListStyle::Normal { comma_first: false };
    emit_list(
        &ordered,
        indent,
        ItemEnd::Semicolon,
        style,
        after,
        &mut lines,
    );
    lines
}

/// The punctuation an item of a uses clause ends with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemEnd {
    /// `,`: more units follow.
    Comma,
    /// `;`: the item ends the clause.
    Semicolon,
    /// Nothing.
    Open,
}

impl ItemEnd {
    fn text(self) -> &'static str {
        match self {
            ItemEnd::Comma => ",",
            ItemEnd::Semicolon => ";",
            ItemEnd::Open => "",
        }
    }
}

/// What an item contributes to the punctuation of its list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemRole {
    Unit,
    Block {
        terminated: bool,
    },
    Directive,
    /// Comments and group separators.
    Other,
}

impl ItemRole {
    fn of(item: &UsesItem) -> Self {
        match item {
            UsesItem::Unit { .. } => ItemRole::Unit,
            UsesItem::IfDefBlock(block) => ItemRole::Block {
                terminated: block.terminated,
            },
            UsesItem::Directive { .. } => ItemRole::Directive,
            UsesItem::Comment(_) => ItemRole::Other,
        }
    }
}

/// How an item of a uses clause is punctuated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Punct {
    /// Write `, ` before the item; for a block, before each unit inside
    /// (comma-first), as the block comes after its list's last unit.
    lead: bool,
    /// What follows the item.
    end: ItemEnd,
}

impl Punct {
    fn plain(end: ItemEnd) -> Self {
        Punct { lead: false, end }
    }

    /// What goes before the item.
    fn lead_text(self) -> &'static str {
        if self.lead { ", " } else { "" }
    }
}

/// The punctuation of each item of a list that ends with `end`; `lead`
/// writes the whole list comma-first. Otherwise the last unit or block
/// takes `end` and the units and blocks before it a `,`. Every
/// configuration must still read `unit (, unit)*`, so when the list does
/// not continue past its end and blocks follow its last unit, that unit
/// gets nothing and those blocks are written comma-first, the last of them
/// taking `end`. When directives follow the item taking a `;`, the `;` is
/// written after the last of them instead, so they stay inside the clause
/// in their source order (an `{$I}` may itself list units).
fn list_puncts(roles: &[ItemRole], end: ItemEnd, lead: bool) -> Vec<Punct> {
    if lead {
        return roles
            .iter()
            .map(|role| Punct {
                lead: *role != ItemRole::Directive,
                end: ItemEnd::Open,
            })
            .collect();
    }
    let mut puncts: Vec<Punct> = roles
        .iter()
        .map(|role| match role {
            ItemRole::Unit | ItemRole::Block { .. } => Punct::plain(ItemEnd::Comma),
            ItemRole::Directive | ItemRole::Other => Punct::plain(ItemEnd::Open),
        })
        .collect();
    let Some(last) = roles
        .iter()
        .rposition(|role| matches!(role, ItemRole::Unit | ItemRole::Block { .. }))
    else {
        return puncts;
    };
    if end != ItemEnd::Comma
        && let Some(last_unit) = roles.iter().rposition(|role| *role == ItemRole::Unit)
        && last_unit < last
    {
        puncts[last_unit] = Punct::plain(ItemEnd::Open);
        for idx in last_unit + 1..=last {
            if matches!(roles[idx], ItemRole::Block { .. }) {
                puncts[idx] = Punct {
                    lead: true,
                    end: ItemEnd::Open,
                };
            }
        }
    }
    puncts[last].end = end;
    // A terminated block already ends with the `;` inside its branches.
    if end == ItemEnd::Semicolon
        && roles[last] != (ItemRole::Block { terminated: true })
        && let Some(offset) = roles[last + 1..]
            .iter()
            .rposition(|role| *role == ItemRole::Directive)
    {
        puncts[last].end = ItemEnd::Open;
        puncts[last + 1 + offset].end = ItemEnd::Semicolon;
    }
    puncts
}

/// How the punctuation of a list is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListStyle {
    /// Laid out by [`list_puncts`], comma-first if `comma_first`.
    Normal { comma_first: bool },
    /// As in the source: each item keeps the `,` that followed it, a `,`
    /// that started the list (`lead`) goes before its first item, and the
    /// list's end follows its last item.
    Source { lead: bool },
}

/// The punctuation of each item of a list written as in the source.
fn source_puncts(items: &[Option<&UsesItem>], end: ItemEnd, lead: bool) -> Vec<Punct> {
    let mut puncts: Vec<Punct> = items
        .iter()
        .map(|item| match item {
            Some(item) if item.comma_after() => Punct::plain(ItemEnd::Comma),
            _ => Punct::plain(ItemEnd::Open),
        })
        .collect();
    let listed =
        |item: &Option<&UsesItem>| item.is_some_and(|i| !matches!(i, UsesItem::Comment(_)));
    if lead && let Some(first) = items.iter().position(listed) {
        puncts[first].lead = true;
    }
    if end == ItemEnd::Semicolon
        && let Some(last) = items.iter().rposition(listed)
    {
        puncts[last].end = ItemEnd::Semicolon;
    }
    puncts
}

/// Recursively emit a single `UsesItem` into `lines`.
fn emit_uses_item(
    item: &UsesItem,
    indent: &str,
    punct: Punct,
    source: bool,
    lines: &mut Vec<String>,
) {
    match item {
        UsesItem::Unit {
            name,
            leading,
            trailing,
            ..
        } => emit_unit(name, leading, trailing, indent, punct, lines),
        UsesItem::Directive { text, .. } => {
            lines.push(format!(
                "{indent}{}{text}{}",
                punct.lead_text(),
                punct.end.text()
            ));
        }
        UsesItem::Comment(text) => lines.push(format!("{indent}{text}")),
        UsesItem::IfDefBlock(block) => {
            emit_ifdef_block(block, indent, punct, source, lines);
        }
    }
}

/// Emit a branch's items as a list ending with `end`, written in `style`.
fn emit_branch_items(
    items: &[UsesItem],
    indent: &str,
    end: ItemEnd,
    style: ListStyle,
    lines: &mut Vec<String>,
) {
    let items: Vec<Option<&UsesItem>> = items.iter().map(Some).collect();
    emit_list(&items, indent, end, style, &[], lines);
}

/// Emit a list of items (`None` is a group separator) ending with `end`,
/// written in `style`. Comments after a directive carrying the `;` stay
/// on its line, so they remain inside the clause. A list with nothing to
/// carry a `;` gets it alone on a line. `after` follows the `;` on its
/// line (see [`with_after`]).
fn emit_list(
    items: &[Option<&UsesItem>],
    indent: &str,
    end: ItemEnd,
    style: ListStyle,
    after: &[String],
    lines: &mut Vec<String>,
) {
    let puncts = match style {
        ListStyle::Normal { comma_first } => {
            let roles: Vec<ItemRole> = items
                .iter()
                .map(|item| item.map_or(ItemRole::Other, ItemRole::of))
                .collect();
            list_puncts(&roles, end, comma_first)
        }
        ListStyle::Source { lead } => source_puncts(items, end, lead),
    };
    let source = matches!(style, ListStyle::Source { .. });
    // A `,` that starts a list with no item to write it before gets a line.
    if style == (ListStyle::Source { lead: true }) && !puncts.iter().any(|p| p.lead) {
        lines.push(format!("{indent},"));
    }
    let mut idx = 0;
    while idx < items.len() {
        let ends_clause = puncts[idx].end == ItemEnd::Semicolon;
        match items[idx] {
            None => lines.push(String::new()),
            Some(UsesItem::Directive { text, .. }) if ends_clause => {
                let mut trailing = Vec::new();
                while let Some(Some(UsesItem::Comment(comment))) = items.get(idx + 1) {
                    trailing.push(comment.clone());
                    idx += 1;
                }
                let trailing = with_after(&trailing, after);
                let lead = puncts[idx].lead_text();
                push_with_trailing(format!("{indent}{lead}{text};"), &trailing, indent, lines);
            }
            Some(UsesItem::Unit {
                name,
                leading,
                trailing,
                ..
            }) if ends_clause => {
                let trailing = with_after(trailing, after);
                emit_unit(name, leading, &trailing, indent, puncts[idx], lines);
            }
            Some(UsesItem::IfDefBlock(block)) if ends_clause => {
                let block = IfDefBlock {
                    trailing: with_after(&block.trailing, after),
                    ..block.clone()
                };
                emit_ifdef_block(&block, indent, puncts[idx], source, lines);
            }
            Some(item) => emit_uses_item(item, indent, puncts[idx], source, lines),
        }
        idx += 1;
    }
    if end == ItemEnd::Semicolon && !puncts.iter().any(|p| p.end == ItemEnd::Semicolon) {
        push_with_trailing(format!("{indent};"), after, indent, lines);
    }
}

/// The comments of the item ending a clause, then `after`, the trivia
/// after the clause's end; `after` goes first when a `//` comment among
/// them would swallow it.
fn with_after(trailing: &[String], after: &[String]) -> Vec<String> {
    if trailing.iter().any(|c| c.starts_with("//")) {
        after.iter().chain(trailing).cloned().collect()
    } else {
        trailing.iter().chain(after).cloned().collect()
    }
}

/// Emit a unit line, its leading comments on their own lines above it.
fn emit_unit(
    name: &str,
    leading: &[String],
    trailing: &[String],
    indent: &str,
    punct: Punct,
    lines: &mut Vec<String>,
) {
    for comment in leading {
        lines.push(format!("{indent}{comment}"));
    }
    push_with_trailing(
        format!("{indent}{}{name}{}", punct.lead_text(), punct.end.text()),
        trailing,
        indent,
        lines,
    );
}

/// Push `line` followed by its trailing comments. A comment after a `//`
/// comment starts its own line, since the `//` would swallow it.
fn push_with_trailing(
    mut line: String,
    trailing: &[String],
    indent: &str,
    lines: &mut Vec<String>,
) {
    let mut after_line_comment = false;
    for comment in trailing {
        if after_line_comment {
            lines.push(std::mem::replace(&mut line, format!("{indent}{comment}")));
        } else {
            line.push(' ');
            line.push_str(comment);
        }
        after_line_comment = comment.starts_with("//");
    }
    lines.push(line);
}

/// Emit an `IfDefBlock` punctuated by `punct`. Followed by more units,
/// every unit inside gets a `,`. Written comma-first, every unit inside
/// gets a leading `, ` and `punct.end` follows the `{$ENDIF}`. Otherwise
/// each branch is a list ending the clause: a terminated block ends each
/// branch with the `;`, other blocks end with nothing and a `;` follows the
/// `{$ENDIF}`.
///
/// Written as in the source (`source`), each branch keeps its own
/// punctuation, `punct.end` follows the `{$ENDIF}` (except a terminated
/// block's `;`, which ends each branch), and a leading `,` gets a line.
fn emit_ifdef_block(
    block: &IfDefBlock,
    indent: &str,
    punct: Punct,
    source: bool,
    lines: &mut Vec<String>,
) {
    let (branch_end, comma_first, after_endif) = match punct.end {
        ItemEnd::Semicolon if block.terminated => (ItemEnd::Semicolon, false, ""),
        _ if source => (ItemEnd::Open, false, punct.end.text()),
        _ if punct.lead => (ItemEnd::Open, true, punct.end.text()),
        ItemEnd::Comma => (ItemEnd::Comma, false, ""),
        ItemEnd::Semicolon => (ItemEnd::Open, false, ";"),
        ItemEnd::Open => (ItemEnd::Open, false, ""),
    };
    let style = |lead: bool| {
        if source {
            ListStyle::Source { lead }
        } else {
            ListStyle::Normal { comma_first }
        }
    };
    if source && punct.lead {
        lines.push(format!("{indent},"));
    }

    // Emit if_branch directive
    push_with_trailing(
        format!("{indent}{}", block.if_branch.directive),
        &block.if_branch.trailing,
        indent,
        lines,
    );
    let if_style = style(block.if_branch.lead);
    emit_branch_items(&block.if_branch.items, indent, branch_end, if_style, lines);

    // Emit elseif branches
    for branch in &block.else_if_branches {
        push_with_trailing(
            format!("{indent}{}", branch.directive),
            &branch.trailing,
            indent,
            lines,
        );
        emit_branch_items(&branch.items, indent, branch_end, style(branch.lead), lines);
    }

    // Emit else branch
    if let Some(else_items) = &block.else_branch {
        push_with_trailing(
            format!("{indent}{}", block.else_directive),
            &block.else_trailing,
            indent,
            lines,
        );
        emit_branch_items(
            else_items,
            indent,
            branch_end,
            style(block.else_lead),
            lines,
        );
    }

    // Emit endif
    push_with_trailing(
        format!("{indent}{}{after_endif}", block.endif),
        &block.trailing,
        indent,
        lines,
    );
}

/// Recursively scan directories for `.pas` files and collect unit names (lowercased).
pub fn scan_external_paths(project_root: &Path, external_paths: &[String]) -> HashSet<String> {
    let mut units = HashSet::new();
    for rel_path in external_paths {
        let dir = project_root.join(rel_path);
        for entry in walkdir::WalkDir::new(&dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if let Some(ext) = path.extension()
                && ext.eq_ignore_ascii_case("pas")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                units.insert(stem.to_lowercase());
            }
        }
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UsesConfig;

    fn default_config() -> UsesConfig {
        UsesConfig {
            group: true,
            ..UsesConfig::default()
        }
    }

    #[test]
    fn dotted_core_units_classified() {
        assert_eq!(
            classify_unit("System.SysUtils", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("Vcl.Forms", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("Fmx.Types", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("Data.DB", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("FireDAC.Comp.Client", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("REST.Client", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("Xml.XMLDoc", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("Winapi.Windows", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
    }

    #[test]
    fn legacy_core_units_classified() {
        assert_eq!(
            classify_unit("SysUtils", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("Classes", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("Forms", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("DB", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("Windows", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
    }

    #[test]
    fn legacy_core_units_case_insensitive() {
        assert_eq!(
            classify_unit("classes", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("SYSUTILS", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("forms", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("db", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("windows", &default_config(), &HashSet::new()),
            UnitSection::Core
        );
    }

    #[test]
    fn unknown_unit_classified_as_project() {
        assert_eq!(
            classify_unit("MyApp.MainForm", &default_config(), &HashSet::new()),
            UnitSection::Project
        );
        assert_eq!(
            classify_unit("MyUnit", &default_config(), &HashSet::new()),
            UnitSection::Project
        );
    }

    #[test]
    fn external_prefix_classified() {
        let mut config = default_config();
        config.external_prefixes = vec!["Spring".to_string(), "Neon".to_string()];
        let empty = HashSet::new();
        assert_eq!(
            classify_unit("Spring.Container", &config, &empty),
            UnitSection::External
        );
        assert_eq!(
            classify_unit("Neon.JSON", &config, &empty),
            UnitSection::External
        );
        assert_eq!(
            classify_unit("MyApp.Utils", &config, &empty),
            UnitSection::Project
        );
    }

    #[test]
    fn external_scanned_unit_classified() {
        let config = default_config();
        let mut external_units = HashSet::new();
        external_units.insert("superobject".to_string());
        assert_eq!(
            classify_unit("SuperObject", &config, &external_units),
            UnitSection::External
        );
    }

    #[test]
    fn scan_external_paths_finds_pas_files() {
        use std::fs;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let vendor = dir.path().join("vendor");
        fs::create_dir_all(vendor.join("sub")).unwrap();
        fs::write(
            vendor.join("Spring.Container.pas"),
            "unit Spring.Container;",
        )
        .unwrap();
        fs::write(vendor.join("sub").join("Neon.JSON.pas"), "unit Neon.JSON;").unwrap();
        fs::write(vendor.join("README.md"), "not a pascal file").unwrap();

        let result = scan_external_paths(dir.path(), &["vendor".to_string()]);
        assert!(result.contains("spring.container"));
        assert!(result.contains("neon.json"));
        assert!(!result.contains("readme"));
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn scan_external_paths_empty_config() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let result = scan_external_paths(dir.path(), &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn scan_external_paths_missing_dir() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let result = scan_external_paths(dir.path(), &["nonexistent".to_string()]);
        assert!(result.is_empty());
    }

    #[test]
    fn core_takes_precedence_over_external() {
        let mut config = default_config();
        config.external_prefixes = vec!["System".to_string()];
        let mut external_units = HashSet::new();
        external_units.insert("system.sysutils".to_string());
        assert_eq!(
            classify_unit("System.SysUtils", &config, &external_units),
            UnitSection::Core
        );
    }

    #[test]
    fn group_units_three_sections() {
        let mut config = default_config();
        config.external_prefixes = vec!["Spring".to_string()];
        let units = vec![
            "MyApp.MainForm".to_string(),
            "System.SysUtils".to_string(),
            "Spring.Container".to_string(),
            "System.Classes".to_string(),
            "MyApp.Utils".to_string(),
            "Spring.Collections".to_string(),
            "Vcl.Forms".to_string(),
        ];
        let groups = group_units(&units, &config, &HashSet::new());
        assert_eq!(groups.len(), 3);
        // Core: alphabetical
        assert_eq!(
            groups[0],
            vec!["System.Classes", "System.SysUtils", "Vcl.Forms"]
        );
        // External: alphabetical
        assert_eq!(groups[1], vec!["Spring.Collections", "Spring.Container"]);
        // Project: alphabetical
        assert_eq!(groups[2], vec!["MyApp.MainForm", "MyApp.Utils"]);
    }

    #[test]
    fn group_units_skips_empty_sections() {
        let config = default_config();
        let units = vec!["System.SysUtils".to_string(), "MyApp.MainForm".to_string()];
        let groups = group_units(&units, &config, &HashSet::new());
        // No external configured, so only 2 sections
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0], vec!["System.SysUtils"]);
        assert_eq!(groups[1], vec!["MyApp.MainForm"]);
    }

    #[test]
    fn group_units_no_grouping() {
        let mut config = default_config();
        config.group = false;
        let units = vec!["B".to_string(), "A".to_string()];
        let groups = group_units(&units, &config, &HashSet::new());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0], vec!["A", "B"]);
    }

    #[test]
    fn format_uses_three_sections() {
        let mut config = default_config();
        config.external_prefixes = vec!["Spring".to_string()];
        let items: Vec<UsesItem> = vec![
            "Vcl.Forms",
            "System.SysUtils",
            "Spring.Container",
            "MyApp.Utils",
        ]
        .into_iter()
        .map(UsesItem::unit)
        .collect();
        let output = format_uses_items(&items, &config, "  ", &HashSet::new());
        let expected =
            "  System.SysUtils,\n  Vcl.Forms,\n\n  Spring.Container,\n\n  MyApp.Utils;\n";
        assert_eq!(output, expected);
    }

    // ─── Task 5: Data model constructability ─────────────────────────────────

    #[test]
    fn uses_item_unit_constructable() {
        let item = UsesItem::unit("SysUtils");
        match item {
            UsesItem::Unit { name, .. } => assert_eq!(name, "SysUtils"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn uses_item_directive_constructable() {
        let item = UsesItem::directive("{$I compilers.inc}");
        match item {
            UsesItem::Directive { text, .. } => assert_eq!(text, "{$I compilers.inc}"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn uses_item_ifdef_block_constructable() {
        let block = IfDefBlock {
            if_branch: CondBranch {
                directive: "{$IFDEF FOO}".to_string(),
                items: vec![UsesItem::unit("SpecialUnit")],
                ..CondBranch::default()
            },
            else_if_branches: Vec::new(),
            else_branch: Some(vec![UsesItem::unit("OtherUnit")]),
            endif: "{$ENDIF}".to_string(),
            ..IfDefBlock::default()
        };
        assert_eq!(block.if_branch.directive, "{$IFDEF FOO}");
        assert_eq!(block.endif, "{$ENDIF}");
        assert!(block.else_branch.is_some());
    }

    // ─── Task 6: extract_uses_items() ────────────────────────────────────────

    fn parse_source(src: &str) -> (tree_sitter::Tree, Vec<u8>) {
        let bytes = src.as_bytes().to_vec();
        let info = pascal_core::FileInfo::new(std::path::PathBuf::from("test.pas"));
        let (tree, _) = pascal_core::parser::parse_file(&info, &bytes).unwrap();
        (tree, bytes)
    }

    fn find_decl_uses(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
        if node.kind() == "declUses" {
            return Some(node);
        }
        for child in node.children(&mut node.walk()) {
            if let Some(found) = find_decl_uses(child) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn extract_plain_units() {
        let src = "unit Foo;\ninterface\nuses\n  SysUtils,\n  Classes;\nimplementation\nend.";
        let (tree, bytes) = parse_source(src);
        let uses_node = find_decl_uses(tree.root_node()).expect("no declUses");
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let items = extract_uses_items(uses_node, &bytes, &comments, &DirectiveMap::empty()).items;
        assert_eq!(items.len(), 2);
        match &items[0] {
            UsesItem::Unit { name, .. } => assert_eq!(name, "SysUtils"),
            _ => panic!("expected Unit"),
        }
        match &items[1] {
            UsesItem::Unit { name, .. } => assert_eq!(name, "Classes"),
            _ => panic!("expected Unit"),
        }
    }

    #[test]
    fn extract_ifdef_block() {
        let src = concat!(
            "unit Foo;\ninterface\nuses\n",
            "  SysUtils,\n",
            "  {$IFDEF FOO}\n",
            "  SpecialUnit,\n",
            "  {$ELSE}\n",
            "  OtherUnit,\n",
            "  {$ENDIF}\n",
            "  Classes;\nimplementation\nend."
        );
        let (tree, bytes) = parse_source(src);
        let uses_node = find_decl_uses(tree.root_node()).expect("no declUses");
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let items = extract_uses_items(uses_node, &bytes, &comments, &DirectiveMap::empty()).items;

        // Expect: Unit(SysUtils), IfDefBlock(...), Unit(Classes)
        assert_eq!(items.len(), 3);
        match &items[0] {
            UsesItem::Unit { name, .. } => assert_eq!(name, "SysUtils"),
            _ => panic!("expected Unit at 0"),
        }
        match &items[1] {
            UsesItem::IfDefBlock(block) => {
                assert!(block.if_branch.directive.contains("IFDEF"));
                assert_eq!(block.if_branch.items.len(), 1);
                match &block.if_branch.items[0] {
                    UsesItem::Unit { name, .. } => assert_eq!(name, "SpecialUnit"),
                    _ => panic!("expected Unit in if_branch"),
                }
                assert!(block.else_branch.is_some());
                let else_items = block.else_branch.as_ref().unwrap();
                assert_eq!(else_items.len(), 1);
                match &else_items[0] {
                    UsesItem::Unit { name, .. } => assert_eq!(name, "OtherUnit"),
                    _ => panic!("expected Unit in else_branch"),
                }
                assert!(block.endif.contains("ENDIF"));
            }
            _ => panic!("expected IfDefBlock at 1"),
        }
        match &items[2] {
            UsesItem::Unit { name, .. } => assert_eq!(name, "Classes"),
            _ => panic!("expected Unit at 2"),
        }
    }

    #[test]
    fn extract_standalone_directive() {
        let src = concat!(
            "unit Foo;\ninterface\nuses\n",
            "  {$I compilers.inc}\n",
            "  SysUtils;\nimplementation\nend."
        );
        let (tree, bytes) = parse_source(src);
        let uses_node = find_decl_uses(tree.root_node()).expect("no declUses");
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let items = extract_uses_items(uses_node, &bytes, &comments, &DirectiveMap::empty()).items;

        // ppDirective is an extra — it may appear before SysUtils
        let directive_items: Vec<_> = items
            .iter()
            .filter(|i| matches!(i, UsesItem::Directive { .. }))
            .collect();
        assert!(
            !directive_items.is_empty(),
            "expected at least one Directive"
        );
        match &directive_items[0] {
            UsesItem::Directive { text, .. } => assert!(text.contains("compilers.inc")),
            _ => panic!("expected Directive"),
        }
    }

    #[test]
    fn extract_nested_ifdef() {
        let src = concat!(
            "unit Foo;\ninterface\nuses\n",
            "  {$IFDEF OUTER}\n",
            "  OuterUnit,\n",
            "  {$IFDEF INNER}\n",
            "  InnerUnit,\n",
            "  {$ENDIF}\n",
            "  {$ENDIF}\n",
            "  Classes;\nimplementation\nend."
        );
        let (tree, bytes) = parse_source(src);
        let uses_node = find_decl_uses(tree.root_node()).expect("no declUses");
        let comments = CommentMap::build(tree.root_node(), &bytes);
        let items = extract_uses_items(uses_node, &bytes, &comments, &DirectiveMap::empty()).items;

        // Find the outer IfDefBlock
        let outer_block = items.iter().find_map(|i| match i {
            UsesItem::IfDefBlock(b) => Some(b),
            _ => None,
        });
        assert!(outer_block.is_some(), "expected outer IfDefBlock");
        let outer = outer_block.unwrap();
        assert!(outer.if_branch.directive.contains("OUTER"));

        // Find a nested IfDefBlock inside the outer if_branch items
        let has_nested = outer
            .if_branch
            .items
            .iter()
            .any(|i| matches!(i, UsesItem::IfDefBlock(_)));
        assert!(
            has_nested,
            "expected nested IfDefBlock inside outer if_branch"
        );
    }

    // ─── Task 7: format_uses_items() ─────────────────────────────────────────

    #[test]
    fn format_items_ifdef_block_left_last_moves_before_last_unit() {
        // SysUtils, {$IFDEF FOO} SpecialUnit {$ELSE} OtherUnit {$ENDIF}, Classes
        // After sort (no grouping here): Classes, SysUtils
        let config = UsesConfig {
            sort: true,
            group: false,
            ..UsesConfig::default()
        };

        let block = IfDefBlock {
            if_branch: CondBranch {
                directive: "{$IFDEF FOO}".to_string(),
                items: vec![UsesItem::unit("SpecialUnit")],
                ..CondBranch::default()
            },
            else_if_branches: Vec::new(),
            else_branch: Some(vec![UsesItem::unit("OtherUnit")]),
            else_directive: "{$ELSE}".to_string(),
            endif: "{$ENDIF}".to_string(),
            ..IfDefBlock::default()
        };

        let items = vec![
            UsesItem::unit("SysUtils"),
            UsesItem::IfDefBlock(block),
            UsesItem::unit("Classes"),
        ];

        let output = format_uses_items(&items, &config, "  ", &HashSet::new());
        // The block would follow its anchor SysUtils and end the clause,
        // leaving `SpecialUnit,` dangling; it moves in front of SysUtils.
        assert_eq!(
            output,
            "  Classes,\n  {$IFDEF FOO}\n  SpecialUnit,\n  {$ELSE}\n  OtherUnit,\n  {$ENDIF}\n  SysUtils;\n"
        );
    }

    #[test]
    fn format_items_block_left_last_leaves_its_empty_section() {
        // A Project-only block forms a section of its own after the Core units.
        let mut config = default_config();
        config.sort = true;
        let block = IfDefBlock {
            if_branch: CondBranch {
                directive: "{$IFDEF X}".to_string(),
                items: vec![UsesItem::unit("MyApp.Extra")],
                ..CondBranch::default()
            },
            endif: "{$ENDIF}".to_string(),
            ..IfDefBlock::default()
        };
        let items = vec![
            UsesItem::unit("SysUtils"),
            UsesItem::IfDefBlock(block),
            UsesItem::unit("Classes"),
        ];
        let output = format_uses_items(&items, &config, "  ", &HashSet::new());
        assert_eq!(
            output,
            "  Classes,\n  {$IFDEF X}\n  MyApp.Extra,\n  {$ENDIF}\n  SysUtils;\n"
        );
    }

    #[test]
    fn format_items_directive_at_start_stays_first() {
        // Directive with anchor=None should stay at the very beginning.
        let config = UsesConfig {
            sort: true,
            group: false,
            ..UsesConfig::default()
        };

        let items = vec![
            UsesItem::directive("{$HINTS OFF}"),
            UsesItem::unit("SysUtils"),
            UsesItem::unit("Classes"),
        ];

        let output = format_uses_items(&items, &config, "  ", &HashSet::new());
        // Directive should be first
        assert!(
            output.starts_with("  {$HINTS OFF}\n"),
            "directive should be first: {output:?}"
        );
        // Classes sorts before SysUtils
        let classes_pos = output.find("  Classes,\n").unwrap();
        let sysutils_pos = output.find("  SysUtils;\n").unwrap();
        assert!(classes_pos < sysutils_pos);
    }

    #[test]
    fn format_items_directive_between_units_follows_anchor() {
        // SysUtils, {$HINTS OFF}, Classes
        // After sort: Classes, SysUtils
        // Directive anchor = SysUtils → inserted after SysUtils
        let config = UsesConfig {
            sort: true,
            group: false,
            ..UsesConfig::default()
        };

        let items = vec![
            UsesItem::unit("SysUtils"),
            UsesItem::directive("{$HINTS OFF}"),
            UsesItem::unit("Classes"),
        ];

        let output = format_uses_items(&items, &config, "  ", &HashSet::new());
        // The directive now ends the clause, so the `;` follows it.
        assert_eq!(output, "  Classes,\n  SysUtils\n  {$HINTS OFF};\n");
    }

    #[test]
    fn format_items_with_an_include_keep_their_order() {
        // What the include lists is unknown, so nothing is sorted around it.
        let items = vec![
            UsesItem::Unit {
                name: "SysUtils".to_string(),
                leading: Vec::new(),
                trailing: Vec::new(),
                comma_after: true,
            },
            UsesItem::Directive {
                text: "{$I myinc.inc}".to_string(),
                comma_after: true,
            },
            UsesItem::unit("Classes"),
        ];
        let output = format_uses_items(&items, &default_config(), "  ", &HashSet::new());
        assert_eq!(output, "  SysUtils,\n  {$I myinc.inc},\n  Classes;\n");
    }

    #[test]
    fn include_directives_are_recognised() {
        for text in [
            "{$I a.inc}",
            "{$i 'b c.inc'}",
            "{$INCLUDE a.inc}",
            "(*$I a.inc*)",
        ] {
            assert!(is_include_directive(text), "{text}");
        }
        for text in [
            "{$I+}",
            "{$I-}",
            "{$IFDEF X}",
            "{$HINTS OFF}",
            "{$R *.res}",
            "(*$I+*)",
        ] {
            assert!(!is_include_directive(text), "{text}");
        }
    }

    #[test]
    fn format_items_multiple_directives_at_start_preserve_order() {
        let items = vec![
            UsesItem::directive("{$I a.inc}"),
            UsesItem::directive("{$I b.inc}"),
            UsesItem::unit("SysUtils"),
        ];
        let output = format_uses_items(&items, &default_config(), "  ", &HashSet::new());
        let a_pos = output.find("{$I a.inc}").expect("a.inc missing");
        let b_pos = output.find("{$I b.inc}").expect("b.inc missing");
        assert!(
            a_pos < b_pos,
            "a.inc should appear before b.inc, got:\n{}",
            output
        );
    }

    // ─── Section-placement for IfDef blocks ───────────────────────────────

    #[test]
    fn ifdef_all_core_placed_in_core_section() {
        // All units in the ifdef are Core → block goes to Core section,
        // not pinned to the anchor (which is a Project unit).
        let mut config = default_config();
        config.sort = true;

        let block = IfDefBlock {
            if_branch: CondBranch {
                directive: "{$IFDEF DELPHI_XE6_UP}".to_string(),
                items: vec![
                    UsesItem::unit("ibx.IBDatabase"),
                    UsesItem::unit("ibx.IBSQL"),
                ],
                ..CondBranch::default()
            },
            else_if_branches: Vec::new(),
            else_branch: Some(vec![UsesItem::unit("IBDatabase"), UsesItem::unit("IBSQL")]),
            endif: "{$ENDIF}".to_string(),
            ..IfDefBlock::default()
        };

        let items = vec![
            UsesItem::unit("MDIBDatabase"),
            UsesItem::IfDefBlock(block),
            UsesItem::unit("Utils"),
            UsesItem::unit("ibxUtils"),
        ];

        let output = format_uses_items(&items, &config, "  ", &HashSet::new());
        // The ifdef block should be in the Core section (before the group separator),
        // not pinned after MDIBDatabase in the Project section.
        let endif_pos = output.find("{$ENDIF}").expect("ENDIF missing");
        let group_sep = output.find("\n\n").expect("group separator missing");
        assert!(
            endif_pos < group_sep,
            "ifdef block should be in Core section (before separator):\n{output}"
        );
    }

    #[test]
    fn ifdef_mixed_sections_stays_pinned() {
        // Units in the ifdef are in different sections → stays pinned to anchor.
        let mut config = default_config();
        config.sort = true;

        let block = IfDefBlock {
            if_branch: CondBranch {
                directive: "{$IFDEF FOO}".to_string(),
                items: vec![UsesItem::unit("System.SysUtils")],
                ..CondBranch::default()
            },
            else_if_branches: Vec::new(),
            else_branch: Some(vec![UsesItem::unit("MyProject.Utils")]),
            endif: "{$ENDIF}".to_string(),
            ..IfDefBlock::default()
        };

        let items = vec![
            UsesItem::unit("MyApp.Main"),
            UsesItem::IfDefBlock(block),
            UsesItem::unit("Vcl.Forms"),
            // Keeps a unit after the block (a block is never left last).
            UsesItem::unit("MyApp.Zed"),
        ];

        let output = format_uses_items(&items, &config, "  ", &HashSet::new());
        // The ifdef block should stay pinned after MyApp.Main (its anchor) in Project section.
        let main_pos = output.find("MyApp.Main").expect("MyApp.Main missing");
        let ifdef_pos = output.find("{$IFDEF FOO}").expect("IFDEF missing");
        assert!(
            main_pos < ifdef_pos,
            "ifdef should follow its anchor MyApp.Main:\n{output}"
        );
    }

    #[test]
    fn ifdef_creates_section_when_only_block_units() {
        // No plain Core units, but the ifdef block is all-Core.
        // A Core section should be created for it.
        let mut config = default_config();
        config.sort = true;

        let block = IfDefBlock {
            if_branch: CondBranch {
                directive: "{$IFDEF XE6}".to_string(),
                items: vec![UsesItem::unit("ibx.IBDatabase")],
                ..CondBranch::default()
            },
            else_if_branches: Vec::new(),
            else_branch: Some(vec![UsesItem::unit("IBDatabase")]),
            endif: "{$ENDIF}".to_string(),
            ..IfDefBlock::default()
        };

        let items = vec![UsesItem::unit("MyApp.Main"), UsesItem::IfDefBlock(block)];

        let output = format_uses_items(&items, &config, "  ", &HashSet::new());
        // Core section (ifdef block) should come before Project section (MyApp.Main).
        let ifdef_pos = output.find("{$IFDEF XE6}").expect("IFDEF missing");
        let main_pos = output.find("MyApp.Main").expect("MyApp.Main missing");
        assert!(
            ifdef_pos < main_pos,
            "ifdef Core section should precede Project section:\n{output}"
        );
    }

    #[test]
    fn ifdef_no_section_placement_without_grouping() {
        // Grouping disabled → ifdef block stays pinned (no section placement).
        let config = UsesConfig {
            sort: true,
            group: false,
            ..UsesConfig::default()
        };

        let block = IfDefBlock {
            if_branch: CondBranch {
                directive: "{$IFDEF FOO}".to_string(),
                items: vec![UsesItem::unit("System.SysUtils")],
                ..CondBranch::default()
            },
            else_if_branches: Vec::new(),
            else_branch: Some(vec![UsesItem::unit("Classes")]),
            endif: "{$ENDIF}".to_string(),
            ..IfDefBlock::default()
        };

        let items = vec![
            UsesItem::unit("Zebra"),
            UsesItem::IfDefBlock(block),
            UsesItem::unit("Alpha"),
            // Keeps a unit after the block (a block is never left last).
            UsesItem::unit("Zulu"),
        ];

        let output = format_uses_items(&items, &config, "  ", &HashSet::new());
        // Without grouping: Alpha sorts first, Zebra second, block pinned after Zebra.
        let alpha_pos = output.find("Alpha").expect("Alpha missing");
        let zebra_pos = output.find("Zebra").expect("Zebra missing");
        let ifdef_pos = output.find("{$IFDEF FOO}").expect("IFDEF missing");
        assert!(alpha_pos < zebra_pos, "Alpha before Zebra");
        assert!(
            zebra_pos < ifdef_pos,
            "ifdef follows anchor Zebra:\n{output}"
        );
    }

    #[test]
    fn ibx_legacy_units_classify_as_core() {
        let config = default_config();
        assert_eq!(
            classify_unit("IBDatabase", &config, &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("IBSQL", &config, &HashSet::new()),
            UnitSection::Core
        );
        assert_eq!(
            classify_unit("IBQuery", &config, &HashSet::new()),
            UnitSection::Core
        );
    }

    #[test]
    fn extract_ifdef_with_elseif() {
        let src = b"unit T;\ninterface\nuses\n  {$IFDEF XE6}\n  XE6Unit,\n  {$ELSEIF XE5}\n  XE5Unit,\n  {$ELSE}\n  OldUnit,\n  {$ENDIF}\n  Classes;\nimplementation\nend.\n";
        let info = pascal_core::FileInfo::new(std::path::PathBuf::from("test.pas"));
        let (tree, _) = pascal_core::parser::parse_file(&info, src).unwrap();
        let uses_node = find_decl_uses(tree.root_node()).unwrap();
        let comments = CommentMap::build(tree.root_node(), src);
        let items = extract_uses_items(uses_node, src, &comments, &DirectiveMap::empty()).items;
        // IfDefBlock + Classes
        assert_eq!(items.len(), 2);
        if let UsesItem::IfDefBlock(block) = &items[0] {
            assert!(block.if_branch.directive.contains("IFDEF XE6"));
            assert_eq!(block.if_branch.items.len(), 1);
            assert_eq!(
                block.else_if_branches.len(),
                1,
                "expected one elseif branch"
            );
            assert!(block.else_if_branches[0].directive.contains("ELSEIF XE5"));
            assert!(block.else_branch.is_some(), "expected else branch");
        } else {
            panic!("expected IfDefBlock as first item");
        }
    }
}
