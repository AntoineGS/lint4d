---
id: TASK-81
title: 'ReadPolicy::new_with_installation_roots adds <root>/C:/... as a read root'
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
priority: low
type: bug
ordinal: 81000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

`ReadPolicy::new_with_installation_roots` (`pascal-project`) has the
same `is_absolute()` check and adds `<root>/C:/...` as a configured
read root. Harmless (the directory never exists), but inconsistent.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/project-small-fixes, branch fix/project-small-fixes. Root cause: ReadPolicy::new_with_installation_roots (pascal-project/src/lib.rs ~311) uses source.is_absolute() so a Windows-style source path like C:/x is joined under each root on Linux. Fix: reuse is_windows_absolute (the helper from 8c96a75 'accept native Windows paths on Windows') alongside is_absolute. Test: ReadPolicy with source_paths ["C:/Delphi/lib"] and a root does not add <root>/C:/Delphi/lib as a read root.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related review task: TASK-52 (CORE-10). Check it before starting to avoid duplicate work.

Ruling: foreign Windows source paths are skipped (not authorized) rather than added as-is, since they cannot be read on this host; the second loop already ignored them.

Implemented on branch fix/project-small-fixes (worktree .worktrees/project-small-fixes), awaiting review and merge.

Merged into master as 87e5b1e (no-ff), 2026-10-03. Merged master tree is identical to the verified integration tree: cargo fmt --check clean, clippy --workspace --all-targets -D warnings clean, cargo test --workspace 3256 passed / 0 failed / 9 ignored.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: ReadPolicy::new_with_installation_roots joined Windows-style source paths (C:/...) under each root on Linux, adding <root>/C:/... as a read root. Change: skip such paths using is_foreign_windows_path (the helper from 8c96a75), consistent with the workspace walker; native Windows is unaffected. Commit c3d263e. Test: read_policy_does_not_join_windows_source_paths_under_the_root (unix). RED: roots included <tmp>/C:/Delphi/lib. GREEN: cargo test -p pascal-project all pass, fmt/clippy clean.
<!-- SECTION:FINAL_SUMMARY:END -->
