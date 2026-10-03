---
id: TASK-25
title: 'FMT-1: Blank-line normalization must not touch protected spans'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:28'
labels:
  - arch-review
  - fmt
  - correctness
milestone: m-3
dependencies: []
priority: high
type: bug
ordinal: 25000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (FMT-1).

Severity: high. Cost: correctness.

**Problem.** `crates/fmt4d/src/formatter.rs:70` runs
`normalize_blank_lines` over the entire rendered string;
`blank_lines.rs:5-27` trims whitespace-only lines and collapses blank runs
with no knowledge of comments, format-off regions or multiline string
literals, so `Doc::Raw` (`doc_builder.rs:68-70`, `:157`, `:198-204`) is not
verbatim end to end.

**Approach.** Emit blank-line decisions in the Doc builder (`Doc::Hardline`
counts) and delete the post-pass; or, as a minimal step, make the post-pass
skip byte ranges recorded as protected by the renderer.

**Tests first.** That fixture in `crates/fmt4d/tests/fmt_bugs_test.rs`
(fails today).

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A multiline string literal containing three consecutive blank lines and trailing spaces round-trips byte-identical inside a `{$FORMAT OFF}` region and outside it.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Minimal step from the task's Approach (post-pass skips protected ranges recorded by the renderer); the full Doc-builder rewrite stays out of scope.

1. Tests first (crates/fmt4d/tests/fmt_bugs_test.rs): a Delphi multiline string ('''...''') with three consecutive blank lines (two whitespace-only) plus a line with trailing spaces must survive byte-identical (a) in normal code and (b) inside a {$FMT.OFF}/{$FMT.ON} region (the project's format-off directive). Confirm both fail on the original code.
2. Renderer (crates/fmt4d/src/renderer.rs): record output byte ranges of verbatim text that spans lines, at the two push sites: Doc::Raw (format-off node text, comments, directives) and Doc::Token text containing '\n' (multiline string literals, pp fragments). Add render_with_protected_spans() returning (String, Vec<Range<usize>>); render() delegates.
3. blank_lines::normalize_blank_lines takes the ranges; a line whose start offset lies strictly inside a range (i.e. after a newline that belongs to protected text) is copied verbatim and resets the blank-run counter.
4. formatter.rs passes the ranges through. Unit tests in blank_lines.rs for the protected path.
5. Run the fmt4d suite + clippy for fmt4d.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Tests first: multiline_string_blank_lines_survive_formatting and format_off_region_with_multiline_string_survives_formatting (fmt_bugs_test.rs) both failed on the original code. The blank run inside the ''' literal was collapsed to one empty line and the whitespace-only lines were trimmed. They pass now. The blank_lines.rs unit tests protected_lines_are_kept_verbatim and blank_run_ending_at_protected_text_is_still_collapsed failed to compile (no protected parameter) before the change and pass now.

The project's format-off directive is {$FMT.OFF}/{$FMT.ON} (pascal_core::directives::parse_format_regions). AC #1's "{$FORMAT OFF}" is verified with that directive.

Pre-existing, out of scope: a node inside a {$FMT.OFF} region is returned as Doc::Raw before its leading comments/directives are emitted (doc_builder.rs doc_for_node), so the {$FMT.OFF} line itself disappears from the output. The second pass then formats the region, so the format-off fixture is not idempotent. For that reason the format-off test asserts the region byte-for-byte without idempotency_check. Proposed to the user as a follow-up.

Verification: cargo test -p fmt4d (all suites green), cargo clippy -p fmt4d --all-targets -- -D warnings clean, cargo fmt --check clean, cargo test -p pascal-lsp format green.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
## FMT-1: blank-line normalization skips protected spans

**Problem.** `normalize_blank_lines` ran over the whole rendered output, so blank runs and whitespace-only lines inside multiline string literals, multi-line comments and `{$FMT.OFF}` regions were collapsed and trimmed, even though the Doc builder emits those verbatim.

**Change.** This is the minimal step from the task's approach: the post-pass skips ranges that the renderer records as protected.
- `renderer.rs`: the two verbatim push sites (`Doc::Raw`, and token text in `emit_with_spacing`) go through `push_verbatim`, which records the output byte range whenever the text spans lines. New `render_with_protected_spans()` returns `(String, Vec<Range<usize>>)`. `render()` is now test-only.
- `blank_lines.rs`: `normalize_blank_lines` takes the ascending ranges. A line whose start offset lies strictly inside a range (after a newline that belongs to verbatim text) is copied unchanged and resets the blank-run counter. Line splitting matches `str::lines`.
- `formatter.rs`: passes the ranges through.

**Behaviour change.** Blank and whitespace-only lines inside any multi-line verbatim text are now preserved, including block comments and pp fragments, not only string literals. Blank runs between code are still collapsed as before.

**Tests.** Two fmt_bugs_test.rs fixtures (literal in normal code plus idempotency; literal inside a `{$FMT.OFF}` region) and two unit tests in blank_lines.rs. All failed before the change. The whole fmt4d suite, clippy `-D warnings` and the pascal-lsp formatting tests pass.

**Follow-up (not done).** Nodes inside a `{$FMT.OFF}` region drop their leading comments/directives, so the `{$FMT.OFF}` line itself is lost and the region is reformatted on the next run.
<!-- SECTION:FINAL_SUMMARY:END -->
