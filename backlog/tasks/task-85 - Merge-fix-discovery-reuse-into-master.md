---
id: TASK-85
title: Merge fix/discovery-reuse into master
status: Done
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - workspace-perf
  - lsp
dependencies: []
type: chore
ordinal: 85000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

Merge `fix/discovery-reuse` into master (fast-forward to `5bdf66b`,
history rewritten from the `wip:` checkpoints).
<!-- SECTION:DESCRIPTION:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Merge `fix/discovery-reuse` into master (fast-forward to `5bdf66b`,
history rewritten from the `wip:` checkpoints).
<!-- SECTION:FINAL_SUMMARY:END -->
