---
id: TASK-18
title: 'LSP-12: Warm cache hits should not read and decode the file first'
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
priority: high
type: enhancement
ordinal: 18000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-12). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime i/o.

**Problem.** `workspace.rs:7498-7546`, `:7636-7648`: a worker's fresh
`NavigationIndex` reads and decodes a closed source to compute the hash
before it can look up the cached parse. Import lookup
(`:6755-6759`, `:6780-6805`, `:6815-6818`, `:6879-6882`) copies indexed
text and discovers compiled candidates before checking the cache; a hit
clones the resolved graph, report and probes
(`project_cache.rs:200-224`) and re-stats every probe.

**Approach.** Key the first lookup by (path, mtime, len) from one `stat`;
only read the file when the stat key misses. Return `Arc` values from hits
instead of clones. Trust watcher-backed invalidation (LSP-8 reverse map)
for probe freshness; keep a full probe verification path behind a
configuration flag for environments without a watcher.

**Tests first.** Read-count characterization test.

**Depends on.** LSP-8.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Second definition request on the same closed unit performs zero file reads (count via a test hook on the source store).
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
