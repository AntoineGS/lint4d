---
id: TASK-64
title: 'FMT-6: One trivia index for comments and directives'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 03:28'
labels:
  - arch-review
  - fmt
  - maintenance
milestone: m-6
dependencies:
  - TASK-26
priority: low
type: task
ordinal: 64000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (FMT-6). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: low. Cost: maintenance.

**Problem.** `comments.rs:31-37`, `:104-130` and `directive_map.rs:50-55`,
`:160-168`, `:201-212` each walk the tree to collect trivia and each build
the same non-extra leaf index.

**Approach.** `TriviaIndex::build(tree)` once; both maps consume it.

**Tests first.** Pure refactor.

**Depends on.** FMT-2 first.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 One leaf-index implementation; fmt suites green.
- [ ] #2 No behaviour change: the existing suites named in the task stay green (cargo fmt --check, clippy -D warnings, and the affected crate's tests).
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Known behaviour to revisit (from the TASK-26 review): under uses sorting, a header comment above the first unit counts as that unit's leading comment and moves with it, e.g. `uses\n  // first\n  B, A;` → `uses\n  A,\n  // first\n  B;`. A comment meant as a header for the whole clause ends up in the middle of it. A single trivia index could tell a clause header (above the first unit, maybe followed by a blank line) from a unit's own comment.
<!-- SECTION:NOTES:END -->
