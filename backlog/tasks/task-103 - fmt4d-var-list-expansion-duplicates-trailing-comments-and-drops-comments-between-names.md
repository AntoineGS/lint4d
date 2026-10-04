---
id: TASK-103
title: >-
  fmt4d: var-list expansion duplicates trailing comments and drops comments
  between names
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 03:14'
updated_date: '2026-10-04 02:39'
labels:
  - fmt
  - correctness
dependencies: []
modified_files:
  - crates/fmt4d/src/doc_builder.rs
  - crates/fmt4d/src/doc_builder_decls.rs
  - crates/fmt4d/src/doc_builder_alignment.rs
  - crates/fmt4d/tests/common/mod.rs
  - crates/fmt4d/tests/fmt_bugs_test.rs
  - crates/fmt4d/tests/fmt_alignment_test.rs
  - crates/fmt4d/tests/fmt_roundtrip_test.rs
priority: high
type: bug
ordinal: 105000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while building the TASK-63 round-trip oracle.

build_comma_ident_decl (crates/fmt4d/src/doc_builder_decls.rs) expands `A, B: T;` into one declaration per identifier by cloning the suffix Doc (`: T;` with its comments). Comments attached to the suffix are therefore emitted once per identifier, and comments attached to the commas (which are not emitted) are dropped.

Repro (default config):
```pascal
unit T;
interface
implementation
procedure P;
var
  A, B: Integer; // shared
  C, { why } D: string;
begin
end;
end.
```
Output:
```pascal
var
  A: Integer; // shared
  B: Integer; // shared
  C: string;
  D: string;
```
`// shared` is duplicated and `{ why }` is lost. The output is stable on a second run (no commas left), so the idempotency checks do not notice.

Expected: every comment kept exactly once, e.g. the suffix's trailing comments only on the last expanded declaration and comments around a comma kept with the identifier before it.

Done when: a regression test in fmt_bugs_test.rs covers both cases and the round-trip oracle (check_same_program in fmt_roundtrip_test.rs) accepts the repro.

Acceptance Criteria:
--------------------------------------------------
No acceptance criteria defined
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/fmt-trivia-fixes, branch fix/fmt-trivia-fixes.
Root cause: build_comma_ident_decl clones suffix doc per identifier and drops comma-attached comments.
Steps: RED tests in fmt_bugs_test/fmt_alignment_test + roundtrip fixtures; fix: comments after each comma/ident kept with preceding identifier; suffix trailing comments only on last decl; verify idempotent aligned/unaligned.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED (before fix): 8 of 9 new fmt_bugs_test tests failed (e.g. `// own line` appeared 3 times, `{ c }`/`// d`/`// a` count 0, `// z` twice), 3 of 4 new fmt_alignment_test tests failed (`// shared` twice with alignment.comments=false, comma comments count 0, directive before a later identifier left a blank line), and the new round-trip test failed.
Ruling: comments after a comma (and a `//` comment trailing an identifier, which would swallow the `:`) go after the declaration's `;` on the same line, with the identifier before the comma — the comma is not emitted so nothing else can carry them. In DECL_ARG/DECL_FIELD groups they stay right after the `,` token (a `//` comment forces a hard break).
Ruling: in aligned mode leading comments/directives of later identifiers are lifted above their row (they were dropped / left inside the cell, which made blank lines); extends TASK-96 which left directives in place.
Found: the suffix's trailing directives were also duplicated per row; fixed with the comments.
Concern: a `//` comment trailing the last identifier before the colon (`A // c\n : T`) still swallows the colon; and `A // x\n, B` in DECL_ARG groups would swallow the comma (not handled, pre-existing).

Implemented on branch fix/fmt-trivia-fixes (worktree .worktrees/fmt-trivia-fixes), awaiting review and merge.

Fix round 1 (review, commit 291b3c7):
1. A `//` comment among the comma's trivia (leading or trailing) now always forces a line break in parameter/field lists; the comma's leading comments were emitted after the `,` followed by a soft Line, which commented out the next identifier (`A, // lc B: Integer`).
2. Comma trivia (new SeparatorTrivia / separator_trivia in doc_builder_decls.rs) is sorted by source position, spaced from the previous item's end (first item: one space), and everything after a `//` item goes on its own lines (plain: after the declaration; aligned: above the next row; groups: before the next identifier). A `//` comment trailing an identifier right before a comma in a parameter/field list is moved after the comma (this fixes the comma shape of TASK-112; the colon shape remains).
3. Ruling: suffix-interior trivia (`A, B: {t} Integer {u};`, `M, N\n { blk }\n : Byte;`) is emitted on the last expanded declaration only, same as the trailing trivia. The other copies are built under DocBuilder::without_trivia (a Cell flag that the comment/directive doc helpers, has_trailing_line_comment and trailing_comment_cell honour). — one rule for all suffix trivia; idempotent in all three modes.
4. Tests now assert rendered lines. Also found and fixed: in aligned record sections a non-row item after a `//`-ended item was rendered on the same line (`// sharedD // d, E` — pre-existing on d55c0f6); build_aligned_section now starts a line before non-row items (Doc::LineStart).
RED (on b3e06ca src): comment_before_comma_does_not_swallow_the_next_parameter / _field and line_comment_after_identifier_does_not_swallow_the_next_field failed with 'formatting failed: Parse ... unexpected token' on the second pass; var_list_comma_trivia_keeps_source_order_and_spacing, var_list_trivia_after_a_line_comment_starts_a_new_line, var_list_trivia_inside_the_type_is_emitted_once failed their assertions; roundtrip_comments_around_list_commas_in_three_modes failed 'child count changed: 4 -> 2, declClass'; field_list_after_a_line_comment_starts_its_own_line failed with `// sharedD, // d` when only the LineStart hunk was reverted.
Known limit: when one comma carries several items (`A, {$R+} // c`, `C, {x} {$R-} {y} D`), the first pass is correct but the expanded line then has several trailing items on one token, which the second pass re-spaces/reorders (pre-existing single-token bug, filed as TASK-125). Probes s2/s5 are not idempotent for that reason only (base dropped these comments altogether).

Fix round 2 (controller ruling): pulled TASK-125 into this branch, commit 3b9e287. Several trailing items after one token now keep source order with stable spacing in all three modes, so the multi-item comma probes s2/s5 are idempotent. The comma-moved items are split into data cell / comment cell the same way the next pass reads them (SeparatorTrivia.first_comment, per-item gaps). Probes s1-s9 are idempotent in plain, aligned and aligned comments=false; cargo test -p fmt4d green.

Fix round 3 (re-review, commit 078b577): (1) the LineStart that round 1 added before non-row items in aligned sections is skipped when the item's doc starts with a hardline; before, an `{$IFDEF}` declaration got a blank line and the next pass realigned (DUnitX.FixtureProvider.pas, DUnitX.Constants.pas, naming/local_var_in_ifdef.pas). RED: var_alignment_with_ifdef_blocks_adds_no_blank_lines showed blank lines after `var` and before `{$IFDEF X}`. (2) var_list_trivia_inside_the_type_is_emitted_once asserts exact lines. Gate: base d55c0f6 vs head on 326 files (all worktree fixtures + /tmp/fmtrev/corp) in plain and aligned modes: 0 differences, 0 non-idempotent; the same gate on 3b9e287 found the 4 reported regressions. Probes s1-s12 idempotent in all three modes. cargo test -p fmt4d 492/0; clippy and fmt clean.

Implemented on branch fix/fmt-trivia-fixes (worktree .worktrees/fmt-trivia-fixes), awaiting review and merge.

Merged into master as d0d55c7 (branch fix/fmt-trivia-fixes, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: build_comma_ident_decl (plain) and expand_comma_var_rows (aligned) copied the `: T;` suffix, including all its comments and directives, onto every expanded declaration, and never emitted comments attached to commas. DECL_ARG/DECL_FIELD groups dropped those comments too.
Change: b3e06ca (initial), 291b3c7 (review round 1), 3b9e287 (round 2, with TASK-125), on fix/fmt-trivia-fixes.
- All suffix trivia is emitted on the last expanded declaration only; the other copies are built under DocBuilder::without_trivia.
- Comma trivia (SeparatorTrivia) keeps source order and the spacing between items. Items stay on the same line up to the first `//` comment; the rest go on lines of their own. In var lists they follow the preceding identifier's declaration; in aligned mode, directives before the first comment follow the data and the rest forms the comment cell. In parameter and field lists they follow the `,`, with a hard break after a `//`. A `//` comment trailing an identifier before a comma moves after the comma.
- Later identifiers' leading comments and directives are lifted above their aligned row. Aligned sections start a new line before non-row items.
- With TASK-125, trailing trivia of every token is one source-ordered run with stable gaps, so the expanded output is idempotent.
Tests: fmt_bugs_test, fmt_alignment_test and fmt_roundtrip_test regressions (three modes, run twice); RED is recorded per round in the notes. cargo test -p fmt4d green; clippy -D warnings and fmt clean. Reviewer probes s1-s8 plus s9 are idempotent in plain, aligned and aligned comments=false.
Limits: the TASK-112 colon shape (`A // c\n : T`) still swallows the colon.
<!-- SECTION:FINAL_SUMMARY:END -->
