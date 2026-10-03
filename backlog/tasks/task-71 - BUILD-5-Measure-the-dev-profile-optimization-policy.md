---
id: TASK-71
title: 'BUILD-5: Measure the dev-profile optimization policy'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - build
  - build-time
milestone: m-6
dependencies: []
priority: low
type: chore
ordinal: 71000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (BUILD-5). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: low. Cost: build time.

**Problem.** Workspace `Cargo.toml:25-62` optimizes all dependencies at
`opt-level = 3` in dev and lists 16 proc-macro exceptions by name. New
macro crates silently get optimized until added.

**Approach.** Time clean dev build, incremental build and the pascal-lsp
test suite with the wildcard removed and with `opt-level = 3` only on
`tree-sitter`, `tree-sitter-pascal`, `regex-automata`, `globset`,
`serde_json`. Keep whichever is faster overall; document the numbers in
the manifest comment.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Numbers recorded; the exception list is either deleted or justified by a measurement.
<!-- AC:END -->
