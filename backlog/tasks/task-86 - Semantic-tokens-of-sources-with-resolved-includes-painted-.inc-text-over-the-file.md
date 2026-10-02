---
id: TASK-86
title: >-
  Semantic tokens of sources with resolved includes painted .inc text over the
  file
status: Done
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - workspace-perf
  - lsp
dependencies: []
type: bug
ordinal: 86000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

Merge `fix/semantic-tokens-includes` into master (fast-forward to
`575e497`; release binary not rebuilt yet): semantic tokens of a source
with a resolved `{$I ...}` were encoded against the include-expanded
text, painting `.inc` comments over the file (MDIBDatabase.pas with
`MDDatabaseXE3.dproj` selected). Tokens are now mapped back through the
expansion source map.
<!-- SECTION:DESCRIPTION:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Merge `fix/semantic-tokens-includes` into master (fast-forward to
`575e497`; release binary not rebuilt yet): semantic tokens of a source
with a resolved `{$I ...}` were encoded against the include-expanded
text, painting `.inc` comments over the file (MDIBDatabase.pas with
`MDDatabaseXE3.dproj` selected). Tokens are now mapped back through the
expansion source map.
<!-- SECTION:FINAL_SUMMARY:END -->
