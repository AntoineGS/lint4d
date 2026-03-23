use tree_sitter::{Node, Tree};

use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::helpers::{
    constructor_has_owner_args, is_constructor_call, node_text, text_frees_variable,
};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

// ─── Shared data structures ───────────────────────────────────────────────────

/// Information about a class collected from the interface section.
#[derive(Debug)]
struct ClassInfo {
    /// The class name (e.g. `TLeaky`).
    name: String,
    /// Field names declared in the class (e.g. `FChild`, `FLogger`).
    fields: Vec<String>,
}

/// A field creation assignment found inside a method body.
#[derive(Debug)]
struct FieldCreation {
    /// The field being assigned (e.g. `FChild`).
    field_name: String,
    /// 1-based line of the assignment node.
    line: usize,
    /// 1-based column of the assignment node.
    column: usize,
    /// 1-based end line.
    end_line: usize,
    /// 1-based end column.
    end_column: usize,
}

// ─── AST helpers ─────────────────────────────────────────────────────────────

/// Collect all class declarations from the interface section.
fn collect_classes(root: Node, source: &[u8]) -> Vec<ClassInfo> {
    let mut classes = Vec::new();
    collect_classes_recursive(root, source, &mut classes);
    classes
}

fn collect_classes_recursive(node: Node, source: &[u8], out: &mut Vec<ClassInfo>) {
    if node.kind() == "declType" {
        if let Some(info) = parse_decl_type(node, source) {
            out.push(info);
            return; // Don't recurse into a type decl
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_classes_recursive(child, source, out);
    }
}

/// Parse a `declType` node into a `ClassInfo` if it is a class declaration.
fn parse_decl_type(node: Node, source: &[u8]) -> Option<ClassInfo> {
    // First child should be the class name identifier.
    let name_node = node.child(0)?;
    if name_node.kind() != "identifier" {
        return None;
    }
    let name = node_text(name_node, source);

    // Find the `declClass` child.
    let mut cursor = node.walk();
    let decl_class = node
        .children(&mut cursor)
        .find(|c| c.kind() == "declClass")?;

    let fields = collect_fields(decl_class, source);
    Some(ClassInfo { name, fields })
}

/// Collect all field names from a `declClass` node.
fn collect_fields(decl_class: Node, source: &[u8]) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cursor = decl_class.walk();
    for section in decl_class.children(&mut cursor) {
        if section.kind() == "declSection" {
            let mut section_cursor = section.walk();
            for item in section.children(&mut section_cursor) {
                if item.kind() == "declField" {
                    // First child of declField is the identifier
                    if let Some(id_node) = item.child(0) {
                        if id_node.kind() == "identifier" {
                            fields.push(node_text(id_node, source));
                        }
                    }
                }
            }
        }
    }
    fields
}

/// Extract class name and flags from a `defProc` node.
///
/// Returns `(class_name, is_constructor, is_destructor)`.
fn parse_def_proc(def_proc: Node, source: &[u8]) -> Option<(String, bool, bool)> {
    let mut cursor = def_proc.walk();
    let decl_proc = def_proc
        .children(&mut cursor)
        .find(|c| c.kind() == "declProc")?;

    let is_constructor = decl_proc
        .children(&mut decl_proc.walk())
        .any(|c| c.kind() == "kConstructor");
    let is_destructor = decl_proc
        .children(&mut decl_proc.walk())
        .any(|c| c.kind() == "kDestructor");

    // Find the genericDot which contains class_name.method_name
    let generic_dot = decl_proc
        .children(&mut decl_proc.walk())
        .find(|c| c.kind() == "genericDot")?;

    let idents: Vec<Node> = generic_dot
        .children(&mut generic_dot.walk())
        .filter(|c| c.kind() == "identifier")
        .collect();

    if idents.is_empty() {
        return None;
    }

    let class_name = node_text(idents[0], source);

    Some((class_name, is_constructor, is_destructor))
}

/// Find the `block` child of a `defProc` node.
fn get_method_block(def_proc: Node) -> Option<Node> {
    let mut cursor = def_proc.walk();
    let result = def_proc.children(&mut cursor).find(|c| c.kind() == "block");
    result
}

/// Collect all field-creation assignments in a block.
///
/// Returns a list of `FieldCreation` for assignments where:
/// - LHS is a known field name (case-insensitive)
/// - RHS is a constructor call (not owner-managed)
fn collect_field_creations(block: Node, source: &[u8], fields: &[String]) -> Vec<FieldCreation> {
    let mut creations = Vec::new();
    collect_field_creations_recursive(block, source, fields, &mut creations);
    creations
}

fn collect_field_creations_recursive(
    node: Node,
    source: &[u8],
    fields: &[String],
    out: &mut Vec<FieldCreation>,
) {
    if node.kind() == "assignment" {
        if let Some(creation) = parse_field_creation(node, source, fields) {
            out.push(creation);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_field_creations_recursive(child, source, fields, out);
    }
}

/// Parse an `assignment` node as a potential field creation.
fn parse_field_creation(node: Node, source: &[u8], fields: &[String]) -> Option<FieldCreation> {
    let lhs = node.child_by_field_name("lhs")?;
    let rhs = node.child_by_field_name("rhs")?;

    if !is_constructor_call(rhs, source) {
        return None;
    }
    if constructor_has_owner_args(rhs, source) {
        return None;
    }

    let lhs_text = node_text(lhs, source);
    // Check if the LHS matches a known field (case-insensitive)
    let field_name = fields
        .iter()
        .find(|f| f.eq_ignore_ascii_case(&lhs_text))?
        .clone();

    let start = node.start_position();
    let end = node.end_position();
    Some(FieldCreation {
        field_name,
        line: start.row + 1,
        column: start.column + 1,
        end_line: end.row + 1,
        end_column: end.column + 1,
    })
}

/// Extract source text from the start of a block up to (but not including) a
/// given byte offset.
fn text_before_byte(source: &[u8], from_byte: usize, to_byte: usize) -> String {
    if from_byte >= to_byte {
        return String::new();
    }
    let end = to_byte.min(source.len());
    let start = from_byte.min(end);
    std::str::from_utf8(&source[start..end])
        .unwrap_or("")
        .to_string()
}

// ─── field-not-freed rule ─────────────────────────────────────────────────────

pub struct FieldNotFreedRule {
    meta: RuleMeta,
}

impl Default for FieldNotFreedRule {
    fn default() -> Self {
        Self::new()
    }
}

impl FieldNotFreedRule {
    pub fn new() -> Self {
        FieldNotFreedRule {
            meta: RuleMeta {
                id: "field-not-freed",
                name: "Field Not Freed",
                category: RuleCategory::ResourceManagement,
                default_severity: Severity::Warning,
                description:
                    "Detects object fields assigned in a constructor that are never freed \
                     in the destructor.",
            },
        }
    }
}

impl Rule for FieldNotFreedRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn check(
        &self,
        _file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        _config: &crate::config::Config,
        ctx: &mut LintContext,
    ) {
        check_field_not_freed(tree.root_node(), source, ctx);
    }
}

fn check_field_not_freed(root: Node, source: &[u8], ctx: &mut LintContext) {
    let classes = collect_classes(root, source);

    // Walk all defProc nodes
    let def_procs = collect_def_procs(root, source);

    for class in &classes {
        // Find all defProcs belonging to this class
        let class_procs: Vec<&DefProcInfo> = def_procs
            .iter()
            .filter(|p| p.class_name.eq_ignore_ascii_case(&class.name))
            .collect();

        // Find constructor block(s) and collect created fields
        let mut created_fields: Vec<FieldCreation> = Vec::new();
        for proc_info in &class_procs {
            if proc_info.is_constructor {
                if let Some(block) = proc_info.block {
                    let creations = collect_field_creations(block, source, &class.fields);
                    created_fields.extend(creations);
                }
            }
        }

        if created_fields.is_empty() {
            continue;
        }

        // Find destructor and get its text
        let destructor_text = class_procs
            .iter()
            .filter(|p| p.is_destructor)
            .filter_map(|p| p.block.map(|b| node_text(b, source)))
            .next();

        // Check each created field
        for creation in &created_fields {
            let is_freed = match &destructor_text {
                Some(text) => text_frees_variable(text, &creation.field_name),
                None => false, // No destructor → not freed
            };

            if !is_freed {
                ctx.report(Diagnostic {
                    rule_id: "field-not-freed".to_string(),
                    severity: Severity::Warning,
                    message: format!(
                        "Field '{}' is assigned a new object in the constructor but is \
                         not freed in the destructor.",
                        creation.field_name
                    ),
                    line: creation.line,
                    column: creation.column,
                    end_line: creation.end_line,
                    end_column: creation.end_column,
                    help: Some(format!(
                        "Add '{}.Free;' (or 'FreeAndNil({});') to the destructor.",
                        creation.field_name, creation.field_name
                    )),
                });
            }
        }
    }
}

// ─── field-reassign-leak rule ─────────────────────────────────────────────────

pub struct FieldReassignLeakRule {
    meta: RuleMeta,
}

impl Default for FieldReassignLeakRule {
    fn default() -> Self {
        Self::new()
    }
}

impl FieldReassignLeakRule {
    pub fn new() -> Self {
        FieldReassignLeakRule {
            meta: RuleMeta {
                id: "field-reassign-leak",
                name: "Field Reassign Leak",
                category: RuleCategory::ResourceManagement,
                default_severity: Severity::Warning,
                description: "Detects object fields reassigned with a constructor call without \
                     freeing the old value first.",
            },
        }
    }
}

impl Rule for FieldReassignLeakRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn check(
        &self,
        _file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        _config: &crate::config::Config,
        ctx: &mut LintContext,
    ) {
        check_field_reassign_leak(tree.root_node(), source, ctx);
    }
}

fn check_field_reassign_leak(root: Node, source: &[u8], ctx: &mut LintContext) {
    let classes = collect_classes(root, source);
    let def_procs = collect_def_procs(root, source);

    for class in &classes {
        let class_procs: Vec<&DefProcInfo> = def_procs
            .iter()
            .filter(|p| p.class_name.eq_ignore_ascii_case(&class.name))
            .collect();

        for proc_info in &class_procs {
            let block = match proc_info.block {
                Some(b) => b,
                None => continue,
            };

            let creations = collect_field_creations(block, source, &class.fields);

            if proc_info.is_constructor {
                // In the constructor, only flag the second (and later) assignments
                // to the same field if no free precedes the re-assignment.
                check_constructor_reassigns(&creations, block, source, ctx);
            } else {
                // In any other method, flag every field creation that is not
                // preceded by a free within the same method block.
                for creation in &creations {
                    let block_start = block.start_byte();
                    let assign_start = byte_of_line(source, creation.line);
                    let preceding = text_before_byte(source, block_start, assign_start);
                    if !text_frees_variable(&preceding, &creation.field_name) {
                        ctx.report(Diagnostic {
                            rule_id: "field-reassign-leak".to_string(),
                            severity: Severity::Warning,
                            message: format!(
                                "Field '{}' is reassigned a new object without freeing \
                                 the old value first.",
                                creation.field_name
                            ),
                            line: creation.line,
                            column: creation.column,
                            end_line: creation.end_line,
                            end_column: creation.end_column,
                            help: Some(format!(
                                "Add '{}.Free;' (or 'FreeAndNil({});') before reassigning.",
                                creation.field_name, creation.field_name
                            )),
                        });
                    }
                }
            }
        }
    }
}

/// In a constructor, detect re-assignments to the same field.
///
/// The first assignment to a field is always legitimate; a second assignment
/// without an intervening free is a bug.
fn check_constructor_reassigns(
    creations: &[FieldCreation],
    block: Node,
    source: &[u8],
    ctx: &mut LintContext,
) {
    use std::collections::HashSet;
    let mut seen: HashSet<String> = HashSet::new();

    // Sort by line so we process in order
    let mut sorted: Vec<&FieldCreation> = creations.iter().collect();
    sorted.sort_by_key(|c| c.line);

    for creation in sorted {
        let field_lower = creation.field_name.to_lowercase();
        if seen.contains(&field_lower) {
            // This is a second (or later) assignment — check for preceding free
            let block_start = block.start_byte();
            let assign_start = byte_of_line(source, creation.line);
            let preceding = text_before_byte(source, block_start, assign_start);
            if !text_frees_variable(&preceding, &creation.field_name) {
                ctx.report(Diagnostic {
                    rule_id: "field-reassign-leak".to_string(),
                    severity: Severity::Warning,
                    message: format!(
                        "Field '{}' is reassigned a new object in the constructor \
                         without freeing the old value first.",
                        creation.field_name
                    ),
                    line: creation.line,
                    column: creation.column,
                    end_line: creation.end_line,
                    end_column: creation.end_column,
                    help: Some(format!(
                        "Add '{}.Free;' (or 'FreeAndNil({});') before reassigning.",
                        creation.field_name, creation.field_name
                    )),
                });
            }
        } else {
            seen.insert(field_lower);
        }
    }
}

/// Return the byte offset of the start of a 1-based line number.
fn byte_of_line(source: &[u8], target_line: usize) -> usize {
    if target_line <= 1 {
        return 0;
    }
    let mut current_line = 1usize;
    for (i, &byte) in source.iter().enumerate() {
        if byte == b'\n' {
            current_line += 1;
            if current_line == target_line {
                return i + 1;
            }
        }
    }
    source.len()
}

// ─── Shared defProc collector ─────────────────────────────────────────────────

struct DefProcInfo<'a> {
    class_name: String,
    is_constructor: bool,
    is_destructor: bool,
    block: Option<Node<'a>>,
}

fn collect_def_procs<'a>(root: Node<'a>, source: &[u8]) -> Vec<DefProcInfo<'a>> {
    let mut result = Vec::new();
    collect_def_procs_recursive(root, source, &mut result);
    result
}

fn collect_def_procs_recursive<'a>(node: Node<'a>, source: &[u8], out: &mut Vec<DefProcInfo<'a>>) {
    if node.kind() == "defProc" {
        if let Some((class_name, is_constructor, is_destructor)) =
            parse_def_proc(node, source)
        {
            let block = get_method_block(node);
            out.push(DefProcInfo {
                class_name,
                is_constructor,
                is_destructor,
                block,
            });
            return; // Don't recurse inside defProc (avoid nested proc confusion)
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_def_procs_recursive(child, source, out);
    }
}
