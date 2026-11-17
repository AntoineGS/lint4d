use super::dcc_switches::{
    CodeSwitch, GlobalSwitch, SwitchState, SwitchTable, common_switch_table, switch_table,
};
use super::{Document, SemanticDiagnostic, SemanticDiagnosticKind};
use pascal_core::conditional::{DirectiveKind, Truth};
use pascal_project::PropertyValue;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceOverride {
    Inherit,
    State(SwitchState),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeAction {
    Set(SwitchState),
    Default,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DirectiveAction {
    Global(GlobalSwitch, Option<SwitchState>),
    Code(String, CodeAction),
    All(CodeAction),
    UnknownWarnings,
    UnknownWarningIds,
    UnknownAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarnAllSupport {
    Accepted,
    Rejected,
    Unknown,
}

#[derive(Debug, Clone)]
struct Event {
    offset: usize,
    order: usize,
    activity: Truth,
    action: DirectiveAction,
}

#[derive(Debug)]
struct CodeState {
    entry: CodeSwitch,
    project_override: Option<Option<SwitchState>>,
    initial: Option<SwitchState>,
    source_override: SourceOverride,
    changes: Vec<(usize, Option<SwitchState>)>,
    switched: bool,
}

impl CodeState {
    fn effective(
        &self,
        warnings: Option<SwitchState>,
        hints: Option<SwitchState>,
    ) -> Option<SwitchState> {
        match self.source_override {
            SourceOverride::State(state) => Some(state),
            SourceOverride::Unknown => None,
            SourceOverride::Inherit => {
                if let Some(project_override) = self.project_override {
                    project_override
                } else {
                    inherited_state(
                        global_for(self.entry.global, warnings, hints),
                        self.entry.default,
                    )
                }
            }
        }
    }
}

/// The positional warning state for one parsed source document.
///
/// Conditional analysis has already lexed compiler directives in both Pascal
/// comment forms and expanded include sources. This pass consumes those
/// directives instead of searching comment text again.
#[derive(Debug, Default)]
pub(crate) struct WarningSwitches {
    codes: BTreeMap<String, CodeState>,
    settled_positions: bool,
}

impl WarningSwitches {
    pub(crate) fn new(document: &Document) -> Self {
        let table = document
            .conditional_context
            .compiler_version
            .and_then(|version| switch_table(version.major, version.minor))
            .or_else(common_switch_table);
        let Some(table) = table else {
            return Self::default();
        };
        Self::from_document(document, &table)
    }

    fn from_document(document: &Document, table: &SwitchTable) -> Self {
        let warn_all_support = match document
            .conditional_context
            .compiler_version
            .map(|version| (version.major, version.minor))
        {
            Some((37, 0)) => WarnAllSupport::Accepted,
            Some((21, 0) | (24, 0)) => WarnAllSupport::Rejected,
            _ => WarnAllSupport::Unknown,
        };
        let warning_properties = document.warning_properties.as_deref();
        let initial_warnings = initial_global(warning_properties, GlobalSwitch::Warnings);
        let initial_hints = initial_global(warning_properties, GlobalSwitch::Hints);
        let mut codes = BTreeMap::new();
        for entry in table.entries() {
            let property_override = entry.id.as_deref().and_then(|id| {
                let property = format!("DCC_{id}");
                property_value(warning_properties, &property)
                    .map(|value| parse_property_state(value, entry.error_capable))
            });
            let initial = property_override.unwrap_or_else(|| {
                inherited_state(
                    global_for(entry.global, initial_warnings, initial_hints),
                    entry.default,
                )
            });
            let code = entry.code.to_owned();
            codes.insert(
                code,
                CodeState {
                    entry: entry.clone(),
                    project_override: property_override,
                    initial,
                    source_override: SourceOverride::Inherit,
                    changes: vec![(0, initial)],
                    switched: false,
                },
            );
        }

        let mut events = Vec::new();
        let mut order = 0usize;
        if !document.conditionals.complete {
            events.push(Event {
                offset: 0,
                order,
                activity: Truth::True,
                action: DirectiveAction::UnknownAll,
            });
            order += 1;
        }
        for directive in &document.conditionals.directives {
            if directive.activity == Truth::False {
                continue;
            }
            let action = if directive.kind == DirectiveKind::Include {
                Some(DirectiveAction::UnknownAll)
            } else {
                directive_action(table, &directive.body, warn_all_support)
            };
            if let Some(action) = action {
                events.push(Event {
                    offset: directive.end,
                    order,
                    activity: directive.activity,
                    action,
                });
                order += 1;
            }
        }
        for offset in unresolved_include_offsets(document) {
            events.push(Event {
                offset,
                order,
                activity: Truth::True,
                action: DirectiveAction::UnknownAll,
            });
            order += 1;
        }
        events.sort_by_key(|event| (event.offset, event.order));

        let mut warnings = initial_warnings;
        let mut hints = initial_hints;
        for event in events {
            let action = if event.activity == Truth::Unknown {
                match event.action {
                    DirectiveAction::Global(global, _) => DirectiveAction::Global(global, None),
                    DirectiveAction::Code(id, _) => DirectiveAction::Code(id, CodeAction::Unknown),
                    DirectiveAction::All(_) => DirectiveAction::UnknownWarningIds,
                    DirectiveAction::UnknownWarnings => DirectiveAction::UnknownWarnings,
                    DirectiveAction::UnknownWarningIds => DirectiveAction::UnknownWarningIds,
                    DirectiveAction::UnknownAll => DirectiveAction::UnknownAll,
                }
            } else {
                event.action
            };

            match action {
                DirectiveAction::Global(global, state) => {
                    set_global(global, state, &mut warnings, &mut hints);
                    for state in codes
                        .values_mut()
                        .filter(|state| state.entry.global == global)
                    {
                        state.switched = true;
                        let effective = state.effective(warnings, hints);
                        state.changes.push((event.offset, effective));
                    }
                }
                DirectiveAction::Code(id, code_action) => {
                    for state in codes
                        .values_mut()
                        .filter(|state| state.entry.id.as_deref() == Some(id.as_str()))
                    {
                        state.switched = true;
                        state.source_override = match code_action {
                            CodeAction::Set(switch) => SourceOverride::State(switch),
                            CodeAction::Default => state
                                .initial
                                .map_or(SourceOverride::Unknown, SourceOverride::State),
                            CodeAction::Unknown => SourceOverride::Unknown,
                        };
                        let effective = state.effective(warnings, hints);
                        state.changes.push((event.offset, effective));
                    }
                }
                DirectiveAction::All(code_action) => {
                    for state in codes.values_mut().filter(|state| state.entry.id.is_some()) {
                        state.switched = true;
                        state.source_override = match code_action {
                            CodeAction::Set(switch) => SourceOverride::State(switch),
                            CodeAction::Default => state
                                .initial
                                .map_or(SourceOverride::Unknown, SourceOverride::State),
                            CodeAction::Unknown => SourceOverride::Unknown,
                        };
                        let effective = state.effective(warnings, hints);
                        state.changes.push((event.offset, effective));
                    }
                }
                DirectiveAction::UnknownWarnings => {
                    warnings = None;
                    for state in codes
                        .values_mut()
                        .filter(|state| state.entry.global == GlobalSwitch::Warnings)
                    {
                        state.switched = true;
                        let effective = state.effective(warnings, hints);
                        state.changes.push((event.offset, effective));
                    }
                }
                DirectiveAction::UnknownWarningIds => {
                    for state in codes.values_mut().filter(|state| state.entry.id.is_some()) {
                        state.switched = true;
                        state.source_override = SourceOverride::Unknown;
                        state.changes.push((event.offset, None));
                    }
                }
                DirectiveAction::UnknownAll => {
                    warnings = None;
                    hints = None;
                    for state in codes.values_mut() {
                        state.switched = true;
                        state.source_override = SourceOverride::Unknown;
                        state.changes.push((event.offset, None));
                    }
                }
            }
        }

        Self {
            codes,
            settled_positions: true,
        }
    }

    /// Return the switch governing a finding. `None` means the version,
    /// directive state, or governing position is not proven.
    pub(crate) fn effective_switch(
        &self,
        document: &Document,
        code: &str,
        offset: usize,
    ) -> Option<SwitchState> {
        let code = code.to_ascii_uppercase();
        let state = self.codes.get(&code)?;
        if !self.settled_positions && state.switched {
            return None;
        }
        if !has_recorded_position_rule(&code) && state.switched {
            return None;
        }
        let governing_offset = if code == "H2219" {
            // dcc reports H2219 at the end-of-unit position, not at the field
            // declaration. The paired fixture records this behavior.
            document.source.len()
        } else {
            offset
        };
        let index = state
            .changes
            .partition_point(|(position, _)| *position <= governing_offset);
        state
            .changes
            .get(index.saturating_sub(1))
            .and_then(|(_, state)| *state)
    }

    pub(crate) fn error_capable(&self, code: &str) -> bool {
        self.codes
            .get(&code.to_ascii_uppercase())
            .is_some_and(|state| state.entry.error_capable)
    }
}

pub(crate) fn filter_report(document: &Document, diagnostics: &mut Vec<SemanticDiagnostic>) {
    let switches = WarningSwitches::new(document);
    diagnostics.retain_mut(|diagnostic| {
        let SemanticDiagnosticKind::Dcc(message) = diagnostic.kind else {
            return true;
        };
        let code = message.code();
        if !(code.starts_with('W') || code.starts_with('H')) {
            return true;
        }
        let Some(state) = switches.effective_switch(document, code, diagnostic.span.start) else {
            return false;
        };
        match state {
            SwitchState::On => true,
            SwitchState::Off => false,
            SwitchState::Error if switches.error_capable(code) => {
                diagnostic.kind = SemanticDiagnosticKind::PromotedDcc(message);
                true
            }
            SwitchState::Error => true,
        }
    });
}

fn has_recorded_position_rule(code: &str) -> bool {
    matches!(
        code,
        "H2077"
            | "H2135"
            | "H2164"
            | "H2219"
            | "W1000"
            | "W1001"
            | "W1002"
            | "W1003"
            | "W1004"
            | "W1005"
            | "W1006"
            | "W1007"
            | "W1009"
            | "W1010"
            | "W1035"
            | "W1036"
            | "W1037"
    )
}

fn global_for(
    global: GlobalSwitch,
    warnings: Option<SwitchState>,
    hints: Option<SwitchState>,
) -> Option<SwitchState> {
    match global {
        GlobalSwitch::Warnings => warnings,
        GlobalSwitch::Hints => hints,
    }
}

fn set_global(
    global: GlobalSwitch,
    state: Option<SwitchState>,
    warnings: &mut Option<SwitchState>,
    hints: &mut Option<SwitchState>,
) {
    match global {
        GlobalSwitch::Warnings => *warnings = state,
        GlobalSwitch::Hints => *hints = state,
    }
}

fn inherited_state(global: Option<SwitchState>, default: SwitchState) -> Option<SwitchState> {
    match global {
        Some(SwitchState::On) => Some(default),
        Some(SwitchState::Off) => Some(SwitchState::Off),
        Some(SwitchState::Error) | None => None,
    }
}

fn initial_global(
    properties: Option<&BTreeMap<String, PropertyValue>>,
    global: GlobalSwitch,
) -> Option<SwitchState> {
    let name = match global {
        GlobalSwitch::Warnings => "DCC_Warnings",
        GlobalSwitch::Hints => "DCC_Hints",
    };
    property_value(properties, name).map_or(Some(SwitchState::On), |value| match value {
        PropertyValue::Known(value) if value.trim().eq_ignore_ascii_case("true") => {
            Some(SwitchState::On)
        }
        PropertyValue::Known(value) if value.trim().eq_ignore_ascii_case("false") => {
            Some(SwitchState::Off)
        }
        _ => None,
    })
}

fn property_value<'a>(
    properties: Option<&'a BTreeMap<String, PropertyValue>>,
    name: &str,
) -> Option<&'a PropertyValue> {
    properties?
        .iter()
        .find_map(|(property, value)| property.eq_ignore_ascii_case(name).then_some(value))
}

fn parse_property_state(value: &PropertyValue, error_capable: bool) -> Option<SwitchState> {
    let PropertyValue::Known(value) = value else {
        return None;
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "true" => Some(SwitchState::On),
        "false" => Some(SwitchState::Off),
        "error" if error_capable => Some(SwitchState::Error),
        _ => None,
    }
}

fn directive_action(
    table: &SwitchTable,
    body: &str,
    warn_all_support: WarnAllSupport,
) -> Option<DirectiveAction> {
    let mut tokens = body.split_ascii_whitespace();
    let keyword = tokens.next()?.trim_end_matches(':').to_ascii_uppercase();
    let arguments = tokens.collect::<Vec<_>>();
    match keyword.as_str() {
        "WARNINGS" => {
            let state = match arguments.as_slice() {
                [state] if state.eq_ignore_ascii_case("on") => Some(SwitchState::On),
                [state] if state.eq_ignore_ascii_case("off") => Some(SwitchState::Off),
                _ => None,
            };
            Some(DirectiveAction::Global(GlobalSwitch::Warnings, state))
        }
        "HINTS" => {
            let state = match arguments.as_slice() {
                [state] if state.eq_ignore_ascii_case("on") => Some(SwitchState::On),
                [state] if state.eq_ignore_ascii_case("off") => Some(SwitchState::Off),
                _ => None,
            };
            Some(DirectiveAction::Global(GlobalSwitch::Hints, state))
        }
        "WARN" => {
            let Some(id) = arguments.first() else {
                return Some(DirectiveAction::UnknownWarnings);
            };
            if id.eq_ignore_ascii_case("ALL") {
                return match warn_all_support {
                    WarnAllSupport::Accepted => {
                        let action = match arguments.as_slice() {
                            [_, state] if state.eq_ignore_ascii_case("on") => {
                                CodeAction::Set(SwitchState::On)
                            }
                            [_, state] if state.eq_ignore_ascii_case("off") => {
                                CodeAction::Set(SwitchState::Off)
                            }
                            [_, state] if state.eq_ignore_ascii_case("error") => {
                                CodeAction::Set(SwitchState::Error)
                            }
                            [_, state] if state.eq_ignore_ascii_case("default") => {
                                CodeAction::Default
                            }
                            _ => CodeAction::Unknown,
                        };
                        Some(if action == CodeAction::Unknown {
                            DirectiveAction::UnknownWarningIds
                        } else {
                            DirectiveAction::All(action)
                        })
                    }
                    WarnAllSupport::Rejected => None,
                    WarnAllSupport::Unknown => Some(DirectiveAction::UnknownWarningIds),
                };
            }
            let known_id = table
                .entries()
                .find(|entry| {
                    entry
                        .id
                        .as_deref()
                        .is_some_and(|known| known.eq_ignore_ascii_case(id))
                })
                .and_then(|entry| entry.id.as_deref());
            let known_id = known_id?;
            let action = match arguments.as_slice() {
                [_, state] if state.eq_ignore_ascii_case("on") => CodeAction::Set(SwitchState::On),
                [_, state] if state.eq_ignore_ascii_case("off") => {
                    CodeAction::Set(SwitchState::Off)
                }
                [_, state] if state.eq_ignore_ascii_case("error") => {
                    CodeAction::Set(SwitchState::Error)
                }
                [_, state] if state.eq_ignore_ascii_case("default") => CodeAction::Default,
                _ => CodeAction::Unknown,
            };
            Some(DirectiveAction::Code(known_id.to_owned(), action))
        }
        _ => None,
    }
}

fn unresolved_include_offsets(document: &Document) -> Vec<usize> {
    let Some(session) = document.compile_time_session.as_deref() else {
        return Vec::new();
    };
    let active_unresolved = session
        .includes
        .iter()
        .filter(|include| include.child.is_none())
        .filter(|include| {
            session
                .occurrences
                .iter()
                .find(|occurrence| occurrence.id == include.owner)
                .and_then(|occurrence| {
                    occurrence.analysis.directives.iter().find(|directive| {
                        directive.kind == DirectiveKind::Include
                            && directive.start == include.directive.range.start
                            && directive.end == include.directive.range.end
                    })
                })
                .is_none_or(|directive| directive.activity != Truth::False)
        })
        .count();
    if active_unresolved == 0 {
        return Vec::new();
    }
    let markers = document
        .source
        .match_indices("{ unresolved include }")
        .map(|(offset, _)| offset)
        .take(active_unresolved)
        .collect::<Vec<_>>();
    if markers.len() == active_unresolved {
        markers
    } else {
        vec![0]
    }
}

#[cfg(test)]
mod tests {
    use super::{SwitchTable, WarningSwitches};
    use crate::navigation::NavigationIndex;
    use lsp_types::Url;

    fn synthetic_table(warning_id: &str) -> SwitchTable {
        let text = format!(
            "# dcc-switches v1\n\
             # banner: synthetic\n\
             # script-version: 1\n\
             [codes]\n\
             W1000 ON WARNINGS SYMBOL_DEPRECATED yes\n\
             W1009 ON WARNINGS {warning_id} yes\n\
             [properties]\n\
             DCC_SYMBOL_DEPRECATED SYMBOL_DEPRECATED\n\
             DCC_Warnings Warnings\n"
        );
        SwitchTable::parse(&text).expect("synthetic switch table")
    }

    #[test]
    fn disagreeing_switch_rows_leave_that_warning_state_unknown() {
        let first = synthetic_table("HIDING_MEMBER");
        let second = synthetic_table("OTHER_MEMBER");
        let table = SwitchTable::consensus(&[&first, &second]).expect("W1000 agrees across tables");
        let uri = Url::parse("file:///tmp/switch-filter-table-disagreement.pas").unwrap();
        let mut index = NavigationIndex::new();
        index
            .update(
                uri.clone(),
                "unit Probe; interface implementation end.".to_owned(),
            )
            .expect("probe parses");
        let document = index.documents.get(&uri).expect("probe document");

        let switches = WarningSwitches::from_document(document, &table);

        assert_eq!(
            switches.effective_switch(document, "W1009", 0),
            None,
            "a code that disagrees between synthetic tables remains unknown"
        );
    }
}
