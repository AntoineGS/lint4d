---
id: TASK-81
title: 'ReadPolicy::new_with_installation_roots adds <root>/C:/... as a read root'
status: To Do
assignee: []
created_date: '2026-10-02 23:28'
updated_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - workspace-perf
  - core
dependencies: []
priority: low
type: bug
ordinal: 81000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

`ReadPolicy::new_with_installation_roots` (`pascal-project`) has the
same `is_absolute()` check and adds `<root>/C:/...` as a configured
read root. Harmless (the directory never exists), but inconsistent.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related review task: TASK-52 (CORE-10). Check it before starting to avoid duplicate work.
<!-- SECTION:NOTES:END -->
