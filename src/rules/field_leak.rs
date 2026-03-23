use tree_sitter::{Node, Tree};

use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::helpers::{
    constructor_has_owner_args, is_constructor_call, node_text, text_frees_variable,
};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};
use std::collections::{HashMap, HashSet};

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

/// A field creation with the original assignment node preserved for AST queries.
#[derive(Debug)]
struct FieldCreationWithNode<'a> {
    creation: FieldCreation,
    node: Node<'a>,
}

fn collect_field_creations_with_nodes<'a>(
    block: Node<'a>,
    source: &[u8],
    fields: &[String],
) -> Vec<FieldCreationWithNode<'a>> {
    let mut results = Vec::new();
    collect_field_creations_with_nodes_recursive(block, source, fields, &mut results);
    results
}

fn collect_field_creations_with_nodes_recursive<'a>(
    node: Node<'a>,
    source: &[u8],
    fields: &[String],
    out: &mut Vec<FieldCreationWithNode<'a>>,
) {
    if node.kind() == "assignment" {
        if let Some(creation) = parse_field_creation(node, source, fields) {
            out.push(FieldCreationWithNode { creation, node });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_field_creations_with_nodes_recursive(child, source, fields, out);
    }
}

/// Pre-computed analysis of a class for field leak rules.
#[derive(Debug)]
struct ClassAnalysis {
    #[allow(dead_code)]
    class_name: String,
    #[allow(dead_code)]
    fields: Vec<String>,
    /// Fields assigned via constructor calls in the class constructor(s).
    constructor_creations: Vec<FieldCreation>,
    /// Fields assigned via constructor calls in non-constructor methods.
    /// Each entry is (method_name, creations_in_that_method).
    method_creations: Vec<(String, Vec<FieldCreation>)>,
    /// Set of field names freed in the destructor (lowercase).
    destructor_frees: HashSet<String>,
}

/// Analyze a single class: collect constructor creations, method creations,
/// and destructor frees.
fn analyze_class(class: &ClassInfo, def_procs: &[DefProcInfo], source: &[u8]) -> ClassAnalysis {
    let class_procs: Vec<&DefProcInfo> = def_procs
        .iter()
        .filter(|p| p.class_name.eq_ignore_ascii_case(&class.name))
        .collect();

    let mut constructor_creations = Vec::new();
    let mut method_creations = Vec::new();

    for proc_info in &class_procs {
        let block = match proc_info.block {
            Some(b) => b,
            None => continue,
        };

        let creations = collect_field_creations(block, source, &class.fields);

        if proc_info.is_constructor {
            constructor_creations.extend(creations);
        } else if !proc_info.is_destructor && !creations.is_empty() {
            method_creations.push((proc_info.method_name.clone(), creations));
        }
    }

    // Collect fields freed in the destructor
    let mut destructor_frees = HashSet::new();
    for proc_info in &class_procs {
        if proc_info.is_destructor {
            if let Some(block) = proc_info.block {
                let block_text = node_text(block, source);
                for field in &class.fields {
                    if text_frees_variable(&block_text, field) {
                        destructor_frees.insert(field.to_lowercase());
                    }
                }
            }
        }
    }

    ClassAnalysis {
        class_name: class.name.clone(),
        fields: class.fields.clone(),
        constructor_creations,
        method_creations,
        destructor_frees,
    }
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

/// Extract class name, method name, and flags from a `defProc` node.
///
/// Returns `(class_name, method_name, is_constructor, is_destructor)`.
fn parse_def_proc(def_proc: Node, source: &[u8]) -> Option<(String, String, bool, bool)> {
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
    let method_name = if idents.len() > 1 {
        node_text(idents[1], source)
    } else {
        String::new()
    };

    Some((class_name, method_name, is_constructor, is_destructor))
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

// ─── Branch detection ────────────────────────────────────────────────────────

/// Which side of an `ifElse` node an assignment lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchSide {
    Then,
    Else,
}

/// Walk up from `node` and return the first `ifElse` ancestor's ID and which
/// branch side the node is in.
///
/// Returns `None` if the node is not inside any `ifElse` then/else branch.
fn branch_id(node: Node) -> Option<(usize, BranchSide)> {
    let mut child = node;
    let mut current = node.parent();

    while let Some(parent) = current {
        if parent.kind() == "ifElse" {
            // Check if child is in the "then" field or "else" field.
            if let Some(then_node) = parent.child_by_field_name("then") {
                if then_node.id() == child.id() || is_ancestor_of(then_node, child) {
                    return Some((parent.id(), BranchSide::Then));
                }
            }
            // else field: can have multiple children
            let mut cursor = parent.walk();
            for else_child in parent.children_by_field_name("else", &mut cursor) {
                if else_child.id() == child.id() || is_ancestor_of(else_child, child) {
                    return Some((parent.id(), BranchSide::Else));
                }
            }
        }
        child = parent;
        current = parent.parent();
    }

    None
}

/// Check if `ancestor` is an ancestor of `descendant` (or equal).
fn is_ancestor_of(ancestor: Node, descendant: Node) -> bool {
    if ancestor.id() == descendant.id() {
        return true;
    }
    let range = ancestor.byte_range();
    let desc_range = descendant.byte_range();
    range.start <= desc_range.start && desc_range.end <= range.end
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
                    "Detects object fields assigned via constructor calls that are never \
                     freed in the destructor.",
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
    let def_procs = collect_def_procs(root, source);

    for class in &classes {
        let analysis = analyze_class(class, &def_procs, source);

        let mut seen: HashSet<String> = HashSet::new();
        let mut to_check: Vec<(&FieldCreation, Option<&str>)> = Vec::new();

        // Constructor creations first (priority)
        for creation in &analysis.constructor_creations {
            let lower = creation.field_name.to_lowercase();
            if seen.insert(lower) {
                to_check.push((creation, None));
            }
        }

        // Then method creations (skip if already seen from constructor)
        for (method_name, creations) in &analysis.method_creations {
            for creation in creations {
                let lower = creation.field_name.to_lowercase();
                if seen.insert(lower) {
                    to_check.push((creation, Some(method_name.as_str())));
                }
            }
        }

        for (creation, method_name) in &to_check {
            let lower = creation.field_name.to_lowercase();
            if !analysis.destructor_frees.contains(&lower) {
                let message = match method_name {
                    None => format!(
                        "Field '{}' is assigned a new object in the constructor but is \
                         not freed in the destructor.",
                        creation.field_name
                    ),
                    Some(name) => format!(
                        "Field '{}' is assigned a new object in '{}' but is \
                         not freed in the destructor.",
                        creation.field_name, name
                    ),
                };
                ctx.report(Diagnostic {
                    rule_id: "field-not-freed".to_string(),
                    severity: Severity::Warning,
                    message,
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
        let analysis = analyze_class(class, &def_procs, source);

        let constructor_fields: HashSet<String> = analysis
            .constructor_creations
            .iter()
            .map(|c| c.field_name.to_lowercase())
            .collect();

        let class_procs: Vec<&DefProcInfo> = def_procs
            .iter()
            .filter(|p| p.class_name.eq_ignore_ascii_case(&class.name))
            .collect();

        for proc_info in &class_procs {
            let block = match proc_info.block {
                Some(b) => b,
                None => continue,
            };

            if proc_info.is_constructor {
                let creations = collect_field_creations(block, source, &class.fields);
                check_constructor_reassigns(&creations, block, source, ctx);
            } else if !proc_info.is_destructor {
                check_method_reassigns(
                    block,
                    source,
                    &class.fields,
                    &constructor_fields,
                    ctx,
                );
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

/// In a non-constructor method, detect reassignments:
/// 1. Cross-method: field was created in constructor and is assigned here without freeing first.
/// 2. Same-method: field is assigned 2+ times in this method, with branch awareness.
fn check_method_reassigns(
    block: Node,
    source: &[u8],
    fields: &[String],
    constructor_fields: &HashSet<String>,
    ctx: &mut LintContext,
) {
    let creations = collect_field_creations_with_nodes(block, source, fields);

    let mut by_field: HashMap<String, Vec<&FieldCreationWithNode>> =
        HashMap::new();
    for c in &creations {
        by_field
            .entry(c.creation.field_name.to_lowercase())
            .or_default()
            .push(c);
    }

    for (field_lower, group) in &by_field {
        let is_in_constructor = constructor_fields.contains(field_lower);

        let mut sorted: Vec<&&FieldCreationWithNode> = group.iter().collect();
        sorted.sort_by_key(|c| c.creation.line);

        for (i, creation_ref) in sorted.iter().enumerate() {
            let creation = &creation_ref.creation;

            let block_start = block.start_byte();
            let assign_start = byte_of_line(source, creation.line);
            let preceding = text_before_byte(source, block_start, assign_start);
            let has_preceding_free = text_frees_variable(&preceding, &creation.field_name);

            if i == 0 {
                // First assignment in this method.
                // Only flag if cross-method (field was in constructor) and no free before.
                if is_in_constructor && !has_preceding_free {
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
            } else {
                // 2nd+ assignment in the same method.
                let prev = &sorted[i - 1];
                let cur_branch = branch_id(creation_ref.node);
                let prev_branch = branch_id(prev.node);

                let mutually_exclusive = match (cur_branch, prev_branch) {
                    (Some((id_a, side_a)), Some((id_b, side_b))) => {
                        id_a == id_b && side_a != side_b
                    }
                    _ => false,
                };

                // For 2nd+ assignments, only check for free BETWEEN the
                // previous assignment and this one (not from block start).
                let prev_end = byte_of_line(source, prev.creation.line + 1);
                let between_text = text_before_byte(source, prev_end, assign_start);
                let has_free_between = text_frees_variable(&between_text, &creation.field_name);

                if !mutually_exclusive && !has_free_between {
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
    method_name: String,
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
        if let Some((class_name, method_name, is_constructor, is_destructor)) =
            parse_def_proc(node, source)
        {
            let block = get_method_block(node);
            out.push(DefProcInfo {
                class_name,
                method_name,
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
