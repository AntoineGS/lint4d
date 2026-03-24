use std::collections::HashMap;

use crate::config::{Config, RuleSeverityOverride};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextEdit {
    pub start_byte: usize,
    pub end_byte: usize,
    pub new_text: String,
}

/// Rename map built from naming rule violations.
#[derive(Debug, Default)]
pub struct RenameMap {
    /// File-scoped renames: lowercase old name -> new name.
    pub file: HashMap<String, String>,
    /// Method-scoped renames: (proc_start_byte, proc_end_byte, lowercase old name) -> new name.
    pub local: HashMap<(usize, usize, String), String>,
}

/// Context for the current procedure during the edit-collection walk.
pub(crate) struct ProcContext {
    pub start_byte: usize,
    pub end_byte: usize,
    pub method_scope: HashMap<String, String>,
    pub class_fields: Option<HashMap<String, String>>,
}

/// Pre-computed flags for which fix rules are enabled.
pub(crate) struct FixConfig {
    pub type_prefix: bool,
    pub intf_prefix: bool,
    pub const_naming: bool,
    pub local_var: bool,
    pub casing: bool,
}

impl FixConfig {
    pub(crate) fn from_config(config: &Config) -> Self {
        Self {
            type_prefix: !matches!(
                config.rule_severity("type-prefix"),
                Some(RuleSeverityOverride::Off)
            ),
            intf_prefix: !matches!(
                config.rule_severity("interface-prefix"),
                Some(RuleSeverityOverride::Off)
            ),
            const_naming: !matches!(
                config.rule_severity("constant-naming"),
                Some(RuleSeverityOverride::Off)
            ),
            local_var: !matches!(
                config.rule_severity("local-variable-naming"),
                Some(RuleSeverityOverride::Off)
            ),
            casing: !matches!(
                config.rule_severity("identifier-casing"),
                Some(RuleSeverityOverride::Off)
            ),
        }
    }
}
