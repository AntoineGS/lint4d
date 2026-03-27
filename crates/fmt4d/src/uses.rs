use crate::config::UsesConfig;
use std::collections::HashMap;

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

fn unit_group<'a>(unit_name: &str, config: &'a UsesConfig) -> Option<&'a str> {
    if let Some(dot_pos) = unit_name.find('.') {
        let prefix = &unit_name[..dot_pos];
        for group in &config.group_order {
            if prefix.eq_ignore_ascii_case(group) {
                return Some(group);
            }
        }
        return None;
    }
    if let Some(namespace) = legacy_namespace(unit_name) {
        for group in &config.group_order {
            if namespace.eq_ignore_ascii_case(group) {
                return Some(group);
            }
        }
    }
    None
}

pub fn group_units(units: &[String], config: &UsesConfig) -> Vec<Vec<String>> {
    if !config.group {
        let mut sorted = units.to_vec();
        if config.sort {
            sorted.sort_by_key(|a: &String| a.to_lowercase());
        }
        return vec![sorted];
    }

    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    let mut other: Vec<String> = Vec::new();

    for unit in units {
        match unit_group(unit, config) {
            Some(group) => {
                groups
                    .entry(group.to_string())
                    .or_default()
                    .push(unit.clone());
            }
            None => other.push(unit.clone()),
        }
    }

    if config.sort {
        for group in groups.values_mut() {
            group.sort_by_key(|a: &String| a.to_lowercase());
        }
        other.sort_by_key(|a: &String| a.to_lowercase());
    }

    let mut result: Vec<Vec<String>> = Vec::new();
    for group_name in &config.group_order {
        if let Some(units) = groups.remove(group_name) {
            if !units.is_empty() {
                result.push(units);
            }
        }
    }
    if !other.is_empty() {
        result.push(other);
    }
    result
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

    #[test]
    fn group_units_by_namespace() {
        let config = UsesConfig::default();
        let units = vec![
            "MyUnit".to_string(),
            "System.SysUtils".to_string(),
            "Vcl.Forms".to_string(),
            "System.Classes".to_string(),
            "Data.DB".to_string(),
        ];
        let groups = group_units(&units, &config);
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[0], vec!["System.Classes", "System.SysUtils"]);
        assert_eq!(groups[1], vec!["Vcl.Forms"]);
        assert_eq!(groups[2], vec!["Data.DB"]);
        assert_eq!(groups[3], vec!["MyUnit"]);
    }

    #[test]
    fn legacy_units_mapped_to_groups() {
        let config = UsesConfig::default();
        let units = vec![
            "SysUtils".to_string(),
            "Classes".to_string(),
            "Forms".to_string(),
            "DB".to_string(),
        ];
        let groups = group_units(&units, &config);
        assert_eq!(groups[0], vec!["Classes", "SysUtils"]);
        assert_eq!(groups[1], vec!["Forms"]);
        assert_eq!(groups[2], vec!["DB"]);
    }

    #[test]
    fn format_uses_clause() {
        let config = UsesConfig::default();
        let units = vec![
            "Vcl.Forms".to_string(),
            "System.SysUtils".to_string(),
            "System.Classes".to_string(),
        ];
        let output = format_uses(&units, &config, "  ");
        let expected = "  System.Classes,\n  System.SysUtils,\n\n  Vcl.Forms;\n";
        assert_eq!(output, expected);
    }

    #[test]
    fn no_grouping_when_disabled() {
        let mut config = UsesConfig::default();
        config.group = false;
        let units = vec!["B".to_string(), "A".to_string()];
        let groups = group_units(&units, &config);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0], vec!["A", "B"]);
    }
}
