---
id: TASK-36
title: 'LSP-19: Freshness witnesses validated by generation, not by re-reading'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-5
dependencies:
  - TASK-33
priority: high
type: enhancement
ordinal: 36000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-19).

Severity: high. Cost: runtime i/o and "stale" churn.

**Problem.** `workspace/rename.rs:6318-6321`, `:7603-7618`, `:7026-7041`
record directories, source paths, project membership and content hashes
(including files rejected by the identifier filter). Revalidation
(`:1639-1651`, `:1703-1820`, `:1858-1911`, `:1937-1952`) re-reads closed
sources, hashes them and re-checks membership. Any changed transport stamp
rejects. The longer the computation, the larger the window for one witness
to change, which produces the "work for minutes, then stale" behavior.

**Approach.** Witnesses become (index generation, set of document ids
actually bound). Validation compares the generation and, on mismatch,
checks whether any of the bound ids changed (from the change log kept by
the index). Content re-reads happen only at `workspace/applyEdit` time.

**Tests first.** Two barrier-harness protocol tests for the two cases.

**Depends on.** LSP-16.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A references result computed while an unrelated file changes is delivered, not rejected. A result whose bound document changed is rejected.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related task converted from `AGENT_TODOS.md`: TASK-92 (Investigate frequent 'analysis result became stale' and source-map work limit in the nvim log).
<!-- SECTION:NOTES:END -->
