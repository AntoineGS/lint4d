use crate::config::UsesConfig;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitSection {
    Core,
    External,
    Project,
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
    match name {
        "SysUtils" | "Classes" | "Types" | "Variants" | "SysConst" | "Math" | "StrUtils"
        | "DateUtils" | "IOUtils" | "RegularExpressions" | "SyncObjs" | "Rtti" | "TypInfo"
        | "Contnrs" => Some("System"),

        "Forms" | "Controls" | "StdCtrls" | "ExtCtrls" | "ComCtrls" | "Dialogs" | "Graphics"
        | "Menus" | "ActnList" | "Grids" | "Buttons" | "ImgList" | "ToolWin" | "AppEvnts" => {
            Some("Vcl")
        }

        "DB" | "DBClient" | "Provider" | "DBGrids" | "DBCtrls" | "SqlExpr" => Some("Data"),

        "Windows" | "Messages" | "ShellAPI" | "ActiveX" | "CommCtrl" | "ShlObj" => Some("Winapi"),

        _ => None,
    }
}

pub fn group_units(
    units: &[String],
    config: &UsesConfig,
    external_units: &HashSet<String>,
) -> Vec<Vec<String>> {
    if !config.group {
        let mut sorted = units.to_vec();
        if config.sort {
            sorted.sort_by_key(|a: &String| a.to_lowercase());
        }
        return vec![sorted];
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

    let mut result: Vec<Vec<String>> = Vec::new();
    if !core.is_empty() {
        result.push(core);
    }
    if !external.is_empty() {
        result.push(external);
    }
    if !project.is_empty() {
        result.push(project);
    }
    result
}

pub fn format_uses(
    units: &[String],
    config: &UsesConfig,
    indent: &str,
    external_units: &HashSet<String>,
) -> String {
    let groups = group_units(units, config, external_units);
    let mut output = String::new();

    for (group_idx, group) in groups.iter().enumerate() {
        if group_idx > 0 {
            output.push('\n');
        }
        for (unit_idx, unit) in group.iter().enumerate() {
            output.push_str(indent);
            output.push_str(unit);
            if group_idx == groups.len() - 1 && unit_idx == group.len() - 1 {
                output.push_str(";\n");
            } else {
                output.push_str(",\n");
            }
        }
    }
    output
}

pub fn extract_uses_units(node: tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut units = Vec::new();
    for child in node.children(&mut node.walk()) {
        if child.kind() == "moduleName" {
            let text = std::str::from_utf8(&source[child.start_byte()..child.end_byte()])
                .unwrap_or("")
                .to_string();
            if !text.is_empty() {
                units.push(text);
            }
        }
    }
    units
}

/// Recursively scan directories for `.pas` files and collect unit names (lowercased).
pub fn scan_external_paths(project_root: &Path, external_paths: &[String]) -> HashSet<String> {
    let mut units = HashSet::new();
    for rel_path in external_paths {
        let dir = project_root.join(rel_path);
        if dir.is_dir() {
            scan_dir_recursive(&dir, &mut units);
        }
    }
    units
}

fn scan_dir_recursive(dir: &Path, units: &mut HashSet<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_recursive(&path, units);
        } else if let Some(ext) = path.extension() {
            if ext.eq_ignore_ascii_case("pas") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    units.insert(stem.to_lowercase());
                }
            }
        }
    }
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
        let units = vec![
            "Vcl.Forms".to_string(),
            "System.SysUtils".to_string(),
            "Spring.Container".to_string(),
            "MyApp.Utils".to_string(),
        ];
        let output = format_uses(&units, &config, "  ", &HashSet::new());
        let expected =
            "  System.SysUtils,\n  Vcl.Forms,\n\n  Spring.Container,\n\n  MyApp.Utils;\n";
        assert_eq!(output, expected);
    }
}
