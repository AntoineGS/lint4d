---
id: TASK-64
title: 'FMT-6: One trivia index for comments and directives'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
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
