//! Naming quick-fix planning for the workspace snapshot.

use super::canonical_file_uri;
use super::rename::{
    CANCELLATION_MESSAGE, Computed, RenameSnapshot, SnapshotMode, SnapshotSeed, WorkspaceInput,
    build_snapshot, check_includes, identifier_at_position, input_source_is_editable, is_cancelled,
    snapshot_records, source_for_input, workspace_edit,
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
    CodeActionParams, Diagnostic, NumberOrString, Range, Url, WorkspaceEdit,
};
use pascal_core::{FileInfo, parser};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;
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
    let (source, target_record) = match source_for_input(&input, &uri) {
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

    let candidates = match naming_candidates(&uri, &source, params.range, &params.context) {
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
            records: vec![target_record],
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
        Some(SnapshotSeed::new(target_record)),
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
    let records = snapshot_records(&snapshot);
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
            match plan_candidate(&snapshot, &candidate, features.document_changes) {
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
    let (target_source, target_record) = match source_for_input(&input, &target_uri) {
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
    let snapshot = match build_snapshot(
        &input,
        std::slice::from_ref(&target_uri),
        &candidate_names,
        mode,
        Some(SnapshotSeed::new(target_record)),
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
    let fingerprint = match config_fingerprint(&target_uri) {
        Ok(fingerprint) => fingerprint,
        Err(error) => return failed(source_generation, configuration_generation, error),
    };
    if fingerprint != data.config_fingerprint {
        return failed(
            source_generation,
            configuration_generation,
            "code action configuration is stale; request code actions again".to_string(),
        );
    }
    let context = CodeActionContext {
        diagnostics: action.diagnostics.clone().unwrap_or_default(),
        only: Some(vec![CodeActionKind::QUICKFIX]),
        trigger_kind: None,
    };
    let candidates = match naming_candidates(&target_uri, source, data.anchor, &context) {
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
            records: snapshot_records(&snapshot),
        };
    }

    match plan_candidate(&snapshot, &candidate, features.document_changes) {
        Ok((edit, _records)) => {
            let mut resolved = action;
            resolved.edit = Some(edit);
            resolved.disabled = None;
            Computed {
                source_generation,
                configuration_generation,
                value: Ok(resolved),
                records: snapshot_records(&snapshot),
            }
        }
        Err(error) if features.disabled => {
            let mut disabled_action = action;
            disabled_action.disabled = Some(CodeActionDisabled { reason: error });
            Computed {
                source_generation,
                configuration_generation,
                value: Ok(disabled_action),
                records: snapshot_records(&snapshot),
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

fn set_disabled_or_skip(action: &mut CodeAction, supported: bool, reason: String) -> bool {
    if !supported {
        return false;
    }
    action.disabled = Some(CodeActionDisabled { reason });
    true
}

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
    check_includes(snapshot, &candidate.uri, position)?;
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
) -> Result<Vec<NamingCandidate>, String> {
    let path = uri
        .to_file_path()
        .map_err(|_| format!("not a file URI: {uri}"))?;
    let (config, _) = Config::discover(path.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|error| format!("could not discover lint configuration: {error}"))?;
    let config_fingerprint = config_fingerprint(uri)?;
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
    if !rule_is_off(&config, CONSTANT_RULE) {
        collect_constants(
            tree.root_node(),
            config.constant_style(),
            &request,
            &mut candidates,
        );
    }
    if !rule_is_off(&config, LOCAL_RULE) {
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

fn config_fingerprint(uri: &Url) -> Result<u64, String> {
    let path = uri
        .to_file_path()
        .map_err(|_| format!("not a file URI: {uri}"))?;
    let (config, root) = Config::discover(path.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|error| format!("could not discover lint configuration: {error}"))?;
    let config_path = root.join(".lint4d.toml");
    let mut hasher = DefaultHasher::new();
    config_path.to_string_lossy().hash(&mut hasher);
    if let Ok(bytes) = fs::read(&config_path) {
        bytes.hash(&mut hasher);
    } else {
        config.constant_style().hash(&mut hasher);
        config.local_variable_style().hash(&mut hasher);
        format!("{:?}", config.rule_severity(CONSTANT_RULE)).hash(&mut hasher);
        format!("{:?}", config.rule_severity(LOCAL_RULE)).hash(&mut hasher);
    }
    Ok(hasher.finish())
}
