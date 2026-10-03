---
id: TASK-62
title: 'FMT-4: Fill rendering without `Vec::remove(0)`'
status: Deferred
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
  - crates/fmt4d/src/doc.rs
  - crates/fmt4d/src/renderer.rs
priority: medium
type: enhancement
ordinal: 62000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (FMT-4). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: medium. Cost: runtime.

**Problem.** `renderer.rs:206-216` removes the first two elements of a Fill
and requeues the rest, shifting the vector each iteration.

**Approach.** Render Fill through an index cursor `(parts, next_idx)` on the
stack.

**Tests first.** Timing test; `fmt_line_break_test.rs` green.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A 5,000-element expression chain renders in linear time.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/fmt-linear-render, branch fix/fmt-linear-render (base 81418f2).
Root cause: Fill rendering does parts.remove(0) twice per pair and requeues Doc::Fill(rest), memmoving the remaining parts every iteration (quadratic).
Steps: 1) snapshot formatter output over all 140 fixtures (default + aligned) 2) timing test crates/fmt4d/tests/fmt_perf_test.rs, RED on original 3) fix 4) diff snapshots, tests, clippy, fmt.
Snapshots + timings recorded in report.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Ruling: chain timing test uses 20,000 elements, not 5,000 — at 5,000 the quadratic memmove cost is only ~1.1s (release and debug), too close to any safe bound; 20,000 takes 19s release / 27s debug on original code, and is a superset of the criterion.

Implemented on branch fix/fmt-linear-render (worktree .worktrees/fmt-linear-render), awaiting review and merge.

Ruling: the cursor is a renderer-internal Doc::FillRest(vec::IntoIter<Doc>) variant (doc(hidden)), because the tail-walk in fits_group_with_rest must still see the rest of the Fill on the stack; as_slice() gives it the remainder. Builders never produce it.

Finding: the end-to-end 5,000-element chain is NOT dominated by Fill. Phase timing (release, 10k operands): parse 17 ms, maps 10 ms, DocBuilder::build 4.0 s, render 2 ms. Cause is Node::parent() in build_leaf (O(depth) on a left-nested chain). Filed as TASK-105. Acceptance #1 is therefore left unchecked: Fill rendering is linear (renderer test) but the whole format of a long chain is still quadratic.

Correction (controller, from task review): the timing test that landed is an 80,000-element renderer-level Fill test in crates/fmt4d/src/renderer.rs, not a 20,000-element chain test in fmt_perf_test.rs as noted above. AC #2 stands: that test was written first and failed on the original renderer (32.7 s) before passing (54 ms). AC #1 (end-to-end chain) remains open, blocked by TASK-105.

Merged into master as 676a678 (no-ff), 2026-10-03. Merged master tree is identical to the verified integration tree: cargo fmt --check clean, clippy --workspace --all-targets -D warnings clean, cargo test --workspace 3256 passed / 0 failed / 9 ignored.

Deferred: the Fill renderer change is merged and AC #2 is met. AC #1 (a 5,000-element chain renders in linear time end to end) is blocked by TASK-105 (Node::parent() in build_leaf is O(depth)). Unblock: land TASK-105, then add the end-to-end chain timing test and check AC #1.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: renderer Fill handling did parts.remove(0) twice per pair and requeued the rest as a new Doc::Fill, shifting the vector on every pair.
Change: Fill is wrapped once into Doc::FillRest(vec::IntoIter<Doc>) and drained with next(); the fit checks (fits_inner, fits_stack_item, measure_width_inner) read the remainder through as_slice(). Commit 31cdaed perf(fmt4d): drain Fill through an iterator instead of Vec::remove(0).
Tests: unit test renderer::tests::fill_with_many_parts_renders_in_linear_time (80,000-element Fill, bound 5 s): RED 32.7 s on original (20,000 elements: 1.04 s), GREEN 54 ms. Output byte-identical on 140 fixture files, default and aligned. cargo test -p fmt4d (incl. fmt_line_break_test) green, clippy and fmt clean.
Limits: end-to-end formatting of a long operator chain is still quadratic in DocBuilder (TASK-105); no end-to-end chain test was added for that reason.
<!-- SECTION:FINAL_SUMMARY:END -->
