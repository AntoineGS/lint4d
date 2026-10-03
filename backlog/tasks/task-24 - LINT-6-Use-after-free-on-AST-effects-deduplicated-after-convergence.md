---
id: TASK-24
title: 'LINT-6: Use-after-free on AST effects, deduplicated after convergence'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 21:55'
labels:
  - arch-review
  - lint
  - correctness
  - cross-repo
milestone: m-3
dependencies: []
modified_files:
  - crates/lint4d/src/rules/use_after_free.rs
  - crates/lint4d/src/rules/helpers.rs
  - crates/lint4d/tests/rules_use_after_free_test.rs
  - >-
    crates/lint4d/tests/fixtures/use_after_free/bad_use_after_free_with_parens.pas
  - crates/lint4d/tests/fixtures/use_after_free/good_nil_after_free.pas
  - crates/lint4d/tests/fixtures/use_after_free/bad_use_in_loop_body.pas
  - >-
    crates/lint4d/tests/fixtures/use_after_free/good_name_in_string_and_comment.pas
  - crates/lint4d/tests/fixtures/use_after_free/bad_assignment_reads_freed.pas
priority: high
type: bug
ordinal: 24000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-6). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: correctness.

**Problem.** `crates/lint4d/src/rules/use_after_free.rs:98-106`,
`:146-216` (`process_statement`), `:219-256`, `:275-299` re-parse statement
text with string matching: `.Free()` does not match the `.free` suffix
check; assignments return before RHS references are checked; identifier
search can match inside string literals or comments in the range; findings
are emitted during worklist iteration so a revisited block reports twice.
AST helpers for free calls already exist in `rules/helpers.rs:89-129` and
cfg-pascal classifies calls in `../cfg-pascal/src/calls.rs:207-261`.

**Approach.** Compute per-statement `Effects { frees: Vec<Sym>,
assigns: Vec<Sym>, reads: Vec<Sym> }` from the AST once using the helpers;
run the dataflow over effects; collect findings in a set keyed by
(range, symbol); emit after the worklist converges.

**Tests first.** Those three in `crates/lint4d/tests/rules_use_after_free_test.rs`
(first and third fail today).

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Fixtures: `Obj.Free(); Obj.Foo;` reports; `Obj := nil` after free does not; a loop body reports once.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/use-after-free-effects, branch fix/use-after-free-effects (from master 81418f2).

Root cause: crates/lint4d/src/rules/use_after_free.rs re-parses each CFG statement's text with string matching (parse_free_call suffix check misses `.Free()`; parse_assignment returns before the RHS is checked; contains_word matches inside strings/comments) and reports inside the worklist loop, so a block revisited after a state change reports again.

Steps:
1. RED: add regression tests to crates/lint4d/tests/rules_use_after_free_test.rs: `Obj.Free(); Obj.Foo;` reports; `Obj := nil` after free does not; a loop body revisited after a state change reports once; identifier only in a string literal/comment inside the statement range is not a use; assignment whose RHS reads a freed object reports. Record which fail on the original code.
2. Fix: compute per-statement Effects { frees, assigns, reads } once from the analysis tree (node covering the StmtRef byte range, walking only descendants inside the range so if/while/for header spans stay header-only); run the freed-set dataflow over effects; collect findings in a BTreeSet keyed by (range, symbol, kind) and emit after the worklist converges.
3. Keep existing use-after-free tests (fixtures, MainForm integration, CLI/engine prepared-source tests) green.
4. cargo fmt --check, clippy -p lint4d --all-targets -D warnings, lint4d test suites.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED (original code at 81418f2 plus the new tests): `CARGO_BUILD_JOBS=3 cargo test -p lint4d --test rules_use_after_free_test` → 7 passed, 4 failed: with_parens ([] vs 1 finding), assignment_rhs ([] vs 1), loop body (2 identical findings on line 13 vs 1), strings/comments (2 false findings on lines 13, 14). `Obj := nil` after free (good_nil_after_free) already passed on the old code. That records the old behaviour, and the test stays as a regression guard.

Ruling: free targets come from a new `helpers::freed_identifier` that returns the freed identifier node. It mirrors the Free/Destroy/FreeAndNil semantics of cfg-pascal `calls::classify_call` at the pinned rev dd64c75. I did not call `classify_call` because the rule needs the node to exclude the free target from reads, and classify_call only returns text.

Ruling: member names (the identifier on the rhs of an exprDot) and the identifier under `inherited` are not reads, so `Other.Obj` no longer matches a freed local `Obj`. Consequence: `Obj.Free; Self.Obj.Foo` is no longer reported (the old word match caught it).

Ruling: findings are one per (statement range, symbol, kind), as the task specifies, instead of the old one per statement. They are collected across all CFGs of the file in a BTreeSet and emitted after every CFG converges. Diagnostic end columns now come from the statement's byte range instead of the decoded text length, which was wrong for non-ASCII Latin-1 statements.

Ruling: the build ran with the main checkout's gitignored .cargo/config.toml path overrides in effect, because cargo finds that file through the worktree's parent directories. I checked that the sibling checkouts are clean and at exactly the pinned revs (tree-sitter-pascal 22cf861, cfg-core b4131e6, cfg-pascal dd64c75), so the sources built are the pinned ones.

Follow-up created: TASK-99. Nil checks after FreeAndNil are reported as uses; this behaviour predates the change.

Implemented on branch fix/use-after-free-effects (worktree .worktrees/use-after-free-effects), awaiting review and merge.

Merged into master as a989419 (no-ff), 2026-10-03. Merged master tree is identical to the verified integration tree: cargo fmt --check clean, clippy --workspace --all-targets -D warnings clean, cargo test --workspace 3256 passed / 0 failed / 9 ignored.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: the use-after-free rule re-parsed CFG statement text with string matching. It missed `Obj.Free()`, skipped RHS reads in assignments, matched names inside strings and comments, and reported during worklist iteration, so a revisited loop body reported twice.

Change (commit 51ed0c6 on fix/use-after-free-effects): per-statement `Effects { frees, assigns, reads }` are computed once from the analysis tree and cached by statement range. The walk is pruned to the StmtRef byte range, so if/while/for/case/with header spans stay header-only. A new `helpers::freed_identifier` recognises Free/Destroy (bare or called) and FreeAndNil. The dataflow is a union over freed-variable sets. Reads are checked before the statement's own frees and assigns. Findings go into a BTreeSet keyed by (range, symbol, kind) and are emitted after all CFGs converge. The text-parsing helpers and their unit tests were removed.

Tests: five new fixture tests in rules_use_after_free_test.rs (parens free, nil after free, loop body once, string/comment, assignment RHS). Four failed on the original code; nil-after-free already passed and records the old behaviour. All 11 pass now. `cargo test -p lint4d` is all green, including the MainForm integration and the CLI/engine prepared-source tests. `cargo fmt --check` and `cargo clippy -p lint4d --all-targets -D warnings` are clean.

Known limits: `Self.Obj` after `Obj.Free` is no longer reported, because member names are not reads. Nil checks after FreeAndNil still report (follow-up TASK-99). Only plain identifiers are tracked, not fields or dotted paths.
<!-- SECTION:FINAL_SUMMARY:END -->
