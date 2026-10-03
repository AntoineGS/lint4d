---
id: TASK-90
title: Remove stale bare raise; ERROR suppression (is_bare_raise_error)
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:28'
updated_date: '2026-10-03 21:55'
labels:
  - agent-todos
  - rtl-units
  - core
dependencies: []
modified_files:
  - crates/pascal-core/src/parser.rs
priority: low
type: chore
ordinal: 92000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Delphi RTL Units (Classes.pas)".

Section context: Classes.pas from the D2010 installation reported "include expansion was incomplete; lint diagnostics were withheld" and navigation inside it was unavailable.

Original item:

lint4d's bare `raise;` ERROR suppression (`is_bare_raise_error` in
`pascal-core/src/parser.rs`) is stale: the grammar parses bare
`raise;` without errors.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/uri-at-sign, branch fix/uri-at-sign (second commit, after TASK-91).
1. Prove the claim: test parsing bare raise; in several contexts (procedure body, except block, on-handler, nested begin/end without semicolon, if/else, case arm, initialization section) against the pinned grammar and assert no ERROR/MISSING node anywhere.
2. If it holds, delete is_bare_raise_error and its call sites in crates/pascal-core/src/parser.rs (visit_node, has_real_error), keeping the test as regression guard; otherwise narrow the suppression to the failing context and record a ruling.
3. fmt, clippy -p pascal-core, pascal-core tests.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Claim verified against the pinned grammar (tree-sitter-pascal 22cf861, sibling checkout at that rev and clean): bare raise parses as a (raise) node with no ERROR/MISSING in procedure bodies, with and without trailing semicolon, in except blocks, on-handlers, except-else, nested begin/end without semicolons, if/else, case arms and else, while, after try..finally, and in an initialization section. No narrowing needed.

Ruling: kept has_real_error's explicit ERROR/MISSING walk (minus the raise branch) rather than switching to Node::has_error, so the change only removes the suppression.

Follow-up: TASK-106 — lint4d's raise-in-destructor rule has the same dead ERROR/kRaise branch (crates/lint4d/src/rules/exception.rs:378-382, 404); out of scope for this pascal-core chore.

Implemented on branch fix/uri-at-sign (worktree .worktrees/uri-at-sign), awaiting review and merge.

Merged into master as 82f5767 (no-ff), 2026-10-03. Merged master tree is identical to the verified integration tree: cargo fmt --check clean, clippy --workspace --all-targets -D warnings clean, cargo test --workspace 3256 passed / 0 failed / 9 ignored.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: pascal-core suppressed ERROR nodes consisting of a lone kRaise (is_bare_raise_error) because an older grammar could not parse bare `raise;`; the pinned grammar parses it, so the workaround was dead code that could also hide a genuine error shaped like that.

Change (commit d6f11a7 on fix/uri-at-sign): removed is_bare_raise_error and its two call sites (visit_node, has_real_error) in crates/pascal-core/src/parser.rs. Replaced has_real_error_false_for_bare_raise_only with grammar_parses_bare_raise_without_error_nodes, which parses bare raise in 12 contexts and asserts no ERROR/MISSING node, a (raise) node in the tree, and no diagnostics or patches from parse_file_with_patches — the regression guard if a grammar bump regresses.

Verification: cargo test -p pascal-core (all suites pass, 102 lib tests), cargo test -p lint4d (all pass), cargo clippy -p pascal-core --all-targets -D warnings clean, cargo fmt --check clean. Not RED-first: this is a chore removing dead code; the new test passed on the old code too (it proves the premise).

Follow-up: TASK-106 (same dead branch in lint4d's raise-in-destructor rule).
<!-- SECTION:FINAL_SUMMARY:END -->
