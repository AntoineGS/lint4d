---
id: TASK-32
title: 'CORE-9: Split `pascal-project/src/lib.rs`'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - core
  - maintenance
milestone: m-4
dependencies: []
priority: high
type: task
ordinal: 32000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-9). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: maintenance.

**Problem.** 9,600 lines of production code before a 1,595-line test module
at `:9614`. Natural seams noted by the reviewer: authorization/provenance
(`:175-918`), filesystem reads and freshness (`:919-1201`), discovery caches
(`:1205-1933`), public context/model (`:1937-4375`), candidate selection and
ownership probing (`:4618-5346`), context assembly (`:6631-7240`,
`build_project_context` alone ~730 lines), package metadata
(`:7251-7490`), XML/property expansion/MSBuild conditions (`:7566-9393`),
Pascal membership lexing (`:9397-9610`).

**Approach.** Mechanical moves into `src/authorization.rs`, `src/reads.rs`,
`src/discovery_cache.rs`, `src/context.rs`, `src/selection.rs`,
`src/assembly.rs`, `src/packages.rs`, `src/msbuild_eval.rs`,
`src/membership.rs`; `lib.rs` becomes the `pub use` facade. One commit per
module; tests move with their code.

**Tests first.** None; pure moves.

**Depends on.** Do before CORE-2/CORE-8 or after, but not concurrently with
them.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `lib.rs` under 500 lines; no file over 3,000; the four pascal-project test files green.
- [ ] #2 No behaviour change: the existing suites named in the task stay green (cargo fmt --check, clippy -D warnings, and the affected crate's tests).
<!-- AC:END -->
