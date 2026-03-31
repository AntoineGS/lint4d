use crate::config::UsesConfig;

// Used by classify_unit (Task 3) for non-dotted legacy unit classification.
#[allow(dead_code)]
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
