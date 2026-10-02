---
id: TASK-60
title: 'LINT-16: Fold cfg-core into cfg-pascal'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-02 23:23'
labels:
  - arch-review
  - lint
  - maintenance
  - cross-repo
milestone: m-6
dependencies:
  - TASK-3
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: task
ordinal: 60000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-16). Read that file's Global constraints section before starting.

Severity: medium. Cost: maintenance.

**Problem.** cfg-core (about 1,000 lines) is "language-agnostic" but has no
second frontend; it exposes petgraph publicly, carries Delphi-flavoured
handler kinds (`../cfg-core/src/types.rs:29-35`), domain summaries, and a
call graph lint4d does not use (`engine/mod.rs:139`). cfg-pascal couples to
the concrete builder (`pascal_builder.rs:551-552`). The split costs a
git-pin plus the `cfg_pascal::cfg_core` re-export dance documented in
`docs/shared-resolver-architecture.md:231-241`.

**Approach.** Move cfg-core's modules into `../cfg-pascal/src/core/`,
keep the public paths via `pub mod core`, delete the git dependency, bump
the lint4d pin, remove cfg-core from `.cargo/deny.toml` and
`.cargo/config.toml`. Archive the cfg-core repo with a README pointer.

**Tests first.** None; mechanical. Run both test suites.

**Depends on.** BUILD-4 decision; if the answer is "monorepo", do that
instead.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `cargo tree -p lint4d | grep cfg-core` is empty; all lint4d and cfg-pascal tests green.
- [ ] #2 No behaviour change: the existing suites named in the task stay green (cargo fmt --check, clippy -D warnings, and the affected crate's tests).
<!-- AC:END -->
