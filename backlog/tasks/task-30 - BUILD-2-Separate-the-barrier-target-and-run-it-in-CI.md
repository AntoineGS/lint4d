---
id: TASK-30
title: 'BUILD-2: Separate the barrier target and run it in CI'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
labels:
  - arch-review
  - build
  - maintenance
milestone: m-4
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: chore
ordinal: 30000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (BUILD-2). Read that file's Global constraints section before starting.

Severity: medium. Cost: maintenance.

**Problem.** `tests/protocol_barriers.rs:1-7` is `include!("protocol.rs")`
behind `test-support`, so a feature-enabled run compiles the 60k-line file
twice. CI never enables `test-support` (`.github/workflows/ci.yml:48`,
`:59`, `:110`). The Lua smoke tests (`tests/neovim.rs:21-33`) pass silently
when `nvim` is absent, and CI does not install Neovim.

**Approach.** Extract the shared harness into `tests/common/`; make
`protocol_barriers.rs` contain only barrier cases; add a CI step
`cargo test -p pascal-lsp --features test-support --test protocol_barriers`
and a step that installs a pinned Neovim and sets a `REQUIRE_NVIM=1` env
that makes `neovim.rs` fail rather than skip.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 CI runs both; a missing `nvim` fails the job.
<!-- AC:END -->
