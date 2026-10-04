---
id: TASK-108
title: 'fmt4d: uses sorting that leaves an {$IFDEF} block last gives a dangling comma'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 03:28'
updated_date: '2026-10-04 02:39'
labels:
  - fmt
  - correctness
dependencies: []
modified_files:
  - crates/fmt4d/src/uses.rs
  - crates/fmt4d/tests/fmt_bugs_test.rs
  - crates/fmt4d/tests/fmt_roundtrip_test.rs
  - crates/fmt4d/tests/fixtures/uses/uses_block_left_last.pas
  - crates/fmt4d/tests/fixtures/uses/uses_only_conditional.pas
  - crates/fmt4d/tests/fixtures/uses/uses_nested_conditionals.pas
  - crates/fmt4d/tests/fixtures/uses/uses_trailing_block_in_branch.pas
  - crates/fmt4d/tests/fixtures/uses/uses_comma_first.pas
priority: high
type: bug
ordinal: 110000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the strengthened TASK-63 round-trip oracle (sanity test oracle_rejects_punctuation_that_breaks_a_configuration in crates/fmt4d/tests/fmt_roundtrip_test.rs uses this shape). This bug predates the TASK-26 branch.

Repro (default config, uses sorting on):
```pascal
unit T;
interface
uses B, {$IFDEF X} C, {$ENDIF} A;
implementation
end.
```
Output:
```pascal
uses
  A,
  B,
  {$IFDEF X}
  C,
  {$ENDIF};
```
The block is pinned after its anchor B. Once A sorts first, the block is the last slot and takes the `;` after `{$ENDIF}`, while the units inside it, and B, keep commas. With X defined the clause reads `uses A, B, C, ;`, without X `uses A, B, ;`, so neither compiles. The second pass formats it the same way, so idempotency tests do not notice.

Cause: layout_uses_items / emit_ifdef_block in crates/fmt4d/src/uses.rs. The clause terminator goes to the last non-separator slot, and units inside a block always get commas. This is related to TASK-102 (`;` inside each branch) and TASK-104 (last slot is a directive): all three come from the same terminator logic.

Expected: every configuration of the conditional blocks still reads `unit (, unit)* ;`. For example, keep the last plain unit as the terminator (`A, B, {$IFDEF X} C, {$ENDIF} D;`), or place a block that has no terminator inside it before the last unit.

Done when: a regression test in fmt_bugs_test.rs formats the repro into a clause that is valid in both configurations, and the round-trip oracle accepts it.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/fmt-uses-fixes, branch fix/fmt-uses-fixes (from master d55c0f6). One commit per task, order TASK-102, TASK-104, TASK-108.

Root cause: layout_uses_items pins a block after its anchor unit; after sorting the block can be the last slot, and units inside a block always get ',' while the block's {$ENDIF} gets ';'. Every configuration where the block contributes units then reads 'A, B, C, ;'.

Steps:
1. RED: fmt_bugs_test regression for 'uses B, {$IFDEF X} C, {$ENDIF} A;' (every configuration reads unit (, unit)* ; checked via the parse with X defined and undefined equivalents, idempotent) and a fixture under crates/fmt4d/tests/fixtures/uses/ for the oracle sweep.
2. When a block (not ending in its own ';') ends up after the last plain unit, move the pinned items up to that block to just before the last plain unit, so a plain unit terminates the clause. A clause with no plain unit at all ends its last block 'open' (no ',' after each branch's last unit, ';' after {$ENDIF}).
3. Full fmt4d tests, fmt, clippy.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED: `CARGO_BUILD_JOBS=2 cargo test -p fmt4d --test fmt_bugs_test -- uses_sorting_never uses_clause_of_only` -> 2 FAILED: the repro gave `A,` / `B,` / `{$IFDEF X}` / `C,` / `{$ENDIF};`; `uses {$IFDEF X} A {$ELSE} B, C {$ENDIF};` gave `A,` ... `C,` / `{$ENDIF};`. Sweep with new fixtures: uses_block_left_last.pas `per configuration ["U,U,U;", "U,U;"] -> ["U,U,U,;", "U,U,;"]`, uses_only_conditional.pas `["U;", "U,U;"] -> ["U,;", "U,U,;"]`.

Ruling: when blocks end up after the last plain unit, the pinned items up to the last such block move in front of that unit (the task's second option); group separators of sections left empty are dropped. Chosen over comma-first output (`A {$IFDEF X}, C{$ENDIF};`) because it keeps the one-unit-per-line layout and is stable on the second pass (the block's new anchor is the unit before it). [Superseded in fix round 1: the move now only happens with sorting on; without sorting such blocks are written comma-first.]

Ruling: a clause with no plain unit at all (only blocks) ends its last block 'open': no ',' after each branch's last unit and ';' after {$ENDIF} (as Delphi code like `uses {$IFDEF X} A {$ELSE} B {$ENDIF};` writes it). The same open form is used for a plain block that is the last item of a terminated branch.

Changed unit tests that asserted the buggy output: format_items_ifdef_block_follows_anchor asserted `{$ENDIF};` after a block with `SpecialUnit,` inside; renamed format_items_ifdef_block_left_last_moves_before_last_unit, it asserts the fixed output. ifdef_mixed_sections_stays_pinned and ifdef_no_section_placement_without_grouping get a unit after the block so they still test anchor pinning. New unit test format_items_block_left_last_leaves_its_empty_section (grouping, Project-only block after Core units).

GREEN: full `cargo test -p fmt4d` green (lib 113, fmt_bugs_test 147, fmt_roundtrip_test 26 incl. fixtures uses_block_left_last.pas, uses_only_conditional.pas, uses_nested_conditionals.pas); clippy -p fmt4d --all-targets -D warnings and fmt --check clean. Manual probes (several trailing blocks with a comment, nested blocks, directive after a block, block inside a terminated branch) all parse and are idempotent. Commit 02a46e9 (first committed as 7b4aab9, amended before review).

Implemented on branch fix/fmt-uses-fixes (worktree .worktrees/fmt-uses-fixes), awaiting review and merge.

Commit amended (comment wording only: the move also covers a source written comma-first, and format_uses_items' doc mentions it): 7b4aab9 -> 02a46e9. Re-verified: fmt --check, clippy -D warnings clean, cargo test -p fmt4d 468 passed / 0 failed.

Fix round 1 (review "Needs fixes"):
- RED: `CARGO_BUILD_JOBS=2 cargo test -p fmt4d --test fmt_bugs_test -- uses_branch_ending uses_trailing_block` -> 2 FAILED. `uses {$IFDEF X} A {$IFDEF Y}, C {$ENDIF}; {$ELSE} D; {$ENDIF}` gave `A,` / `{$IFDEF Y}` / `C` / `{$ENDIF};` (X without Y reads `uses A, ;`); with sort = false `uses C, A {$IFDEF X}, B{$ENDIF};` gave `C,` / block / `A;` (A and B swapped). Sweeps: every_fixture_round_trips failed on the new uses_trailing_block_in_branch.pas (`["U,U;", "U;", "U;"] -> ["U,U;", "U,;", "U;"]`); the new every_uses_fixture_round_trips_with_sorting_off failed on uses_comma_first.pas, uses_only_conditional.pas and uses_trailing_block_in_branch.pas (node kind changed moduleName -> ppUsesBlock = units moved; child count changed).
- Change (uses.rs): list_ends became list_puncts, returning Punct { lead, end }. In any list that does not continue past its end (the clause's top level, a terminated branch, an open branch), when blocks follow the last unit, that unit gets nothing and the blocks are written comma-first (`, C` for every unit inside, nested blocks too), the last one taking the list's end. Branch order is never changed. The top-level move step now only runs when config.sort is on; without sorting the top level uses the same comma-first form, so no unit moves.
- Ruling: comma-first lines are written `  , C` (indent, comma, space, name), one unit per line like the rest of the clause. Re-parse ignores comma positions, so the layout is stable.
- New tests: fmt_bugs_test::uses_branch_ending_in_a_block_writes_it_comma_first, ::uses_trailing_block_keeps_its_place_with_sorting_off; fixtures uses_trailing_block_in_branch.pas, uses_comma_first.pas; fmt_roundtrip_test::every_uses_fixture_round_trips_with_sorting_off (oracle with exact uses comparison + idempotency, sort = false, over crates/fmt4d/tests/fixtures/uses/).
- GREEN: cargo test -p fmt4d 471 passed / 0 failed; fmt --check, clippy -p fmt4d --all-targets -D warnings clean. Reviewer harness /tmp/fmtrev-u/probe.py (copy in /tmp/fmtu-fix pointing at this worktree's build), all case files, sort on/off/grouping: the only PROBLEM entries left are sources the parser rejects (`in 'file'` clauses, `Z, {$IFDEF X} A, B; ...`, terminated-in-terminated), sources already invalid in some configuration, TASK-114 (directive after the final ';' dropped) and TASK-128 (comma before a trailing directive dropped).
- Commit 4e07a97. Follow-up filed: TASK-128.
- Implemented on branch fix/fmt-uses-fixes (worktree .worktrees/fmt-uses-fixes), awaiting review and merge.

Merged into master as c492d6a (branch fix/fmt-uses-fixes, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: with uses sorting, an {$IFDEF} block pinned after its anchor could become the clause's last item; units inside a block always got ',' and the block's {$ENDIF} got the ';', so every configuration read like `uses A, B, C, ;`. A clause made only of a block (`uses {$IFDEF X} A {$ELSE} B {$ENDIF};`) was broken the same way.

Change (commit 02a46e9, branch fix/fmt-uses-fixes, crates/fmt4d/src/uses.rs):
- layout_uses_items: when blocks end up after the last plain unit, the pinned items up to the last such block move in front of that unit (empty sections' separators dropped), so a plain unit ends the clause. Repro output: `A,` / `{$IFDEF X}` / `C,` / `{$ENDIF}` / `B;`.
- emit_ifdef_block: a non-terminated block that ends the clause (only possible when there is no plain unit) is written open: each branch's last unit has no punctuation and ';' follows {$ENDIF}.

Tests: fmt_bugs_test::uses_sorting_never_leaves_a_conditional_block_last and ::uses_clause_of_only_a_conditional_block_ends_after_endif (exact output, parse without diagnostics, idempotent); fixtures crates/fmt4d/tests/fixtures/uses/uses_block_left_last.pas, uses_only_conditional.pas, uses_nested_conditionals.pas in the oracle sweep. All failed before (RED in notes) and pass now. Three uses.rs unit tests that asserted the buggy shape were updated, one added. Full cargo test -p fmt4d green, clippy -D warnings and fmt --check clean.

Fix round 1 (commit 4e07a97): the rule that a unit ends the list now applies to every branch too: blocks after a list's last unit are written comma-first (`A` / `{$IFDEF Y}` / `, C` / `{$ENDIF};`), so `uses {$IFDEF X} A {$IFDEF Y}, C {$ENDIF}; {$ELSE} D; {$ENDIF}` no longer reads `uses A, ;` with X but not Y. The move in front of the last unit only happens with sorting on; with sort = false the top level is written comma-first and no unit moves (this also resolves TASK-115). New tests: fmt_bugs_test::uses_branch_ending_in_a_block_writes_it_comma_first, ::uses_trailing_block_keeps_its_place_with_sorting_off; fixtures uses_trailing_block_in_branch.pas, uses_comma_first.pas; fmt_roundtrip_test::every_uses_fixture_round_trips_with_sorting_off. cargo test -p fmt4d 471 passed, clippy/fmt clean, reviewer probe harness clean apart from known/unrelated cases.

Known limits: with sorting on, moved blocks lose their section's visual grouping (they sit just before the last unit). A ',' between the last unit and a trailing {$I} is dropped (TASK-128). A directive after the final ';' is dropped (TASK-114).
<!-- SECTION:FINAL_SUMMARY:END -->
