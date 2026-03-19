use tree_sitter::{Node, Tree};

use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

pub struct EmptyExceptRule {
    meta: RuleMeta,
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

    fn check(&self, _file: &FileInfo, tree: &Tree, _source: &[u8], ctx: &mut LintContext) {
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
        if let Some(except_kw) = except_children
            .iter()
            .find(|c| c.kind() == "kExcept")
        {
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
