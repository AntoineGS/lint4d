---
id: TASK-62
title: 'FMT-4: Fill rendering without `Vec::remove(0)`'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
labels:
  - arch-review
  - fmt
  - runtime
milestone: m-6
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: enhancement
ordinal: 62000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (FMT-4). Read that file's Global constraints section before starting.

Severity: medium. Cost: runtime.

**Problem.** `renderer.rs:206-216` removes the first two elements of a Fill
and requeues the rest, shifting the vector each iteration.

**Approach.** Render Fill through an index cursor `(parts, next_idx)` on the
stack.

**Tests first.** Timing test; `fmt_line_break_test.rs` green.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A 5,000-element expression chain renders in linear time.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
