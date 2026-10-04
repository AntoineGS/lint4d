---
id: TASK-137
title: >-
  fmt4d: punctuation after a directive followed by a unit in a uses clause is
  dropped or invented
status: To Do
assignee: []
created_date: '2026-10-04 22:49'
labels:
  - fmt
  - correctness
milestone: m-3
dependencies:
  - TASK-128
priority: medium
type: bug
ordinal: 139000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while fixing TASK-128 (branch fix/fmt-punctuation-trivia); pre-existing on master 9322cce.

TASK-128 keeps a ',' that sat before a directive (UsesItem::Directive.after_comma, restore_commas_before_directives in crates/fmt4d/src/uses.rs). The ',' *after* a directive is still not recorded, and a unit before a directive always gets a ',' when units follow, so a directive followed by a unit in the output can read wrongly. An include is opaque, so the source punctuation around it carries meaning. Default config (sorting on) unless noted:

- 'uses A, {$I u.inc}, B;' -> 'A,' / '{$I u.inc}' / 'B;'. If u.inc holds 'X' the source reads 'A, X, B' and the output 'A, X B' (does not compile).
- 'uses {$I u.inc}, B;' -> '{$I u.inc}' / 'B;' (same: the ',' after the directive is lost).
- 'uses A {$I u.inc}, B;' -> 'A,' / '{$I u.inc}' / 'B;'. If u.inc holds ', X' the source reads 'A, X, B' and the output 'A, , X B'.
- Sorting: 'uses B, A, {$I u.inc};' -> 'A,' / '{$I u.inc}' / 'B;'. The directive is pinned after its anchor A, so B now follows it without a ','. Source 'B, A, X;' becomes 'A, X B;'.

Likely shape of the fix: record the punctuation after each directive too (comma_after), give a directive followed by a unit or block its source ','; do not add a ',' to a unit before a directive that had none in the source; under sorting, keep a directive that ended the source list at the end of the clause (like the block handling in layout_uses_items), so its ';' context is kept.

Tests first: fmt_bugs_test.rs regressions asserting the output shapes (the oracle cannot see inside the include, though it does compare punctuation per configuration), sorting on and off, parse and idempotency; a uses fixture.
<!-- SECTION:DESCRIPTION:END -->
