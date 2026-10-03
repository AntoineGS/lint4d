---
id: TASK-19
title: 'LSP-14: Cache coordinate indexes and binary-search source maps'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-2
dependencies: []
priority: medium
type: enhancement
ordinal: 19000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-14). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: medium. Cost: runtime cpu.

**Problem.** `workspace.rs:13335-13350`, `:13374-13397` call the one-shot
text conversion per diagnostic endpoint, rebuilding a line vector of the
whole physical source each time. `include_expansion.rs:149-155`, `:203-205`
scan segments from the start for every forward lookup; reverse lookup
scans all segments. Mapping budgets then convert this cost into withheld
results (`MAX_SNAPSHOT_MAPPING_WORK = 1_000_000` at
`workspace/rename.rs:81`, matching the "include source-map work limit
reached" log in `AGENT_TODOS.md`).

**Approach.** One `PositionIndex` per physical source cached with the
`PreparedSource` (LSP-11). Sort source-map segments by expanded offset and
binary search; build a `HashMap<Url, Vec<segment idx>>` for reverse lookup.

**Tests first.** Unit test that builds such a unit and asserts no
work-limit error.

**Depends on.** LSP-11 optional.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Mapping 10,000 diagnostics on a 1 MiB include-heavy unit completes without hitting `MAX_SNAPSHOT_MAPPING_WORK`.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related task converted from `AGENT_TODOS.md`: TASK-92 (Investigate frequent 'analysis result became stale' and source-map work limit in the nvim log).
<!-- SECTION:NOTES:END -->
