---
id: TASK-38
title: 'CORE-1: One owner for project metadata interpretation'
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
ordinal: 38000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-1). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: both.

**Problem.** `crates/pascal-core/src/discovery_dproj.rs:14-78` extracts every
`DCCReference` without evaluating conditions, properties, imports, mappings
or read policy. `crates/pascal-project/src/lib.rs:7386-7404`, `:7742-7754`,
`:8409` evaluate all of those. `lint4d/src/main.rs:405-410`, `:438-443` and
`fmt4d/src/main.rs:154` select files with the simple reader and resolve
dependencies with the rich one, so the two can disagree on the same
`.dproj`. BDS ownership is also split: `discovery_bds.rs:82-156` (registry
and filesystem) versus `pascal-project/src/installations/identity.rs:52-116`
and `ide_paths.rs:856-985`. `quick-xml` and `winreg` are declared in both
crates for this reason. `lint4d/src/discovery/mod.rs` re-exports the
pascal-core versions.

**Approach.**
1. Add `ProjectContext::source_files(&self) -> Vec<PathBuf>` in
   pascal-project that returns the evaluated `DCCReference` list.
2. Port lint4d and fmt4d `--project` to it.
3. Delete `discovery_dproj.rs`; make `discovery_bds.rs` a thin adapter over
   pascal-project installations, or move it there. Drop `quick-xml` and
   `winreg` from pascal-core's `Cargo.toml` and `winreg` from lint4d's.
4. Keep real `msbuild.exe` execution (`discovery_msbuild.rs`) as a platform
   adapter; it is not duplicated.

**Tests first.** Fixture `.dproj` with one conditionally included unit;
assert `lint4d --project` output matches expected file list (currently
fails because the simple reader includes it unconditionally).

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `cargo tree -p pascal-core | grep -E 'quick-xml|winreg'` is empty; `lint4d --project` on a `.dproj` with a conditional `DCCReference` lints the same file set the resolver uses.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
