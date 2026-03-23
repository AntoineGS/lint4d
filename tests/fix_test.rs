use std::path::PathBuf;

use lint4d::config::Config;
use lint4d::engine::{parse_file, FileInfo};
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
