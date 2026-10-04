---
id: TASK-104
title: 'fmt4d: uses clause ending in a directive loses its semicolon'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 03:17'
updated_date: '2026-10-04 02:39'
labels:
  - fmt
  - correctness
dependencies: []
modified_files:
  - crates/fmt4d/src/uses.rs
  - crates/fmt4d/tests/fmt_bugs_test.rs
  - crates/fmt4d/tests/fixtures/uses/uses_ends_in_directive.pas
priority: medium
type: bug
ordinal: 106000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while working on TASK-26 (not caused by it).

Repro (default config):
```pascal
unit T;
interface
uses A {$I x.inc};
implementation
end.
```
First pass output:
```pascal
uses
  A,
  {$I x.inc}
```
There is no `;`, so the output no longer parses: the second pass fails with a parse-error diagnostic.

Cause: layout_uses_items in crates/fmt4d/src/uses.rs (formerly format_uses_items) gives the clause's `;` to the last non-separator slot (`last_real_idx`). When that slot is a pinned UsesItem::Directive, emit_uses_item ignores is_last for directives, so no unit gets the `;`. TASK-26 made pinned comments skip that role; directives still take it.

Expected: the `;` goes to the last unit or ifdef block, with any directives anchored after it emitted after the `;` or before it, as long as the result still parses as the same clause. Check that the directive stays inside the clause on re-parse, so the output is idempotent.

Done when: a regression test in fmt_bugs_test.rs formats the repro, the output parses, and the formatter is idempotent on it.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/fmt-uses-fixes, branch fix/fmt-uses-fixes (from master d55c0f6). One commit per task, order TASK-102, TASK-104, TASK-108.

Root cause: layout_uses_items (crates/fmt4d/src/uses.rs) gives the clause ';' to the last non-comment slot; when that is a pinned Directive, emit_uses_item ignores it, so no ';' is written.

Steps:
1. RED: fmt_bugs_test regression for 'uses A {$I x.inc};' (output parses without diagnostics, directive kept once and inside the clause, idempotent) and a fixture under crates/fmt4d/tests/fixtures/uses/ for the oracle sweep.
2. The ';' goes to the last unit or block; when directives follow it, it is written after the last of them (as in the source: 'A' / '{$I x.inc};'), so the directive stays inside the clause on re-parse and its order relative to units is kept (an include may itself list units).
3. Full fmt4d tests, fmt, clippy.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED: `CARGO_BUILD_JOBS=2 cargo test -p fmt4d --test fmt_bugs_test -- uses_clause_ending` -> uses_clause_ending_in_a_directive_keeps_its_semicolon FAILED with output `uses` / `  A,` / `  {$I x.inc}` (no `;`). `cargo test -p fmt4d --test fmt_roundtrip_test -- every_fixture` with the new fixture crates/fmt4d/tests/fixtures/uses/uses_ends_in_directive.pas -> `child count changed: 7 -> 6, unit at 1:1` (the output no longer parses).

Ruling: the `;` is written after the last directive that follows the clause's last unit/block (`A` / `{$I x.inc};`), not after the unit with the directive moved before or after it — this mirrors the source, keeps the directive inside the clause on re-parse (idempotent), and keeps its order relative to the units (an {$I} may itself list units). Comments pinned after that directive go on its `;` line so they stay inside the clause too (found by the fixture: on its own line after the `;` the comment left the clause and the second pass differed).

Change: list_ends (uses.rs) assigns each item's punctuation from its role (unit, block, directive, other) for both the top level and every branch; emit_list emits a list, layout_uses_items converts its slots into one. The uses.rs unit test format_items_directive_between_units_follows_anchor asserted only positions of the buggy output (`SysUtils,` then `{$I myinc.inc}`, no `;`); it now asserts the exact fixed output.

GREEN: full `cargo test -p fmt4d` green (lib 112, fmt_bugs_test 145, fmt_roundtrip_test 26 incl. the new fixture); clippy -p fmt4d --all-targets -D warnings and fmt --check clean. Commit 2532a32.

Follow-up filed: TASK-114 (pre-existing, confirmed against master's uses.rs): a directive on the same line after the clause's final `;` or {$ENDIF} is dropped.

Implemented on branch fix/fmt-uses-fixes (worktree .worktrees/fmt-uses-fixes), awaiting review and merge.

Merged into master as c492d6a (branch fix/fmt-uses-fixes, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: a uses clause whose last item was a pinned directive (`uses A {$I x.inc};`) lost its `;`: layout_uses_items gave the clause terminator to the last non-comment slot, and a directive does not write one. The output no longer parsed.

Change (commit 2532a32, branch fix/fmt-uses-fixes, crates/fmt4d/src/uses.rs): punctuation is now assigned per list by list_ends: the last unit or block takes the list's end, earlier units/blocks a `,`; when directives follow that last item, it gets nothing and the last directive carries the `;` (comments pinned after it stay on its line). The top level and every branch go through one emit_list. Output for the repro: `uses` / `  A` / `  {$I x.inc};`.

Tests: fmt_bugs_test::uses_clause_ending_in_a_directive_keeps_its_semicolon (exact output, parses without diagnostics, idempotent) and fixture crates/fmt4d/tests/fixtures/uses/uses_ends_in_directive.pas (oracle + idempotency sweep, includes a sorted clause ending in a directive and a comment after it) both failed before and pass now. Updated unit test format_items_directive_between_units_follows_anchor (it asserted the buggy output). Full cargo test -p fmt4d green, clippy -D warnings and fmt --check clean.

Known limits: a directive on the same line after the clause's final `;` is still dropped (pre-existing, TASK-114).
<!-- SECTION:FINAL_SUMMARY:END -->
