---
id: TASK-89
title: Fix grammar gaps exposed by Classes.pas in tree-sitter-pascal
status: Done
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - rtl-units
  - grammar
  - cross-repo
dependencies: []
type: bug
ordinal: 91000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Delphi RTL Units (Classes.pas)".

Section context: Classes.pas from the D2010 installation reported "include expansion was incomplete; lint diagnostics were withheld" and navigation inside it was unavailable.

Original item:

Grammar gaps in `../tree-sitter-pascal` exposed by Classes.pas:
subrange constant-expression bounds, `raise E at ReturnAddr`, and
routine attribute keywords (`Default`, `Local`, ...) as variable
names. Merged in tree-sitter-pascal `2167e95`; Classes.pas has no
parse errors with it. Generate with tree-sitter-cli 0.24.7 (the
locked version): the system 0.26.9 CLI rewrites every generated file.
<!-- SECTION:DESCRIPTION:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Grammar gaps in `../tree-sitter-pascal` exposed by Classes.pas:
subrange constant-expression bounds, `raise E at ReturnAddr`, and
routine attribute keywords (`Default`, `Local`, ...) as variable
names. Merged in tree-sitter-pascal `2167e95`; Classes.pas has no
parse errors with it. Generate with tree-sitter-cli 0.24.7 (the
locked version): the system 0.26.9 CLI rewrites every generated file.
<!-- SECTION:FINAL_SUMMARY:END -->
