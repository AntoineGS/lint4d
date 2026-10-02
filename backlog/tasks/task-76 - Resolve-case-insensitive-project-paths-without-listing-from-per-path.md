---
id: TASK-76
title: Resolve case-insensitive project paths without listing from / per path
status: Done
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - workspace-perf
  - core
dependencies: []
type: enhancement
ordinal: 76000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

Resolve case-insensitive project paths without listing every directory
from `/` per path: exact-path fast path plus a per-discovery listing
cache, shared via `Arc`. (`1645002`)
<!-- SECTION:DESCRIPTION:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Resolve case-insensitive project paths without listing every directory
from `/` per path: exact-path fast path plus a per-discovery listing
cache, shared via `Arc`. (`1645002`)
<!-- SECTION:FINAL_SUMMARY:END -->
