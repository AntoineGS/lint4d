---
id: TASK-90
title: Remove stale bare raise; ERROR suppression (is_bare_raise_error)
status: To Do
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - rtl-units
  - core
dependencies: []
priority: low
type: chore
ordinal: 92000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Delphi RTL Units (Classes.pas)".

Section context: Classes.pas from the D2010 installation reported "include expansion was incomplete; lint diagnostics were withheld" and navigation inside it was unavailable.

Original item:

lint4d's bare `raise;` ERROR suppression (`is_bare_raise_error` in
`pascal-core/src/parser.rs`) is stale: the grammar parses bare
`raise;` without errors.
<!-- SECTION:DESCRIPTION:END -->
