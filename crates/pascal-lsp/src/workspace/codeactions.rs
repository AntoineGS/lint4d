//! Naming quick-fix planning for the workspace snapshot.

use super::rename::{
    CANCELLATION_MESSAGE, Computed, OverlayInput, RenameSnapshot, SnapshotMode, SnapshotSeed,
    SourceRecord, WorkspaceInput, build_snapshot, check_includes, identifier_at_position,
    input_source_is_editable, is_cancelled, may_contain_include_directive, snapshot_records,
    source_for_input_with_cancel, text_content_hash, workspace_edit,
};
use super::{
    absolute_path, canonical_file_uri, is_configuration_file, is_lint_excluded, path_stamp,
};
use crate::configuration::{config_directories, resolve_lint};
use crate::navigation::{
    AssistanceBudget, MAX_MISSING_UNIT_REQUEST_BYTES, MAX_MISSING_UNIT_REQUEST_WORK,
    MissingInterfaceMethodImplementationCandidate, MissingMethodImplementationCandidate,
    MissingUnitCandidate, MissingUnitUseKind, SemanticDiagnosticKind,
};
use crate::text;
use lint4d::config::{Config, RuleSeverityOverride};
use lint4d::engine::suppress::parse_suppressions;
use lint4d::rules::helpers::effective_children;
use lint4d::rules::naming::{
    to_camel_case, to_pascal_case, to_upper_snake_case, violates_naming_style,
};
use lsp_types::{
    CodeAction, CodeActionContext, CodeActionDisabled, CodeActionKind, CodeActionOrCommand,
    CodeActionParams, Diagnostic, NumberOrString, Range, TextEdit, Url, WorkspaceEdit,
};
use pascal_core::{FileInfo, parser};
use pascal_project::{has_invalid_project_selection, project_candidates};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::AtomicBool;
use tree_sitter::Node;

const ACTION_DATA_VERSION: u8 = 4;
const MAX_ACTION_ID_BYTES: usize = 64;
const MAX_ACTION_NAME_BYTES: usize = 256;
const MAX_ACTION_UNIT_BYTES: usize = 256;
const MAX_ACTION_URI_BYTES: usize = 4096;
const MAX_MISSING_UNIT_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_METHOD_HEADER_BYTES: usize = 16 * 1024;
const MISSING_UNIT_ACTION_KIND: &str = "add-missing-unit";
const METHOD_IMPLEMENTATION_ACTION_KIND: &str = "implement-method";
const METHOD_IMPLEMENTATION_ACTION_DATA_VERSION: u8 = 1;
const INTERFACE_METHOD_IMPLEMENTATION_ACTION_KIND: &str = "implement-interface-method";
const INTERFACE_METHOD_IMPLEMENTATION_ACTION_DATA_VERSION: u8 = 1;
const INTERFACE_METHOD_IMPLEMENTATION_CODE_ACTION_KIND: CodeActionKind =
    CodeActionKind::new("quickfix.implement-interface-method");
const CONSTANT_RULE: &str = "constant-naming";
const LOCAL_RULE: &str = "local-variable-naming";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RenameActionData {
    pub(crate) version: u8,
    pub(crate) action_id: String,
    pub(crate) rule: String,
    pub(crate) uri: Url,
    pub(crate) anchor: Range,
    pub(crate) new_name: String,
    #[serde(with = "decimal_u64")]
    pub(crate) source_generation: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) configuration_generation: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) config_fingerprint: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) source_hash: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MissingUnitActionData {
    pub(crate) version: u8,
    pub(crate) action_id: String,
    pub(crate) kind: String,
    pub(crate) uri: Url,
    pub(crate) anchor: Range,
    pub(crate) identifier: String,
    pub(crate) unit_name: String,
    pub(crate) provider_uri: Url,
    pub(crate) provider_symbol: String,
    pub(crate) use_kind: MissingUnitUseKind,
    #[serde(with = "decimal_u64")]
    pub(crate) provider_source_hash: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) provider_declaration_fingerprint: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) provider_context_fingerprint: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) use_context_fingerprint: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) source_generation: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) configuration_generation: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) config_fingerprint: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) source_hash: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MethodImplementationActionData {
    pub(crate) version: u8,
    pub(crate) action_id: String,
    pub(crate) kind: String,
    pub(crate) uri: Url,
    pub(crate) anchor: Range,
    pub(crate) declaration: Range,
    pub(crate) owner: String,
    pub(crate) method: String,
    pub(crate) unit_name: String,
    #[serde(with = "decimal_u64")]
    pub(crate) identity: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) source_generation: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) configuration_generation: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) config_fingerprint: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) source_hash: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InterfaceMethodImplementationActionData {
    pub(crate) version: u8,
    pub(crate) action_id: String,
    pub(crate) kind: String,
    pub(crate) uri: Url,
    pub(crate) anchor: Range,
    pub(crate) class_declaration: Range,
    pub(crate) owner: String,
    pub(crate) method: String,
    pub(crate) interface_uri: Url,
    pub(crate) interface_owner: String,
    pub(crate) interface_method: String,
    pub(crate) unit_name: String,
    #[serde(with = "decimal_u64")]
    pub(crate) identity: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) source_generation: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) configuration_generation: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) config_fingerprint: u64,
    #[serde(with = "decimal_u64")]
    pub(crate) source_hash: u64,
}

mod decimal_u64 {
    use super::*;

    pub(super) fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone)]
struct NamingCandidate {
    rule: String,
    uri: Url,
    anchor: Range,
    old_name: String,
    new_name: String,
    config_fingerprint: u64,
    source_hash: u64,
    diagnostic: Option<Diagnostic>,
}

#[derive(Debug, Clone)]
struct MissingUnitPlan {
    candidate: MissingUnitCandidate,
    uri: Url,
    position: lsp_types::Position,
    identifier: String,
    source_hash: u64,
    config_fingerprint: u64,
    diagnostic: Option<Diagnostic>,
    edit: TextEdit,
}

#[derive(Debug, Clone)]
struct MethodImplementationPlan {
    candidate: MissingMethodImplementationCandidate,
    uri: Url,
    source_hash: u64,
    config_fingerprint: u64,
    edit: TextEdit,
}

#[derive(Debug, Clone)]
struct InterfaceMethodImplementationPlan {
    candidate: MissingInterfaceMethodImplementationCandidate,
    uri: Url,
    source_hash: u64,
    config_fingerprint: u64,
    diagnostic: Option<Diagnostic>,
    edits: Vec<TextEdit>,
}

#[derive(Debug, Clone)]
enum ParsedActionData {
    Rename(RenameActionData),
    MissingUnit(MissingUnitActionData),
    MethodImplementation(MethodImplementationActionData),
    InterfaceMethodImplementation(InterfaceMethodImplementationActionData),
}

struct CandidateRequest<'a> {
    uri: &'a Url,
    source: &'a str,
    config_fingerprint: u64,
    suppressions: &'a [pascal_core::directives::Suppression],
    requested_range: Range,
    context: &'a CodeActionContext,
}

#[derive(Debug, Clone)]
pub(crate) struct ClientActionFeatures {
    pub(crate) resolve: bool,
    pub(crate) document_changes: bool,
    pub(crate) disabled: bool,
}

pub(crate) fn code_actions_from_input(
    input: WorkspaceInput,
    params: CodeActionParams,
    features: ClientActionFeatures,
    cancel: &AtomicBool,
) -> Computed<Vec<CodeActionOrCommand>> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let requests_quickfix = requests_quickfix(&params.context);
    let requests_interface_method = requests_interface_method(&params.context);
    if !requests_quickfix && !requests_interface_method {
        return Computed {
            source_generation,
            configuration_generation,
            value: Ok(Vec::new()),
            records: Vec::new(),
        };
    }
    let uri = canonical_file_uri(&params.text_document.uri);
    let (source, target_record) = match source_for_input_with_cancel(&input, &uri, Some(cancel)) {
        Ok(source) => source,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if !input_source_is_editable(&input, &uri) {
        return failed(
            source_generation,
            configuration_generation,
            format!("code-action document is outside configured workspace roots: {uri}"),
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let (config, config_fingerprint, excluded, configuration_records) =
        match lint_configuration_for_input(&input, &uri) {
            Ok(config) => config,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if excluded {
        let mut records = vec![target_record];
        append_records(&mut records, configuration_records);
        return Computed {
            source_generation,
            configuration_generation,
            value: Ok(Vec::new()),
            records,
        };
    }
    let candidates = if requests_quickfix {
        match naming_candidates(
            &uri,
            &source,
            params.range,
            &params.context,
            &config,
            config_fingerprint,
        ) {
            Ok(candidates) => candidates,
            Err(error) => return failed(source_generation, configuration_generation, error),
        }
    } else {
        Vec::new()
    };
    let (missing_plans, missing_records) = if requests_quickfix {
        match missing_unit_plans_from_input(
            &input,
            &uri,
            &source,
            &target_record,
            &params,
            config_fingerprint,
            &configuration_records,
            cancel,
        ) {
            Ok(result) => result,
            Err(error) => return failed(source_generation, configuration_generation, error),
        }
    } else {
        (Vec::new(), Vec::new())
    };
    let (method_plans, method_records) = if requests_quickfix {
        match method_implementation_plans_from_input(
            &input,
            &uri,
            &source,
            &target_record,
            &params,
            config_fingerprint,
            &configuration_records,
            cancel,
        ) {
            Ok(result) => result,
            Err(error) => return failed(source_generation, configuration_generation, error),
        }
    } else {
        (Vec::new(), Vec::new())
    };
    let (interface_method_plans, interface_method_records) = if requests_interface_method {
        match interface_method_implementation_plans_from_input(
            &input,
            &uri,
            &source,
            &target_record,
            &params,
            config_fingerprint,
            &configuration_records,
            cancel,
        ) {
            Ok(result) => result,
            Err(error) => return failed(source_generation, configuration_generation, error),
        }
    } else {
        (Vec::new(), Vec::new())
    };
    if features.resolve {
        let mut actions: Vec<CodeActionOrCommand> = Vec::new();
        for candidate in &candidates {
            let action = CodeActionOrCommand::CodeAction({
                let data =
                    RenameActionData::new(candidate, source_generation, configuration_generation);
                CodeAction {
                    title: action_title(candidate),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: candidate
                        .diagnostic
                        .clone()
                        .map(|diagnostic| vec![diagnostic]),
                    edit: None,
                    command: None,
                    is_preferred: Some(true),
                    disabled: None,
                    data: Some(
                        serde_json::to_value(&data).expect("rename action data is serializable"),
                    ),
                }
            });
            match push_bounded_code_action(&mut actions, action) {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => return failed(source_generation, configuration_generation, error),
            }
        }
        for plan in &missing_plans {
            let data =
                MissingUnitActionData::new(plan, source_generation, configuration_generation);
            let action = CodeActionOrCommand::CodeAction(CodeAction {
                title: missing_unit_action_title(&plan.candidate),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: plan.diagnostic.clone().map(|diagnostic| vec![diagnostic]),
                edit: None,
                command: None,
                is_preferred: Some(missing_plans.len() == 1),
                disabled: None,
                data: Some(
                    serde_json::to_value(&data).expect("missing unit action data is serializable"),
                ),
            });
            match push_bounded_code_action(&mut actions, action) {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => return failed(source_generation, configuration_generation, error),
            }
        }
        for plan in &method_plans {
            let data = MethodImplementationActionData::new(
                plan,
                source_generation,
                configuration_generation,
            );
            let action = CodeActionOrCommand::CodeAction(CodeAction {
                title: method_implementation_action_title(&plan.candidate),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: None,
                command: None,
                is_preferred: Some(method_plans.len() == 1),
                disabled: None,
                data: Some(
                    serde_json::to_value(&data)
                        .expect("method implementation action data is serializable"),
                ),
            });
            match push_bounded_code_action(&mut actions, action) {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => return failed(source_generation, configuration_generation, error),
            }
        }
        for plan in &interface_method_plans {
            let data = InterfaceMethodImplementationActionData::new(
                plan,
                source_generation,
                configuration_generation,
            );
            let action = CodeActionOrCommand::CodeAction(CodeAction {
                title: interface_method_implementation_action_title(&plan.candidate),
                kind: Some(INTERFACE_METHOD_IMPLEMENTATION_CODE_ACTION_KIND),
                diagnostics: plan.diagnostic.clone().map(|diagnostic| vec![diagnostic]),
                edit: None,
                command: None,
                is_preferred: Some(interface_method_plans.len() == 1),
                disabled: None,
                data: Some(
                    serde_json::to_value(&data)
                        .expect("interface method implementation action data is serializable"),
                ),
            });
            match push_bounded_code_action(&mut actions, action) {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => return failed(source_generation, configuration_generation, error),
            }
        }
        return Computed {
            source_generation,
            configuration_generation,
            value: Ok(actions),
            records: {
                let mut records = vec![target_record];
                append_records(&mut records, missing_records);
                append_records(&mut records, method_records);
                append_records(&mut records, interface_method_records);
                append_records(&mut records, configuration_records);
                records
            },
        };
    }

    let mut candidate_names = Vec::new();
    for candidate in &candidates {
        for name in [&candidate.old_name, &candidate.new_name] {
            if !candidate_names.iter().any(|existing| existing == name) {
                candidate_names.push(name.clone());
            }
        }
    }
    let mode = if candidates
        .iter()
        .all(|candidate| candidate.rule == LOCAL_RULE)
    {
        SnapshotMode::Local
    } else {
        SnapshotMode::Workspace
    };
    let snapshot = match build_snapshot(
        &input,
        std::slice::from_ref(&uri),
        &candidate_names,
        mode,
        Some(
            SnapshotSeed::new(target_record.clone())
                .with_consumed_configuration(&configuration_records),
        ),
        &[],
        cancel,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let Some(_source) = snapshot.sources.get(&uri) else {
        return failed(
            source_generation,
            configuration_generation,
            format!("code-action document was not retained in the workspace snapshot: {uri}"),
        );
    };
    if !snapshot.editable.contains(&uri) {
        return failed(
            source_generation,
            configuration_generation,
            format!("code-action document is outside configured workspace roots: {uri}"),
        );
    }
    let mut actions = Vec::new();
    for candidate in candidates {
        if is_cancelled(cancel) {
            return cancelled(source_generation, configuration_generation);
        }
        let data = RenameActionData::new(&candidate, source_generation, configuration_generation);
        let title = action_title(&candidate);
        let mut action = CodeAction {
            title,
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: candidate
                .diagnostic
                .clone()
                .map(|diagnostic| vec![diagnostic]),
            edit: None,
            command: None,
            is_preferred: Some(true),
            disabled: None,
            data: Some(serde_json::to_value(&data).expect("rename action data is serializable")),
        };

        if !snapshot.complete {
            let reason = snapshot
                .incomplete_reason
                .clone()
                .unwrap_or_else(|| "bounded workspace discovery did not finish".to_string());
            if !set_disabled_or_skip(
                &mut action,
                features.disabled,
                format!("workspace scan incomplete: {reason}"),
            ) {
                continue;
            }
        } else if !features.resolve {
            match plan_candidate(
                &snapshot,
                &candidate,
                features.document_changes,
                Some(cancel),
            ) {
                Ok((edit, _edit_records)) => {
                    action.edit = Some(edit);
                }
                Err(error) => {
                    if !set_disabled_or_skip(&mut action, features.disabled, error) {
                        continue;
                    }
                }
            }
        }
        match push_bounded_code_action(&mut actions, CodeActionOrCommand::CodeAction(action)) {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => return failed(source_generation, configuration_generation, error),
        }
    }
    for plan in &missing_plans {
        if is_cancelled(cancel) {
            return cancelled(source_generation, configuration_generation);
        }
        let data = MissingUnitActionData::new(plan, source_generation, configuration_generation);
        let mut action = CodeAction {
            title: missing_unit_action_title(&plan.candidate),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: plan.diagnostic.clone().map(|diagnostic| vec![diagnostic]),
            edit: None,
            command: None,
            is_preferred: Some(missing_plans.len() == 1),
            disabled: None,
            data: Some(
                serde_json::to_value(&data).expect("missing unit action data is serializable"),
            ),
        };
        if !features.resolve {
            let mut raw_edits = std::collections::HashMap::new();
            raw_edits.insert(uri.clone(), vec![plan.edit.clone()]);
            let mut records_by_uri = std::collections::HashMap::new();
            records_by_uri.insert(uri.clone(), target_record.clone());
            match workspace_edit(raw_edits, &records_by_uri, features.document_changes) {
                Ok(edit) => action.edit = Some(edit),
                Err(error) => {
                    if !set_disabled_or_skip(&mut action, features.disabled, error) {
                        continue;
                    }
                }
            }
        }
        match push_bounded_code_action(&mut actions, CodeActionOrCommand::CodeAction(action)) {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => return failed(source_generation, configuration_generation, error),
        }
    }
    for plan in &method_plans {
        if is_cancelled(cancel) {
            return cancelled(source_generation, configuration_generation);
        }
        let data =
            MethodImplementationActionData::new(plan, source_generation, configuration_generation);
        let mut action = CodeAction {
            title: method_implementation_action_title(&plan.candidate),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: None,
            edit: None,
            command: None,
            is_preferred: Some(method_plans.len() == 1),
            disabled: None,
            data: Some(
                serde_json::to_value(&data)
                    .expect("method implementation action data is serializable"),
            ),
        };
        if !features.resolve {
            let mut raw_edits = std::collections::HashMap::new();
            raw_edits.insert(uri.clone(), vec![plan.edit.clone()]);
            let mut records_by_uri = std::collections::HashMap::new();
            records_by_uri.insert(uri.clone(), target_record.clone());
            match workspace_edit(raw_edits, &records_by_uri, features.document_changes) {
                Ok(edit) => action.edit = Some(edit),
                Err(error) => {
                    if !set_disabled_or_skip(&mut action, features.disabled, error) {
                        continue;
                    }
                }
            }
        }
        match push_bounded_code_action(&mut actions, CodeActionOrCommand::CodeAction(action)) {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => return failed(source_generation, configuration_generation, error),
        }
    }
    for plan in &interface_method_plans {
        if is_cancelled(cancel) {
            return cancelled(source_generation, configuration_generation);
        }
        let data = InterfaceMethodImplementationActionData::new(
            plan,
            source_generation,
            configuration_generation,
        );
        let mut action = CodeAction {
            title: interface_method_implementation_action_title(&plan.candidate),
            kind: Some(INTERFACE_METHOD_IMPLEMENTATION_CODE_ACTION_KIND),
            diagnostics: plan.diagnostic.clone().map(|diagnostic| vec![diagnostic]),
            edit: None,
            command: None,
            is_preferred: Some(interface_method_plans.len() == 1),
            disabled: None,
            data: Some(
                serde_json::to_value(&data)
                    .expect("interface method implementation action data is serializable"),
            ),
        };
        if !features.resolve {
            let mut raw_edits = std::collections::HashMap::new();
            raw_edits.insert(uri.clone(), plan.edits.clone());
            let mut records_by_uri = std::collections::HashMap::new();
            records_by_uri.insert(uri.clone(), target_record.clone());
            match workspace_edit(raw_edits, &records_by_uri, features.document_changes) {
                Ok(edit) => action.edit = Some(edit),
                Err(error) => {
                    if !set_disabled_or_skip(&mut action, features.disabled, error) {
                        continue;
                    }
                }
            }
        }
        match push_bounded_code_action(&mut actions, CodeActionOrCommand::CodeAction(action)) {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => return failed(source_generation, configuration_generation, error),
        }
    }
    let mut records = snapshot_records(&snapshot);
    append_records(&mut records, missing_records);
    append_records(&mut records, method_records);
    append_records(&mut records, interface_method_records);
    append_records(&mut records, configuration_records);
    Computed {
        source_generation,
        configuration_generation,
        value: Ok(actions),
        records,
    }
}

#[allow(clippy::too_many_arguments)]
fn missing_unit_plans_from_input(
    input: &WorkspaceInput,
    uri: &Url,
    source: &str,
    target_record: &SourceRecord,
    params: &CodeActionParams,
    config_fingerprint: u64,
    configuration_records: &[SourceRecord],
    cancel: &AtomicBool,
) -> Result<(Vec<MissingUnitPlan>, Vec<SourceRecord>), String> {
    if may_contain_include_directive(source.as_bytes()) {
        return Ok((Vec::new(), Vec::new()));
    }
    let Some(identifier) = identifier_at_position(source, params.range.start) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let snapshot = build_snapshot(
        input,
        std::slice::from_ref(uri),
        std::slice::from_ref(&identifier),
        SnapshotMode::Assistance,
        Some(
            SnapshotSeed::new(target_record.clone())
                .with_consumed_configuration(configuration_records)
                .with_completion_position(Some(params.range.start)),
        ),
        &[],
        cancel,
    )?;
    if !snapshot.complete
        || !snapshot.include_errors.is_empty()
        || !snapshot.editable.contains(uri)
        || !snapshot.sources.contains_key(uri)
        || snapshot
            .sources
            .get(uri)
            .is_none_or(|indexed| indexed != source)
    {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut budget = AssistanceBudget::new(
        MAX_MISSING_UNIT_REQUEST_WORK,
        MAX_MISSING_UNIT_REQUEST_BYTES,
        "missing-unit request",
    );
    let candidates = snapshot.index.missing_unit_candidates_with_budget(
        uri,
        params.range.start,
        cancel,
        &mut budget,
    )?;
    let mut plans = Vec::new();
    for candidate in candidates {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        if !missing_unit_candidate_identity_is_bounded(&candidate, uri, &identifier) {
            continue;
        }
        let diagnostic = matching_missing_unit_diagnostic(&candidate, &params.context);
        if !params.context.diagnostics.is_empty() && diagnostic.is_none() {
            continue;
        }
        let Some(edit) = snapshot.index.missing_unit_edit_with_cancel(
            uri,
            params.range.start,
            &candidate.unit_name,
            cancel,
        )?
        else {
            continue;
        };
        let Some(updated_source) = apply_text_edit(source, &edit) else {
            continue;
        };
        let Some(updated_position) =
            position_after_text_edit(source, params.range.start, &edit, &updated_source)
        else {
            continue;
        };
        let binds = missing_unit_binding_proof(
            input,
            uri,
            target_record,
            configuration_records,
            updated_source,
            updated_position,
            &identifier,
            &candidate.unit_name,
            &candidate.provider_uri,
            &candidate.symbol_name,
            candidate.use_kind,
            candidate.provider_source_hash,
            candidate.provider_declaration_fingerprint,
            candidate.provider_context_fingerprint,
            candidate.use_context_fingerprint,
            cancel,
            &mut budget,
        )?;
        if !binds {
            continue;
        }
        plans.push(MissingUnitPlan {
            candidate,
            uri: uri.clone(),
            position: params.range.start,
            identifier: identifier.clone(),
            source_hash: source_hash(source),
            config_fingerprint,
            diagnostic,
            edit,
        });
    }
    if plans.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    Ok((plans, snapshot_records(&snapshot)))
}

#[allow(clippy::too_many_arguments)]
fn method_implementation_plans_from_input(
    input: &WorkspaceInput,
    uri: &Url,
    source: &str,
    target_record: &SourceRecord,
    params: &CodeActionParams,
    config_fingerprint: u64,
    configuration_records: &[SourceRecord],
    cancel: &AtomicBool,
) -> Result<(Vec<MethodImplementationPlan>, Vec<SourceRecord>), String> {
    // A declaration or insertion point inside an include expansion has no
    // single physical owner unless a separate provenance proof is available.
    // This action deliberately chooses the safe, direct-source subset.
    if may_contain_include_directive(source.as_bytes()) {
        return Ok((Vec::new(), Vec::new()));
    }
    let Some(identifier) = identifier_at_position(source, params.range.start) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let snapshot = match build_snapshot(
        input,
        std::slice::from_ref(uri),
        std::slice::from_ref(&identifier),
        SnapshotMode::Workspace,
        Some(
            SnapshotSeed::new(target_record.clone())
                .with_consumed_configuration(configuration_records),
        ),
        &[],
        cancel,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) if is_cancelled(cancel) => return Err(error),
        // Method generation is an optional action. An incomplete project
        // context suppresses this action, but must not hide unrelated
        // naming or missing-unit actions from the same request.
        Err(_) => return Ok((Vec::new(), Vec::new())),
    };
    if !snapshot.complete
        || !snapshot.include_errors.is_empty()
        || !snapshot.editable.contains(uri)
        || snapshot
            .sources
            .get(uri)
            .is_none_or(|indexed| indexed != source)
        || snapshot
            .index
            .source_text(uri)
            .is_none_or(|indexed| indexed != source)
    {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut budget = AssistanceBudget::new(
        MAX_MISSING_UNIT_REQUEST_WORK,
        MAX_MISSING_UNIT_REQUEST_BYTES,
        "method implementation request",
    );
    let candidates = snapshot
        .index
        .missing_method_implementation_candidates_with_budget(
            uri,
            params.range.start,
            cancel,
            &mut budget,
        )?;
    let mut plans = Vec::new();
    for candidate in candidates {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        if !method_candidate_identity_is_bounded(&candidate, uri) {
            continue;
        }
        let Some(edit) = method_implementation_edit(source, &candidate) else {
            continue;
        };
        plans.push(MethodImplementationPlan {
            candidate,
            uri: uri.clone(),
            source_hash: source_hash(source),
            config_fingerprint,
            edit,
        });
    }
    if plans.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    Ok((plans, snapshot_records(&snapshot)))
}

#[allow(clippy::too_many_arguments)]
fn interface_method_implementation_plans_from_input(
    input: &WorkspaceInput,
    uri: &Url,
    source: &str,
    target_record: &SourceRecord,
    params: &CodeActionParams,
    config_fingerprint: u64,
    configuration_records: &[SourceRecord],
    cancel: &AtomicBool,
) -> Result<(Vec<InterfaceMethodImplementationPlan>, Vec<SourceRecord>), String> {
    // A class declaration inside an include expansion has no unique physical
    // owner. Interface generation deliberately refuses that case rather than
    // writing a declaration or implementation into the including unit.
    if may_contain_include_directive(source.as_bytes()) {
        return Ok((Vec::new(), Vec::new()));
    }
    let Some(identifier) = identifier_at_position(source, params.range.start) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let snapshot = match build_snapshot(
        input,
        std::slice::from_ref(uri),
        std::slice::from_ref(&identifier),
        SnapshotMode::Workspace,
        Some(
            SnapshotSeed::new(target_record.clone())
                .with_consumed_configuration(configuration_records),
        ),
        &[],
        cancel,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) if is_cancelled(cancel) => return Err(error),
        // Interface generation is an optional assistance action. An
        // incomplete project graph must not hide unrelated code actions.
        Err(_) => return Ok((Vec::new(), Vec::new())),
    };
    if !snapshot.complete
        || !snapshot.include_errors.is_empty()
        || !snapshot.editable.contains(uri)
        || snapshot
            .sources
            .get(uri)
            .is_none_or(|indexed| indexed != source)
        || snapshot
            .index
            .source_text(uri)
            .is_none_or(|indexed| indexed != source)
    {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut budget = AssistanceBudget::new(
        MAX_MISSING_UNIT_REQUEST_WORK,
        MAX_MISSING_UNIT_REQUEST_BYTES,
        "interface method implementation request",
    );
    let candidates = snapshot
        .index
        .missing_interface_method_implementation_candidates_with_budget(
            uri,
            params.range.start,
            cancel,
            &mut budget,
        )?;
    let mut plans = Vec::new();
    for candidate in candidates {
        if is_cancelled(cancel) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        if !interface_method_candidate_identity_is_bounded(&candidate, uri) {
            continue;
        }
        let diagnostic = matching_missing_interface_diagnostic(&candidate, &params.context);
        if !params.context.diagnostics.is_empty() && diagnostic.is_none() {
            continue;
        }
        let Some(edits) = interface_method_implementation_edits(uri, source, &candidate) else {
            continue;
        };
        if !interface_post_edit_proves_obligation(
            input,
            uri,
            source,
            target_record,
            configuration_records,
            &candidate,
            &edits,
            cancel,
        )? {
            continue;
        }
        plans.push(InterfaceMethodImplementationPlan {
            candidate,
            uri: uri.clone(),
            source_hash: source_hash(source),
            config_fingerprint,
            diagnostic,
            edits,
        });
    }
    if plans.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    Ok((plans, snapshot_records(&snapshot)))
}

fn method_candidate_identity_is_bounded(
    candidate: &MissingMethodImplementationCandidate,
    uri: &Url,
) -> bool {
    !candidate.owner.is_empty()
        && candidate.owner.len() <= MAX_ACTION_UNIT_BYTES
        && !candidate.method.is_empty()
        && candidate.method.len() <= MAX_ACTION_NAME_BYTES
        && !candidate.unit_name.is_empty()
        && candidate.unit_name.len() <= MAX_ACTION_UNIT_BYTES
        && candidate.header.len() <= MAX_METHOD_HEADER_BYTES
        && uri.as_str().len() <= MAX_ACTION_URI_BYTES
}

fn method_implementation_edit(
    source: &str,
    candidate: &MissingMethodImplementationCandidate,
) -> Option<TextEdit> {
    method_implementation_edit_for_header(
        source,
        candidate.insertion_offset,
        &candidate.header,
        &candidate.owner,
        &candidate.method,
    )
}

fn method_implementation_edit_for_header(
    source: &str,
    insertion_offset: usize,
    header: &str,
    owner: &str,
    method: &str,
) -> Option<TextEdit> {
    if owner.contains(['\r', '\n', '\0'])
        || method.contains(['\r', '\n', '\0'])
        || header.contains('\0')
    {
        return None;
    }
    if insertion_offset > source.len() || !source.is_char_boundary(insertion_offset) {
        return None;
    }
    let position = text::offset_to_position(source, insertion_offset)?;
    let line_start = source[..insertion_offset]
        .rfind('\n')
        .map_or(0, |index| index.saturating_add(1));
    let line_start = if source[line_start..insertion_offset].contains('\r') {
        source[..insertion_offset]
            .rfind('\r')
            .map_or(0, |index| index.saturating_add(1))
    } else {
        line_start
    };
    let current_prefix = source.get(line_start..insertion_offset)?;
    if current_prefix.len() > MAX_METHOD_HEADER_BYTES {
        return None;
    }
    let indentation = if current_prefix.trim().is_empty() {
        current_prefix
    } else {
        ""
    };
    let line_ending = source_line_ending(source);
    let header = normalize_line_endings(header, line_ending);
    let leading = if current_prefix.trim().is_empty() {
        ""
    } else {
        line_ending
    };
    let mut new_text = String::new();
    new_text.push_str(leading);
    new_text.push_str(&header);
    new_text.push_str(line_ending);
    new_text.push_str(indentation);
    new_text.push_str("begin");
    new_text.push_str(line_ending);
    new_text.push_str(indentation);
    new_text.push_str("  // TODO: Implement ");
    new_text.push_str(owner);
    new_text.push('.');
    new_text.push_str(method);
    new_text.push('.');
    new_text.push_str(line_ending);
    new_text.push_str(indentation);
    new_text.push_str("end;");
    new_text.push_str(line_ending);
    new_text.push_str(indentation);
    if new_text.len() > MAX_METHOD_HEADER_BYTES.saturating_mul(2) {
        return None;
    }
    Some(TextEdit::new(Range::new(position, position), new_text))
}

fn interface_method_candidate_identity_is_bounded(
    candidate: &MissingInterfaceMethodImplementationCandidate,
    target_uri: &Url,
) -> bool {
    let bounded_name = |value: &str, limit: usize| {
        !value.is_empty() && value.len() <= limit && !value.contains(['\r', '\n', '\0'])
    };
    bounded_name(&candidate.owner, MAX_ACTION_UNIT_BYTES)
        && bounded_name(&candidate.method, MAX_ACTION_NAME_BYTES)
        && bounded_name(&candidate.interface_owner, MAX_ACTION_UNIT_BYTES)
        && bounded_name(&candidate.interface_method, MAX_ACTION_NAME_BYTES)
        && bounded_name(&candidate.unit_name, MAX_ACTION_UNIT_BYTES)
        && candidate
            .declaration_header
            .as_deref()
            .is_none_or(|header| !header.contains('\0') && header.len() <= MAX_METHOD_HEADER_BYTES)
        && candidate.implementation_header.len() <= MAX_METHOD_HEADER_BYTES
        && candidate
            .declaration_indent
            .as_deref()
            .is_none_or(|indent| indent.len() <= MAX_ACTION_UNIT_BYTES && indent.trim().is_empty())
        && candidate
            .declaration_owner_indent
            .as_deref()
            .is_none_or(|indent| indent.len() <= MAX_ACTION_UNIT_BYTES && indent.trim().is_empty())
        && target_uri.as_str().len() <= MAX_ACTION_URI_BYTES
        && candidate.interface_uri.as_str().len() <= MAX_ACTION_URI_BYTES
}

fn interface_method_implementation_edits(
    uri: &Url,
    source: &str,
    candidate: &MissingInterfaceMethodImplementationCandidate,
) -> Option<Vec<TextEdit>> {
    let line_ending = source_line_ending(source);
    let mut edits = Vec::with_capacity(2);
    match (
        candidate.declaration_header.as_deref(),
        candidate.declaration_insert_start,
        candidate.declaration_insert_end,
        candidate.declaration_indent.as_deref(),
        candidate.declaration_owner_indent.as_deref(),
    ) {
        (None, None, None, None, None) => {}
        (Some(header), Some(start), Some(end), Some(indent), Some(owner_indent)) => {
            if start > end
                || end > source.len()
                || !source.is_char_boundary(start)
                || !source.is_char_boundary(end)
                || !indent.trim().is_empty()
                || !owner_indent.trim().is_empty()
            {
                return None;
            }
            let start_position = text::offset_to_position(source, start)?;
            let end_position = text::offset_to_position(source, end)?;
            let header = normalize_line_endings(header, line_ending);
            let mut new_text = String::new();
            if candidate.declaration_add_public {
                new_text.push_str(owner_indent);
                new_text.push_str("public");
                new_text.push_str(line_ending);
            }
            new_text.push_str(indent);
            new_text.push_str(&header);
            new_text.push_str(line_ending);
            new_text.push_str(owner_indent);
            if new_text.len() > MAX_METHOD_HEADER_BYTES.saturating_mul(2) {
                return None;
            }
            edits.push(TextEdit::new(
                Range::new(start_position, end_position),
                new_text,
            ));
        }
        _ => return None,
    }

    let implementation = method_implementation_edit_for_header(
        source,
        candidate.implementation_insertion_offset,
        &candidate.implementation_header,
        &candidate.owner,
        &candidate.method,
    )?;
    edits.push(implementation);
    let updated = apply_text_edits(source, &edits)?;
    if !validate_interface_method_generation(uri, &updated, candidate, &edits) {
        return None;
    }
    // LSP clients interpret all ranges against the original document. Keep
    // the order deterministic, including when a future renderer introduces
    // same-offset insertions.
    edits.sort_by(|left, right| {
        left.range
            .start
            .cmp(&right.range.start)
            .then_with(|| left.range.end.cmp(&right.range.end))
            .then_with(|| left.new_text.cmp(&right.new_text))
    });
    Some(edits)
}

fn source_line_ending(source: &str) -> &'static str {
    if source.contains("\r\n") {
        "\r\n"
    } else if source.contains('\n') {
        "\n"
    } else if source.contains('\r') {
        "\r"
    } else {
        "\n"
    }
}

fn normalize_line_endings(text: &str, line_ending: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', line_ending)
}

fn apply_text_edits(source: &str, edits: &[TextEdit]) -> Option<String> {
    let mut byte_edits = Vec::with_capacity(edits.len());
    for edit in edits {
        let start = text::position_to_offset(source, edit.range.start)?;
        let end = text::position_to_offset(source, edit.range.end)?;
        if start > end {
            return None;
        }
        byte_edits.push((start, end, edit.new_text.as_str()));
    }
    byte_edits.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    for pair in byte_edits.windows(2) {
        if pair[0].1 > pair[1].0 {
            return None;
        }
    }
    let mut updated = source.to_owned();
    byte_edits.sort_by(|left, right| right.0.cmp(&left.0).then(right.1.cmp(&left.1)));
    for (start, end, text) in byte_edits {
        updated.replace_range(start..end, text);
    }
    Some(updated)
}

fn validate_interface_method_generation(
    uri: &Url,
    updated_source: &str,
    candidate: &MissingInterfaceMethodImplementationCandidate,
    edits: &[TextEdit],
) -> bool {
    let Ok(path) = uri.to_file_path() else {
        return false;
    };
    let Ok((tree, _)) = parser::parse_file(&FileInfo::new(path), updated_source.as_bytes()) else {
        return false;
    };
    if tree.root_node().has_error() {
        return false;
    }
    let implementation_header = normalize_line_endings(
        &candidate.implementation_header,
        source_line_ending(updated_source),
    );
    if !has_one_generated_interface_definition(
        tree.root_node(),
        updated_source,
        &implementation_header,
    ) {
        return false;
    }
    if let Some(declaration) = candidate.declaration_header.as_deref() {
        let declaration = normalize_line_endings(declaration, source_line_ending(updated_source));
        if !has_interface_declaration(updated_source, &declaration) {
            return false;
        }
    }
    let todo_text = format!(
        "// TODO: Implement {}.{}.",
        candidate.owner, candidate.method
    );
    let Some(todo) = updated_source.find(&todo_text) else {
        return false;
    };
    let Some(body_end) = updated_source[todo..].find("end;") else {
        return false;
    };
    !edits.is_empty() && todo.saturating_add(body_end) > todo
}

#[allow(clippy::too_many_arguments)]
fn interface_post_edit_proves_obligation(
    input: &WorkspaceInput,
    uri: &Url,
    source: &str,
    target_record: &SourceRecord,
    configuration_records: &[SourceRecord],
    candidate: &MissingInterfaceMethodImplementationCandidate,
    edits: &[TextEdit],
    cancel: &AtomicBool,
) -> Result<bool, String> {
    let Some(updated_source) = apply_text_edits(source, edits) else {
        return Ok(false);
    };
    let Some(identifier) = identifier_at_position(&updated_source, candidate.anchor.start) else {
        return Ok(false);
    };

    let mut proof_input = input.clone();
    proof_input.overlays.insert(
        uri.clone(),
        OverlayInput {
            text: updated_source.clone(),
            version: target_record.version.unwrap_or_default(),
        },
    );
    let mut proof_record = target_record.clone();
    proof_record.text = updated_source.clone();
    proof_record.parsed_text_hash = Some(text_content_hash(&updated_source));
    proof_record.content_hash = None;
    let snapshot = match build_snapshot(
        &proof_input,
        std::slice::from_ref(uri),
        std::slice::from_ref(&identifier),
        SnapshotMode::Workspace,
        Some(SnapshotSeed::new(proof_record).with_consumed_configuration(configuration_records)),
        &[],
        cancel,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
        Err(_) => return Ok(false),
    };
    if !snapshot.complete
        || !snapshot.include_errors.is_empty()
        || !snapshot.editable.contains(uri)
        || snapshot
            .sources
            .get(uri)
            .is_none_or(|text| text != &updated_source)
        || snapshot
            .index
            .source_text(uri)
            .is_none_or(|text| text != updated_source)
    {
        return Ok(false);
    }

    let mut budget = AssistanceBudget::new(
        MAX_MISSING_UNIT_REQUEST_WORK,
        MAX_MISSING_UNIT_REQUEST_BYTES,
        "interface method post-edit validation",
    );
    let remaining = snapshot
        .index
        .missing_interface_method_implementation_candidates_with_budget(
            uri,
            candidate.anchor.start,
            cancel,
            &mut budget,
        )?;
    let exact_obligation_remains = remaining.iter().any(|remaining| {
        remaining.obligation_identity == candidate.obligation_identity
            && remaining.interface_uri == candidate.interface_uri
            && remaining.interface_owner == candidate.interface_owner
            && remaining.interface_method == candidate.interface_method
    });
    if exact_obligation_remains {
        return Ok(false);
    }

    let diagnostics = match snapshot.index.semantic_diagnostics_with_cancel(uri, cancel) {
        Ok(diagnostics) => diagnostics,
        Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
        Err(_) => return Ok(false),
    };
    let Some(anchor_start) = text::position_to_offset(&updated_source, candidate.anchor.start)
    else {
        return Ok(false);
    };
    let same_named_obligation_remains = remaining.iter().any(|remaining| {
        remaining.interface_uri == candidate.interface_uri
            && remaining.interface_owner == candidate.interface_owner
            && remaining.interface_method == candidate.interface_method
    });
    if diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == SemanticDiagnosticKind::MissingInterfaceImplementation
            && diagnostic.span.start == anchor_start
            && diagnostic.message.contains(&candidate.interface_method)
            && !same_named_obligation_remains
    }) {
        return Ok(false);
    }
    Ok(true)
}

fn has_one_generated_interface_definition(
    root: Node<'_>,
    source: &str,
    expected_header: &str,
) -> bool {
    let mut definitions = 0usize;
    walk(root, &mut |node| {
        if node.kind() != "defProc" {
            return;
        }
        let Some(header) = node.child_by_field_name("header") else {
            return;
        };
        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        if source
            .get(header.start_byte()..header.end_byte())
            .is_some_and(|text| text == expected_header)
            && body.start_byte() > header.end_byte()
        {
            definitions = definitions.saturating_add(1);
        }
    });
    definitions == 1
}

fn has_interface_declaration(source: &str, expected_header: &str) -> bool {
    source.contains(expected_header)
}

fn matching_missing_interface_diagnostic(
    candidate: &MissingInterfaceMethodImplementationCandidate,
    context: &CodeActionContext,
) -> Option<Diagnostic> {
    context
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic_code(diagnostic) == Some("pascal-missing-interface-implementation")
                && diagnostic.range == candidate.anchor
        })
        .cloned()
}

#[allow(clippy::too_many_arguments)]
fn missing_unit_binding_proof(
    input: &WorkspaceInput,
    uri: &Url,
    target_record: &SourceRecord,
    configuration_records: &[SourceRecord],
    updated_source: String,
    updated_position: lsp_types::Position,
    identifier: &str,
    unit_name: &str,
    provider_uri: &Url,
    symbol_name: &str,
    use_kind: MissingUnitUseKind,
    provider_source_hash: u64,
    provider_declaration_fingerprint: u64,
    provider_context_fingerprint: u64,
    use_context_fingerprint: u64,
    cancel: &AtomicBool,
    budget: &mut AssistanceBudget,
) -> Result<bool, String> {
    let mut proof_input = input.clone();
    proof_input.overlays.insert(
        uri.clone(),
        OverlayInput {
            text: updated_source.clone(),
            version: target_record.version.unwrap_or_default(),
        },
    );
    let mut proof_record = target_record.clone();
    proof_record.text = updated_source.clone();
    proof_record.parsed_text_hash = Some(text_content_hash(&updated_source));
    proof_record.content_hash = None;
    let snapshot = build_snapshot(
        &proof_input,
        std::slice::from_ref(uri),
        std::slice::from_ref(&identifier.to_string()),
        SnapshotMode::Assistance,
        Some(
            SnapshotSeed::new(proof_record)
                .with_consumed_configuration(configuration_records)
                .with_completion_position(Some(updated_position)),
        ),
        &[],
        cancel,
    )?;
    if !snapshot.complete
        || !snapshot.include_errors.is_empty()
        || snapshot
            .sources
            .get(uri)
            .is_none_or(|source| source != &updated_source)
        || snapshot
            .index
            .source_text(provider_uri)
            .is_none_or(|source| source_hash(source) != provider_source_hash)
    {
        return Ok(false);
    }
    snapshot.index.missing_unit_binds_after_import_with_cancel(
        uri,
        updated_position,
        unit_name,
        provider_uri,
        symbol_name,
        use_kind,
        provider_declaration_fingerprint,
        provider_context_fingerprint,
        use_context_fingerprint,
        cancel,
        budget,
    )
}

fn missing_unit_candidate_identity_is_bounded(
    candidate: &MissingUnitCandidate,
    target_uri: &Url,
    identifier: &str,
) -> bool {
    !identifier.is_empty()
        && identifier.len() <= MAX_ACTION_NAME_BYTES
        && !candidate.unit_name.is_empty()
        && candidate.unit_name.len() <= MAX_ACTION_UNIT_BYTES
        && !candidate.symbol_name.is_empty()
        && candidate.symbol_name.len() <= MAX_ACTION_NAME_BYTES
        && target_uri.as_str().len() <= MAX_ACTION_URI_BYTES
        && candidate.provider_uri.as_str().len() <= MAX_ACTION_URI_BYTES
}

fn push_bounded_code_action(
    actions: &mut Vec<CodeActionOrCommand>,
    action: CodeActionOrCommand,
) -> Result<bool, String> {
    let mut candidate = actions.clone();
    candidate.push(action.clone());
    let encoded = serde_json::to_vec(&candidate)
        .map_err(|error| format!("could not serialize bounded code actions: {error}"))?;
    if encoded.len() > MAX_MISSING_UNIT_RESPONSE_BYTES {
        return Ok(false);
    }
    actions.push(action);
    Ok(true)
}

fn ensure_bounded_resolved_action(action: &CodeAction) -> Result<(), String> {
    let encoded = serde_json::to_vec(&[CodeActionOrCommand::CodeAction(action.clone())])
        .map_err(|error| format!("could not serialize resolved code action: {error}"))?;
    if encoded.len() > MAX_MISSING_UNIT_RESPONSE_BYTES {
        return Err(format!(
            "resolved code action exceeds the {MAX_MISSING_UNIT_RESPONSE_BYTES}-byte response limit"
        ));
    }
    Ok(())
}

fn matching_missing_unit_diagnostic(
    candidate: &MissingUnitCandidate,
    context: &CodeActionContext,
) -> Option<Diagnostic> {
    context
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic_code(diagnostic) == Some("pascal-unresolved-identifier")
                && diagnostic.range == candidate.anchor
        })
        .cloned()
}

fn apply_text_edit(source: &str, edit: &TextEdit) -> Option<String> {
    let start = text::position_to_offset(source, edit.range.start)?;
    let end = text::position_to_offset(source, edit.range.end)?;
    if start > end {
        return None;
    }
    let mut updated = source.to_owned();
    updated.replace_range(start..end, &edit.new_text);
    Some(updated)
}

fn position_after_text_edit(
    source: &str,
    position: lsp_types::Position,
    edit: &TextEdit,
    updated_source: &str,
) -> Option<lsp_types::Position> {
    let original_offset = text::position_to_offset(source, position)?;
    let start = text::position_to_offset(source, edit.range.start)?;
    let end = text::position_to_offset(source, edit.range.end)?;
    let offset = if original_offset >= end {
        start
            .saturating_add(edit.new_text.len())
            .saturating_add(original_offset.saturating_sub(end))
    } else if original_offset >= start {
        start.saturating_add(edit.new_text.len())
    } else {
        original_offset
    };
    text::offset_to_position(updated_source, offset)
}

pub(crate) fn resolve_from_input(
    input: WorkspaceInput,
    action: CodeAction,
    features: ClientActionFeatures,
    cancel: &AtomicBool,
) -> Computed<CodeAction> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    let data = match parse_action_data(action.data.as_ref()) {
        Ok(data) => data,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let data = match data {
        ParsedActionData::MissingUnit(data) => {
            return resolve_missing_unit_from_input(input, action, data, features, cancel);
        }
        ParsedActionData::MethodImplementation(data) => {
            return resolve_method_implementation_from_input(input, action, data, features, cancel);
        }
        ParsedActionData::InterfaceMethodImplementation(data) => {
            return resolve_interface_method_implementation_from_input(
                input, action, data, features, cancel,
            );
        }
        ParsedActionData::Rename(data) => data,
    };
    if let Err(error) =
        validate_rename_action_data(&data, &action, source_generation, configuration_generation)
    {
        return failed(source_generation, configuration_generation, error);
    }
    let target_uri = canonical_file_uri(&data.uri);
    let (target_source, target_record) =
        match source_for_input_with_cancel(&input, &target_uri, Some(cancel)) {
            Ok(source) => source,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if source_hash(&target_source) != data.source_hash {
        return failed(
            source_generation,
            configuration_generation,
            "code action source changed; request code actions again".to_string(),
        );
    }
    let Some(old_name) = identifier_at_position(&target_source, data.anchor.start) else {
        return failed(
            source_generation,
            configuration_generation,
            "code-action source no longer contains its declaration identifier".to_string(),
        );
    };
    let candidate_names = vec![old_name, data.new_name.clone()];
    let mode = if data.rule == LOCAL_RULE {
        SnapshotMode::Local
    } else {
        SnapshotMode::Workspace
    };
    let (config, current_fingerprint, excluded, configuration_records) =
        match lint_configuration_for_input(&input, &target_uri) {
            Ok(config) => config,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if excluded {
        return failed(
            source_generation,
            configuration_generation,
            "code action source is excluded by lint configuration".to_string(),
        );
    }
    if current_fingerprint != data.config_fingerprint {
        return failed(
            source_generation,
            configuration_generation,
            "code action configuration is stale; request code actions again".to_string(),
        );
    }
    let snapshot = match build_snapshot(
        &input,
        std::slice::from_ref(&target_uri),
        &candidate_names,
        mode,
        Some(SnapshotSeed::new(target_record).with_consumed_configuration(&configuration_records)),
        &[],
        cancel,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let Some(source) = snapshot.sources.get(&target_uri) else {
        return failed(
            source_generation,
            configuration_generation,
            format!(
                "code-action source disappeared from the workspace snapshot: {}",
                target_uri
            ),
        );
    };
    if !snapshot.editable.contains(&target_uri) {
        return failed(
            source_generation,
            configuration_generation,
            format!(
                "code-action source is outside configured workspace roots: {}",
                target_uri
            ),
        );
    }
    let context = CodeActionContext {
        diagnostics: action.diagnostics.clone().unwrap_or_default(),
        only: Some(vec![CodeActionKind::QUICKFIX]),
        trigger_kind: None,
    };
    let candidates = match naming_candidates(
        &target_uri,
        source,
        data.anchor,
        &context,
        &config,
        current_fingerprint,
    ) {
        Ok(candidates) => candidates,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let Some(candidate) = candidates.into_iter().find(|candidate| {
        candidate.rule == data.rule
            && candidate.anchor == data.anchor
            && candidate.new_name == data.new_name
            && candidate.config_fingerprint == data.config_fingerprint
            && candidate.source_hash == data.source_hash
    }) else {
        return failed(
            source_generation,
            configuration_generation,
            "code action declaration or rule is stale; request code actions again".to_string(),
        );
    };
    if action_title(&candidate) != action.title
        || action.kind.as_ref() != Some(&CodeActionKind::QUICKFIX)
    {
        return failed(
            source_generation,
            configuration_generation,
            "code action identity was modified by the client".to_string(),
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    if !snapshot.complete {
        let reason = snapshot
            .incomplete_reason
            .clone()
            .unwrap_or_else(|| "bounded workspace discovery did not finish".to_string());
        if !features.disabled {
            return failed(
                source_generation,
                configuration_generation,
                format!("workspace scan incomplete: {reason}"),
            );
        }
        let mut disabled_action = action;
        disabled_action.disabled = Some(CodeActionDisabled {
            reason: format!("workspace scan incomplete: {reason}"),
        });
        return Computed {
            source_generation,
            configuration_generation,
            value: Ok(disabled_action),
            records: {
                let mut records = snapshot_records(&snapshot);
                append_records(&mut records, configuration_records);
                records
            },
        };
    }

    match plan_candidate(
        &snapshot,
        &candidate,
        features.document_changes,
        Some(cancel),
    ) {
        Ok((edit, _records)) => {
            let mut resolved = action;
            resolved.edit = Some(edit);
            resolved.disabled = None;
            if let Err(error) = ensure_bounded_resolved_action(&resolved) {
                return failed(source_generation, configuration_generation, error);
            }
            Computed {
                source_generation,
                configuration_generation,
                value: Ok(resolved),
                records: {
                    let mut records = snapshot_records(&snapshot);
                    append_records(&mut records, configuration_records);
                    records
                },
            }
        }
        Err(error) if features.disabled => {
            let mut disabled_action = action;
            disabled_action.disabled = Some(CodeActionDisabled { reason: error });
            Computed {
                source_generation,
                configuration_generation,
                value: Ok(disabled_action),
                records: {
                    let mut records = snapshot_records(&snapshot);
                    append_records(&mut records, configuration_records);
                    records
                },
            }
        }
        Err(error) => failed(source_generation, configuration_generation, error),
    }
}

fn resolve_missing_unit_from_input(
    input: WorkspaceInput,
    action: CodeAction,
    data: MissingUnitActionData,
    features: ClientActionFeatures,
    cancel: &AtomicBool,
) -> Computed<CodeAction> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    if let Err(error) = validate_missing_unit_action_data(&data, &action) {
        return failed(source_generation, configuration_generation, error);
    }

    let target_uri = canonical_file_uri(&data.uri);
    let (target_source, target_record) =
        match source_for_input_with_cancel(&input, &target_uri, Some(cancel)) {
            Ok(source) => source,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if !input_source_is_editable(&input, &target_uri) {
        return failed(
            source_generation,
            configuration_generation,
            format!("code-action document is outside configured workspace roots: {target_uri}"),
        );
    }
    if source_hash(&target_source) != data.source_hash {
        return failed(
            source_generation,
            configuration_generation,
            "code action source changed; request code actions again".to_string(),
        );
    }
    if identifier_at_position(&target_source, data.anchor.start)
        .is_none_or(|identifier| identifier != data.identifier)
    {
        return failed(
            source_generation,
            configuration_generation,
            "missing-unit identifier changed; request code actions again".to_string(),
        );
    }

    let (_config, config_fingerprint, excluded, configuration_records) =
        match lint_configuration_for_input(&input, &target_uri) {
            Ok(config) => config,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if excluded {
        return failed(
            source_generation,
            configuration_generation,
            "code action source is excluded by lint configuration".to_string(),
        );
    }
    if config_fingerprint != data.config_fingerprint {
        return failed(
            source_generation,
            configuration_generation,
            "code action configuration is stale; request code actions again".to_string(),
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let params = CodeActionParams {
        text_document: lsp_types::TextDocumentIdentifier {
            uri: target_uri.clone(),
        },
        range: data.anchor,
        context: CodeActionContext {
            diagnostics: action.diagnostics.clone().unwrap_or_default(),
            only: Some(vec![CodeActionKind::QUICKFIX]),
            trigger_kind: None,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let (plans, missing_records) = match missing_unit_plans_from_input(
        &input,
        &target_uri,
        &target_source,
        &target_record,
        &params,
        config_fingerprint,
        &configuration_records,
        cancel,
    ) {
        Ok(result) => result,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let Some(plan) = plans.into_iter().find(|plan| {
        plan.uri == target_uri
            && plan.position == data.anchor.start
            && plan.identifier == data.identifier
            && plan.candidate.anchor == data.anchor
            && plan.candidate.unit_name == data.unit_name
            && plan.candidate.provider_uri == data.provider_uri
            && plan.candidate.symbol_name == data.provider_symbol
            && plan.candidate.use_kind == data.use_kind
            && plan.candidate.provider_source_hash == data.provider_source_hash
            && plan.candidate.provider_declaration_fingerprint
                == data.provider_declaration_fingerprint
            && plan.candidate.provider_context_fingerprint == data.provider_context_fingerprint
            && plan.candidate.use_context_fingerprint == data.use_context_fingerprint
            && plan.config_fingerprint == data.config_fingerprint
            && plan.source_hash == data.source_hash
    }) else {
        return failed(
            source_generation,
            configuration_generation,
            "missing-unit provider or binding proof is stale; request code actions again"
                .to_string(),
        );
    };

    let mut raw_edits = std::collections::HashMap::new();
    raw_edits.insert(target_uri.clone(), vec![plan.edit]);
    let mut records_by_uri = std::collections::HashMap::new();
    records_by_uri.insert(target_uri, target_record);
    let edit = match workspace_edit(raw_edits, &records_by_uri, features.document_changes) {
        Ok(edit) => edit,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let mut resolved = action;
    resolved.edit = Some(edit);
    resolved.disabled = None;
    if let Err(error) = ensure_bounded_resolved_action(&resolved) {
        return failed(source_generation, configuration_generation, error);
    }
    let mut records = missing_records;
    append_records(&mut records, configuration_records);
    Computed {
        source_generation,
        configuration_generation,
        value: Ok(resolved),
        records,
    }
}

fn resolve_method_implementation_from_input(
    input: WorkspaceInput,
    action: CodeAction,
    data: MethodImplementationActionData,
    features: ClientActionFeatures,
    cancel: &AtomicBool,
) -> Computed<CodeAction> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    if let Err(error) = validate_method_implementation_action_data(
        &data,
        &action,
        source_generation,
        configuration_generation,
    ) {
        return failed(source_generation, configuration_generation, error);
    }

    let target_uri = canonical_file_uri(&data.uri);
    let (target_source, target_record) =
        match source_for_input_with_cancel(&input, &target_uri, Some(cancel)) {
            Ok(source) => source,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if !input_source_is_editable(&input, &target_uri) {
        return failed(
            source_generation,
            configuration_generation,
            format!("code-action document is outside configured workspace roots: {target_uri}"),
        );
    }
    if source_hash(&target_source) != data.source_hash {
        return failed(
            source_generation,
            configuration_generation,
            "code action source changed; request code actions again".to_string(),
        );
    }
    if identifier_at_position(&target_source, data.anchor.start)
        .is_none_or(|identifier| identifier != data.method)
    {
        return failed(
            source_generation,
            configuration_generation,
            "method declaration changed; request code actions again".to_string(),
        );
    }

    let (_config, config_fingerprint, excluded, configuration_records) =
        match lint_configuration_for_input(&input, &target_uri) {
            Ok(config) => config,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if excluded {
        return failed(
            source_generation,
            configuration_generation,
            "code action source is excluded by lint configuration".to_string(),
        );
    }
    if config_fingerprint != data.config_fingerprint {
        return failed(
            source_generation,
            configuration_generation,
            "code action configuration is stale; request code actions again".to_string(),
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let params = CodeActionParams {
        text_document: lsp_types::TextDocumentIdentifier {
            uri: target_uri.clone(),
        },
        range: data.anchor,
        context: CodeActionContext {
            diagnostics: action.diagnostics.clone().unwrap_or_default(),
            only: Some(vec![CodeActionKind::QUICKFIX]),
            trigger_kind: None,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let (plans, method_records) = match method_implementation_plans_from_input(
        &input,
        &target_uri,
        &target_source,
        &target_record,
        &params,
        config_fingerprint,
        &configuration_records,
        cancel,
    ) {
        Ok(result) => result,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let Some(plan) = plans.into_iter().find(|plan| {
        plan.uri == target_uri
            && plan.candidate.anchor == data.anchor
            && plan.candidate.declaration == data.declaration
            && plan.candidate.owner == data.owner
            && plan.candidate.method == data.method
            && plan.candidate.unit_name == data.unit_name
            && plan.candidate.identity == data.identity
            && plan.config_fingerprint == data.config_fingerprint
            && plan.source_hash == data.source_hash
    }) else {
        return failed(
            source_generation,
            configuration_generation,
            "method declaration, owner, or implementation proof is stale; request code actions again"
                .to_string(),
        );
    };

    let mut raw_edits = std::collections::HashMap::new();
    raw_edits.insert(target_uri.clone(), vec![plan.edit]);
    let mut records_by_uri = std::collections::HashMap::new();
    records_by_uri.insert(target_uri, target_record);
    let edit = match workspace_edit(raw_edits, &records_by_uri, features.document_changes) {
        Ok(edit) => edit,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let mut resolved = action;
    resolved.edit = Some(edit);
    resolved.disabled = None;
    if let Err(error) = ensure_bounded_resolved_action(&resolved) {
        return failed(source_generation, configuration_generation, error);
    }
    let mut records = method_records;
    append_records(&mut records, configuration_records);
    Computed {
        source_generation,
        configuration_generation,
        value: Ok(resolved),
        records,
    }
}

fn resolve_interface_method_implementation_from_input(
    input: WorkspaceInput,
    action: CodeAction,
    data: InterfaceMethodImplementationActionData,
    features: ClientActionFeatures,
    cancel: &AtomicBool,
) -> Computed<CodeAction> {
    let source_generation = input.source_generation;
    let configuration_generation = input.configuration_generation;
    if let Err(error) = validate_interface_method_implementation_action_data(
        &data,
        &action,
        source_generation,
        configuration_generation,
    ) {
        return failed(source_generation, configuration_generation, error);
    }

    let target_uri = canonical_file_uri(&data.uri);
    let (target_source, target_record) =
        match source_for_input_with_cancel(&input, &target_uri, Some(cancel)) {
            Ok(source) => source,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if !input_source_is_editable(&input, &target_uri) {
        return failed(
            source_generation,
            configuration_generation,
            format!("code-action document is outside configured workspace roots: {target_uri}"),
        );
    }
    if source_hash(&target_source) != data.source_hash {
        return failed(
            source_generation,
            configuration_generation,
            "code action source changed; request code actions again".to_string(),
        );
    }
    if identifier_at_position(&target_source, data.anchor.start)
        .is_none_or(|identifier| identifier != data.owner.rsplit('<').next().unwrap_or(&data.owner))
    {
        return failed(
            source_generation,
            configuration_generation,
            "interface class declaration changed; request code actions again".to_string(),
        );
    }

    let (_config, config_fingerprint, excluded, configuration_records) =
        match lint_configuration_for_input(&input, &target_uri) {
            Ok(config) => config,
            Err(error) => return failed(source_generation, configuration_generation, error),
        };
    if excluded {
        return failed(
            source_generation,
            configuration_generation,
            "code action source is excluded by lint configuration".to_string(),
        );
    }
    if config_fingerprint != data.config_fingerprint {
        return failed(
            source_generation,
            configuration_generation,
            "code action configuration is stale; request code actions again".to_string(),
        );
    }
    if is_cancelled(cancel) {
        return cancelled(source_generation, configuration_generation);
    }

    let params = CodeActionParams {
        text_document: lsp_types::TextDocumentIdentifier {
            uri: target_uri.clone(),
        },
        range: data.anchor,
        context: CodeActionContext {
            diagnostics: action.diagnostics.clone().unwrap_or_default(),
            only: Some(vec![CodeActionKind::QUICKFIX]),
            trigger_kind: None,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let (plans, interface_records) = match interface_method_implementation_plans_from_input(
        &input,
        &target_uri,
        &target_source,
        &target_record,
        &params,
        config_fingerprint,
        &configuration_records,
        cancel,
    ) {
        Ok(result) => result,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let Some(plan) = plans.into_iter().find(|plan| {
        plan.uri == target_uri
            && plan.candidate.anchor == data.anchor
            && plan.candidate.class_declaration == data.class_declaration
            && plan.candidate.owner == data.owner
            && plan.candidate.method == data.method
            && plan.candidate.interface_uri == data.interface_uri
            && plan.candidate.interface_owner == data.interface_owner
            && plan.candidate.interface_method == data.interface_method
            && plan.candidate.unit_name == data.unit_name
            && plan.candidate.identity == data.identity
            && plan.config_fingerprint == data.config_fingerprint
            && plan.source_hash == data.source_hash
    }) else {
        return failed(
            source_generation,
            configuration_generation,
            "interface obligation or implementation proof is stale; request code actions again"
                .to_string(),
        );
    };

    let mut raw_edits = std::collections::HashMap::new();
    raw_edits.insert(target_uri.clone(), plan.edits);
    let mut records_by_uri = std::collections::HashMap::new();
    records_by_uri.insert(target_uri, target_record);
    let edit = match workspace_edit(raw_edits, &records_by_uri, features.document_changes) {
        Ok(edit) => edit,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    let mut resolved = action;
    resolved.edit = Some(edit);
    resolved.disabled = None;
    if let Err(error) = ensure_bounded_resolved_action(&resolved) {
        return failed(source_generation, configuration_generation, error);
    }
    let mut records = interface_records;
    append_records(&mut records, configuration_records);
    Computed {
        source_generation,
        configuration_generation,
        value: Ok(resolved),
        records,
    }
}

fn failed<T>(source_generation: u64, configuration_generation: u64, error: String) -> Computed<T> {
    Computed {
        source_generation,
        configuration_generation,
        value: Err(error),
        records: Vec::new(),
    }
}

fn cancelled<T>(source_generation: u64, configuration_generation: u64) -> Computed<T> {
    failed(
        source_generation,
        configuration_generation,
        CANCELLATION_MESSAGE.to_string(),
    )
}

fn append_records(records: &mut Vec<SourceRecord>, additional: Vec<SourceRecord>) {
    for record in additional {
        let duplicate = record.path.as_ref().is_some_and(|path| {
            if is_configuration_file(path) {
                return false;
            }
            records
                .iter()
                .any(|existing| same_record_observation(existing, &record))
        });
        if !duplicate {
            records.push(record);
        }
    }
}

fn same_record_observation(left: &SourceRecord, right: &SourceRecord) -> bool {
    left.uri == right.uri
        && left.text == right.text
        && left.version == right.version
        && left.stamp == right.stamp
        && left.open == right.open
        && left.path == right.path
        && left.path_stamp == right.path_stamp
        && left.content_hash == right.content_hash
        && left.content_bytes == right.content_bytes
        && left.directory_observation == right.directory_observation
}

fn set_disabled_or_skip(action: &mut CodeAction, supported: bool, reason: String) -> bool {
    if !supported {
        return false;
    }
    action.disabled = Some(CodeActionDisabled { reason });
    true
}

#[cfg(test)]
thread_local! {
    static AFTER_LINT_CONFIGURATION_HOOK:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
}

#[cfg(test)]
fn set_after_lint_configuration_hook(hook: impl FnOnce() + 'static) {
    AFTER_LINT_CONFIGURATION_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn run_after_lint_configuration_hook() {
    let hook = AFTER_LINT_CONFIGURATION_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(not(test))]
fn run_after_lint_configuration_hook() {}

fn requests_quickfix(context: &CodeActionContext) -> bool {
    context.only.as_ref().is_none_or(|kinds| {
        kinds
            .iter()
            .any(|kind| code_action_kind_contains(kind, &CodeActionKind::QUICKFIX))
    })
}

fn requests_interface_method(context: &CodeActionContext) -> bool {
    context.only.as_ref().is_none_or(|kinds| {
        kinds.iter().any(|kind| {
            code_action_kind_contains(kind, &INTERFACE_METHOD_IMPLEMENTATION_CODE_ACTION_KIND)
        })
    })
}

/// LSP code-action kinds form a dot-separated hierarchy.  An empty filter is
/// the root, while a filter matches its exact kind and all descendants.
fn code_action_kind_contains(filter: &CodeActionKind, action: &CodeActionKind) -> bool {
    filter.as_str().is_empty()
        || filter == action
        || action.as_str().starts_with(filter.as_str())
            && action.as_str().as_bytes().get(filter.as_str().len()) == Some(&b'.')
}

fn plan_candidate(
    snapshot: &RenameSnapshot,
    candidate: &NamingCandidate,
    document_changes: bool,
    cancel: Option<&AtomicBool>,
) -> Result<(WorkspaceEdit, Vec<super::rename::SourceRecord>), String> {
    let source = snapshot.sources.get(&candidate.uri).ok_or_else(|| {
        format!(
            "code-action source was not retained in the workspace snapshot: {}",
            candidate.uri
        )
    })?;
    if source_hash(source) != candidate.source_hash {
        return Err("code action source changed; request code actions again".to_string());
    }
    let position = candidate.anchor.start;
    let raw = snapshot
        .index
        .rename_edits(&candidate.uri, position, &candidate.new_name)?;
    let candidate_names = [candidate.old_name.clone(), candidate.new_name.clone()];
    check_includes(snapshot, &candidate.uri, position, &candidate_names, cancel)?;
    for uri in raw.keys() {
        if !snapshot.editable.contains(uri) {
            return Err(format!(
                "rename would modify source outside configured workspace roots: {uri}"
            ));
        }
        if !snapshot.records.contains_key(uri) {
            return Err(format!(
                "rename source was not retained in the complete workspace snapshot: {uri}"
            ));
        }
    }
    let records = raw
        .keys()
        .filter_map(|uri| snapshot.records.get(uri).cloned())
        .collect::<Vec<_>>();
    let edit = workspace_edit(raw, &snapshot.records, document_changes)?;
    Ok((edit, records))
}

fn naming_candidates(
    uri: &Url,
    source: &str,
    requested_range: Range,
    context: &CodeActionContext,
    config: &Config,
    config_fingerprint: u64,
) -> Result<Vec<NamingCandidate>, String> {
    let path = uri
        .to_file_path()
        .map(absolute_path)
        .map_err(|_| format!("not a file URI: {uri}"))?;
    let (tree, _) = parser::parse_file(&FileInfo::new(path), source.as_bytes())
        .map_err(|error| format!("could not parse code-action source: {error}"))?;
    let suppressions = parse_suppressions(source.as_bytes());
    let request = CandidateRequest {
        uri,
        source,
        config_fingerprint,
        suppressions: &suppressions,
        requested_range,
        context,
    };
    let mut candidates = Vec::new();
    if !rule_is_off(config, CONSTANT_RULE) {
        collect_constants(
            tree.root_node(),
            config.constant_style(),
            &request,
            &mut candidates,
        );
    }
    if !rule_is_off(config, LOCAL_RULE) {
        collect_locals(
            tree.root_node(),
            config.local_variable_style(),
            &request,
            &mut candidates,
        );
    }
    candidates.sort_by(|left, right| {
        left.anchor
            .start
            .cmp(&right.anchor.start)
            .then_with(|| left.rule.cmp(&right.rule))
    });
    candidates.dedup_by(|left, right| left.rule == right.rule && left.anchor == right.anchor);
    Ok(candidates)
}

fn lint_configuration_for_input(
    input: &WorkspaceInput,
    uri: &Url,
) -> Result<(Config, u64, bool, Vec<SourceRecord>), String> {
    let path = uri
        .to_file_path()
        .map(absolute_path)
        .map_err(|_| format!("not a file URI: {uri}"))?;
    let mut workspace = super::Workspace::with_override_session(
        input.roots.clone(),
        input.options.clone(),
        input.overrides.clone(),
    );
    workspace.project_selections = input.project_selections.clone();
    workspace.document_owners = input.document_owners.clone();
    let context_key = workspace.context_for_uri(uri)?;
    let context = workspace
        .contexts
        .get(&context_key)
        .map(|state| state.context.clone())
        .ok_or_else(|| format!("project context was not retained for {uri}"))?;
    if has_invalid_project_selection(&context) {
        return Err(
            "project selection is invalid; select a current project or Automatic".to_string(),
        );
    }
    let candidates = project_candidates(&path, &input.roots)?;
    let project_directory =
        workspace.configuration_project_directory(&path, &context, Some(&context_key), &candidates);
    let directories = config_directories(&path, project_directory.as_deref(), &input.roots)?;
    let resolved = resolve_lint(&directories, 4 * 1024 * 1024)?;
    run_after_lint_configuration_hook();
    let mut hasher = DefaultHasher::new();
    resolved.path.hash(&mut hasher);
    resolved.checked_paths.hash(&mut hasher);
    resolved.checked_contents.hash(&mut hasher);
    resolved.bytes.hash(&mut hasher);
    if resolved.bytes.is_none() {
        format!("{:?}", resolved.value).hash(&mut hasher);
    }
    let fingerprint = hasher.finish();
    let excluded = is_lint_excluded(&path, resolved.path.as_deref(), &resolved.value.exclude);
    let records = resolved
        .checked_paths
        .iter()
        .zip(resolved.checked_contents.iter())
        .filter_map(|(path, content_bytes)| {
            Some(SourceRecord {
                uri: Url::from_file_path(path).ok()?,
                text: String::new(),
                version: None,
                stamp: None,
                open: false,
                path: Some(path.clone()),
                path_stamp: content_bytes.as_ref().and_then(|_| path_stamp(path)),
                content_hash: None,
                parsed_text_hash: None,
                content_bytes: content_bytes.clone(),
                candidate_membership: None,
                candidate_observations: Vec::new(),
                read_policy: Some(context.read_policy.clone()),
                path_entry: super::context_path_entry(&context, path),
                include_payload: false,
                missing_provider_candidate: false,
                directory_observation: false,
                missing_provider_scope: None,
                auto_import_provider_observation: false,
                auto_import_scopes: Vec::new(),
            })
        })
        .collect();
    Ok((resolved.value, fingerprint, excluded, records))
}

fn rule_is_off(config: &Config, rule: &str) -> bool {
    matches!(config.rule_severity(rule), Some(RuleSeverityOverride::Off))
}

fn collect_constants(
    root: Node<'_>,
    style: &str,
    request: &CandidateRequest<'_>,
    candidates: &mut Vec<NamingCandidate>,
) {
    walk(root, &mut |node| {
        if node.kind() != "declConst" || node.child_by_field_name("type").is_some() {
            return;
        }
        for name_node in field_identifier_nodes(node, "name") {
            add_candidate(CONSTANT_RULE, name_node, style, request, candidates);
        }
    });
}

fn collect_locals(
    root: Node<'_>,
    style: &str,
    request: &CandidateRequest<'_>,
    candidates: &mut Vec<NamingCandidate>,
) {
    walk(root, &mut |node| {
        if !matches!(node.kind(), "defProc" | "lambda") {
            return;
        }
        if let Some(header) = node.child_by_field_name("header") {
            for child in effective_children(header) {
                if child.kind() != "declArgs" {
                    continue;
                }
                for argument in effective_children(child) {
                    if argument.kind() != "declArg" {
                        continue;
                    }
                    for name_node in field_identifier_nodes(argument, "name") {
                        add_candidate(LOCAL_RULE, name_node, style, request, candidates);
                    }
                }
            }
        }
        for child in effective_children(node) {
            if child.kind() != "declVars" {
                continue;
            }
            for declaration in effective_children(child) {
                if declaration.kind() != "declVar" {
                    continue;
                }
                for name_node in field_identifier_nodes(declaration, "name") {
                    add_candidate(LOCAL_RULE, name_node, style, request, candidates);
                }
            }
        }
    });
}

fn add_candidate(
    rule: &str,
    name_node: Node<'_>,
    style: &str,
    request: &CandidateRequest<'_>,
    candidates: &mut Vec<NamingCandidate>,
) {
    let old_name = node_text(name_node, request.source);
    if !violates(rule, &old_name, style) {
        return;
    }
    let line = name_node.start_position().row + 1;
    if request
        .suppressions
        .iter()
        .any(|suppression| suppression.matches(rule, line))
    {
        return;
    }
    let Some(anchor) = range_for_node(name_node, request.source) else {
        return;
    };
    if !ranges_intersect(anchor, request.requested_range) {
        return;
    }
    let matching_diagnostic = matching_diagnostic(rule, anchor, request.context);
    if !request.context.diagnostics.is_empty() && matching_diagnostic.is_none() {
        return;
    }
    candidates.push(NamingCandidate {
        rule: rule.to_string(),
        uri: request.uri.clone(),
        anchor,
        old_name: old_name.clone(),
        new_name: replacement(rule, &old_name, style),
        config_fingerprint: request.config_fingerprint,
        source_hash: source_hash(request.source),
        diagnostic: matching_diagnostic,
    });
}

fn violates(rule: &str, name: &str, style: &str) -> bool {
    match rule {
        CONSTANT_RULE if style == "PascalCase" => !name
            .chars()
            .next()
            .is_some_and(|character| character.is_uppercase()),
        CONSTANT_RULE => !name.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
        }),
        LOCAL_RULE => violates_naming_style(name, style),
        _ => false,
    }
}

fn replacement(rule: &str, name: &str, style: &str) -> String {
    match (rule, style) {
        (CONSTANT_RULE, "PascalCase") => to_pascal_case(name),
        (CONSTANT_RULE, _) => to_upper_snake_case(name),
        (LOCAL_RULE, "camelCase") => to_camel_case(name),
        (LOCAL_RULE, _) => to_pascal_case(name),
        _ => name.to_string(),
    }
}

fn matching_diagnostic(
    rule: &str,
    anchor: Range,
    context: &CodeActionContext,
) -> Option<Diagnostic> {
    context
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic_code(diagnostic) == Some(rule) && ranges_overlap(diagnostic.range, anchor)
        })
        .cloned()
}

fn ranges_overlap(left: Range, right: Range) -> bool {
    left.start < right.end && right.start < left.end
}

fn diagnostic_code(diagnostic: &Diagnostic) -> Option<&str> {
    match diagnostic.code.as_ref()? {
        NumberOrString::String(code) => Some(code.as_str()),
        NumberOrString::Number(_) => None,
    }
}

fn range_for_node(node: Node<'_>, source: &str) -> Option<Range> {
    Some(Range::new(
        text::offset_to_position(source, node.start_byte())?,
        text::offset_to_position(source, node.end_byte())?,
    ))
}

fn ranges_intersect(left: Range, right: Range) -> bool {
    left.start <= right.end && right.start <= left.end
}

fn field_identifier_nodes<'a>(node: Node<'a>, field: &str) -> Vec<Node<'a>> {
    (0..node.child_count())
        .filter_map(|index| {
            let child = node.child(index)?;
            (child.kind() == "identifier" && node.field_name_for_child(index as u32) == Some(field))
                .then_some(child)
        })
        .collect()
}

fn walk(root: Node<'_>, callback: &mut impl FnMut(Node<'_>)) {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        callback(node);
        let mut cursor = node.walk();
        let children = node.children(&mut cursor).collect::<Vec<_>>();
        pending.extend(children.into_iter().rev());
    }
}

fn node_text(node: Node<'_>, source: &str) -> String {
    source
        .get(node.start_byte()..node.end_byte())
        .unwrap_or_default()
        .to_string()
}

fn action_title(candidate: &NamingCandidate) -> String {
    format!(
        "Rename '{}' to '{}'",
        candidate.old_name, candidate.new_name
    )
}

fn missing_unit_action_title(candidate: &MissingUnitCandidate) -> String {
    format!("Add unit '{}' to uses", candidate.unit_name)
}

fn method_implementation_action_title(candidate: &MissingMethodImplementationCandidate) -> String {
    format!("Implement '{}.{}'", candidate.owner, candidate.method)
}

fn interface_method_implementation_action_title(
    candidate: &MissingInterfaceMethodImplementationCandidate,
) -> String {
    format!("Implement '{}.{}'", candidate.owner, candidate.method)
}

impl RenameActionData {
    fn new(
        candidate: &NamingCandidate,
        source_generation: u64,
        configuration_generation: u64,
    ) -> Self {
        let mut data = Self {
            version: ACTION_DATA_VERSION,
            action_id: String::new(),
            rule: candidate.rule.clone(),
            uri: candidate.uri.clone(),
            anchor: candidate.anchor,
            new_name: candidate.new_name.clone(),
            source_generation,
            configuration_generation,
            config_fingerprint: candidate.config_fingerprint,
            source_hash: candidate.source_hash,
        };
        data.action_id = action_id(&data);
        data
    }
}

impl MissingUnitActionData {
    fn new(plan: &MissingUnitPlan, source_generation: u64, configuration_generation: u64) -> Self {
        let mut data = Self {
            version: ACTION_DATA_VERSION,
            action_id: String::new(),
            kind: MISSING_UNIT_ACTION_KIND.to_string(),
            uri: plan.uri.clone(),
            anchor: plan.candidate.anchor,
            identifier: plan.identifier.clone(),
            unit_name: plan.candidate.unit_name.clone(),
            provider_uri: plan.candidate.provider_uri.clone(),
            provider_symbol: plan.candidate.symbol_name.clone(),
            use_kind: plan.candidate.use_kind,
            provider_source_hash: plan.candidate.provider_source_hash,
            provider_declaration_fingerprint: plan.candidate.provider_declaration_fingerprint,
            provider_context_fingerprint: plan.candidate.provider_context_fingerprint,
            use_context_fingerprint: plan.candidate.use_context_fingerprint,
            source_generation,
            configuration_generation,
            config_fingerprint: plan.config_fingerprint,
            source_hash: plan.source_hash,
        };
        data.action_id = missing_unit_action_id(&data);
        data
    }
}

impl MethodImplementationActionData {
    fn new(
        plan: &MethodImplementationPlan,
        source_generation: u64,
        configuration_generation: u64,
    ) -> Self {
        let mut data = Self {
            version: METHOD_IMPLEMENTATION_ACTION_DATA_VERSION,
            action_id: String::new(),
            kind: METHOD_IMPLEMENTATION_ACTION_KIND.to_string(),
            uri: plan.uri.clone(),
            anchor: plan.candidate.anchor,
            declaration: plan.candidate.declaration,
            owner: plan.candidate.owner.clone(),
            method: plan.candidate.method.clone(),
            unit_name: plan.candidate.unit_name.clone(),
            identity: plan.candidate.identity,
            source_generation,
            configuration_generation,
            config_fingerprint: plan.config_fingerprint,
            source_hash: plan.source_hash,
        };
        data.action_id = method_implementation_action_id(&data);
        data
    }
}

impl InterfaceMethodImplementationActionData {
    fn new(
        plan: &InterfaceMethodImplementationPlan,
        source_generation: u64,
        configuration_generation: u64,
    ) -> Self {
        let mut data = Self {
            version: INTERFACE_METHOD_IMPLEMENTATION_ACTION_DATA_VERSION,
            action_id: String::new(),
            kind: INTERFACE_METHOD_IMPLEMENTATION_ACTION_KIND.to_string(),
            uri: plan.uri.clone(),
            anchor: plan.candidate.anchor,
            class_declaration: plan.candidate.class_declaration,
            owner: plan.candidate.owner.clone(),
            method: plan.candidate.method.clone(),
            interface_uri: plan.candidate.interface_uri.clone(),
            interface_owner: plan.candidate.interface_owner.clone(),
            interface_method: plan.candidate.interface_method.clone(),
            unit_name: plan.candidate.unit_name.clone(),
            identity: plan.candidate.identity,
            source_generation,
            configuration_generation,
            config_fingerprint: plan.config_fingerprint,
            source_hash: plan.source_hash,
        };
        data.action_id = interface_method_implementation_action_id(&data);
        data
    }
}

fn action_id(data: &RenameActionData) -> String {
    let mut hasher = DefaultHasher::new();
    data.version.hash(&mut hasher);
    data.rule.hash(&mut hasher);
    data.uri.hash(&mut hasher);
    data.anchor.start.line.hash(&mut hasher);
    data.anchor.start.character.hash(&mut hasher);
    data.anchor.end.line.hash(&mut hasher);
    data.anchor.end.character.hash(&mut hasher);
    data.new_name.hash(&mut hasher);
    data.source_generation.hash(&mut hasher);
    data.configuration_generation.hash(&mut hasher);
    data.config_fingerprint.hash(&mut hasher);
    data.source_hash.hash(&mut hasher);
    format!("pascal-lsp:{:016x}", hasher.finish())
}

fn missing_unit_action_id(data: &MissingUnitActionData) -> String {
    let mut hasher = DefaultHasher::new();
    data.version.hash(&mut hasher);
    data.kind.hash(&mut hasher);
    data.uri.hash(&mut hasher);
    data.anchor.start.line.hash(&mut hasher);
    data.anchor.start.character.hash(&mut hasher);
    data.anchor.end.line.hash(&mut hasher);
    data.anchor.end.character.hash(&mut hasher);
    data.identifier.hash(&mut hasher);
    data.unit_name.hash(&mut hasher);
    data.provider_uri.hash(&mut hasher);
    data.provider_symbol.hash(&mut hasher);
    data.use_kind.hash(&mut hasher);
    data.provider_source_hash.hash(&mut hasher);
    data.provider_declaration_fingerprint.hash(&mut hasher);
    data.provider_context_fingerprint.hash(&mut hasher);
    data.use_context_fingerprint.hash(&mut hasher);
    data.source_generation.hash(&mut hasher);
    data.configuration_generation.hash(&mut hasher);
    data.config_fingerprint.hash(&mut hasher);
    data.source_hash.hash(&mut hasher);
    format!("pascal-lsp:{:016x}", hasher.finish())
}

fn method_implementation_action_id(data: &MethodImplementationActionData) -> String {
    let mut hasher = DefaultHasher::new();
    data.version.hash(&mut hasher);
    data.kind.hash(&mut hasher);
    data.uri.hash(&mut hasher);
    data.anchor.start.line.hash(&mut hasher);
    data.anchor.start.character.hash(&mut hasher);
    data.anchor.end.line.hash(&mut hasher);
    data.anchor.end.character.hash(&mut hasher);
    data.declaration.start.line.hash(&mut hasher);
    data.declaration.start.character.hash(&mut hasher);
    data.declaration.end.line.hash(&mut hasher);
    data.declaration.end.character.hash(&mut hasher);
    data.owner.hash(&mut hasher);
    data.method.hash(&mut hasher);
    data.unit_name.hash(&mut hasher);
    data.identity.hash(&mut hasher);
    data.source_generation.hash(&mut hasher);
    data.configuration_generation.hash(&mut hasher);
    data.config_fingerprint.hash(&mut hasher);
    data.source_hash.hash(&mut hasher);
    format!("pascal-lsp:{:016x}", hasher.finish())
}

fn interface_method_implementation_action_id(
    data: &InterfaceMethodImplementationActionData,
) -> String {
    let mut hasher = DefaultHasher::new();
    data.version.hash(&mut hasher);
    data.kind.hash(&mut hasher);
    data.uri.hash(&mut hasher);
    data.anchor.start.line.hash(&mut hasher);
    data.anchor.start.character.hash(&mut hasher);
    data.anchor.end.line.hash(&mut hasher);
    data.anchor.end.character.hash(&mut hasher);
    data.class_declaration.start.line.hash(&mut hasher);
    data.class_declaration.start.character.hash(&mut hasher);
    data.class_declaration.end.line.hash(&mut hasher);
    data.class_declaration.end.character.hash(&mut hasher);
    data.owner.hash(&mut hasher);
    data.method.hash(&mut hasher);
    data.interface_uri.hash(&mut hasher);
    data.interface_owner.hash(&mut hasher);
    data.interface_method.hash(&mut hasher);
    data.unit_name.hash(&mut hasher);
    data.identity.hash(&mut hasher);
    data.source_generation.hash(&mut hasher);
    data.configuration_generation.hash(&mut hasher);
    data.config_fingerprint.hash(&mut hasher);
    data.source_hash.hash(&mut hasher);
    format!("pascal-lsp:{:016x}", hasher.finish())
}

fn source_hash(source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

fn parse_action_data(value: Option<&Value>) -> Result<ParsedActionData, String> {
    let Some(value) = value else {
        return Err("code action has no resolve data".to_string());
    };
    if value.get("kind").and_then(Value::as_str)
        == Some(INTERFACE_METHOD_IMPLEMENTATION_ACTION_KIND)
    {
        let mut data: InterfaceMethodImplementationActionData =
            serde_json::from_value(value.clone()).map_err(|error| {
                format!("invalid interface method implementation resolve data: {error}")
            })?;
        data.uri = canonical_file_uri(&data.uri);
        data.interface_uri = canonical_file_uri(&data.interface_uri);
        if data.action_id.len() > MAX_ACTION_ID_BYTES
            || data.owner.len() > MAX_ACTION_UNIT_BYTES
            || data.method.len() > MAX_ACTION_NAME_BYTES
            || data.interface_owner.len() > MAX_ACTION_UNIT_BYTES
            || data.interface_method.len() > MAX_ACTION_NAME_BYTES
            || data.unit_name.len() > MAX_ACTION_UNIT_BYTES
            || data.uri.as_str().len() > MAX_ACTION_URI_BYTES
            || data.interface_uri.as_str().len() > MAX_ACTION_URI_BYTES
        {
            return Err(
                "interface method implementation resolve data exceeds its bounded identity limits"
                    .to_string(),
            );
        }
        Ok(ParsedActionData::InterfaceMethodImplementation(data))
    } else if value.get("kind").and_then(Value::as_str) == Some(METHOD_IMPLEMENTATION_ACTION_KIND) {
        let mut data: MethodImplementationActionData = serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid method implementation resolve data: {error}"))?;
        data.uri = canonical_file_uri(&data.uri);
        if data.action_id.len() > MAX_ACTION_ID_BYTES
            || data.owner.len() > MAX_ACTION_UNIT_BYTES
            || data.method.len() > MAX_ACTION_NAME_BYTES
            || data.unit_name.len() > MAX_ACTION_UNIT_BYTES
            || data.uri.as_str().len() > MAX_ACTION_URI_BYTES
        {
            return Err(
                "method implementation resolve data exceeds its bounded identity limits"
                    .to_string(),
            );
        }
        Ok(ParsedActionData::MethodImplementation(data))
    } else if value.get("kind").is_some() {
        let mut data: MissingUnitActionData = serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid missing-unit resolve data: {error}"))?;
        data.uri = canonical_file_uri(&data.uri);
        data.provider_uri = canonical_file_uri(&data.provider_uri);
        if data.action_id.len() > MAX_ACTION_ID_BYTES
            || data.identifier.len() > MAX_ACTION_NAME_BYTES
            || data.unit_name.len() > MAX_ACTION_UNIT_BYTES
            || data.provider_symbol.len() > MAX_ACTION_NAME_BYTES
            || data.uri.as_str().len() > MAX_ACTION_URI_BYTES
            || data.provider_uri.as_str().len() > MAX_ACTION_URI_BYTES
        {
            return Err(
                "missing-unit resolve data exceeds its bounded identity limits".to_string(),
            );
        }
        Ok(ParsedActionData::MissingUnit(data))
    } else {
        let mut data: RenameActionData = serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid code action resolve data: {error}"))?;
        data.uri = canonical_file_uri(&data.uri);
        if data.action_id.len() > MAX_ACTION_ID_BYTES || data.new_name.len() > MAX_ACTION_NAME_BYTES
        {
            return Err("code action resolve data exceeds its bounded identity limits".to_string());
        }
        Ok(ParsedActionData::Rename(data))
    }
}

fn validate_rename_action_data(
    data: &RenameActionData,
    action: &CodeAction,
    source_generation: u64,
    configuration_generation: u64,
) -> Result<(), String> {
    if data.version != ACTION_DATA_VERSION
        || data.action_id != action_id(data)
        || data.source_generation != source_generation
        || data.configuration_generation != configuration_generation
    {
        return Err("code action resolve data is stale or tampered".to_string());
    }
    if data.rule != CONSTANT_RULE && data.rule != LOCAL_RULE {
        return Err("code action rule is unsupported".to_string());
    }
    if data.new_name.is_empty() || data.new_name.len() > MAX_ACTION_NAME_BYTES {
        return Err("code action proposed name is invalid".to_string());
    }
    if action.data.as_ref().is_none_or(|value| value.is_null()) {
        return Err("code action resolve data is missing".to_string());
    }
    Ok(())
}

fn validate_missing_unit_action_data(
    data: &MissingUnitActionData,
    action: &CodeAction,
) -> Result<(), String> {
    if data.version != ACTION_DATA_VERSION
        || data.kind != MISSING_UNIT_ACTION_KIND
        || data.action_id != missing_unit_action_id(data)
    {
        return Err("missing-unit code action resolve data is stale or tampered".to_string());
    }
    if data.identifier.is_empty()
        || data.identifier.len() > MAX_ACTION_NAME_BYTES
        || data.unit_name.is_empty()
        || data.unit_name.len() > MAX_ACTION_UNIT_BYTES
        || data.provider_symbol.is_empty()
        || data.provider_symbol.len() > MAX_ACTION_NAME_BYTES
    {
        return Err("missing-unit code action identity is invalid".to_string());
    }
    if action.title != format!("Add unit '{}' to uses", data.unit_name)
        || action.kind.as_ref() != Some(&CodeActionKind::QUICKFIX)
    {
        return Err("missing-unit code action identity was modified by the client".to_string());
    }
    if action.data.as_ref().is_none_or(|value| value.is_null()) {
        return Err("code action resolve data is missing".to_string());
    }
    Ok(())
}

fn validate_method_implementation_action_data(
    data: &MethodImplementationActionData,
    action: &CodeAction,
    source_generation: u64,
    configuration_generation: u64,
) -> Result<(), String> {
    if data.version != METHOD_IMPLEMENTATION_ACTION_DATA_VERSION
        || data.kind != METHOD_IMPLEMENTATION_ACTION_KIND
        || data.action_id != method_implementation_action_id(data)
        || data.source_generation != source_generation
        || data.configuration_generation != configuration_generation
    {
        return Err("method implementation resolve data is stale or tampered".to_string());
    }
    if data.owner.is_empty()
        || data.owner.len() > MAX_ACTION_UNIT_BYTES
        || data.method.is_empty()
        || data.method.len() > MAX_ACTION_NAME_BYTES
        || data.unit_name.is_empty()
        || data.unit_name.len() > MAX_ACTION_UNIT_BYTES
    {
        return Err("method implementation action identity is invalid".to_string());
    }
    if action.title != format!("Implement '{}.{}'", data.owner, data.method)
        || action.kind.as_ref() != Some(&CodeActionKind::QUICKFIX)
    {
        return Err("method implementation action identity was modified by the client".to_string());
    }
    if action.data.as_ref().is_none_or(|value| value.is_null()) {
        return Err("code action resolve data is missing".to_string());
    }
    Ok(())
}

fn validate_interface_method_implementation_action_data(
    data: &InterfaceMethodImplementationActionData,
    action: &CodeAction,
    source_generation: u64,
    configuration_generation: u64,
) -> Result<(), String> {
    if data.version != INTERFACE_METHOD_IMPLEMENTATION_ACTION_DATA_VERSION
        || data.kind != INTERFACE_METHOD_IMPLEMENTATION_ACTION_KIND
        || data.action_id != interface_method_implementation_action_id(data)
        || data.source_generation != source_generation
        || data.configuration_generation != configuration_generation
    {
        return Err(
            "interface method implementation resolve data is stale or tampered".to_string(),
        );
    }
    if data.owner.is_empty()
        || data.owner.len() > MAX_ACTION_UNIT_BYTES
        || data.method.is_empty()
        || data.method.len() > MAX_ACTION_NAME_BYTES
        || data.interface_owner.is_empty()
        || data.interface_owner.len() > MAX_ACTION_UNIT_BYTES
        || data.interface_method.is_empty()
        || data.interface_method.len() > MAX_ACTION_NAME_BYTES
        || data.unit_name.is_empty()
        || data.unit_name.len() > MAX_ACTION_UNIT_BYTES
    {
        return Err("interface method implementation action identity is invalid".to_string());
    }
    if action.title != format!("Implement '{}.{}'", data.owner, data.method)
        || action.kind.as_ref() != Some(&INTERFACE_METHOD_IMPLEMENTATION_CODE_ACTION_KIND)
    {
        return Err(
            "interface method implementation action identity was modified by the client"
                .to_string(),
        );
    }
    if action.data.as_ref().is_none_or(|value| value.is_null()) {
        return Err("code action resolve data is missing".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ClientActionFeatures, code_actions_from_input, lint_configuration_for_input,
        resolve_from_input, set_after_lint_configuration_hook,
    };
    use crate::workspace::Workspace;
    use crate::workspace::WorkspaceOptions;
    use crate::workspace::rename::revalidate_input;
    use lsp_types::CodeActionOrCommand;
    use lsp_types::Url;
    use pascal_project::delphi_overrides::OverrideSession;
    use serde_json::json;
    use std::fs::{self, File, FileTimes};
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::sync::atomic::AtomicBool;

    fn test_workspace(roots: Vec<std::path::PathBuf>, options: WorkspaceOptions) -> Workspace {
        Workspace::with_override_session(roots, options, OverrideSession::new(None))
    }

    #[test]
    fn local_code_actions_ignore_unused_shadowed_and_formatter_configurations() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let app = root.join("app");
        let source_path = app.join("Main.pas");
        let source = "unit Main;\ninterface\nimplementation\nprocedure Use;\nvar\n  BadVariable: Integer;\nbegin\n  BadVariable := 1;\nend;\nend.\n";
        fs::create_dir_all(&app).expect("project directory");
        fs::write(&source_path, source).expect("source");
        fs::write(
            app.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        )
        .expect("project");
        fs::write(app.join("App.dpr"), "program App; begin end.\n").expect("program");
        fs::write(
            app.join(".lint4d.toml"),
            "[rules]\nlocal-variable-naming = \"warning\"\n[rules.naming]\nlocal_variable_style = \"camelCase\"\n",
        )
        .expect("selected lint configuration");
        fs::write(root.join(".lint4d.toml"), vec![b'x'; 4 * 1024 * 1024 + 1])
            .expect("shadowed lint configuration");
        fs::write(root.join(".fmt4d.toml"), vec![b'x'; 4 * 1024 * 1024 + 1])
            .expect("unused formatter configuration");

        let uri = Url::from_file_path(&source_path).expect("source URI");
        let input = test_workspace(vec![root.clone()], Default::default()).analysis_input();
        let start = lsp_types::Position::new(5, 2);
        let end = lsp_types::Position::new(5, 13);
        let params: lsp_types::CodeActionParams = serde_json::from_value(json!({
            "textDocument": {"uri": uri},
            "range": {"start": start, "end": end},
            "context": {"diagnostics": [], "only": ["quickfix"]}
        }))
        .expect("code action parameters");
        let cancel = AtomicBool::new(false);
        let features = ClientActionFeatures {
            resolve: false,
            document_changes: false,
            disabled: false,
        };
        let computed = code_actions_from_input(input.clone(), params, features.clone(), &cancel);
        let actions = computed
            .value
            .as_ref()
            .expect("unused configuration must not fail an eager local fix");
        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected a code action");
        };
        assert_eq!(action.title, "Rename 'BadVariable' to 'badVariable'");
        assert!(action.edit.is_some());
        assert!(
            computed.records.iter().all(|record| {
                record.path.as_deref() != Some(root.join(".lint4d.toml").as_path())
                    && record.path.as_deref() != Some(root.join(".fmt4d.toml").as_path())
            }),
            "unused configuration files must not enter the local action identity"
        );

        let resolve_params: lsp_types::CodeActionParams = serde_json::from_value(json!({
            "textDocument": {"uri": uri},
            "range": {"start": start, "end": end},
            "context": {"diagnostics": [], "only": ["quickfix"]}
        }))
        .expect("resolve code action parameters");
        let resolve_action = code_actions_from_input(
            input.clone(),
            resolve_params,
            ClientActionFeatures {
                resolve: true,
                document_changes: false,
                disabled: false,
            },
            &cancel,
        )
        .value
        .expect("resolved action listing")[0]
            .clone();
        let CodeActionOrCommand::CodeAction(resolve_action) = resolve_action else {
            panic!("expected an unresolved code action");
        };
        let resolved = resolve_from_input(
            input,
            resolve_action,
            ClientActionFeatures {
                resolve: true,
                document_changes: false,
                disabled: false,
            },
            &cancel,
        );
        assert!(
            resolved.value.is_ok(),
            "unused configurations blocked resolve: {:?}",
            resolved.value
        );
    }

    #[test]
    fn code_actions_accept_a_stale_overlapping_diagnostic_after_identifier_edit() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let source_path = root.join("Provider.pas");
        fs::create_dir_all(&root).expect("workspace directory");
        fs::write(
            &source_path,
            "unit Provider;\ninterface\nconst\n  renamedConst = 1;\nimplementation\nend.\n",
        )
        .expect("source");

        let uri = Url::from_file_path(&source_path).expect("source URI");
        let params: lsp_types::CodeActionParams = serde_json::from_value(json!({
            "textDocument": {"uri": uri},
            "range": {
                "start": {"line": 3, "character": 2},
                "end": {"line": 3, "character": 14}
            },
            "context": {
                "diagnostics": [{
                    "code": "constant-naming",
                    "message": "Constant 'badConst' should use UPPER_CASE naming convention.",
                    "range": {
                        "start": {"line": 3, "character": 2},
                        "end": {"line": 3, "character": 10}
                    },
                    "severity": 4,
                    "source": "lint4d"
                }],
                "only": ["quickfix"]
            }
        }))
        .expect("code action parameters");
        let computed = code_actions_from_input(
            test_workspace(vec![root], Default::default()).analysis_input(),
            params,
            ClientActionFeatures {
                resolve: false,
                document_changes: false,
                disabled: false,
            },
            &AtomicBool::new(false),
        );
        let actions = computed
            .value
            .expect("stale diagnostic must not suppress the current fix");
        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected a code action");
        };
        assert_eq!(action.title, "Rename 'renamedConst' to 'RENAMED_CONST'");
    }

    #[test]
    fn workspace_constant_code_actions_ignore_unused_shadowed_and_formatter_configurations() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("workspace");
        let app = root.join("app");
        let source_path = app.join("Main.pas");
        let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
        let project_path = app.join("App.dproj");
        let root_lint_path = root.join(".lint4d.toml");
        let root_fmt_path = root.join(".fmt4d.toml");
        fs::create_dir_all(&app).expect("project directory");
        fs::write(&source_path, source).expect("source");
        fs::write(
            &project_path,
            "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup></Project>",
        )
        .expect("project");
        fs::write(
            app.join(".lint4d.toml"),
            "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
        )
        .expect("selected lint configuration");
        fs::write(&root_lint_path, vec![b'x'; 4 * 1024 * 1024 + 1])
            .expect("shadowed lint configuration");
        fs::write(&root_fmt_path, vec![b'x'; 4 * 1024 * 1024 + 1])
            .expect("unused formatter configuration");

        let uri = Url::from_file_path(&source_path).expect("source URI");
        let input = test_workspace(vec![root], Default::default()).analysis_input();
        let range = lsp_types::Range::new(
            lsp_types::Position::new(3, 2),
            lsp_types::Position::new(3, 10),
        );
        let params: lsp_types::CodeActionParams = serde_json::from_value(json!({
            "textDocument": {"uri": uri},
            "range": range,
            "context": {"diagnostics": [], "only": ["quickfix"]}
        }))
        .expect("code action parameters");
        let cancel = AtomicBool::new(false);
        let eager = code_actions_from_input(
            input.clone(),
            params,
            ClientActionFeatures {
                resolve: false,
                document_changes: false,
                disabled: false,
            },
            &cancel,
        );
        let eager_actions = eager
            .value
            .as_ref()
            .expect("unused configurations must not block an eager workspace constant fix");
        assert_eq!(eager_actions.len(), 1);
        let CodeActionOrCommand::CodeAction(eager_action) = &eager_actions[0] else {
            panic!("expected an eager code action");
        };
        assert_eq!(eager_action.title, "Rename 'badConst' to 'BAD_CONST'");
        assert!(eager_action.edit.is_some());
        assert!(eager.records.iter().all(|record| {
            record.path.as_deref() != Some(root_lint_path.as_path())
                && record.path.as_deref() != Some(root_fmt_path.as_path())
        }));

        let resolve_params: lsp_types::CodeActionParams = serde_json::from_value(json!({
            "textDocument": {"uri": uri},
            "range": range,
            "context": {"diagnostics": [], "only": ["quickfix"]}
        }))
        .expect("resolve code action parameters");
        let unresolved = code_actions_from_input(
            input.clone(),
            resolve_params,
            ClientActionFeatures {
                resolve: true,
                document_changes: false,
                disabled: false,
            },
            &cancel,
        )
        .value
        .expect("unused configurations must not block resolved action listing");
        let CodeActionOrCommand::CodeAction(unresolved) = unresolved[0].clone() else {
            panic!("expected an unresolved code action");
        };
        let resolved = resolve_from_input(
            input,
            unresolved,
            ClientActionFeatures {
                resolve: true,
                document_changes: false,
                disabled: false,
            },
            &cancel,
        );
        let resolved_action = resolved
            .value
            .as_ref()
            .expect("unused configurations must not block a resolved workspace constant fix");
        assert!(resolved_action.edit.is_some());
        assert!(resolved.records.iter().all(|record| {
            record.path.as_deref() != Some(root_lint_path.as_path())
                && record.path.as_deref() != Some(root_fmt_path.as_path())
        }));
    }

    #[test]
    fn eager_code_action_revalidation_rejects_changed_configuration() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let source_path = root.join("Main.pas");
        let config_path = root.join(".lint4d.toml");
        let source = "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&source_path, source).expect("source");
        fs::write(
            &config_path,
            "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
        )
        .expect("configuration");

        let uri = Url::from_file_path(&source_path).expect("source URI");
        let workspace = test_workspace(vec![root], Default::default());
        let input = workspace.analysis_input();
        let validation_input = input.clone();
        let original_bytes = fs::read(&config_path).expect("original configuration bytes");
        let params = serde_json::from_value(json!({
            "textDocument": {"uri": uri},
            "range": {
                "start": {"line": 3, "character": 2},
                "end": {"line": 3, "character": 10}
            },
            "context": {"diagnostics": [], "only": ["quickfix"]}
        }))
        .expect("code action parameters");
        let cancel = AtomicBool::new(false);
        let original_metadata =
            fs::metadata(&config_path).expect("original configuration metadata");
        let original_mtime = original_metadata
            .modified()
            .expect("original configuration mtime");
        let changed_configuration = "[rules.naming]\nconstant_style = \"PascalCase\"\n".to_string();
        assert_eq!(
            changed_configuration.len(),
            original_metadata.len() as usize,
            "the mutation must keep the same byte length"
        );
        let config_for_hook = config_path.clone();
        set_after_lint_configuration_hook(move || {
            fs::write(&config_for_hook, &changed_configuration).expect("changed configuration");
            File::options()
                .write(true)
                .open(&config_for_hook)
                .expect("open changed configuration for timestamp restore")
                .set_times(FileTimes::new().set_modified(original_mtime))
                .expect("restore configuration mtime");
        });

        let computed = code_actions_from_input(
            validation_input.clone(),
            params,
            ClientActionFeatures {
                resolve: false,
                document_changes: false,
                disabled: false,
            },
            &cancel,
        );
        let actions = computed.value.as_ref().expect("computed actions");
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected a code action");
        };
        assert_eq!(action.title, "Rename 'badConst' to 'BAD_CONST'");
        assert_eq!(
            fs::metadata(&config_path)
                .expect("changed configuration metadata")
                .modified()
                .expect("changed configuration mtime"),
            original_mtime,
            "the mutation must restore the original mtime"
        );
        assert_ne!(
            fs::read(&config_path).expect("changed configuration bytes"),
            original_bytes,
            "configuration bytes must differ"
        );
        let configuration_record_count = computed
            .records
            .iter()
            .filter(|record| record.path.as_deref() == Some(config_path.as_path()))
            .count();
        assert!(
            configuration_record_count >= 2,
            "parsed and snapshot configuration observations must both be retained"
        );

        let error = revalidate_input(&validation_input, &computed.records, &cancel).expect_err(
            "configuration mutation between parse and snapshot must invalidate the action",
        );
        assert!(
            error.contains("configuration") || error.contains("stale"),
            "unexpected stale configuration error: {error}"
        );
    }

    #[test]
    fn absent_configuration_observation_rejects_a_later_created_candidate() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let source_path = root.join("Main.pas");
        let config_path = root.join(".lint4d.toml");
        let source = "unit Main;\ninterface\nimplementation\nend.\n";
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(&source_path, source).expect("source");

        let uri = Url::from_file_path(&source_path).expect("source URI");
        let workspace = test_workspace(vec![root], Default::default());
        let input = workspace.analysis_input();
        let config_for_hook = config_path.clone();
        set_after_lint_configuration_hook(move || {
            fs::write(
                &config_for_hook,
                "[rules.naming]\nconstant_style = \"UPPER_CASE\"\n",
            )
            .expect("create configuration");
        });

        let (_, _, _, records) =
            lint_configuration_for_input(&input, &uri).expect("default configuration resolution");
        let record = records
            .iter()
            .find(|record| record.path.as_deref() == Some(config_path.as_path()))
            .expect("absent configuration candidate record");
        assert!(record.content_bytes.is_none());
        assert!(
            record.path_stamp.is_none(),
            "an absent observation must not adopt metadata from a later file creation"
        );

        let cancel = AtomicBool::new(false);
        let error = revalidate_input(&input, &records, &cancel)
            .expect_err("a candidate created after an absent observation must invalidate it");
        assert!(
            error.contains("configuration") || error.contains("changed"),
            "unexpected absent-to-present error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn absent_configuration_observation_rejects_a_newly_dangling_symlink() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().join("fixture");
        let source_path = root.join("Main.pas");
        let config_path = root.join(".lint4d.toml");
        fs::create_dir_all(&root).expect("fixture directory");
        fs::write(
            &source_path,
            "unit Main;\ninterface\nimplementation\nend.\n",
        )
        .expect("source");

        let uri = Url::from_file_path(&source_path).expect("source URI");
        let workspace = test_workspace(vec![root], Default::default());
        let input = workspace.analysis_input();
        let config_for_hook = config_path.clone();
        set_after_lint_configuration_hook(move || {
            symlink("missing-lint-config", &config_for_hook).expect("dangling configuration link");
        });

        let (_, _, _, records) =
            lint_configuration_for_input(&input, &uri).expect("default configuration resolution");
        let record = records
            .iter()
            .find(|record| record.path.as_deref() == Some(config_path.as_path()))
            .expect("absent configuration candidate record");
        assert!(record.content_bytes.is_none());
        let cancel = AtomicBool::new(false);
        let error = revalidate_input(&input, &records, &cancel)
            .expect_err("a dangling candidate must not validate as absent");
        assert!(
            error.contains("configuration") || error.contains("changed"),
            "unexpected absent-to-dangling error: {error}"
        );
    }
}
