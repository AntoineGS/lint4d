---
id: TASK-39
title: 'CORE-2: Typed completeness instead of warning-string matching'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - core
  - correctness
  - maintenance
milestone: m-5
dependencies: []
priority: high
type: bug
ordinal: 39000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-2).

Severity: high. Cost: maintenance and correctness.

**Problem.** `pascal-project/src/lib.rs:5478-5533`
(`project_context_warnings_incomplete`) decides completeness with
`warning.contains("unresolved property")` and a dozen other substrings;
`:5286`, `:5306` duplicate the predicate. `pascal-core/src/resolver_store.rs:387-408`
converts string errors to typed errors by searching for "exceed", "regular
file", "not authorized". `installations/ide_paths.rs:943-948` similar.
Rewording a message changes resolution semantics.

**Approach.** Introduce `enum DiscoveryIssue { UnresolvedProperty{..},
MissingPath{..}, UnreadableOptset{..}, MetadataLimit, ... }` with
`fn affects_completeness(&self, explicit: bool) -> bool`. Store issues on
`ProjectContext`; render `warnings: Vec<String>` from them at the API
boundary for compatibility. Same for `SourceStore` read errors:
`enum ReadError { TooLarge, NotRegularFile, Unauthorized, Io(..) }`.

**Tests first.** For each substring currently matched, a test that
constructs the typed issue and asserts the same completeness outcome as
today, so the port is checked.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No `contains("` call participates in a completeness or error classification decision in either crate (grep gate in a test or CI script).
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
