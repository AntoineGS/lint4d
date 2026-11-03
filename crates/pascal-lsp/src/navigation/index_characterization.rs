//! Characterization of the per-document navigation index.
//!
//! `render` prints everything `Document::parse_with_session` derives from the
//! tree in a deterministic order, so changes to how the index is computed can
//! be checked against snapshots recorded before the change.  Set
//! `L4D_BLESS_INDEX_SNAPSHOTS=1` to rewrite the snapshots after an intended
//! change in output.

use super::*;
use std::fmt::{Debug, Write as _};
use std::path::Path;
use std::time::Instant;

const BLESS_VARIABLE: &str = "L4D_BLESS_INDEX_SNAPSHOTS";

fn sorted<T: Debug>(items: impl IntoIterator<Item = T>) -> Vec<String> {
    let mut rendered: Vec<String> = items.into_iter().map(|item| format!("{item:?}")).collect();
    rendered.sort();
    rendered
}

fn sorted_set_map<K: Debug, V: Debug>(map: &HashMap<K, HashSet<V>>) -> Vec<(String, Vec<String>)> {
    let mut rendered: Vec<_> = map
        .iter()
        .map(|(key, values)| (format!("{key:?}"), sorted(values)))
        .collect();
    rendered.sort();
    rendered
}

fn section(output: &mut String, name: &str, value: impl Debug) {
    writeln!(output, "## {name}\n{value:#?}").expect("write snapshot");
}

fn lines<T: Debug>(output: &mut String, name: &str, items: impl IntoIterator<Item = T>) {
    writeln!(output, "## {name}").expect("write snapshot");
    for (index, item) in items.into_iter().enumerate() {
        writeln!(output, "{index}: {item:?}").expect("write snapshot");
    }
}

pub(super) fn render(document: &ParsedDocument) -> String {
    let mut output = String::new();
    section(&mut output, "unit_name", &document.unit_name);
    section(
        &mut output,
        "unit_display_name",
        &document.unit_display_name,
    );
    section(&mut output, "interface_range", document.interface_range);
    section(
        &mut output,
        "implementation_range",
        document.implementation_range,
    );
    section(&mut output, "verified_header", document.verified_header);
    section(
        &mut output,
        "has_initialization",
        document.has_initialization,
    );
    section(&mut output, "has_finalization", document.has_finalization);
    lines(
        &mut output,
        "parser_recovery_spans",
        &document.parser_recovery_spans,
    );
    lines(&mut output, "uses_clauses", &document.uses_clauses);
    lines(&mut output, "interface_uses", &document.interface_uses);
    lines(
        &mut output,
        "implementation_uses",
        &document.implementation_uses,
    );
    lines(&mut output, "imports", &document.imports);
    lines(
        &mut output,
        "unknown_imports",
        sorted(&document.unknown_imports),
    );
    lines(
        &mut output,
        "interface_routine_keys",
        sorted(&document.interface_routine_keys),
    );
    lines(&mut output, "scopes", &document.scopes);
    lines(&mut output, "with_contexts", &document.with_contexts);
    lines(
        &mut output,
        "cache_unsafe_scopes",
        sorted(&document.cache_unsafe_scopes),
    );
    lines(
        &mut output,
        "owner_type_contexts",
        &document.owner_type_contexts,
    );
    lines(&mut output, "symbols", &document.symbols);
    lines(&mut output, "opaque_ranges", &document.opaque_ranges);
    lines(
        &mut output,
        "conditional_unknown_symbols",
        &document.conditional_unknown_symbols,
    );
    lines(
        &mut output,
        "unknown_class_owners",
        sorted(&document.unknown_class_owners),
    );
    lines(
        &mut output,
        "known_non_class_owners",
        sorted(&document.known_non_class_owners),
    );
    lines(
        &mut output,
        "symbol_indices_by_scope_key",
        sorted(&document.symbol_indices_by_scope_key),
    );
    lines(
        &mut output,
        "scope_symbol_indices",
        sorted(&document.scope_symbol_indices),
    );
    lines(
        &mut output,
        "member_symbol_indices",
        sorted(&document.member_symbol_indices),
    );
    lines(
        &mut output,
        "member_binding_keys_by_owner",
        sorted_set_map(&document.member_binding_keys_by_owner),
    );
    lines(
        &mut output,
        "scope_binding_keys",
        document.scope_binding_keys.iter().map(sorted),
    );
    lines(
        &mut output,
        "member_symbol_indices_by_owner",
        sorted(&document.member_symbol_indices_by_owner),
    );
    lines(
        &mut output,
        "type_ancestry",
        sorted(&document.type_ancestry),
    );
    lines(
        &mut output,
        "type_symbol_indices",
        sorted(&document.type_symbol_indices),
    );
    lines(
        &mut output,
        "direct_symbol_indices",
        sorted(&document.direct_symbol_indices),
    );
    lines(
        &mut output,
        "routine_symbol_indices",
        sorted(&document.routine_symbol_indices),
    );
    lines(
        &mut output,
        "routine_symbol_indices_by_body_scope",
        sorted(&document.routine_symbol_indices_by_body_scope),
    );
    lines(
        &mut output,
        "exported_symbol_indices",
        &document.exported_symbol_indices,
    );
    lines(
        &mut output,
        "interface_member_routine_keys",
        sorted_set_map(&document.interface_member_routine_keys),
    );
    lines(
        &mut output,
        "method_resolutions",
        &document.method_resolutions,
    );
    lines(
        &mut output,
        "interface_delegations",
        &document.interface_delegations,
    );
    lines(
        &mut output,
        "generic_parameter_contexts",
        document
            .generic_parameter_contexts
            .iter()
            .map(|context| (context.span, sorted(&context.names), context.parent)),
    );
    lines(&mut output, "helpers", &document.helpers);
    lines(&mut output, "documentation", &document.documentation);
    output
}

pub(super) fn parse_source(path: &str, source: String) -> Document {
    parse_source_in(path, source, &ConditionalContext::default())
}

fn parse_source_in(path: &str, source: String, context: &ConditionalContext) -> Document {
    let uri = Url::parse(&format!("file:///characterization/{path}")).expect("fixture URI");
    Document::parse_with_cancel(uri, source, context, &AtomicBool::new(false), None)
        .expect("fixture parses")
}

fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn read_source(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_else(|error| panic!("read {path:?}: {error}"));
    pascal_core::text::decode_bytes(&bytes).into_owned()
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn collect_fixture_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("fixture directory entry").path();
        if path.is_dir() {
            collect_fixture_files(&path, files);
        } else if path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            ["pas", "dpr", "dpk", "inc", "pp", "lpr"]
                .iter()
                .any(|candidate| e.eq_ignore_ascii_case(candidate))
        }) {
            files.push(path);
        }
    }
}

fn check_snapshot(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/navigation/testdata")
        .join(name);
    if std::env::var_os(BLESS_VARIABLE).is_some() {
        std::fs::create_dir_all(path.parent().expect("snapshot directory"))
            .expect("create snapshot directory");
        std::fs::write(&path, actual).expect("write snapshot");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("read snapshot {path:?}: {error}; bless with {BLESS_VARIABLE}=1")
    });
    if expected != actual {
        let first_difference = expected
            .lines()
            .zip(actual.lines())
            .enumerate()
            .find(|(_, (expected, actual))| expected != actual)
            .map(|(line, (expected, actual))| {
                format!(
                    "line {}:\n  expected: {expected}\n  actual:   {actual}",
                    line + 1
                )
            })
            .unwrap_or_else(|| "length differs".to_owned());
        panic!("index snapshot {name} changed; {first_difference}");
    }
}

/// Covers the constructs whose owner, visibility, scope and region the
/// index derives from the declaration's ancestors.
pub(super) const REPRESENTATIVE_UNIT: &str = r#"unit Characterized.Sample;

interface

uses
  System.SysUtils, Classes;

const
  CMax = 10;

type
  TColor = (clRed, clGreen, clBlue);
  TPoint = record
    X, Y: Integer;
    procedure Offset(DX, DY: Integer);
    class function Origin: TPoint; static;
  case Kind: Integer of
    0: (A: Byte);
    1: (B: Word);
  end;

  IShape = interface(IInterface)
    ['{6B2D1A00-0000-0000-0000-000000000001}']
    function Area: Double;
    property Name: string read GetName;
  end;

  TContainer<T: class> = class(TInterfacedObject, IShape)
  public type
    TNode = class
    private
      FValue: T;
    public
      Next: TNode;
      function Value: T;
    end;
    TNodeKind = (nkLeaf, nkBranch);
  strict private
    FCount: Integer;
    class var FInstances: Integer;
    const CDefault = 3;
  private
    FItems: array of T;
    function GetName: string;
  strict protected
    procedure Grow; virtual;
  protected
    procedure Changed; dynamic;
  public
    constructor Create(ACapacity: Integer = 4);
    destructor Destroy; override;
    function Area: Double;
    function Find<K>(const AKey: K): T; overload;
    class procedure Reset; static;
    property Count: Integer read FCount write FCount;
    property Items[Index: Integer]: T read GetItem; default;
  published
    property Name: string read GetName;
  end;

  TSimple = class
    FDefault: Integer;
    procedure Run;
  end;

  THelper = class helper for TSimple
    procedure Help;
  end;

  TProc = reference to procedure(const AValue: Integer);
  TForward = class;

var
  GCounter: Integer;
  GPoint: TPoint;

procedure Global(var AValue: Integer; out AResult: string);
function Twice<T>(const AValue: T): T;

implementation

type
  TPrivate = class(TSimple)
  private
    FHidden: Integer;
  end;

  TForward = class
  end;

var
  GHidden: string;

threadvar
  GThreadLocal: Integer;

resourcestring
  SMessage = 'message';

procedure TPoint.Offset(DX, DY: Integer);
begin
  X := X + DX;
  Y := Y + DY;
end;

class function TPoint.Origin: TPoint;
begin
  Result.X := 0;
end;

constructor TContainer<T>.Create(ACapacity: Integer);
var
  LIndex: Integer;
begin
  inherited Create;
  for LIndex := 0 to ACapacity - 1 do
    Grow;
end;

destructor TContainer<T>.Destroy;
begin
  inherited;
end;

function TContainer<T>.Area: Double;
begin
  Result := FCount;
end;

function TContainer<T>.Find<K>(const AKey: K): T;
begin
  Result := Default(T);
end;

class procedure TContainer<T>.Reset;
begin
end;

function TContainer<T>.GetName: string;
begin
  Result := 'container';
end;

procedure TContainer<T>.Grow;
begin
  SetLength(FItems, Length(FItems) + 1);
end;

procedure TContainer<T>.Changed;
begin
end;

function TContainer<T>.TNode.Value: T;
begin
  Result := FValue;
end;

procedure TSimple.Run;
begin
end;

procedure THelper.Help;
begin
  with Self do
    Run;
end;

procedure Global(var AValue: Integer; out AResult: string);
const
  CLocal = 2;
type
  TLocal = record
    Field: Integer;
  end;
var
  LLocal: TLocal;
  LCount: Integer = 1;

  procedure Nested(ADepth: Integer);
  var
    LNested: Integer;

    function Deeper: Integer;
    label
      Done;
    begin
      goto Done;
    Done:
      Result := LNested + ADepth;
    end;

  begin
    LNested := Deeper;
  end;

begin
  for var LInline := 0 to CLocal do
    Nested(LInline);
  var LAssigned := 3;
  LLocal.Field := LAssigned + LCount;
  AValue := AValue + LLocal.Field;
  AResult := SMessage;
end;

function Twice<T>(const AValue: T): T;
begin
  Result := AValue;
end;

procedure Abbreviated;
begin
end;

initialization
  GCounter := 0;

finalization
  GCounter := -1;

end.
"#;

pub(super) const REPRESENTATIVE_PROGRAM: &str = r#"program Characterized;

{$APPTYPE CONSOLE}

uses
  SysUtils,
  Characterized.Sample in 'Characterized.Sample.pas';

type
  TLocalRecord = packed record
    Value: Integer;
  end;

var
  GValue: TLocalRecord;

procedure Run(const AText: string);

  function Inner(AValue: Integer): Integer;
  begin
    Result := AValue * 2;
  end;

begin
  Writeln(AText, Inner(GValue.Value));
end;

begin
  Run('hello');
end.
"#;

#[test]
fn representative_unit_index_matches_snapshot() {
    let document = parse_source("Characterized.Sample.pas", REPRESENTATIVE_UNIT.to_owned());
    check_snapshot("representative_unit.index.txt", &render(&document.parsed));
}

#[test]
fn representative_program_index_matches_snapshot() {
    let document = parse_source("Characterized.dpr", REPRESENTATIVE_PROGRAM.to_owned());
    check_snapshot(
        "representative_program.index.txt",
        &render(&document.parsed),
    );
}

/// Every Pascal fixture in the workspace, reduced to a digest of its index.
#[test]
fn workspace_fixture_indexes_match_digests() {
    let root = workspace_root();
    let mut files = Vec::new();
    for directory in ["crates", "tests"] {
        collect_fixture_files(&root.join(directory), &mut files);
    }
    files.retain(|path| {
        !path.components().any(|component| {
            component.as_os_str() == "target" || component.as_os_str() == "node_modules"
        })
    });
    files.sort();
    assert!(
        files.len() > 100,
        "expected the workspace fixtures, found {}",
        files.len()
    );
    let mut digests = String::new();
    for path in files {
        let relative = path
            .strip_prefix(&root)
            .expect("fixture under workspace")
            .to_string_lossy()
            .replace('\\', "/");
        let source = read_source(&path);
        let document = parse_source(&relative, source.clone());
        let rendered = render(&document.parsed);
        assert_eq!(
            rendered,
            render(&parse_source(&relative, source).parsed),
            "index rendering of {relative} is not deterministic"
        );
        writeln!(
            digests,
            "{relative} {:016x} {}",
            fnv1a(&rendered),
            document.parsed.symbols.len()
        )
        .expect("write digest");
    }
    check_snapshot("workspace_fixtures.digests.txt", &digests);
}

/// Index external files and report digest, symbol count and elapsed time.
///
/// `L4D_INDEX_PROBE_FILES` lists the files, separated by the platform path
/// separator, and `L4D_INDEX_PROBE_DEFINES` optionally lists comma-separated
/// defines of a closed define set.  Run with `--release --ignored
/// --nocapture` to compare index output and timing across changes on real
/// sources.
#[test]
#[ignore = "probe for external sources named by L4D_INDEX_PROBE_FILES"]
fn index_probe() {
    let Some(files) = std::env::var_os("L4D_INDEX_PROBE_FILES") else {
        return;
    };
    let context = std::env::var("L4D_INDEX_PROBE_DEFINES").map_or_else(
        |_| ConditionalContext::default(),
        |defines| {
            let defines: Vec<String> = defines.split(',').map(str::to_owned).collect();
            ConditionalContext::from_defines(&defines)
                .with_absent_define(pascal_project::ConditionalFact::False)
        },
    );
    for path in std::env::split_paths(&files) {
        let source = read_source(&path);
        let started = Instant::now();
        let document = parse_source_in(&path.to_string_lossy(), source, &context);
        let elapsed = started.elapsed();
        let rendered = render(&document.parsed);
        println!(
            "{} {:016x} symbols={} elapsed_ms={}",
            path.display(),
            fnv1a(&rendered),
            document.parsed.symbols.len(),
            elapsed.as_millis()
        );
    }
}

/// A unit whose sections hold `count` declarations of every indexed kind
/// side by side, so the declarations have many earlier siblings.
fn flat_unit(count: usize) -> String {
    let mut source = String::from("unit Flat;\n\ninterface\n\nuses\n");
    for index in 0..count {
        writeln!(source, "  Used{index},").expect("write fixture");
    }
    source.push_str("  System;\n\nconst\n");
    for index in 0..count {
        writeln!(source, "  C{index} = {index};").expect("write fixture");
    }
    source.push_str("\ntype\n  TEnum = (");
    for index in 0..count {
        write!(source, "e{index}, ").expect("write fixture");
    }
    source.push_str("eLast);\n  TFlat = class\n  private\n");
    for index in 0..count {
        writeln!(source, "    F{index}: Integer;").expect("write fixture");
    }
    source.push_str("  public\n");
    for index in 0..count {
        writeln!(
            source,
            "    procedure M{index}(A{index}: Integer);\n    property P{index}: Integer read F{index};"
        )
        .expect("write fixture");
    }
    source.push_str("  end;\n");
    for index in 0..count {
        writeln!(source, "  TAlias{index} = Integer;").expect("write fixture");
        writeln!(source, "  TGeneric{index}<T> = record Value: T; end;").expect("write fixture");
    }
    source.push_str("\nvar\n");
    for index in 0..count {
        writeln!(source, "  V{index}: Integer;").expect("write fixture");
    }
    for index in 0..count {
        writeln!(source, "procedure R{index}(const A: Integer);").expect("write fixture");
    }
    source.push_str("\nimplementation\n\n");
    for index in 0..count {
        writeln!(
            source,
            "procedure TFlat.M{index}(A{index}: Integer);\nvar\n  L: Integer;\nbegin\n  L := A{index};\nend;\n"
        )
        .expect("write fixture");
        writeln!(
            source,
            "procedure R{index};\nlabel Done;\nconst K = 1;\nbegin\nDone:\nend;\n"
        )
        .expect("write fixture");
    }
    source.push_str("initialization\nend.\n");
    source
}

fn flat_unit_parent_lookup_steps(count: usize) -> (u64, usize) {
    let source = flat_unit(count);
    let mut symbols = 0;
    let steps = test_parent_lookup_steps(|| {
        let document = parse_source("Flat.pas", source);
        assert!(document.parsed.parser_recovery_spans.is_empty());
        symbols = document.parsed.symbols.len();
    });
    (steps, symbols)
}

/// Tree-sitter's `parent` rescans siblings from the root, so indexing that
/// asks for the ancestors of every declaration grows quadratically with the
/// declarations in one section (l4d-16x7). Doubling them must at most about
/// double the parent lookup work.
#[test]
fn indexing_parent_lookups_grow_linearly_with_flat_declarations() {
    let (small_steps, small_symbols) = flat_unit_parent_lookup_steps(100);
    let (large_steps, large_symbols) = flat_unit_parent_lookup_steps(200);
    assert!(
        large_symbols >= 2 * small_symbols - 10,
        "{small_symbols} vs {large_symbols}"
    );
    assert!(
        large_steps <= 2 * small_steps + 1_000,
        "parent lookups grew from {small_steps} to {large_steps} steps when the declarations doubled"
    );
}

/// The owner lookup the index used before it carried ancestors: the nearest
/// type declaration whose type contains the node decides, and a declaration
/// without a name identifier makes the node top-level.
fn reference_enclosing_type(node: Node<'_>, source: &str) -> Option<String> {
    let node_span = Span::from_node(node);
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "declType"
            && let Some(type_node) = parent.child_by_field_name("type")
            && Span::from_node(type_node).contains(node_span)
        {
            return declaration_name_identifiers(parent)
                .last()
                .map(|identifier| canonical_name(&node_text(*identifier, source)));
        }
        current = parent.parent();
    }
    None
}

/// Error-recovery trees (including type declarations that lost their name)
/// give every declaration the owner the previous lookup gave it, and top-level
/// types are classified the same way.
#[test]
fn recovered_trees_keep_the_owner_lookup() {
    let bodies = [
        "type\n  = class\n    FField: Integer;\n  end;\n  TAfter = class end;\n",
        "type\n  TA = class end;\n  = record\n    FField: Integer;\n  end;\n  TB = class end;\n",
        "type\n  TA = class\n  FField: Integer;\n  end;\n  : class\n    FField: Integer;\n  end;\n",
        "type\n  <T> = class\n    FField: Integer;\n  end;\n  TB = class end;\n",
        "type\n  TA = class\n    type = class end;\n    FField: Integer;\n  end;\n",
    ];
    let checked_total = std::cell::RefCell::new(0);
    for body in bodies {
        let source = format!("unit U;\ninterface\n{body}implementation\nend.\n");
        let document = parse_source("Recovered.pas", source.clone());
        let parsed = &document.parsed;
        collect_nodes_with_ancestors(parsed.tree.root_node(), &mut |node, ancestors| {
            if !matches!(
                node.kind(),
                "declType" | "declProc" | "declProp" | "declField"
            ) {
                return;
            }
            assert_eq!(
                enclosing_type_among(node, nearest_first(ancestors), &source),
                reference_enclosing_type(node, &source),
                "{} at {:?} in {body:?}",
                node.kind(),
                Span::from_node(node)
            );
            *checked_total.borrow_mut() += 1;
        });
    }
    assert!(*checked_total.borrow() > 8);
}
