---
id: TASK-63
title: 'FMT-5: Round-trip oracle compares token content'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - fmt
  - correctness
milestone: m-6
dependencies:
  - TASK-25
  - TASK-26
priority: medium
type: bug
ordinal: 63000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (FMT-5).

Severity: medium. Cost: correctness.

**Problem.** `tests/fmt_roundtrip_test.rs:6-21` `ast_eq` compares node kinds
and child counts, filters extras, and never compares identifier or literal
text; changing an identifier passes, dropping a comment passes. Default
uses sorting (`config.rs:103-110`) reorders initialization, so intentional
transformations need an explicit allow-list.

**Approach.** Extend `ast_eq` to compare leaf text for identifiers,
literals and comments, with an exception for uses-clause children when
sorting is enabled. Run it over every fixture under `tests/fixtures`.

**Tests first.** This task is tests; it will likely expose FMT-1/FMT-2
cases.

**Depends on.** FMT-1, FMT-2 to be green.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The oracle fails on an injected identifier change (sanity test) and passes on all fixtures.
<!-- AC:END -->
