---
id: TASK-26
title: 'FMT-2: Uses clause must preserve comments and EOF trivia'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 21:55'
labels:
  - arch-review
  - fmt
  - correctness
milestone: m-3
dependencies: []
modified_files:
  - crates/fmt4d/src/comments.rs
  - crates/fmt4d/src/directive_map.rs
  - crates/fmt4d/src/doc_builder.rs
  - crates/fmt4d/src/uses.rs
  - crates/fmt4d/tests/fmt_bugs_test.rs
priority: high
type: bug
ordinal: 26000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (FMT-2). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

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
- [x] #1 `uses A, // why A\n B;` keeps the comment; a file ending in a comment after `end.` keeps it.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/fmt-uses-trivia, branch fix/fmt-uses-trivia (from master 81418f2).

Root cause:
- build_uses (crates/fmt4d/src/doc_builder.rs, fn build_uses) rebuilds the clause from UsesItem (uses.rs extract_uses_items / parse_pp_uses_block) and never descends into the clause's leaves, so every comment attached (by CommentMap) to a leaf inside declUses — including the `uses` keyword's leading/trailing comments — is never emitted. collect_pre_uses_directives only looks at sibling extras and stops at a comment.
- CommentMap::build (comments.rs) and attach_one (directive_map.rs) attach standalone trivia to the next leaf; after the last leaf of the file there is none, so it is dropped.

Steps:
1. RED: regression tests in crates/fmt4d/tests/fmt_bugs_test.rs (uses trailing comment, leading comment moving with sorted unit, comments around keyword/ifdef block/semicolon, comment and directive after `end.`), all with idempotency checks.
2. uses.rs: UsesItem::Unit becomes a struct variant carrying leading/trailing comments; add UsesItem::Comment(String) for standalone comments with no unit to attach to; IfDefBlock gets trailing comments. extract_uses_items/parse_pp_uses_block take the CommentMap and gather comments from the leaves of each child (leading of first leaf, trailing of last leaf, and both for the following comma/semicolon). Layout into lines that may contain multi-line block comments; build_uses emits each line as one token so multi-line comments stay protected. format_uses_items keeps its String API.
3. build_uses emits leading comments + directives of the `uses` keyword leaf (by span order) instead of collect_pre_uses_directives, and its trailing comments after the keyword.
4. CommentMap/DirectiveMap: standalone trivia after the last leaf go to an end-of-file list (the "synthetic end leaf"); DocBuilder::build appends them, one per line, keeping a source blank line before each.
5. GREEN focused tests, then full fmt4d suites, fmt --check, clippy -p fmt4d.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED (before the fix): `CARGO_BUILD_JOBS=3 cargo test -p fmt4d --test fmt_bugs_test -- uses_clause_ comment_after_final directive_after_final` → 0 passed, 5 failed: uses comments dropped (`uses\n  A,\n  B;`), `// deps` count 0, output after `end.` was just `end.\n` for both the comment and the `{$ENDREGION}` cases.

Ruling: a comment on a unit's line (before or after its `,`/`;`) belongs to that unit and moves with it when sorting reorders units; comments on their own lines above a unit are its leading comments and move with it too. Comments that were between a unit and its punctuation are emitted after the punctuation (`A {c}, B` → `A, {c}`) — keeping them before the punctuation would let a `//` comment swallow the `,`/`;`. — smallest rule that never breaks code and is idempotent.

Ruling: a comment that follows a `//` comment on the same logical line is moved to its own line (a `//` would swallow it); a comment with no unit to attach to (e.g. after a pinned directive) becomes a pinned UsesItem::Comment. Comments after `{$ELSE}`/`{$ENDIF}`/`{$IFDEF}` stay on that directive's line. — preserves every comment; output stable on the second pass.

Ruling: 'synthetic end leaf' implemented as a dedicated end-of-file list in CommentMap (eof_comments) and DirectiveMap (eof_directives) rather than a fake node id; DocBuilder::build appends them after the root unless the whole root is format-off (its verbatim text already spans them). Same-line spacing and one blank line are kept as in the source. — equivalent behaviour without inventing node ids.

Side effect worth knowing: `{$FMT.ON}` (or any directive/comment) after `end.` was previously dropped; doc_builder unit test format_off_region_returns_raw asserted the old shape and was updated to expect the Raw unit followed by `{$FMT.ON}` once. collect_pre_uses_directives was removed: it stopped at the first comment sibling and could not see same-line attachment; leading trivia of the `uses` keyword now comes from the maps.

GREEN: `CARGO_BUILD_JOBS=3 cargo test -p fmt4d` all suites ok (lib 110, fmt_bugs_test 142, ...); `cargo clippy -p fmt4d --all-targets -- -D warnings` clean; `cargo fmt --check` clean. Commit afdc0b7.

Implemented on branch fix/fmt-uses-trivia (worktree .worktrees/fmt-uses-trivia), awaiting review and merge.

Follow-up filed: TASK-104 (pre-existing): a uses clause whose last item is a pinned directive (`uses A {$I x.inc};`) loses its `;` and the output no longer parses.

Fix round 1 (review): a file with no code leaves (only comments/directives) got a stray leading space, `// only a comment` -> ` // only a comment`. eof_trivia_doc measured the first gap from byte 0, and an empty gap still produced Raw(" "). The first item now has no previous end (None). RED: new test fmt_bugs_test::file_with_only_trivia_gets_no_leading_space failed with left " // only a comment\n{$DEFINE X}\n" vs right "// only a comment\n{$DEFINE X}\n". GREEN after commit 0b9f3c7, and it checks idempotency. Full cargo test -p fmt4d green.

Follow-up filed from the review: TASK-107, end-of-file trivia inside an unclosed {$FMT.OFF} region is re-laid-out instead of emitted verbatim. Known behaviour recorded on TASK-64: under sorting, a clause header comment above the first unit moves with that unit.

Merged into master as 4d694bf (no-ff), 2026-10-03. Merged master tree is identical to the verified integration tree: cargo fmt --check clean, clippy --workspace --all-targets -D warnings clean, cargo test --workspace 3256 passed / 0 failed / 9 ignored.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: fmt4d dropped every comment inside a uses clause (build_uses rebuilt the clause from UsesItem values, which had no comment slot, and never descended into the clause's leaves) and every standalone comment/directive after the file's last token (CommentMap and DirectiveMap only attach trivia to a following leaf or a same-line preceding leaf).

Change (commit afdc0b7, branch fix/fmt-uses-trivia):
- uses.rs: UsesItem::Unit is now a struct variant { name, leading, trailing }; new UsesItem::Comment for comments with no unit to attach to; CondBranch.trailing, IfDefBlock.else_trailing/trailing for same-line comments on conditional directives. extract_uses_items/parse_pp_uses_block read comments from the CommentMap (leading of a child's first leaf, trailing of its last leaf, both sides of the following ,/;). Units carry their comments through sorting. New pub(crate) layout_uses_items returns lines (multi-line block comments stay one line/token so the renderer protects them); format_uses_items keeps its String API.
- doc_builder.rs: build_uses emits the uses keyword's leading comments/directives (in source order, same-line layout and blank line kept) and its trailing comments; collect_pre_uses_directives removed. DocBuilder::build appends end-of-file trivia via shared trivia_items/trivia_run_doc helpers.
- comments.rs / directive_map.rs: end-of-file lists (eof_comments / eof_directives); DirectiveMap attachment refactored into a method.

Tests: 5 new regression tests in crates/fmt4d/tests/fmt_bugs_test.rs (uses trailing comment; leading comment moving with sorted unit; comments around keyword, ifdef block and semicolon; comment and directive after end.), each with an idempotency check. All 5 failed before the change (RED recorded in notes), pass now. Full `cargo test -p fmt4d` green, clippy -D warnings and fmt --check clean. Manual probes (elseif/else/nested blocks, multi-line block comments, same-line trivia after end.) all idempotent.

Known limits: a comment between a unit and its , or ; is emitted after the punctuation; a comment that follows a // comment is moved to its own line, where on the next run it leads the following unit; comment/directive relative order inside the clause can swap when they were interleaved on one line (stable after one pass). Stale citations: build_uses is now doc_builder.rs ~fn build_uses, extract_uses_items in uses.rs ~line 420.
<!-- SECTION:FINAL_SUMMARY:END -->
