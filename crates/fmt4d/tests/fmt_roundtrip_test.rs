use std::path::PathBuf;

mod common;
use common::{format_source, idempotency_check};

fn ast_eq(a: tree_sitter::Node, b: tree_sitter::Node) -> bool {
    if a.kind() != b.kind() {
        return false;
    }
    let ac: Vec<_> = a
        .children(&mut a.walk())
        .filter(|c| !c.is_extra())
        .collect();
    let bc: Vec<_> = b
        .children(&mut b.walk())
        .filter(|c| !c.is_extra())
        .collect();
    if ac.len() != bc.len() {
        return false;
    }
    ac.iter().zip(bc.iter()).all(|(x, y)| ast_eq(*x, *y))
}

fn roundtrip_check(source: &str) {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let formatted = format_source(source);

    // Parse both
    let (tree_before, _) =
        pascal_core::parser::parse_file(&info, source.as_bytes()).expect("parse original failed");
    let (tree_after, _) = pascal_core::parser::parse_file(&info, formatted.as_bytes())
        .expect("parse formatted failed");

    // Compare structure
    assert!(
        ast_eq(tree_before.root_node(), tree_after.root_node()),
        "AST changed!\nOriginal:\n{}\nFormatted:\n{}",
        source,
        formatted
    );
}

// ── Round-trip tests ────────────────────────────────────────────

#[test]
fn roundtrip_simple_unit() {
    let source = "unit Test;\ninterface\nimplementation\nend.\n";
    roundtrip_check(source);
}

#[test]
fn roundtrip_procedure_with_body() {
    let source = r#"unit Test;

interface

implementation

procedure DoSomething;
var
  x: Integer;
begin
  x := 1;
  if x > 0 then
  begin
    x := x + 1;
  end;
end;

end.
"#;
    roundtrip_check(source);
}

#[test]
fn roundtrip_class_declaration() {
    let source = r#"unit Test;

interface

type
  TMyClass = class
  private
    FName: string;
  public
    procedure Run;
  end;

implementation

end.
"#;
    roundtrip_check(source);
}

// ── Idempotency tests ───────────────────────────────────────────

#[test]
fn roundtrip_conditional_method_attributes() {
    let source = r#"unit Test;

interface

type
  TConditional = class
    procedure IfDefElse; {$IFDEF DELPHI_XE6_UP}reintroduce{$ELSE}override{$ENDIF};
    procedure IfDefNoElse; {$IFDEF DELPHI_XE6_UP}reintroduce{$ENDIF};
    procedure StandardThenConditional; virtual; {$IFDEF DELPHI_XE6_UP}reintroduce{$ELSE}override{$ENDIF};
    procedure ConditionalThenStandard; {$IFDEF DELPHI_XE6_UP}reintroduce{$ELSE}override{$ENDIF}; overload;
  end;

implementation

end.
"#;
    let formatted = format_source(source);

    for token in [
        "{$IFDEF DELPHI_XE6_UP}",
        "{$ELSE}",
        "{$ENDIF}",
        "reintroduce",
        "override",
        "virtual",
        "overload",
    ] {
        assert!(
            formatted.contains(token),
            "formatted source dropped {token:?}:\n{formatted}"
        );
    }

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let (tree_before, diagnostics_before) =
        pascal_core::parser::parse_file(&info, source.as_bytes()).expect("parse original failed");
    let (tree_after, diagnostics_after) =
        pascal_core::parser::parse_file(&info, formatted.as_bytes())
            .expect("parse formatted failed");
    assert!(
        diagnostics_before.is_empty(),
        "original conditional attributes produced diagnostics: {diagnostics_before:?}"
    );
    assert!(
        diagnostics_after.is_empty(),
        "formatted conditional attributes produced diagnostics: {diagnostics_after:?}"
    );
    assert!(ast_eq(tree_before.root_node(), tree_after.root_node()));
    assert_eq!(formatted, format_source(&formatted));
}

#[test]
fn roundtrip_conditional_method_attribute_trailing_comment() {
    let source = "unit Test;\ninterface\nprocedure P; {$IFDEF X}reintroduce // comment\n {$ELSE}override{$ENDIF};\nimplementation\nend.\n";
    let formatted = format_source(source);

    let mut search_from = 0;
    for directive in ["{$IFDEF X}", "{$ELSE}", "{$ENDIF}"] {
        let relative = formatted[search_from..]
            .find(directive)
            .unwrap_or_else(|| panic!("formatted source dropped {directive:?}:\n{formatted}"));
        search_from += relative + directive.len();
    }
    assert!(
        formatted.contains("reintroduce // comment\n"),
        "line comment must end before the next directive:\n{formatted}"
    );

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let (tree_before, diagnostics_before) =
        pascal_core::parser::parse_file(&info, source.as_bytes()).expect("parse original failed");
    let (tree_after, diagnostics_after) =
        pascal_core::parser::parse_file(&info, formatted.as_bytes())
            .expect("parse formatted failed");
    assert!(
        diagnostics_before.is_empty(),
        "original conditional attribute produced diagnostics: {diagnostics_before:?}"
    );
    assert!(
        diagnostics_after.is_empty(),
        "formatted conditional attribute produced diagnostics: {diagnostics_after:?}"
    );
    assert!(ast_eq(tree_before.root_node(), tree_after.root_node()));
    assert_eq!(formatted, format_source(&formatted));
}

#[test]
fn roundtrip_conditional_method_attribute_comment_is_three_pass_idempotent() {
    let source = r#"unit Test;

interface

procedure P; {$IFDEF X}reintroduce{$ELSE}override{$ENDIF}; // comment
overload;

implementation

end.
"#;
    let first = format_source(source);
    let second = format_source(&first);
    let third = format_source(&second);

    assert_eq!(
        first, second,
        "formatter changed the second pass:\n{second}"
    );
    assert_eq!(second, third, "formatter changed the third pass:\n{third}");
    assert!(
        first.contains("// comment\n"),
        "line comment must not gain trailing spaces:\n{first}"
    );

    let mut search_from = 0;
    for directive in ["{$IFDEF X}", "{$ELSE}", "{$ENDIF}"] {
        let relative = first[search_from..]
            .find(directive)
            .unwrap_or_else(|| panic!("formatted source dropped {directive:?}:\n{first}"));
        search_from += relative + directive.len();
    }

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let (tree_before, diagnostics_before) =
        pascal_core::parser::parse_file(&info, source.as_bytes()).expect("parse original failed");
    let (tree_after, diagnostics_after) =
        pascal_core::parser::parse_file(&info, first.as_bytes()).expect("parse formatted failed");
    assert!(
        diagnostics_before.is_empty(),
        "original conditional attribute produced diagnostics: {diagnostics_before:?}"
    );
    assert!(
        diagnostics_after.is_empty(),
        "formatted conditional attribute produced diagnostics: {diagnostics_after:?}"
    );
    assert!(ast_eq(tree_before.root_node(), tree_after.root_node()));
}

#[test]
fn idempotent_simple_unit() {
    let source = "unit Test;\ninterface\nimplementation\nend.\n";
    idempotency_check(source);
}

#[test]
fn idempotent_formatted_fixture() {
    let source = std::fs::read_to_string("../../tests/fixtures/format/indent/basic_expected.pas")
        .expect("failed to read fixture");
    idempotency_check(&source);
}

#[test]
fn idempotent_unformatted_source() {
    let source = r#"unit Test;
interface
type
TMyClass = class
private
FValue: Integer;
public
procedure DoSomething;
end;
implementation
procedure TMyClass.DoSomething;
var
x: Integer;
begin
x := 1;
if x > 0 then
begin
x := x + 1;
end;
end;
end.
"#;
    // Format once, then check idempotency
    let formatted = format_source(source);
    idempotency_check(&formatted);
}

// ── Alias keyword misparse ──────────────────────────────────────

#[test]
fn idempotent_var_named_alias() {
    let source = "\
unit Test;
interface
implementation
procedure DoSomething;
var
  I: Integer;
  JoinIdx: Integer;
  F: TFieldDef;
  LookupSql: RawUtf8;
  Alias: RawUtf8;
  FirstOwnerKey: RawUtf8;
  FirstLookupKey: RawUtf8;
  FirstResultField: RawUtf8;
begin
end;
end.
";
    idempotency_check(source);
}

#[test]
fn idempotent_var_named_alias_first_in_block() {
    let source = "\
unit Test;
interface
implementation
procedure Foo;
var
  Alias: Integer;
  X: Integer;
begin
end;
end.
";
    idempotency_check(source);
}

#[test]
fn idempotent_var_named_alias_lowercase() {
    let source = "\
unit Test;
interface
implementation
procedure Foo;
var
  X: Integer;
  alias: String;
  Y: Boolean;
begin
end;
end.
";
    idempotency_check(source);
}

#[test]
fn var_named_alias_not_merged_with_previous() {
    let source = "\
unit Test;
interface
implementation
procedure Foo;
var
  LookupSql: RawUtf8;
  Alias: RawUtf8;
begin
end;
end.
";
    let formatted = format_source(source);
    assert!(
        formatted.contains("LookupSql: RawUtf8;\n"),
        "LookupSql should end its own line. Got:\n{}",
        formatted
    );
    assert!(
        formatted.contains("Alias: RawUtf8;\n"),
        "Alias should be on its own line. Got:\n{}",
        formatted
    );
}
