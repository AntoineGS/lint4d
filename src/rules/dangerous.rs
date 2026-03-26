use tree_sitter::{Node, Tree};

use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

// ---------------------------------------------------------------------------
// WithStatementRule
// ---------------------------------------------------------------------------

pub struct WithStatementRule {
    meta: RuleMeta,
}

impl Default for WithStatementRule {
    fn default() -> Self {
        Self::new()
    }
}

impl WithStatementRule {
    pub fn new() -> Self {
        WithStatementRule {
            meta: RuleMeta {
                id: "with-statement",
                name: "With Statement",
                category: RuleCategory::DangerousPattern,
                default_severity: Severity::Warning,
                description: "Detects use of 'with' statements which can cause subtle bugs.",
            },
        }
    }
}

impl Rule for WithStatementRule {
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
        visit_with(tree.root_node(), ctx);
    }
}

fn visit_with(node: Node, ctx: &mut LintContext) {
    if node.kind() == "with" {
        if let Some(kw) = node
            .children(&mut node.walk())
            .find(|c| c.kind() == "kWith")
        {
            let start = kw.start_position();
            let end = kw.end_position();
            ctx.report(Diagnostic {
                rule_id: "with-statement".to_string(),
                severity: Severity::Warning,
                message: "'with' statement obscures scope and can cause subtle bugs.".to_string(),
                line: start.row + 1,
                column: start.column + 1,
                end_line: end.row + 1,
                end_column: end.column + 1,
                help: Some(
                    "Use explicit qualified access (e.g., 'obj.Field') instead of 'with obj do'."
                        .to_string(),
                ),
            });
        }
    }

    for child in node.children(&mut node.walk()) {
        visit_with(child, ctx);
    }
}
