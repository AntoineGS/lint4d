---
id: TASK-20
title: 'LSP-15: Share directory catalogues across requests and prune excluded trees'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-2
dependencies:
  - TASK-13
priority: medium
type: enhancement
ordinal: 20000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-15). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: medium. Cost: runtime i/o.

**Problem.** `workspace.rs:3205-3265`, `:12086-12113`, `:12203-12220`,
`:12236-12275`: reconstructed worker workspaces do not keep filename,
directory or package catalogues, so they are rebuilt per request. A hit
within a request stats all remembered directories and deep-clones the
catalogue. The 10,000-entry limit (`:3269-3297`) counts every filesystem
entry; excluded directories are only skipped once reached, not pruned.

**Approach.** Store catalogues as `Arc` values in the shared project cache
keyed by (root set, exclude set, generation); invalidate through the
watcher reverse map (LSP-8). Prune excluded directories before descending.
Count only `.pas`/`.dpr`/`.dpk`/`.inc` entries against the limit.

**Tests first.** Walk-count test; excluded-tree test.

**Depends on.** LSP-8.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Second references request on the same workspace performs no directory walk (count via a test hook). A `node_modules`-style excluded tree of 50,000 files does not consume the limit.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related task converted from `AGENT_TODOS.md`: TASK-82 (Find out why many sources still miss project discovery reuse).
<!-- SECTION:NOTES:END -->
