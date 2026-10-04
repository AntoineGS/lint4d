---
id: TASK-112
title: >-
  fmt4d: a // comment before a colon or comma in a declaration list swallows the
  punctuation
status: To Do
assignee: []
created_date: '2026-10-03 22:14'
updated_date: '2026-10-03 23:40'
labels:
  - fmt
  - correctness
milestone: m-3
dependencies:
  - TASK-103
priority: high
type: bug
ordinal: 114000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while fixing TASK-103 (branch fix/fmt-trivia-fixes, b3e06ca); pre-existing on master d55c0f6.

A `//` line comment that trails the last identifier before the `:` of a var/field declaration, or that sits before a `,` in a parameter or field list, ends up in front of that punctuation in the output. The `:` or `,` then lands inside the line comment, so the output no longer compiles (or changes meaning).

Shapes to reproduce (exact repros in the TASK-103 report: .superpowers/sdd/day-2026-10-03/FMT-T-report.md in the main checkout):
```pascal
var
  A, B // why
    : Integer;

procedure P(A // first
  , B: Integer);
```

**Tests first.** Regression tests in crates/fmt4d/tests/fmt_bugs_test.rs for the var-declaration and parameter/field-list shapes, plus fixtures so the round-trip oracle (fmt_roundtrip_test.rs) covers them; check idempotency with alignment on and off.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A // comment before `:` or `,` in var, field and parameter lists never swallows the punctuation; output compiles and is idempotent with alignment on and off
- [ ] #2 Regression tests fail on the original code and pass after the fix
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
TASK-103 fix round 1 (branch fix/fmt-trivia-fixes, 291b3c7) fixes the comma shape: a `//` comment trailing an identifier before a `,` in a parameter/field list now moves after the comma (`A, // first` then `B`), and in expanded var lists after the declaration's `;`. The colon shape (`A, B // why\n : Integer;`) is still open.
<!-- SECTION:NOTES:END -->
