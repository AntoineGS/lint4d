---
id: TASK-73
title: 'LSP-26: Quarantine template-only refactorings'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - lsp
  - maintenance
milestone: m-6
dependencies: []
priority: low
type: task
ordinal: 73000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-26). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: low. Cost: maintenance.

**Problem.** `workspace/extract.rs:25-40`, `:63-99` supports only an
unqualified procedure with a selected numeric-literal assignment.
`workspace/signature.rs:18-37`, `:67-93`, `:113-116` rejects any
comment or directive brace anywhere in the file and requires one specific
two-argument shape. Both parse the source independently of the index. They
are advertised as features while supporting template cases.

**Approach.** Either gate them behind an `experimental` initialization
option and say so in `crates/pascal-lsp/README.md`, or remove them until
they can be built on `TypeQueries` (LSP-21) and the shared edit planner.

**Tests first.** Protocol test that the actions are absent by default.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The README accurately describes what the two actions support, and they do not appear in `codeAction` responses unless enabled.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
