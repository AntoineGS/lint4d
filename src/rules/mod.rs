pub mod dangerous;
pub mod exception;
pub mod naming;

use tree_sitter::Tree;

use crate::engine::{Diagnostic, FileInfo, Severity};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleCategory {
    ResourceManagement,
    ExceptionHandling,
    NamingConvention,
    DangerousPattern,
}

pub struct RuleMeta {
    pub id: &'static str,
    pub name: &'static str,
    pub category: RuleCategory,
    pub default_severity: Severity,
    pub description: &'static str,
}

pub struct LintContext {
    pub diagnostics: Vec<Diagnostic>,
}

impl LintContext {
    pub fn new() -> Self {
        LintContext {
            diagnostics: Vec::new(),
        }
    }

    pub fn report(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }
}

impl Default for LintContext {
    fn default() -> Self {
        Self::new()
    }
}

pub trait Rule: Send + Sync {
    fn meta(&self) -> &RuleMeta;
    fn check(&self, file: &FileInfo, tree: &Tree, source: &[u8], ctx: &mut LintContext);
}

struct StubRule {
    meta: RuleMeta,
}

impl Rule for StubRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn check(&self, _file: &FileInfo, _tree: &Tree, _source: &[u8], _ctx: &mut LintContext) {
        // Stub: does nothing
    }
}

pub struct RuleRegistry {
    rules: Vec<Box<dyn Rule>>,
}

impl RuleRegistry {
    pub fn new() -> Self {
        let rules: Vec<Box<dyn Rule>> = vec![
            Box::new(StubRule {
                meta: RuleMeta {
                    id: "resource-leak-unprotected",
                    name: "Resource Leak: Unprotected",
                    category: RuleCategory::ResourceManagement,
                    default_severity: Severity::Error,
                    description: "Detects resources created without try..finally protection.",
                },
            }),
            Box::new(StubRule {
                meta: RuleMeta {
                    id: "resource-leak-no-try",
                    name: "Resource Leak: No Try Block",
                    category: RuleCategory::ResourceManagement,
                    default_severity: Severity::Warning,
                    description: "Detects resources created without any try block.",
                },
            }),
            Box::new(exception::EmptyExceptRule::new()),
            Box::new(exception::BareExceptRule::new()),
            Box::new(naming::TypePrefixRule::new()),
            Box::new(naming::InterfacePrefixRule::new()),
            Box::new(StubRule {
                meta: RuleMeta {
                    id: "constant-naming",
                    name: "Constant Naming Convention",
                    category: RuleCategory::NamingConvention,
                    default_severity: Severity::Hint,
                    description: "Enforces naming conventions for constants.",
                },
            }),
            Box::new(dangerous::WithStatementRule::new()),
        ];

        RuleRegistry { rules }
    }

    pub fn all_rules(&self) -> &[Box<dyn Rule>] {
        &self.rules
    }

    pub fn get(&self, id: &str) -> Option<&dyn Rule> {
        self.rules
            .iter()
            .find(|r| r.meta().id == id)
            .map(|r| r.as_ref())
    }
}

impl Default for RuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}
