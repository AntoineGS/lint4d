pub mod casing;
pub mod dangerous;
pub mod exception;
pub mod field_leak;
pub mod helpers;
pub mod inherited;
pub mod naming;
pub mod resource_leak;
pub mod scope;

use tree_sitter::Tree;

use crate::cfg::analysis::AnalysisContext;
use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::source_context::SourceContext;

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

pub struct LintContext<'a> {
    pub diagnostics: Vec<Diagnostic>,
    pub source_ctx: Option<&'a SourceContext>,
}

impl<'a> LintContext<'a> {
    pub fn new() -> Self {
        LintContext {
            diagnostics: Vec::new(),
            source_ctx: None,
        }
    }

    pub fn with_source_ctx(source_ctx: &'a SourceContext) -> Self {
        LintContext {
            diagnostics: Vec::new(),
            source_ctx: Some(source_ctx),
        }
    }

    pub fn report(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }
}

impl Default for LintContext<'_> {
    fn default() -> Self {
        Self::new()
    }
}

pub trait Rule: Send + Sync {
    fn meta(&self) -> &RuleMeta;

    fn requires_context(&self) -> bool {
        false
    }

    fn check(
        &self,
        file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        config: &crate::config::Config,
        ctx: &mut LintContext<'_>,
    );

    fn check_with_context(
        &self,
        file: &FileInfo,
        tree: &Tree,
        source: &[u8],
        config: &crate::config::Config,
        _project: &crate::dcu::ProjectContext,
        ctx: &mut LintContext<'_>,
    ) {
        self.check(file, tree, source, config, ctx);
    }

    fn check_cfg(
        &self,
        _file: &FileInfo,
        _tree: &Tree,
        _source: &[u8],
        _config: &crate::config::Config,
        _analysis: &AnalysisContext<'_>,
        ctx: &mut LintContext<'_>,
    ) {
        // Default no-op: rules override this when they need CFG-based analysis.
        let _ = ctx;
    }

    fn requires_cfg(&self) -> bool {
        false
    }
}

pub struct RuleRegistry {
    rules: Vec<Box<dyn Rule>>,
}

impl RuleRegistry {
    pub fn new() -> Self {
        let rules: Vec<Box<dyn Rule>> = vec![
            Box::new(resource_leak::ResourceLeakUnprotectedRule::new()),
            Box::new(resource_leak::ResourceLeakNoTryRule::new()),
            Box::new(field_leak::FieldNotFreedRule::new()),
            Box::new(field_leak::FieldReassignLeakRule::new()),
            Box::new(exception::EmptyExceptRule::new()),
            Box::new(exception::BareExceptRule::new()),
            Box::new(exception::RaiseInDestructorRule::new()),
            Box::new(naming::TypePrefixRule::new()),
            Box::new(naming::InterfacePrefixRule::new()),
            Box::new(naming::ConstantNamingRule::new()),
            Box::new(naming::LocalVariableNamingRule::new()),
            Box::new(casing::IdentifierCasingRule::new()),
            Box::new(dangerous::WithStatementRule::new()),
            Box::new(inherited::InheritedCallOrderRule::new()),
            Box::new(inherited::InheritedCallMissingRule::new()),
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
