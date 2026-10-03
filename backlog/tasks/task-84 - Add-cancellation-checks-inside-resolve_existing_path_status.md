---
id: TASK-84
title: Add cancellation checks inside resolve_existing_path_status
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:28'
updated_date: '2026-10-03 21:55'
labels:
  - agent-todos
  - workspace-perf
  - core
dependencies: []
modified_files:
  - crates/pascal-project/src/lib.rs
priority: medium
type: bug
ordinal: 84000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

Add cancellation checks inside `resolve_existing_path_status`; a
cancelled request can keep a worker busy for up to ~5 s.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/project-small-fixes, branch fix/project-small-fixes. resolve_existing_path_status (pascal-project/src/lib.rs) takes no cancellation and its callers are many; directory listing is thread-local cached via PathResolutionCacheScope. Plan: add a thread-local cancel scope (same pattern as the cache scope) installed in the discover entry point that already owns the cancel token and PathResolutionCacheScope; resolve_existing_path_status checks it at each component iteration and inside the directory-listing entry loop (every 1024 entries), returning a new ExistingPathStatus::Cancelled outcome (callers treat as unresolvable/not found). Test: set cancelled scope, resolve returns Cancelled promptly.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related review task: TASK-43 (CORE-6). Check it before starting to avoid duplicate work.

Ruling: cancelled walk returns Unresolvable rather than a new ExistingPathStatus variant — every caller already treats it as incomplete and ~12 match sites would otherwise change; the caller loop-boundary checks report the cancellation.

Ruling: thread-local cancel scope with a lifetime-bound raw pointer (one unsafe block, SAFETY documented) instead of threading a token through resolve_existing_path and ~10 callers (conditional parser, import processing, ReadPolicy construction). Reviewer may prefer plumbing; TASK-43's DirectoryProvider could replace it.

Implemented on branch fix/project-small-fixes (worktree .worktrees/project-small-fixes), awaiting review and merge.

Fix round 1: with_path_resolution_cancel(cancel, run) replaces the public guard (guard private, drop order enforced by the call stack; SAFETY comment rewritten; None keeps the outer token). discover_context_with_selections is now a wrapper around _inner inside that scope, and checks work_budget then the token before returning Ok, so a token set during token-less standalone builds yields 'request cancelled'. Commit 2c6b67f. Regression test discovery_never_returns_ok_after_the_token_was_set (RED at budget call 25 before the fix; the test sweeps 120 points).

Ruling (fix 1): the final token check follows the budget check, since a token set after the last check is a valid post-completion cancel.

Follow-up for other entry points: TASK-101

Merged into master as 87e5b1e (no-ff), 2026-10-03. Merged master tree is identical to the verified integration tree: cargo fmt --check clean, clippy --workspace --all-targets -D warnings clean, cargo test --workspace 3256 passed / 0 failed / 9 ignored.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: resolve_existing_path_status never checked cancellation. Change: private-guard helper with_path_resolution_cancel (thread-local pointer scoped to the call stack, one documented unsafe read) wrapped around discover_context_with_selections; walk checks before each component and every 1024 listing entries (cancelled listings are never cached); a cancelled walk returns Unresolvable; discovery returns 'request cancelled' instead of Ok if the token was set at any point, so silently dropped paths are never returned. Commits e92c5b5, 2c6b67f. Tests: pre-cancelled walk lists nothing, cancelled listing not cached, nesting/restore incl. None keeps outer, discovery_never_returns_ok_after_the_token_was_set (RED before wrapper). pascal-project suites, fmt, clippy -D warnings pass. Limits: entry points outside discover_context_with_selections do not install the scope (follow-up TASK-101).
<!-- SECTION:FINAL_SUMMARY:END -->
