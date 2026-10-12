use std::path::Path;

use lint4d::digest::{CallCount, Degradation, FileDigest, RoutineDigest, digest_source, render};

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
  L17 if A > 0 → L18 exit (runs finally L22)
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
L10 for var I := 0 to A → L11 calls Inner
L12 while Items.Count > 0 → L13 calls Items.Delete
L14 with TBOField(Fields[0]) → L15 calls Clear
L16 try
  L17 calls inherited
L18 except
  L19 on E: EAbort → L20 re-raise
  L21 on E: Exception → L22 calls Log
L24 conditional
  L24 {$IFDEF DEBUG}
    L25 calls Trace
  L26 {$ELSE}
    L27 calls Trace
L29 calls TThread.Queue
L29 anonymous method calls Ping
L30 if A = 1 → L31 calls Bar
L32 else → L32 if A = 2 → L33 calls Baz
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
        "L6 if S = '\u{e9}t' → L7 calls Bar\n"
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
        first.starts_with("L6 if (SomeVeryLongVariableName > 0) and (AnotherVeryLon…"),
        "{tree}"
    );
    assert!(first.contains("… [calls IsReady]"), "{tree}");
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

fn wrap(body: &str) -> String {
    format!("unit T;\ninterface\nimplementation\nprocedure P;\nbegin\n{body}end;\nend.\n")
}

fn tree_of(body: &str) -> String {
    let file = digest(wrap(body).as_bytes());
    render::render_tree(&routine(&file, "P").body)
}

#[test]
fn a_call_in_three_runs_is_hoisted_with_its_total_count() {
    let source = wrap(
        "  Log('a'); Foo;
  if A then
    Bar;
  Log('b');
  if B then
    Baz;
  Log('c'); Log('d');
",
    );
    let file = digest(source.as_bytes());
    let p = routine(&file, "P");
    assert_eq!(
        p.frequent_calls,
        vec![CallCount {
            name: "Log".into(),
            count: 4
        }]
    );
    assert_eq!(
        render::render_tree(&p.body),
        "L6 calls Foo
L7 if A → L8 calls Bar
L10 if B → L11 calls Baz
"
    );
}

#[test]
fn a_call_in_two_runs_stays_in_place() {
    let source = wrap(
        "  Log('a'); Foo;
  if A then
    Bar;
  Log('b'); Log('c');
",
    );
    let file = digest(source.as_bytes());
    let p = routine(&file, "P");
    assert!(p.frequent_calls.is_empty());
    assert_eq!(
        render::render_tree(&p.body),
        "L6 calls Log, Foo
L7 if A → L8 calls Bar
L9 calls Log ×2
"
    );
}

#[test]
fn lambda_calls_are_not_hoisted() {
    let source = wrap(
        "  Q1(procedure begin Ping; end);
  Q2(procedure begin Ping; end);
  Q3(procedure begin Ping; end);
",
    );
    let file = digest(source.as_bytes());
    let p = routine(&file, "P");
    assert!(p.frequent_calls.is_empty());
    assert_eq!(
        render::render_tree(&p.body)
            .matches("anonymous method calls Ping")
            .count(),
        3
    );
}

#[test]
fn hoisted_calls_render_after_the_objects_line() {
    let source = wrap("  Log('a');\n  if A then\n    Log('b');\n  if B then\n    Log('c');\n");
    let file = digest(source.as_bytes());
    let p = routine(&file, "P");
    let text = render::render_routine(p, "t.pas", None);
    assert!(
        text.lines().nth(1) == Some("frequent calls: Log ×3"),
        "{text}"
    );
    assert!(!text.contains("calls Log\n"), "{text}");
    let json = render::render_json("t.pas", &[p]);
    assert!(json.contains("\"frequent_calls\""), "{json}");
}

#[test]
fn a_factless_if_is_dropped_and_a_call_in_its_condition_is_not() {
    assert_eq!(
        tree_of("  if A then\n    X := 1;\n  Bar;\n"),
        "L8 calls Bar\n"
    );
    assert_eq!(
        tree_of("  if Check(X) then\n    Y := 1;\n"),
        "L6 if Check(X)\n"
    );
}

#[test]
fn an_empty_if_stays_before_a_kept_else_and_an_empty_else_goes() {
    assert_eq!(
        tree_of("  if A then\n    X := 1\n  else\n    Bar;\n"),
        "L6 if A
L9 else → L9 calls Bar
"
    );
    assert_eq!(
        tree_of("  if A then\n    Bar\n  else\n    X := 1;\n"),
        "L6 if A → L7 calls Bar\n"
    );
}

#[test]
fn empty_loops_nested_in_empty_ifs_vanish() {
    assert_eq!(
        tree_of("  if A then\n    for var I := 0 to 3 do\n      X := I;\n"),
        ""
    );
}

#[test]
fn a_factless_try_is_dropped() {
    assert_eq!(tree_of("  try\n    X := 1;\n  except\n  end;\n"), "");
    assert_eq!(
        tree_of("  try\n    X := 1;\n  finally\n    Y := 2;\n  end;\n"),
        ""
    );
}

#[test]
fn a_kept_try_keeps_its_empty_handlers() {
    assert_eq!(
        tree_of("  try\n    Bar;\n  except\n    on E: Exception do\n      X := 1;\n  end;\n"),
        "L6 try
  L7 calls Bar
L8 except
  L9 on E: Exception
"
    );
    assert_eq!(
        tree_of("  try\n    Bar;\n  except\n  end;\n"),
        "L6 try
  L7 calls Bar
L8 except
"
    );
}

#[test]
fn empty_case_arms_are_dropped_and_an_empty_case_goes() {
    assert_eq!(
        tree_of("  case A of\n    1: X := 1;\n    2: Bar;\n  else\n    Y := 2;\n  end;\n"),
        "L6 case A
  2: L8 calls Bar
"
    );
    assert_eq!(
        tree_of("  case A of\n    1: X := 1;\n  else\n    Y := 2;\n  end;\n"),
        ""
    );
    assert_eq!(
        tree_of("  case Pick(A) of\n    1: X := 1;\n  end;\n"),
        "L6 case Pick(A)\n"
    );
}

#[test]
fn dropping_empty_controls_leaves_the_markers_alone() {
    let source = wrap(
        "  if A then
    X := 1;
  try
    X := 2;
  except
  end;
  DB.StartTransaction;
  if B then
    Exit;
  raise Exception.Create('x');
",
    );
    let file = digest(source.as_bytes());
    let markers = routine(&file, "P").markers();
    assert!(markers.tx);
    assert_eq!((markers.raises, markers.exits), (1, 1));
}

#[test]
fn labels_are_cut_at_fifty_characters() {
    let condition =
        "(AAAAAAAAAA = 1) and (BBBBBBBBBB = 2) and (CCCCCCCCCC = 3) and (DDDDDDDDDD = 4)";
    let tree = tree_of(&format!("  if {condition} then\n    Bar;\n"));
    let expected: String = condition.chars().take(50).collect();
    assert_eq!(
        tree.lines().next().unwrap(),
        format!("L6 if {expected}… → L7 calls Bar")
    );
}

#[test]
fn a_bare_except_body_keeps_an_empty_if_before_its_else() {
    assert_eq!(
        tree_of(
            "  try\n    Bar;\n  except\n    if A then\n      X := 1\n    else\n      Touch;\n  end;\n"
        ),
        "L6 try
  L7 calls Bar
L8 except
  L9 if A
  L12 else → L12 calls Touch
"
    );
}

#[test]
fn a_call_hidden_by_the_label_cut_keeps_an_otherwise_empty_if() {
    let tree = tree_of(
        "  if (AAAAAAAAAA = 1) and (BBBBBBBBBB = 2) and (CCCCCCCCCC = 3) and Ready(X) then\n    Y := 1;\n",
    );
    assert!(tree.starts_with("L6 if (AAAAAAAAAA"), "{tree}");
    assert!(tree.trim_end().ends_with("… [calls Ready]"), "{tree}");
    assert_eq!(tree.lines().count(), 1, "{tree}");
}

#[test]
fn json_omits_frequent_calls_when_empty() {
    let file = digest(wrap("  Bar;\n").as_bytes());
    let p = routine(&file, "P");
    assert!(!render::render_json("t.pas", &[p]).contains("frequent_calls"));
}

#[test]
fn single_child_chains_fold_onto_one_line() {
    assert_eq!(
        tree_of("  if A then\n    Bar\n  else\n    Baz;\n"),
        "L6 if A → L7 calls Bar
L9 else → L9 calls Baz
"
    );
    assert_eq!(
        tree_of("  if A then\n    for var I := 0 to 3 do\n      with X do\n        Bar;\n"),
        "L6 if A → L7 for var I := 0 to 3 → L8 with X → L9 calls Bar\n"
    );
    assert_eq!(
        tree_of(
            "  case A of\n    1: if B then Bar;\n    2: Baz;\n    3: begin Bar; Baz; end;\n  end;\n"
        ),
        "L6 case A
  1: L7 if B → L7 calls Bar
  2: L8 calls Baz
  3: L9 calls Bar, Baz
"
    );
}

#[test]
fn chains_ending_in_several_children_or_an_unfoldable_control_stay_nested() {
    assert_eq!(
        tree_of(
            "  if A then\n    for var I := 0 to 3 do\n    begin\n      Bar;\n      DB.Commit;\n    end;\n"
        ),
        "L6 if A
  L7 for var I := 0 to 3
    L9 calls Bar
    L10 tx commit DB.Commit
"
    );
    assert_eq!(
        tree_of("  if A then\n    try\n      Bar;\n    finally\n      Baz;\n    end;\n"),
        "L6 if A
  L7 try
    L8 calls Bar
  L9 finally
    L10 calls Baz
"
    );
}

#[test]
fn collapsed_controls_are_not_folded_into_a_chain() {
    let source = wrap("  if A then\n    if B then\n      Bar;\n");
    let file = digest(source.as_bytes());
    let p = routine(&file, "P");
    let text = render::render_routine(p, "t.pas", Some(1));
    assert!(text.contains("L6-8 if A … (3 lines, calls Bar)"), "{text}");
}

#[test]
fn raise_text_is_cut_at_fifty_characters() {
    let text = "Exception.Create('AAAAAAAAAA BBBBBBBBBB CCCCCCCCCC DDDDDDDDDD EEEEEEEEEE')";
    let tree = tree_of(&format!("  raise {text};\n"));
    let expected: String = text.chars().take(50).collect();
    assert_eq!(tree, format!("L6 raise {expected}…\n"));
}
