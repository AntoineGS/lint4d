---
id: TASK-112
title: >-
  fmt4d: a // comment before a colon or comma in a declaration list swallows the
  punctuation
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 22:14'
updated_date: '2026-10-05 00:24'
labels:
  - fmt
  - correctness
milestone: m-3
dependencies:
  - TASK-103
modified_files:
  - crates/fmt4d/src/doc_builder_decls.rs
  - crates/fmt4d/src/doc_builder_alignment.rs
  - crates/fmt4d/tests/fmt_bugs_test.rs
  - crates/fmt4d/tests/fmt_roundtrip_test.rs
priority: high
type: bug
ordinal: 114000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while fixing TASK-103 (branch fix/fmt-trivia-fixes, b3e06ca); pre-existing on master d55c0f6.

A `//` line comment that trails the last identifier before the `:` of a var/field declaration, or that sits before a `,` in a parameter or field list, ends up in front of that punctuation in the output. The `:` or `,` then lands inside the line comment, so the output no longer compiles (or changes meaning).

Shapes to reproduce (exact repros in the TASK-103 report: .superpowers/sdd/day-2026-10-03/FMT-T-report.md in the main checkout):
```pascal
var
  A, B // why
    : Integer;

procedure P(A // first
  , B: Integer);
```

**Tests first.** Regression tests in crates/fmt4d/tests/fmt_bugs_test.rs for the var-declaration and parameter/field-list shapes, plus fixtures so the round-trip oracle (fmt_roundtrip_test.rs) covers them; check idempotency with alignment on and off.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A // comment before `:` or `,` in var, field and parameter lists never swallows the punctuation; output compiles and is idempotent with alignment on and off
- [x] #2 Regression tests fail on the original code and pass after the fix
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Branch fix/fmt-punctuation-trivia (main checkout, from master 9322cce), one commit per task.

Comma shape: already fixed by TASK-103 round 1 (291b3c7); existing tests cover it. Open: the colon shape.

Root cause (confirmed with fmt4d on master):
- Plain mode, crates/fmt4d/src/doc_builder_decls.rs build_comma_ident_decl: the last identifier before the ':' is emitted with doc_for_node (its trailing '//' included) and the suffix ': T;' follows on the same line. DECL_VAR expansion only moves a '//' of non-last identifiers (var_list_ident move_own = !is_last); the DECL_ARG/DECL_FIELD group never breaks before the suffix. Output: 'B // why: Integer;' (var), 'A, B // c: Integer;' (param, field).
- Aligned mode, crates/fmt4d/src/doc_builder_alignment.rs: decompose_var_or_field bails out on a '//' in the name list via name_list_has_line_comment, but that scans only from the first to the last name's end byte (and needs 2+ nodes), so a comment trailing the last name is missed; expand_comma_var_rows has no check. Even the single-identifier 'A // c\n : Integer;' becomes 'A // c: Integer;' when aligned.

Fix:
1. Plain: when the last identifier before the ':' has a trailing '//' comment, break the line before the suffix (Hardline), as build_children already does for single-identifier declarations ('A // c' / ': Integer;'). Applies to the DECL_VAR expansion and the DECL_ARG/DECL_FIELD group.
2. Aligned: decompose_var_or_field and expand_comma_var_rows return None (fall back to the plain renderer) when the last name trails a '//' comment.

Tests first: fmt_bugs_test.rs regressions for var (single and list), parameter and field lists, asserting rendered lines in plain, aligned and aligned-without-comment-cells modes and idempotency (format_three_modes_idempotent); a round-trip test in fmt_roundtrip_test.rs (assert_same_program for the three modes).

Adjustment found by the tests: in aligned var sections, bailing out of the whole expanded list made pass 1 write the earlier identifiers unaligned ('A: Integer;') and pass 2 align them ('A  : Integer;'). expand_comma_var_rows now keeps rows for the earlier identifiers and lays out only the last one unaligned (VarListRow.body = RowBody::Plain), which is what the second pass reads back.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
TASK-103 fix round 1 (branch fix/fmt-trivia-fixes, 291b3c7) fixes the comma shape: a `//` comment trailing an identifier before a `,` in a parameter/field list now moves after the comma (`A, // first` then `B`), and in expanded var lists after the declaration's `;`. The colon shape (`A, B // why\n : Integer;`) is still open.

RED (master src): line_comment_before_colon_does_not_swallow_a_var_type / _a_parameter_type / _a_field_type failed ('B // why: Integer;', 'A, B // c: Integer;'; the field case failed with 'formatting failed' on the second pass), and roundtrip_line_comment_before_colon_in_three_modes failed with 'child count changed: 4 -> 3, declClass'. The aligned single-identifier case ('A // c\n : Integer;') was broken too: 'A // c: Integer;'.
Ruling: the line breaks before the ':' ('B // why' / ': Integer;'), the layout the formatter already gave a single-identifier declaration in plain mode; the comment is not moved after the ';' because the suffix may carry its own trailing comments, whose order would change.
Change: doc_builder_decls.rs build_comma_ident_decl adds a Hardline before the suffix when the last identifier trails a '//' comment (DECL_VAR expansion and DECL_ARG/DECL_FIELD group); doc_builder_alignment.rs decompose_var_or_field also bails out when the name right before the ':' trails a '//' comment (name_list_has_line_comment only scans up to the last name's end), and expand_comma_var_rows lays out only that last identifier unaligned (RowBody::Plain, emitted like other non-row items).
GREEN: cargo test -p fmt4d all pass (fmt_bugs_test 172, fmt_roundtrip_test 31, lib 113); clippy -p fmt4d --all-targets -D warnings and cargo fmt --check clean. Corpus gate (/tmp/fmtx/gate.sh: 345 files = repo .pas fixtures + /tmp/fmtrev/corp, plain and aligned, base = master 9322cce): 0 differences from base, 0 non-idempotent.

Merged into master by fast-forward to a6850d2 (branch fix/fmt-punctuation-trivia, 2026-10-04). Merged tree verified: cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean; cargo test --workspace 3610 passed / 0 failed / 9 ignored.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: a '//' comment trailing the last name before the ':' of a declaration ('A, B // why\n : Integer;', 'procedure P(A, B // c\n : Integer)', record fields) was followed on the same line by ': Type', so the comment swallowed it and the output no longer compiled. In aligned var/field sections even a single name ('A // c\n : Integer;') was affected. The comma shape ('A // c\n , B') was already fixed by TASK-103.

Change (branch fix/fmt-punctuation-trivia):
- doc_builder_decls.rs, build_comma_ident_decl: when the last identifier trails a '//' comment, the line breaks before the suffix ('B // why' / ': Integer;'), in the var expansion and in parameter/field groups; this is the layout single-name declarations already had.
- doc_builder_alignment.rs: decompose_var_or_field falls back to the plain renderer when the name right before the ':' trails a '//' comment (the existing check only scanned up to that name's end); expand_comma_var_rows keeps aligned rows for the earlier names and lays out only the last one unaligned (new RowBody::Plain), so a second pass reads back the same shape.

Tests: fmt_bugs_test.rs line_comment_before_colon_does_not_swallow_a_var_type / _a_parameter_type / _a_field_type (plain, aligned, aligned with comments=false; parse clean, comment kept once, idempotent); fmt_roundtrip_test.rs roundtrip_line_comment_before_colon_in_three_modes. All failed on master and pass now. cargo test -p fmt4d, clippy -p fmt4d -D warnings, fmt --check clean; corpus gate 345 files, no differences from master, none non-idempotent. Workspace verification recorded with the branch's last task.
<!-- SECTION:FINAL_SUMMARY:END -->
