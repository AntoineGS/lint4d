---
id: TASK-77
title: Reuse project discovery for sibling sources within one request
status: Done
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - workspace-perf
  - lsp
dependencies: []
type: enhancement
ordinal: 77000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

Reuse project discovery for sibling sources within one request, with
freshness checks, and stop pruning contexts once per file. (`baf5774`;
still misses for many files, see below)
<!-- SECTION:DESCRIPTION:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Reuse project discovery for sibling sources within one request, with
freshness checks, and stop pruning contexts once per file. (`baf5774`;
still misses for many files, see below)
<!-- SECTION:FINAL_SUMMARY:END -->
