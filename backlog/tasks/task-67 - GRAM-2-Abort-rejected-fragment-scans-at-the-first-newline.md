---
id: TASK-67
title: 'GRAM-2: Abort rejected fragment scans at the first newline'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - grammar
  - runtime
  - cross-repo
milestone: m-6
dependencies:
  - TASK-66
priority: medium
type: enhancement
ordinal: 67000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (GRAM-2). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: medium. Cost: runtime.

**Problem.** `src/scanner.c:187-207`, `:225-226`, `:260-267`: the scanner
scans to the matching conditional terminator, records that a newline
occurred, then returns false. Nested blocks cause overlapping scans;
unterminated directives scan to EOF.

**Approach.** Return false as soon as a newline is seen when the fragment
cannot be multi-line; cap the lookahead for unterminated directives.

**Tests first.** Corpus cases for nested and unterminated directives.

**Depends on.** GRAM-1 benchmark.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A corpus file with 1,000 nested multi-line conditionals parses in linear time (benchmark from GRAM-1).
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
