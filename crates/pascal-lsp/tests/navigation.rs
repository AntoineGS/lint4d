use lsp_types::{Location, Position, Url};
use pascal_lsp::workspace::{Workspace, WorkspaceOptions};
use pascal_lsp::{NavigationIndex, NavigationTarget, text};
use std::collections::HashMap;
use std::fs;
use std::process::Command;

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

const PROVIDER: &str = r#"unit Provider;
interface

type
  TWidget = class
  private
    FValue: Integer;
    procedure DoThing;
  public
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
fn overloads_return_all_viable_candidates() {
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
    assert_eq!(candidates.len(), 2);
    assert!(
        candidates
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
    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
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
    let mut workspace = Workspace::new(vec![root], WorkspaceOptions::default());
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
