---
id: TASK-96
title: Aligned sections keep standalone directives without inserting blank lines
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 01:49'
updated_date: '2026-10-03 02:02'
labels:
  - fmt
  - correctness
milestone: m-3
dependencies: []
priority: high
type: bug
ordinal: 98000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while fixing TASK-95. With alignment enabled ([format.alignment] enabled = true), a standalone compiler directive on its own line before a declaration in an aligned section gets a blank line inserted before it. The blank line then splits the alignment group, so the next run aligns the declarations differently: formatting is not idempotent, and format-on-save keeps changing the file.

Repro (alignment enabled): `unit T;\ninterface\nimplementation\nprocedure P;\nvar\n  A: Integer;\n  {$R+}\n  CCC: Integer;\nbegin\nend;\nend.\n`. Pass 1 gives `  A   : Integer;\n\n  {$R+}\n  CCC: Integer;`, and pass 2 gives `  A: Integer;\n\n  {$R+}\n  CCC: Integer;`.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 With alignment enabled, a standalone directive between two declarations of an aligned var/const/type/field/property section is kept on its own line with no blank line added before or after it
- [x] #2 Formatting such a section twice gives identical output
- [x] #3 Behaviour with alignment disabled is unchanged
- [x] #4 Regression tests in crates/fmt4d/tests fail on the original code and pass after the fix; the fmt4d suite and clippy stay green
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/aligned-directive, branch fix/aligned-directive.

Root cause: directives, like comments, are attached to leaves; a standalone directive before a declaration leads that declaration's first leaf. build_aligned_section lifts the first leaf's leading *comments* to group level (leading_comments_doc(child), falling back to its first code child) and the decomposers drop them from the cell via doc_for_node_sans_leading. But that function only drops comments: the leaf's leading directives stay inside the row's first cell. The renderer has already started the row's line and written its indentation, so the directive's leading Hardline leaves a whitespace-only line, which becomes a blank line and splits the group on the next pass.

Fix:
1. Tests first (fmt_alignment_test.rs): standalone directive in var, const, comma-var (expand_comma_var_rows) and class-field sections; no blank line around it, emitted once, idempotent. All four fail on 00f5223.
2. doc_for_node_sans_leading omits leading directives as well as leading comments (its callers are all alignment cells); format_off_doc mirrors that for with_leading = false.
3. One helper in doc_builder_alignment.rs that builds a row's leading comments + directives with the existing first-child fallback, used by the three row branches (comma expansion, alias misparse, decompose).
4. fmt4d suite, clippy, fmt; non-aligned path untouched (sans_leading is only used by alignment).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Tests (fmt_alignment_test.rs), all four failing on 00f5223 with a blank line around the directive: var_/const_/comma_var_/field_alignment_keeps_standalone_directive. All pass now.

Comma declarations: only the first identifier's leading trivia is lifted above the rows. Later identifiers (I, <directive> J) keep their leading directives inside the cell exactly as before, so nothing new is dropped. Leading comments on later identifiers were already dropped by sans_leading before this change (pre-existing, untouched).

A/B check: master (00f5223) vs branch binaries on 400 sampled .pas files with directives from the multidev checkout. Plain mode: 0 files differ. Aligned mode: 1 file differs, and only at a {$ifend} before an aligned var declaration (master's blank line is gone and the declaration rejoins the group). Branch output is idempotent there.

Verification: cargo test -p fmt4d 441/0, clippy -D warnings clean, fmt --check clean, pascal-lsp format 28/0. Committed on fix/aligned-directive as 3802823.

Merged into master as 3802823 (fast-forward); master suite green (fmt4d 441 passed, clippy clean).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
## Keep standalone directives between aligned declarations

**Problem.** With alignment enabled, a standalone directive before a declaration (e.g. `{$R+}`, `{$ifend}`) got a blank line inserted before it. The blank line split the alignment group on the next run, so aligned output was not idempotent.

**Cause.** Directives, like comments, attach to leaves, so the directive leads the declaration's first leaf. `build_aligned_section` lifted that leaf's leading comments above the row, but `doc_for_node_sans_leading` only dropped comments. The directive stayed in the row's first cell, and its Hardline fired after the row had already started its line and written its indentation.

**Change (commit 3802823).**
- `doc_for_node_sans_leading` omits leading directives as well (its only callers are alignment cells); `format_off_doc` mirrors that.
- New `row_leading_docs` helper emits a declaration's leading comments and directives above its row (with the existing first-child fallback). It replaces three duplicated blocks.
- Comma declarations: only the first identifier's trivia is lifted. Later identifiers keep their directives in the cell as before.

**Tests.** Four tests in fmt_alignment_test.rs (var, const, comma-var, class fields), all failing on 00f5223. fmt4d 441/0, clippy -D warnings and fmt clean, pascal-lsp format 28/0. A/B on 400 real files: plain mode identical; aligned mode differs only at one real occurrence of this bug.

**Known/out of scope.** Leading comments on later identifiers of a comma declaration (`I,\n  { c }\n  J: Integer;`) were already dropped before this change.
<!-- SECTION:FINAL_SUMMARY:END -->
