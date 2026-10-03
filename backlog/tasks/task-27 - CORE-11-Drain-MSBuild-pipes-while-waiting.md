---
id: TASK-27
title: 'CORE-11: Drain MSBuild pipes while waiting'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 21:55'
labels:
  - arch-review
  - core
  - runtime
milestone: m-3
dependencies: []
modified_files:
  - crates/pascal-core/src/discovery_msbuild.rs
priority: medium
type: bug
ordinal: 27000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-11). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: medium. Cost: runtime.

**Problem.** `discovery_msbuild.rs:315-345` (`wait_with_timeout`) pipes
stdout and stderr but reads them only after `try_wait` reports exit. If
either pipe fills, MSBuild blocks and the 15 s timeout fires. Timeout
kills the `cmd` child without reaping or handling descendants
(`:245-250`).

**Approach.** Spawn two reader threads at launch that read to a bounded
buffer (1 MiB each, then discard with a flag); join them after exit. On
timeout, kill and `wait()` the child; on Windows use a job object or
`taskkill /T` for the tree.

**Tests first.** That test with a small helper script (fails today).

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A test child that writes 4 MiB to stdout then exits completes without timeout.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/project-small-fixes, branch fix/project-small-fixes. Root cause: wait_with_timeout (pascal-core/src/discovery_msbuild.rs) polls try_wait and reads pipes only after exit; >64 KiB output blocks the child until the 15 s timeout, and the timeout kill does not wait(). Steps: (1) make wait_with_timeout compile on all platforms (allow dead_code off-Windows) and add a unix test spawning a child writing 4 MiB to stdout and one that hangs; RED on old code; (2) spawn two reader threads at launch reading to a 1 MiB bounded buffer (continuing to drain and discarding), join after exit; on timeout kill + wait + join; (3) Windows tree kill deferred unless checkable (no windows target installed -> follow-up task).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Ruling: Windows process-tree kill deferred — no Windows target installed, code could not be compiled or tested here; follow-up task created.

Note: wait_with_timeout lives in crates/pascal-core/src/discovery_msbuild.rs (not pascal-project); the task's :315-345 citation is stale (now near the bottom of file). The function was ungated from cfg(windows) (dead_code allowed off-Windows) so it can be tested on Linux. On timeout readers are not joined, so descendants holding pipes cannot hang the caller.

Implemented on branch fix/project-small-fixes (worktree .worktrees/project-small-fixes), awaiting review and merge.

Fix round 1: drain_pipe returns (bytes, truncated); wait_with_timeout returns (Output, stdout_truncated). Ruling: discover_dcu_paths_via_msbuild treats stdout truncation as a failure (warning on stderr, empty Vec) like its other MSBuild failures, instead of returning a partial DCU list. The Windows-only caller is not compiled on Linux; reviewed by eye. Commit f612b39; the 4 MiB test asserts the flag.

Merged into master as 87e5b1e (no-ff), 2026-10-03. Merged master tree is identical to the verified integration tree: cargo fmt --check clean, clippy --workspace --all-targets -D warnings clean, cargo test --workspace 3256 passed / 0 failed / 9 ignored.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: wait_with_timeout read pipes only after exit, so >64 KiB output blocked the child until the timeout; timeout did not reap. Change: reader threads from launch keep 1 MiB per pipe, discard the rest and flag truncation; kill+wait on timeout; stdout truncation makes discover_dcu_paths_via_msbuild warn and return no paths. Commits e8727cb, f612b39. Tests: 4 MiB stdout + 2 MiB stderr completes with truncation flag set; hung child times out (RED: timeout on old code). pascal-core lib 104 pass, fmt/clippy clean. Limits: Windows tree kill deferred (TASK-98); Windows-only caller unverified here.
<!-- SECTION:FINAL_SUMMARY:END -->
