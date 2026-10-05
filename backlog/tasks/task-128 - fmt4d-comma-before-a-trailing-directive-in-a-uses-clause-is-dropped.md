---
id: TASK-128
title: 'fmt4d: comma before a trailing directive in a uses clause is dropped'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 23:40'
updated_date: '2026-10-05 00:50'
labels:
  - fmt
  - correctness
dependencies:
  - TASK-104
modified_files:
  - crates/fmt4d/src/uses.rs
  - crates/fmt4d/tests/fmt_bugs_test.rs
  - crates/fmt4d/tests/fixtures/uses/uses_comma_before_directive.pas
priority: medium
ordinal: 130000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in review of branch fix/fmt-uses-fixes (TASK-104 fix). Before that branch the output was broken differently (the clause lost its ';').

Repro (default config):
```pascal
unit T;
interface
uses A, {$I units.inc};
implementation
end.
```
Output:
```pascal
uses
  A
  {$I units.inc};
```
If units.inc contains `B`, the source reads `uses A, B;` but the output reads `uses A B;`, which does not compile. The include is opaque to the formatter, so the source's punctuation around it carries meaning.

Cause: list_puncts in crates/fmt4d/src/uses.rs (the TASK-104 rule): when directives follow the clause's last unit, that unit gets no punctuation and the last directive carries the ';'. extract_uses_items does not record whether a ',' sat between the unit and the directive (commas are only read for their comments).

Expected: a unit followed by ',' and then an include in the source keeps its ',' (`A,` / `{$I units.inc};`), while `uses A {$I x.inc};` keeps the TASK-104 output. Probably needs the comma position recorded on the item (and care under sorting: a unit that moved away from its directive cannot keep its comma). Regression test in fmt_bugs_test.rs; the round-trip oracle cannot see inside the include, so assert the output shape directly.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Branch fix/fmt-punctuation-trivia, own commit (after TASK-114, same file).

Root cause (confirmed): list_puncts (uses.rs) gives the clause's last unit no punctuation when directives follow it and moves the ';' to the last directive (TASK-104 rule); extract_uses_items does not record whether a ',' sat between the unit and the directive.

Fix: UsesItem::Directive records after_comma (a ',' came right before it in the source, i.e. since the previous unit, block or directive). In list_puncts, an item with no punctuation that is followed (comments and group separators skipped) by an after_comma directive gets a ','; directives can now end with ',' too (emit_uses_item writes the end). Pinned directives stay right after their anchor, so the item before one is its source predecessor (or a unit that already has a ','); 'uses A {$I x.inc};' keeps the TASK-104 output.

Out of scope (follow-up task): a directive followed by a unit in the output ('A, {$I u}, B;' drops the ',' after the directive; '{$I u}, B;' and 'A {$I u}, B;' likewise; sorting 'B, A, {$I u};' puts B after the include without a ',').

Tests first: fmt_bugs_test.rs regressions asserting the output shape ('A,' / '{$I units.inc};'; two directives 'A,' / '{$I a.inc},' / '{$I b.inc};'; 'A {$I x.inc};' unchanged), parse and idempotency, sorting on and off; a uses fixture for the oracle.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED (master src): comma_before_a_directive_ending_a_uses_list_is_kept failed ('uses A, {$I units.inc};' -> 'A' / '{$I units.inc};'); every_fixture_round_trips and every_uses_fixture_round_trips_with_sorting_off failed on the new fixture ('units and punctuation per configuration ["U,U,;"] -> ["U,U;"]', 'child count changed: 6 -> 5').
Change: uses.rs UsesItem::Directive is now { text, after_comma } (UsesItem::directive() builds one without), set in extract_uses_items and parse_pp_uses_block from the last non-comment child; ItemRole::Directive carries it; restore_commas_before_directives gives a ',' to the unit, block or directive right before such a directive (comments and group separators skipped) when it has no punctuation, on every list_puncts path; emit_uses_item writes a directive's end. Also covers a ',' between two directives ('A,' / '{$I a.inc},' / '{$I b.inc};') and inside a branch ('{$IFDEF X} A, {$I u.inc} {$ENDIF};').
GREEN: cargo test -p fmt4d 507 passed, 0 failed; clippy -p fmt4d --all-targets -D warnings and fmt --check clean. Corpus gate (347 files, plain and aligned, base master 9322cce): only the two new uses fixtures differ (TASK-114 and TASK-128), 0 non-idempotent.
Branch verification (all three commits): cargo fmt --check clean; cargo clippy --workspace --all-targets -D warnings clean; cargo test --workspace 3609 passed / 0 failed / 9 ignored; pascal-lsp protocol_barriers (test-support) 913/916: the two TASK-1.2 timing tests (cancel_/shutdown_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline) and sixty_four_project_metadata_changes_share_the_notification_budget_and_stale_pull_results (receive timeout under full-suite load; passes 3/3 alone). None touch fmt4d.
Follow-up filed: TASK-137 for the ',' after a directive followed by a unit, and sorting moving a unit after a trailing include (out of scope here).

Review round 1 (code-reviewer subagent on 9322cce..bf881df; TASK-112 and TASK-114 judged sound): in a comma-first list the ',' before a directive was still lost. list_puncts returned early for 'lead' lists, and the restore rule only handled a predecessor with no punctuation, not one written comma-first. 'uses A {$IFDEF X}, B, {$I u.inc} {$ENDIF};' (sorting off) gave '{$IFDEF X}' / ', B' / '{$I u.inc}', which reads 'A, B U' with X defined.
RED: comma_before_a_directive_in_a_comma_first_list_is_kept failed ('{$I u.inc}' without the ',').
Fix: restore_commas_before_directives(roles, puncts, comma_first) writes the directive comma-first (', {$I u.inc}') when it has no item before it, when that item is written comma-first, or when the whole list is; the item before it gets the ',' otherwise; an item that already ends in punctuation is left alone. Directives honour Punct.lead (Punct::lead_text, also used by emit_unit).
GREEN: cargo test -p fmt4d 508 passed; cargo test --workspace 3610 passed / 0 failed / 9 ignored; clippy --workspace -D warnings and fmt --check clean; corpus gate 347 files, only the two new uses fixtures differ, 0 non-idempotent.
Declined from the review, filed under TASK-137: with sorting on, the same clause moves the block in front of A, and the include then precedes A without a ',' (directive followed by a unit).

Merged into master by fast-forward to a6850d2 (branch fix/fmt-punctuation-trivia, 2026-10-04). Merged tree verified: cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean; cargo test --workspace 3610 passed / 0 failed / 9 ignored.

Regression found during TASK-137: restore_commas_before_directives applied to every directive, so with sorting 'uses B, {$R+} A;' became 'A,' / 'B,' / '{$R+};' (reads 'uses A, B, ;'). Fixed on branch fix/fmt-uses-include-punctuation by TASK-137, which replaces the restore rule: clauses holding an include keep the source's punctuation, and other directives never move punctuation. Regression test: uses_sorting_around_other_directives_keeps_the_clause_valid.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: with directives after a uses list's last unit, the unit lost its punctuation (TASK-104 rule) even when the source had a ',' there: 'uses A, {$I units.inc};' became 'A' / '{$I units.inc};', which reads 'uses A B;' when units.inc lists B.

Change (branch fix/fmt-punctuation-trivia, crates/fmt4d/src/uses.rs): a directive records whether a ',' came right before it in the source (UsesItem::Directive { text, after_comma }). list_puncts gives that ',' back to the item before such a directive when that item has no punctuation (restore_commas_before_directives); directives can now end with ','. Pinned directives stay after the item they followed, so the ',' lands where the source had it. 'uses A {$I x.inc};' keeps the TASK-104 output.

Tests: fmt_bugs_test.rs comma_before_a_directive_ending_a_uses_list_is_kept (eight shapes incl. two directives, a comment on the comma, branches, and the no-comma cases; sorting on and off; parse and idempotency) and fixture tests/fixtures/uses/uses_comma_before_directive.pas for the oracle. Both failed on master.

Verification of the branch: fmt --check, clippy --workspace -D warnings clean; cargo test --workspace 3609/0/9; protocol_barriers 913/916 (timing tests only, see notes); corpus gate 347 files, only the new fixtures differ, none non-idempotent.

Out of scope, filed as a follow-up: a directive followed by a unit ('A, {$I u}, B;', '{$I u}, B;', 'A {$I u}, B;') and sorting moving a unit after a trailing include ('uses B, A, {$I u};').

Review round 1: a directive that followed a ',' in a comma-first list (blocks after the last unit, written ', B') is now written comma-first (', {$I u.inc}') instead of losing its ','. Test: comma_before_a_directive_in_a_comma_first_list_is_kept (five shapes, sorting off). cargo test --workspace 3610/0/9 after the fix.
<!-- SECTION:FINAL_SUMMARY:END -->
