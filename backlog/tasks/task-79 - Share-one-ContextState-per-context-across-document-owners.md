---
id: TASK-79
title: Share one ContextState per context across document owners
status: Done
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - workspace-perf
  - lsp
dependencies: []
type: enhancement
ordinal: 79000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

Share one `ContextState` per context across document owners
(`Arc`, reused only while equal to the current context, released with
the last owner), and stop deep-cloning contexts per source in
`discover_enumerated_contexts`. RSS at 30 s: 2.4 GB -> 1.2 GB; at
270 s: 6.2 GB -> 4.8 GB (references on a public symbol). Remaining
growth is the indexing phase. (`20f25ad`)
<!-- SECTION:DESCRIPTION:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Share one `ContextState` per context across document owners
(`Arc`, reused only while equal to the current context, released with
the last owner), and stop deep-cloning contexts per source in
`discover_enumerated_contexts`. RSS at 30 s: 2.4 GB -> 1.2 GB; at
270 s: 6.2 GB -> 4.8 GB (references on a public symbol). Remaining
growth is the indexing phase. (`20f25ad`)
<!-- SECTION:FINAL_SUMMARY:END -->
