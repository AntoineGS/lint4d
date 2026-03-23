use std::fs;
use std::path::PathBuf;

use lint4d::config::Config;
use lint4d::engine::{parse_file, run_lint, FileInfo};
use lint4d::engine::suppress::parse_suppressions;
use lint4d::fix::{build_rename_map, fix_file};

fn build_map_from_source(source: &str) -> lint4d::fix::RenameMap {
    let config = "version = 1".parse::<Config>().unwrap();
    let file = FileInfo::new(PathBuf::from("test.pas"));
    let source_bytes = source.as_bytes();
    let (tree, _) = parse_file(&file, source_bytes).unwrap();
    let suppressions = parse_suppressions(source_bytes);
    build_rename_map(tree.root_node(), source_bytes, &config, &suppressions)
}

#[test]
fn pass1_type_prefix_builds_rename() {
    let source = r#"unit Test;
interface
type
  MyClass = class(TObject)
  end;
implementation
end."#;
    let map = build_map_from_source(source);
    assert_eq!(map.file.get("myclass"), Some(&"TMyClass".to_string()));
}

#[test]
fn pass1_type_prefix_skips_t_prefix() {
    let source = r#"unit Test;
interface
type
  TMyClass = class(TObject)
  end;
implementation
end."#;
    let map = build_map_from_source(source);
    assert!(map.file.is_empty());
}

#[test]
fn pass1_type_prefix_skips_e_prefix() {
    let source = r#"unit Test;
interface
type
  EMyError = class(Exception)
  end;
implementation
end."#;
    let map = build_map_from_source(source);
    assert!(map.file.is_empty());
}

#[test]
fn pass1_interface_prefix_builds_rename() {
    let source = r#"unit Test;
interface
type
  MyIntf = interface
    procedure DoWork;
  end;
implementation
end."#;
    let map = build_map_from_source(source);
    assert_eq!(map.file.get("myintf"), Some(&"IMyIntf".to_string()));
}

#[test]
fn pass1_constant_naming_upper_case() {
    let source = r#"unit Test;
interface
const
  maxSize = 100;
  ALREADY_GOOD = 42;
implementation
end."#;
    let map = build_map_from_source(source);
    assert_eq!(map.file.get("maxsize"), Some(&"MAX_SIZE".to_string()));
    assert!(map.file.get("already_good").is_none());
}

#[test]
fn pass1_constant_naming_pascal_case() {
    let config = r#"version = 1
[rules.naming]
constant_style = "PascalCase""#
        .parse::<Config>()
        .unwrap();
    let source = r#"unit Test;
interface
const
  httpPort = 8080;
implementation
end."#;
    let file = FileInfo::new(PathBuf::from("test.pas"));
    let source_bytes = source.as_bytes();
    let (tree, _) = parse_file(&file, source_bytes).unwrap();
    let suppressions = parse_suppressions(source_bytes);
    let map = build_rename_map(tree.root_node(), source_bytes, &config, &suppressions);
    assert_eq!(map.file.get("httpport"), Some(&"HttpPort".to_string()));
}

#[test]
fn pass1_local_variable_camel_case() {
    let source = r#"unit Test;
interface
implementation
procedure DoWork;
var
  MyCounter: Integer;
  x: Integer;
begin
end;
end."#;
    let map = build_map_from_source(source);
    assert!(map.file.is_empty());
    // Find the local rename — key is (proc_start, proc_end, "mycounter")
    let local_entry = map
        .local
        .iter()
        .find(|((_, _, name), _)| name == "mycounter");
    assert_eq!(
        local_entry.map(|(_, v)| v.as_str()),
        Some("myCounter"),
    );
    // Single-char 'x' should be exempt
    let x_entry = map.local.iter().find(|((_, _, name), _)| name == "x");
    assert!(x_entry.is_none());
}

#[test]
fn pass1_skips_suppressed_declarations() {
    let source = r#"unit Test;
interface
type
  // lint4d:ignore type-prefix
  MyClass = class(TObject)
  end;
implementation
end."#;
    let map = build_map_from_source(source);
    assert!(map.file.get("myclass").is_none());
}

#[test]
fn pass1_skips_disabled_rules() {
    let config = r#"version = 1
[rules]
type-prefix = "off""#
        .parse::<Config>()
        .unwrap();
    let source = r#"unit Test;
interface
type
  MyClass = class(TObject)
  end;
implementation
end."#;
    let file = FileInfo::new(PathBuf::from("test.pas"));
    let source_bytes = source.as_bytes();
    let (tree, _) = parse_file(&file, source_bytes).unwrap();
    let suppressions = parse_suppressions(source_bytes);
    let map = build_rename_map(tree.root_node(), source_bytes, &config, &suppressions);
    assert!(map.file.is_empty());
}

// ---------------------------------------------------------------------------
// Pass 2 + apply_edits tests
// ---------------------------------------------------------------------------

fn fix_source(source: &str) -> String {
    let config = "version = 1".parse::<Config>().unwrap();
    let file = FileInfo::new(PathBuf::from("test.pas"));
    let (result, _count) = fix_file(&file, source.as_bytes(), &config).unwrap();
    String::from_utf8(result).unwrap()
}

#[allow(dead_code)]
fn fix_source_with_config(source: &str, config_str: &str) -> String {
    let config = config_str.parse::<Config>().unwrap();
    let file = FileInfo::new(PathBuf::from("test.pas"));
    let (result, _count) = fix_file(&file, source.as_bytes(), &config).unwrap();
    String::from_utf8(result).unwrap()
}

#[test]
fn fix_type_prefix_renames_declaration_and_usages() {
    let source = r#"unit Test;

interface

type
  MyClass = class(TObject)
  public
    procedure DoWork;
  end;

var
  Obj: MyClass;

implementation

procedure MyClass.DoWork;
var
  Local: MyClass;
begin
end;

end."#;
    let fixed = fix_source(source);
    // Declaration renamed
    assert!(fixed.contains("TMyClass = class(TObject)"));
    // Var type renamed (typeref)
    assert!(fixed.contains("Obj: TMyClass;"));
    // genericDot header renamed
    assert!(fixed.contains("procedure TMyClass.DoWork;"));
    // Local var type renamed (variable itself renamed to camelCase "local")
    assert!(fixed.contains("local: TMyClass;"));
    // Old name gone
    assert!(!fixed.contains(" MyClass"));
}

#[test]
fn fix_constant_naming_renames_declaration_and_usages() {
    let source = r#"unit Test;

interface

const
  maxSize = 100;

implementation

procedure DoWork;
var
  x: Integer;
begin
  x := maxSize;
end;

end."#;
    let fixed = fix_source(source);
    assert!(fixed.contains("MAX_SIZE = 100;"));
    assert!(fixed.contains("x := MAX_SIZE;"));
    assert!(!fixed.contains("maxSize"));
}

#[test]
fn fix_local_variable_renames_within_procedure() {
    let source = r#"unit Test;

interface

implementation

procedure DoWork;
var
  MyCounter: Integer;
begin
  MyCounter := 1;
end;

end."#;
    let fixed = fix_source(source);
    assert!(fixed.contains("myCounter: Integer;"));
    assert!(fixed.contains("myCounter := 1;"));
    assert!(!fixed.contains("MyCounter"));
}

#[test]
fn fix_identifier_casing_normalizes_usages() {
    let source = r#"unit Test;

interface

const
  MY_CONST = 42;

implementation

procedure DoWork;
var
  counter: Integer;
begin
  COUNTER := my_const;
end;

end."#;
    let fixed = fix_source(source);
    // Casing should match declarations
    assert!(fixed.contains("counter := MY_CONST;"), "ACTUAL:\n{fixed}");
}

#[test]
fn fix_skips_dpr_files() {
    let config = "version = 1".parse::<Config>().unwrap();
    let file = FileInfo::new(PathBuf::from("Project.dpr"));
    let source = b"program MyProject;\nbegin\nend.";
    let (result, count) = fix_file(&file, source, &config).unwrap();
    assert_eq!(count, 0);
    assert_eq!(result, source);
}

// ---------------------------------------------------------------------------
// Edge case tests
// ---------------------------------------------------------------------------

#[test]
fn fix_interface_prefix_renames_declaration_and_usages() {
    let source = r#"unit InterfacePrefixFix;

interface

type
  Printable = interface
    procedure Print;
  end;

  TDoc = class(TObject, Printable)
  public
    procedure Print;
  end;

implementation

procedure TDoc.Print;
begin
end;

end."#;
    let fixed = fix_source(source);
    // Declaration renamed
    assert!(fixed.contains("IPrintable = interface"), "ACTUAL:\n{fixed}");
    // Usage in class inheritance list renamed
    assert!(fixed.contains("TDoc = class(TObject, IPrintable)"), "ACTUAL:\n{fixed}");
    // Old bare name gone (without 'I' prefix)
    assert!(!fixed.contains("  Printable = interface"), "ACTUAL:\n{fixed}");
}

#[test]
fn fix_combined_violations() {
    let source = r#"unit CombinedFix;

interface

type
  MyClass = class(TObject)
  public
    procedure DoWork;
  end;

const
  maxRetries = 3;

implementation

procedure MyClass.DoWork;
var
  RetryCount: Integer;
begin
  RetryCount := maxRetries;
end;

end."#;
    let fixed = fix_source(source);
    // Type renamed
    assert!(fixed.contains("TMyClass = class(TObject)"), "ACTUAL:\n{fixed}");
    // Constant renamed
    assert!(fixed.contains("MAX_RETRIES = 3;"), "ACTUAL:\n{fixed}");
    // Local variable renamed
    assert!(fixed.contains("retryCount: Integer;"), "ACTUAL:\n{fixed}");
    // Usage of renamed local variable
    assert!(fixed.contains("retryCount := MAX_RETRIES;"), "ACTUAL:\n{fixed}");
    // Old names gone
    assert!(!fixed.contains("maxRetries"), "ACTUAL:\n{fixed}");
    assert!(!fixed.contains("RetryCount"), "ACTUAL:\n{fixed}");
}

#[test]
fn fix_nested_procedure_outer_variable() {
    // Outer procedure declares MyVar; inner procedure uses it.
    // Both declaration and usage should be renamed to myVar.
    let source = r#"unit Test;

interface

implementation

procedure Outer;
var
  MyVar: Integer;

  procedure Inner;
  begin
    MyVar := 1;
  end;

begin
  MyVar := 2;
end;

end."#;
    let fixed = fix_source(source);
    // Outer proc declaration renamed
    assert!(fixed.contains("myVar: Integer;"), "ACTUAL:\n{fixed}");
    // Inner proc usage renamed
    assert!(fixed.contains("myVar := 1;"), "ACTUAL:\n{fixed}");
    // Outer proc usage renamed
    assert!(fixed.contains("myVar := 2;"), "ACTUAL:\n{fixed}");
    // Old name gone
    assert!(!fixed.contains("MyVar"), "ACTUAL:\n{fixed}");
}

#[test]
fn fix_suppressed_declaration_not_renamed() {
    let source = r#"unit SuppressedFix;

interface

type
  // lint4d:ignore type-prefix
  MyClass = class(TObject)
  end;

const
  maxSize = 100; // lint4d:ignore constant-naming

implementation

end."#;
    let fixed = fix_source(source);
    // Suppressed type declaration must not be renamed
    assert!(fixed.contains("MyClass = class(TObject)"), "ACTUAL:\n{fixed}");
    // Suppressed constant must not be renamed
    assert!(fixed.contains("maxSize = 100;"), "ACTUAL:\n{fixed}");
}

#[test]
fn fix_multi_name_declaration() {
    // `MyA, MyB: Integer` — both names must be renamed to camelCase.
    let source = r#"unit Test;

interface

implementation

procedure DoWork;
var
  MyA, MyB: Integer;
begin
  MyA := 1;
  MyB := 2;
end;

end."#;
    let fixed = fix_source(source);
    // Declaration renamed
    assert!(fixed.contains("myA, myB: Integer;"), "ACTUAL:\n{fixed}");
    // Usages renamed
    assert!(fixed.contains("myA := 1;"), "ACTUAL:\n{fixed}");
    assert!(fixed.contains("myB := 2;"), "ACTUAL:\n{fixed}");
    // Old names gone
    assert!(!fixed.contains("MyA"), "ACTUAL:\n{fixed}");
    assert!(!fixed.contains("MyB"), "ACTUAL:\n{fixed}");
}

#[test]
fn fix_preserves_clean_identifiers() {
    // Already-conforming code should produce zero edits.
    let source = r#"unit Clean;

interface

type
  TMyClass = class(TObject)
  public
    procedure DoWork;
  end;

const
  MY_CONST = 42;

implementation

procedure TMyClass.DoWork;
var
  myVar: Integer;
begin
  myVar := MY_CONST;
end;

end."#;
    let config = "version = 1".parse::<Config>().unwrap();
    let file = FileInfo::new(PathBuf::from("test.pas"));
    let (_, count) = fix_file(&file, source.as_bytes(), &config).unwrap();
    assert_eq!(count, 0, "Expected zero edits for already-clean code");
}

#[test]
fn fix_no_changes_for_clean_file() {
    let source = r#"unit Clean;

interface

type
  TMyClass = class
  end;

const
  MY_CONST = 42;

implementation

procedure DoWork;
var
  myVar: Integer;
begin
  myVar := MY_CONST;
end;

end."#;
    let config = "version = 1".parse::<Config>().unwrap();
    let file = FileInfo::new(PathBuf::from("test.pas"));
    let (_, count) = fix_file(&file, source.as_bytes(), &config).unwrap();
    assert_eq!(count, 0);
}

// ---------------------------------------------------------------------------
// Fixture-based integration tests
// ---------------------------------------------------------------------------

fn fix_fixture(fixture_path: &str) -> (String, usize) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture_path);
    let source = fs::read(&path).unwrap();
    let file = FileInfo::new(PathBuf::from(fixture_path));
    let config = "version = 1".parse::<Config>().unwrap();
    let (result, count) = fix_file(&file, &source, &config).unwrap();
    (String::from_utf8(result).unwrap(), count)
}

fn lint_source(source: &str) -> Vec<lint4d::engine::Diagnostic> {
    let config = "version = 1".parse::<Config>().unwrap();
    let file = FileInfo::new(PathBuf::from("test.pas"));
    run_lint(&file, source.as_bytes(), &config)
}

#[test]
fn fixture_type_prefix_fix() {
    let (fixed, count) = fix_fixture("tests/fixtures/fix/type_prefix.pas");
    assert!(count > 0, "Expected fixes to be applied");
    assert!(fixed.contains("TMyClass = class(TObject)"));
    assert!(fixed.contains("Obj: TMyClass;"));
    assert!(fixed.contains("procedure TMyClass.DoWork;"));
}

#[test]
fn fixture_constant_naming_fix() {
    let (fixed, count) = fix_fixture("tests/fixtures/fix/constant_naming.pas");
    assert!(count > 0);
    assert!(fixed.contains("MAX_SIZE = 100;"));
    assert!(fixed.contains("HTTP_PORT = 8080;"));
    assert!(fixed.contains("ALREADY_GOOD = 42;")); // unchanged
}

#[test]
fn fixture_local_variable_fix() {
    let (fixed, count) = fix_fixture("tests/fixtures/fix/local_variable.pas");
    assert!(count > 0);
    assert!(fixed.contains("myCounter: Integer;"));
    assert!(fixed.contains("anotherBadName: string;"));
    assert!(fixed.contains("x: Integer;")); // single-char exempt
}

#[test]
fn fixture_interface_prefix_fix() {
    let (fixed, count) = fix_fixture("tests/fixtures/fix/interface_prefix.pas");
    assert!(count > 0);
    assert!(fixed.contains("IPrintable = interface"));
    assert!(fixed.contains("TDoc = class(TObject, IPrintable)"));
}

#[test]
fn fixture_combined_fix() {
    let (fixed, count) = fix_fixture("tests/fixtures/fix/combined.pas");
    assert!(count > 0);
    assert!(fixed.contains("TMyClass = class(TObject)"));
    assert!(fixed.contains("MAX_RETRIES = 3;"));
    assert!(fixed.contains("retryCount: Integer;"));
}

#[test]
fn fixture_suppressed_not_fixed() {
    let (fixed, count) = fix_fixture("tests/fixtures/fix/suppressed.pas");
    // Both declarations are suppressed — nothing to fix
    assert_eq!(count, 0);
    assert!(fixed.contains("MyClass = class(TObject)"));
    assert!(fixed.contains("maxSize = 100;"));
}

/// After fixing, linting should produce zero naming violations.
#[test]
fn fix_then_lint_produces_no_naming_violations() {
    let source = r#"unit Test;

interface

type
  MyClass = class(TObject)
  public
    procedure DoWork;
  end;

const
  maxRetries = 3;

implementation

procedure MyClass.DoWork;
var
  RetryCount: Integer;
begin
  RetryCount := maxRetries;
end;

end."#;
    let fixed = fix_source(source);
    let diagnostics = lint_source(&fixed);
    let naming_issues: Vec<_> = diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d.rule_id.as_str(),
                "type-prefix"
                    | "interface-prefix"
                    | "constant-naming"
                    | "local-variable-naming"
                    | "identifier-casing"
            )
        })
        .collect();
    assert!(
        naming_issues.is_empty(),
        "Expected zero naming violations after fix, got: {:?}",
        naming_issues
    );
}
