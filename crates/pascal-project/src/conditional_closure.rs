use crate::build_selection::{BuildSelection, BuildSelectionMode};
use crate::conditional::{ConditionalContext, ConditionalFact, ConstantValue};
use crate::rtl_constants::RtlConstantSource;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceOrigin {
    ProjectCompiled,
    Library,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenReason {
    NoProject,
    DiscoveryIncomplete,
    CompilerVersionUnknown,
    ConfigUnresolved,
    ConfigInvalid,
    PlatformUnresolved,
    PlatformInvalid,
}

impl OpenReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoProject => "noProject",
            Self::DiscoveryIncomplete => "discoveryIncomplete",
            Self::CompilerVersionUnknown => "compilerVersionUnknown",
            Self::ConfigUnresolved => "configUnresolved",
            Self::ConfigInvalid => "configInvalid",
            Self::PlatformUnresolved => "platformUnresolved",
            Self::PlatformInvalid => "platformInvalid",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConditionalClosure {
    pub closed: bool,
    pub open_reasons: Vec<OpenReason>,
    pub rtl_source: RtlConstantSource,
    pub rtl_constants: Option<Vec<String>>,
}

/// Determine why a project context must remain open.
pub fn open_reasons(
    has_project: bool,
    discovery_complete: bool,
    compiler_version_known: bool,
    config: Option<&str>,
    platform: Option<&str>,
    config_selection: &BuildSelection,
    platform_selection: &BuildSelection,
) -> Vec<OpenReason> {
    let mut reasons = Vec::new();
    if !has_project {
        reasons.push(OpenReason::NoProject);
    }
    // Global configured values can retain Configured mode even when no current
    // project candidate matches; their compiler facts are still incomplete.
    if !discovery_complete
        || !build_selection_values_are_candidates(config_selection, platform_selection)
    {
        reasons.push(OpenReason::DiscoveryIncomplete);
    }
    if !compiler_version_known {
        reasons.push(OpenReason::CompilerVersionUnknown);
    }
    if config.is_none() {
        reasons.push(OpenReason::ConfigUnresolved);
    }
    if platform.is_none() {
        reasons.push(OpenReason::PlatformUnresolved);
    }
    if config_selection.mode == BuildSelectionMode::Invalid {
        reasons.push(OpenReason::ConfigInvalid);
    }
    if platform_selection.mode == BuildSelectionMode::Invalid {
        reasons.push(OpenReason::PlatformInvalid);
    }
    reasons
}

pub(crate) fn build_selection_values_are_candidates(
    config_selection: &BuildSelection,
    platform_selection: &BuildSelection,
) -> bool {
    [config_selection, platform_selection]
        .into_iter()
        .all(
            |selection| match (selection.mode, selection.selected.as_deref()) {
                (BuildSelectionMode::Invalid, _) | (_, None) => true,
                (_, Some(selected)) => selection
                    .candidates
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(selected)),
            },
        )
}

pub(crate) fn merge_predefined_facts(
    context: &mut ConditionalContext,
    predefined: &BTreeMap<String, ConditionalFact>,
) {
    for (name, fact) in predefined {
        if !context.defines.contains_key(name) {
            context.set_define(name, *fact);
        }
    }
}

pub(crate) fn merge_rtl_constants(
    context: &mut ConditionalContext,
    rtl_constants: Option<&[String]>,
) {
    if let Some(names) = rtl_constants {
        context
            .constants
            .retain(|name, _| !ConditionalContext::is_rtl_version_constant(name));
        for name in names {
            context.set_constant(name, ConstantValue::Boolean(true));
        }
        context.rtl_constants_known = true;
    } else {
        context.rtl_constants_known = false;
    }
}

pub(crate) fn source_has_console_apptype(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'/' && bytes.get(cursor + 1) == Some(&b'/') {
            cursor += 2;
            while cursor < bytes.len() && !matches!(bytes[cursor], b'\r' | b'\n') {
                cursor += 1;
            }
        } else if bytes[cursor] == b'(' && bytes.get(cursor + 1) == Some(&b'*') {
            let Some(end) = source[cursor + 2..].find("*)") else {
                break;
            };
            let end = cursor + 2 + end;
            if bytes.get(cursor + 2) == Some(&b'$') && console_directive(&source[cursor + 2..end]) {
                return true;
            }
            cursor = end + 2;
        } else if bytes[cursor] == b'\'' {
            cursor += 1;
            while cursor < bytes.len() {
                if bytes[cursor] == b'\'' {
                    if bytes.get(cursor + 1) == Some(&b'\'') {
                        cursor += 2;
                    } else {
                        cursor += 1;
                        break;
                    }
                } else {
                    cursor += 1;
                }
            }
        } else if bytes[cursor] == b'{' {
            let Some(end) = bytes[cursor + 1..].iter().position(|byte| *byte == b'}') else {
                break;
            };
            let end = cursor + 1 + end;
            if console_directive(&source[cursor + 1..end]) {
                return true;
            }
            cursor = end + 1;
        } else {
            cursor += 1;
        }
    }
    false
}

fn console_directive(comment: &str) -> bool {
    let Some(directive) = comment.trim().strip_prefix('$') else {
        return false;
    };
    let mut tokens = directive.split_ascii_whitespace();
    tokens
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("APPTYPE"))
        && tokens
            .next()
            .is_some_and(|target| target.eq_ignore_ascii_case("CONSOLE"))
        && tokens.next().is_none()
}
