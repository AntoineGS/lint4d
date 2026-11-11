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
