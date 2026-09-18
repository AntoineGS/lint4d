use lsp_types::{Location, Position, PrepareRenameResponse, Range, TextEdit, Url};
use pascal_lsp::NavigationIndex;
use pascal_lsp::workspace::{Workspace, WorkspaceOptions};
use std::collections::HashMap;
use std::fs;

fn uri(name: &str) -> Url {
    Url::parse(&format!("file:///workspace/{name}.pas")).expect("valid test URI")
}

fn position_of(source: &str, needle: &str, occurrence: usize) -> Position {
    let mut search_from = 0;
    let mut offset = None;
    for _ in 0..=occurrence {
        let relative = source[search_from..]
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} occurrence {occurrence} not found"));
        search_from += relative;
        offset = Some(search_from);
        search_from += needle.len();
    }
    let offset = offset.expect("at least one occurrence");
    let line_start = source[..offset].rfind('\n').map_or(0, |idx| idx + 1);
    Position {
        line: source[..offset]
            .bytes()
            .filter(|&byte| byte == b'\n')
            .count() as u32,
        character: source[line_start..offset].encode_utf16().count() as u32,
    }
}

fn property_position(source: &str, name: &str) -> Position {
    let mut position = position_of(source, &format!("property {name}"), 0);
    position.character += "property ".encode_utf16().count() as u32;
    position
}

fn update(index: &mut NavigationIndex, name: &str, source: &str) -> Url {
    let uri = uri(name);
    index
        .update(uri.clone(), source.to_string())
        .expect("fixture parses");
    uri
}

fn range_of(source: &str, needle: &str, occurrence: usize) -> Range {
    let start = position_of(source, needle, occurrence);
    let end = Position {
        line: start.line,
        character: start.character + needle.encode_utf16().count() as u32,
    };
    Range { start, end }
}

fn range_in(source: &str, surrounding: &str, identifier: &str, occurrence: usize) -> Range {
    range_in_occurrence(source, surrounding, identifier, occurrence, 0)
}

fn range_in_occurrence(
    source: &str,
    surrounding: &str,
    identifier: &str,
    occurrence: usize,
    identifier_occurrence: usize,
) -> Range {
    let surrounding_start = position_of(source, surrounding, occurrence);
    let mut search_from = 0;
    let mut identifier_offset = None;
    for _ in 0..=identifier_occurrence {
        let relative = surrounding[search_from..]
            .find(identifier)
            .expect("identifier is present in surrounding text");
        search_from += relative;
        identifier_offset = Some(search_from);
        search_from += identifier.len();
    }
    let identifier_offset = identifier_offset.expect("identifier occurrence is present");
    let start = Position {
        line: surrounding_start.line,
        character: surrounding_start.character
            + surrounding[..identifier_offset].encode_utf16().count() as u32,
    };
    let end = Position {
        line: start.line,
        character: start.character + identifier.encode_utf16().count() as u32,
    };
    Range { start, end }
}

fn edit_signature(uri: &Url, range: Range, new_text: &str) -> (String, u32, u32, u32, u32, String) {
    (
        uri.to_string(),
        range.start.line,
        range.start.character,
        range.end.line,
        range.end.character,
        new_text.to_owned(),
    )
}

fn exact_edit_signatures(
    edits: &std::collections::HashMap<Url, Vec<TextEdit>>,
) -> Vec<(String, u32, u32, u32, u32, String)> {
    let mut result = edits
        .iter()
        .flat_map(|(uri, document_edits)| {
            document_edits
                .iter()
                .map(|edit| edit_signature(uri, edit.range, &edit.new_text))
        })
        .collect::<Vec<_>>();
    result.sort();
    result
}

fn assert_exact_edits(
    edits: &std::collections::HashMap<Url, Vec<TextEdit>>,
    mut expected: Vec<(Url, Range, String)>,
) {
    let mut expected_signatures = expected
        .drain(..)
        .map(|(uri, range, new_text)| edit_signature(&uri, range, &new_text))
        .collect::<Vec<_>>();
    expected_signatures.sort();
    assert_eq!(exact_edit_signatures(edits), expected_signatures);
}

fn location_signature(location: &Location) -> (String, u32, u32, u32, u32) {
    (
        location.uri.to_string(),
        location.range.start.line,
        location.range.start.character,
        location.range.end.line,
        location.range.end.character,
    )
}

fn assert_exact_locations(locations: &[Location], mut expected: Vec<(Url, Range)>) {
    let mut actual = locations.iter().map(location_signature).collect::<Vec<_>>();
    let mut expected = expected
        .drain(..)
        .map(|(uri, range)| location_signature(&Location { uri, range }))
        .collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

fn apply_edits(source: &str, edits: &[TextEdit]) -> String {
    let mut replacements = edits
        .iter()
        .map(|edit| {
            let start = pascal_lsp::text::position_to_offset(source, edit.range.start)
                .expect("valid edit start");
            let end = pascal_lsp::text::position_to_offset(source, edit.range.end)
                .expect("valid edit end");
            (start, end, edit.new_text.as_str())
        })
        .collect::<Vec<_>>();
    replacements.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));

    let mut result = source.to_owned();
    for (start, end, new_text) in replacements {
        result.replace_range(start..end, new_text);
    }
    result
}

fn assert_document_after_edits(source: &str, edits: &[TextEdit], expected_source: &str) {
    assert_eq!(apply_edits(source, edits), expected_source);
}

#[test]
fn renames_public_class_constant_across_units() {
    let provider_uri = uri("Provider");
    let consumer_uri = uri("Consumer");
    let provider = "unit Provider;
interface
type TLog = class
public const kSQLDebugFile = 'debug.log';
end;
implementation
end.
";
    let consumer = "unit Consumer;
interface
uses Provider;
implementation
procedure Run;
begin
  WriteLn(TLog.kSQLDebugFile);
end;
end.
";

    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_string())
        .expect("provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_string())
        .expect("consumer parses");
    let mut bindings = HashMap::new();
    bindings.insert("Provider".to_string(), provider_uri.clone());
    index.bind_imports(&consumer_uri, bindings);

    let edits = index
        .rename_edits(&provider_uri, Position::new(3, 14), "K_SQL_DEBUG_FILE")
        .expect("rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                provider_uri.clone(),
                range_of(provider, "kSQLDebugFile", 0),
                "K_SQL_DEBUG_FILE".to_owned(),
            ),
            (
                consumer_uri.clone(),
                range_of(consumer, "kSQLDebugFile", 0),
                "K_SQL_DEBUG_FILE".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        provider,
        &edits[&provider_uri],
        &provider.replacen("kSQLDebugFile", "K_SQL_DEBUG_FILE", 1),
    );
    assert_document_after_edits(
        consumer,
        &edits[&consumer_uri],
        &consumer.replacen("kSQLDebugFile", "K_SQL_DEBUG_FILE", 1),
    );
}

#[test]
fn renames_record_helper_members_and_uses() {
    let source = r#"unit HelperRename;
interface
type
  TPoint = record
  end;
  TPointHelper = record helper for TPoint
    procedure Offset;
  end;

procedure Run;

implementation

procedure TPointHelper.Offset;
begin
end;

procedure Run;
var
  Point: TPoint;
begin
  Point.Offset;
end;

end.
"#;
    let source_uri = uri("HelperRename");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("helper rename source parses");

    let edits = index
        .rename_edits(&source_uri, position_of(source, "Offset", 0), "Shift")
        .expect("helper rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_of(source, "Offset", 0),
                "Shift".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "Offset", 1),
                "Shift".to_owned(),
            ),
            (
                source_uri,
                range_of(source, "Offset", 2),
                "Shift".to_owned(),
            ),
        ],
    );
}

#[test]
fn renames_imported_class_helper_members_and_uses() {
    let target = r#"unit HelperTarget;
interface
type
  TWidget = class
  end;
implementation
end.
"#;
    let helper = r#"unit WidgetHelper;
interface
uses HelperTarget;
type
  TWidgetHelper = class helper for TWidget
    procedure Touch;
  end;
implementation
procedure TWidgetHelper.Touch;
begin
end;
end.
"#;
    let consumer = r#"unit HelperConsumer;
interface
uses HelperTarget, WidgetHelper;
procedure Run;
implementation
procedure Run;
var
  Widget: TWidget;
begin
  Widget.Touch;
end;
end.
"#;
    let mut index = NavigationIndex::new();
    let target_uri = update(&mut index, "HelperTarget", target);
    let helper_uri = update(&mut index, "WidgetHelper", helper);
    let consumer_uri = update(&mut index, "HelperConsumer", consumer);

    let edits = index
        .rename_edits(&helper_uri, position_of(helper, "Touch", 0), "Activate")
        .expect("imported helper rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                helper_uri.clone(),
                range_of(helper, "Touch", 0),
                "Activate".to_owned(),
            ),
            (
                helper_uri,
                range_of(helper, "Touch", 1),
                "Activate".to_owned(),
            ),
            (
                consumer_uri,
                range_of(consumer, "Touch", 0),
                "Activate".to_owned(),
            ),
        ],
    );
    assert!(!edits.contains_key(&target_uri));
}

#[test]
fn renames_helper_member_instead_of_colliding_helped_type_member() {
    let source = r#"unit HelperRenamePrecedence;
interface
type
  TWidget = record
    Value: Integer;
  end;
  TWidgetHelper = record helper for TWidget
    property Value: Integer;
  end;

implementation

procedure Run;
var
  Widget: TWidget;
begin
  WriteLn(Widget.Value);
end;

end.
"#;
    let source_uri = uri("HelperRenamePrecedence");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("helper rename precedence source parses");

    let edits = index
        .rename_edits(&source_uri, property_position(source, "Value"), "Activate")
        .expect("helper rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_in(source, "property Value", "Value", 0),
                "Activate".to_owned(),
            ),
            (
                source_uri,
                range_in(source, "Widget.Value", "Value", 0),
                "Activate".to_owned(),
            ),
        ],
    );
}

#[test]
fn binding_locations_filter_both_parameter_declaration_sites() {
    let source = "unit ParameterReferences;\ninterface\nprocedure Run(Value: Integer);\nimplementation\nprocedure Run(Value: Integer);\nbegin\n  Value := Value + 1;\nend;\nend.\n";
    let uri = uri("ParameterReferences");
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_string())
        .expect("parameter source parses");

    let without_declarations = index
        .binding_locations(&uri, position_of(source, "Value", 0), false)
        .expect("parameter references are resolved");
    assert_eq!(without_declarations.len(), 2);
    assert!(
        without_declarations
            .iter()
            .all(|location| location.range.start.line == 6)
    );

    let with_declarations = index
        .binding_locations(&uri, position_of(source, "Value", 0), true)
        .expect("parameter declarations are resolved");
    assert_eq!(with_declarations.len(), 4);
}

#[test]
fn unicode_identifier_recovery_refuses_a_partial_public_rename() {
    let provider = "unit Provider;
interface
const badConst = 1;
implementation
end.
";
    let consumer = "unit Consumer;
interface
uses Provider;
implementation
procedure Use;
var badConsté: Integer;
begin
  badConsté := 2;
  WriteLn(badConst);
  WriteLn(badConsté);
end;
end.
";
    let mut index = NavigationIndex::new();
    let provider_uri = update(&mut index, "UnicodeProvider", provider);
    let consumer_uri = update(&mut index, "UnicodeConsumer", consumer);
    index.bind_imports(
        &consumer_uri,
        [("Provider".to_owned(), provider_uri.clone())],
    );

    let result = index.rename_edits(
        &provider_uri,
        position_of(provider, "badConst", 0),
        "RENAMED_CONST",
    );

    assert!(
        result.is_err(),
        "unsupported Unicode identifier recovery must not authorize a partial rename"
    );
}

#[test]
fn selecting_a_reference_uses_the_same_binding_as_its_declaration() {
    let provider = "unit Provider;
interface
type TLog = class
public const kSQLDebugFile = 'debug.log';
end;
implementation
end.
";
    let consumer = "unit Consumer;
interface
uses Provider;
implementation
procedure Run;
begin
  WriteLn(TLog.kSQLDebugFile);
end;
end.
";
    let mut index = NavigationIndex::new();
    let provider_uri = update(&mut index, "Provider", provider);
    let consumer_uri = update(&mut index, "Consumer", consumer);

    let edits = index
        .rename_edits(
            &consumer_uri,
            position_of(consumer, "kSQLDebugFile", 0),
            "K_SQL_DEBUG_FILE",
        )
        .expect("reference rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                provider_uri.clone(),
                range_of(provider, "kSQLDebugFile", 0),
                "K_SQL_DEBUG_FILE".to_owned(),
            ),
            (
                consumer_uri.clone(),
                range_of(consumer, "kSQLDebugFile", 0),
                "K_SQL_DEBUG_FILE".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        provider,
        &edits[&provider_uri],
        &provider.replacen("kSQLDebugFile", "K_SQL_DEBUG_FILE", 1),
    );
    assert_document_after_edits(
        consumer,
        &edits[&consumer_uri],
        &consumer.replacen("kSQLDebugFile", "K_SQL_DEBUG_FILE", 1),
    );
}

#[test]
fn unit_bindings_are_rejected_without_rename_file_support() {
    let provider = "unit Provider;
interface
implementation
end.
";
    let consumer = "unit Consumer;
interface
uses Provider;
implementation
end.
";
    let mut index = NavigationIndex::new();
    let provider_uri = update(&mut index, "Provider", provider);
    let consumer_uri = update(&mut index, "Consumer", consumer);

    let declaration_position = position_of(provider, "Provider", 0);
    assert!(
        index
            .prepare_rename(&provider_uri, declaration_position)
            .is_err(),
        "unit declaration rename requires RenameFile support"
    );
    assert!(
        index
            .rename_edits(&provider_uri, declaration_position, "RenamedProvider")
            .is_err(),
        "unit declaration rename must not return text edits"
    );
    assert!(
        index
            .rename_edits(
                &consumer_uri,
                position_of(consumer, "Provider", 0),
                "RenamedProvider",
            )
            .is_err(),
        "uses unit rename must not return text edits"
    );
}

#[test]
fn imported_reference_capture_by_a_local_name_is_rejected() {
    let provider = "unit ImportedCaptureProvider;
interface
const Value = 1;
implementation
end.
";
    let consumer = "unit ImportedCaptureConsumer;
interface
uses ImportedCaptureProvider;
implementation
procedure Run;
var
  NewValue: Integer;
begin
  NewValue := Value;
end;
end.
";
    let mut index = NavigationIndex::new();
    let provider_uri = update(&mut index, "ImportedCaptureProvider", provider);
    let consumer_uri = update(&mut index, "ImportedCaptureConsumer", consumer);
    let mut bindings = HashMap::new();
    bindings.insert("ImportedCaptureProvider".to_string(), provider_uri.clone());
    index.bind_imports(&consumer_uri, bindings);

    assert!(
        index
            .rename_edits(&provider_uri, position_of(provider, "Value", 0), "NewValue")
            .is_err(),
        "a consumer local must not capture an imported binding"
    );
}

#[test]
fn local_rename_can_shadow_an_outer_name_without_capturing_references() {
    let source = "unit LocalOuterName;
interface
const NewValue = 0;
procedure Run;
implementation
procedure Run;
var
  Value: Integer;
begin
  Value := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "LocalOuterName", source);

    let mut selected = position_of(source, "  Value", 0);
    selected.character += 2;
    let edits = index
        .rename_edits(&source_uri, selected, "NewValue")
        .expect("local binding may shadow an outer declaration");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_in(source, "  Value: Integer", "Value", 0),
                "NewValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in(source, "  Value := 1", "Value", 0),
                "NewValue".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &edits[&source_uri],
        &source.replacen("  Value", "  NewValue", 2),
    );
}

#[test]
fn disjoint_local_new_name_references_are_not_reverse_capture() {
    let source = "unit DisjointNewName;
interface
implementation
procedure Run;
var Value: Integer;
begin Value := 1; end;
procedure Other;
var Count: Integer;
begin Count := 2; end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "DisjointNewName", source);

    let edits = index
        .rename_edits(
            &source_uri,
            position_of(source, "Value: Integer", 0),
            "Count",
        )
        .expect("a disjoint local reference is not captured");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_in(source, "Value: Integer", "Value", 0),
                "Count".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in(source, "Value := 1", "Value", 0),
                "Count".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &edits[&source_uri],
        &source
            .replacen("Value: Integer", "Count: Integer", 1)
            .replacen("Value := 1", "Count := 1", 1),
    );
}

#[test]
fn class_constant_rename_does_not_capture_explicitly_disjoint_local() {
    let source = "unit ClassConstantLocalName;
interface
type TBox = class
 public const OldValue = 1;
end;
implementation
procedure Run;
var NewValue: Integer;
begin
 NewValue := 7;
 WriteLn(TBox.OldValue, NewValue);
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "ClassConstantLocalName", source);

    let edits = index
        .rename_edits(
            &source_uri,
            position_of(source, "OldValue = 1", 0),
            "NewValue",
        )
        .expect("an explicitly qualified class constant is disjoint from a free local");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_in(source, " public const OldValue = 1", "OldValue", 0),
                "NewValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in(source, " WriteLn(TBox.OldValue, NewValue)", "OldValue", 0),
                "NewValue".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &edits[&source_uri],
        &source.replacen("OldValue = 1", "NewValue = 1", 1).replacen(
            "TBox.OldValue",
            "TBox.NewValue",
            1,
        ),
    );
}

#[test]
fn unresolved_new_name_reference_in_target_scope_is_rejected() {
    let source = "unit UnresolvedNewName;
interface
implementation
procedure Run;
var Value: Integer;
begin
 Value := 1;
 WriteLn(NewValue);
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "UnresolvedNewName", source);

    assert!(
        index
            .rename_edits(
                &source_uri,
                position_of(source, "Value: Integer", 0),
                "NewValue",
            )
            .is_err(),
        "an unresolved reference in the target scope could be captured by the rename"
    );
}

#[test]
fn inherited_proposed_name_reference_rejects_reverse_capture() {
    let source = "unit Probe;
interface
const NewValue = 7;
type TBase = class
 public const OldValue = 1;
end;
TChild = class(TBase)
 procedure Run;
end;
implementation
procedure TChild.Run;
begin WriteLn(NewValue); end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "InheritedProposedNameCapture", source);

    assert!(
        index
            .rename_edits(
                &source_uri,
                position_of(source, "OldValue = 1", 0),
                "NewValue",
            )
            .is_err(),
        "an inherited class member must not capture a global proposed-name reference"
    );
}

#[test]
fn with_proposed_name_reference_rejects_reverse_capture() {
    let source = "unit Probe;
interface
const NewValue = 7;
type TBox = class
 public const OldValue = 1;
end;
implementation
procedure Run(Box: TBox);
begin
 with Box do WriteLn(NewValue);
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "WithProposedNameCapture", source);

    assert!(
        index
            .rename_edits(
                &source_uri,
                position_of(source, "OldValue = 1", 0),
                "NewValue",
            )
            .is_err(),
        "a class member must not capture a global proposed-name reference through with"
    );
}

#[test]
fn local_rename_rejects_reverse_capture_of_existing_new_name_reference() {
    let source = "unit ReverseCaptureVariable;
interface
const NewValue = 7;
implementation
procedure Run;
var Value: Integer;
begin
 Value := NewValue;
 WriteLn(Value);
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "ReverseCaptureVariable", source);

    assert!(
        index
            .rename_edits(
                &source_uri,
                position_of(source, "Value: Integer", 0),
                "NewValue",
            )
            .is_err(),
        "an existing reference to the proposed local name must not be captured"
    );
}

#[test]
fn local_constant_rename_rejects_reverse_capture_of_existing_new_name_reference() {
    let source = "unit ReverseCaptureConstant;
interface
const NewValue = 7;
implementation
procedure Run;
const Value = 1;
begin
 WriteLn(Value, NewValue);
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "ReverseCaptureConstant", source);

    assert!(
        index
            .rename_edits(&source_uri, position_of(source, "Value = 1", 0), "NewValue",)
            .is_err(),
        "an existing constant reference must not be captured by the renamed local constant"
    );
}

#[test]
fn cross_unit_same_named_class_owner_rejects_inherited_member_rename() {
    let provider = "unit Provider;
interface
type TBox = class
 public const Value = 1;
end;
implementation
end.
";
    let consumer = "unit Consumer;
interface
uses Provider;
const Value = 100;
type TBox = class(Provider.TBox)
 procedure Run;
end;
implementation
procedure TBox.Run;
begin WriteLn(Value); end;
end.
";
    let mut index = NavigationIndex::new();
    let provider_uri = update(&mut index, "Provider", provider);
    let consumer_uri = update(&mut index, "Consumer", consumer);
    let mut bindings = HashMap::new();
    bindings.insert("Provider".to_owned(), provider_uri.clone());
    index.bind_imports(&consumer_uri, bindings);
    let selected = position_of(provider, "Value = 1", 0);

    assert!(
        index.prepare_rename(&provider_uri, selected).is_err(),
        "same-named class owners in different units must not be treated as identical"
    );
    assert!(
        index
            .rename_edits(&provider_uri, selected, "Count")
            .is_err(),
        "inherited member lookup across same-named units must be rejected"
    );
}

#[test]
fn cross_unit_same_named_class_owner_rejects_virtual_override_rename() {
    let provider = "unit Provider;
interface
type TBox = class
 procedure Work; virtual;
end;
implementation
procedure TBox.Work; begin end;
end.
";
    let consumer = "unit Consumer;
interface
uses Provider;
type TBox = class(Provider.TBox)
 procedure Work; override;
end;
implementation
procedure TBox.Work; begin end;
end.
";
    let mut index = NavigationIndex::new();
    let provider_uri = update(&mut index, "Provider", provider);
    let consumer_uri = update(&mut index, "Consumer", consumer);
    let mut bindings = HashMap::new();
    bindings.insert("Provider".to_owned(), provider_uri.clone());
    index.bind_imports(&consumer_uri, bindings);
    let selected = position_of(provider, "Work; virtual", 0);

    assert!(
        index.prepare_rename(&provider_uri, selected).is_err(),
        "same-named class owners in different units must not hide overrides"
    );
    assert!(
        index
            .rename_edits(&provider_uri, selected, "RunWork")
            .is_err(),
        "cross-unit virtual override families must be rejected"
    );
}

#[test]
fn ordinary_local_rename_is_case_insensitive_and_scope_limited() {
    let source = "unit LocalRename;
interface
procedure Run;
procedure Other;
implementation
procedure Run;
var
  Value: Integer;
begin
  vAlUe := 1;
  Value := Value + 1;
end;
procedure Other;
var
  Value: Integer;
begin
  Value := 2;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "LocalRename", source);

    let edits = index
        .rename_edits(&source_uri, position_of(source, "Value", 0), "RenamedValue")
        .expect("local rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_of(source, "Value", 0),
                "RenamedValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "vAlUe", 0),
                "RenamedValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in_occurrence(source, "  Value := Value + 1", "Value", 0, 0),
                "RenamedValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in_occurrence(source, "  Value := Value + 1", "Value", 0, 1),
                "RenamedValue".to_owned(),
            ),
        ],
    );
    let expected_source = source
        .replacen("  Value: Integer", "  RenamedValue: Integer", 1)
        .replacen("  vAlUe := 1", "  RenamedValue := 1", 1)
        .replacen(
            "  Value := Value + 1",
            "  RenamedValue := RenamedValue + 1",
            1,
        );
    assert_document_after_edits(source, &edits[&source_uri], &expected_source);
}

#[test]
fn nested_shadowing_is_left_alone() {
    let source = "unit NestedShadowing;
interface
procedure Run;
implementation
procedure Run;
var
  Value: Integer;
  procedure Nested;
  var
    Value: Integer;
  begin
    Value := 2;
  end;
begin
  Value := 1;
  Nested;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "NestedShadowing", source);

    let edits = index
        .rename_edits(&source_uri, position_of(source, "Value", 0), "RenamedValue")
        .expect("outer local rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_in(source, "  Value: Integer", "Value", 0),
                "RenamedValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in(source, "  Value := 1", "Value", 0),
                "RenamedValue".to_owned(),
            ),
        ],
    );
    let expected_source = source
        .replacen("  Value: Integer", "  RenamedValue: Integer", 1)
        .replacen("  Value := 1", "  RenamedValue := 1", 1);
    assert_document_after_edits(source, &edits[&source_uri], &expected_source);
}

#[test]
fn nested_local_capture_aborts_without_partial_edits() {
    let source = "unit NestedCapture;
interface
procedure Run;
implementation
procedure Run;
var
  Value: Integer;
  procedure Nested;
  var
    RenamedValue: Integer;
  begin
    Value := 2;
  end;
begin
  Value := 1;
  Nested;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "NestedCapture", source);

    let result = index.rename_edits(&source_uri, position_of(source, "Value", 0), "RenamedValue");
    assert!(result.is_err(), "nested local capture must be rejected");
}

#[test]
fn qualified_class_and_self_member_uses_are_renamed() {
    let source = "unit QualifiedMembers;
interface
type
  TWidget = class
    FValue: Integer;
    procedure Run;
  end;
implementation
procedure TWidget.Run;
begin
  Self.FValue := 1;
  TWidget.FValue := 2;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "QualifiedMembers", source);

    let edits = index
        .rename_edits(&source_uri, position_of(source, "FValue", 0), "FCount")
        .expect("qualified member rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_of(source, "FValue", 0),
                "FCount".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "FValue", 1),
                "FCount".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "FValue", 2),
                "FCount".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &edits[&source_uri],
        &source.replacen("FValue", "FCount", 3),
    );
}

#[test]
fn function_result_member_uses_are_renamed_without_guessing_a_type() {
    let source = "unit FunctionResultMemberRename;
interface
type
  TObj = class
    Member: Integer;
  end;
function MakeValue: TObj;
implementation
function MakeValue: TObj;
begin
  Result := TObj.Create;
end;
procedure Caller;
begin
  MakeValue().Member := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "FunctionResultMemberRename", source);

    let edits = index
        .rename_edits(
            &source_uri,
            position_of(source, "Member", 1),
            "RenamedMember",
        )
        .expect("function result member rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_of(source, "Member", 1),
                "RenamedMember".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "Member", 2),
                "RenamedMember".to_owned(),
            ),
        ],
    );
}

#[test]
fn implementation_only_function_result_rename_excludes_its_body_type_member() {
    let provider_uri = uri("ImplementationOnlyRenameSource");
    let consumer_uri = uri("ImplementationOnlyRenameConsumer");
    let provider = "unit ImplementationOnlyRenameSource;
interface
type
  TResult = class
    Name: Integer;
  end;
end.
";
    let consumer = "unit ImplementationOnlyRenameConsumer;
interface
uses ImplementationOnlyRenameSource;
implementation
function Make: TResult;
type
  TResult = record
    Name: string;
  end;
begin
  Result.Name := '';
end;
procedure Caller;
begin
  Make().Name := 1;
end;
end.
";

    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("rename provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("rename consumer parses");
    let mut bindings = HashMap::new();
    bindings.insert(
        "ImplementationOnlyRenameSource".to_owned(),
        provider_uri.clone(),
    );
    index.bind_imports(&consumer_uri, bindings);

    let edits = index
        .rename_edits(
            &provider_uri,
            position_of(provider, "Name", 0),
            "RenamedName",
        )
        .expect("implementation-only result rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                provider_uri.clone(),
                range_of(provider, "Name", 0),
                "RenamedName".to_owned(),
            ),
            (
                consumer_uri.clone(),
                range_of(consumer, "Name", 1),
                "RenamedName".to_owned(),
            ),
            (
                consumer_uri.clone(),
                range_of(consumer, "Name", 2),
                "RenamedName".to_owned(),
            ),
        ],
    );
}

#[test]
fn nested_function_result_rename_keeps_the_declaration_type_scope() {
    let provider_uri = uri("NestedResultRenameSource");
    let consumer_uri = uri("NestedResultRenameConsumer");
    let provider = r#"unit NestedResultRenameSource;
interface
type
  TResult = class
    Name: Integer;
  end;
end.
"#;
    let consumer = r#"unit NestedResultRenameConsumer;
interface
uses NestedResultRenameSource;
implementation
procedure Outer;
type
  TResult = record
    Name: string;
  end;
  function Make: TResult;
  type
    TResult = record
      Name: Boolean;
    end;
  begin
    Result.Name := False;
  end;
begin
  Make().Name := '';
end;
end.
"#;

    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("nested rename provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("nested rename consumer parses");
    let mut bindings = HashMap::new();
    bindings.insert("NestedResultRenameSource".to_owned(), provider_uri.clone());
    index.bind_imports(&consumer_uri, bindings);

    let edits = index
        .rename_edits(
            &consumer_uri,
            position_of(consumer, "Name", 0),
            "RenamedName",
        )
        .expect("nested result rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                consumer_uri.clone(),
                range_of(consumer, "Name", 0),
                "RenamedName".to_owned(),
            ),
            (
                consumer_uri.clone(),
                range_of(consumer, "Name", 2),
                "RenamedName".to_owned(),
            ),
            (
                consumer_uri,
                range_of(consumer, "Name", 3),
                "RenamedName".to_owned(),
            ),
        ],
    );
}

#[test]
fn shadowed_qualified_cast_type_root_does_not_authorize_a_member_rename() {
    let provider_uri = uri("ShadowedCastRenameSource");
    let consumer_uri = uri("ShadowedCastRenameConsumer");
    let provider = "unit ShadowedCastRenameSource;
interface
type
  TResult = class
    Member: Integer;
  end;
end.
";
    let consumer = "unit ShadowedCastRenameConsumer;
interface
uses ShadowedCastRenameSource;
type
  TWidget = class
  end;
procedure Caller;
var
  Obj: TWidget;
  ShadowedCastRenameSource: Integer;
begin
  (Obj as ShadowedCastRenameSource.TResult).Member := 1;
end;
end.
";

    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("shadowed cast rename provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("shadowed cast rename consumer parses");
    let mut bindings = HashMap::new();
    bindings.insert("ShadowedCastRenameSource".to_owned(), provider_uri.clone());
    index.bind_imports(&consumer_uri, bindings);

    assert!(
        index
            .rename_edits(
                &provider_uri,
                position_of(provider, "Member", 0),
                "RenamedMember",
            )
            .is_err(),
        "a shadowed cast root must not authorize a partial member rename"
    );
}

#[test]
fn invalid_cast_variable_rhs_does_not_authorize_a_member_rename() {
    let source = "unit InvalidCastVariableRename;
interface
type
  TWidget = class
    Member: Integer;
  end;
  TOther = class
    Member: Integer;
  end;
procedure Caller;
var
  Obj: TWidget;
  OtherObj: TOther;
begin
  Obj.Member := 1;
  (Obj as OtherObj).Member := 2;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "InvalidCastVariableRename", source);

    assert!(
        index
            .rename_edits(
                &source_uri,
                position_of(source, "Member", 1),
                "RenamedMember"
            )
            .is_err(),
        "an invalid cast receiver must not produce a partial member rename"
    );
}

#[test]
fn unrelated_class_homonyms_are_not_renamed() {
    let source = "unit ClassHomonyms;
interface
type
  TOne = class
    Value: Integer;
  end;
  TTwo = class
    Value: Integer;
  end;
  TCaller = class
    One: TOne;
    Two: TTwo;
    procedure Run;
  end;
implementation
procedure TCaller.Run;
begin
  One.Value := 1;
  Two.Value := 2;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "ClassHomonyms", source);

    let edits = index
        .rename_edits(&source_uri, position_of(source, "Value", 0), "Count")
        .expect("homonym rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_of(source, "Value", 0),
                "Count".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "Value", 2),
                "Count".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &edits[&source_uri],
        &source
            .replacen("Value", "Count", 1)
            .replacen("One.Value", "One.Count", 1),
    );
}

fn class_field_cache_source(first_before_second: bool) -> String {
    let first = "  TFirst = class\n    FValue: Integer;\n    property Value: Integer read FValue;\n    procedure Touch;\n  end;\n";
    let second = "  TSecond = class\n    FValue: Integer;\n  end;\n";
    let declarations = if first_before_second {
        format!("{first}{second}")
    } else {
        format!("{second}{first}")
    };
    format!(
        "unit ClassFieldCache;\ninterface\ntype\n{declarations}var\n  FValue: Integer;\nimplementation\nprocedure TFirst.Touch;\nvar\n  FValue: Integer;\nbegin\n  FValue := 1;\n  Self.FValue := 2;\n  TFirst.FValue := 3;\nend;\nprocedure GlobalTouch;\nbegin\n  FValue := 4;\nend;\nend.\n"
    )
}

#[test]
fn class_field_references_and_renames_do_not_share_unqualified_cache() {
    for (case_name, first_before_second) in [("first-first", true), ("second-first", false)] {
        let source = class_field_cache_source(first_before_second);
        let mut index = NavigationIndex::new();
        let source_uri = update(&mut index, case_name, &source);
        let target_declaration_occurrence = if first_before_second { 0 } else { 1 };
        let target_position = range_in(
            &source,
            "    FValue: Integer",
            "FValue",
            target_declaration_occurrence,
        )
        .start;
        let expected_target = vec![
            (
                source_uri.clone(),
                range_in(
                    &source,
                    "    FValue: Integer",
                    "FValue",
                    target_declaration_occurrence,
                ),
            ),
            (
                source_uri.clone(),
                range_in(&source, "property Value: Integer read FValue", "FValue", 0),
            ),
            (
                source_uri.clone(),
                range_in(&source, "Self.FValue", "FValue", 0),
            ),
            (
                source_uri.clone(),
                range_in(&source, "TFirst.FValue", "FValue", 0),
            ),
        ];

        let without_declaration = index
            .binding_locations(&source_uri, target_position, false)
            .expect("class field references without declarations resolve");
        assert_exact_locations(&without_declaration, expected_target[1..].to_vec());

        let with_declaration = index
            .binding_locations(&source_uri, target_position, true)
            .expect("class field references with declarations resolve");
        assert_exact_locations(&with_declaration, expected_target.clone());

        let edits = index
            .rename_edits(&source_uri, target_position, "FChanged")
            .expect("class field rename resolves");
        assert_exact_edits(
            &edits,
            expected_target
                .into_iter()
                .map(|(uri, range)| (uri, range, "FChanged".to_owned()))
                .collect(),
        );
    }
}

#[test]
fn class_field_cache_does_not_merge_a_distinct_record_owner() {
    let record = "  TRecord = record\n    FValue: Integer;\n  end;\n";
    let class = "  TClass = class\n    FValue: Integer;\n    property Value: Integer read FValue;\n  end;\n";

    for (case_name, record_before_class) in [("record-first", true), ("class-first", false)] {
        let declarations = if record_before_class {
            format!("{record}{class}")
        } else {
            format!("{class}{record}")
        };
        let source = format!(
            "unit RecordClassCache;\ninterface\ntype\n{declarations}implementation\nend.\n"
        );
        let mut index = NavigationIndex::new();
        let source_uri = update(&mut index, case_name, &source);
        let class_declaration_occurrence = if record_before_class { 1 } else { 0 };
        let property_occurrence = if record_before_class { 2 } else { 1 };
        let target = range_in(
            &source,
            "    FValue: Integer",
            "FValue",
            class_declaration_occurrence,
        );
        let property = range_in(&source, "property Value: Integer read FValue", "FValue", 0);
        assert_eq!(
            position_of(&source, "FValue", property_occurrence),
            property.start,
            "fixture occurrence accounting must select the class accessor"
        );

        let without_declaration = index
            .binding_locations(&source_uri, target.start, false)
            .expect("class field references without declarations resolve");
        assert_exact_locations(&without_declaration, vec![(source_uri.clone(), property)]);

        let with_declaration = index
            .binding_locations(&source_uri, target.start, true)
            .expect("class field references with declarations resolve");
        assert_exact_locations(
            &with_declaration,
            vec![(source_uri.clone(), target), (source_uri.clone(), property)],
        );

        let edits = index
            .rename_edits(&source_uri, target.start, "FChanged")
            .expect("class field rename resolves");
        assert_exact_edits(
            &edits,
            vec![
                (source_uri.clone(), target, "FChanged".to_owned()),
                (source_uri, property, "FChanged".to_owned()),
            ],
        );
    }
}

#[test]
fn comments_and_strings_are_not_rename_occurrences() {
    let source = "unit RenameNoise;
interface
procedure Run;
implementation
procedure Run;
var
  Value: Integer;
begin
  // Value
  Value := 1;
  WriteLn('Value');
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "RenameNoise", source);

    let edits = index
        .rename_edits(&source_uri, position_of(source, "Value", 0), "RenamedValue")
        .expect("noise-aware rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_of(source, "Value", 0),
                "RenamedValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in(source, "  Value := 1", "Value", 0),
                "RenamedValue".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &edits[&source_uri],
        &source
            .replacen("Value: Integer", "RenamedValue: Integer", 1)
            .replacen("Value := 1", "RenamedValue := 1", 1),
    );
}

#[test]
fn invalid_and_keyword_new_names_are_rejected() {
    let source = "unit InvalidRename;
interface
procedure Run;
implementation
procedure Run;
var
  Value: Integer;
begin
  Value := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "InvalidRename", source);

    for new_name in ["", "begin", "read", "bad-name", "123Value", "Value "] {
        assert!(
            index
                .rename_edits(&source_uri, position_of(source, "Value", 0), new_name,)
                .is_err(),
            "{new_name:?} must be rejected"
        );
    }
}

#[test]
fn case_insensitive_declaration_collision_is_rejected() {
    let source = "unit RenameCollision;
interface
procedure Run;
implementation
procedure Run;
var
  Value, Other: Integer;
begin
  Value := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "RenameCollision", source);

    assert!(
        index
            .rename_edits(&source_uri, position_of(source, "Value", 0), "other",)
            .is_err(),
        "case-insensitive same-scope collision must be rejected"
    );
}

#[test]
fn property_and_getter_bindings_are_separate() {
    let source = "unit PropertyRename;
interface
type
  TConfig = class
    FValue: Integer;
    function GetValue: Integer;
    property Value: Integer read GetValue;
    procedure Run;
  end;
implementation
function TConfig.GetValue: Integer;
begin
  Result := FValue;
end;
procedure TConfig.Run;
begin
  Self.Value := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "PropertyRename", source);

    let property_edits = index
        .rename_edits(
            &source_uri,
            property_position(source, "Value"),
            "RenamedValue",
        )
        .expect("property rename succeeds");
    assert_exact_edits(
        &property_edits,
        vec![
            (
                source_uri.clone(),
                range_in(source, "property Value: Integer", "Value", 0),
                "RenamedValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in(source, "Self.Value", "Value", 0),
                "RenamedValue".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &property_edits[&source_uri],
        &source
            .replacen("property Value", "property RenamedValue", 1)
            .replacen("Self.Value", "Self.RenamedValue", 1),
    );

    let getter_edits = index
        .rename_edits(&source_uri, position_of(source, "GetValue", 0), "ReadValue")
        .expect("getter rename succeeds");
    assert_exact_edits(
        &getter_edits,
        vec![
            (
                source_uri.clone(),
                range_of(source, "GetValue", 0),
                "ReadValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "GetValue", 1),
                "ReadValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "GetValue", 2),
                "ReadValue".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &getter_edits[&source_uri],
        &source
            .replacen("function GetValue:", "function ReadValue:", 1)
            .replacen("read GetValue", "read ReadValue", 1)
            .replacen(
                "function TConfig.GetValue:",
                "function TConfig.ReadValue:",
                1,
            ),
    );
}

#[test]
fn prepare_rename_returns_the_selected_original_source_range() {
    let source = "unit PrepareRename;
interface
procedure Run;
implementation
procedure Run;
var
  Value: Integer;
begin
  Value := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "PrepareRename", source);
    let selected = position_of(source, "Value", 1);

    let response = index
        .prepare_rename(&source_uri, selected)
        .expect("prepare rename succeeds");
    match response {
        PrepareRenameResponse::Range(range) => {
            assert_eq!(range.start, selected);
            assert_eq!(range.end.character, selected.character + 5);
        }
        other => panic!("unexpected prepare response: {other:?}"),
    }
}

#[test]
fn utf16_positions_and_crlf_source_spans_are_preserved() {
    let source = "unit Utf16Rename;\r\ninterface\r\nprocedure Run;\r\nimplementation\r\nprocedure Run;\r\nvar\r\n  é, Value: Integer;\r\nbegin\r\n  é := 1; Value := 2;\r\nend;\r\nend.\r\n";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "Utf16Rename", source);
    let selected = position_of(source, "Value", 0);

    let edits = index
        .rename_edits(&source_uri, selected, "Renamed")
        .expect("UTF-16/CRLF rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_of(source, "Value", 0),
                "Renamed".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "Value", 1),
                "Renamed".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &edits[&source_uri],
        &source.replacen("Value", "Renamed", 2),
    );
}

#[test]
fn overloaded_reference_aborts_without_partial_edits() {
    let source = "unit OverloadedRename;
interface
procedure Do(Value: Integer); overload;
procedure Do(Value: string); overload;
procedure Run;
implementation
procedure Do(Value: Integer);
begin
  Value := 1;
end;
procedure Do(Value: string);
begin
  Value := 'x';
end;
procedure Run;
begin
  Do(1);
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "OverloadedRename", source);

    assert!(
        index
            .rename_edits(&source_uri, position_of(source, "Do", 0), "RenamedDo",)
            .is_err(),
        "an overload use that cannot be disambiguated must abort the rename"
    );
}

#[test]
fn selecting_one_overload_without_a_call_still_aborts_conservatively() {
    let source = "unit OverloadedDeclarationRename;
interface
procedure Do(Value: Integer); overload;
procedure Do(Value: string); overload;
implementation
procedure Do(Value: Integer);
begin
end;
procedure Do(Value: string);
begin
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "OverloadedDeclarationRename", source);

    assert!(
        index
            .rename_edits(&source_uri, position_of(source, "Do", 0), "RenamedDo",)
            .is_err(),
        "overloaded declarations must not yield a partial rename"
    );
}

#[test]
fn with_member_reference_aborts_without_partial_edits() {
    let source = "unit WithRename;
interface
type
  TConfig = class
    Value: Integer;
    procedure Run;
  end;
implementation
procedure TConfig.Run;
begin
  with Self do
    Value := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "WithRename", source);

    assert!(
        index
            .rename_edits(&source_uri, position_of(source, "Value", 0), "RenamedValue",)
            .is_err(),
        "with references must not produce a partial rename"
    );
}

#[test]
fn with_member_homonym_fallback_aborts_without_partial_edits() {
    let source = "unit WithHomonymRename;
interface
type TBox = class
 Value: Integer;
end;
implementation
procedure Run(Box: TBox);
var Value: Integer;
begin
 with Box do Value := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "WithHomonymRename", source);

    assert!(
        index
            .rename_edits(
                &source_uri,
                position_of(source, "Value: Integer", 0),
                "Count",
            )
            .is_err(),
        "an unsupported with lookup must not fall back to a local homonym"
    );
}

#[test]
fn unrelated_local_rename_inside_with_body_remains_safe() {
    let source = "unit WithSafeLocalRename;
interface
type
  TBox = class
    Value: Integer;
  end;
implementation
procedure Run(Box: TBox);
var
  LocalValue: Integer;
begin
  with Box do
    LocalValue := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "WithSafeLocalRename", source);
    let declaration = range_in(source, "LocalValue: Integer", "LocalValue", 0);

    let edits = index
        .rename_edits(&source_uri, declaration.start, "RenamedLocal")
        .expect("an unrelated local inside with can be renamed safely");
    assert_exact_edits(
        &edits,
        vec![
            (source_uri.clone(), declaration, "RenamedLocal".to_owned()),
            (
                source_uri,
                range_in(source, "LocalValue := 1", "LocalValue", 0),
                "RenamedLocal".to_owned(),
            ),
        ],
    );
}

#[test]
fn receiver_list_binding_keeps_local_rename_edits_exact() {
    let source = "unit WithReceiverListRename;
interface
type
  TInner = record
    Value: Integer;
  end;
  TOuter = record
    Inner: TInner;
  end;
implementation
procedure Run;
var
  OuterValue: TOuter;
  Inner: TInner;
begin
  with OuterValue, Inner do
    Value := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "WithReceiverListRename", source);
    let local_declaration = range_in(source, "Inner: TInner", "Inner", 1);

    let edits = index
        .rename_edits(&source_uri, local_declaration.start, "RenamedInner")
        .expect("unused local rename remains safe");
    assert_exact_edits(
        &edits,
        vec![(
            source_uri.clone(),
            local_declaration,
            "RenamedInner".to_owned(),
        )],
    );

    assert!(
        index
            .rename_edits(
                &source_uri,
                position_of(source, "Inner do", 0),
                "RenamedInner",
            )
            .is_err(),
        "with-dependent receiver rename must be rejected"
    );
}

#[test]
fn inherited_member_reference_aborts_without_partial_edits() {
    let source = "unit InheritedRename;
interface
type
  TBase = class
    Value: Integer;
  end;
  TChild = class(TBase)
    procedure Run;
  end;
implementation
procedure TChild.Run;
begin
  Self.Value := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "InheritedRename", source);

    assert!(
        index
            .rename_edits(&source_uri, position_of(source, "Value", 0), "RenamedValue",)
            .is_err(),
        "inherited references must not produce a partial rename"
    );
}

#[test]
fn nested_member_rename_uses_the_declaring_type_context() {
    let provider = r#"unit NestedMemberRenameProvider;
interface
type
  TP = class
    Shared: Integer;
  end;
  TBase = class
    F: TP;
  end;
implementation
end.
"#;
    let consumer = r#"unit NestedMemberRenameConsumer;
interface
uses NestedMemberRenameProvider;
type
  TP = class
    Shared: string;
  end;
  TChild = class(NestedMemberRenameProvider.TBase)
  end;
implementation
procedure Caller;
var
  Obj: TChild;
begin
  Obj.F.Shared;
end;
end.
"#;
    let mut index = NavigationIndex::new();
    let provider_uri = update(&mut index, "NestedMemberRenameProvider", provider);
    let consumer_uri = update(&mut index, "NestedMemberRenameConsumer", consumer);

    let edits = index
        .rename_edits(
            &consumer_uri,
            position_of(consumer, "Shared", 1),
            "RenamedShared",
        )
        .expect("nested member rename resolves its declaring type");
    assert_eq!(
        exact_edit_signatures(&edits),
        vec![
            edit_signature(
                &consumer_uri,
                range_of(consumer, "Shared", 1),
                "RenamedShared",
            ),
            edit_signature(
                &provider_uri,
                range_of(provider, "Shared", 0),
                "RenamedShared",
            ),
        ],
        "nested member rename must not edit the consumer's unrelated TP.Shared"
    );
}

#[test]
fn inherited_unqualified_homonym_fallback_aborts_without_partial_edits() {
    let source = "unit InheritedHomonymRename;
interface
const Value = 100;
type TBase = class
 public const Value = 1;
end;
TChild = class(TBase)
 procedure Run;
end;
implementation
procedure TChild.Run;
begin
 WriteLn(Value);
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "InheritedHomonymRename", source);
    let mut selected = position_of(source, "public const Value", 0);
    selected.character += "public const ".encode_utf16().count() as u32;

    assert!(
        index.rename_edits(&source_uri, selected, "Count",).is_err(),
        "an unqualified descendant lookup must not fall back to a global homonym"
    );
}

#[test]
fn virtual_override_family_rename_is_rejected_without_explicit_calls() {
    let source = "unit VirtualOverrideRename;
interface
type TBase = class
 procedure Work; virtual;
end;
TChild = class(TBase)
 procedure Work; override;
end;
implementation
procedure TBase.Work; begin end;
procedure TChild.Work; begin end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "VirtualOverrideRename", source);

    assert!(
        index
            .rename_edits(
                &source_uri,
                position_of(source, "Work; virtual", 0),
                "RunWork",
            )
            .is_err(),
        "virtual override families must not be partially renamed"
    );
}

#[test]
fn virtual_override_family_with_implicit_inherited_call_is_rejected() {
    let source = "unit VirtualInheritedRename;
interface
type TBase = class
 procedure Work; virtual;
end;
TChild = class(TBase)
 procedure Work; override;
end;
implementation
procedure TBase.Work; begin end;
procedure TChild.Work;
begin
 inherited;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "VirtualInheritedRename", source);

    assert!(
        index
            .rename_edits(
                &source_uri,
                position_of(source, "Work; virtual", 0),
                "RunWork",
            )
            .is_err(),
        "implicit inherited calls must not preserve an incomplete override family rename"
    );
}

#[test]
fn forward_class_completion_rename_is_rejected_without_type_references() {
    let source = "unit ForwardClassRename;
interface
type TBox = class;
 TBox = class
 end;
implementation
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "ForwardClassRename", source);

    assert!(
        index
            .rename_edits(
                &source_uri,
                position_of(source, "TBox = class;", 0),
                "TNewBox",
            )
            .is_err(),
        "forward class declarations must not be renamed independently"
    );
}

#[test]
fn unused_abbreviated_parameter_collision_is_rejected() {
    let source = "unit AbbreviatedParameterCollision;
interface
procedure Run(Value: Integer);
implementation
procedure Run;
var Count: Integer;
begin
 Count := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "AbbreviatedParameterCollision", source);

    assert!(
        index
            .rename_edits(
                &source_uri,
                position_of(source, "Value: Integer", 0),
                "Count",
            )
            .is_err(),
        "an unused abbreviated parameter must still collide with a body local"
    );
}

#[test]
fn opaque_directive_occurrence_aborts_without_partial_edits() {
    let source = "unit OpaqueRename;
interface
procedure Run;
implementation
procedure Run;
var
  Value: Integer;
begin
  Value := 1;
{$IF UNKNOWN_CONDITION}
  not valid Pascal Value @@@
{$IFEND}
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "OpaqueRename", source);

    assert!(
        index
            .rename_edits(&source_uri, position_of(source, "Value", 0), "RenamedValue",)
            .is_err(),
        "opaque directive occurrences must not produce a partial rename"
    );
}

#[test]
fn paired_routine_declaration_and_definition_are_renamed_together() {
    let source = "unit PairedRoutineRename;
interface
procedure DoWork;
implementation
procedure DoWork;
begin
  DoWork;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "PairedRoutineRename", source);

    let edits = index
        .rename_edits(&source_uri, position_of(source, "DoWork", 0), "RunWork")
        .expect("paired routine rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_of(source, "DoWork", 0),
                "RunWork".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "DoWork", 1),
                "RunWork".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "DoWork", 2),
                "RunWork".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &edits[&source_uri],
        &source.replacen("DoWork", "RunWork", 3),
    );
}

#[test]
fn duplicate_routine_declarations_are_not_coalesced_as_a_pair() {
    let source = "unit DuplicateRoutineRename;
interface
procedure DoWork;
procedure DoWork;
implementation
procedure DoWork;
begin
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "DuplicateRoutineRename", source);

    assert!(
        index
            .rename_edits(&source_uri, position_of(source, "DoWork", 0), "RunWork",)
            .is_err(),
        "duplicate declarations must not be treated as one routine pair"
    );
}

#[test]
fn routine_parameter_declaration_and_body_are_renamed_together() {
    let source = "unit ParameterRename;
interface
procedure Run(Value: Integer);
implementation
procedure Run(Arg: Integer);
begin
  aRg := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "ParameterRename", source);

    let edits = index
        .rename_edits(
            &source_uri,
            position_of(source, "Value: Integer", 0),
            "NewValue",
        )
        .expect("parameter rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_of(source, "Value", 0),
                "NewValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "Arg", 0),
                "NewValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "aRg", 0),
                "NewValue".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &edits[&source_uri],
        &source
            .replacen("Value: Integer", "NewValue: Integer", 1)
            .replacen("Arg: Integer", "NewValue: Integer", 1)
            .replacen("aRg :=", "NewValue :=", 1),
    );
}

#[test]
fn paired_parameter_alias_selection_has_the_same_rename_plan() {
    let source = "unit PairedParameterAlias;
interface
procedure Run(Value: Integer);
implementation
procedure Run(Arg: Integer);
begin Arg := 1; end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "PairedParameterAlias", source);

    let prototype_edits = index
        .rename_edits(&source_uri, position_of(source, "Value: Integer", 0), "Arg")
        .expect("prototype selection can use the implementation spelling");
    let implementation_edits = index
        .rename_edits(&source_uri, position_of(source, "Arg", 0), "Arg")
        .expect("implementation selection can keep its existing spelling");
    assert_eq!(
        exact_edit_signatures(&prototype_edits),
        exact_edit_signatures(&implementation_edits),
        "all selections of one paired parameter must produce one plan"
    );
    assert_exact_edits(
        &prototype_edits,
        vec![
            (
                source_uri.clone(),
                range_of(source, "Value", 0),
                "Arg".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "Arg", 0),
                "Arg".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "Arg", 1),
                "Arg".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &prototype_edits[&source_uri],
        &source.replacen("Value: Integer", "Arg: Integer", 1),
    );
}

#[test]
fn parameter_rename_can_shadow_an_outer_unit_name() {
    let source = "unit ParameterOuterName;
interface
const NewValue = 0;
procedure Run(Value: Integer);
implementation
procedure Run(Arg: Integer);
begin
  Arg := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "ParameterOuterName", source);

    let edits = index
        .rename_edits(
            &source_uri,
            position_of(source, "Value: Integer", 0),
            "NewValue",
        )
        .expect("parameter may shadow an outer unit name");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_in(source, "Value: Integer", "Value", 0),
                "NewValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "Arg", 0),
                "NewValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "Arg", 1),
                "NewValue".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &edits[&source_uri],
        &source
            .replacen("Value: Integer", "NewValue: Integer", 1)
            .replacen("Arg: Integer", "NewValue: Integer", 1)
            .replacen("  Arg := 1", "  NewValue := 1", 1),
    );
}

#[test]
fn overloaded_routine_parameter_rename_is_rejected() {
    let source = "unit OverloadedParameterRename;
interface
procedure Do(Value: Integer); overload;
procedure Do(Value: string); overload;
implementation
procedure Do(Arg: Integer);
begin
  Arg := 1;
end;
procedure Do(Arg: string);
begin
  Arg := 'x';
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "OverloadedParameterRename", source);

    assert!(
        index
            .rename_edits(&source_uri, position_of(source, "Value", 0), "NewValue",)
            .is_err(),
        "overloaded routine parameters must not yield a partial rename"
    );
}

#[test]
fn ordinary_conditional_directives_do_not_make_a_rename_opaque() {
    let source = "unit ConditionalRename;
interface
procedure Run;
implementation
procedure Run;
var
  Value: Integer;
begin
{$IFDEF ENABLE_VALUE}
  Value := 1;
{$ENDIF}
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "ConditionalRename", source);

    let edits = index
        .rename_edits(&source_uri, position_of(source, "Value", 0), "NewValue")
        .expect("conditional source rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_of(source, "Value", 0),
                "NewValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "Value", 1),
                "NewValue".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &edits[&source_uri],
        &source.replacen("Value", "NewValue", 2),
    );
}

#[test]
fn known_inactive_conditional_rename_preserves_utf16_ranges_and_active_edits() {
    let source = "unit ConditionalUtf16Rename;
interface
procedure Run;
implementation
procedure Run;
var
  Value: Integer;
begin
{$UNDEF OFF}
{$IFDEF OFF}
  Value := 0;
{$ENDIF}
  WriteLn('😀'); Value := 1;
end;
end.
";
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "ConditionalUtf16Rename", source);

    let edits = index
        .rename_edits(
            &source_uri,
            position_of(source, "Value: Integer", 0),
            "RenamedValue",
        )
        .expect("known-inactive conditional rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_of(source, "Value", 0),
                "RenamedValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_of(source, "Value", 2),
                "RenamedValue".to_owned(),
            ),
        ],
    );
    assert_document_after_edits(
        source,
        &edits[&source_uri],
        &source
            .replacen("Value: Integer", "RenamedValue: Integer", 1)
            .replacen("Value := 1", "RenamedValue := 1", 1),
    );
}

#[test]
fn rename_skips_an_unresolved_include_in_a_known_inactive_branch() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("ConditionalInclude.pas");
    let source = "unit ConditionalInclude;\ninterface\n{$UNDEF OFF}\n{$IFDEF OFF}\n{$I Missing.inc}\n{$ENDIF}\nimplementation\nprocedure Run;\nvar\n  Value: Integer;\nbegin\n  Value := 1;\nend;\nend.\n";
    let source_uri = Url::from_file_path(&source_path).expect("source URI");
    fs::create_dir_all(temp.path()).expect("workspace directory");
    fs::write(&source_path, source).expect("source");

    let mut workspace =
        Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
    workspace
        .open_document(source_uri.clone(), source.to_owned(), 1)
        .expect("open conditional source");
    let edit = workspace
        .rename_edits(
            &source_uri,
            position_of(source, "Value: Integer", 0),
            "RenamedValue",
            false,
        )
        .expect("known-inactive unresolved include must not block a safe rename");
    let edits = edit.changes.expect("plain workspace edit changes");
    let edits = edits.get(&source_uri).expect("target edits");
    assert_eq!(edits.len(), 2);
    assert_document_after_edits(
        source,
        edits,
        &source
            .replacen("Value: Integer", "RenamedValue: Integer", 1)
            .replacen("Value :=", "RenamedValue :=", 1),
    );
}

#[test]
fn active_or_unknown_unresolved_includes_still_block_rename() {
    for (name, directives) in [
        (
            "ActiveInclude",
            "{$IFDEF ENABLED}\n{$I Missing.inc}\n{$ENDIF}\n",
        ),
        (
            "UnknownInclude",
            "{$IF CompilerVersion >= 24}\n{$I Missing.inc}\n{$ENDIF}\n",
        ),
    ] {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let source_path = temp.path().join(format!("{name}.pas"));
        let source = format!(
            "unit {name};\ninterface\n{directives}implementation\nprocedure Run;\nvar\n  Value: Integer;\nbegin\n  Value := 1;\nend;\nend.\n"
        );
        let source_uri = Url::from_file_path(&source_path).expect("source URI");
        fs::write(&source_path, &source).expect("source");
        let mut workspace =
            Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
        workspace
            .open_document(source_uri.clone(), source.clone(), 1)
            .expect("open conditional source");
        assert!(
            workspace
                .rename_edits(
                    &source_uri,
                    position_of(&source, "Value: Integer", 0),
                    "RenamedValue",
                    false,
                )
                .is_err(),
            "{name} unresolved include must keep rename conservative"
        );
    }
}

#[test]
fn resolved_source_bearing_include_supports_physical_rename_edits() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root_path = temp.path().join("Main.pas");
    let include_path = temp.path().join("Shared.inc");
    let root = "unit Main;\ninterface\n{$I Shared.inc}\nimplementation\nprocedure Run;\nbegin\n  SharedValue := 1;\nend;\nend.\n";
    let include = "const SharedValue = 1;\n";
    fs::write(&root_path, root).expect("root source");
    fs::write(&include_path, include).expect("include source");
    let root_uri = Url::from_file_path(&root_path).expect("root URI");
    let include_uri = Url::from_file_path(&include_path).expect("include URI");

    let mut workspace =
        Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
    workspace
        .open_document(root_uri.clone(), root.to_owned(), 1)
        .expect("open root source");

    let edit = workspace
        .rename_edits(
            &root_uri,
            position_of(root, "SharedValue :=", 0),
            "RenamedShared",
            false,
        )
        .expect("resolved include should support rename");
    let changes = edit.changes.expect("plain workspace edit changes");
    assert_exact_edits(
        &changes,
        vec![
            (
                root_uri,
                range_of(root, "SharedValue", 0),
                "RenamedShared".to_owned(),
            ),
            (
                include_uri,
                range_of(include, "SharedValue", 0),
                "RenamedShared".to_owned(),
            ),
        ],
    );
}

#[test]
fn include_declaration_rename_updates_its_root_consumers() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root_path = temp.path().join("Main.pas");
    let include_path = temp.path().join("Shared.inc");
    let root = "unit Main;\ninterface\n{$I Shared.inc}\nimplementation\nprocedure Run;\nbegin\n  SharedValue := 1;\nend;\nend.\n";
    let include = "const SharedValue = 1;\n";
    fs::write(&root_path, root).expect("root source");
    fs::write(&include_path, include).expect("include source");
    let root_uri = Url::from_file_path(&root_path).expect("root URI");
    let include_uri = Url::from_file_path(&include_path).expect("include URI");

    let mut workspace =
        Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
    workspace
        .open_document(include_uri.clone(), include.to_owned(), 1)
        .expect("open include source");

    let edit = workspace
        .rename_edits(
            &include_uri,
            position_of(include, "SharedValue", 0),
            "RenamedShared",
            false,
        )
        .expect("include declaration should support rename");
    let changes = edit.changes.expect("plain workspace edit changes");
    assert_exact_edits(
        &changes,
        vec![
            (
                root_uri,
                range_of(root, "SharedValue", 0),
                "RenamedShared".to_owned(),
            ),
            (
                include_uri,
                range_of(include, "SharedValue", 0),
                "RenamedShared".to_owned(),
            ),
        ],
    );
}

#[test]
fn fresh_include_rename_uses_its_single_owning_root_context() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root_path = temp.path().join("Main.pas");
    let include_path = temp.path().join("Use.inc");
    let root = "unit Main;\ninterface\nconst RootValue = 1;\nimplementation\nprocedure Run;\nbegin\n{$I Use.inc}\nend;\nend.\n";
    let include = "Log(RootValue);\n";
    fs::write(&root_path, root).expect("root source");
    fs::write(&include_path, include).expect("include source");
    let include_uri = Url::from_file_path(&include_path).expect("include URI");
    let root_uri = Url::from_file_path(&root_path).expect("root URI");
    let mut workspace =
        Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());

    let edit = workspace
        .rename_edits(
            &include_uri,
            position_of(include, "RootValue", 0),
            "RenamedRoot",
            false,
        )
        .expect("fresh include rename should use its owning root");
    let changes = edit.changes.expect("plain workspace edit changes");
    assert_exact_edits(
        &changes,
        vec![
            (
                root_uri,
                range_of(root, "RootValue", 0),
                "RenamedRoot".to_owned(),
            ),
            (
                include_uri,
                range_of(include, "RootValue", 0),
                "RenamedRoot".to_owned(),
            ),
        ],
    );
}

#[test]
fn rename_rejects_a_physical_include_edit_with_conflicting_root_bindings() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let a_path = temp.path().join("A.pas");
    let b_path = temp.path().join("B.pas");
    let use_path = temp.path().join("Use.inc");
    let a = "unit A;\ninterface\nconst SharedValue = 1;\nimplementation\nprocedure Run;\nbegin\n{$I Use.inc}\nend;\nend.\n";
    let b = "unit B;\ninterface\nconst SharedValue = 1;\nimplementation\nprocedure Run;\nbegin\n{$I Use.inc}\nend;\nend.\n";
    let use_source = "Log(SharedValue);\n";
    fs::write(&a_path, a).expect("A source");
    fs::write(&b_path, b).expect("B source");
    fs::write(&use_path, use_source).expect("include source");
    let a_uri = Url::from_file_path(&a_path).expect("A URI");
    let mut workspace =
        Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
    workspace
        .open_document(a_uri.clone(), a.to_owned(), 1)
        .expect("open A");

    assert!(
        workspace
            .rename_edits(
                &a_uri,
                position_of(a, "SharedValue", 0),
                "ChangedValue",
                false,
            )
            .is_err(),
        "a physical include edit must not be applied to a conflicting root binding"
    );
}

#[test]
fn rename_rejects_a_repeated_include_with_distinct_local_bindings() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let main_path = temp.path().join("Main.pas");
    let use_path = temp.path().join("Use.inc");
    let source = "unit Main;\ninterface\nimplementation\nprocedure First;\nvar SharedValue: Integer;\nbegin\n{$I Use.inc}\nend;\nprocedure Second;\nvar SharedValue: Integer;\nbegin\n{$I Use.inc}\nend;\nend.\n";
    let use_source = "Log(SharedValue);\n";
    fs::write(&main_path, source).expect("main source");
    fs::write(&use_path, use_source).expect("include source");
    let main_uri = Url::from_file_path(&main_path).expect("main URI");
    let mut workspace =
        Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
    workspace
        .open_document(main_uri.clone(), source.to_owned(), 1)
        .expect("open main");

    assert!(
        workspace
            .rename_edits(
                &main_uri,
                position_of(source, "SharedValue: Integer", 0),
                "ChangedValue",
                false,
            )
            .is_err(),
        "a repeated include must not inherit one local binding's rename"
    );
}

#[test]
fn mixed_boolean_and_comparison_precedence_keeps_the_active_include_audited() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let source_path = temp.path().join("MixedOperators.pas");
    let source = "unit MixedOperators;\ninterface\n{$IF True or False = False}\nconst ThenBranch = 1;\n{$ELSE}\n{$I Missing.inc}\n{$ENDIF}\nimplementation\nprocedure Run;\nvar\n  Value: Integer;\nbegin\n  Value := 1;\nend;\nend.\n";
    let source_uri = Url::from_file_path(&source_path).expect("source URI");
    fs::create_dir_all(temp.path()).expect("workspace directory");
    fs::write(&source_path, source).expect("source");

    let mut workspace =
        Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
    workspace
        .open_document(source_uri.clone(), source.to_owned(), 1)
        .expect("open conditional source");

    assert!(
        workspace
            .rename_edits(
                &source_uri,
                position_of(source, "Value: Integer", 0),
                "RenamedValue",
                false,
            )
            .is_err(),
        "Pascal's lower-precedence comparison must leave the ELSE include active"
    );
}

#[test]
fn generic_type_parameter_rename_fails_closed_until_instantiations_are_modeled() {
    let source = r#"unit GenericRename;
interface
type
  TBox<T> = class
    Value: T;
  end;
  TWidget = class
  end;
var
  Box: TBox<TWidget>;
implementation
procedure Run;
begin
  Box.Value := TWidget.Create;
end;
end.
"#;
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "GenericRename", source);

    assert!(
        index
            .rename_edits(&source_uri, Position::new(3, 7), "TOther")
            .is_err(),
        "generic parameter rename must remain conservative"
    );
}

#[test]
fn global_type_rename_excludes_routine_generic_formals() {
    let source = r#"unit GenericFormalShadow;
interface
type
  T = class
    LocalMember: Integer;
  end;
function Identity<T>(Value: T): T;
implementation
function Identity<T>(Value: T): T;
begin
  Result := Value;
end;
procedure Run;
var
  Obj: T;
begin
  Identity(Obj).LocalMember;
end;
end.
"#;
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "GenericFormalShadow", source);

    let edits = index
        .rename_edits(&source_uri, position_of(source, "T = class", 0), "TRenamed")
        .expect("global type rename must remain resolvable");
    assert_exact_edits(
        &edits,
        vec![
            (
                source_uri.clone(),
                range_in(source, "T = class", "T", 0),
                "TRenamed".to_owned(),
            ),
            (
                source_uri,
                range_in(source, "Obj: T", "T", 0),
                "TRenamed".to_owned(),
            ),
        ],
    );
}

#[test]
fn field_rename_resolves_call_expression_members_without_capturing_formals() {
    let source = r#"unit ExpressionReceiverRename;
interface
type
  THolder = class
    T: Integer;
  end;
function GetHolder: THolder;
procedure Run<T>(Value: T);
implementation
function GetHolder: THolder;
begin
end;
procedure Run<T>(Value: T);
begin
  GetHolder().T := 1;
  THolder(GetHolder()).T := 2;
end;
end.
"#;
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "ExpressionReceiverRename", source);
    let declaration = range_in(source, "T: Integer", "T", 0);
    let use_range = range_in(source, "GetHolder().T", "T", 0);
    let cast_range = range_in_occurrence(source, "THolder(GetHolder()).T", "T", 0, 1);

    let edits = index
        .rename_edits(&source_uri, declaration.start, "Renamed")
        .expect("field rename through a call receiver succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (source_uri.clone(), declaration, "Renamed".to_owned()),
            (source_uri.clone(), use_range, "Renamed".to_owned()),
            (source_uri.clone(), cast_range, "Renamed".to_owned()),
        ],
    );

    let bindings = index
        .binding_locations(&source_uri, use_range.start, true)
        .expect("call receiver field bindings resolve");
    assert_exact_locations(
        &bindings,
        vec![
            (source_uri.clone(), declaration),
            (source_uri.clone(), use_range),
            (source_uri, cast_range),
        ],
    );
}

#[test]
fn class_field_rename_keeps_qualified_type_and_routine_formals_disjoint() {
    let source = r#"unit QualifiedFieldRename;
interface
type
  T = class
    GlobalMember: Integer;
  end;
  Holder = class
    T: Integer;
  end;
procedure Run<T>(Obj: Holder);
implementation
procedure Run<T>(Obj: Holder);
var
  Qualified: QualifiedFieldRename.T;
begin
  Qualified.GlobalMember;
  Obj.T := 1;
end;
end.
"#;
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "QualifiedFieldRename", source);
    let target = range_in(source, "T: Integer", "T", 0);

    let edits = index
        .rename_edits(&source_uri, target.start, "FieldValue")
        .expect("qualified class field rename succeeds");
    assert_exact_edits(
        &edits,
        vec![
            (source_uri.clone(), target, "FieldValue".to_owned()),
            (
                source_uri,
                range_in(source, "Obj.T", "T", 0),
                "FieldValue".to_owned(),
            ),
        ],
    );
}

#[test]
fn rename_respects_private_protected_and_strict_private_access() {
    let provider = r#"unit RenameAccessibilityProvider;
interface
type
  TBase = class
  private
    PrivateField: Integer;
  protected
    ProtectedField: Integer;
  strict private
    StrictSecretField: Integer;
  end;
  procedure SameUnit;
implementation
procedure SameUnit;
var
  Obj: TBase;
begin
  Obj.PrivateField := 1;
end;
end.
"#;
    let consumer = r#"unit RenameAccessibilityConsumer;
interface
uses RenameAccessibilityProvider;
type
  TChild = class(TBase)
    procedure Run;
  end;
  TUnrelated = class
    procedure Run;
  end;
implementation
procedure TChild.Run;
var
  Obj: TBase;
begin
  Obj.ProtectedField := 1;
end;
procedure TUnrelated.Run;
var
  Obj: TBase;
begin
  Obj.StrictSecretField := 1;
end;
end.
"#;
    let provider_uri = uri("RenameAccessibilityProvider");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("rename accessibility provider parses");
    let consumer_uri = update(&mut index, "RenameAccessibilityConsumer", consumer);

    let private_declaration = position_of(provider, "PrivateField", 0);
    let private_edits = index
        .rename_edits(&provider_uri, private_declaration, "RenamedPrivate")
        .expect("ordinary private rename within its unit succeeds");
    assert_exact_edits(
        &private_edits,
        vec![
            (
                provider_uri.clone(),
                range_of(provider, "PrivateField", 0),
                "RenamedPrivate".to_owned(),
            ),
            (
                provider_uri.clone(),
                range_of(provider, "PrivateField", 1),
                "RenamedPrivate".to_owned(),
            ),
        ],
    );

    let protected_declaration = position_of(provider, "ProtectedField", 0);
    let protected_edits = index
        .rename_edits(&provider_uri, protected_declaration, "RenamedProtected")
        .expect("protected rename from a descendant succeeds");
    assert_exact_edits(
        &protected_edits,
        vec![
            (
                provider_uri.clone(),
                range_of(provider, "ProtectedField", 0),
                "RenamedProtected".to_owned(),
            ),
            (
                consumer_uri,
                range_of(consumer, "ProtectedField", 0),
                "RenamedProtected".to_owned(),
            ),
        ],
    );

    assert!(
        index
            .rename_edits(
                &provider_uri,
                position_of(provider, "StrictSecretField", 0),
                "RenamedStrictPrivate",
            )
            .is_err(),
        "strict-private access from another class must reject a partial rename"
    );
}

#[test]
fn inline_variable_rename_stays_within_each_nested_block() {
    let source = r#"unit BlockInlineRename;
interface
var
  Value: Integer;
implementation
procedure Run;
begin
  Value := 1;
  if True then
  begin
    var Value: Integer;
    Value := 2;
    if True then
    begin
      var Value: Integer;
      Value := 3;
    end;
    Value := 4;
  end;
  Value := 5;
end;
end.
"#;
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "BlockInlineRename", source);

    let global_declaration = range_in(source, "Value: Integer", "Value", 0);
    let outer_declaration = range_in(source, "Value: Integer", "Value", 1);
    let inner_declaration = range_in(source, "Value: Integer", "Value", 2);

    let global_edits = index
        .rename_edits(&source_uri, global_declaration.start, "GlobalValue")
        .expect("global inline-shadow rename succeeds");
    assert_exact_edits(
        &global_edits,
        vec![
            (
                source_uri.clone(),
                global_declaration,
                "GlobalValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in(source, "Value := 1", "Value", 0),
                "GlobalValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in(source, "Value := 5", "Value", 0),
                "GlobalValue".to_owned(),
            ),
        ],
    );

    let outer_edits = index
        .rename_edits(&source_uri, outer_declaration.start, "OuterValue")
        .expect("outer inline rename succeeds");
    assert_exact_edits(
        &outer_edits,
        vec![
            (
                source_uri.clone(),
                outer_declaration,
                "OuterValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in(source, "Value := 2", "Value", 0),
                "OuterValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in(source, "Value := 4", "Value", 0),
                "OuterValue".to_owned(),
            ),
        ],
    );

    let inner_edits = index
        .rename_edits(&source_uri, inner_declaration.start, "InnerValue")
        .expect("inner inline rename succeeds");
    assert_exact_edits(
        &inner_edits,
        vec![
            (
                source_uri.clone(),
                inner_declaration,
                "InnerValue".to_owned(),
            ),
            (
                source_uri,
                range_in(source, "Value := 3", "Value", 0),
                "InnerValue".to_owned(),
            ),
        ],
    );
}

#[test]
fn local_declaration_rename_respects_prior_initializer_bindings() {
    let source = r#"unit DeclarationOrderRename;
interface
const
  Value = 1;
type
  TGlobal = Integer;
implementation
procedure Run;
const
  BeforeValue = Value;
  Value = 2;
type
  TBefore = TGlobal;
  TGlobal = string;
var
  BeforeVar: TGlobal;
begin
  WriteLn(Value);
end;
end.
"#;
    let mut index = NavigationIndex::new();
    let source_uri = update(&mut index, "DeclarationOrderRename", source);

    let global_value_edits = index
        .rename_edits(
            &source_uri,
            position_of(source, "Value = 1", 0),
            "GlobalValue",
        )
        .expect("global constant rename succeeds");
    assert_exact_edits(
        &global_value_edits,
        vec![
            (
                source_uri.clone(),
                range_in(source, "Value = 1", "Value", 0),
                "GlobalValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in_occurrence(source, "BeforeValue = Value", "Value", 0, 1),
                "GlobalValue".to_owned(),
            ),
        ],
    );

    let local_value_edits = index
        .rename_edits(
            &source_uri,
            position_of(source, "Value = 2", 0),
            "LocalValue",
        )
        .expect("local constant rename succeeds");
    assert_exact_edits(
        &local_value_edits,
        vec![
            (
                source_uri.clone(),
                range_in(source, "Value = 2", "Value", 0),
                "LocalValue".to_owned(),
            ),
            (
                source_uri.clone(),
                range_in(source, "WriteLn(Value)", "Value", 0),
                "LocalValue".to_owned(),
            ),
        ],
    );

    let local_type_edits = index
        .rename_edits(
            &source_uri,
            position_of(source, "TGlobal = string", 0),
            "LocalType",
        )
        .expect("local type rename succeeds");
    assert_exact_edits(
        &local_type_edits,
        vec![
            (
                source_uri.clone(),
                range_in(source, "TGlobal = string", "TGlobal", 0),
                "LocalType".to_owned(),
            ),
            (
                source_uri,
                range_in(source, "BeforeVar: TGlobal", "TGlobal", 0),
                "LocalType".to_owned(),
            ),
        ],
    );
}

#[test]
fn rename_rejects_inaccessible_receiver_unit_fallback() {
    let provider = r#"unit RenameReceiverUnitProvider;
interface
type
  TBase = class
  private
    Hidden: Integer;
  end;
end.
"#;
    let consumer = r#"unit RenameReceiverUnitConsumer;
interface
uses RenameReceiverUnitProvider, Hidden;
type
  TChild = class(TBase)
    procedure Run;
  end;
implementation
procedure TChild.Run;
begin
  Hidden.Exposed := 1;
end;
end.
"#;
    let hidden = r#"unit Hidden;
interface
var
  Exposed: Integer;
implementation
end.
"#;
    let hidden_uri = uri("Hidden");
    let mut index = NavigationIndex::new();
    update(&mut index, "RenameReceiverUnitProvider", provider);
    update(&mut index, "RenameReceiverUnitConsumer", consumer);
    update(&mut index, "Hidden", hidden);

    let result = index.rename_edits(&hidden_uri, position_of(hidden, "Exposed", 0), "Changed");
    assert!(
        result.is_err(),
        "rename accepted an inaccessible receiver's unit fallback: {result:?}"
    );
}
