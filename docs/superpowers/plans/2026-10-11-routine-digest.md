# Routine Digest Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `lint4d digest <FILE>`, a compact, line-anchored, per-routine control-flow digest for coding agents, after fixing the three CFG gaps it depends on.

**Architecture:** Generic CFG fixes land first in `cfg-pascal`/`cfg-core` (bare call statements, finally-copy grouping and path queries, per-statement exception edges). A new `lint4d::digest` module builds a serde data model (`FileDigest`): the control tree comes from the syntax tree, and object-lifecycle verdicts come from CFG path queries. Pure renderers produce text, JSON and a routine index. `main.rs` gains clap subcommands; plain `lint4d <paths>` is unchanged.

**Tech Stack:** Rust 2024 workspace (rust 1.99), tree-sitter 0.24, petgraph 0.7, clap 4 (derive), serde/serde_json, insta, assert_cmd, Beads (`bd`) for tracking.

**Spec:** `docs/superpowers/specs/2026-10-11-routine-digest-design.md` (approved 2026-10-11). Issues: l4d-z6w, l4d-wzt, l4d-d8d, l4d-7mw.1.

## Global Constraints

- Workspace lints: `warnings = "deny"`; CI runs `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`, `cargo test --workspace --no-fail-fast`.
- lint4d uses cfg-core only through the `cfg_pascal::cfg_core` re-export; never add cfg-core as a direct dependency of lint4d.
- CFG gaps are fixed in `crates/cfg-pascal` / `crates/cfg-core`, never worked around in the digest (AGENTS.md).
- Bug fixes are TDD: failing test first, minimal fix, refactor. No fix without a regression test.
- Soundness over recall: never report "freed on all paths" unless the CFG proves it.
- Digest exit codes: `0` success; `2` unreadable file, invalid arguments, or selector matching nothing. `1` is never used by `digest`.
- `--max-lines` default 300; conditions and case labels are truncated to 70 characters followed by `…`.
- The digest reads one file per call and never reads `.lint4d.toml`.
- Tracking: `bd update <id> --claim` before code, `bd close <id> --reason "..."` after verification. Name the issue in every commit message. Do not push. Do not edit `TODOS.md` or recreate `backlog/`.
- `/docs/` is git-ignored: add docs with `git add -f`.

## Review Focus

1. Latin-1 (Windows-1252) source bytes in conditions and labels: no panic, text decoded as Latin-1. Test: Task 5, `latin1_condition_is_decoded`.
2. CRLF line endings: line numbers identical to LF, no `\r` in output. Test: Task 5, `crlf_source_has_same_lines`.
3. Empty routines (`begin end`) and bodiless `forward` declarations: empty body renders only the header; forward declarations are absent from the index. Test: Task 5, `empty_and_forward_routines`.
4. Synthetic module sections (`Demo.<initialization>`, `<main>`): selectable with `--line` and `--routine`. Test: Task 8, `line_in_initialization_selects_section`.
5. `--line` pointing outside every routine (interface section): exit 2 with the nearest routines listed. Test: Task 8, `line_outside_routines_fails_with_suggestions`.

## Spec clarifications (applied to the spec in Task 4)

- §6.3 step 4: `Result :=` always yields `NotFreedHere (returned as Result)`. A non-local target (field, global or parameter) with no free in the routine yields `NotFreedHere (assigned to a non-local)`. A non-local target that *is* freed in the routine gets the normal path verdict, because "not freed in this routine" would be false.
- §8: a collapsed subtree line keeps its head text: `L4430-4441 while not aQry.Eof … (12 lines, calls aQry.Next, existingSkus.Add)`.
- `goto`, `Break` and `Continue` produce no fact line in v1.

## File Structure

| File | Responsibility |
| --- | --- |
| `crates/cfg-pascal/src/calls.rs` | `classify_call` handles parameterless call statements (Task 1) |
| `crates/cfg-core/src/query.rs`, `src/lib.rs` | `blocks_for_stmt`, `path_avoiding`, `must_pass_through_any`, `PathEdge` (Task 2) |
| `crates/cfg-pascal/src/pascal_builder.rs` | exception edges for every executable statement (Task 3) |
| `crates/cfg-pascal/docs/cfg-lsp-support.md` | documents finally copies and exception edges (Tasks 2, 3) |
| `crates/lint4d/src/digest/mod.rs` | `digest_source`, `LineIndex`, shared syntax helpers |
| `crates/lint4d/src/digest/model.rs` | serde data model, `Markers` |
| `crates/lint4d/src/digest/render.rs` | text, index and JSON renderers, `--max-lines` collapse |
| `crates/lint4d/src/digest/tree.rs` | syntax tree → `Vec<DigestNode>` |
| `crates/lint4d/src/digest/lifecycle.rs` | object verdicts from CFG paths |
| `crates/lint4d/src/digest/select.rs` | `--routine` / `--line` selection and suggestions |
| `crates/lint4d/src/main.rs` | clap subcommands, `run_digest` |
| `crates/lint4d/tests/digest_*_test.rs`, `tests/fixtures/digest/` | integration, CLI and snapshot tests |
| `skills/pascal-digest/SKILL.md`, `README.md` | agent skill and user docs |

Work in a worktree (`.worktrees/routine-digest`, branch `routine-digest`) created with superpowers:using-git-worktrees at execution time.

---

### Task 1: classify parameterless call statements (l4d-z6w)

**Files:**
- Modify: `crates/cfg-pascal/src/calls.rs` (`classify_call` at ~line 207, tests at ~419-441)

**Interfaces:**
- Produces: `classify_call(node, source)` returns `Some(CallKind::FunctionCall { name })` for a `statement` node whose only non-extra named child is an `identifier`, except `Exit`, `Break` and `Continue` (case-insensitive). Every other input behaves as before.

- [ ] **Step 1: Claim the issue**

```bash
bd update l4d-z6w --claim
bd update l4d-z6w --design "classify_call accepts statement nodes whose sole named child is an identifier (Touch;) as FunctionCall; Exit/Break/Continue excluded. Plan: docs/superpowers/plans/2026-10-11-routine-digest.md Task 1."
```

- [ ] **Step 2: Write the failing tests**

In `crates/cfg-pascal/src/calls.rs`, replace the whole `classifies_function_call` test (it currently asserts nothing) with:

```rust
    #[test]
    fn classifies_function_call() {
        let source = b"program T; begin DoWork; end.";
        let tree = parse(source);
        assert_eq!(
            first_classified(&tree, source),
            Some(CallKind::FunctionCall {
                name: "DoWork".to_string()
            }),
            "a parameterless call statement is a FunctionCall"
        );
    }

    #[test]
    fn parameterless_call_inside_if_is_classified() {
        let source = b"program T; begin if X then Touch; end.";
        let tree = parse(source);
        assert_eq!(
            first_classified(&tree, source),
            Some(CallKind::FunctionCall {
                name: "Touch".to_string()
            })
        );
    }

    #[test]
    fn exit_break_continue_statements_are_not_calls() {
        for word in ["Exit", "exit", "Break", "Continue"] {
            let source = format!("program T; begin {word}; end.");
            let tree = parse(source.as_bytes());
            assert_eq!(
                first_classified(&tree, source.as_bytes()),
                None,
                "{word} is a control transfer, not a call"
            );
        }
    }

    #[test]
    fn inherited_statement_is_not_a_function_call() {
        let source = b"program T; begin inherited; end.";
        let tree = parse(source);
        assert_eq!(first_classified(&tree, source), None);
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p cfg-pascal --lib calls::tests`
Expected: FAIL in `classifies_function_call` and `parameterless_call_inside_if_is_classified` (left: `None`). The other two pass.

- [ ] **Step 4: Implement**

In `classify_call`, add the `statement` arm, and extend the doc-comment list with
`/// - \`statement\` whose only child is an \`identifier\` (\`Touch;\`) → \`FunctionCall\`, except \`Exit\`, \`Break\`, \`Continue\``:

```rust
pub fn classify_call(node: Node, source: &[u8]) -> Option<CallKind> {
    match node.kind() {
        "exprCall" => classify_expr_call(node, source),
        "exprDot" => classify_bare_expr_dot(node, source),
        "statement" => classify_bare_identifier_statement(node, source),
        _ => None,
    }
}
```

Add after `classify_bare_expr_dot`:

```rust
/// Classify `Touch;`: a statement whose only child is an identifier is a
/// parameterless call. `Exit`, `Break` and `Continue` are control transfers.
fn classify_bare_identifier_statement(node: Node, source: &[u8]) -> Option<CallKind> {
    let mut cursor = node.walk();
    let mut children = node
        .named_children(&mut cursor)
        .filter(|child| !child.is_extra());
    let child = children.next()?;
    if children.next().is_some() || child.kind() != "identifier" {
        return None;
    }
    let name = node_text(child, source);
    if ["exit", "break", "continue"]
        .iter()
        .any(|word| name.eq_ignore_ascii_case(word))
    {
        return None;
    }
    Some(CallKind::FunctionCall { name })
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p cfg-pascal --lib calls::tests`
Expected: PASS.

- [ ] **Step 6: Run the dependent suites**

Run: `cargo test -p cfg-pascal -p lint4d && cargo clippy -p cfg-pascal --all-targets -- -D warnings && cargo fmt --check`
Expected: all pass. Nothing outside `calls.rs` calls `classify_call` today, so no rule output can change.

- [ ] **Step 7: Commit and close**

```bash
git add crates/cfg-pascal/src/calls.rs
git commit -m "fix(cfg-pascal): classify parameterless call statements (l4d-z6w)"
bd update l4d-z6w --acceptance "- [x] Failing test for bare identifier call statements first
- [x] Bare identifier statements classified as FunctionCall; Exit/Break/Continue and goto targets excluded
- [x] Existing rule tests unchanged or updated with justification"
bd close l4d-z6w --reason "classify_call classifies statement nodes whose sole named child is an identifier as FunctionCall, excluding Exit/Break/Continue. Commit: <sha>. Verified: cargo test -p cfg-pascal -p lint4d, clippy, fmt."
```

### Task 2: statement-copy grouping and set-based path queries (l4d-wzt)

**Files:**
- Modify: `crates/cfg-core/src/query.rs` (new methods and tests), `crates/cfg-core/src/lib.rs` (export `PathEdge`)
- Create: `crates/cfg-pascal/tests/finally_copies_test.rs`
- Modify: `crates/cfg-pascal/docs/cfg-lsp-support.md`

**Interfaces:**
- Produces (on `impl Cfg`, all exported as `cfg_pascal::cfg_core::…`):
  - `pub struct PathEdge { pub from: BlockId, pub to: BlockId, pub kind: EdgeKind }` (`Debug, Clone, PartialEq, Eq`)
  - `pub fn blocks_for_stmt(&self, range: Range<usize>) -> Vec<BlockId>`: every block holding the *innermost* statement reference that contains `range`. All copies of a duplicated `finally` statement are returned, sorted by block index.
  - `pub fn path_avoiding(&self, from: BlockId, to: BlockId, avoid: &HashSet<BlockId>, skip_from: &[EdgeKind]) -> Option<Vec<PathEdge>>`: shortest path that starts with an outgoing edge of `from`, never enters a block in `avoid`, and ends at `to`. `from == to` finds a cycle. Edges *leaving `from`* whose kind is in `skip_from` are ignored everywhere on the path.
  - `pub fn must_pass_through_any(&self, from: BlockId, through: &HashSet<BlockId>) -> bool`

- [ ] **Step 1: Claim the issue**

```bash
bd update l4d-wzt --claim
bd update l4d-wzt --design "cfg-core: Cfg::blocks_for_stmt (innermost statement ref, all finally copies), Cfg::path_avoiding (BFS witness path avoiding a block set, optional skipped first-edge kinds), Cfg::must_pass_through_any. Plan Task 2."
```

- [ ] **Step 2: Write the failing cfg-core unit tests**

Append inside `mod tests` in `crates/cfg-core/src/query.rs` (it already has `build_linear_cfg`, `build_diamond_cfg`, `build_try_finally_cfg`):

```rust
    use std::collections::HashSet;

    fn set(blocks: &[BlockId]) -> HashSet<BlockId> {
        blocks.iter().copied().collect()
    }

    #[test]
    fn path_avoiding_finds_the_other_diamond_arm() {
        let (cfg, entry, then_blk, else_blk, _join, exit) = build_diamond_cfg();
        let path = cfg
            .path_avoiding(entry, exit, &set(&[then_blk]), &[])
            .expect("the else arm bypasses then");
        assert_eq!(path.first().map(|e| e.to), Some(else_blk));
        assert_eq!(path.last().map(|e| e.to), Some(exit));
        assert!(cfg
            .path_avoiding(entry, exit, &set(&[then_blk, else_blk]), &[])
            .is_none());
    }

    #[test]
    fn path_avoiding_skips_edge_kinds_leaving_from() {
        let (cfg, entry, try_body, finally, exit) = build_try_finally_cfg();
        assert!(cfg
            .path_avoiding(entry, exit, &set(&[finally]), &[EdgeKind::ExceptionThrow])
            .is_none());
        let path = cfg
            .path_avoiding(entry, exit, &set(&[try_body]), &[])
            .expect("the exception edge reaches finally then exit");
        assert_eq!(path[0].kind, EdgeKind::ExceptionThrow);
    }

    #[test]
    fn path_avoiding_finds_cycles_back_to_from() {
        let mut b = DefaultCfgBuilder::new("Loop".to_string(), 0..10);
        let entry = b.new_block(BasicBlockKind::Entry);
        let head = b.new_block(BasicBlockKind::Normal);
        let body = b.new_block(BasicBlockKind::Normal);
        let exit = b.new_block(BasicBlockKind::Exit);
        b.add_edge(entry, head, EdgeKind::Normal);
        b.add_edge(head, body, EdgeKind::ConditionalTrue);
        b.add_edge(body, head, EdgeKind::LoopBack);
        b.add_edge(head, exit, EdgeKind::LoopExit);
        b.set_entry(entry);
        b.set_exit(exit);
        let cfg = b.finish();
        let path = cfg
            .path_avoiding(body, body, &HashSet::new(), &[])
            .expect("body reaches itself through the loop header");
        assert_eq!(path.len(), 2);
        assert!(cfg.path_avoiding(body, body, &set(&[head]), &[]).is_none());
    }

    #[test]
    fn must_pass_through_any_accepts_a_set() {
        let (cfg, entry, then_blk, else_blk, _join, _exit) = build_diamond_cfg();
        assert!(cfg.must_pass_through_any(entry, &set(&[then_blk, else_blk])));
        assert!(!cfg.must_pass_through_any(entry, &set(&[then_blk])));
    }

    #[test]
    fn blocks_for_stmt_returns_every_copy_of_the_innermost_statement() {
        let stmt = |range: std::ops::Range<usize>| StmtRef {
            byte_range: range,
            node_kind: "statement".to_string(),
        };
        let mut b = DefaultCfgBuilder::new("Copies".to_string(), 0..100);
        let entry = b.new_block(BasicBlockKind::Entry);
        let outer = b.new_block(BasicBlockKind::Normal);
        let copy_a = b.new_block(BasicBlockKind::FinallyHandler);
        let copy_b = b.new_block(BasicBlockKind::FinallyHandler);
        let exit = b.new_block(BasicBlockKind::Exit);
        b.add_stmt(outer, stmt(0..50));
        b.add_stmt(copy_a, stmt(10..20));
        b.add_stmt(copy_b, stmt(10..20));
        b.add_edge(entry, outer, EdgeKind::Normal);
        b.add_edge(outer, exit, EdgeKind::Normal);
        b.set_entry(entry);
        b.set_exit(exit);
        let cfg = b.finish();
        assert_eq!(cfg.blocks_for_stmt(12..15), vec![copy_a, copy_b]);
        assert_eq!(cfg.blocks_for_stmt(30..35), vec![outer]);
        assert!(cfg.blocks_for_stmt(60..70).is_empty());
    }
```

`BlockId` needs `Ord` for the sorted result: add `PartialOrd, Ord` to its derive in `crates/cfg-core/src/types.rs` (`NodeIndex` is `Ord`).

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p cfg-core`
Expected: compile errors `no method named path_avoiding` / `blocks_for_stmt` / `must_pass_through_any`.

- [ ] **Step 4: Implement**

In `crates/cfg-core/src/query.rs`, extend the imports and add before the `#[cfg(test)]` module:

```rust
use petgraph::visit::EdgeRef;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;

/// One edge of a path returned by [`Cfg::path_avoiding`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEdge {
    pub from: BlockId,
    pub to: BlockId,
    pub kind: EdgeKind,
}

impl Cfg {
    /// Every block holding the innermost statement reference that contains
    /// `range`, sorted by block index.
    ///
    /// The Pascal builder copies a `finally` body once per continuation
    /// (normal completion, exception, each `Exit`/`Break`/`goto`), so one
    /// source statement can live in several blocks; all copies are returned.
    pub fn blocks_for_stmt(&self, range: Range<usize>) -> Vec<BlockId> {
        let contains = |s: &StmtRef| s.byte_range.start <= range.start && range.end <= s.byte_range.end;
        let Some(innermost) = self
            .graph
            .node_weights()
            .flat_map(|block| block.stmts.iter())
            .filter(|s| contains(s))
            .map(|s| s.byte_range.len())
            .min()
        else {
            return Vec::new();
        };
        let mut blocks: Vec<BlockId> = self
            .graph
            .node_indices()
            .filter(|&index| {
                self.graph[index]
                    .stmts
                    .iter()
                    .any(|s| contains(s) && s.byte_range.len() == innermost)
            })
            .map(BlockId)
            .collect();
        blocks.sort();
        blocks
    }

    /// Shortest path from `from` to `to` that never enters a block in
    /// `avoid`. The path starts with an outgoing edge of `from`, so
    /// `from == to` asks for a cycle. Edges leaving `from` whose kind is in
    /// `skip_from` are never taken (for example, ignore exceptions raised by
    /// the statement that is the subject of the query).
    pub fn path_avoiding(
        &self,
        from: BlockId,
        to: BlockId,
        avoid: &HashSet<BlockId>,
        skip_from: &[EdgeKind],
    ) -> Option<Vec<PathEdge>> {
        if avoid.contains(&to) {
            return None;
        }
        let mut parent: HashMap<BlockId, PathEdge> = HashMap::new();
        let mut queue = VecDeque::from([from]);
        let mut expanded = HashSet::new();
        while let Some(block) = queue.pop_front() {
            if !expanded.insert(block) {
                continue;
            }
            for edge in self.graph.edges(block.0) {
                if block == from && skip_from.contains(edge.weight()) {
                    continue;
                }
                let target = BlockId(edge.target());
                if avoid.contains(&target) || parent.contains_key(&target) {
                    continue;
                }
                parent.insert(
                    target,
                    PathEdge {
                        from: block,
                        to: target,
                        kind: edge.weight().clone(),
                    },
                );
                if target == to {
                    return Some(reconstruct(&parent, from, to));
                }
                queue.push_back(target);
            }
        }
        None
    }

    /// True if every path from `from` to exit enters a block of `through`.
    pub fn must_pass_through_any(&self, from: BlockId, through: &HashSet<BlockId>) -> bool {
        through.contains(&from) || self.path_avoiding(from, self.exit, through, &[]).is_none()
    }
}

fn reconstruct(parent: &HashMap<BlockId, PathEdge>, from: BlockId, to: BlockId) -> Vec<PathEdge> {
    let mut path = Vec::new();
    let mut current = to;
    loop {
        let edge = parent[&current].clone();
        current = edge.from;
        path.push(edge);
        if current == from {
            break;
        }
    }
    path.reverse();
    path
}
```

`reconstruct` terminates because every entry in `parent` was reached from a block already expanded from `from`. In `crates/cfg-core/src/lib.rs` add `pub use query::PathEdge;`.

- [ ] **Step 5: Run the unit tests**

Run: `cargo test -p cfg-core`
Expected: PASS.

- [ ] **Step 6: Write the cfg-pascal integration test**

Create `crates/cfg-pascal/tests/finally_copies_test.rs`:

```rust
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
    assert!(frees.len() >= 2, "finally is copied per continuation: {frees:?}");
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
```

Run: `cargo test -p cfg-pascal --test finally_copies_test`
Expected: PASS (the builder already duplicates finally bodies; this pins the public contract).

- [ ] **Step 7: Document**

In `crates/cfg-pascal/docs/cfg-lsp-support.md`, add before `## Project snapshot API`:

```markdown
## Statement copies

A `finally` body is built once per continuation that runs it: normal
completion, exception propagation, and each `Exit`, `Break`, `Continue` or
`goto` that leaves the protected region. Each copy is a separate
`FinallyHandler` block holding the same statement references. Consumers must
treat the copies as one source region: `Cfg::blocks_for_stmt(range)` returns
every block holding the innermost statement that contains `range`, and
`Cfg::path_avoiding` / `Cfg::must_pass_through_any` accept the resulting set.
```

- [ ] **Step 8: Verify, commit and close**

Run: `cargo test -p cfg-core -p cfg-pascal -p lint4d && cargo clippy -p cfg-core -p cfg-pascal --all-targets -- -D warnings && cargo fmt --check`
Expected: all pass.

```bash
git add crates/cfg-core/src crates/cfg-pascal/tests/finally_copies_test.rs
git add -f crates/cfg-pascal/docs/cfg-lsp-support.md
git commit -m "feat(cfg-core): group finally copies and add set-based path queries (l4d-wzt)"
bd update l4d-wzt --acceptance "- [x] Duplicated finally copies identifiable as one source region via public API
- [x] Set-based must-pass query or equivalent
- [x] Duplication documented in cfg-lsp-support.md"
bd close l4d-wzt --reason "Added Cfg::blocks_for_stmt, Cfg::path_avoiding (PathEdge witness), Cfg::must_pass_through_any; documented finally copies. Commit: <sha>. Verified: cfg-core unit tests, cfg-pascal finally_copies_test, workspace clippy/fmt."
```

`git add -f` on the docs file is harmless if the path is not ignored.

### Task 3: exception edges for statements outside try regions (l4d-d8d)

**Files:**
- Modify: `crates/cfg-pascal/src/pascal_builder.rs` (`BuildContext` init ~line 338, field doc ~591-595, `prepare_statement_block` ~2347, `implicit_exception_transfers` ~2359)
- Modify: `crates/cfg-pascal/tests/exception_flow_test.rs` (new test; update test at ~1690-1773)
- Modify: `crates/cfg-pascal/tests/builder_test.rs` (~line 33), `crates/cfg-pascal/tests/control_flow_test.rs` (~line 250-275)
- Modify: `crates/cfg-pascal/docs/cfg-lsp-support.md`

**Interfaces:**
- Produces: in every routine CFG, each executable statement and each condition/loop header ends its basic block and has an `ExceptionThrow` edge to the nearest enclosing handler, or to Exit when no handler encloses it. This uses the same may-raise rule as inside `try` (every executable statement may raise). Task 6 relies on it.

A planning probe (2026-10-11) started the walk at `implicit_exception_depth = 1`: exactly three cfg-pascal tests failed (all asserting the old shape) and every lint4d rule test passed.

- [ ] **Step 1: Claim the issue and record the baseline binary**

```bash
bd update l4d-d8d --claim
bd update l4d-d8d --design "Every executable statement may raise, inside or outside try: start the routine walk with implicit-exception modelling active so unprotected statements split per block and route ExceptionThrow to Exit. Plan Task 3."
cargo build --release -p lint4d && cp target/release/lint4d /tmp/lint4d-before-d8d
```

- [ ] **Step 2: Write the failing test**

Append to `crates/cfg-pascal/tests/exception_flow_test.rs` (its helpers `parse_clean`, `cfg_for`, `block_with_stmt`, `successors` already exist; add `EdgeKind` to the `cfg_core` import if it is missing):

```rust
#[test]
fn unprotected_statements_propagate_exceptions_to_exit() {
    let source = br#"
unit Unprotected;
interface
implementation

procedure Straight;
var
  L: TStringList;
begin
  L := TStringList.Create;
  L.Add('x');
  if L.Count > 0 then
    L.Clear;
  L.Free;
end;

end.
"#
    .to_vec();
    let tree = parse_clean(&source);
    let cfgs = build_file_cfgs(&tree, &source);
    let cfg = cfg_for(&cfgs, "Straight");

    let create = block_with_stmt(cfg, &source, "assignment", "TStringList.Create");
    let add = block_with_stmt(cfg, &source, "statement", "L.Add");
    let condition = block_with_stmt(cfg, &source, "if", "if L.Count");
    let free = block_with_stmt(cfg, &source, "statement", "L.Free");
    assert_ne!(create, add, "each statement that may raise ends its block");
    assert_ne!(add, free);
    for block in [create, add, condition, free] {
        assert!(
            successors(cfg, block).contains(&(cfg.exit, EdgeKind::ExceptionThrow)),
            "unprotected block {block:?} must propagate exceptions to Exit"
        );
    }
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test -p cfg-pascal --test exception_flow_test unprotected_statements_propagate_exceptions_to_exit`
Expected: FAIL at `assert_ne!(create, add, ...)` (the unprotected statements share one block).

- [ ] **Step 4: Minimal fix**

In `build_scope_cfg` (`crates/cfg-pascal/src/pascal_builder.rs`, ~line 338) change

```rust
        implicit_exception_depth: 0,
```
to
```rust
        // Every executable statement may raise (l4d-d8d): unprotected
        // statements propagate to Exit exactly as protected ones reach
        // their handler.
        implicit_exception_depth: 1,
```

Replace the `implicit_exception_depth` field doc (~591-595) with:

```rust
    /// Number of active exception-modelling scopes. The routine walk starts
    /// at 1, so every executable statement ends its block and has an
    /// exceptional edge to the nearest handler, or to Exit when none
    /// encloses it. Try bodies, handlers and finally bodies add a level.
```

Replace the `prepare_statement_block` doc comment with
`/// Start a fresh block for an executable statement when the current block already holds one, so its exceptional edge covers only that statement.`

- [ ] **Step 5: Run the new test**

Run: `cargo test -p cfg-pascal --test exception_flow_test unprotected_statements_propagate_exceptions_to_exit`
Expected: PASS.

- [ ] **Step 6: Update the three tests that pinned the old shape**

Run `cargo test -p cfg-pascal --no-fail-fast` and confirm exactly these fail: `linear_proc_has_entry_body_exit`, `exit_with_value_inside_statement_wrapper_terminates_function`, `implicit_exceptions_attach_to_protected_statements_and_conditions_only`. Any other failure is a new finding: stop and analyse it before continuing.

1. `builder_test.rs::linear_proc_has_entry_body_exit` (`X := 1; X := X + 1;`): entry, `X := 1`, `X := X + 1`, exit. Edges: entry→b1, b1→b2, b2→exit Normal, plus b1→exit and b2→exit ExceptionThrow. Change the expectations and messages to:

```rust
    // entry -> X := 1 -> X := X + 1 -> exit, each statement with an
    // exception edge to exit = 4 blocks, 5 edges
    assert_eq!(cfg.graph.node_count(), 4, "SimpleLinear should have 4 blocks");
    assert_eq!(cfg.graph.edge_count(), 5, "SimpleLinear should have 5 edges");
```

If the failure output shows different counts, dump the graph (`println!("{}", cfg.to_dot())`) and set the numbers only after confirming every extra edge is an `ExceptionThrow` to Exit.

2. `control_flow_test.rs::exit_with_value_inside_statement_wrapper_terminates_function`: `Exit(Value)` now also may raise while evaluating its argument. Replace the `assert_eq!(successors(...), vec![(cfg.exit, EdgeKind::Normal)], ...)` with:

```rust
    let exit_successors = successors(cfg, exit_stmt);
    assert!(
        exit_successors.contains(&(cfg.exit, EdgeKind::Normal)),
        "Exit(value) wrapped in a statement node must terminate the function"
    );
    assert!(
        exit_successors.iter().all(|(target, _)| *target == cfg.exit),
        "Exit(value) only leaves to Exit: {exit_successors:?}"
    );
```

3. `exception_flow_test.rs::implicit_exceptions_attach_to_protected_statements_and_conditions_only`: replace the `before` assertion with:

```rust
    let before_successors = successors(cfg, before);
    assert!(
        !before_successors.contains(&(handler, EdgeKind::ExceptionThrow)),
        "unprotected code before try must not enter the inner handler"
    );
    assert!(
        before_successors.contains(&(cfg.exit, EdgeKind::ExceptionThrow)),
        "unprotected code before try propagates to Exit"
    );
```

and rename the test to `implicit_exceptions_reach_the_nearest_handler_or_exit`.

Run: `cargo test -p cfg-pascal --no-fail-fast`
Expected: PASS.

- [ ] **Step 7: Review the CFG rules**

Run: `cargo test -p lint4d --no-fail-fast`
Expected: PASS (`use_after_free`, `nil_check`/`unchecked-nil`, `transaction` included).

Compare real-world output against the baseline (run from the Multidev root so its `.lint4d.toml` applies):

```bash
cargo build --release -p lint4d
cd ~/gits/multidev
/tmp/lint4d-before-d8d --format json --fail-on error Common/BO Projects/WebImportExport > /tmp/d8d-before.json || true
~/gits/lint4d/target/release/lint4d --format json --fail-on error Common/BO Projects/WebImportExport > /tmp/d8d-after.json || true
python3 -I -c 'import json,sys; [print(json.dumps(json.load(open(p)), indent=1, sort_keys=True), file=open(p + ".sorted", "w")) for p in sys.argv[1:]]' /tmp/d8d-before.json /tmp/d8d-after.json
diff /tmp/d8d-before.json.sorted /tmp/d8d-after.json.sorted | head -200
```

Expected: no diff. For each diagnostic that changes, read the source and decide whether the new result is correct. Record every change and its justification with `bd update l4d-d8d --append-notes "..."`. If a change is a false positive, create `bd create --type=bug --deps discovered-from:l4d-d8d ...` and do not close l4d-d8d until it is fixed (test first) or the user accepts it.

- [ ] **Step 8: Refactor (optional, only if the tests stay green)**

`implicit_exception_depth` can no longer be 0, so `prepare_statement_block`'s `implicit_exception_depth == 0 ||` test, the `== 0` branch of `implicit_exception_transfers`, and the `> 0` guard in the `raise` arm (~line 1458) are dead. Remove those three conditions; leave the counter and its increments in place, because it is also part of the finalizer cache key (~line 1990). Re-run `cargo test -p cfg-pascal -p lint4d`.

- [ ] **Step 9: Document**

In `crates/cfg-pascal/docs/cfg-lsp-support.md`, add after the "Statement copies" section:

```markdown
## Exception edges

Every executable statement, condition and loop header may raise. Each ends
its basic block and has an `ExceptionThrow` edge to the nearest enclosing
handler (a typed or bare `except` dispatch, or a `finally` copy) or, when no
handler encloses it, to the routine's Exit. "Can this routine raise to its
caller" is therefore answered by an `ExceptionThrow` edge into Exit.
```

- [ ] **Step 10: Verify, commit and close**

Run: `cargo test --workspace --no-fail-fast && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: all pass.

```bash
git add crates/cfg-pascal/src/pascal_builder.rs crates/cfg-pascal/tests
git add -f crates/cfg-pascal/docs/cfg-lsp-support.md
git commit -m "fix(cfg-pascal): model exception propagation for statements outside try (l4d-d8d)"
bd update l4d-d8d --acceptance "- [x] Decision recorded (model propagation vs documented limitation)
- [x] If modelled: failing test first; statements outside handlers can reach Exit via ExceptionThrow
- [x] CFG-based rule tests reviewed for changed results"
bd close l4d-d8d --reason "Routine walk starts with exception modelling active: every executable statement ends its block and propagates to the nearest handler or Exit. Updated 3 cfg-pascal tests that pinned the old shape. Lint rule tests unchanged; Multidev Common/BO + WebImportExport lint diff: <result>. Commit: <sha>."
```

### Task 4: digest data model and renderers

**Files:**
- Create: `crates/lint4d/src/digest/mod.rs`, `crates/lint4d/src/digest/model.rs`, `crates/lint4d/src/digest/render.rs`
- Modify: `crates/lint4d/src/lib.rs` (add `pub mod digest;`)
- Modify: `docs/superpowers/specs/2026-10-11-routine-digest-design.md` (clarifications)

**Interfaces:**
- Produces (`lint4d::digest`): every model type below; `LineIndex::new(&[u8])`, `LineIndex::line(byte) -> usize` (1-based); `normalize(&str) -> String`; `truncate(&str, usize) -> String`; `RoutineDigest::line_count()`, `RoutineDigest::markers() -> Markers`, `Markers::render() -> String`.
- Produces (`lint4d::digest::render`): `render_index(&FileDigest) -> String`, `render_routine(&RoutineDigest, file_name: &str, max_lines: Option<usize>) -> String`, `render_tree(&[DigestNode]) -> String` (uncollapsed body only), `render_json(path: &str, routines: &[&RoutineDigest]) -> String`, `file_name(path: &str) -> String`.

- [ ] **Step 1: Claim the digest issue and apply the spec clarifications**

```bash
bd update l4d-7mw.1 --claim
bd update l4d-7mw.1 --design "Plan: docs/superpowers/plans/2026-10-11-routine-digest.md Tasks 4-10 (lint4d::digest model/render/tree/lifecycle/select, clap subcommand, snapshots, skill)."
```

In the spec, replace

```
4. Assignment to `Result`, to a field, or no free at all yields
   `NotFreedHere`.
```
with
```
4. Assignment to `Result` yields `NotFreedHere (returned as Result)`. A
   non-local target (field, global or parameter) with no free in the routine
   yields `NotFreedHere (assigned to a non-local)`; a local with no free
   yields `NotFreedHere (no free found)`. A non-local that is freed in the
   routine gets the normal path verdict.
```
and replace `subtrees collapse to `L4430-4441 … (12 lines, calls aQry.Next, existingSkus.Add)`.` with
`subtrees collapse to `L4430-4441 while not aQry.Eof … (12 lines, calls aQry.Next, existingSkus.Add)`, keeping the head text.` Add one line to §6.4: `` `goto`, `Break` and `Continue` produce no fact line. ``

- [ ] **Step 2: Write the model**

Create `crates/lint4d/src/digest/model.rs`:

```rust
//! Serializable digest model shared by the text, index and JSON renderers.

use std::ops::Range;

use cfg_pascal::calls::TransactionOp;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileDigest {
    pub path: String,
    pub routines: Vec<RoutineDigest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoutineDigest {
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
    pub byte_range: Range<usize>,
    pub objects: Vec<ObjectVerdict>,
    pub body: Vec<DigestNode>,
    pub degraded: Vec<Degradation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum DigestNode {
    Control {
        kind: ControlKind,
        line: usize,
        end_line: usize,
        label: String,
        children: Vec<DigestNode>,
    },
    Fact {
        line: usize,
        fact: Fact,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    If,
    Else,
    Case,
    Arm,
    For,
    While,
    Repeat,
    Try,
    Finally,
    Except,
    On,
    ExceptElse,
    With,
    Conditional,
    Branch,
    Asm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fact {
    New { class: String, target: Option<String> },
    Free { var: String },
    Tx { op: TxOp, receiver: String, method: String },
    Raise { text: String },
    ReRaise,
    Exit { runs_finally: Option<usize> },
    Calls { calls: Vec<CallCount> },
    Nested { name: String },
    Lambda { calls: Vec<CallCount> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CallCount {
    pub name: String,
    pub count: usize,
}

/// Add one call, keeping first-use order.
pub fn add_call(calls: &mut Vec<CallCount>, name: String) {
    match calls.iter_mut().find(|call| call.name == name) {
        Some(call) => call.count += 1,
        None => calls.push(CallCount { name, count: 1 }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TxOp {
    Start,
    Commit,
    Rollback,
    InTransaction,
}

impl From<TransactionOp> for TxOp {
    fn from(op: TransactionOp) -> Self {
        match op {
            TransactionOp::Start => Self::Start,
            TransactionOp::Commit => Self::Commit,
            TransactionOp::Rollback => Self::Rollback,
            TransactionOp::InTransaction => Self::InTransaction,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectVerdict {
    pub var: String,
    pub class: String,
    pub line: usize,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Verdict {
    FreedAllPaths,
    /// At most three escapes, ordered by line; `more` when others exist.
    Escapes { via: Vec<Escape>, more: bool },
    NotFreedHere { why: NotFreedReason },
    Unknown { why: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Escape {
    pub line: usize,
    pub how: EscapeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EscapeKind {
    Exception,
    Exit,
    FallsOffEnd,
    ReCreated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotFreedReason {
    Result,
    NonLocal,
    NeverFreed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Degradation {
    ParseErrors { lines: Vec<usize> },
    Conditional { regions: usize },
    HiddenDirective { line: usize },
    AnonymousMethods,
}

/// Routine-index markers, derived from the same model as the full digest.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Markers {
    pub tx: bool,
    pub raises: usize,
    pub exits: usize,
    pub escape: bool,
    pub unknown: bool,
}

impl Markers {
    pub fn render(&self) -> String {
        let mut parts = Vec::new();
        if self.tx {
            parts.push("tx".to_string());
        }
        if self.raises > 0 {
            parts.push(format!("raise{}", self.raises));
        }
        if self.exits > 0 {
            parts.push(format!("exit{}", self.exits));
        }
        if self.escape {
            parts.push("!escape".to_string());
        }
        if self.unknown {
            parts.push("?".to_string());
        }
        parts.join(" ")
    }
}

impl RoutineDigest {
    pub fn line_count(&self) -> usize {
        self.end_line + 1 - self.start_line
    }

    pub fn markers(&self) -> Markers {
        let mut markers = Markers {
            escape: self
                .objects
                .iter()
                .any(|o| matches!(o.verdict, Verdict::Escapes { .. })),
            unknown: self
                .objects
                .iter()
                .any(|o| matches!(o.verdict, Verdict::Unknown { .. })),
            ..Markers::default()
        };
        count_facts(&self.body, &mut markers);
        markers
    }
}

fn count_facts(nodes: &[DigestNode], markers: &mut Markers) {
    for node in nodes {
        match node {
            DigestNode::Control { children, .. } => count_facts(children, markers),
            DigestNode::Fact { fact, .. } => match fact {
                Fact::Tx { .. } => markers.tx = true,
                Fact::Raise { .. } | Fact::ReRaise => markers.raises += 1,
                Fact::Exit { .. } => markers.exits += 1,
                _ => {}
            },
        }
    }
}
```

- [ ] **Step 3: Write `mod.rs`**

Create `crates/lint4d/src/digest/mod.rs`:

```rust
//! Per-routine control-flow digests for coding agents (l4d-7mw.1).

pub mod model;
pub mod render;

pub use model::*;

/// 1-based line lookup over source bytes. CRLF and LF both end at `\n`.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(source: &[u8]) -> Self {
        let mut starts = vec![0];
        starts.extend(
            source
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'\n')
                .map(|(index, _)| index + 1),
        );
        Self { starts }
    }

    pub fn line(&self, byte: usize) -> usize {
        match self.starts.binary_search(&byte) {
            Ok(index) => index + 1,
            Err(index) => index,
        }
    }
}

/// Collapse every whitespace run (including CR and LF) to one space.
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut `text` to `max` characters followed by `…`.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index_is_one_based_for_lf_and_crlf() {
        let lf = LineIndex::new(b"a\nbc\nd");
        assert_eq!((lf.line(0), lf.line(2), lf.line(3), lf.line(5)), (1, 2, 2, 3));
        let crlf = LineIndex::new(b"a\r\nbc\r\nd");
        assert_eq!((crlf.line(0), crlf.line(3), crlf.line(7)), (1, 2, 3));
    }

    #[test]
    fn truncate_counts_characters() {
        assert_eq!(truncate("été", 3), "été");
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(normalize(" a\r\n  b\t c "), "a b c");
    }
}
```

Add `pub mod digest;` to `crates/lint4d/src/lib.rs` (alphabetical, after `pub mod config;`).

- [ ] **Step 4: Write the failing renderer tests**

Create `crates/lint4d/src/digest/render.rs` containing only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fact(line: usize, fact: Fact) -> DigestNode {
        DigestNode::Fact { line, fact }
    }

    fn call(name: &str, count: usize) -> CallCount {
        CallCount { name: name.to_string(), count }
    }

    fn arm(line: usize, end_line: usize, label: &str, children: Vec<DigestNode>) -> DigestNode {
        DigestNode::Control { kind: ControlKind::Arm, line, end_line, label: label.to_string(), children }
    }

    fn sample() -> RoutineDigest {
        RoutineDigest {
            name: "TFoo.Run".to_string(),
            start_line: 10,
            end_line: 30,
            byte_range: 0..100,
            objects: vec![
                ObjectVerdict { var: "L".into(), class: "TStringList".into(), line: 12, verdict: Verdict::FreedAllPaths },
                ObjectVerdict {
                    var: "Q".into(),
                    class: "TQuery".into(),
                    line: 13,
                    verdict: Verdict::Escapes {
                        via: vec![
                            Escape { line: 14, how: EscapeKind::Exception },
                            Escape { line: 15, how: EscapeKind::Exit },
                            Escape { line: 16, how: EscapeKind::FallsOffEnd },
                        ],
                        more: true,
                    },
                },
                ObjectVerdict { var: "Result".into(), class: "TList".into(), line: 17, verdict: Verdict::NotFreedHere { why: NotFreedReason::Result } },
                ObjectVerdict { var: "M".into(), class: "TList".into(), line: 18, verdict: Verdict::Unknown { why: "reassigned at L20".into() } },
            ],
            body: vec![
                fact(11, Fact::Calls { calls: vec![call("Log", 2), call("Prepare", 1)] }),
                DigestNode::Control {
                    kind: ControlKind::Case,
                    line: 19,
                    end_line: 24,
                    label: "A".into(),
                    children: vec![
                        arm(20, 20, "1", vec![fact(20, Fact::Tx { op: TxOp::Commit, receiver: "Db".into(), method: "Commit".into() })]),
                        arm(21, 23, "else", vec![fact(22, Fact::Free { var: "L".into() }), fact(23, Fact::ReRaise)]),
                    ],
                },
                fact(25, Fact::Exit { runs_finally: Some(28) }),
            ],
            degraded: vec![Degradation::Conditional { regions: 2 }, Degradation::AnonymousMethods],
        }
    }

    const HEAD: &str = "TFoo.Run  Foo.pas:10-30 (21 lines)
! conditional code: 2 {$IF…} regions, all branches shown
! anonymous methods not analysed
objects: L TStringList L12 freed on all paths; Q TQuery L13 NOT freed on all paths (escapes via L14 exception, L15 exit, L16 falls off end, …); Result TList L17 not freed in this routine (returned as Result); M TList L18 unknown (reassigned at L20)
";

    #[test]
    fn renders_a_full_routine() {
        let expected = format!(
            "{HEAD}L11 calls Log ×2, Prepare
L19 case A
  1: L20 tx commit Db.Commit
  else:
    L22 free L
    L23 re-raise
L25 exit (runs finally L28)
"
        );
        assert_eq!(render_routine(&sample(), "Foo.pas", None), expected);
    }

    #[test]
    fn collapses_deepest_controls_to_fit_max_lines() {
        let expected = format!(
            "{HEAD}L11 calls Log ×2, Prepare
L19-24 case A … (6 lines)
L25 exit (runs finally L28)
"
        );
        assert_eq!(render_routine(&sample(), "Foo.pas", Some(8)), expected);
    }

    #[test]
    fn renders_the_index_from_markers() {
        let file = FileDigest { path: "src/Foo.pas".into(), routines: vec![sample()] };
        assert_eq!(
            render_index(&file),
            "Foo.pas  1 routine\nL10-30  21  TFoo.Run  tx raise1 exit1 !escape ?\n"
        );
    }

    #[test]
    fn json_uses_tagged_variants() {
        let routine = sample();
        let json: serde_json::Value =
            serde_json::from_str(&render_json("src/Foo.pas", &[&routine])).unwrap();
        assert_eq!(json["path"], "src/Foo.pas");
        let r = &json["routines"][0];
        assert_eq!(r["objects"][1]["verdict"]["status"], "escapes");
        assert_eq!(r["objects"][1]["verdict"]["via"][0]["how"], "exception");
        assert_eq!(r["body"][0]["node"], "fact");
        assert_eq!(r["body"][0]["fact"]["kind"], "calls");
        assert_eq!(r["body"][1]["kind"], "case");
        assert_eq!(r["degraded"][1]["kind"], "anonymous_methods");
    }
}
```

Add `pub mod render;` is already in `mod.rs`. Run: `cargo test -p lint4d --lib digest`
Expected: compile errors (`render_routine`, `render_index`, `render_json` not found).

- [ ] **Step 5: Implement the renderers**

Prepend to `crates/lint4d/src/digest/render.rs` (above the test module):

```rust
//! Text, index and JSON views of a [`FileDigest`]. Pure functions of the model.

use std::path::Path;

use serde::Serialize;

use super::model::*;

/// The last path component, used in headers and the index.
pub fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |name| name.to_string_lossy().into_owned())
}

pub fn render_index(file: &FileDigest) -> String {
    let count = file.routines.len();
    let mut out = format!(
        "{}  {count} routine{}\n",
        file_name(&file.path),
        if count == 1 { "" } else { "s" }
    );
    let spans: Vec<String> = file
        .routines
        .iter()
        .map(|r| format!("L{}-{}", r.start_line, r.end_line))
        .collect();
    let span_w = spans.iter().map(String::len).max().unwrap_or(0);
    let count_w = file
        .routines
        .iter()
        .map(|r| r.line_count().to_string().len())
        .max()
        .unwrap_or(0);
    let name_w = file
        .routines
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);
    for (routine, span) in file.routines.iter().zip(&spans) {
        let line = format!(
            "{span:<span_w$}  {:>count_w$}  {:<name_w$}  {}",
            routine.line_count(),
            routine.name,
            routine.markers().render()
        );
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Header, degradation lines, objects line, then the body. With
/// `max_lines`, the deepest control subtrees collapse until the output fits;
/// the header lines are never collapsed.
pub fn render_routine(routine: &RoutineDigest, file_name: &str, max_lines: Option<usize>) -> String {
    let mut head = vec![format!(
        "{}  {file_name}:{}-{} ({} lines)",
        routine.name,
        routine.start_line,
        routine.end_line,
        routine.line_count()
    )];
    head.extend(routine.degraded.iter().map(|d| format!("! {}", degradation_text(d))));
    if !routine.objects.is_empty() {
        let objects: Vec<String> = routine.objects.iter().map(object_text).collect();
        head.push(format!("objects: {}", objects.join("; ")));
    }
    let mut body = Vec::new();
    render_nodes(&routine.body, 0, usize::MAX, &mut body);
    if let Some(limit) = max_lines {
        let budget = limit.saturating_sub(head.len());
        let mut depth = max_depth(&routine.body, 0);
        while body.len() > budget {
            body.clear();
            render_nodes(&routine.body, 0, depth, &mut body);
            if depth == 0 {
                break;
            }
            depth -= 1;
        }
    }
    head.extend(body);
    let mut out = head.join("\n");
    out.push('\n');
    out
}

/// The uncollapsed body only.
pub fn render_tree(nodes: &[DigestNode]) -> String {
    let mut lines = Vec::new();
    render_nodes(nodes, 0, usize::MAX, &mut lines);
    lines.iter().map(|line| format!("{line}\n")).collect()
}

pub fn render_json(path: &str, routines: &[&RoutineDigest]) -> String {
    #[derive(Serialize)]
    struct Output<'a> {
        path: &'a str,
        routines: &'a [&'a RoutineDigest],
    }
    serde_json::to_string_pretty(&Output { path, routines }).expect("digest model serializes")
}

fn max_depth(nodes: &[DigestNode], depth: usize) -> usize {
    nodes
        .iter()
        .filter_map(|node| match node {
            DigestNode::Control { children, .. } if !children.is_empty() => {
                Some(max_depth(children, depth + 1).max(depth))
            }
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn render_nodes(nodes: &[DigestNode], depth: usize, collapse_at: usize, out: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    for node in nodes {
        match node {
            DigestNode::Fact { line, fact } => out.push(format!("{indent}L{line} {}", fact_text(fact))),
            DigestNode::Control { kind, line, end_line, label, children } => {
                if depth >= collapse_at && !children.is_empty() {
                    let lines = end_line + 1 - line;
                    out.push(format!(
                        "{indent}L{line}-{end_line} {} … ({lines} line{}{})",
                        head_text(*kind, label),
                        if lines == 1 { "" } else { "s" },
                        calls_suffix(children)
                    ));
                } else if *kind == ControlKind::Arm {
                    if let [DigestNode::Fact { line, fact }] = children.as_slice() {
                        out.push(format!("{indent}{label}: L{line} {}", fact_text(fact)));
                    } else {
                        out.push(format!("{indent}{label}:"));
                        render_nodes(children, depth + 1, collapse_at, out);
                    }
                } else {
                    out.push(format!("{indent}L{line} {}", head_text(*kind, label)));
                    render_nodes(children, depth + 1, collapse_at, out);
                }
            }
        }
    }
}

fn calls_suffix(nodes: &[DigestNode]) -> String {
    let mut names: Vec<String> = Vec::new();
    collect_call_names(nodes, &mut names);
    if names.is_empty() {
        return String::new();
    }
    let more = names.len() > 5;
    names.truncate(5);
    format!(", calls {}{}", names.join(", "), if more { ", …" } else { "" })
}

fn collect_call_names(nodes: &[DigestNode], names: &mut Vec<String>) {
    for node in nodes {
        match node {
            DigestNode::Control { children, .. } => collect_call_names(children, names),
            DigestNode::Fact { fact: Fact::Calls { calls }, .. } => {
                for call in calls {
                    if !names.contains(&call.name) {
                        names.push(call.name.clone());
                    }
                }
            }
            DigestNode::Fact { .. } => {}
        }
    }
}

fn head_text(kind: ControlKind, label: &str) -> String {
    match kind {
        ControlKind::If => format!("if {label}"),
        ControlKind::Else | ControlKind::ExceptElse => "else".to_string(),
        ControlKind::Case => format!("case {label}"),
        ControlKind::Arm => format!("{label}:"),
        ControlKind::For | ControlKind::While | ControlKind::Branch => label.to_string(),
        ControlKind::Repeat => format!("repeat until {label}"),
        ControlKind::Try => "try".to_string(),
        ControlKind::Finally => "finally".to_string(),
        ControlKind::Except => "except".to_string(),
        ControlKind::On => format!("on {label}"),
        ControlKind::With => format!("with {label}"),
        ControlKind::Conditional => "conditional".to_string(),
        ControlKind::Asm => "asm".to_string(),
    }
}

fn calls_text(calls: &[CallCount]) -> String {
    calls
        .iter()
        .map(|call| match call.count {
            1 => call.name.clone(),
            n => format!("{} ×{n}", call.name),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn fact_text(fact: &Fact) -> String {
    match fact {
        Fact::New { class, target: Some(target) } => format!("new {class} -> {target}"),
        Fact::New { class, target: None } => format!("new {class}"),
        Fact::Free { var } => format!("free {var}"),
        Fact::Tx { op, receiver, method } => {
            let op = match op {
                TxOp::Start => "start",
                TxOp::Commit => "commit",
                TxOp::Rollback => "rollback",
                TxOp::InTransaction => "in-transaction",
            };
            format!("tx {op} {receiver}.{method}")
        }
        Fact::Raise { text } => format!("raise {text}"),
        Fact::ReRaise => "re-raise".to_string(),
        Fact::Exit { runs_finally: Some(line) } => format!("exit (runs finally L{line})"),
        Fact::Exit { runs_finally: None } => "exit".to_string(),
        Fact::Calls { calls } => format!("calls {}", calls_text(calls)),
        Fact::Nested { name } => format!("nested routine {name}"),
        Fact::Lambda { calls } if calls.is_empty() => "anonymous method".to_string(),
        Fact::Lambda { calls } => format!("anonymous method calls {}", calls_text(calls)),
    }
}

fn degradation_text(degradation: &Degradation) -> String {
    match degradation {
        Degradation::ParseErrors { lines } => {
            let lines: Vec<String> = lines.iter().map(|line| format!("L{line}")).collect();
            format!("parse errors at {}", lines.join(", "))
        }
        Degradation::Conditional { regions } => {
            format!("conditional code: {regions} {{$IF…}} regions, all branches shown")
        }
        Degradation::HiddenDirective { line } => format!("hidden directive block at L{line}"),
        Degradation::AnonymousMethods => "anonymous methods not analysed".to_string(),
    }
}

fn object_text(object: &ObjectVerdict) -> String {
    let verdict = match &object.verdict {
        Verdict::FreedAllPaths => "freed on all paths".to_string(),
        Verdict::Escapes { via, more } => {
            let mut items: Vec<String> = via
                .iter()
                .map(|escape| {
                    let how = match escape.how {
                        EscapeKind::Exception => "exception",
                        EscapeKind::Exit => "exit",
                        EscapeKind::FallsOffEnd => "falls off end",
                        EscapeKind::ReCreated => "re-created",
                    };
                    format!("L{} {how}", escape.line)
                })
                .collect();
            if *more {
                items.push("…".to_string());
            }
            format!("NOT freed on all paths (escapes via {})", items.join(", "))
        }
        Verdict::NotFreedHere { why } => {
            let why = match why {
                NotFreedReason::Result => "returned as Result",
                NotFreedReason::NonLocal => "assigned to a non-local",
                NotFreedReason::NeverFreed => "no free found",
            };
            format!("not freed in this routine ({why})")
        }
        Verdict::Unknown { why } => format!("unknown ({why})"),
    };
    format!("{} {} L{} {verdict}", object.var, object.class, object.line)
}
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p lint4d --lib digest`
Expected: PASS (6 tests: 2 in `mod.rs`, 4 in `render.rs`).

- [ ] **Step 7: Lint and commit**

Run: `cargo clippy -p lint4d --all-targets -- -D warnings && cargo fmt --check` (run `cargo fmt` first if it only reports formatting).

```bash
git add crates/lint4d/src/lib.rs crates/lint4d/src/digest
git add -f docs/superpowers/specs/2026-10-11-routine-digest-design.md
git commit -m "feat(lint4d): digest data model and renderers (l4d-7mw.1)"
```

### Task 5: control tree from the syntax tree

**Files:**
- Create: `crates/lint4d/src/digest/tree.rs`
- Modify: `crates/lint4d/src/digest/mod.rs` (`digest_source`, shared syntax helpers)
- Test: `crates/lint4d/tests/digest_tree_test.rs`

**Interfaces:**
- Consumes: Task 1 (`classify_call` on `statement`), Task 2 (`blocks_for_stmt`, `path_avoiding`), Task 4 model and `render_tree`.
- Produces: `lint4d::digest::digest_source(path: &Path, source: &[u8]) -> Result<FileDigest, String>`; one `RoutineDigest` per CFG from `build_file_cfgs`, in CFG order. `objects` stays empty until Task 6.
- Produces (pub in `digest`): `named_children<'t>(Node<'t>) -> Vec<Node<'t>>` (non-extra named), `field_children<'t>(Node<'t>, &str) -> Vec<Node<'t>>`, `is_exit_statement(Node, &[u8]) -> bool`, `is_keyword(Node) -> bool`, `target_name(lhs: Node, &[u8]) -> String`.
- Produces: `tree::build(scope, cfg, text, lines) -> (Vec<DigestNode>, tree::Stats { conditional_regions, lambdas })`.

- [ ] **Step 1: Write the failing tests**

Create `crates/lint4d/tests/digest_tree_test.rs`:

```rust
use std::path::Path;

use lint4d::digest::{Degradation, FileDigest, RoutineDigest, digest_source, render};

fn digest(source: &[u8]) -> FileDigest {
    digest_source(Path::new("t.pas"), source).expect("digest")
}

fn routine<'a>(file: &'a FileDigest, name: &str) -> &'a RoutineDigest {
    file.routines
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("{name} not in {:?}", file.routines.iter().map(|r| &r.name).collect::<Vec<_>>()))
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
    assert_eq!(render::render_tree(&routine(&file, "Foo").body), LIFECYCLE_TREE);
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
        vec![Degradation::Conditional { regions: 1 }, Degradation::AnonymousMethods]
    );
    assert_eq!(render::render_tree(&routine(&file, "Outer.Inner").body), "L7 calls Bar\n");
}

#[test]
fn latin1_condition_is_decoded() {
    let mut source = b"unit T;\ninterface\nimplementation\nprocedure P(S: string);\nbegin\n  if S = '".to_vec();
    source.push(0xE9);
    source.extend_from_slice(b"t' then\n    Bar;\nend;\nend.\n");
    let file = digest(&source);
    assert_eq!(render::render_tree(&routine(&file, "P").body), "L6 if S = '\u{e9}t'\n  L7 calls Bar\n");
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
    assert!(first.starts_with("L6 if (SomeVeryLongVariableName > 0) and (AnotherVeryLongVariableName < 10)"), "{tree}");
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
    assert_eq!(render::render_routine(empty, "t.pas", None), "Empty  t.pas:5-7 (3 lines)\n");
}
```

Run: `cargo test -p lint4d --test digest_tree_test`
Expected: compile error (`digest_source` not found).

- [ ] **Step 2: Add `digest_source` and the shared helpers to `mod.rs`**

In `crates/lint4d/src/digest/mod.rs`, add `pub mod tree;` next to the other modules, and add:

```rust
use std::ops::Range;
use std::path::Path;

use cfg_pascal::build_file_cfgs;
use cfg_pascal::cfg_core::Cfg;
use pascal_core::FileInfo;
use pascal_core::directive_fragment_rewrite::DirectivePatch;
use pascal_core::parser::{node_text, parse_file_with_parser_source};
use tree_sitter::Node;

/// Parse `source` the way lint does and digest every routine.
///
/// Byte offsets of the rewritten parser source equal the original ones, so
/// line numbers refer to the file as written.
pub fn digest_source(path: &Path, source: &[u8]) -> Result<FileDigest, String> {
    let parsed = parse_file_with_parser_source(&FileInfo::new(path.to_path_buf()), source)?;
    let text = parsed.parser_source.as_slice();
    let lines = LineIndex::new(source);
    let hidden: Vec<Range<usize>> = parsed
        .patches
        .iter()
        .filter_map(|patch| match patch {
            DirectivePatch::OpaqueBlock(block) => Some(block.start..block.end),
            DirectivePatch::Markers(_) => None,
        })
        .collect();
    let root = parsed.tree.root_node();
    let routines = build_file_cfgs(&parsed.tree, text)
        .iter()
        .map(|cfg| digest_routine(root, cfg, text, &lines, &hidden))
        .collect();
    Ok(FileDigest { path: path.display().to_string(), routines })
}

fn digest_routine(
    root: Node<'_>,
    cfg: &Cfg,
    text: &[u8],
    lines: &LineIndex,
    hidden: &[Range<usize>],
) -> RoutineDigest {
    let range = cfg.byte_range.clone();
    let scope = scope_node(root, &range);
    let mut degraded = Vec::new();
    let errors = scope.map(|node| error_lines(node, lines)).unwrap_or_default();
    if !errors.is_empty() {
        degraded.push(Degradation::ParseErrors { lines: errors });
    }
    let (body, stats) = match scope {
        Some(node) => tree::build(node, cfg, text, lines),
        None => (Vec::new(), tree::Stats::default()),
    };
    if stats.conditional_regions > 0 {
        degraded.push(Degradation::Conditional { regions: stats.conditional_regions });
    }
    let hidden: Vec<Range<usize>> = hidden
        .iter()
        .filter(|block| range.start <= block.start && block.end <= range.end)
        .cloned()
        .collect();
    degraded.extend(hidden.iter().map(|block| Degradation::HiddenDirective { line: lines.line(block.start) }));
    if stats.lambdas > 0 {
        degraded.push(Degradation::AnonymousMethods);
    }
    RoutineDigest {
        name: cfg.proc_name.clone(),
        start_line: lines.line(range.start),
        end_line: lines.line(range.end.saturating_sub(1).max(range.start)),
        byte_range: range,
        objects: Vec::new(),
        body,
        degraded,
    }
}

/// The node whose byte range is exactly the CFG's routine range: a
/// `defProc`, a program `block`, or an `initialization`/`finalization` section.
fn scope_node<'t>(root: Node<'t>, range: &Range<usize>) -> Option<Node<'t>> {
    let mut node = root.descendant_for_byte_range(range.start, range.end)?;
    loop {
        if node.byte_range() == *range {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// Lines of the first five ERROR/MISSING nodes inside `scope`.
fn error_lines(scope: Node<'_>, lines: &LineIndex) -> Vec<usize> {
    let mut found = Vec::new();
    let mut stack = vec![scope];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            let line = lines.line(node.start_byte());
            if !found.contains(&line) {
                found.push(line);
            }
            continue;
        }
        if node.has_error() {
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
    }
    found.sort_unstable();
    found.truncate(5);
    found
}

/// Named children that are not extras (comments).
pub fn named_children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !child.is_extra())
        .collect()
}

pub fn field_children<'t>(node: Node<'t>, field: &str) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.children_by_field_name(field, &mut cursor).collect()
}

/// Keyword tokens such as `kBegin`, `kElse` are named nodes in this grammar.
pub fn is_keyword(node: Node<'_>) -> bool {
    let kind = node.kind().as_bytes();
    kind.len() > 1 && kind[0] == b'k' && kind[1].is_ascii_uppercase()
}

/// `Exit;` or `Exit(value);`.
pub fn is_exit_statement(node: Node<'_>, source: &[u8]) -> bool {
    if node.kind() != "statement" {
        return false;
    }
    let children = named_children(node);
    let [child] = children.as_slice() else {
        return false;
    };
    let callee = match child.kind() {
        "identifier" => *child,
        "exprCall" => match child.child_by_field_name("entity") {
            Some(entity) if entity.kind() == "identifier" => entity,
            _ => return false,
        },
        _ => return false,
    };
    node_text(callee, source).eq_ignore_ascii_case("exit")
}

/// Display text of an assignment target; an inline `var X` yields `X`.
pub fn target_name(lhs: Node<'_>, source: &[u8]) -> String {
    let node = if lhs.kind() == "varAssignDef" {
        named_children(lhs)
            .into_iter()
            .find(|child| child.kind() == "identifier")
            .unwrap_or(lhs)
    } else {
        lhs
    };
    normalize(&node_text(node, source))
}
```

- [ ] **Step 3: Write the tree builder**

Create `crates/lint4d/src/digest/tree.rs`:

```rust
//! Builds a routine's control tree from its syntax tree; facts sit inline.

use std::collections::HashSet;

use cfg_pascal::calls::{CallKind, classify_call, lookup_transaction_method};
use cfg_pascal::cfg_core::{BasicBlockKind, Cfg, EdgeKind};
use pascal_core::decode_bytes;
use pascal_core::parser::node_text;
use tree_sitter::Node;

use super::model::{CallCount, ControlKind, DigestNode, Fact, add_call};
use super::{
    LineIndex, field_children, is_exit_statement, is_keyword, named_children, normalize,
    target_name, truncate,
};

const LABEL_WIDTH: usize = 70;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub conditional_regions: usize,
    pub lambdas: usize,
}

pub fn build(scope: Node<'_>, cfg: &Cfg, text: &[u8], lines: &LineIndex) -> (Vec<DigestNode>, Stats) {
    let mut builder = Builder { cfg, text, lines, stats: Stats::default() };
    let mut seq = Seq::default();
    let body = if scope.kind() == "defProc" {
        for local in field_children(scope, "local") {
            let name = local
                .child_by_field_name("header")
                .and_then(|header| header.child_by_field_name("name"));
            if let (true, Some(name)) = (local.kind() == "defProc", name) {
                seq.push(DigestNode::Fact {
                    line: lines.line(local.start_byte()),
                    fact: Fact::Nested { name: format!("{}.{}", cfg.proc_name, node_text(name, text)) },
                });
            }
        }
        scope.child_by_field_name("body")
    } else {
        Some(scope)
    };
    if let Some(body) = body {
        builder.item(body, &mut seq);
    }
    (seq.finish(), builder.stats)
}

/// Statements in source order; consecutive calls merge into one `Calls` fact.
#[derive(Default)]
struct Seq {
    out: Vec<DigestNode>,
    run: Option<(usize, Vec<CallCount>)>,
}

impl Seq {
    fn call(&mut self, line: usize, name: String) {
        let (_, calls) = self.run.get_or_insert_with(|| (line, Vec::new()));
        add_call(calls, name);
    }

    fn push(&mut self, node: DigestNode) {
        self.flush();
        self.out.push(node);
    }

    fn flush(&mut self) {
        if let Some((line, calls)) = self.run.take() {
            self.out.push(DigestNode::Fact { line, fact: Fact::Calls { calls } });
        }
    }

    fn finish(mut self) -> Vec<DigestNode> {
        self.flush();
        self.out
    }
}

enum Item {
    Fact(Fact),
    Call(String),
}

struct Builder<'a> {
    cfg: &'a Cfg,
    text: &'a [u8],
    lines: &'a LineIndex,
    stats: Stats,
}

impl Builder<'_> {
    fn item(&mut self, node: Node<'_>, seq: &mut Seq) {
        if node.is_extra() || is_keyword(node) {
            return;
        }
        let line = self.lines.line(node.start_byte());
        match node.kind() {
            "block" | "statements" | "labeledStatement" | "labeledStatementTr" | "initialization"
            | "finalization" => {
                for child in named_children(node) {
                    self.item(child, seq);
                }
            }
            "label" | "goto" => {}
            "if" | "ifElse" => self.if_node(node, seq),
            "for" | "foreach" | "while" => {
                let kind = if node.kind() == "while" { ControlKind::While } else { ControlKind::For };
                let label = self.header(node);
                let children = self.child_seq(node.child_by_field_name("body"));
                seq.push(self.control(kind, node.start_byte(), node.end_byte(), label, children));
            }
            "repeat" => {
                let label = node
                    .child_by_field_name("condition")
                    .map(|condition| self.condition_label(condition))
                    .unwrap_or_default();
                let children = self.nodes_seq(&field_children(node, "body"));
                seq.push(self.control(ControlKind::Repeat, node.start_byte(), node.end_byte(), label, children));
            }
            "case" => self.case_node(node, seq),
            "try" => self.try_node(node, seq),
            "with" => {
                let entities: Vec<String> = field_children(node, "entity")
                    .into_iter()
                    .map(|entity| self.short(entity, LABEL_WIDTH))
                    .collect();
                let children = self.child_seq(node.child_by_field_name("body"));
                seq.push(self.control(ControlKind::With, node.start_byte(), node.end_byte(), entities.join(", "), children));
            }
            "ppBlock" => self.conditional(node, seq),
            "asm" => seq.push(self.control(ControlKind::Asm, node.start_byte(), node.end_byte(), String::new(), Vec::new())),
            "raise" => {
                let fact = match node.child_by_field_name("exception") {
                    Some(exception) => Fact::Raise { text: self.short(exception, 60) },
                    None => Fact::ReRaise,
                };
                seq.push(DigestNode::Fact { line, fact });
            }
            "statement" if is_exit_statement(node, self.text) => {
                let fact = Fact::Exit { runs_finally: self.runs_finally(node) };
                seq.push(DigestNode::Fact { line, fact });
            }
            _ => self.simple(node, seq),
        }
    }

    fn control(&self, kind: ControlKind, start: usize, end: usize, label: String, children: Vec<DigestNode>) -> DigestNode {
        DigestNode::Control {
            kind,
            line: self.lines.line(start),
            end_line: self.lines.line(end.saturating_sub(1).max(start)),
            label,
            children,
        }
    }

    fn child_seq(&mut self, node: Option<Node<'_>>) -> Vec<DigestNode> {
        let mut seq = Seq::default();
        if let Some(node) = node {
            self.item(node, &mut seq);
        }
        seq.finish()
    }

    fn nodes_seq(&mut self, nodes: &[Node<'_>]) -> Vec<DigestNode> {
        let mut seq = Seq::default();
        for node in nodes {
            self.item(*node, &mut seq);
        }
        seq.finish()
    }

    fn short(&self, node: Node<'_>, max: usize) -> String {
        truncate(&normalize(&node_text(node, self.text)), max)
    }

    /// Loop header text up to the body, without the trailing `do`.
    fn header(&self, node: Node<'_>) -> String {
        let end = node.child_by_field_name("body").map_or(node.end_byte(), |body| body.start_byte());
        let text = normalize(&decode_bytes(&self.text[node.start_byte()..end]));
        let text = if text.to_ascii_lowercase().ends_with(" do") { &text[..text.len() - 3] } else { text.as_str() };
        truncate(text, LABEL_WIDTH)
    }

    /// Condition text; calls cut off by truncation are listed after it.
    fn condition_label(&mut self, condition: Node<'_>) -> String {
        let full = normalize(&node_text(condition, self.text));
        let label = truncate(&full, LABEL_WIDTH);
        if full.chars().count() <= LABEL_WIDTH {
            return label;
        }
        let hidden: Vec<String> = self
            .calls_in(condition)
            .into_iter()
            .map(|call| call.name)
            .filter(|name| !label.contains(name.as_str()))
            .collect();
        if hidden.is_empty() { label } else { format!("{label} [calls {}]", hidden.join(", ")) }
    }

    fn if_node(&mut self, node: Node<'_>, seq: &mut Seq) {
        let label = node
            .child_by_field_name("condition")
            .map(|condition| self.condition_label(condition))
            .unwrap_or_default();
        let then = node.child_by_field_name("then");
        let then_end = then.map_or(node.end_byte(), |then| then.end_byte());
        let children = self.child_seq(then);
        seq.push(self.control(ControlKind::If, node.start_byte(), then_end, label, children));
        if let Some(other) = node.child_by_field_name("else") {
            let children = self.child_seq(Some(other));
            seq.push(self.control(ControlKind::Else, other.start_byte(), other.end_byte(), String::new(), children));
        }
    }

    fn case_node(&mut self, node: Node<'_>, seq: &mut Seq) {
        let mut selector = String::new();
        let mut arms = Vec::new();
        let mut else_start = None;
        let mut else_nodes = Vec::new();
        for child in named_children(node) {
            match child.kind() {
                "caseCase" => {
                    let labels: Vec<String> = field_children(child, "label")
                        .into_iter()
                        .map(|label| self.short(label, LABEL_WIDTH))
                        .collect();
                    let children = self.child_seq(child.child_by_field_name("body"));
                    arms.push(self.control(ControlKind::Arm, child.start_byte(), child.end_byte(), labels.join(", "), children));
                }
                "kElse" | "kOtherwise" => else_start = Some(child.start_byte()),
                _ if is_keyword(child) => {}
                _ if else_start.is_some() => else_nodes.push(child),
                _ if selector.is_empty() && arms.is_empty() => selector = self.short(child, 50),
                _ => {}
            }
        }
        if let Some(start) = else_start {
            let end = else_nodes.last().map_or(start + 1, |last| last.end_byte());
            let children = self.nodes_seq(&else_nodes);
            arms.push(self.control(ControlKind::Arm, start, end, "else".to_string(), children));
        }
        seq.push(self.control(ControlKind::Case, node.start_byte(), node.end_byte(), selector, arms));
    }

    fn try_node(&mut self, node: Node<'_>, seq: &mut Seq) {
        let body = field_children(node, "try");
        let body_end = body.last().map_or(node.end_byte(), |last| last.end_byte());
        let children = self.nodes_seq(&body);
        seq.push(self.control(ControlKind::Try, node.start_byte(), body_end, String::new(), children));

        let finally = field_children(node, "finally");
        if let Some(keyword) = finally.iter().find(|n| n.kind() == "kFinally") {
            let children = self.nodes_seq(&finally);
            seq.push(self.control(ControlKind::Finally, keyword.start_byte(), node.end_byte(), String::new(), children));
        }

        let except = field_children(node, "except");
        if let Some(keyword) = except.iter().find(|n| n.kind() == "kExcept") {
            let mut inner = Seq::default();
            for handler in &except {
                match handler.kind() {
                    "exceptionHandler" => {
                        let label = self.handler_label(*handler);
                        let children = self.child_seq(handler.child_by_field_name("body"));
                        inner.push(self.control(ControlKind::On, handler.start_byte(), handler.end_byte(), label, children));
                    }
                    "exceptionElse" => {
                        let children = self.nodes_seq(&named_children(*handler));
                        inner.push(self.control(ControlKind::ExceptElse, handler.start_byte(), handler.end_byte(), String::new(), children));
                    }
                    _ => self.item(*handler, &mut inner),
                }
            }
            seq.push(self.control(ControlKind::Except, keyword.start_byte(), node.end_byte(), String::new(), inner.finish()));
        }
    }

    fn handler_label(&self, handler: Node<'_>) -> String {
        let exception = handler
            .child_by_field_name("exception")
            .map(|exception| self.short(exception, LABEL_WIDTH));
        match (handler.child_by_field_name("variable"), exception) {
            (Some(variable), Some(exception)) => format!("{}: {exception}", node_text(variable, self.text)),
            (None, Some(exception)) => exception,
            _ => String::new(),
        }
    }

    /// `{$IF…}` alternatives; every branch is shown.
    fn conditional<'t>(&mut self, node: Node<'t>, seq: &mut Seq) {
        self.stats.conditional_regions += 1;
        let mut branches = Vec::new();
        let mut current: Option<(Node<'t>, Vec<Node<'t>>)> = None;
        for child in named_children(node) {
            match child.kind() {
                "ppIf" | "ppElse" => {
                    if let Some((directive, nodes)) = current.take() {
                        branches.push(self.branch(directive, &nodes));
                    }
                    current = Some((child, Vec::new()));
                }
                "ppEndIf" => {}
                _ => {
                    if let Some((_, nodes)) = current.as_mut() {
                        nodes.push(child);
                    }
                }
            }
        }
        if let Some((directive, nodes)) = current {
            branches.push(self.branch(directive, &nodes));
        }
        seq.push(self.control(ControlKind::Conditional, node.start_byte(), node.end_byte(), String::new(), branches));
    }

    fn branch(&mut self, directive: Node<'_>, nodes: &[Node<'_>]) -> DigestNode {
        let end = nodes.last().map_or(directive.end_byte(), |last| last.end_byte());
        let label = self.short(directive, LABEL_WIDTH);
        let children = self.nodes_seq(nodes);
        self.control(ControlKind::Branch, directive.start_byte(), end, label, children)
    }

    /// Line of the first `finally` statement an `Exit` runs on its way out.
    fn runs_finally(&self, node: Node<'_>) -> Option<usize> {
        let none = HashSet::new();
        self.cfg.blocks_for_stmt(node.byte_range()).into_iter().find_map(|block| {
            let path = self.cfg.path_avoiding(block, self.cfg.exit, &none, &[EdgeKind::ExceptionThrow])?;
            path.iter().find_map(|edge| {
                let target = &self.cfg.graph[edge.to.index()];
                if target.kind != BasicBlockKind::FinallyHandler {
                    return None;
                }
                target.stmts.first().map(|stmt| self.lines.line(stmt.byte_range.start))
            })
        })
    }

    fn simple(&mut self, node: Node<'_>, seq: &mut Seq) {
        self.scan(node, &mut |line, item| match item {
            Item::Call(name) => seq.call(line, name),
            Item::Fact(fact) => seq.push(DigestNode::Fact { line, fact }),
        });
    }

    fn calls_in(&mut self, node: Node<'_>) -> Vec<CallCount> {
        let mut calls = Vec::new();
        self.scan(node, &mut |_, item| {
            if let Item::Call(name) = item {
                add_call(&mut calls, name);
            }
        });
        calls
    }

    /// Pre-order walk of one simple statement or expression.
    fn scan(&mut self, root: Node<'_>, sink: &mut dyn FnMut(usize, Item)) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let line = self.lines.line(node.start_byte());
            match node.kind() {
                "lambda" => {
                    self.stats.lambdas += 1;
                    let calls = node
                        .child_by_field_name("body")
                        .map(|body| self.calls_in(body))
                        .unwrap_or_default();
                    sink(line, Item::Fact(Fact::Lambda { calls }));
                    continue;
                }
                "defProc" => continue,
                "inherited" => {
                    let member = named_children(node).into_iter().find(|child| child.kind() == "identifier");
                    let name = member.map_or_else(
                        || "inherited".to_string(),
                        |member| format!("inherited {}", node_text(member, self.text)),
                    );
                    sink(line, Item::Call(name));
                    continue;
                }
                _ => {}
            }
            if let Some(item) = self.classify(node) {
                sink(line, item);
            }
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
    }

    fn classify(&self, node: Node<'_>) -> Option<Item> {
        let kind = match node.kind() {
            "exprCall" | "statement" => classify_call(node, self.text)?,
            "exprDot" => {
                let parent = node.parent()?;
                if parent.kind() == "exprCall" && parent.child_by_field_name("entity") == Some(node) {
                    return None;
                }
                match classify_call(node, self.text)? {
                    // A dotted name without parentheses is a call only as a
                    // statement; elsewhere it may be a property read (l4d-ftm).
                    CallKind::MethodCall { .. } if parent.kind() != "statement" => return None,
                    other => other,
                }
            }
            _ => return None,
        };
        Some(match kind {
            CallKind::Constructor { class_name } => Item::Fact(Fact::New {
                class: class_name,
                target: assignment_target(node, self.text),
            }),
            CallKind::Free { var_name } | CallKind::Destroy { var_name } | CallKind::FreeAndNil { var_name } => {
                Item::Fact(Fact::Free { var: var_name })
            }
            CallKind::MethodCall { receiver, method } => match lookup_transaction_method(&method).first() {
                Some(entry) => Item::Fact(Fact::Tx { op: entry.op.into(), receiver, method }),
                None => Item::Call(format!("{receiver}.{method}")),
            },
            CallKind::FunctionCall { name } => {
                if is_cast(&name) || ["exit", "break", "continue"].iter().any(|w| name.eq_ignore_ascii_case(w)) {
                    return None;
                }
                Item::Call(name)
            }
        })
    }
}

/// `TFoo(x)` is a cast by Delphi naming convention, not a call.
fn is_cast(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > 1 && bytes[0] == b'T' && bytes[1].is_ascii_uppercase()
}

/// The left-hand side when `node` is the right-hand side of an assignment.
fn assignment_target(node: Node<'_>, text: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    if parent.kind() != "assignment" || parent.child_by_field_name("rhs") != Some(node) {
        return None;
    }
    parent.child_by_field_name("lhs").map(|lhs| target_name(lhs, text))
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p lint4d --test digest_tree_test`
Expected: PASS. If a tree differs, print it with `println!("{}", render::render_tree(...))`, compare against the tree-sitter shape (`npx --no-install tree-sitter parse <file>` in `crates/tree-sitter-pascal`), and fix the builder, not the expectation, unless the expectation contradicts spec §5.

- [ ] **Step 5: Lint and commit**

Run: `cargo clippy -p lint4d --all-targets -- -D warnings && cargo fmt && cargo fmt --check`

```bash
git add crates/lint4d/src/digest crates/lint4d/tests/digest_tree_test.rs
git commit -m "feat(lint4d): build digest control trees from the syntax tree (l4d-7mw.1)"
```

### Task 6: object lifecycle verdicts

**Files:**
- Create: `crates/lint4d/src/digest/lifecycle.rs`
- Modify: `crates/lint4d/src/digest/mod.rs` (`pub mod lifecycle;`, wire `objects` in `digest_routine`)
- Test: `crates/lint4d/tests/digest_lifecycle_test.rs`

**Interfaces:**
- Consumes: Task 2 (`blocks_for_stmt`, `path_avoiding`, `PathEdge`), Task 3 (per-statement exception edges), Task 5 helpers (`field_children`, `named_children`, `is_exit_statement`, `target_name`, `normalize`).
- Produces: `lifecycle::verdicts(scope: Node, cfg: &Cfg, text: &[u8], lines: &LineIndex, context: &lifecycle::Context) -> Vec<ObjectVerdict>`, with `pub struct Context<'a> { pub parse_errors: bool, pub hidden: &'a [Range<usize>] }`. One verdict per assignment whose right-hand side is a constructor, in source order.

- [ ] **Step 1: Write the failing tests**

Create `crates/lint4d/tests/digest_lifecycle_test.rs`. Bodies start at line 4 of the generated unit.

```rust
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
    ObjectVerdict { var: var.into(), class: class.into(), line, verdict }
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
    assert_eq!(objects(body, "Foo"), vec![object("L", "TStringList", 8, Verdict::FreedAllPaths)]);
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
        Verdict::Escapes { via: vec![Escape { line: 10, how: EscapeKind::Exception }], more: false }
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
        Verdict::Escapes { via: vec![Escape { line: 9, how: EscapeKind::Exception }], more: false }
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
    assert!(via.contains(&Escape { line: 10, how: EscapeKind::ReCreated }), "{via:?}");
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
    assert_eq!(verdict(body, "R"), Verdict::Unknown { why: "reassigned at L10".into() });
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
            object("Result", "TList", 8, Verdict::NotFreedHere { why: NotFreedReason::Result }),
            object("FItems", "TList", 9, Verdict::NotFreedHere { why: NotFreedReason::NonLocal }),
            object("L", "TList", 10, Verdict::NotFreedHere { why: NotFreedReason::NeverFreed }),
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
            via.iter().all(|e| e.line == 9 && e.how == EscapeKind::Exception),
            "only the case selector itself may escape: {via:?}"
        ),
        other => panic!("unexpected {other:?}"),
    }
}
```

Run: `cargo test -p lint4d --test digest_lifecycle_test`
Expected: FAIL; `objects` is always empty (left: `[]`).

- [ ] **Step 2: Implement**

Create `crates/lint4d/src/digest/lifecycle.rs`:

```rust
//! Object lifecycle verdicts from CFG paths (spec §6.3).

use std::collections::HashSet;
use std::ops::Range;

use cfg_pascal::calls::{CallKind, classify_call};
use cfg_pascal::cfg_core::{BlockId, Cfg, EdgeKind, PathEdge};
use pascal_core::parser::node_text;
use tree_sitter::Node;

use super::model::{Escape, EscapeKind, NotFreedReason, ObjectVerdict, Verdict};
use super::{LineIndex, field_children, is_exit_statement, named_children, normalize, target_name};

const MAX_ESCAPES: usize = 3;

/// Conditions that make every verdict of a routine unknown or suspect.
pub struct Context<'a> {
    pub parse_errors: bool,
    /// Opaque `{$IF}…{$IFEND}` blocks inside the routine, blanked by the parser rewrite.
    pub hidden: &'a [Range<usize>],
}

pub fn verdicts(
    scope: Node<'_>,
    cfg: &Cfg,
    text: &[u8],
    lines: &LineIndex,
    context: &Context<'_>,
) -> Vec<ObjectVerdict> {
    let body = if scope.kind() == "defProc" { scope.child_by_field_name("body") } else { Some(scope) };
    let Some(body) = body else {
        return Vec::new();
    };
    let facts = Facts::collect(body, text);
    let locals = local_names(scope, &facts, text);
    facts
        .creations
        .iter()
        .map(|creation| ObjectVerdict {
            var: creation.target.clone(),
            class: creation.class.clone(),
            line: lines.line(creation.range.start),
            verdict: verdict(creation, &facts, &locals, cfg, lines, context),
        })
        .collect()
}

struct Creation {
    key: String,
    target: String,
    class: String,
    range: Range<usize>,
}

#[derive(Default)]
struct Facts {
    creations: Vec<Creation>,
    assignments: Vec<(String, Range<usize>)>,
    frees: Vec<(String, Range<usize>)>,
    exits: Vec<Range<usize>>,
    inline_vars: HashSet<String>,
}

impl Facts {
    fn collect(body: Node<'_>, text: &[u8]) -> Self {
        let mut facts = Self::default();
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "lambda" | "defProc" => continue,
                "assignment" => facts.assignment(node, text),
                "statement" if is_exit_statement(node, text) => facts.exits.push(node.byte_range()),
                _ => {}
            }
            if let Some(var) = freed_var(node, text) {
                facts.frees.push((key(&var), node.byte_range()));
            }
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        facts
    }

    fn assignment(&mut self, node: Node<'_>, text: &[u8]) {
        let Some(lhs) = node.child_by_field_name("lhs") else {
            return;
        };
        let target = target_name(lhs, text);
        let key = key(&target);
        if lhs.kind() == "varAssignDef" {
            self.inline_vars.insert(key.clone());
        }
        if let Some(CallKind::Constructor { class_name }) =
            node.child_by_field_name("rhs").and_then(|rhs| classify_call(rhs, text))
        {
            self.creations.push(Creation { key: key.clone(), target, class: class_name, range: node.byte_range() });
        }
        self.assignments.push((key, node.byte_range()));
    }
}

fn key(name: &str) -> String {
    normalize(name).to_ascii_lowercase()
}

fn freed_var(node: Node<'_>, text: &[u8]) -> Option<String> {
    if !matches!(node.kind(), "exprCall" | "exprDot" | "statement") {
        return None;
    }
    let parent = node.parent();
    if node.kind() == "exprDot"
        && parent.is_some_and(|p| p.kind() == "exprCall" && p.child_by_field_name("entity") == Some(node))
    {
        return None;
    }
    match classify_call(node, text)? {
        CallKind::Free { var_name } | CallKind::Destroy { var_name } | CallKind::FreeAndNil { var_name } => {
            Some(var_name)
        }
        _ => None,
    }
}

fn local_names(scope: Node<'_>, facts: &Facts, text: &[u8]) -> HashSet<String> {
    let mut names = facts.inline_vars.clone();
    if scope.kind() == "defProc" {
        for section in field_children(scope, "local") {
            if section.kind() != "declVars" {
                continue;
            }
            for decl in named_children(section).into_iter().filter(|d| d.kind() == "declVar") {
                for name in field_children(decl, "name") {
                    names.insert(key(&node_text(name, text)));
                }
            }
        }
    }
    names
}

fn verdict(
    creation: &Creation,
    facts: &Facts,
    locals: &HashSet<String>,
    cfg: &Cfg,
    lines: &LineIndex,
    context: &Context<'_>,
) -> Verdict {
    if context.parse_errors {
        return Verdict::Unknown { why: "parse errors".to_string() };
    }
    if creation.key == "result" {
        return Verdict::NotFreedHere { why: NotFreedReason::Result };
    }
    if let Some((_, range)) = facts
        .assignments
        .iter()
        .find(|(key, range)| *key == creation.key && *range != creation.range)
    {
        return Verdict::Unknown { why: format!("reassigned at L{}", lines.line(range.start)) };
    }
    let frees: Vec<&Range<usize>> = facts
        .frees
        .iter()
        .filter(|(key, _)| *key == creation.key)
        .map(|(_, range)| range)
        .collect();
    if frees.is_empty() {
        let why = if locals.contains(&creation.key) { NotFreedReason::NeverFreed } else { NotFreedReason::NonLocal };
        return Verdict::NotFreedHere { why };
    }
    if let Some(block) = context.hidden.iter().find(|block| block.start > creation.range.start) {
        return Verdict::Unknown { why: format!("hidden directive block at L{}", lines.line(block.start)) };
    }
    let free_blocks: HashSet<BlockId> = frees
        .iter()
        .flat_map(|range| cfg.blocks_for_stmt((*range).clone()))
        .collect();
    escapes(cfg, creation, &free_blocks, &facts.exits, lines)
}

/// Walk forward from the creation without entering a free; ignore
/// exceptions raised by the creating statement itself (nothing to free yet).
fn escapes(
    cfg: &Cfg,
    creation: &Creation,
    free_blocks: &HashSet<BlockId>,
    exits: &[Range<usize>],
    lines: &LineIndex,
) -> Verdict {
    let mut found: Vec<Escape> = Vec::new();
    for start in cfg.blocks_for_stmt(creation.range.clone()) {
        if cfg.path_avoiding(start, start, free_blocks, &[EdgeKind::ExceptionThrow]).is_some() {
            found.push(Escape { line: lines.line(creation.range.start), how: EscapeKind::ReCreated });
        }
        let mut avoid = free_blocks.clone();
        for _ in 0..=MAX_ESCAPES {
            let Some(path) = cfg.path_avoiding(start, cfg.exit, &avoid, &[EdgeKind::ExceptionThrow]) else {
                break;
            };
            let (block, escape) = escape_point(cfg, &path, exits, lines);
            if !found.contains(&escape) {
                found.push(escape);
            }
            if block == start || !avoid.insert(block) {
                break;
            }
        }
    }
    found.sort();
    found.dedup();
    if found.is_empty() {
        return Verdict::FreedAllPaths;
    }
    let more = found.len() > MAX_ESCAPES;
    found.truncate(MAX_ESCAPES);
    Verdict::Escapes { via: found, more }
}

/// Where `path` leaves for good: the first exception edge, else an `Exit`
/// statement, else the last statement before the routine ends.
fn escape_point(cfg: &Cfg, path: &[PathEdge], exits: &[Range<usize>], lines: &LineIndex) -> (BlockId, Escape) {
    let has_stmts = |block: &BlockId| !cfg.graph[block.index()].stmts.is_empty();
    let last_line = |block: BlockId| {
        cfg.graph[block.index()]
            .stmts
            .last()
            .map_or(0, |stmt| lines.line(stmt.byte_range.start))
    };
    if let Some(index) = path.iter().position(|edge| edge.kind == EdgeKind::ExceptionThrow) {
        if let Some(block) = path[..=index].iter().rev().map(|edge| edge.from).find(has_stmts) {
            return (block, Escape { line: last_line(block), how: EscapeKind::Exception });
        }
    }
    let is_exit = |block: &BlockId| {
        cfg.graph[block.index()].stmts.iter().any(|stmt| {
            exits.iter().any(|exit| exit.start <= stmt.byte_range.start && stmt.byte_range.end <= exit.end)
        })
    };
    if let Some(block) = path.iter().map(|edge| edge.from).find(is_exit) {
        return (block, Escape { line: last_line(block), how: EscapeKind::Exit });
    }
    match path.iter().rev().map(|edge| edge.from).find(has_stmts) {
        Some(block) => (block, Escape { line: last_line(block), how: EscapeKind::FallsOffEnd }),
        None => (
            path[0].from,
            Escape { line: lines.line(cfg.byte_range.end.saturating_sub(1)), how: EscapeKind::FallsOffEnd },
        ),
    }
}
```

In `mod.rs` add `pub mod lifecycle;`, and in `digest_routine`:
1. replace
```rust
    let errors = scope.map(|node| error_lines(node, lines)).unwrap_or_default();
    if !errors.is_empty() {
```
with
```rust
    let errors = scope.map(|node| error_lines(node, lines)).unwrap_or_default();
    let parse_errors = !errors.is_empty();
    if parse_errors {
```
2. after the `degraded.extend(hidden…)` line, add
```rust
    let context = lifecycle::Context { parse_errors, hidden: &hidden };
    let objects = scope
        .map(|node| lifecycle::verdicts(node, cfg, text, lines, &context))
        .unwrap_or_default();
```
3. in the `RoutineDigest { … }` literal, replace `objects: Vec::new(),` with `objects,`.

- [ ] **Step 3: Run the tests**

Run: `cargo test -p lint4d --test digest_lifecycle_test && cargo test -p lint4d --test digest_tree_test`
Expected: PASS. If `raising_call_before_try_escapes_through_outer_except` reports a different line, dump the CFG (`cfg.to_dot()`) and check that Task 3 gives `Db.Open` its own block with an `ExceptionThrow` edge; fix the CFG, not the expectation.

- [ ] **Step 4: Lint and commit**

Run: `cargo clippy -p lint4d --all-targets -- -D warnings && cargo fmt && cargo fmt --check`

```bash
git add crates/lint4d/src/digest crates/lint4d/tests/digest_lifecycle_test.rs
git commit -m "feat(lint4d): object lifecycle verdicts from CFG paths (l4d-7mw.1)"
```

### Task 7: routine selection

**Files:**
- Create: `crates/lint4d/src/digest/select.rs` (with unit tests)
- Modify: `crates/lint4d/src/digest/mod.rs` (`pub mod select;`)

**Interfaces:**
- Produces: `pub enum Selector { Index, All, Routine(String), Line(usize) }`; `pub struct SelectError { pub message: String, pub suggestions: Vec<String> }`; `pub fn select<'a>(file: &'a FileDigest, selector: &Selector) -> Result<Vec<&'a RoutineDigest>, SelectError>`. Suggestions are at most 10, formatted `"{name} (L{start}-{end})"`.

- [ ] **Step 1: Write the failing tests**

Create `crates/lint4d/src/digest/select.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn routine(name: &str, start_line: usize, end_line: usize) -> RoutineDigest {
        RoutineDigest {
            name: name.into(),
            start_line,
            end_line,
            byte_range: 0..0,
            objects: Vec::new(),
            body: Vec::new(),
            degraded: Vec::new(),
        }
    }

    fn file() -> FileDigest {
        FileDigest {
            path: "t.pas".into(),
            routines: vec![
                routine("TFoo.Load", 5, 8),
                routine("TFoo.Load", 10, 13),
                routine("TBar.Save", 15, 30),
                routine("TBar.Save.Inner", 17, 20),
                routine("T.<initialization>", 32, 33),
            ],
        }
    }

    fn names(selected: &[&RoutineDigest]) -> Vec<(String, usize)> {
        selected.iter().map(|r| (r.name.clone(), r.start_line)).collect()
    }

    #[test]
    fn index_and_all_select_everything() {
        assert_eq!(select(&file(), &Selector::Index).unwrap().len(), 5);
        assert_eq!(select(&file(), &Selector::All).unwrap().len(), 5);
    }

    #[test]
    fn qualified_names_match_case_insensitively_with_overloads() {
        let file = file();
        let selected = select(&file, &Selector::Routine("tfoo.load".into())).unwrap();
        assert_eq!(names(&selected), vec![("TFoo.Load".into(), 5), ("TFoo.Load".into(), 10)]);
    }

    #[test]
    fn last_component_is_the_fallback() {
        let file = file();
        for query in ["save", "TBaz.Save"] {
            let selected = select(&file, &Selector::Routine(query.into())).unwrap();
            assert_eq!(names(&selected), vec![("TBar.Save".into(), 15)], "{query}");
        }
    }

    #[test]
    fn line_selects_the_innermost_routine() {
        let file = file();
        assert_eq!(names(&select(&file, &Selector::Line(18)).unwrap()), vec![("TBar.Save.Inner".into(), 17)]);
        assert_eq!(names(&select(&file, &Selector::Line(32)).unwrap()), vec![("T.<initialization>".into(), 32)]);
    }

    #[test]
    fn misses_suggest_the_closest_routines() {
        let error = select(&file(), &Selector::Routine("Lod".into())).unwrap_err();
        assert!(error.message.contains("no routine named 'Lod'"), "{}", error.message);
        assert_eq!(error.suggestions[0], "TFoo.Load (L5-8)");
        let error = select(&file(), &Selector::Line(2)).unwrap_err();
        assert!(error.message.contains("no routine contains line 2"), "{}", error.message);
        assert_eq!(error.suggestions[0], "TFoo.Load (L5-8)");
        assert!(error.suggestions.len() <= 10);
    }
}
```

Add `pub mod select;` to `mod.rs`. Run: `cargo test -p lint4d --lib digest::select`
Expected: compile errors (`select`, `Selector` not found).

- [ ] **Step 2: Implement**

Prepend to `select.rs`:

```rust
//! `--routine` / `--line` selection with suggestions on a miss.

use super::model::{FileDigest, RoutineDigest};

const MAX_SUGGESTIONS: usize = 10;

pub enum Selector {
    Index,
    All,
    Routine(String),
    Line(usize),
}

#[derive(Debug)]
pub struct SelectError {
    pub message: String,
    pub suggestions: Vec<String>,
}

pub fn select<'a>(file: &'a FileDigest, selector: &Selector) -> Result<Vec<&'a RoutineDigest>, SelectError> {
    match selector {
        Selector::Index | Selector::All => Ok(file.routines.iter().collect()),
        Selector::Routine(name) => {
            let exact: Vec<_> = file.routines.iter().filter(|r| r.name.eq_ignore_ascii_case(name)).collect();
            if !exact.is_empty() {
                return Ok(exact);
            }
            let wanted = last_component(name);
            let by_last: Vec<_> = file
                .routines
                .iter()
                .filter(|r| last_component(&r.name).eq_ignore_ascii_case(wanted))
                .collect();
            if !by_last.is_empty() {
                return Ok(by_last);
            }
            Err(SelectError {
                message: format!("no routine named '{name}' in {}", file.path),
                suggestions: closest_by_name(file, name),
            })
        }
        Selector::Line(line) => file
            .routines
            .iter()
            .filter(|r| r.start_line <= *line && *line <= r.end_line)
            .min_by_key(|r| r.end_line - r.start_line)
            .map(|r| vec![r])
            .ok_or_else(|| SelectError {
                message: format!("no routine contains line {line} in {}", file.path),
                suggestions: closest_by_line(file, *line),
            }),
    }
}

fn last_component(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn suggestion(routine: &RoutineDigest) -> String {
    format!("{} (L{}-{})", routine.name, routine.start_line, routine.end_line)
}

fn closest_by_name(file: &FileDigest, name: &str) -> Vec<String> {
    let query = name.to_ascii_lowercase();
    let query_last = last_component(&query).to_string();
    let mut scored: Vec<(usize, &RoutineDigest)> = file
        .routines
        .iter()
        .map(|r| {
            let candidate = r.name.to_ascii_lowercase();
            let distance = levenshtein(&query, &candidate).min(levenshtein(&query_last, last_component(&candidate)));
            (distance, r)
        })
        .collect();
    scored.sort_by_key(|(distance, r)| (*distance, r.start_line));
    scored.into_iter().take(MAX_SUGGESTIONS).map(|(_, r)| suggestion(r)).collect()
}

fn closest_by_line(file: &FileDigest, line: usize) -> Vec<String> {
    let mut scored: Vec<(usize, &RoutineDigest)> = file
        .routines
        .iter()
        .map(|r| (if line < r.start_line { r.start_line - line } else { line.saturating_sub(r.end_line) }, r))
        .collect();
    scored.sort_by_key(|(distance, r)| (*distance, r.start_line));
    scored.into_iter().take(MAX_SUGGESTIONS).map(|(_, r)| suggestion(r)).collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut current = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != *cb);
            current.push(substitution.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        previous = current;
    }
    previous[b.len()]
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p lint4d --lib digest::select`
Expected: PASS.

- [ ] **Step 4: Lint and commit**

Run: `cargo clippy -p lint4d --all-targets -- -D warnings && cargo fmt --check`

```bash
git add crates/lint4d/src/digest
git commit -m "feat(lint4d): select digest routines by name or line (l4d-7mw.1)"
```

### Task 8: `lint4d digest` subcommand

**Files:**
- Modify: `crates/lint4d/src/main.rs` (clap structs ~155-212, `real_main` ~245, every `&Cli` parameter)
- Test: `crates/lint4d/tests/digest_cli_test.rs`

**Interfaces:**
- Consumes: `digest_source`, `render::{render_index, render_routine, render_json, file_name}`, `select::{select, Selector}`.
- Produces: `lint4d digest <FILE> [--routine NAME | --line N | --all] [--format text|json] [--max-lines N]`; every existing `lint4d …` invocation is unchanged.

- [ ] **Step 1: Write the failing CLI tests**

Create `crates/lint4d/tests/digest_cli_test.rs`:

```rust
use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

const DEMO: &str = "unit Demo;
interface
implementation

procedure TFoo.Load(A: Integer);
begin
  Bar;
end;

procedure TFoo.Load(S: string);
begin
  Baz;
end;

procedure TBar.Save;
begin
  Qux;
end;

initialization
  Setup;
end.
";

fn demo() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("demo.pas"), DEMO).unwrap();
    dir
}

fn digest(dir: &TempDir) -> Command {
    let mut command = Command::cargo_bin("lint4d").unwrap();
    command.current_dir(dir.path()).arg("digest").arg("demo.pas");
    command
}

#[test]
fn default_is_the_routine_index() {
    let dir = demo();
    digest(&dir)
        .assert()
        .code(0)
        .stdout(predicate::str::starts_with("demo.pas  4 routines\n"))
        .stdout(predicate::str::contains("TBar.Save"))
        .stdout(predicate::str::contains("Demo.<initialization>"));
}

#[test]
fn routine_matches_every_overload() {
    let dir = demo();
    digest(&dir).args(["--routine", "TFoo.Load"]).assert().code(0).stdout(
        "TFoo.Load  demo.pas:5-8 (4 lines)\nL7 calls Bar\n\nTFoo.Load  demo.pas:10-13 (4 lines)\nL12 calls Baz\n",
    );
}

#[test]
fn routine_falls_back_to_the_last_component() {
    let dir = demo();
    digest(&dir)
        .args(["--routine", "save"])
        .assert()
        .code(0)
        .stdout("TBar.Save  demo.pas:15-18 (4 lines)\nL17 calls Qux\n");
}

#[test]
fn line_selects_one_routine() {
    let dir = demo();
    digest(&dir)
        .args(["--line", "12"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("L12 calls Baz"))
        .stdout(predicate::str::contains("L7 calls Bar").not());
}

#[test]
fn line_in_initialization_selects_section() {
    let dir = demo();
    digest(&dir)
        .args(["--line", "21"])
        .assert()
        .code(0)
        .stdout(predicate::str::starts_with("Demo.<initialization>  demo.pas:20-"))
        .stdout(predicate::str::contains("L21 calls Setup"));
}

#[test]
fn unknown_routine_fails_with_suggestions() {
    let dir = demo();
    digest(&dir)
        .args(["--routine", "Lod"])
        .assert()
        .code(2)
        .stdout("")
        .stderr(predicate::str::contains("no routine named 'Lod'"))
        .stderr(predicate::str::contains("closest routines:"))
        .stderr(predicate::str::contains("TFoo.Load (L5-8)"));
}

#[test]
fn line_outside_routines_fails_with_suggestions() {
    let dir = demo();
    digest(&dir)
        .args(["--line", "2"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no routine contains line 2"))
        .stderr(predicate::str::contains("TFoo.Load (L5-8)"));
}

#[test]
fn json_serializes_the_selected_routines() {
    let dir = demo();
    let output = digest(&dir).args(["--format", "json", "--routine", "TBar.Save"]).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["routines"].as_array().unwrap().len(), 1);
    assert_eq!(json["routines"][0]["name"], "TBar.Save");
}

#[test]
fn unreadable_file_and_bad_arguments_exit_2() {
    let dir = TempDir::new().unwrap();
    let mut command = Command::cargo_bin("lint4d").unwrap();
    command.current_dir(dir.path()).args(["digest", "missing.pas"]);
    command.assert().code(2).stderr(predicate::str::contains("cannot read missing.pas"));
    let dir = demo();
    digest(&dir).args(["--format", "xml"]).assert().code(2);
    digest(&dir).args(["--line", "3", "--all"]).assert().code(2);
}

#[test]
fn a_directory_named_digest_is_still_linted_with_a_path_prefix() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("digest")).unwrap();
    fs::write(dir.path().join("digest/Clean.pas"), "unit Clean;\n\ninterface\n\nimplementation\n\nend.\n").unwrap();
    Command::cargo_bin("lint4d")
        .unwrap()
        .current_dir(dir.path())
        .arg("./digest")
        .assert()
        .code(0);
}
```

If `Clean.pas` produces a lint finding, use the source of `lint_clean_file_exits_zero` in `tests/cli_test.rs` instead.

Run: `cargo test -p lint4d --test digest_cli_test`
Expected: FAIL; `digest` is treated as a lint path.

- [ ] **Step 2: Split the clap structs**

In `crates/lint4d/src/main.rs`:

1. Change `use clap::{CommandFactory, Parser};` to `use clap::{Args, CommandFactory, Parser, Subcommand};`.
2. Replace the three lines

```rust
#[derive(Parser)]
#[command(name = "lint4d", version, about = "An open-source Delphi linter")]
struct Cli {
```
with
```rust
#[derive(Parser)]
#[command(
    name = "lint4d",
    version,
    about = "An open-source Delphi linter",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    lint: LintArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Summarise routines (control flow, object lifetimes, transactions, calls) for coding agents
    Digest(DigestArgs),
}

#[derive(Args)]
struct DigestArgs {
    /// Pascal source file
    file: PathBuf,

    /// Digest routines with this qualified or last-component name
    #[arg(long, conflicts_with_all = ["line", "all"])]
    routine: Option<String>,

    /// Digest the innermost routine containing this 1-based line
    #[arg(long, conflicts_with = "all")]
    line: Option<usize>,

    /// Digest every routine
    #[arg(long)]
    all: bool,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    format: String,

    /// Text lines per routine before the deepest blocks collapse
    #[arg(long, default_value_t = 300)]
    max_lines: usize,
}

/// Arguments of the default lint command.
#[derive(Args)]
struct LintArgs {
```
The existing field list continues unchanged under `LintArgs`.

3. Replace the first line of `real_main`, `let mut cli = Cli::parse();`, with:

```rust
    let cli = Cli::parse();
    if let Some(Command::Digest(args)) = cli.command {
        process::exit(run_digest(&args));
    }
    let mut cli = cli.lint;
```

4. Change every function parameter typed `&Cli` / `&mut Cli` to `&LintArgs` / `&mut LintArgs` (`grep -n '&Cli\|&mut Cli' crates/lint4d/src/main.rs`). Keep `Cli::command()` in `validate_cli`; it renders the full help.

- [ ] **Step 3: Add `run_digest`**

Add to `main.rs` after `real_main`:

```rust
/// `lint4d digest`: exit 0 on success, 2 on any error. Never reads config.
fn run_digest(args: &DigestArgs) -> i32 {
    use lint4d::digest::select::{Selector, select};
    use lint4d::digest::{digest_source, render};

    if args.format != "text" && args.format != "json" {
        eprintln!("lint4d: invalid --format value: '{}' (expected 'text' or 'json')", args.format);
        return EXIT_ERROR;
    }
    let source = match fs::read(&args.file) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("lint4d: cannot read {}: {error}", args.file.display());
            return EXIT_ERROR;
        }
    };
    let file = match digest_source(&args.file, &source) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("lint4d: {error}");
            return EXIT_ERROR;
        }
    };
    let selector = match (&args.routine, args.line) {
        (Some(name), _) => Selector::Routine(name.clone()),
        (None, Some(line)) => Selector::Line(line),
        (None, None) if args.all => Selector::All,
        (None, None) => Selector::Index,
    };
    let chosen = match select(&file, &selector) {
        Ok(chosen) => chosen,
        Err(error) => {
            eprintln!("lint4d: {}", error.message);
            if !error.suggestions.is_empty() {
                eprintln!("closest routines:");
                for suggestion in &error.suggestions {
                    eprintln!("  {suggestion}");
                }
            }
            return EXIT_ERROR;
        }
    };
    let output = if args.format == "json" {
        render::render_json(&file.path, &chosen)
    } else if matches!(selector, Selector::Index) {
        render::render_index(&file)
    } else {
        let name = render::file_name(&file.path);
        let limit = (!matches!(selector, Selector::All)).then_some(args.max_lines);
        chosen
            .iter()
            .map(|routine| render::render_routine(routine, &name, limit))
            .collect::<Vec<_>>()
            .join("\n")
    };
    print!("{output}");
    EXIT_OK
}
```

- [ ] **Step 4: Run the CLI tests and the existing CLI suite**

Run: `cargo test -p lint4d --test digest_cli_test && cargo test -p lint4d --test cli_test --test e2e_test`
Expected: PASS, with no change to `cli_test.rs` or `e2e_test.rs`.

- [ ] **Step 5: Lint and commit**

Run: `cargo clippy -p lint4d --all-targets -- -D warnings && cargo fmt && cargo fmt --check`

```bash
git add crates/lint4d/src/main.rs crates/lint4d/tests/digest_cli_test.rs
git commit -m "feat(lint4d): add the digest subcommand (l4d-7mw.1)"
```

### Task 9: fixture snapshots, collapse and the index invariant

**Files:**
- Create: `crates/lint4d/tests/fixtures/digest/{lifecycle,constructs,degraded,collapse}.pas`
- Create: `crates/lint4d/tests/digest_snapshot_test.rs` and its `tests/snapshots/digest_snapshot_test__*.snap`

**Interfaces:**
- Consumes: `digest_source`, `render::*`, `RoutineDigest::markers`.

- [ ] **Step 1: Write the fixtures**

- `lifecycle.pas`: `unit Lifecycle; interface implementation`, then the `Foo` routine of Task 5's `LIFECYCLE`, the `W` routine of Task 6's `raising_call_before_try_escapes_through_outer_except`, and the `F` function of Task 6's `result_non_local_and_never_freed`, then `end.` (one item per line, as in those tests).
- `constructs.pas`: Task 5's `CONSTRUCTS` source verbatim.
- `collapse.pas` (exactly these 23 lines):

```pascal
unit C;
interface
implementation
procedure Deep(A: Integer);
begin
  Start;
  if A > 0 then
  begin
    One;
    if A > 1 then
    begin
      Two;
      if A > 2 then
      begin
        Three;
        if A > 3 then
          Four;
      end;
    end;
  end;
  Finish;
end;
end.
```

- `degraded.pas`:

```pascal
unit D;
interface
implementation
procedure Hidden;
var
  L: TStringList;
begin
  L := TStringList.Create;
  {$IF DEFINED(X)}
  rappel: developper en 32 bits pour plus de stabilite
  {$IFEND}
  L.Free;
end;
procedure Broken;
begin
  Foo(;
end;
end.
```

- [ ] **Step 2: Write the tests**

Create `crates/lint4d/tests/digest_snapshot_test.rs`:

```rust
use std::fs;
use std::path::{Path, PathBuf};

use lint4d::digest::{Degradation, FileDigest, digest_source, render};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/digest")
}

fn digest(name: &str) -> FileDigest {
    let source = fs::read(fixtures().join(name)).unwrap();
    digest_source(Path::new(name), &source).unwrap()
}

fn all_text(name: &str) -> String {
    let file = digest(name);
    let routines: Vec<String> = file.routines.iter().map(|r| render::render_routine(r, name, None)).collect();
    format!("{}\n{}", render::render_index(&file), routines.join("\n"))
}

#[test]
fn snapshot_lifecycle_text() {
    insta::assert_snapshot!(all_text("lifecycle.pas"));
}

#[test]
fn snapshot_lifecycle_json() {
    let file = digest("lifecycle.pas");
    let routines: Vec<_> = file.routines.iter().collect();
    insta::assert_snapshot!(render::render_json(&file.path, &routines));
}

#[test]
fn snapshot_constructs_text() {
    insta::assert_snapshot!(all_text("constructs.pas"));
}

#[test]
fn degraded_input_is_reported() {
    let text = all_text("degraded.pas");
    assert!(text.contains("! hidden directive block at L9"), "{text}");
    assert!(text.contains("L TStringList L8 unknown (hidden directive block at L9)"), "{text}");
    let file = digest("degraded.pas");
    let broken = file.routines.iter().find(|r| r.name == "Broken").expect("Broken routine");
    assert!(
        broken.degraded.iter().any(|d| matches!(d, Degradation::ParseErrors { .. })),
        "{text}"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn collapse_fits_max_lines() {
    let file = digest("collapse.pas");
    assert_eq!(
        render::render_routine(&file.routines[0], "collapse.pas", Some(8)),
        "Deep  collapse.pas:4-22 (19 lines)
L6 calls Start
L7 if A > 0
  L9 calls One
  L10 if A > 1
    L12 calls Two
    L13-18 if A > 2 … (6 lines, calls Three, Four)
L21 calls Finish
"
    );
}

/// The fact text after `Lnnn `, also behind an inline arm label (`1: L12 …`).
fn fact_text(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let line = match line.find(": L") {
        Some(index) if !line.starts_with('L') => &line[index + 2..],
        _ => line,
    };
    let (number, fact) = line.strip_prefix('L')?.split_once(' ')?;
    number.chars().all(|c| c.is_ascii_digit()).then_some(fact)
}

#[test]
fn index_markers_match_the_full_digest() {
    for entry in fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "pas") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        for routine in &digest(&name).routines {
            let markers = routine.markers();
            let text = render::render_routine(routine, &name, None);
            let facts: Vec<&str> = text.lines().filter_map(fact_text).collect();
            let context = format!("{name} {}:\n{text}", routine.name);
            assert_eq!(markers.raises, facts.iter().filter(|f| f.starts_with("raise ") || **f == "re-raise").count(), "{context}");
            assert_eq!(markers.exits, facts.iter().filter(|f| **f == "exit" || f.starts_with("exit (")).count(), "{context}");
            assert_eq!(markers.tx, facts.iter().any(|f| f.starts_with("tx ")), "{context}");
            assert_eq!(markers.escape, text.contains("NOT freed on all paths"), "{context}");
            let objects = text.lines().find(|l| l.starts_with("objects:")).unwrap_or_default();
            assert_eq!(markers.unknown, objects.contains(" unknown ("), "{context}");
        }
    }
}
```

If tree-sitter's error recovery leaves `Broken` without a CFG, change its body to another single malformed statement (for example `X := ;`) until `digest_source` returns a `Broken` routine with `ParseErrors`, and keep that form.

- [ ] **Step 3: Create and review the snapshots**

Run: `INSTA_UPDATE=always cargo test -p lint4d --test digest_snapshot_test`
Then read every new `crates/lint4d/tests/snapshots/digest_snapshot_test__*.snap` and check by hand:
- lifecycle text: `Foo` objects line `L TStringList L9 freed on all paths`; `W` objects line `L TStringList L… NOT freed on all paths (escapes via L… exception)` naming the `Db.Open` line; `F` has the three `not freed in this routine` reasons; `Foo` body equals Task 5's `LIFECYCLE_TREE`; the index shows `tx raise1 exit1` for `Foo` and `!escape` for `W`.
- lifecycle JSON: every node has `"node"`, every fact has `"kind"`, every verdict has `"status"`; byte ranges are `{"start": …, "end": …}`.
- constructs text: `Outer` body equals Task 5's expected tree, with `! conditional code: 1 {$IF…} regions, all branches shown` and `! anonymous methods not analysed` under its header.
- degraded text: header `!` lines as asserted, and nothing else surprising.

Fix the code, not a snapshot, for any mismatch with spec §5. Then run `cargo test -p lint4d --test digest_snapshot_test` without `INSTA_UPDATE`.
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/lint4d/tests/digest_snapshot_test.rs crates/lint4d/tests/fixtures/digest crates/lint4d/tests/snapshots/digest_snapshot_test__*.snap
git commit -m "test(lint4d): digest fixture snapshots and index invariant (l4d-7mw.1)"
```

### Task 10: agent skill, README, Multidev acceptance, close

**Files:**
- Create: `skills/pascal-digest/SKILL.md`
- Modify: `README.md`

- [ ] **Step 1: Write the skill**

Create `skills/pascal-digest/SKILL.md`:

```markdown
---
name: pascal-digest
description: Use before reading a long Delphi/Object Pascal routine (over ~150 lines), or when asked what a Pascal routine does, whether its objects are freed, or where it starts, commits or rolls back transactions. Produces a compact, line-anchored digest with `lint4d digest`.
---

# Pascal routine digests

`lint4d digest` summarises Pascal routines: control flow with conditions,
objects created and whether they are freed on every path, transactions,
raises, exits and calls, each with its source line.

## When to use it

- Before reading a Pascal routine longer than about 150 lines.
- When you have a `file:line` (grep hit, compiler error, stack trace) inside a long routine.
- To choose which routine of a large unit to read.

## Commands

- `lint4d digest <file>`: routine index with line spans, sizes and markers
  (`tx`, `raiseN`, `exitN`, `!escape` = an object is not freed on every path,
  `?` = an undecided verdict).
- `lint4d digest <file> --line N`: the innermost routine containing line N.
- `lint4d digest <file> --routine NAME`: qualified (`TFoo.Bar`) or last
  component (`Bar`); every overload is shown.
- `--all` digests every routine; `--format json` prints the data model;
  `--max-lines N` (default 300) collapses the deepest blocks to
  `Lstart-end … (N lines, calls …)`.

## Reading a digest

- Read only the source lines the digest points to.
- `NOT freed on all paths (escapes via L… exception)` and `unknown (…)` are
  leads: verify them in the source before calling them bugs.
- `not freed in this routine` describes ownership (returned as `Result`,
  stored in a field); it is not a leak claim.
- Facts are file-local: a call is a name, not the callee's behaviour. Digest
  or read the callee when its effect matters.
- `!` lines warn about parse errors, conditional code (every `{$IF}` branch is
  shown) and anonymous methods (not analysed).

Exit codes: 0 success; 2 error (unreadable file, unknown routine or line; the
closest routines are listed on stderr).
```

- [ ] **Step 2: Document in the README**

In `README.md`, add after the `lint4d --fail-on error src/` example in "Quick Start → lint4d":

~~~markdown
Summarise routines for coding agents (control flow, object lifetimes,
transactions, calls, each with its line):

```sh
lint4d digest MyUnit.pas                      # routine index
lint4d digest MyUnit.pas --line 2417          # routine containing a line
lint4d digest MyUnit.pas --routine TFoo.Save  # by name (all overloads)
```

`digest` is a subcommand; to lint a directory literally named `digest`, write
`lint4d ./digest`. See `skills/pascal-digest/SKILL.md` for the agent skill.
~~~

- [ ] **Step 3: Workspace verification**

Run: `cargo test --workspace --no-fail-fast && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: all pass.

- [ ] **Step 4: Multidev acceptance (manual; the corpus is private)**

```bash
cargo build --release -p lint4d
L=~/gits/lint4d/target/release/lint4d
cd ~/gits/multidev
F1=Common/BO/BusinessObject.pas
F2=Projects/WebImportExport/WebImportExportAPI/WebImportExportExecutePOS.pas
F3=Common/BO/BOConnection.Defaults.pas
for spec in "$F1|TBusinessObject.WriteToDB" "$F1|TBOMap.LoadBySQL" "$F1|TBusinessObject.getInstance" \
            "$F2|WriteTransfer" "$F2|GetProductStock" "$F2|UpdateCustomer" \
            "$F3|TBoConnection.TryBoDbOperation" "$F3|TBoConnection.UnRegisterConnection"; do
  f=${spec%%|*}; p=${spec##*|}
  raw=$("$L" digest "$f" --routine "$p" --format json | python3 -I -c 'import json,sys; r=json.load(sys.stdin)["routines"][0]["byte_range"]; print(r["end"]-r["start"])')
  out=$("$L" digest "$f" --routine "$p" | wc -c)
  echo "$p raw=$raw digest=$out ratio=$(python3 -I -c "print(round($raw/$out, 1))")"
done
```
Expected: ratio ≥ 3 for every routine over 100 lines.

```bash
"$L" digest "$F2" --routine WriteTransfer | grep '^objects:'
"$L" digest "$F1" --routine TBusinessObject.WriteToDB | grep '^objects:'
```
Expected: `existingSkus TStringList L4399 NOT freed on all paths (escapes via L4400 exception…` (L4402 is also acceptable); `params TParams L2417 freed on all paths` and `fieldsMessages TStringList L2455 freed on all paths`.

Robustness over first-party code (run in the background; it takes several minutes):

```bash
git ls-files -z 'Common/*.pas' 'Projects/*.pas' 'Branches/*.pas' > /tmp/digest-files.z
rm -f /tmp/digest-crashes.txt /tmp/digest-errors.txt
start=$(date +%s)
while IFS= read -r -d '' f; do
  "$L" digest "$f" --all > /dev/null 2>> /tmp/digest-errors.txt
  code=$?
  [ "$code" -ne 0 ] && [ "$code" -ne 2 ] && echo "exit $code $f" >> /tmp/digest-crashes.txt
done < /tmp/digest-files.z
echo "elapsed $(( $(date +%s) - start ))s"
wc -l /tmp/digest-crashes.txt /tmp/digest-errors.txt 2>/dev/null
```
Expected: no `/tmp/digest-crashes.txt` (no panics, exit code 101). Investigate every exit-2 line in `/tmp/digest-errors.txt`; file a bug (`--deps discovered-from:l4d-7mw.1`) for each real failure.

- [ ] **Step 5: Commit, record and close**

```bash
git add README.md skills/pascal-digest/SKILL.md
git commit -m "docs: document lint4d digest and add the pascal-digest agent skill (l4d-7mw.1)"
bd update l4d-7mw.1 --acceptance "- [x] Approved design spec committed
- [x] lint4d command produces digests for a file or a named routine, with optional skeleton
- [x] Resource, transaction, raise and exit facts are verified against fixtures, including duplicated finally copies and same-block create/free
- [x] Measured size reduction on fixtures recorded
- [x] Agent skill describing when to request a digest"
bd close l4d-7mw.1 --reason "lint4d digest subcommand (index, --routine, --line, --all, text/json, --max-lines) built on lint4d::digest. Commits: <shas>. Verified: workspace tests/clippy/fmt; Multidev sizes <table>; WriteTransfer escape <line>; WriteToDB freed on all paths; first-party corpus <files> files, 0 panics, <elapsed>."
```

Then use superpowers:finishing-a-development-branch to integrate the branch. Do not push without the user's approval.

