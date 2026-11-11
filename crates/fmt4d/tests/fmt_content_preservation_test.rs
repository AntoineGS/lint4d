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
