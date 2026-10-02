---
id: TASK-58
title: 'LINT-13: Single-pass edit application and neutral traversal budgets'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
labels:
  - arch-review
  - lint
  - runtime
  - maintenance
milestone: m-6
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: enhancement
ordinal: 58000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-13). Read that file's Global constraints section before starting.

Severity: medium. Cost: both.

**Problem.** `fix/apply.rs:314-445`, `:451-478` splices a growing buffer per
edit (edits x file size). `rules/scope.rs:6`, `:38-59` depends on
`fix::FixWorkBudget`, a rules-to-fix back-dependency; `fix/mod.rs:18-44`,
`:149-156` spends much of its length on allocation estimation with a fixed
64x parser envelope.

**Approach.** Sort validated edits, write output in one forward pass.
Move `WorkBudget` to `pascal_core::budget` and drop the estimation
helpers that no longer matter once application is linear.

**Tests first.** `crates/lint4d/tests/fix_budget_test.rs` and `fix_test.rs`
green; timing test.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Applying 10,000 edits to a 5 MiB file is linear (loose timing test); `rules/` has no import from `fix/`.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
