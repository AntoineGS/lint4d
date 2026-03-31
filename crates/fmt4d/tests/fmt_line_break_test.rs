//! Tests for AST-aware line breaking (smart line splitting).

use std::path::PathBuf;

fn format_source(source: &str) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    fmt4d::formatter::format_source(source.as_bytes(), &info, &config).expect("formatting failed")
}

fn format_source_with_max(source: &str, max_line_length: usize) -> String {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let mut config = fmt4d::config::FmtConfig::default();
    config.max_line_length = max_line_length;
    fmt4d::formatter::format_source(source.as_bytes(), &info, &config).expect("formatting failed")
}

/// Assert that no line in the output exceeds max_line_length.
fn assert_no_long_lines(output: &str, max_line_length: usize) {
    let long_lines: Vec<(usize, &str)> = output
        .lines()
        .enumerate()
        .filter(|(_, l)| l.len() > max_line_length)
        .collect();
    assert!(
        long_lines.is_empty(),
        "Lines exceed {} chars:\n{}",
        max_line_length,
        long_lines
            .iter()
            .map(|(i, l)| format!("  line {}: ({} chars) {}", i + 1, l.len(), l))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Assert that formatting is idempotent.
fn assert_idempotent(source: &str) {
    let first = format_source(source);
    let second = format_source(&first);
    assert_eq!(
        first, second,
        "Formatting is not idempotent.\nFirst:\n{}\nSecond:\n{}",
        first, second
    );
}

fn assert_idempotent_with_max(source: &str, max_line_length: usize) {
    let first = format_source_with_max(source, max_line_length);
    let second = format_source_with_max(&first, max_line_length);
    assert_eq!(
        first, second,
        "Formatting is not idempotent.\nFirst:\n{}\nSecond:\n{}",
        first, second
    );
}

// ── Measurement Validation ───────────────────────────────────────

#[test]
fn short_procedure_stays_on_one_line() {
    let src = "\
unit T;
interface
  procedure Foo(A: Integer; B: string);
implementation
end.
";
    let result = format_source(src);
    let proc_line = result.lines().find(|l| l.contains("procedure Foo"));
    assert!(proc_line.is_some(), "procedure not found:\n{}", result);
    let line = proc_line.unwrap();
    assert!(
        line.contains("(A: Integer; B: string)"),
        "params should be on one line:\n{}",
        line
    );
}

#[test]
fn short_if_stays_on_one_line() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if (A = 1) and (B = 2) then
    DoSomething;
end;
end.
";
    let result = format_source(src);
    let if_line = result.lines().find(|l| l.trim_start().starts_with("if "));
    assert!(if_line.is_some(), "if not found:\n{}", result);
    assert!(
        if_line.unwrap().contains("and"),
        "short if should stay on one line:\n{}",
        if_line.unwrap()
    );
}

// ── Method Signature Breaking (declArgs at ;) ───────────────────

#[test]
fn long_param_list_breaks_at_semicolons() {
    let src = "\
unit T;
interface
  procedure DoSomething(const AParam1: string; const AParam2: Integer; const AParam3: Boolean; const AParam4: TObject; const AParam5: TList);
implementation
end.
";
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
    assert_idempotent_with_max(src, 80);
    let lines: Vec<&str> = result
        .lines()
        .filter(|l| l.contains("const A") || l.contains("const "))
        .collect();
    assert!(
        lines.len() > 1,
        "long param list should be broken across lines:\n{}",
        result
    );
}

#[test]
fn short_param_list_stays_on_one_line_at_80() {
    let src = "\
unit T;
interface
  procedure Foo(A: Integer; B: string);
implementation
end.
";
    let result = format_source_with_max(src, 80);
    let proc_line = result.lines().find(|l| l.contains("procedure Foo"));
    assert!(
        proc_line.unwrap().contains("(A: Integer; B: string)"),
        "short param list should stay on one line:\n{}",
        result
    );
}

#[test]
fn param_list_join_then_reflow() {
    let src = "\
unit T;
interface
  procedure DoSomething(
    const A: string;
    const B: Integer;
    const C: Boolean);
implementation
end.
";
    let result = format_source_with_max(src, 120);
    let proc_line = result.lines().find(|l| l.contains("procedure DoSomething"));
    assert!(proc_line.is_some(), "procedure not found:\n{}", result);
    assert!(
        proc_line.unwrap().contains("const C: Boolean"),
        "params should be joined onto one line at width 120:\n{}",
        result
    );
    assert_idempotent_with_max(src, 120);
}

#[test]
fn param_list_with_generics_breaks_correctly() {
    let src = "\
unit T;
interface
  procedure Foo(const A: TList<TPair<string, Integer>>; const B: TDictionary<string, TList<Integer>>; const C: TObject);
implementation
end.
";
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
    assert_idempotent_with_max(src, 80);
}

#[test]
fn param_list_idempotent() {
    let src = "\
unit T;
interface
  procedure DoSomething(const AParam1: string; const AParam2: Integer; const AParam3: Boolean; const AParam4: TObject; const AParam5: TList);
implementation
end.
";
    assert_idempotent_with_max(src, 80);
}

// ── Function Call Arguments Breaking (exprCall at ,) ────────────

#[test]
fn long_call_args_break_at_commas() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  Result := DoSomething(LongArgument1, LongArgument2, LongArgument3, LongArgument4, LongArgument5);
end;
end.
";
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
    assert_idempotent_with_max(src, 80);
}

#[test]
fn short_call_stays_on_one_line() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  Result := Foo(A, B, C);
end;
end.
";
    let result = format_source_with_max(src, 80);
    assert!(
        result.lines().any(|l| l.contains("Foo(A, B, C)")),
        "short call should stay on one line:\n{}",
        result
    );
}

#[test]
fn nested_calls_break_outer_first() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  Result := Outer(Inner1(A, B, C), Inner2(D, E, F), SimpleArg, AnotherArg, MoreArgs);
end;
end.
";
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
    assert_idempotent_with_max(src, 80);
    // The inner calls should stay intact if they fit
    assert!(
        result.contains("Inner1(A, B, C)"),
        "inner call should stay on one line:\n{}",
        result
    );
}

#[test]
fn call_args_idempotent() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  Result := DoSomething(LongArgument1, LongArgument2, LongArgument3, LongArgument4, LongArgument5);
end;
end.
";
    assert_idempotent_with_max(src, 80);
}

// ── If Condition Breaking (before and/or) ───────────────────────

#[test]
fn long_if_breaks_before_and() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if (SomeCondition = True) and (AnotherCondition > 10) and (YetAnother <> '') and (FourthCheck = 1) then
    DoSomething;
end;
end.
";
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
    assert_idempotent_with_max(src, 80);
    let and_lines: Vec<&str> = result
        .lines()
        .filter(|l| l.trim_start().starts_with("and "))
        .collect();
    assert!(
        !and_lines.is_empty(),
        "`and` should lead continuation lines:\n{}",
        result
    );
}

#[test]
fn long_if_breaks_before_or() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if (SomeCondition = True) or (AnotherCondition > 10) or (YetAnother <> '') or (FourthCheck = 1) then
    DoSomething;
end;
end.
";
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
    assert_idempotent_with_max(src, 80);
    let or_lines: Vec<&str> = result
        .lines()
        .filter(|l| l.trim_start().starts_with("or "))
        .collect();
    assert!(
        !or_lines.is_empty(),
        "`or` should lead continuation lines:\n{}",
        result
    );
}

#[test]
fn mixed_and_or_breaks_correctly() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if (SomeCondition = True) and (AnotherCondition > 10) or (YetAnother <> '') and (FourthCheck = 1) then
    DoSomething;
end;
end.
";
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
    assert_idempotent_with_max(src, 80);
}

#[test]
fn nested_paren_groups_stay_atomic() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if ((A = 1) and (B = 2)) or ((C = 3) and (D = 4)) or ((E = 5) and (F = 6)) then
    DoSomething;
end;
end.
";
    let result = format_source_with_max(src, 70);
    assert_no_long_lines(&result, 70);
    assert_idempotent_with_max(src, 70);
}

#[test]
fn deeply_nested_conditions() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if (((A = 1) and (B = 2)) or (C = 3)) and (D = 4) and (E = 5) or (F = 6) then
    DoSomething;
end;
end.
";
    let result = format_source_with_max(src, 70);
    assert_no_long_lines(&result, 70);
    assert_idempotent_with_max(src, 70);
}

#[test]
fn function_calls_inside_conditions() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if (GetValue(Param1, Param2, Param3) > 0) and (Foo(Bar, Baz) = True) and (Check(X) <> 0) then
    DoSomething;
end;
end.
";
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
    assert_idempotent_with_max(src, 80);
}

#[test]
fn bare_and_or_without_parens() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if LongConditionA and LongConditionB and LongConditionC and LongConditionD and LongConditionE then
    DoSomething;
end;
end.
";
    let result = format_source_with_max(src, 70);
    assert_no_long_lines(&result, 70);
    assert_idempotent_with_max(src, 70);
}

#[test]
fn short_if_no_break() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if (A = 1) and (B = 2) then
    DoSomething;
end;
end.
";
    let result = format_source(src);
    let if_line = result
        .lines()
        .find(|l| l.trim_start().starts_with("if "))
        .unwrap();
    assert!(
        if_line.contains("then"),
        "short if should stay on one line with then:\n{}",
        result
    );
}

#[test]
fn if_condition_join_then_reflow() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if (A = 1)
    and (B = 2)
    and (C = 3) then
    DoSomething;
end;
end.
";
    let result = format_source_with_max(src, 120);
    let if_line = result
        .lines()
        .find(|l| l.trim_start().starts_with("if "))
        .unwrap();
    assert!(
        if_line.contains("(C = 3) then"),
        "condition should be joined at width 120:\n{}",
        result
    );
    assert_idempotent_with_max(src, 120);
}

#[test]
fn else_if_chain_breaks_correctly() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if (A = 1) and (B = 2) and (C = 3) and (D = 4) then
    DoFirst
  else if (E = 5) or (F = 6) or (G = 7) or (H = 8) then
    DoSecond;
end;
end.
";
    let result = format_source_with_max(src, 60);
    assert_no_long_lines(&result, 60);
    assert_idempotent_with_max(src, 60);
}

#[test]
fn if_condition_idempotent() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if (SomeCondition = True) and (AnotherCondition > 10) and (YetAnother <> '') and (FourthCheck = 1) then
    DoSomething;
end;
end.
";
    assert_idempotent_with_max(src, 80);
}

// ── Assignment Expression Breaking (before operators) ───────────

#[test]
fn long_assignment_breaks_before_operator() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  Result := VeryLongExpression + AnotherLongExpression + YetMoreStuffHere + FinalPartOfExpression;
end;
end.
";
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
    assert_idempotent_with_max(src, 80);
    // Operator should lead continuation line
    let cont_lines: Vec<&str> = result
        .lines()
        .filter(|l| l.trim_start().starts_with("+ "))
        .collect();
    assert!(
        !cont_lines.is_empty(),
        "`+` should lead continuation lines:\n{}",
        result
    );
}

#[test]
fn short_assignment_stays_on_one_line() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  Result := A + B + C;
end;
end.
";
    let result = format_source_with_max(src, 80);
    assert!(
        result.lines().any(|l| l.contains("A + B + C")),
        "short assignment should stay on one line:\n{}",
        result
    );
}

#[test]
fn string_concat_breaks_before_plus() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  Msg := 'First long part of the string ' + 'Second long part of the string ' + 'Third long part ' + 'Fourth part';
end;
end.
";
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
    assert_idempotent_with_max(src, 80);
}

#[test]
fn mixed_operators_in_expression() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  Result := LongValue1 + LongValue2 - LongValue3 * LongValue4 div LongValue5 + LongValue6;
end;
end.
";
    let result = format_source_with_max(src, 70);
    assert_no_long_lines(&result, 70);
    assert_idempotent_with_max(src, 70);
}

#[test]
fn expression_idempotent() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  Result := VeryLongExpression + AnotherLongExpression + YetMoreStuffHere + FinalPartOfExpression;
end;
end.
";
    assert_idempotent_with_max(src, 80);
}

// ── Column Tracking Validation ──────────────────────────────────

#[test]
fn column_tracking_matches_output() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  WriteLn('hello');
end;
end.
";
    let result = format_source(src);
    assert!(result.contains("procedure P;"), "output:\n{}", result);
    let second = format_source(&result);
    assert_eq!(result, second, "column tracking broke idempotency");
}

// ── Uses Clause Wrapping ────────────────────────────────────────

#[test]
fn uses_clause_already_wraps() {
    let src = "\
unit T;
interface
uses Unit1, Unit2, Unit3, Unit4, Unit5, Unit6, Unit7, Unit8, Unit9, Unit10;
implementation
end.
";
    let result = format_source(src);
    assert_no_long_lines(&result, 120);
    assert_idempotent(src);
    // Each unit should be on its own line (format_uses puts one per line)
    let unit_lines: Vec<&str> = result
        .lines()
        .filter(|l| l.trim_start().starts_with("Unit"))
        .collect();
    assert!(
        unit_lines.len() >= 2,
        "uses units should be on separate lines:\n{}",
        result
    );
}

// ── Cross-Context Integration Tests ─────────────────────────────

#[test]
fn call_inside_condition_breaks_correctly() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  if (GetValue(VeryLongParam1, VeryLongParam2, VeryLongParam3) > Threshold) and (AnotherCheck(LongArg1, LongArg2) <> '') then
    DoSomething;
end;
end.
";
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
    assert_idempotent_with_max(src, 80);
}

#[test]
fn method_chain_call_breaks() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  Result := Self.Factory.CreateInstance(Param1, Param2, Param3).Initialize(Config1, Config2, Config3);
end;
end.
";
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
    assert_idempotent_with_max(src, 80);
}

#[test]
fn assignment_with_call_breaks_outer_first() {
    let src = "\
unit T;
interface
implementation
procedure P;
begin
  Result := Foo(LongArg1 + LongArg2, AnotherArg + MoreStuff, ThirdArg + Extra, FourthArg);
end;
end.
";
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
    assert_idempotent_with_max(src, 80);
}

#[test]
fn no_regression_on_existing_formatting() {
    let src = "\
unit T;
interface

type
  TMyClass = class
  public
    procedure Short(A: Integer);
    function GetValue: string;
  end;

implementation

procedure TMyClass.Short(A: Integer);
begin
  if A > 0 then
    WriteLn('positive');
end;

function TMyClass.GetValue: string;
begin
  Result := 'hello';
end;

end.
";
    let result = format_source(src);
    assert_no_long_lines(&result, 120);
    assert_idempotent(src);
}

#[test]
fn all_contexts_idempotent() {
    let src = "\
unit T;
interface
  procedure VeryLongProcedureName(const FirstParam: string; const SecondParam: Integer; const ThirdParam: Boolean; const FourthParam: TObject);
implementation
procedure VeryLongProcedureName(const FirstParam: string; const SecondParam: Integer; const ThirdParam: Boolean; const FourthParam: TObject);
begin
  if (SomeCondition = True) and (AnotherCondition > 10) and (YetAnother <> '') and (FourthCheck = 1) then
  begin
    Result := DoSomething(LongArgument1, LongArgument2, LongArgument3, LongArgument4, Arg5);
    Value := VeryLongExpression + AnotherLongExpression + YetMoreStuff + FinalPartHere;
  end;
end;
end.
";
    assert_idempotent_with_max(src, 80);
    let result = format_source_with_max(src, 80);
    assert_no_long_lines(&result, 80);
}
