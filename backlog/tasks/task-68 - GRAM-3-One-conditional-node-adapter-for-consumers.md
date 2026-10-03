---
id: TASK-68
title: 'GRAM-3: One conditional-node adapter for consumers'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - grammar
  - maintenance
  - cross-repo
milestone: m-6
dependencies: []
priority: medium
type: task
ordinal: 68000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (GRAM-3). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: medium. Cost: maintenance.

**Problem.** Conditionals appear as extras, `ppBlock`/`ppUsesBlock` nodes,
directive-wrapped routines and opaque fragments
(`grammar.js:115-164`, `:355-357`, `:856-913`). fmt4d (`uses.rs:250-375`),
lint4d (`rules/helpers.rs:428-448`) and cfg-pascal
(`pascal_builder.rs:1627-1658`) each decode them separately.

**Approach.** `pascal_core::conditional_nodes` exposing
`Branch { condition_range, body_nodes, kind }` iteration over any of the
shapes; port the three consumers.

**Tests first.** Adapter unit tests over each node shape.

**Depends on.** LINT-12 may share the module.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Three consumers use the adapter; grammar change to a `pp*` node requires editing one module.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
