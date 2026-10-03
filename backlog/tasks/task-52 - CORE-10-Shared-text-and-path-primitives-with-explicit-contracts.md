---
id: TASK-52
title: 'CORE-10: Shared text and path primitives with explicit contracts'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - core
  - runtime
  - maintenance
milestone: m-6
dependencies: []
priority: medium
type: enhancement
ordinal: 52000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-10). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: medium. Cost: both.

**Problem.** Unit-name normalization removes internal whitespace and keeps
case in `resolver.rs:3265-3270`, `:3356-3380`, `:3439-3449`, but lowercases
without removing whitespace in `pascal-project/src/lib.rs:3174-3210`,
`:6323-6326`, `:6438-6439`. Native path identity, case-insensitive compare
and Windows-path recognition exist in `lib.rs:6641-6657`, `:7145-7149`,
`delphi_overrides.rs:740-766`, `:847-912`, `discovery_msbuild.rs:82-153`,
`pascal-lsp/src/workspace/resolver.rs:62-83`. UTF-8/Latin-1 decoding is
duplicated between project and core; the LSP adapter adds UTF-16 BOM
handling separately. `AGENT_TODOS.md` also notes
`ReadPolicy::new_with_installation_roots` has the same `is_absolute()`
inconsistency as the fixed workspace walker.

**Approach.** New module in pascal-project (lowest crate): `textprim`
(decode with BOM handling, one function) and `pathid` with
`native_identity(&Path)`, `pascal_identity(&Path)` (case-folded, separator
normalized), `is_foreign_windows_path(&Path)`, `normalize_unit_name(&str)`.
Replace each duplicate; fix the `new_with_installation_roots` case.

**Tests first.** The input table, run against the old functions first to
record current behavior and flag the whitespace/case divergence for a
decision.

**Depends on.** CORE-9 optional.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 One definition each; a test table of inputs asserts identical outputs from every former call site.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related task converted from `AGENT_TODOS.md`: TASK-81 (ReadPolicy::new_with_installation_roots adds <root>/C:/... as a read root).
<!-- SECTION:NOTES:END -->
