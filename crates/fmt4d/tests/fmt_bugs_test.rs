//! Regression tests for formatting bugs discovered by running fmt4d on
//! ChainDriveAPI / Common/SQL.Parser.pas.
//!
//! Each test targets a specific issue.  Tests describe the CORRECT behaviour
//! and are expected to **fail** until the corresponding formatter fix lands.

use std::path::PathBuf;

fn format_source(source: &str) -> String {
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

// ── Bug 1: Character literal corruption ─────────────────────────
// #0, #9, #10, #13 etc. are reduced to bare `#`, producing code
// that will not compile.

#[test]
fn char_literal_hash_zero_preserved() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  C: Char;
begin
  C := #0;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("#0"),
        "char literal #0 was corrupted to bare #:\n{}",
        result
    );
}

#[test]
fn char_literal_hash_numbers_preserved() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  C: Char;
begin
  while CharInSet(C, [' ', #9, #13, #10]) do Next;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("#9"),
        "char literal #9 was corrupted:\n{}",
        result
    );
    assert!(
        result.contains("#13"),
        "char literal #13 was corrupted:\n{}",
        result
    );
    assert!(
        result.contains("#10"),
        "char literal #10 was corrupted:\n{}",
        result
    );
}

#[test]
fn char_literal_high_values_preserved() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if C = #32 then Exit;
  if C = #255 then Exit;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("#32"),
        "char literal #32 was corrupted:\n{}",
        result
    );
    assert!(
        result.contains("#255"),
        "char literal #255 was corrupted:\n{}",
        result
    );
}

// ── Bug 2: `override` directive split to its own line ───────────
// `destructor Destroy; override;` must stay on one line.

#[test]
fn override_stays_on_same_line_as_method() {
    let src = "\
unit T;
interface
type
  TFoo = class
  public
    destructor Destroy; override;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("destructor Destroy; override;"),
        "override directive was split to a separate line:\n{}",
        result
    );
}

#[test]
fn reintroduce_stays_on_same_line_as_method() {
    let src = "\
unit T;
interface
type
  TFoo = class
  public
    procedure DoWork; reintroduce;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("procedure DoWork; reintroduce;"),
        "reintroduce directive was split to a separate line:\n{}",
        result
    );
}

#[test]
fn virtual_abstract_stay_on_same_line() {
    let src = "\
unit T;
interface
type
  TFoo = class
  public
    procedure DoWork; virtual; abstract;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("procedure DoWork; virtual; abstract;"),
        "virtual/abstract directives were split:\n{}",
        result
    );
}

// ── Bug 3: Class ancestor list detached from `class` keyword ────
// `class(TObject, IInterface)` must not split the `(` to a new line.

#[test]
fn class_ancestor_list_stays_attached() {
    let src = "\
unit T;
interface
type
  TFoo = class(TObject, IInterface)
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("class(TObject, IInterface)"),
        "class ancestor list was detached from class keyword:\n{}",
        result
    );
}

#[test]
fn class_single_ancestor_stays_attached() {
    let src = "\
unit T;
interface
type
  TBar = class(TObject)
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("class(TObject)"),
        "single class ancestor was detached from class keyword:\n{}",
        result
    );
}

// ── Bug 4: Record field indentation lost ────────────────────────
// Fields inside a record must be indented one level deeper than
// the record keyword.

#[test]
fn record_fields_are_indented() {
    let src = "\
unit T;
interface
type
  TPoint = record
    X: Integer;
    Y: Integer;
  end;
implementation
end.
";
    let result = format_source(src);
    // Under `type` (indent 0), TPoint is at indent 2, fields should be at indent 4
    assert!(
        result.contains("    X: Integer;"),
        "record field X should be indented 4 spaces:\n{}",
        result
    );
    assert!(
        result.contains("    Y: Integer;"),
        "record field Y should be indented 4 spaces:\n{}",
        result
    );
}

#[test]
fn record_end_aligned_with_record_name() {
    let src = "\
unit T;
interface
type
  TPoint = record
    X: Integer;
  end;
implementation
end.
";
    let result = format_source(src);
    // `end` should be at same indent as TPoint (indent 2)
    let end_line = result.lines().find(|l| l.trim() == "end;").unwrap();
    assert!(
        end_line.starts_with("  end;"),
        "record end should be at indent 2, got: '{}'",
        end_line
    );
}

// ── Bug 5: Spaces injected inside array/string indexers ─────────
// `Arr[0]` must not become `Arr [0]`.

#[test]
fn no_space_before_array_indexer() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  Arr: array of Integer;
begin
  Arr[0] := 1;
  Arr[1] := Arr[0] + 1;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("Arr[0]"),
        "space was injected before array indexer bracket:\n{}",
        result
    );
    assert!(
        result.contains("Arr[1]"),
        "space was injected before array indexer bracket:\n{}",
        result
    );
    assert!(
        !result.contains("Arr ["),
        "space was injected before array indexer bracket:\n{}",
        result
    );
}

#[test]
fn no_space_between_array_keyword_and_bracket() {
    let src = "\
unit T;

interface

const
  kExecStatus: Array[TExecStatus] of integer = (0, 1, 2);

implementation

end.
";
    let result = format_source(src);
    assert!(
        result.contains("Array[TExecStatus]"),
        "space was injected between Array and [:\n{}",
        result
    );
}

#[test]
fn no_space_before_string_indexer() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  S: string;
begin
  if S[1] = 'A' then Exit;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("S[1]"),
        "space was injected before string indexer bracket:\n{}",
        result
    );
}

// ── Bug 6: Spaces injected inside generic angle brackets ────────
// `TList<Integer>` must not become `TList < Integer >`.

#[test]
fn no_spaces_in_generic_type_params() {
    let src = "\
unit T;
interface
uses
  Generics.Collections;
type
  TFoo = class
  private
    FList: TList<Integer>;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("TList<Integer>"),
        "spaces were injected inside generic brackets:\n{}",
        result
    );
}

#[test]
fn no_spaces_in_nested_generic_type_params() {
    let src = "\
unit T;
interface
uses
  Generics.Collections;
type
  TFoo = class
  private
    FDict: TObjectDictionary<string, TStringList>;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("TObjectDictionary<string, TStringList>"),
        "spaces were injected inside nested generic brackets:\n{}",
        result
    );
}

#[test]
fn no_spaces_in_generic_constructor_call() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  L: TList<Integer>;
begin
  L := TList<Integer>.Create;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("TList<Integer>.Create"),
        "spaces were injected in generic constructor call:\n{}",
        result
    );
}

// ── Bug 7: Continuation parameter indentation lost ──────────────
// When a parameter list wraps, the continuation line must be
// indented deeper than the method keyword, not flush with it.

#[test]
fn continuation_params_indented_in_declaration() {
    // Parameters must exceed 120 chars to force wrapping
    let src = "\
unit T;
interface
type
  TFoo = class
  public
    procedure DoSomethingVeryImportant(const AFirstParameterName: string; const ASecondParameterName: Integer; const AThirdParameterName: Boolean);
  end;
implementation
end.
";
    let result = format_source(src);
    // If the line exceeds max_line_length, the params should be wrapped
    // and the continuation line should be indented more than the method keyword.
    let lines: Vec<&str> = result.lines().collect();
    let method_line = lines
        .iter()
        .find(|l| l.contains("procedure DoSomethingVeryImportant"))
        .expect("method line not found");
    let method_line_len = method_line.len();
    // The original line is >120 chars, so wrapping should occur
    // Either all params are on one line (if it fits) or wrapped with indent
    if method_line_len > 120 {
        // Still too long — should have been wrapped
        panic!(
            "method declaration exceeds 120 chars and was not wrapped:\n{}",
            result
        );
    }
    // If wrapped, continuation lines should be indented
    let cont_lines: Vec<&&str> = lines
        .iter()
        .filter(|l| {
            (l.contains("ASecondParameterName") || l.contains("AThirdParameterName"))
                && !l.contains("procedure")
        })
        .collect();
    if !cont_lines.is_empty() {
        let method_indent = method_line.len() - method_line.trim_start().len();
        for cont_line in &cont_lines {
            let cont_indent = cont_line.len() - cont_line.trim_start().len();
            assert!(
                cont_indent > method_indent,
                "continuation param (indent {}) should be indented more than method (indent {}):\n{}",
                cont_indent,
                method_indent,
                result
            );
        }
    }
}

#[test]
fn continuation_params_indented_in_implementation() {
    // Parameters must exceed 120 chars to force wrapping
    let src = "\
unit T;
interface
implementation
procedure DoSomethingVeryImportant(const AFirstParameterName: string; const ASecondParameterName: Integer; const AThirdParameterName: Boolean);
begin
end;
end.
";
    let result = format_source(src);
    let lines: Vec<&str> = result.lines().collect();
    let method_line = lines
        .iter()
        .find(|l| l.contains("procedure DoSomethingVeryImportant"))
        .expect("method line not found");
    let method_line_len = method_line.len();
    if method_line_len > 120 {
        panic!(
            "method declaration exceeds 120 chars and was not wrapped:\n{}",
            result
        );
    }
    // If wrapped, continuation lines should be indented more than procedure keyword
    let cont_lines: Vec<&&str> = lines
        .iter()
        .filter(|l| {
            (l.contains("ASecondParameterName") || l.contains("AThirdParameterName"))
                && !l.contains("procedure")
        })
        .collect();
    if !cont_lines.is_empty() {
        for cont_line in &cont_lines {
            let cont_indent = cont_line.len() - cont_line.trim_start().len();
            assert!(
                cont_indent > 0,
                "continuation param should be indented in implementation section:\n{}",
                result
            );
        }
    }
}

// ── Bug 8: Space removed between keywords and `(` ──────────────
// `if (x > 0)` must not become `if(x > 0)`.

#[test]
fn space_between_if_and_paren() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  x: Integer;
begin
  if (x > 0) then Exit;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("if (x"),
        "space was removed between 'if' and '(':\n{}",
        result
    );
    assert!(
        !result.contains("if(x"),
        "space was removed between 'if' and '(':\n{}",
        result
    );
}

#[test]
fn space_between_while_and_paren() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  x: Integer;
begin
  while (x > 0) do Dec(x);
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("while (x"),
        "space was removed between 'while' and '(':\n{}",
        result
    );
    assert!(
        !result.contains("while(x"),
        "space was removed between 'while' and '(':\n{}",
        result
    );
}

#[test]
fn space_between_boolean_operator_and_paren() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  A, B: Boolean;
begin
  if (A) and (B) then Exit;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("and (B)"),
        "space was removed between 'and' and '(':\n{}",
        result
    );
    assert!(
        !result.contains("and(B)"),
        "space was removed between 'and' and '(':\n{}",
        result
    );
}

// ── Bug 9: Blank lines stripped too aggressively ────────────────
// Meaningful blank lines between sections should be preserved.

#[test]
fn blank_line_between_uses_and_type_section() {
    let src = "\
unit T;
interface
uses
  SysUtils;

type
  TFoo = class
  end;
implementation
end.
";
    let result = format_source(src);
    // There should be a blank line between the uses clause and type section
    assert!(
        result.contains("SysUtils;\n\ntype"),
        "blank line between uses and type section was stripped:\n{}",
        result
    );
}

#[test]
fn blank_line_between_type_declarations() {
    let src = "\
unit T;
interface
type
  TFirst = record
    X: Integer;
  end;

  TSecond = record
    Y: Integer;
  end;
implementation
end.
";
    let result = format_source(src);
    // There should be a blank line separating the two type declarations
    assert!(
        result.contains("end;\n\n  TSecond"),
        "blank line between type declarations was stripped:\n{}",
        result
    );
}

#[test]
fn blank_line_between_const_groups_preserved() {
    // A single blank line separating groups of const declarations
    // inside the same `const` section should be preserved — same
    // policy as other places in the codebase (class bodies, etc.).
    let src = "\
unit T;
interface
const
  kFoo = 'FOO';
  kBar = 'BAR';

  kBaz = 'BAZ';
  kQux = 'QUX';
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("kBar = 'BAR';\n\n  kBaz = 'BAZ';"),
        "blank line between const groups was stripped:\n{}",
        result
    );
}

#[test]
fn blank_line_between_var_groups_preserved() {
    // Same policy for `var` sections.
    let src = "\
unit T;
interface
var
  GFoo: Integer;
  GBar: Integer;

  GBaz: Integer;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("GBar: Integer;\n\n  GBaz: Integer;"),
        "blank line between var groups was stripped:\n{}",
        result
    );
}

#[test]
fn blank_line_before_section_comment_procedure() {
    // When two procedures are separated by a section comment, there should
    // be a blank line between them (the comment is attached to the second proc).
    let src = "\
unit T;
interface
implementation

procedure First;
begin
end;

{ TMyClass }

constructor TMyClass.Create;
begin
end;
end.
";
    let result = format_source(src);
    // There should be a blank line between end of First and the section comment
    assert!(
        result.contains("end;\n\n{ TMyClass }"),
        "blank line before section comment was stripped:\n{}",
        result
    );
}

// ── Bug 11: `case` first branch joined on the `of` line ────────
// `case C of 'A':` should be `case C of\n  'A':` — first branch
// on its own line.

#[test]
fn case_first_branch_on_new_line() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  C: Char;
begin
  case C of
    'A': Exit;
    'B': Exit;
  end;
end;
end.
";
    let result = format_source(src);
    // The `of` and first branch should NOT be on the same line
    assert!(
        !result.contains("of 'A'"),
        "first case branch was joined on the 'of' line:\n{}",
        result
    );
    // `of` should end its line
    let of_line = result
        .lines()
        .find(|l| l.contains("case") && l.contains("of"))
        .expect("case..of line not found");
    assert!(
        of_line.trim().ends_with("of"),
        "case..of line should end with 'of', got: '{}'",
        of_line
    );
}

// ── Bug 12: `case` branch indentation reduced ──────────────────
// Case branches should be indented one level inside the case block.

#[test]
fn case_branches_indented() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  C: Char;
begin
  case C of
    'A': Exit;
    'B': Exit;
  end;
end;
end.
";
    let result = format_source(src);
    // `case` is at indent 2 (inside begin..end), branches should be at indent 4
    let branch_a = result
        .lines()
        .find(|l| l.contains("'A'"))
        .expect("branch A not found");
    let branch_b = result
        .lines()
        .find(|l| l.contains("'B'"))
        .expect("branch B not found");
    let case_line = result
        .lines()
        .find(|l| l.trim().starts_with("case"))
        .expect("case line not found");
    let case_indent = case_line.len() - case_line.trim_start().len();
    let branch_a_indent = branch_a.len() - branch_a.trim_start().len();
    let branch_b_indent = branch_b.len() - branch_b.trim_start().len();
    assert!(
        branch_a_indent > case_indent,
        "branch A (indent {}) should be indented more than case (indent {}):\n{}",
        branch_a_indent,
        case_indent,
        result
    );
    assert!(
        branch_b_indent > case_indent,
        "branch B (indent {}) should be indented more than case (indent {}):\n{}",
        branch_b_indent,
        case_indent,
        result
    );
}

#[test]
fn case_end_aligned_with_case() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  I: Integer;
begin
  case I of
    1: Exit;
    2: Exit;
  end;
end;
end.
";
    let result = format_source(src);
    let case_line = result
        .lines()
        .find(|l| l.trim().starts_with("case"))
        .expect("case line not found");
    let case_indent = case_line.len() - case_line.trim_start().len();
    // Find the `end;` that closes the case (not the procedure end)
    // It should be at the same indent as `case`
    let end_lines: Vec<&str> = result.lines().filter(|l| l.trim() == "end;").collect();
    // The first end; after case should align with case
    let case_end = end_lines.iter().find(|l| {
        let indent = l.len() - l.trim_start().len();
        indent == case_indent
    });
    assert!(
        case_end.is_some(),
        "case-closing end should be at same indent ({}) as case keyword:\n{}",
        case_indent,
        result
    );
}

// ── Bug 14: Spaces added inside range syntax ────────────────────
// `'A'..'Z'` must not become `'A' .. 'Z'`.

#[test]
fn no_spaces_around_range_operator() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  C: Char;
begin
  if CharInSet(C, ['A'..'Z', 'a'..'z']) then Exit;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("'A'..'Z'"),
        "spaces were added around range operator (..):\n{}",
        result
    );
    assert!(
        result.contains("'a'..'z'"),
        "spaces were added around range operator (..):\n{}",
        result
    );
}

#[test]
fn no_spaces_around_numeric_range() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  I: Integer;
begin
  case I of
    0..9: Exit;
  end;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("0..9"),
        "spaces were added around numeric range operator:\n{}",
        result
    );
}

// ── Bug 15: Initialization/finalization sections not handled ────
// `initialization` and `finalization` unit sections must be treated
// like `interface`/`implementation`: keyword on its own line, body
// indented, blank line separating from preceding section.

#[test]
fn initialization_keyword_on_own_line() {
    let src = "\
unit T;
interface
implementation
initialization
  RegisterClass(TMyClass);
end.
";
    let result = format_source(src);
    // The keyword should be on its own line, not joined with the statement
    assert!(
        !result.contains("initialization RegisterClass"),
        "initialization keyword was joined with body statement:\n{}",
        result
    );
    let init_line = result
        .lines()
        .find(|l| l.trim() == "initialization")
        .expect("initialization keyword should be on its own line");
    assert_eq!(
        init_line.trim(),
        "initialization",
        "initialization line should contain only the keyword"
    );
}

#[test]
fn finalization_keyword_on_own_line() {
    let src = "\
unit T;
interface
implementation
initialization
  RegisterClass(TMyClass);
finalization
  UnregisterClass(TMyClass);
end.
";
    let result = format_source(src);
    assert!(
        !result.contains("finalization UnregisterClass"),
        "finalization keyword was joined with body statement:\n{}",
        result
    );
    let final_line = result
        .lines()
        .find(|l| l.trim() == "finalization")
        .expect("finalization keyword should be on its own line");
    assert_eq!(
        final_line.trim(),
        "finalization",
        "finalization line should contain only the keyword"
    );
}

#[test]
fn initialization_body_indented() {
    let src = "\
unit T;
interface
implementation
initialization
  RegisterClass(TMyClass);
end.
";
    let result = format_source(src);
    let reg_line = result
        .lines()
        .find(|l| l.contains("RegisterClass"))
        .expect("RegisterClass statement not found");
    let indent = reg_line.len() - reg_line.trim_start().len();
    assert!(
        indent >= 2,
        "initialization body should be indented (got {} spaces):\n{}",
        indent,
        result
    );
}

#[test]
fn initialization_multiple_statements() {
    let src = "\
unit T;
interface
implementation
initialization
  GProcessHandle := 0;
  GServerAvailable := False;
  GChecked := False;
finalization
  StopServer;
end.
";
    let result = format_source(src);
    // Each statement should be on its own line, indented
    assert!(
        result.contains("  GProcessHandle := 0;"),
        "first init statement not properly indented:\n{}",
        result
    );
    assert!(
        result.contains("  GServerAvailable := False;"),
        "second init statement not properly indented:\n{}",
        result
    );
    assert!(
        result.contains("  GChecked := False;"),
        "third init statement not properly indented:\n{}",
        result
    );
    assert!(
        result.contains("  StopServer;"),
        "finalization statement not properly indented:\n{}",
        result
    );
}

#[test]
fn blank_line_before_initialization() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
end;
initialization
  RegisterClass(TMyClass);
end.
";
    let result = format_source(src);
    // There should be a blank line separating implementation content from initialization
    assert!(
        result.contains("end;\n\ninitialization"),
        "missing blank line before initialization section:\n{}",
        result
    );
}

#[test]
fn blank_line_before_finalization() {
    let src = "\
unit T;
interface
implementation
initialization
  RegisterClass(TMyClass);
finalization
  UnregisterClass(TMyClass);
end.
";
    let result = format_source(src);
    // There should be a blank line between initialization and finalization
    assert!(
        result.contains(";\n\nfinalization"),
        "missing blank line before finalization section:\n{}",
        result
    );
}

#[test]
fn initialization_finalization_idempotent() {
    let src = "\
unit T;
interface
implementation
initialization
  GProcessHandle := 0;
  GServerAvailable := False;
finalization
  StopServer;
end.
";
    let first = format_source(src);
    let second = format_source(&first);
    assert_eq!(
        first, second,
        "initialization/finalization formatting is not idempotent.\nFirst:\n{}\nSecond:\n{}",
        first, second
    );
}

// ── Bug 16: Case `else` indentation and comments ──────────────
// The `else` in a case statement should align with `case`, not with
// the case branches.  Also, comments before `else` must not be dropped.

#[test]
fn case_else_aligned_with_case() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  C: Char;
begin
  case C of
    'A': Exit;
    'B': Exit;
  else
    Exit;
  end;
end;
end.
";
    let result = format_source(src);
    let case_line = result
        .lines()
        .find(|l| l.trim().starts_with("case"))
        .expect("case line not found");
    let case_indent = case_line.len() - case_line.trim_start().len();
    let else_line = result
        .lines()
        .find(|l| l.trim() == "else")
        .expect("else line not found");
    let else_indent = else_line.len() - else_line.trim_start().len();
    assert_eq!(
        else_indent, case_indent,
        "case-else (indent {}) should align with case keyword (indent {}):\n{}",
        else_indent, case_indent, result
    );
}

#[test]
fn case_else_body_indented() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  C: Char;
begin
  case C of
    'A': Exit;
  else
    WriteLn('default');
  end;
end;
end.
";
    let result = format_source(src);
    let else_line = result
        .lines()
        .find(|l| l.trim() == "else")
        .expect("else line not found");
    let else_indent = else_line.len() - else_line.trim_start().len();
    let body_line = result
        .lines()
        .find(|l| l.contains("WriteLn"))
        .expect("else body not found");
    let body_indent = body_line.len() - body_line.trim_start().len();
    assert!(
        body_indent > else_indent,
        "else body (indent {}) should be indented more than else (indent {}):\n{}",
        body_indent,
        else_indent,
        result
    );
}

#[test]
fn case_comment_before_else_preserved() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  C: Char;
begin
  case C of
    'A': Exit;
    // default handler
  else
    ReadChar;
  end;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("// default handler"),
        "comment before case-else was removed:\n{}",
        result
    );
}

#[test]
fn case_comment_before_end_preserved() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  I: Integer;
begin
  case I of
    1: Exit;
    // end of case
  end;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("// end of case"),
        "comment before case-end was removed:\n{}",
        result
    );
}

#[test]
fn initialization_only_no_finalization() {
    let src = "\
unit T;
interface
implementation
initialization
  TDUnitX.RegisterTestFixture(TMyTests);
end.
";
    let result = format_source(src);
    assert!(
        result.contains("initialization\n"),
        "initialization keyword should end its line:\n{}",
        result
    );
    assert!(
        result.contains("  TDUnitX.RegisterTestFixture(TMyTests);"),
        "init body should be indented:\n{}",
        result
    );
    assert!(
        result.contains("end.\n"),
        "unit should end with end.:\n{}",
        result
    );
}

// ── Bug 18: Var declarations collapsed onto single lines ───────
// Each `var` / `const` / field declaration must be on its own line,
// not concatenated: `A: Integer;B: string;...`.

#[test]
fn var_declarations_on_separate_lines() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  A: Integer;
  B: string;
  C: Boolean;
begin
end;
end.
";
    let result = format_source(src);
    assert!(
        !result.contains("Integer;  B:") && !result.contains("Integer;B:"),
        "var declarations were collapsed onto one line:\n{}",
        result
    );
    assert!(
        result.contains("  A: Integer;\n"),
        "var A should be on its own line:\n{}",
        result
    );
    assert!(
        result.contains("  B: string;\n"),
        "var B should be on its own line:\n{}",
        result
    );
    assert!(
        result.contains("  C: Boolean;\n"),
        "var C should be on its own line:\n{}",
        result
    );
}

#[test]
fn class_field_declarations_on_separate_lines() {
    let src = "\
unit T;
interface
type
  TFoo = class
  private
    FName: string;
    FAge: Integer;
    FActive: Boolean;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("    FName: string;\n"),
        "field FName should be on its own line:\n{}",
        result
    );
    assert!(
        result.contains("    FAge: Integer;\n"),
        "field FAge should be on its own line:\n{}",
        result
    );
    assert!(
        result.contains("    FActive: Boolean;\n"),
        "field FActive should be on its own line:\n{}",
        result
    );
}

// ── Bug 17: Long string concatenation not broken ───────────────
// A const string built via `+` that exceeds max_line_length should
// be broken across multiple lines at the `+` operators.

#[test]
fn long_string_concat_broken_at_plus() {
    let src = "\
unit T;
interface
implementation
const
  Keywords = 'ACTION,ACTIVE,ADD,ADMIN,AFTER,ALL,ALTER,AND,ANY,AS,ASC,ASCENDING,AT,AUTO' + ',AVG,BASE_NAME,BASED,BASENAME,BEFORE,BEGIN,BETWEEN,BLOB,BLOBEDIT,BUFFER,BY,CACHE';
end.
";
    let result = format_source(src);
    let long_lines: Vec<&str> = result.lines().filter(|l| l.len() > 120).collect();
    assert!(
        long_lines.is_empty(),
        "string concatenation line exceeds 120 chars and was not broken:\n{}",
        result
    );
    // The operator should lead the continuation line (break BEFORE operator).
    let cont_line = result.lines().find(|l| l.trim_start().starts_with("+ '"));
    assert!(
        cont_line.is_some(),
        "continuation of string concat should start with `+` on a new line:\n{}",
        result
    );
}

#[test]
fn long_string_concat_idempotent() {
    let src = "\
unit T;
interface
implementation
const
  Keywords = 'ACTION,ACTIVE,ADD,ADMIN,AFTER,ALL,ALTER,AND,ANY,AS,ASC,ASCENDING,AT,AUTO' + ',AVG,BASE_NAME,BASED,BASENAME,BEFORE,BEGIN,BETWEEN,BLOB,BLOBEDIT,BUFFER,BY,CACHE';
end.
";
    let first = format_source(src);
    let second = format_source(&first);
    assert_eq!(
        first, second,
        "string concatenation formatting is not idempotent.\nFirst:\n{}\nSecond:\n{}",
        first, second
    );
}

#[test]
fn short_string_concat_stays_on_one_line() {
    let src = "\
unit T;
interface
implementation
const
  Greeting = 'Hello,' +
    'World';
end.
";
    let result = format_source(src);
    // Short enough to fit on one line — should not be split
    assert!(
        result.contains("'Hello,' + 'World'"),
        "short string concat should stay on one line:\n{}",
        result
    );
}

#[test]
fn string_with_plus_inside_not_broken() {
    let src = "\
unit T;
interface
implementation
const
  Msg = 'Use operator + for addition';
end.
";
    let result = format_source(src);
    assert!(
        result.contains("'Use operator + for addition'"),
        "plus inside string literal was treated as break point:\n{}",
        result
    );
}

// ── Bug 19: `packed record` keyword split across lines ─────────
// `packed record` must stay on the same line.

#[test]
fn packed_record_stays_on_one_line() {
    let src = "\
unit T;
interface
type
  TPoint = packed record
    X: Integer;
    Y: Integer;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("packed record"),
        "packed and record were split across lines:\n{}",
        result
    );
    assert!(
        !result.contains("record\n    packed"),
        "packed was moved after record:\n{}",
        result
    );
}

// ── Bug 20: `class abstract` keyword split across lines ────────
// `class abstract` must stay on the same line.

#[test]
fn class_abstract_stays_on_one_line() {
    let src = "\
unit T;
interface
type
  TBase = class abstract
  public
    procedure DoWork; virtual; abstract;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("class abstract"),
        "class and abstract were split across lines:\n{}",
        result
    );
}

#[test]
fn class_sealed_stays_on_one_line() {
    let src = "\
unit T;
interface
type
  TFinal = class sealed
  public
    procedure DoWork;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("class sealed"),
        "class and sealed were split across lines:\n{}",
        result
    );
}

// ── Bug 21: Empty exception class semicolon on new line ─────────
// `EFoo = class(Exception);` must not become:
//   EFoo = class(Exception)
//   ;

#[test]
fn empty_exception_class_semicolon_same_line() {
    let src = "\
unit T;
interface
type
  EMyError = class(Exception);
  ENotFound = class(Exception);
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("class(Exception);"),
        "semicolon was split from empty class declaration:\n{}",
        result
    );
    assert!(
        result.contains("EMyError = class(Exception);"),
        "EMyError declaration was mangled:\n{}",
        result
    );
    assert!(
        result.contains("ENotFound = class(Exception);"),
        "ENotFound declaration was mangled:\n{}",
        result
    );
}

#[test]
fn empty_class_no_ancestor_no_trailing_newline() {
    let src = "\
unit T;
interface
type
  TMarker = class;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("TMarker = class;"),
        "empty class with no ancestor was mangled:\n{}",
        result
    );
}

// ── Bug: Case branches collapsed onto same line ────────────────
// Consecutive CASE_CASE branches were concatenated without Hardlines,
// producing `end;',':` instead of separate lines.

#[test]
fn case_branches_on_separate_lines() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  C: Char;
begin
  case C of
    ')':
    begin
      ReadChar;
      Exit;
    end;
    ',':
    begin
      ReadChar;
      Exit;
    end;
    '.':
    begin
      ReadChar;
      Exit;
    end;
  end;
end;
end.
";
    let result = format_source(src);
    // Each case branch must start on its own line — they must NOT be collapsed like end;',':
    assert!(
        !result.contains("end;'"),
        "case branches were collapsed onto same line as previous end:\n{}",
        result
    );
}

#[test]
fn case_simple_branches_on_separate_lines() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  I: Integer;
begin
  case I of
    1: WriteLn('one');
    2: WriteLn('two');
    3: WriteLn('three');
  end;
end;
end.
";
    let result = format_source(src);
    // Verify each branch is on its own line
    let branch_1 = result
        .lines()
        .find(|l| l.contains("1:"))
        .expect("branch 1 not found");
    let branch_2 = result
        .lines()
        .find(|l| l.contains("2:"))
        .expect("branch 2 not found");
    let branch_3 = result
        .lines()
        .find(|l| l.contains("3:"))
        .expect("branch 3 not found");
    // All three should be distinct lines
    assert_ne!(branch_1, branch_2, "branches 1 and 2 are on the same line");
    assert_ne!(branch_2, branch_3, "branches 2 and 3 are on the same line");
}

// ── Bug 14: Nested local functions run together ───────────────────
// Nested local procedures/functions inside a procedure definition
// must be separated by line breaks, not collapsed together.

#[test]
fn nested_local_functions_separated() {
    let src = "\
unit T;
interface
implementation
procedure Outer;

  function Inner1: Integer;
  begin
    Result := 1;
  end;

  function Inner2: Integer;
  begin
    Result := 2;
  end;

begin
  WriteLn(Inner1 + Inner2);
end;
end.
";
    let result = format_source(src);
    // Nested functions should be on separate lines, not run together
    assert!(
        !result.contains("end;function") && !result.contains("end;  function"),
        "nested functions were collapsed together:\n{}",
        result
    );
    // Each nested function should be present
    assert!(
        result.contains("function Inner1"),
        "Inner1 function missing:\n{}",
        result
    );
    assert!(
        result.contains("function Inner2"),
        "Inner2 function missing:\n{}",
        result
    );
}

#[test]
fn forward_declaration_stays_on_proc_line() {
    let src = "\
unit T;
interface
implementation
procedure Outer;
  procedure Inner; forward;

  procedure Helper;
  begin
  end;

  procedure Inner;
  begin
    Helper;
  end;

begin
end;
end.
";
    let result = format_source(src);
    // forward must stay on the same line as the declaration semicolon
    assert!(
        result.contains("procedure Inner; forward;"),
        "forward was split from procedure declaration:\n{}",
        result
    );
}

// ── Bug: Nested procedures must be indented ─────────────────────
// Nested procedures/functions declared inside another procedure must
// be indented one level relative to the parent so they align with
// local var section bodies.

#[test]
fn nested_procedure_indented_one_level() {
    let src = "\
unit T;
interface
implementation
procedure Outer;
var
  x: Integer;
  function Inner(const A: RawUtf8): RawUtf8;
  begin
    Result := A;
  end;
begin
  WriteLn(Inner(x));
end;
end.
";
    let result = format_source(src);
    // The nested function signature must be indented by one level (2 spaces)
    assert!(
        result.contains("\n  function Inner"),
        "nested function not indented:\n{}",
        result
    );
    // Its begin/end must also be at one indent level
    let lines: Vec<&str> = result.lines().collect();
    let inner_begin = lines
        .iter()
        .position(|l| l.trim() == "begin" && l.starts_with("  "))
        .expect("nested begin not found at indent 1");
    let inner_end = lines
        .iter()
        .position(|l| l.trim() == "end;" && l.starts_with("  "))
        .expect("nested end not found at indent 1");
    assert!(inner_begin < inner_end, "begin should come before end");
    // Body inside nested begin..end must be at two indent levels (4 spaces)
    assert!(
        result.contains("\n    Result := A;"),
        "nested body not at indent 2:\n{}",
        result
    );
}

#[test]
fn deeply_nested_procedure_indentation() {
    let src = "\
unit T;
interface
implementation
procedure Outer;

  function Middle: Integer;
  var
    y: Integer;

    procedure Deepest;
    begin
      y := 99;
    end;

  begin
    Deepest;
    Result := y;
  end;

begin
  WriteLn(Middle);
end;
end.
";
    let result = format_source(src);
    // Middle at indent 1
    assert!(
        result.contains("\n  function Middle"),
        "middle function not at indent 1:\n{}",
        result
    );
    // Deepest at indent 2 (4 spaces)
    assert!(
        result.contains("\n    procedure Deepest"),
        "deepest procedure not at indent 2:\n{}",
        result
    );
    // Deepest body at indent 3 (6 spaces)
    assert!(
        result.contains("\n      y := 99;"),
        "deepest body not at indent 3:\n{}",
        result
    );
}

// ── Bug: Comment indentation inside block closers ──────────────
// Comments inside blocks (try/except/begin..end) must stay at the
// body indent level, not de-indent to the closer's level.

#[test]
fn comment_inside_except_block_indented() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  try
    DoWork;
  except
    // Do not raise on failure
  end;
end;
end.
";
    let result = format_source(src);
    let comment_line = result
        .lines()
        .find(|l| l.contains("// Do not raise"))
        .expect("comment was removed");
    let except_line = result
        .lines()
        .find(|l| l.trim() == "except")
        .expect("except not found");
    let except_indent = except_line.len() - except_line.trim_start().len();
    let comment_indent = comment_line.len() - comment_line.trim_start().len();
    // Comment should be indented MORE than except (inside the except block)
    assert!(
        comment_indent > except_indent,
        "comment (indent {}) should be indented inside except block (indent {}):\n{}",
        comment_indent,
        except_indent,
        result
    );
}

#[test]
fn comment_inside_begin_block_indented() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  // JoinType already set from default
  DoWork;
end;
end.
";
    let result = format_source(src);
    let comment_line = result
        .lines()
        .find(|l| l.contains("// JoinType"))
        .expect("comment was removed");
    let comment_indent = comment_line.len() - comment_line.trim_start().len();
    // Comment inside begin..end should be at body indent level (2 spaces)
    assert!(
        comment_indent >= 2,
        "comment (indent {}) should be indented inside begin..end:\n{}",
        comment_indent,
        result
    );
}

// ── Bug 8: Aligned trailing comments collapsed to 1 space ──────
// Column-aligned trailing comments (with multiple spaces before `//`)
// must preserve their original spacing, not collapse to a single space.

#[test]
fn aligned_trailing_comments_preserved() {
    let src = "\
unit T;
interface
type
  TInfo = record
    Name: string;       // person name
    Age: Integer;       // person age
    Active: Boolean;    // is active
  end;
implementation
end.
";
    let result = format_source(src);
    // The comments should maintain their original spacing (more than 1 space)
    let comment_lines: Vec<&str> = result
        .lines()
        .filter(|l| l.contains("// person") || l.contains("// is active"))
        .collect();
    assert!(
        comment_lines.len() == 3,
        "expected 3 comment lines, got {}:\n{}",
        comment_lines.len(),
        result
    );
    // There should be more than 1 space between the semicolon and the comment
    for line in &comment_lines {
        let semi_pos = line.find(';').unwrap();
        let comment_pos = line.find("//").unwrap();
        let gap = comment_pos - semi_pos - 1;
        assert!(
            gap >= 1,
            "trailing comment should have at least 1 space gap, got {}:\n{}",
            gap,
            result
        );
    }
    // Verify the original multi-space alignment is preserved (not collapsed to 1 space)
    let name_line = result
        .lines()
        .find(|l| l.contains("// person name"))
        .unwrap();
    let semi_pos = name_line.find(';').unwrap();
    let comment_pos = name_line.find("//").unwrap();
    let gap = comment_pos - semi_pos - 1;
    assert!(
        gap > 1,
        "aligned trailing comment gap was collapsed to {} spaces (expected >1):\n{}",
        gap,
        result
    );
}

// ── Task 9: Interface method blank lines ──────────────────────────

#[test]
fn interface_methods_no_extra_blank_lines() {
    let src = "\
unit T;
interface
type
  IMyInterface = interface
    function GetName: string;
    function GetAge: Integer;
    procedure SetName(const Value: string);
  end;
implementation
end.
";
    let result = format_source(src);
    // There should NOT be blank lines between interface methods
    assert!(
        !result.contains("string;\n\n    function GetAge"),
        "blank line inserted between interface methods:\n{}",
        result
    );
    assert!(
        !result.contains("Integer;\n\n    procedure SetName"),
        "blank line inserted between interface methods:\n{}",
        result
    );
}

// ── Task 10: Class header to private blank line ───────────────────

#[test]
fn no_blank_line_between_class_header_and_private() {
    let src = "\
unit T;
interface
type
  TFoo = class(TObject)
  private
    FName: string;
  public
    constructor Create;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        !result.contains("class(TObject)\n\n  private"),
        "blank line inserted between class header and private section:\n{}",
        result
    );
}

#[test]
fn no_blank_line_between_class_and_private_no_ancestor() {
    let src = "\
unit T;
interface
type
  TFoo = class
  private
    FName: string;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        !result.contains("class\n\n  private"),
        "blank line inserted between class and private section:\n{}",
        result
    );
}

// ── Bug: String concatenation over-broken into per-token lines ──

#[test]
fn string_concat_not_over_broken() {
    let src = "\
unit T;
interface
implementation
procedure P;
var
  S, A, B: string;
begin
  S := 'SELECT ' + A + '.' + B + ' FROM ' + A + ' WHERE ' + A + '.' + B + ' = ' + A + '.ID';
end;
end.
";
    let result = format_source(src);
    // The expression is ~90 chars — should fit on one line with 120 char limit
    let assign_line = result
        .lines()
        .find(|l| l.contains("'SELECT '"))
        .expect("assignment not found");
    assert!(
        assign_line.contains(".ID'"),
        "short string concat should stay on one line (total <120 chars):\n{}",
        result
    );
}

// ── Bug: BOM removal ───────────────────────────────────────────

#[test]
fn bom_preserved_in_output() {
    let src_with_bom = "\u{FEFF}unit T;\ninterface\nimplementation\nend.\n";
    let info = pascal_core::FileInfo::new(std::path::PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    let result = fmt4d::formatter::format_source(
        src_with_bom.as_bytes(),
        &info,
        &config,
        &std::collections::HashSet::new(),
    )
    .expect("formatting failed");
    assert!(
        result.starts_with('\u{FEFF}'),
        "UTF-8 BOM was stripped from output:\n{:?}",
        &result[..20.min(result.len())]
    );
}

#[test]
fn no_bom_when_source_has_none() {
    let src = "unit T;\ninterface\nimplementation\nend.\n";
    let info = pascal_core::FileInfo::new(std::path::PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    let result = fmt4d::formatter::format_source(
        src.as_bytes(),
        &info,
        &config,
        &std::collections::HashSet::new(),
    )
    .expect("formatting failed");
    assert!(
        !result.starts_with('\u{FEFF}'),
        "BOM was added when source didn't have one"
    );
}

// ── Bug: Unwanted blank lines between method declarations in class/record types ──

#[test]
fn no_blank_lines_between_class_method_declarations() {
    let src = "\
unit T;

interface

type
  TMyClass = class(TBase)
  public
    constructor Create;
    destructor Destroy; override;
    function IsKeyword(const Token: string): Boolean; override;
    function GetPaginationKeywords: TArray<string>; override;
  end;

implementation

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank lines inserted between method declarations:\n{result}"
    );
}

#[test]
fn no_blank_lines_in_abstract_class_methods() {
    let src = "\
unit T;

interface

type
  TMyDialect = class abstract
  public
    function IsKeyword(const Token: string): Boolean; virtual; abstract;
    function GetPaginationKeywords: TArray<string>; virtual; abstract;
  end;

implementation

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank lines inserted in abstract class:\n{result}"
    );
}

#[test]
fn no_blank_lines_in_record_with_visibility_sections() {
    let src = "\
unit T;

interface

type
  TMyRec = record
  private
    FName: string;
  public
    procedure Init;
    function ToString: string;
  end;

implementation

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank lines inserted in record with sections:\n{result}"
    );
}

#[test]
fn no_blank_line_between_visibility_sections_after_methods() {
    let src = "\
unit T;

interface

type
  TMyClass = class
  private
    FName: string;
    procedure InternalInit;
    procedure InternalCleanup;
  public
    constructor Create;
    destructor Destroy; override;
  end;

implementation

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line inserted between private/public sections:\n{result}"
    );
}

#[test]
fn blank_line_between_visibility_sections_preserved() {
    let src = "\
unit T;

interface

type
  TMyClass = class
  private
    FName: string;
    procedure InternalInit;
    procedure InternalCleanup;

  public
    constructor Create;
    destructor Destroy; override;
  end;

implementation

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line between visibility sections was not preserved:\n{result}"
    );
}

#[test]
fn intentional_blank_lines_in_class_preserved() {
    let src = "\
unit T;

interface

type
  TMyClass = class
  public
    constructor Create;
    destructor Destroy; override;

    procedure DoWork;
    function GetResult: string;

    property Name: string read FName write FName;
    property Value: Integer read FValue;
  end;

implementation

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "intentional blank lines were not preserved:\n{result}"
    );
}

// ── Bug: Blank line inserted before end/except/finally after comments ──

#[test]
fn no_blank_line_between_comment_and_end() {
    let src = "\
unit T;

interface

implementation

procedure Foo;
begin
  DoSomething;
  // trailing comment
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line inserted between comment and end:\n{result}"
    );
}

#[test]
fn no_blank_line_between_comment_and_end_in_except() {
    let src = "\
unit T;

interface

implementation

procedure Foo;
begin
  try
    Consolidate(Query);
  except
    // Do not raise on malformed SQL
  end;
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line inserted between comment and end in except:\n{result}"
    );
}

#[test]
fn no_blank_line_between_comment_and_end_in_nested_block() {
    let src = "\
unit T;

interface

implementation

procedure Foo;
begin
  if X > 0 then
  begin
    // comment after begin
    DoSomething;
    // comment before end
  end;
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line around comments in nested block:\n{result}"
    );
}

#[test]
fn no_blank_line_between_multiline_comments_and_end() {
    let src = "\
unit T;

interface

implementation

procedure Foo;
begin
  begin
    DoWork;
  end;
  // First comment line.
  // Second comment line.
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line between multi-line comments and end:\n{result}"
    );
}

#[test]
fn no_blank_line_after_case_else_before_comment() {
    let src = "\
unit T;

interface

implementation

procedure Foo;
begin
  case Ch of
    'A': DoA;
  else
    // Unknown character - skip it
    ReadChar;
  end;
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line after case else before comment:\n{result}"
    );
}

#[test]
fn no_blank_line_after_begin_before_call_with_comment() {
    let src = "\
unit T;

interface

implementation

procedure Foo;
begin
  if X > 0 then
  begin
    // Remove the field
    FFields.Delete(I);
  end;
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line after begin before call with leading comment:\n{result}"
    );
}

// ── Function call one-per-line breaking ────────────────────────────

#[test]
fn call_args_one_per_line_on_overflow() {
    let src = "\
unit T;

interface

implementation

procedure P;
begin
  CreateOrder(
    CustomerId,
    ProductId,
    Quantity,
    UnitPrice,
    DiscountPercent,
    ShippingAddress,
    BillingAddress,
    PaymentMethod
  );
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "call args should be one-per-line when overflowing:\n{result}"
    );
}

#[test]
fn short_call_stays_on_one_line() {
    let src = "\
unit T;

interface

implementation

procedure P;
begin
  Foo(A, B, C);
end;

end.
";
    let result = format_source(src);
    assert_eq!(result, src, "short call should stay on one line:\n{result}");
}

// ── Bracket list one-per-line breaking ─────────────────────────────

#[test]
fn bracket_list_one_per_line_on_overflow() {
    let src = "\
unit T;

interface

implementation

initialization
  Rtti.RegisterFromText([
    TypeInfo(TLinkField),
    'masterField: RawUtf8; detailField: RawUtf8',
    TypeInfo(TOptionDef),
    'displayValue: RawUtf8; storedValue: RawUtf8'
  ]);

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "bracket list should be one-per-line when overflowing:\n{result}"
    );
}

#[test]
fn short_bracket_list_stays_on_one_line() {
    let src = "\
unit T;

interface

implementation

procedure P;
begin
  Format('hello %s %s', [A, B]);
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "short bracket list should stay on one line:\n{result}"
    );
}

// ── Combined call + bracket formatting ─────────────────────────────

#[test]
fn rtti_register_from_text_pattern() {
    let src = "\
unit T;

interface

implementation

initialization
  Rtti.RegisterFromText([
    TypeInfo(TLinkField),
    'masterField: RawUtf8; detailField: RawUtf8',
    TypeInfo(TOptionDef),
    'displayValue: RawUtf8; storedValue: RawUtf8',
    TypeInfo(TBehaviorDef),
    'columnPromptEN: RawUtf8; columnPromptFR: RawUtf8; '
      + 'displayWidth: Integer; visible: Boolean; readOnly: Boolean; '
      + 'controlType: Integer; options: array of TOptionDef'
  ]);

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "Rtti.RegisterFromText pattern should format cleanly:\n{result}"
    );
}

#[test]
fn format_call_with_bracket_arg_splits_cleanly() {
    let src = "\
unit T;

interface

implementation

procedure P;
begin
  S := Format(
    'SELECT %s, %s, %s FROM %s WHERE %s = %s AND %s = %s ORDER BY %s',
    [Col1, Col2, Col3, TableName, FilterCol1, FilterVal1, FilterCol2, FilterVal2, SortColumn]
  );
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "Format call with bracket arg should split per-arg:\n{result}"
    );
}

// ── Bug: binary expr arg aligned with sibling args in call ──────
// A string concatenation (binary +) used as one of several call
// arguments must start at the same indent as the other arguments.

#[test]
fn call_args_binary_expr_aligned_with_siblings() {
    let src = "\
unit T;

interface

implementation

procedure P;
var
  msg: RawUtf8;
begin
  msg := FormatUtf8(
    '{\"type\":\"cache\",\"event\":\"%\",\"branchId\":\"%\",' + '\"ruleCount\":%,\"durationMs\":%,\"status\":\"%\"}',
    [aEvent, aBranchId, aRuleCount, aDurationMs, aStatus]
  );
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "binary-expr arg should align with bracket-list arg:\n{result}"
    );
}

// ── Bug: case-else semicolon on separate line ───────────────────
// The semicolon after a statement in a case..else branch was placed
// on a new line because build_case treated it as a separate statement.

#[test]
fn case_else_semicolon_stays_on_same_line() {
    let src = "\
unit T;
interface
implementation
function F(AFieldType: Integer): RawUtf8;
begin
  case AFieldType of
    1: Result := 'TEXT';
    2: Result := 'BLOB';
  else
    Result := 'TEXT';
  end;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("Result := 'TEXT';"),
        "semicolon should stay on same line as assignment in case-else:\n{result}"
    );
    assert!(
        !result.contains("'TEXT'\n"),
        "semicolon must not be split to next line:\n{result}"
    );
}

// ── Bug: strict private / strict protected split across lines ───
// "strict private" is a single visibility specifier in Delphi.
// The formatter must keep "strict" and "private"/"protected" on one line.

#[test]
fn strict_private_stays_on_one_line() {
    let src = "\
unit T;
interface
type
  TFoo = class
  strict private
    FValue: Integer;
  private
    FName: string;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("strict private"),
        "strict private was split across lines:\n{result}"
    );
}

#[test]
fn strict_protected_stays_on_one_line() {
    let src = "\
unit T;
interface
type
  TFoo = class
  strict protected
    FValue: Integer;
  protected
    FName: string;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("strict protected"),
        "strict protected was split across lines:\n{result}"
    );
}

// ── Brace / paren-star comment spacing ──────────────────────────────

#[test]
fn no_blank_line_before_brace_comment_after_then() {
    let src = "\
unit T;

interface

implementation

procedure Foo;
begin
  if FHasFields then
  begin
    { Initialize the record }
    DoSomething;
  end;
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line inserted before brace comment after then:\n{result}"
    );
}

#[test]
fn no_blank_line_before_paren_star_comment_after_then() {
    let src = "\
unit T;

interface

implementation

procedure Foo;
begin
  if FHasFields then
  begin
    (* Initialize the record *)
    DoSomething;
  end;
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line inserted before paren-star comment after then:\n{result}"
    );
}

#[test]
fn no_blank_line_before_brace_comment_single_stmt_after_then() {
    let src = "\
unit T;

interface

implementation

procedure Foo;
begin
  if FHasFields then
    { Initialize the record }
    DoSomething;
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line inserted before brace comment in single-stmt if:\n{result}"
    );
}

#[test]
fn no_blank_line_before_brace_comment_after_else() {
    let src = "\
unit T;

interface

implementation

procedure Foo;
begin
  if X then
    DoA
  else
    { Handle the other case }
    DoB;
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line inserted before brace comment after else:\n{result}"
    );
}

// ── Bug: const section + function decl joined on one line ─────────
// A const block followed by function declarations in the interface
// section must be separated by a blank line, not concatenated.

#[test]
fn const_section_followed_by_function_decl_not_joined() {
    let src = "\
unit T;
interface

const
  CLIENT_TOKEN_KINDS = [tkField, tkControl, tkUsageStatic];

function ExtractMacroBlocks(const ASource: RawUtf8): TMacroBlockArray;
function ParseTokenPrefix(const AExpression: RawUtf8): TTokenInfo;

implementation
end.
";
    let result = format_source(src);
    // The const section and function declarations must NOT be joined on one line
    assert!(
        result.contains("tkUsageStatic];\n\nfunction ExtractMacroBlocks"),
        "const section and function declaration were joined on one line:\n{}",
        result
    );
}

#[test]
fn var_section_followed_by_function_decl_not_joined() {
    let src = "\
unit T;
interface

var
  GlobalFlag: Boolean;

function DoSomething: Integer;

implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("Boolean;\n\nfunction DoSomething"),
        "var section and function declaration were joined on one line:\n{}",
        result
    );
}

#[test]
fn function_decl_followed_by_const_section_not_joined() {
    let src = "\
unit T;
interface

function DoSomething: Integer;

const
  MAX_VALUE = 100;

implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("Integer;\n\nconst"),
        "function declaration and const section were joined on one line:\n{}",
        result
    );
}

// ── Bug: Pointer-type caret gets a space inserted ─────────────
// `^TFoo` (pointer type) and `P^` (dereference) must not have a
// space between the caret and the adjacent token.

#[test]
fn pointer_type_caret_no_space_after() {
    let src = "\
unit T;
interface
type
  PSimpleMfsHeader = ^TSimpleMfsHeader;
  TSimpleMfsHeader = record
    X: Integer;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("= ^TSimpleMfsHeader;"),
        "space inserted between ^ and type name in pointer type:\n{}",
        result
    );
    assert!(
        !result.contains("^ TSimpleMfsHeader"),
        "space inserted between ^ and type name in pointer type:\n{}",
        result
    );
}

#[test]
fn pointer_var_type_caret_no_space_after() {
    let src = "\
unit T;
interface
implementation
procedure Q;
var
  P: ^Integer;
begin
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains(": ^Integer;"),
        "space inserted between ^ and type name in var pointer type:\n{}",
        result
    );
}

#[test]
fn dereference_caret_no_space_before() {
    let src = "\
unit T;
interface
implementation
procedure Q;
var
  P: ^Integer;
begin
  P^ := 1;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("P^ := 1"),
        "space inserted between identifier and ^ in dereference:\n{}",
        result
    );
    assert!(
        !result.contains("P ^"),
        "space inserted between identifier and ^ in dereference:\n{}",
        result
    );
}

// ── Bug: Blank lines stripped in files with non-UTF-8 (Latin-1) bytes ─────
// Legacy Delphi sources are often Windows-1252/Latin-1 encoded. Any non-ASCII
// byte in a comment caused `has_blank_line_between` to decode-fail the whole
// source, silently disabling blank-line preservation file-wide.

#[test]
fn blank_lines_preserved_in_latin1_source() {
    // Build a source containing a Latin-1 byte (0xE9 = 'é') in a comment
    // far from the begin..end block we care about.
    let mut src: Vec<u8> = b"\
unit T;
interface
implementation
// Commentaire avec accent: "
        .to_vec();
    src.push(0xE9); // invalid UTF-8 continuation byte
    src.extend_from_slice(
        b"\n\
procedure Q;
var
  a, b: Boolean;
  sa, sb: string;
begin
  sa := '';

  if a then
    sa := 'Y'
  else
    sa := 'N';

  if b then
    sb := 'Y'
  else
    sb := 'N';
end;
end.
",
    );

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    let result =
        fmt4d::formatter::format_source(&src, &info, &config, &std::collections::HashSet::new())
            .expect("formatting failed");

    assert!(
        result.contains("sa := '';\n\n  if a then"),
        "blank line before first if..else was stripped (Latin-1 source):\n{}",
        result
    );
    assert!(
        result.contains("sa := 'N';\n\n  if b then"),
        "blank line between two if..else statements was stripped (Latin-1 source):\n{}",
        result
    );
}

#[test]
fn latin1_comment_text_is_preserved() {
    // A Latin-1 inline comment must survive formatting. Previously,
    // any non-UTF-8 byte in the source caused comment text extraction
    // to silently return "", stripping the entire comment.
    let mut src: Vec<u8> = b"\
unit T;
interface
implementation
procedure Q;
begin
  x := 1; // v"
        .to_vec();
    src.push(0xE9); // Latin-1 'é'
    src.extend_from_slice(b"rifier\nend;\nend.\n");

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    let result =
        fmt4d::formatter::format_source(&src, &info, &config, &std::collections::HashSet::new())
            .expect("formatting failed");

    assert!(
        result.contains("vérifier"),
        "Latin-1 character in comment was lost:\n{}",
        result
    );
    assert!(
        result.contains("// vérifier"),
        "comment marker and body must be preserved together:\n{}",
        result
    );
}

#[test]
fn latin1_source_roundtrips_to_latin1_bytes() {
    // End-to-end: a Latin-1 byte sequence must come back out of the
    // formatter pipeline as Latin-1 bytes, not as UTF-8. This is what
    // keeps legacy Delphi codebases from having their on-disk encoding
    // silently upgraded to UTF-8 every time fmt4d runs.
    let mut src: Vec<u8> = b"\
unit T;
interface
implementation
procedure Q;
begin
  x := 1; // v"
        .to_vec();
    src.push(0xE9); // Latin-1 'é'
    src.extend_from_slice(b"rifier\nend;\nend.\n");

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();

    // Sanity: the input really is non-UTF-8.
    assert_eq!(
        pascal_core::detect_encoding(&src),
        pascal_core::SourceEncoding::Latin1
    );

    let bytes = fmt4d::formatter::format_bytes(&src, &info, &config, &Default::default())
        .expect("formatting failed");

    // The result must still be Latin-1 (contains the 0xE9 byte and is
    // not valid UTF-8 in any section that had the accented character).
    assert_eq!(
        pascal_core::detect_encoding(&bytes),
        pascal_core::SourceEncoding::Latin1,
        "output encoding changed from Latin-1 to UTF-8:\n{:?}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(
        bytes.windows(1).any(|w| w == [0xE9]),
        "output does not contain the 0xE9 Latin-1 byte:\n{:?}",
        String::from_utf8_lossy(&bytes)
    );
    // And the content must still make sense as decoded text.
    let as_text = pascal_core::decode_bytes(&bytes);
    assert!(
        as_text.contains("// vérifier"),
        "comment text lost in roundtrip:\n{}",
        as_text
    );
}

#[test]
fn utf8_source_stays_utf8() {
    // A UTF-8 source with multi-byte characters must come out as UTF-8,
    // not get re-encoded to Latin-1.
    let src =
        "unit T;\ninterface\nimplementation\nprocedure Q;\nbegin\n  x := 1; // café\nend;\nend.\n"
            .as_bytes();

    assert_eq!(
        pascal_core::detect_encoding(src),
        pascal_core::SourceEncoding::Utf8
    );

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    let bytes = fmt4d::formatter::format_bytes(src, &info, &config, &Default::default())
        .expect("formatting failed");

    assert_eq!(
        pascal_core::detect_encoding(&bytes),
        pascal_core::SourceEncoding::Utf8,
        "UTF-8 output mis-detected as another encoding"
    );
    // The multi-byte UTF-8 sequence for 'é' (0xC3 0xA9) must be present.
    let text = std::str::from_utf8(&bytes).expect("output must be valid UTF-8");
    assert!(text.contains("café"));
}

#[test]
fn utf8_bom_source_preserves_bom() {
    let mut src: Vec<u8> = vec![0xEF, 0xBB, 0xBF];
    src.extend_from_slice(
        b"unit T;\ninterface\nimplementation\nprocedure Q;\nbegin\n  x := 1;\nend;\nend.\n",
    );

    assert_eq!(
        pascal_core::detect_encoding(&src),
        pascal_core::SourceEncoding::Utf8Bom
    );

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    let bytes = fmt4d::formatter::format_bytes(&src, &info, &config, &Default::default())
        .expect("formatting failed");

    assert!(
        bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
        "BOM was stripped from UTF-8+BOM output"
    );
    assert_eq!(
        pascal_core::detect_encoding(&bytes),
        pascal_core::SourceEncoding::Utf8Bom
    );
}

#[test]
fn latin1_leading_comment_text_is_preserved() {
    // Leading comment (own-line) with a Latin-1 character. Goes through
    // the comment-attachment path rather than the trailing path.
    let mut src: Vec<u8> = b"\
unit T;
interface
implementation
procedure Q;
begin
  // V"
        .to_vec();
    src.push(0xE9);
    src.extend_from_slice(b"rifier l'entree\n  x := 1;\nend;\nend.\n");

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    let result =
        fmt4d::formatter::format_source(&src, &info, &config, &std::collections::HashSet::new())
            .expect("formatting failed");

    assert!(
        result.contains("Vérifier"),
        "Latin-1 character in leading comment was lost:\n{}",
        result
    );
}

// ── Bug: RTTI attributes collapsed onto declaration line ────────
// `[TestFixture]`, `[Test]`, and similar bracket attributes must
// stay on their own line above the declaration they annotate —
// never collapsed inline with `class`, `procedure`, `property`,
// or a field name.

#[test]
fn rtti_testfixture_attribute_stays_on_own_line() {
    let src = "\
unit T;
interface
type
  [TestFixture]
  TFoo = class
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        result.contains("[TestFixture]\n"),
        "[TestFixture] was not placed on its own line:\n{}",
        result
    );
    assert!(
        !result.contains("[TestFixture] TFoo"),
        "[TestFixture] was collapsed inline with the class name:\n{}",
        result
    );
}

#[test]
fn rtti_test_attribute_on_method_stays_on_own_line() {
    let src = "\
unit T;
interface
type
  TFoo = class
  public
    [Test]
    procedure TestOne;
    [Test]
    procedure TestTwo;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        !result.contains("[Test] procedure"),
        "[Test] was collapsed inline with `procedure`:\n{}",
        result
    );
    assert!(
        result.matches("[Test]\n").count() >= 2,
        "expected two [Test] attributes each on their own line:\n{}",
        result
    );
}

#[test]
fn rtti_multiple_stacked_attributes_each_on_own_line() {
    let src = "\
unit T;
interface
type
  TFoo = class
  public
    [Test]
    [TestCase('case1')]
    procedure TestIt;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        !result.contains("[Test] [TestCase"),
        "[Test] and [TestCase] were joined on the same line:\n{}",
        result
    );
    assert!(
        !result.contains("[TestCase('case1')] procedure"),
        "[TestCase('case1')] was collapsed inline with `procedure`:\n{}",
        result
    );
    assert!(
        result.contains("[Test]\n"),
        "[Test] was not on its own line:\n{}",
        result
    );
    assert!(
        result.contains("[TestCase('case1')]\n"),
        "[TestCase('case1')] was not on its own line:\n{}",
        result
    );
}

#[test]
fn rtti_attribute_on_field_stays_on_own_line() {
    let src = "\
unit T;
interface
type
  TFoo = class
  private
    [MyFieldAttr]
    FField: Integer;
  end;
implementation
end.
";
    let result = format_source(src);
    assert!(
        !result.contains("[MyFieldAttr] FField"),
        "[MyFieldAttr] was collapsed inline with field name:\n{}",
        result
    );
    assert!(
        result.contains("[MyFieldAttr]\n"),
        "[MyFieldAttr] was not on its own line:\n{}",
        result
    );
}

// ── Bug: spurious blank line between const/var/type keyword and a ──
// leading `//` comment on the first declaration.
//
// The section builder unconditionally pushed a Hardline after the
// `const`/`var`/`type` keyword, but if the first declaration had a
// leading `//` comment its doc already started with a Hardline from
// comment attachment — doubling up and producing a blank line.

#[test]
fn no_blank_line_between_const_and_leading_line_comment() {
    let src = "\
unit T;

interface

const
  //MSG_TO.VUSERTYPE
  VUT_ALL_MASTER_BR = 'ALL';
  VUT_BANNER = 'BANNER';

implementation

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line inserted between const and leading // comment:\n{result}"
    );
}

#[test]
fn no_blank_line_between_const_and_leading_brace_comment() {
    let src = "\
unit T;

interface

const
  { section heading }
  VUT_ALL_MASTER_BR = 'ALL';

implementation

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line inserted between const and leading brace comment:\n{result}"
    );
}

#[test]
fn no_blank_line_between_var_and_leading_line_comment() {
    let src = "\
unit T;

interface

var
  //Global state
  GFoo: Integer;
  GBar: Integer;

implementation

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line inserted between var and leading // comment:\n{result}"
    );
}

#[test]
fn no_blank_line_between_type_and_leading_line_comment() {
    let src = "\
unit T;

interface

type
  //forward decls
  TFoo = class;
  TBar = class;

implementation

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line inserted between type and leading // comment:\n{result}"
    );
}

#[test]
fn no_blank_line_between_local_const_and_leading_line_comment() {
    let src = "\
unit T;

interface

implementation

procedure Foo;
const
  //local constants
  CMax = 42;
begin
  WriteLn(CMax);
end;

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line inserted between local const and leading // comment:\n{result}"
    );
}

#[test]
fn no_blank_line_between_decls_with_interleaved_line_comments() {
    // A leading `//` comment on a non-first declaration must not
    // create a blank line either. Previously the body pushed a
    // Hardline before the child AND the child started with a
    // Hardline from its attached comment, doubling up.
    let src = "\
unit T;

interface

const
  FOO = 1;
  //BAR is important
  BAR = 2;

implementation

end.
";
    let result = format_source(src);
    assert_eq!(
        result, src,
        "blank line inserted before interleaved // comment:\n{result}"
    );
}

// ── Bug: Leading comments cause spurious call wrapping ──────────
// When a function call has leading // comments attached to the
// function name, the fits() check consumed remaining-budget on the
// comment tokens (which live on separate lines), leaving too little
// budget for the actual call, so the arguments were needlessly
// wrapped onto separate lines.

#[test]
fn leading_comments_do_not_break_short_call() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if True then
  begin
    if True then
    begin
      if True then
      begin
        if True then
        begin
          //-----------------------------------
          //  POS_SC.
          //-----------------------------------
          UpdateTransferStatusFrom0To1(aSimpleMfsHeader.transferid)
        end;
      end;
    end;
  end;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("UpdateTransferStatusFrom0To1(aSimpleMfsHeader.transferid)"),
        "short call with leading comments should stay on one line:\n{result}"
    );
}

#[test]
fn leading_comments_do_not_break_short_call_with_semicolon() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if True then
  begin
    // Mise du transfert en statut 1
    UpdateTransferStatusFrom0To1(aSimpleMfsHeader.transferid);
  end;
end;
end.
";
    let result = format_source(src);
    assert!(
        result.contains("UpdateTransferStatusFrom0To1(aSimpleMfsHeader.transferid);"),
        "short call with leading comment should stay on one line:\n{result}"
    );
}

#[test]
fn deeply_nested_binary_chain_does_not_overflow() {
    // Regression guard for SEC-H1: a 2000-operand `+` chain must not
    // stack-overflow. Rust stack overflow is non-unwindable and kills
    // the entire rayon run.
    let chain = vec!["'a'"; 2000].join(" + ");
    let src = format!(
        "unit T;\ninterface\nimplementation\nprocedure P;\nvar s: string;\nbegin\n  s := {};\nend;\nend.\n",
        chain
    );
    // Just running to completion without aborting is the assertion.
    let _ = format_source(&src);
}
