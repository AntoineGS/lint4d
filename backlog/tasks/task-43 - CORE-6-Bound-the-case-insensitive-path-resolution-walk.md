---
id: TASK-43
title: 'CORE-6: Bound the case-insensitive path-resolution walk'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - core
  - runtime
  - maintenance
milestone: m-5
dependencies:
  - TASK-41
priority: high
type: enhancement
ordinal: 43000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-6). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: both.

**Problem.** `pascal-project/src/lib.rs:6675-6745`, `:6851-6899`,
`:6908-6911`: the case-insensitive path cache enumerates whole directories
into name vectors with no cap and no cancellation, while the neighbouring
candidate lister (`:3314-3348`) and the core walker
(`resolver.rs:2091`) are bounded. A single case-mismatched component
triggers it, also on Linux. `AGENT_TODOS.md` notes cancelled requests can
keep a worker busy ~5 s inside `resolve_existing_path_status`.

**Approach.** Route both through one `DirectoryProvider` that takes the
budget and cancel token from CORE-4's index.

**Tests first.** Budget and cancellation tests in
`crates/pascal-project/tests/public_api.rs`.

**Depends on.** CORE-4.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `resolve_existing_path_status` on a directory with 100,000 entries returns `Incomplete` after the budget and checks cancellation at least every 1,000 entries.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related task converted from `AGENT_TODOS.md`: TASK-84 (Add cancellation checks inside resolve_existing_path_status).
<!-- SECTION:NOTES:END -->
