//! Root causes of fmt4d content loss (l4d-um8g step 2): each fixture is
//! minimized from a multidev file the content-preservation net refused. The
//! formatter must now keep the content, lay it out as expected, and give the
//! same output when run on its own output.

use pascal_core::FileInfo;
use std::collections::HashSet;
use std::path::PathBuf;

fn format(source: &str) -> fmt4d::Result<String> {
    fmt4d::format_source(
        source.as_bytes(),
        &FileInfo::new(PathBuf::from("T.pas")),
        &fmt4d::FmtConfig::default(),
        &HashSet::new(),
    )
}

#[track_caller]
fn assert_formats_to(source: &str, expected: &str) {
    let output = match format(source) {
        Ok(output) => output,
        Err(error) => panic!("formatting failed: {error}"),
    };
    assert_eq!(output, expected, "unexpected output:\n{output}");
    let again = match format(&output) {
        Ok(again) => again,
        Err(error) => panic!("formatting the output failed: {error}"),
    };
    assert_eq!(again, output, "not idempotent:\n{again}");
}

/// Comments and directives before or after the `{$IF…}`, `{$ELSE}` and
/// `{$ENDIF}` of a conditional block are attached to those directive leaves,
/// which the block was emitting bare (ZEOS `{$I ZDbc.inc}` before `{$IF}`,
/// `{$ENDIF} // comment`, a comment before `{$IFNDEF}` in a statement list).
#[test]
fn trivia_around_conditional_block_directives_is_kept() {
    assert_formats_to(
        "\
unit T;
interface
{$I ZParseSql.inc}
  { Parameters }
{$IFNDEF ZEOS_DISABLE_OLEDB} // enabled
uses
  ZODBCToken;
{$ELSE} // disabled
const A = 1;
{$ENDIF} // ZEOS_DISABLE_OLEDB
implementation
procedure P;
begin
  // Verify if form is created
  {$IFNDEF CDLIGHT}
  X := 1;
  {$ENDIF} // CDLIGHT
end;
end.
",
        "\
unit T;

interface

{$I ZParseSql.inc}
{ Parameters }
{$IFNDEF ZEOS_DISABLE_OLEDB} // enabled
uses
  ZODBCToken;

{$ELSE} // disabled
const
  A = 1;
{$ENDIF} // ZEOS_DISABLE_OLEDB

implementation

procedure P;
begin
  // Verify if form is created
  {$IFNDEF CDLIGHT}
  X := 1
  ;
  {$ENDIF} // CDLIGHT
end;

end.
",
    );
}

/// Minimized from Spring4D `Spring.ValueConverters.pas`: the `{$REGION}`
/// before an `{$IFNDEF}`-guarded type declaration leads the `{$IFNDEF}`.
#[test]
fn region_directives_around_ifndef_type_declarations_are_kept() {
    assert_formats_to(
        "\
unit T;

interface

type

  {$REGION 'TA'}

  TA = class
  end;

  {$ENDREGION}


  {$REGION 'TB'}

{$IFNDEF NO_B}
  TB = class
  end;
{$ENDIF}

  {$ENDREGION}


  {$REGION 'TC'}

{$IFNDEF NO_C}
  TC = class
  end;
{$ENDIF}

  {$ENDREGION}

implementation

end.
",
        "\
unit T;

interface

type
  {$REGION 'TA'}
  TA = class
  end;

  {$ENDREGION}

  {$REGION 'TB'}

  {$IFNDEF NO_B}
  TB = class
  end;
  {$ENDIF}

  {$ENDREGION}

  {$REGION 'TC'}

  {$IFNDEF NO_C}
  TC = class
  end;
  {$ENDIF}

{$ENDREGION}
implementation

end.
",
    );
}

/// Directives before `end`, `finally` and `except` lead those keywords,
/// whose block builder emitted only their comments (`{$Warnings On}` before
/// `end;` in many payment hooks, `{$ENDIF}` before `End;`).
#[test]
fn directives_before_block_keywords_are_kept() {
    assert_formats_to(
        "\
unit T;
interface
implementation
procedure P;
begin
  try
    {$WARNINGS OFF}
    X;
    {$WARNINGS ON} // on
  finally
    {$IFDEF A}
    Y;
    {$ENDIF}
  end {$IFDEF B} {B} {$ENDIF};
  try
    // before except
    {$I x.inc}
  except
    {$R-}
  end;
end;
end.
",
        "\
unit T;

interface

implementation

procedure P;
begin
  try
    {$WARNINGS OFF}
    X;
    {$WARNINGS ON} // on
  finally
    {$IFDEF A}
    Y;
    {$ENDIF}
  end {$IFDEF B} {B} {$ENDIF};
  try
    // before except
    {$I x.inc}
  except
    {$R-}
  end;
end;

end.
",
    );
}

/// Comments and directives leading the same token keep their relative
/// order: a comment after a switch directive stays after it.
#[test]
fn interleaved_leading_comments_and_directives_keep_their_order() {
    assert_formats_to(
        "\
unit T;
interface
implementation
procedure P;
begin
  // before
  {$R-}
  // after
  X;
end;
end.
",
        "\
unit T;

interface

implementation

procedure P;
begin
  // before
  {$R-}
  // after
  X;
end;

end.
",
    );
}

/// A `//` comment must end its output line: code after it in the output
/// would become part of the comment (`if A // c` + newline + `then` became
/// `if A // c then`; a `//` comment trailing a standalone directive
/// swallowed the next declaration).
#[test]
fn line_comments_always_end_their_line() {
    assert_formats_to(
        "\
unit T;
interface
implementation
{$IFDEF D2010} // D2010
procedure P(A: Integer);
{$ENDIF}
begin
  if A = 0   // no mask
     then Exit;
  if A // c
  then B := 1
  else C := 2;
end;
end.
",
        "\
unit T;

interface

implementation

{$IFDEF D2010} // D2010
procedure P(A: Integer);
{$ENDIF}
begin
  if A = 0   // no mask
  then
    Exit;
  if A // c
  then
    B := 1
  else
    C := 2;
end;

end.
",
    );
}

/// Since the grammar parses `asm` blocks (l4d-1nk2), their bodies hold
/// only the identifiers as nodes: the formatter re-joined those and dropped
/// operands, commas, brackets, numbers and line breaks. The body is now
/// copied as written, with its comments and line structure.
#[test]
fn asm_bodies_are_copied_verbatim() {
    assert_formats_to(
        "\
unit T;
interface
implementation
function F(X: Integer): Integer;
asm
  // entry
  mov eax, [ebp+8]   // load
@@L1:   rol eax, 16;
        db $90, $90
  { before end }
end;
procedure Q;
begin
  X:=1;
  asm
    mov   eax,[ebp]  { frame }
    mov   x,eax
  end;
  asm end;
end;
end.
",
        "\
unit T;

interface

implementation

function F(X: Integer): Integer;
asm
  // entry
  mov eax, [ebp+8]   // load
@@L1:   rol eax, 16;
        db $90, $90
  { before end }
end;

procedure Q;
begin
  X := 1;
  asm
    mov   eax,[ebp]  { frame }
    mov   x,eax
  end;
  asm
  end;
end;

end.
",
    );
}

/// The class ancestor list was rebuilt from the text of its type
/// references, losing the directives and comments attached around them
/// (`class(TForm{$IFDEF CDLAMIA}, ISkinable{$ENDIF})` in many forms).
#[test]
fn trivia_in_class_ancestor_lists_is_kept() {
    assert_formats_to(
        "\
unit T;
interface
type
  TFoo = class(TForm{$IFDEF X}, ISkinable{$ENDIF})
  end;
  TBar = class(TA, IB{$IFNDEF NO_SAFECALL}, IC{$ENDIF}) // c
  end;
  TBaz = class({ base } TObject, {$IFDEF Y} IY {$ENDIF})
  end;
implementation
end.
",
        "\
unit T;

interface

type
  TFoo = class(TForm {$IFDEF X}, ISkinable {$ENDIF})

  end;

  TBar = class(TA, IB {$IFNDEF NO_SAFECALL}, IC {$ENDIF}) // c

  end;

  TBaz = class( { base } TObject, {$IFDEF Y} IY {$ENDIF})

  end;

implementation

end.
",
    );
}

/// A DOS end-of-file mark (Ctrl-Z) after `end.` is blank to the compiler
/// but was dropped; it is kept at the end of the output.
#[test]
fn end_of_file_mark_is_kept() {
    assert_formats_to(
        "unit T;\ninterface\nimplementation\nend.\n\u{1a}",
        "unit T;\n\ninterface\n\nimplementation\n\nend.\n\u{1a}",
    );
    assert_formats_to(
        "unit T;\ninterface\nimplementation\nend.\u{1a}\n",
        "unit T;\n\ninterface\n\nimplementation\n\nend.\n\u{1a}",
    );
}

/// A string literal made of several parts is emitted as its text; the
/// comments attached to its first and last parts lie outside that text.
#[test]
fn comments_around_multi_part_string_literals_are_kept() {
    assert_formats_to(
        "\
unit T;
interface
implementation
procedure P;
begin
  S := S + #13#10{#13#10};
  S := 'a'#9{tab} + 'b';
end;
end.
",
        "\
unit T;

interface

implementation

procedure P;
begin
  S := S + #13#10 {#13#10};
  S := 'a'#9 {tab} + 'b';
end;

end.
",
    );
}

/// `1do` is the number `1` then `do`: separating them is not a content
/// change (the net compared `1do` as one word).
#[test]
fn a_number_glued_to_a_keyword_is_separated() {
    assert_formats_to(
        "\
unit T;
interface
implementation
procedure P;
begin
  for I := 0 to 1do X;
  if A = -1then B;
end;
end.
",
        "\
unit T;

interface

implementation

procedure P;
begin
  for I := 0 to 1 do
    X;
  if A = -1 then
    B;
end;

end.
",
    );
}

/// Two rewrites change the order of compiler directives by design and are
/// recorded exactly: sorting a `uses` clause moves a conditional group with
/// its unit, and expanding `A, B: T` repeats the directives of `T`.
#[test]
fn recorded_directive_rewrites_are_accepted() {
    assert_formats_to(
        "\
unit T;
interface
uses
  Zeta, {$IFDEF Y} Yankee, {$ENDIF} Mid, {$IFDEF X} Alpha, {$ENDIF} Beta;
implementation
procedure P;
var
  ctx, ctx1: {$IFNDEF FPC}TMD5_CTX{$ELSE}TMD5Context{$IFEND};
begin
end;
end.
",
        "\
unit T;

interface

uses
  Beta,
  Mid,
  {$IFDEF X}
  Alpha,
  {$ENDIF}
  {$IFDEF Y}
  Yankee,
  {$ENDIF}
  Zeta;

implementation

procedure P;
var
  ctx: {$IFNDEF FPC}TMD5_CTX{$ELSE}TMD5Context{$IFEND};
  ctx1: {$IFNDEF FPC}TMD5_CTX{$ELSE}TMD5Context{$IFEND};
begin
end;

end.
",
    );
}

/// A string literal continued after a directive (`'a'` `{$ELSE}` `'b'`) is
/// one node whose only children are its directives: its text holds them,
/// but they were also attached to the next token and emitted twice.
#[test]
fn directives_inside_a_string_literal_are_emitted_once() {
    assert_formats_to(
        "\
unit T;
interface
const
  K = {$IFDEF SANDBOX}'/a.asmx'
      {$ELSE}'/b.asmx'{$ENDIF};
implementation
end.
",
        "\
unit T;

interface

const
  K = {$IFDEF SANDBOX} '/a.asmx'
      {$ELSE}'/b.asmx' {$ENDIF};

implementation

end.
",
    );
}

/// A calling convention after the `;` of a procedural type keeps its space
/// (`G: procedure; cdecl = nil;` became `procedure;cdecl`).
#[test]
fn procedural_type_directives_keep_their_space() {
    assert_formats_to(
        "\
unit T;
interface
var
  G: procedure; cdecl = nil;
  H: function(A: Integer): Integer; stdcall;
type
  TP = procedure(X: Integer) of object; cdecl;
implementation
end.
",
        "\
unit T;

interface

var
  G: procedure; cdecl = nil;
  H: function(A: Integer): Integer; stdcall;

type
  TP = procedure(X: Integer) of object; cdecl;

implementation

end.
",
    );
}

#[track_caller]
fn assert_package_formats_to(source: &str, expected: &str) {
    let format = |text: &str| {
        fmt4d::format_source(
            text.as_bytes(),
            &FileInfo::new(PathBuf::from("P.dpk")),
            &fmt4d::FmtConfig::default(),
            &HashSet::new(),
        )
        .unwrap_or_else(|error| panic!("formatting failed: {error}"))
    };
    let output = format(source);
    assert_eq!(output, expected, "unexpected output:\n{output}");
    assert_eq!(format(&output), output, "not idempotent");
}

/// A package source (`.dpk`) was formatted onto one line after its
/// directives: `requires rtl, vcl;contains A in 'A.pas';end.`.
#[test]
fn package_sources_keep_one_unit_per_line() {
    assert_package_formats_to(
        "\
package P;

{$R *.res}
{$IMPLICITBUILD OFF}

requires
  rtl,
  vcl; // ui

contains
  A in 'A.pas', // first
  // second
  B in 'B.pas' {Form};

end.
",
        "\
package P;

{$R *.res}
{$IMPLICITBUILD OFF}
requires
  rtl,
  vcl; // ui

contains
  A in 'A.pas', // first
  // second
  B in 'B.pas' {Form};

end.
",
    );
}

/// Whether declarations get a blank line between them depended on whether
/// they spanned one line in the source, which formatting changes: a
/// signature joined onto one line, or broken because it is too long, got a
/// blank line on one run and lost it (or gained it) on the next. It now
/// depends on whether the declaration's layout forces line breaks.
#[test]
fn blank_lines_between_declarations_are_stable() {
    assert_formats_to(
        "\
unit T;
interface
type
  TA = (a,
    b);
  TB = Integer;
  TC = class
  end;
  TD = Byte;
function F(
  A: Integer): Integer;
function G: Integer;
procedure ThisIsAVeryLongProcedureName(const FirstArgument: Integer; const SecondArgument: string; const Third: Boolean);
procedure H;
implementation
end.
",
        "\
unit T;

interface

type
  TA = (a, b);
  TB = Integer;

  TC = class
  end;

  TD = Byte;

function F(A: Integer): Integer;
function G: Integer;
procedure ThisIsAVeryLongProcedureName(
  const FirstArgument: Integer;
  const SecondArgument: string;
  const Third: Boolean
);
procedure H;

implementation

end.
",
    );
}

/// The grammar reads `-B < C` as `-(B < C)`; the breaking expression
/// builder laid out only the binary operand of that unary expression and
/// dropped the `-` (FMTBcd.pas: `else if -DecPos < Length(Result)`).
#[test]
fn unary_operator_before_a_binary_expression_is_kept() {
    assert_formats_to(
        "\
unit T;
interface
implementation
procedure P;
begin
  if A then X
  else if -B < C then Y;
end;
end.
",
        "\
unit T;

interface

implementation

procedure P;
begin
  if A then
    X
  else if - B < C then
    Y;
end;

end.
",
    );
}

/// A call whose sole argument is a bracket list (`Polyline([...])`) put
/// new commas between the list items, dropping the comments attached to
/// the source commas (RTL CaptionedDockTree.pas).
#[test]
fn comments_after_commas_of_a_hugged_bracket_argument_are_kept() {
    assert_formats_to(
        "\
unit T;
interface
implementation
procedure P;
begin
  Canvas.Polyline([Point(Left + 2, Top),
    Point(Right - 2, Top), { Top line }
    Point(Left + 3, Top)]); { Top left curve }
end;
end.
",
        "\
unit T;

interface

implementation

procedure P;
begin
  Canvas.Polyline([
    Point(Left + 2, Top),
    Point(Right - 2, Top), { Top line }
    Point(Left + 3, Top)
  ]); { Top left curve }
end;

end.
",
    );
}

/// When a broken conditional elsewhere makes the parser mask conditional
/// blocks, a masked block in a `uses` clause exists only as a virtual
/// directive leading the next unit, which the uses layout ignored
/// (RTL ExtActns.pas lost `{$IF DEFINED(CLR)} System.Runtime.InteropServices,
/// {$IFEND}`).
#[test]
fn masked_conditional_blocks_in_uses_clauses_are_kept() {
    assert_formats_to(
        "\
unit T;
interface
uses
{$IF DEFINED(CLR)}
  System.Runtime.InteropServices,
{$IFEND}
  ActnList, StdActns;
const
  A = 1;
{$IF NOT DEFINED(LINUX)}
    B = 2,
{$ENDIF}
implementation
end.
",
        "\
unit T;

interface

uses
  {$IF DEFINED(CLR)}
  System.Runtime.InteropServices,
{$IFEND}
  ActnList,
  StdActns;

const
  A = 1;

{$IF NOT DEFINED(LINUX)}
    B = 2,
{$ENDIF}
implementation

end.
",
    );
}
