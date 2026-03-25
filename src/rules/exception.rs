use tree_sitter::{Node, Tree};

use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::field_leak::{get_method_block, parse_def_proc};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

// ---------------------------------------------------------------------------
// BareExceptRule
// ---------------------------------------------------------------------------

pub struct BareExceptRule {
    meta: RuleMeta,
}

impl Default for BareExceptRule {
    fn default() -> Self {
        Self::new()
    }
}

impl BareExceptRule {
    pub fn new() -> Self {
        BareExceptRule {
            meta: RuleMeta {
                id: "bare-except",
                name: "Bare Except Block",
                category: RuleCategory::ExceptionHandling,
                default_severity: Severity::Warning,
                description: "Detects except blocks without a specific exception type.",
            },
        }
    }
}

impl Rule for BareExceptRule {
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
        visit_bare_except(tree.root_node(), source, ctx);
    }
}

fn visit_bare_except(node: Node, source: &[u8], ctx: &mut LintContext) {
    if node.kind() == "try" {
        check_try_bare_except(node, source, ctx);
    }

    for child in node.children(&mut node.walk()) {
        visit_bare_except(child, source, ctx);
    }
}

/// Returns true if the UTF-8 text contains a standalone `raise` keyword.
///
/// Because bare `raise;` (re-raise without an argument) produces an ERROR
/// node in tree-sitter-pascal, we fall back to a text-based check. We look
/// for the word "raise" that is not immediately followed by a non-whitespace
/// identifier character (to avoid matching something like `RaiseError`).
fn source_contains_raise(text: &str) -> bool {
    let lower = text.to_lowercase();
    let bytes = lower.as_bytes();
    let raise = b"raise";
    let len = raise.len();

    let mut i = 0;
    while i + len <= bytes.len() {
        // Check we have "raise" at position i
        if bytes[i..i + len] == *raise {
            // Check that it's not preceded by an identifier character
            let preceded_by_ident =
                i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            // Check that it's not followed by an identifier character
            let followed_by_ident = i + len < bytes.len()
                && (bytes[i + len].is_ascii_alphanumeric() || bytes[i + len] == b'_');

            if !preceded_by_ident && !followed_by_ident {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Check whether a `try` node represents a bare except (no `on` clauses).
///
/// A bare except has `statements` under the except field but no
/// `exceptionHandler` nodes. If the except block also contains a `raise`
/// (cleanup-then-propagate pattern), we skip it.
fn check_try_bare_except(node: Node, source: &[u8], ctx: &mut LintContext) {
    let except_children: Vec<_> = node
        .children_by_field_name("except", &mut node.walk())
        .collect();

    // No except field → try-finally, not try-except.
    if except_children.is_empty() {
        return;
    }

    // If there are exceptionHandler nodes, the except block uses `on E: X do`
    // clauses — that is fine.
    let has_on_clause = except_children
        .iter()
        .any(|child| child.kind() == "exceptionHandler");

    if has_on_clause {
        return;
    }

    // If there are no statements (only kExcept keyword), that is an empty
    // except block handled by empty-except, not this rule.
    let has_statements = except_children
        .iter()
        .any(|child| child.kind() == "statements");

    if !has_statements {
        return;
    }

    // We have a bare except with statements. Check for raise using text search
    // on the except block's source span, because bare `raise;` produces ERROR
    // nodes in tree-sitter-pascal.
    let except_start = except_children
        .iter()
        .map(|c| c.start_byte())
        .min()
        .unwrap_or(0);
    let except_end = except_children
        .iter()
        .map(|c| c.end_byte())
        .max()
        .unwrap_or(0);

    if except_end <= except_start || except_end > source.len() {
        return;
    }

    let except_text = std::str::from_utf8(&source[except_start..except_end]).unwrap_or("");
    if source_contains_raise(except_text) {
        return;
    }

    // Find the kExcept keyword node for location reporting.
    if let Some(except_kw) = except_children.iter().find(|c| c.kind() == "kExcept") {
        let start = except_kw.start_position();
        let end = except_kw.end_position();
        ctx.report(Diagnostic {
            rule_id: "bare-except".to_string(),
            severity: Severity::Warning,
            message: "Bare except block catches all exceptions without specifying a type."
                .to_string(),
            line: start.row + 1,
            column: start.column + 1,
            end_line: end.row + 1,
            end_column: end.column + 1,
            help: Some(
                "Use 'on E: ExceptionType do' to catch specific exceptions, \
                 or add a 'raise' statement to re-raise after cleanup."
                    .to_string(),
            ),
        });
    }
}

pub struct EmptyExceptRule {
    meta: RuleMeta,
}

impl Default for EmptyExceptRule {
    fn default() -> Self {
        Self::new()
    }
}

impl EmptyExceptRule {
    pub fn new() -> Self {
        EmptyExceptRule {
            meta: RuleMeta {
                id: "empty-except",
                name: "Empty Except Block",
                category: RuleCategory::ExceptionHandling,
                default_severity: Severity::Warning,
                description: "Detects empty except blocks that silently swallow exceptions.",
            },
        }
    }
}

impl Rule for EmptyExceptRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn check(
        &self,
        _file: &FileInfo,
        tree: &Tree,
        _source: &[u8],
        _config: &crate::config::Config,
        ctx: &mut LintContext,
    ) {
        visit_node(tree.root_node(), ctx);
    }
}

fn visit_node(node: Node, ctx: &mut LintContext) {
    if node.kind() == "try" {
        check_try_node(node, ctx);
    }

    for child in node.children(&mut node.walk()) {
        visit_node(child, ctx);
    }
}

/// Check whether a `try` node has an empty except block.
///
/// In the tree-sitter-pascal grammar, a try-except block looks like:
///   (try (kTry) try: (statements ...) except: (kExcept) except: (exceptionHandler ...) (kEnd))
///
/// An empty except block has only the `kExcept` keyword with no handler or statements:
///   (try (kTry) try: (statements ...) except: (kExcept) (kEnd))
fn check_try_node(node: Node, ctx: &mut LintContext) {
    let except_children: Vec<_> = node
        .children_by_field_name("except", &mut node.walk())
        .collect();

    // No except field at all means this is a try-finally, not try-except.
    if except_children.is_empty() {
        return;
    }

    // Check if any except child is a handler or statements (not just the keyword).
    let has_body = except_children
        .iter()
        .any(|child| child.kind() != "kExcept");

    if !has_body {
        // Find the kExcept keyword for precise location reporting.
        if let Some(except_kw) = except_children.iter().find(|c| c.kind() == "kExcept") {
            let start = except_kw.start_position();
            let end = except_kw.end_position();
            ctx.report(Diagnostic {
                rule_id: "empty-except".to_string(),
                severity: Severity::Warning,
                message: "Empty except block silently swallows exceptions.".to_string(),
                line: start.row + 1,
                column: start.column + 1,
                end_line: end.row + 1,
                end_column: end.column + 1,
                help: Some(
                    "Add an exception handler or log the error. \
                     Use 'on E: Exception do ...' to handle specific exceptions."
                        .to_string(),
                ),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// RaiseInDestructorRule
// ---------------------------------------------------------------------------

pub struct RaiseInDestructorRule {
    meta: RuleMeta,
}

impl Default for RaiseInDestructorRule {
    fn default() -> Self {
        Self::new()
    }
}

impl RaiseInDestructorRule {
    pub fn new() -> Self {
        RaiseInDestructorRule {
            meta: RuleMeta {
                id: "raise-in-destructor",
                name: "Raise In Destructor",
                category: RuleCategory::ExceptionHandling,
                default_severity: Severity::Warning,
                description: "Detects unguarded 'raise' statements inside destructors \
                              that can escape and cause memory leaks or broken teardown.",
            },
        }
    }
}

impl Rule for RaiseInDestructorRule {
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
        visit_destructor_raises(tree.root_node(), source, ctx);
    }
}

fn visit_destructor_raises(node: Node, source: &[u8], ctx: &mut LintContext) {
    if node.kind() == "defProc" {
        check_destructor_raises(node, source, ctx);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_destructor_raises(child, source, ctx);
    }
}

fn check_destructor_raises(def_proc: Node, source: &[u8], ctx: &mut LintContext) {
    let Some((class_name, method_name, _is_constructor, is_destructor)) =
        parse_def_proc(def_proc, source)
    else {
        return;
    };

    if !is_destructor {
        return;
    }

    let Some(block) = get_method_block(def_proc) else {
        return;
    };

    find_unguarded_raises(block, source, &class_name, &method_name, ctx);
}

/// Recursively scan for raise statements, skipping try..except subtrees.
fn find_unguarded_raises(
    node: Node,
    source: &[u8],
    class_name: &str,
    method_name: &str,
    ctx: &mut LintContext,
) {
    // Normal raise statement
    if node.kind() == "raise" {
        report_raise(node, class_name, method_name, ctx);
        return;
    }

    // Bare re-raise: ERROR node with a single kRaise child
    if node.is_error() && is_bare_raise(node) {
        report_raise(node, class_name, method_name, ctx);
        return;
    }

    // If this is a try node with an except clause, the raises inside are guarded
    if node.kind() == "try" && try_has_except(node) {
        return;
    }

    // Descend into all children (including try..finally, nested procs, etc.)
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_unguarded_raises(child, source, class_name, method_name, ctx);
    }
}

/// Check if a try node has an except clause.
fn try_has_except(node: Node) -> bool {
    node.children_by_field_name("except", &mut node.walk())
        .any(|_| true)
}

/// Check if an ERROR node represents a bare `raise;`.
fn is_bare_raise(node: Node) -> bool {
    if node.child_count() == 1 {
        if let Some(child) = node.child(0) {
            return child.kind() == "kRaise";
        }
    }
    false
}

fn report_raise(node: Node, class_name: &str, method_name: &str, ctx: &mut LintContext) {
    let start = node.start_position();
    let end = node.end_position();
    ctx.report(Diagnostic {
        rule_id: "raise-in-destructor".to_string(),
        severity: Severity::Warning,
        message: format!(
            "Unguarded 'raise' in destructor {}.{} may cause memory leaks or broken teardown",
            class_name, method_name
        ),
        line: start.row + 1,
        column: start.column + 1,
        end_line: end.row + 1,
        end_column: end.column + 1,
        help: Some(
            "Wrap this code in a try..except block to prevent the exception \
             from escaping the destructor"
                .to_string(),
        ),
    });
}
