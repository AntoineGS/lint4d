---
id: TASK-104
title: 'fmt4d: uses clause ending in a directive loses its semicolon'
status: To Do
assignee: []
created_date: '2026-10-03 03:17'
labels:
  - fmt
  - correctness
dependencies: []
priority: medium
type: bug
ordinal: 106000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while working on TASK-26 (not caused by it).

Repro (default config):
```pascal
unit T;
interface
uses A {$I x.inc};
implementation
end.
```
First pass output:
```pascal
uses
  A,
  {$I x.inc}
```
There is no `;`, so the output no longer parses: the second pass fails with a parse-error diagnostic.

Cause: layout_uses_items in crates/fmt4d/src/uses.rs (formerly format_uses_items) gives the clause's `;` to the last non-separator slot (`last_real_idx`). When that slot is a pinned UsesItem::Directive, emit_uses_item ignores is_last for directives, so no unit gets the `;`. TASK-26 made pinned comments skip that role; directives still take it.

Expected: the `;` goes to the last unit or ifdef block, with any directives anchored after it emitted after the `;` or before it, as long as the result still parses as the same clause. Check that the directive stays inside the clause on re-parse, so the output is idempotent.

Done when: a regression test in fmt_bugs_test.rs formats the repro, the output parses, and the formatter is idempotent on it.
<!-- SECTION:DESCRIPTION:END -->
