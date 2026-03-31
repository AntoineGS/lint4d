//! Regression tests for formatting bugs discovered by running fmt4d on
//! ChainDriveAPI / Common/SQL.Parser.pas.
//!
//! Each test targets a specific issue.  Tests describe the CORRECT behaviour
//! and are expected to **fail** until the corresponding formatter fix lands.

use std::path::PathBuf;

fn format_source(source: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    fmt4d::formatter::format_source(source.as_bytes(), &info, &config).expect("formatting failed")
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
