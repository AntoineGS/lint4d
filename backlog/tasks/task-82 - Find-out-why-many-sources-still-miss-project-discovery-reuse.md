---
id: TASK-82
title: Find out why many sources still miss project discovery reuse
status: To Do
assignee: []
created_date: '2026-10-02 23:28'
updated_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - workspace-perf
  - lsp
dependencies: []
priority: medium
type: spike
ordinal: 82000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

Find out why many sources still miss discovery reuse (profiling at
120 s and 300 s still shows per-file `build_project_context`).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related review task: TASK-20 (LSP-15). Check it before starting to avoid duplicate work.

Related review task: TASK-33 (LSP-16). Check it before starting to avoid duplicate work.
<!-- SECTION:NOTES:END -->
