---
id: TASK-13
title: 'LSP-8: Take superlinear scans and watcher calls out of the cache mutex'
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
priority: high
type: enhancement
ordinal: 13000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-8). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime latency.

**Problem.** `project_cache.rs:1907-1945` LRU eviction scans every slot per
victim and, per candidate, scans all pin-owner sets and clones its URI.
`:1950-1976`, `:2089-2128` path invalidation scans every entry's probes and
deduplicates with `Vec::contains`. `file_watch.rs:57-70`, `:125-136`
repeats for each event path. `workspace.rs:12949-12955` calls
watch/unwatch while holding the mutex.

**Approach.** Keep an intrusive LRU order (`VecDeque` of keys or a linked
map), an aggregate pin count per entry, and a reverse map
`path -> HashSet<entry key>`. Collect watcher operations into a `Vec` under
the lock and execute them after releasing it.

**Tests first.** Characterization tests for eviction order and pin
semantics using the existing public cache API, so the refactor is checked.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Eviction and invalidation are O(affected entries). A test with 10,000 entries invalidating one path touches one entry (count via test hook).
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
