---
id: TASK-135
title: Fix pascal-lsp tests failing on Windows CI after the 2026-10-03 merge
status: Done
assignee:
  - '@claude'
created_date: '2026-10-04 03:51'
updated_date: '2026-10-04 13:17'
labels:
  - lsp
  - build
  - windows
milestone: m-0
dependencies: []
modified_files:
  - crates/pascal-lsp/src/project_cache.rs
  - crates/pascal-lsp/src/workspace.rs
priority: high
type: bug
ordinal: 137000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Master 912d7cb, CI run 37171876568: Test (windows-latest) fails; every other job (incl. macOS and Linux tests) is green. The perf/lsp-workspace-memory (TASK-13/18/10/11/12) and perf/lsp-protocol-commits (TASK-14/15/16/8) branches never ran on Windows CI before merging (they were not PRs).

Failures (pascal-lsp lib tests, 696 passed / 6 failed):
- project_cache::cache_tests::a_leased_parse_nothing_keeps_is_not_charged — project_cache.rs:1611 'the lease does not keep the parse'
- project_cache::cache_tests::a_re_cached_parse_is_charged_by_its_entry_again — :1661 left 0 right 10
- project_cache::cache_tests::leasing_a_parse_evicted_after_its_hit_keeps_the_budget — :1632 left 0 right 1904
- project_cache::cache_tests::pinning_a_ready_entry_counts_it_as_pinned_until_unpinned — :1312 'an evicted pinned entry no longer counts'
- project_cache::cache_tests::watcher_calls_run_without_the_cache_lock — :1408 expected [watch /ws/includes, unwatch /ws/includes], got only the watch
- workspace::tests::dependency_scoped_freshness_with_4000_retained_observations_is_cheap — workspace.rs:16755, 10.654 ms against a 10 ms wall-clock bound (TASK-16)

Likely areas: Unix-style fixture paths (/ws/...) are not absolute on Windows; interaction between TASK-13's path-keyed indexes / TASK-10 leases and the Windows path identity (TASK-94/TASK-74 path_identity, path_lookup_key folds case on case-insensitive volumes); the TASK-16 test asserts wall-clock time, which the project avoids.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Root cause recorded for each failing test; product bugs fixed with a regression test, fixture-only problems fixed in the fixture with the reason recorded
- [x] #2 The TASK-16 freshness test asserts a work count (or similar deterministic measure) instead of a wall-clock bound
- [x] #3 All 11 CI jobs green on Windows, macOS and Linux
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/windows-lsp-tests, branch fix/windows-lsp-tests (from master 912d7cb). CI iteration through a draft PR (push authorised for this task).

Root causes (from code reading + CI log of run 37171876568):
1. The five project_cache::cache_tests failures share one cause: the fixture helper uri(name) builds file:///ws/<name>. On Windows, Url::to_file_path() rejects a drive-less file URL, so indexed_paths() (project_cache.rs) never indexes the entry's own path under state.exact, and invalidate_path(Path::new("/ws/A.pas")) evicts nothing. The pre-TASK-13 linear scan compared key.uri.to_file_path().ok() == Some(path) with the same result, so this is not a product regression: production URIs on Windows always carry a drive or UNC host. Fixture-only: make uri()/paths host-neutral (C: prefix on Windows, as include_expansion.rs tests already do).
2. workspace dependency_scoped_freshness test: 10.654 ms vs a 10 ms wall-clock bound on a slower Windows runner; the algorithm is unchanged. Replace the timing with a test-only thread-local counter of retained observations examined by dependency_scoped_result_is_fresh, asserting one pass (== OBSERVATIONS) for unrelated candidates rather than candidates x observations.

Steps: fix fixtures; add counter + rewrite assertion; fmt/clippy/focused tests locally; push, draft PR, iterate on CI until all 11 jobs green (max ~6 iterations).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Root cause (5 project_cache::cache_tests failures, one cause, fixture-only): cache_tests::uri() built file:///ws/<name>. On Windows Url::to_file_path() returns Err for a drive-less, host-less file URL, so indexed_paths() (project_cache.rs ~2934) never put the entry's own path in state.exact, and invalidate_path(Path::new("/ws/A.pas")) evicted nothing -> the pin/lease/watch assertions after the invalidation all fail. The pre-TASK-13 linear scan compared key.uri.to_file_path().ok() == Some(path) and would have failed identically; production URIs on Windows always carry a drive or UNC host, so no product change. Fix: ws_path() fixture helper rooted at C:\ws on Windows and /ws elsewhere (the include_expansion.rs test uri() precedent); uri() = Url::from_file_path(ws_path(name)); invalidate_path calls use ws_path(). Local reproduction on Linux: temporarily building uri() as file://host/ws/<name> (which also has no file path on Linux) gives the same 5 failures with the same assertions (left 0 right 10, watch without unwatch, etc.).

Root cause (dependency_scoped_freshness_with_4000_retained_observations_is_cheap): no product change; the best-of-ten timing was 10.654 ms against a 10 ms wall-clock bound on the Windows runner. Replaced with a test-only thread-local counter (TEST_FRESHNESS_OBSERVATION_VISITS in workspace.rs) incremented for every retained observation examined: the newer-observation filter pass and each paths_equal_ci comparison after a hash hit. Test asserts exactly OBSERVATIONS (4000) visits for 4096 unrelated candidates. Mutation check: removing the case_folded_path_hash prefilter makes the count 16,644,000 and the test fail.

Ruling: the watcher-dir paths (/ws/includes) and byte-size fixtures (/ws/deep..., /ws/ddd...) were left Unix-shaped - they are only map keys / lengths and never round-trip through a URI, so they behave the same on every host.

Follow-up filed: TASK-136 (project cache path indexes vs Windows spelling: drive-letter/case differences between watcher paths and URI-derived paths). Not a cause of these failures.

Commit 678f6f3 pushed to fix/windows-lsp-tests; draft PR https://github.com/AntoineGS/lint4d/pull/8 (CI run 37175637394).

CI run 37175637394 on PR #8: all 11 jobs green on the first iteration (Windows pascal-lsp lib: 702 passed, 0 failed).

Implemented on branch fix/windows-lsp-tests (worktree .worktrees/windows-lsp-tests), awaiting review and merge.

Merged into master as 678f6f3 (fast-forward of fix/windows-lsp-tests, PR #8), 2026-10-04. CI run 37175637394 on that commit: all 11 jobs green; the Windows pascal-lsp lib run had 702 passed / 0 failed, including all six previously failing tests. Task review: approved.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: master 912d7cb failed 6 pascal-lsp lib tests on Windows CI (run 37171876568).

Change (commit 678f6f3, branch fix/windows-lsp-tests, draft PR #8):
- project_cache::cache_tests (5 tests, fixture-only): uri() built file:///ws/<name>, which has no file path on Windows, so the entry's own path was never indexed and invalidate_path evicted nothing. New ws_path() helper roots fixtures at C:\ws on Windows and /ws elsewhere; uri() and the invalidate_path calls use it. The pre-TASK-13 scan had the same dependency on to_file_path, so production (URIs always carry a drive/UNC host) is unaffected.
- workspace dependency_scoped_freshness test: the 10 ms wall-clock bound is replaced by a test-only thread-local counter of retained observations examined; asserts exactly 4000 (one pass) for 4096 unrelated candidates.

Verification: local RED for the cache tests by building uri() as file://host/ws/... on Linux (same 5 failures, same assertions); GREEN after the fix. Mutation check on the counter: removing the hash prefilter gives 16,644,000 visits and fails. Local: fmt --check, clippy -p pascal-lsp --all-targets (and --all-features) -D warnings clean, pascal-lsp lib 736 passed. CI run 37175637394: all 11 jobs green; Windows pascal-lsp lib 702 passed, 0 failed, including all six tests.

Known limits: possible path-spelling mismatch in the cache indexes on Windows is filed as TASK-136 (not a cause of these failures).
<!-- SECTION:FINAL_SUMMARY:END -->
