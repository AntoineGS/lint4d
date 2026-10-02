---
id: TASK-93
title: Go to definition into an RTL unit should keep the importer's project context
status: To Do
assignee: []
created_date: '2026-10-02 23:28'
updated_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - rtl-units
  - lsp
dependencies:
  - TASK-91
priority: medium
type: bug
ordinal: 95000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Delphi RTL Units (Classes.pas)".

Section context: Classes.pas from the D2010 installation reported "include expansion was incomplete; lint diagnostics were withheld" and navigation inside it was unavailable.

Original item:

Inherited owners: go to definition into an RTL unit did not leave
the unit with its importer's project context when opened afterwards
(not reproduced end to end; definition requests were stale in the
probe because of the URI issue above).
<!-- SECTION:DESCRIPTION:END -->
