use std::path::Path;

use lint4d::digest::{Degradation, FileDigest, RoutineDigest, digest_source, render};

fn digest(source: &[u8]) -> FileDigest {
    digest_source(Path::new("t.pas"), source).expect("digest")
}

fn routine<'a>(file: &'a FileDigest, name: &str) -> &'a RoutineDigest {
    file.routines
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| {
            panic!(
                "{name} not in {:?}",
                file.routines.iter().map(|r| &r.name).collect::<Vec<_>>()
            )
        })
}

const LIFECYCLE: &str = r#"unit T;
interface
implementation
procedure Foo(A: Integer);
var
  L: TStringList;
begin
  Log('start'); Log('again'); Prepare;
  L := TStringList.Create;
  try
    case A of
      1: Bar;
      2: begin Baz; Qux; end;
    else
      raise Exception.Create('bad');
    end;
    if A > 0 then
      Exit;
    DB.StartTransaction;
    X := 1;
  finally
    L.Free;
  end;
end;
end.
"#;

const LIFECYCLE_TREE: &str = "L8 calls Log ×2, Prepare
L9 new TStringList -> L
L10 try
  L11 case A
    1: L12 calls Bar
    2: L13 calls Baz, Qux
    else: L15 raise Exception.Create('bad')
  L17 if A > 0
    L18 exit (runs finally L22)
  L19 tx start DB.StartTransaction
L21 finally
  L22 free L
";

#[test]
fn facts_sit_inline_in_the_control_tree() {
    let file = digest(LIFECYCLE.as_bytes());
    assert_eq!(
        render::render_tree(&routine(&file, "Foo").body),
        LIFECYCLE_TREE
    );
}

const CONSTRUCTS: &str = r#"unit T;
interface
implementation
procedure Outer(A: Integer);
  procedure Inner;
  begin
    Bar;
  end;
begin
  for var I := 0 to A do
    Inner;
  while Items.Count > 0 do
    Items.Delete(0);
  with TBOField(Fields[0]) do
    Clear;
  try
    inherited;
  except
    on E: EAbort do
      raise;
    on E: Exception do
      Log(E.Message);
  end;
  {$IFDEF DEBUG}
  Trace('x');
  {$ELSE}
  Trace('y');
  {$ENDIF}
  TThread.Queue(nil, procedure begin Ping; end);
  if A = 1 then
    Bar
  else if A = 2 then
    Baz;
end;
end.
"#;

#[test]
fn loops_handlers_conditionals_and_lambdas() {
    let file = digest(CONSTRUCTS.as_bytes());
    let outer = routine(&file, "Outer");
    assert_eq!(
        render::render_tree(&outer.body),
        "L5 nested routine Outer.Inner
L10 for var I := 0 to A
  L11 calls Inner
L12 while Items.Count > 0
  L13 calls Items.Delete
L14 with TBOField(Fields[0])
  L15 calls Clear
L16 try
  L17 calls inherited
L18 except
  L19 on E: EAbort
    L20 re-raise
  L21 on E: Exception
    L22 calls Log
L24 conditional
  L24 {$IFDEF DEBUG}
    L25 calls Trace
  L26 {$ELSE}
    L27 calls Trace
L29 calls TThread.Queue
L29 anonymous method calls Ping
L30 if A = 1
  L31 calls Bar
L32 else
  L32 if A = 2
    L33 calls Baz
"
    );
    assert_eq!(
        outer.degraded,
        vec![
            Degradation::Conditional { regions: 1 },
            Degradation::AnonymousMethods
        ]
    );
    assert_eq!(
        render::render_tree(&routine(&file, "Outer.Inner").body),
        "L7 calls Bar\n"
    );
}

#[test]
fn latin1_condition_is_decoded() {
    let mut source =
        b"unit T;\ninterface\nimplementation\nprocedure P(S: string);\nbegin\n  if S = '".to_vec();
    source.push(0xE9);
    source.extend_from_slice(b"t' then\n    Bar;\nend;\nend.\n");
    let file = digest(&source);
    assert_eq!(
        render::render_tree(&routine(&file, "P").body),
        "L6 if S = '\u{e9}t'\n  L7 calls Bar\n"
    );
}

#[test]
fn crlf_source_has_same_lines() {
    let crlf = LIFECYCLE.replace('\n', "\r\n");
    let file = digest(crlf.as_bytes());
    let tree = render::render_tree(&routine(&file, "Foo").body);
    assert_eq!(tree, LIFECYCLE_TREE);
    assert!(!tree.contains('\r'));
}

#[test]
fn long_conditions_are_truncated_and_list_hidden_calls() {
    let source = "unit T;\ninterface\nimplementation\nprocedure P;\nbegin\n  if (SomeVeryLongVariableName > 0) and (AnotherVeryLongVariableName < 10) and IsReady(X) then\n    Bar;\nend;\nend.\n";
    let file = digest(source.as_bytes());
    let tree = render::render_tree(&routine(&file, "P").body);
    let first = tree.lines().next().unwrap();
    assert!(
        first.starts_with(
            "L6 if (SomeVeryLongVariableName > 0) and (AnotherVeryLongVariableName < 10)"
        ),
        "{tree}"
    );
    assert!(first.ends_with("… [calls IsReady]"), "{tree}");
}

#[test]
fn empty_and_forward_routines() {
    let source = "unit T;\ninterface\nimplementation\nprocedure Fwd; forward;\nprocedure Empty;\nbegin\nend;\nprocedure Fwd;\nbegin\n  Bar;\nend;\nend.\n";
    let file = digest(source.as_bytes());
    let names: Vec<&str> = file.routines.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["Empty", "Fwd"]);
    assert_eq!(routine(&file, "Fwd").start_line, 8);
    let empty = routine(&file, "Empty");
    assert!(empty.body.is_empty());
    assert_eq!(
        render::render_routine(empty, "t.pas", None),
        "Empty  t.pas:5-7 (3 lines)\n"
    );
}
