---
id: TASK-75
title: Treat interface/forward-declared routine parameters as local
status: Done
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - workspace-perf
  - lsp
dependencies: []
type: bug
ordinal: 75000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

Treat parameters of `interface`/forward-declared routines and class
methods as local, so references, rename and highlighting stay in the
file. (`05c2c79`)
<!-- SECTION:DESCRIPTION:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Treat parameters of `interface`/forward-declared routines and class
methods as local, so references, rename and highlighting stay in the
file. (`05c2c79`)
<!-- SECTION:FINAL_SUMMARY:END -->
