---
id: TASK-47
title: 'LINT-9: One conditional evaluator shared with cfg-pascal preparation'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-02 23:23'
labels:
  - arch-review
  - lint
  - runtime
  - maintenance
  - cross-repo
milestone: m-5
dependencies:
  - TASK-40
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: enhancement
ordinal: 47000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-9). Read that file's Global constraints section before starting.

Severity: medium. Cost: both.

**Problem.** In a project lint, pascal-core analyzes the file, the adapter
analyzes again with include callbacks and masks directives
(`cfg/project_snapshot.rs:546-593`, `:609-655`, `:735-737`), then
`../cfg-pascal/src/prepared.rs:450-489`, `:1280-1304` runs its own narrower
boolean evaluator and splicer with separate budgets (100,000,000 work
units). cfg-pascal's expansion, occurrence identities and validated source
maps are valuable; its evaluator is the duplicate.

**Approach.** Give `cfg_pascal::prepare_source` an input
`ConditionalDecisions` (per directive range: active/inactive/unknown)
produced by pascal-core's analysis, and make the internal evaluator a
fallback used only when decisions are absent. Delete the adapter's second
analysis pass.

**Tests first.** Count test; `../cfg-pascal/tests/preparation_test.rs` green
with decisions supplied for its fixtures.

**Depends on.** CORE-3. Two-repo pin bump (cfg-pascal, then lint4d).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A project lint runs pascal-core's analyzer once per source (count via hook) and cfg-pascal's evaluator zero times when decisions are supplied.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
