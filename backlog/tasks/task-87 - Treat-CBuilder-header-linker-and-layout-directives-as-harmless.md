---
id: TASK-87
title: 'Treat C++Builder header, linker and layout directives as harmless'
status: Done
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - rtl-units
  - core
dependencies: []
type: enhancement
ordinal: 89000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Delphi RTL Units (Classes.pas)".

Section context: Classes.pas from the D2010 installation reported "include expansion was incomplete; lint diagnostics were withheld" and navigation inside it was unavailable.

Original item:

Treat C++Builder header, linker and layout directives (`NOINCLUDE`,
`EXTERNALSYM`, `HPPEMIT`, `NODEFINE`, `WEAKPACKAGEUNIT`, `{$L file}`,
`ALIGN`, `MINENUMSIZE`, ...) as harmless; `SCOPEDENUMS` and
`POINTERMATH` stay unsupported. (`fb472e0`)
<!-- SECTION:DESCRIPTION:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Treat C++Builder header, linker and layout directives (`NOINCLUDE`,
`EXTERNALSYM`, `HPPEMIT`, `NODEFINE`, `WEAKPACKAGEUNIT`, `{$L file}`,
`ALIGN`, `MINENUMSIZE`, ...) as harmless; `SCOPEDENUMS` and
`POINTERMATH` stay unsupported. (`fb472e0`)
<!-- SECTION:FINAL_SUMMARY:END -->
