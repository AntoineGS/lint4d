---
id: TASK-92
title: >-
  Investigate frequent 'analysis result became stale' and source-map work limit
  in the nvim log
status: To Do
assignee: []
created_date: '2026-10-02 23:28'
updated_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - rtl-units
  - lsp
dependencies: []
priority: medium
type: spike
ordinal: 94000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Delphi RTL Units (Classes.pas)".

Section context: Classes.pas from the D2010 installation reported "include expansion was incomplete; lint diagnostics were withheld" and navigation inside it was unavailable.

Original item:

Find out why the user's nvim log shows frequent `analysis result became
stale` for semantic tokens and `include source-map work limit (1000000)
reached`.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related review task: TASK-19 (LSP-14). Check it before starting to avoid duplicate work.

Related review task: TASK-36 (LSP-19). Check it before starting to avoid duplicate work.
<!-- SECTION:NOTES:END -->
