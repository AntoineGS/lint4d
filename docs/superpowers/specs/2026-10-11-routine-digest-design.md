# l4d-7mw.1: per-routine control-flow digests for agents

- Date: 2026-10-11
- Status: written spec approved by the user on 2026-10-11
- Issue: l4d-7mw.1 (epic l4d-7mw)
- Blocked by: l4d-z6w, l4d-wzt, l4d-d8d

## 1. Intent and constraints

Coding agents working in large Delphi codebases spend most of their context
reading whole routines, and miss facts that live on exception paths or deep in
nested branches. `lint4d digest` gives an agent a compact, line-anchored
description of a routine (control structure, object lifecycle, transactions,
raises, exits, calls) so it can decide which source lines to read.

Constraints agreed with the user:

- The digest is a `lint4d` subcommand; agents call it from the shell. The MCP
  server (l4d-7mw.2) wraps it later.
- File-local only: no `.dproj`, no project resolution, no configuration file.
- Soundness over recall: the digest never claims "freed on all paths" unless
  the CFG proves it. A missing fact is acceptable; a wrong proof is not.
- CFG gaps are fixed in `cfg-pascal`/`cfg-core` first (AGENTS.md), not worked
  around in the digest.
- This spec changes no product code and does not authorize implementation
  before written-spec and implementation-plan approval.

## 2. Evidence from the spike

A throwaway probe (2026-10-09, `/tmp/cfg-probe`, not committed) built digests
from `cfg_pascal::build_file_cfgs` over the Multidev monorepo:

| Routine | Source | Facts only | Facts + skeleton |
| --- | ---: | ---: | ---: |
| `TBusinessObject.WriteToDB` (242 lines) | 9.9 KB | 1.0 KB | 3.7 KB |
| `TBOMap.LoadBySQL` | 10.0 KB | 1.3 KB | 3.1 KB |
| `TBusinessObject.getInstance` | 6.6 KB | 1.1 KB | 3.0 KB |
| `WriteTransfer` (435 lines) | 25.1 KB | 1.4 KB | 4.4 KB |
| `GetProductStock` (797 lines) | 39.7 KB | 2.5 KB | 8.9 KB |
| `UpdateCustomer` | 42.3 KB | 3.0 KB | 8.4 KB |
| `TBoConnection.TryBoDbOperation` | 2.8 KB | 0.6 KB | 1.6 KB |
| `TBoConnection.UnRegisterConnection` | 5.0 KB | 0.6 KB | 1.4 KB |

- 20,869 files, 437,358 routines, 0 panics, 177 s single-threaded; slowest
  file `SynCommons.pas` at 5 s.
- It found a real leak: `WebImportExportExecutePOS.pas` `WriteTransfer` creates
  `existingSkus` at L4399, then calls `aDatabase.Open` before the `try` at
  L4403; an exception there is swallowed by the outer `except` at L4773 and the
  list is never freed.

Spike lessons that shape this design: `finally` handlers are duplicated per
continuation; there are no exception edges outside `try` regions; bare call
statements are not classified; property reads look like parameterless calls; a
flat fact list misrepresents mutually exclusive `case` arms.

## 3. Decisions

| Question | Decision |
| --- | --- |
| Delivery surface | `lint4d digest` subcommand |
| Missing exception edges (l4d-d8d) | Fix the CFG first; l4d-d8d blocks this issue |
| Layout | Facts inline in a control-flow tree |
| Calls | Each distinct call once per straight-line run, with `×N` |
| No routine selected | Print a routine index |
| Architecture | Data model and renderers in `lint4d::digest`; only generic CFG fixes in `cfg-pascal`/`cfg-core` (approach A) |

Rejected: placing the model in `cfg-pascal` (adds an agent-facing format to a
crate l4d-60 is restructuring); attaching lint diagnostics to the tree (the
leak rules are AST-pattern based and could disagree with path verdicts; every
digest would pay for a lint run). The latter can be a later flag.

`pascal-lsp` already depends on the `lint4d` library, so the MCP server and
any LSP request can reuse `lint4d::digest` without a new crate.

## 4. Prerequisites in cfg-pascal / cfg-core

Each is its own issue, test-first.

- **l4d-z6w**: `classify_call` classifies a statement whose only named child is
  an identifier as `FunctionCall`, excluding `Exit`, `Break`, `Continue`.
- **l4d-wzt**: a public way to identify every copy of one `finally` handler
  (group id or `blocks_for_stmt`), and a must-pass query over a set of blocks.
- **l4d-d8d**: per-statement exception modelling. Every statement that may
  raise ends its basic block and has an `ExceptionThrow` edge to the nearest
  enclosing handler, or to Exit when none encloses it. Which statements may
  raise is decided in l4d-d8d. Its acceptance includes re-running the
  CFG-based rules (`use_after_free`, `nil_check`, `transaction`,
  `unchecked-nil`) and justifying every changed result.

## 5. Output format

Line numbers are absolute and 1-based (`L2417`). Indentation is nesting.
Conditions and `case` labels are truncated to about 50 characters with `…`; so is the text after `raise`.

### 5.1 Routine digest

```
TBusinessObject.WriteToDB  BusinessObject.pas:2333-2574 (242 lines)
objects: params TParams L2417 freed on all paths; fieldsMessages TStringList L2455 freed on all paths
frequent calls: LogToConsoleBOF ×9, RefreshDBStatus ×3
L2351 calls GetBofSync, aBofSync.BeginWrite
L2356 try
  L2358 if checkAndClipStringFieldsToMaxCapacityIfNecessary
    L2360 for i := 0 to Fields.Count - 1
      L2364 if (aBOField.isInitialized) and (not aBOField.isReadO…
        …
  L2417 new TParams -> params
  L2418 try
    L2419 case aBOUpdateKind
      boukUpdate: L2420 calls GenerateUpdateSQL
      boukInsert: L2421 calls GenerateInsertSQL
      boukDelete: L2422 calls GenerateDeleteSQL
      else: L2424 raise EBOException.Create('Unknown operation to perform …
    …
  L2504 finally
    L2505 free params
L2570 finally
  L2571 calls aBofSync.EndWrite
```

Fact lines:

- `new <Class> -> <target>`
- `free <var>` (`.Free`, `.Destroy`, `FreeAndNil`)
- `tx start|commit|rollback <receiver>.<method>`
- `raise <text>`; a bare `raise;` is `re-raise`
- `exit`, with `(runs finally L…)` when the exit passes through a `finally`
- `calls a, b ×N, …`: distinct calls in a straight-line run, first-use order
- `nested routine <Name>`, `anonymous method calls …`

Runs with no facts and no calls (assignments only) are omitted.

A call name found in three or more separate runs of one routine is removed
from those runs and listed once, with its total count, on a `frequent calls:`
line after `objects:` (before the body): `frequent calls: LogToConsoleBOF ×9,
GetBofSync ×3`. The count is always shown, names keep first-use order, runs
left empty disappear, and calls inside anonymous methods are not counted. The
line is omitted when nothing qualifies.

In the text output, a chain of single-child controls (`if`, `else`, `case`
arm, `for`, `while`, `with`, `on`) that ends in one fact renders on one line,
joined by ` → ` with every line number kept: `L463 if aDataset is TIBQuery →
L465 calls TIBQuery(aDataset).ExecSQL`, or `hoExecSQL: L463 if … → L465 calls
…` for an arm. A chain ending in a control with several children, or passing
through `try`, `case`, `repeat`, `{$IF}` or `asm`, is not folded, and a
control that `--max-lines` collapses renders its collapse line instead. JSON
and the model are not affected.

A control with no fact anywhere below it and no call in its own condition or
label is omitted, bottom-up (an `if` whose only child was such a `for` goes
too). Exceptions: an `if` stays when the `else` right after it is kept; an
empty `else` is dropped; empty `case` arms are dropped, and a `case` with no
arm left goes unless its selector holds a call; a `try` is dropped only when
its body and every `finally`, `except`, `on` and `else` handler are empty, and
a kept `try` keeps all its handlers, so an empty `except` that swallows
exceptions stays visible; likewise a kept `{$IF}` keeps all its branches, and
`asm` blocks are always kept. Routine-index markers count facts only, so this
does not change them.

The `objects:` line holds one verdict per local variable assigned from a
constructor:

- `freed on all paths`
- `NOT freed on all paths (escapes via L4400 exception, L4410 exit, …)`: up to
  three escape points ordered by line, then `…`; `re-created at L…` for a
  creation reached again before its free
- `not freed in this routine`: assigned to `Result`, a field, or never freed;
  a description, not a leak claim
- `unknown (<reason>)`

### 5.2 Routine index

```
WebImportExportExecutePOS.pas  412 routines
L4343-4777  435  WriteTransfer          tx raise5 exit4 !escape
L4779-4801   23  ReadTransferStatus
```

Markers: `tx` (transaction operations), `raiseN`, `exitN`, `!escape` (some
object is NOT freed on all paths), `?` (some verdict is unknown).

### 5.3 JSON

`--format json` serializes the data model of section 6 with lines and byte
ranges. Text is the default.

## 6. Data model and analysis

### 6.1 Model (`crates/lint4d/src/digest/model.rs`, serde)

```rust
FileDigest     { path, routines: Vec<RoutineDigest> }
RoutineDigest  { name, lines, bytes, objects: Vec<ObjectVerdict>, frequent_calls: Vec<(name, count)>, body: Vec<Node>, degraded: Vec<Degradation> }
Node           = Control { kind, line, label, children }   // if/else/case/arm/loop/try/finally/except/on/with/$IFDEF
               | Fact { line, fact }
Fact           = New { class, target } | Free { var } | Tx { op, receiver, method }
               | Raise { text } | ReRaise | Exit { runs_finally: Option<line> }
               | Calls(Vec<(name, count)>) | Nested { name } | Lambda { calls }
ObjectVerdict  { var, class, line, verdict }
Verdict        = FreedAllPaths | Escapes(Vec<Escape>) | NotFreedHere { why } | Unknown { why }
```

`frequent_calls` is skipped in JSON when empty. The index and the text and JSON renderers are pure functions of
`FileDigest`.

### 6.2 Tree (`digest/tree.rs`)

Structure comes from the syntax tree; one `RoutineDigest` per CFG returned by
`build_file_cfgs`, keyed by the CFG's routine node.

- Control constructs become `Control` nodes; `case` labels and `else` become
  `arm` children.
- Consecutive simple statements form a run; their calls merge into one
  `Calls` fact. Lifecycle, transaction, raise and exit facts are always their
  own lines.
- Facts use `cfg_pascal::calls::classify_call` and
  `lookup_transaction_method`. A bare dotted name counts as a call only in
  statement position (property reads are indistinguishable, see l4d-ftm).
- Calls inside a condition stay visible in the condition text; any cut off by
  truncation are listed after it.
- Nested routines become one `Nested` line and get their own index entry
  (`Outer.Inner`). Anonymous methods become `Lambda` with merged calls and no
  verdicts.

### 6.3 Verdicts (`digest/lifecycle.rs`)

For each local variable assigned from a constructor:

1. Free set F: every CFG block containing a free of the variable, including
   all copies of a `finally` handler (l4d-wzt).
2. Walk forward from the creating statement, never entering F. Reaching Exit
   yields `Escapes`, reporting the statement where each path leaves (source of
   an exception edge, an `exit`, or falling off the end). Reaching the creating
   statement again yields `Escapes (re-created at L…)`. The variable is
   non-nil after the creation, so the walk never takes the nil branch of an
   `if`/`while` condition that is exactly a nil test of it (`Assigned(L)`,
   `L <> nil`, `L = nil`, negated or parenthesized), and such a test of a
   local raises no exception (l4d-rqmx).
3. Any other assignment to the variable in the routine yields
   `Unknown (reassigned at L…)`, except `L := nil` where no path from the
   creation reaches it without a free: initialization before the `try`, or
   clearing after the free (l4d-rqmx).
4. Assignment to `Result` yields `NotFreedHere (returned as Result)`. A
   non-local target (field, global or parameter) with no free in the routine
   yields `NotFreedHere (assigned to a non-local)`; a local with no free
   yields `NotFreedHere (no free found)`. A non-local that is freed in the
   routine gets the normal path verdict.
5. Parse errors in the routine yield `Unknown (parse errors)` for every
   verdict; a hidden directive block (6.5) between a creation and its free
   yields `Unknown (hidden directive block at L…)`.
6. A creation, assignment or free of the variable inside a `with` body yields
   `Unknown (named inside with at L…)`: the name may denote a member of the
   `with` subject (l4d-pwvv). Such a free never joins F.

`Exit { runs_finally }` records the first `finally` on the exit's path.

### 6.4 Out of scope for v1

Transaction verdicts (listed only; the `transaction` rule covers them),
interprocedural effects (l4d-7mw.3), receiver resolution, directories and
`--project`.

`goto`, `Break` and `Continue` produce no fact line.

### 6.5 Degradation

Shown as `! ` lines after the routine header and as `degraded` in JSON:

- `! parse errors at L…`
- `! conditional code: N {$IF…} regions, all branches shown`
- `! hidden directive block at L…`: lint's directive rewrite blanked an
  `{$IF}…{$IFEND}` opaque block
- `! anonymous methods not analysed`

## 7. Command line

`main.rs` moves to clap subcommands. The existing `Cli` fields become a
flattened `LintArgs` with `args_conflicts_with_subcommands`, so every existing
invocation behaves as before. A bare path literally named `digest` is read as
the subcommand; `./digest` works and the README says so.

```
lint4d digest <FILE> [--routine NAME | --line N | --all] [--format text|json] [--max-lines N]
```

- No selector: routine index.
- `--routine NAME`: case-insensitive qualified-name match, falling back to the
  last name component; all matches (overloads, same method in two classes) are
  digested under their own headers.
- `--line N`: the innermost routine containing line N.
- `--all`: every routine.
- One file per call. `.lint4d.toml` is not read.

Parsing uses `pascal_core::parser::parse_file`, the same entry point as lint;
its directive rewrite preserves byte offsets, so line numbers match the file.

## 8. Errors and limits

- Exit codes: `0` success; `2` for unreadable file, invalid arguments, or a
  selector matching nothing. `1` is not used. A failed match prints the 10
  closest routine names to stderr.
- `--max-lines N` (default 300) bounds text per routine. Past it, the deepest
  subtrees collapse to `L4430-4441 while not aQry.Eof … (12 lines, calls aQry.Next, existingSkus.Add)`, keeping the head text.
  The header and `objects:` line are never collapsed. JSON, `--all` and the
  index are not bounded.
- No new budgets beyond lint's 256-level tree-depth guard.

## 9. Testing

1. Verdict unit tests in `digest/lifecycle.rs`, one fixture each: free in
   `finally` across all copies; `exit` inside `try`; a raising call between
   creation and `try` under a swallowing outer `except` (the `WriteTransfer`
   shape); creation, raising call and free in one straight line; creation in
   a loop freed after it; `FreeAndNil` and `.Destroy`; reassignment
   (`Unknown`); `Result :=` and field assignment (`NotFreedHere`); frees in
   separate `case` arms.
2. `insta` snapshot tests (text and JSON) over `tests/fixtures/digest/`: run
   merging, `case` arms and `else`, condition truncation, nested routines,
   anonymous methods, `{$IFDEF}`, parse-error, hidden-directive and
   `--max-lines` collapse output.
3. `assert_cmd` CLI tests: default index; `--routine` qualified, last
   component and overloads; `--line`; no match exits 2 with suggestions;
   `./digest` as a path; existing `cli_test.rs` unchanged and passing.
4. Invariant test: index markers agree with the full digest for every fixture.
5. `cargo fmt`, `cargo clippy` and workspace tests clean.

## 10. Agent skill

`skills/pascal-digest/SKILL.md` in this repository, linked or copied by the
user into `~/.config/opencode/skills/`:

- Before reading a Pascal routine longer than about 150 lines, run
  `lint4d digest <file> --line N` (or `--routine`).
- Read only the source lines the digest points to.
- Treat `Escapes` and `Unknown` verdicts as leads to verify, never as proven
  bugs.
- Facts are file-local; calls are names, not resolved effects.

## 11. Acceptance on Multidev (manual)

Recorded in the l4d-7mw.1 close reason; the corpus is private and not in CI.

- Size: the eight routines of section 2 are at least 3× smaller than their
  source with the tree view (above 100 lines).
- Robustness: `--all` over every first-party file in `Common`, `Projects` and
  `Branches` with no panics; elapsed time recorded.
- Known cases: `WriteTransfer` reports `existingSkus` as NOT freed on all
  paths, escaping through an exception at L4400 or L4402 (which of the two
  depends on l4d-d8d's may-raise rule);
  `WriteToDB` reports both objects freed on all paths.
