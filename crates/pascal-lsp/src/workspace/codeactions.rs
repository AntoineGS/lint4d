//! Naming quick-fix planning for the workspace snapshot.

use super::rename::{
    CANCELLATION_MESSAGE, Computed, RenameSnapshot, SnapshotMode, SnapshotSeed, SourceRecord,
    WorkspaceInput, build_snapshot, check_includes, identifier_at_position,
    input_source_is_editable, is_cancelled, snapshot_records, source_for_input_with_cancel,
    workspace_edit,
};
use super::{
    absolute_path, canonical_file_uri, is_configuration_file, is_lint_excluded, path_stamp,
};
use crate::configuration::{config_directories, resolve_lint};
use crate::project::{has_invalid_project_selection, project_candidates};
use crate::text;
use lint4d::config::{Config, RuleSeverityOverride};
use lint4d::engine::suppress::parse_suppressions;
use lint4d::rules::helpers::effective_children;
use lint4d::rules::naming::{
    to_camel_case, to_pascal_case, to_upper_snake_case, violates_naming_style,
};
use lsp_types::{
    CodeAction, CodeActionContext, CodeActionDisabled, CodeActionKind, CodeActionOrCommand,
    CodeActionParams, Diagnostic, NumberOrString, Range, Url, WorkspaceEdit,
};
use pascal_core::{FileInfo, parser};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::AtomicBool;
use tree_sitter::Node;

const ACTION_DATA_VERSION: u8 = 2;
const MAX_ACTION_ID_BYTES: usize = 64;
const MAX_ACTION_NAME_BYTES: usize = 256;
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
    if !requests_quickfix(&params.context) {
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
    let candidates = match naming_candidates(
        &uri,
        &source,
        params.range,
        &params.context,
        &config,
        config_fingerprint,
    ) {
        Ok(candidates) => candidates,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if features.resolve {
        let actions = candidates
            .iter()
            .map(|candidate| {
                let data =
                    RenameActionData::new(candidate, source_generation, configuration_generation);
                CodeActionOrCommand::CodeAction(CodeAction {
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
                })
            })
            .collect();
        return Computed {
            source_generation,
            configuration_generation,
            value: Ok(actions),
            records: {
                let mut records = vec![target_record];
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
        Some(SnapshotSeed::new(target_record).with_consumed_configuration(&configuration_records)),
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
        actions.push(CodeActionOrCommand::CodeAction(action));
    }
    let mut records = snapshot_records(&snapshot);
    append_records(&mut records, configuration_records);
    Computed {
        source_generation,
        configuration_generation,
        value: Ok(actions),
        records,
    }
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
    if let Err(error) =
        validate_action_data(&data, &action, source_generation, configuration_generation)
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
    context
        .only
        .as_ref()
        .is_none_or(|kinds| kinds.iter().any(|kind| kind == &CodeActionKind::QUICKFIX))
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
    let mut workspace = super::Workspace::new(input.roots.clone(), input.options.clone());
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
                content_bytes: content_bytes.clone(),
                candidate_membership: None,
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
        .find(|diagnostic| diagnostic_code(diagnostic) == Some(rule) && diagnostic.range == anchor)
        .cloned()
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

fn source_hash(source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

fn parse_action_data(value: Option<&Value>) -> Result<RenameActionData, String> {
    let Some(value) = value else {
        return Err("code action has no resolve data".to_string());
    };
    let mut data: RenameActionData = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid code action resolve data: {error}"))?;
    data.uri = canonical_file_uri(&data.uri);
    if data.action_id.len() > MAX_ACTION_ID_BYTES || data.new_name.len() > MAX_ACTION_NAME_BYTES {
        return Err("code action resolve data exceeds its bounded identity limits".to_string());
    }
    Ok(data)
}

fn validate_action_data(
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

#[cfg(test)]
mod tests {
    use super::{
        ClientActionFeatures, code_actions_from_input, lint_configuration_for_input,
        resolve_from_input, set_after_lint_configuration_hook,
    };
    use crate::workspace::Workspace;
    use crate::workspace::rename::revalidate_input;
    use lsp_types::CodeActionOrCommand;
    use lsp_types::Url;
    use serde_json::json;
    use std::fs::{self, File, FileTimes};
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::sync::atomic::AtomicBool;

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
        let input = Workspace::new(vec![root.clone()], Default::default()).analysis_input();
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
        let input = Workspace::new(vec![root], Default::default()).analysis_input();
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
        let workspace = Workspace::new(vec![root], Default::default());
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
        let workspace = Workspace::new(vec![root], Default::default());
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
        let workspace = Workspace::new(vec![root], Default::default());
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
