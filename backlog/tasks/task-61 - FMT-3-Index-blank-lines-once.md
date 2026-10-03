---
id: TASK-61
title: 'FMT-3: Index blank lines once'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 21:55'
labels:
  - arch-review
  - fmt
  - runtime
milestone: m-6
dependencies: []
modified_files:
  - crates/fmt4d/src/doc_builder.rs
  - crates/fmt4d/tests/fmt_perf_test.rs
priority: medium
type: enhancement
ordinal: 61000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (FMT-3). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: medium. Cost: runtime.

**Problem.** `doc_builder.rs:377-399` `has_blank_line_between` scans the
source from byte zero on every call; callers in `doc_builder_decls.rs:58-65`,
`doc_builder_alignment.rs:613`, `:823` call it per declaration, so long
declaration lists are quadratic in source bytes.

**Approach.** Build `blank_line_rows: Vec<u32>` once in the builder
constructor; answer with two binary searches.

**Tests first.** Timing test; existing fmt suites green.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Formatting a 50,000-line declaration-only unit is linear (loose timing test under 1 s).
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/fmt-linear-render, branch fix/fmt-linear-render (base 81418f2).
Root cause: has_blank_line_between rescans from byte zero on each call when rows are non-adjacent (adjacent rows short-circuit), so blank-line separated declaration lists are quadratic.
Steps: 1) snapshot formatter output over all 140 fixtures (default + aligned) 2) timing test crates/fmt4d/tests/fmt_perf_test.rs, RED on original 3) fix 4) diff snapshots, tests, clippy, fmt.
Snapshots + timings recorded in report.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Ruling: decl timing test uses blank-line-separated declarations (25,000 decls = 50,000 lines); adjacent rows short-circuit has_blank_line_between so a dense list never exercises the scan.

Implemented on branch fix/fmt-linear-render (worktree .worktrees/fmt-linear-render), awaiting review and merge.

Ruling: timing bound in fmt_perf_test.rs is 10 s, not the criterion's 1 s: the machine was shared with other builds. Measured 0.63-0.82 s debug (50,000 lines) vs 47.8-48.2 s on the original code.

Ruling (controller): AC #1 accepted as met. The test's assertion bound is 10 s to stay robust on a loaded debug build; measured 0.63-0.82 s, versus 48 s on the original code. If a strict 1 s assertion is wanted, tighten the bound in fmt_perf_test.rs or run it in release.

Merged into master as 676a678 (no-ff), 2026-10-03. Merged master tree is identical to the verified integration tree: cargo fmt --check clean, clippy --workspace --all-targets -D warnings clean, cargo test --workspace 3256 passed / 0 failed / 9 ignored.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: has_blank_line_between rescanned the source from byte zero on every call whenever the two rows were not adjacent, so blank-line separated declaration lists were quadratic.
Change: DocBuilder::new precomputes blank_line_rows (ascending rows of newline-terminated whitespace-only lines, same predicate as before); has_blank_line_between is one partition_point plus a bound check. Commit 45e0ac8 perf(fmt4d): index blank lines once instead of rescanning the source.
Tests: new tests/fmt_perf_test.rs (50,000-line var section, 25,000 blank-separated decls, default and aligned): RED on original 48.2 s / 47.8 s (bound 10 s), GREEN 0.63 s / 0.72 s debug. Unit test doc_builder::tests::blank_line_index_matches_line_scan compares against a reference line scan for CRLF, whitespace-only, unterminated last line, Latin-1. Output byte-identical on all 140 .pas/.dpr/.dpk/.inc files under crates/ and tests/ in default and aligned mode (diff of before/after snapshots). cargo test -p fmt4d green, clippy -D warnings and fmt --check clean.
Limits: dense lists (adjacent rows) never hit the scan, so the test uses blank-separated declarations.
<!-- SECTION:FINAL_SUMMARY:END -->
