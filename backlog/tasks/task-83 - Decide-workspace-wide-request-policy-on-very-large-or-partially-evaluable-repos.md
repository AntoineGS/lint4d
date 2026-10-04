---
id: TASK-83
title: >-
  Decide workspace-wide request policy on very large or partially evaluable
  repos
status: To Do
assignee: []
created_date: '2026-10-02 23:28'
updated_date: '2026-10-03 22:04'
labels:
  - agent-todos
  - workspace-perf
  - lsp
dependencies: []
priority: high
type: spike
ordinal: 83000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

Decide what workspace-wide requests should do on very large or
partially evaluable repos: partial results marked incomplete, treating
unknown projects differently, or making `workspace/diagnostic` opt-in.
Today references/rename on public symbols cannot work on `multidev`.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related review task: TASK-6 (LSP-1). Check it before starting to avoid duplicate work.

Related review task: TASK-33 (LSP-16). Check it before starting to avoid duplicate work.

Decision (user, 2026-10-03): workspace-wide requests return partial results marked incomplete (the TASK-6 / LSP-1 direction) rather than failing. The user is also considering a persistent on-disk cache (files or SQLite) for very large repos such as multidev, so that full results become reachable across sessions; that design is not settled and needs a brainstorming session with the user before any work.
<!-- SECTION:NOTES:END -->
