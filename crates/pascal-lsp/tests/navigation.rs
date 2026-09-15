use lsp_types::{CompletionTextEdit, HoverContents, Location, MarkedString, Position, Range, Url};
use pascal_core::delphi_overrides::OverrideSession;
use pascal_lsp::workspace::{Workspace, WorkspaceOptions};
use pascal_lsp::{NavigationIndex, NavigationTarget, text};
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
fn signature_help_rejects_lookup_inside_a_with_receiver_context() {
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

    assert!(
        index
            .signature_help(&source_uri, position_after(source, "    Run(", 0))
            .expect("with signature projection")
            .is_none(),
        "with receiver lookup must fail closed"
    );
}

#[test]
fn completion_rejects_blank_positions_inside_a_with_context() {
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
    assert!(completion.items.is_empty());
    assert!(!completion.is_incomplete);
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
