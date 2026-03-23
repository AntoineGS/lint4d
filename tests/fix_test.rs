use std::path::PathBuf;

use lint4d::config::Config;
use lint4d::engine::{parse_file, FileInfo};
use lint4d::engine::suppress::parse_suppressions;
use lint4d::fix::build_rename_map;

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
