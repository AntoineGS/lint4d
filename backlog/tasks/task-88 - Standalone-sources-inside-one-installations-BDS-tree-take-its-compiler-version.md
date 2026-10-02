---
id: TASK-88
title: >-
  Standalone sources inside one installation's BDS tree take its compiler
  version
status: Done
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - rtl-units
  - core
dependencies: []
type: enhancement
ordinal: 90000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Delphi RTL Units (Classes.pas)".

Section context: Classes.pas from the D2010 installation reported "include expansion was incomplete; lint diagnostics were withheld" and navigation inside it was unavailable.

Original item:

Standalone sources inside exactly one configured installation's `BDS`
tree take that installation's compiler version and profile properties
(`Platform`). Classes.pas now gets 150 lint diagnostics. (`09eb6d8`,
merged in `04c5ec9`)
<!-- SECTION:DESCRIPTION:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Standalone sources inside exactly one configured installation's `BDS`
tree take that installation's compiler version and profile properties
(`Platform`). Classes.pas now gets 150 lint diagnostics. (`09eb6d8`,
merged in `04c5ec9`)
<!-- SECTION:FINAL_SUMMARY:END -->
