---
id: TASK-25
title: 'FMT-1: Blank-line normalization must not touch protected spans'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
labels:
  - arch-review
  - fmt
  - correctness
milestone: m-3
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: high
type: bug
ordinal: 25000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (FMT-1). Read that file's Global constraints section before starting.

Severity: high. Cost: correctness.

**Problem.** `crates/fmt4d/src/formatter.rs:70` runs
`normalize_blank_lines` over the entire rendered string;
`blank_lines.rs:5-27` trims whitespace-only lines and collapses blank runs
with no knowledge of comments, format-off regions or multiline string
literals, so `Doc::Raw` (`doc_builder.rs:68-70`, `:157`, `:198-204`) is not
verbatim end to end.

**Approach.** Emit blank-line decisions in the Doc builder (`Doc::Hardline`
counts) and delete the post-pass; or, as a minimal step, make the post-pass
skip byte ranges recorded as protected by the renderer.

**Tests first.** That fixture in `crates/fmt4d/tests/fmt_bugs_test.rs`
(fails today).

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A multiline string literal containing three consecutive blank lines and trailing spaces round-trips byte-identical inside a `{$FORMAT OFF}` region and outside it.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
