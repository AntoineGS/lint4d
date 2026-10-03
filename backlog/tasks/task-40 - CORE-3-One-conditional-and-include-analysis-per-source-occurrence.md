---
id: TASK-40
title: 'CORE-3: One conditional and include analysis per source occurrence'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - core
  - runtime
  - maintenance
milestone: m-5
dependencies: []
priority: high
type: enhancement
ordinal: 40000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-3). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: both.

**Problem.** `pascal-core/src/resolver.rs:667-685` analyzes imports without
an include callback, and an active include clears the environment
(`:966-997`). The later include walk (`:2591-2597`, `:2643-2742`) analyzes
again with the environment propagated through includes, and first runs a
full analysis just to enumerate include directives. A define set by an
include is understood for includes but imports guarded by it are already
marked incomplete, which then triggers lint4d's file-local fallback
(`docs/shared-resolver-architecture.md:115-119` documents this limitation).
Separately, `conditional.rs:1029-1052` clones the whole environment on
every conditional start (`clone_environment`), again on branch transition
and on first merge (`:1072-1086`, `:1266-1278`, `:1882-1934`,
`:2007-2075`), even when branch activity is already known. Cost scales with
environment size x branch count. The 16 MiB copy budget turns this into
incompleteness rather than avoiding the copies.

**Approach.**
1. Make `analyze_with_context_and_cancel` accept the include callback and
   return both import activity and include occurrences from one ordered
   walk. Import binding and include reporting consume the same result.
2. Replace environment snapshots with a change journal: push
   `(key, previous value)` entries on define/undef, truncate on branch
   restore, and merge by replaying journals. Skip journal work entirely
   when the branch condition is `Truth::False` or `Truth::True`.

**Tests first.** The include-defines-import fixture in
`crates/pascal-core/tests/unit_resolver.rs` (fails today with "unknown
conditional activity"); clone-count test in the `conditional.rs` test
module.

**Depends on.** nothing. Coordinate with LINT-9.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A unit whose `{$I defs.inc}` defines `FOO` and whose uses clause is wrapped in `{$IFDEF FOO}` resolves its imports as active and complete. A 10,000-entry environment with 1,000 known-inactive branches performs zero environment clones (count via test hook).
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
