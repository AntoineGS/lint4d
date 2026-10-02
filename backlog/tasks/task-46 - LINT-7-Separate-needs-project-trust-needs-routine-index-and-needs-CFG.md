---
id: TASK-46
title: 'LINT-7: Separate "needs project trust", "needs routine index" and "needs CFG"'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
labels:
  - arch-review
  - lint
  - runtime
  - maintenance
milestone: m-5
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: enhancement
ordinal: 46000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-7). Read that file's Global constraints section before starting.

Severity: medium. Cost: both.

**Problem.** `engine/mod.rs:111-135` builds CFGs whenever `run_cfg_rules`
is true, regardless of which enabled rules consume them; the comment says
otherwise. Four registered CFG rules ignore `AnalysisContext` entirely
(`rules/resource_leak.rs:46-70`, `rules/field_leak.rs:370-395`, `:493-518`,
`rules/exception.rs:311-335`). `rules/transaction.rs:685-701` uses CFGs only
to enumerate routine names. The exception index and CFG costs are paid with
no graph consumer.

**Approach.** Replace the boolean capability with
`Needs { project_facts: bool, routine_index: bool, cfg: bool }` on the
`Rule` trait; the engine builds each input only if some enabled rule needs
it. Build a `RoutineIndex` (name, range, kind) once per file and give it to
rules that only enumerate.

**Tests first.** CFG-build-count test in `crates/lint4d/tests/engine_run_test.rs`.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 With only `with-statement` and naming rules enabled, no CFG is built (count via test hook).
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
