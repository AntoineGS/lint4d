---
id: TASK-80
title: Skip drive-letter/UNC sourcePaths in the workspace walker on non-Windows
status: Done
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - workspace-perf
  - lsp
dependencies: []
type: bug
ordinal: 80000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

`sourcePaths` such as `C:/DelphiSources/rtl/sys` were joined onto the
workspace root by the workspace walker (`.../multidev/C:/DelphiSources/
...`), so every workspace-wide snapshot on the user's setup was
incomplete before discovery started. On non-Windows hosts the walker
now skips drive-letter/UNC entries; project discovery already maps
them per installation and `enumerate_mapped_sources` walks them per
context. `workspace/diagnostic` on `multidev` now reports the real
reason (an ambiguous project context) in 0.8 s. (`5bdf66b`)
<!-- SECTION:DESCRIPTION:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
`sourcePaths` such as `C:/DelphiSources/rtl/sys` were joined onto the
workspace root by the workspace walker (`.../multidev/C:/DelphiSources/
...`), so every workspace-wide snapshot on the user's setup was
incomplete before discovery started. On non-Windows hosts the walker
now skips drive-letter/UNC entries; project discovery already maps
them per installation and `enumerate_mapped_sources` walks them per
context. `workspace/diagnostic` on `multidev` now reports the real
reason (an ambiguous project context) in 0.8 s. (`5bdf66b`)
<!-- SECTION:FINAL_SUMMARY:END -->
