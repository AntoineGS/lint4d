---
id: TASK-84
title: Add cancellation checks inside resolve_existing_path_status
status: To Do
assignee: []
created_date: '2026-10-02 23:28'
updated_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - workspace-perf
  - core
dependencies: []
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related review task: TASK-43 (CORE-6). Check it before starting to avoid duplicate work.
<!-- SECTION:NOTES:END -->
