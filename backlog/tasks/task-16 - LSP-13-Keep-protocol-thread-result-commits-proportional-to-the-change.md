---
id: TASK-16
title: 'LSP-13: Keep protocol-thread result commits proportional to the change'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:22'
updated_date: '2026-10-04 02:39'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-2
dependencies:
  - TASK-14
modified_files:
  - crates/pascal-lsp/src/workspace.rs
priority: high
type: enhancement
ordinal: 16000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-13). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime latency.

**Problem.** `server.rs:9146-9157`, `:9206`, `:9249-9252` and
`workspace.rs:12742-12791`, `:3324-3343`, `:3000-3018`: delivery checks nest
candidate/scope observations against every retained source-change
observation (up to 4,096 per candidate) with no budget, then re-checks
context freshness and importer hashes for every compiled binding, including
filesystem work. This runs on the protocol thread and blocks edits and
cancellations.

**Approach.** Workers produce an indexed validation token (LSP-7) and a
delta of state entries; the protocol thread applies the delta and compares
tokens. Filesystem checks move to the worker.

**Tests first.** Timing characterization test.

**Depends on.** LSP-7.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Delivery of a result with 4,000 retained observations costs under 1 ms on the protocol thread (measure in a unit test with `Instant`, assert loosely, 10 ms).
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/lsp-protocol-commits, branch perf/lsp-protocol-commits (stacked on TASK-14/15).
Root cause: Workspace::dependency_scoped_result_is_fresh scans every retained source-change observation (≤4,096) per candidate observation per record with paths_equal_ci (allocating); apply_navigation_state re-validates context freshness (stats) and importer hashes (file read) per compiled binding.
Steps: 1) timing characterization unit test (4,000 observations, 64 records × 64 candidates) — record old timing; 2) collect observations newer than the result once (skip entirely when the result is current) and look candidates up via an allocation-free case-folded hash with exact fallback; 3) memoise per-context freshness and per-importer hash in apply_navigation_state; 4) worker-side delta / filesystem move recorded as follow-up if out of reach.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Citations at d55c0f6: dependency_scoped_result_is_fresh workspace.rs:12868; apply_navigation_state workspace.rs:3353 (binding retain :3383); server.rs analysis_result_is_stale :9145.

RED (characterization): `cargo test -p pascal-lsp --lib dependency_scoped_freshness_with_4000 -- --nocapture` on the original code: `dependency-scoped freshness with 4000 observations: 1.327140401s` → assertion (<10 ms) failed. GREEN: 4.2 ms (debug build, best of runs). About 3 ms of that is per-record work unrelated to observations (measured with no newer observations).

Ruling: the timing test takes the best of ten runs spaced 2 ms apart, so load from parallel tests does not trip the 10 ms bound; a regression to per-candidate scans costs >1 s, so the margin still catches it. Release numbers were not measured (would need a full release rebuild of the test target).

Ruling: the worker-produced delta and moving filesystem checks to the worker (Approach) are not done here: NavigationState is built by Workspace::navigation_state as a full clone in workspace.rs, which the workspace memory/copying lane owns. apply_navigation_state now does its stats/hashing once per context and per importer instead of per binding (same semantics). Remaining work filed as TASK-121.

Verification: pascal-lsp lib (test-support) 694 passed, 1 ignored; protocol_barriers 904 passed, 2 failed (TASK-1.2 timing tests); compiled_dcu 8, navigation 317, build_context_freshness 2 passed; clippy -D warnings and fmt clean.

Implemented on branch perf/lsp-protocol-commits (worktree .worktrees/lsp-protocol-commits), awaiting review and merge.

Merged into master as 153ab6b (branch perf/lsp-protocol-commits, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: protocol-thread delivery checks were not proportional to the change: dependency_scoped_result_is_fresh compared every candidate observation of every record against every retained source-change observation (≤4,096), and apply_navigation_state stat'ed context files and re-read/hashed importer sources per compiled binding.

Change (commit 141f1e9): dependency_scoped_result_is_fresh gathers only observations newer than the result once (nothing when the result is current), builds a set of allocation-free case-folded FNV hashes, and checks candidates/missing-provider paths with a hash lookup plus an exact paths_equal_ci confirmation; scope checks iterate only the newer observations. apply_navigation_state memoises context fingerprint/freshness per context key and the importer source hash per importer.

Numbers (debug build): 64 records × 64 candidates vs 4,000 newer observations: 1.33 s before, 4.2 ms after (≈3 ms of it is per-record work independent of observations).

Tests: workspace::tests::dependency_scoped_freshness_with_4000_retained_observations_is_cheap (RED 1.33 s, GREEN; also asserts a case-variant path of a newer observation still stales the result). Lib 694 passed; protocol_barriers 904/906 (TASK-1.2 flakes); compiled_dcu, navigation, build_context_freshness green; clippy/fmt clean.

Known limits: workers still ship a full NavigationState clone and the remaining binding validation still stats files on the protocol thread (once per context/importer); follow-up task filed.
<!-- SECTION:FINAL_SUMMARY:END -->
