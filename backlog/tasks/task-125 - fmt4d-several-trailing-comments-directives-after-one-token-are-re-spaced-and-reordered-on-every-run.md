---
id: TASK-125
title: >-
  fmt4d: several trailing comments/directives after one token are re-spaced and
  reordered on every run
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 23:33'
updated_date: '2026-10-04 02:39'
labels:
  - fmt
  - correctness
dependencies: []
modified_files:
  - crates/fmt4d/src/doc_builder.rs
  - crates/fmt4d/src/doc_builder_alignment.rs
  - crates/fmt4d/src/doc_builder_decls.rs
  - crates/fmt4d/tests/fmt_bugs_test.rs
priority: high
type: bug
ordinal: 127000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in the TASK-103 review (fix round 1); pre-existing on master d55c0f6.

When a token has more than one trailing comment or directive on its line, doc_for_node emits them through trailing_comments_doc then trailing_directives_doc (crates/fmt4d/src/doc_builder.rs, fn trailing_comments_doc / trailing_directives_doc):
- each item's gap is measured from the preceding *code token* (AttachedComment.gap / AttachedDirective.gap), not from the previous trivia item, so the second and later items get the whole span from the token as spacing, and the spacing grows on every run;
- comments are emitted before directives regardless of source order, so `{$R+} // end` becomes `// end {$R+}` and the directive is commented out.

Repros (default config, inside `procedure P; var ... begin end;`):
- `S: Byte; {t1} // t2` is not idempotent: the gap before `// t2` widens each pass.
- `R: Byte; {$R+} // end` -> `R: Byte; // end {$R+}` (directive lost into the comment).
- `C: Byte; {x} {$R-} {y}` -> `{x} {y} {$R-}`, then wider gaps.
Aligned mode shows the same through trailing_comment_cell (comments only; directives appended after).

Fix idea: one merged, source-ordered trailing trivia run per leaf, with gaps from the previous item's end (TASK-64 'one trivia index' is the structural version). The TASK-103 SeparatorTrivia helper (doc_builder_decls.rs) already does this for list commas.

Tests first: fmt_bugs_test regressions for the three repros, idempotent in plain, aligned, and aligned with comments = false.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Pulled into branch fix/fmt-trivia-fixes (worktree .worktrees/fmt-trivia-fixes) by controller ruling (TASK-103 fix round 2).
Root cause: doc_for_node emits a leaf's trailing comments (trailing_comments_doc) then its trailing directives (trailing_directives_doc); each item's gap is AttachedComment/Directive.gap, measured from the code token, so later items get the whole span as spacing (grows each run) and directives always follow comments (`{$R+} // c` -> `// c {$R+}`). Aligned mode splits them: trailing_comment_cell takes comments only, directives stay in the value cell (reorders `{x} {$R-} {y}`).
Steps: 1) RED tests running the formatter twice in plain/aligned/aligned comments=false (S: Byte; {t1} // t2, {$R+} // end, {x} {$R-} {y}, and the var-list comma probes s2/s5). 2) One merged trailing run per node: source order, first gap from the token, later gaps from the previous item's end, line break before anything after a `//`. 3) Aligned: comment cell holds the run from the first trailing comment on (comments and directives); sans_trailing keeps only the directives before it. 4) Full fmt4d suite, clippy, fmt, probes s1-s8.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related: TASK-112 (same family: trivia placed in front of punctuation / other trivia in declaration lists); not a hard dependency either way. Until this is fixed, a var-list comma carrying a directive plus a comment (`A, {$R+} // c`) formats correctly on the first pass after TASK-103 fix round 1, but the second pass hits this bug on the expanded `A: Integer; {$R+} // c` line.

RED (on 291b3c7): fmt_bugs_test several_trailing_items_keep_order_and_spacing failed 'plain: formatter is not idempotent'. The first pass already gave `S: Byte; {t1}      // t2`, `R: Byte;       // end {$R+}`, `C: Byte; {x}           {y}     {$R-}`, `T: Byte;   {a}          {b}`. var_list_with_several_comma_items_is_idempotent and var_list_comma_trivia_keeps_source_order_and_spacing (now three-mode idempotent) failed the same way.
Ruling: in aligned mode a trailing directive before the first trailing comment stays with the data cell, and everything from the first comment on (comments and directives, in source order) forms the comment cell. sans_trailing therefore drops directives after the first comment, and the TASK-103 comma-moved items are split the same way, so a second pass reads back the same cells. — order kept; the comment column still aligns.
Ruling: the first item of an aligned comment cell or of comma-moved trivia gets one space; later items keep their source distance from the previous item, and an item after a `//` starts a new line (unreachable from source order, kept as a guard).
GREEN: commit 3b9e287. All three tests pass; cargo test -p fmt4d 0 failures; fmt --check, clippy -p fmt4d -D warnings clean. Probes s1-s9 (s4 = this task's repros, s9 adds wide gaps and a statement `X := 1; {p}   {$R-} // q`) are idempotent in plain, aligned and aligned comments=false.
Implemented on branch fix/fmt-trivia-fixes (worktree .worktrees/fmt-trivia-fixes), awaiting review and merge.

Fix round 3 (re-review, commit 078b577): trailing_run_doc pushed each gap as its own whitespace-only Doc::Raw. The renderer clears the previous token after such a Raw, so the token after a trailing comment lost its space (`A: {t}Integer`, `X := {a}1`, `if {c}X`, `A + {c}B`). The gap and the text are now one Raw. RED: comment_inside_code_keeps_the_space_after_it failed with `A: {t}Integer;`, `B: {$R+}Integer;`, `X := {a}1;`. GREEN on 078b577. Probe s10/s12 output matches base except `C: {t} {$R-} Integer;`, where base had a grown gap (`{t}     {$R-}`); that difference is this task's intended fix. The corpus gate (326 files, plain + aligned) has 0 differences from base and 0 non-idempotent files.

Implemented on branch fix/fmt-trivia-fixes (worktree .worktrees/fmt-trivia-fixes), awaiting review and merge.

Merged into master as d0d55c7 (branch fix/fmt-trivia-fixes, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: a token's trailing comments and directives were emitted as two separate runs (all comments, then all directives), and each item's gap was measured from the code token. The spacing before second and later items grew on every run, and `{$R+} // c` became `// c {$R+}`, which comments out the directive. Aligned mode put comments in the comment cell and directives in the data cell, which reordered them.
Change (commit 3b9e287, branch fix/fmt-trivia-fixes): doc_builder.rs has TrailingItem plus trailing_items (merged, source-ordered) and trailing_run_doc (first gap from the token or given; later gaps from the previous item's end; a line break after `//`). doc_for_node and sans_leading use trailing_trivia_doc. trailing_comments_doc and trailing_directives_doc use the same run. sans_trailing keeps only the directives before the first comment. Aligned trailing_comment_cell holds the items from the first comment on. TASK-103's SeparatorTrivia records the first comment and per-item gaps so comma-moved items get the same cell split.
Tests: fmt_bugs_test several_trailing_items_keep_order_and_spacing and var_list_with_several_comma_items_is_idempotent, plus var_list_comma_trivia_keeps_source_order_and_spacing made idempotent. All run plain, aligned and aligned comments=false twice; all were RED on 291b3c7. cargo test -p fmt4d green; clippy and fmt clean; probes s1-s9 idempotent in all three modes.
Limits: only same-line trailing trivia is covered; the uses-clause path has its own layout and was not touched.
<!-- SECTION:FINAL_SUMMARY:END -->
