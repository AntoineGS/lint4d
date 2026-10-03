---
id: TASK-42
title: 'CORE-5: Indexed observation accumulator'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - core
  - runtime
  - maintenance
milestone: m-5
dependencies: []
priority: high
type: enhancement
ordinal: 42000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-5).

Severity: high. Cost: both.

**Problem.** `resolver.rs:175-181`, `:262-282`, `:3322-3353` scan all
retained observations on each insert; comparisons include `ReadPolicy`
values. Reports and per-operation outcomes clone observation vectors
(`:1048-1077`, `:2890-2892`). `pascal-project/src/lib.rs:1539-1552`,
`:1584-1622`, `:6285-6320` retain metadata `Vec<u8>` payloads in project
read observations.

**Approach.** `ObservationSet { by_key: HashMap<ObservationKey, usize>,
items: Vec<Observation> }` with `ReadPolicy` interned to a `u32` id;
payloads replaced by (len, hash). Materialize ordered reports only in
`finish()`.

**Tests first.** Timing test; type-level test that `Observation` has no
payload field (compile-time).

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Inserting 50,000 observations is linear (timing test with a loose bound) and no observation holds a payload `Vec<u8>`.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
