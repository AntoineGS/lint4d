use crate::comments::CommentMap;
use crate::config::UsesConfig;
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
    },
    /// An {$IFDEF}...{$ENDIF} block — pinned in position, contents untouched.
    IfDefBlock(IfDefBlock),
    /// A standalone directive ({$I ...}, {$HINTS OFF}, etc.) — pinned in position.
    Directive(String),
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
        }
    }
}

/// A complete {$IFDEF}...{$ENDIF} conditional block.
#[derive(Debug, Clone, Default)]
pub struct IfDefBlock {
    /// The opening condition branch.
    pub if_branch: CondBranch,
    /// Zero or more {$ELSEIF ...} branches.
    pub else_if_branches: Vec<CondBranch>,
    /// Optional {$ELSE} fallback branch (units only, directive text is implicit "{$ELSE}").
    pub else_branch: Option<Vec<UsesItem>>,
    /// Comments on the `{$ELSE}` line.
    pub else_trailing: Vec<String>,
    /// The closing directive text, e.g. "{$ENDIF}".
    pub endif: String,
    /// Comments on the `{$ENDIF}` line, after any `;`.
    pub trailing: Vec<String>,
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
            UsesItem::Directive(_) | UsesItem::Comment(_) => {}
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
    })
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

    for child in node.children(&mut node.walk()) {
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
                    });
                    state = BranchState::ElseIf;
                } else {
                    // It's a plain {$ELSE}
                    block.else_branch = Some(Vec::new());
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
                    branch_items(&mut block, state).push(item);
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
            }
            K::PP_DIRECTIVE => {
                let text = node_text(child, source);
                branch_items(&mut block, state).push(UsesItem::Directive(text));
            }
            K::COMMA | K::SEMICOLON => {
                let texts = punctuation_texts(child, comments);
                if ended {
                    block.trailing.extend(texts);
                } else {
                    attach_after(branch_items(&mut block, state), texts);
                }
            }
            _ => {} // skip comments (taken from `comments`), etc.
        }
    }

    block
}

/// Extract all items from a `declUses` node into a `Vec<UsesItem>`.
///
/// Comments come from `comments`; those attached to the `uses` keyword
/// are left to the caller.
pub(crate) fn extract_uses_items(
    node: tree_sitter::Node,
    source: &[u8],
    comments: &CommentMap,
) -> Vec<UsesItem> {
    let mut items = Vec::new();
    for child in node.children(&mut node.walk()) {
        match child.kind() {
            K::MODULE_NAME => items.extend(unit_item(child, source, comments)),
            K::PP_USES_BLOCK | K::PP_USES_BLOCK_WITH_SEMI => {
                items.extend(
                    leading_texts(child, comments)
                        .into_iter()
                        .map(UsesItem::Comment),
                );
                items.push(UsesItem::IfDefBlock(parse_pp_uses_block(
                    child, source, comments,
                )));
            }
            K::PP_DIRECTIVE => {
                let text = node_text(child, source);
                if !text.is_empty() {
                    items.push(UsesItem::Directive(text));
                }
            }
            K::COMMA | K::SEMICOLON => attach_after(&mut items, punctuation_texts(child, comments)),
            _ => {} // skip kUses keyword, comments, etc.
        }
    }
    items
}

/// Format a list of `UsesItem`s with anchor-based pinning for directives/ifdef blocks.
///
/// Units are sorted/grouped according to `config`; pinned items are re-inserted
/// after their anchor unit (the unit that immediately preceded them in the
/// original list), preserving their relative order.
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
    layout_uses_items(items, config, indent, external_units)
        .into_iter()
        .map(|line| line + "\n")
        .collect()
}

/// A unit's leading and trailing comments.
type UnitComments<'a> = (&'a [String], &'a [String]);

/// Like [`format_uses_items`], but return the output lines without their
/// newlines. An empty line separates groups; a line holding a multi-line
/// block comment contains its inner newlines.
pub(crate) fn layout_uses_items(
    items: &[UsesItem],
    config: &UsesConfig,
    indent: &str,
    external_units: &HashSet<String>,
) -> Vec<String> {
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
            UsesItem::IfDefBlock(_) | UsesItem::Directive(_) | UsesItem::Comment(_) => {
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
        Pinned(UsesItem),
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
            slots.push(Slot::Pinned(block_item.clone()));
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
                slots.insert(none_insert_pos, Slot::Pinned(pinned_item));
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
                        slots.insert(insert_at, Slot::Pinned(pinned_item));
                    }
                    None => {
                        // Anchor unit was not in the sorted list (e.g. it was inside an
                        // IfDefBlock). Append at the end.
                        slots.push(Slot::Pinned(pinned_item));
                    }
                }
            }
        }
    }

    // The last unit or ifdef block takes the clause's semicolon; pinned
    // comments after it cannot.
    let last_real_idx = slots
        .iter()
        .rposition(|s| !matches!(s, Slot::GroupSep | Slot::Pinned(UsesItem::Comment(_))))
        .unwrap_or(0);

    // Emit the output.
    let mut lines = Vec::new();
    for (slot_idx, slot) in slots.iter().enumerate() {
        let is_last = slot_idx == last_real_idx;
        match slot {
            Slot::GroupSep => lines.push(String::new()),
            Slot::Unit { name } => {
                let (leading, trailing) = unit_comments
                    .get_mut(name.as_str())
                    .and_then(VecDeque::pop_front)
                    .unwrap_or((&[], &[]));
                emit_unit(name, leading, trailing, indent, is_last, &mut lines);
            }
            Slot::Pinned(item) => {
                emit_uses_item(item, indent, is_last, &mut lines);
            }
        }
    }

    lines
}

/// Recursively emit a single `UsesItem` into `lines`.
fn emit_uses_item(item: &UsesItem, indent: &str, is_last_overall: bool, lines: &mut Vec<String>) {
    match item {
        UsesItem::Unit {
            name,
            leading,
            trailing,
        } => emit_unit(name, leading, trailing, indent, is_last_overall, lines),
        UsesItem::Directive(text) | UsesItem::Comment(text) => {
            lines.push(format!("{indent}{text}"));
        }
        UsesItem::IfDefBlock(block) => {
            emit_ifdef_block(block, indent, is_last_overall, lines);
        }
    }
}

/// Emit a unit line, its leading comments on their own lines above it.
fn emit_unit(
    name: &str,
    leading: &[String],
    trailing: &[String],
    indent: &str,
    is_last_overall: bool,
    lines: &mut Vec<String>,
) {
    for comment in leading {
        lines.push(format!("{indent}{comment}"));
    }
    let punctuation = if is_last_overall { ';' } else { ',' };
    push_with_trailing(
        format!("{indent}{name}{punctuation}"),
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

/// Emit an `IfDefBlock`. If `is_last_overall` is true, the `{$ENDIF}` line
/// gets a trailing `;`.
fn emit_ifdef_block(
    block: &IfDefBlock,
    indent: &str,
    is_last_overall: bool,
    lines: &mut Vec<String>,
) {
    // Emit if_branch directive
    push_with_trailing(
        format!("{indent}{}", block.if_branch.directive),
        &block.if_branch.trailing,
        indent,
        lines,
    );

    // Emit if_branch items (never the very last item of the clause, since the
    // last item is determined at the top level). Within the block, all units get commas.
    for item in &block.if_branch.items {
        emit_uses_item(item, indent, false, lines);
    }

    // Emit elseif branches
    for branch in &block.else_if_branches {
        push_with_trailing(
            format!("{indent}{}", branch.directive),
            &branch.trailing,
            indent,
            lines,
        );
        for item in &branch.items {
            emit_uses_item(item, indent, false, lines);
        }
    }

    // Emit else branch
    if let Some(else_items) = &block.else_branch {
        push_with_trailing(
            format!("{indent}{{$ELSE}}"),
            &block.else_trailing,
            indent,
            lines,
        );
        for item in else_items {
            emit_uses_item(item, indent, false, lines);
        }
    }

    // Emit endif
    let semicolon = if is_last_overall { ";" } else { "" };
    push_with_trailing(
        format!("{indent}{}{semicolon}", block.endif),
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
        let item = UsesItem::Directive("{$I compilers.inc}".to_string());
        match item {
            UsesItem::Directive(text) => assert_eq!(text, "{$I compilers.inc}"),
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
        let items = extract_uses_items(uses_node, &bytes, &comments);
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
        let items = extract_uses_items(uses_node, &bytes, &comments);

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
        let items = extract_uses_items(uses_node, &bytes, &comments);

        // ppDirective is an extra — it may appear before SysUtils
        let directive_items: Vec<_> = items
            .iter()
            .filter(|i| matches!(i, UsesItem::Directive(_)))
            .collect();
        assert!(
            !directive_items.is_empty(),
            "expected at least one Directive"
        );
        match &directive_items[0] {
            UsesItem::Directive(text) => assert!(text.contains("compilers.inc")),
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
        let items = extract_uses_items(uses_node, &bytes, &comments);

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
    fn format_items_ifdef_block_follows_anchor() {
        // SysUtils, {$IFDEF FOO} SpecialUnit {$ELSE} OtherUnit {$ENDIF}, Classes
        // After sort (no grouping here): Classes, SysUtils
        // The IfDefBlock anchor is SysUtils (preceded it in original list)
        // So result should be: Classes, SysUtils, {IFDEF block}
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
            endif: "{$ENDIF}".to_string(),
            ..IfDefBlock::default()
        };

        let items = vec![
            UsesItem::unit("SysUtils"),
            UsesItem::IfDefBlock(block),
            UsesItem::unit("Classes"),
        ];

        let output = format_uses_items(&items, &config, "  ", &HashSet::new());
        // Classes sorts before SysUtils; IfDefBlock anchored to SysUtils stays after it.
        // Expected: Classes,\nSysUtils,\n{$IFDEF FOO}\nSpecialUnit,\n{$ELSE}\nOtherUnit,\n{$ENDIF};\n
        assert!(
            output.contains("  Classes,\n"),
            "Classes should appear with comma: {output:?}"
        );
        let classes_pos = output.find("  Classes,\n").unwrap();
        let sysutils_pos = output.find("  SysUtils,\n").unwrap();
        let ifdef_pos = output.find("  {$IFDEF FOO}\n").unwrap();
        assert!(classes_pos < sysutils_pos, "Classes before SysUtils");
        assert!(sysutils_pos < ifdef_pos, "SysUtils before IFDEF block");
        // The last line should end with {$ENDIF};
        assert!(
            output.contains("  {$ENDIF};\n"),
            "endif should have semicolon: {output:?}"
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
            UsesItem::Directive("{$I compilers.inc}".to_string()),
            UsesItem::unit("SysUtils"),
            UsesItem::unit("Classes"),
        ];

        let output = format_uses_items(&items, &config, "  ", &HashSet::new());
        // Directive should be first
        assert!(
            output.starts_with("  {$I compilers.inc}\n"),
            "directive should be first: {output:?}"
        );
        // Classes sorts before SysUtils
        let classes_pos = output.find("  Classes,\n").unwrap();
        let sysutils_pos = output.find("  SysUtils;\n").unwrap();
        assert!(classes_pos < sysutils_pos);
    }

    #[test]
    fn format_items_directive_between_units_follows_anchor() {
        // SysUtils, {$I inc}, Classes
        // After sort: Classes, SysUtils
        // Directive anchor = SysUtils → inserted after SysUtils
        let config = UsesConfig {
            sort: true,
            group: false,
            ..UsesConfig::default()
        };

        let items = vec![
            UsesItem::unit("SysUtils"),
            UsesItem::Directive("{$I myinc.inc}".to_string()),
            UsesItem::unit("Classes"),
        ];

        let output = format_uses_items(&items, &config, "  ", &HashSet::new());
        let classes_pos = output.find("  Classes,\n").unwrap();
        let sysutils_pos = output.find("  SysUtils,\n").unwrap();
        let directive_pos = output.find("  {$I myinc.inc}\n").unwrap();
        assert!(
            classes_pos < sysutils_pos,
            "Classes before SysUtils after sort"
        );
        assert!(
            sysutils_pos < directive_pos,
            "directive follows its anchor SysUtils: {output:?}"
        );
    }

    #[test]
    fn format_items_multiple_directives_at_start_preserve_order() {
        let items = vec![
            UsesItem::Directive("{$I a.inc}".to_string()),
            UsesItem::Directive("{$I b.inc}".to_string()),
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
        let items = extract_uses_items(uses_node, src, &comments);
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
