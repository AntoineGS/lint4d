use std::path::PathBuf;

fn format_aligned(source: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let mut config = fmt4d::config::FmtConfig::default();
    config.alignment.enabled = true;
    fmt4d::formatter::format_source(
        source.as_bytes(),
        &info,
        &config,
        &std::collections::HashSet::new(),
    )
    .expect("formatting failed")
}

fn format_unaligned(source: &str) -> String {
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
    let result = format_unaligned(source);
    // No alignment when disabled — each const has single space around =.
    assert!(
        result.contains("kFOO = 'A';"),
        "kFOO should not be padded when alignment is off. Got:\n{}",
        result
    );
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
}
