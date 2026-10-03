---
id: TASK-103
title: >-
  fmt4d: var-list expansion duplicates trailing comments and drops comments
  between names
status: To Do
assignee: []
created_date: '2026-10-03 03:14'
updated_date: '2026-10-03 03:15'
labels:
  - fmt
  - correctness
dependencies: []
priority: high
type: bug
ordinal: 105000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while building the TASK-63 round-trip oracle.

build_comma_ident_decl (crates/fmt4d/src/doc_builder_decls.rs) expands `A, B: T;` into one declaration per identifier by cloning the suffix Doc (`: T;` with its comments). Comments attached to the suffix are therefore emitted once per identifier, and comments attached to the commas (which are not emitted) are dropped.

Repro (default config):
```pascal
unit T;
interface
implementation
procedure P;
var
  A, B: Integer; // shared
  C, { why } D: string;
begin
end;
end.
```
Output:
```pascal
var
  A: Integer; // shared
  B: Integer; // shared
  C: string;
  D: string;
```
`// shared` is duplicated and `{ why }` is lost. The output is stable on a second run (no commas left), so the idempotency checks do not notice.

Expected: every comment kept exactly once, e.g. the suffix's trailing comments only on the last expanded declaration and comments around a comma kept with the identifier before it.

Done when: a regression test in fmt_bugs_test.rs covers both cases and the round-trip oracle (check_same_program in fmt_roundtrip_test.rs) accepts the repro.

Acceptance Criteria:
--------------------------------------------------
No acceptance criteria defined
<!-- SECTION:DESCRIPTION:END -->
