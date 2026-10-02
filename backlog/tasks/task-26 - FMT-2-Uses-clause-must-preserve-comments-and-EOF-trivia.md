---
id: TASK-26
title: 'FMT-2: Uses clause must preserve comments and EOF trivia'
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
ordinal: 26000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (FMT-2). Read that file's Global constraints section before starting.

Severity: high. Cost: correctness.

**Problem.** `doc_builder.rs:23-25`, `:435-474` (`build_uses`) rebuilds the
clause through `UsesItem` (`uses.rs:379-400` `extract_uses_items`), which has
unit, ifdef-block and directive variants but no comment variant; comments
inside the clause have no emission path. Both attachment maps
(`comments.rs:42-70`, `directive_map.rs:148-157`) drop standalone trivia
after the last code leaf.

**Approach.** Add `UsesItem::Comment(String)` and carry leading/trailing
trivia on `UsesItem::Unit`; emit through Docs. Attach EOF trivia to a
synthetic end leaf in both maps.

**Tests first.** Both fixtures in `fmt_bugs_test.rs` (fail today).

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `uses A, // why A\n B;` keeps the comment; a file ending in a comment after `end.` keeps it.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
