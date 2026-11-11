//! Content-preservation safety net (l4d-um8g): `format_source` must refuse
//! to return output that drops or adds source content, so no caller (CLI,
//! format-on-save through pascal-lsp) can write a lossy result.

use pascal_core::FileInfo;
use std::collections::HashSet;
use std::path::PathBuf;

/// Minimized from Spring4D `Spring.ValueConverters.pas`: the formatter drops
/// `{$REGION}`/`{$ENDREGION}` directives (and their captions) around
/// `{$IFNDEF}`-guarded type declarations.
const REGION_AROUND_IFNDEF_TYPE: &str = "\
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
";

/// Minimized from `cashzNewPetty.pas`: the formatter drops a conditional
/// inside a class ancestor list.
const IFDEF_IN_CLASS_ANCESTORS: &str = "\
unit T;
interface
type
  TFoo = class(TForm{$IFDEF X}, ISkinable{$ENDIF})
  end;
implementation
end.
";

fn format(source: &str) -> fmt4d::Result<String> {
    fmt4d::format_source(
        source.as_bytes(),
        &FileInfo::new(PathBuf::from("T.pas")),
        &fmt4d::FmtConfig::default(),
        &HashSet::new(),
    )
}

fn assert_refused(name: &str, source: &str, lost: &[&str]) {
    match format(source) {
        Ok(output) => panic!("{name}: lossy output was returned instead of refused:\n{output}"),
        Err(error) => {
            assert!(
                matches!(error, fmt4d::FmtError::ContentChanged { .. }),
                "{name}: expected ContentChanged, got {error:?}"
            );
            let message = error.to_string();
            for word in lost {
                assert!(
                    message.contains(word),
                    "{name}: summary does not name lost `{word}`: {message}"
                );
            }
        }
    }
}

#[test]
fn region_directives_around_ifndef_type_declarations_are_refused_not_dropped() {
    assert_refused(
        "region_around_ifndef_type",
        REGION_AROUND_IFNDEF_TYPE,
        &["`REGION` x2 (first at line 7)", "`ENDREGION` x2"],
    );
}

#[test]
fn ifdef_inside_class_ancestor_list_is_refused_not_dropped() {
    assert_refused(
        "ifdef_in_class_ancestors",
        IFDEF_IN_CLASS_ANCESTORS,
        &["`IFDEF` (line 4)", "`ENDIF` (line 4)"],
    );
}

#[test]
fn content_preserving_formatting_still_succeeds() {
    let source = "unit T;\ninterface\nUSES SysUtils, Classes; // units\nimplementation\nprocedure P;\nbegin\n  if A then B;  { done }\nend;\nend.\n";
    let output = format(source).expect("content-preserving formatting must succeed");
    assert_ne!(output, source, "the fixture is expected to be reformatted");
    assert!(output.contains("uses"));
    assert!(output.contains("// units"));
    assert!(output.contains("{ done }"));
}
