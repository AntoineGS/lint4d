use tree_sitter::Tree;

use crate::cfg::analysis::AnalysisContext;
use crate::engine::{FileInfo, Severity};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

pub struct UncheckedNilRule {
    meta: RuleMeta,
}

impl Default for UncheckedNilRule {
    fn default() -> Self {
        Self::new()
    }
}

impl UncheckedNilRule {
    pub fn new() -> Self {
        UncheckedNilRule {
            meta: RuleMeta {
                id: "unchecked-nil",
                name: "Unchecked Nil",
                category: RuleCategory::NullSafety,
                default_severity: Severity::Warning,
                description: "Detects nillable variables used without a preceding nil check.",
                enabled_by_default: false,
            },
        }
    }
}

impl Rule for UncheckedNilRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn requires_cfg(&self) -> bool {
        true
    }

    fn check(
        &self,
        _file: &FileInfo,
        _tree: &Tree,
        _source: &[u8],
        _config: &crate::config::Config,
        _ctx: &mut LintContext,
    ) {
        // CFG-based rule; analysis happens in check_cfg.
    }

    fn check_cfg(
        &self,
        _file: &FileInfo,
        _tree: &Tree,
        _source: &[u8],
        _config: &crate::config::Config,
        _analysis: &AnalysisContext<'_>,
        _ctx: &mut LintContext,
    ) {
        // Placeholder — implemented in Task 4
    }
}
