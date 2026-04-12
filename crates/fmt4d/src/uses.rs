use crate::config::UsesConfig;
use pascal_core::node_kind as K;
use std::collections::HashSet;
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
    /// A regular unit name — participates in sorting/grouping.
    Unit(String),
    /// An {$IFDEF}...{$ENDIF} block — pinned in position, contents untouched.
    IfDefBlock(IfDefBlock),
    /// A standalone directive ({$I ...}, {$HINTS OFF}, etc.) — pinned in position.
    Directive(String),
}

/// A complete {$IFDEF}...{$ENDIF} conditional block.
#[derive(Debug, Clone)]
pub struct IfDefBlock {
    /// The opening condition branch.
    pub if_branch: CondBranch,
    /// Zero or more {$ELSEIF ...} branches.
    pub else_if_branches: Vec<CondBranch>,
    /// Optional {$ELSE} fallback branch (units only, directive text is implicit "{$ELSE}").
    pub else_branch: Option<Vec<UsesItem>>,
    /// The closing directive text, e.g. "{$ENDIF}".
    pub endif: String,
}

/// A conditional branch with its directive text and items.
#[derive(Debug, Clone)]
pub struct CondBranch {
    /// The directive text: "{$IFDEF DELPHI_XE6_UP}", "{$ELSEIF expr}", etc.
    pub directive: String,
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
            UsesItem::Unit(name) => out.push(name.clone()),
            UsesItem::IfDefBlock(block) => collect_ifdef_units(block, out),
            UsesItem::Directive(_) => {}
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

/// Walk children of a `ppUsesBlock` node and return an `IfDefBlock`.
fn parse_pp_uses_block(node: tree_sitter::Node, source: &[u8]) -> IfDefBlock {
    let children: Vec<tree_sitter::Node> = node.children(&mut node.walk()).collect();

    let mut if_branch = CondBranch {
        directive: String::new(),
        items: Vec::new(),
    };
    let mut else_if_branches: Vec<CondBranch> = Vec::new();
    let mut else_branch: Option<Vec<UsesItem>> = None;
    let mut endif = String::new();

    // State machine: 0 = in if_branch, 1 = in an elseif branch, 2 = in else_branch
    let mut state = 0usize;
    // Current elseif branch being built (used when state==1)
    let mut current_elseif: Option<CondBranch> = None;

    for child in children {
        match child.kind() {
            k if k == K::PP_IF => {
                if_branch.directive = node_text(child, source);
            }
            k if k == K::PP_ELSE => {
                let text = node_text(child, source);
                if text.to_lowercase().contains("elseif") {
                    // Flush current branch
                    if state == 0 {
                        // we were in if_branch, nothing to flush to else_if_branches yet
                    } else if state == 1 {
                        if let Some(branch) = current_elseif.take() {
                            else_if_branches.push(branch);
                        }
                    }
                    current_elseif = Some(CondBranch {
                        directive: text,
                        items: Vec::new(),
                    });
                    state = 1;
                } else {
                    // It's a plain {$ELSE}
                    if state == 1 {
                        if let Some(branch) = current_elseif.take() {
                            else_if_branches.push(branch);
                        }
                    }
                    else_branch = Some(Vec::new());
                    state = 2;
                }
            }
            k if k == K::PP_END_IF => {
                // Flush any pending elseif
                if state == 1 {
                    if let Some(branch) = current_elseif.take() {
                        else_if_branches.push(branch);
                    }
                }
                endif = node_text(child, source);
            }
            k if k == K::MODULE_NAME => {
                let text = node_text(child, source);
                if !text.is_empty() {
                    let item = UsesItem::Unit(text);
                    match state {
                        0 => if_branch.items.push(item),
                        1 => {
                            if let Some(ref mut branch) = current_elseif {
                                branch.items.push(item);
                            }
                        }
                        2 => {
                            if let Some(ref mut v) = else_branch {
                                v.push(item);
                            }
                        }
                        _ => {}
                    }
                }
            }
            k if k == K::PP_USES_BLOCK => {
                let nested = UsesItem::IfDefBlock(parse_pp_uses_block(child, source));
                match state {
                    0 => if_branch.items.push(nested),
                    1 => {
                        if let Some(ref mut branch) = current_elseif {
                            branch.items.push(nested);
                        }
                    }
                    2 => {
                        if let Some(ref mut v) = else_branch {
                            v.push(nested);
                        }
                    }
                    _ => {}
                }
            }
            k if k == K::PP_DIRECTIVE => {
                let text = node_text(child, source);
                let item = UsesItem::Directive(text);
                match state {
                    0 => if_branch.items.push(item),
                    1 => {
                        if let Some(ref mut branch) = current_elseif {
                            branch.items.push(item);
                        }
                    }
                    2 => {
                        if let Some(ref mut v) = else_branch {
                            v.push(item);
                        }
                    }
                    _ => {}
                }
            }
            _ => {} // skip kUses, commas, etc.
        }
    }

    IfDefBlock {
        if_branch,
        else_if_branches,
        else_branch,
        endif,
    }
}

/// Extract all items from a `declUses` node into a `Vec<UsesItem>`.
pub fn extract_uses_items(node: tree_sitter::Node, source: &[u8]) -> Vec<UsesItem> {
    let mut items = Vec::new();
    let children: Vec<tree_sitter::Node> = node.children(&mut node.walk()).collect();
    for child in children {
        match child.kind() {
            k if k == K::MODULE_NAME => {
                let text = node_text(child, source);
                if !text.is_empty() {
                    items.push(UsesItem::Unit(text));
                }
            }
            k if k == K::PP_USES_BLOCK => {
                items.push(UsesItem::IfDefBlock(parse_pp_uses_block(child, source)));
            }
            k if k == K::PP_DIRECTIVE => {
                let text = node_text(child, source);
                if !text.is_empty() {
                    items.push(UsesItem::Directive(text));
                }
            }
            _ => {} // skip kUses keyword, commas, semicolons, etc.
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
    // Separate plain units from pinned items, recording the anchor (preceding unit name).
    // When grouping is enabled, ifdef blocks whose units all belong to one section
    // are placed in that section rather than pinned.
    let mut plain_units: Vec<String> = Vec::new();
    // pinned: (anchor: Option<String>, item)
    // anchor is None when the pinned item appears before any unit.
    let mut pinned: Vec<(Option<String>, UsesItem)> = Vec::new();
    // section_blocks: ifdef blocks placed into a specific section.
    let mut section_blocks: Vec<(UnitSection, UsesItem)> = Vec::new();
    let mut last_unit: Option<String> = None;

    for item in items {
        match item {
            UsesItem::Unit(name) => {
                plain_units.push(name.clone());
                last_unit = Some(name.clone());
            }
            UsesItem::IfDefBlock(block) if config.group => {
                if let Some(section) = classify_ifdef_block(block, config, external_units) {
                    section_blocks.push((section, item.clone()));
                } else {
                    pinned.push((last_unit.clone(), item.clone()));
                }
            }
            UsesItem::IfDefBlock(_) | UsesItem::Directive(_) => {
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

    // Count total real items (units + pinned items) to determine the last one for semicolon.
    // We need to find the last non-GroupSep slot.
    let last_real_idx = slots
        .iter()
        .rposition(|s| !matches!(s, Slot::GroupSep))
        .unwrap_or(0);

    // Emit the output.
    let mut output = String::new();
    for (slot_idx, slot) in slots.iter().enumerate() {
        let is_last = slot_idx == last_real_idx;
        match slot {
            Slot::GroupSep => {
                output.push('\n');
            }
            Slot::Unit { name } => {
                output.push_str(indent);
                output.push_str(name);
                if is_last {
                    output.push_str(";\n");
                } else {
                    output.push_str(",\n");
                }
            }
            Slot::Pinned(item) => {
                emit_uses_item(item, indent, is_last, &mut output);
            }
        }
    }

    output
}

/// Recursively emit a single `UsesItem` into `output`.
fn emit_uses_item(item: &UsesItem, indent: &str, is_last_overall: bool, output: &mut String) {
    match item {
        UsesItem::Unit(name) => {
            output.push_str(indent);
            output.push_str(name);
            if is_last_overall {
                output.push_str(";\n");
            } else {
                output.push_str(",\n");
            }
        }
        UsesItem::Directive(text) => {
            output.push_str(indent);
            output.push_str(text);
            output.push('\n');
        }
        UsesItem::IfDefBlock(block) => {
            emit_ifdef_block(block, indent, is_last_overall, output);
        }
    }
}

/// Emit an `IfDefBlock`. If `semicolon_after_endif` is true, the `{$ENDIF}` line
/// gets a trailing `;`.
fn emit_ifdef_block(block: &IfDefBlock, indent: &str, is_last_overall: bool, output: &mut String) {
    // Emit if_branch directive
    output.push_str(indent);
    output.push_str(&block.if_branch.directive);
    output.push('\n');

    // Emit if_branch items (never the very last item of the clause, since the
    // last item is determined at the top level). Within the block, all units get commas.
    for item in &block.if_branch.items {
        emit_uses_item(item, indent, false, output);
    }

    // Emit elseif branches
    for branch in &block.else_if_branches {
        output.push_str(indent);
        output.push_str(&branch.directive);
        output.push('\n');
        for item in &branch.items {
            emit_uses_item(item, indent, false, output);
        }
    }

    // Emit else branch
    if let Some(else_items) = &block.else_branch {
        output.push_str(indent);
        output.push_str("{$ELSE}");
        output.push('\n');
        for item in else_items {
            emit_uses_item(item, indent, false, output);
        }
    }

    // Emit endif
    output.push_str(indent);
    output.push_str(&block.endif);
    if is_last_overall {
        output.push(';');
    }
    output.push('\n');
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
            if let Some(ext) = path.extension() {
                if ext.eq_ignore_ascii_case("pas") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        units.insert(stem.to_lowercase());
                    }
                }
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
        let mut cfg = UsesConfig::default();
        cfg.group = true;
        cfg
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
        .map(|s| UsesItem::Unit(s.to_string()))
        .collect();
        let output = format_uses_items(&items, &config, "  ", &HashSet::new());
        let expected =
            "  System.SysUtils,\n  Vcl.Forms,\n\n  Spring.Container,\n\n  MyApp.Utils;\n";
        assert_eq!(output, expected);
    }

    // ─── Task 5: Data model constructability ─────────────────────────────────

    #[test]
    fn uses_item_unit_constructable() {
        let item = UsesItem::Unit("SysUtils".to_string());
        match item {
            UsesItem::Unit(name) => assert_eq!(name, "SysUtils"),
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
                items: vec![UsesItem::Unit("SpecialUnit".to_string())],
            },
            else_if_branches: Vec::new(),
            else_branch: Some(vec![UsesItem::Unit("OtherUnit".to_string())]),
            endif: "{$ENDIF}".to_string(),
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
        let items = extract_uses_items(uses_node, &bytes);
        assert_eq!(items.len(), 2);
        match &items[0] {
            UsesItem::Unit(name) => assert_eq!(name, "SysUtils"),
            _ => panic!("expected Unit"),
        }
        match &items[1] {
            UsesItem::Unit(name) => assert_eq!(name, "Classes"),
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
        let items = extract_uses_items(uses_node, &bytes);

        // Expect: Unit(SysUtils), IfDefBlock(...), Unit(Classes)
        assert_eq!(items.len(), 3);
        match &items[0] {
            UsesItem::Unit(name) => assert_eq!(name, "SysUtils"),
            _ => panic!("expected Unit at 0"),
        }
        match &items[1] {
            UsesItem::IfDefBlock(block) => {
                assert!(block.if_branch.directive.contains("IFDEF"));
                assert_eq!(block.if_branch.items.len(), 1);
                match &block.if_branch.items[0] {
                    UsesItem::Unit(name) => assert_eq!(name, "SpecialUnit"),
                    _ => panic!("expected Unit in if_branch"),
                }
                assert!(block.else_branch.is_some());
                let else_items = block.else_branch.as_ref().unwrap();
                assert_eq!(else_items.len(), 1);
                match &else_items[0] {
                    UsesItem::Unit(name) => assert_eq!(name, "OtherUnit"),
                    _ => panic!("expected Unit in else_branch"),
                }
                assert!(block.endif.contains("ENDIF"));
            }
            _ => panic!("expected IfDefBlock at 1"),
        }
        match &items[2] {
            UsesItem::Unit(name) => assert_eq!(name, "Classes"),
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
        let items = extract_uses_items(uses_node, &bytes);

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
        let items = extract_uses_items(uses_node, &bytes);

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
        let mut config = UsesConfig::default();
        config.sort = true;
        config.group = false;

        let block = IfDefBlock {
            if_branch: CondBranch {
                directive: "{$IFDEF FOO}".to_string(),
                items: vec![UsesItem::Unit("SpecialUnit".to_string())],
            },
            else_if_branches: Vec::new(),
            else_branch: Some(vec![UsesItem::Unit("OtherUnit".to_string())]),
            endif: "{$ENDIF}".to_string(),
        };

        let items = vec![
            UsesItem::Unit("SysUtils".to_string()),
            UsesItem::IfDefBlock(block),
            UsesItem::Unit("Classes".to_string()),
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
        let mut config = UsesConfig::default();
        config.sort = true;
        config.group = false;

        let items = vec![
            UsesItem::Directive("{$I compilers.inc}".to_string()),
            UsesItem::Unit("SysUtils".to_string()),
            UsesItem::Unit("Classes".to_string()),
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
        let mut config = UsesConfig::default();
        config.sort = true;
        config.group = false;

        let items = vec![
            UsesItem::Unit("SysUtils".to_string()),
            UsesItem::Directive("{$I myinc.inc}".to_string()),
            UsesItem::Unit("Classes".to_string()),
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
            UsesItem::Unit("SysUtils".to_string()),
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
                    UsesItem::Unit("ibx.IBDatabase".to_string()),
                    UsesItem::Unit("ibx.IBSQL".to_string()),
                ],
            },
            else_if_branches: Vec::new(),
            else_branch: Some(vec![
                UsesItem::Unit("IBDatabase".to_string()),
                UsesItem::Unit("IBSQL".to_string()),
            ]),
            endif: "{$ENDIF}".to_string(),
        };

        let items = vec![
            UsesItem::Unit("MDIBDatabase".to_string()),
            UsesItem::IfDefBlock(block),
            UsesItem::Unit("Utils".to_string()),
            UsesItem::Unit("ibxUtils".to_string()),
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
                items: vec![UsesItem::Unit("System.SysUtils".to_string())],
            },
            else_if_branches: Vec::new(),
            else_branch: Some(vec![UsesItem::Unit("MyProject.Utils".to_string())]),
            endif: "{$ENDIF}".to_string(),
        };

        let items = vec![
            UsesItem::Unit("MyApp.Main".to_string()),
            UsesItem::IfDefBlock(block),
            UsesItem::Unit("Vcl.Forms".to_string()),
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
                items: vec![UsesItem::Unit("ibx.IBDatabase".to_string())],
            },
            else_if_branches: Vec::new(),
            else_branch: Some(vec![UsesItem::Unit("IBDatabase".to_string())]),
            endif: "{$ENDIF}".to_string(),
        };

        let items = vec![
            UsesItem::Unit("MyApp.Main".to_string()),
            UsesItem::IfDefBlock(block),
        ];

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
        let mut config = UsesConfig::default();
        config.sort = true;
        config.group = false;

        let block = IfDefBlock {
            if_branch: CondBranch {
                directive: "{$IFDEF FOO}".to_string(),
                items: vec![UsesItem::Unit("System.SysUtils".to_string())],
            },
            else_if_branches: Vec::new(),
            else_branch: Some(vec![UsesItem::Unit("Classes".to_string())]),
            endif: "{$ENDIF}".to_string(),
        };

        let items = vec![
            UsesItem::Unit("Zebra".to_string()),
            UsesItem::IfDefBlock(block),
            UsesItem::Unit("Alpha".to_string()),
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
        let items = extract_uses_items(uses_node, src);
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
