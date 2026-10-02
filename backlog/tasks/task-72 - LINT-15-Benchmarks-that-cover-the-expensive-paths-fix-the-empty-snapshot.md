---
id: TASK-72
title: 'LINT-15: Benchmarks that cover the expensive paths; fix the empty snapshot'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
labels:
  - arch-review
  - lint
  - maintenance
milestone: m-6
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: low
type: task
ordinal: 72000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-15). Read that file's Global constraints section before starting.

Severity: low. Cost: maintenance.

**Problem.** `crates/lint4d/benches/lint_bench.rs:5-24` benchmarks one tiny
fixture; CI only compiles benches. Snapshot
`tests/snapshots/snapshot_test__snapshot_rule_resource_leak_no_try.snap` is
`[]` because the helper in `tests/snapshot_test.rs:38-68` supplies no DCU
context, so the named rule never runs. Rule snapshots run the whole
default registry and churn when unrelated rules change.

**Approach.** Add bench cases: project lint with 200 units, DCU-enabled
field rules, `--fix-fmt` with 1,000 edits. Make `snapshot_rule_*` tests
enable only the named rule and supply its required context. Add a CI job
that runs `cargo bench -- --test` to keep benches executing.

**Tests first.** This task is tests.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The no-try snapshot is non-empty; each rule snapshot is isolated.
<!-- AC:END -->
