---
id: TASK-115
title: >-
  fmt4d: with uses sorting off, a trailing {$IFDEF} block is moved before the
  last unit
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 22:22'
updated_date: '2026-10-04 02:39'
labels:
  - fmt
dependencies:
  - TASK-108
modified_files:
  - crates/fmt4d/src/uses.rs
  - crates/fmt4d/tests/fmt_bugs_test.rs
  - crates/fmt4d/tests/fmt_roundtrip_test.rs
  - crates/fmt4d/tests/fixtures/uses/uses_comma_first.pas
priority: low
ordinal: 117000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-up from TASK-108 (behaviour introduced deliberately there; recorded as a ruling).

To keep every configuration of a uses clause valid, layout_uses_items (crates/fmt4d/src/uses.rs) moves conditional blocks that end up after the last plain unit in front of that unit, so a plain unit carries the clause's `;`. With sorting on (the default) this is just another reordering. With `[uses] sort = false` it still applies, so the comma-first idiom

```pascal
uses A {$IFDEF X}, B{$ENDIF};
```
becomes
```pascal
uses
  {$IFDEF X}
  B,
  {$ENDIF}
  A;
```
which is valid in both configurations but swaps A and B (initialization order, identifier resolution) although the user turned sorting off.

Option: when sorting is off, keep the source order and write trailing blocks comma-first (`A` / `{$IFDEF X}` / `, B` / `{$ENDIF};`), which the grammar accepts (ppUsesBlock allows ',' anywhere). Decide whether that layout is acceptable; add a regression test with sort = false either way.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fixed in TASK-108 fix round 1 (commit 4e07a97, branch fix/fmt-uses-fixes): the move in front of the last unit (layout_uses_items) now only runs with config.sort on. With sort = false, blocks after the last unit stay in place and are written comma-first by list_puncts: `uses C, A {$IFDEF X}, B{$ENDIF};` -> `C,` / `A` / `{$IFDEF X}` / `, B` / `{$ENDIF};`. Decision on the layout: comma-first lines are `  , B`, one unit per line. Regression tests: fmt_bugs_test::uses_trailing_block_keeps_its_place_with_sorting_off (also checks the TASK-108 repro keeps source order with sort off) and fmt_roundtrip_test::every_uses_fixture_round_trips_with_sorting_off (exact uses comparison + idempotency over crates/fmt4d/tests/fixtures/uses/, including uses_comma_first.pas). RED/GREEN recorded on TASK-108. Remaining note: with grouping on and sorting off, section placement of an all-one-section block still moves it within the clause (pre-existing grouping behaviour, not this task). No acceptance criteria to check.

Implemented on branch fix/fmt-uses-fixes (worktree .worktrees/fmt-uses-fixes), awaiting review and merge.

Merged into master as c492d6a (branch fix/fmt-uses-fixes, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: TASK-108's fix moved a trailing {$IFDEF} block in front of the last unit even with uses sorting off, reordering units (initialization order, identifier resolution) the user had asked to keep.

Change (commit 4e07a97, branch fix/fmt-uses-fixes, crates/fmt4d/src/uses.rs): the move only runs with sorting on; without sorting such blocks keep their place and are written comma-first (`A` / `{$IFDEF X}` / `, B` / `{$ENDIF};`), valid in every configuration.

Tests: fmt_bugs_test::uses_trailing_block_keeps_its_place_with_sorting_off, fmt_roundtrip_test::every_uses_fixture_round_trips_with_sorting_off; cargo test -p fmt4d 471 passed, fmt/clippy clean.

Known limits: grouping (independent of sorting) still places all-one-section blocks at their section's end.
<!-- SECTION:FINAL_SUMMARY:END -->
