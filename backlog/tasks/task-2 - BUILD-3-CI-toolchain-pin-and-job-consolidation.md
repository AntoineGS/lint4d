---
id: TASK-2
title: 'BUILD-3: CI toolchain pin and job consolidation'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - build
  - build-time
milestone: m-0
dependencies: []
priority: medium
type: chore
ordinal: 2000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (BUILD-3). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: medium. Cost: build time, reproducibility.

**Problem.** `rust-toolchain.toml` pins 1.99.0; every CI job uses
`dtolnay/rust-toolchain@stable`. Check and test each run on three OSes
(`ci.yml:22-59`); coverage rebuilds instrumented (`:80`); `cargo install
cargo-audit` and `cargo-llvm-cov` are unversioned (`:109-110`); `lfs: true`
is set though `.gitattributes` only marks `*.dcu binary`.

**Approach.** Use `dtolnay/rust-toolchain@1.99.0` for required jobs and one
advisory `stable` job with `continue-on-error`. Merge check into test per
OS (test implies check). Pin tool versions via `taiki-e/install-action`.
Run coverage on a schedule or on master only. Drop `lfs: true` unless LFS
objects exist.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Required jobs use the pinned toolchain; total CI wall time recorded before and after in the PR.
<!-- AC:END -->
