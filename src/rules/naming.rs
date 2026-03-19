use tree_sitter::{Node, Tree};

use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

// ---------------------------------------------------------------------------
// TypePrefixRule
// ---------------------------------------------------------------------------

pub struct TypePrefixRule {
    meta: RuleMeta,
}

impl TypePrefixRule {
    pub fn new() -> Self {
        TypePrefixRule {
            meta: RuleMeta {
                id: "type-prefix",
                name: "Type Prefix Convention",
                category: RuleCategory::NamingConvention,
                default_severity: Severity::Hint,
                description: "Enforces 'T' prefix on type declarations.",
            },
        }
    }
}

impl Rule for TypePrefixRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn check(&self, _file: &FileInfo, tree: &Tree, source: &[u8], ctx: &mut LintContext) {
        visit_type_prefix(tree.root_node(), source, ctx);
    }
}

fn visit_type_prefix(node: Node, source: &[u8], ctx: &mut LintContext) {
    // Look for declType nodes whose type child is a declClass (class or record).
    if node.kind() == "declType" {
        if let Some(type_node) = node.child_by_field_name("type") {
            if type_node.kind() == "declClass" {
                check_type_name(node, source, ctx);
            }
        }
    }

    for child in node.children(&mut node.walk()) {
        visit_type_prefix(child, source, ctx);
    }
}

fn check_type_name(decl_type: Node, source: &[u8], ctx: &mut LintContext) {
    let name_node = match decl_type.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };

    let name = match std::str::from_utf8(&source[name_node.start_byte()..name_node.end_byte()]) {
        Ok(s) => s,
        Err(_) => return,
    };

    // Allow 'T' prefix (standard types) and 'E' prefix (exception classes).
    if name.starts_with('T') || name.starts_with('E') {
        return;
    }

    let start = name_node.start_position();
    let end = name_node.end_position();
    ctx.report(Diagnostic {
        rule_id: "type-prefix".to_string(),
        severity: Severity::Hint,
        message: format!(
            "Type '{}' should start with 'T' prefix (or 'E' for exception classes).",
            name
        ),
        line: start.row + 1,
        column: start.column + 1,
        end_line: end.row + 1,
        end_column: end.column + 1,
        help: Some(format!("Rename to 'T{}'.", name)),
    });
}

// ---------------------------------------------------------------------------
// InterfacePrefixRule
// ---------------------------------------------------------------------------

pub struct InterfacePrefixRule {
    meta: RuleMeta,
}

impl InterfacePrefixRule {
    pub fn new() -> Self {
        InterfacePrefixRule {
            meta: RuleMeta {
                id: "interface-prefix",
                name: "Interface Prefix Convention",
                category: RuleCategory::NamingConvention,
                default_severity: Severity::Hint,
                description: "Enforces 'I' prefix on interface declarations.",
            },
        }
    }
}

impl Rule for InterfacePrefixRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn check(&self, _file: &FileInfo, tree: &Tree, source: &[u8], ctx: &mut LintContext) {
        visit_interface_prefix(tree.root_node(), source, ctx);
    }
}

fn visit_interface_prefix(node: Node, source: &[u8], ctx: &mut LintContext) {
    // Look for declType nodes whose type child is a declIntf.
    if node.kind() == "declType" {
        if let Some(type_node) = node.child_by_field_name("type") {
            if type_node.kind() == "declIntf" {
                check_interface_name(node, source, ctx);
            }
        }
    }

    for child in node.children(&mut node.walk()) {
        visit_interface_prefix(child, source, ctx);
    }
}

fn check_interface_name(decl_type: Node, source: &[u8], ctx: &mut LintContext) {
    let name_node = match decl_type.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };

    let name = match std::str::from_utf8(&source[name_node.start_byte()..name_node.end_byte()]) {
        Ok(s) => s,
        Err(_) => return,
    };

    if name.starts_with('I') {
        return;
    }

    let start = name_node.start_position();
    let end = name_node.end_position();
    ctx.report(Diagnostic {
        rule_id: "interface-prefix".to_string(),
        severity: Severity::Hint,
        message: format!("Interface '{}' should start with 'I' prefix.", name),
        line: start.row + 1,
        column: start.column + 1,
        end_line: end.row + 1,
        end_column: end.column + 1,
        help: Some(format!("Rename to 'I{}'.", name)),
    });
}

// ---------------------------------------------------------------------------
// ConstantNamingRule
// ---------------------------------------------------------------------------

pub struct ConstantNamingRule {
    meta: RuleMeta,
}

impl ConstantNamingRule {
    pub fn new() -> Self {
        ConstantNamingRule {
            meta: RuleMeta {
                id: "constant-naming",
                name: "Constant Naming Convention",
                category: RuleCategory::NamingConvention,
                default_severity: Severity::Hint,
                description: "Enforces UPPER_CASE naming for untyped constants.",
            },
        }
    }
}

impl Rule for ConstantNamingRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn check(&self, _file: &FileInfo, tree: &Tree, source: &[u8], ctx: &mut LintContext) {
        visit_constant_naming(tree.root_node(), source, ctx);
    }
}

fn visit_constant_naming(node: Node, source: &[u8], ctx: &mut LintContext) {
    if node.kind() == "declConst" {
        // Skip typed constants — they have a "type" field (e.g., `TypedConst: Integer = 42`)
        if node.child_by_field_name("type").is_none() {
            check_constant_name(node, source, ctx);
        }
    }

    for child in node.children(&mut node.walk()) {
        visit_constant_naming(child, source, ctx);
    }
}

fn check_constant_name(decl_const: Node, source: &[u8], ctx: &mut LintContext) {
    let name_node = match decl_const.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };

    let name = match std::str::from_utf8(&source[name_node.start_byte()..name_node.end_byte()]) {
        Ok(s) => s,
        Err(_) => return,
    };

    // UPPER_CASE: only uppercase letters, digits, and underscores
    if name
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return;
    }

    let start = name_node.start_position();
    let end = name_node.end_position();
    ctx.report(Diagnostic {
        rule_id: "constant-naming".to_string(),
        severity: Severity::Hint,
        message: format!(
            "Constant '{}' should use UPPER_CASE naming convention.",
            name
        ),
        line: start.row + 1,
        column: start.column + 1,
        end_line: end.row + 1,
        end_column: end.column + 1,
        help: Some(format!(
            "Rename '{}' to '{}'.",
            name,
            to_upper_snake_case(name)
        )),
    });
}

fn to_upper_snake_case(name: &str) -> String {
    let mut result = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            let prev = name.chars().nth(i - 1).unwrap_or('_');
            if prev.is_ascii_lowercase() || prev.is_ascii_digit() {
                result.push('_');
            }
        }
        result.push(ch.to_ascii_uppercase());
    }
    result
}
