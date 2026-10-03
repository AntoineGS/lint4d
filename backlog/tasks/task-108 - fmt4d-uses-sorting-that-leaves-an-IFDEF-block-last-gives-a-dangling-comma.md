---
id: TASK-108
title: 'fmt4d: uses sorting that leaves an {$IFDEF} block last gives a dangling comma'
status: To Do
assignee: []
created_date: '2026-10-03 03:28'
labels:
  - fmt
  - correctness
dependencies: []
priority: high
type: bug
ordinal: 110000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the strengthened TASK-63 round-trip oracle (sanity test oracle_rejects_punctuation_that_breaks_a_configuration in crates/fmt4d/tests/fmt_roundtrip_test.rs uses this shape). This bug predates the TASK-26 branch.

Repro (default config, uses sorting on):
```pascal
unit T;
interface
uses B, {$IFDEF X} C, {$ENDIF} A;
implementation
end.
```
Output:
```pascal
uses
  A,
  B,
  {$IFDEF X}
  C,
  {$ENDIF};
```
The block is pinned after its anchor B. Once A sorts first, the block is the last slot and takes the `;` after `{$ENDIF}`, while the units inside it, and B, keep commas. With X defined the clause reads `uses A, B, C, ;`, without X `uses A, B, ;`, so neither compiles. The second pass formats it the same way, so idempotency tests do not notice.

Cause: layout_uses_items / emit_ifdef_block in crates/fmt4d/src/uses.rs. The clause terminator goes to the last non-separator slot, and units inside a block always get commas. This is related to TASK-102 (`;` inside each branch) and TASK-104 (last slot is a directive): all three come from the same terminator logic.

Expected: every configuration of the conditional blocks still reads `unit (, unit)* ;`. For example, keep the last plain unit as the terminator (`A, B, {$IFDEF X} C, {$ENDIF} D;`), or place a block that has no terminator inside it before the last unit.

Done when: a regression test in fmt_bugs_test.rs formats the repro into a clause that is valid in both configurations, and the round-trip oracle accepts it.
<!-- SECTION:DESCRIPTION:END -->
