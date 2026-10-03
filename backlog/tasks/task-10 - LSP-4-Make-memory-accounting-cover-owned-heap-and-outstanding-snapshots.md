---
id: TASK-10
title: 'LSP-4: Make memory accounting cover owned heap and outstanding snapshots'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - lsp
  - runtime
  - agent-todos
milestone: m-2
dependencies: []
priority: high
type: enhancement
ordinal: 10000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-4). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime memory.

**Problem.** Admission counts source bytes (`workspace.rs:452-465`) while
parsed retention is 48 bytes per source byte
(`project_cache.rs:19-24`, `RETAINED_BYTES_PER_SOURCE_BYTE`, marked
`dead_code`). A 256 MiB source allowance therefore permits a multi-gigabyte
index outside the 2 GiB cache budget. Import accounting omits
report/probes/context (`project_cache.rs:1325-1332`, `:1927-1941`), interface
accounting omits probes (`:1991-2010`), pinned entries may exceed the budget,
and evicted `Arc` values stay resident while workers hold them.

**Approach.**
1. Give every cached value a `retained_bytes()` that includes probes,
   reports and contexts; add a unit test that compares it to
   `RETAINED_BYTES_PER_SOURCE_BYTE * source_len` within 20 percent on a
   large RTL fixture.
2. Track outstanding snapshot bytes (values handed to workers) in the same
   budget; eviction counts only when the last `Arc` drops (use a drop guard
   that decrements the counter).
3. Admission for `max_total_bytes` uses the 48x estimate, not raw bytes.

**Tests first.** Unit tests for `retained_bytes()` on each cached type and
for the drop-guard counter.

**Depends on.** nothing. Pairs with LSP-6.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `max_cache_bytes = 512 MiB` on the 24k-file repo keeps RSS under roughly 1 GiB during a references request (measure with `/proc/self/status` VmRSS logged at debug level). Document the number.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From `AGENT_TODOS.md` (section "Workspace-Wide Performance"), folded into this task:

Workspace-wide indexing phase still grows RSS by ~3 GB on `multidev`
for references on a public symbol; consider bounding it.
<!-- SECTION:NOTES:END -->
