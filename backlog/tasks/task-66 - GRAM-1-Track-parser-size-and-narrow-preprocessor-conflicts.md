---
id: TASK-66
title: 'GRAM-1: Track parser size and narrow preprocessor conflicts'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - grammar
  - runtime
  - build-time
  - cross-repo
milestone: m-6
dependencies:
  - TASK-4
priority: medium
type: enhancement
ordinal: 66000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (GRAM-1). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: medium. Cost: build and runtime.

**Problem.** Versus upstream Isopod master: states 2,715 to 5,644, parser.c
3.5 MB to 9 MB, 129,529 to 310,590 lines. `grammar.js:361-422` has 36
conflict entries; `:888-903` defines a universal `ppBlock` admitting
declaration items, sections, statements, nesting and punctuation.
Every clean consumer build compiles parser.c with `cc`
(`bindings/rust/build.rs`). Parse-time impact is unmeasured.

**Approach.**
1. Add `scripts/parser-stats.sh` printing state count and parser.c size;
   record in the README and in each grammar PR.
2. Add a `tree-sitter parse --time` benchmark over `examples/` and a large
   RTL unit; record baseline.
3. Replace the universal `ppBlock` with context-specific variants
   (`ppDeclBlock`, `ppStmtBlock`, `ppSectionBlock`) and remove conflicts
   that become unnecessary; stop when corpus tests pass and stats go down.

**Tests first.** Corpus is the test; add cases for each removed conflict.

**Depends on.** GRAM-4.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 State count and parse time are recorded, and the context-specific split lands with no corpus regressions.
<!-- AC:END -->
