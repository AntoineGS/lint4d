---
id: TASK-44
title: 'LINT-2: Run-scoped shared project analysis in the CLI'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - lint
  - runtime
  - cross-repo
milestone: m-5
dependencies: []
priority: high
type: enhancement
ordinal: 44000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-2). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime.

**Problem.** `crates/lint4d/src/main.rs:426-479` (`lint_project_file`)
creates a fresh `UnitResolver` and `FilesystemSourceStore` per rayon task,
resolves the full dependency closure, and `cfg/project_snapshot.rs:348-370`,
`:711-743` parses and prepares every loaded unit and builds a project-wide
exception index per target. `../cfg-pascal/src/prepared.rs:1295-1304`,
`:1793-1816` additionally re-lexes every include-binding owner per
`prepare_source` call. Shared RTL units are parsed once per lint target.
The integration logic lives in `main.rs`, not in the library.

**Approach.** Move `lint_project_file` into `lint4d::engine` as
`ProjectRun::new(context, roots) -> ProjectRun` holding
`Arc<SharedProjectIndex>` (parsed trees, prepared units, exception index,
keyed by path and content hash, behind a `Mutex`/`RwLock` map). Workers
take a request-local resolver that reads from the shared store via a
`SourceStore` adapter (CORE-4 index). CFGs are still built per target.

**Tests first.** Parse-count test; golden output test of the existing
project fixtures.

**Depends on.** CORE-4 optional; LINT-3 shares files, coordinate.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Linting a 200-unit project with one shared RTL unit parses that unit once (count via test hook). The project in `crates/lint4d/tests/msbuild_integration_test.rs` produces identical output.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
