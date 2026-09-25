use super::rename::{
    CANCELLATION_MESSAGE, Computed, SourceRecord, WorkspaceInput, input_source_is_editable,
    input_source_is_readable_with_owner, is_cancelled, owner_for_input,
    source_for_input_with_owner, text_content_hash,
};
use super::{canonical_file_uri, queries};
use crate::navigation::NavigationTarget;
use crate::text;
use lsp_types::{CodeLens, Command, Position, Range, Url};
use pascal_core::{FileInfo, parser};
use pascal_project::has_invalid_project_selection;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use tree_sitter::Node;

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_PROCEDURES: usize = 64;
const MAX_LOCATIONS: usize = 128;
const DATA_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum LensKind {
    References,
    Implementation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LensData {
    version: u8,
    uri: Url,
    range: Range,
    implementation_position: Position,
    source_hash: String,
    source_generation: String,
    configuration_generation: String,
    kind: LensKind,
}

fn simple_procedure_name<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    if node.kind() != "declProc" || node.child_by_field_name("args").is_some() {
        return None;
    }
    let name_node = node.child_by_field_name("name")?;
    if name_node.kind() != "identifier" {
        return None;
    }
    let name = &source[name_node.start_byte()..name_node.end_byte()];
    let prefix = &source[node.start_byte()..name_node.start_byte()];
    let suffix = &source[name_node.end_byte()..node.end_byte()];
    if name.is_empty()
        || name.len() > 64
        || !name.as_bytes()[0].is_ascii_alphabetic()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || !prefix
            .get(.."procedure".len())
            .is_some_and(|keyword| keyword.eq_ignore_ascii_case("procedure"))
        || !prefix["procedure".len()..].chars().all(char::is_whitespace)
        || suffix.trim() != ";"
    {
        return None;
    }
    Some(name)
}

fn declaration_name<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    let header = if matches!(node.kind(), "defProc" | "defFunc") {
        node.child_by_field_name("header")?
    } else {
        node
    };
    if !matches!(header.kind(), "declProc" | "declFunc") {
        return None;
    }
    let name = header.child_by_field_name("name")?;
    (name.kind() == "identifier").then(|| &source[name.start_byte()..name.end_byte()])
}

fn lenses_for_source(
    uri: &Url,
    source: &str,
    source_generation: u64,
    configuration_generation: u64,
) -> Result<Vec<CodeLens>, String> {
    if source.len() > MAX_SOURCE_BYTES || source.contains("{$") || source.contains("(*$") {
        return Ok(Vec::new());
    }
    let path = uri
        .to_file_path()
        .map_err(|_| "code-lens URI is not a local source file".to_string())?;
    if !path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pas"))
    {
        return Ok(Vec::new());
    }
    let (tree, diagnostics) = parser::parse_file(&FileInfo::new(path), source.as_bytes())?;
    if tree.root_node().has_error() || !diagnostics.is_empty() {
        return Ok(Vec::new());
    }
    let Some(unit) = tree
        .root_node()
        .named_child(0)
        .filter(|node| node.kind() == "unit")
    else {
        return Ok(Vec::new());
    };
    let mut cursor = unit.walk();
    let sections = unit.named_children(&mut cursor).collect::<Vec<_>>();
    let Some(interface) = sections.iter().find(|node| node.kind() == "interface") else {
        return Ok(Vec::new());
    };
    let Some(implementation) = sections.iter().find(|node| node.kind() == "implementation") else {
        return Ok(Vec::new());
    };
    let mut names: HashMap<String, (Vec<Node<'_>>, Vec<Node<'_>>)> = HashMap::new();
    for (section, is_interface) in [(*interface, true), (*implementation, false)] {
        let mut cursor = section.walk();
        for node in section.named_children(&mut cursor) {
            if let Some(name) = declaration_name(node, source) {
                let pair = names.entry(name.to_ascii_lowercase()).or_default();
                if is_interface {
                    pair.0.push(node)
                } else {
                    pair.1.push(node)
                }
            }
        }
    }
    let mut lenses = Vec::new();
    let source_hash = text_content_hash(source);
    for (_, (declarations, implementations)) in names {
        if declarations.len() != 1 || implementations.len() != 1 {
            continue;
        }
        let declaration = declarations[0];
        let definition = implementations[0];
        let Some(name) = simple_procedure_name(declaration, source) else {
            continue;
        };
        if definition.kind() != "defProc" || definition.child_by_field_name("local").is_some() {
            continue;
        }
        let Some(header) = definition.child_by_field_name("header") else {
            continue;
        };
        let Some(body) = definition
            .child_by_field_name("body")
            .filter(|body| body.kind() == "block")
        else {
            continue;
        };
        if !simple_procedure_name(header, source)
            .is_some_and(|other| other.eq_ignore_ascii_case(name))
            || !source[header.end_byte()..body.start_byte()]
                .chars()
                .all(char::is_whitespace)
        {
            continue;
        }
        let Some(name_node) = declaration.child_by_field_name("name") else {
            continue;
        };
        let Some(implementation_name) = header.child_by_field_name("name") else {
            continue;
        };
        let Some(implementation_position) =
            text::offset_to_position(source, implementation_name.start_byte())
        else {
            continue;
        };
        let Some(start) = text::offset_to_position(source, name_node.start_byte()) else {
            continue;
        };
        let Some(end) = text::offset_to_position(source, name_node.end_byte()) else {
            continue;
        };
        let range = Range::new(start, end);
        for kind in [LensKind::References, LensKind::Implementation] {
            let data = LensData {
                version: DATA_VERSION,
                uri: uri.clone(),
                range,
                implementation_position,
                source_hash: source_hash.to_string(),
                source_generation: source_generation.to_string(),
                configuration_generation: configuration_generation.to_string(),
                kind,
            };
            lenses.push(CodeLens {
                range,
                command: None,
                data: Some(serde_json::to_value(data).map_err(|error| error.to_string())?),
            });
        }
        if lenses.len() > MAX_PROCEDURES * 2 {
            return Ok(Vec::new());
        }
    }
    lenses.sort_by_key(|lens| (lens.range.start.line, lens.range.start.character));
    Ok(lenses)
}

fn computed<T>(
    input: &WorkspaceInput,
    value: Result<T, String>,
    records: Vec<SourceRecord>,
) -> Computed<T> {
    Computed {
        source_generation: input.source_generation,
        configuration_generation: input.configuration_generation,
        value,
        records,
    }
}

pub(crate) fn discover(
    input: WorkspaceInput,
    uri: &Url,
    cancel: &AtomicBool,
) -> Computed<Vec<CodeLens>> {
    if is_cancelled(cancel) {
        return computed(&input, Err(CANCELLATION_MESSAGE.to_owned()), Vec::new());
    }
    let uri = canonical_file_uri(uri);
    if !input_source_is_editable(&input, &uri) {
        return computed(&input, Ok(Vec::new()), Vec::new());
    }
    let owner = match owner_for_input(&input, &uri, cancel) {
        Ok(owner) => owner,
        Err(error) if error == CANCELLATION_MESSAGE => {
            return computed(&input, Err(error), Vec::new());
        }
        Err(_) => return computed(&input, Ok(Vec::new()), Vec::new()),
    };
    if has_invalid_project_selection(&owner.state.context)
        || owner.state.context.override_error.is_some()
        || !input_source_is_readable_with_owner(&input, &uri, &owner)
    {
        return computed(&input, Ok(Vec::new()), Vec::new());
    }
    let (source, record) = match source_for_input_with_owner(&input, &uri, &owner, Some(cancel)) {
        Ok(pair) => pair,
        Err(error) => return computed(&input, Err(error), Vec::new()),
    };
    let result = lenses_for_source(
        &uri,
        &source,
        input.source_generation,
        input.configuration_generation,
    );
    computed(&input, result, vec![record])
}

pub(crate) fn resolve(
    input: WorkspaceInput,
    mut lens: CodeLens,
    cancel: &AtomicBool,
) -> Computed<CodeLens> {
    if is_cancelled(cancel) {
        return computed(&input, Err(CANCELLATION_MESSAGE.to_owned()), Vec::new());
    }
    let Some(data) = lens
        .data
        .as_ref()
        .and_then(|value| serde_json::from_value::<LensData>(value.clone()).ok())
    else {
        return computed(&input, Err("invalid code-lens data".to_owned()), Vec::new());
    };
    if lens.command.is_some()
        || data.version != DATA_VERSION
        || data.range != lens.range
        || data.uri != canonical_file_uri(&data.uri)
        || data.source_generation != input.source_generation.to_string()
        || data.configuration_generation != input.configuration_generation.to_string()
    {
        return computed(
            &input,
            Err("code lens is stale or invalid".to_owned()),
            Vec::new(),
        );
    }
    let found = discover(input.clone(), &data.uri, cancel);
    let records = found.records;
    let expected = match found.value {
        Ok(lenses) => lenses,
        Err(error) => return computed(&input, Err(error), records),
    };
    if !expected.iter().any(|candidate| candidate == &lens) {
        return computed(
            &input,
            Err("code lens no longer identifies a unique routine".to_owned()),
            records,
        );
    }
    let result = match data.kind {
        LensKind::References => {
            let result = queries::references_from_input(
                input.clone(),
                &data.uri,
                data.range.start,
                false,
                cancel,
            );
            (result.value, result.records, "references")
        }
        LensKind::Implementation => {
            let result = queries::navigation_from_input(
                input.clone(),
                &data.uri,
                data.range.start,
                NavigationTarget::Implementation,
                cancel,
            );
            (
                result.value.map(|navigation| navigation.locations),
                result.records,
                "implementations",
            )
        }
    };
    let (locations, query_records, title) = result;
    let mut records = records;
    records.extend(query_records);
    let locations = match locations {
        Ok(locations) if locations.len() <= MAX_LOCATIONS => locations,
        Ok(_) => {
            return computed(
                &input,
                Err("too many code-lens locations".to_owned()),
                records,
            );
        }
        Err(error) => return computed(&input, Err(error), records),
    };
    if data.kind == LensKind::Implementation
        && (locations.len() != 1
            || locations[0].uri != data.uri
            || locations[0].range.start != data.implementation_position)
    {
        return computed(
            &input,
            Err("routine implementation binding was not proven".to_owned()),
            records,
        );
    }
    if is_cancelled(cancel) {
        return computed(&input, Err(CANCELLATION_MESSAGE.to_owned()), records);
    }
    let count = locations.len();
    lens.command = (count != 0).then(|| Command {
        title: format!("{count} {title}"),
        command: "editor.action.showReferences".to_owned(),
        arguments: Some(vec![
            json!(data.uri),
            json!(data.range.start),
            json!(locations),
        ]),
    });
    lens.data = None;
    computed(&input, Ok(lens), records)
}

#[cfg(test)]
mod tests {
    use super::lenses_for_source;
    use lsp_types::Url;

    const SOURCE: &str = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";

    #[test]
    fn advertised_procedure_has_lazy_reference_and_implementation_lenses() {
        let uri = Url::parse("file:///workspace/Provider.pas").unwrap();
        let lenses = lenses_for_source(&uri, SOURCE, 7, 3).unwrap();
        assert_eq!(lenses.len(), 2);
        assert!(
            lenses
                .iter()
                .all(|lens| lens.command.is_none() && lens.data.is_some())
        );
        assert!(
            lenses
                .iter()
                .all(|lens| lens.range.start.line == 2 && lens.range.start.character == 10)
        );
        assert!(lenses.iter().all(|lens| {
            lens.data.as_ref().is_some_and(|data| {
                data["implementationPosition"]["line"] == 4
                    && data["implementationPosition"]["character"] == 10
            })
        }));
        assert!(
            lenses.iter().all(|lens| {
                lens.data.as_ref().is_some_and(|data| {
                    data["sourceHash"].is_string()
                        && data["sourceGeneration"].is_string()
                        && data["configurationGeneration"].is_string()
                })
            }),
            "lens data must survive JavaScript number round-trips"
        );
    }

    #[test]
    fn ambiguous_or_unimplemented_procedure_has_no_lens() {
        let uri = Url::parse("file:///workspace/Provider.pas").unwrap();
        let overload = SOURCE.replace(
            "procedure PublicRoutine;\nimplementation",
            "procedure PublicRoutine;\nprocedure PublicRoutine(A: Integer);\nimplementation",
        );
        assert!(lenses_for_source(&uri, &overload, 7, 3).unwrap().is_empty());
        let missing = SOURCE.replace("procedure PublicRoutine;\nbegin\nend;\n", "");
        assert!(lenses_for_source(&uri, &missing, 7, 3).unwrap().is_empty());
    }

    #[test]
    fn ordinary_comments_do_not_hide_a_unique_exported_procedure() {
        let uri = Url::parse("file:///workspace/Provider.pas").unwrap();
        let commented = SOURCE.replace("interface\n", "interface\n// Public API\n");
        assert_eq!(lenses_for_source(&uri, &commented, 7, 3).unwrap().len(), 2);
    }

    #[test]
    fn simple_procedure_spacing_does_not_hide_the_same_binding() {
        let uri = Url::parse("file:///workspace/Provider.pas").unwrap();
        let spaced = SOURCE.replace("procedure PublicRoutine;", "procedure   PublicRoutine ;");
        assert_eq!(lenses_for_source(&uri, &spaced, 7, 3).unwrap().len(), 2);
    }

    #[test]
    fn too_many_procedures_withholds_all_lenses_instead_of_an_arbitrary_subset() {
        let uri = Url::parse("file:///workspace/Bulk.pas").unwrap();
        let mut source = "unit Bulk;\ninterface\n".to_owned();
        for index in 0..65 {
            source.push_str(&format!("procedure P{index};\n"));
        }
        source.push_str("implementation\n");
        for index in 0..65 {
            source.push_str(&format!("procedure P{index}; begin end;\n"));
        }
        source.push_str("end.\n");
        assert!(lenses_for_source(&uri, &source, 7, 3).unwrap().is_empty());
    }
}
