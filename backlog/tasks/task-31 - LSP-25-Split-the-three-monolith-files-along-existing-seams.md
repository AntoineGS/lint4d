---
id: TASK-31
title: 'LSP-25: Split the three monolith files along existing seams'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - lsp
  - maintenance
  - build-time
milestone: m-4
dependencies: []
priority: medium
type: task
ordinal: 31000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-25).

Severity: medium. Cost: maintenance and build time.

**Problem.** `navigation.rs` has roughly 21.5k implementation lines plus 6.2k
test lines combining storage (`:555-13902`), diagnostics (`:2330`,
`:2553`), resolution (`:6488`), model types (`:14032`), parsing and index
construction (`:15980`) and AST helpers (`:16665-21497`). `server.rs` and
`workspace.rs` mix transport, scheduling, diagnostic stores, configuration,
source storage, project validation and reconciliation accounting
(`server.rs:60-191`, `:4151-4250`, `:4755-5083`; `workspace.rs:1305-1782`,
`:4435-4997`). Touching any leaf file in pascal-lsp costs 1m07s before a
test can run (baseline table).

**Approach.** Mechanical moves only, no behavior change, one commit per
extracted module:
- `navigation/model.rs` (types from `:14032`), `navigation/parse.rs`
  (`:15980` onward), `navigation/ast.rs` (`:16665-21497`),
  `navigation/resolve.rs` (`:6488` onward), `navigation/diagnostics.rs`
  (`:2330-2553` region).
- `server/transport.rs`, `server/scheduler.rs`, `server/delivery.rs`,
  `server/records.rs` (freshness records).
- `workspace/documents.rs`, `workspace/recovery.rs`,
  `workspace/diagnostics.rs`, `workspace/catalogue.rs`.
Move each embedded test module next to its code.

**Tests first.** None; pure moves. `cargo fmt`, clippy and the full
pascal-lsp suite after every move.

**Depends on.** Do after LSP-22 to avoid moving code that is about to be
deleted, or before Theme A refactors if you prefer smaller files first;
either way coordinate so two agents are not moving the same regions.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No file in `crates/pascal-lsp/src` exceeds 5,000 lines. `cargo test -p pascal-lsp` green. Re-measure the touch-one-file rebuild.
- [ ] #2 No behaviour change: the existing suites named in the task stay green (cargo fmt --check, clippy -D warnings, and the affected crate's tests).
<!-- AC:END -->
