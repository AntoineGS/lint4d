---
id: TASK-22
title: 'LINT-4: Naming auto-fix must be binding-aware or restricted'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - lint
  - correctness
milestone: m-3
dependencies: []
priority: high
type: bug
ordinal: 22000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-4).

Severity: high. Cost: correctness.

**Problem.** `crates/lint4d/src/fix/rename_map.rs:151-182` keys renames by
lowercase spelling per file. `fix/apply.rs:236-278` applies the map without
proving the binding and skips dotted RHS identifiers. `fix/mod.rs:158` and
`main.rs:741-777` write results without project-wide reference or collision
validation and discard parse diagnostics. A public type rename does not
update consumers; a shadowing local can receive the file-level replacement.
pascal-lsp has a binding-aware rename (`navigation/rename.rs:1019-1078`).

**Approach.** Short term: restrict `--fix-fmt` to casing-only changes
(`identifier-casing`) and to local variables whose scope is one routine;
emit a warning listing skipped declaration renames. Long term: expose the
LSP rename planner as a library (`pascal-lsp` already depends on lint4d, so
the planner would have to move down into a new `pascal-semantics` crate;
record this as a follow-up, not part of this task).

**Tests first.** That fixture in `crates/lint4d/tests/fix_test.rs` (fails
today: public type renamed in place).

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `lint4d --fix-fmt` on a unit with a public `TFoo` missing its prefix and a local shadowing identifier leaves the public type untouched, renames the local, and prints the skip warning.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
