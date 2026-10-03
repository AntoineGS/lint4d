---
id: TASK-61
title: 'FMT-3: Index blank lines once'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - fmt
  - runtime
milestone: m-6
dependencies: []
priority: medium
type: enhancement
ordinal: 61000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (FMT-3). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: medium. Cost: runtime.

**Problem.** `doc_builder.rs:377-399` `has_blank_line_between` scans the
source from byte zero on every call; callers in `doc_builder_decls.rs:58-65`,
`doc_builder_alignment.rs:613`, `:823` call it per declaration, so long
declaration lists are quadratic in source bytes.

**Approach.** Build `blank_line_rows: Vec<u32>` once in the builder
constructor; answer with two binary searches.

**Tests first.** Timing test; existing fmt suites green.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Formatting a 50,000-line declaration-only unit is linear (loose timing test under 1 s).
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
