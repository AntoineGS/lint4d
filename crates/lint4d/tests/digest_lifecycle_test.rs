use std::path::Path;

use lint4d::digest::{Escape, EscapeKind, NotFreedReason, ObjectVerdict, Verdict, digest_source};

fn objects(body: &str, routine: &str) -> Vec<ObjectVerdict> {
    let source = format!("unit T;\ninterface\nimplementation\n{body}end.\n");
    let file = digest_source(Path::new("t.pas"), source.as_bytes()).expect("digest");
    file.routines
        .into_iter()
        .find(|r| r.name == routine)
        .expect("routine")
        .objects
}

fn verdict(body: &str, routine: &str) -> Verdict {
    let objects = objects(body, routine);
    assert_eq!(objects.len(), 1, "{objects:?}");
    objects[0].verdict.clone()
}

fn object(var: &str, class: &str, line: usize, verdict: Verdict) -> ObjectVerdict {
    ObjectVerdict {
        var: var.into(),
        class: class.into(),
        line,
        verdict,
    }
}

#[test]
fn freed_in_finally_on_every_copy() {
    let body = "procedure Foo(A: Integer);
var
  L: TStringList;
begin
  L := TStringList.Create;
  try
    if A > 0 then
      Exit;
    L.Add('x');
  finally
    L.Free;
  end;
end;
";
    assert_eq!(
        objects(body, "Foo"),
        vec![object("L", "TStringList", 8, Verdict::FreedAllPaths)]
    );
}

#[test]
fn raising_call_before_try_escapes_through_outer_except() {
    let body = "procedure W;
var
  L: TStringList;
begin
  try
    L := TStringList.Create;
    Db.Open;
    try
      L.Add('x');
    finally
      L.Free;
    end;
  except
    on E: Exception do
      Log(E.Message);
  end;
end;
";
    assert_eq!(
        verdict(body, "W"),
        Verdict::Escapes {
            via: vec![Escape {
                line: 10,
                how: EscapeKind::Exception
            }],
            more: false
        }
    );
}

#[test]
fn straight_line_raise_escapes() {
    let body = "procedure S;
var
  L: TStringList;
begin
  L := TStringList.Create;
  L.Add('x');
  L.Free;
end;
";
    assert_eq!(
        verdict(body, "S"),
        Verdict::Escapes {
            via: vec![Escape {
                line: 9,
                how: EscapeKind::Exception
            }],
            more: false
        }
    );
}

#[test]
fn created_in_loop_and_freed_after_is_recreated() {
    let body = "procedure P;
var
  I: Integer;
  L: TStringList;
begin
  for I := 0 to 3 do
    L := TStringList.Create;
  L.Free;
end;
";
    let Verdict::Escapes { via, .. } = verdict(body, "P") else {
        panic!("a creation inside a loop is not freed before it is re-created");
    };
    assert!(
        via.contains(&Escape {
            line: 10,
            how: EscapeKind::ReCreated
        }),
        "{via:?}"
    );
}

#[test]
fn free_and_nil_and_destroy_count_as_frees() {
    let body = "procedure A;
var
  L, M: TStringList;
begin
  L := TStringList.Create;
  try
    M := TStringList.Create;
    try
      Work;
    finally
      M.Destroy;
    end;
  finally
    FreeAndNil(L);
  end;
end;
";
    assert_eq!(
        objects(body, "A"),
        vec![
            object("L", "TStringList", 8, Verdict::FreedAllPaths),
            object("M", "TStringList", 10, Verdict::FreedAllPaths),
        ]
    );
}

#[test]
fn reassignment_is_unknown() {
    let body = "procedure R;
var
  L: TStringList;
begin
  L := TStringList.Create;
  try
    L := nil;
  finally
    L.Free;
  end;
end;
";
    assert_eq!(
        verdict(body, "R"),
        Verdict::Unknown {
            why: "reassigned at L10".into()
        }
    );
}

#[test]
fn result_non_local_and_never_freed() {
    let body = "function F: TList;
var
  L: TList;
begin
  Result := TList.Create;
  FItems := TList.Create;
  L := TList.Create;
end;
";
    assert_eq!(
        objects(body, "F"),
        vec![
            object(
                "Result",
                "TList",
                8,
                Verdict::NotFreedHere {
                    why: NotFreedReason::Result
                }
            ),
            object(
                "FItems",
                "TList",
                9,
                Verdict::NotFreedHere {
                    why: NotFreedReason::NonLocal
                }
            ),
            object(
                "L",
                "TList",
                10,
                Verdict::NotFreedHere {
                    why: NotFreedReason::NeverFreed
                }
            ),
        ]
    );
}

#[test]
fn frees_in_every_case_arm_cover_the_arms() {
    let body = "procedure C(A: Integer);
var
  L: TStringList;
begin
  L := TStringList.Create;
  case A of
    1: L.Free;
  else
    FreeAndNil(L);
  end;
end;
";
    match verdict(body, "C") {
        Verdict::FreedAllPaths => {}
        Verdict::Escapes { via, .. } => assert!(
            via.iter()
                .all(|e| e.line == 9 && e.how == EscapeKind::Exception),
            "only the case selector itself may escape: {via:?}"
        ),
        other => panic!("unexpected {other:?}"),
    }
}

fn escapes_of(body: &str, routine: &str) -> Vec<Escape> {
    match verdict(body, routine) {
        Verdict::Escapes { via, more: false } => via,
        other => panic!("expected complete escapes, got {other:?}"),
    }
}

fn esc(line: usize, how: EscapeKind) -> Escape {
    Escape { line, how }
}

#[test]
fn conditional_free_is_not_all_paths() {
    let body = "procedure Q(A: Boolean);
var
  L: TStringList;
begin
  L := TStringList.Create;
  if A then
    L.Free;
end;
";
    assert_eq!(
        escapes_of(body, "Q"),
        vec![
            esc(9, EscapeKind::Exception),
            esc(11, EscapeKind::FallsOffEnd)
        ]
    );
}

#[test]
fn exit_before_free_escapes() {
    let body = "procedure E(A: Boolean);
var
  L: TStringList;
begin
  L := TStringList.Create;
  if A then
    Exit;
  L.Free;
end;
";
    assert_eq!(
        escapes_of(body, "E"),
        vec![esc(9, EscapeKind::Exception), esc(10, EscapeKind::Exit)]
    );
}

#[test]
fn free_in_except_only_misses_the_normal_path() {
    let body = "procedure Y;
var
  L: TStringList;
begin
  L := TStringList.Create;
  try
    Work;
  except
    L.Free;
  end;
end;
";
    assert_eq!(
        escapes_of(body, "Y"),
        vec![esc(14, EscapeKind::FallsOffEnd)]
    );
}

#[test]
fn hidden_block_before_creation_in_a_loop_is_unknown() {
    let body = "procedure P(N: Integer);
var I: Integer;
begin
  for I := 0 to N do
  begin
    {$IF DEFINED(X)}
    rappel: developper en 32 bits pour plus de stabilite
    {$IFEND}
    FreeAndNil(FObj);
    FObj := TFoo.Create;
  end;
  FreeAndNil(FObj);
end;
";
    assert_eq!(
        verdict(body, "P"),
        Verdict::Unknown {
            why: "hidden directive block at L9".into()
        }
    );
}

#[test]
fn hidden_block_wins_over_never_freed() {
    let body = "procedure H;
var
  L: TStringList;
begin
  {$IF DEFINED(X)}
  rappel: developper en 32 bits pour plus de stabilite
  {$IFEND}
  L := TStringList.Create;
end;
";
    assert_eq!(
        verdict(body, "H"),
        Verdict::Unknown {
            why: "hidden directive block at L8".into()
        }
    );
}

#[test]
fn assignment_in_lambda_is_a_reassignment() {
    let body = "procedure Lm;
var
  L: TStringList;
begin
  L := TStringList.Create;
  try
    Run(procedure begin L := nil; end);
  finally
    L.Free;
  end;
end;
";
    assert_eq!(
        verdict(body, "Lm"),
        Verdict::Unknown {
            why: "reassigned at L10".into()
        }
    );
}

#[test]
fn assignment_in_nested_routine_is_a_reassignment() {
    let body = "procedure Nr;
var
  L: TStringList;
  procedure Reset;
  begin
    L := nil;
  end;
begin
  L := TStringList.Create;
  try
    Reset;
  finally
    L.Free;
  end;
end;
";
    assert_eq!(
        verdict(body, "Nr"),
        Verdict::Unknown {
            why: "reassigned at L9".into()
        }
    );
}
