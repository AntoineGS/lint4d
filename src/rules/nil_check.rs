use std::collections::HashMap;

use tree_sitter::{Node, Tree};

use crate::cfg::analysis::AnalysisContext;
use crate::dcu::{ProjectContext, TypeKind};
use crate::engine::{FileInfo, Severity};
use crate::rules::helpers::{
    extract_type_from_decl_arg, extract_type_from_decl_var, has_out_modifier, node_text,
};
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

/// Metadata about a tracked nillable variable.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct VarInfo {
    /// The declared name (original casing).
    declared_name: String,
    /// Whether this is a parameter (true) or local variable (false).
    is_param: bool,
}

/// Check if a type is nillable based on DCU type resolution.
#[allow(dead_code)]
fn is_nillable_type(type_name: &str, project: &ProjectContext, uses: &[String]) -> bool {
    match project.resolve_type(type_name, uses) {
        Some(ty) => matches!(
            ty.kind,
            TypeKind::Class | TypeKind::Interface | TypeKind::Pointer | TypeKind::Procedural
        ),
        None => false,
    }
}

/// Collect all nillable parameters and local variables from a `defProc` node.
///
/// Returns a map of lowercase variable name -> VarInfo for all nillable vars.
#[allow(dead_code)]
fn build_nillable_vars(
    def_proc: Node,
    source: &[u8],
    project: &ProjectContext,
    uses: &[String],
) -> HashMap<String, VarInfo> {
    let mut vars = HashMap::new();

    // Collect parameters from header
    if let Some(header) = def_proc.child_by_field_name("header") {
        let mut header_cursor = header.walk();
        for child in header.children(&mut header_cursor) {
            if child.kind() == "declArgs" {
                let mut args_cursor = child.walk();
                for arg in child.children(&mut args_cursor) {
                    if arg.kind() != "declArg" {
                        continue;
                    }
                    if has_out_modifier(arg) {
                        continue;
                    }
                    let type_name = match extract_type_from_decl_arg(arg, source) {
                        Some(t) => t,
                        None => continue,
                    };
                    if !is_nillable_type(&type_name, project, uses) {
                        continue;
                    }
                    let arg_count = arg.child_count();
                    for i in 0..arg_count {
                        let c = match arg.child(i) {
                            Some(c) => c,
                            None => continue,
                        };
                        if c.kind() == "identifier"
                            && arg.field_name_for_child(i as u32) == Some("name")
                        {
                            let name = node_text(c, source);
                            vars.insert(
                                name.to_lowercase(),
                                VarInfo {
                                    declared_name: name,
                                    is_param: true,
                                },
                            );
                        }
                    }
                }
            }
        }
    }

    // Collect local variables from declVars
    let mut proc_cursor = def_proc.walk();
    for child in def_proc.children(&mut proc_cursor) {
        if child.kind() != "declVars" {
            continue;
        }
        let mut vars_cursor = child.walk();
        for decl_var in child.children(&mut vars_cursor) {
            if decl_var.kind() != "declVar" {
                continue;
            }
            let type_name = match extract_type_from_decl_var(decl_var, source) {
                Some(t) => t,
                None => continue,
            };
            if !is_nillable_type(&type_name, project, uses) {
                continue;
            }
            let dv_count = decl_var.child_count();
            for i in 0..dv_count {
                let c = match decl_var.child(i) {
                    Some(c) => c,
                    None => continue,
                };
                if c.kind() == "identifier"
                    && decl_var.field_name_for_child(i as u32) == Some("name")
                {
                    let name = node_text(c, source);
                    vars.insert(
                        name.to_lowercase(),
                        VarInfo {
                            declared_name: name,
                            is_param: false,
                        },
                    );
                }
            }
        }
    }

    vars
}
