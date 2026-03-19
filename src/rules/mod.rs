pub mod dangerous;
pub mod exception;
pub mod naming;
pub mod resource_leak;

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

pub struct RuleRegistry {
    rules: Vec<Box<dyn Rule>>,
}

impl RuleRegistry {
    pub fn new() -> Self {
        let rules: Vec<Box<dyn Rule>> = vec![
            Box::new(resource_leak::ResourceLeakUnprotectedRule::new()),
            Box::new(resource_leak::ResourceLeakNoTryRule::new()),
            Box::new(exception::EmptyExceptRule::new()),
            Box::new(exception::BareExceptRule::new()),
            Box::new(naming::TypePrefixRule::new()),
            Box::new(naming::InterfacePrefixRule::new()),
            Box::new(naming::ConstantNamingRule::new()),
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
