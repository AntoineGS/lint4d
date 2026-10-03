---
id: TASK-51
title: 'CORE-8: Canonical immutable `ProjectContext` and a complete cache key'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - core
  - runtime
  - maintenance
milestone: m-6
dependencies: []
priority: medium
type: bug
ordinal: 51000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-8).

Severity: medium. Cost: both.

**Problem.** `pascal-project/src/lib.rs:1217-1245`, `:1254-1281`,
`:958-989` expose paths both as bare vectors and as provenance entries,
plus compatibility projections for units and defines; public fields let
them disagree, forcing fallback branches in the resolver
(`resolver.rs:1170-1209`, `:1225-1233`). `ProjectOptionsCacheKey`
(`lib.rs:2675-2689`, `:4641-4666`) omits `build_selections` although
context construction consumes them, so different selections can collide in
one cache scope.

**Approach.** Make the provenance-bearing representation the only stored
one; derive bare vectors through accessor methods; make fields private.
Derive the cache key from a hash of the canonical representation including
`build_selections`.

**Tests first.** The cache-key test (fails today).

**Depends on.** CORE-2 is easier after this but not required.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Two contexts differing only in `build_selections` have different cache keys (test). No public mutable field on `ProjectContext`.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
