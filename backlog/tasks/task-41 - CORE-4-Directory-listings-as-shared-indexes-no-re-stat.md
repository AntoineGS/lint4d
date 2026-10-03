---
id: TASK-41
title: 'CORE-4: Directory listings as shared indexes, no re-stat'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - core
  - runtime
milestone: m-5
dependencies: []
priority: high
type: enhancement
ordinal: 41000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-4).

Severity: high. Cost: runtime.

**Problem.** `resolver.rs:1575-1598` (`paths_for_names`) linearly scans
`listing.files` with `eq_ignore_ascii_case` per name per tier; the cache
hit path (`:1623-1695`) `.cloned()`s the `DirectoryListing`. On a miss,
`resolver_store.rs:208-220` stats files for size accounting and the
resolver stats them again, bypassing `SourceStore`.

**Approach.** `DirectoryListing` becomes
`Arc<DirectoryIndex { by_folded_name: HashMap<Box<str>, PathBuf>, bytes: u64 }>`
built once by the store; the resolver looks names up in the map and takes
accounting facts from the index.

**Tests first.** Counting-store test in `tests/unit_resolver.rs`.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Resolving 200 units against a 3,000-file RTL directory performs one `read_dir` and zero `stat` calls in the resolver (count via a test `SourceStore`).
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
