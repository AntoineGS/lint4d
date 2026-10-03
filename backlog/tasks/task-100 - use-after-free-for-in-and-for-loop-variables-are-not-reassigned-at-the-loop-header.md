---
id: TASK-100
title: >-
  use-after-free: for-in and for loop variables are not reassigned at the loop
  header
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 03:06'
updated_date: '2026-10-03 21:55'
labels:
  - lint
  - correctness
milestone: m-3
dependencies:
  - TASK-24
modified_files:
  - crates/lint4d/src/rules/use_after_free.rs
  - crates/lint4d/tests/rules_use_after_free_test.rs
  - >-
    crates/lint4d/tests/fixtures/use_after_free/good_for_in_frees_loop_variable.pas
  - >-
    crates/lint4d/tests/fixtures/use_after_free/bad_use_after_free_in_for_in_body.pas
  - >-
    crates/lint4d/tests/fixtures/use_after_free/good_for_loop_variable_reassigned.pas
  - crates/lint4d/tests/fixtures/use_after_free/good_for_in_inline_var_frees.pas
priority: medium
type: bug
ordinal: 102000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while reviewing TASK-24 (LINT-6). Pre-existing on master 81418f2 and on the TASK-24 branch fix/use-after-free-effects (51ed0c6).

**Repro.**
```pascal
for Item in List do
  Item.Free;
```
reports a use-after-free on the loop header and a double free on the body. This is very common Delphi code.

**Cause.** The for-in header is not modelled as an assignment of the loop variable, so `Item` stays in the freed set along the back edge. The same applies to the `for I := ... to ...` control variable.

**Fix direction.** In `collect_node_effects` (`crates/lint4d/src/rules/use_after_free.rs`, after TASK-24 lands), treat the for-in loop variable and the `for` control variable as an assign at the header statement.

**Tests first.** A fixture in `crates/lint4d/tests/rules_use_after_free_test.rs` with the repro above (no findings) that fails today.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 `for Item in List do Item.Free;` reports neither a use-after-free nor a double free
- [x] #2 A regression test fails on the original code and passes after the fix
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/uaf-loop-variables, branch fix/uaf-loop-variables (stacked on 51ed0c6).
Root cause: for/for-in header is not modelled as assigning the loop variable, so it stays freed along the back edge.
Steps: 1 RED fixtures+tests (for-in free, for-to body, genuine UAF in for-in body still reports). 2 Check how CFG represents for header range; add assign effect for control variable in collect_node_effects. 3 fmt, clippy, lint4d tests.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Finding: the `for I := a to b` control variable was already handled. Its header range covers an `assignment` node (start field), which collect_node_effects already turns into an assign. Only `foreach` was broken: the header range ends at the iterable, so the foreach node is never inside it and its `iterator` field was read as an ordinary identifier.

Ruling: assign effect for foreach is added in statement_effects (new collect_foreach_effects), not collect_node_effects, because that function only sees nodes fully inside the range. The iterator identifier is excluded from reads; the iterable is still read, so `for X in X.Items` after a free reports.

Implemented on branch fix/uaf-loop-variables (worktree .worktrees/uaf-loop-variables), awaiting review and merge.

Fix round 1 (82f3494): extracted `assign_target` helper shared by the assignment and foreach paths. `for var Item in List do Item.Free;` parses to a varAssignDef iterator, so the branch stays; added fixture and test. Mutation check (disabling the branch) makes the test fail.

Merged into master as 7e1bb2a (no-ff), 2026-10-03. Merged master tree is identical to the verified integration tree: cargo fmt --check clean, clippy --workspace --all-targets -D warnings clean, cargo test --workspace 3256 passed / 0 failed / 9 ignored.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: `for Item in List do Item.Free;` reported a use-after-free on the header and a double free in the body, because the for-in variable stayed in the freed set along the back edge.

Change (commits 020fd62, 82f3494 on fix/uaf-loop-variables, stacked on 51ed0c6): the foreach header statement ends at the iterable, so the foreach node was never inside the statement range and its iterator was counted as a read. statement_effects now calls collect_foreach_effects, which records the iterator (plain, or inline `var X`) as an assign and not a read. A shared `assign_target` helper serves both the assignment and foreach paths. The `for I := a to b` control variable already worked via its assignment node.

Tests: four new tests (for-in free, genuine use-after-free in a for-in body still reports once, header reassignment for for-in and for-to, inline `for var`). The first three failed on 51ed0c6; the inline-var test passes there and was checked by mutation. 15/15 in rules_use_after_free_test; `cargo test -p lint4d` green; fmt and clippy -D warnings clean.

Known limits: only plain identifier iterators are tracked (not `Self.Item`).
<!-- SECTION:FINAL_SUMMARY:END -->
