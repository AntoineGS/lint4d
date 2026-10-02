---
id: TASK-21
title: 'LINT-1: CFG routine identity must not collide on overloads'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
labels:
  - arch-review
  - lint
  - correctness
  - cross-repo
milestone: m-3
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: high
type: bug
ordinal: 21000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-1). Read that file's Global constraints section before starting.

Severity: high. Cost: correctness.

**Problem.** `crates/lint4d/src/engine/mod.rs:126-132` collects CFGs into a
`HashMap<ProcId, _>` where `ProcId` is unit name plus qualified name
(`../cfg-core/src/summary.rs:3-14`). Overloaded routines overwrite each
other silently. `rules/transaction.rs:654-680` then searches the AST by
that same non-unique name with an unqualified fallback.

**Approach.** Add `scope_range: (usize, usize)` (byte range of the routine
body) to `ProcId` in cfg-core, set it in `../cfg-pascal/src/pascal_builder.rs:124-138`,
and look routines up by range in transaction analysis. Bump the cfg-core and
cfg-pascal pins.

**Tests first.** That fixture in `crates/lint4d/tests/rules_resource_leak_test.rs`
(fails today: one CFG lost).

**Depends on.** nothing. Three-repo pin bump.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A unit with two `procedure Foo` overloads, one leaking a resource, reports the leak on the correct overload.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
