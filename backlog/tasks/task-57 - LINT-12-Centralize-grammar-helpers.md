---
id: TASK-57
title: 'LINT-12: Centralize grammar helpers'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - lint
  - maintenance
  - cross-repo
milestone: m-6
dependencies: []
priority: medium
type: task
ordinal: 57000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-12). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: medium. Cost: maintenance.

**Problem.** Node text decoding, uses extraction, `.Create` recognition and
qualified routine-name extraction are copied across
`rules/helpers.rs:11-39`, `:222-262`, `pascal-core/src/parser.rs:370`,
`../cfg-pascal/src/factory.rs:106-143`, `:212-249`, `:282-299`,
`engine/mod.rs:512-554`, `rules/transaction.rs:602-651`,
`rules/nil_check.rs:229-240`.

**Approach.** `pascal_core::syntax` module with `node_text`,
`unit_name`, `uses_units`, `is_create_call`, `routine_qualified_name`,
`enclosing_routine`; lint4d and cfg-pascal import it (cfg-pascal would
need pascal-core as a dependency, which inverts nothing: pascal-core does
not depend on cfg-pascal). Decide whether that dependency is acceptable
or whether the helpers live in a tiny new `pascal-syntax` crate.

**Tests first.** Table test per helper recording current outputs from each
copy to detect divergence before merging.

**Depends on.** nothing. Pin bump if cfg-pascal changes.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each helper has one definition; grep gate.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
