---
id: TASK-55
title: 'LINT-8: One syntax index per file for the simple rules'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
labels:
  - arch-review
  - lint
  - runtime
milestone: m-6
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: enhancement
ordinal: 55000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-8). Read that file's Global constraints section before starting.

Severity: medium. Cost: runtime.

**Problem.** `rules/mod.rs:111-130` runs 18 registered rules; for a default
`.pas` run the reviewer counted 17 rule-level root traversals (naming 4,
inherited 2, exception 3, dangerous 1, resource leak 1, field rules 4,
casing 1, transaction uses 1) plus two engine scope walks and casing's
routine-body pass (`rules/naming.rs:49`, `:141`, `:229`, `:418`,
`rules/field_leak.rs:398-403`, `:521-526`, `rules/casing.rs:62-64`,
`rules/resource_leak.rs:236-275`, `:303`, `engine/mod.rs:101`, `:219`).

**Approach.** Build `SyntaxIndex { routines, classes, interfaces, consts,
locals, with_statements, except_blocks, uses }` in one cursor walk per
file; port the simple rules to query it. Keep complex rules on their own
walks until measured.

**Tests first.** Pure refactor; all `rules_*_test.rs` and snapshot tests
green. Add a traversal-count hook test.

**Depends on.** LINT-7 optional.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Default run performs at most 3 root traversals (parse, index, scope); `cargo bench -p lint4d` shows the lint phase faster on `benches/lint_bench.rs` (record before/after in the PR).
- [ ] #2 No behaviour change: the existing suites named in the task stay green (cargo fmt --check, clippy -D warnings, and the affected crate's tests).
<!-- AC:END -->
