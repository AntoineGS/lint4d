---
id: TASK-15
title: 'LSP-10: Reconcile mutations without taking the whole workspace'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-2
dependencies:
  - TASK-14
priority: high
type: enhancement
ordinal: 15000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-10).

Severity: high. Cost: runtime latency.

**Problem.** `server.rs:10581-10608`, `:10920-10928`, `:10990-11009`,
`:11196-11208`: the mutation worker `mem::take`s the entire `Workspace`.
While it runs, new requests are deferred and completed results are not
delivered. The 30 s deadline only requests cooperative cancellation.

**Approach.** Reconcile against an immutable workspace revision into a
`ChangeSet`, then apply the change set under a short critical section.
Completed results whose generation predates the commit are still
deliverable if their dependency token (LSP-7) still matches.

**Tests first.** Barrier-harness protocol test for the above.

**Depends on.** LSP-7 for the dependency token.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 During a 1,000-file watcher batch, a hover request already completed before the batch is delivered without waiting for reconciliation.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
