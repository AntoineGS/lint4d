---
id: TASK-106
title: Remove the stale bare-raise ERROR branch in the raise-in-destructor rule
status: To Do
assignee: []
created_date: '2026-10-03 03:27'
labels:
  - core
  - lint4d
dependencies:
  - TASK-90
priority: low
ordinal: 108000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-up from TASK-90. crates/lint4d/src/rules/exception.rs:378-382 (find_unguarded_raises) still treats an ERROR node with a single kRaise child as a bare re-raise, via is_bare_raise (exception.rs:404). The pinned tree-sitter-pascal (22cf861) parses bare `raise;` as a normal `raise` node in every context tested by pascal-core's grammar_parses_bare_raise_without_error_nodes (crates/pascal-core/src/parser.rs), so this branch is dead. Remove it (and is_bare_raise) after adding a raise-in-destructor test with a bare `raise;` outside try..except proving it is still reported via the K::RAISE branch.
<!-- SECTION:DESCRIPTION:END -->
