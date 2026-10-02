---
id: TASK-91
title: Treat %40 and @ in file URIs as the same document
status: To Do
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - rtl-units
  - lsp
dependencies: []
priority: medium
type: bug
ordinal: 93000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Delphi RTL Units (Classes.pas)".

Section context: Classes.pas from the D2010 installation reported "include expansion was incomplete; lint diagnostics were withheld" and navigation inside it was unavailable.

Original item:

The server treats `file:///...%40...` and `file:///...@...` as
different documents: an open document whose URI encodes `@` is reported
as "disappeared" by `dependency_scoped_result_is_fresh`, so every pull
is stale. nvim sends a raw `@`, so it is unaffected.
<!-- SECTION:DESCRIPTION:END -->
