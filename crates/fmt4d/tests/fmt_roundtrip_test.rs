use std::path::PathBuf;

fn format_source_helper(source: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    fmt4d::formatter::format_source(
        source.as_bytes(),
        &info,
        &config,
        &std::collections::HashSet::new(),
    )
    .expect("formatting failed")
}

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
    let config = fmt4d::config::FmtConfig::default();
    let formatted = fmt4d::formatter::format_source(
        source.as_bytes(),
        &info,
        &config,
        &std::collections::HashSet::new(),
    )
    .expect("formatting failed");

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

fn idempotency_check(source: &str) {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    let first = fmt4d::formatter::format_source(
        source.as_bytes(),
        &info,
        &config,
        &std::collections::HashSet::new(),
    )
    .expect("first format failed");
    let second = fmt4d::formatter::format_source(
        first.as_bytes(),
        &info,
        &config,
        &std::collections::HashSet::new(),
    )
    .expect("second format failed");
    assert_eq!(first, second, "Not idempotent!");
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
    let formatted = format_source_helper(source);
    idempotency_check(&formatted);
}
