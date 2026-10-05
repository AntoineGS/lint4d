---
id: TASK-137
title: >-
  fmt4d: punctuation after a directive followed by a unit in a uses clause is
  dropped or invented
status: Done
assignee:
  - '@claude'
created_date: '2026-10-04 22:49'
updated_date: '2026-10-05 00:57'
labels:
  - fmt
  - correctness
milestone: m-3
dependencies:
  - TASK-128
modified_files:
  - crates/fmt4d/src/uses.rs
  - crates/fmt4d/tests/fmt_bugs_test.rs
  - crates/fmt4d/tests/fixtures/uses/uses_include_keeps_order.pas
priority: medium
type: bug
ordinal: 139000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while fixing TASK-128 (branch fix/fmt-punctuation-trivia); pre-existing on master 9322cce.

TASK-128 keeps a ',' that sat before a directive (UsesItem::Directive.after_comma, restore_commas_before_directives in crates/fmt4d/src/uses.rs). The ',' *after* a directive is still not recorded, and a unit before a directive always gets a ',' when units follow, so a directive followed by a unit in the output can read wrongly. An include is opaque, so the source punctuation around it carries meaning. Default config (sorting on) unless noted:

- 'uses A, {$I u.inc}, B;' -> 'A,' / '{$I u.inc}' / 'B;'. If u.inc holds 'X' the source reads 'A, X, B' and the output 'A, X B' (does not compile).
- 'uses {$I u.inc}, B;' -> '{$I u.inc}' / 'B;' (same: the ',' after the directive is lost).
- 'uses A {$I u.inc}, B;' -> 'A,' / '{$I u.inc}' / 'B;'. If u.inc holds ', X' the source reads 'A, X, B' and the output 'A, , X B'.
- Sorting: 'uses B, A, {$I u.inc};' -> 'A,' / '{$I u.inc}' / 'B;'. The directive is pinned after its anchor A, so B now follows it without a ','. Source 'B, A, X;' becomes 'A, X B;'.
- Sorting: 'uses A {$IFDEF X}, B, {$I u.inc} {$ENDIF};' moves the block in front of A (layout_uses_items) -> '{$IFDEF X}' / 'B,' / '{$I u.inc}' / '{$ENDIF}' / 'A;', which reads 'B, U A;' with X defined. With sorting off it is correct since TASK-128 review round 1 (', {$I u.inc}').

Likely shape of the fix: record the punctuation after each directive too (comma_after), give a directive followed by a unit or block its source ','; do not add a ',' to a unit before a directive that had none in the source; under sorting, keep a directive that ended the source list at the end of the clause (like the block handling in layout_uses_items), so its ';' context is kept.

Tests first: fmt_bugs_test.rs regressions asserting the output shapes (the oracle cannot see inside the include, though it does compare punctuation per configuration), sorting on and off, parse and idempotency; a uses fixture.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Decision (user, 2026-10-04): do not sort around includes. A uses clause that contains an include directive ({$I file} / {$INCLUDE file}, also (*$I ...*); not the {$I+}/{$I-} switch) keeps its source order (no sorting, no grouping), and each separator next to an include copies the source. The formatter cannot see what an include lists, so sorting around one cannot be done safely.

Also fixes a regression from TASK-128 (on master, unpushed): restore_commas_before_directives applies to every directive, so with sorting 'uses B, {$R+} A;' became 'A,' / 'B,' / '{$R+};' ('uses A, B, ;'). Only includes have content that a ',' can separate; other directives are transparent and must not move punctuation.

Plan (crates/fmt4d/src/uses.rs):
1. is_include_directive(text). UsesItem::Directive { text, comma_before, comma_after }: the literal ',' right before and right after it in its list (rename of after_comma; comma_after set when the next non-comment child is a ',').
2. layout_uses_items: when any item (recursively, in blocks) is an include, lay the clause out with sort and group off.
3. list_puncts: replace restore_commas_before_directives with a pass over include directives only. Before an include, the separator copies comma_before: the previous item (unit, block, any directive) gets or loses its ',' (or, if it or the list is comma-first, or there is none, the include is written comma-first). After an include, the separator copies comma_after: the include gets or loses its ',' (a comma-first next item supplies its own; a comma-first next unit/directive loses its lead when there is none). When includes follow the list's last unit/block and the list ends with ',' (a branch followed by more units), that ',' goes to the last include, as the ';' already does (TASK-104).
4. Update the uses.rs unit tests that sort around '{$I ...}' to use a non-include directive (their point is anchoring), and add one that an include keeps source order.

Tests first: fmt_bugs_test.rs regression for the TASK-137 shapes (sorting on and off, exact output, parse, idempotent) and for non-include directives under sorting (parse cleanly); a uses fixture for the oracle.

Revision (found by probes before commit): copying only the separators next to an include is not enough when a conditional block sits beside it. 'uses A {$IFDEF X}, B {$ENDIF} {$I u.inc}, C;' came out as 'A,' / block / '{$I u.inc},' / 'C;', which without X reads 'A, U, C' where the source reads 'A U, C'. The include's real neighbour depends on the configuration. New approach, same decision: a clause holding an include is laid out in source order with its source punctuation throughout.
- Extraction records the literal ',' after every unit, block (after its {$ENDIF}) and directive (comma_after), and a ',' at the start of each branch (CondBranch.lead / IfDefBlock.else_lead).
- layout_uses_items: for such a clause, skip the sort/anchor machinery and emit the items as they are in a 'source' list style: each item ends with its own ',' or nothing, a branch that starts with ',' writes it before its first item, and the clause's ';' goes after the last item (inside the branches of a terminated block). emit_list / emit_ifdef_block take the list style.
- Other clauses keep the existing layout; directives there never move punctuation, which removes the TASK-128 restore rule (and its {$R+} regression). The TASK-128 shapes all contain an include, so they are now laid out from the source.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED (master a6850d2 src): uses_clause_with_an_include_keeps_its_order_and_punctuation failed ('uses A, {$I u.inc}, B;' -> 'A,' / '{$I u.inc}' / 'B;'); uses_sorting_around_other_directives_keeps_the_clause_valid failed ('uses B, {$R+} A;' -> 'A,' / 'B,' / '{$R+};', the TASK-128 regression); every_fixture_round_trips / every_uses_fixture_round_trips_with_sorting_off failed on the new fixture uses_include_keeps_order.pas ('["U,U,,U,U;"] -> ["U,U,U,U;"]').
First attempt (copying only the separators next to an include) was dropped before commit: probes showed it wrong next to a conditional block (see plan revision).
Change (crates/fmt4d/src/uses.rs): Unit/Directive/IfDefBlock record comma_after, CondBranch.lead / IfDefBlock.else_lead record a ',' that starts a branch (SourceCommas, while children are read); is_include_directive / has_include; layout_uses_items writes a clause with an include in ListStyle::Source (source_puncts; emit_ifdef_block keeps each branch's punctuation and puts the block's ',' or ';' after {$ENDIF}, a terminated block's ';' in its branches). Other clauses use list_puncts as before TASK-128; directives there never get or move punctuation (restore_commas_before_directives removed). Directives honour Punct.lead only where a source list starts with ','.
Test updates: comma_before_a_directive_in_a_comma_first_list_is_kept (TASK-128 review) now expects the source's punctuation ('{$IFDEF X}' / ', B,' / '{$I u.inc}' instead of ', B' / ', {$I u.inc}'); uses.rs format_items_directive_at_start_stays_first / _between_units_follows_anchor use '{$HINTS OFF}' (they test anchoring under sorting, which no longer applies to includes); new unit tests format_items_with_an_include_keep_their_order and include_directives_are_recognised.
GREEN: cargo test -p fmt4d 512 passed; cargo test --workspace 3614 passed / 0 failed / 9 ignored; clippy --workspace --all-targets -D warnings and fmt --check clean. Corpus gate (348 files, base master 9322cce): differences only in the uses fixtures with includes (uses_ends_in_directive.pas's implementation clause 'D, B, E {$I y.inc}' is no longer sorted, as decided; the TASK-114/128 fixtures as before); 0 non-idempotent.

Review round 1 (code-reviewer subagent on b867a60..1c50c48; approach judged sound):
1. A branch holding only a ',' lost it in source mode: 'uses A {$IFDEF X}, {$ELSE}, {$ENDIF} {$I u.inc};' (reads 'A, U' either way) came out with empty branches ('A U'). RED: new shape in uses_clause_with_an_include_keeps_its_order_and_punctuation. Fix: emit_list writes a list's leading ',' on its own line when the list has no item to put it before. GREEN.
2. '(*$I u.inc*)' never switches a clause to source mode: the grammar parses the '(*$' spelling as a comment (so do (*$IFDEF*) blocks), so fmt4d formats it as a comment everywhere. Not fixable in the uses layout alone (comments are placed after a unit's punctuation). is_include_directive no longer claims to accept '(*$'; filed TASK-138 for the grammar.
After the fixes: cargo test -p fmt4d 512 passed; cargo test --workspace 3614 / 0 / 9; clippy -D warnings, fmt --check clean; corpus gate: only the include fixtures differ, 0 non-idempotent.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: an include in a uses clause is opaque, so the punctuation around it carries meaning, but the layout re-punctuated and sorted around it: 'uses A, {$I u.inc}, B;' lost the ',' after the include, 'uses A {$I u.inc}, B;' gained one before it, and sorting 'uses B, A, {$I u.inc};' put B after the include with no ','. TASK-128's comma restore also applied to directives that list nothing, so with sorting 'uses B, {$R+} A;' became 'uses A, B, {$R+};' (regression on master since bf881df).

Decision (user): a uses clause holding an include ({$I file} / {$INCLUDE file}, not {$I+}) is written as in the source: no sorting or grouping, and every separator where the source had it (after each unit, block and directive, and at the start of a branch). Copying only the separators next to the include was tried first and is wrong next to a conditional block, whose content changes the include's neighbour per configuration.

Change (crates/fmt4d/src/uses.rs, branch fix/fmt-uses-include-punctuation): items record the ',' that followed them and branches a leading ','; layout_uses_items lays such a clause out in ListStyle::Source (source_puncts, emit_ifdef_block in source mode). Other clauses are laid out as before TASK-128; directives there never move punctuation.

Tests: fmt_bugs_test.rs uses_clause_with_an_include_keeps_its_order_and_punctuation (nine shapes, sorting on and off), uses_sorting_around_other_directives_keeps_the_clause_valid; fixture tests/fixtures/uses/uses_include_keeps_order.pas; uses.rs unit tests. All failed on master. The TASK-128 comma-first test now expects the source layout. cargo test --workspace 3614/0/9, clippy -D warnings and fmt --check clean; corpus gate: only the include fixtures differ, none non-idempotent.

Review round 1: a conditional branch holding only a ',' keeps it (own line). The '(*$I ...*)' spelling is out of scope: the grammar parses it as a comment; follow-up TASK-138.
<!-- SECTION:FINAL_SUMMARY:END -->
