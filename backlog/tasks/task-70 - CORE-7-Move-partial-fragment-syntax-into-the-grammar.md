---
id: TASK-70
title: 'CORE-7: Move partial-fragment syntax into the grammar'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-02 23:23'
labels:
  - arch-review
  - core
  - runtime
  - maintenance
  - cross-repo
milestone: m-6
dependencies:
  - TASK-4
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: enhancement
ordinal: 70000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-7). Read that file's Global constraints section before starting.

Severity: medium. Cost: both.

**Problem.** `pascal-core/src/directive_fragment_rewrite.rs:100-146`,
`:163-202`, `:529-540`, `:584-616` blank conditional markers around partial
control-flow headers and may blank whole "non-Pascal" `IF` blocks, parsing
each body in two synthetic harnesses before reparsing the file; patch
positions are computed by rescanning from the start. `parser.rs:108-157`,
`:178-203` wires it into every parse; `fmt4d/src/directive_map.rs` reinjects;
`pascal-lsp/src/navigation.rs:16053-16060` tracks opaque ranges. About 633
implementation lines plus 454 test lines in core, plus consumers.
`../tree-sitter-pascal/grammar.js:888-913` already has `ppFragmentExpr` and
`ppFragmentStmt` externals.

**Approach.**
1. Collect every fixture that exercises the rewriter into
   `../tree-sitter-pascal/test/corpus/preprocessor-fragments.txt`.
2. Extend the fragment externals/grammar until those parse without the
   rewriter (pair with GRAM-1 so parser size is tracked).
3. Reduce the rewriter to an opaque-body adapter for bodies that are
   genuinely not Pascal; delete the rest.

**Tests first.** Corpus tests in the grammar repo (fail before grammar
change).

**Depends on.** GRAM-4 (regeneration check) so grammar changes are
reproducible. Requires the three-repo pin bump.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `directive_fragment_rewrite.rs` is under 200 lines and `crates/pascal-core/tests/conditional_task24.rs` plus fmt4d fragment tests (`crates/fmt4d/tests/pp_fragment.rs`) stay green.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
