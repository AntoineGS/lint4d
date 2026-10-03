use cfg_core::{BlockId, Cfg, EdgeKind};
use cfg_pascal::{
    build_file_cfgs, build_file_cfgs_in_project, ProjectSnapshot, ProjectSourceId, ProjectUnitId,
    ProjectUnitInput,
};
use tree_sitter::{Parser, Tree};

fn parse_clean(source: &str) -> Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&cfg_pascal::LANGUAGE.into())
        .expect("failed to set Pascal language");
    let tree = parser
        .parse(source.as_bytes(), None)
        .expect("parser returned no tree");
    assert!(
        !tree.root_node().has_error(),
        "semantic-review fixture must be parser-clean:\n{}",
        tree.root_node().to_sexp()
    );
    tree
}

fn cfg_for<'a>(cfgs: &'a [Cfg], name: &str) -> &'a Cfg {
    cfgs.iter()
        .find(|cfg| cfg.proc_name == name)
        .unwrap_or_else(|| panic!("CFG for {name:?} not found"))
}

fn block_with_stmt(cfg: &Cfg, source: &str, kind: &str, text: &str) -> BlockId {
    let blocks: Vec<_> = cfg
        .graph
        .node_indices()
        .filter_map(|index| {
            let block = &cfg.graph[index];
            block
                .stmts
                .iter()
                .any(|stmt| {
                    stmt.node_kind == kind
                        && source
                            .get(stmt.byte_range.clone())
                            .is_some_and(|stmt_text| stmt_text.contains(text))
                })
                .then(|| BlockId::from(index))
        })
        .collect();
    assert_eq!(
        blocks.len(),
        1,
        "expected one {kind:?} statement containing {text:?}, got {blocks:?}"
    );
    blocks[0]
}

fn successors(cfg: &Cfg, from: BlockId) -> Vec<(BlockId, EdgeKind)> {
    cfg.graph
        .edge_indices()
        .filter_map(|edge| {
            let (source, target) = cfg.graph.edge_endpoints(edge)?;
            (source == from.index()).then(|| (BlockId::from(target), cfg.graph[edge].clone()))
        })
        .collect()
}

fn successful_raise_block(cfg: &Cfg, raise: BlockId) -> BlockId {
    successors(cfg, raise)
        .into_iter()
        .find_map(|(target, kind)| {
            (kind == EdgeKind::Normal && cfg.graph[target.index()].stmts.is_empty())
                .then_some(target)
        })
        .expect("constructor raise must have a synthetic successful-raise block")
}

fn assert_successful_raise_retains_handlers(
    source: &str,
    procedure: &str,
    raise_text: &str,
    handler_texts: &[&str],
) {
    let tree = parse_clean(source);
    let cfgs = build_file_cfgs(&tree, source.as_bytes());
    let cfg = cfg_for(&cfgs, procedure);
    let raise = block_with_stmt(cfg, source, "raise", raise_text);
    let successful_raise = successful_raise_block(cfg, raise);
    let successful_successors = successors(cfg, successful_raise);

    for handler_text in handler_texts {
        let handler = block_with_stmt(cfg, source, "statement", handler_text);
        assert!(
            successful_successors.contains(&(handler, EdgeKind::ExceptionThrow)),
            "successful raise must retain handler {handler_text:?}"
        );
    }
}

fn assert_successful_raise_reaches(source: &str, procedure: &str, raise_text: &str, handler: &str) {
    let tree = parse_clean(source);
    let cfgs = build_file_cfgs(&tree, source.as_bytes());
    let cfg = cfg_for(&cfgs, procedure);
    let raise = block_with_stmt(cfg, source, "raise", raise_text);
    let successful_raise = successful_raise_block(cfg, raise);
    let handler = block_with_stmt(cfg, source, "statement", handler);

    assert!(
        successors(cfg, successful_raise).contains(&(handler, EdgeKind::ExceptionThrow)),
        "a successful or conservatively unresolved raise must retain {handler:?}"
    );
}

#[test]
fn same_module_qualification_uses_module_scope_not_a_local_shadow() {
    assert_successful_raise_reaches(
        r#"
unit U;
interface
implementation
type
  E = class constructor Create; end;
  RootE = E;
procedure P;
type
  E = class constructor Create; end;
begin
  try
    raise U.E.Create;
  except
    on E do WrongLocal;
    on RootE do CorrectRoot;
  end;
end;
end.
"#,
        "P",
        "raise U.E.Create",
        "CorrectRoot",
    );
}

#[test]
fn a_value_shadowing_the_module_qualifier_blocks_module_fallback() {
    let source = r#"
unit U;
interface
implementation
type
  E = class constructor Create; end;
  RootE = E;
procedure P(U: Integer);
begin
  try
    raise U.E.Create;
  except
    on E do First;
    on RootE do Second;
  end;
end;
end.
"#;
    let tree = parse_clean(source);
    let cfgs = build_file_cfgs(&tree, source.as_bytes());
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, source, "raise", "raise U.E.Create");
    let successful_raise = successful_raise_block(cfg, raise);
    let first = block_with_stmt(cfg, source, "statement", "First");
    let second = block_with_stmt(cfg, source, "statement", "Second");

    let successful_successors = successors(cfg, successful_raise);
    assert!(successful_successors.contains(&(first, EdgeKind::ExceptionThrow)));
    assert!(successful_successors.contains(&(second, EdgeKind::ExceptionThrow)));
    assert!(successful_successors.contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn ordinary_comments_do_not_disable_precise_same_file_dispatch() {
    let source = r#"
unit CommentBarrier;
interface
implementation
type
  A = class constructor Create; end;
  B = class constructor Create; end;
procedure P;
begin
  try
    { this comment is not a preprocessor directive }
    raise A.Create;
  except
    on B do Wrong;
    on A do Right;
  end;
end;
end.
"#;
    let tree = parse_clean(source);
    let cfgs = build_file_cfgs(&tree, source.as_bytes());
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, source, "raise", "raise A.Create");
    let successful_raise = successful_raise_block(cfg, raise);
    let right = block_with_stmt(cfg, source, "statement", "Right");

    assert_eq!(
        successors(cfg, successful_raise),
        vec![(right, EdgeKind::ExceptionThrow)],
        "ordinary comments must not turn a proven constructor into an unknown raise"
    );
}

#[test]
fn inherited_member_shadowing_does_not_fall_back_to_a_global_type() {
    assert_successful_raise_reaches(
        r#"
unit U;
interface
implementation
type
  E = class constructor Create; end;
  Other = class(E) constructor Create; end;
  Meta = class of E;
  Parent = class E: Meta; end;
  Child = class(Parent) procedure P; end;
procedure Child.P;
begin
  E := Other;
  try
    raise E.Create;
  except
    on Other do First;
    on U.E do Second;
  end;
end;
end.
"#,
        "Child.P",
        "raise E.Create",
        "First",
    );
}

#[test]
fn with_bodies_do_not_resolve_unqualified_receivers_as_globals() {
    assert_successful_raise_reaches(
        r#"
unit U;
interface
implementation
type
  E = class constructor Create; end;
  Other = class(E) constructor Create; end;
  Meta = class of E;
  Holder = class E: Meta; end;
procedure P(H: Holder);
begin
  H.E := Other;
  with H do
  try
    raise E.Create;
  except
    on Other do First;
    on U.E do Second;
  end;
end;
end.
"#,
        "P",
        "raise E.Create",
        "First",
    );
}

#[test]
fn with_dotted_heads_do_not_treat_members_as_module_qualifiers() {
    let source = r#"
unit U;
interface
implementation
type
  E = class constructor Create; end;
  B = class(E) constructor Create; end;
  Meta = class of E;
  Fields = class
    E: Meta;
  end;
  Holder = class
    U: Fields;
  end;
procedure P(H: Holder);
begin
  H.U.E := B;
  try
    with H do
      raise U.E.Create;
  except
    on B do First;
    on E do Second;
  end;
end;
end.
"#;
    let tree = parse_clean(source);
    let cfgs = build_file_cfgs(&tree, source.as_bytes());
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, source, "raise", "raise U.E.Create");
    let successful_raise = successful_raise_block(cfg, raise);
    let first = block_with_stmt(cfg, source, "statement", "First");
    let second = block_with_stmt(cfg, source, "statement", "Second");

    let successful_successors = successors(cfg, successful_raise);
    assert!(
        successful_successors.contains(&(first, EdgeKind::ExceptionThrow)),
        "a with-provided dotted head must not lose the subclass handler"
    );
    assert!(successful_successors.contains(&(second, EdgeKind::ExceptionThrow)));
    assert!(
        successful_successors.contains(&(cfg.exit, EdgeKind::ExceptionThrow)),
        "an implicit with member must keep the unmatched outward path"
    );
}

#[test]
fn with_nested_dotted_heads_do_not_fall_back_to_global_types() {
    let source = r#"
unit U;
interface
implementation
type
  E = class constructor Create; end;
  B = class(E) constructor Create; end;
  Types = class
    type
      ErrorType = E;
  end;
  Holder = class
    Types: Types;
  end;
procedure P(H: Holder);
begin
  try
    with H do
      raise Types.ErrorType.Create;
  except
    on B do First;
    on E do Second;
  end;
end;
end.
"#;
    let tree = parse_clean(source);
    let cfgs = build_file_cfgs(&tree, source.as_bytes());
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, source, "raise", "raise Types.ErrorType.Create");
    let successful_raise = successful_raise_block(cfg, raise);
    let first = block_with_stmt(cfg, source, "statement", "First");
    let second = block_with_stmt(cfg, source, "statement", "Second");

    let successful_successors = successors(cfg, successful_raise);
    assert!(successful_successors.contains(&(first, EdgeKind::ExceptionThrow)));
    assert!(successful_successors.contains(&(second, EdgeKind::ExceptionThrow)));
    assert!(successful_successors.contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn initialized_typed_inline_variables_shadow_global_exception_types_conservatively() {
    assert_successful_raise_retains_handlers(
        r#"
unit U;
interface
implementation
type
  E = class constructor Create; end;
  B = class(E) end;
  Meta = class of E;
procedure P;
begin
  var E: Meta := B;
  try
    raise E.Create;
  except
    on U.B do Correct;
    on U.E do Wrong;
  end;
end;
end.
"#,
        "P",
        "raise E.Create",
        &["Correct"],
    );
}

#[test]
fn initialized_inferred_inline_variables_shadow_global_exception_types_conservatively() {
    assert_successful_raise_retains_handlers(
        r#"
unit U;
interface
implementation
type
  E = class constructor Create; end;
  B = class(E) end;
procedure P;
begin
  var E := B;
  try
    raise E.Create;
  except
    on U.B do Correct;
    on U.E do Wrong;
  end;
end;
end.
"#,
        "P",
        "raise E.Create",
        &["Correct"],
    );
}

#[test]
fn nested_inline_var_definitions_are_collected_without_global_fallback() {
    assert_successful_raise_retains_handlers(
        r#"
unit U;
interface
implementation
type
  E = class constructor Create; end;
  B = class(E) end;
  Meta = class of E;
procedure P;
begin
  begin
    var E: Meta;
    E := B;
    try
      raise E.Create;
    except
      on U.B do Correct;
      on U.E do Wrong;
    end;
  end;
end;
end.
"#,
        "P",
        "raise E.Create",
        &["Correct"],
    );
}

#[test]
fn aliases_of_forward_classes_remain_conservative_until_the_definition_is_complete() {
    assert_successful_raise_retains_handlers(
        r#"
unit U;
interface
implementation
type
  E = class;
  AliasE = E;
  E = class constructor Create; end;
procedure P;
begin
  try
    raise E.Create;
  except
    on AliasE do Correct;
    on E do Wrong;
  end;
end;
end.
"#,
        "P",
        "raise E.Create",
        &["Correct"],
    );
}

#[test]
fn preprocessor_directives_make_conditional_aliases_conservative() {
    assert_successful_raise_reaches(
        r#"
unit ConditionalAlias;
interface
implementation
type
  A = class constructor Create; end;
  B = class constructor Create; end;
{$IFDEF CHOOSE_A}
  E = A;
{$ELSE}
  E = B;
{$ENDIF}
procedure P;
begin
  try
    raise E.Create;
  except
    on A do First;
    on B do Second;
  end;
end;
end.
"#,
        "P",
        "raise E.Create",
        "First",
    );
}

#[test]
fn modern_preprocessor_directives_keep_successful_constructor_raises_unknown() {
    let source = r#"
unit ModernPreprocessorBarrier;
interface
implementation
{$IFDEF MODERN}
{$ENDIF}
type
  E = class constructor Create; end;
procedure P;
begin
  try
    raise E.Create;
  except
    on E do Handle;
  end;
end;
end.
"#;
    let tree = parse_clean(source);
    let cfgs = build_file_cfgs(&tree, source.as_bytes());
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, source, "raise", "raise E.Create");
    let successful_raise = successful_raise_block(cfg, raise);
    let handler = block_with_stmt(cfg, source, "statement", "Handle");

    let successful_successors = successors(cfg, successful_raise);
    assert!(successful_successors.contains(&(handler, EdgeKind::ExceptionThrow)));
    assert!(
        successful_successors.contains(&(cfg.exit, EdgeKind::ExceptionThrow)),
        "a modern preprocessor directive is a file-wide configuration barrier, including on the successful constructor path"
    );
}

#[test]
fn modern_preprocessor_blocks_make_conditional_exception_classes_unknown() {
    let source = r#"
unit ConditionalTypeBlock;
interface
{$IFDEF MODERN}
type
  E = class constructor Create; end;
{$ENDIF}
implementation
procedure P;
begin
  try
    raise E.Create;
  except
    on E do Handle;
  end;
end;
end.
"#;
    let tree = parse_clean(source);
    let cfgs = build_file_cfgs(&tree, source.as_bytes());
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, source, "raise", "raise E.Create");
    let successful_raise = successful_raise_block(cfg, raise);
    let handler = block_with_stmt(cfg, source, "statement", "Handle");

    let successful_successors = successors(cfg, successful_raise);
    assert!(successful_successors.contains(&(handler, EdgeKind::ExceptionThrow)));
    assert!(successful_successors.contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn comment_style_preprocessor_directives_keep_raw_and_project_dispatch_unknown() {
    let source = r#"
unit CommentStylePreprocessorBarrier;
interface
(*$IFDEF MODERN*)
type
  E = class constructor Create; end;
(*$ENDIF*)
implementation
procedure P;
begin
  try
    raise E.Create;
  except
    on E do Handle;
  end;
end;
end.
"#;
    let tree = parse_clean(source);
    let assert_unknown = |cfgs: &[Cfg]| {
        let cfg = cfg_for(cfgs, "P");
        let raise = block_with_stmt(cfg, source, "raise", "raise E.Create");
        let successful_raise = successful_raise_block(cfg, raise);
        let handler = block_with_stmt(cfg, source, "statement", "Handle");
        let successful_successors = successors(cfg, successful_raise);
        assert!(successful_successors.contains(&(handler, EdgeKind::ExceptionThrow)));
        assert!(
            successful_successors.contains(&(cfg.exit, EdgeKind::ExceptionThrow)),
            "comment-style preprocessor directives must remain a file-wide barrier"
        );
    };

    let raw_cfgs = build_file_cfgs(&tree, source.as_bytes());
    assert_unknown(&raw_cfgs);

    let unit = ProjectUnitInput::new(
        ProjectUnitId::new("comment-style"),
        ProjectSourceId::new("comment-style.pas"),
        tree,
        source.as_bytes(),
    );
    let snapshot = ProjectSnapshot::new(vec![unit], Vec::new()).unwrap();
    let project_cfgs =
        build_file_cfgs_in_project(&snapshot, &ProjectUnitId::new("comment-style")).unwrap();
    assert_unknown(&project_cfgs);
}

#[test]
fn lambda_local_type_does_not_shadow_the_enclosing_routine_after_the_lambda() {
    let source = r#"
unit LambdaScope;
interface
implementation
type
  E = class constructor Create; end;
  Other = class constructor Create; end;
procedure P;
begin
  Apply(function: Integer
    type
      E = Other;
    begin
      Result := 1;
    end);
  try
    raise E.Create;
  except
    on Other do WrongLambda;
    on E do CorrectOuter;
  end;
end;
end.
"#;
    let tree = parse_clean(source);
    let cfgs = build_file_cfgs(&tree, source.as_bytes());
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, source, "raise", "raise E.Create");
    let successful_raise = successful_raise_block(cfg, raise);
    let outer = block_with_stmt(cfg, source, "statement", "CorrectOuter");
    let lambda = block_with_stmt(cfg, source, "statement", "WrongLambda");

    assert_eq!(
        successors(cfg, successful_raise),
        vec![(outer, EdgeKind::ExceptionThrow)],
        "lambda-local declarations must not leak into the enclosing routine scope"
    );
    assert!(!successors(cfg, successful_raise).contains(&(lambda, EdgeKind::ExceptionThrow)));
}

#[test]
fn inline_for_variable_does_not_shadow_an_outer_type_after_the_loop() {
    let source = r#"
unit InlineForScope;
interface
implementation
type
  E = class constructor Create; end;
  Other = class constructor Create; end;
procedure P;
begin
  for var E := 0 to 1 do
    Work;
  try
    raise E.Create;
  except
    on Other do WrongLoop;
    on E do CorrectOuter;
  end;
end;
end.
"#;
    let tree = parse_clean(source);
    let cfgs = build_file_cfgs(&tree, source.as_bytes());
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, source, "raise", "raise E.Create");
    let successful_raise = successful_raise_block(cfg, raise);
    let outer = block_with_stmt(cfg, source, "statement", "CorrectOuter");
    let loop_handler = block_with_stmt(cfg, source, "statement", "WrongLoop");

    assert_eq!(
        successors(cfg, successful_raise),
        vec![(outer, EdgeKind::ExceptionThrow)],
        "the inline for variable must not escape its loop scope"
    );
    assert!(!successors(cfg, successful_raise).contains(&(loop_handler, EdgeKind::ExceptionThrow)));
}

#[test]
fn implicit_function_values_shadow_same_named_global_types() {
    assert_successful_raise_reaches(
        r#"
unit ImplicitResult;
interface
implementation
type
  Result = class constructor Create; end;
  Child = class(Result) constructor Create; end;
  RootResult = Result;
  Meta = class of RootResult;
function P: Meta;
begin
  Result := Child;
  try
    raise Result.Create;
  except
    on Child do First;
    on RootResult do Second;
  end;
end;
end.
"#,
        "P",
        "raise Result.Create",
        "First",
    );
}

#[test]
fn unresolved_generic_method_owners_block_global_fallbacks() {
    assert_successful_raise_reaches(
        r#"
unit GenericOwner;
interface
implementation
type
  E = class constructor Create; end;
  Other = class(E) constructor Create; end;
  Meta = class of E;
  Holder<T> = class E: Meta; procedure P; end;
procedure Holder<T>.P;
begin
  E := Other;
  try
    raise E.Create;
  except
    on Other do First;
    on U.E do Second;
  end;
end;
end.
"#,
        "P",
        "raise E.Create",
        "First",
    );
}

#[test]
fn unresolved_generic_owner_does_not_assume_a_module_qualified_head() {
    let source = r#"
unit U;
interface
implementation
type
  E = class constructor Create; end;
  L = class(E) constructor Create; end;
  Meta = class of E;
  Fields = class
    E: Meta;
  end;
  Holder<T> = class
    U: Fields;
  end;
procedure Holder<T>.P;
begin
  U.E := L;
  try
    raise U.E.Create;
  except
    on U.L do First;
    on U.E do Second;
  end;
end;
end.
"#;
    let tree = parse_clean(source);
    let cfgs = build_file_cfgs(&tree, source.as_bytes());
    let cfg = cfg_for(&cfgs, "P");
    let raise = block_with_stmt(cfg, source, "raise", "raise U.E.Create");
    let successful_raise = successful_raise_block(cfg, raise);
    let first = block_with_stmt(cfg, source, "statement", "First");
    let second = block_with_stmt(cfg, source, "statement", "Second");

    let successful_successors = successors(cfg, successful_raise);
    assert!(successful_successors.contains(&(first, EdgeKind::ExceptionThrow)));
    assert!(successful_successors.contains(&(second, EdgeKind::ExceptionThrow)));
    assert!(successful_successors.contains(&(cfg.exit, EdgeKind::ExceptionThrow)));
}

#[test]
fn conditional_handler_reraises_keep_all_outer_handler_alternatives() {
    let source = r#"
unit ConditionalReraise;
interface
implementation
type
  A = class constructor Create; end;
  B = class constructor Create; end;
{$IFDEF CHOOSE_A}
  E = A;
{$ELSE}
  E = B;
{$ENDIF}
procedure P;
begin
  try
    try
      raise UnknownError;
    except
      on E do raise;
  end;
  except
    on A do First;
    on B do Second;
  end;
end;
end.
"#;
    let tree = parse_clean(source);
    let cfgs = build_file_cfgs(&tree, source.as_bytes());
    let cfg = cfg_for(&cfgs, "P");
    let reraise = block_with_stmt(cfg, source, "raise", "raise;");
    let first = block_with_stmt(cfg, source, "statement", "First");

    assert!(
        successors(cfg, reraise).contains(&(first, EdgeKind::ExceptionThrow)),
        "a conditional typed handler must not make a bare re-raise look like only B"
    );
}
