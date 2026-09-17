use lsp_types::{
    CompletionItemKind, CompletionTextEdit, Documentation as LspDocumentation, HoverContents,
    Location, MarkedString, MarkupKind, Position, Range, Url,
};
use pascal_lsp::workspace::{Workspace, WorkspaceOptions};
use pascal_lsp::{NavigationIndex, NavigationTarget, text};
use pascal_project::delphi_overrides::OverrideSession;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::process::Command;

fn test_workspace(roots: Vec<std::path::PathBuf>, options: WorkspaceOptions) -> Workspace {
    Workspace::with_override_session(roots, options, OverrideSession::new(None))
}

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

fn position_after(source: &str, needle: &str, occurrence: usize) -> Position {
    let start = position_of(source, needle, occurrence);
    Position::new(
        start.line,
        start.character + needle.encode_utf16().count() as u32,
    )
}

fn property_position(source: &str, name: &str) -> Position {
    let mut position = position_of(source, &format!("property {name}"), 0);
    position.character += "property ".encode_utf16().count() as u32;
    position
}

fn locations_at(
    index: &NavigationIndex,
    source_uri: &Url,
    source: &str,
    needle: &str,
    occurrence: usize,
    target: NavigationTarget,
) -> Vec<Location> {
    index.navigate(source_uri, position_of(source, needle, occurrence), target)
}

fn assert_location_start(location: &Location, expected_uri: &Url, expected: Position) {
    assert_eq!(&location.uri, expected_uri);
    assert_eq!(location.range.start, expected);
}

#[test]
fn class_helper_members_navigate_from_the_helped_class() {
    let source = r#"unit ClassHelperNavigation;
interface
type
  TWidget = class
  end;
  TWidgetHelper = class helper for TWidget
    procedure Touch;
  end;

procedure Run;

implementation

procedure TWidgetHelper.Touch;
begin
end;

procedure Run;
var
  Widget: TWidget;
begin
  Widget.Touch;
end;

end.
"#;
    let source_uri = uri("ClassHelperNavigation");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("class helper source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Touch", 2),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_location_start(&locations[0], &source_uri, position_of(source, "Touch", 0));
}

#[test]
fn class_helper_inheritance_exposes_parent_helper_members() {
    let source = r#"unit ClassHelperInheritance;
interface
type
  TWidget = class
  end;
  TBaseWidgetHelper = class helper for TWidget
    procedure Base;
  end;
  TDerivedWidgetHelper = class helper (TBaseWidgetHelper) for TWidget
    procedure Derived;
  end;

procedure Run;

implementation

procedure TBaseWidgetHelper.Base;
begin
end;

procedure TDerivedWidgetHelper.Derived;
begin
end;

procedure Run;
var
  Widget: TWidget;
begin
  Widget.Base;
  Widget.Derived;
end;

end.
"#;
    let source_uri = uri("ClassHelperInheritance");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("class helper inheritance source parses");

    let base = index.navigate(
        &source_uri,
        position_of(source, "Base;", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(base.len(), 1);
    assert_location_start(&base[0], &source_uri, position_of(source, "Base;", 0));

    let derived = index.navigate(
        &source_uri,
        position_of(source, "Derived;", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(derived.len(), 1);
    assert_location_start(&derived[0], &source_uri, position_of(source, "Derived;", 0));
}

#[test]
fn record_helper_members_navigate_from_the_helped_record() {
    let source = r#"unit RecordHelperNavigation;
interface
type
  TPoint = record
    X: Integer;
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
    let source_uri = uri("RecordHelperNavigation");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("record helper source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Offset", 2),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_location_start(&locations[0], &source_uri, position_of(source, "Offset", 0));
}

#[test]
fn helper_self_uses_the_helped_type_and_helper_members() {
    let source = r#"unit HelperSelfNavigation;
interface
type
  TPoint = record
    X: Integer;
  end;
  TPointHelper = record helper for TPoint
    procedure Offset;
  end;

implementation

procedure TPointHelper.Offset;
begin
  Self.X := 1;
  Self.Offset;
end;

end.
"#;
    let source_uri = uri("HelperSelfNavigation");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("helper self source parses");

    let field = index.navigate(
        &source_uri,
        position_of(source, "X", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(field.len(), 1);
    assert_location_start(&field[0], &source_uri, position_of(source, "X", 0));

    let method = index.navigate(
        &source_uri,
        position_of(source, "Offset", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(method.len(), 1);
    assert_location_start(&method[0], &source_uri, position_of(source, "Offset", 0));
}

#[test]
fn helper_members_feed_completion_hover_and_signature_help() {
    let source = r#"unit HelperAssistance;
interface
type
  TPoint = record
    X: Integer;
  end;
  TPointHelper = record helper for TPoint
    procedure Offset(Value: Integer);
  end;

procedure Run;

implementation

procedure TPointHelper.Offset(Value: Integer);
begin
end;

procedure Run;
var
  Point: TPoint;
begin
  Point.Offset(1);
  Point.
  Point.Offset(1);
end;

end.
"#;
    let source_uri = uri("HelperAssistance");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("helper assistance source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "Point.", 1))
        .expect("helper completion");
    let labels = completion
        .items
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();
    assert!(
        labels.contains(&"Offset"),
        "helper member missing: {labels:?}"
    );

    assert!(
        index
            .hover(&source_uri, position_of(source, "Offset", 2))
            .is_some(),
        "helper member hover should resolve"
    );

    let signature = index
        .signature_help(&source_uri, position_after(source, "Point.Offset(1", 0))
        .expect("helper signature help")
        .expect("helper method signature");
    assert_eq!(signature.signatures.len(), 1);
    assert!(signature.signatures[0].label.contains("Offset"));
}

#[test]
fn helper_method_results_feed_nested_member_navigation() {
    let source = r#"unit HelperResultNavigation;
interface
type
  TChild = class
    procedure Run;
  end;
  TWidget = class
  end;
  TWidgetHelper = class helper for TWidget
    function Child: TChild;
  end;

implementation

procedure TChild.Run;
begin
end;

function TWidgetHelper.Child: TChild;
begin
end;

procedure Caller;
var
  Widget: TWidget;
begin
  Widget.Child().Run;
end;

end.
"#;
    let source_uri = uri("HelperResultNavigation");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("helper result source parses");

    let child = index.navigate(
        &source_uri,
        position_of(source, "Child", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(child.len(), 1);
    assert_location_start(&child[0], &source_uri, position_of(source, "TChild", 0));

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Run", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(&locations[0], &source_uri, position_of(source, "Run", 0));

    let child_type = index.type_definitions(&source_uri, position_of(source, "Child", 5));
    assert_eq!(child_type.len(), 1);
    assert_location_start(
        &child_type[0],
        &source_uri,
        position_of(source, "TChild", 0),
    );
}

#[test]
fn specialized_generic_helper_targets_match_specialized_receivers() {
    let source = r#"unit GenericHelperNavigation;
interface
type
  TBox<T> = class
  end;
  TBoxHelper = class helper for TBox<Integer>
    procedure Touch;
  end;

procedure Run;

implementation

procedure TBoxHelper.Touch;
begin
end;

procedure Run;
var
  Box: TBox<Integer>;
begin
  Box.Touch;
end;

end.
"#;
    let source_uri = uri("GenericHelperNavigation");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic helper source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Touch", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(&locations[0], &source_uri, position_of(source, "Touch", 0));
}

#[test]
fn generic_helpers_match_their_generic_target_specialization() {
    let source = r#"unit GenericHelperNavigation;
interface
type
  TBox<T> = class
  end;
  TBoxHelper<T> = class helper for TBox<T>
    procedure Touch;
  end;

procedure Run;

implementation

procedure TBoxHelper<T>.Touch;
begin
end;

procedure Run;
var
  Box: TBox<Integer>;
begin
  Box.Touch;
end;

end.
"#;
    let source_uri = uri("GenericHelperNavigation");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic helper specialization source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Touch", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(&locations[0], &source_uri, position_of(source, "Touch", 0));
}

#[test]
fn helper_members_shadow_helped_type_members_for_navigation() {
    let source = r#"unit HelperMemberPrecedence;
interface
type
  TWidget = class
    Value: Integer;
  end;
  TWidgetHelper = class helper for TWidget
    procedure Value;
  end;

procedure Run;

implementation

procedure TWidgetHelper.Value;
begin
end;

procedure Run;
var
  Widget: TWidget;
begin
  Widget.Value;
end;

end.
"#;
    let source_uri = uri("HelperMemberPrecedence");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("helper precedence source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Value;", 2),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_location_start(&locations[0], &source_uri, position_of(source, "Value;", 0));
}

#[test]
fn imported_helpers_are_visible_independently_of_consumer_offsets() {
    let target = r#"unit HelperVisibilityTarget;
interface
type
  TWidget = class
  end;
implementation
end.
"#;
    let helper = format!(
        "unit LateHelper;\ninterface\nuses HelperVisibilityTarget;\n{}type\n  TWidgetHelper = class helper for TWidget\n    procedure Touch;\n  end;\nimplementation\nprocedure TWidgetHelper.Touch;\nbegin\nend;\nend.\n",
        "\n".repeat(256)
    );
    let consumer = r#"unit HelperVisibilityConsumer;
interface
uses HelperVisibilityTarget, LateHelper;
implementation
procedure Run;
var
  Widget: TWidget;
begin
  Widget.Touch;
end;
end.
"#;

    let target_uri = uri("HelperVisibilityTarget");
    let helper_uri = uri("LateHelper");
    let consumer_uri = uri("HelperVisibilityConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(target_uri, target.to_owned())
        .expect("helper target parses");
    index
        .update(helper_uri.clone(), helper.clone())
        .expect("late helper parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("helper consumer parses");

    let locations = index.navigate(
        &consumer_uri,
        position_of(consumer, "Touch", 0),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &helper_uri,
        position_of(&helper, "Touch;", 0),
    );
}

#[test]
fn base_helpers_apply_to_descendants_but_specific_helpers_win() {
    let source = r#"unit HelperTargetAncestry;
interface
type
  TBase = class
  end;
  TChild = class(TBase)
  end;
  TOther = class(TBase)
  end;
  TBaseHelper = class helper for TBase
    procedure BaseOnly;
    procedure Shared;
  end;
  TChildHelper = class helper for TChild
    procedure Shared;
  end;

implementation

procedure TBaseHelper.BaseOnly;
begin
end;

procedure TBaseHelper.Shared;
begin
end;

procedure TChildHelper.Shared;
begin
end;

procedure Run;
var
  Child: TChild;
  Other: TOther;
begin
  Other.BaseOnly;
  Child.BaseOnly;
  Child.Shared;
end;

end.
"#;
    let source_uri = uri("HelperTargetAncestry");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("helper ancestry source parses");

    let base_only = index.navigate(
        &source_uri,
        position_of(source, "BaseOnly", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(base_only.len(), 1);
    assert_location_start(
        &base_only[0],
        &source_uri,
        position_of(source, "BaseOnly", 0),
    );

    let shadowed_base_only = index.navigate(
        &source_uri,
        position_of(source, "BaseOnly", 3),
        NavigationTarget::Declaration,
    );
    assert!(
        shadowed_base_only.is_empty(),
        "a more-specific helper must win instead of unioning helper members"
    );

    let shared = index.navigate(
        &source_uri,
        position_of(source, "Shared", 4),
        NavigationTarget::Declaration,
    );
    assert_eq!(shared.len(), 1);
    assert_location_start(&shared[0], &source_uri, position_of(source, "Shared", 1));
}

#[test]
fn helper_methods_resolve_implicit_target_members_without_breaking_local_shadowing() {
    let source = r#"unit HelperImplicitMembers;
interface
type
  TPoint = record
    X: Integer;
  end;
  TPointHelper = record helper for TPoint
    procedure Update;
    procedure Shadow;
  end;

implementation

procedure TPointHelper.Update;
begin
  X := 1;
  Self.X := 2;
end;

procedure TPointHelper.Shadow;
var
  X: Integer;
begin
  X := 3;
  Self.X := 4;
end;

end.
"#;
    let source_uri = uri("HelperImplicitMembers");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("implicit helper member source parses");

    let implicit = index.navigate(
        &source_uri,
        position_of(source, "X := 1", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(implicit.len(), 1);
    assert_location_start(&implicit[0], &source_uri, position_of(source, "X:", 0));

    let self_start = position_of(source, "Self.X", 0);
    let self_member = index.navigate(
        &source_uri,
        Position::new(self_start.line, self_start.character + 5),
        NavigationTarget::Declaration,
    );
    assert_eq!(self_member.len(), 1);
    assert_location_start(&self_member[0], &source_uri, position_of(source, "X:", 0));

    let local = index.navigate(
        &source_uri,
        position_of(source, "X := 3", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(local.len(), 1);
    assert_location_start(&local[0], &source_uri, position_of(source, "X: Integer", 1));
}

#[test]
fn specialized_generic_helpers_are_available_in_completion() {
    let source = r#"unit GenericHelperCompletion;
interface
type
  TBox<T> = class
  end;
  TBoxHelper = class helper for TBox<Integer>
    procedure Touch;
  end;

procedure Run;

implementation

procedure TBoxHelper.Touch;
begin
end;

procedure Run;
var
  Box: TBox<Integer>;
begin
  Box.Touch;
  Box.
end;

end.
"#;
    let source_uri = uri("GenericHelperCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic helper completion source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "Box.", 1))
        .expect("generic helper completion succeeds");
    assert!(
        completion.items.iter().any(|item| item.label == "Touch"),
        "specialized helper member missing: {:?}",
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn imported_helpers_use_only_the_last_helper_in_uses_order() {
    let provider = r#"unit HelperTarget;
interface
type
  TWidget = class
  end;
implementation
end.
"#;
    let first_helper = r#"unit FirstWidgetHelper;
interface
uses HelperTarget;
type
  TFirstWidgetHelper = class helper for TWidget
    procedure Shared;
    procedure FirstOnly;
  end;
implementation
procedure TFirstWidgetHelper.Shared;
begin
end;
procedure TFirstWidgetHelper.FirstOnly;
begin
end;
end.
"#;
    let second_helper = r#"unit SecondWidgetHelper;
interface
uses HelperTarget;
type
  TSecondWidgetHelper = class helper for TWidget
    procedure Shared;
    procedure SecondOnly;
  end;
implementation
procedure TSecondWidgetHelper.Shared;
begin
end;
procedure TSecondWidgetHelper.SecondOnly;
begin
end;
end.
"#;
    let consumer = r#"unit HelperConsumer;
interface
uses HelperTarget, FirstWidgetHelper, SecondWidgetHelper;
procedure Run;
implementation
procedure Run;
var
  Widget: TWidget;
begin
  Widget.Shared;
  Widget.FirstOnly;
  Widget.SecondOnly;
end;
end.
"#;
    let reverse_consumer = r#"unit ReverseHelperConsumer;
interface
uses HelperTarget, SecondWidgetHelper, FirstWidgetHelper;
procedure Run;
implementation
procedure Run;
var
  Widget: TWidget;
begin
  Widget.Shared;
  Widget.FirstOnly;
  Widget.SecondOnly;
end;
end.
"#;

    let provider_uri = uri("HelperTarget");
    let first_uri = uri("FirstWidgetHelper");
    let second_uri = uri("SecondWidgetHelper");
    let consumer_uri = uri("HelperConsumer");
    let reverse_uri = uri("ReverseHelperConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri, provider.to_owned())
        .expect("helper target parses");
    index
        .update(first_uri.clone(), first_helper.to_owned())
        .expect("first helper parses");
    index
        .update(second_uri.clone(), second_helper.to_owned())
        .expect("second helper parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("helper consumer parses");
    index
        .update(reverse_uri.clone(), reverse_consumer.to_owned())
        .expect("reverse helper consumer parses");

    let shared = index.navigate(
        &consumer_uri,
        position_of(consumer, "Shared", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(shared.len(), 1);
    assert_location_start(
        &shared[0],
        &second_uri,
        position_of(second_helper, "Shared", 0),
    );
    assert!(
        index
            .navigate(
                &consumer_uri,
                position_of(consumer, "FirstOnly", 0),
                NavigationTarget::Declaration,
            )
            .is_empty(),
        "an inactive helper must not leak its unique member"
    );
    let second_only = index.navigate(
        &consumer_uri,
        position_of(consumer, "SecondOnly", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(second_only.len(), 1);
    assert_location_start(
        &second_only[0],
        &second_uri,
        position_of(second_helper, "SecondOnly", 0),
    );

    let reverse_shared = index.navigate(
        &reverse_uri,
        position_of(reverse_consumer, "Shared", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(reverse_shared.len(), 1);
    assert_location_start(
        &reverse_shared[0],
        &first_uri,
        position_of(first_helper, "Shared", 0),
    );
    assert!(
        index
            .navigate(
                &reverse_uri,
                position_of(reverse_consumer, "SecondOnly", 0),
                NavigationTarget::Declaration,
            )
            .is_empty(),
        "reversing uses order must switch the active helper"
    );
}

#[test]
fn unknown_conditional_import_blocks_known_helper_selection() {
    let target = r#"unit ConditionalHelperTarget;
interface
type
  TWidget = class
  end;
implementation
end.
"#;
    let known_helper = r#"unit KnownConditionalHelper;
interface
uses ConditionalHelperTarget;
type
  TKnownHelper = class helper for TWidget
    procedure Touch;
  end;
implementation
procedure TKnownHelper.Touch;
begin
end;
end.
"#;
    let maybe_helper = r#"unit MaybeConditionalHelper;
interface
uses ConditionalHelperTarget;
type
  TMaybeHelper = class helper for TWidget
    procedure Touch;
  end;
implementation
procedure TMaybeHelper.Touch;
begin
end;
end.
"#;
    let consumer = r#"unit ConditionalHelperConsumer;
interface
uses
  ConditionalHelperTarget,
  KnownConditionalHelper,
  {$IF CompilerVersion >= 24}
  MaybeConditionalHelper
  {$ENDIF};
implementation
procedure Run;
var
  Widget: ConditionalHelperTarget.TWidget;
begin
  Widget.Touch;
end;
end.
"#;

    let target_uri = uri("ConditionalHelperTarget");
    let known_uri = uri("KnownConditionalHelper");
    let maybe_uri = uri("MaybeConditionalHelper");
    let consumer_uri = uri("ConditionalHelperConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(target_uri, target.to_owned())
        .expect("conditional helper target parses");
    index
        .update(known_uri, known_helper.to_owned())
        .expect("known conditional helper parses");
    index
        .update(maybe_uri, maybe_helper.to_owned())
        .expect("maybe conditional helper parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("conditional helper consumer parses");

    assert!(
        index
            .navigate(
                &consumer_uri,
                position_of(consumer, "Touch", 0),
                NavigationTarget::Declaration,
            )
            .is_empty(),
        "a matching helper hidden behind an unknown conditional import must block a unique target"
    );

    let reverse_consumer = consumer.replace(
        "ConditionalHelperTarget,\n  KnownConditionalHelper,\n  {$IF CompilerVersion >= 24}\n  MaybeConditionalHelper\n  {$ENDIF};",
        "ConditionalHelperTarget,\n  {$IF CompilerVersion >= 24}\n  MaybeConditionalHelper\n  {$ENDIF},\n  KnownConditionalHelper;",
    );
    let reverse_uri = uri("ReverseConditionalHelperConsumer");
    index
        .update(reverse_uri.clone(), reverse_consumer.clone())
        .expect("reverse conditional helper consumer parses");
    let reverse_locations = index.navigate(
        &reverse_uri,
        position_of(&reverse_consumer, "Touch", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(reverse_locations.len(), 1);
    assert_location_start(
        &reverse_locations[0],
        &uri("KnownConditionalHelper"),
        position_of(known_helper, "Touch;", 0),
    );
}

const PROVIDER: &str = r#"unit Provider;
interface

type
  TWidget = class
  public
    FValue: Integer;
    procedure DoThing;
    property Value: Integer read FValue;
  end;

procedure PublicRoutine;
procedure Overloaded(Value: Integer); overload;
procedure Overloaded(Value: string); overload;

implementation

procedure TWidget.DoThing;
begin
  Self.FValue := 1;
end;

procedure PublicRoutine;
begin
end;

procedure Overloaded(Value: Integer);
begin
end;

procedure Overloaded(Value: string);
begin
end;

end.
"#;

const MAIN: &str = r#"unit Main;
interface

uses Provider;

type
  TMain = class
    FInterfaceWidget: TWidget;
    FImplementationWidget: TImplementationWidget;
  end;

procedure Run;
procedure LocalShadowOne;
procedure LocalShadowTwo;

implementation

uses ImplementationProvider;

procedure Run;
var
  Widget: TWidget;
begin
  PublicRoutine;
  pUbLiCrOuTiNe;
  ImplementationRoutine;
  Widget.DoThing;
  Widget.FValue;
  Widget.Value;
  Self.DoThing;
  Unknown.DoThing;
  Overloaded(1);
end;

procedure LocalShadowOne;
var
  Shadowed: Integer;
begin
  Shadowed := 1;
end;

procedure LocalShadowTwo;
var
  Shadowed: Integer;
begin
  Shadowed := 2;
end;

end.
"#;

const IMPLEMENTATION_PROVIDER: &str = r#"unit ImplementationProvider;
interface
procedure ImplementationRoutine;
type TImplementationWidget = class end;
implementation
procedure ImplementationRoutine;
begin
end;
end.
"#;

const QUALIFIED_MAIN: &str = r#"unit QualifiedMain;
interface
uses Provider;
implementation
procedure Run;
begin
  TWidget.DoThing;
  Provider.PublicRoutine;
end;
end.
"#;

const UNRELATED_PROVIDER: &str = r#"unit UnrelatedProvider;
interface
procedure UnrelatedRoutine;
implementation
procedure UnrelatedRoutine;
begin
end;
end.
"#;

const UNRELATED_CALLER: &str = r#"unit UnrelatedCaller;
interface
implementation
procedure Run;
begin
  UnrelatedRoutine;
  UnrelatedProvider.UnrelatedRoutine;
end;
end.
"#;

const PRIVATE_PROVIDER: &str = r#"unit PrivateProvider;
interface
implementation
procedure HiddenRoutine;
begin
end;
end.
"#;

const PRIVATE_CALLER: &str = r#"unit PrivateCaller;
interface
implementation
uses PrivateProvider;
procedure Run;
begin
  HiddenRoutine;
end;
end.
"#;

const PARAMETER_SCOPE: &str = r#"unit ParameterScope;
interface
var Value: Integer;
procedure Caller;
implementation
procedure Caller(Value: Integer);
begin
  Value := 1;
end;
end.
"#;

const MULTI_PARAMETER_SCOPE: &str = r#"unit MultiParameterScope;
interface
procedure Caller(A, B: Integer);
implementation
procedure Caller(A, B: Integer);
begin
  B := A;
end;
end.
"#;

const MEMBER_PRECEDENCE_PROVIDER: &str = r#"unit MemberPrecedenceProvider;
interface
var
  FValue: Integer;
implementation
end.
"#;

const MEMBER_PRECEDENCE_CALLER: &str = r#"unit MemberPrecedenceCaller;
interface
uses MemberPrecedenceProvider;
type
  TWidget = class
    FValue: Integer;
    procedure Run;
  end;
var
  FValue: Integer;
implementation
procedure TWidget.Run;
begin
  FValue := 1;
end;
end.
"#;

const METHOD_LOCAL_SCOPE: &str = r#"unit MethodLocalScope;
interface
type
  TWidget = class
    FValue: Integer;
    procedure Run;
  end;
implementation
procedure TWidget.Run;
var
  FValue: Integer;
begin
  FValue := 1;
end;
end.
"#;

const TYPES_UNIT: &str = r#"unit Types;
interface
type
  TWidget = class
    Field: Integer;
  end;
implementation
end.
"#;

const TYPED_PROVIDER: &str = r#"unit TypedProvider;
interface
uses Types;
var
  Widget: TWidget;
implementation
end.
"#;

const TYPED_CALLER: &str = r#"unit TypedCaller;
interface
uses TypedProvider;
type
  TWidget = class
    Field: Integer;
  end;
implementation
procedure Run;
begin
  Widget.Field;
end;
end.
"#;

const CONDITIONAL_TYPE_PROVIDER: &str = r#"unit ConditionalTypeProvider;
interface

type
  TMDIBDatabase = class
  public
    procedure Execute;
  end;

  TMDIBSQL = class
    procedure ExecQuery; {$IFDEF DELPHI_XE6_UP}reintroduce{$ELSE}override{$ENDIF};
  end;

implementation

procedure TMDIBDatabase.Execute;
begin
end;

procedure TMDIBSQL.ExecQuery;
begin
end;

end.
"#;

const CONDITIONAL_TYPE_CALLER: &str = r#"unit ConditionalTypeCaller;
interface
uses ConditionalTypeProvider;
implementation
procedure Run;
var
  Database: TMDIBDatabase;
  GenericDatabase: TList<TMDIBDatabase>;
begin
  Database.Execute;
end;
end.
"#;

const NAMESPACED_TYPES: &str = r#"unit Ns.U;
interface
procedure P;
type
  TType = class
    Field: Integer;
  end;
implementation
procedure P;
begin
end;
end.
"#;

const NAMESPACED_CALLER: &str = r#"unit NamespacedCaller;
interface
uses Ns.U;
type
  TType = class
    Field: Integer;
  end;
  TAlias = Ns.U.TType;
implementation
var
  Value: Ns.U.TType;
procedure Run;
begin
  Ns.U.TType.Field;
  Value.Field;
  Ns.U.P;
end;
end.
"#;

const LOCAL_CLASS: &str = r#"unit LocalClass;
interface
procedure Run;
implementation
type
  TLocal = class
    Field: Integer;
    procedure Work(Value: Integer);
  end;
procedure TLocal.Work;
begin
  Value := 1;
  Self.Field := 1;
end;
procedure Run;
var
  X: TLocal;
begin
  X.Work;
end;
end.
"#;

const NESTED_METHOD: &str = r#"unit NestedMethod;
interface
type
  TWidget = class
    FValue: Integer;
    procedure Run;
  end;
implementation
procedure TWidget.Run;
  procedure Nested;
  begin
    Self.FValue := 1;
  end;
begin
end;
end.
"#;

const SELF_PARAMETER: &str = r#"unit SelfParameter;
interface
type
  TWidget = class
    procedure Run(Arg: Integer);
  end;
implementation
procedure TWidget.Run(Arg: Integer);
begin
  Self.Arg := 1;
end;
end.
"#;

const RECEIVER_UNIT: &str = r#"unit ReceiverUnit;
interface
procedure Work;
implementation
procedure Work;
begin
end;
end.
"#;

const RECEIVER_SHADOW: &str = r#"unit ReceiverShadow;
interface
uses ReceiverUnit;
implementation
procedure Run;
var
  ReceiverUnit: TUnknown;
begin
  ReceiverUnit.Work;
end;
end.
"#;

const ABBREVIATED_UNIQUE: &str = r#"unit AbbreviatedUnique;
interface
procedure P(X: Integer);
procedure Call;
implementation
procedure P;
begin
  X := 1;
end;
procedure Call;
begin
  P(1);
end;
end.
"#;

const ABBREVIATED_AMBIGUOUS: &str = r#"unit AbbreviatedAmbiguous;
interface
procedure P(X: Integer); overload;
procedure P(X: string); overload;
procedure Call;
implementation
procedure P;
begin
  X := 1;
end;
procedure Call;
begin
  P(1);
end;
end.
"#;

const UNIT_U: &str = r#"unit U;
interface
type
  T = record
    P: Integer;
  end;
implementation
end.
"#;

const QUALIFIED_BINDING_SHADOW: &str = r#"unit QualifiedBindingShadow;
interface
uses U;
type
  TLeaf = record
    P: Integer;
  end;
  THolder = record
    T: TLeaf;
  end;
implementation
var
  U: THolder;
procedure Run;
begin
  U.T.P;
end;
end.
"#;

const RECORD_LOCAL_SCOPE: &str = r#"unit RecordLocalScope;
interface
var
  Value: Integer;
implementation
type
  TLocal = record
    Value: Integer;
  end;
procedure Run;
begin
  Value := 1;
end;
end.
"#;

const CYCLIC_DECLARED_TYPE: &str = r#"unit U;
interface
type
  T = class
    X: U.T.X;
  end;
implementation
procedure Run;
var
  Obj: T;
begin
  Obj.X.Foo;
end;
end.
"#;

const PROPERTY_ACCESSOR_CONTEXT: &str = r#"unit PropertyAccessorContext;
interface
var
  FValue: Integer;
type
  T = class
    FValue: Integer;
    function GetValue: Integer;
    procedure SetValue(const AValue: Integer);
    property Value: Integer read FValue write FValue;
    property MethodValue: Integer read GetValue write SetValue;
  end;
implementation
end.
"#;

const PROPERTY_NAVIGATION: &str = r#"unit PropertyNavigation;
interface
var
  fFieldDelimiter: string;
function GetDisplayName: string;
procedure SetWriteOnly(const Value: string);
type
  TBase = class
    function GetInherited: string;
  end;
  TConfig = class
  private
    fFieldDelimiter: string;
    function GetDisplayName: string;
    procedure SetFieldDelimiter(const Value: string);
    procedure SetWriteOnly(const Value: string);
  public
    property FieldDelimiter: string read fFieldDelimiter write SetFieldDelimiter;
    property DisplayName: string read GetDisplayName;
    property WriteOnly: string write SetWriteOnly;
    property Missing: string read MissingAccessor;
    property SelfCycle: string read SelfCycle;
    property Alias: string read FieldDelimiter;
    property InheritedValue: string read GetInherited;
  end;
  TOther = class
  private
    fFieldDelimiter: string;
    function GetDisplayName: string;
    procedure SetWriteOnly(const Value: string);
  end;
implementation
function GetDisplayName: string;
begin
  Result := 'global';
end;
procedure SetWriteOnly(const Value: string);
begin
end;
function TBase.GetInherited: string;
begin
  Result := 'base';
end;
function TConfig.GetDisplayName: string;
begin
  Result := fFieldDelimiter;
end;
procedure TConfig.SetFieldDelimiter(const Value: string);
begin
  fFieldDelimiter := Value;
end;
procedure TConfig.SetWriteOnly(const Value: string);
begin
  fFieldDelimiter := Value;
end;
function TOther.GetDisplayName: string;
begin
  Result := 'other';
end;
procedure TOther.SetWriteOnly(const Value: string);
begin
end;
end.
"#;

const PROPERTY_CALLER: &str = r#"unit PropertyCaller;
interface
uses PropertyNavigation;
implementation
procedure Run;
var
  Config: TConfig;
begin
  Config.FieldDelimiter := '|';
end;
end.
"#;

const RECEIVER_BUDGET_SOURCE_PREFIX: &str = r#"unit ReceiverBudget;
interface
implementation
procedure Run;
begin
  "#;

const RECEIVER_BUDGET_SOURCE_SUFFIX: &str = r#";
end;
end.
"#;

const NESTED_ROUTINE_SCOPE: &str = r#"unit NestedRoutineScope;
interface
procedure Helper;
procedure Run;
implementation
procedure Run;
  procedure Helper;
  begin
  end;
begin
  Helper;
end;
procedure Helper;
begin
end;
end.
"#;

const INHERITED_CLASS_MEMBERS: &str = r#"unit InheritedClassMembers;
interface
type
  TBase = class
  private
    BaseField: Integer;
  public
    procedure BaseMethod;
  end;
  TChild = class(TBase)
    procedure ChildMethod;
  end;
implementation
procedure TBase.BaseMethod;
begin
  BaseField := 1;
end;
procedure TChild.ChildMethod;
begin
  BaseField := 2;
  BaseMethod;
end;
procedure Caller;
var
  Obj: TChild;
begin
  Obj.BaseField;
  Obj.BaseMethod;
end;
end.
"#;

const INHERITED_PROVIDER: &str = r#"unit InheritedProvider;
interface
type
  TBase = class
  public
    CrossField: Integer;
    procedure CrossMethod;
  end;
implementation
procedure TBase.CrossMethod;
begin
  CrossField := 1;
end;
end.
"#;

const INHERITED_CONSUMER: &str = r#"unit InheritedConsumer;
interface
uses InheritedProvider;
type
  TChild = class(InheritedProvider.TBase)
    procedure Run;
  end;
implementation
procedure TChild.Run;
begin
  CrossField := 1;
  CrossMethod;
end;
procedure Caller;
var
  Obj: TChild;
begin
  Obj.CrossField;
  Obj.CrossMethod;
end;
end.
"#;

const INHERITED_INTERFACE_MEMBERS: &str = r#"unit InheritedInterfaceMembers;
interface
type
  IBase = interface
    procedure BaseMethod;
  end;
  IChild = interface(IBase)
    procedure ChildMethod;
  end;
implementation
procedure Caller;
var
  Obj: IChild;
begin
  Obj.BaseMethod;
  Obj.ChildMethod;
end;
end.
"#;

const DECLARED_MEMBER_TYPE_PROVIDER: &str = r#"unit DeclaredMemberTypeProvider;
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

const DECLARED_MEMBER_TYPE_CONSUMER: &str = r#"unit DeclaredMemberTypeConsumer;
interface
uses DeclaredMemberTypeProvider;
type
  TP = class
    Shared: string;
  end;
  TChild = class(DeclaredMemberTypeProvider.TBase)
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

const CLASS_INTERFACE_PARENTS: &str = r#"unit ClassInterfaceParents;
interface
type
  IFoo = interface
    procedure ContractOnly;
    procedure Shared;
  end;
  TBase = class
    procedure Shared;
  end;
  TChild = class(TBase, IFoo)
  end;
implementation
procedure TBase.Shared;
begin
end;
procedure Caller;
var
  Obj: TChild;
begin
  Obj.ContractOnly;
  Obj.Shared;
end;
end.
"#;

const PER_NAME_AMBIGUOUS_INHERITANCE: &str = r#"unit PerNameAmbiguousInheritance;
interface
type
  IA = interface
    procedure Shared;
    procedure Unique;
  end;
  IB = interface
    procedure Shared;
  end;
  IMid = interface(IA, IB)
  end;
  IChild = interface(IMid)
    procedure Shared;
  end;
implementation
procedure Caller;
var
  C: IChild;
begin
  C.Unique;
  C.Shared;
end;
end.
"#;

const ASSISTANCE_PROVIDER: &str = r#"unit AssistanceProvider;
interface
type
  TWidget = class
  private
    MethodHidden: Integer;
  public
    Member: Integer;
    procedure PublicMethod;
  end;
procedure PublicRoutine;
implementation
procedure TWidget.PublicMethod;
begin
  MethodHidden := 1;
end;
procedure PrivateRoutine;
begin
end;
procedure PublicRoutine;
begin
end;
end.
"#;

const ASSISTANCE_MAIN: &str = r#"unit AssistanceMain;
interface
uses AssistanceProvider;
var
  Shadowed: Integer;
implementation
procedure Run;
var
  LocalName: Integer;
  shadowed: Integer;
  Obj: TWidget;
begin
  Loc;
  sha;
  Pri;
  Obj.Me;
  Expr.Me;
end;
end.
"#;

const SIGNATURE_SOURCE: &str = r#"unit SignatureSource;
interface
procedure Run(A, B: Integer; C: string; D: Integer); overload;
procedure Run(A: string); overload;
procedure Other(X, Y: Integer);
implementation
procedure Run(A, B: Integer; C: string; D: Integer);
begin
end;
procedure Run(A: string);
begin
end;
procedure Other(X, Y: Integer);
begin
end;
procedure Caller;
begin
  Run(Other(1, 2), 'a,b', [1,2], 4);
end;
end.
"#;

#[test]
fn overload_selection_drives_navigation_completion_and_signature_help() {
    let source = r#"unit OverloadSelection;
interface
type
  TIntResult = class
    IntMember: Integer;
  end;
  TStringResult = class
    StringMember: string;
  end;
function Select(Value: Integer): TIntResult; overload;
function Select(Value: string): TStringResult; overload;
implementation
function Select(Value: Integer): TIntResult;
begin
  Result := TIntResult.Create;
end;
function Select(Value: string): TStringResult;
begin
  Result := TStringResult.Create;
end;
procedure Caller;
begin
  Select(1).IntMember;
  Select('text' ).StringMember;
end;
end.
"#;
    let source_uri = uri("OverloadSelection");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("overload selection source parses");

    let integer_call = position_of(source, "Select(1)", 0);
    let integer_navigation =
        index.navigate(&source_uri, integer_call, NavigationTarget::Declaration);
    assert_eq!(integer_navigation.len(), 1);
    assert_location_start(
        &integer_navigation[0],
        &source_uri,
        position_of(source, "Select(Value: Integer)", 0),
    );

    let string_call = position_of(source, "Select('text' )", 0);
    let string_navigation = index.navigate(&source_uri, string_call, NavigationTarget::Declaration);
    assert_eq!(string_navigation.len(), 1);
    assert_location_start(
        &string_navigation[0],
        &source_uri,
        position_of(source, "Select(Value: string)", 0),
    );

    let integer_completion = index
        .completion(&source_uri, position_after(source, "Select(1).IntM", 0))
        .expect("integer result completion");
    assert_eq!(
        integer_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["IntMember"]
    );

    let string_completion = index
        .completion(
            &source_uri,
            position_after(source, "Select('text' ).StringM", 0),
        )
        .expect("string result completion");
    assert_eq!(
        string_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["StringMember"]
    );

    let integer_signature = index
        .signature_help(&source_uri, position_after(source, "Select(1", 0))
        .expect("integer signature help")
        .expect("integer call signature");
    assert_eq!(integer_signature.active_signature, Some(0));
    assert_eq!(integer_signature.signatures.len(), 2);

    let string_signature = index
        .signature_help(&source_uri, position_after(source, "Select('text' ", 0))
        .expect("string signature help")
        .expect("string call signature");
    assert_eq!(string_signature.active_signature, Some(1));
    assert_eq!(string_signature.signatures.len(), 2);
}

#[test]
fn nil_selects_reference_overloads_but_preserves_nil_ambiguity() {
    let source = r#"unit NilOverloads;
interface
type
  TClassArgument = class
  end;
  TOtherClassArgument = class
  end;
  TClassResult = class
    ClassMember: Integer;
  end;
  TIntegerResult = class
    IntegerMember: Integer;
  end;
  TOtherResult = class
    OtherMember: Integer;
  end;
function Select(Value: TClassArgument): TClassResult; overload;
function Select(Value: Integer): TIntegerResult; overload;
function Ambiguous(Value: TClassArgument): TClassResult; overload;
function Ambiguous(Value: TOtherClassArgument): TOtherResult; overload;
implementation
procedure Caller;
begin
  Select(nil).ClassMember;
  Ambiguous(nil).ClassMember;
end;
end.
"#;
    let source_uri = uri("NilOverloads");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("nil overload source parses");

    let selected = index.navigate(
        &source_uri,
        position_of(source, "Select(nil)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(selected.len(), 1);
    assert_location_start(
        &selected[0],
        &source_uri,
        position_of(source, "Select(Value: TClassArgument)", 0),
    );

    let ambiguous = index.navigate(
        &source_uri,
        position_of(source, "Ambiguous(nil)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(ambiguous.len(), 2);
}

#[test]
fn overload_selection_handles_typed_nested_calls_defaults_and_parameter_modes() {
    let source = r#"unit TypedOverloads;
interface
type
  TIntResult = class
    IntMember: Integer;
  end;
  TStringResult = class
    StringMember: Integer;
  end;
  TVarResult = class
    VarMember: Integer;
  end;
  TConstResult = class
    ConstMember: Integer;
  end;
function Pick(Value: Integer; Extra: Integer = 0): TIntResult; overload;
function Pick(Value: string): TStringResult; overload;
function Wrap(Value: TIntResult): TVarResult; overload;
function Wrap(Value: TStringResult): TConstResult; overload;
function Mutate(var Value: Integer): TVarResult; overload;
function Mutate(const Value: Integer): TConstResult; overload;
function Store(out Value: Integer): TVarResult; overload;
function Store(const Value: Integer): TConstResult; overload;
implementation
procedure Caller(Number: Integer; Text: string);
var
  Local: Integer;
begin
  Pick(Number).IntMember;
  Pick(1, 2).IntMember;
  Pick(Text).StringMember;
  Wrap(Pick(Number)).VarMember;
  Wrap(Pick(Text)).ConstMember;
  Mutate(1).ConstMember;
  Store(1).ConstMember;
  Mutate(Local).VarMember;
end;
end.
"#;
    let source_uri = uri("TypedOverloads");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("typed overload source parses");

    let number_navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick(Number)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(number_navigation.len(), 1);
    assert_location_start(
        &number_navigation[0],
        &source_uri,
        position_of(source, "Pick(Value: Integer; Extra", 0),
    );

    let default_navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick(1, 2)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(default_navigation.len(), 1);
    assert_location_start(
        &default_navigation[0],
        &source_uri,
        position_of(source, "Pick(Value: Integer; Extra", 0),
    );

    let string_navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick(Text)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(string_navigation.len(), 1);
    assert_location_start(
        &string_navigation[0],
        &source_uri,
        position_of(source, "Pick(Value: string)", 0),
    );

    let nested_number_completion = index
        .completion(
            &source_uri,
            position_after(source, "Wrap(Pick(Number)).VarM", 0),
        )
        .expect("nested number overload completion");
    assert_eq!(
        nested_number_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["VarMember"]
    );

    let nested_string_completion = index
        .completion(
            &source_uri,
            position_after(source, "Wrap(Pick(Text)).ConstM", 0),
        )
        .expect("nested string overload completion");
    assert_eq!(
        nested_string_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["ConstMember"]
    );

    let literal_var_completion = index
        .completion(&source_uri, position_after(source, "Mutate(1).ConstM", 0))
        .expect("var overload literal completion");
    assert_eq!(
        literal_var_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["ConstMember"]
    );

    let literal_out_completion = index
        .completion(&source_uri, position_after(source, "Store(1).ConstM", 0))
        .expect("out overload literal completion");
    assert_eq!(
        literal_out_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["ConstMember"]
    );

    let lvalue_completion = index
        .completion(&source_uri, position_after(source, "Mutate(Local).VarM", 0))
        .expect("var overload lvalue completion");
    assert!(lvalue_completion.items.is_empty());
    assert!(lvalue_completion.is_incomplete);
}

#[test]
fn overload_selection_rejects_properties_and_const_arguments_for_var_and_out() {
    let source = r#"unit NonAssignableOverloads;
interface
type
  TVarResult = class
    VarMember: Integer;
  end;
  TConstResult = class
    ConstMember: Integer;
  end;
  TOutResult = class
    OutMember: Integer;
  end;
  TWidget = class
    FValue: Integer;
    property P: Integer read FValue;
  end;
function Pick(var Value: Integer): TVarResult; overload;
function Pick(const Value: Integer): TConstResult; overload;
function Store(out Value: Integer): TOutResult; overload;
function Store(const Value: Integer): TConstResult; overload;
implementation
procedure Caller(const C: Integer; Widget: TWidget);
begin
  Pick(C).ConstMember;
  Pick(Widget.P).ConstMember;
  Store(C).ConstMember;
  Store(Widget.P).ConstMember;
end;
end.
"#;
    let source_uri = uri("NonAssignableOverloads");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("non-assignable overload source parses");

    for (call, declaration) in [
        ("Pick(C)", "Pick(const Value: Integer)"),
        ("Pick(Widget.P)", "Pick(const Value: Integer)"),
        ("Store(C)", "Store(const Value: Integer)"),
        ("Store(Widget.P)", "Store(const Value: Integer)"),
    ] {
        let navigation = index.navigate(
            &source_uri,
            position_of(source, call, 0),
            NavigationTarget::Declaration,
        );
        assert_eq!(navigation.len(), 1, "{call} must have one viable overload");
        assert_location_start(
            &navigation[0],
            &source_uri,
            position_of(source, declaration, 0),
        );
    }
}

#[test]
fn overload_selection_requires_exact_types_for_var_and_out_parameters() {
    let source = r#"unit ExactVarOutTypes;
interface
type
  TGrand = class
  end;
  TBase = class(TGrand)
  end;
  TChild = class(TBase)
  end;
  TGrandResult = class
    GrandMember: Integer;
  end;
  TBaseResult = class
    BaseMember: Integer;
  end;
function Pick(var Value: TBase): TBaseResult; overload;
function Pick(Value: TGrand): TGrandResult; overload;
implementation
procedure Caller(const C: TChild);
begin
  Pick(C).GrandMember;
end;
end.
"#;
    let source_uri = uri("ExactVarOutTypes");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("exact var/out source parses");

    let navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick(C)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(navigation.len(), 1);
    assert_location_start(
        &navigation[0],
        &source_uri,
        position_of(source, "Pick(Value: TGrand)", 0),
    );
}

#[test]
fn overload_selection_resolves_builtins_after_lexical_and_unit_bindings() {
    let arbitrary = r#"unit ArbitraryInteger;
interface
type
  Integer = class
  end;
end.
"#;
    let source = r#"unit BuiltinShadowing;
interface
uses ArbitraryInteger;
type
  Integer = class
  end;
  TShadowResult = class
    ShadowMember: Integer;
  end;
  TSystemResult = class
    SystemMember: Integer;
  end;
  TUnitResult = class
    UnitMember: Integer;
  end;
function Pick(Value: Integer): TShadowResult; overload;
function Pick(Value: System.Integer): TSystemResult; overload;
function UnitPick(Value: ArbitraryInteger.Integer): TUnitResult; overload;
function UnitPick(Value: System.Integer): TSystemResult; overload;
implementation
procedure Caller;
begin
  Pick(1).SystemMember;
  UnitPick(1).SystemMember;
end;
end.
"#;
    let arbitrary_uri = uri("ArbitraryInteger");
    let source_uri = uri("BuiltinShadowing");
    let mut index = NavigationIndex::new();
    index
        .update(arbitrary_uri, arbitrary.to_owned())
        .expect("arbitrary integer source parses");
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("builtin shadowing source parses");

    for call in ["Pick(1)", "UnitPick(1)"] {
        let navigation = index.navigate(
            &source_uri,
            position_of(source, call, 0),
            NavigationTarget::Declaration,
        );
        assert_eq!(navigation.len(), 1, "{call} must select System.Integer");
        assert_location_start(
            &navigation[0],
            &source_uri,
            position_of(
                source,
                if call == "Pick(1)" {
                    "Pick(Value: System.Integer)"
                } else {
                    "UnitPick(Value: System.Integer)"
                },
                0,
            ),
        );
    }
}

#[test]
fn overload_selection_parses_radix_literals_before_exponents() {
    let source = r#"unit RadixOverloads;
interface
type
  TIntResult = class
    IntMember: Integer;
  end;
  TRealResult = class
    RealMember: Integer;
  end;
function Pick(Value: Integer): TIntResult; overload;
function Pick(Value: Real): TRealResult; overload;
implementation
procedure Caller;
begin
  Pick($FE).IntMember;
end;
end.
"#;
    let source_uri = uri("RadixOverloads");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("radix overload source parses");

    let navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick($FE)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(navigation.len(), 1);
    assert_location_start(
        &navigation[0],
        &source_uri,
        position_of(source, "Pick(Value: Integer)", 0),
    );
}

#[test]
fn overload_selection_distinguishes_string_fragments_from_characters() {
    let source = r#"unit LiteralFragments;
interface
type
  TIntResult = class
    IntMember: Integer;
  end;
  TStringResult = class
    StringMember: Integer;
  end;
function Pick(Value: Integer): TIntResult; overload;
function Pick(Value: string): TStringResult; overload;
function Ord(Value: Char): Integer;
implementation
procedure Caller(CharValue: Char);
begin
  Pick('A'#66).StringMember;
  Pick(#65#66).StringMember;
  Pick(#65'B').StringMember;
  Pick('A''B').StringMember;
  Pick(#65).StringMember;
  Pick(CharValue).StringMember;
  Pick(Ord(CharValue)).IntMember;
end;
end.
"#;
    let source_uri = uri("LiteralFragments");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("literal fragment source parses");

    let string_navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick('A'#66)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(string_navigation.len(), 1);
    assert_location_start(
        &string_navigation[0],
        &source_uri,
        position_of(source, "Pick(Value: string)", 0),
    );

    for call in [
        "Pick(#65#66)",
        "Pick(#65'B')",
        "Pick('A''B')",
        "Pick(#65)",
        "Pick(CharValue)",
    ] {
        let navigation = index.navigate(
            &source_uri,
            position_of(source, call, 0),
            NavigationTarget::Declaration,
        );
        assert_eq!(navigation.len(), 1, "{call} must use the string overload");
        assert_location_start(
            &navigation[0],
            &source_uri,
            position_of(source, "Pick(Value: string)", 0),
        );
    }

    let ordinal_navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick(Ord(CharValue))", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(ordinal_navigation.len(), 1);
    assert_location_start(
        &ordinal_navigation[0],
        &source_uri,
        position_of(source, "Pick(Value: Integer)", 0),
    );
}

#[test]
fn overload_selection_classifies_hex_codepoint_fragments_conservatively() {
    let source = r#"unit HexCodepointFragments;
interface
type
  TCharResult = class
    CharMember: Integer;
  end;
  TStringResult = class
    StringMember: Integer;
  end;
function Pick(Value: Char; Extra: Integer): TCharResult; overload;
function Pick(Value: string; Extra: Integer): TStringResult; overload;
implementation
procedure Caller;
begin
  Pick(#65, 1).CharMember;
  Pick(#$41, 1).CharMember;
  Pick(#$41#66, 1).StringMember;
  Pick(#$41'B', 1).StringMember;
  Pick(#$41#$42, 1).StringMember;
  Pick(#$GG, 1).StringMember;
end;
end.
"#;
    let source_uri = uri("HexCodepointFragments");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("hex codepoint source parses");

    for (call, declaration) in [
        ("Pick(#65, 1)", "Pick(Value: Char; Extra: Integer)"),
        ("Pick(#$41, 1)", "Pick(Value: Char; Extra: Integer)"),
        ("Pick(#$41#66, 1)", "Pick(Value: string; Extra: Integer)"),
        ("Pick(#$41'B', 1)", "Pick(Value: string; Extra: Integer)"),
        ("Pick(#$41#$42, 1)", "Pick(Value: string; Extra: Integer)"),
    ] {
        let navigation = index.navigate(
            &source_uri,
            position_of(source, call, 0),
            NavigationTarget::Declaration,
        );
        assert_eq!(navigation.len(), 1, "{call} must select one overload");
        assert_location_start(
            &navigation[0],
            &source_uri,
            position_of(source, declaration, 0),
        );
    }

    let hex_completion = index
        .completion(
            &source_uri,
            position_after(source, "Pick(#$41, 1).CharM", 0),
        )
        .expect("hex codepoint result completion");
    assert_eq!(
        hex_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["CharMember"]
    );

    let hex_signature_result = index
        .signature_help(&source_uri, position_after(source, "Pick(#$41, 1", 0))
        .expect("hex codepoint signature help");
    let hex_signature = hex_signature_result.expect("hex codepoint call signature");
    let char_signature = hex_signature.signatures.iter().position(|signature| {
        signature
            .label
            .contains("Pick(Value: Char; Extra: Integer)")
    });
    assert!(char_signature.is_some());
    assert_eq!(hex_signature.signatures.len(), 2);
    assert_eq!(
        hex_signature.active_signature,
        char_signature.map(|index| index as u32)
    );

    let unsupported_completion = index
        .completion(
            &source_uri,
            position_after(source, "Pick(#$GG, 1).StringM", 0),
        )
        .expect("unsupported codepoint result completion");
    assert!(unsupported_completion.items.is_empty());
    assert!(unsupported_completion.is_incomplete);
}

#[test]
fn overload_selection_keeps_integer_alias_parameters_unknown_for_literals() {
    let source = r#"unit IntegerAliasOverloads;
interface
type
  TNum = Integer;
  TIntResult = class
    IntMember: Integer;
  end;
  TDoubleResult = class
    DoubleMember: Integer;
  end;
function Pick(Value: TNum): TIntResult; overload;
function Pick(Value: Double): TDoubleResult; overload;
implementation
procedure Caller;
begin
  Pick(1).IntMember;
end;
end.
"#;
    let source_uri = uri("IntegerAliasOverloads");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("integer alias source parses");

    let navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick(1)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(
        navigation.len(),
        2,
        "an unsupported integer alias parameter must remain ambiguous for a literal"
    );
}

#[test]
fn overload_selection_treats_value_parameters_as_writable_var_arguments() {
    let source = r#"unit WritableValueParameters;
interface
type
  TVarResult = class
    VarMember: Integer;
  end;
  TDoubleResult = class
    DoubleMember: Integer;
  end;
function Pick(var Value: Integer): TVarResult; overload;
function Pick(const Value: Double): TDoubleResult; overload;
implementation
procedure Run(C: Integer);
begin
  Pick(C).VarMember;
end;
end.
"#;
    let source_uri = uri("WritableValueParameters");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("writable value parameter source parses");

    let navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick(C)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(navigation.len(), 1);
    assert_location_start(
        &navigation[0],
        &source_uri,
        position_of(source, "Pick(var Value: Integer)", 0),
    );
}

#[test]
fn overload_selection_preserves_integer_width_and_literal_ranges() {
    let source = r#"unit IntegerKinds;
interface
type
  TByteResult = class
    ByteMember: Integer;
  end;
  TIntResult = class
    IntMember: Integer;
  end;
  TInt64Result = class
    Int64Member: Integer;
  end;
  TLongResult = class
    LongMember: Integer;
  end;
  TDoubleResult = class
    DoubleMember: Integer;
  end;
function RangePick(Value: Byte): TByteResult; overload;
function RangePick(Value: Integer): TIntResult; overload;
function WidthPick(Value: Integer): TIntResult; overload;
function WidthPick(Value: Int64): TInt64Result; overload;
function AliasPick(Value: LongInt): TLongResult; overload;
function AliasPick(Value: Double): TDoubleResult; overload;
implementation
procedure Caller(Number: Integer; Wide: Int64);
begin
  RangePick(1000).IntMember;
  WidthPick(Number).IntMember;
  WidthPick(Wide).Int64Member;
  AliasPick(Number).LongMember;
end;
end.
"#;
    let source_uri = uri("IntegerKinds");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("integer kinds source parses");

    for (call, declaration) in [
        ("RangePick(1000)", "RangePick(Value: Integer)"),
        ("WidthPick(Number)", "WidthPick(Value: Integer)"),
        ("WidthPick(Wide)", "WidthPick(Value: Int64)"),
        ("AliasPick(Number)", "AliasPick(Value: LongInt)"),
    ] {
        let navigation = index.navigate(
            &source_uri,
            position_of(source, call, 0),
            NavigationTarget::Declaration,
        );
        assert_eq!(
            navigation.len(),
            1,
            "{call} must select one integer-kind overload"
        );
        assert_location_start(
            &navigation[0],
            &source_uri,
            position_of(source, declaration, 0),
        );
    }
}

#[test]
fn overload_selection_does_not_charge_omitted_defaults_as_conversion_cost() {
    let source = r#"unit DefaultCosts;
interface
type
  TIntResult = class
    IntMember: Integer;
  end;
  TRealResult = class
    RealMember: Integer;
  end;
function Pick(Value: Real): TRealResult; overload;
function Pick(Value: Integer; Extra: Integer = 0): TIntResult; overload;
implementation
procedure Caller;
begin
  Pick(1).IntMember;
end;
end.
"#;
    let source_uri = uri("DefaultCosts");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("default cost source parses");

    let navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick(1)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(navigation.len(), 1);
    assert_location_start(
        &navigation[0],
        &source_uri,
        position_of(source, "Pick(Value: Integer; Extra", 0),
    );
}

#[test]
fn overload_selection_extends_method_overloads_through_inheritance() {
    let source = r#"unit ExtendedMethodOverloads;
interface
type
  TIntResult = class
    IntMember: Integer;
  end;
  TDoubleResult = class
    DoubleMember: Integer;
  end;
  TBase = class
    function Pick(Value: Integer): TIntResult; overload;
  end;
  TChild = class(TBase)
    function Pick(Value: Double): TDoubleResult; overload;
  end;
implementation
procedure Caller(Value: TChild);
begin
  Value.Pick(1).IntMember;
end;
end.
"#;
    let source_uri = uri("ExtendedMethodOverloads");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("extended method overload source parses");

    let navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick(1)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(navigation.len(), 1);
    assert_location_start(
        &navigation[0],
        &source_uri,
        position_of(source, "Pick(Value: Integer)", 0),
    );
}

#[test]
fn overload_selection_respects_reintroduced_method_hiding() {
    let source = r#"unit ReintroducedMethod;
interface
type
  TIntResult = class
    IntMember: Integer;
  end;
  TDoubleResult = class
    DoubleMember: Integer;
  end;
  TBase = class
    function Pick(Value: Integer): TIntResult;
  end;
  TChild = class(TBase)
    function Pick(Value: Double): TDoubleResult; reintroduce;
  end;
implementation
function TBase.Pick(Value: Integer): TIntResult;
begin
end;
function TChild.Pick(Value: Double): TDoubleResult;
begin
end;
procedure Caller(Value: TChild);
begin
  Value.Pick(1).DoubleMember;
end;
end.
"#;
    let source_uri = uri("ReintroducedMethod");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("reintroduced method source parses");

    let navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick(1)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(navigation.len(), 1);
    assert_location_start(
        &navigation[0],
        &source_uri,
        position_of(source, "Pick(Value: Double)", 0),
    );
}

#[test]
fn overload_selection_keeps_reintroduced_overloads_across_assistance() {
    let provider = r#"unit ReintroducedOverloadProvider;
interface
type
  TBaseResult = class
    BaseMember: Integer;
  end;
  TChildResult = class
    ChildMember: Integer;
  end;
  THiddenResult = class
    HiddenMember: Integer;
  end;
  TBase = class
    function Pick(Value: Integer): TBaseResult; overload;
  end;
  TChild = class(TBase)
    function Pick(Value: Double): TChildResult; reintroduce; overload;
  end;
  THiddenChild = class(TBase)
    function Pick(Value: Double): THiddenResult; reintroduce;
  end;
implementation
function TBase.Pick(Value: Integer): TBaseResult;
begin
end;
function TChild.Pick(Value: Double): TChildResult;
begin
end;
function THiddenChild.Pick(Value: Double): THiddenResult;
begin
end;
end.
"#;
    let consumer = r#"unit ReintroducedOverloadConsumer;
interface
uses ReintroducedOverloadProvider;
procedure Caller(C: TChild; H: THiddenChild);
implementation
procedure Caller(C: TChild; H: THiddenChild);
begin
  C.Pick(1).BaseMember;
  C.Pick(1.0).ChildMember;
  H.Pick(1).HiddenMember;
end;
end.
"#;
    let provider_uri = uri("ReintroducedOverloadProvider");
    let consumer_uri = uri("ReintroducedOverloadConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("reintroduced overload provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("reintroduced overload consumer parses");

    let integer_navigation = index.navigate(
        &consumer_uri,
        position_of(consumer, "Pick(1)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(integer_navigation.len(), 1);
    assert_location_start(
        &integer_navigation[0],
        &provider_uri,
        position_of(provider, "Pick(Value: Integer)", 0),
    );

    let base_completion = index
        .completion(
            &consumer_uri,
            position_after(consumer, "C.Pick(1).BaseM", 0),
        )
        .expect("base inherited result completion");
    assert_eq!(
        base_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["BaseMember"]
    );

    let child_completion = index
        .completion(
            &consumer_uri,
            position_after(consumer, "C.Pick(1.0).ChildM", 0),
        )
        .expect("child reintroduced result completion");
    assert_eq!(
        child_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["ChildMember"]
    );

    let integer_signature = index
        .signature_help(&consumer_uri, position_after(consumer, "C.Pick(1", 0))
        .expect("inherited overload signature help")
        .expect("inherited overload call signature");
    let integer_signature_index = integer_signature
        .signatures
        .iter()
        .position(|signature| signature.label.contains("Pick(Value: Integer)"));
    let double_signature_index = integer_signature
        .signatures
        .iter()
        .position(|signature| signature.label.contains("Pick(Value: Double)"));
    assert!(integer_signature_index.is_some());
    assert!(double_signature_index.is_some());
    assert_eq!(integer_signature.signatures.len(), 2);
    assert_eq!(
        integer_signature.active_signature,
        integer_signature_index.map(|index| index as u32)
    );

    let hidden_navigation = index.navigate(
        &consumer_uri,
        position_of(consumer, "Pick(1)", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(hidden_navigation.len(), 1);
    assert_location_start(
        &hidden_navigation[0],
        &provider_uri,
        position_of(provider, "Pick(Value: Double)", 1),
    );

    let hidden_completion = index
        .completion(
            &consumer_uri,
            position_after(consumer, "H.Pick(1).HiddenM", 0),
        )
        .expect("reintroduced hidden result completion");
    assert_eq!(
        hidden_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["HiddenMember"]
    );
}

#[test]
fn overload_selection_collapses_proven_method_overrides() {
    let source = r#"unit OverrideMethod;
interface
type
  TResult = class
    Member: Integer;
  end;
  TBase = class
    function Pick(Value: Integer): TResult; virtual;
  end;
  TChild = class(TBase)
    function Pick(Value: Integer): TResult; override;
  end;
implementation
function TBase.Pick(Value: Integer): TResult;
begin
end;
function TChild.Pick(Value: Integer): TResult;
begin
end;
procedure Caller(Value: TChild);
begin
  Value.Pick(1).Member;
end;
end.
"#;
    let source_uri = uri("OverrideMethod");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("override method source parses");

    let navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick(1)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(navigation.len(), 1);
    assert_location_start(
        &navigation[0],
        &source_uri,
        position_of(source, "Pick(Value: Integer)", 1),
    );
}

#[test]
fn overload_selection_infers_primitive_nested_call_results() {
    let source = r#"unit PrimitiveNestedResults;
interface
type
  TIntResult = class
    IntMember: Integer;
  end;
  TStringResult = class
    StringMember: Integer;
  end;
function GetInt: Integer;
function Pick(Value: Integer): TIntResult; overload;
function Pick(Value: string): TStringResult; overload;
implementation
function GetInt: Integer;
begin
  Result := 1;
end;
procedure Caller;
begin
  Pick(GetInt()).IntMember;
end;
end.
"#;
    let source_uri = uri("PrimitiveNestedResults");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("primitive nested result source parses");

    let navigation = index.navigate(
        &source_uri,
        position_of(source, "Pick(GetInt())", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(navigation.len(), 1);
    assert_location_start(
        &navigation[0],
        &source_uri,
        position_of(source, "Pick(Value: Integer)", 0),
    );
}

#[test]
fn overload_selection_classifies_parenthesized_literal_types() {
    let source = r#"unit ParenthesizedLiterals;
interface
type
  TStringResult = class
    StringMember: Integer;
  end;
  TBooleanResult = class
    BooleanMember: Integer;
  end;
  TClassArgument = class
  end;
  TClassResult = class
    ClassMember: Integer;
  end;
  TIntResult = class
    IntMember: Integer;
  end;
function Pick(Value: string): TStringResult; overload;
function Pick(Value: Boolean): TBooleanResult; overload;
function Pick(Value: TClassArgument): TClassResult; overload;
function Pick(Value: Integer): TIntResult; overload;
implementation
procedure Caller;
begin
  Pick(('text')).StringMember;
  Pick((True)).BooleanMember;
  Pick((nil)).ClassMember;
end;
end.
"#;
    let source_uri = uri("ParenthesizedLiterals");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("parenthesized literal source parses");

    for (call, declaration) in [
        ("Pick(('text'))", "Pick(Value: string)"),
        ("Pick((True))", "Pick(Value: Boolean)"),
        ("Pick((nil))", "Pick(Value: TClassArgument)"),
    ] {
        let navigation = index.navigate(
            &source_uri,
            position_of(source, call, 0),
            NavigationTarget::Declaration,
        );
        assert_eq!(
            navigation.len(),
            1,
            "{call} must select one literal overload"
        );
        assert_location_start(
            &navigation[0],
            &source_uri,
            position_of(source, declaration, 0),
        );
    }
}

#[test]
fn overload_selection_preserves_cross_unit_type_identity() {
    const FIRST: &str = r#"unit DistinctFirst;
interface
type
  TValue = class
  end;
  TFirstResult = class
    FirstMember: Integer;
  end;
function Choose(Value: TValue): TFirstResult; overload;
implementation
function Choose(Value: TValue): TFirstResult;
begin
end;
end.
"#;
    const SECOND: &str = r#"unit DistinctSecond;
interface
type
  TValue = class
  end;
  TSecondResult = class
    SecondMember: Integer;
  end;
function Choose(Value: TValue): TSecondResult; overload;
implementation
function Choose(Value: TValue): TSecondResult;
begin
end;
end.
"#;
    let consumer = r#"unit DistinctConsumer;
interface
uses DistinctFirst, DistinctSecond;
implementation
procedure Caller;
var
  Value: DistinctFirst.TValue;
begin
  Choose(Value).FirstMember;
end;
end.
"#;
    let first_uri = uri("DistinctFirst");
    let second_uri = uri("DistinctSecond");
    let consumer_uri = uri("DistinctConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(first_uri.clone(), FIRST.to_owned())
        .expect("first distinct type source parses");
    index
        .update(second_uri, SECOND.to_owned())
        .expect("second distinct type source parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("cross-unit overload source parses");

    let navigation = index.navigate(
        &consumer_uri,
        position_of(consumer, "Choose(Value)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(navigation.len(), 1);
    assert_location_start(
        &navigation[0],
        &first_uri,
        position_of(FIRST, "Choose(Value: TValue)", 0),
    );

    let completion = index
        .completion(
            &consumer_uri,
            position_after(consumer, "Choose(Value).FirstM", 0),
        )
        .expect("cross-unit overload completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["FirstMember"]
    );
}

#[test]
fn overload_selection_handles_inherited_methods_and_constructors() {
    let source = r#"unit MethodOverloads;
interface
type
  TIntResult = class
    IntMember: Integer;
  end;
  TStringResult = class
    StringMember: Integer;
  end;
  TBase = class
    function Make(Value: Integer): TIntResult; overload;
    function Make(Value: string): TStringResult; overload;
  end;
  TChild = class(TBase)
    ChildMember: Integer;
    constructor Create(Value: Integer); overload;
    constructor Create(Value: string); overload;
  end;
implementation
function TBase.Make(Value: Integer): TIntResult;
begin
end;
function TBase.Make(Value: string): TStringResult;
begin
end;
constructor TChild.Create(Value: Integer);
begin
end;
constructor TChild.Create(Value: string);
begin
end;
procedure Caller;
var
  Value: TChild;
begin
  Value.Make(1).IntMember;
  Value.Make('text').StringMember;
  TChild.Create(1).ChildMember;
  TChild.Create('text').ChildMember;
end;
end.
"#;
    let source_uri = uri("MethodOverloads");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("method overload source parses");

    let base_method = index.navigate(
        &source_uri,
        position_of(source, "Make(1)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(base_method.len(), 1);
    assert_location_start(
        &base_method[0],
        &source_uri,
        position_of(source, "Make(Value: Integer)", 0),
    );

    let child_method = index.navigate(
        &source_uri,
        position_of(source, "Make('text')", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(child_method.len(), 1);
    assert_location_start(
        &child_method[0],
        &source_uri,
        position_of(source, "Make(Value: string)", 0),
    );

    let base_completion = index
        .completion(&source_uri, position_after(source, "Value.Make(1).IntM", 0))
        .expect("inherited method result completion");
    assert_eq!(
        base_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["IntMember"]
    );

    let child_completion = index
        .completion(
            &source_uri,
            position_after(source, "Value.Make('text').StringM", 0),
        )
        .expect("child method result completion");
    assert_eq!(
        child_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["StringMember"]
    );

    let integer_constructor = index.navigate(
        &source_uri,
        position_of(source, "Create(1)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(integer_constructor.len(), 1);
    assert_location_start(
        &integer_constructor[0],
        &source_uri,
        position_of(source, "Create(Value: Integer)", 0),
    );

    let string_constructor = index.navigate(
        &source_uri,
        position_of(source, "Create('text')", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(string_constructor.len(), 1);
    assert_location_start(
        &string_constructor[0],
        &source_uri,
        position_of(source, "Create(Value: string)", 0),
    );

    let constructor_completion = index
        .completion(
            &source_uri,
            position_after(source, "TChild.Create(1).ChildM", 0),
        )
        .expect("constructor result completion");
    assert_eq!(
        constructor_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["ChildMember"]
    );
}

#[test]
fn overload_selection_keeps_equal_and_unknown_matches_ambiguous() {
    let source = r#"unit AmbiguousOverloads;
interface
type
  TIntResult = class
    IntMember: Integer;
  end;
  TCardinalResult = class
    CardinalMember: Integer;
  end;
  TRealResult = class
    RealMember: Integer;
  end;
function Tie(Value: Integer): TIntResult; overload;
function Tie(Value: Cardinal): TCardinalResult; overload;
function Prefer(Value: Integer): TIntResult; overload;
function Prefer(Value: Real): TRealResult; overload;
function Solo(Value: Integer): TIntResult; overload;
implementation
procedure Caller;
begin
  Tie(1).IntMember;
  Prefer(1).IntMember;
  Prefer(UnknownValue).IntMember;
  Solo(UnknownValue).IntMember;
  Solo('text').IntMember;
end;
end.
"#;
    let source_uri = uri("AmbiguousOverloads");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("ambiguous overload source parses");

    let equal_navigation = index.navigate(
        &source_uri,
        position_of(source, "Tie(1)", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(equal_navigation.len(), 2);

    let widening_completion = index
        .completion(&source_uri, position_after(source, "Prefer(1).IntM", 0))
        .expect("widening overload completion");
    assert_eq!(
        widening_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["IntMember"]
    );

    let unknown_completion = index
        .completion(
            &source_uri,
            position_after(source, "Prefer(UnknownValue).IntM", 0),
        )
        .expect("unknown overload completion");
    assert!(unknown_completion.items.is_empty());
    assert!(unknown_completion.is_incomplete);

    let single_unknown_completion = index
        .completion(
            &source_uri,
            position_after(source, "Solo(UnknownValue).IntM", 0),
        )
        .expect("single unknown overload completion");
    assert!(single_unknown_completion.items.is_empty());
    assert!(single_unknown_completion.is_incomplete);

    let incompatible_navigation = index.navigate(
        &source_uri,
        position_of(source, "Solo('text')", 0),
        NavigationTarget::Declaration,
    );
    assert!(incompatible_navigation.is_empty());

    let unknown_signature = index
        .signature_help(
            &source_uri,
            position_after(source, "Prefer(UnknownValue", 0),
        )
        .expect("unknown overload signature help")
        .expect("unknown overload signatures");
    assert_eq!(unknown_signature.signatures.len(), 2);
    assert_eq!(unknown_signature.active_signature, None);
}

#[test]
fn signature_help_keeps_overloads_for_an_incomplete_known_call() {
    let source = r#"unit IncompleteOverloads;
interface
type
  TIntResult = class
  end;
  TStringResult = class
  end;
function Pick(Value: Integer): TIntResult; overload;
function Pick(Value: string): TStringResult; overload;
implementation
procedure Caller;
begin
  Pick(1
end;
end.
"#;
    let source_uri = uri("IncompleteOverloads");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("incomplete overload source parses");

    let help = index
        .signature_help(&source_uri, position_after(source, "Pick(1", 0))
        .expect("incomplete overload signature help")
        .expect("incomplete overload call");
    assert_eq!(help.signatures.len(), 2);
    assert_eq!(help.active_signature, Some(0));
    assert_eq!(help.active_parameter, Some(0));
}

#[test]
fn signature_help_rejects_an_oversized_overload_selection() {
    let mut source = String::from("unit OverloadSelectionLimit;\ninterface\n");
    for index in 0..129 {
        writeln!(&mut source, "procedure Run(Value: T{index}); overload;")
            .expect("write overload declaration");
    }
    source.push_str("implementation\nprocedure Caller;\nbegin\n  Run(UnknownValue);\nend;\nend.\n");
    let source_uri = uri("OverloadSelectionLimit");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("overload selection limit source parses");

    let error = index
        .signature_help(&source_uri, position_after(&source, "  Run(", 0))
        .expect_err("overload selection must be bounded");
    assert!(
        error.contains("overload selection exceeds the 128-group limit"),
        "{error}"
    );
}

#[test]
fn completion_projection_is_scope_aware_and_uses_plain_identifier_edits() {
    let mut index = NavigationIndex::new();
    let main_uri = uri("AssistanceMain");
    let provider_uri = uri("AssistanceProvider");
    index
        .update(provider_uri, ASSISTANCE_PROVIDER.to_string())
        .expect("assistance provider parses");
    index
        .update(main_uri.clone(), ASSISTANCE_MAIN.to_string())
        .expect("assistance main parses");

    let local = index
        .completion(&main_uri, position_after(ASSISTANCE_MAIN, "Loc", 0))
        .expect("local completion projection");
    assert_eq!(
        local
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["LocalName"]
    );
    let local_edit = match local.items[0]
        .text_edit
        .as_ref()
        .expect("completion must provide a text edit")
    {
        CompletionTextEdit::Edit(edit) => edit,
        CompletionTextEdit::InsertAndReplace(_) => {
            panic!("completion must use a plain TextEdit")
        }
    };
    assert_eq!(local_edit.new_text, "LocalName");
    assert_eq!(
        local_edit.range,
        Range::new(
            position_of(ASSISTANCE_MAIN, "Loc", 0),
            position_after(ASSISTANCE_MAIN, "LocalName", 0),
        )
    );

    let shadowed = index
        .completion(&main_uri, position_after(ASSISTANCE_MAIN, "sha", 0))
        .expect("shadowed completion projection");
    assert_eq!(
        shadowed
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["shadowed"]
    );

    let member = index
        .completion(&main_uri, position_after(ASSISTANCE_MAIN, "Me", 0))
        .expect("member completion projection");
    assert_eq!(
        member
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Member"]
    );

    let private = index
        .completion(&main_uri, position_after(ASSISTANCE_MAIN, "Pri", 0))
        .expect("private imported completion projection");
    assert!(private.items.is_empty());

    let unknown_member = index
        .completion(&main_uri, position_after(ASSISTANCE_MAIN, "Me", 1))
        .expect("unknown receiver completion projection");
    assert!(unknown_member.items.is_empty());
}

#[test]
fn signature_projection_keeps_source_labels_and_counts_nested_arguments() {
    let source_uri = uri("SignatureSource");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), SIGNATURE_SOURCE.to_string())
        .expect("signature source parses");

    let help = index
        .signature_help(&source_uri, position_after(SIGNATURE_SOURCE, "[1,2], ", 0))
        .expect("signature help projection")
        .expect("cursor is inside a supported call");
    assert_eq!(help.active_signature, None);
    assert_eq!(help.active_parameter, Some(3));
    assert_eq!(
        help.signatures
            .iter()
            .map(|signature| signature.label.as_str())
            .collect::<Vec<_>>(),
        [
            "procedure Run(A, B: Integer; C: string; D: Integer);",
            "procedure Run(A: string);",
        ]
    );
    assert_eq!(
        help.signatures[0]
            .parameters
            .as_ref()
            .expect("expanded grouped parameters")
            .iter()
            .map(|parameter| match &parameter.label {
                lsp_types::ParameterLabel::Simple(_) => panic!("expected UTF-16 offsets"),
                lsp_types::ParameterLabel::LabelOffsets(offsets) => *offsets,
            })
            .collect::<Vec<_>>(),
        [[14, 15], [17, 18], [29, 30], [40, 41]]
    );
}

#[test]
fn signature_help_resolves_a_known_object_qualified_call() {
    let source = r#"unit QualifiedObject;
interface
type
  TObj = class
    procedure Run(A: Integer);
  end;
var
  Obj: TObj;
procedure Run(A: string);
implementation
procedure TObj.Run(A: Integer);
begin
end;
procedure Run(A: string);
begin
end;
procedure Caller;
begin
  Obj.Run(1);
end;
end.
"#;
    let source_uri = uri("QualifiedObject");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("qualified object source parses");

    let result = index
        .signature_help(&source_uri, position_after(source, "Obj.Run(1", 0))
        .expect("qualified object signature projection");
    assert!(result.is_some(), "known object call result: {result:?}");
    let help = result.expect("known object call must resolve");
    assert_eq!(
        help.signatures
            .iter()
            .map(|signature| signature.label.as_str())
            .collect::<Vec<_>>(),
        ["procedure Run(A: Integer);"]
    );
}

#[test]
fn signature_help_resolves_an_imported_unit_qualified_call() {
    let provider = r#"unit QualifiedProvider;
interface
procedure Run(A: Integer);
implementation
procedure Run(A: Integer);
begin
end;
end.
"#;
    let consumer = r#"unit QualifiedConsumer;
interface
uses QualifiedProvider;
procedure Run(A: string);
implementation
procedure Run(A: string);
begin
end;
procedure Caller;
begin
  QualifiedProvider.Run(1);
end;
end.
"#;
    let provider_uri = uri("QualifiedProvider");
    let consumer_uri = uri("QualifiedConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri, provider.to_owned())
        .expect("qualified provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("qualified consumer parses");

    let help = index
        .signature_help(
            &consumer_uri,
            position_after(consumer, "QualifiedProvider.Run(", 0),
        )
        .expect("qualified unit signature projection")
        .expect("imported unit call must resolve");
    assert_eq!(
        help.signatures
            .iter()
            .map(|signature| signature.label.as_str())
            .collect::<Vec<_>>(),
        ["procedure Run(A: Integer);"]
    );
}

#[test]
fn signature_help_rejects_unknown_formals_but_keeps_known_headers_with_unknown_bodies() {
    let source = r#"unit ConditionalFormal;
interface
procedure OptionalFormal(A: Integer {$IFDEF MAYBE}; B: string{$ENDIF});
procedure KnownHeader(A: Integer);
implementation
procedure OptionalFormal(A: Integer {$IFDEF MAYBE}; B: string{$ENDIF});
begin
{$IFDEF MAYBE}
  WriteLn(B);
{$ENDIF}
end;
procedure KnownHeader(A: Integer);
begin
{$IFDEF MAYBE}
  WriteLn(A);
{$ENDIF}
end;
procedure Caller;
begin
  OptionalFormal(1);
  KnownHeader(1);
end;
end.
"#;
    let source_uri = uri("ConditionalFormal");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("conditional formal source parses");

    let optional = index
        .signature_help(&source_uri, position_after(source, "OptionalFormal(1", 0))
        .expect("conditional formal signature projection");
    assert!(
        optional.is_none(),
        "a conditional formal list must not produce definite signature metadata: {optional:?}"
    );

    let known_header = index
        .signature_help(&source_uri, position_after(source, "KnownHeader(1", 0))
        .expect("known header signature projection")
        .expect("body-only uncertainty must not hide a known header");
    assert_eq!(known_header.signatures.len(), 1);
    assert_eq!(
        known_header.signatures[0].parameters.as_ref().map(Vec::len),
        Some(1)
    );
}

#[test]
fn inline_variables_are_visible_only_inside_their_blocks() {
    let source = r#"unit BlockInline;
interface
var
  Value: Integer;
  Inferred: Integer;
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
      var Inferred := Value;
      Inferred := 3;
    end;
  end;
  Value := 4;
  Inferred := 5;
end;
end.
"#;
    let source_uri = uri("BlockInline");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("inline variable source parses");

    let global_value = position_of(source, "Value: Integer", 0);
    let local_value = position_of(source, "Value: Integer", 1);
    let global_inferred = position_of(source, "Inferred: Integer", 0);
    let nested_inferred = position_of(source, "Inferred := 3", 0);

    let before_block = index.navigate(
        &source_uri,
        position_of(source, "Value := 1", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(before_block.len(), 1);
    assert_location_start(&before_block[0], &source_uri, global_value);

    let inside_block = index.navigate(
        &source_uri,
        position_of(source, "Value := 2", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(inside_block.len(), 1);
    assert_location_start(&inside_block[0], &source_uri, local_value);

    let nested_use = index.navigate(&source_uri, nested_inferred, NavigationTarget::Declaration);
    assert_eq!(nested_use.len(), 1);
    assert_location_start(
        &nested_use[0],
        &source_uri,
        position_of(source, "Inferred :=", 0),
    );

    let after_block = index.navigate(
        &source_uri,
        position_of(source, "Value := 4", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(after_block.len(), 1);
    assert_location_start(&after_block[0], &source_uri, global_value);

    let after_nested_block = index.navigate(
        &source_uri,
        position_of(source, "Inferred := 5", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(after_nested_block.len(), 1);
    assert_location_start(&after_nested_block[0], &source_uri, global_inferred);
}

#[test]
fn unknown_member_access_keeps_overload_and_result_resolution_uncertain() {
    let provider = r#"unit UnknownOverloadProvider;
interface
type
  TProtectedResult = class
    ProtectedMember: Integer;
  end;
  TPublicResult = class
    PublicMember: Integer;
  end;
  TBase = class
  strict protected
    function Pick(Value: Integer): TProtectedResult; overload;
  public
    function Pick(Value: Int64): TPublicResult; overload;
  end;
implementation
function TBase.Pick(Value: Integer): TProtectedResult;
begin
  Result := TProtectedResult.Create;
end;
function TBase.Pick(Value: Int64): TPublicResult;
begin
  Result := TPublicResult.Create;
end;
end.
"#;
    let consumer = r#"unit UnknownOverloadConsumer;
interface
uses UnknownOverloadProvider;
type
  TChild = class(MissingBase)
    procedure Run;
  end;
implementation
procedure TChild.Run;
var
  Obj: TBase;
  N: Integer;
begin
  Obj.Pick(N);
  Obj.Pick(N).PublicMember;
end;
end.
"#;
    let provider_uri = uri("UnknownOverloadProvider");
    let consumer_uri = uri("UnknownOverloadConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("unknown overload provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("unknown overload consumer parses");

    let navigation = index.navigate(
        &consumer_uri,
        position_of(consumer, "Pick(N)", 0),
        NavigationTarget::Declaration,
    );
    assert!(
        navigation.is_empty(),
        "unknown access selected a public overload: {navigation:?}"
    );

    let signature = index
        .signature_help(&consumer_uri, position_after(consumer, "Obj.Pick(N", 0))
        .expect("unknown access signature help");
    assert!(
        signature.is_none(),
        "unknown access produced a definite signature: {signature:?}"
    );

    let result_navigation = index.navigate(
        &consumer_uri,
        position_of(consumer, "PublicMember", 0),
        NavigationTarget::Declaration,
    );
    assert!(
        result_navigation.is_empty(),
        "unknown access selected a public result: {result_navigation:?}"
    );

    let completion = index
        .completion(
            &consumer_uri,
            position_after(consumer, "Obj.Pick(N).Pub", 0),
        )
        .expect("unknown access result completion");
    assert!(
        completion.items.is_empty() && completion.is_incomplete,
        "unknown access produced a definite result completion: {completion:?}"
    );
}

fn run_deep_qualified_signature_repro() {
    let mut deep_source = String::from(
        "unit DeepQualified;\ninterface\nprocedure Run(A: Integer);\nimplementation\nprocedure Run(A: Integer);\nbegin\nend;\nprocedure Caller;\nbegin\n  ",
    );
    deep_source.push_str(&"A.".repeat(4_096));
    deep_source.push_str("Run(1);\nend;\nend.\n");
    let deep_uri = uri("DeepQualified");
    let deep_call = deep_source.rfind("Run(1)").expect("deep qualified call");
    let deep_position = text::offset_to_position(&deep_source, deep_call + "Run(".len())
        .expect("deep qualified position");

    let healthy_source = r#"unit HealthyQualified;
interface
type
  TObj = class
    procedure Run(A: Integer);
  end;
var
  Obj: TObj;
implementation
procedure TObj.Run(A: Integer);
begin
end;
procedure Caller;
begin
  Obj.Run(1);
end;
end.
"#;
    let healthy_uri = uri("HealthyQualified");
    let healthy_position = position_after(healthy_source, "Obj.Run(1", 0);

    let mut index = NavigationIndex::new();
    index
        .update(deep_uri.clone(), deep_source)
        .expect("deep qualified source parses");
    index
        .update(healthy_uri.clone(), healthy_source.to_owned())
        .expect("healthy qualified source parses");

    let deep_result = index.signature_help(&deep_uri, deep_position);
    assert!(
        matches!(deep_result, Ok(None) | Err(_)),
        "an over-depth qualified request must fail closed: {deep_result:?}"
    );

    let healthy = index
        .signature_help(&healthy_uri, healthy_position)
        .expect("healthy qualified request must remain available")
        .expect("healthy qualified request must resolve");
    assert_eq!(
        healthy
            .signatures
            .iter()
            .map(|signature| signature.label.as_str())
            .collect::<Vec<_>>(),
        ["procedure Run(A: Integer);"]
    );
}

#[test]
fn deep_qualified_signature_help_is_stack_safe() {
    if std::env::var_os("PASCAL_LSP_DEEP_QUALIFIED_CHILD").is_some() {
        run_deep_qualified_signature_repro();
        return;
    }

    let output = Command::new(std::env::current_exe().expect("test executable path"))
        .args([
            "--exact",
            "deep_qualified_signature_help_child",
            "--nocapture",
        ])
        .env("PASCAL_LSP_DEEP_QUALIFIED_CHILD", "1")
        .output()
        .expect("run deep qualified child");
    assert!(
        output.status.success(),
        "deep qualified child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn deep_qualified_signature_help_child() {
    if std::env::var_os("PASCAL_LSP_DEEP_QUALIFIED_CHILD").is_none() {
        return;
    }

    let stack = std::thread::Builder::new()
        .name("deep-qualified-analysis".to_owned())
        .stack_size(2 * 1024 * 1024);
    stack
        .spawn(run_deep_qualified_signature_repro)
        .expect("spawn deep qualified analysis")
        .join()
        .expect("deep qualified analysis must not overflow its stack");
}

#[test]
fn completion_reports_conditional_uncertainty_instead_of_a_complete_empty_list() {
    let source = r#"unit ConditionalCompletion;
interface
const
{$IFDEF MAYBE}
  MaybeName = 1;
{$ENDIF}
implementation
procedure Run;
begin
  Ma;
end;
end.
"#;
    let source_uri = uri("ConditionalCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("conditional completion source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "Ma", 1))
        .expect("conditional completion projection");
    assert!(
        completion.items.is_empty(),
        "unexpected large-context labels: {:?}",
        completion
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(
        completion.is_incomplete,
        "unknown conditional candidates must not look complete"
    );
}

#[test]
fn completion_reports_an_unknown_conditional_cursor_as_incomplete() {
    let source = r#"unit UnknownCursorCompletion;
interface
implementation
procedure Run;
begin
{$IFDEF MAYBE}
  Maybe;
{$ENDIF}
end;
end.
"#;
    let source_uri = uri("UnknownCursorCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown-cursor completion source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "  Maybe", 0))
        .expect("unknown-cursor completion projection");
    assert!(completion.items.is_empty());
    assert!(
        completion.is_incomplete,
        "an unknown conditional cursor must not look complete"
    );
}

#[test]
fn completion_keeps_a_known_local_shadowing_an_uncertain_import() {
    let provider = r#"unit ConditionalProvider;
interface
{$IFDEF MAYBE}
const
  Shadowed = 1;
{$ENDIF}
implementation
end.
"#;
    let main = r#"unit ConditionalMain;
interface
uses ConditionalProvider;
implementation
procedure Run;
var
  Shadowed: Integer;
begin
  Sha;
end;
end.
"#;
    let provider_uri = uri("ConditionalProvider");
    let main_uri = uri("ConditionalMain");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri, provider.to_owned())
        .expect("conditional provider parses");
    index
        .update(main_uri.clone(), main.to_owned())
        .expect("conditional consumer parses");

    let completion = index
        .completion(&main_uri, position_after(main, "Sha", 0))
        .expect("shadowing completion projection");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Shadowed"]
    );
    assert!(
        !completion.is_incomplete,
        "a known local binding hides an uncertain imported binding"
    );
}

#[test]
fn completion_does_not_offer_symbols_inside_comments_or_strings() {
    let source = r#"unit LexicalCompletion;
interface
const
  VisibleName = 1;
implementation
procedure Run;
begin
  // Vis
  WriteLn('Vis');
  Vis;
end;
end.
"#;
    let source_uri = uri("LexicalCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("lexical completion source parses");

    let comment = index
        .completion(&source_uri, position_after(source, "  // Vis", 0))
        .expect("comment completion projection");
    assert!(comment.items.is_empty());
    assert!(!comment.is_incomplete);

    let string_start = position_of(source, "'Vis'", 0);
    let string = index
        .completion(
            &source_uri,
            Position::new(string_start.line, string_start.character + 2),
        )
        .expect("string completion projection");
    assert!(string.items.is_empty());
    assert!(!string.is_incomplete);

    let code_start = position_of(source, "  Vis;", 0);
    let code = index
        .completion(
            &source_uri,
            Position::new(code_start.line, code_start.character + 5),
        )
        .expect("code completion projection");
    assert_eq!(
        code.items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["VisibleName"]
    );
}

#[test]
fn signature_help_ignores_comments_strings_and_unknown_receivers() {
    let source = r#"unit UnsupportedSignature;
interface
procedure Run(Value: Integer);
implementation
procedure Run(Value: Integer);
begin
end;
procedure Caller;
begin
  // Run(1)
  WriteLn('Run(1)');
  Unknown.Run(1);
end;
end.
"#;
    let source_uri = uri("UnsupportedSignature");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unsupported signature source parses");

    let comment_start = position_of(source, "Run(1)", 0);
    assert!(
        index
            .signature_help(
                &source_uri,
                Position::new(comment_start.line, comment_start.character + 4),
            )
            .expect("comment signature projection")
            .is_none()
    );

    let string_start = position_of(source, "Run(1)", 1);
    assert!(
        index
            .signature_help(
                &source_uri,
                Position::new(string_start.line, string_start.character + 4),
            )
            .expect("string signature projection")
            .is_none()
    );

    let unknown_start = position_of(source, "Unknown.Run(1)", 0);
    assert!(
        index
            .signature_help(
                &source_uri,
                Position::new(unknown_start.line, unknown_start.character + 12),
            )
            .expect("unknown receiver signature projection")
            .is_none()
    );
}

#[test]
fn signature_help_rejects_argument_scans_over_the_fixed_bound() {
    let mut source = String::from(
        "unit SignatureLimit;\ninterface\nprocedure Run(Value: Integer);\nimplementation\nprocedure Run(Value: Integer);\nbegin\nend;\nprocedure Caller;\nbegin\n  Run(",
    );
    source.push_str(&"1,".repeat(40_000));
    source.push_str("1);\nend;\nend.\n");
    let source_uri = uri("SignatureLimit");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("signature limit source parses");
    let call_start = source.rfind("Run(").expect("call open");
    let open = call_start + "Run".len();
    let position =
        text::offset_to_position(&source, open + 1 + 65_536).expect("signature limit position");
    assert_eq!(
        text::position_to_offset(&source, position),
        Some(open + 1 + 65_536)
    );
    let error = index
        .signature_help(&source_uri, position)
        .expect_err("oversized argument scan must fail closed");
    assert!(error.contains("65536-byte scan limit"), "{error}");
}

#[test]
fn completion_marks_only_truncated_response_lists_incomplete() {
    fn source_with_names(count: usize) -> String {
        let mut source =
            String::from("unit CompletionLimit;\ninterface\nimplementation\nprocedure Run;\nvar\n");
        for index in 0..count {
            writeln!(&mut source, "  Name{index:03}: Integer;").expect("write completion fixture");
        }
        source.push_str("begin\n  Nam\nend;\nend.\n");
        source
    }

    let source_uri = uri("CompletionLimit");
    let mut index = NavigationIndex::new();
    let exact_source = source_with_names(256);
    index
        .update(source_uri.clone(), exact_source.clone())
        .expect("exact completion source parses");
    let exact = index
        .completion(&source_uri, position_after(&exact_source, "  Nam", 0))
        .expect("exact completion projection");
    assert_eq!(exact.items.len(), 256);
    assert!(
        !exact.is_incomplete,
        "an untruncated exact-bound list is complete"
    );

    let overflowing_source = source_with_names(257);
    index
        .update(source_uri.clone(), overflowing_source.clone())
        .expect("overflowing completion source parses");
    let overflowing = index
        .completion(&source_uri, position_after(&overflowing_source, "  Nam", 0))
        .expect("overflowing completion projection");
    assert_eq!(overflowing.items.len(), 256);
    assert!(
        overflowing.is_incomplete,
        "a truncated list must be incomplete"
    );
}

#[test]
fn completion_replaces_the_entire_mid_token_with_utf16_prefix() {
    let marked = r#"unit MidTokenCompletion;
interface
implementation
procedure Caller;
var
  LocalName: Integer;
begin
  WriteLn('😀', Loc|alName);
end;
end.
"#;
    let cursor_offset = marked.find('|').expect("mid-token cursor");
    let source = marked.replacen('|', "", 1);
    let source_uri = uri("MidTokenCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("mid-token source parses");

    let position = text::offset_to_position(&source, cursor_offset).expect("mid-token position");
    let completion = index
        .completion(&source_uri, position)
        .expect("mid-token completion");
    assert_eq!(completion.items.len(), 1);
    let edit = match completion.items[0]
        .text_edit
        .as_ref()
        .expect("mid-token completion edit")
    {
        CompletionTextEdit::Edit(edit) => edit,
        CompletionTextEdit::InsertAndReplace(_) => panic!("expected a plain TextEdit"),
    };
    assert_eq!(edit.new_text, "LocalName");
    assert_eq!(
        edit.range,
        Range::new(
            position_of(&source, "Loc", 1),
            position_after(&source, "LocalName", 1),
        )
    );

    let start = text::position_to_offset(&source, edit.range.start).expect("edit start");
    let end = text::position_to_offset(&source, edit.range.end).expect("edit end");
    let mut applied = source.clone();
    applied.replace_range(start..end, &edit.new_text);
    assert!(applied.contains("WriteLn('😀', LocalName);"), "{applied}");
}

#[test]
fn completion_edit_application_covers_token_boundaries_unicode_and_escaped_identifiers() {
    fn apply_completion(marked: &str) -> String {
        let cursor_offset = marked.find('|').expect("completion cursor");
        let source = marked.replacen('|', "", 1);
        let source_uri = uri("CompletionEditBoundaries");
        let mut index = NavigationIndex::new();
        index
            .update(source_uri.clone(), source.clone())
            .expect("completion boundary source parses");
        let position =
            text::offset_to_position(&source, cursor_offset).expect("completion boundary position");
        let completion = index
            .completion(&source_uri, position)
            .expect("completion boundary request");
        let item = completion
            .items
            .iter()
            .find(|item| item.label == "LocalName")
            .expect("LocalName completion item");
        let edit = match item.text_edit.as_ref().expect("completion edit") {
            CompletionTextEdit::Edit(edit) => edit,
            CompletionTextEdit::InsertAndReplace(_) => panic!("expected a plain TextEdit"),
        };
        let start = text::position_to_offset(&source, edit.range.start).expect("edit start");
        let end = text::position_to_offset(&source, edit.range.end).expect("edit end");
        let mut applied = source;
        applied.replace_range(start..end, &edit.new_text);
        applied
    }

    let prefix = "unit CompletionEditBoundaries;\ninterface\nimplementation\nprocedure Caller;\nvar\n  LocalName: Integer;\nbegin\n";
    let suffix = "end;\nend.\n";
    for (case_name, marked_body, expected_body) in [
        ("start", "  |LocalName;\n", "  LocalName;\n"),
        ("middle", "  Loc|alName;\n", "  LocalName;\n"),
        ("end", "  LocalName|;\n", "  LocalName;\n"),
        ("whitespace", "  |;\n", "  LocalName;\n"),
        (
            "nonBMP",
            "  WriteLn('😀'); Lo|calName;\n",
            "  WriteLn('😀'); LocalName;\n",
        ),
        ("escaped", "  &Loc|alName;\n", "  LocalName;\n"),
    ] {
        let marked = format!("{prefix}{marked_body}{suffix}");
        let expected = format!("{prefix}{expected_body}{suffix}");
        assert_eq!(apply_completion(&marked), expected, "{case_name} edit");
    }
}

#[test]
fn completion_bare_dot_resolves_a_known_object_without_global_fallback() {
    let provider = r#"unit BareObjectProvider;
interface
type
  TObj = class
  public
    Member: Integer;
  end;
implementation
end.
"#;
    let main = r#"unit BareObjectMain;
interface
uses BareObjectProvider;
implementation
procedure Caller;
var
  Obj: TObj;
begin
  Obj.|;
end;
end.
"#;
    let provider_uri = uri("BareObjectProvider");
    let main_uri = uri("BareObjectMain");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri, provider.to_owned())
        .expect("bare object provider parses");
    index
        .update(main_uri.clone(), main.to_owned())
        .expect("bare object consumer parses");

    let completion = index
        .completion(&main_uri, position_after(main, "Obj.", 0))
        .expect("bare object completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Member"]
    );
}

#[test]
fn completion_bare_dot_resolves_an_imported_unit_without_global_fallback() {
    let provider = r#"unit BareUnitProvider;
interface
procedure UnitMember;
implementation
procedure UnitMember;
begin
end;
end.
"#;
    let main = r#"unit BareUnitMain;
interface
uses BareUnitProvider;
implementation
procedure Caller;
begin
  BareUnitProvider.|;
end;
end.
"#;
    let provider_uri = uri("BareUnitProvider");
    let main_uri = uri("BareUnitMain");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri, provider.to_owned())
        .expect("bare unit provider parses");
    index
        .update(main_uri.clone(), main.to_owned())
        .expect("bare unit consumer parses");

    let completion = index
        .completion(&main_uri, position_after(main, "BareUnitProvider.", 0))
        .expect("bare unit completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["UnitMember"]
    );
}

#[test]
fn completion_bare_dot_with_an_unknown_receiver_does_not_fallback_to_globals() {
    let source = r#"unit BareUnknownReceiver;
interface
const
  GlobalName = 1;
implementation
procedure Caller;
begin
  Expr.|;
end;
end.
"#;
    let source_uri = uri("BareUnknownReceiver");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown receiver source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "Expr.", 0))
        .expect("unknown receiver completion");
    assert!(completion.items.is_empty());
    assert!(!completion.is_incomplete);
}

#[test]
fn completion_bare_dot_with_an_expression_receiver_fails_closed() {
    let source = r#"unit BareExpressionReceiver;
interface
const
  GlobalName = 1;
function MakeValue: Integer;
implementation
function MakeValue: Integer;
begin
  Result := 1;
end;
procedure Caller;
begin
  MakeValue().|;
end;
end.
"#;
    let source_uri = uri("BareExpressionReceiver");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("expression receiver source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "MakeValue().", 0))
        .expect("expression receiver completion");
    assert!(completion.items.is_empty());
    assert!(!completion.is_incomplete);
}

#[test]
fn completion_bare_dot_allows_a_bounded_whitespace_gap_after_an_expression() {
    let marked = r#"unit BareExpressionWhitespace;
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
  MakeValue().   |;
end;
end.
"#;
    let cursor_offset = marked.find('|').expect("whitespace cursor");
    let source = marked.replacen('|', "", 1);
    let source_uri = uri("BareExpressionWhitespace");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("whitespace expression source parses");

    let completion = index
        .completion(
            &source_uri,
            text::offset_to_position(&source, cursor_offset).expect("whitespace position"),
        )
        .expect("whitespace expression completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Member"]
    );
}

#[test]
fn completion_bare_dot_rejects_an_unbounded_whitespace_gap_after_an_expression() {
    let gap = " ".repeat(257);
    let source = format!(
        "unit BareExpressionWhitespaceBound;\ninterface\ntype\n  TObj = class\n    Member: Integer;\n  end;\nfunction MakeValue: TObj;\nimplementation\nfunction MakeValue: TObj;\nbegin\n  Result := TObj.Create;\nend;\nprocedure Caller;\nbegin\n  MakeValue().{gap};\nend;\nend.\n"
    );
    let source_uri = uri("BareExpressionWhitespaceBound");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("bounded whitespace source parses");
    let cursor_offset = source
        .find(&format!("MakeValue().{gap}"))
        .map(|start| start + "MakeValue().".len() + gap.len())
        .expect("bounded whitespace cursor");

    let completion = index
        .completion(
            &source_uri,
            text::offset_to_position(&source, cursor_offset).expect("bounded whitespace position"),
        )
        .expect("bounded whitespace completion");
    assert!(completion.items.is_empty());
}

#[test]
fn completion_bare_dot_with_a_malformed_expression_receiver_fails_closed() {
    let source = r#"unit BareMalformedExpression;
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
  MakeValue(.   ;
end;
end.
"#;
    let source_uri = uri("BareMalformedExpression");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("malformed expression source parses with recovery");

    let completion = index
        .completion(&source_uri, position_after(source, "MakeValue(.   ", 0))
        .expect("malformed expression completion");
    assert!(completion.items.is_empty());
}

#[test]
fn completion_bare_dot_resolves_a_source_typed_function_call_receiver() {
    let marked = r#"unit SourceTypedExpressionReceiver;
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
  MakeValue().|;
end;
end.
"#;
    let cursor_offset = marked.find('|').expect("function receiver cursor");
    let source = marked.replacen('|', "", 1);
    let source_uri = uri("SourceTypedExpressionReceiver");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("source-typed expression receiver parses");

    let completion = index
        .completion(
            &source_uri,
            text::offset_to_position(&source, cursor_offset).expect("function receiver position"),
        )
        .expect("source-typed expression receiver completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Member"]
    );
}

#[test]
fn unknown_conditional_helpers_do_not_yield_a_unique_member() {
    let source = r#"unit UnknownConditionalHelper;
interface
type
  TWidget = class
  end;
{$IF CompilerVersion >= 24}
  TWidgetHelper = class helper for TWidget
    procedure Touch;
  end;
{$ENDIF}

procedure Run;

implementation

procedure TWidgetHelper.Touch;
begin
end;

procedure Run;
var
  Widget: TWidget;
begin
  Widget.Touch;
end;

end.
"#;
    let source_uri = uri("UnknownConditionalHelper");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown conditional helper source parses");

    assert!(
        index
            .navigate(
                &source_uri,
                position_of(source, "Touch", 2),
                NavigationTarget::Declaration,
            )
            .is_empty(),
        "an unknown conditional helper must not produce a unique member"
    );
}

#[test]
fn navigation_resolves_a_source_typed_function_call_receiver() {
    let source = r#"unit SourceTypedNavigationReceiver;
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
  MakeValue().Member;
end;
end.
"#;
    let source_uri = uri("SourceTypedNavigationReceiver");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("source-typed navigation receiver parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Member", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(&locations[0], &source_uri, position_of(source, "Member", 0));
}

#[test]
fn navigation_resolves_constructor_and_cast_expression_receivers() {
    let source = r#"unit ConstructorAndCastReceivers;
interface
type
  TWidget = class
    constructor Create;
    Member: Integer;
  end;
procedure Caller;
implementation
constructor TWidget.Create;
begin
end;
procedure Caller;
var
  Obj: TWidget;
begin
  TWidget.Create.Member;
  TWidget(Obj).Member;
  (Obj as TWidget).Member;
end;
end.
"#;
    let source_uri = uri("ConstructorAndCastReceivers");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("constructor and cast source parses");

    for occurrence in 1..=3 {
        let locations = index.navigate(
            &source_uri,
            position_of(source, "Member", occurrence),
            NavigationTarget::Declaration,
        );
        assert_eq!(locations.len(), 1, "Member occurrence {occurrence}");
        assert_location_start(&locations[0], &source_uri, position_of(source, "Member", 0));
    }

    let constructor_type = index.type_definitions(&source_uri, position_of(source, "Create", 2));
    assert_eq!(constructor_type.len(), 1);
    assert_location_start(
        &constructor_type[0],
        &source_uri,
        position_of(source, "TWidget", 0),
    );
}

#[test]
fn inherited_constructor_type_definition_uses_the_constructed_type() {
    let source = r#"unit InheritedConstructorTypeDefinition;
interface
type
  TBase = class
    constructor Create;
  end;
  TChild = class(TBase)
  end;
implementation
constructor TBase.Create;
begin
end;
procedure Caller;
begin
  TChild.Create;
end;
end.
"#;
    let source_uri = uri("InheritedConstructorTypeDefinition");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("inherited constructor source parses");

    let definition = index.navigate(
        &source_uri,
        position_of(source, "Create", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(definition.len(), 1);
    assert_location_start(
        &definition[0],
        &source_uri,
        position_of(source, "Create", 0),
    );

    let type_definition = index.type_definitions(&source_uri, position_of(source, "Create", 2));
    assert_eq!(type_definition.len(), 1);
    assert_location_start(
        &type_definition[0],
        &source_uri,
        position_of(source, "TChild", 0),
    );
}

#[test]
fn navigation_resolves_nested_function_result_member_chains() {
    let source = r#"unit NestedFunctionResultChain;
interface
type
  TLeaf = class
    Name: Integer;
  end;
  TFactory = class
    Child: TLeaf;
  end;
function Factory: TFactory;
implementation
function Factory: TFactory;
begin
  Result := TFactory.Create;
end;
procedure Caller;
begin
  Factory().Child.Name;
end;
end.
"#;
    let source_uri = uri("NestedFunctionResultChain");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("nested function result source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Name", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(&locations[0], &source_uri, position_of(source, "Name", 0));
}

#[test]
fn nested_constructor_call_receivers_stop_at_the_receiver_work_bound() {
    let chain_length = 14;
    let mut chain = String::from("  TObj");
    for _ in 0..chain_length {
        chain.push_str(".Create()");
    }
    chain.push_str(".Member;\n");
    let source = format!(
        "unit NestedConstructorReceiverBound;\ninterface\ntype\n  TObj = class\n    constructor Create;\n    Member: Integer;\n  end;\nimplementation\nconstructor TObj.Create;\nbegin\nend;\nprocedure Caller;\nbegin\n{chain}end;\nend.\n"
    );
    let source_uri = uri("NestedConstructorReceiverBound");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("nested constructor receiver source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(&source, "Member", 1),
        NavigationTarget::Declaration,
    );
    assert!(
        locations.is_empty(),
        "nested constructor calls must fail closed at the receiver work bound"
    );
}

#[test]
fn deep_constructor_receiver_completion_reports_receiver_uncertainty() {
    let chain_length = 14;
    let mut expression = String::from("TObj");
    for _ in 0..chain_length {
        expression.push_str(".Create()");
    }
    expression.push_str(".Me");
    let source = format!(
        "unit DeepConstructorCompletion;\ninterface\ntype\n  TObj = class\n    constructor Create;\n    Member: Integer;\n  end;\nimplementation\nconstructor TObj.Create;\nbegin\nend;\nprocedure Caller;\nbegin\n  {expression};\nend;\nend.\n"
    );
    let source_uri = uri("DeepConstructorCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("deep constructor completion source parses");

    let completion = index
        .completion(&source_uri, position_after(&source, &expression, 0))
        .expect("deep constructor completion");
    assert!(completion.items.is_empty());
    assert!(
        completion.is_incomplete,
        "receiver work/depth exhaustion must remain incomplete"
    );
}

#[test]
fn function_result_types_keep_the_declaring_unit_context() {
    let provider = r#"unit ResultProvider;
interface
type
  TResult = class
    ProviderMember: Integer;
  end;
function MakeResult: TResult;
implementation
function MakeResult: TResult;
begin
  Result := TResult.Create;
end;
end.
"#;
    let consumer = r#"unit ResultConsumer;
interface
uses ResultProvider;
type
  TResult = class
    ConsumerMember: Integer;
  end;
implementation
procedure Caller;
begin
  ResultProvider.MakeResult().ProviderMember;
end;
end.
"#;
    let provider_uri = uri("ResultProvider");
    let consumer_uri = uri("ResultConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("result provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("result consumer parses");

    let locations = index.navigate(
        &consumer_uri,
        position_of(consumer, "ProviderMember", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &provider_uri,
        position_of(provider, "ProviderMember", 0),
    );
}

#[test]
fn function_result_annotations_keep_the_interface_type_scope() {
    let provider = r#"unit ResultAnnotationProvider;
interface
type
  TResult = class
    Name: Integer;
  end;
end.
"#;
    let consumer = r#"unit ResultAnnotationConsumer;
interface
uses ResultAnnotationProvider;
function Make: TResult;
implementation
type
  TResult = record
    Name: string;
  end;
function Make: TResult;
begin
end;
procedure Caller;
begin
  Make().Name;
end;
end.
"#;
    let provider_uri = uri("ResultAnnotationProvider");
    let consumer_uri = uri("ResultAnnotationConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("annotation provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("annotation consumer parses");
    index.bind_imports(
        &consumer_uri,
        [("ResultAnnotationProvider".to_owned(), provider_uri.clone())],
    );

    let member = index.navigate(
        &consumer_uri,
        position_of(consumer, "Name", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(member.len(), 1);
    assert_location_start(&member[0], &provider_uri, position_of(provider, "Name", 0));

    let hover = index
        .hover(&consumer_uri, position_of(consumer, "Name", 1))
        .expect("provider result member hover");
    assert!(hover_text(&hover).contains("Name: Integer"));
    assert!(!hover_text(&hover).contains("Name: string"));

    let type_definition = index.type_definitions(&consumer_uri, position_of(consumer, "Make", 2));
    assert_eq!(type_definition.len(), 1);
    assert_location_start(
        &type_definition[0],
        &provider_uri,
        position_of(provider, "TResult", 0),
    );
}

#[test]
fn implementation_only_function_result_excludes_its_own_body_type_scope() {
    let provider = r#"unit TypeSource;
interface
type
  TResult = class
    Name: Integer;
  end;
end.
"#;
    let consumer = r#"unit ImplementationOnlyResult;
interface
uses TypeSource;
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
  Make().Name;
end;
end.
"#;
    let provider_uri = uri("TypeSource");
    let consumer_uri = uri("ImplementationOnlyResult");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("type source parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("implementation-only result source parses");
    index.bind_imports(
        &consumer_uri,
        [("TypeSource".to_owned(), provider_uri.clone())],
    );

    let result_member_position = position_of(consumer, "Name", 1);
    let result_member = index.navigate(
        &consumer_uri,
        result_member_position,
        NavigationTarget::Declaration,
    );
    assert_eq!(result_member.len(), 1);
    assert_location_start(
        &result_member[0],
        &provider_uri,
        position_of(provider, "Name", 0),
    );

    let result_hover = index
        .hover(&consumer_uri, result_member_position)
        .expect("provider Result member hover");
    assert!(hover_text(&result_hover).contains("Name: Integer"));
    assert!(!hover_text(&result_hover).contains("Name: string"));

    let result_type_definition =
        index.type_definitions(&consumer_uri, position_of(consumer, "Result", 3));
    assert_eq!(result_type_definition.len(), 1);
    assert_location_start(
        &result_type_definition[0],
        &provider_uri,
        position_of(provider, "TResult", 0),
    );

    let member_position = position_of(consumer, "Name", 2);
    let member = index.navigate(
        &consumer_uri,
        member_position,
        NavigationTarget::Declaration,
    );
    assert_eq!(member.len(), 1);
    assert_location_start(&member[0], &provider_uri, position_of(provider, "Name", 0));

    let hover = index
        .hover(&consumer_uri, member_position)
        .expect("provider result member hover");
    assert!(hover_text(&hover).contains("Name: Integer"));
    assert!(!hover_text(&hover).contains("Name: string"));

    let type_definition = index.type_definitions(&consumer_uri, position_of(consumer, "Make", 1));
    assert_eq!(type_definition.len(), 1);
    assert_location_start(
        &type_definition[0],
        &provider_uri,
        position_of(provider, "TResult", 0),
    );
}

#[test]
fn nested_function_result_excludes_its_own_body_but_keeps_outer_type_scope() {
    let provider = r#"unit NestedTypeSource;
interface
type
  TResult = class
    Name: Integer;
  end;
end.
"#;
    let consumer = r#"unit NestedImplementationOnlyResult;
interface
uses NestedTypeSource;
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
  Make().Name;
end;
end.
"#;
    let provider_uri = uri("NestedTypeSource");
    let consumer_uri = uri("NestedImplementationOnlyResult");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("nested type source parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("nested implementation-only result source parses");
    index.bind_imports(
        &consumer_uri,
        [("NestedTypeSource".to_owned(), provider_uri.clone())],
    );

    let result_member_position = position_of(consumer, "Name", 2);
    let result_member = index.navigate(
        &consumer_uri,
        result_member_position,
        NavigationTarget::Declaration,
    );
    assert_eq!(result_member.len(), 1);
    assert_location_start(
        &result_member[0],
        &consumer_uri,
        position_of(consumer, "Name", 0),
    );

    let result_hover = index
        .hover(&consumer_uri, result_member_position)
        .expect("outer Result member hover");
    assert!(hover_text(&result_hover).contains("Name: string"));
    assert!(!hover_text(&result_hover).contains("Name: Boolean"));
    assert!(!hover_text(&result_hover).contains("Name: Integer"));

    let result_type_definition =
        index.type_definitions(&consumer_uri, position_of(consumer, "Result", 4));
    assert_eq!(result_type_definition.len(), 1);
    assert_location_start(
        &result_type_definition[0],
        &consumer_uri,
        position_of(consumer, "TResult", 0),
    );

    let member_position = position_of(consumer, "Name", 3);
    let member = index.navigate(
        &consumer_uri,
        member_position,
        NavigationTarget::Declaration,
    );
    assert_eq!(member.len(), 1);
    assert_location_start(&member[0], &consumer_uri, position_of(consumer, "Name", 0));

    let hover = index
        .hover(&consumer_uri, member_position)
        .expect("outer result member hover");
    assert!(hover_text(&hover).contains("Name: string"));
    assert!(!hover_text(&hover).contains("Name: Boolean"));
    assert!(!hover_text(&hover).contains("Name: Integer"));

    let type_definition = index.type_definitions(&consumer_uri, position_of(consumer, "Make", 1));
    assert_eq!(type_definition.len(), 1);
    assert_location_start(
        &type_definition[0],
        &consumer_uri,
        position_of(consumer, "TResult", 0),
    );
}

#[test]
fn type_definitions_distinguish_result_members_from_implicit_function_results() {
    let source = r#"unit MemberTypeDefinition;
interface
type
  TFieldType = class end;
  TReturnType = class end;
  TBox = record
    Result: TFieldType;
  end;
function Make: TReturnType;
implementation
function Make: TReturnType;
var
  Box: TBox;
begin
  Box.Result := nil;
  Result := nil;
end;
end.
"#;
    let source_uri = uri("ResultMemberTypeDefinition");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("result member type-definition source parses");

    let declaration =
        index.type_definitions(&source_uri, position_of(source, "Result: TFieldType", 0));
    assert_exact_type_location(&declaration, &source_uri, source, "TFieldType", 0);

    let qualified_member = index.type_definitions(&source_uri, position_of(source, "Result", 1));
    assert_exact_type_location(&qualified_member, &source_uri, source, "TFieldType", 0);

    let implicit_result = index.type_definitions(&source_uri, position_of(source, "Result", 2));
    assert_exact_type_location(&implicit_result, &source_uri, source, "TReturnType", 0);

    let member_position = position_of(source, "Result", 1);
    let definition = index.navigate(&source_uri, member_position, NavigationTarget::Declaration);
    assert_eq!(definition.len(), 1);
    assert_location_start(
        &definition[0],
        &source_uri,
        position_of(source, "Result: TFieldType", 0),
    );
    let hover = index
        .hover(&source_uri, member_position)
        .expect("Result field hover");
    assert!(hover_text(&hover).contains("Result: TFieldType"));
}

#[test]
fn implicit_result_type_definitions_out_rank_class_and_unit_bindings() {
    let source = r#"unit Precedence;
interface
type
  TFieldType = class end;
  TReturnType = class end;
  TBox = class
    Result: TFieldType;
    function Make: TReturnType;
  end;
var
  Result: TFieldType;
implementation
function TBox.Make: TReturnType;
begin
  Self.Result := nil;
  Result := nil;
end;
function FreeMake: TReturnType;
begin
  Result := nil;
end;
function LocalMake: TReturnType;
var
  Result: TFieldType;
begin
  Result := nil;
end;
end.
"#;
    let source_uri = uri("ResultPrecedence");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("result precedence source parses");

    let field_declaration =
        index.type_definitions(&source_uri, position_of(source, "Result: TFieldType", 0));
    assert_exact_type_location(&field_declaration, &source_uri, source, "TFieldType", 0);

    let unit_declaration =
        index.type_definitions(&source_uri, position_of(source, "Result: TFieldType", 1));
    assert_exact_type_location(&unit_declaration, &source_uri, source, "TFieldType", 0);

    let member = index.type_definitions(&source_uri, position_of(source, "Result", 2));
    assert_exact_type_location(&member, &source_uri, source, "TFieldType", 0);

    let method_result = index.type_definitions(&source_uri, position_of(source, "Result", 3));
    assert_exact_type_location(&method_result, &source_uri, source, "TReturnType", 0);

    let free_result = index.type_definitions(&source_uri, position_of(source, "Result", 4));
    assert_exact_type_location(&free_result, &source_uri, source, "TReturnType", 0);

    let local_result = index.type_definitions(&source_uri, position_of(source, "Result", 6));
    assert_exact_type_location(&local_result, &source_uri, source, "TFieldType", 0);
}

#[test]
fn implicit_result_type_definitions_out_rank_imported_bindings() {
    let provider = r#"unit ImportedProvider;
interface
type
  TImportedField = class end;
var
  Result: TImportedField;
implementation
end.
"#;
    let consumer = r#"unit ImportedConsumer;
interface
uses ImportedProvider;
type
  TReturnType = class end;
function Make: TReturnType;
implementation
function Make: TReturnType;
begin
  Result := nil;
end;
end.
"#;
    let provider_uri = uri("ImportedProvider");
    let consumer_uri = uri("ImportedConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("imported result provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("imported result consumer parses");
    index.bind_imports(
        &consumer_uri,
        [("ImportedProvider".to_owned(), provider_uri.clone())],
    );

    let result_position = position_of(consumer, "Result", 0);
    let definition = index.navigate(
        &consumer_uri,
        result_position,
        NavigationTarget::Declaration,
    );
    assert_eq!(definition.len(), 1);
    assert_location_start(
        &definition[0],
        &provider_uri,
        position_of(provider, "Result: TImportedField", 0),
    );

    let result = index.type_definitions(&consumer_uri, result_position);
    assert_exact_type_location(&result, &consumer_uri, consumer, "TReturnType", 0);
}

#[test]
fn inherited_methods_preserve_their_result_type_context() {
    let source = r#"unit InheritedResultContext;
interface
type
  TResult = class
    Member: Integer;
  end;
  TBase = class
    function Build: TResult;
  end;
  TChild = class(TBase)
  end;
implementation
function TBase.Build: TResult;
begin
  Result := TResult.Create;
end;
procedure Caller;
var
  Obj: TChild;
begin
  Obj.Build().Member;
end;
end.
"#;
    let source_uri = uri("InheritedResultContext");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("inherited result source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Member", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(&locations[0], &source_uri, position_of(source, "Member", 0));
}

#[test]
fn known_argument_selects_an_overload_receiver() {
    let source = r#"unit AmbiguousFunctionResult;
interface
type
  TObj = class
    Member: Integer;
  end;
function Make(Value: Integer): TObj; overload;
function Make(Value: string): TObj; overload;
implementation
function Make(Value: Integer): TObj;
begin
  Result := TObj.Create;
end;
function Make(Value: string): TObj;
begin
  Result := TObj.Create;
end;
procedure Caller;
begin
  Make(1).Member;
  Make('text').Member;
end;
end.
"#;
    let source_uri = uri("AmbiguousFunctionResult");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("ambiguous function result source parses");

    let navigation = index.navigate(
        &source_uri,
        position_of(source, "Member", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(navigation.len(), 1);
    assert_location_start(
        &navigation[0],
        &source_uri,
        position_of(source, "Member", 0),
    );

    let completion = index
        .completion(&source_uri, position_after(source, "Make(1).Me", 0))
        .expect("integer overload result completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Member"]
    );
    assert!(!completion.is_incomplete);

    let string_completion = index
        .completion(&source_uri, position_after(source, "Make('text').Me", 0))
        .expect("string overload result completion");
    assert_eq!(
        string_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Member"]
    );
    assert!(!string_completion.is_incomplete);
}

#[test]
fn type_definition_resolves_a_function_result_type() {
    let source = r#"unit FunctionResultTypeDefinition;
interface
type
  TObj = class
  end;
function MakeValue: TObj;
implementation
function MakeValue: TObj;
begin
  Result := TObj.Create;
end;
procedure Caller;
begin
  MakeValue().Create;
end;
end.
"#;
    let source_uri = uri("FunctionResultTypeDefinition");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("function result type definition source parses");

    let locations = index.type_definitions(&source_uri, position_of(source, "MakeValue", 2));
    assert_eq!(locations.len(), 1);
    assert_location_start(&locations[0], &source_uri, position_of(source, "TObj", 0));
}

#[test]
fn completion_resolves_cast_and_nested_result_receivers() {
    let source = r#"unit CompletionExpressionReceivers;
interface
type
  TLeaf = class
    Name: Integer;
  end;
  TFactory = class
    Child: TLeaf;
  end;
  TWidget = class
    Member: Integer;
    constructor Create;
  end;
function Factory: TFactory;
implementation
function Factory: TFactory;
begin
  Result := TFactory.Create;
end;
constructor TWidget.Create;
begin
end;
procedure Caller;
var
  Obj: TWidget;
begin
  TWidget.Create.Me;
  TWidget(Obj).Me;
  (Obj as TWidget).Me;
  Factory().Child.Na;
end;
end.
"#;
    let source_uri = uri("CompletionExpressionReceivers");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("completion expression receiver source parses");

    let cast_completion = index
        .completion(&source_uri, position_after(source, "TWidget(Obj).Me", 0))
        .expect("cast completion");
    assert_eq!(
        cast_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Member"]
    );

    let constructor_completion = index
        .completion(&source_uri, position_after(source, "TWidget.Create.Me", 0))
        .expect("constructor completion");
    assert_eq!(
        constructor_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Member"]
    );

    let as_completion = index
        .completion(
            &source_uri,
            position_after(source, "(Obj as TWidget).Me", 0),
        )
        .expect("as completion");
    assert_eq!(
        as_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Member"]
    );

    let chain_completion = index
        .completion(&source_uri, position_after(source, "Factory().Child.Na", 0))
        .expect("nested result completion");
    assert_eq!(
        chain_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Name"]
    );
}

#[test]
fn casts_reject_variable_rhs_receivers() {
    let source = r#"unit InvalidCastVariableReceiver;
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
  (Obj as OtherObj).Member;
end;
end.
"#;
    let source_uri = uri("InvalidCastVariableReceiver");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("invalid cast source parses");

    let navigation = index.navigate(
        &source_uri,
        position_of(source, "Member", 2),
        NavigationTarget::Declaration,
    );
    assert!(navigation.is_empty());

    let completion = index
        .completion(
            &source_uri,
            position_after(source, "(Obj as OtherObj).Me", 0),
        )
        .expect("invalid cast completion");
    assert!(completion.items.is_empty());
}

#[test]
fn qualified_cast_type_root_respects_a_shadowing_local_value() {
    let provider = r#"unit CastTypes;
interface
type
  TResult = class
    Member: Integer;
  end;
end.
"#;
    let consumer = r#"unit QualifiedCastRootShadow;
interface
uses CastTypes;
type
  TWidget = class
  end;
procedure Caller;
var
  Obj: TWidget;
  CastTypes: Integer;
begin
  (Obj as CastTypes.TResult).Member;
end;
end.
"#;
    let provider_uri = uri("CastTypes");
    let consumer_uri = uri("QualifiedCastRootShadow");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("cast type provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("qualified cast shadow source parses");
    index.bind_imports(
        &consumer_uri,
        [("CastTypes".to_owned(), provider_uri.clone())],
    );

    let member = index.navigate(
        &consumer_uri,
        position_of(consumer, "Member", 0),
        NavigationTarget::Declaration,
    );
    assert!(member.is_empty(), "a value shadow must block the unit root");

    let completion = index
        .completion(
            &consumer_uri,
            position_after(consumer, "(Obj as CastTypes.TResult).Me", 0),
        )
        .expect("qualified cast shadow completion");
    assert!(completion.items.is_empty());
}

#[test]
fn signature_help_resolves_a_function_result_method_receiver() {
    let source = r#"unit SignatureFunctionResultReceiver;
interface
type
  TObj = class
    procedure Run(Value: Integer);
  end;
function MakeValue: TObj;
implementation
function MakeValue: TObj;
begin
  Result := TObj.Create;
end;
procedure TObj.Run(Value: Integer);
begin
end;
procedure Caller;
begin
  MakeValue().Run(1);
end;
end.
"#;
    let source_uri = uri("SignatureFunctionResultReceiver");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("signature function result source parses");

    let result = index
        .signature_help(&source_uri, position_after(source, "MakeValue().Run(1", 0))
        .expect("function result signature projection");
    let help = result.expect("function result method call must resolve");
    assert_eq!(
        help.signatures
            .iter()
            .map(|signature| signature.label.as_str())
            .collect::<Vec<_>>(),
        ["procedure Run(Value: Integer);"]
    );
}

#[test]
fn hover_resolves_a_function_result_member_receiver() {
    let source = r#"unit HoverFunctionResultReceiver;
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
"#;
    let source_uri = uri("HoverFunctionResultReceiver");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("hover function result source parses");

    let hover = index
        .hover(&source_uri, position_of(source, "Member", 1))
        .expect("function result member hover");
    assert!(hover_text(&hover).contains("Member: Integer"));
}

#[test]
fn signature_help_resolves_lookup_inside_a_with_receiver_context() {
    let source = r#"unit WithSignature;
interface
type
  TObj = class
    procedure Run(Text: string);
  end;
procedure Run(Value: Integer);
implementation
procedure TObj.Run(Text: string);
begin
end;
procedure Run(Value: Integer);
begin
end;
procedure Caller;
var
  Obj: TObj;
begin
  with Obj do
    Run(1);
end;
end.
"#;
    let source_uri = uri("WithSignature");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("with signature source parses");

    let signature = index
        .signature_help(&source_uri, position_after(source, "    Run(", 0))
        .expect("with signature projection")
        .expect("with receiver lookup");
    assert_eq!(
        signature
            .signatures
            .iter()
            .map(|signature| signature.label.as_str())
            .collect::<Vec<_>>(),
        ["procedure Run(Text: string);"]
    );
}

#[test]
fn completion_lists_members_at_a_blank_position_inside_a_with_context() {
    let marked = r#"unit WithCompletion;
interface
type
  TObj = record
    Field: Integer;
  end;
procedure Run(Value: Integer);
implementation
procedure Run(Value: Integer);
begin
end;
procedure Caller;
var
  Obj: TObj;
begin
  with Obj do begin
    |
  end;
end;
end.
"#;
    let cursor_offset = marked.find('|').expect("blank with cursor");
    let source = marked.replacen('|', "", 1);
    let source_uri = uri("WithCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("with completion source parses");

    let position = text::offset_to_position(&source, cursor_offset).expect("blank with position");
    let completion = index
        .completion(&source_uri, position)
        .expect("blank with completion");
    assert!(
        completion.items.iter().any(|item| item.label == "Field"),
        "unexpected labels: {:?}",
        completion
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(!completion.is_incomplete);
}

#[test]
fn with_implicit_members_resolve_rightmost_then_earlier_receiver() {
    let source = r#"unit TypedWithLookup;
interface
type
  TLeft = record
    LeftField: Integer;
  end;
  TRight = record
    RightField: Integer;
  end;
procedure Caller;
implementation
procedure Caller;
var
  LeftValue: TLeft;
  RightValue: TRight;
begin
  with LeftValue, RightValue do begin
    RightField := LeftField;
  end;
end;
end.
"#;
    let source_uri = uri("TypedWithLookup");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("typed with source parses");

    let right_field = index.navigate(
        &source_uri,
        position_of(source, "RightField", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(right_field.len(), 1);
    assert_location_start(
        &right_field[0],
        &source_uri,
        position_of(source, "RightField", 0),
    );

    let left_field = index.navigate(
        &source_uri,
        position_of(source, "LeftField", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(left_field.len(), 1);
    assert_location_start(
        &left_field[0],
        &source_uri,
        position_of(source, "LeftField", 0),
    );
}

#[test]
fn with_members_shadow_same_named_locals_only_inside_the_body() {
    let source = r#"unit TypedWithShadowing;
interface
type
  TBox = record
    Value: Integer;
  end;
procedure Caller;
implementation
procedure Caller;
var
  Box: TBox;
  Value: Integer;
begin
  with Box do begin
    Value := 1;
  end;
  Value := 2;
end;
end.
"#;
    let source_uri = uri("TypedWithShadowing");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("shadowing with source parses");

    let member = index.navigate(
        &source_uri,
        position_of(source, "Value :=", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(member.len(), 1);
    assert_location_start(
        &member[0],
        &source_uri,
        position_of(source, "Value: Integer", 0),
    );

    let local = index.navigate(
        &source_uri,
        position_of(source, "Value :=", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(local.len(), 1);
    assert_location_start(
        &local[0],
        &source_uri,
        position_of(source, "Value: Integer", 1),
    );
}

#[test]
fn declarations_inside_with_bodies_are_not_implicit_members() {
    let source = r#"unit TypedWithDeclaration;
interface
type
  TBox = record
    Value: Integer;
  end;
procedure Caller;
implementation
procedure Caller;
var
  Box: TBox;
begin
  with Box do begin
    var Value: Integer;
    Value := 1;
  end;
end;
end.
"#;
    let source_uri = uri("TypedWithDeclaration");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("declaration with source parses");

    let declaration = index.navigate(
        &source_uri,
        position_after(source, "var ", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(declaration.len(), 1);
    assert_location_start(
        &declaration[0],
        &source_uri,
        position_after(source, "var ", 0),
    );
}

#[test]
fn with_receiver_list_uses_earlier_receiver_for_later_receiver_expression() {
    let source = r#"unit TypedWithReceiverList;
interface
type
  TInner = record
    Value: Integer;
  end;
  TOuter = record
    Inner: TInner;
  end;
procedure Caller;
implementation
procedure Caller;
var
  OuterValue: TOuter;
  Inner: TInner;
begin
  with OuterValue, Inner do
    Value := 1;
end;
end.
"#;
    let source_uri = uri("TypedWithReceiverList");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("receiver-list with source parses");

    let receiver = index.navigate(
        &source_uri,
        position_of(source, "Inner do", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(receiver.len(), 1);
    assert_location_start(
        &receiver[0],
        &source_uri,
        position_of(source, "Inner: TInner", 0),
    );

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Value :=", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "Value: Integer", 0),
    );
}

#[test]
fn with_factory_result_resolves_a_single_statement_member() {
    let source = r#"unit TypedWithFactory;
interface
type
  TObj = class
    Member: Integer;
  end;
function MakeObj: TObj;
implementation
function MakeObj: TObj;
begin
  Result := TObj.Create;
end;
procedure Caller;
begin
  with MakeObj() do
    Member := 1;
end;
end.
"#;
    let source_uri = uri("TypedWithFactory");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("factory with source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Member :=", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "Member: Integer", 0),
    );
}

#[test]
fn with_generic_receiver_preserves_specialized_nested_member_types() {
    let source = r#"unit TypedWithGeneric;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  TBox<T> = class
    Value: T;
  end;
procedure Caller;
implementation
procedure Caller;
var
  Box: TBox<TWidget>;
begin
  with Box do
    Value.WidgetMember := 1;
end;
end.
"#;
    let source_uri = uri("TypedWithGeneric");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic with source parses");

    let box_locations = index.navigate(
        &source_uri,
        position_after(source, "with ", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(box_locations.len(), 1);
    assert_location_start(
        &box_locations[0],
        &source_uri,
        position_of(source, "Box:", 0),
    );

    let value_locations = index.navigate(
        &source_uri,
        position_of(source, "Value.WidgetMember", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(value_locations.len(), 1);
    assert_location_start(
        &value_locations[0],
        &source_uri,
        position_of(source, "Value: T", 0),
    );

    let locations = index.navigate(
        &source_uri,
        position_of(source, "WidgetMember :=", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "WidgetMember: Integer", 0),
    );
}

#[test]
fn deeply_nested_with_completion_stays_bounded_and_keeps_proven_members() {
    let depth = 512;
    let mut source = String::from(
        "unit DeepTypedWith;\ninterface\ntype\n  TBox = class\n    Member: Integer;\n  end;\nimplementation\nprocedure Run;\nvar\n  Box: TBox;\nbegin\n",
    );
    for _ in 0..depth {
        source.push_str("  with Box do begin\n");
    }
    let cursor_offset = source.len() + 4;
    source.push_str("    \n");
    for _ in 0..depth {
        source.push_str("  end;\n");
    }
    source.push_str("end;\nend.\n");

    let source_uri = uri("DeepTypedWith");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("deep with source parses");
    let position = text::offset_to_position(&source, cursor_offset).expect("deep with position");
    let completion = index
        .completion(&source_uri, position)
        .expect("deep with completion remains responsive");
    assert!(completion.is_incomplete);
}

#[test]
fn with_receiver_activates_record_helpers_for_implicit_members() {
    let source = r#"unit TypedWithHelper;
interface
type
  TPoint = record
    X: Integer;
  end;
  TPointHelper = record helper for TPoint
    procedure Offset;
  end;
procedure Caller;
implementation
procedure TPointHelper.Offset;
begin
end;
procedure Caller;
var
  Point: TPoint;
begin
  with Point do
    Offset;
end;
end.
"#;
    let source_uri = uri("TypedWithHelper");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("helper with source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Offset", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(&locations[0], &source_uri, position_of(source, "Offset", 0));
}

#[test]
fn explicit_member_qualification_bypasses_an_implicit_with_receiver() {
    let source = r#"unit TypedWithQualification;
interface
type
  TObj = record
    Field: Integer;
  end;
  TOther = record
    Field: string;
  end;
procedure Caller;
implementation
procedure Caller;
var
  Obj: TObj;
  Other: TOther;
begin
  with Obj do
    Other.Field := 'value';
end;
end.
"#;
    let source_uri = uri("TypedWithQualification");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("qualified with source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Field :=", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "Field: string", 0),
    );
}

#[test]
fn dotted_with_root_remains_implicit_with_lookup() {
    let source = r#"unit TypedWithDottedRoot;
interface
type
  TLocal = record
    Value: string;
  end;
  TInner = record
    Value: Integer;
  end;
  TObj = record
    Child: TInner;
  end;
procedure Caller;
implementation
procedure Caller;
var
  Obj: TObj;
  Child: TLocal;
begin
  with Obj do
    Child.Value := 1;
end;
end.
"#;
    let source_uri = uri("TypedWithDottedRoot");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("dotted with source parses");

    let child = index.navigate(
        &source_uri,
        position_of(source, "Child.Value", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(child.len(), 1);
    assert_location_start(
        &child[0],
        &source_uri,
        position_of(source, "Child: TInner", 0),
    );

    let locations = index.navigate(
        &source_uri,
        position_of(source, "Value :=", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "Value: Integer", 0),
    );
}

#[test]
fn unresolved_with_root_keeps_the_bound_member_and_fails_closed_after_type_failure() {
    let source = r#"unit TypedWithUnknownRoot;
interface
type
  TOuter = record
    Child: TUnknown;
  end;
  TLocal = record
    Value: string;
  end;
procedure Caller;
implementation
procedure Caller;
var
  Obj: TOuter;
  Child: TLocal;
begin
  with Obj do begin
    Child.Value := 1;
    Child.Va := 1;
  end;
end;
end.
"#;
    let source_uri = uri("TypedWithUnknownRoot");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown typed-with root source parses");

    let child = index.navigate(
        &source_uri,
        position_of(source, "Child.Value", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(child.len(), 1);
    assert_location_start(
        &child[0],
        &source_uri,
        position_of(source, "Child: TUnknown", 0),
    );

    let value = index.navigate(
        &source_uri,
        position_of(source, "Value :=", 0),
        NavigationTarget::Declaration,
    );
    assert!(
        value.is_empty(),
        "an unresolved bound field must not fall back to the local Value: {value:?}"
    );
    assert!(
        index
            .hover(&source_uri, position_of(source, "Value :=", 0))
            .is_none(),
        "an unresolved bound field must not expose the local hover"
    );

    let completion = index
        .completion(&source_uri, position_after(source, "Child.Va", 0))
        .expect("unknown typed-with root completion");
    assert!(
        completion.items.iter().all(|item| item.label != "Value"),
        "an unresolved bound field must not expose local completion: {:?}",
        completion
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(completion.is_incomplete);
}

#[test]
fn unknown_with_receiver_blocks_an_unrelated_global_fallback() {
    let source = r#"unit TypedWithUnknown;
interface
const
  GlobalValue = 1;
procedure Caller;
implementation
procedure Caller;
begin
  with UnknownReceiver do
    GlobalValue := 2;
end;
end.
"#;
    let source_uri = uri("TypedWithUnknown");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown with source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "GlobalValue :=", 0),
        NavigationTarget::Declaration,
    );
    assert!(
        locations.is_empty(),
        "unknown with receiver must not select the global declaration"
    );
}

#[test]
fn unknown_with_receiver_blocks_an_unrelated_global_completion() {
    let source = r#"unit TypedWithUnknownCompletion;
interface
const
  GlobalValue = 1;
procedure Caller;
implementation
procedure Caller;
begin
  with UnknownReceiver do
    Glo := 2;
end;
end.
"#;
    let source_uri = uri("TypedWithUnknownCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown completion source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "Glo", 1))
        .expect("unknown with completion");
    assert!(
        completion
            .items
            .iter()
            .all(|item| item.label != "GlobalValue"),
        "unknown with receiver leaked a global: {:?}",
        completion
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(completion.is_incomplete);
}

#[test]
fn proven_inner_with_receiver_precedes_an_unknown_outer_context() {
    let source = r#"unit TypedWithUnknownOuter;
interface
type
  TInner = class
    constructor Create;
    InnerValue: Integer;
  end;
procedure Caller;
implementation
constructor TInner.Create;
begin
end;
procedure Caller;
begin
  with UnknownOuter do
    with TypedWithUnknownOuter.TInner.Create do
      InnerValue := 1;
end;
end.
"#;
    let source_uri = uri("TypedWithUnknownOuter");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown outer with source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "InnerValue :=", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "InnerValue: Integer", 0),
    );
}

#[test]
fn nested_with_uses_inner_then_outer_and_does_not_leak_after_body() {
    let source = r#"unit TypedWithNesting;
interface
type
  TInner = record
    InnerValue: Integer;
  end;
  TOuter = record
    OuterValue: Integer;
    Inner: TInner;
  end;
const
  OuterValue = 0;
procedure Caller;
implementation
procedure Caller;
var
  Outer: TOuter;
begin
  with Outer do begin
    with Inner do begin
      InnerValue := 1;
      OuterValue := 2;
    end;
  end;
  OuterValue := 3;
end;
end.
"#;
    let source_uri = uri("TypedWithNesting");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("nested with source parses");

    let inner = index.navigate(
        &source_uri,
        position_of(source, "InnerValue :=", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(inner.len(), 1);
    assert_location_start(
        &inner[0],
        &source_uri,
        position_of(source, "InnerValue: Integer", 0),
    );

    let outer_inside = index.navigate(
        &source_uri,
        position_of(source, "OuterValue :=", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(outer_inside.len(), 1);
    assert_location_start(
        &outer_inside[0],
        &source_uri,
        position_of(source, "OuterValue: Integer", 0),
    );

    let outer_after = index.navigate(
        &source_uri,
        position_of(source, "OuterValue :=", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(outer_after.len(), 1);
    assert_location_start(
        &outer_after[0],
        &source_uri,
        position_of(source, "OuterValue = 0", 0),
    );
}

#[test]
fn known_with_receivers_fall_back_to_a_local_for_an_unmatched_completion() {
    let source = r#"unit TypedWithCompletionFallback;
interface
type
  TLeft = record
    LeftField: Integer;
  end;
  TRight = record
    RightField: Integer;
  end;
procedure Caller;
implementation
procedure Caller;
var
  LeftValue: TLeft;
  RightValue: TRight;
  LocalValue: Integer;
begin
  with LeftValue, RightValue do
    Loc := 1;
end;
end.
"#;
    let source_uri = uri("TypedWithCompletionFallback");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("known completion fallback source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "Loc", 1))
        .expect("known with completion fallback");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["LocalValue"]
    );
    assert!(!completion.is_incomplete);
}

#[test]
fn an_unknown_comma_receiver_does_not_hide_a_later_qualified_receiver() {
    let source = r#"unit TypedWithUnknownReceiverList;
interface
type
  TInner = class
    constructor Create;
    Value: Integer;
  end;
implementation
constructor TInner.Create;
begin
end;
procedure Caller;
begin
  with UnknownReceiver, TypedWithUnknownReceiverList.TInner.Create do
    Value := 1;
  with UnknownReceiver do
    with TypedWithUnknownReceiverList.TInner.Create do
      Value := 2;
end;
end.
"#;
    let source_uri = uri("TypedWithUnknownReceiverList");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown receiver-list source parses");

    let qualified_receiver = index.navigate(
        &source_uri,
        position_of(source, "TInner.Create", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(qualified_receiver.len(), 1);
    assert_location_start(
        &qualified_receiver[0],
        &source_uri,
        position_of(source, "TInner = class", 0),
    );

    let comma_value = index.navigate(
        &source_uri,
        position_of(source, "Value :=", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(comma_value.len(), 1);
    assert_location_start(
        &comma_value[0],
        &source_uri,
        position_of(source, "Value: Integer", 0),
    );

    let nested_value = index.navigate(
        &source_uri,
        position_of(source, "Value :=", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(nested_value.len(), 1);
    assert_location_start(
        &nested_value[0],
        &source_uri,
        position_of(source, "Value: Integer", 0),
    );
}

#[test]
fn unknown_middle_with_slots_preserve_overlay_barriers_for_comma_and_nested_forms() {
    let source = r#"unit TypedWithUnknownMiddleOverlay;
interface
type
  TInner = class
    constructor Create;
    Value: Integer;
  end;
  TOuter = record
    Inner: TInner;
  end;
const
  Value = 0;
procedure CommaCaller;
procedure NestedCaller;
procedure QualifiedCaller;
implementation
constructor TInner.Create;
begin
end;
procedure CommaCaller;
var
  Obj: TOuter;
begin
  with Obj, UnknownReceiver, Inner do begin
    Value := 1;
    Va := 1;
  end;
end;
procedure NestedCaller;
var
  Obj: TOuter;
begin
  with Obj do
    with UnknownReceiver do
      with Inner do begin
        Value := 2;
        Va := 2;
      end;
end;
procedure QualifiedCaller;
var
  Obj: TOuter;
begin
  with Obj, UnknownReceiver, TypedWithUnknownMiddleOverlay.TInner.Create do begin
    Value := 3;
    Va := 3;
  end;
end;
end.
"#;
    let source_uri = uri("TypedWithUnknownMiddleOverlay");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown middle with source parses");

    for occurrence in 0..2 {
        let receiver = index.navigate(
            &source_uri,
            position_of(source, "Inner do", occurrence),
            NavigationTarget::Declaration,
        );
        assert!(
            receiver.is_empty(),
            "an unknown middle receiver must keep an unqualified later slot unresolved: {receiver:?}"
        );
        assert!(
            index
                .type_definitions(&source_uri, position_of(source, "Inner do", occurrence))
                .is_empty(),
            "an unknown middle receiver must not invent a type for the later slot"
        );
    }

    for occurrence in 0..2 {
        let completion_position = position_of(source, "Va :=", occurrence);
        let completion_position = Position::new(
            completion_position.line,
            completion_position.character + "Va".encode_utf16().count() as u32,
        );
        let value = index.navigate(
            &source_uri,
            position_of(source, "Value :=", occurrence),
            NavigationTarget::Declaration,
        );
        assert!(
            value.is_empty(),
            "an unknown middle receiver must block body fallback for comma/nested form: {value:?}"
        );
        assert!(
            index
                .hover(&source_uri, position_of(source, "Value :=", occurrence))
                .is_none(),
            "an unknown middle receiver must block body hover fallback"
        );
        let completion = index
            .completion(&source_uri, completion_position)
            .expect("unknown middle completion");
        assert!(
            completion.items.iter().all(|item| item.label != "Value"),
            "an unknown middle receiver must block body completion fallback: {:?}",
            completion
                .items
                .iter()
                .map(|item| &item.label)
                .collect::<Vec<_>>()
        );
        assert!(completion.is_incomplete);
    }

    let qualified_receiver = index.navigate(
        &source_uri,
        position_of(source, "TInner.Create do", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(qualified_receiver.len(), 1);
    assert_location_start(
        &qualified_receiver[0],
        &source_uri,
        position_of(source, "TInner = class", 0),
    );
    let qualified_type =
        index.type_definitions(&source_uri, position_of(source, "TInner.Create do", 0));
    assert_exact_type_location(&qualified_type, &source_uri, source, "TInner", 0);

    let qualified_value = index.navigate(
        &source_uri,
        position_of(source, "Value :=", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(qualified_value.len(), 1);
    assert_location_start(
        &qualified_value[0],
        &source_uri,
        position_of(source, "Value: Integer", 0),
    );
    let qualified_hover = index
        .hover(&source_uri, position_of(source, "Value :=", 2))
        .expect("independently qualified later receiver hover");
    assert!(hover_text(&qualified_hover).contains("Value: Integer"));
    let qualified_completion_position = position_of(source, "Va :=", 2);
    let qualified_completion_position = Position::new(
        qualified_completion_position.line,
        qualified_completion_position.character + "Va".encode_utf16().count() as u32,
    );
    let qualified_completion = index
        .completion(&source_uri, qualified_completion_position)
        .expect("independently qualified later receiver completion");
    assert!(
        qualified_completion
            .items
            .iter()
            .any(|item| item.label == "Value"),
        "an independently qualified later receiver must retain its member: {:?}",
        qualified_completion
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
}

#[test]
fn nested_comma_receiver_prefix_precedes_outer_context_across_assistance_endpoints() {
    let source = r#"unit TypedWithNestedCommaPrecedence;
interface
type
  TFirst = class
    Value: Integer;
    FirstOnly: Integer;
    procedure Run(A: Integer);
  end;
  TWrong = class
    Value: string;
    WrongOnly: string;
    procedure Run(A: string);
  end;
  TLeft = class
    Inner: TFirst;
  end;
  TOuter = class
    Left: TLeft;
    Inner: TWrong;
  end;
procedure Caller;
implementation
procedure TFirst.Run(A: Integer);
begin
end;
procedure TWrong.Run(A: string);
begin
end;
procedure Caller;
var
  OuterValue: TOuter;
begin
  with OuterValue do
    with Left, Inner do begin
      Value := 1;
      Fir := 1;
      Run(1);
    end;
end;
end.
"#;
    let source_uri = uri("TypedWithNestedCommaPrecedence");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("nested comma precedence source parses");

    let receiver = index.navigate(
        &source_uri,
        position_of(source, "Inner do", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(receiver.len(), 1);
    assert_location_start(
        &receiver[0],
        &source_uri,
        position_of(source, "Inner: TFirst", 0),
    );

    let value = index.navigate(
        &source_uri,
        position_of(source, "Value :=", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(value.len(), 1);
    assert_location_start(
        &value[0],
        &source_uri,
        position_of(source, "Value: Integer", 0),
    );

    let hover = index
        .hover(&source_uri, position_of(source, "Value :=", 0))
        .expect("nested comma value hover");
    assert!(hover_text(&hover).contains("Value: Integer"));

    let type_definition = index.type_definitions(&source_uri, position_of(source, "Inner do", 0));
    assert_exact_type_location(&type_definition, &source_uri, source, "TFirst", 0);

    let completion = index
        .completion(&source_uri, position_after(source, "      Fir", 0))
        .expect("nested comma member completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["FirstOnly"]
    );
    assert!(!completion.is_incomplete);

    let signature = index
        .signature_help(&source_uri, position_after(source, "      Run(", 0))
        .expect("nested comma signature help")
        .expect("nested comma method signature");
    assert_eq!(
        signature
            .signatures
            .iter()
            .map(|signature| signature.label.as_str())
            .collect::<Vec<_>>(),
        ["procedure Run(A: Integer);"]
    );
}

#[test]
fn nested_comma_receiver_rename_fails_closed_without_partial_edits() {
    let source = r#"unit TypedWithNestedCommaRename;
interface
type
  TFirst = class
    Value: Integer;
  end;
  TLeft = class
    Inner: TFirst;
  end;
  TOuter = class
    Left: TLeft;
    Inner: TFirst;
  end;
implementation
procedure Caller;
var
  OuterValue: TOuter;
begin
  with OuterValue do
    with Left, Inner do
      Value := 1;
end;
end.
"#;
    let source_uri = uri("TypedWithNestedCommaRename");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("nested comma rename source parses");

    assert!(
        index
            .rename_edits(
                &source_uri,
                position_of(source, "Inner: TFirst", 0),
                "RenamedInner",
            )
            .is_err(),
        "a nested comma receiver rename must fail closed"
    );
}

#[test]
fn completion_keeps_known_receiver_prefix_slots_ordered_and_blocks_unknown_middle_slots() {
    let source = r#"unit TypedWithOrderedReceiverCompletion;
interface
type
  TTarget = record
    Value: Integer;
  end;
  TLeft = record
    A: Integer;
  end;
  TRight = record
    B: Integer;
  end;
  TRightMember = record
    B: Integer;
    LocalReceiver: TTarget;
  end;
procedure KnownReceivers;
procedure RightmostMember;
procedure UnknownMiddle;
implementation
procedure KnownReceivers;
var
  L: TLeft;
  R: TRight;
  LocalReceiver: TTarget;
begin
  with L, R, LocalReceiver do begin
    Value := 1;
  end;
end;
procedure RightmostMember;
var
  L: TLeft;
  R: TRightMember;
  LocalReceiver: TTarget;
begin
  with L, R, LocalReceiver do begin
    Value := 2;
  end;
end;
procedure UnknownMiddle;
var
  L: TLeft;
  LocalReceiver: TTarget;
begin
  with L, UnknownReceiver, LocalReceiver do begin
    Value := 3;
  end;
end;
end.
"#;
    let source_uri = uri("TypedWithOrderedReceiverCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("ordered receiver completion source parses");

    let known_body = index.navigate(
        &source_uri,
        position_of(source, "Value :=", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(known_body.len(), 1);
    assert_location_start(
        &known_body[0],
        &source_uri,
        position_of(source, "Value: Integer", 0),
    );

    let known_none = index
        .completion(&source_uri, position_after(source, "with L, R, Loc", 0))
        .expect("known receiver prefix completion");
    let known_none_item = known_none
        .items
        .iter()
        .find(|item| item.label == "LocalReceiver")
        .expect("a known three-receiver prefix must retain the local candidate");
    assert_eq!(known_none_item.kind, Some(CompletionItemKind::VARIABLE));
    assert!(!known_none.is_incomplete);

    let rightmost = index
        .completion(&source_uri, position_after(source, "with L, R, Loc", 1))
        .expect("rightmost receiver member completion");
    let rightmost_item = rightmost
        .items
        .iter()
        .find(|item| item.label == "LocalReceiver")
        .expect("the rightmost prefix member must be offered");
    assert_eq!(rightmost_item.kind, Some(CompletionItemKind::FIELD));
    assert!(!rightmost.is_incomplete);

    let unknown_middle = index
        .completion(
            &source_uri,
            position_after(source, "with L, UnknownReceiver, Loc", 0),
        )
        .expect("unknown middle receiver completion");
    assert!(
        unknown_middle
            .items
            .iter()
            .all(|item| item.label != "LocalReceiver"),
        "an unknown middle receiver must block the local third receiver: {:?}",
        unknown_middle
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(unknown_middle.is_incomplete);
}

#[test]
fn completion_resolves_nested_comma_receiver_prefixes_and_unknown_barriers() {
    let source = r#"unit TypedWithReceiverCompletion;
interface
type
  TTarget = record
    Value: Integer;
  end;
  TLeft = record
    Inner: TTarget;
  end;
  TOuter = record
    Left: TLeft;
  end;
const
  LeftGlobal = 1;
  InnerGlobal = 2;
procedure Caller;
implementation
procedure Caller;
var
  OuterValue: TOuter;
begin
  with OuterValue do
    with Left, Inn do begin
      Value := 1;
    end;
  with UnknownOuter do
    with Left, Inn do begin
      Value := 2;
    end;
end;
end.
"#;
    let source_uri = uri("TypedWithReceiverCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("receiver completion source parses");

    let first_receiver = index
        .completion(&source_uri, position_after(source, "with Left", 0))
        .expect("first nested receiver completion");
    assert!(
        first_receiver.items.iter().any(|item| item.label == "Left"),
        "the outer receiver must complete the first nested receiver: {:?}",
        first_receiver
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(!first_receiver.is_incomplete);

    let second_receiver = index
        .completion(&source_uri, position_after(source, "with Left, Inn", 0))
        .expect("second nested receiver completion");
    assert!(
        second_receiver
            .items
            .iter()
            .any(|item| item.label == "Inner"),
        "the earlier nested receiver must complete Outer.Left.Inner: {:?}",
        second_receiver
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(!second_receiver.is_incomplete);

    let unknown_first = index
        .completion(&source_uri, position_after(source, "with Left", 1))
        .expect("unknown outer first receiver completion");
    assert!(
        unknown_first
            .items
            .iter()
            .all(|item| item.label != "Left" && item.label != "LeftGlobal"),
        "an unknown outer receiver must block lower-priority first-receiver globals: {:?}",
        unknown_first
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(unknown_first.is_incomplete);

    let unknown_second = index
        .completion(&source_uri, position_after(source, "with Left, Inn", 1))
        .expect("unknown outer second receiver completion");
    assert!(
        unknown_second
            .items
            .iter()
            .all(|item| item.label != "Inner" && item.label != "InnerGlobal"),
        "an unknown outer receiver must block lower-priority second-receiver globals: {:?}",
        unknown_second
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(unknown_second.is_incomplete);
}

#[test]
fn with_generic_receiver_substitution_reaches_call_result_assistance() {
    let source = r#"unit TypedWithGenericAssistance;
interface
type
  TWidget = class
    Member: Integer;
  end;
  TBox<T> = class
    Value: T;
    function GetValue: T;
    procedure Put(Item: T);
  end;
procedure Caller;
implementation
function TBox<T>.GetValue: T;
begin
  Result := Value;
end;
procedure TBox<T>.Put(Item: T);
begin
end;
procedure Caller;
var
  Box: TBox<TWidget>;
  Widget: TWidget;
begin
  with Box do begin
    Value.Member;
    GetValue().Member;
    Put(Widget);
  end;
end;
end.
"#;
    let source_uri = uri("TypedWithGenericAssistance");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic with assistance source parses");

    let value_type = index.type_definitions(&source_uri, position_of(source, "Value.Member", 0));
    assert_exact_type_location(&value_type, &source_uri, source, "TWidget", 0);

    let call_result_type = index.type_definitions(&source_uri, position_of(source, "GetValue", 2));
    assert_exact_type_location(&call_result_type, &source_uri, source, "TWidget", 0);

    let call_member = index.navigate(
        &source_uri,
        position_of(source, "Member", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(call_member.len(), 1);
    assert_location_start(
        &call_member[0],
        &source_uri,
        position_of(source, "Member: Integer", 0),
    );

    let hover = index
        .hover(&source_uri, position_of(source, "GetValue", 2))
        .expect("generic with call-result hover");
    assert!(hover_text(&hover).contains("function GetValue: TWidget;"));

    let signature = index
        .signature_help(&source_uri, position_after(source, "Put(", 2))
        .expect("generic with signature help")
        .expect("generic with Put signature");
    assert_eq!(signature.signatures.len(), 1);
    assert_eq!(
        signature.signatures[0].label,
        "procedure Put(Item: TWidget);"
    );
}

#[test]
fn with_binding_is_shared_by_hover_completion_signature_and_type_definition() {
    let source = r#"unit TypedWithAssistance;
interface
type
  TChild = record
    ChildValue: Integer;
  end;
  TObj = record
    Field: TChild;
    procedure Run(Value: Integer);
  end;
procedure Caller;
implementation
procedure TObj.Run(Value: Integer);
begin
end;
procedure Caller;
var
  Obj: TObj;
begin
  with Obj do begin
    Field.ChildValue := 1;
    Run(1);
    Fi;
  end;
end;
end.
"#;
    let source_uri = uri("TypedWithAssistance");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("with assistance source parses");

    let hover = index
        .hover(&source_uri, position_of(source, "Field", 1))
        .expect("with member hover");
    assert!(hover_text(&hover).contains("Field: TChild"));

    let completion = index
        .completion(&source_uri, position_after(source, "  Fi", 0))
        .expect("with member completion");
    assert!(
        completion.items.iter().any(|item| item.label == "Field"),
        "unexpected labels: {:?}",
        completion
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );

    let signature = index
        .signature_help(&source_uri, position_after(source, "    Run(", 0))
        .expect("with signature help")
        .expect("with member signature");
    assert_eq!(
        signature
            .signatures
            .iter()
            .map(|signature| signature.label.as_str())
            .collect::<Vec<_>>(),
        ["procedure Run(Value: Integer);"]
    );

    let type_definition = index.type_definitions(&source_uri, position_of(source, "Field", 1));
    assert_eq!(type_definition.len(), 1);
    assert_location_start(
        &type_definition[0],
        &source_uri,
        position_of(source, "TChild", 0),
    );
}

#[test]
fn completion_rejects_global_fallback_inside_an_unknown_inheritance_ancestor() {
    let source = r#"unit UnknownInheritanceCompletion;
interface
type
  TObj = class(TUnknown)
    procedure Caller;
  end;
const
  GlobalName = 1;
implementation
procedure TObj.Caller;
begin
  Glo;
end;
end.
"#;
    let source_uri = uri("UnknownInheritanceCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown inheritance source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "  Glo", 1))
        .expect("unknown inheritance completion");
    assert!(
        completion.items.is_empty(),
        "unexpected labels: {:?}",
        completion
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(completion.is_incomplete);
}

#[test]
fn completion_rejects_blank_positions_inside_unknown_inheritance_ancestors() {
    let marked = r#"unit BlankUnknownInheritanceCompletion;
interface
type
  TObj = class(TUnknown)
    procedure Caller;
  end;
const
  GlobalName = 1;
implementation
procedure TObj.Caller;
begin
  |
end;
end.
"#;
    let cursor_offset = marked.find('|').expect("blank inheritance cursor");
    let source = marked.replacen('|', "", 1);
    let source_uri = uri("BlankUnknownInheritanceCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("blank unknown inheritance source parses");

    let position = text::offset_to_position(&source, cursor_offset)
        .expect("blank unknown inheritance position");
    let completion = index
        .completion(&source_uri, position)
        .expect("blank unknown inheritance completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Caller"],
        "blank unknown inheritance positions must not enumerate globals"
    );
    assert!(
        completion
            .items
            .iter()
            .all(|item| item.label != "GlobalName" && item.label != "TObj"),
        "blank unknown inheritance positions must not enumerate globals: {:?}",
        completion
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(completion.is_incomplete);
}

#[test]
fn completion_keeps_a_known_local_against_an_unknown_inheritance_global() {
    let marked = r#"unit LocalUnknownInheritanceCompletion;
interface
type
  TObj = class(TUnknown)
    procedure Caller;
  end;
const
  LongGlobal = 1;
implementation
procedure TObj.Caller;
var
  LocalName: Integer;
begin
  Lo|;
end;
end.
"#;
    let cursor_offset = marked.find('|').expect("local inheritance cursor");
    let source = marked.replacen('|', "", 1);
    let source_uri = uri("LocalUnknownInheritanceCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("local unknown inheritance source parses");

    let position = text::offset_to_position(&source, cursor_offset)
        .expect("local unknown inheritance position");
    let completion = index
        .completion(&source_uri, position)
        .expect("local unknown inheritance completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["LocalName"]
    );
    assert!(completion.is_incomplete);
}

#[test]
fn completion_keeps_a_same_class_member_against_an_unknown_inheritance_global() {
    let marked = r#"unit MemberUnknownInheritanceCompletion;
interface
type
  TObj = class(TUnknown)
    Member: Integer;
    procedure Caller;
  end;
const
  MemoryGlobal = 1;
implementation
procedure TObj.Caller;
begin
  Mem|;
end;
end.
"#;
    let cursor_offset = marked.find('|').expect("member inheritance cursor");
    let source = marked.replacen('|', "", 1);
    let source_uri = uri("MemberUnknownInheritanceCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("member unknown inheritance source parses");

    let position = text::offset_to_position(&source, cursor_offset)
        .expect("member unknown inheritance position");
    let completion = index
        .completion(&source_uri, position)
        .expect("member unknown inheritance completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Member"]
    );
    assert!(completion.is_incomplete);
}

#[test]
fn completion_rejects_unknown_inheritance_context_after_the_context_budget() {
    let mut source = String::from(
        "unit UnknownInheritanceContextLimit;\ninterface\ntype\n  TObj = class(TUnknown)\n    procedure Caller;\n  end;\nconst\n  GlobalName = 1;\nimplementation\nprocedure TObj.Caller;\nbegin\n  Glo;\n",
    );
    for _ in 0..120_000 {
        source.push_str("  WriteLn(1);\n");
    }
    source.push_str("end;\nend.\n");
    let source_uri = uri("UnknownInheritanceContextLimit");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("unknown inheritance context limit source parses");

    let position = position_after(&source, "  Glo", 1);
    let completion = index
        .completion(&source_uri, position)
        .expect("unknown inheritance context must use cached bounded metadata");
    assert!(
        completion.items.is_empty(),
        "unexpected large-context labels: {:?}",
        completion
            .items
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(completion.is_incomplete);
}

#[test]
fn unresolved_owner_cache_miss_stays_unknown_across_assistance_endpoints() {
    let source = r#"unit UnknownOwner;
interface
const GlobalName = 1;
procedure Run(Value: Integer);
implementation
procedure TUnknown.Caller;
begin
  Glo;
  GlobalName;
  Run(1);
end;
end.
"#;
    let source_uri = uri("UnknownOwner");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unresolved owner source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "  Glo", 0))
        .expect("unresolved owner completion");
    assert!(
        completion.items.is_empty(),
        "unexpected completion: {completion:?}"
    );
    assert!(completion.is_incomplete);

    assert!(
        index
            .hover(&source_uri, position_of(source, "GlobalName", 1))
            .is_none(),
        "unresolved owner must not expose a global hover"
    );

    assert!(
        index
            .signature_help(&source_uri, position_after(source, "  Run(", 0))
            .expect("unresolved owner signature help")
            .is_none(),
        "unresolved owner must not expose a global signature"
    );
}

#[test]
fn known_non_class_owner_remains_eligible_for_global_assistance() {
    let source = r#"unit RecordOwner;
interface
type
  TRecord = record
    Value: Integer;
  end;
const GlobalName = 1;
procedure Run(Value: Integer);
implementation
procedure TRecord.Caller;
begin
  Glo;
  GlobalName;
  Run(1);
end;
end.
"#;
    let source_uri = uri("RecordOwner");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("record owner source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "  Glo", 0))
        .expect("record owner completion");
    assert!(
        completion
            .items
            .iter()
            .any(|item| item.label == "GlobalName"),
        "known non-class owner must retain global completion: {completion:?}"
    );

    let hover = index
        .hover(&source_uri, position_of(source, "GlobalName", 1))
        .expect("record owner global hover");
    assert!(
        hover_text(&hover).contains("GlobalName"),
        "known non-class owner must retain global hover"
    );

    assert!(
        index
            .signature_help(&source_uri, position_after(source, "  Run(", 0))
            .expect("record owner signature help")
            .is_some(),
        "known non-class owner must retain global signature help"
    );
}

#[test]
fn signature_help_uses_utf16_label_offsets_for_grouped_parameters() {
    let source = r#"unit GroupedLabels;
interface
procedure Run(A, B: Integer; C: string; D: Integer);
implementation
procedure Run(A, B: Integer; C: string; D: Integer);
begin
end;
procedure Caller;
begin
  Run(1, 2);
end;
end.
"#;
    let source_uri = uri("GroupedLabels");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("grouped labels source parses");

    let help = index
        .signature_help(&source_uri, position_after(source, "  Run(", 0))
        .expect("grouped labels projection")
        .expect("grouped labels call");
    assert_eq!(help.signatures.len(), 1);
    assert_eq!(
        help.signatures[0]
            .parameters
            .as_ref()
            .expect("grouped labels parameters")
            .iter()
            .map(|parameter| &parameter.label)
            .collect::<Vec<_>>(),
        [
            &lsp_types::ParameterLabel::LabelOffsets([14, 15]),
            &lsp_types::ParameterLabel::LabelOffsets([17, 18]),
            &lsp_types::ParameterLabel::LabelOffsets([29, 30]),
            &lsp_types::ParameterLabel::LabelOffsets([40, 41]),
        ]
    );
}

#[test]
fn signature_help_does_not_count_nested_callable_formals_as_outer_parameters() {
    let source = r#"unit NestedFormalParameters;
interface
procedure Run(Callback: procedure(X: Integer); Last: Integer);
implementation
procedure Run(Callback: procedure(X: Integer); Last: Integer);
begin
end;
procedure Caller;
begin
  Run(nil, 1);
end;
end.
"#;
    let source_uri = uri("NestedFormalParameters");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("nested formal source parses");

    let help = index
        .signature_help(&source_uri, position_after(source, "  Run(nil, ", 0))
        .expect("nested formal projection")
        .expect("nested formal call");
    assert_eq!(help.active_parameter, Some(1));
    assert_eq!(
        help.signatures[0]
            .parameters
            .as_ref()
            .expect("outer formal parameters")
            .len(),
        2
    );
}

#[test]
fn signature_help_rejects_an_uncertain_overload_instead_of_dropping_it() {
    let source = r#"unit UncertainOverload;
interface
procedure Run(A: Integer); overload;
{$IFDEF MAYBE}
procedure Run(B: string); overload;
{$ENDIF}
implementation
procedure Run(A: Integer);
begin
end;
procedure Caller;
begin
  Run(1);
end;
end.
"#;
    let source_uri = uri("UncertainOverload");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("uncertain overload source parses");

    assert!(
        index
            .signature_help(&source_uri, position_after(source, "  Run(", 0))
            .expect("uncertain overload projection")
            .is_none(),
        "uncertain overloads must not be silently removed"
    );
}

#[test]
fn signature_help_excludes_known_inactive_argument_bytes_from_active_parameter() {
    let marked = r#"unit InactiveArgument;
interface
{$UNDEF OFF}
procedure Run(A, B: Integer);
implementation
procedure Run(A, B: Integer);
begin
end;
procedure Caller;
begin
  Run(1 {$IFDEF OFF}, 2{$ENDIF}, |3);
end;
end.
"#;
    let cursor_offset = marked.find('|').expect("inactive argument cursor");
    let source = marked.replacen('|', "", 1);
    let source_uri = uri("InactiveArgument");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("inactive argument source parses");

    let position =
        text::offset_to_position(&source, cursor_offset).expect("inactive argument position");
    let help = index
        .signature_help(&source_uri, position)
        .expect("inactive argument projection")
        .expect("inactive argument call");
    assert_eq!(help.active_parameter, Some(1));
}

#[test]
fn signature_help_deduplicates_before_limiting_imported_overloads() {
    let mut index = NavigationIndex::new();
    for (unit_name, start, count) in [("OverloadA", 0, 64), ("OverloadB", 64, 16)] {
        let mut source = format!("unit {unit_name};\ninterface\n");
        for number in start..start + count {
            writeln!(&mut source, "procedure Run(X: T{number}); overload;")
                .expect("write overload declaration");
        }
        source.push_str("implementation\n");
        for number in start..start + count {
            writeln!(&mut source, "procedure Run(X: T{number}); begin end;")
                .expect("write overload definition");
        }
        source.push_str("end.\n");
        index
            .update(uri(unit_name), source)
            .expect("overload provider parses");
    }
    let consumer = r#"unit OverloadConsumer;
interface
uses OverloadA, OverloadB;
implementation
procedure Caller;
begin
  Run(1);
end;
end.
"#;
    let consumer_uri = uri("OverloadConsumer");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("overload consumer parses");

    let help = index
        .signature_help(&consumer_uri, position_after(consumer, "  Run(", 0))
        .expect("overload projection")
        .expect("overload call");
    assert_eq!(help.signatures.len(), 80);
}

#[test]
fn signature_help_after_a_closing_parenthesis_returns_null() {
    let source = r#"unit ClosedCall;
interface
procedure Run(Value: Integer);
implementation
procedure Run(Value: Integer);
begin
end;
procedure Caller;
begin
  Run(1)|;
end;
end.
"#;
    let cursor_offset = source.find('|').expect("closed-call cursor");
    let source = source.replacen('|', "", 1);
    let source_uri = uri("ClosedCall");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("closed-call source parses");

    let position = text::offset_to_position(&source, cursor_offset).expect("closed-call position");
    assert!(
        index
            .signature_help(&source_uri, position)
            .expect("closed-call projection")
            .is_none()
    );
}

#[test]
fn signature_help_keeps_an_incomplete_call_at_a_missing_closing_delimiter() {
    let marked = r#"unit MissingClosingCall;
interface
procedure Run(Value: Integer);
implementation
procedure Run(Value: Integer);
begin
end;
procedure Caller;
begin
  Run(1|;
end;
end.
"#;
    let cursor_offset = marked.find('|').expect("missing-closing cursor");
    let source = marked.replacen('|', "", 1);
    let source_uri = uri("MissingClosingCall");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("missing-closing source parses");

    let position =
        text::offset_to_position(&source, cursor_offset).expect("missing-closing position");
    let help = index
        .signature_help(&source_uri, position)
        .expect("missing-closing signature projection")
        .expect("an unfinished call must retain signature help");
    assert_eq!(help.signatures.len(), 1);
    assert_eq!(help.active_parameter, Some(0));
    assert_eq!(help.signatures[0].label, "procedure Run(Value: Integer);");
}

#[test]
fn signature_help_selects_an_inner_call_before_a_large_enclosing_tail() {
    let mut source = String::from(
        "unit InnerBeforeLargeOuter;\ninterface\nprocedure Outer(A, B: Integer);\nfunction Inner(A: Integer): Integer;\nimplementation\nprocedure Outer(A, B: Integer);\nbegin\nend;\nfunction Inner(A: Integer): Integer;\nbegin\n  Result := A;\nend;\nprocedure Caller;\nbegin\n  Outer(Inner(",
    );
    let cursor_offset = source.len();
    source.push_str("1), {");
    source.push_str(&"x".repeat(65_536));
    source.push_str("} 2);\nend;\nend.\n");
    let source_uri = uri("InnerBeforeLargeOuter");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("inner-call source parses");

    let position = text::offset_to_position(&source, cursor_offset).expect("inner-call position");
    let help = index
        .signature_help(&source_uri, position)
        .expect("inner call must not inherit the outer tail scan limit")
        .expect("inner call signature help");
    assert_eq!(help.signatures.len(), 1);
    assert_eq!(help.active_parameter, Some(0));
    assert_eq!(
        help.signatures[0].label,
        "function Inner(A: Integer): Integer;"
    );
}

#[test]
fn signature_help_rejects_tree_traversal_over_the_work_limit() {
    let mut source = String::from(
        "unit SignatureTraversalLimit;\ninterface\nprocedure Run(Value: Integer);\nimplementation\nprocedure Run(Value: Integer);\nbegin\nend;\nprocedure Caller;\nbegin\n",
    );
    for _ in 0..120_000 {
        source.push_str("  Run(1);\n");
    }
    source.push_str("  Run(1);\nend;\nend.\n");
    let source_uri = uri("SignatureTraversalLimit");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("signature traversal source parses");

    let call = source.rfind("  Run(1);").expect("last call");
    let position = text::offset_to_position(&source, call + "Run(".len())
        .expect("signature traversal position");
    let error = index
        .signature_help(&source_uri, position)
        .expect_err("signature tree traversal must stop at its work limit");
    assert!(error.contains("node traversal"), "{error}");
}

#[test]
fn signature_help_rejects_an_oversized_rendered_parameter_response() {
    let mut source = String::from("unit SignatureParameterLimit;\ninterface\nprocedure Run(");
    for index in 0..5_000 {
        if index != 0 {
            source.push_str(", ");
        }
        write!(&mut source, "Parameter{index}").expect("write parameter name");
    }
    source
        .push_str(": Integer);\nimplementation\nprocedure Caller;\nbegin\n  Run(1);\nend;\nend.\n");
    let source_uri = uri("SignatureParameterLimit");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("signature parameter source parses");

    let position = position_after(&source, "  Run(", 0);
    let error = index
        .signature_help(&source_uri, position)
        .expect_err("signature response metadata must have a hard budget");
    assert!(
        error.contains("parameter") || error.contains("response"),
        "{error}"
    );
}

#[test]
fn signature_help_rejects_an_oversized_aggregate_response() {
    let mut source = String::from("unit SignatureResponseLimit;\ninterface\n");
    for index in 0..128 {
        writeln!(
            &mut source,
            "procedure Run(X: T{index}{}); overload;",
            "X".repeat(2_048)
        )
        .expect("write oversized signature");
    }
    source.push_str("implementation\nprocedure Caller;\nbegin\n  Run(1);\nend;\nend.\n");
    let source_uri = uri("SignatureResponseLimit");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("aggregate response source parses");

    let error = index
        .signature_help(&source_uri, position_after(&source, "  Run(", 0))
        .expect_err("aggregate signature response must have a hard budget");
    assert!(error.contains("response"), "{error}");
}

#[test]
fn signature_help_rejects_an_oversized_entity_to_parenthesis_scan() {
    let mut source = String::from(
        "unit SignatureOpenScanLimit;\ninterface\nprocedure Run(Value: Integer);\nimplementation\nprocedure Run(Value: Integer);\nbegin\nend;\nprocedure Caller;\nbegin\n  Run",
    );
    source.push_str(&" ".repeat(65_536));
    source.push_str("(1);\nend;\nend.\n");
    let source_uri = uri("SignatureOpenScanLimit");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.clone())
        .expect("signature open scan source parses");

    let open = source.rfind('(').expect("call open");
    let position = text::offset_to_position(&source, open + 1).expect("open scan position");
    let error = index
        .signature_help(&source_uri, position)
        .expect_err("entity-to-parenthesis scan must be bounded");
    assert!(error.contains("scan limit"), "{error}");
}

#[test]
fn routine_call_navigates_to_interface_declaration_and_body_definition() {
    let mut index = NavigationIndex::new();
    let main_uri = uri("Main");
    let provider_uri = uri("Provider");
    index
        .update(main_uri.clone(), MAIN.to_string())
        .expect("main parses");
    index
        .update(provider_uri.clone(), PROVIDER.to_string())
        .expect("provider parses");

    let call = locations_at(
        &index,
        &main_uri,
        MAIN,
        "PublicRoutine",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(call.len(), 1);
    assert_location_start(
        &call[0],
        &provider_uri,
        position_of(PROVIDER, "PublicRoutine", 0),
    );

    let body = locations_at(
        &index,
        &main_uri,
        MAIN,
        "PublicRoutine",
        0,
        NavigationTarget::Definition,
    );
    assert_eq!(body.len(), 1);
    assert_location_start(
        &body[0],
        &provider_uri,
        position_of(PROVIDER, "PublicRoutine", 1),
    );
}

#[test]
fn method_declaration_definition_fields_properties_and_self_are_scope_aware() {
    let mut index = NavigationIndex::new();
    let main_uri = uri("Main");
    let provider_uri = uri("Provider");
    index
        .update(main_uri.clone(), MAIN.to_string())
        .expect("main parses");
    index
        .update(provider_uri.clone(), PROVIDER.to_string())
        .expect("provider parses");

    let method_declaration = locations_at(
        &index,
        &main_uri,
        MAIN,
        "DoThing",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(method_declaration.len(), 1);
    assert_location_start(
        &method_declaration[0],
        &provider_uri,
        position_of(PROVIDER, "DoThing", 0),
    );

    let method_definition = locations_at(
        &index,
        &main_uri,
        MAIN,
        "DoThing",
        0,
        NavigationTarget::Definition,
    );
    assert_eq!(method_definition.len(), 1);
    assert_location_start(
        &method_definition[0],
        &provider_uri,
        position_of(PROVIDER, "DoThing", 1),
    );

    let field = locations_at(
        &index,
        &main_uri,
        MAIN,
        "FValue",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(field.len(), 1);
    assert_location_start(&field[0], &provider_uri, position_of(PROVIDER, "FValue", 0));

    let property = locations_at(
        &index,
        &main_uri,
        MAIN,
        "Value",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(property.len(), 1);
    assert_location_start(
        &property[0],
        &provider_uri,
        position_of(PROVIDER, "Value", 1),
    );

    let self_method = locations_at(
        &index,
        &provider_uri,
        PROVIDER,
        "FValue",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(self_method.len(), 1);
    assert_location_start(
        &self_method[0],
        &provider_uri,
        position_of(PROVIDER, "FValue", 0),
    );
}

#[test]
fn inherited_class_members_resolve_for_navigation_completion_and_hover() {
    let source_uri = uri("InheritedClassMembers");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), INHERITED_CLASS_MEMBERS.to_owned())
        .expect("inherited class source parses");

    let field = index.navigate(
        &source_uri,
        position_of(INHERITED_CLASS_MEMBERS, "BaseField := 2", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(field.len(), 1);
    assert_location_start(
        &field[0],
        &source_uri,
        position_of(INHERITED_CLASS_MEMBERS, "BaseField", 0),
    );

    let method = index.navigate(
        &source_uri,
        position_of(INHERITED_CLASS_MEMBERS, "BaseMethod;\nend;", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(method.len(), 1);
    assert_location_start(
        &method[0],
        &source_uri,
        position_of(INHERITED_CLASS_MEMBERS, "BaseMethod", 0),
    );

    let completion = index
        .completion(
            &source_uri,
            position_after(INHERITED_CLASS_MEMBERS, "Obj.Ba", 0),
        )
        .expect("inherited member completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["BaseField", "BaseMethod"]
    );

    let hover = index
        .hover(
            &source_uri,
            position_of(INHERITED_CLASS_MEMBERS, "BaseField := 2", 0),
        )
        .expect("inherited member hover");
    assert!(hover_text(&hover).contains("BaseField: Integer"));
}

#[test]
fn inherited_members_resolve_through_a_cross_unit_ancestor() {
    let provider_uri = uri("InheritedProvider");
    let consumer_uri = uri("InheritedConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), INHERITED_PROVIDER.to_owned())
        .expect("inherited provider parses");
    index
        .update(consumer_uri.clone(), INHERITED_CONSUMER.to_owned())
        .expect("inherited consumer parses");

    let field = index.navigate(
        &consumer_uri,
        position_of(INHERITED_CONSUMER, "CrossField := 1", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(field.len(), 1);
    assert_location_start(
        &field[0],
        &provider_uri,
        position_of(INHERITED_PROVIDER, "CrossField", 0),
    );

    let completion = index
        .completion(
            &consumer_uri,
            position_after(INHERITED_CONSUMER, "Obj.Cross", 0),
        )
        .expect("cross-unit inherited completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["CrossField", "CrossMethod"]
    );

    let hover = index
        .hover(
            &consumer_uri,
            position_of(INHERITED_CONSUMER, "CrossMethod;\nend;", 0),
        )
        .expect("cross-unit inherited hover");
    assert!(hover_text(&hover).contains("procedure CrossMethod;"));
}

#[test]
fn inherited_interface_members_resolve_from_a_derived_interface() {
    let source_uri = uri("InheritedInterfaceMembers");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), INHERITED_INTERFACE_MEMBERS.to_owned())
        .expect("inherited interface source parses");

    let base_method = index.navigate(
        &source_uri,
        position_of(INHERITED_INTERFACE_MEMBERS, "BaseMethod", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(base_method.len(), 1);
    assert_location_start(
        &base_method[0],
        &source_uri,
        position_of(INHERITED_INTERFACE_MEMBERS, "BaseMethod", 0),
    );

    let completion = index
        .completion(
            &source_uri,
            position_after(INHERITED_INTERFACE_MEMBERS, "Obj.Ba", 0),
        )
        .expect("inherited interface completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["BaseMethod"]
    );
}

#[test]
fn nested_member_type_lookup_uses_the_declaring_unit_context() {
    let provider_uri = uri("DeclaredMemberTypeProvider");
    let consumer_uri = uri("DeclaredMemberTypeConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(
            provider_uri.clone(),
            DECLARED_MEMBER_TYPE_PROVIDER.to_owned(),
        )
        .expect("declared member type provider parses");
    index
        .update(
            consumer_uri.clone(),
            DECLARED_MEMBER_TYPE_CONSUMER.to_owned(),
        )
        .expect("declared member type consumer parses");

    let usage = position_of(DECLARED_MEMBER_TYPE_CONSUMER, "Shared", 1);
    let locations = index.navigate(&consumer_uri, usage, NavigationTarget::Declaration);
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &provider_uri,
        position_of(DECLARED_MEMBER_TYPE_PROVIDER, "Shared", 0),
    );

    let hover = index
        .hover(&consumer_uri, usage)
        .expect("declared member type hover");
    let text = hover_text(&hover);
    assert!(text.contains("Shared: Integer"), "unexpected hover: {text}");
    assert!(!text.contains("Shared: string"), "wrong hover: {text}");
}

#[test]
fn class_lookup_excludes_implemented_interface_members() {
    let source_uri = uri("ClassInterfaceParents");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), CLASS_INTERFACE_PARENTS.to_owned())
        .expect("class/interface parent source parses");

    let contract = index.navigate(
        &source_uri,
        position_of(CLASS_INTERFACE_PARENTS, "ContractOnly", 1),
        NavigationTarget::Declaration,
    );
    assert!(
        contract.is_empty(),
        "implemented interface contract leaked into class lookup: {contract:?}"
    );

    let base_method = index.navigate(
        &source_uri,
        position_of(CLASS_INTERFACE_PARENTS, "Shared", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(base_method.len(), 1);
    assert_location_start(
        &base_method[0],
        &source_uri,
        position_of(CLASS_INTERFACE_PARENTS, "Shared", 1),
    );

    let completion = index
        .completion(
            &source_uri,
            position_after(CLASS_INTERFACE_PARENTS, "Obj.Co", 0),
        )
        .expect("class/interface parent completion");
    assert!(
        completion.items.is_empty(),
        "implemented interface contract completion leaked: {:?}",
        completion.items
    );
}

#[test]
fn completion_keeps_proven_members_when_an_inherited_name_is_ambiguous() {
    let source_uri = uri("PerNameAmbiguousInheritance");
    let mut index = NavigationIndex::new();
    index
        .update(
            source_uri.clone(),
            PER_NAME_AMBIGUOUS_INHERITANCE.to_owned(),
        )
        .expect("per-name ambiguous inheritance source parses");

    let unique = index.navigate(
        &source_uri,
        position_of(PER_NAME_AMBIGUOUS_INHERITANCE, "Unique", 1),
        NavigationTarget::Declaration,
    );
    assert_eq!(unique.len(), 1);
    assert_location_start(
        &unique[0],
        &source_uri,
        position_of(PER_NAME_AMBIGUOUS_INHERITANCE, "Unique", 0),
    );

    let unique_completion = index
        .completion(
            &source_uri,
            position_after(PER_NAME_AMBIGUOUS_INHERITANCE, "C.Unique", 0),
        )
        .expect("unambiguous inherited completion");
    assert_eq!(
        unique_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Unique"]
    );
    assert!(!unique_completion.is_incomplete);

    let shared_completion = index
        .completion(
            &source_uri,
            position_after(PER_NAME_AMBIGUOUS_INHERITANCE, "C.Shared", 0),
        )
        .expect("direct shadowing completion");
    assert_eq!(
        shared_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Shared"]
    );
    assert!(!shared_completion.is_incomplete);
}

#[test]
fn cross_unit_inherited_private_members_are_not_completion_visible() {
    let provider = r#"unit PrivateInheritedProvider;
interface
type
  TBase = class
  private
    Hidden: Integer;
  end;
end.
"#;
    let consumer = r#"unit PrivateInheritedConsumer;
interface
uses PrivateInheritedProvider;
type
  TChild = class(PrivateInheritedProvider.TBase)
  end;
implementation
procedure Caller;
var
  Obj: TChild;
begin
  Obj.Hid;
end;
end.
"#;
    let provider_uri = uri("PrivateInheritedProvider");
    let consumer_uri = uri("PrivateInheritedConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri, provider.to_owned())
        .expect("private inherited provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("private inherited consumer parses");

    let completion = index
        .completion(&consumer_uri, position_after(consumer, "Obj.Hid", 0))
        .expect("private inherited completion");
    assert!(
        completion.items.is_empty(),
        "private inherited member leaked into completion: {:?}",
        completion.items
    );
}

#[test]
fn unqualified_inherited_member_completion_keeps_proven_members() {
    let source = r#"unit UnqualifiedInheritedCompletion;
interface
type
  TBase = class
    BaseField: Integer;
  end;
  TChild = class(TBase)
    procedure Run;
  end;
implementation
procedure TChild.Run;
begin
  BaseF;
end;
end.
"#;
    let source_uri = uri("UnqualifiedInheritedCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unqualified inherited completion source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "  BaseF", 1))
        .expect("unqualified inherited completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["BaseField"]
    );
}

#[test]
fn unknown_ancestry_completion_is_reported_incomplete() {
    let source = r#"unit UnknownAncestryCompletion;
interface
type
  TChild = class(TMissing)
  end;
implementation
procedure Caller;
var
  Obj: TChild;
begin
  Obj.Un;
end;
end.
"#;
    let source_uri = uri("UnknownAncestryCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown ancestry completion source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "Obj.Un", 0))
        .expect("unknown ancestry completion");
    assert!(completion.items.is_empty());
    assert!(
        completion.is_incomplete,
        "unknown ancestry was reported as complete"
    );
}

#[test]
fn helper_completion_retains_unknown_target_ancestry_with_proven_members() {
    let source = r#"unit UnknownHelperAncestryCompletion;
interface
type
  T = class(TMissing)
    X: Integer;
  end;
  H = class helper for T
    procedure P;
  end;
var
  V: T;
implementation
procedure H.P;
begin
  Self.X := 1;
  ;
end;
end.
"#;
    let source_uri = uri("UnknownHelperAncestryCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown helper ancestry source parses");

    let qualified = index
        .completion(&source_uri, position_after(source, "Self.", 0))
        .expect("qualified helper completion");
    let qualified_labels = qualified
        .items
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();
    assert!(
        qualified_labels.contains(&"P"),
        "helper member missing: {qualified_labels:?}"
    );
    assert!(
        qualified_labels.contains(&"X"),
        "target member missing: {qualified_labels:?}"
    );
    assert!(
        qualified.is_incomplete,
        "unknown target ancestry was reported as complete: {qualified_labels:?}"
    );

    let blank = position_of(source, "  ;", 0);
    let implicit = index
        .completion(&source_uri, Position::new(blank.line, blank.character + 2))
        .expect("implicit helper completion");
    let implicit_labels = implicit
        .items
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();
    assert!(
        implicit_labels.contains(&"P"),
        "helper member missing: {implicit_labels:?}"
    );
    assert!(
        implicit_labels.contains(&"X"),
        "target member missing: {implicit_labels:?}"
    );
    assert!(
        !implicit_labels.contains(&"V"),
        "unrelated global leaked through unknown target ancestry: {implicit_labels:?}"
    );
    assert!(
        implicit.is_incomplete,
        "unknown target ancestry was reported as complete: {implicit_labels:?}"
    );
}

#[test]
fn ambiguous_interface_completion_is_reported_incomplete() {
    let source = r#"unit AmbiguousInterfaceCompletion;
interface
type
  IA = interface
    procedure Shared;
  end;
  IB = interface
    procedure Shared;
  end;
  IChild = interface(IA, IB)
  end;
implementation
procedure Caller;
var
  Obj: IChild;
begin
  Obj.Sh;
end;
end.
"#;
    let source_uri = uri("AmbiguousInterfaceCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("ambiguous interface completion source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "Obj.Sh", 0))
        .expect("ambiguous interface completion");
    assert!(completion.items.is_empty());
    assert!(
        completion.is_incomplete,
        "ambiguous interface lookup was reported as complete"
    );
}

#[test]
fn direct_derived_members_shadow_inherited_members_and_completions() {
    let source = r#"unit DerivedShadow;
interface
type
  TBase = class
    Shared: Integer;
    procedure Run;
  end;
  TChild = class(TBase)
    Shared: string;
    procedure Run;
  end;
implementation
procedure TBase.Run;
begin
end;
procedure TChild.Run;
begin
end;
procedure Caller;
var
  Obj: TChild;
begin
  Obj.Shared;
  Obj.Run;
end;
end.
"#;
    let source_uri = uri("DerivedShadow");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("derived shadow source parses");

    let shared = index.navigate(
        &source_uri,
        position_of(source, "Shared", 2),
        NavigationTarget::Declaration,
    );
    assert_eq!(shared.len(), 1);
    assert_location_start(&shared[0], &source_uri, position_of(source, "Shared", 1));

    let shared_completion = index
        .completion(&source_uri, position_after(source, "Obj.Sh", 0))
        .expect("derived shadow field completion");
    assert_eq!(
        shared_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Shared"]
    );

    let run_completion = index
        .completion(&source_uri, position_after(source, "Obj.Ru", 0))
        .expect("derived override completion");
    assert_eq!(
        run_completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Run"]
    );
}

#[test]
fn unresolved_ancestry_does_not_fall_back_to_unrelated_members() {
    let source = r#"unit MissingAncestor;
interface
type
  TChild = class(TMissing)
    procedure Run;
  end;
const
  GlobalName = 1;
implementation
procedure TChild.Run;
begin
  GlobalName;
end;
procedure Caller;
var
  Obj: TChild;
begin
  Obj.UnknownMember;
end;
end.
"#;
    let source_uri = uri("MissingAncestor");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("missing ancestor source parses");

    assert!(
        index
            .navigate(
                &source_uri,
                position_of(source, "GlobalName", 1),
                NavigationTarget::Declaration,
            )
            .is_empty()
    );
    assert!(
        index
            .completion(&source_uri, position_after(source, "Obj.Un", 0))
            .expect("missing ancestor completion")
            .items
            .is_empty()
    );
}

#[test]
fn cyclic_ancestry_fails_closed_without_recursive_lookup() {
    let source = r#"unit CyclicAncestor;
interface
type
  TA = class(TB)
    procedure Run;
  end;
  TB = class(TA)
    CycleField: Integer;
  end;
implementation
procedure TA.Run;
begin
  CycleField;
end;
procedure Caller;
var
  Obj: TA;
begin
  Obj.CycleField;
end;
end.
"#;
    let source_uri = uri("CyclicAncestor");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("cyclic ancestor source parses");

    let locations = index.navigate(
        &source_uri,
        position_of(source, "CycleField", 2),
        NavigationTarget::Declaration,
    );
    assert!(locations.is_empty());
}

#[test]
fn ambiguous_ancestry_fails_closed_without_selecting_a_parent() {
    let parent_a = r#"unit ParentA;
interface
type
  TBase = class
    Shared: Integer;
  end;
end.
"#;
    let parent_b = r#"unit ParentB;
interface
type
  TBase = class
    Shared: string;
  end;
end.
"#;
    let consumer = r#"unit AmbiguousAncestor;
interface
uses ParentA, ParentB;
type
  TChild = class(TBase)
  end;
implementation
procedure Caller;
var
  Obj: TChild;
begin
  Obj.Shared;
end;
end.
"#;
    let parent_a_uri = uri("ParentA");
    let parent_b_uri = uri("ParentB");
    let consumer_uri = uri("AmbiguousAncestor");
    let mut index = NavigationIndex::new();
    index
        .update(parent_a_uri, parent_a.to_owned())
        .expect("first ambiguous parent parses");
    index
        .update(parent_b_uri, parent_b.to_owned())
        .expect("second ambiguous parent parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("ambiguous consumer parses");

    assert!(
        index
            .navigate(
                &consumer_uri,
                position_of(consumer, "Shared", 0),
                NavigationTarget::Declaration,
            )
            .is_empty()
    );
}

#[test]
fn unqualified_class_members_precede_unit_and_imported_globals() {
    let mut index = NavigationIndex::new();
    let caller_uri = uri("MemberPrecedenceCaller");
    let provider_uri = uri("MemberPrecedenceProvider");
    index
        .update(caller_uri.clone(), MEMBER_PRECEDENCE_CALLER.to_string())
        .expect("member precedence caller parses");
    index
        .update(provider_uri, MEMBER_PRECEDENCE_PROVIDER.to_string())
        .expect("member precedence provider parses");

    let result = locations_at(
        &index,
        &caller_uri,
        MEMBER_PRECEDENCE_CALLER,
        "FValue",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(result.len(), 1);
    assert_location_start(
        &result[0],
        &caller_uri,
        position_of(MEMBER_PRECEDENCE_CALLER, "FValue", 0),
    );
}

#[test]
fn method_locals_still_shadow_class_members() {
    let mut index = NavigationIndex::new();
    let source_uri = uri("MethodLocalScope");
    index
        .update(source_uri.clone(), METHOD_LOCAL_SCOPE.to_string())
        .expect("method local scope parses");

    let result = locations_at(
        &index,
        &source_uri,
        METHOD_LOCAL_SCOPE,
        "FValue",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(result.len(), 1);
    assert_location_start(
        &result[0],
        &source_uri,
        position_of(METHOD_LOCAL_SCOPE, "FValue", 1),
    );
}

#[test]
fn imported_variable_types_resolve_in_their_declaration_context() {
    let mut index = NavigationIndex::new();
    let types_uri = uri("Types");
    let provider_uri = uri("TypedProvider");
    let caller_uri = uri("TypedCaller");
    index
        .update(types_uri.clone(), TYPES_UNIT.to_string())
        .expect("types unit parses");
    index
        .update(provider_uri, TYPED_PROVIDER.to_string())
        .expect("typed provider parses");
    index
        .update(caller_uri.clone(), TYPED_CALLER.to_string())
        .expect("typed caller parses");

    let result = locations_at(
        &index,
        &caller_uri,
        TYPED_CALLER,
        "Field",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(result.len(), 1);
    assert_location_start(&result[0], &types_uri, position_of(TYPES_UNIT, "Field", 0));
}

#[test]
fn conditional_method_attributes_preserve_cross_unit_type_navigation() {
    let mut index = NavigationIndex::new();
    let provider_uri = uri("ConditionalTypeProvider");
    let caller_uri = uri("ConditionalTypeCaller");
    index
        .update(provider_uri.clone(), CONDITIONAL_TYPE_PROVIDER.to_string())
        .expect("conditional type provider parses");
    index
        .update(caller_uri.clone(), CONDITIONAL_TYPE_CALLER.to_string())
        .expect("conditional type caller parses");

    let generic_type = locations_at(
        &index,
        &caller_uri,
        CONDITIONAL_TYPE_CALLER,
        "TMDIBDatabase",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(generic_type.len(), 1);
    assert_location_start(
        &generic_type[0],
        &provider_uri,
        position_of(CONDITIONAL_TYPE_PROVIDER, "TMDIBDatabase", 0),
    );

    let receiver_definition = locations_at(
        &index,
        &caller_uri,
        CONDITIONAL_TYPE_CALLER,
        "Execute",
        0,
        NavigationTarget::Definition,
    );
    assert_eq!(receiver_definition.len(), 1);
    assert_location_start(
        &receiver_definition[0],
        &provider_uri,
        position_of(CONDITIONAL_TYPE_PROVIDER, "Execute", 1),
    );
}

#[test]
fn generic_field_specialization_resolves_nested_member_navigation() {
    let source = r#"unit GenericField;
interface
type
  TBox<T> = class
    Value: T;
    function GetValue: T;
    property Item: T read Value;
  end;
  TWidget = class
    Member: Integer;
  end;
procedure Caller;
implementation
function TBox<T>.GetValue: T;
begin
  Result := Value;
end;
procedure Caller;
var
  Box: TBox<TWidget>;
  Nested: TBox<TBox<TWidget>>;
begin
  Box.Value.Member;
  Box.GetValue().Member;
  Box.Item.Member;
  Nested.Value.Value.Member;
end;
end.
"#;
    let source_uri = uri("GenericField");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic field source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "Member",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "Member: Integer", 0),
    );

    let result_locations = locations_at(
        &index,
        &source_uri,
        source,
        "Member",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(result_locations.len(), 1);
    assert_location_start(
        &result_locations[0],
        &source_uri,
        position_of(source, "Member: Integer", 0),
    );

    let property_locations = locations_at(
        &index,
        &source_uri,
        source,
        "Member",
        3,
        NavigationTarget::Declaration,
    );
    assert_eq!(property_locations.len(), 1);
    assert_location_start(
        &property_locations[0],
        &source_uri,
        position_of(source, "Member: Integer", 0),
    );

    let nested_locations = locations_at(
        &index,
        &source_uri,
        source,
        "Member",
        4,
        NavigationTarget::Declaration,
    );
    assert_eq!(nested_locations.len(), 1);
    assert_location_start(
        &nested_locations[0],
        &source_uri,
        position_of(source, "Member: Integer", 0),
    );
}

#[test]
fn generic_inherited_specialization_resolves_members() {
    let source = r#"unit GenericInherited;
interface
type
  TBox<T> = class
    Value: T;
  end;
  TWidget = class
    Member: Integer;
  end;
  TChild = class(TBox<TWidget>)
  end;
procedure Caller;
implementation
procedure Caller;
var
  Child: TChild;
begin
  Child.Value.Member;
end;
end.
"#;
    let source_uri = uri("GenericInherited");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic inherited source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "Member",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "Member: Integer", 0),
    );
}

#[test]
fn generic_constructor_result_preserves_specialization() {
    let source = r#"unit GenericConstructor;
interface
type
  TBox<T> = class
    constructor Create(Value: T);
    Value: T;
  end;
  TWidget = class
    Member: Integer;
  end;
implementation
constructor TBox<T>.Create(Value: T);
begin
end;
procedure Caller;
var
  Widget: TWidget;
begin
  TBox<TWidget>.Create(Widget).Value.Member;
end;
end.
"#;
    let source_uri = uri("GenericConstructor");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic constructor source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "Member",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "Member: Integer", 0),
    );
}

#[test]
fn generic_routine_explicit_and_inferred_results_resolve_members() {
    let source = r#"unit GenericRoutine;
interface
type
  TWidget = class
    Member: Integer;
  end;
function Identity<T>(Value: T): T;
implementation
function Identity<T>(Value: T): T;
begin
  Result := Value;
end;
procedure Caller;
var
  Widget: TWidget;
begin
  Identity<TWidget>(Widget).Member;
  Identity(Widget).Member;
end;
end.
"#;
    let source_uri = uri("GenericRoutine");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic routine source parses");

    for occurrence in [1, 2] {
        let locations = locations_at(
            &index,
            &source_uri,
            source,
            "Member",
            occurrence,
            NavigationTarget::Declaration,
        );
        assert_eq!(
            locations.len(),
            1,
            "generic routine occurrence {occurrence}"
        );
        assert_location_start(
            &locations[0],
            &source_uri,
            position_of(source, "Member: Integer", 0),
        );
    }
}

#[test]
fn generic_routine_inference_requires_consistent_type_arguments() {
    let source = r#"unit GenericInference;
interface
type
  TWidget = class
    Member: Integer;
  end;
  TOther = class
    Member: Integer;
  end;
function Same<T>(Left: T; Right: T): T;
implementation
function Same<T>(Left: T; Right: T): T;
begin
  Result := Left;
end;
procedure Caller;
var
  Widget: TWidget;
  Other: TOther;
begin
  Same(Widget, Widget).Member;
  Same(Widget, Other).Member;
end;
end.
"#;
    let source_uri = uri("GenericInference");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic inference source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "Member",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "Member: Integer", 0),
    );

    let ambiguous = locations_at(
        &index,
        &source_uri,
        source,
        "Member",
        3,
        NavigationTarget::Declaration,
    );
    assert!(ambiguous.is_empty());
}

#[test]
fn generic_routine_constraints_reject_unproven_actual_types() {
    let source = r#"unit GenericConstraints;
interface
type
  TBase = class
    BaseMember: Integer;
  end;
  TChild = class(TBase)
  end;
  TUnrelated = class
    Member: Integer;
  end;
function Need<T: TBase>(Value: T): T;
implementation
function Need<T: TBase>(Value: T): T;
begin
  Result := Value;
end;
procedure Caller;
var
  Child: TChild;
  Unrelated: TUnrelated;
begin
  Need(Child).BaseMember;
  Need(Unrelated).Member;
end;
end.
"#;
    let source_uri = uri("GenericConstraints");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic constraint source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "BaseMember",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "BaseMember: Integer", 0),
    );

    let rejected = locations_at(
        &index,
        &source_uri,
        source,
        "Member",
        3,
        NavigationTarget::Declaration,
    );
    assert!(rejected.is_empty());
}

#[test]
fn unsupported_generic_constraints_fail_closed() {
    let source = r#"unit UnsupportedGenericConstraint;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
function Bad<U: ^TWidget>(Value: U): U;
implementation
function Bad<U: ^TWidget>(Value: U): U;
begin
  Result := Value;
end;
procedure Caller;
var
  Widget: TWidget;
begin
  Bad(Widget).WidgetMember;
end;
end.
"#;
    let source_uri = uri("UnsupportedGenericConstraint");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unsupported generic constraint source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "WidgetMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(locations.is_empty());
}

#[test]
fn generic_routine_inference_preserves_specialized_actual_types() {
    let source = r#"unit GenericNestedInference;
interface
type
  TBox<T> = class
    Value: T;
  end;
  TWidget = class
    Member: Integer;
  end;
function Identity<T>(Value: T): T;
implementation
function Identity<T>(Value: T): T;
begin
  Result := Value;
end;
procedure Caller;
var
  Box: TBox<TWidget>;
begin
  Identity(Box).Value.Member;
end;
end.
"#;
    let source_uri = uri("GenericNestedInference");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("nested generic inference source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "Member",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "Member: Integer", 0),
    );
}

#[test]
fn generic_specialization_preserves_same_spelling_cross_unit_types() {
    let provider_a = r#"unit GenericProviderA;
interface
type
  TWidget = class
    AMember: Integer;
  end;
implementation
end.
"#;
    let provider_b = r#"unit GenericProviderB;
interface
type
  TWidget = class
    BMember: Integer;
  end;
implementation
end.
"#;
    let consumer = r#"unit GenericCrossConsumer;
interface
uses GenericProviderA, GenericProviderB;
type
  TBox<T> = class
    Value: T;
  end;
implementation
procedure Caller;
var
  Box: TBox<GenericProviderA.TWidget>;
begin
  Box.Value.AMember;
  Box.Value.BMember;
end;
end.
"#;
    let provider_a_uri = uri("GenericProviderA");
    let provider_b_uri = uri("GenericProviderB");
    let consumer_uri = uri("GenericCrossConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_a_uri, provider_a.to_owned())
        .expect("generic provider A parses");
    index
        .update(provider_b_uri, provider_b.to_owned())
        .expect("generic provider B parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("generic cross-unit consumer parses");

    let a_member = locations_at(
        &index,
        &consumer_uri,
        consumer,
        "AMember",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(a_member.len(), 1);
    assert_location_start(
        &a_member[0],
        &uri("GenericProviderA"),
        position_of(provider_a, "AMember: Integer", 0),
    );

    let b_member = locations_at(
        &index,
        &consumer_uri,
        consumer,
        "BMember",
        0,
        NavigationTarget::Declaration,
    );
    assert!(b_member.is_empty());
}

#[test]
fn generic_method_parameters_shadow_and_retain_owner_parameters() {
    let source = r#"unit GenericMethodShadowing;
interface
type
  TBox<T> = class
    function Shadow<T>(Value: T): T;
    function Keep<U>(Value: U): T;
  end;
  TWidget = class
    Member: Integer;
  end;
  TOther = class
    OtherMember: Integer;
  end;
implementation
function TBox<T>.Shadow<T>(Value: T): T;
begin
  Result := Value;
end;
function TBox<T>.Keep<U>(Value: U): T;
begin
  Result := Default(T);
end;
procedure Caller;
var
  Box: TBox<TWidget>;
  Other: TOther;
begin
  Box.Shadow<TOther>(Other).OtherMember;
  Box.Keep<TOther>(Other).Member;
end;
end.
"#;
    let source_uri = uri("GenericMethodShadowing");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic method shadowing source parses");

    let shadowed = locations_at(
        &index,
        &source_uri,
        source,
        "OtherMember",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(shadowed.len(), 1);
    assert_location_start(
        &shadowed[0],
        &source_uri,
        position_of(source, "OtherMember: Integer", 0),
    );

    let owner = locations_at(
        &index,
        &source_uri,
        source,
        "Member",
        3,
        NavigationTarget::Declaration,
    );
    assert_eq!(owner.len(), 1);
    assert_location_start(
        &owner[0],
        &source_uri,
        position_of(source, "Member: Integer", 0),
    );
}

#[test]
fn generic_method_assistance_uses_explicit_and_inferred_method_substitutions() {
    let source = r#"unit GenericMethodAssistance;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  TOther = class
    OtherMember: Integer;
  end;
  TBox<T> = class
    function Shadow<T>(Value: T): T;
  end;
implementation
function TBox<T>.Shadow<T>(Value: T): T;
begin
  Result := Value;
end;
procedure Caller;
var
  B: TBox<TWidget>;
  Other: TOther;
begin
  B.Shadow<TOther>(Other).OtherMember;
  B.Shadow(Other).OtherMember;
end;
end.
"#;
    let source_uri = uri("GenericMethodAssistance");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic method assistance source parses");

    for occurrence in [1, 2] {
        let locations = locations_at(
            &index,
            &source_uri,
            source,
            "OtherMember",
            occurrence,
            NavigationTarget::Declaration,
        );
        assert_exact_type_location(&locations, &source_uri, source, "OtherMember", 0);
    }

    for occurrence in [2, 3] {
        let position = position_of(source, "Shadow", occurrence);
        let type_definition = index.type_definitions(&source_uri, position);
        assert_exact_type_location(&type_definition, &source_uri, source, "TOther", 0);

        let hover = index
            .hover(&source_uri, position)
            .expect("generic method hover");
        assert!(
            hover_text(&hover).contains("function Shadow<T>(Value: TOther): TOther;"),
            "{}",
            hover_text(&hover)
        );
    }

    let signature = index
        .signature_help(&source_uri, position_after(source, "B.Shadow(", 0))
        .expect("generic method signature help")
        .expect("generic method signature");
    assert_eq!(signature.signatures.len(), 1);
    assert_eq!(
        signature.signatures[0].label,
        "function Shadow<T>(Value: TOther): TOther;"
    );

    for needle in ["B.Shadow<TOther>(", "B.Shadow<TOther>(Other"] {
        let signature = index
            .signature_help(&source_uri, position_after(source, needle, 0))
            .expect("explicit generic method signature help")
            .expect("explicit generic method signature");
        assert_eq!(signature.signatures.len(), 1);
        assert_eq!(signature.active_signature, Some(0));
        assert_eq!(signature.active_parameter, Some(0));
        assert_eq!(
            signature.signatures[0].label,
            "function Shadow<T>(Value: TOther): TOther;"
        );
    }
}

#[test]
fn generic_method_type_definitions_require_a_selected_method_substitution() {
    let source = r#"unit GenericMethodUnknownTypeDefinition;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  TBox<T> = class
    function Shadow<T>(Value: T): T;
  end;
implementation
function TBox<T>.Shadow<T>(Value: T): T;
begin
  Result := Value;
end;
procedure Caller;
var
  Box: TBox<TWidget>;
begin
  Box.Shadow();
  Box.Shadow(UnknownValue);
end;
end.
"#;
    let source_uri = uri("GenericMethodUnknownTypeDefinition");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown generic method source parses");

    for occurrence in [2, 3] {
        let type_definition =
            index.type_definitions(&source_uri, position_of(source, "Shadow", occurrence));
        assert!(
            type_definition.is_empty(),
            "unselected generic method occurrence {occurrence} must not use the owner substitution"
        );
    }
}

#[test]
fn generic_type_constraints_reject_invalid_specializations() {
    let source = r#"unit GenericTypeConstraints;
interface
type
  TBase = class
    BaseMember: Integer;
  end;
  TBox<T: TBase> = class
    Value: T;
  end;
  TChild = class(TBase)
  end;
  TUnrelated = class
    Member: Integer;
  end;
procedure Caller;
implementation
procedure Caller;
var
  Valid: TBox<TChild>;
  Invalid: TBox<TUnrelated>;
begin
  Valid.Value.BaseMember;
  Invalid.Value.Member;
end;
end.
"#;
    let source_uri = uri("GenericTypeConstraints");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic type constraint source parses");

    let valid = locations_at(
        &index,
        &source_uri,
        source,
        "BaseMember",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(valid.len(), 1);
    assert_location_start(
        &valid[0],
        &source_uri,
        position_of(source, "BaseMember: Integer", 0),
    );

    let invalid = locations_at(
        &index,
        &source_uri,
        source,
        "Member",
        3,
        NavigationTarget::Declaration,
    );
    assert!(invalid.is_empty());
}

#[test]
fn generic_kind_constraints_accept_only_matching_specializations() {
    let source = r#"unit GenericKindConstraints;
interface
type
  TClassBox<T: class> = class
    Value: T;
  end;
  TRecordBox<T: record> = class
    Value: T;
  end;
  TInterfaceBox<T: interface> = class
    Value: T;
  end;
  TClass = class
    ClassMember: Integer;
  end;
  TRecord = record
    RecordMember: Integer;
  end;
  TInterface = interface
    procedure InterfaceMethod;
  end;
procedure Caller;
implementation
procedure Caller;
var
  ValidClass: TClassBox<TClass>;
  InvalidClass: TClassBox<TRecord>;
  ValidRecord: TRecordBox<TRecord>;
  InvalidRecord: TRecordBox<TClass>;
  ValidInterface: TInterfaceBox<TInterface>;
  InvalidInterface: TInterfaceBox<TClass>;
begin
  ValidClass.Value.ClassMember;
  InvalidClass.Value.RecordMember;
  ValidRecord.Value.RecordMember;
  InvalidRecord.Value.ClassMember;
  ValidInterface.Value.InterfaceMethod;
  InvalidInterface.Value.ClassMember;
end;
end.
"#;
    let source_uri = uri("GenericKindConstraints");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic kind constraint source parses");

    let valid_class = locations_at(
        &index,
        &source_uri,
        source,
        "ClassMember",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(valid_class.len(), 1);
    assert_location_start(
        &valid_class[0],
        &source_uri,
        position_of(source, "ClassMember: Integer", 0),
    );

    let invalid_class = locations_at(
        &index,
        &source_uri,
        source,
        "RecordMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(invalid_class.is_empty());

    let valid_record = locations_at(
        &index,
        &source_uri,
        source,
        "RecordMember",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(valid_record.len(), 1);
    assert_location_start(
        &valid_record[0],
        &source_uri,
        position_of(source, "RecordMember: Integer", 0),
    );

    let invalid_record = locations_at(
        &index,
        &source_uri,
        source,
        "ClassMember",
        2,
        NavigationTarget::Declaration,
    );
    assert!(invalid_record.is_empty());

    let valid_interface = locations_at(
        &index,
        &source_uri,
        source,
        "InterfaceMethod",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(valid_interface.len(), 1);
    assert_location_start(
        &valid_interface[0],
        &source_uri,
        position_of(source, "InterfaceMethod", 0),
    );

    let invalid_interface = locations_at(
        &index,
        &source_uri,
        source,
        "ClassMember",
        3,
        NavigationTarget::Declaration,
    );
    assert!(invalid_interface.is_empty());
}

#[test]
fn generic_constructor_constraints_require_a_proven_constructor() {
    let source = r#"unit GenericConstructorConstraints;
interface
type
  TBox<T: constructor> = class
    Value: T;
  end;
  TConstructible = class
    constructor Create;
    Member: Integer;
  end;
  TUnconstructible = class
    OtherMember: Integer;
  end;
procedure Caller;
implementation
constructor TConstructible.Create;
begin
end;
procedure Caller;
var
  Valid: TBox<TConstructible>;
  Invalid: TBox<TUnconstructible>;
begin
  Valid.Value.Member;
  Invalid.Value.OtherMember;
end;
end.
"#;
    let source_uri = uri("GenericConstructorConstraints");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic constructor constraint source parses");

    let valid = locations_at(
        &index,
        &source_uri,
        source,
        "Member",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(valid.len(), 1);
    assert_location_start(
        &valid[0],
        &source_uri,
        position_of(source, "Member: Integer", 0),
    );

    let invalid = locations_at(
        &index,
        &source_uri,
        source,
        "OtherMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(invalid.is_empty());
}

#[test]
fn generic_primitive_results_feed_overload_selection() {
    let source = r#"unit GenericPrimitiveOverload;
interface
function Identity<T>(Value: T): T;
procedure Consume(Value: Integer); overload;
procedure Consume(Value: String); overload;
implementation
function Identity<T>(Value: T): T;
begin
  Result := Value;
end;
procedure Caller;
begin
  Consume(Identity(1));
end;
end.
"#;
    let source_uri = uri("GenericPrimitiveOverload");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic primitive overload source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "Consume",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "Consume(Value: Integer)", 0),
    );
}

#[test]
fn generic_specialized_receivers_feed_completion_hover_and_type_definition() {
    let source = r#"unit GenericAssistance;
interface
type
  TBox<T> = class
    Value: T;
    function GetValue: T;
  end;
  TWidget = class
    Member: Integer;
  end;
implementation
function TBox<T>.GetValue: T;
begin
  Result := Value;
end;
procedure Caller;
var
  Box: TBox<TWidget>;
begin
  Box.Value.Me;
  Box.GetValue().Member;
end;
end.
"#;
    let source_uri = uri("GenericAssistance");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic assistance source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "Box.Value.Me", 0))
        .expect("generic specialized completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Member"]
    );

    let hover = index
        .hover(&source_uri, position_of(source, "Member", 1))
        .expect("generic specialized hover");
    assert!(hover_text(&hover).contains("Member: Integer"));

    let type_definition = index.type_definitions(&source_uri, position_of(source, "Value", 4));
    assert_exact_type_location(&type_definition, &source_uri, source, "TWidget", 0);
}

#[test]
fn inherited_generic_routine_result_preserves_parent_specialization() {
    let source = r#"unit GenericInheritedRoutine;
interface
type
  TBox<T> = class
    function GetValue: T;
  end;
  TWidget = class
    WidgetMember: Integer;
  end;
  TChild = class(TBox<TWidget>)
  end;
implementation
function TBox<T>.GetValue: T;
begin
end;
procedure Caller;
var
  Child: TChild;
begin
  Child.GetValue().WidgetMember;
end;
end.
"#;
    let source_uri = uri("GenericInheritedRoutine");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic inherited routine source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "WidgetMember",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "WidgetMember: Integer", 0),
    );
}

#[test]
fn uninferred_generic_method_does_not_use_the_owner_specialization() {
    let source = r#"unit GenericMethodInference;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  TBox<T> = class
    function Shadow<T>: T;
  end;
implementation
function TBox<T>.Shadow<T>: T;
begin
end;
procedure Caller;
var
  Box: TBox<TWidget>;
begin
  Box.Shadow().WidgetMember;
end;
end.
"#;
    let source_uri = uri("GenericMethodInference");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic method inference source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "WidgetMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(locations.is_empty());
}

#[test]
fn generic_receiver_specialization_selects_the_compatible_overload() {
    let source = r#"unit GenericOverloadSpecialization;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  TOther = class
    OtherMember: Integer;
  end;
  TBox<T> = class
  end;
function Choose(B: TBox<TOther>; N: Integer): TOther; overload;
function Choose(B: TBox<TWidget>; N: Real): TWidget; overload;
implementation
procedure Caller;
var
  Box: TBox<TWidget>;
begin
  Choose(Box, 1).WidgetMember;
end;
end.
"#;
    let source_uri = uri("GenericOverloadSpecialization");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic overload specialization source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "WidgetMember",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "WidgetMember: Integer", 0),
    );
}

#[test]
fn generic_call_with_wrong_explicit_arity_fails_closed() {
    let source = r#"unit GenericCallArity;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  TOther = class
    OtherMember: Integer;
  end;
function Identity<T>(Value: T): T;
implementation
function Identity<T>(Value: T): T;
begin
  Result := Value;
end;
procedure Caller;
var
  Widget: TWidget;
begin
  Identity<TWidget, TOther>(Widget).WidgetMember;
end;
end.
"#;
    let source_uri = uri("GenericCallArity");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic call arity source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "WidgetMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(locations.is_empty());
}

#[test]
fn generic_actuals_use_call_site_scope_and_preserve_multi_argument_arity() {
    let source = r#"unit GenericActualScope;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  T = class
    LocalMember: Integer;
  end;
  TBox<T> = class
    function Pick<U>: U;
  end;
function GlobalPick<A, B>: B;
implementation
function TBox<T>.Pick<U>: U;
begin
end;
function GlobalPick<A, B>: B;
begin
end;
procedure Caller;
var
  Box: TBox<TWidget>;
begin
  GlobalPick<TWidget, T>().LocalMember;
  GlobalPick<TWidget, T>().WidgetMember;
  Box.Pick<T>().LocalMember;
  Box.Pick<T>().WidgetMember;
end;
end.
"#;
    let source_uri = uri("GenericActualScope");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic actual scope source parses");

    for (member, occurrence) in [("LocalMember", 1), ("LocalMember", 2)] {
        let locations = locations_at(
            &index,
            &source_uri,
            source,
            member,
            occurrence,
            NavigationTarget::Declaration,
        );
        assert_eq!(locations.len(), 1, "{member} occurrence {occurrence}");
        assert_location_start(
            &locations[0],
            &source_uri,
            position_of(source, "LocalMember: Integer", 0),
        );
    }

    for (member, occurrence) in [("WidgetMember", 1), ("WidgetMember", 2)] {
        let locations = locations_at(
            &index,
            &source_uri,
            source,
            member,
            occurrence,
            NavigationTarget::Declaration,
        );
        assert!(
            locations.is_empty(),
            "{member} occurrence {occurrence} must not use the owner or declaration scope"
        );
    }
}

#[test]
fn inherited_generic_routine_results_keep_the_declared_parent_arguments() {
    let source = r#"unit GenericInheritedRoutineArguments;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  TOther = class
    OtherMember: Integer;
  end;
  TBox<T> = class
    Value: T;
    function GetValue: T;
  end;
  TChild<T> = class(TBox<TWidget>)
  end;
implementation
function TBox<T>.GetValue: T;
begin
end;
procedure Caller;
var
  Child: TChild<TOther>;
begin
  Child.GetValue().WidgetMember;
  Child.GetValue().OtherMember;
  Child.Value.WidgetMember;
end;
end.
"#;
    let source_uri = uri("GenericInheritedRoutineArguments");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("inherited generic routine argument source parses");

    for (member, occurrence) in [("WidgetMember", 1), ("WidgetMember", 2)] {
        let locations = locations_at(
            &index,
            &source_uri,
            source,
            member,
            occurrence,
            NavigationTarget::Declaration,
        );
        assert_eq!(locations.len(), 1, "{member} occurrence {occurrence}");
        assert_location_start(
            &locations[0],
            &source_uri,
            position_of(source, "WidgetMember: Integer", 0),
        );
    }

    let other_locations = locations_at(
        &index,
        &source_uri,
        source,
        "OtherMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(other_locations.is_empty());
}

#[test]
fn unknown_generic_constraint_prevents_selecting_a_proven_overload() {
    let source = r#"unit UnknownGenericConstraintOverload;
interface
type
  IFoo = interface
    procedure Foo;
  end;
  TBase = class
  end;
  TChild = class(TBase, IFoo)
    procedure Foo;
  end;
  TWidget = class
    WidgetMember: Integer;
  end;
  TOther = class
    OtherMember: Integer;
  end;
function Choose<T: IFoo>(Value: T): TOther; overload;
function Choose(Value: TBase): TWidget; overload;
implementation
procedure TChild.Foo;
begin
end;
procedure Caller;
var
  Child: TChild;
begin
  Choose(Child).WidgetMember;
end;
end.
"#;
    let source_uri = uri("UnknownGenericConstraintOverload");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown generic constraint source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "WidgetMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(
        locations.is_empty(),
        "an unknown generic constraint must not select the proven fallback overload"
    );
}

#[test]
fn contradictory_generic_constraints_fail_closed() {
    let source = r#"unit ContradictoryGenericConstraint;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
function Bad<T: class, record>(Value: T): T;
implementation
function Bad<T: class, record>(Value: T): T;
begin
  Result := Value;
end;
procedure Caller;
var
  Widget: TWidget;
begin
  Bad(Widget).WidgetMember;
end;
end.
"#;
    let source_uri = uri("ContradictoryGenericConstraint");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("contradictory constraint source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "WidgetMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(locations.is_empty());
}

#[test]
fn generic_formal_lookup_does_not_capture_qualified_types_or_members() {
    let source = r#"unit QualifiedRename;
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
  Qualified: QualifiedRename.T;
begin
  Qualified.GlobalMember;
  Obj.T := 1;
end;
end.
"#;
    let source_uri = uri("QualifiedRename");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("qualified generic formal source parses");

    let qualified_type = index.navigate(
        &source_uri,
        final_qualified_type_position(source, "QualifiedRename.T"),
        NavigationTarget::Declaration,
    );
    assert_eq!(qualified_type.len(), 1);
    assert_location_start(
        &qualified_type[0],
        &source_uri,
        position_of(source, "T = class", 0),
    );

    let member = index.navigate(
        &source_uri,
        final_qualified_type_position(source, "Obj.T"),
        NavigationTarget::Declaration,
    );
    assert_eq!(member.len(), 1);
    assert_location_start(
        &member[0],
        &source_uri,
        position_of(source, "T: Integer", 0),
    );
}

#[test]
fn inferred_and_explicit_generic_actuals_keep_the_caller_type_scope() {
    let source = r#"unit GenericInferenceScope;
interface
type
  T = class
    LocalMember: Integer;
  end;
  TWidget = class
    WidgetMember: Integer;
  end;
  TBox<T> = class
    function Pick<U>(Value: U): U;
  end;
implementation
function TBox<T>.Pick<U>(Value: U): U;
begin
  Result := Value;
end;
procedure Caller;
var
  B: TBox<TWidget>;
  Obj: T;
begin
  B.Pick(Obj).LocalMember;
  B.Pick(Obj).WidgetMember;
  B.Pick<T>(Obj).LocalMember;
  B.Pick<T>(Obj).WidgetMember;
end;
end.
"#;
    let source_uri = uri("GenericInferenceScope");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic inference scope source parses");

    for occurrence in [1, 2] {
        let locations = locations_at(
            &index,
            &source_uri,
            source,
            "LocalMember",
            occurrence,
            NavigationTarget::Declaration,
        );
        assert_eq!(locations.len(), 1);
        assert_location_start(
            &locations[0],
            &source_uri,
            position_of(source, "LocalMember: Integer", 0),
        );
    }
    for occurrence in [1, 2] {
        let locations = locations_at(
            &index,
            &source_uri,
            source,
            "WidgetMember",
            occurrence,
            NavigationTarget::Declaration,
        );
        assert!(
            locations.is_empty(),
            "generic occurrence {occurrence} used Widget"
        );
    }
}

#[test]
fn inherited_constructors_satisfy_constructor_constraints_without_fallback() {
    let source = r#"unit InheritedConstructorConstraint;
interface
type
  TBase = class
    constructor Create;
  end;
  TChild = class(TBase)
  end;
  TGood = class
    GoodMember: Integer;
  end;
  TBad = class
    BadMember: Integer;
  end;
function Choose<T: constructor>(Value: T): TGood; overload;
function Choose(Value: TBase): TBad; overload;
implementation
constructor TBase.Create;
begin
end;
procedure Caller;
var
  Obj: TChild;
begin
  Choose(Obj).GoodMember;
  Choose(Obj).BadMember;
end;
end.
"#;
    let source_uri = uri("InheritedConstructorConstraint");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("inherited constructor constraint source parses");

    let good = locations_at(
        &index,
        &source_uri,
        source,
        "GoodMember",
        1,
        NavigationTarget::Declaration,
    );
    assert_exact_type_location(&good, &source_uri, source, "GoodMember", 0);

    let bad = locations_at(
        &index,
        &source_uri,
        source,
        "BadMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(bad.is_empty());
}

#[test]
fn unproven_constructor_constraints_do_not_select_a_fallback_overload() {
    let source = r#"unit UnknownConstructorConstraint;
interface
type
  TBase = class
  end;
  TChild = class(TBase)
  end;
  TGood = class
    GoodMember: Integer;
  end;
  TBad = class
    BadMember: Integer;
  end;
function Choose<T: constructor>(Value: T): TGood; overload;
function Choose(Value: TBase): TBad; overload;
implementation
procedure Caller;
var
  Obj: TChild;
begin
  Choose(Obj).GoodMember;
  Choose(Obj).BadMember;
end;
end.
"#;
    let source_uri = uri("UnknownConstructorConstraint");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unproven constructor constraint source parses");

    let good = locations_at(
        &index,
        &source_uri,
        source,
        "GoodMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(
        good.is_empty(),
        "an unproven constructor constraint must not select the generic overload"
    );

    let bad = locations_at(
        &index,
        &source_uri,
        source,
        "BadMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(
        bad.is_empty(),
        "an unproven constructor constraint must not select the fallback overload"
    );
}

#[test]
fn generic_ancestry_preserves_instantiated_parent_arguments() {
    let source = r#"unit GenericInstantiatedAncestry;
interface
type
  TBox<T> = class
  end;
  TChild<T> = class(TBox<T>)
  end;
  TWidget = class
    WidgetMember: Integer;
  end;
  TOther = class
    OtherMember: Integer;
  end;
function Good(Value: TBox<TWidget>): TWidget;
function Bad(Value: TBox<TOther>): TOther;
implementation
procedure Caller;
var
  Child: TChild<TWidget>;
begin
  Good(Child).WidgetMember;
  Bad(Child).OtherMember;
end;
end.
"#;
    let source_uri = uri("GenericInstantiatedAncestry");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("instantiated ancestry source parses");

    let good = locations_at(
        &index,
        &source_uri,
        source,
        "WidgetMember",
        1,
        NavigationTarget::Declaration,
    );
    assert_exact_type_location(&good, &source_uri, source, "WidgetMember", 0);

    let bad = locations_at(
        &index,
        &source_uri,
        source,
        "OtherMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(bad.is_empty());
}

#[test]
fn recovered_generic_constraint_declarations_are_unsupported() {
    let source = r#"unit RecoveredGenericConstraint;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
function Bad<U: class, record>(Value: U): U;
implementation
function Bad<U: class, record>(Value: U): U;
begin
  Result := Value;
end;
procedure Caller;
var
  Obj: TWidget;
begin
  Bad<TWidget, TWidget>(Obj).WidgetMember;
end;
end.
"#;
    let source_uri = uri("RecoveredGenericConstraint");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("recovered generic constraint source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "WidgetMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(locations.is_empty());
}

#[test]
fn generic_type_expression_arguments_preserve_all_parameters() {
    let source = r#"unit GenericTypeExpressionArguments;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  TOther = class
    OtherMember: Integer;
  end;
  TBox<T; U> = class
    Value: U;
  end;
  TPair<T; U> = class
    Second: U;
  end;
implementation
procedure Caller;
begin
  TBox<TWidget, TOther>.Value.WidgetMember;
  TPair<TWidget, TOther>.Second.OtherMember;
end;
end.
"#;
    let source_uri = uri("GenericTypeExpressionArguments");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic type expression source parses");

    let invalid = locations_at(
        &index,
        &source_uri,
        source,
        "WidgetMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(invalid.is_empty());

    let valid = locations_at(
        &index,
        &source_uri,
        source,
        "OtherMember",
        1,
        NavigationTarget::Declaration,
    );
    assert_exact_type_location(&valid, &source_uri, source, "OtherMember", 0);
}

#[test]
fn unsupported_generic_type_actuals_preserve_arity_and_nested_failure() {
    let source = r#"unit UnsupportedGenericTypeActuals;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  TOther = class
    OtherMember: Integer;
  end;
  TBox<T> = class
    Value: T;
    function Create: TBox<T>;
  end;
  TPair<T; U> = class
    Value: U;
  end;
implementation
function TBox<T>.Create: TBox<T>;
begin
end;
procedure Caller;
var
  Other: TOther;
begin
  TBox<^TOther, TWidget>.Create().Value.WidgetMember;
  TBox<TWidget, ^TOther>.Create().Value.WidgetMember;
  TBox<TBox<^TOther, TWidget>>.Create().Value.Value.WidgetMember;
  TPair<TWidget, TOther>.Value.OtherMember;
end;
end.
"#;
    let source_uri = uri("UnsupportedGenericTypeActuals");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unsupported generic type actual source parses");

    for occurrence in [1, 2, 3] {
        let locations = locations_at(
            &index,
            &source_uri,
            source,
            "WidgetMember",
            occurrence,
            NavigationTarget::Declaration,
        );
        assert!(
            locations.is_empty(),
            "unsupported actual occurrence {occurrence} must fail closed"
        );
    }

    let valid = locations_at(
        &index,
        &source_uri,
        source,
        "OtherMember",
        1,
        NavigationTarget::Declaration,
    );
    assert_exact_type_location(&valid, &source_uri, source, "OtherMember", 0);
}

#[test]
fn generic_type_arguments_ignore_comments_but_reject_unsupported_actuals() {
    let source = r#"unit GenericCommentedTypeArguments;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  TOther = class
    OtherMember: Integer;
  end;
  TBox<T> = class
    Value: T;
    function Create: TBox<T>;
  end;
  TPair<T; U> = class
    Value: U;
  end;
implementation
function TBox<T>.Create: TBox<T>;
begin
end;
procedure Caller;
begin
  TBox<TWidget {brace note}>.Create().Value.WidgetMember;
  TBox<TWidget (*paren-star note*)>.Create().Value.WidgetMember;
  TPair<TWidget {first actual note}, TOther (*second actual note*)>.Value.OtherMember;
  TBox<TWidget, ^TOther>.Create().Value.WidgetMember;
end;
end.
"#;
    let source_uri = uri("GenericCommentedTypeArguments");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("commented generic type argument source parses");

    for occurrence in [1, 2] {
        let locations = locations_at(
            &index,
            &source_uri,
            source,
            "WidgetMember",
            occurrence,
            NavigationTarget::Declaration,
        );
        assert_exact_type_location(&locations, &source_uri, source, "WidgetMember", 0);
    }

    let pair = locations_at(
        &index,
        &source_uri,
        source,
        "OtherMember",
        1,
        NavigationTarget::Declaration,
    );
    assert_exact_type_location(&pair, &source_uri, source, "OtherMember", 0);

    let unsupported = locations_at(
        &index,
        &source_uri,
        source,
        "WidgetMember",
        3,
        NavigationTarget::Declaration,
    );
    assert!(unsupported.is_empty());
}

#[test]
fn generic_call_arguments_ignore_comments_but_preserve_arity() {
    let source = r#"unit GenericCommentArguments;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  TOther = class
  end;
function Identity<T>(Value: T): T;
implementation
function Identity<T>(Value: T): T;
begin
  Result := Value;
end;
procedure Caller;
var
  Obj: TWidget;
begin
  Identity<TWidget { brace note }>(Obj).WidgetMember;
  Identity<TWidget (* star note *)>(Obj).WidgetMember;
  Identity<TWidget, TOther { invalid extra actual }>(Obj).WidgetMember;
end;
end.
"#;
    let source_uri = uri("GenericCommentArguments");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic comment argument source parses");

    for occurrence in [1, 2] {
        let locations = locations_at(
            &index,
            &source_uri,
            source,
            "WidgetMember",
            occurrence,
            NavigationTarget::Declaration,
        );
        assert_exact_type_location(&locations, &source_uri, source, "WidgetMember", 0);
    }
    let invalid = locations_at(
        &index,
        &source_uri,
        source,
        "WidgetMember",
        3,
        NavigationTarget::Declaration,
    );
    assert!(invalid.is_empty());
}

#[test]
fn generic_declaration_comments_do_not_create_constraints_or_reenable_recovery() {
    let source = r#"unit GenericDeclarationComments;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  TOther = class
    OtherMember: Integer;
  end;
function Pick<T {note: harmless}, U>(Value: U): U;
function PickStar<T (*block: harmless*), U>(Value: U): U;
function Bad<T: class, record>(Value: T): T;
implementation
function Pick<T {note: harmless}, U>(Value: U): U;
begin
  Result := Value;
end;
function PickStar<T (*block: harmless*), U>(Value: U): U;
begin
  Result := Value;
end;
function Bad<T: class, record>(Value: T): T;
begin
  Result := Value;
end;
procedure Caller;
var
  Widget: TWidget;
  Other: TOther;
begin
  Pick<TWidget, TOther>(Other).OtherMember;
  PickStar<TWidget, TOther>(Other).OtherMember;
  Bad<TWidget, TWidget>(Widget).WidgetMember;
end;
end.
"#;
    let source_uri = uri("GenericDeclarationComments");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic declaration comment source parses");

    for occurrence in [1, 2] {
        let locations = locations_at(
            &index,
            &source_uri,
            source,
            "OtherMember",
            occurrence,
            NavigationTarget::Declaration,
        );
        assert_exact_type_location(&locations, &source_uri, source, "OtherMember", 0);
    }

    let invalid = locations_at(
        &index,
        &source_uri,
        source,
        "WidgetMember",
        1,
        NavigationTarget::Declaration,
    );
    assert!(invalid.is_empty());
}

#[test]
fn type_definition_resolves_a_specialized_generic_routine_result() {
    let source = r#"unit GenericRoutineTypeDefinition;
interface
type
  TBox<T> = class
    function GetValue: T;
  end;
  TWidget = class
    WidgetMember: Integer;
  end;
implementation
function TBox<T>.GetValue: T;
begin
end;
procedure Caller;
var
  Box: TBox<TWidget>;
begin
  Box.GetValue().WidgetMember;
end;
end.
"#;
    let source_uri = uri("GenericRoutineTypeDefinition");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic routine type definition source parses");

    let type_definition = index.type_definitions(
        &source_uri,
        final_qualified_type_position(source, "Box.GetValue"),
    );
    assert_eq!(type_definition.len(), 1);
    assert_location_start(
        &type_definition[0],
        &source_uri,
        position_of(source, "TWidget = class", 0),
    );
}

#[test]
fn generic_member_completion_excludes_formal_declarations() {
    let source = r#"unit GenericMemberCompletion;
interface
type
  TBox<T> = class
    Value: T;
  end;
  TWidget = class
  end;
procedure Caller;
implementation
procedure Caller;
var
  Box: TBox<TWidget>;
begin
  Box.
end;
end.
"#;
    let source_uri = uri("GenericMemberCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic member completion source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "Box.", 0))
        .expect("generic member completion");
    assert_eq!(
        completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["Value"]
    );
}

#[test]
fn specialized_generic_assistance_renders_bound_routine_labels() {
    let source = r#"unit GenericAssistanceLabels;
interface
type
  TWidget = class
    WidgetMember: Integer;
  end;
  TOther = class
    OtherMember: Integer;
  end;
  T = class
  end;
  TBox<T> = class
    Value: T;
    function GetValue: T;
    procedure Put(Value: T);
  end;
implementation
procedure Caller;
var
  Box: TBox<TWidget>;
  Obj: TWidget;
begin
  Box.GetValue().WidgetMember;
  Box.Value.WidgetMember;
  Box.Put(Obj);
end;
end.
"#;
    let source_uri = uri("GenericAssistanceLabels");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic assistance labels source parses");

    let mut value_position = position_of(source, "Box.Value.WidgetMember", 0);
    value_position.character += "Box.".encode_utf16().count() as u32;
    let value_hover = index
        .hover(&source_uri, value_position)
        .expect("specialized generic field hover");
    assert!(hover_text(&value_hover).contains("Value: TWidget;"));
    assert!(!hover_text(&value_hover).contains("Value: T;"));

    let get_value_position = final_qualified_type_position(source, "Box.GetValue");
    let hover = index
        .hover(&source_uri, get_value_position)
        .expect("specialized generic routine hover");
    assert!(hover_text(&hover).contains("function GetValue: TWidget;"));
    assert!(!hover_text(&hover).contains("function GetValue: T;"));
    assert_eq!(
        hover.range,
        Some(Range::new(
            get_value_position,
            Position::new(
                get_value_position.line,
                get_value_position.character + "GetValue".encode_utf16().count() as u32,
            ),
        ))
    );

    let signature = index
        .signature_help(&source_uri, position_after(source, "Box.Put(", 0))
        .expect("specialized generic routine signature help")
        .expect("specialized generic routine signature");
    assert_eq!(signature.signatures.len(), 1);
    assert_eq!(
        signature.signatures[0].label,
        "procedure Put(Value: TWidget);"
    );
}

#[test]
fn conditional_navigation_uses_only_a_provably_active_branch() {
    let source = r#"unit ConditionalNavigation;
interface
{$UNDEF OFF}
{$IFDEF OFF}
const Target = 1;
{$ELSE}
const Target = 2;
{$ENDIF}
implementation
procedure Run;
begin
  WriteLn(Target);
end;
end.
"#;
    let source_uri = uri("ConditionalNavigation");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("conditional navigation source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "Target",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &source_uri,
        position_of(source, "Target = 2", 0),
    );
    assert!(
        index
            .navigate(
                &source_uri,
                position_of(source, "Target = 1", 0),
                NavigationTarget::Declaration,
            )
            .is_empty(),
        "known-inactive declarations must not remain visible"
    );
}

#[test]
fn unknown_conditional_alternatives_never_collapse_to_a_unique_binding() {
    let source = r#"unit UnknownConditionalNavigation;
interface
{$IF CompilerVersion >= 24}
const Target = 1;
{$ELSE}
const Target = 2;
{$ENDIF}
implementation
procedure Run;
begin
  WriteLn(Target);
end;
end.
"#;
    let source_uri = uri("UnknownConditionalNavigation");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown conditional source parses");

    assert!(
        locations_at(
            &index,
            &source_uri,
            source,
            "Target",
            2,
            NavigationTarget::Declaration,
        )
        .is_empty(),
        "unknown alternatives must not collapse to a falsely unique target"
    );
}

#[test]
fn conditional_type_annotations_do_not_yield_a_unique_type_definition() {
    let source = r#"unit ConditionalTypeAnnotation;
interface
type TAlpha = Integer; TBeta = String;
var Item:
{$IFDEF MAYBE}
  TAlpha
{$ELSE}
  TBeta
{$ENDIF}
;
implementation
procedure Run;
begin
  WriteLn(Item);
end;
end.
"#;
    let source_uri = uri("ConditionalTypeAnnotation");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("conditional type annotation source parses");

    assert!(
        index
            .type_definitions(&source_uri, position_of(source, "Item", 1))
            .is_empty(),
        "an unknown type annotation must not collapse to one type definition"
    );
}

#[test]
fn direct_unknown_type_definition_does_not_yield_a_unique_type_definition() {
    let source = r#"unit DirectUnknownType;
interface
type
{$IFDEF MAYBE}
  TMaybe = Integer;
{$ENDIF}
var
  Item: TMaybe;
implementation
procedure Run;
begin
  WriteLn(Item);
end;
end.
"#;
    let source_uri = uri("DirectUnknownType");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("direct unknown type source parses");

    assert!(
        index
            .type_definitions(&source_uri, position_of(source, "Item", 1))
            .is_empty(),
        "a directly unknown type definition must not appear uniquely resolvable"
    );
}

#[test]
fn unknown_type_definition_target_does_not_yield_a_unique_type_definition() {
    let source = r#"unit UnknownTypeDefinitionTarget;
interface
{$IFDEF MAYBE}
type TMaybe = Integer;
{$ENDIF}
var Item: TMaybe;
implementation
procedure Run;
begin
  WriteLn(Item);
end;
end.
"#;
    let source_uri = uri("UnknownTypeDefinitionTarget");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("unknown type definition target source parses");

    assert!(
        index
            .type_definitions(&source_uri, position_of(source, "Item", 1))
            .is_empty(),
        "an unknown type definition target must not appear uniquely resolvable"
    );
}

#[test]
fn mixed_known_and_unknown_type_definition_targets_do_not_collapse_to_one() {
    let source = r#"unit MixedTypeDefinitionTargets;
interface
type
  TMaybe = Integer;
{$IFDEF MAYBE}
  TMaybe = String;
{$ENDIF}
var Item: TMaybe;
implementation
procedure Run;
begin
  WriteLn(Item);
end;
end.
"#;
    let source_uri = uri("MixedTypeDefinitionTargets");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("mixed type definition target source parses");

    assert!(
        index
            .type_definitions(&source_uri, position_of(source, "Item", 1))
            .is_empty(),
        "an unknown alternative must not be filtered out of type-definition uniqueness"
    );
}

#[test]
fn qualified_unknown_type_definition_alternatives_do_not_collapse_to_one() {
    let provider = r#"unit Provider;
interface
type
  TMaybe = Integer;
{$IFDEF MAYBE}
  TMaybe = String;
{$ENDIF}
implementation
end.
"#;
    let consumer = r#"unit QualifiedUnknownTypeDefinition;
interface
uses Provider;
var Item: Provider.TMaybe;
implementation
procedure Run;
begin
  WriteLn(Item);
end;
end.
"#;
    let provider_uri = uri("Provider");
    let consumer_uri = uri("QualifiedUnknownTypeDefinition");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri, provider.to_owned())
        .expect("qualified provider source parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("qualified consumer source parses");

    assert!(
        index
            .type_definitions(&consumer_uri, position_of(consumer, "Item", 1))
            .is_empty(),
        "a qualified unknown alternative must not collapse to one type definition"
    );
}

#[test]
fn conditional_receiver_uncertainty_blocks_member_navigation_and_hover() {
    let source = r#"unit ConditionalReceiver;
interface
type
  TKnown = class
    procedure Execute;
  end;
implementation
procedure TKnown.Execute;
begin
end;
procedure Run;
var
{$IFDEF MAYBE}
  Item: TKnown;
{$ENDIF}
begin
  Item.Execute;
end;
end.
"#;
    let source_uri = uri("ConditionalReceiver");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("conditional receiver source parses");

    let member_position = position_of(source, "Execute", 2);
    assert!(
        index
            .navigate(&source_uri, member_position, NavigationTarget::Declaration)
            .is_empty(),
        "an uncertain receiver must not expose a unique member"
    );
    assert!(
        index.hover(&source_uri, member_position).is_none(),
        "an uncertain receiver must not expose member hover"
    );
}

#[test]
fn conditional_elseif_and_ifndef_select_the_only_provable_declaration() {
    let source = r#"unit ConditionalElseIf;
interface
{$UNDEF FIRST}
{$DEFINE SECOND}
{$IFDEF FIRST}
const Target = 1;
 {$ELSEIF DEFINED(SECOND)}
const Target = 2;
{$ELSE}
const Target = 3;
{$ENDIF}
{$IFNDEF FIRST}
const Enabled = 4;
{$ENDIF}
implementation
procedure Run;
begin
  WriteLn(Target, Enabled);
end;
end.
"#;
    let source_uri = uri("ConditionalElseIf");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("conditional elseif source parses");

    let target_locations = locations_at(
        &index,
        &source_uri,
        source,
        "Target",
        3,
        NavigationTarget::Declaration,
    );
    assert_eq!(target_locations.len(), 1);
    assert_location_start(
        &target_locations[0],
        &source_uri,
        position_of(source, "Target = 2", 0),
    );
    let enabled_locations = locations_at(
        &index,
        &source_uri,
        source,
        "Enabled",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(enabled_locations.len(), 1);
    assert_location_start(
        &enabled_locations[0],
        &source_uri,
        position_of(source, "Enabled = 4", 0),
    );
}

#[test]
fn conditional_uses_expose_only_the_provably_active_import() {
    let provider_a = "unit ProviderA;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let provider_b = "unit ProviderB;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let source = r#"unit ConditionalUses;
interface
{$DEFINE USE_A}
uses
  {$IFDEF USE_A}
  ProviderA
  {$ELSE}
  ProviderB
  {$ENDIF};
implementation
procedure Run;
begin
  PublicRoutine;
end;
end.
"#;
    let provider_a_uri = uri("ProviderA");
    let provider_b_uri = uri("ProviderB");
    let source_uri = uri("ConditionalUses");
    let mut index = NavigationIndex::new();
    index
        .update(provider_a_uri.clone(), provider_a.to_owned())
        .expect("provider A parses");
    index
        .update(provider_b_uri, provider_b.to_owned())
        .expect("provider B parses");
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("conditional uses source parses");

    let locations = locations_at(
        &index,
        &source_uri,
        source,
        "PublicRoutine",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_location_start(
        &locations[0],
        &provider_a_uri,
        position_of(provider_a, "PublicRoutine", 0),
    );
}

#[test]
fn active_include_side_effects_poison_later_conditional_facts() {
    let source = r#"unit IncludeSideEffects;
interface
{$DEFINE FEATURE}
{$I Generated.inc}
{$IFDEF FEATURE}
const Target = 1;
{$ELSE}
const Target = 2;
{$ENDIF}
implementation
procedure Run;
begin
  WriteLn(Target);
end;
end.
"#;
    let source_uri = uri("IncludeSideEffects");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("include side-effect source parses");

    assert!(
        locations_at(
            &index,
            &source_uri,
            source,
            "Target",
            2,
            NavigationTarget::Declaration,
        )
        .is_empty(),
        "an active include must not preserve a project fact it may redefine"
    );
}

#[test]
fn malformed_conditional_structure_is_not_used_for_navigation() {
    let source = "unit MalformedConditional;\ninterface\n{$IFDEF OFF}\nconst Target = 1;\nimplementation\nprocedure Run;\nbegin\n  WriteLn(Target);\nend;\nend.\n";
    let source_uri = uri("MalformedConditional");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("malformed conditional source still parses as a tree");

    assert!(
        index
            .navigate(
                &source_uri,
                position_of(source, "Target", 1),
                NavigationTarget::Declaration,
            )
            .is_empty(),
        "unbalanced directives must remain conservative"
    );
}

#[test]
fn namespaced_types_are_not_replaced_by_local_types() {
    let mut index = NavigationIndex::new();
    let caller_uri = uri("NamespacedCaller");
    let types_uri = uri("Ns.U");
    index
        .update(caller_uri.clone(), NAMESPACED_CALLER.to_string())
        .expect("namespaced caller parses");
    index
        .update(types_uri.clone(), NAMESPACED_TYPES.to_string())
        .expect("namespaced types parses");

    let alias_type = locations_at(
        &index,
        &caller_uri,
        NAMESPACED_CALLER,
        "TType",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(alias_type.len(), 1);
    assert_location_start(
        &alias_type[0],
        &types_uri,
        position_of(NAMESPACED_TYPES, "TType", 0),
    );

    let namespace = locations_at(
        &index,
        &caller_uri,
        NAMESPACED_CALLER,
        "Ns",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(namespace.len(), 1);
    assert_location_start(
        &namespace[0],
        &types_uri,
        position_of(NAMESPACED_TYPES, "Ns", 0),
    );

    let qualified_member = locations_at(
        &index,
        &caller_uri,
        NAMESPACED_CALLER,
        "Field",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(qualified_member.len(), 1);
    assert_location_start(
        &qualified_member[0],
        &types_uri,
        position_of(NAMESPACED_TYPES, "Field", 0),
    );

    let typed_member = locations_at(
        &index,
        &caller_uri,
        NAMESPACED_CALLER,
        "Field",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(typed_member.len(), 1);
    assert_location_start(
        &typed_member[0],
        &types_uri,
        position_of(NAMESPACED_TYPES, "Field", 0),
    );

    let unit_member = locations_at(
        &index,
        &caller_uri,
        NAMESPACED_CALLER,
        "P",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(unit_member.len(), 1);
    assert_location_start(
        &unit_member[0],
        &types_uri,
        position_of(NAMESPACED_TYPES, "P", 0),
    );
}

#[test]
fn implementation_local_class_members_are_visible_inside_the_unit() {
    let mut index = NavigationIndex::new();
    let source_uri = uri("LocalClass");
    index
        .update(source_uri.clone(), LOCAL_CLASS.to_string())
        .expect("local class parses");

    let field = locations_at(
        &index,
        &source_uri,
        LOCAL_CLASS,
        "Field",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(field.len(), 1);
    assert_location_start(&field[0], &source_uri, position_of(LOCAL_CLASS, "Field", 0));

    let method = locations_at(
        &index,
        &source_uri,
        LOCAL_CLASS,
        "Work",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(method.len(), 1);
    assert_location_start(&method[0], &source_uri, position_of(LOCAL_CLASS, "Work", 0));

    let parameter = locations_at(
        &index,
        &source_uri,
        LOCAL_CLASS,
        "Value",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(parameter.len(), 1);
    assert_location_start(
        &parameter[0],
        &source_uri,
        position_of(LOCAL_CLASS, "Value", 0),
    );
}

#[test]
fn local_receiver_binding_precedes_a_same_named_unit() {
    let mut index = NavigationIndex::new();
    let caller_uri = uri("QualifiedBindingShadow");
    let unit_uri = uri("U");
    index
        .update(caller_uri.clone(), QUALIFIED_BINDING_SHADOW.to_string())
        .expect("qualified binding shadow parses");
    index
        .update(unit_uri, UNIT_U.to_string())
        .expect("unit U parses");

    let result = locations_at(
        &index,
        &caller_uri,
        QUALIFIED_BINDING_SHADOW,
        "P",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(result.len(), 1);
    assert_location_start(
        &result[0],
        &caller_uri,
        position_of(QUALIFIED_BINDING_SHADOW, "P", 0),
    );
}

#[test]
fn record_members_do_not_leak_into_unqualified_free_procedures() {
    let mut index = NavigationIndex::new();
    let source_uri = uri("RecordLocalScope");
    index
        .update(source_uri.clone(), RECORD_LOCAL_SCOPE.to_string())
        .expect("record local scope parses");

    let result = locations_at(
        &index,
        &source_uri,
        RECORD_LOCAL_SCOPE,
        "Value",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(result.len(), 1);
    assert_location_start(
        &result[0],
        &source_uri,
        position_of(RECORD_LOCAL_SCOPE, "Value", 0),
    );
}

fn run_cyclic_declared_type_repro() {
    let mut index = NavigationIndex::new();
    let source_uri = uri("CyclicDeclaredType");
    index
        .update(source_uri.clone(), CYCLIC_DECLARED_TYPE.to_string())
        .expect("cyclic declared type parses");

    let result = index.navigate(
        &source_uri,
        position_of(CYCLIC_DECLARED_TYPE, "Foo", 0),
        NavigationTarget::Declaration,
    );
    assert!(result.is_empty());
}

#[test]
fn cyclic_declared_type_resolution_does_not_crash() {
    if std::env::var_os("PASCAL_LSP_CYCLIC_TYPE_CHILD").is_some() {
        run_cyclic_declared_type_repro();
        return;
    }

    let output = Command::new(std::env::current_exe().expect("test executable path"))
        .args([
            "--exact",
            "cyclic_declared_type_resolution_child",
            "--nocapture",
        ])
        .env("PASCAL_LSP_CYCLIC_TYPE_CHILD", "1")
        .output()
        .expect("run cyclic type child");
    assert!(
        output.status.success(),
        "cyclic type child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cyclic_declared_type_resolution_child() {
    if std::env::var_os("PASCAL_LSP_CYCLIC_TYPE_CHILD").is_none() {
        return;
    }
    run_cyclic_declared_type_repro();
}

#[test]
fn property_accessors_keep_the_enclosing_class_context() {
    let mut index = NavigationIndex::new();
    let source_uri = uri("PropertyAccessorContext");
    index
        .update(source_uri.clone(), PROPERTY_ACCESSOR_CONTEXT.to_string())
        .expect("property accessor source parses");

    for occurrence in [2, 3] {
        let result = locations_at(
            &index,
            &source_uri,
            PROPERTY_ACCESSOR_CONTEXT,
            "FValue",
            occurrence,
            NavigationTarget::Declaration,
        );
        assert_eq!(result.len(), 1);
        assert_location_start(
            &result[0],
            &source_uri,
            position_of(PROPERTY_ACCESSOR_CONTEXT, "FValue", 1),
        );
    }

    for (name, expected) in [("GetValue", 0), ("SetValue", 0)] {
        let result = locations_at(
            &index,
            &source_uri,
            PROPERTY_ACCESSOR_CONTEXT,
            name,
            1,
            NavigationTarget::Declaration,
        );
        assert_eq!(result.len(), 1);
        assert_location_start(
            &result[0],
            &source_uri,
            position_of(PROPERTY_ACCESSOR_CONTEXT, name, expected),
        );
    }
}

#[test]
fn property_definition_uses_explicit_accessors_and_declaration_stays_property() {
    let mut index = NavigationIndex::new();
    let source_uri = uri("PropertyNavigation");
    index
        .update(source_uri.clone(), PROPERTY_NAVIGATION.to_string())
        .expect("property navigation source parses");

    let field_property = property_position(PROPERTY_NAVIGATION, "FieldDelimiter");
    let declaration = index.navigate(&source_uri, field_property, NavigationTarget::Declaration);
    assert_eq!(declaration.len(), 1);
    assert_location_start(&declaration[0], &source_uri, field_property);

    for target in [
        NavigationTarget::Definition,
        NavigationTarget::Implementation,
    ] {
        let result = index.navigate(&source_uri, field_property, target);
        assert_eq!(result.len(), 1);
        assert_location_start(
            &result[0],
            &source_uri,
            position_of(PROPERTY_NAVIGATION, "fFieldDelimiter: string", 1),
        );
    }

    let getter_property = property_position(PROPERTY_NAVIGATION, "DisplayName");
    let mut getter_body = position_of(PROPERTY_NAVIGATION, "function TConfig.GetDisplayName", 0);
    getter_body.character += "function TConfig.".encode_utf16().count() as u32;
    let getter_definition =
        index.navigate(&source_uri, getter_property, NavigationTarget::Definition);
    assert_eq!(getter_definition.len(), 1);
    assert_location_start(&getter_definition[0], &source_uri, getter_body);

    let setter_property = property_position(PROPERTY_NAVIGATION, "WriteOnly");
    let mut setter_body = position_of(PROPERTY_NAVIGATION, "procedure TConfig.SetWriteOnly", 0);
    setter_body.character += "procedure TConfig.".encode_utf16().count() as u32;
    let setter_definition =
        index.navigate(&source_uri, setter_property, NavigationTarget::Definition);
    assert_eq!(setter_definition.len(), 1);
    assert_location_start(&setter_definition[0], &source_uri, setter_body);

    for property_name in ["Missing", "SelfCycle", "Alias", "InheritedValue"] {
        let property = property_position(PROPERTY_NAVIGATION, property_name);
        let result = index.navigate(&source_uri, property, NavigationTarget::Definition);
        assert_eq!(
            result.len(),
            1,
            "{property_name} should have a safe fallback"
        );
        assert_location_start(&result[0], &source_uri, property);
    }
}

#[test]
fn typed_property_references_keep_cross_unit_accessor_and_declaration_semantics() {
    let mut index = NavigationIndex::new();
    let provider_uri = uri("PropertyNavigation");
    let caller_uri = uri("PropertyCaller");
    index
        .update(provider_uri.clone(), PROPERTY_NAVIGATION.to_string())
        .expect("property provider parses");
    index
        .update(caller_uri.clone(), PROPERTY_CALLER.to_string())
        .expect("property caller parses");

    let usage = position_of(PROPERTY_CALLER, "FieldDelimiter", 0);
    let declaration = index.navigate(&caller_uri, usage, NavigationTarget::Declaration);
    assert_eq!(declaration.len(), 1);
    assert_location_start(
        &declaration[0],
        &provider_uri,
        property_position(PROPERTY_NAVIGATION, "FieldDelimiter"),
    );

    let definition = index.navigate(&caller_uri, usage, NavigationTarget::Definition);
    assert_eq!(definition.len(), 1);
    assert_location_start(
        &definition[0],
        &provider_uri,
        position_of(PROPERTY_NAVIGATION, "fFieldDelimiter: string", 1),
    );
}

#[test]
fn deeply_nested_receiver_expressions_are_bounded() {
    let source = format!(
        "{}{}X{}{}.Next{}",
        RECEIVER_BUDGET_SOURCE_PREFIX,
        "(".repeat(512),
        ")".repeat(512),
        "",
        RECEIVER_BUDGET_SOURCE_SUFFIX,
    );
    let mut index = NavigationIndex::new();
    let source_uri = uri("ReceiverBudget");
    index
        .update(source_uri.clone(), source.clone())
        .expect("receiver budget source parses");

    let result = locations_at(
        &index,
        &source_uri,
        &source,
        "Next",
        0,
        NavigationTarget::Declaration,
    );
    assert!(result.is_empty());
}

#[test]
fn nested_procedures_keep_the_enclosing_method_class_context() {
    let mut index = NavigationIndex::new();
    let source_uri = uri("NestedMethod");
    index
        .update(source_uri.clone(), NESTED_METHOD.to_string())
        .expect("nested method parses");

    let result = locations_at(
        &index,
        &source_uri,
        NESTED_METHOD,
        "FValue",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(result.len(), 1);
    assert_location_start(
        &result[0],
        &source_uri,
        position_of(NESTED_METHOD, "FValue", 0),
    );
}

#[test]
fn self_does_not_expose_method_prototype_parameters_as_members() {
    let mut index = NavigationIndex::new();
    let source_uri = uri("SelfParameter");
    index
        .update(source_uri.clone(), SELF_PARAMETER.to_string())
        .expect("self parameter parses");

    let result = locations_at(
        &index,
        &source_uri,
        SELF_PARAMETER,
        "Arg",
        2,
        NavigationTarget::Declaration,
    );
    assert!(result.is_empty());
}

#[test]
fn unknown_typed_receiver_does_not_fall_back_to_a_unit_name() {
    let mut index = NavigationIndex::new();
    let caller_uri = uri("ReceiverShadow");
    let unit_uri = uri("ReceiverUnit");
    index
        .update(caller_uri.clone(), RECEIVER_SHADOW.to_string())
        .expect("receiver shadow parses");
    index
        .update(unit_uri, RECEIVER_UNIT.to_string())
        .expect("receiver unit parses");

    let result = locations_at(
        &index,
        &caller_uri,
        RECEIVER_SHADOW,
        "Work",
        0,
        NavigationTarget::Declaration,
    );
    assert!(result.is_empty());
}

#[test]
fn unique_abbreviated_body_pairs_and_injects_interface_parameters() {
    let mut index = NavigationIndex::new();
    let source_uri = uri("AbbreviatedUnique");
    index
        .update(source_uri.clone(), ABBREVIATED_UNIQUE.to_string())
        .expect("unique abbreviated source parses");

    let definition = locations_at(
        &index,
        &source_uri,
        ABBREVIATED_UNIQUE,
        "P",
        2,
        NavigationTarget::Definition,
    );
    assert_eq!(definition.len(), 1);
    assert_location_start(
        &definition[0],
        &source_uri,
        position_of(ABBREVIATED_UNIQUE, "P", 1),
    );

    let parameter = locations_at(
        &index,
        &source_uri,
        ABBREVIATED_UNIQUE,
        "X",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(parameter.len(), 1);
    assert_location_start(
        &parameter[0],
        &source_uri,
        position_of(ABBREVIATED_UNIQUE, "X", 0),
    );
}

#[test]
fn ambiguous_abbreviated_body_does_not_guess_an_overload_or_parameter() {
    let mut index = NavigationIndex::new();
    let source_uri = uri("AbbreviatedAmbiguous");
    index
        .update(source_uri.clone(), ABBREVIATED_AMBIGUOUS.to_string())
        .expect("ambiguous abbreviated source parses");

    let definition = locations_at(
        &index,
        &source_uri,
        ABBREVIATED_AMBIGUOUS,
        "P",
        3,
        NavigationTarget::Definition,
    );
    let body_position = position_of(ABBREVIATED_AMBIGUOUS, "P", 2);
    assert!(
        definition
            .iter()
            .all(|location| location.range.start != body_position),
        "ambiguous overload must not select the abbreviated body"
    );

    let parameter = locations_at(
        &index,
        &source_uri,
        ABBREVIATED_AMBIGUOUS,
        "X",
        2,
        NavigationTarget::Declaration,
    );
    assert!(parameter.is_empty());
}

#[test]
fn local_variables_shadow_same_named_symbols_in_each_routine() {
    let mut index = NavigationIndex::new();
    let main_uri = uri("Main");
    let provider_uri = uri("Provider");
    index
        .update(main_uri.clone(), MAIN.to_string())
        .expect("main parses");
    index
        .update(provider_uri, PROVIDER.to_string())
        .expect("provider parses");

    let one = locations_at(
        &index,
        &main_uri,
        MAIN,
        "Shadowed",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(one.len(), 1);
    assert_location_start(&one[0], &main_uri, position_of(MAIN, "Shadowed", 0));

    let two = locations_at(
        &index,
        &main_uri,
        MAIN,
        "Shadowed",
        3,
        NavigationTarget::Declaration,
    );
    assert_eq!(two.len(), 1);
    assert_location_start(&two[0], &main_uri, position_of(MAIN, "Shadowed", 2));
}

#[test]
fn only_interface_and_implementation_uses_are_visible_in_their_scopes() {
    let mut index = NavigationIndex::new();
    let main_uri = uri("Main");
    let provider_uri = uri("Provider");
    let implementation_uri = uri("ImplementationProvider");
    index
        .update(main_uri.clone(), MAIN.to_string())
        .expect("main parses");
    index
        .update(provider_uri.clone(), PROVIDER.to_string())
        .expect("provider parses");
    index
        .update(
            implementation_uri.clone(),
            IMPLEMENTATION_PROVIDER.to_string(),
        )
        .expect("implementation provider parses");

    let interface_type = locations_at(
        &index,
        &main_uri,
        MAIN,
        "TWidget",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(interface_type.len(), 1);
    assert_location_start(
        &interface_type[0],
        &provider_uri,
        position_of(PROVIDER, "TWidget", 0),
    );

    let implementation_type = locations_at(
        &index,
        &main_uri,
        MAIN,
        "TImplementationWidget",
        0,
        NavigationTarget::Declaration,
    );
    assert!(implementation_type.is_empty());

    let implementation_routine = locations_at(
        &index,
        &main_uri,
        MAIN,
        "ImplementationRoutine",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(implementation_routine.len(), 1);
    assert_location_start(
        &implementation_routine[0],
        &implementation_uri,
        position_of(IMPLEMENTATION_PROVIDER, "ImplementationRoutine", 0),
    );

    let unknown_receiver = locations_at(
        &index,
        &main_uri,
        MAIN,
        "DoThing",
        2,
        NavigationTarget::Declaration,
    );
    assert!(unknown_receiver.is_empty());
}

#[test]
fn uses_unit_and_case_insensitive_navigation_are_supported() {
    let mut index = NavigationIndex::new();
    let main_uri = uri("Main");
    let provider_uri = uri("Provider");
    index
        .update(main_uri.clone(), MAIN.to_string())
        .expect("main parses");
    index
        .update(provider_uri.clone(), PROVIDER.to_string())
        .expect("provider parses");

    let uses = locations_at(
        &index,
        &main_uri,
        MAIN,
        "Provider",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(uses.len(), 1);
    assert_location_start(
        &uses[0],
        &provider_uri,
        position_of(PROVIDER, "Provider", 0),
    );

    let case_insensitive = locations_at(
        &index,
        &main_uri,
        MAIN,
        "pUbLiCrOuTiNe",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(case_insensitive.len(), 1);
}

#[test]
fn overload_navigation_selects_a_known_argument_candidate() {
    let mut index = NavigationIndex::new();
    let main_uri = uri("Main");
    let provider_uri = uri("Provider");
    index
        .update(main_uri.clone(), MAIN.to_string())
        .expect("main parses");
    index
        .update(provider_uri.clone(), PROVIDER.to_string())
        .expect("provider parses");

    let candidates = locations_at(
        &index,
        &main_uri,
        MAIN,
        "Overloaded",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(candidates.len(), 1);
    assert_location_start(
        &candidates[0],
        &provider_uri,
        position_of(PROVIDER, "Overloaded(Value: Integer)", 0),
    );

    let unknown_uri = uri("UnknownOverloadCall");
    let unknown_source = r#"unit UnknownOverloadCall;
interface
uses Provider;
implementation
procedure Run;
begin
  Overloaded(UnknownValue);
end;
end.
"#;
    index
        .update(unknown_uri.clone(), unknown_source.to_owned())
        .expect("unknown overload source parses");
    let unknown_candidates = locations_at(
        &index,
        &unknown_uri,
        unknown_source,
        "Overloaded",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(unknown_candidates.len(), 2);
    assert!(
        unknown_candidates
            .iter()
            .all(|location| location.uri == provider_uri)
    );
}

#[test]
fn comments_and_strings_are_not_navigation_references() {
    let mut index = NavigationIndex::new();
    let main_uri = uri("Main");
    let provider_uri = uri("Provider");
    index
        .update(main_uri.clone(), MAIN.to_string())
        .expect("main parses");
    index
        .update(provider_uri, PROVIDER.to_string())
        .expect("provider parses");

    let source = "unit Main; interface uses Provider; implementation\n".to_string()
        + "procedure Run; begin\n"
        + "  // PublicRoutine\n"
        + "  WriteLn('PublicRoutine');\n"
        + "end; end.\n";
    index
        .update(main_uri.clone(), source.clone())
        .expect("comment/string source parses");

    let comment = locations_at(
        &index,
        &main_uri,
        &source,
        "PublicRoutine",
        0,
        NavigationTarget::Declaration,
    );
    assert!(comment.is_empty());

    let string = locations_at(
        &index,
        &main_uri,
        &source,
        "PublicRoutine",
        1,
        NavigationTarget::Declaration,
    );
    assert!(string.is_empty());
}

#[test]
fn qualified_type_and_unit_members_navigate_without_global_matching() {
    let mut index = NavigationIndex::new();
    let main_uri = uri("QualifiedMain");
    let provider_uri = uri("Provider");
    index
        .update(main_uri.clone(), QUALIFIED_MAIN.to_string())
        .expect("qualified caller parses");
    index
        .update(provider_uri.clone(), PROVIDER.to_string())
        .expect("provider parses");

    let type_member = locations_at(
        &index,
        &main_uri,
        QUALIFIED_MAIN,
        "DoThing",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(type_member.len(), 1);
    assert_location_start(
        &type_member[0],
        &provider_uri,
        position_of(PROVIDER, "DoThing", 0),
    );

    let unit_member = locations_at(
        &index,
        &main_uri,
        QUALIFIED_MAIN,
        "PublicRoutine",
        0,
        NavigationTarget::Definition,
    );
    assert_eq!(unit_member.len(), 1);
    assert_location_start(
        &unit_member[0],
        &provider_uri,
        position_of(PROVIDER, "PublicRoutine", 1),
    );
}

#[test]
fn an_indexed_but_unused_unit_does_not_supply_free_routines() {
    let mut index = NavigationIndex::new();
    let caller_uri = uri("UnrelatedCaller");
    let provider_uri = uri("UnrelatedProvider");
    index
        .update(caller_uri.clone(), UNRELATED_CALLER.to_string())
        .expect("unrelated caller parses");
    index
        .update(provider_uri, UNRELATED_PROVIDER.to_string())
        .expect("unrelated provider parses");

    let result = locations_at(
        &index,
        &caller_uri,
        UNRELATED_CALLER,
        "UnrelatedRoutine",
        0,
        NavigationTarget::Declaration,
    );
    assert!(result.is_empty());

    let qualified_result = locations_at(
        &index,
        &caller_uri,
        UNRELATED_CALLER,
        "UnrelatedRoutine",
        1,
        NavigationTarget::Declaration,
    );
    assert!(qualified_result.is_empty());
}

#[test]
fn implementation_uses_do_not_expose_private_provider_routines() {
    let mut index = NavigationIndex::new();
    let caller_uri = uri("PrivateCaller");
    let provider_uri = uri("PrivateProvider");
    index
        .update(caller_uri.clone(), PRIVATE_CALLER.to_string())
        .expect("private caller parses");
    index
        .update(provider_uri, PRIVATE_PROVIDER.to_string())
        .expect("private provider parses");

    let result = locations_at(
        &index,
        &caller_uri,
        PRIVATE_CALLER,
        "HiddenRoutine",
        0,
        NavigationTarget::Declaration,
    );
    assert!(result.is_empty());
}

#[test]
fn parameters_shadow_unit_variables_inside_their_routine() {
    let mut index = NavigationIndex::new();
    let source_uri = uri("ParameterScope");
    index
        .update(source_uri.clone(), PARAMETER_SCOPE.to_string())
        .expect("parameter scope parses");

    let result = locations_at(
        &index,
        &source_uri,
        PARAMETER_SCOPE,
        "Value",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(result.len(), 1);
    assert_location_start(
        &result[0],
        &source_uri,
        position_of(PARAMETER_SCOPE, "Value", 1),
    );
}

#[test]
fn all_identifiers_in_a_parameter_name_list_are_indexed() {
    let mut index = NavigationIndex::new();
    let source_uri = uri("MultiParameterScope");
    index
        .update(source_uri.clone(), MULTI_PARAMETER_SCOPE.to_string())
        .expect("multiple parameter scope parses");

    let result = locations_at(
        &index,
        &source_uri,
        MULTI_PARAMETER_SCOPE,
        "B",
        2,
        NavigationTarget::Declaration,
    );
    assert_eq!(result.len(), 1);
    assert_location_start(
        &result[0],
        &source_uri,
        position_of(MULTI_PARAMETER_SCOPE, "B", 1),
    );
}

#[test]
fn nested_routines_do_not_share_direct_navigation_with_global_routines() {
    let mut index = NavigationIndex::new();
    let source_uri = uri("NestedRoutineScope");
    index
        .update(source_uri.clone(), NESTED_ROUTINE_SCOPE.to_string())
        .expect("nested routine scope parses");

    let result = locations_at(
        &index,
        &source_uri,
        NESTED_ROUTINE_SCOPE,
        "Helper",
        1,
        NavigationTarget::Declaration,
    );
    assert_eq!(result.len(), 1);
    assert_location_start(
        &result[0],
        &source_uri,
        position_of(NESTED_ROUTINE_SCOPE, "Helper", 1),
    );
}

#[test]
fn update_replaces_old_symbols_and_remove_clears_a_document() {
    let mut index = NavigationIndex::new();
    let main_uri = uri("Main");
    let provider_uri = uri("Provider");
    index
        .update(main_uri.clone(), MAIN.to_string())
        .expect("main parses");
    index
        .update(provider_uri.clone(), PROVIDER.to_string())
        .expect("provider parses");

    let before_remove = locations_at(
        &index,
        &main_uri,
        MAIN,
        "PublicRoutine",
        0,
        NavigationTarget::Declaration,
    );
    assert_eq!(before_remove.len(), 1);

    index.remove(&provider_uri);
    assert!(
        locations_at(
            &index,
            &main_uri,
            MAIN,
            "PublicRoutine",
            0,
            NavigationTarget::Declaration,
        )
        .is_empty()
    );

    index
        .update(provider_uri, PROVIDER.to_string())
        .expect("provider re-adds");
    let old_call_position = position_of(MAIN, "PublicRoutine", 0);
    let changed = MAIN.replace("PublicRoutine;", "RenamedRoutine;");
    index
        .update(main_uri.clone(), changed.clone())
        .expect("updated main parses");
    assert!(
        index
            .navigate(&main_uri, old_call_position, NavigationTarget::Declaration)
            .is_empty()
    );
}

#[test]
fn incomplete_source_does_not_panic_or_return_stale_symbols() {
    let mut index = NavigationIndex::new();
    let main_uri = uri("Main");
    let initial = "unit Main; interface procedure Run; implementation procedure Run; begin";
    index
        .update(main_uri.clone(), initial.to_string())
        .expect("tree-sitter returns a recovery tree");
    let changed = "unit Main; interface procedure Other; begin";
    index
        .update(main_uri.clone(), changed.to_string())
        .expect("short recovery source parses");
    let current = index.navigate(
        &main_uri,
        position_of(changed, "Other", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(current.len(), 1);
    assert_location_start(&current[0], &main_uri, position_of(changed, "Other", 0));
}

#[test]
fn text_positions_use_utf16_and_treat_crlf_as_one_line_break() {
    let source = "a😀b\r\né\nlast";
    assert_eq!(
        text::position_to_offset(source, Position::new(0, 1)),
        Some(1)
    );
    assert_eq!(
        text::position_to_offset(source, Position::new(0, 3)),
        Some(5)
    );
    assert_eq!(
        text::position_to_offset(source, Position::new(1, 0)),
        Some(8)
    );
    assert_eq!(
        text::offset_to_position(source, 5),
        Some(Position::new(0, 3))
    );
    assert_eq!(
        text::offset_to_position(source, 8),
        Some(Position::new(1, 0))
    );
    assert_eq!(
        text::position_to_offset(source, Position::new(0, 4)),
        Some(6)
    );
    assert_eq!(text::offset_to_position(source, 7), None);
    assert_eq!(text::position_to_offset(source, Position::new(0, 2)), None);
    assert_eq!(
        text::position_to_offset(source, Position::new(0, 100)),
        None
    );
    assert_eq!(text::position_to_offset(source, Position::new(10, 0)), None);
    assert_eq!(text::offset_to_position(source, 2), None);
    assert_eq!(
        text::offset_to_position(source, source.len()),
        Some(Position::new(2, 4))
    );

    assert_eq!(text::position_to_offset("", Position::new(0, 0)), Some(0));
    assert_eq!(text::offset_to_position("", 0), Some(Position::new(0, 0)));
    assert_eq!(text::position_to_offset("", Position::new(1, 0)), None);
    assert_eq!(text::offset_to_position("", 1), None);
}

#[test]
fn document_symbols_follow_declaration_ranges_and_containment() {
    let source = "unit Outline;\ninterface\ntype\n  TRecord = record\n    Value: Integer;\n  end;\n  TWidget = class\n  private\n    FValue: Integer;\n  public\n    property Value: Integer read FValue;\n    procedure Run(A: Integer);\n  end;\nvar\n  Global: Integer;\nimplementation\nprocedure TWidget.Run(A: Integer);\nvar\n  Local: Integer;\nbegin\n  Local := A;\nend;\nend.\n";
    let source_uri = uri("Outline");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_string())
        .expect("outline source parses");

    let symbols = index
        .document_symbols(&source_uri)
        .expect("document symbols are available");
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0].name, "Outline");
    let root_children = symbols[0].children.as_ref().expect("unit children");
    assert_eq!(
        root_children
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>(),
        ["TRecord", "TWidget", "Global", "Run"]
    );

    let record = &root_children[0];
    assert_eq!(record.kind, lsp_types::SymbolKind::STRUCT);
    assert_eq!(record.selection_range.start, Position::new(3, 2));
    assert_eq!(record.range.end, Position::new(5, 6));
    assert_eq!(
        record
            .children
            .as_ref()
            .expect("record field")
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>(),
        ["Value"]
    );

    let widget = &root_children[1];
    assert_eq!(widget.kind, lsp_types::SymbolKind::CLASS);
    assert_eq!(widget.range.start, Position::new(6, 2));
    assert_eq!(widget.range.end, Position::new(12, 6));
    assert_eq!(
        widget
            .children
            .as_ref()
            .expect("class members")
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.kind))
            .collect::<Vec<_>>(),
        [
            ("FValue", lsp_types::SymbolKind::FIELD),
            ("Value", lsp_types::SymbolKind::PROPERTY),
            ("Run", lsp_types::SymbolKind::METHOD),
        ]
    );
    assert_eq!(
        widget.children.as_ref().unwrap()[2].range.start,
        Position::new(11, 4)
    );
    assert_eq!(
        widget.children.as_ref().unwrap()[2].range.end,
        Position::new(11, 30)
    );

    let implementation = &root_children[3];
    assert_eq!(implementation.kind, lsp_types::SymbolKind::METHOD);
    assert_eq!(implementation.range.start, Position::new(16, 0));
    assert_eq!(implementation.range.end, Position::new(21, 4));
    assert_eq!(
        implementation
            .children
            .as_ref()
            .expect("local variable")
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>(),
        ["Local"]
    );
}

#[test]
fn workspace_symbol_result_limit_is_an_error() {
    let mut source = String::from("unit Many;\ninterface\nvar\n");
    for index in 0..10_000 {
        source.push_str(&format!("  Symbol{index}: Integer;\n"));
    }
    source.push_str("implementation\nend.\n");

    let source_uri = uri("Many");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri, source)
        .expect("large symbol source parses");
    let error = index
        .workspace_symbols("")
        .expect_err("result cap must not silently truncate");
    assert_eq!(
        error,
        "workspace symbol query returned more than 10000 results; narrow the query"
    );
}

#[test]
fn workspace_symbols_keep_overloads_and_use_container_labels() {
    let provider_uri = uri("Provider");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri, PROVIDER.to_string())
        .expect("provider parses");

    let symbols = index
        .workspace_symbols("LOADED")
        .expect("workspace symbols are available");
    assert_eq!(symbols.len(), 4);
    assert!(symbols.iter().all(|symbol| symbol.name == "Overloaded"));
    assert!(
        symbols
            .iter()
            .all(|symbol| symbol.container_name.as_deref() == Some("Provider"))
    );
    assert!(
        symbols
            .windows(2)
            .all(|pair| pair[0].location.range.start <= pair[1].location.range.start)
    );
}

#[test]
fn workspace_symbols_keep_names_with_a_shared_declaration_range() {
    let source_uri = uri("Grouped");
    let source = "unit Grouped;\ninterface\nvar\n  Alpha, Beta: Integer;\nimplementation\nend.\n";
    let mut index = NavigationIndex::new();
    index
        .update(source_uri, source.to_string())
        .expect("grouped declarations parse");

    let symbols = index
        .workspace_symbols("a")
        .expect("grouped declarations are searchable");
    assert_eq!(
        symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>(),
        ["Alpha", "Beta"]
    );
}

#[test]
fn type_aliases_use_array_and_function_symbol_kinds() {
    let source_uri = uri("TypeAliases");
    let source = "unit TypeAliases;\ninterface\ntype\n  TArray = array[0..1] of Integer;\n  TProc = procedure(A: Integer);\nimplementation\nend.\n";
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_string())
        .expect("type aliases parse");

    let symbols = index
        .document_symbols(&source_uri)
        .expect("type aliases are outlined");
    let children = symbols[0].children.as_ref().expect("unit children");
    assert_eq!(children[0].name, "TArray");
    assert_eq!(children[0].kind, lsp_types::SymbolKind::ARRAY);
    assert_eq!(children[1].name, "TProc");
    assert_eq!(children[1].kind, lsp_types::SymbolKind::FUNCTION);
}

#[test]
fn workspace_symbol_filtering_happens_before_the_response_bound() {
    let mut source = String::from("unit Many;\ninterface\nvar\n");
    for index in 0..=10_000 {
        source.push_str(&format!("  Symbol{index}: Integer;\n"));
    }
    source.push_str("implementation\nend.\n");

    let source_uri = uri("ManyFiltered");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri, source)
        .expect("large symbol source parses");
    let symbols = index
        .workspace_symbols("Symbol10000")
        .expect("a narrow query stays below the response bound");
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0].name, "Symbol10000");
}

#[test]
fn workspace_symbols_accept_exactly_the_response_bound_including_unit() {
    let mut source = String::from("unit Exact;\ninterface\nvar\n");
    for index in 0..9_999 {
        source.push_str(&format!("  Symbol{index}: Integer;\n"));
    }
    source.push_str("implementation\nend.\n");

    let source_uri = uri("Exact");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri, source)
        .expect("exact-bound source parses");
    assert_eq!(
        index
            .workspace_symbols("")
            .expect("exactly the response bound is valid")
            .len(),
        10_000
    );
}

#[test]
fn document_symbols_report_response_bound_exhaustion() {
    let mut source = String::from("unit ManyDocumentSymbols;\ninterface\nvar\n");
    for index in 0..10_000 {
        source.push_str(&format!("  Symbol{index}: Integer;\n"));
    }
    source.push_str("implementation\nend.\n");

    let source_uri = uri("ManyDocumentSymbols");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source)
        .expect("large document-symbol source parses");
    let error = index
        .document_symbols(&source_uri)
        .expect_err("document symbol results must not silently truncate");
    assert!(error.contains("10000"));
    assert!(error.contains("document symbol"));
}

fn document_symbol_count(symbols: &[lsp_types::DocumentSymbol]) -> usize {
    let mut pending = symbols.iter().collect::<Vec<_>>();
    let mut count = 0;
    while let Some(symbol) = pending.pop() {
        count += 1;
        if let Some(children) = symbol.children.as_deref() {
            pending.extend(children);
        }
    }
    count
}

#[test]
fn document_symbols_accept_exactly_the_response_bound_including_unit() {
    let mut source = String::from("unit ExactDocumentSymbols;\ninterface\nvar\n");
    for index in 0..9_999 {
        source.push_str(&format!("  Symbol{index}: Integer;\n"));
    }
    source.push_str("implementation\nend.\n");

    let source_uri = uri("ExactDocumentSymbols");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source)
        .expect("exact-bound document source parses");
    let symbols = index
        .document_symbols(&source_uri)
        .expect("exactly the document response bound is valid");
    assert_eq!(document_symbol_count(&symbols), 10_000);
}

#[test]
fn document_symbol_selection_ranges_count_non_bmp_utf16_units() {
    let source_uri = uri("UnicodeSymbols");
    let source = "unit UnicodeSymbols;\ninterface\nconst {😀} Value = 1;\nimplementation\nend.\n";
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_string())
        .expect("unicode symbol source parses");

    let symbols = index
        .document_symbols(&source_uri)
        .expect("unicode symbols are available");
    let value = symbols[0]
        .children
        .as_ref()
        .expect("unit children")
        .iter()
        .find(|symbol| symbol.name == "Value")
        .expect("Value symbol");
    assert_eq!(value.selection_range.start, Position::new(2, 11));
    assert_eq!(value.selection_range.end, Position::new(2, 16));
}

#[test]
fn document_symbols_survive_parser_recovery_after_valid_declarations() {
    let source_uri = uri("RecoveredSymbols");
    let source = "unit RecoveredSymbols;\ninterface\ntype\n  TWidget = class\n    Value: Integer;\n  end;\nimplementation\nprocedure VisibleThing;\nbegin\n  Value := 1;\n";
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_string())
        .expect("recoverable source parses");

    let symbols = index
        .document_symbols(&source_uri)
        .expect("recovered symbols are available");
    let names = symbols[0]
        .children
        .as_ref()
        .expect("recovered unit children")
        .iter()
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"TWidget"));
    assert!(names.contains(&"VisibleThing"));
}

#[test]
fn folding_ranges_cover_multiline_pascal_constructs_without_single_line_noise() {
    let source_uri = uri("FoldingRanges");
    let source = "unit FoldingRanges;\ninterface\ntype\n  TRecord = record\n    Value: Integer;\n  end;\n  TWidget = class\n  public\n    procedure Run;\n  end;\nimplementation\nprocedure TWidget.Run;\nbegin\n  if True then\n  begin\n    while True do\n    begin\n    end;\n  end;\nend;\nend.\n";
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("folding source parses");

    let ranges = index
        .folding_ranges(&source_uri)
        .expect("folding ranges are available");
    let spans = ranges
        .iter()
        .map(|range| (range.start_line, range.end_line))
        .collect::<Vec<_>>();
    assert!(
        spans.contains(&(3, 5)),
        "record declaration is foldable: {spans:?}"
    );
    assert!(
        spans.contains(&(6, 9)),
        "class declaration is foldable: {spans:?}"
    );
    assert!(
        spans.contains(&(11, 19)),
        "routine declaration is foldable: {spans:?}"
    );
    assert!(
        spans.contains(&(12, 19)),
        "outer begin block is foldable: {spans:?}"
    );
    assert!(spans.contains(&(13, 18)), "if block is foldable: {spans:?}");
    assert!(
        spans.contains(&(15, 17)),
        "while block is foldable: {spans:?}"
    );
    assert!(
        !spans.contains(&(8, 8)),
        "single-line declarations are not foldable: {spans:?}"
    );
}

#[test]
fn folding_ranges_include_nested_regions_and_multiline_comments_only() {
    let source_uri = uri("FoldingCommentsAndRegions");
    let source = "unit FoldingCommentsAndRegions;\ninterface\nimplementation\nconst Text = '{$REGION not-a-region}';\n{comment text\n  continues}\n{$REGION Outer}\n{$REGION Inner}\nprocedure Run;\nbegin\nend;\n{$ENDREGION}\n{$ENDREGION}\nend.\n";
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("comment and region source parses");

    let ranges = index
        .folding_ranges(&source_uri)
        .expect("comment and region ranges are available");
    let comments = ranges
        .iter()
        .filter(|range| range.kind == Some(lsp_types::FoldingRangeKind::Comment))
        .collect::<Vec<_>>();
    let regions = ranges
        .iter()
        .filter(|range| range.kind == Some(lsp_types::FoldingRangeKind::Region))
        .collect::<Vec<_>>();
    assert_eq!(comments.len(), 1, "only the real comment folds: {ranges:?}");
    assert_eq!(comments[0].start_line, 4);
    assert_eq!(comments[0].end_line, 5);
    assert_eq!(
        regions.len(),
        2,
        "nested regions must both fold: {ranges:?}"
    );
    assert!(
        regions
            .iter()
            .any(|range| (range.start_line, range.end_line) == (6, 12)),
        "outer region range missing: {regions:?}"
    );
    assert!(
        regions
            .iter()
            .any(|range| (range.start_line, range.end_line) == (7, 11)),
        "inner region range missing: {regions:?}"
    );
}

#[test]
fn folding_ranges_tag_multiline_uses_as_imports() {
    let source_uri = uri("FoldingImports");
    let source = "unit FoldingImports;\ninterface\nuses\n  FirstUnit,\n  SecondUnit;\nimplementation\nend.\n";
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("uses source parses");

    let ranges = index
        .folding_ranges(&source_uri)
        .expect("import folding ranges are available");
    assert!(
        ranges.iter().any(|range| {
            range.kind == Some(lsp_types::FoldingRangeKind::Imports)
                && (range.start_line, range.end_line) == (2, 4)
        }),
        "multiline uses range missing: {ranges:?}"
    );
}

#[test]
fn folding_ranges_preserve_crlf_and_utf16_positions() {
    let source_uri = uri("FoldingUtf16");
    let source =
        "unit FoldingUtf16;\r\ninterface\r\nimplementation\r\n{\r\n  body\r\n  😀}\r\nend.\r\n";
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("CRLF comment source parses");

    let ranges = index
        .folding_ranges(&source_uri)
        .expect("CRLF comment ranges are available");
    let comment = ranges
        .iter()
        .find(|range| range.kind == Some(lsp_types::FoldingRangeKind::Comment))
        .expect("multiline comment range");
    assert_eq!(
        (
            comment.start_line,
            comment.start_character,
            comment.end_line,
            comment.end_character,
        ),
        (3, Some(0), 5, Some(5))
    );
}

#[test]
fn folding_ranges_cover_try_and_case_blocks() {
    let source_uri = uri("FoldingTryCase");
    let source = "unit FoldingTryCase;\ninterface\nimplementation\nprocedure Run;\nbegin\n  try\n    case Value of\n      1:\n      begin\n      end;\n    else\n      Value := 2;\n    end;\n  finally\n    Value := 3;\n  end;\nend;\nend.\n";
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("try/case source parses");

    let ranges = index
        .folding_ranges(&source_uri)
        .expect("try/case ranges are available");
    let spans = ranges
        .iter()
        .map(|range| (range.start_line, range.end_line))
        .collect::<Vec<_>>();
    assert!(spans.contains(&(5, 15)), "try block is foldable: {spans:?}");
    assert!(
        spans.contains(&(6, 12)),
        "case block is foldable: {spans:?}"
    );
    assert!(
        spans.contains(&(8, 9)),
        "case arm block is foldable: {spans:?}"
    );
}

#[test]
fn folding_ranges_do_not_cross_inactive_or_unknown_conditional_text() {
    let source_uri = uri("FoldingConditionals");
    let source = "unit FoldingConditionals;\ninterface\nimplementation\n{$IFDEF HIDDEN}\nprocedure Hidden;\nbegin\nend;\n{$ENDIF}\nprocedure Visible;\nbegin\nend;\nend.\n";
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("conditional source parses");

    let ranges = index
        .folding_ranges(&source_uri)
        .expect("conditional folding ranges are available");
    assert!(
        ranges.iter().all(|range| range.start_line >= 8),
        "inactive procedure must not produce a range: {ranges:?}"
    );
    assert!(
        ranges
            .iter()
            .any(|range| (range.start_line, range.end_line) == (8, 10)),
        "active procedure remains foldable: {ranges:?}"
    );

    let malformed_uri = uri("FoldingMalformedConditional");
    let malformed = "unit FoldingMalformedConditional;\ninterface\nimplementation\n{$IFDEF HIDDEN}\nprocedure Hidden;\nbegin\nend;\n";
    index
        .update(malformed_uri.clone(), malformed.to_owned())
        .expect("malformed conditional source parses");
    assert!(
        index
            .folding_ranges(&malformed_uri)
            .expect("malformed conditional ranges are available")
            .is_empty(),
        "malformed conditional text must not create speculative ranges"
    );
}

#[test]
fn folding_ranges_include_unit_lifecycle_sections_and_ignore_unmatched_regions() {
    let source_uri = uri("FoldingSections");
    let source = "unit FoldingSections;\ninterface\nimplementation\ninitialization\n  StartUp;\nfinalization\n  ShutDown;\nend.\n";
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("lifecycle source parses");
    let ranges = index
        .folding_ranges(&source_uri)
        .expect("lifecycle ranges are available");
    let spans = ranges
        .iter()
        .map(|range| (range.start_line, range.end_line))
        .collect::<Vec<_>>();
    assert!(
        spans.contains(&(3, 4)),
        "initialization section is foldable: {spans:?}"
    );
    assert!(
        spans.contains(&(5, 6)),
        "finalization section is foldable: {spans:?}"
    );

    let malformed_uri = uri("FoldingUnmatchedRegion");
    let malformed = "unit FoldingUnmatchedRegion;\ninterface\nimplementation\n{$REGION never-closed}\nprocedure Run;\nbegin\nend;\nend.\n";
    index
        .update(malformed_uri.clone(), malformed.to_owned())
        .expect("unmatched region source parses");
    assert!(
        index
            .folding_ranges(&malformed_uri)
            .expect("unmatched region ranges are available")
            .iter()
            .all(|range| range.kind != Some(lsp_types::FoldingRangeKind::Region)),
        "unmatched regions must not produce speculative ranges"
    );
}

#[test]
fn folding_ranges_reject_excessive_syntax_depth() {
    let source_uri = uri("FoldingDepth");
    let depth = 160;
    let mut source =
        String::from("unit FoldingDepth;\ninterface\nimplementation\nprocedure Run;\nbegin\n");
    for _ in 0..depth {
        source.push_str("if True then begin\n");
    }
    for _ in 0..depth {
        source.push_str("end;\n");
    }
    source.push_str("end;\nend.\n");

    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source)
        .expect("deep folding source parses");
    let error = index
        .folding_ranges(&source_uri)
        .expect_err("excessive folding depth must fail closed");
    assert!(
        error.contains("folding syntax hierarchy"),
        "unexpected folding depth error: {error}"
    );
}

fn deeply_nested_symbol_source(depth: usize) -> String {
    let mut source = String::from("unit DeepSymbols;\ninterface\nimplementation\nprocedure P0;\n");
    for index in 1..depth {
        source.push_str(&format!("{}procedure P{index};\n", "  ".repeat(index)));
    }
    for index in (1..depth).rev() {
        let indent = "  ".repeat(index);
        source.push_str(&format!("{indent}begin\n{indent}end;\n"));
    }
    source.push_str("begin\nend;\nend.\n");
    source
}

#[test]
fn document_symbols_reject_hierarchy_beyond_the_modest_depth_bound() {
    let source_uri = uri("DeepSymbols");
    let source = deeply_nested_symbol_source(40);
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source)
        .expect("deep symbol source parses");

    let error = index
        .document_symbols(&source_uri)
        .expect_err("deep document symbol hierarchy must fail closed");
    assert!(error.contains("hierarchy"));
    assert!(error.contains("32"));
}

#[test]
fn workspace_symbol_queries_do_not_inherit_document_hierarchy_depth() {
    let source_uri = uri("DeepWorkspaceSymbols");
    let source = deeply_nested_symbol_source(40);
    let mut index = NavigationIndex::new();
    index
        .update(source_uri, source)
        .expect("deep symbol source parses");

    let symbols = index
        .workspace_symbols("P39")
        .expect("flat workspace symbols do not need a hierarchy");
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0].name, "P39");
}

#[test]
fn deeply_nested_expression_does_not_overflow_navigation_or_parser_traversal() {
    let mut source = String::from(
        "unit DeepExpression;\ninterface\nimplementation\nprocedure Run;\nbegin\n  X := 1",
    );
    for _ in 1..10_000 {
        source.push_str(" + 1");
    }
    source.push_str(";\nend;\nend.\n");
    assert!(source.len() >= 40_000);

    let mut index = NavigationIndex::new();
    let source_uri = uri("DeepExpression");
    index
        .update(source_uri.clone(), source.clone())
        .expect("deep expression parses without overflowing");
    assert!(
        index
            .navigate(
                &source_uri,
                position_of(&source, "X", 0),
                NavigationTarget::Declaration,
            )
            .is_empty()
    );
}

#[test]
fn imports_are_exposed_and_empty_workspace_bindings_block_global_unit_matches() {
    let mut index = NavigationIndex::new();
    let caller_uri = uri("BindingCaller");
    let provider_uri = uri("BindingProvider");
    let caller = "unit BindingCaller;\ninterface\nuses BindingProvider;\nimplementation\nprocedure Run; begin end;\nend.\n";
    let provider = "unit BindingProvider;\ninterface\nprocedure Work;\nimplementation\nprocedure Work; begin end;\nend.\n";
    index
        .update(caller_uri.clone(), caller.to_string())
        .expect("caller parses");
    index
        .update(provider_uri.clone(), provider.to_string())
        .expect("provider parses");

    let imports = index.imports(&caller_uri);
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].name, "bindingprovider");
    assert!(imports[0].span.start < imports[0].span.end);

    index.bind_imports(&caller_uri, HashMap::new());
    assert!(
        index
            .navigate(
                &caller_uri,
                position_of(caller, "BindingProvider", 0),
                NavigationTarget::Declaration,
            )
            .is_empty(),
        "an explicitly empty workspace binding must not fall back to the global unit index"
    );

    let mut bindings = HashMap::new();
    bindings.insert("BindingProvider".to_string(), provider_uri.clone());
    index.bind_imports(&caller_uri, bindings);
    assert_eq!(
        index
            .navigate(
                &caller_uri,
                position_of(caller, "BindingProvider", 0),
                NavigationTarget::Declaration,
            )
            .first()
            .map(|location| &location.uri),
        Some(&provider_uri)
    );
}

#[test]
fn project_navigation_does_not_probe_unrelated_explicit_references() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let provider = root.join("Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  ProviderRoutine;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    fs::create_dir_all(&root).expect("create workspace");
    fs::write(&main, main_source).expect("write main");
    fs::write(&provider, provider_source).expect("write provider");
    let mut references = String::new();
    for index in 0..12 {
        let name = format!("Unrelated{index}.pas");
        fs::write(
            root.join(&name),
            format!("unit Unrelated{index}; interface implementation end.\n"),
        )
        .expect("write unrelated reference");
        references.push_str(&format!("<DCCReference Include=\"{name}\" />"));
    }
    fs::write(
        root.join("App.dproj"),
        format!(
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup><ItemGroup>{references}</ItemGroup></Project>"
        ),
    )
    .expect("write project");
    fs::write(root.join("App.dpr"), "program App; begin end.\n").expect("write main project");

    let main_uri = Url::from_file_path(&main).expect("main URI");
    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
    let locations = workspace.navigate(
        &main_uri,
        position_of(main_source, "ProviderRoutine", 0),
        NavigationTarget::Declaration,
    );

    assert_eq!(locations.len(), 1);
    assert_eq!(
        locations[0].uri,
        Url::from_file_path(&provider).expect("provider URI")
    );
    assert_eq!(
        workspace.parsed_document_count(),
        2,
        "only the requested source and its matching dependency should be parsed"
    );
}

#[test]
fn local_navigation_does_not_revalidate_unrelated_disk_cache() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let root = temp.path().join("workspace");
    let main = root.join("Main.pas");
    let unrelated = root.join("Unrelated.pas");
    let main_source = "unit Main;\ninterface\nprocedure MainRoutine;\nimplementation\nprocedure MainRoutine; begin end;\nend.\n";
    let unrelated_source = "unit Unrelated;\ninterface\nprocedure UnrelatedRoutine;\nimplementation\nprocedure UnrelatedRoutine; begin end;\nend.\n";
    fs::create_dir_all(&root).expect("create workspace");
    fs::write(&main, main_source).expect("write main");
    fs::write(&unrelated, unrelated_source).expect("write unrelated");

    let main_uri = Url::from_file_path(&main).expect("main URI");
    let unrelated_uri = Url::from_file_path(&unrelated).expect("unrelated URI");
    let mut workspace = test_workspace(vec![root], WorkspaceOptions::default());
    assert_eq!(
        workspace
            .navigate(
                &unrelated_uri,
                position_of(unrelated_source, "UnrelatedRoutine", 1),
                NavigationTarget::Declaration,
            )
            .len(),
        1
    );
    assert_eq!(
        workspace
            .navigate(
                &main_uri,
                position_of(main_source, "MainRoutine", 1),
                NavigationTarget::Declaration,
            )
            .len(),
        1
    );
    fs::remove_file(&unrelated).expect("delete unrelated source");

    workspace.refresh_for_navigation();
    assert_eq!(
        workspace.parsed_document_count(),
        2,
        "a compatibility refresh must not revalidate unrelated cached files"
    );
    assert_eq!(
        workspace
            .navigate(
                &main_uri,
                position_of(main_source, "MainRoutine", 1),
                NavigationTarget::Declaration,
            )
            .len(),
        1
    );
}

#[test]
fn binding_locations_exclude_all_routine_declaration_sites() {
    let source = "unit RoutineReferences;\ninterface\nprocedure Work;\nimplementation\nprocedure Work;\nbegin\n  Work;\nend;\nend.\n";
    let source_uri = uri("RoutineReferences");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_string())
        .expect("routine reference source parses");

    let without_declaration = index
        .binding_locations(&source_uri, position_of(source, "Work", 0), false)
        .expect("bound routine references are available");
    assert_eq!(without_declaration.len(), 1);
    assert_eq!(
        without_declaration[0].range.start,
        position_of(source, "Work", 2)
    );

    let with_declaration = index
        .binding_locations(&source_uri, position_of(source, "Work", 0), true)
        .expect("bound routine declarations are available");
    assert_eq!(with_declaration.len(), 3);
}

#[test]
fn binding_locations_ignore_comments_strings_and_whitespace() {
    let source = "unit IgnoredReferences;\ninterface\nconst Value = 1;\nimplementation\nprocedure Run;\nbegin\n  // Value\n  Log('Value');\n  Log(Value);\nend;\nend.\n";
    let source_uri = uri("IgnoredReferences");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_string())
        .expect("ignored reference source parses");

    assert!(
        index
            .binding_locations(&source_uri, position_of(source, "Value", 1), true,)
            .expect("comment position is queryable")
            .is_empty()
    );
    assert!(
        index
            .binding_locations(&source_uri, position_of(source, "Value", 2), true,)
            .expect("string position is queryable")
            .is_empty()
    );
    assert!(
        index
            .binding_locations(&source_uri, Position::new(5, 5), true)
            .expect("whitespace position is queryable")
            .is_empty()
    );
}

#[test]
fn binding_locations_respect_lexical_shadowing() {
    let source = "unit ShadowedReferences;\ninterface\nconst Value = 1;\nprocedure Run;\nimplementation\nprocedure Run;\nvar\n  Value: Integer;\nbegin\n  Value := 2;\nend;\nprocedure Other;\nbegin\n  Value := 3;\nend;\nend.\n";
    let source_uri = uri("ShadowedReferences");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_string())
        .expect("shadowing source parses");

    let global_locations = index
        .binding_locations(&source_uri, position_of(source, "Value", 0), true)
        .expect("global binding is resolved");
    assert_eq!(global_locations.len(), 2);
    assert!(
        global_locations
            .iter()
            .all(|location| { location.range.start.line == 2 || location.range.start.line == 13 })
    );

    let local_locations = index
        .binding_locations(&source_uri, position_of(source, "Value", 1), true)
        .expect("local binding is resolved");
    assert_eq!(local_locations.len(), 2);
    assert!(
        local_locations
            .iter()
            .all(|location| { location.range.start.line == 7 || location.range.start.line == 9 })
    );
}

#[test]
fn binding_locations_convert_non_bmp_prefixes_to_utf16() {
    let source = "unit UnicodeReferences;\ninterface\nconst Value = 1;\nimplementation\nprocedure Run;\nbegin\n  Log('😀', Value);\nend;\nend.\n";
    let source_uri = uri("UnicodeReferences");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_string())
        .expect("unicode reference source parses");

    let locations = index
        .binding_locations(&source_uri, position_of(source, "Value", 0), true)
        .expect("unicode binding is resolved");
    let use_location = locations
        .iter()
        .find(|location| location.range.start.line == 6)
        .expect("unicode use location");
    assert_eq!(use_location.range.start.character, 12);
    assert_eq!(use_location.range.end.character, 17);
}

#[test]
fn binding_locations_preserve_positions_across_large_ascii_and_unicode_consumers() {
    let provider =
        "unit PositionProvider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n";
    let mut ascii_consumer = String::from(
        "unit AsciiConsumer;\ninterface\nuses PositionProvider;\nimplementation\nprocedure Run;\nbegin\n",
    );
    for _ in 0..2_000 {
        ascii_consumer.push_str("  Log(SharedValue);\n");
    }
    ascii_consumer.push_str("end;\nend.\n");

    let mut unicode_consumer = String::from(
        "unit UnicodeConsumer;\ninterface\nuses PositionProvider;\nimplementation\nprocedure Run;\nbegin\n",
    );
    for _ in 0..2_000 {
        unicode_consumer.push_str("  Log('😀', SharedValue);\n");
    }
    unicode_consumer.push_str("end;\nend.\n");

    let provider_uri = uri("PositionProvider");
    let ascii_uri = uri("AsciiConsumer");
    let unicode_uri = uri("UnicodeConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("provider parses");
    index
        .update(ascii_uri.clone(), ascii_consumer.clone())
        .expect("ASCII consumer parses");
    index
        .update(unicode_uri.clone(), unicode_consumer.clone())
        .expect("Unicode consumer parses");

    let locations = index
        .binding_locations(
            &provider_uri,
            position_of(provider, "SharedValue", 0),
            false,
        )
        .expect("large multi-file reference query resolves");
    assert_eq!(locations.len(), 4_000);

    let ascii_locations = locations
        .iter()
        .filter(|location| location.uri == ascii_uri)
        .collect::<Vec<_>>();
    assert_eq!(ascii_locations.len(), 2_000);
    assert_eq!(
        ascii_locations.first().map(|location| location.range.start),
        Some(position_of(&ascii_consumer, "SharedValue", 0))
    );
    assert_eq!(
        ascii_locations.last().map(|location| location.range.start),
        Some(position_of(&ascii_consumer, "SharedValue", 1_999))
    );

    let unicode_locations = locations
        .iter()
        .filter(|location| location.uri == unicode_uri)
        .collect::<Vec<_>>();
    assert_eq!(unicode_locations.len(), 2_000);
    assert_eq!(
        unicode_locations
            .first()
            .map(|location| location.range.start),
        Some(position_of(&unicode_consumer, "SharedValue", 0))
    );
    assert_eq!(
        unicode_locations.first().map(|location| location.range.end),
        Some(Position::new(
            position_of(&unicode_consumer, "SharedValue", 0).line,
            position_of(&unicode_consumer, "SharedValue", 0)
                .character
                .saturating_add("SharedValue".encode_utf16().count() as u32),
        ))
    );
}

#[test]
fn binding_locations_do_not_merge_distinct_overloads() {
    let source = "unit OverloadReferences;\ninterface\nprocedure Overloaded(Value: Integer); overload;\nprocedure Overloaded(Value: string); overload;\nimplementation\nprocedure Overloaded(Value: Integer);\nbegin\nend;\nprocedure Overloaded(Value: string);\nbegin\nend;\nprocedure Run;\nbegin\n  Overloaded(1);\n  Overloaded('text');\nend;\nend.\n";
    let source_uri = uri("OverloadReferences");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_string())
        .expect("overload source parses");

    for occurrence in [4, 5] {
        let error = index
            .binding_locations(
                &source_uri,
                position_of(source, "Overloaded", occurrence),
                true,
            )
            .expect_err("unproven overload calls must fail closed");
        assert!(
            error.contains("ambiguous") || error.contains("unresolved"),
            "unexpected overload error: {error}"
        );
    }
}

fn hover_text(hover: &lsp_types::Hover) -> String {
    match &hover.contents {
        HoverContents::Markup(content) => content.value.clone(),
        HoverContents::Scalar(MarkedString::String(value)) => value.clone(),
        HoverContents::Scalar(MarkedString::LanguageString(value)) => value.value.clone(),
        HoverContents::Array(values) => values
            .iter()
            .map(|value| match value {
                MarkedString::String(value) => value.clone(),
                MarkedString::LanguageString(value) => value.value.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[test]
fn hover_projects_bound_declarations_without_bodies_or_false_receiver_fallbacks() {
    let provider = r#"unit HoverProvider;
interface
type
  TWidget = class
  private
    FValue: Integer;
  public
    property Value: Integer read FValue;
  end;
procedure Overloaded(Value: Integer); overload;
procedure Overloaded(Value: string); overload;
implementation
procedure Overloaded(Value: Integer);
begin
end;
procedure Overloaded(Value: string);
begin
end;
end.
"#;
    let consumer = r#"unit HoverConsumer;
interface
uses HoverProvider;
implementation
procedure Run;
var
  Widget: TWidget;
  Value: Integer;
  Qualified: HoverProvider.TWidget;
begin
  Log('😀', Widget.Value);
  Overloaded(1);
  Value := 1;
  // Overloaded
  WriteLn('Overloaded');
  Unknown.Value;
end;
end.
"#;
    let provider_uri = uri("HoverProvider");
    let consumer_uri = uri("HoverConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_string())
        .expect("hover provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_string())
        .expect("hover consumer parses");

    let property_position = position_of(consumer, "Value", 1);
    let property = index
        .hover(&consumer_uri, property_position)
        .expect("property hover");
    assert_eq!(
        property.range,
        Some(Range::new(
            property_position,
            Position::new(property_position.line, property_position.character + 5),
        ))
    );
    let property_text = hover_text(&property);
    assert!(property_text.contains("property Value: Integer read FValue"));
    assert!(!property_text.contains("procedure Overloaded"));

    let overload = index
        .hover(&consumer_uri, position_of(consumer, "Overloaded", 0))
        .expect("overload hover");
    let overload_text = hover_text(&overload);
    assert_eq!(
        overload_text
            .matches("procedure Overloaded(Value: Integer);")
            .count(),
        1
    );
    assert_eq!(
        overload_text
            .matches("procedure Overloaded(Value: string);")
            .count(),
        1
    );
    assert!(!overload_text.contains("begin\nend;"));

    let shadowed = index
        .hover(&consumer_uri, position_of(consumer, "Value", 2))
        .expect("local shadow hover");
    assert!(hover_text(&shadowed).contains("Value: Integer"));

    let qualified_type = index
        .hover(&consumer_uri, position_of(consumer, "TWidget", 1))
        .expect("qualified imported type hover");
    assert!(hover_text(&qualified_type).contains("TWidget = class"));
    assert!(hover_text(&qualified_type).contains("HoverProvider"));

    assert!(
        index
            .hover(&consumer_uri, position_of(consumer, "Value", 3))
            .is_none(),
        "unknown receivers must not fall back to a same-named declaration"
    );

    let comment_position = position_of(consumer, "Overloaded", 1);
    assert!(
        index.hover(&consumer_uri, comment_position).is_none(),
        "comments must not produce hover content"
    );
    assert!(
        index
            .hover(&consumer_uri, position_of(consumer, "Overloaded", 2))
            .is_none(),
        "strings must not produce hover content"
    );
}

#[test]
fn hover_includes_adjacent_xml_documentation_for_the_resolved_declaration() {
    let source = r#"unit DocumentationHover;
interface
/// <summary>Returns <c>the value</c> for <paramref name="Name"/>.</summary>
function ValueFor(Name: string): Integer;
implementation
function ValueFor(Name: string): Integer;
begin
  Result := 1;
end;
end.
"#;
    let source_uri = uri("DocumentationHover");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("documentation source parses");

    let hover = index
        .hover(&source_uri, position_of(source, "ValueFor", 0))
        .expect("documented declaration hover");

    assert_eq!(
        hover_text(&hover),
        "`DocumentationHover`\n\n```pascal\nfunction ValueFor(Name: string): Integer;\n```\n\nReturns `the value` for `Name`."
    );
}

#[test]
fn completion_includes_markdown_documentation_for_the_resolved_declaration() {
    let source = r#"unit DocumentationCompletion;
interface
/// <summary>Returns <c>the value</c>.</summary>
function ValueFor(Name: string): Integer;
procedure Caller;
implementation
function ValueFor(Name: string): Integer;
begin
  Result := 1;
end;
procedure Caller;
begin
  Val
end;
end.
"#;
    let source_uri = uri("DocumentationCompletion");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("completion documentation source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "  Val", 0))
        .expect("documented completion");
    let item = completion
        .items
        .iter()
        .find(|item| item.label == "ValueFor")
        .expect("documented function completion item");

    assert_eq!(
        item.documentation,
        Some(LspDocumentation::MarkupContent(lsp_types::MarkupContent {
            kind: MarkupKind::Markdown,
            value: "Returns `the value`.".to_owned(),
        }))
    );
}

#[test]
fn signature_help_includes_markdown_summary_and_parameter_documentation() {
    let source = r#"unit DocumentationSignature;
interface
/// <summary>Returns <c>the value</c>.</summary>
/// <param name="Name">The lookup name.</param>
/// <returns>The integer result.</returns>
function ValueFor(Name: string): Integer;
procedure Caller;
implementation
function ValueFor(Name: string): Integer;
begin
  Result := 1;
end;
procedure Caller;
begin
  ValueFor('text' );
end;
end.
"#;
    let source_uri = uri("DocumentationSignature");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("signature documentation source parses");

    let signature = index
        .signature_help(&source_uri, position_after(source, "ValueFor('text' ", 0))
        .expect("documented signature help")
        .expect("documented signature");
    assert_eq!(signature.signatures.len(), 1);
    assert_eq!(
        signature.signatures[0].documentation,
        Some(LspDocumentation::MarkupContent(lsp_types::MarkupContent {
            kind: MarkupKind::Markdown,
            value: "Returns `the value`.\n\n**Returns**\n\nThe integer result.".to_owned(),
        }))
    );
    assert_eq!(
        signature.signatures[0].parameters.as_ref().unwrap()[0].documentation,
        Some(LspDocumentation::MarkupContent(lsp_types::MarkupContent {
            kind: MarkupKind::Markdown,
            value: "The lookup name.".to_owned(),
        }))
    );
}

#[test]
fn documentation_pairs_declarations_and_definitions_without_crossing_overloads() {
    let source = r#"unit DocumentationPairing;
interface
/// <summary>Integer declaration.</summary>
function Pick(Value: Integer): Integer; overload;
function Fallback(Value: Integer): Integer;
/// <summary>String declaration.</summary>
function Pick(Value: string): string; overload;
implementation
/// <summary>Integer implementation.</summary>
function Pick(Value: Integer): Integer;
begin
  Result := Value;
end;
/// <summary>Fallback implementation.</summary>
function Fallback(Value: Integer): Integer;
begin
  Result := Value;
end;
/// <summary>String implementation.</summary>
function Pick(Value: string): string;
begin
  Result := Value;
end;
procedure Caller;
begin
  Pick(1);
  Fallback(1);
end;
end.
"#;
    let source_uri = uri("DocumentationPairing");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("documentation pairing source parses");

    let pick = index
        .hover(&source_uri, position_of(source, "Pick(1)", 0))
        .expect("overload documentation hover");
    let pick_text = hover_text(&pick);
    assert!(pick_text.contains("Integer declaration."), "{pick_text}");
    assert!(pick_text.contains("String declaration."), "{pick_text}");
    assert!(
        !pick_text.contains("Integer implementation."),
        "{pick_text}"
    );
    assert!(!pick_text.contains("String implementation."), "{pick_text}");

    let fallback = index
        .hover(&source_uri, position_of(source, "Fallback(1)", 0))
        .expect("implementation fallback hover");
    let fallback_text = hover_text(&fallback);
    assert!(
        fallback_text.contains("Fallback implementation."),
        "{fallback_text}"
    );
}

#[test]
fn documentation_attaches_to_types_fields_and_properties() {
    let source = r#"unit DocumentationMembers;
interface
type
  /// <summary>Widget type.</summary>
  TWidget = class
    /// <summary>Stored field.</summary>
    Field: Integer;
    /// <summary>Visible property.</summary>
    property Name: string;
  end;
procedure Caller;
implementation
procedure Caller;
var
  Widget: TWidget;
begin
  Widget.Field := 1;
  Widget.Name := 'widget';
end;
end.
"#;
    let source_uri = uri("DocumentationMembers");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("documented members source parses");

    let type_hover = index
        .hover(&source_uri, position_of(source, "TWidget", 1))
        .expect("type documentation hover");
    assert!(hover_text(&type_hover).contains("Widget type."));

    let field_hover = index
        .hover(&source_uri, position_of(source, "Field", 1))
        .expect("field documentation hover");
    assert!(hover_text(&field_hover).contains("Stored field."));

    let property_hover = index
        .hover(&source_uri, position_of(source, "Name", 1))
        .expect("property documentation hover");
    assert!(hover_text(&property_hover).contains("Visible property."));
}

#[test]
fn documentation_pairing_keeps_generic_and_nongeneric_overloads_distinct() {
    let source = r#"unit GenericDocumentationPairing;
interface
function Pick: Integer; overload;
function Pick<T>: T; overload;
implementation
/// <summary>Generic implementation.</summary>
function Pick<T>: T;
begin
  Result := Default(T);
end;
procedure Caller;
begin
  Pick<Integer>;
end;
end.
"#;
    let source_uri = uri("GenericDocumentationPairing");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic documentation pairing source parses");

    let generic_hover = index
        .hover(&source_uri, position_of(source, "Pick<T>", 0))
        .expect("generic documentation hover");
    let generic_text = hover_text(&generic_hover);
    assert!(
        generic_text.contains("Generic implementation."),
        "{generic_text}"
    );
    assert!(!generic_text.contains("Non-generic"), "{generic_text}");

    let nongeneric_hover = index
        .hover(&source_uri, position_of(source, "Pick: Integer", 0))
        .expect("non-generic documentation hover");
    let nongeneric_text = hover_text(&nongeneric_hover);
    assert!(
        !nongeneric_text.contains("Generic implementation."),
        "{nongeneric_text}"
    );
}

#[test]
fn hover_fails_closed_for_unqualified_routines_in_unknown_class_ancestors() {
    let source = r#"unit InheritedRoutine;
interface
type
  TParent = class(TMissing)
    procedure Reset(X: Integer);
  end;
  TChild = class(TParent)
    procedure LocalRun;
    procedure Run;
  end;
procedure Reset(S: string);
implementation
procedure Reset(S: string);
begin
end;
procedure TParent.Reset(X: Integer);
begin
end;
procedure TChild.LocalRun;
var
  Reset: Integer;
begin
  Reset := 123;
end;
procedure TChild.Run;
begin
  Reset(123);
end;
end.
"#;
    let mut index = NavigationIndex::new();
    let source_uri = uri("InheritedRoutine");
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("inherited routine source parses");

    assert!(
        index
            .hover(&source_uri, position_of(source, "Reset(123)", 0))
            .is_none(),
        "an unresolved inherited routine must not resolve to a global routine"
    );
    assert!(
        index
            .navigate(
                &source_uri,
                position_of(source, "Reset(123)", 0),
                NavigationTarget::Definition,
            )
            .is_empty(),
        "shared binding must not navigate to an unrelated global routine"
    );

    let local = index
        .hover(&source_uri, position_of(source, "Reset := 123", 0))
        .expect("local shadow hover");
    assert!(hover_text(&local).contains("Reset: Integer"));

    let imported_source = r#"unit ImportedConsumer;
interface
uses ImportedProvider;
type
  TChild = class(TMissing)
    procedure Run;
  end;
implementation
procedure TChild.Run;
begin
  Reset(123);
end;
end.
"#;
    let imported_provider = r#"unit ImportedProvider;
interface
procedure Reset(S: string);
implementation
procedure Reset(S: string);
begin
end;
end.
"#;
    let imported_consumer_uri = uri("ImportedConsumer");
    let imported_provider_uri = uri("ImportedProvider");
    let mut imported_index = NavigationIndex::new();
    imported_index
        .update(imported_consumer_uri.clone(), imported_source.to_owned())
        .expect("imported consumer parses");
    imported_index
        .update(imported_provider_uri.clone(), imported_provider.to_owned())
        .expect("imported provider parses");
    imported_index.bind_imports(
        &imported_consumer_uri,
        [("ImportedProvider".to_owned(), imported_provider_uri)],
    );
    assert!(
        imported_index
            .hover(
                &imported_consumer_uri,
                position_of(imported_source, "Reset(123)", 0)
            )
            .is_none(),
        "an imported global must not satisfy unresolved inherited lookup"
    );
}

#[test]
fn hover_bounds_empty_structured_type_excerpts_to_the_type_node() {
    let source = "unit EmptyType; interface type TEmptyClass = class end; const ClassValue = 42; TEmptyRecord = record end; const RecordValue = 43; TForward = class; const ForwardValue = 44; implementation procedure Surprise; begin WriteLn(123); end; end.\n";
    let mut index = NavigationIndex::new();
    let source_uri = uri("EmptyType");
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("empty structured type source parses");

    for (type_name, occurrence) in [("TEmptyClass", 0), ("TEmptyRecord", 0), ("TForward", 0)] {
        let hover = index
            .hover(&source_uri, position_of(source, type_name, occurrence))
            .unwrap_or_else(|| panic!("missing hover for {type_name}"));
        let text = hover_text(&hover);
        assert!(!text.contains("ClassValue"), "{type_name}: {text}");
        assert!(!text.contains("RecordValue"), "{type_name}: {text}");
        assert!(!text.contains("ForwardValue"), "{type_name}: {text}");
        assert!(!text.contains("Surprise"), "{type_name}: {text}");
        assert!(!text.contains("WriteLn"), "{type_name}: {text}");
    }
}

#[test]
fn hover_preserves_literals_and_line_comment_boundaries_in_headers() {
    let source = r#"unit HeaderTokens;
interface
procedure Run(S: string = 'a  b');
procedure Multi(
  A: Integer; // first parameter
  B: string);
implementation
end.
"#;
    let mut index = NavigationIndex::new();
    let source_uri = uri("HeaderTokens");
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("header token source parses");

    let run = index
        .hover(&source_uri, position_of(source, "Run", 0))
        .expect("default-value hover");
    let run_text = hover_text(&run);
    assert!(run_text.contains("'a  b'"), "{run_text}");

    let multi = index
        .hover(&source_uri, position_of(source, "Multi", 0))
        .expect("multiline-header hover");
    let multi_text = hover_text(&multi);
    assert!(
        multi_text.contains("// first parameter\n  B: string"),
        "{multi_text}"
    );
}

#[test]
fn hover_projects_synthetic_abbreviated_parameters_from_their_declaration() {
    let source = r#"unit AbbreviatedHover;
interface
procedure Run(const Arg: Integer);
procedure Grouped(const First, Second: Integer; var LabelValue: string);
implementation
procedure Run;
begin
  WriteLn(Arg);
end;
procedure Grouped;
begin
  WriteLn(First);
end;
end.
"#;
    let mut index = NavigationIndex::new();
    let source_uri = uri("AbbreviatedHover");
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("abbreviated parameter source parses");

    let hover = index
        .hover(&source_uri, position_of(source, "Arg", 1))
        .expect("abbreviated parameter hover");
    assert!(hover_text(&hover).contains("const Arg: Integer"));

    let grouped = index
        .hover(&source_uri, position_of(source, "First", 1))
        .expect("grouped abbreviated parameter hover");
    assert!(hover_text(&grouped).contains("const First, Second: Integer"));
}

fn assert_exact_type_location(
    actual: &[Location],
    expected_uri: &Url,
    expected_source: &str,
    needle: &str,
    occurrence: usize,
) {
    let start = position_of(expected_source, needle, occurrence);
    let end = Position::new(
        start.line,
        start.character + needle.encode_utf16().count() as u32,
    );
    assert_eq!(
        actual,
        &[Location {
            uri: expected_uri.clone(),
            range: Range::new(start, end),
        }]
    );
}

fn final_qualified_type_position(source: &str, qualified_name: &str) -> Position {
    let mut position = position_of(source, qualified_name, 0);
    let prefix = qualified_name
        .rsplit_once('.')
        .map_or(0, |(prefix, _)| prefix.len() + 1);
    position.character += prefix as u32;
    position
}

#[test]
fn type_definitions_resolve_named_source_types_without_unsafe_fallbacks() {
    let other = r#"unit Other;
interface
type
  TFoo = class
    Value: Integer;
  end;
  TAlias = TFoo;
var
  Item: Integer;
implementation
end.
"#;
    let consumer = r#"unit TypeDefinitionConsumer;
interface
uses Other;
type
  TLocal = record
    Value: Integer;
  end;
  TAlias = Other.TFoo;
  TCycleA = TCycleB;
  TCycleB = TCycleA;
  TContainer = class
    Field: TFoo;
    property Prop: TFoo read Field;
    procedure Method(Param: TFoo);
  end;
implementation
procedure TContainer.Method(Param: TFoo);
var
  Item: TFoo;
  AliasItem: TAlias;
  QualifiedItem: Other.TFoo;
  LocalItem: TLocal;
  CycleItem: TCycleA;
  Primitive: Integer;
  UnknownItem: MissingType;
begin
  Item.Value;
  AliasItem.Value;
  QualifiedItem.Value;
  LocalItem.Value;
  CycleItem;
  Primitive;
  UnknownItem;
  Param.Value;
  Self.Field.Value;
  Self.Prop.Value;
end;
end.
"#;
    let other_uri = uri("Other");
    let consumer_uri = uri("TypeDefinitionConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(other_uri.clone(), other.to_owned())
        .expect("type provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("type consumer parses");

    let local_variable =
        index.type_definitions(&consumer_uri, position_of(consumer, "Item: TFoo", 0));
    assert_exact_type_location(&local_variable, &other_uri, other, "TFoo", 0);

    let local_variable_use =
        index.type_definitions(&consumer_uri, position_of(consumer, "Item.Value", 0));
    assert_exact_type_location(&local_variable_use, &other_uri, other, "TFoo", 0);

    let parameter = index.type_definitions(&consumer_uri, position_of(consumer, "Param: TFoo", 0));
    assert_exact_type_location(&parameter, &other_uri, other, "TFoo", 0);

    let field = index.type_definitions(&consumer_uri, position_of(consumer, "Field: TFoo", 0));
    assert_exact_type_location(&field, &other_uri, other, "TFoo", 0);

    let property = index.type_definitions(&consumer_uri, position_of(consumer, "Prop: TFoo", 0));
    assert_exact_type_location(&property, &other_uri, other, "TFoo", 0);

    let qualified_variable = index.type_definitions(
        &consumer_uri,
        position_of(consumer, "QualifiedItem: Other.TFoo", 0),
    );
    assert_exact_type_location(&qualified_variable, &other_uri, other, "TFoo", 0);

    let qualified_type = index.type_definitions(
        &consumer_uri,
        final_qualified_type_position(consumer, "Other.TFoo"),
    );
    assert_exact_type_location(&qualified_type, &other_uri, other, "TFoo", 0);

    let mut unqualified_type_position = position_of(consumer, "Param: TFoo", 0);
    unqualified_type_position.character += "Param: ".encode_utf16().count() as u32;
    let unqualified_type = index.type_definitions(&consumer_uri, unqualified_type_position);
    assert_exact_type_location(&unqualified_type, &other_uri, other, "TFoo", 0);

    let direct_type = index.type_definitions(
        &consumer_uri,
        position_of(consumer, "TAlias = Other.TFoo", 0),
    );
    assert_exact_type_location(&direct_type, &consumer_uri, consumer, "TAlias", 0);

    let alias_variable =
        index.type_definitions(&consumer_uri, position_of(consumer, "AliasItem: TAlias", 0));
    assert_exact_type_location(&alias_variable, &consumer_uri, consumer, "TAlias", 0);

    let cycle = index.type_definitions(
        &consumer_uri,
        position_of(consumer, "CycleItem: TCycleA", 0),
    );
    assert_exact_type_location(&cycle, &consumer_uri, consumer, "TCycleA", 0);

    let shadowed_global =
        index.type_definitions(&consumer_uri, position_of(consumer, "Item.Value", 0));
    assert_eq!(shadowed_global.len(), 1);
    assert_eq!(shadowed_global[0].uri, other_uri);

    for position in [
        position_of(consumer, "Primitive: Integer", 0),
        position_of(consumer, "UnknownItem: MissingType", 0),
        position_of(consumer, "MissingType", 0),
    ] {
        assert!(
            index.type_definitions(&consumer_uri, position).is_empty(),
            "primitive and unresolved named types must not produce a target"
        );
    }
}

#[test]
fn type_definitions_reject_anonymous_types_but_keep_named_type_aliases() {
    let source = r#"unit AnonymousTypeDefinition;
interface
type
  TFoo = class end;
  TArrayAlias = array of TFoo;
  TPointerAlias = ^TFoo;
implementation
procedure Run;
var
  Direct: TFoo;
  Many: array of TFoo;
  Ptr: ^TFoo;
  NamedMany: TArrayAlias;
  NamedPtr: TPointerAlias;
begin
  Direct := nil;
  Many := nil;
  Ptr := nil;
  NamedMany := nil;
  NamedPtr := nil;
end;
end.
"#;
    let source_uri = uri("AnonymousTypeDefinition");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("anonymous type source parses");

    let direct = index.type_definitions(&source_uri, position_of(source, "Direct: TFoo", 0));
    assert_exact_type_location(&direct, &source_uri, source, "TFoo", 0);

    for declaration in ["Many: array of TFoo", "Ptr: ^TFoo"] {
        assert!(
            index
                .type_definitions(&source_uri, position_of(source, declaration, 0))
                .is_empty(),
            "anonymous type {declaration:?} must not navigate to a contained identifier"
        );
    }

    let named_array = index.type_definitions(
        &source_uri,
        position_of(source, "NamedMany: TArrayAlias", 0),
    );
    assert_exact_type_location(&named_array, &source_uri, source, "TArrayAlias", 0);

    let named_pointer = index.type_definitions(
        &source_uri,
        position_of(source, "NamedPtr: TPointerAlias", 0),
    );
    assert_exact_type_location(&named_pointer, &source_uri, source, "TPointerAlias", 0);
}

#[test]
fn type_definitions_qualified_names_ignore_unrelated_routine_local_types() {
    let source = r#"unit QualifiedTypeShadow;
interface
type
  TFoo = class end;
implementation
procedure Hidden;
type
  TFoo = record end;
begin
end;
procedure Run;
var
  Direct: TFoo;
  Qualified: QualifiedTypeShadow.TFoo;
begin
  Direct := nil;
  Qualified := nil;
end;
end.
"#;
    let source_uri = uri("QualifiedTypeShadow");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("qualified shadow source parses");

    let direct = index.type_definitions(&source_uri, position_of(source, "Direct: TFoo", 0));
    assert_exact_type_location(&direct, &source_uri, source, "TFoo", 0);

    let qualified_variable = index.type_definitions(
        &source_uri,
        position_of(source, "Qualified: QualifiedTypeShadow.TFoo", 0),
    );
    assert_exact_type_location(&qualified_variable, &source_uri, source, "TFoo", 0);

    let qualified_type = index.type_definitions(
        &source_uri,
        final_qualified_type_position(source, "QualifiedTypeShadow.TFoo"),
    );
    assert_exact_type_location(&qualified_type, &source_uri, source, "TFoo", 0);
}

#[test]
fn accessibility_is_shared_across_navigation_assistance_and_type_definition() {
    let provider = r#"unit AccessibilityProvider;
interface
type
  TPayload = class
  end;
  TBase = class
    DefaultField: Integer;
    procedure Check;
  private
    PrivateField: TPayload;
    procedure PrivateMethod(Value: Integer);
  protected
    ProtectedField: Integer;
    procedure ProtectedMethod(Value: Integer);
  strict private
    StrictPrivateSlot: Integer;
    procedure StrictPrivateCall(Value: Integer);
  strict protected
    StrictProtectedSlot: Integer;
    procedure StrictProtectedCall(Value: Integer);
  public
    PublicField: Integer;
    procedure PublicMethod(Value: Integer);
  published
    PublishedField: Integer;
  end;
  TRecord = record
    RecordField: Integer;
  end;
  IContract = interface
    procedure InterfaceMethod;
  end;
implementation
procedure TBase.Check;
var
  Obj: TBase;
begin
  Self.PrivateField;
  Self.ProtectedField;
  Self.StrictPrivateSlot;
  Self.StrictProtectedSlot;
end;
procedure SameUnit;
var
  Obj: TBase;
begin
  Obj.PrivateField;
  Obj.ProtectedField;
  Obj.StrictPrivateSlot;
  Obj.StrictProtectedSlot;
end;
end.
"#;
    let consumer = r#"unit AccessibilityConsumer;
interface
uses AccessibilityProvider;
type
  TChild = class(AccessibilityProvider.TBase)
    procedure Check;
  end;
  TUnrelated = class
    procedure Check;
  end;
implementation
procedure TChild.Check;
var
  Obj: TBase;
  R: TRecord;
  Contract: IContract;
begin
  Obj.ProtectedField;
  Obj.StrictProtectedSlot;
  Obj.PrivateField;
  Obj.StrictPrivateSlot;
  Obj.DefaultField;
  Obj.PublicField;
  Obj.PublishedField;
  Obj.ProtectedMethod(1);
  Obj.StrictProtectedCall(1);
  Obj.PrivateMethod(1);
  Obj.StrictPrivateCall(1);
  R.RecordField;
  Contract.InterfaceMethod;
end;
procedure TUnrelated.Check;
var
  Obj: TBase;
begin
  Obj.ProtectedField;
  Obj.StrictProtectedSlot;
  Obj.PrivateField;
  Obj.StrictPrivateSlot;
  Obj.DefaultField;
  Obj.PublicField;
  Obj.PublishedField;
end;
end.
"#;
    let provider_uri = uri("AccessibilityProvider");
    let consumer_uri = uri("AccessibilityConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri.clone(), provider.to_owned())
        .expect("accessibility provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("accessibility consumer parses");

    let assert_access = |source_uri: &Url,
                         source: &str,
                         member: &str,
                         member_occurrence: usize,
                         completion_prefix: &str,
                         completion_occurrence: usize,
                         accessible: bool| {
        let position = position_of(source, member, member_occurrence);
        let navigation = index.navigate(source_uri, position, NavigationTarget::Declaration);
        assert_eq!(
            navigation.len(),
            usize::from(accessible),
            "unexpected navigation for {member} at {source_uri}: {navigation:?}"
        );
        if accessible {
            assert_location_start(
                &navigation[0],
                &provider_uri,
                position_of(provider, member, 0),
            );
        }

        let completion = index
            .completion(
                source_uri,
                position_after(source, completion_prefix, completion_occurrence),
            )
            .expect("accessibility completion");
        let labels = completion
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();
        if accessible {
            assert_eq!(labels, [member], "unexpected completion for {member}");
        } else {
            assert!(
                labels.is_empty(),
                "inaccessible member {member} leaked into completion: {labels:?}"
            );
        }
    };

    // A declaring class can use every class member, while another routine in
    // the same unit gets ordinary private/protected access but not strict access.
    assert_access(
        &provider_uri,
        provider,
        "PrivateField",
        1,
        "Self.PrivateF",
        0,
        true,
    );
    assert_access(
        &provider_uri,
        provider,
        "StrictPrivateSlot",
        1,
        "Self.StrictPrivateS",
        0,
        true,
    );
    assert_access(
        &provider_uri,
        provider,
        "PrivateField",
        2,
        "Obj.PrivateF",
        0,
        true,
    );
    assert_access(
        &provider_uri,
        provider,
        "StrictPrivateSlot",
        2,
        "Obj.StrictPrivateS",
        0,
        false,
    );
    assert_access(
        &provider_uri,
        provider,
        "ProtectedField",
        1,
        "Self.ProtectedF",
        0,
        true,
    );
    assert_access(
        &provider_uri,
        provider,
        "StrictProtectedSlot",
        1,
        "Self.StrictProtectedS",
        0,
        true,
    );
    assert_access(
        &provider_uri,
        provider,
        "ProtectedField",
        2,
        "Obj.ProtectedF",
        0,
        true,
    );
    assert_access(
        &provider_uri,
        provider,
        "StrictProtectedSlot",
        2,
        "Obj.StrictProtectedS",
        0,
        false,
    );

    // A descendant in another unit gets protected access, including strict
    // protected access, but neither form of private access.
    assert_access(
        &consumer_uri,
        consumer,
        "ProtectedField",
        0,
        "Obj.ProtectedF",
        0,
        true,
    );
    assert_access(
        &consumer_uri,
        consumer,
        "StrictProtectedSlot",
        0,
        "Obj.StrictProtectedS",
        0,
        true,
    );
    assert_access(
        &consumer_uri,
        consumer,
        "PrivateField",
        0,
        "Obj.PrivateF",
        0,
        false,
    );
    assert_access(
        &consumer_uri,
        consumer,
        "StrictPrivateSlot",
        0,
        "Obj.StrictPrivateS",
        0,
        false,
    );
    for (member, prefix) in [
        ("DefaultField", "Obj.DefaultF"),
        ("PublicField", "Obj.PublicF"),
        ("PublishedField", "Obj.PublishedF"),
        ("RecordField", "R.RecordF"),
        ("InterfaceMethod", "Contract.InterfaceM"),
    ] {
        assert_access(&consumer_uri, consumer, member, 0, prefix, 0, true);
    }

    // An unrelated cross-unit caller cannot see restricted members, even when
    // the receiver's static type is the declaring class.
    assert_access(
        &consumer_uri,
        consumer,
        "ProtectedField",
        1,
        "Obj.ProtectedF",
        1,
        false,
    );
    assert_access(
        &consumer_uri,
        consumer,
        "StrictProtectedSlot",
        1,
        "Obj.StrictProtectedS",
        1,
        false,
    );
    assert_access(
        &consumer_uri,
        consumer,
        "PrivateField",
        1,
        "Obj.PrivateF",
        1,
        false,
    );
    assert_access(
        &consumer_uri,
        consumer,
        "StrictPrivateSlot",
        1,
        "Obj.StrictPrivateS",
        1,
        false,
    );
    for (member, prefix) in [
        ("DefaultField", "Obj.DefaultF"),
        ("PublicField", "Obj.PublicF"),
        ("PublishedField", "Obj.PublishedF"),
    ] {
        assert_access(&consumer_uri, consumer, member, 0, prefix, 1, true);
    }

    let private_field_position = position_of(consumer, "PrivateField", 0);
    assert!(
        index.hover(&consumer_uri, private_field_position).is_none(),
        "inaccessible member leaked into hover"
    );
    assert!(
        index
            .type_definitions(&consumer_uri, private_field_position)
            .is_empty(),
        "inaccessible member leaked into type definition"
    );

    let private_method_position = position_of(consumer, "PrivateMethod", 0);
    assert!(
        index
            .signature_help(
                &consumer_uri,
                position_after(consumer, "Obj.PrivateMethod(1", 0),
            )
            .expect("inaccessible signature help")
            .is_none(),
        "inaccessible member leaked into signature help"
    );
    assert!(
        index
            .hover(&consumer_uri, private_method_position)
            .is_none(),
        "inaccessible method leaked into hover"
    );

    let protected_method_position = position_of(consumer, "ProtectedMethod", 0);
    assert!(
        index
            .hover(&consumer_uri, protected_method_position)
            .is_some(),
        "accessible protected method lost hover"
    );
    assert!(
        index
            .signature_help(
                &consumer_uri,
                position_after(consumer, "Obj.ProtectedMethod(1", 0),
            )
            .expect("accessible signature help")
            .is_some(),
        "accessible protected method lost signature help"
    );
}

#[test]
fn lexical_declaration_order_keeps_future_bindings_out_of_scope() {
    let source = r#"unit DeclarationOrder;
interface
var
  Value: Integer;
type
  TWidget = class
    property ReadLater: Integer read LaterField;
    LaterField: Integer;
  end;
  TInterface = class
    Field: TImplementationOnly;
  end;
implementation
type
  TImplementationOnly = class
  end;
procedure TWidget.Check;
begin
  Self.LaterField;
end;
procedure Outer;
  procedure Forwarded; forward;
  procedure Uses;
  begin
    Forwarded;
    Later;
  end;
  procedure Forwarded;
  begin
  end;
  procedure Later;
  begin
  end;
begin
  Uses;
  Later;
end;
procedure Inline;
begin
  Value := 1;
  var Value: Integer;
  Value := 2;
end;
end.
"#;
    let source_uri = uri("DeclarationOrder");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("declaration order source parses");

    let early_later = index.navigate(
        &source_uri,
        position_of(source, "Later;", 0),
        NavigationTarget::Declaration,
    );
    assert!(
        early_later.is_empty(),
        "future nested routine captured an earlier call: {early_later:?}"
    );

    let forwarded = index.navigate(
        &source_uri,
        position_of(source, "Forwarded;", 1),
        NavigationTarget::Declaration,
    );
    assert!(
        !forwarded.is_empty(),
        "forward-declared nested routine was not available"
    );

    let later = index.navigate(
        &source_uri,
        position_of(source, "Later;", 2),
        NavigationTarget::Declaration,
    );
    assert!(
        !later.is_empty(),
        "declared nested routine was not available after its declaration"
    );

    let inline_before = index.navigate(
        &source_uri,
        position_of(source, "Value := 1", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(
        inline_before.len(),
        1,
        "the earlier use should resolve the unit-level declaration"
    );
    assert_location_start(
        &inline_before[0],
        &source_uri,
        position_of(source, "Value: Integer", 0),
    );

    let inline_after = index.navigate(
        &source_uri,
        position_of(source, "Value := 2", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(inline_after.len(), 1);
    assert_location_start(
        &inline_after[0],
        &source_uri,
        position_of(source, "Value: Integer", 1),
    );

    let class_member = index.navigate(
        &source_uri,
        position_of(source, "LaterField", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(class_member.len(), 1);
    assert_location_start(
        &class_member[0],
        &source_uri,
        position_of(source, "LaterField", 1),
    );

    assert!(
        index
            .navigate(
                &source_uri,
                position_of(source, "TImplementationOnly", 0),
                NavigationTarget::Declaration,
            )
            .is_empty(),
        "interface code captured an implementation-only type"
    );
}

#[test]
fn local_const_type_and_var_initializers_use_only_prior_bindings() {
    let source = r#"unit LocalDeclarationOrder;
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
    let source_uri = uri("LocalDeclarationOrder");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("local declaration order source parses");

    let before_value = index.navigate(
        &source_uri,
        position_after(source, "BeforeValue = ", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(before_value.len(), 1);
    assert_location_start(
        &before_value[0],
        &source_uri,
        position_of(source, "Value = 1", 0),
    );

    let local_value = index.navigate(
        &source_uri,
        position_after(source, "WriteLn(", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(local_value.len(), 1);
    assert_location_start(
        &local_value[0],
        &source_uri,
        position_of(source, "Value = 2", 0),
    );

    let before_type = index.navigate(
        &source_uri,
        position_of(source, "TGlobal;", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(before_type.len(), 1);
    assert_location_start(
        &before_type[0],
        &source_uri,
        position_of(source, "TGlobal = Integer", 0),
    );

    let local_type = index.navigate(
        &source_uri,
        position_after(source, "BeforeVar: ", 0),
        NavigationTarget::Declaration,
    );
    assert_eq!(local_type.len(), 1);
    assert_location_start(
        &local_type[0],
        &source_uri,
        position_of(source, "TGlobal = string", 0),
    );
}

#[test]
fn completion_filters_future_bindings_before_shadowing_precedence() {
    let source = r#"unit CompletionDeclarationOrder;
interface
const
  Value = 1;
implementation
procedure Run;
begin
  if True then
  begin
    Val;
    var Value: Integer;
    Value := 2;
  end;
end;
end.
"#;
    let source_uri = uri("CompletionDeclarationOrder");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("completion declaration order source parses");

    let completion = index
        .completion(&source_uri, position_after(source, "Val", 1))
        .expect("completion before future local declaration");
    assert_eq!(completion.items.len(), 1);
    assert_eq!(completion.items[0].label, "Value");
    assert_eq!(completion.items[0].kind, Some(CompletionItemKind::CONSTANT));
}

#[test]
fn completion_filters_inaccessible_members_before_shadowing_precedence() {
    let provider = r#"unit CompletionAccessProvider;
interface
type
  TBase = class
  private
    Value: Integer;
  end;
end.
"#;
    let consumer = r#"unit CompletionAccessConsumer;
interface
uses CompletionAccessProvider;
const
  Value = 1;
type
  TChild = class(TBase)
    procedure Run;
  end;
implementation
procedure TChild.Run;
begin
  Value;
  Val
end;
end.
"#;
    let provider_uri = uri("CompletionAccessProvider");
    let consumer_uri = uri("CompletionAccessConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri, provider.to_owned())
        .expect("completion access provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("completion access consumer parses");

    let value_position = position_of(consumer, "Value;", 0);
    assert!(
        index
            .navigate(&consumer_uri, value_position, NavigationTarget::Declaration,)
            .is_empty(),
        "inaccessible member leaked through navigation"
    );
    assert!(
        index.hover(&consumer_uri, value_position).is_none(),
        "inaccessible member leaked through hover"
    );

    let completion = index
        .completion(&consumer_uri, position_after(consumer, "Val", 2))
        .expect("completion with inaccessible member collision");
    assert!(
        completion.items.is_empty(),
        "inaccessible member leaked through completion: {completion:?}"
    );
}

#[test]
fn inaccessible_member_cannot_be_used_as_a_receiver_for_nested_lookup() {
    let provider = r#"unit NestedAccessProvider;
interface
type
  TPayload = class
    Exposed: Integer;
  end;
  TBase = class
  private
    Hidden: TPayload;
  end;
end.
"#;
    let consumer = r#"unit NestedAccessConsumer;
interface
uses NestedAccessProvider;
procedure ReadValue;
implementation
procedure ReadValue;
var
  Obj: TBase;
begin
  Obj.Hidden.Exposed;
end;
end.
"#;
    let provider_uri = uri("NestedAccessProvider");
    let consumer_uri = uri("NestedAccessConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri, provider.to_owned())
        .expect("nested access provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("nested access consumer parses");

    let exposed_position = position_of(consumer, "Exposed", 0);
    assert!(
        index
            .navigate(
                &consumer_uri,
                exposed_position,
                NavigationTarget::Declaration,
            )
            .is_empty(),
        "nested lookup traversed an inaccessible receiver"
    );
    assert!(
        index.hover(&consumer_uri, exposed_position).is_none(),
        "nested lookup leaked an inaccessible receiver into hover"
    );

    let completion = index
        .completion(
            &consumer_uri,
            position_after(consumer, "Obj.Hidden.Expos", 0),
        )
        .expect("nested access completion");
    assert!(
        completion.items.iter().all(|item| item.label != "Exposed"),
        "nested lookup leaked an inaccessible receiver into completion"
    );
}

#[test]
fn unqualified_inaccessible_receiver_is_filtered_before_nested_lookup() {
    let provider = r#"unit UnqualifiedReceiverProvider;
interface
type
  TPayload = class
    Exposed: Integer;
  end;
  TBase = class
  private
    Hidden: TPayload;
  end;
end.
"#;
    let consumer = r#"unit UnqualifiedReceiverConsumer;
interface
uses UnqualifiedReceiverProvider;
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
    let provider_uri = uri("UnqualifiedReceiverProvider");
    let consumer_uri = uri("UnqualifiedReceiverConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri, provider.to_owned())
        .expect("unqualified receiver provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("unqualified receiver consumer parses");

    assert!(
        index
            .navigate(
                &consumer_uri,
                position_of(consumer, "Hidden", 0),
                NavigationTarget::Declaration,
            )
            .is_empty(),
        "inaccessible unqualified receiver was exposed"
    );
    assert!(
        index
            .navigate(
                &consumer_uri,
                position_of(consumer, "Exposed", 0),
                NavigationTarget::Declaration,
            )
            .is_empty(),
        "nested member lookup traversed an inaccessible unqualified receiver"
    );

    let completion = index
        .completion(&consumer_uri, position_after(consumer, "Hidden.Exp", 0))
        .expect("unqualified receiver completion");
    assert!(
        completion.items.is_empty(),
        "inaccessible unqualified receiver leaked nested members: {completion:?}"
    );
}

#[test]
fn inaccessible_unqualified_receiver_does_not_fall_back_to_imported_unit() {
    let provider = r#"unit ReceiverUnitProvider;
interface
type
  TBase = class
  private
    Value: Integer;
    Hidden: Integer;
  end;
end.
"#;
    let consumer = r#"unit ReceiverUnitConsumer;
interface
uses ReceiverUnitProvider, Hidden;
const
  Value = 1;
type
  TChild = class(TBase)
    procedure Run;
  end;
implementation
procedure TChild.Run;
begin
  Value;
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
    let provider_uri = uri("ReceiverUnitProvider");
    let consumer_uri = uri("ReceiverUnitConsumer");
    let hidden_uri = uri("Hidden");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri, provider.to_owned())
        .expect("receiver unit provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("receiver unit consumer parses");
    index
        .update(hidden_uri, hidden.to_owned())
        .expect("hidden unit parses");

    assert!(
        index
            .navigate(
                &consumer_uri,
                position_of(consumer, "Hidden.Exposed", 0),
                NavigationTarget::Declaration,
            )
            .is_empty(),
        "inaccessible receiver fell back to an unrelated unit"
    );
    assert!(
        index
            .navigate(
                &consumer_uri,
                position_of(consumer, "Exposed", 0),
                NavigationTarget::Declaration,
            )
            .is_empty(),
        "unit member navigation survived an inaccessible receiver"
    );

    let completion = index
        .completion(&consumer_uri, position_after(consumer, "Hidden.Exp", 0))
        .expect("receiver unit completion");
    assert!(
        completion.items.is_empty(),
        "inaccessible receiver fell back to an unrelated unit in completion: {completion:?}"
    );
}

#[test]
fn free_procedure_rejects_restricted_members_but_selects_public_overload() {
    for visibility in ["private", "strict private", "strict protected", "protected"] {
        let provider = format!(
            r#"unit FreeProcedureAccessProvider;
interface
type
  TBase = class
  {visibility}
    procedure Pick(X: Integer); overload;
  public
    procedure Pick(X: string); overload;
  end;
end.
"#
        );
        let consumer = r#"unit FreeProcedureAccessConsumer;
interface
uses FreeProcedureAccessProvider;
implementation
procedure Run;
var
  Obj: TBase;
  S: string;
begin
  Obj.Pick(S);
end;
end.
"#;
        let provider_uri = uri("FreeProcedureAccessProvider");
        let consumer_uri = uri("FreeProcedureAccessConsumer");
        let mut index = NavigationIndex::new();
        index
            .update(provider_uri.clone(), provider.clone())
            .expect("free procedure provider parses");
        index
            .update(consumer_uri.clone(), consumer.to_owned())
            .expect("free procedure consumer parses");

        let navigation = index.navigate(
            &consumer_uri,
            position_of(consumer, "Pick(S)", 0),
            NavigationTarget::Declaration,
        );
        assert_eq!(
            navigation.len(),
            1,
            "free procedure restricted visibility blocked public overload: {visibility}"
        );
        assert_location_start(
            &navigation[0],
            &provider_uri,
            position_of(&provider, "Pick(X: string)", 0),
        );

        let signature = index
            .signature_help(&consumer_uri, position_after(consumer, "Obj.Pick(S", 0))
            .expect("free procedure signature help");
        let signature = signature
            .as_ref()
            .expect("public overload signature is available");
        assert_eq!(
            signature.signatures.len(),
            1,
            "restricted overload leaked into free procedure signature help: {visibility}"
        );
        assert!(
            signature.signatures[0].label.contains("string"),
            "public string overload was not selected: {signature:?}"
        );
    }
}

#[test]
fn unknown_access_ancestry_is_hidden_and_marks_completion_incomplete() {
    let provider = r#"unit UnknownAccessProvider;
interface
type
  TBase = class
  protected
    ProtectedField: Integer;
  end;
end.
"#;
    let consumer = r#"unit UnknownAccessConsumer;
interface
uses UnknownAccessProvider;
type
  TUnknownChild = class(MissingBase)
    procedure ReadValue;
  end;
implementation
procedure TUnknownChild.ReadValue;
var
  Obj: TBase;
begin
  Obj.ProtectedField;
end;
end.
"#;
    let provider_uri = uri("UnknownAccessProvider");
    let consumer_uri = uri("UnknownAccessConsumer");
    let mut index = NavigationIndex::new();
    index
        .update(provider_uri, provider.to_owned())
        .expect("unknown access provider parses");
    index
        .update(consumer_uri.clone(), consumer.to_owned())
        .expect("unknown access consumer parses");

    let position = position_of(consumer, "ProtectedField", 0);
    assert!(
        index
            .navigate(&consumer_uri, position, NavigationTarget::Declaration)
            .is_empty(),
        "unknown ancestry guessed protected access"
    );

    let completion = index
        .completion(&consumer_uri, position_after(consumer, "Obj.ProtectedF", 0))
        .expect("unknown access completion");
    assert!(
        completion
            .items
            .iter()
            .all(|item| item.label != "ProtectedField"),
        "unknown ancestry leaked protected completion"
    );
    assert!(
        completion.is_incomplete,
        "unknown ancestry should make completion conservative"
    );
}

#[test]
fn generic_descendant_proves_strict_protected_access() {
    let source = r#"unit GenericAccess;
interface
type
  TBase<T> = class
  strict protected
    ProtectedField: Integer;
  end;
  TChild<T> = class(TBase<T>)
    procedure ReadValue;
  end;
implementation
procedure TChild<T>.ReadValue;
var
  Obj: TBase<Integer>;
begin
  Obj.ProtectedField;
end;
end.
"#;
    let source_uri = uri("GenericAccess");
    let mut index = NavigationIndex::new();
    index
        .update(source_uri.clone(), source.to_owned())
        .expect("generic access source parses");

    let position = position_of(source, "ProtectedField", 1);
    let navigation = index.navigate(&source_uri, position, NavigationTarget::Declaration);
    assert_eq!(navigation.len(), 1, "generic descendant lost strict access");
    assert_location_start(
        &navigation[0],
        &source_uri,
        position_of(source, "ProtectedField", 0),
    );
}
