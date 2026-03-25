use tree_sitter::{Node, Tree};

use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

// ---------------------------------------------------------------------------
// TypePrefixRule
// ---------------------------------------------------------------------------

pub struct TypePrefixRule {
    meta: RuleMeta,
}

impl Default for TypePrefixRule {
    fn default() -> Self {
        Self::new()
    }
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

    fn check(
        &self,
        _file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        _config: &crate::config::Config,
        ctx: &mut LintContext<'_>,
    ) {
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

impl Default for InterfacePrefixRule {
    fn default() -> Self {
        Self::new()
    }
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

    fn check(
        &self,
        _file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        _config: &crate::config::Config,
        ctx: &mut LintContext<'_>,
    ) {
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

impl Default for ConstantNamingRule {
    fn default() -> Self {
        Self::new()
    }
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

    fn check(
        &self,
        _file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        config: &crate::config::Config,
        ctx: &mut LintContext<'_>,
    ) {
        visit_constant_naming(tree.root_node(), source, config.constant_style(), ctx);
    }
}

fn visit_constant_naming(node: Node, source: &[u8], style: &str, ctx: &mut LintContext) {
    if node.kind() == "declConst" {
        // Skip typed constants — they have a "type" field (e.g., `TypedConst: Integer = 42`)
        if node.child_by_field_name("type").is_none() {
            check_constant_name(node, source, style, ctx);
        }
    }

    for child in node.children(&mut node.walk()) {
        visit_constant_naming(child, source, style, ctx);
    }
}

fn check_constant_name(decl_const: Node, source: &[u8], style: &str, ctx: &mut LintContext) {
    let name_node = match decl_const.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };

    let name = match std::str::from_utf8(&source[name_node.start_byte()..name_node.end_byte()]) {
        Ok(s) => s,
        Err(_) => return,
    };

    let (conforms, expected_label, suggestion) = if style == "PascalCase" {
        let ok = name.chars().next().is_some_and(|c| c.is_uppercase());
        (ok, "PascalCase", name.to_string())
    } else {
        // Default: UPPER_CASE
        let ok = name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        (ok, "UPPER_CASE", to_upper_snake_case(name))
    };

    if conforms {
        return;
    }

    let start = name_node.start_position();
    let end = name_node.end_position();
    ctx.report(Diagnostic {
        rule_id: "constant-naming".to_string(),
        severity: Severity::Hint,
        message: format!(
            "Constant '{}' should use {} naming convention.",
            name, expected_label
        ),
        line: start.row + 1,
        column: start.column + 1,
        end_line: end.row + 1,
        end_column: end.column + 1,
        help: Some(format!("Rename '{}' to '{}'.", name, suggestion)),
    });
}

pub fn to_upper_snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut result = String::with_capacity(name.len() + 4);
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            let prev = chars[i - 1];
            if prev.is_ascii_lowercase() || prev.is_ascii_digit() {
                // camelCase boundary: `fooBar` → `FOO_BAR`
                result.push('_');
            } else if prev.is_ascii_uppercase() {
                // Consecutive uppercase: check if next char is lowercase
                // to detect end of acronym: `HTTPPort` → `HTTP_PORT`
                if let Some(&next) = chars.get(i + 1) {
                    if next.is_ascii_lowercase() {
                        result.push('_');
                    }
                }
            }
        }
        result.push(ch.to_ascii_uppercase());
    }
    result
}

/// Convert a name to camelCase by lowercasing the first alphabetic character.
/// Preserves leading underscores. `MyVar` → `myVar`, `_Count` → `_count`.
pub fn to_camel_case(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    let mut first_alpha_done = false;
    for c in name.chars() {
        if !first_alpha_done && c.is_alphabetic() {
            result.push(c.to_ascii_lowercase());
            first_alpha_done = true;
        } else {
            result.push(c);
        }
    }
    result
}

/// Convert a name to PascalCase.
///
/// - If the name contains underscores: split on underscores, capitalize the first
///   letter of each segment, lowercase the rest, join. `my_const` → `MyConst`.
/// - If no underscores: uppercase the first alphabetic character. `httpPort` → `HttpPort`.
/// - Leading underscores are preserved. `_myVar` → `_MyVar`.
pub fn to_pascal_case(name: &str) -> String {
    let leading_underscores: String = name.chars().take_while(|c| *c == '_').collect();
    let rest = &name[leading_underscores.len()..];

    if rest.is_empty() {
        return name.to_string();
    }

    let transformed = if rest.contains('_') {
        // Underscore-separated: capitalize each segment
        rest.split('_')
            .filter(|s| !s.is_empty())
            .map(|s| {
                let mut chars = s.chars();
                match chars.next() {
                    Some(c) => {
                        let rest_lower: String = chars.map(|ch| ch.to_ascii_lowercase()).collect();
                        format!("{}{}", c.to_ascii_uppercase(), rest_lower)
                    }
                    None => String::new(),
                }
            })
            .collect::<String>()
    } else {
        // No underscores: uppercase first alpha
        let mut result = String::with_capacity(rest.len());
        let mut first_alpha_done = false;
        for c in rest.chars() {
            if !first_alpha_done && c.is_alphabetic() {
                result.push(c.to_ascii_uppercase());
                first_alpha_done = true;
            } else {
                result.push(c);
            }
        }
        result
    };

    format!("{}{}", leading_underscores, transformed)
}

// ---------------------------------------------------------------------------
// LocalVariableNamingRule
// ---------------------------------------------------------------------------

pub struct LocalVariableNamingRule {
    meta: RuleMeta,
}

impl Default for LocalVariableNamingRule {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalVariableNamingRule {
    pub fn new() -> Self {
        LocalVariableNamingRule {
            meta: RuleMeta {
                id: "local-variable-naming",
                name: "Local Variable Naming Convention",
                category: RuleCategory::NamingConvention,
                default_severity: Severity::Hint,
                description: "Enforces camelCase or PascalCase naming for local variables.",
            },
        }
    }
}

impl Rule for LocalVariableNamingRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn check(
        &self,
        _file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        config: &crate::config::Config,
        ctx: &mut LintContext<'_>,
    ) {
        let style = config.local_variable_style();
        visit_local_variable_naming(tree.root_node(), source, style, ctx);
    }
}

/// Walk the AST looking for `defProc` and `lambda` nodes, then check
/// their local variable declarations.
fn visit_local_variable_naming(node: Node, source: &[u8], style: &str, ctx: &mut LintContext) {
    if node.kind() == "defProc" || node.kind() == "lambda" {
        check_proc_local_vars(node, source, style, ctx);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_local_variable_naming(child, source, style, ctx);
    }
}

/// Find all `declVars` children under a `defProc` or `lambda` and check
/// each variable name against the configured style.
fn check_proc_local_vars(proc_node: Node, source: &[u8], style: &str, ctx: &mut LintContext) {
    let mut cursor = proc_node.walk();
    for child in proc_node.children(&mut cursor) {
        if child.kind() == "declVars" {
            check_decl_vars(child, source, style, ctx);
        }
    }
}

/// Iterate all `declVar` nodes inside a `declVars` block and check each
/// identifier against the naming style.
fn check_decl_vars(decl_vars: Node, source: &[u8], style: &str, ctx: &mut LintContext) {
    let mut cursor = decl_vars.walk();
    for child in decl_vars.children(&mut cursor) {
        if child.kind() == "declVar" {
            check_decl_var_names(child, source, style, ctx);
        }
    }
}

/// Check all identifier names in a single `declVar` node (handles
/// multi-name declarations like `a, b: Integer`).
fn check_decl_var_names(decl_var: Node, source: &[u8], style: &str, ctx: &mut LintContext) {
    let child_count = decl_var.child_count();
    for i in 0..child_count {
        let child = match decl_var.child(i) {
            Some(c) => c,
            None => continue,
        };
        // Only process children that are identifiers in the "name" field.
        let field = decl_var.field_name_for_child(i as u32);
        if child.kind() != "identifier" || field != Some("name") {
            continue;
        }

        let name = match std::str::from_utf8(&source[child.start_byte()..child.end_byte()]) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if !violates_naming_style(name, style) {
            continue;
        }

        let start = child.start_position();
        let end = child.end_position();
        let expected = if style == "camelCase" {
            "camelCase"
        } else {
            "PascalCase"
        };
        ctx.report(crate::engine::Diagnostic {
            rule_id: "local-variable-naming".to_string(),
            severity: Severity::Hint,
            message: format!(
                "Local variable '{}' should use {} naming convention.",
                name, expected
            ),
            line: start.row + 1,
            column: start.column + 1,
            end_line: end.row + 1,
            end_column: end.column + 1,
            help: Some(format!(
                "Rename '{}' to follow {} convention.",
                name, expected
            )),
        });
    }
}

/// Returns `true` when the name violates the given style.
///
/// Single-character names are always exempt. Leading underscores are
/// skipped when determining the effective first character.
pub fn violates_naming_style(name: &str, style: &str) -> bool {
    // Skip leading underscores to find the first alpha character.
    let first_alpha = name.chars().find(|c| c.is_alphabetic());
    let first = match first_alpha {
        Some(c) => c,
        // No alpha characters — exempt (numeric or underscore only).
        None => return false,
    };

    // Count the alphabetic characters; single-char names are exempt.
    let alpha_count = name.chars().filter(|c| c.is_alphabetic()).count();
    if alpha_count <= 1 {
        return false;
    }

    if style == "camelCase" {
        // Violation: first alpha is uppercase.
        first.is_uppercase()
    } else {
        // PascalCase violation: first alpha is lowercase.
        first.is_lowercase()
    }
}
