---
id: TASK-30
title: 'BUILD-2: Separate the barrier target and run it in CI'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 03:11'
labels:
  - arch-review
  - build
  - maintenance
milestone: m-4
dependencies: []
priority: medium
type: chore
ordinal: 30000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (BUILD-2). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-03 (from TASK-1.1): `cargo clippy --workspace --all-targets --all-features -- -D warnings` was failing on master 81418f2 (collapsible_if in server.rs and tests/protocol.rs, manual_is_multiple_of in tests/protocol.rs). CI's clippy job does not enable test-support, so nobody noticed. Branch fix/master-tests-clippy (9f3a2f1) cleans them up. When this task adds the barrier target to CI, also run clippy with --features test-support (or --all-features). Note that every #[cfg(feature = "test-support")] test in tests/protocol.rs is skipped by `cargo test --workspace`, including the two TASK-1.2 sixty_sixth_ordinary_frame tests.
<!-- SECTION:NOTES:END -->
