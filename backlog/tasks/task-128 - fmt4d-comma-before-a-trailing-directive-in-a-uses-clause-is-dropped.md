---
id: TASK-128
title: 'fmt4d: comma before a trailing directive in a uses clause is dropped'
status: To Do
assignee: []
created_date: '2026-10-03 23:40'
labels:
  - fmt
  - correctness
dependencies:
  - TASK-104
priority: medium
ordinal: 130000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in review of branch fix/fmt-uses-fixes (TASK-104 fix). Before that branch the output was broken differently (the clause lost its ';').

Repro (default config):
```pascal
unit T;
interface
uses A, {$I units.inc};
implementation
end.
```
Output:
```pascal
uses
  A
  {$I units.inc};
```
If units.inc contains `B`, the source reads `uses A, B;` but the output reads `uses A B;`, which does not compile. The include is opaque to the formatter, so the source's punctuation around it carries meaning.

Cause: list_puncts in crates/fmt4d/src/uses.rs (the TASK-104 rule): when directives follow the clause's last unit, that unit gets no punctuation and the last directive carries the ';'. extract_uses_items does not record whether a ',' sat between the unit and the directive (commas are only read for their comments).

Expected: a unit followed by ',' and then an include in the source keeps its ',' (`A,` / `{$I units.inc};`), while `uses A {$I x.inc};` keeps the TASK-104 output. Probably needs the comma position recorded on the item (and care under sorting: a unit that moved away from its directive cannot keep its comma). Regression test in fmt_bugs_test.rs; the round-trip oracle cannot see inside the include, so assert the output shape directly.
<!-- SECTION:DESCRIPTION:END -->
