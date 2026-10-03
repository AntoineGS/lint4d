---
id: TASK-24
title: 'LINT-6: Use-after-free on AST effects, deduplicated after convergence'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - lint
  - correctness
  - cross-repo
milestone: m-3
dependencies: []
priority: high
type: bug
ordinal: 24000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-6). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: correctness.

**Problem.** `crates/lint4d/src/rules/use_after_free.rs:98-106`,
`:146-216` (`process_statement`), `:219-256`, `:275-299` re-parse statement
text with string matching: `.Free()` does not match the `.free` suffix
check; assignments return before RHS references are checked; identifier
search can match inside string literals or comments in the range; findings
are emitted during worklist iteration so a revisited block reports twice.
AST helpers for free calls already exist in `rules/helpers.rs:89-129` and
cfg-pascal classifies calls in `../cfg-pascal/src/calls.rs:207-261`.

**Approach.** Compute per-statement `Effects { frees: Vec<Sym>,
assigns: Vec<Sym>, reads: Vec<Sym> }` from the AST once using the helpers;
run the dataflow over effects; collect findings in a set keyed by
(range, symbol); emit after the worklist converges.

**Tests first.** Those three in `crates/lint4d/tests/rules_use_after_free_test.rs`
(first and third fail today).

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Fixtures: `Obj.Free(); Obj.Foo;` reports; `Obj := nil` after free does not; a loop body reports once.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
