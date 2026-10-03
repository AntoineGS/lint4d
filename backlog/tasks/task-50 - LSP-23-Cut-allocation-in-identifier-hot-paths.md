---
id: TASK-50
title: 'LSP-23: Cut allocation in identifier hot paths'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-6
dependencies: []
priority: medium
type: enhancement
ordinal: 50000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-23).

Severity: medium. Cost: runtime cpu.

**Problem.** `navigation.rs:19772-19773`, `:21116-21120`, `:7376-7378`,
`:7779-7802`, `navigation/rename.rs:1864-1865`, `:1924-1932`, `:1959-1977`:
every identifier is copied by `node_text` then copied again lowercased,
even when discarded by the name filter. Export lookup allocates a `String`
to probe a tuple-keyed map; unit lookup returns cloned `Url` vectors;
occurrence matching uses linear `symbols.iter().any` although
`direct_symbol_indices` exists (`:19021-19027`). Several walkers materialize
a child `Vec` per node though a cursor walker exists at `:19063-19099`.

**Approach.** Intern identifier names into `u32` symbols per index
(a `HashMap<Box<str>, u32>` with case-folded keys); compare symbols, not
strings. Use `&str` slices of the source for text. Use the existing
declaration indexes and cursor walker everywhere.

**Tests first.** Pure refactor; existing tests. Add the benchmark.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The `cargo bench`-style microbenchmark you add for occurrence discovery on a 50k-line unit shows at least 2x fewer allocations (count with a counting allocator in the bench).
- [ ] #2 No behaviour change: the existing suites named in the task stay green (cargo fmt --check, clippy -D warnings, and the affected crate's tests).
<!-- AC:END -->
