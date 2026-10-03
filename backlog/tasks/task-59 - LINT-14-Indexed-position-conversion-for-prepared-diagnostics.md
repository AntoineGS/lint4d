---
id: TASK-59
title: 'LINT-14: Indexed position conversion for prepared diagnostics'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - lint
  - runtime
  - cross-repo
milestone: m-6
dependencies: []
priority: medium
type: enhancement
ordinal: 59000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-14).

Severity: medium. Cost: runtime.

**Problem.** `engine/mod.rs:350-398`, `:403-437` do two position-to-byte and
two byte-to-position scans from the buffer start per prepared diagnostic;
`rules/helpers.rs:413-425` likewise; `../cfg-pascal/src/source_map.rs:794-801`
`map_range` skips segments linearly; `cfg/project_snapshot.rs:829-868`,
`:899-909` scan the map and search the prepared AST per import.

**Approach.** Line-start index per source snapshot; sorted segments with
binary search in `SourceMap`; index import nodes by range once.

**Tests first.** Timing test; `../cfg-pascal/tests/source_map_test.rs`
green.

**Depends on.** nothing. Pin bump for cfg-pascal.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 10,000 diagnostics on a 2 MiB prepared unit convert in linear time (loose timing test).
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
