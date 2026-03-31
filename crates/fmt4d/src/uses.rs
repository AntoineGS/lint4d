use crate::config::UsesConfig;
use std::collections::HashSet;

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

pub fn group_units(units: &[String], config: &UsesConfig) -> Vec<Vec<String>> {
    let mut sorted = units.to_vec();
    if config.sort {
        sorted.sort_by_key(|a: &String| a.to_lowercase());
    }
    vec![sorted]
}

pub fn format_uses(units: &[String], config: &UsesConfig, indent: &str) -> String {
    let groups = group_units(units, config);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UsesConfig;

    fn default_config() -> UsesConfig {
        UsesConfig::default()
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
}
