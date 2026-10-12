use std::collections::HashSet;

use cfg_pascal::build_file_cfgs;
use cfg_pascal::cfg_core::{BasicBlockKind, EdgeKind};
use tree_sitter::Parser;

const SOURCE: &[u8] = br#"
unit Copies;
interface
implementation

procedure Guarded(A: Integer);
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

end.
"#;

fn range_of(text: &str) -> std::ops::Range<usize> {
    let source = std::str::from_utf8(SOURCE).unwrap();
    let start = source.find(text).expect(text);
    start..start + text.len()
}

#[test]
fn finally_copies_are_grouped_by_statement() {
    let mut parser = Parser::new();
    parser.set_language(&cfg_pascal::LANGUAGE.into()).unwrap();
    let tree = parser.parse(SOURCE, None).unwrap();
    let cfgs = build_file_cfgs(&tree, SOURCE);
    let cfg = cfgs.iter().find(|c| c.proc_name == "Guarded").unwrap();

    let frees = cfg.blocks_for_stmt(range_of("L.Free"));
    assert!(
        frees.len() >= 2,
        "finally is copied per continuation: {frees:?}"
    );
    assert!(frees
        .iter()
        .all(|b| cfg.graph[b.index()].kind == BasicBlockKind::FinallyHandler));

    let create = cfg.blocks_for_stmt(range_of("L := TStringList.Create"));
    assert_eq!(create.len(), 1);
    let avoid: HashSet<_> = frees.iter().copied().collect();
    assert!(
        cfg.path_avoiding(create[0], cfg.exit, &avoid, &[EdgeKind::ExceptionThrow])
            .is_none(),
        "every continuation after the creation runs a finally copy"
    );
    assert!(
        frees.iter().any(|f| !cfg.must_pass_through(create[0], *f)),
        "no single copy lies on every path"
    );
}
