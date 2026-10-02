---
id: TASK-28
title: 'LINT-10: Single-flight, negative-cached DCU loading'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
labels:
  - arch-review
  - lint
  - runtime
milestone: m-3
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: enhancement
ordinal: 28000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-10). Read that file's Global constraints section before starting.

Severity: medium. Cost: runtime.

**Problem.** `crates/lint4d/src/dcu/mod.rs:188-216` (`ensure_loaded`)
checks the map, releases the lock, reads and parses, then inserts. Concurrent
workers parse the same unit; failures are never cached, so every lookup
repeats the read and the warning. `resolve_type` (`:220-235`) loads every
used unit before searching and clones the whole `TypeInfo`.

**Approach.** `HashMap<String, Arc<OnceLock<Result<Arc<Unit>, DcuError>>>>`;
first caller initializes, others wait; failures stored. `resolve_type`
returns `Arc<TypeInfo>` and loads units lazily in uses order.

**Tests first.** That test in `crates/lint4d/tests/dcu_context_test.rs`.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 8 workers requesting the same missing-on-disk unit produce one warning and one read attempt.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
