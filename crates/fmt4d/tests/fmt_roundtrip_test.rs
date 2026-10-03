use pascal_core::node_kind as K;
use std::path::PathBuf;

mod common;
use common::{format_source, idempotency_check};

/// Round-trip oracle: formatting may change layout but not what the code
/// says. Both texts must parse to trees with the same node kinds and code
/// child counts, the same text in every code leaf, and the same comments
/// and directives in the same order.
///
/// Two intentional transformations are allowed, and only these:
/// - Uses sorting (on by default) reorders units, and their comments move
///   with them, so with `uses_sorted` each `declUses` is compared as a
///   multiset of its leaf tokens and of the trivia inside it.
/// - A `var` declaration naming several identifiers (`A, B: T;`) is
///   expanded into one declaration per identifier, so a node with such a
///   child is compared by its leaf tokens with the expansion applied.
fn check_same_program(before: &str, after: &str, uses_sorted: bool) -> Result<(), String> {
    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let (tree_before, _) = pascal_core::parser::parse_file(&info, before.as_bytes())
        .map_err(|e| format!("parse original failed: {e}"))?;
    let (tree_after, _) = pascal_core::parser::parse_file(&info, after.as_bytes())
        .map_err(|e| format!("parse formatted failed: {e}"))?;
    let a = Side {
        root: tree_before.root_node(),
        source: before.as_bytes(),
    };
    let b = Side {
        root: tree_after.root_node(),
        source: after.as_bytes(),
    };
    compare_nodes(&a, a.root, &b, b.root, uses_sorted)?;

    let (trivia_a, uses_a) = a.trivia(uses_sorted);
    let (trivia_b, uses_b) = b.trivia(uses_sorted);
    if trivia_a != trivia_b {
        return Err(format!(
            "comments or directives changed:\n  before: {trivia_a:?}\n  after:  {trivia_b:?}"
        ));
    }
    if uses_a != uses_b {
        return Err(format!(
            "comments or directives in uses clauses changed:\n  before: {uses_a:?}\n  after:  {uses_b:?}"
        ));
    }
    Ok(())
}

struct Side<'t> {
    root: tree_sitter::Node<'t>,
    source: &'t [u8],
}

impl<'t> Side<'t> {
    fn text(&self, node: tree_sitter::Node) -> String {
        String::from_utf8_lossy(&self.source[node.byte_range()]).replace('\r', "")
    }

    /// Texts of all comments and directives in source order, except those
    /// inside a `declUses` (or after its `;` on the same line) when
    /// `uses_sorted`, which are returned sorted per clause instead.
    fn trivia(&self, uses_sorted: bool) -> (Vec<String>, Vec<Vec<String>>) {
        let mut uses_ranges = Vec::new();
        collect_kind(self.root, K::DECL_USES, &mut uses_ranges);
        let mut trivia = Vec::new();
        collect_trivia(self.root, &mut trivia);
        let mut ordered = Vec::new();
        let mut per_clause = vec![Vec::new(); uses_ranges.len()];
        for node in trivia {
            let clause = uses_ranges.iter().position(|r| {
                r.start_byte() <= node.start_byte()
                    && (node.end_byte() <= r.end_byte()
                        || node.start_position().row == r.end_position().row)
            });
            match clause {
                Some(i) if uses_sorted => per_clause[i].push(self.text(node)),
                _ => ordered.push(self.text(node)),
            }
        }
        for clause in &mut per_clause {
            clause.sort();
        }
        (ordered, per_clause)
    }
}

fn compare_nodes(
    a: &Side,
    x: tree_sitter::Node,
    b: &Side,
    y: tree_sitter::Node,
    uses_sorted: bool,
) -> Result<(), String> {
    let at = || {
        format!(
            "{} at {}:{} (before) / {}:{} (after)",
            x.kind(),
            x.start_position().row + 1,
            x.start_position().column + 1,
            y.start_position().row + 1,
            y.start_position().column + 1
        )
    };
    if x.kind() != y.kind() {
        return Err(format!(
            "node kind changed: {} -> {}, {}",
            x.kind(),
            y.kind(),
            at()
        ));
    }
    if uses_sorted && x.kind() == K::DECL_USES {
        let (mut ta, mut tb) = (Vec::new(), Vec::new());
        collect_leaf_tokens(a, x, &mut ta);
        collect_leaf_tokens(b, y, &mut tb);
        ta.sort();
        tb.sort();
        if ta != tb {
            return Err(format!("uses clause changed: {ta:?} -> {tb:?}, {}", at()));
        }
        return Ok(());
    }
    if code_children(x).iter().any(|c| is_var_list(*c)) {
        let (ta, tb) = (expanded_tokens(a, x), expanded_tokens(b, y));
        if ta != tb {
            return Err(format!("declarations changed: {ta:?} -> {tb:?}, {}", at()));
        }
        return Ok(());
    }
    // Literal nodes' children do not cover their whole text.
    let is_literal = matches!(x.kind(), K::LITERAL_STRING | K::LITERAL_CHAR);
    if (is_literal || code_children(x).is_empty()) && a.text(x) != b.text(y) {
        return Err(format!(
            "token text changed: {:?} -> {:?}, {}",
            a.text(x),
            b.text(y),
            at()
        ));
    }
    let (xc, yc) = (code_children(x), code_children(y));
    if xc.len() != yc.len() {
        return Err(format!(
            "child count changed: {} -> {}, {}",
            xc.len(),
            yc.len(),
            at()
        ));
    }
    for (cx, cy) in xc.into_iter().zip(yc) {
        compare_nodes(a, cx, b, cy, uses_sorted)?;
    }
    Ok(())
}

fn code_children(node: tree_sitter::Node) -> Vec<tree_sitter::Node> {
    node.children(&mut node.walk())
        .filter(|c| !c.is_extra())
        .collect()
}

fn collect_leaf_tokens(side: &Side, node: tree_sitter::Node, out: &mut Vec<(String, String)>) {
    let children = code_children(node);
    if children.is_empty() || matches!(node.kind(), K::LITERAL_STRING | K::LITERAL_CHAR) {
        out.push((node.kind().to_string(), side.text(node)));
        return;
    }
    for child in children {
        collect_leaf_tokens(side, child, out);
    }
}

/// A `declVar` naming several identifiers.
fn is_var_list(node: tree_sitter::Node) -> bool {
    node.kind() == K::DECL_VAR && code_children(node).iter().any(|c| c.kind() == K::COMMA)
}

/// Leaf tokens of `node`'s children, with each `A, B: T;` child written as
/// `A: T; B: T;`.
fn expanded_tokens(side: &Side, node: tree_sitter::Node) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for child in code_children(node) {
        if !is_var_list(child) {
            collect_leaf_tokens(side, child, &mut out);
            continue;
        }
        let parts = code_children(child);
        let colon = parts
            .iter()
            .position(|c| c.kind() == K::COLON)
            .unwrap_or(parts.len());
        let mut suffix = Vec::new();
        for part in &parts[colon..] {
            collect_leaf_tokens(side, *part, &mut suffix);
        }
        for ident in parts[..colon].iter().filter(|c| c.kind() != K::COMMA) {
            collect_leaf_tokens(side, *ident, &mut out);
            out.extend(suffix.iter().cloned());
        }
    }
    out
}

fn collect_kind<'t>(node: tree_sitter::Node<'t>, kind: &str, out: &mut Vec<tree_sitter::Node<'t>>) {
    if node.kind() == kind {
        out.push(node);
    }
    for child in node.children(&mut node.walk()) {
        collect_kind(child, kind, out);
    }
}

fn collect_trivia<'t>(node: tree_sitter::Node<'t>, out: &mut Vec<tree_sitter::Node<'t>>) {
    if node.is_extra() && matches!(node.kind(), K::COMMENT | K::PP_DIRECTIVE) {
        out.push(node);
        return;
    }
    for child in node.children(&mut node.walk()) {
        collect_trivia(child, out);
    }
}

fn roundtrip_check(source: &str) {
    let formatted = format_source(source);
    if let Err(e) = check_same_program(source, &formatted, true) {
        panic!("{e}\nOriginal:\n{source}\nFormatted:\n{formatted}");
    }
}

fn assert_same_program(before: &str, after: &str) {
    if let Err(e) = check_same_program(before, after, true) {
        panic!("{e}\nOriginal:\n{before}\nFormatted:\n{after}");
    }
}

// ── Oracle sanity ───────────────────────────────────────────────

const SANITY_SOURCE: &str = "\
unit Test;
interface
uses
  // first
  B, A; // tail
implementation
procedure P;
begin
  Count := Count + 1; // bump
  S := 'text';
end;
end.
";

#[test]
fn oracle_accepts_the_formatted_source() {
    roundtrip_check(SANITY_SOURCE);
}

#[test]
fn oracle_rejects_an_identifier_change() {
    let formatted = format_source(SANITY_SOURCE);
    let changed = formatted.replacen("Count := ", "Total := ", 1);
    assert_ne!(changed, formatted);
    let err = check_same_program(SANITY_SOURCE, &changed, true).expect_err("identifier change");
    assert!(err.contains("\"Count\" -> \"Total\""), "{err}");
}

#[test]
fn oracle_rejects_a_literal_change() {
    let formatted = format_source(SANITY_SOURCE);
    let changed = formatted.replace("'text'", "'test'");
    assert!(check_same_program(SANITY_SOURCE, &changed, true).is_err());
}

#[test]
fn oracle_rejects_a_dropped_comment() {
    let formatted = format_source(SANITY_SOURCE);
    let changed = formatted.replace(" // bump", "");
    assert_ne!(changed, formatted);
    assert!(check_same_program(SANITY_SOURCE, &changed, true).is_err());
}

#[test]
fn oracle_rejects_a_dropped_uses_comment() {
    let formatted = format_source(SANITY_SOURCE);
    let changed = formatted.replace("// first", "");
    assert_ne!(changed, formatted);
    assert!(check_same_program(SANITY_SOURCE, &changed, true).is_err());
}

#[test]
fn oracle_accepts_var_list_expansion_only() {
    let source = "unit T;\ninterface\nimplementation\nvar\n  A, B: Integer;\nend.\n";
    let expanded = source.replace("A, B: Integer;", "A: Integer;\n  B: Integer;");
    assert!(check_same_program(source, &expanded, true).is_ok());
    let renamed = source.replace("A, B: Integer;", "A: Integer;\n  C: Integer;");
    assert!(check_same_program(source, &renamed, true).is_err());
}

#[test]
fn oracle_compares_uses_order_unless_sorting() {
    let reordered = SANITY_SOURCE.replace("B, A;", "A, B;");
    assert!(check_same_program(SANITY_SOURCE, &reordered, true).is_ok());
    assert!(check_same_program(SANITY_SOURCE, &reordered, false).is_err());
}

// ── Fixture sweep ───────────────────────────────────────────────

/// Fixtures the oracle is known to reject because of formatter bugs that
/// are tracked separately, as (path relative to the workspace root, task
/// ID). Fix the bug, then remove the entry; the sweep fails if a listed
/// fixture starts passing.
const KNOWN_FAILING_FIXTURES: &[(&str, &str)] = &[
    // `SysUtils;` ending each branch becomes `SysUtils,` plus `{$ENDIF};`.
    (
        "crates/fmt4d/tests/fixtures/ppFragment/bucket_c_uses_semi.pas",
        "TASK-102",
    ),
];

fn fixture_files() -> Vec<PathBuf> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files: Vec<PathBuf> = ["tests/fixtures", "crates/fmt4d/tests/fixtures"]
        .iter()
        .flat_map(|dir| walkdir::WalkDir::new(workspace.join(dir)))
        .map(|entry| entry.expect("fixture directory is readable").into_path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pas"))
        })
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            path.strip_prefix(&workspace)
                .expect("fixture is under the workspace")
                .to_path_buf()
        })
        .collect()
}

fn check_fixture(path: &std::path::Path) -> Result<(), String> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = std::fs::read_to_string(workspace.join(path)).map_err(|e| e.to_string())?;
    let formatted = format_source(&source);
    check_same_program(&source, &formatted, true)?;
    let again = format_source(&formatted);
    if again != formatted {
        return Err("formatter is not idempotent".to_string());
    }
    Ok(())
}

#[test]
fn every_fixture_round_trips() {
    let files = fixture_files();
    assert!(files.len() >= 20, "fixtures not found: {files:?}");
    let mut unexpected = Vec::new();
    for path in &files {
        let name = path.to_string_lossy().replace('\\', "/");
        let known = KNOWN_FAILING_FIXTURES.iter().find(|(p, _)| *p == name);
        match (check_fixture(path), known) {
            (Ok(()), None) | (Err(_), Some(_)) => {}
            (Err(e), None) => unexpected.push(format!("{name}: {e}")),
            (Ok(()), Some((_, task))) => unexpected.push(format!(
                "{name} passes now; remove it from KNOWN_FAILING_FIXTURES ({task})"
            )),
        }
    }
    for (listed, task) in KNOWN_FAILING_FIXTURES {
        if !files
            .iter()
            .any(|p| p.to_string_lossy().replace('\\', "/") == *listed)
        {
            unexpected.push(format!("{listed} ({task}) is not a fixture"));
        }
    }
    assert!(unexpected.is_empty(), "{}", unexpected.join("\n\n"));
}

// ── Round-trip tests ────────────────────────────────────────────

#[test]
fn roundtrip_simple_unit() {
    let source = "unit Test;\ninterface\nimplementation\nend.\n";
    roundtrip_check(source);
}

#[test]
fn roundtrip_procedure_with_body() {
    let source = r#"unit Test;

interface

implementation

procedure DoSomething;
var
  x: Integer;
begin
  x := 1;
  if x > 0 then
  begin
    x := x + 1;
  end;
end;

end.
"#;
    roundtrip_check(source);
}

#[test]
fn roundtrip_class_declaration() {
    let source = r#"unit Test;

interface

type
  TMyClass = class
  private
    FName: string;
  public
    procedure Run;
  end;

implementation

end.
"#;
    roundtrip_check(source);
}

// ── Idempotency tests ───────────────────────────────────────────

#[test]
fn roundtrip_conditional_method_attributes() {
    let source = r#"unit Test;

interface

type
  TConditional = class
    procedure IfDefElse; {$IFDEF DELPHI_XE6_UP}reintroduce{$ELSE}override{$ENDIF};
    procedure IfDefNoElse; {$IFDEF DELPHI_XE6_UP}reintroduce{$ENDIF};
    procedure StandardThenConditional; virtual; {$IFDEF DELPHI_XE6_UP}reintroduce{$ELSE}override{$ENDIF};
    procedure ConditionalThenStandard; {$IFDEF DELPHI_XE6_UP}reintroduce{$ELSE}override{$ENDIF}; overload;
  end;

implementation

end.
"#;
    let formatted = format_source(source);

    for token in [
        "{$IFDEF DELPHI_XE6_UP}",
        "{$ELSE}",
        "{$ENDIF}",
        "reintroduce",
        "override",
        "virtual",
        "overload",
    ] {
        assert!(
            formatted.contains(token),
            "formatted source dropped {token:?}:\n{formatted}"
        );
    }

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let (_, diagnostics_before) =
        pascal_core::parser::parse_file(&info, source.as_bytes()).expect("parse original failed");
    let (_, diagnostics_after) = pascal_core::parser::parse_file(&info, formatted.as_bytes())
        .expect("parse formatted failed");
    assert!(
        diagnostics_before.is_empty(),
        "original conditional attributes produced diagnostics: {diagnostics_before:?}"
    );
    assert!(
        diagnostics_after.is_empty(),
        "formatted conditional attributes produced diagnostics: {diagnostics_after:?}"
    );
    assert_same_program(source, &formatted);
    assert_eq!(formatted, format_source(&formatted));
}

#[test]
fn roundtrip_conditional_routine_directives_keep_inner_semicolons() {
    let source = r#"unit Test;

interface

type
  T = class
    function M: Integer; {$IFDEF USE_INLINE} inline; {$ENDIF}
    procedure N; virtual; {$IFDEF FPC} assembler; nostackframe; {$ELSE} register; {$ENDIF} overload;
  end;

function F: Integer; {$IFDEF USE_INLINE}inline;{$ENDIF}

implementation

function T.M: Integer;
begin
  Result := 0;
end;

procedure T.N; {$IFDEF X} inline; {$ELSE} {$IFDEF Y} cdecl; {$ENDIF} {$ENDIF}
begin
end;

function F: Integer;
{$IFDEF USE_INLINE} inline; {$ENDIF}
begin
  Result := 0;
end;

end.
"#;
    let formatted = format_source(source);

    let mut search_from = 0;
    for (directive, attributes) in [
        ("{$IFDEF USE_INLINE}", "inline;"),
        ("{$IFDEF FPC}", "assembler; nostackframe;"),
        ("{$ELSE}", "register;"),
        ("{$IFDEF USE_INLINE}", "inline;"),
        ("{$IFDEF X}", "inline;"),
        ("{$IFDEF Y}", "cdecl;"),
        ("{$IFDEF USE_INLINE}", "inline;"),
    ] {
        let start = search_from
            + formatted[search_from..]
                .find(directive)
                .unwrap_or_else(|| panic!("formatted source dropped {directive:?}:\n{formatted}"))
            + directive.len();
        let end = start
            + formatted[start..]
                .find("{$")
                .expect("directive must be closed");
        let inside = formatted[start..end].split_whitespace().collect::<Vec<_>>();
        assert_eq!(
            inside.join(" "),
            attributes,
            "attributes after {directive:?} must stay inside the directive:\n{formatted}"
        );
        search_from = end;
    }
    assert!(
        !formatted.contains("{$ENDIF};"),
        "formatter must not add a semicolon after a directive:\n{formatted}"
    );

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let (_, diagnostics_before) =
        pascal_core::parser::parse_file(&info, source.as_bytes()).expect("parse original failed");
    let (_, diagnostics_after) = pascal_core::parser::parse_file(&info, formatted.as_bytes())
        .expect("parse formatted failed");
    assert!(
        diagnostics_before.is_empty(),
        "original conditional directives produced diagnostics: {diagnostics_before:?}"
    );
    assert!(
        diagnostics_after.is_empty(),
        "formatted conditional directives produced diagnostics: {diagnostics_after:?}"
    );
    assert_same_program(source, &formatted);
    assert_eq!(formatted, format_source(&formatted));
}

#[test]
fn roundtrip_conditional_method_attribute_trailing_comment() {
    let source = "unit Test;\ninterface\nprocedure P; {$IFDEF X}reintroduce // comment\n {$ELSE}override{$ENDIF};\nimplementation\nend.\n";
    let formatted = format_source(source);

    let mut search_from = 0;
    for directive in ["{$IFDEF X}", "{$ELSE}", "{$ENDIF}"] {
        let relative = formatted[search_from..]
            .find(directive)
            .unwrap_or_else(|| panic!("formatted source dropped {directive:?}:\n{formatted}"));
        search_from += relative + directive.len();
    }
    assert!(
        formatted.contains("reintroduce // comment\n"),
        "line comment must end before the next directive:\n{formatted}"
    );

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let (_, diagnostics_before) =
        pascal_core::parser::parse_file(&info, source.as_bytes()).expect("parse original failed");
    let (_, diagnostics_after) = pascal_core::parser::parse_file(&info, formatted.as_bytes())
        .expect("parse formatted failed");
    assert!(
        diagnostics_before.is_empty(),
        "original conditional attribute produced diagnostics: {diagnostics_before:?}"
    );
    assert!(
        diagnostics_after.is_empty(),
        "formatted conditional attribute produced diagnostics: {diagnostics_after:?}"
    );
    assert_same_program(source, &formatted);
    assert_eq!(formatted, format_source(&formatted));
}

#[test]
fn roundtrip_conditional_method_attribute_comment_is_three_pass_idempotent() {
    let source = r#"unit Test;

interface

procedure P; {$IFDEF X}reintroduce{$ELSE}override{$ENDIF}; // comment
overload;

implementation

end.
"#;
    let first = format_source(source);
    let second = format_source(&first);
    let third = format_source(&second);

    assert_eq!(
        first, second,
        "formatter changed the second pass:\n{second}"
    );
    assert_eq!(second, third, "formatter changed the third pass:\n{third}");
    assert!(
        first.contains("// comment\n"),
        "line comment must not gain trailing spaces:\n{first}"
    );

    let mut search_from = 0;
    for directive in ["{$IFDEF X}", "{$ELSE}", "{$ENDIF}"] {
        let relative = first[search_from..]
            .find(directive)
            .unwrap_or_else(|| panic!("formatted source dropped {directive:?}:\n{first}"));
        search_from += relative + directive.len();
    }

    let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
    let (_, diagnostics_before) =
        pascal_core::parser::parse_file(&info, source.as_bytes()).expect("parse original failed");
    let (_, diagnostics_after) =
        pascal_core::parser::parse_file(&info, first.as_bytes()).expect("parse formatted failed");
    assert!(
        diagnostics_before.is_empty(),
        "original conditional attribute produced diagnostics: {diagnostics_before:?}"
    );
    assert!(
        diagnostics_after.is_empty(),
        "formatted conditional attribute produced diagnostics: {diagnostics_after:?}"
    );
    assert_same_program(source, &first);
}

#[test]
fn idempotent_simple_unit() {
    let source = "unit Test;\ninterface\nimplementation\nend.\n";
    idempotency_check(source);
}

#[test]
fn idempotent_formatted_fixture() {
    let source = std::fs::read_to_string("../../tests/fixtures/format/indent/basic_expected.pas")
        .expect("failed to read fixture");
    idempotency_check(&source);
}

#[test]
fn idempotent_unformatted_source() {
    let source = r#"unit Test;
interface
type
TMyClass = class
private
FValue: Integer;
public
procedure DoSomething;
end;
implementation
procedure TMyClass.DoSomething;
var
x: Integer;
begin
x := 1;
if x > 0 then
begin
x := x + 1;
end;
end;
end.
"#;
    // Format once, then check idempotency
    let formatted = format_source(source);
    idempotency_check(&formatted);
}

// ── Alias keyword misparse ──────────────────────────────────────

#[test]
fn idempotent_var_named_alias() {
    let source = "\
unit Test;
interface
implementation
procedure DoSomething;
var
  I: Integer;
  JoinIdx: Integer;
  F: TFieldDef;
  LookupSql: RawUtf8;
  Alias: RawUtf8;
  FirstOwnerKey: RawUtf8;
  FirstLookupKey: RawUtf8;
  FirstResultField: RawUtf8;
begin
end;
end.
";
    idempotency_check(source);
}

#[test]
fn idempotent_var_named_alias_first_in_block() {
    let source = "\
unit Test;
interface
implementation
procedure Foo;
var
  Alias: Integer;
  X: Integer;
begin
end;
end.
";
    idempotency_check(source);
}

#[test]
fn idempotent_var_named_alias_lowercase() {
    let source = "\
unit Test;
interface
implementation
procedure Foo;
var
  X: Integer;
  alias: String;
  Y: Boolean;
begin
end;
end.
";
    idempotency_check(source);
}

#[test]
fn var_named_alias_not_merged_with_previous() {
    let source = "\
unit Test;
interface
implementation
procedure Foo;
var
  LookupSql: RawUtf8;
  Alias: RawUtf8;
begin
end;
end.
";
    let formatted = format_source(source);
    assert!(
        formatted.contains("LookupSql: RawUtf8;\n"),
        "LookupSql should end its own line. Got:\n{}",
        formatted
    );
    assert!(
        formatted.contains("Alias: RawUtf8;\n"),
        "Alias should be on its own line. Got:\n{}",
        formatted
    );
}
