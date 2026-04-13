use std::path::PathBuf;

mod common;
use common::{format_aligned, format_source, idempotency_check_aligned};

// ── Constant alignment ───────────────────────────────────────────────

#[test]
fn const_alignment_basic() {
    let source = "\
unit Test;
interface
const
  kFOO = 'A';
  kLONGER_NAME = 'B';
  kBAZ = 'C';
implementation
end.
";
    let result = format_aligned(source);
    // All = signs should align to the longest name.
    assert!(
        result.contains("kFOO         = 'A';"),
        "kFOO should be padded. Got:\n{}",
        result
    );
    assert!(
        result.contains("kLONGER_NAME = 'B';"),
        "kLONGER_NAME should have normal spacing. Got:\n{}",
        result
    );
    assert!(
        result.contains("kBAZ         = 'C';"),
        "kBAZ should be padded. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

#[test]
fn const_alignment_disabled_by_default() {
    let source = "\
unit Test;
interface
const
  kFOO = 'A';
  kLONGER_NAME = 'B';
implementation
end.
";
    let result = format_source(source);
    // No alignment when disabled — each const has single space around =.
    assert!(
        result.contains("kFOO = 'A';"),
        "kFOO should not be padded when alignment is off. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

#[test]
fn const_alignment_with_trailing_comments() {
    let source = "\
unit Test;
interface
const
  kFOO = 'A'; // first
  kLONGER = 'B'; // second
  kBAZ = 'C';
implementation
end.
";
    let result = format_aligned(source);
    // Values should align AND trailing comments should align.
    assert!(
        result.contains("kFOO    = 'A';"),
        "kFOO should be padded. Got:\n{}",
        result
    );
    assert!(
        result.contains("kLONGER = 'B';"),
        "kLONGER should have normal spacing. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

#[test]
fn const_alignment_blank_line_breaks_group() {
    let source = "\
unit Test;
interface
const
  kA = 1;
  kBB = 2;

  kC = 3;
  kDDD = 4;
implementation
end.
";
    let result = format_aligned(source);
    // Group 1: kA and kBB align.
    // Group 2: kC and kDDD align (separately).
    assert!(
        result.contains("kA  = 1;"),
        "kA should pad to kBB. Got:\n{}",
        result
    );
    assert!(
        result.contains("kBB = 2;"),
        "kBB is longest in group 1. Got:\n{}",
        result
    );
    assert!(
        result.contains("kC   = 3;"),
        "kC should pad to kDDD. Got:\n{}",
        result
    );
    assert!(
        result.contains("kDDD = 4;"),
        "kDDD is longest in group 2. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

// ── Variable alignment ──────────────────────────────────────────────

#[test]
fn var_alignment_basic() {
    let source = "\
unit Test;
interface
implementation
procedure Foo;
var
  Count: Integer;
  LongVarName: string;
begin
end;
end.
";
    let result = format_aligned(source);
    assert!(
        result.contains("Count      : Integer;"),
        "Count should be padded. Got:\n{}",
        result
    );
    assert!(
        result.contains("LongVarName: string;"),
        "LongVarName should have normal spacing. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

// ── Type alias alignment ────────────────────────────────────────────

#[test]
fn type_alias_alignment_basic() {
    let source = "\
unit Test;
interface
type
  TShort = Integer;
  TVeryLongTypeName = string;
implementation
end.
";
    let result = format_aligned(source);
    assert!(
        result.contains("TShort            = Integer;"),
        "TShort should be padded. Got:\n{}",
        result
    );
    assert!(
        result.contains("TVeryLongTypeName = string;"),
        "TVeryLongTypeName should have normal spacing. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

// ── Enum type preservation with alignment ───────────────────────────

#[test]
fn enum_type_not_collapsed_with_alignment() {
    // Enum types that overflow should keep one-value-per-line layout
    // even when alignment is enabled.
    let source = "\
unit Test;
interface
type
  TSQLTokenKind = (
    tkIdentifier,
    tkNumber,
    tkString,
    tkOperator,
    tkLParen,
    tkRParen,
    tkComma,
    tkDot,
    tkSemicolon,
    tkStar,
    tkEOF
  );
implementation
end.
";
    let result = format_aligned(source);
    let enum_lines: Vec<_> = result
        .lines()
        .filter(|l| l.trim_start().starts_with("tk"))
        .collect();
    assert!(
        enum_lines.len() >= 11,
        "enum values should stay one-per-line with alignment enabled:\n{}",
        result
    );
    // Idempotency
    let result2 = format_aligned(&result);
    assert_eq!(result, result2, "enum with alignment should be idempotent");
    idempotency_check_aligned(source);
}

// ── Field alignment ─────────────────────────────────────────────────

#[test]
fn field_alignment_basic() {
    let source = "\
unit Test;
interface
type
  TMyClass = class
  private
    FValue: Integer;
    FLongName: string;
  end;
implementation
end.
";
    let result = format_aligned(source);
    assert!(
        result.contains("FValue   : Integer;"),
        "FValue should be padded. Got:\n{}",
        result
    );
    assert!(
        result.contains("FLongName: string;"),
        "FLongName should have normal spacing. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

// ── Record field alignment ──────────────────────────────────────────

#[test]
fn record_field_alignment() {
    let source = "\
unit Test;
interface
type
  TAuditTypeDesc = record
    auditId: string;
    auditDesc: string;
  end;
implementation
end.
";
    let result = format_aligned(source);
    assert!(
        result.contains("auditId  : string;"),
        "auditId should be padded in record. Got:\n{}",
        result
    );
    assert!(
        result.contains("auditDesc: string;"),
        "auditDesc should have normal spacing in record. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

#[test]
fn record_field_alignment_mixed_types() {
    let source = "\
unit Test;
interface
type
  TMyRecord = record
    ID: Integer;
    Name: string;
    LongFieldName: Boolean;
  end;
implementation
end.
";
    let result = format_aligned(source);
    assert!(
        result.contains("ID           : Integer;"),
        "ID should be padded. Got:\n{}",
        result
    );
    assert!(
        result.contains("LongFieldName: Boolean;"),
        "LongFieldName should have normal spacing. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

// ── Outlier detection ───────────────────────────────────────────────

#[test]
fn const_alignment_outlier_detection() {
    let source = "\
unit Test;
interface
const
  kA = 1;
  kB = 2;
  kC = 3;
  kD = 4;
  kVERY_EXTREMELY_RIDICULOUSLY_LONG_CONSTANT_NAME = 5;
  kE = 6;
implementation
end.
";
    let result = format_aligned(source);
    // The outlier should NOT cause excessive padding for the others.
    // kA..kE should align to each other, not to the outlier.
    assert!(
        result.contains("kA = 1;"),
        "kA should align to kB/kC/kD/kE, not to outlier. Got:\n{}",
        result
    );
    // The outlier itself should have normal single-space formatting.
    assert!(
        result.contains("kVERY_EXTREMELY_RIDICULOUSLY_LONG_CONSTANT_NAME = 5;"),
        "Outlier should have normal spacing. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

// ── Property alignment ──────────────────────────────────────────────

#[test]
fn property_alignment_basic() {
    let source = "\
unit Test;
interface
type
  TMyClass = class
  public
    property Name: string read FName write SetName;
    property Age: Integer read FAge;
  end;
implementation
end.
";
    let result = format_aligned(source);
    assert!(
        result.contains("property Name: string  read FName write SetName;")
            || result.contains("property Name : string  read FName  write SetName;"),
        "Properties should be aligned. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

// ── Comment alignment ───────────────────────────────────────────────

#[test]
fn const_alignment_comment_column() {
    let source = "\
unit Test;
interface
const
  kA = 1; // first
  kBB = 2; // second
  kCCC = 3;
implementation
end.
";
    let mut config = fmt4d::config::FmtConfig::default();
    config.alignment.enabled = true;
    config.alignment.comments = true;
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let result = fmt4d::formatter::format_source(
        source.as_bytes(),
        &info,
        &config,
        &std::collections::HashSet::new(),
    )
    .expect("formatting failed");

    // Comments should be aligned to the same column.
    let lines: Vec<&str> = result.lines().collect();
    let comment_positions: Vec<Option<usize>> = lines
        .iter()
        .filter(|l| l.contains("= "))
        .map(|l| l.find("//"))
        .collect();
    // The two comment lines should start at the same column.
    let with_comments: Vec<usize> = comment_positions.iter().filter_map(|p| *p).collect();
    if with_comments.len() >= 2 {
        assert_eq!(
            with_comments[0], with_comments[1],
            "Trailing comments should align. Got:\n{}",
            result
        );
    }
    idempotency_check_aligned(source);
}

// ── Selective disable ───────────────────────────────────────────────

#[test]
fn alignment_constants_only() {
    let source = "\
unit Test;
interface
const
  kFOO = 'A';
  kLONGER_NAME = 'B';
implementation
procedure Foo;
var
  Short: Integer;
  VeryLongVar: string;
begin
end;
end.
";
    let mut config = fmt4d::config::FmtConfig::default();
    config.alignment.enabled = true;
    config.alignment.variables = false; // disable var alignment
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let result = fmt4d::formatter::format_source(
        source.as_bytes(),
        &info,
        &config,
        &std::collections::HashSet::new(),
    )
    .expect("formatting failed");

    // Constants should be aligned.
    assert!(
        result.contains("kFOO         = 'A';"),
        "Constants should be aligned. Got:\n{}",
        result
    );
    // Variables should NOT be aligned.
    assert!(
        result.contains("Short: Integer;"),
        "Variables should not be aligned. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

// ── Var with initializer ────────────────────────────────────────────

#[test]
fn var_alignment_with_initializer() {
    let source = "\
unit Test;
interface
implementation
procedure Foo;
var
  Count: Integer = 0;
  Name: string = '';
  LongVarName: Currency = 0;
begin
end;
end.
";
    let result = format_aligned(source);
    // All colons and all = should align.
    assert!(
        result.contains("Count      : Integer  = 0;"),
        "Count should be padded on name and type. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

// ── Idempotency ─────────────────────────────────────────────────────

#[test]
fn alignment_is_idempotent() {
    let source = "\
unit Test;
interface
const
  kFOO = 'A';
  kLONGER_NAME = 'B';
  kBAZ = 'C';
implementation
end.
";
    let first = format_aligned(source);
    let second = format_aligned(&first);
    assert_eq!(first, second, "Alignment should be idempotent");
    idempotency_check_aligned(source);
}

#[test]
fn alignment_idempotent_with_fields() {
    let source = "\
unit Test;
interface
type
  TMyClass = class
  private
    FValue: Integer;
    FLongName: string;
    FX: Boolean;
  end;
implementation
end.
";
    let first = format_aligned(source);
    let second = format_aligned(&first);
    assert_eq!(first, second, "Field alignment should be idempotent");
    idempotency_check_aligned(source);
}

#[test]
fn debug_const_alignment_real_world() {
    let source = "\
unit Test;
interface
const
  kLaunchProcessorHandle = 'LaunchProcessorHandle';
  kLaunchProcessorPath         = 'LaunchProcessorPath';
  kLaunchProcessor             = 'LaunchProcessor';
  kLaunchProcessorChoiceBDLess = 'LaunchProcessorChoiceBDLess';
  kConfigProcessor             = 'ConfigProcessor';
implementation
end.
";
    let result = format_aligned(source);
    eprintln!("=== ALIGNED OUTPUT ===");
    for line in result.lines() {
        eprintln!("{}", line);
    }
    eprintln!("=== END ===");
    // All = signs should be at the same column
    assert!(
        result.contains("kLaunchProcessorHandle       = 'LaunchProcessorHandle';")
            || result.contains("kLaunchProcessorHandle         = 'LaunchProcessorHandle';"),
        "kLaunchProcessorHandle should be padded to align with kLaunchProcessorChoiceBDLess. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

// ── Comma-separated var expansion ──────────────────────────────────

#[test]
fn var_comma_expand_splits_into_separate_lines() {
    let source = "\
unit Test;
interface
implementation
procedure Foo;
var
  I, J, K: Integer;
  FoundTable: string;
begin
end;
end.
";
    let result = format_aligned(source);
    // Each identifier should be on its own line with the type repeated.
    assert!(
        result.contains("I         : Integer;"),
        "I should be expanded and aligned. Got:\n{}",
        result
    );
    assert!(
        result.contains("J         : Integer;"),
        "J should be expanded and aligned. Got:\n{}",
        result
    );
    assert!(
        result.contains("K         : Integer;"),
        "K should be expanded and aligned. Got:\n{}",
        result
    );
    assert!(
        result.contains("FoundTable: string;"),
        "FoundTable should be aligned. Got:\n{}",
        result
    );
    // Must not contain the original comma-separated form.
    assert!(
        !result.contains("I, J, K"),
        "Should not contain comma-separated identifiers. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

#[test]
fn var_comma_expand_is_idempotent() {
    let source = "\
unit Test;
interface
implementation
procedure Foo;
var
  I, J, K: Integer;
  FoundTable: string;
begin
end;
end.
";
    let result = format_aligned(source);
    let result2 = format_aligned(&result);
    assert_eq!(result, result2, "Comma var expansion should be idempotent");
    idempotency_check_aligned(source);
}

// ── Single-line class declarations should not get blank lines ──────

#[test]
fn type_single_line_class_no_blank_lines() {
    let source = "\
unit Test;
interface
type
  EUnresolvedMacros = class(Exception);
  EUsageNotFound = class(Exception);
  EDatabaseUnreachable = class(Exception);
  ERecordNotFound = class(Exception);
  EValidationError = class(Exception);
implementation
end.
";
    let result = format_aligned(source);
    // No blank lines should be inserted between single-line class declarations.
    assert!(
        !result.contains("class(Exception);\n\n  E"),
        "Should not have blank lines between single-line class decls. Got:\n{}",
        result
    );
    // Idempotency check.
    let result2 = format_aligned(&result);
    assert_eq!(
        result, result2,
        "Single-line class alignment should be idempotent"
    );
    idempotency_check_aligned(source);
}

// ── Property alignment: read/write columns ─────────────────────────

#[test]
fn property_alignment_read_write_columns() {
    let source = "\
unit Test;
interface
type
  TFoo = class
  private
    FGetTableFieldsProc: TGetTableFieldsProc;
    FDatabaseName: string;
  published
    property GetTableFieldsProc: TGetTableFieldsProc read FGetTableFieldsProc write FGetTableFieldsProc;
    property DatabaseName: string read FDatabaseName write FDatabaseName;
  end;
implementation
end.
";
    let result = format_aligned(source);
    // The `write` keywords must align across both property lines.
    let lines: Vec<&str> = result.lines().collect();
    let write_cols: Vec<usize> = lines.iter().filter_map(|line| line.find("write")).collect();
    assert!(
        write_cols.len() == 2,
        "Expected 2 lines with 'write', got {}. Output:\n{}",
        write_cols.len(),
        result
    );
    assert_eq!(
        write_cols[0], write_cols[1],
        "write keywords should be at the same column ({} vs {}). Output:\n{}",
        write_cols[0], write_cols[1], result
    );
    idempotency_check_aligned(source);
}

#[test]
fn property_alignment_write_only_aligns_with_read_write() {
    let source = "\
unit Test;
interface
type
  TFoo = class
  published
    property ComputerName: string read FComputerName write FComputerName;
    property FileVersion: string read FFileVersion write FFileVersion;
    property AuditType: TAuditType write SetAuditType;
  end;
implementation
end.
";
    let result = format_aligned(source);
    // The `write` keywords must align across all property lines,
    // even when one property has no `read` specifier.
    let lines: Vec<&str> = result.lines().collect();
    let write_cols: Vec<usize> = lines.iter().filter_map(|line| line.find("write")).collect();
    assert_eq!(
        write_cols.len(),
        3,
        "Expected 3 lines with 'write', got {}. Output:\n{}",
        write_cols.len(),
        result
    );
    assert_eq!(
        write_cols[0], write_cols[1],
        "write columns 1 and 2 should match ({} vs {}). Output:\n{}",
        write_cols[0], write_cols[1], result
    );
    assert_eq!(
        write_cols[0], write_cols[2],
        "write columns 1 and 3 should match ({} vs {}). Output:\n{}",
        write_cols[0], write_cols[2], result
    );
    // Idempotency.
    let second = format_aligned(&result);
    assert_eq!(
        result, second,
        "Write-only property alignment should be idempotent"
    );
    idempotency_check_aligned(source);
}

// ── Leading comment on complex type — no spurious blank line ────────

#[test]
fn type_complex_with_leading_comment_no_blank_line() {
    let source = "\
unit Test;
interface
type
  /// Wraps a cached TPromoRules instance with metadata
  TCachedRuleSet = class
  private
    FPromoRules: TPromoRules;
    FLoadedAt: TDateTime;
    FRuleCount: Integer;
  end;
implementation
end.
";
    let result = format_aligned(source);
    // No blank line should appear between `type` and the `///` comment.
    assert!(
        !result.contains("type\n\n"),
        "Should not have a blank line between type and leading /// comment. Got:\n{}",
        result
    );
    // The comment should immediately follow `type` (indented).
    assert!(
        result.contains("type\n  /// Wraps"),
        "/// comment should follow type on the next line. Got:\n{}",
        result
    );
    // Idempotency.
    let result2 = format_aligned(&result);
    assert_eq!(
        result, result2,
        "Complex type with /// comment should be idempotent"
    );
    idempotency_check_aligned(source);
}

// ── Alias keyword misparse ─────────────────────────────────────────

#[test]
fn var_named_alias_aligned_separate_lines() {
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
    let result = format_aligned(source);
    assert!(
        result.contains("LookupSql       : RawUtf8;\n"),
        "LookupSql should end its own aligned line. Got:\n{}",
        result
    );
    assert!(
        result.contains("Alias           : RawUtf8;\n"),
        "Alias should be on its own aligned line. Got:\n{}",
        result
    );
    idempotency_check_aligned(source);
}

#[test]
fn var_named_alias_aligned_idempotent() {
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
    let first = format_aligned(source);
    let second = format_aligned(&first);
    assert_eq!(first, second, "Aligned var with Alias should be idempotent");
    idempotency_check_aligned(source);
}

#[test]
fn var_named_alias_first_in_block_aligned() {
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
    let result = format_aligned(source);
    assert!(
        result.contains("Alias: Integer;\n"),
        "Alias as first var should be on its own line. Got:\n{}",
        result
    );
    let second = format_aligned(&result);
    assert_eq!(result, second, "Alias-first aligned should be idempotent");
    idempotency_check_aligned(source);
}

#[test]
fn var_named_alias_lowercase_aligned() {
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
    let result = format_aligned(source);
    assert!(
        result.contains("alias: String;\n"),
        "lowercase alias should be on its own line. Got:\n{}",
        result
    );
    let second = format_aligned(&result);
    assert_eq!(
        result, second,
        "lowercase alias aligned should be idempotent"
    );
    idempotency_check_aligned(source);
}
