use std::fs;
use std::path::PathBuf;

mod common;

/// Local helper: integration tests need `uses.group = true` so they can verify
/// the grouping/sorting behaviour against fixtures. This is intentionally
/// distinct from `common::format_source`, which uses the default config.
fn format_source(source: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig {
        uses: fmt4d::config::UsesConfig {
            group: true,
            ..fmt4d::config::UsesConfig::default()
        },
        ..fmt4d::config::FmtConfig::default()
    };
    fmt4d::formatter::format_source(
        source.as_bytes(),
        &info,
        &config,
        &std::collections::HashSet::new(),
    )
    .expect("formatting failed")
}

fn idempotency_check(source: &str) {
    let first = format_source(source);
    let second = format_source(&first);
    assert_eq!(first, second, "formatting is not idempotent");
}

fn roundtrip_ast_check(source: &str) {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let formatted = format_source(source);

    let (tree_before, _) =
        pascal_core::parser::parse_file(&info, source.as_bytes()).expect("parse original failed");
    let (tree_after, _) = pascal_core::parser::parse_file(&info, formatted.as_bytes())
        .expect("parse formatted failed");

    assert_eq!(
        tree_before.root_node().kind(),
        tree_after.root_node().kind(),
        "AST root kind changed"
    );
}

// ── Spacing fixture tests ────────────────────────────────────────

#[test]
fn spacing_fixture_matches_expected() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/spacing/spacing_input.pas").unwrap();
    let expected =
        fs::read_to_string("../../tests/fixtures/format/spacing/spacing_expected.pas").unwrap();
    let result = format_source(&input);
    assert_eq!(result, expected);
}

#[test]
fn spacing_is_idempotent() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/spacing/spacing_input.pas").unwrap();
    let formatted = format_source(&input);
    idempotency_check(&formatted);
}

#[test]
fn spacing_roundtrip_safe() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/spacing/spacing_input.pas").unwrap();
    roundtrip_ast_check(&input);
}

#[test]
fn spacing_adds_space_around_assign() {
    let result = format_source("unit T;\ninterface\nimplementation\nprocedure P;\nvar\n  x:Integer;\nbegin\n  x:=1;\nend;\nend.\n");
    assert!(result.contains("x := 1"), "should add spaces around :=");
}

#[test]
fn spacing_adds_space_after_colon() {
    let result = format_source(
        "unit T;\ninterface\nimplementation\nprocedure P;\nvar\n  x:Integer;\nbegin\nend;\nend.\n",
    );
    assert!(
        result.contains("x: Integer"),
        "should add space after colon in var declaration"
    );
}

#[test]
fn spacing_adds_space_after_comma() {
    let result = format_source(
        "unit T;\ninterface\nimplementation\nprocedure P;\nbegin\n  Foo(a,b,c);\nend;\nend.\n",
    );
    assert!(
        result.contains("Foo(a, b, c)"),
        "should add spaces after commas"
    );
}

// ── Uses fixture tests ───────────────────────────────────────────

#[test]
fn uses_fixture_matches_expected() {
    let input = fs::read_to_string("../../tests/fixtures/format/uses/uses_input.pas").unwrap();
    let expected =
        fs::read_to_string("../../tests/fixtures/format/uses/uses_expected.pas").unwrap();
    let result = format_source(&input);
    assert_eq!(result, expected);
}

#[test]
fn uses_is_idempotent() {
    let input = fs::read_to_string("../../tests/fixtures/format/uses/uses_input.pas").unwrap();
    let formatted = format_source(&input);
    idempotency_check(&formatted);
}

#[test]
fn uses_roundtrip_safe() {
    let input = fs::read_to_string("../../tests/fixtures/format/uses/uses_input.pas").unwrap();
    roundtrip_ast_check(&input);
}

#[test]
fn uses_groups_system_units() {
    let result = format_source("unit T;\ninterface\nuses\n  Forms, System.SysUtils, System.Classes;\nimplementation\nend.\n");
    // All are core units — should be in one group, alphabetically sorted
    assert!(result.contains("  Forms,\n  System.Classes,\n  System.SysUtils;\n"));
}

#[test]
fn uses_sorts_within_groups() {
    let result = format_source(
        "unit T;\ninterface\nuses\n  System.SysUtils, System.Classes;\nimplementation\nend.\n",
    );
    // Both are core units, sorted alphabetically
    assert!(result.contains("  System.Classes,\n  System.SysUtils;\n"));
}

// ── Comments fixture tests ───────────────────────────────────────

#[test]
fn comments_fixture_matches_expected() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/comments/comments_input.pas").unwrap();
    let expected =
        fs::read_to_string("../../tests/fixtures/format/comments/comments_expected.pas").unwrap();
    let result = format_source(&input);
    assert_eq!(result, expected);
}

#[test]
fn comments_is_idempotent() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/comments/comments_input.pas").unwrap();
    let formatted = format_source(&input);
    idempotency_check(&formatted);
}

#[test]
fn comments_roundtrip_safe() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/comments/comments_input.pas").unwrap();
    roundtrip_ast_check(&input);
}

#[test]
fn comments_preserves_inline_comment() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/comments/comments_input.pas").unwrap();
    let result = format_source(&input);
    assert!(
        result.contains("// assign x"),
        "inline comment should be preserved"
    );
}

#[test]
fn comments_preserves_leading_comment() {
    let input =
        fs::read_to_string("../../tests/fixtures/format/comments/comments_input.pas").unwrap();
    let result = format_source(&input);
    assert!(
        result.contains("// This is a header comment"),
        "header comment should be preserved"
    );
    assert!(
        result.contains("// before y"),
        "comment before y should be preserved"
    );
}

// ── External config integration tests ────────────────────────────

#[test]
fn uses_groups_with_external_prefixes() {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let mut config = fmt4d::config::FmtConfig::default();
    config.uses.group = true;
    config.uses.external_prefixes = vec!["Spring".to_string()];

    let source = "unit T;\ninterface\nuses\n  MyUnit, Spring.Container, System.SysUtils, Spring.Collections;\nimplementation\nend.\n";
    let result = fmt4d::formatter::format_source(
        source.as_bytes(),
        &info,
        &config,
        &std::collections::HashSet::new(),
    )
    .unwrap();

    // Core section
    assert!(
        result.contains("  System.SysUtils,\n"),
        "Core unit should be present. Got:\n{}",
        result
    );
    // External section (blank line before)
    assert!(
        result.contains("\n\n  Spring.Collections,\n  Spring.Container,\n"),
        "External section should be present. Got:\n{}",
        result
    );
    // Project section (blank line before)
    assert!(
        result.contains("\n\n  MyUnit;\n"),
        "Project section should be present. Got:\n{}",
        result
    );
}

#[test]
fn uses_groups_with_external_paths() {
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let vendor = dir.path().join("vendor");
    std::fs::create_dir_all(&vendor).unwrap();
    std::fs::write(vendor.join("SuperObject.pas"), "unit SuperObject;").unwrap();

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let mut config = fmt4d::config::FmtConfig::default();
    config.uses.group = true;
    config.uses.external_paths = vec!["vendor".to_string()];
    config.project_root = Some(dir.path().to_path_buf());

    let external_units = fmt4d::uses::scan_external_paths(dir.path(), &config.uses.external_paths);

    let source =
        "unit T;\ninterface\nuses\n  MyUnit, SuperObject, System.SysUtils;\nimplementation\nend.\n";
    let result =
        fmt4d::formatter::format_source(source.as_bytes(), &info, &config, &external_units)
            .unwrap();

    // Core
    assert!(
        result.contains("  System.SysUtils,\n"),
        "Core unit missing. Got:\n{}",
        result
    );
    // External (blank line before)
    assert!(
        result.contains("\n\n  SuperObject,\n"),
        "External unit missing. Got:\n{}",
        result
    );
    // Project (blank line before)
    assert!(
        result.contains("\n\n  MyUnit;\n"),
        "Project unit missing. Got:\n{}",
        result
    );
}

#[test]
fn project_files_override_external_scan() {
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    // Utils.pas exists in BOTH external and project trees.
    let vendor = dir.path().join("vendor");
    std::fs::create_dir_all(&vendor).unwrap();
    std::fs::write(vendor.join("Utils.pas"), "unit Utils;").unwrap();
    std::fs::write(vendor.join("SuperObject.pas"), "unit SuperObject;").unwrap();

    let common = dir.path().join("Common");
    std::fs::create_dir_all(&common).unwrap();
    std::fs::write(common.join("Utils.pas"), "unit Utils;").unwrap();

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let mut config = fmt4d::config::FmtConfig::default();
    config.uses.group = true;
    config.uses.external_paths = vec!["vendor".to_string()];
    config.project_root = Some(dir.path().to_path_buf());

    // Simulate the main.rs logic: scan external paths, then subtract
    // units that are also discovered project files.
    let mut external_units =
        fmt4d::uses::scan_external_paths(dir.path(), &config.uses.external_paths);
    // The project file list (from CLI paths / .dproj) includes Utils.pas.
    let project_files = vec![pascal_core::FileInfo::new(common.join("Utils.pas"))];
    for fi in &project_files {
        if let Some(stem) = fi.path.file_stem().and_then(|s| s.to_str()) {
            external_units.remove(&stem.to_lowercase());
        }
    }

    let source =
        "unit T;\ninterface\nuses\n  SuperObject, Utils, System.SysUtils;\nimplementation\nend.\n";
    let result =
        fmt4d::formatter::format_source(source.as_bytes(), &info, &config, &external_units)
            .unwrap();

    // Utils should be in the project group (last), not external.
    // SuperObject should remain external.
    let lines: Vec<&str> = result.lines().collect();
    let utils_line = lines
        .iter()
        .position(|l| l.trim().starts_with("Utils"))
        .unwrap();
    let super_line = lines
        .iter()
        .position(|l| l.trim().starts_with("SuperObject"))
        .unwrap();
    assert!(
        utils_line > super_line,
        "Utils should be in project group (after external SuperObject). Got:\n{}",
        result
    );
}

// ── Cross-cutting integration tests ──────────────────────────────

#[test]
fn output_is_non_empty_and_valid() {
    let inputs = [
        "../../tests/fixtures/format/spacing/spacing_input.pas",
        "../../tests/fixtures/format/uses/uses_input.pas",
        "../../tests/fixtures/format/comments/comments_input.pas",
    ];
    for path in &inputs {
        let input = fs::read_to_string(path).unwrap();
        let result = format_source(&input);
        assert!(
            !result.is_empty(),
            "output should not be empty for {}",
            path
        );
        assert!(
            result.contains("unit"),
            "output should contain 'unit' for {}",
            path
        );
        assert!(
            result.contains("end."),
            "output should contain 'end.' for {}",
            path
        );
    }
}

#[test]
fn all_fixtures_are_idempotent() {
    let inputs = [
        "../../tests/fixtures/format/spacing/spacing_input.pas",
        "../../tests/fixtures/format/uses/uses_input.pas",
        "../../tests/fixtures/format/comments/comments_input.pas",
        "../../tests/fixtures/format/indent/basic_input.pas",
    ];
    for path in &inputs {
        let input = fs::read_to_string(path).unwrap();
        let formatted = format_source(&input);
        idempotency_check(&formatted);
    }
}

// ── Compiler directive preservation ─────────────────────────────

#[test]
fn uses_preserves_ifdef_block() {
    let input = "unit T;\ninterface\nuses\n  Windows, SysUtils,\n  {$IFDEF FOO}\n  SpecialUnit,\n  {$ELSE}\n  OtherUnit,\n  {$ENDIF}\n  Classes;\nimplementation\nend.\n";
    let result = format_source(input);
    // Unconditional units are sorted; IFDEF block is preserved
    assert!(
        result.contains("{$IFDEF FOO}"),
        "IFDEF directive missing:\n{}",
        result
    );
    assert!(
        result.contains("{$ELSE}"),
        "ELSE directive missing:\n{}",
        result
    );
    assert!(
        result.contains("{$ENDIF}"),
        "ENDIF directive missing:\n{}",
        result
    );
    assert!(
        result.contains("SpecialUnit"),
        "SpecialUnit missing:\n{}",
        result
    );
    assert!(
        result.contains("OtherUnit"),
        "OtherUnit missing:\n{}",
        result
    );
}

#[test]
fn uses_preserves_ifdef_idempotent() {
    let input = "unit T;\ninterface\nuses\n  Windows, SysUtils,\n  {$IFDEF FOO}\n  SpecialUnit,\n  {$ELSE}\n  OtherUnit,\n  {$ENDIF}\n  Classes;\nimplementation\nend.\n";
    let formatted = format_source(input);
    idempotency_check(&formatted);
}

#[test]
fn uses_preserves_standalone_directive() {
    let input = "unit T;\ninterface\nuses\n  {$I compilers.inc}\n  SysUtils, Classes;\nimplementation\nend.\n";
    let result = format_source(input);
    assert!(
        result.contains("{$I compilers.inc}"),
        "Include directive missing:\n{}",
        result
    );
    assert!(result.contains("Classes"), "Classes missing:\n{}", result);
    assert!(result.contains("SysUtils"), "SysUtils missing:\n{}", result);
}

#[test]
fn uses_no_directives_unchanged() {
    // Regression: existing behaviour with no directives should be identical
    let input = "unit T;\ninterface\nuses\n  Forms, System.SysUtils, System.Classes;\nimplementation\nend.\n";
    let result = format_source(input);
    assert!(
        result.contains("  Forms,\n  System.Classes,\n  System.SysUtils;\n"),
        "Got:\n{}",
        result
    );
}

#[test]
fn uses_preserves_pre_uses_directive() {
    let input = "unit T;\ninterface\n{$I MDCompilers.inc}\nuses\n  SysUtils, Classes;\nimplementation\nend.\n";
    let result = format_source(input);
    assert!(
        result.contains("{$I MDCompilers.inc}"),
        "Pre-uses include directive missing:\n{}",
        result
    );
    // The directive should appear before "uses"
    let dir_pos = result.find("{$I MDCompilers.inc}").unwrap();
    let uses_pos = result.find("uses").unwrap();
    assert!(
        dir_pos < uses_pos,
        "Directive should appear before 'uses':\n{}",
        result
    );
}

// ── ppBlock directive preservation ────────────────────────────────

#[test]
fn ppblock_var_section() {
    let input = "\
unit T;
interface
implementation
procedure Foo;
var
  Normal: Integer;
  {$IFDEF DEBUG}
  DebugVar: String;
  {$ENDIF}
begin
end;
end.
";
    let result = format_source(input);
    assert!(
        result.contains("{$IFDEF DEBUG}"),
        "IFDEF missing:\n{}",
        result
    );
    assert!(
        result.contains("DebugVar: String;"),
        "DebugVar missing:\n{}",
        result
    );
    assert!(result.contains("{$ENDIF}"), "ENDIF missing:\n{}", result);
    idempotency_check(&result);
}

#[test]
fn ppblock_const_section() {
    let input = "\
unit T;
interface
implementation
procedure Foo;
const
  {$IFDEF PROD}
  Timeout = 30000;
  {$ELSE}
  Timeout = 5000;
  {$ENDIF}
begin
end;
end.
";
    let result = format_source(input);
    assert!(
        result.contains("{$IFDEF PROD}"),
        "IFDEF missing:\n{}",
        result
    );
    assert!(
        result.contains("Timeout = 30000;"),
        "PROD const missing:\n{}",
        result
    );
    assert!(result.contains("{$ELSE}"), "ELSE missing:\n{}", result);
    assert!(
        result.contains("Timeout = 5000;"),
        "DEV const missing:\n{}",
        result
    );
    assert!(result.contains("{$ENDIF}"), "ENDIF missing:\n{}", result);
    idempotency_check(&result);
}

#[test]
fn ppblock_statements() {
    let input = "\
unit T;
interface
implementation
procedure Foo;
begin
  SetUp;
  {$IFDEF LOGGING}
  Log('entered');
  {$ENDIF}
  DoWork;
end;
end.
";
    let result = format_source(input);
    assert!(
        result.contains("{$IFDEF LOGGING}"),
        "IFDEF missing:\n{}",
        result
    );
    assert!(result.contains("Log("), "Log call missing:\n{}", result);
    assert!(result.contains("{$ENDIF}"), "ENDIF missing:\n{}", result);
    assert!(result.contains("SetUp"), "SetUp missing:\n{}", result);
    assert!(result.contains("DoWork"), "DoWork missing:\n{}", result);
    idempotency_check(&result);
}

#[test]
fn ppblock_class_fields() {
    let input = "\
unit T;
interface
type
  TMyClass = class
  private
    FName: string;
    {$IFDEF EXTENDED_DEBUG}
    FDebugInfo: string;
    {$ENDIF}
  public
    procedure DoSomething;
  end;
implementation
end.
";
    let result = format_source(input);
    assert!(
        result.contains("{$IFDEF EXTENDED_DEBUG}"),
        "IFDEF missing:\n{}",
        result
    );
    assert!(
        result.contains("FDebugInfo: string;"),
        "field missing:\n{}",
        result
    );
    assert!(result.contains("{$ENDIF}"), "ENDIF missing:\n{}", result);
    idempotency_check(&result);
}

#[test]
fn ppblock_interface_declarations() {
    let input = "\
unit T;
interface
{$IFDEF EXTRA}
var
  GlobalVar: Integer;
{$ENDIF}
implementation
end.
";
    let result = format_source(input);
    assert!(
        result.contains("{$IFDEF EXTRA}"),
        "IFDEF missing:\n{}",
        result
    );
    assert!(result.contains("GlobalVar"), "var missing:\n{}", result);
    assert!(result.contains("{$ENDIF}"), "ENDIF missing:\n{}", result);
    idempotency_check(&result);
}

#[test]
fn ppblock_initialization() {
    let input = "\
unit T;
interface
implementation
initialization
  {$IFDEF REGISTER}
  RegisterComponents;
  {$ENDIF}
end.
";
    let result = format_source(input);
    assert!(
        result.contains("{$IFDEF REGISTER}"),
        "IFDEF missing:\n{}",
        result
    );
    assert!(
        result.contains("RegisterComponents"),
        "call missing:\n{}",
        result
    );
    assert!(result.contains("{$ENDIF}"), "ENDIF missing:\n{}", result);
    idempotency_check(&result);
}

#[test]
fn ppblock_nested() {
    let input = "\
unit T;
interface
implementation
procedure Foo;
var
  {$IFDEF WINDOWS}
  WinHandle: THandle;
  {$IFDEF WIN64}
  ExtraPtr: Pointer;
  {$ENDIF}
  {$ENDIF}
begin
end;
end.
";
    let result = format_source(input);
    assert!(
        result.contains("{$IFDEF WINDOWS}"),
        "outer IFDEF missing:\n{}",
        result
    );
    assert!(
        result.contains("{$IFDEF WIN64}"),
        "inner IFDEF missing:\n{}",
        result
    );
    assert!(
        result.contains("WinHandle"),
        "WinHandle missing:\n{}",
        result
    );
    assert!(result.contains("ExtraPtr"), "ExtraPtr missing:\n{}", result);
    let endif_count = result.matches("{$ENDIF}").count();
    assert_eq!(
        endif_count, 2,
        "Expected 2 ENDIFs, got {}:\n{}",
        endif_count, result
    );
    idempotency_check(&result);
}

#[test]
fn ppblock_elseif_chain() {
    let input = "\
unit T;
interface
implementation
procedure Foo;
var
  {$IF DEFINED(WIN64)}
  Handle: Int64;
  {$ELSEIF DEFINED(WIN32)}
  Handle: Integer;
  {$ELSE}
  Handle: LongInt;
  {$ENDIF}
begin
end;
end.
";
    let result = format_source(input);
    assert!(
        result.contains("{$IF DEFINED(WIN64)}"),
        "IF missing:\n{}",
        result
    );
    assert!(
        result.contains("{$ELSEIF DEFINED(WIN32)}"),
        "ELSEIF missing:\n{}",
        result
    );
    assert!(result.contains("{$ELSE}"), "ELSE missing:\n{}", result);
    assert!(result.contains("{$ENDIF}"), "ENDIF missing:\n{}", result);
    idempotency_check(&result);
}

#[test]
fn standalone_directive_preserved() {
    let input = "\
unit T;
interface
implementation
{$R *.dfm}
procedure Foo;
begin
end;
end.
";
    let result = format_source(input);
    assert!(
        result.contains("{$R *.dfm}"),
        "standalone directive missing:\n{}",
        result
    );
    idempotency_check(&result);
}
