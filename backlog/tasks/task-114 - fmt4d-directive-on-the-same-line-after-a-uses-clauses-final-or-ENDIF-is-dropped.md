---
id: TASK-114
title: >-
  fmt4d: directive on the same line after a uses clause's final ; or {$ENDIF} is
  dropped
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 22:18'
updated_date: '2026-10-04 22:39'
labels:
  - fmt
  - correctness
dependencies: []
modified_files:
  - crates/fmt4d/src/uses.rs
  - crates/fmt4d/src/doc_builder.rs
  - crates/fmt4d/tests/fmt_bugs_test.rs
  - crates/fmt4d/tests/fixtures/uses/uses_directive_after_clause.pas
priority: high
ordinal: 116000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while working on TASK-104 (pre-existing; not caused by the uses-clause punctuation fixes).

Repro (default config):
```pascal
unit T;
interface
uses A; {$I d.inc}
implementation
end.
```
Output: `uses\n  A;\n\nimplementation` — the `{$I d.inc}` is gone. Same for a directive after the {$ENDIF} of a clause that is one ppUsesBlockWithSemi (`uses {$IFDEF X} A; {$ELSE} C; {$ENDIF} {$I d.inc}`). On its own line after the clause the directive is kept.

Likely cause: DirectiveMap attaches a same-line directive as trailing trivia of the clause's last leaf (`;` or ppEndIf). build_uses (crates/fmt4d/src/doc_builder.rs, fn build_uses) rebuilds the clause from UsesItem values and only reads comments from CommentMap (uses.rs punctuation_texts / trailing_texts), never trailing directives of leaves inside the clause, so the directive has no emission path. Dropping an {$I} silently changes the program.

Expected: the directive is kept (e.g. on the `;` line or the next line), output parses and is idempotent; a regression test in fmt_bugs_test.rs and the round-trip oracle (which compares directive texts) cover it.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Branch fix/fmt-punctuation-trivia (main checkout), own commit after TASK-112.

Root cause (confirmed on master 9322cce): a directive on the same line after the clause's final ';' or {$ENDIF} is outside declUses in the tree (a sibling extra), and DirectiveMap attaches it as a trailing directive of the clause's last leaf. build_uses (doc_builder.rs) lays the clause out from extract_uses_items (uses.rs), which reads only CommentMap, so nothing emits it.

Fix:
1. extract_uses_items also takes the DirectiveMap and returns a UsesClause { items, after }: 'after' holds the trailing trivia of the clause's last leaf (';' or ppEndIf) from its first directive on, directives and the comments after them, in source order. Those comments are left out of the items. Comments before the first directive keep their owner (the unit/block they follow).
2. layout_uses_items takes 'after'; emit_list puts it after the clause's end on the same line: on the item carrying the ';' (unit, directive, block after {$ENDIF}) or the lone ';'. It follows that item's comments, or precedes them when they hold a '//' comment that would swallow it. It never moves with a unit under sorting.

Tests first: fmt_bugs_test.rs directive_after_uses_clause_on_its_line_is_kept (plain unit, ppUsesBlockWithSemi, block followed by ';', comments around the directive, sorting, '//' comments on the last unit; parse, once, idempotent, sorting on and off) and fixture crates/fmt4d/tests/fixtures/uses/uses_directive_after_clause.pas for the round-trip oracle (sorted and unsorted sweeps).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED (master src): directive_after_uses_clause_on_its_line_is_kept failed ('{$I d.inc}' count 0); every_fixture_round_trips and every_uses_fixture_round_trips_with_sorting_off failed on the new fixture ('comments or directives changed').
Ruling: the trivia after the clause's end stays on the end's line ('A; {$I d.inc}', '{$ENDIF} {$I d.inc}', '{$ENDIF}; {$I d.inc}'), after the comments of the item that now ends the clause; when those hold a '//' comment it goes before them ('A; {$I d.inc} // c'), which reorders that comment and the directive but keeps both on the line. On its own line the directive would lead the next declaration on a second run, so it would not be idempotent. With sorting the directive stays at the clause's end ('uses B, A; {$I d.inc}' -> 'A,' / 'B; {$I d.inc}').
Change: uses.rs UsesClause / after_clause_trivia, extract_uses_items(.., directives) -> UsesClause, layout_uses_items(items, after, ..), emit_list(.., after, ..) and with_after; doc_builder.rs build_uses passes the DirectiveMap and the trivia. The uses.rs unit tests pass DirectiveMap::empty().
GREEN: cargo test -p fmt4d all pass (fmt_bugs_test 173, fmt_roundtrip_test 31, lib 113); clippy -p fmt4d --all-targets -D warnings and fmt --check clean. Corpus gate (346 files, plain and aligned, base master 9322cce): only the new fixture differs (base drops its directives), 0 non-idempotent.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: a directive on the same line after a uses clause's final ';' or {$ENDIF} ('uses A; {$I d.inc}', 'uses {$IFDEF X} A; {$ELSE} C; {$ENDIF} {$I d.inc}') was dropped. It sits outside declUses and trails the clause's last leaf; the uses layout only read comments, so nothing emitted it. Dropping an {$I} silently changes the program.

Change (branch fix/fmt-punctuation-trivia):
- uses.rs: extract_uses_items takes the DirectiveMap and returns a UsesClause; its 'after' holds the trivia trailing the clause's last leaf from the first directive on (with the comments after it, which are no longer given to a unit). layout_uses_items/emit_list write it after the clause's end on that line: after the comments of the item ending the clause, or before them if they hold a '//' comment. It does not move with units under sorting.
- doc_builder.rs: build_uses passes the DirectiveMap and the trivia.

Tests: fmt_bugs_test.rs directive_after_uses_clause_on_its_line_is_kept (seven shapes, sorting on and off: kept once, parses, idempotent, exact layout) and fixture tests/fixtures/uses/uses_directive_after_clause.pas for the round-trip oracle. Both failed on master. cargo test -p fmt4d, clippy -p fmt4d -D warnings, fmt --check clean; corpus gate 346 files: only the new fixture differs from master, none non-idempotent. Workspace verification recorded with the branch's last task.
<!-- SECTION:FINAL_SUMMARY:END -->
