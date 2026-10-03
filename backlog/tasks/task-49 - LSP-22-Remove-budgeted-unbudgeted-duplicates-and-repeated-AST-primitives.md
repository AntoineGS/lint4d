---
id: TASK-49
title: 'LSP-22: Remove budgeted/unbudgeted duplicates and repeated AST primitives'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - lsp
  - maintenance
milestone: m-6
dependencies: []
priority: medium
type: task
ordinal: 49000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-22).

Severity: medium. Cost: maintenance.

**Problem.** `navigation.rs:8011-8071` and `:8268-8346`
(`resolve_with_context_receivers` and its budgeted twin) are the same
algorithm. `navigation.rs:18405-18441` `enclosing_type` versus
`enclosing_type_name`. Declaration-node discovery and identifier-at-offset
exist in `navigation/rename.rs:3891-3904`, `navigation.rs:21170-21192`,
`:21220-21244`, `navigation/assistance.rs:6435-6465`. Identifier scanning is
character-class based in `navigation/rename.rs:4072-4095` and ASCII-byte
based in `workspace/rename.rs:3015-3039`, `:3313-3314`. In server/workspace:
`path_components_equal` and `native_components_equal` are exact duplicates
(`workspace.rs:15033-15044`, `:15109-15120`); completion and diagnostic
record compaction duplicate the same hashing rules
(`server.rs:1860-1925`, `:2461-2538`).

**Approach.** One implementation per primitive, taking
`budget: Option<&mut Budget>`; a `SourceRecord` visitor used by both
compaction paths; one path-identity module with two explicitly named
functions (native identity, Pascal case-insensitive identity).

**Tests first.** Pure refactor; rely on existing tests. Add one test per
merged primitive asserting the budgeted and unbudgeted calls agree on a
fixture.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each listed pair is reduced to one function; all pascal-lsp tests green.
- [ ] #2 No behaviour change: the existing suites named in the task stay green (cargo fmt --check, clippy -D warnings, and the affected crate's tests).
<!-- AC:END -->
