---
id: TASK-107
title: 'fmt4d: end-of-file trivia inside an unclosed {$FMT.OFF} region is re-laid-out'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 03:28'
updated_date: '2026-10-04 02:39'
labels:
  - fmt
  - correctness
dependencies:
  - TASK-26
modified_files:
  - crates/fmt4d/src/doc_builder.rs
  - crates/fmt4d/tests/fmt_bugs_test.rs
  - crates/fmt4d/tests/fmt_roundtrip_test.rs
priority: medium
type: bug
ordinal: 109000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Recorded by the TASK-26 reviewer (not fixed in that branch).

TASK-26 (crates/fmt4d/src/doc_builder.rs, DocBuilder::build / eof_trivia_doc) emits comments and directives after the file's last leaf using the source's line layout: one item per line, same-line spacing and a single blank line kept. That layout ignores any {$FMT.OFF} region. Only when the whole root is in a format-off region is the root emitted verbatim, trivia included.

Repro: a {$FMT.OFF} region that is never closed and covers trivia after `end.`:
```pascal
unit T;
interface
implementation
{$FMT.OFF}
end.
  // tail
```
Output ends with `end.\n// tail`: the indentation (and any extra blank lines) of the trailing trivia is lost although it is inside a format-off region.

Note: whether the root counts as format-off depends on the trailing newline. is_in_format_off_region(root) compares the root's end row, and a final newline moves it to the row after the region's end line.

Suggested fix: when the end-of-file trivia lies inside a format-off region, emit the source slice from the last leaf's end (after its trailing trivia) to EOF verbatim instead of laying the items out.

Done when: a regression test in fmt_bugs_test.rs shows trailing trivia inside an unclosed {$FMT.OFF} region kept byte-for-byte (with and without a final newline) and idempotent.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Same worktree/branch (fix/fmt-trivia-fixes).
Reproduce first with and without final newline. Then: in DocBuilder::build / eof_trivia_doc, when the EOF trivia lies inside a format-off region emit the source slice verbatim; RED test in fmt_bugs_test (byte-for-byte, with and without final newline, idempotent).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Repro: reproduced as described, with and without a final newline: `{$FMT.OFF}\nend.\n  // tail` came out as `end.\n// tail` (indentation lost; blank lines before the trivia would collapse too). The note about the root being format-off only for one of the two newline cases is true for the root check, but both cases reach eof_trivia_doc because the root starts before the region, so the fix does not depend on it.
RED: new fmt_bugs_test::eof_trivia_in_unclosed_format_off_is_verbatim failed on d55c0f6 with left `...end.\n// tail\n` vs right `...end.\n  // tail\n`.
Ruling: the slice runs from the end of the last token (when the region began before it) or from the first in-region item, to the end of the last in-region item; trailing blank lines after it are still normalised to one final newline, like the rest of the file. Items outside any region keep the existing layout. — byte-for-byte except for the formatter's final-newline normalisation.
Implemented on branch fix/fmt-trivia-fixes (worktree .worktrees/fmt-trivia-fixes), awaiting review and merge.

Merged into master as d0d55c7 (branch fix/fmt-trivia-fixes, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: eof_trivia_doc laid out trivia after the last token by source line layout, ignoring {$FMT.OFF} regions, so indentation and blank lines in an unclosed region were lost.
Change (commit ca9b0d9): eof_trivia_doc splits the items by region membership (new in_format_off_region) and emits in-region runs verbatim through verbatim_doc, which format_off_doc now shares. Other items keep the TASK-26 layout.
Tests: fmt_bugs_test eof_trivia_in_unclosed_format_off_is_verbatim (4 variants, with/without final newline, comments and directives, same-line, plain and aligned, idempotent), eof_trivia_after_a_closed_format_off_is_laid_out_normally, round-trip test. RED recorded. cargo test -p fmt4d green, clippy -D warnings and fmt clean.
Limits: trailing blank lines after the last item are still reduced to the final newline.
<!-- SECTION:FINAL_SUMMARY:END -->
