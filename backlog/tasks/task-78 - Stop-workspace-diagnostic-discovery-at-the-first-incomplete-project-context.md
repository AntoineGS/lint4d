---
id: TASK-78
title: Stop workspace/diagnostic discovery at the first incomplete project context
status: Done
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - workspace-perf
  - lsp
dependencies: []
type: enhancement
ordinal: 78000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

Stop `workspace/diagnostic` discovery at the first source with an
incomplete project context (`build_rejectable_workspace_snapshot`).
On `multidev` it now fails in ~1 s instead of 8+ minutes. The
`references`/`rename` early exit for an incomplete requested-file
context already existed. (`41876a3`)
<!-- SECTION:DESCRIPTION:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Stop `workspace/diagnostic` discovery at the first source with an
incomplete project context (`build_rejectable_workspace_snapshot`).
On `multidev` it now fails in ~1 s instead of 8+ minutes. The
`references`/`rename` early exit for an incomplete requested-file
context already existed. (`41876a3`)
<!-- SECTION:FINAL_SUMMARY:END -->
