---
id: TASK-29
title: 'BUILD-1: Move the protocol behavior matrix in-process'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - build
  - build-time
milestone: m-4
dependencies:
  - TASK-30
priority: high
type: chore
ordinal: 29000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (BUILD-1).

Severity: high. Cost: build time, test time, flakiness.

**Problem.** `crates/pascal-lsp/tests/protocol.rs` is 60,273 lines with 855
tests and 835 `TestServer::launch` sites (`:784-825` spawns the binary per
test). 21 `thread::sleep` calls act as synchronization
(`:33258-33305` 50 ms after init; `:11612-11622`, `:11710-11736` 2 s or
more; 100 ms negative-observation windows at `:969-1039`).
`AGENT_TODOS.md` lists the timing-sensitive tests that flake under load.
Relinking this target is most of the 1m07s per-edit cost.

**Approach.**
1. Add an in-process harness `pascal_lsp::test_support::Harness` that
   drives `server::handle_message` directly with a fake transport.
2. Move tests in batches of ~50 from `protocol.rs` to new files under
   `tests/protocol/` using the harness; keep subprocess tests only for
   lifecycle (init, shutdown, exit, stdout pressure).
3. Replace each `sleep` with an explicit readiness signal from the barrier
   harness or a drained-notification acknowledgement.

**Tests first.** This task is tests; every moved test must pass before and
after.

**Depends on.** BUILD-2 for the barrier harness.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `protocol.rs` under 5,000 lines; zero `thread::sleep` in pascal-lsp tests except inside bounded poll helpers; re-measured touch-one-file rebuild under 30 s.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related task converted from `AGENT_TODOS.md`: TASK-1.2 (Make timing-sensitive protocol tests robust to machine load).
<!-- SECTION:NOTES:END -->
