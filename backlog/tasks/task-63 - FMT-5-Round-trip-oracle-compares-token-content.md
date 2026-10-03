---
id: TASK-63
title: 'FMT-5: Round-trip oracle compares token content'
status: Deferred
assignee:
  - '@claude'
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 21:55'
labels:
  - arch-review
  - fmt
  - correctness
milestone: m-6
dependencies:
  - TASK-25
  - TASK-26
modified_files:
  - crates/fmt4d/tests/fmt_roundtrip_test.rs
priority: medium
type: bug
ordinal: 63000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (FMT-5). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: medium. Cost: correctness.

**Problem.** `tests/fmt_roundtrip_test.rs:6-21` `ast_eq` compares node kinds
and child counts, filters extras, and never compares identifier or literal
text; changing an identifier passes, dropping a comment passes. Default
uses sorting (`config.rs:103-110`) reorders initialization, so intentional
transformations need an explicit allow-list.

**Approach.** Extend `ast_eq` to compare leaf text for identifiers,
literals and comments, with an exception for uses-clause children when
sorting is enabled. Run it over every fixture under `tests/fixtures`.

**Tests first.** This task is tests; it will likely expose FMT-1/FMT-2
cases.

**Depends on.** FMT-1, FMT-2 to be green.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The oracle fails on an injected identifier change (sanity test) and passes on all fixtures.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/fmt-uses-trivia, branch fix/fmt-uses-trivia (same branch, on top of TASK-26 commit afdc0b7).

Problem: crates/fmt4d/tests/fmt_roundtrip_test.rs ast_eq compares node kinds and child counts only; identifier/literal text and comments are never compared.

Steps:
1. Replace ast_eq with an oracle (returns Result so it can be sanity-tested) that compares: node kinds and non-extra child counts; the text of every code leaf (and whole text of literal nodes, whose children do not cover them); the sequence of comment and directive texts. Exception when uses sorting is on: each declUses is compared as a multiset of its leaf tokens, and the trivia inside it as a multiset.
2. Sanity tests: the oracle rejects an injected identifier change and a dropped comment.
3. Fixture sweep test: run the oracle (plus idempotency) over every .pas under tests/fixtures (repo) and crates/fmt4d/tests/fixtures.
4. Switch the existing roundtrip tests to the new oracle.
5. Fixture failures caused by other formatter bugs are not fixed here: one backlog task per distinct bug with fixture + minimal repro, and a documented, named allow-list of known-failing fixtures citing those task IDs.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Ruling: fixtures the stronger oracle rejects because of other formatter bugs are not fixed here. Each distinct bug gets a backlog task, and the fixture goes into KNOWN_FAILING_FIXTURES (crates/fmt4d/tests/fmt_roundtrip_test.rs), a named, documented allow-list of (fixture path, task ID). The sweep fails if a listed fixture starts passing or no longer exists, so the list cannot go stale. The comparison itself is not weakened. — keeps the oracle honest without mixing unrelated fixes into this branch (dispatch instruction).

Ruling: the formatter's documented var-list expansion (build_comma_ident_decl: a DECL_VAR `A, B: T;` is always written as one declaration per identifier) is a second intentional transformation besides uses sorting. The oracle compares such a node's leaf tokens after expanding the list the same way; comments are still compared in order, so comment loss or duplication from the expansion is still caught. — the task allows explicit exceptions for intentional transformations; this is the only other one the fixtures need.

Ruling: under uses sorting, comments inside a declUses and those after its final `;` on the same line are compared as a multiset per clause, since TASK-26 makes unit comments move with their units. All other comments/directives are compared as an ordered sequence of texts. Position relative to code is not checked (e.g. `A {c},` → `A, {c}` passes). — catches drops, duplicates, edits and reorders, which is what the task asks.

Found and filed: TASK-102 (uses clause with `;` inside each {$IFDEF} branch gets `SysUtils,` + `{$ENDIF};`, does not compile; fixture bucket_c_uses_semi.pas, allow-listed) and TASK-103 (var-list expansion duplicates the suffix's trailing comment and drops comments between names; found while probing, not in any fixture).

Evidence: with the new test file on the pre-TASK-26 source (temporary worktree at afdc0b7~1), oracle_accepts_the_formatted_source and oracle_rejects_a_dropped_uses_comment fail ("comments or directives in uses clauses changed"), so the oracle catches the FMT-2 bug.

GREEN: `CARGO_BUILD_JOBS=3 cargo test -p fmt4d --test fmt_roundtrip_test` 22 passed (incl. every_fixture_round_trips over 22 fixtures, oracle_rejects_an_identifier_change); full `cargo test -p fmt4d` green; clippy -D warnings and fmt --check clean. Commit 47a304a.

Implemented on branch fix/fmt-uses-trivia (worktree .worktrees/fmt-uses-trivia), awaiting review and merge.

AC #1 caveat: 21 of 22 fixtures pass the oracle; bucket_c_uses_semi.pas is allow-listed against TASK-102 (a real formatter bug, out of scope here per the dispatch ruling). The reviewer may prefer to leave AC #1 unchecked until TASK-102 lands.

Fix round 1 (review): AC #1 unchecked (controller ruling). The dispatch explicitly permitted the allow-list approach, but AC #1 says the oracle passes on ALL fixtures. It can be checked once TASK-102 is fixed and bucket_c_uses_semi.pas is removed from KNOWN_FAILING_FIXTURES.

Fix round 1 (review): under uses sorting a uses clause was compared as a flat multiset of leaf tokens, so a qualifier swap (`System.Classes, Vcl.SysUtils` -> `System.SysUtils, Vcl.Classes`), units moving across {$IFDEF}/{$ELSE} branches and punctuation moves all passed. Commit 0257a42: the clause is now parsed into whole unit names, punctuation and nested blocks. The oracle requires the same block skeleton (directive texts in order), the same unit multiset at the top level and in each branch, and, for every branch configuration (at most 4096), the same unit/punctuation shape. New sanity tests: oracle_accepts_sorted_units_around_a_conditional_block, oracle_rejects_a_unit_qualifier_swap, oracle_rejects_a_unit_moved_across_ifdef (branch move and hoist out of the block), oracle_rejects_punctuation_that_breaks_a_configuration.

Fix round 1 (review): a KNOWN_FAILING_FIXTURES entry now records the expected error text (struct KnownFailure { path, task, error }). bucket_c_uses_semi.pas must fail with `per configuration ["U,U;", "U,U;"] -> ["U,U,;", "U,U,;"]`. Checked by temporarily changing the expectation: the sweep then fails with "fails differently than expected for TASK-102".

Fix round 1: the stronger oracle exposed another pre-existing bug, filed as TASK-108. When sorting leaves an {$IFDEF} block last, the clause ends with `{$ENDIF};` while the units keep their commas (`uses A, B, C, ;`). No fixture hits it; a sanity test uses its shape.

Fix round 1 verification: `cargo test -p fmt4d` all green (fmt_roundtrip_test 26 passed); clippy -p fmt4d --all-targets -D warnings and fmt --check clean.

Merged into master as 4d694bf (no-ff), 2026-10-03. Merged master tree is identical to the verified integration tree: cargo fmt --check clean, clippy --workspace --all-targets -D warnings clean, cargo test --workspace 3256 passed / 0 failed / 9 ignored.

Deferred: the stronger oracle is merged. AC #1 (passes on all fixtures) is blocked by TASK-102: bucket_c_uses_semi.pas is in KNOWN_FAILING_FIXTURES. Unblock: fix TASK-102, remove the allow-list entry, check AC #1.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: the fmt4d round-trip oracle (ast_eq in crates/fmt4d/tests/fmt_roundtrip_test.rs) compared only node kinds and child counts, so changed identifiers or literals and dropped comments passed. It also ran only over a handful of inline sources.

Change (branch fix/fmt-uses-trivia, commits 47a304a and 0257a42, on top of TASK-26):
- check_same_program(before, after, uses_sorted) -> Result checks that both sides have the same node kinds and code child counts, the same text in every code leaf (the whole text for literalString/literalChar), and the same ordered sequence of comment and directive texts.
- Intentional transformations, documented on the function:
  - Uses sorting: a declUses must keep the same conditional-block skeleton and directive texts in order, the same multiset of whole unit names at the top level and in each branch, and, for every branch configuration, the same unit/punctuation shape. Trivia inside the clause (or after its ';' on the same line) is compared as a multiset.
  - A declVar list 'A, B: T;' is compared after per-identifier expansion.
- Sanity tests show the oracle rejects identifier, literal and comment changes, a dropped uses comment, a qualifier swap, units moved across or out of {$IFDEF} branches, and a dangling comma in one configuration. They also show it accepts legitimate sorting and var expansion.
- every_fixture_round_trips runs the oracle and an idempotency check over all 22 .pas fixtures under tests/fixtures and crates/fmt4d/tests/fixtures. KNOWN_FAILING_FIXTURES entries name the fixture, the task and the expected error text. The test fails if a listed fixture passes, fails differently, or disappears.
- The existing roundtrip tests now use the new oracle.

Status: AC #1 is NOT checked. 21 of 22 fixtures pass; bucket_c_uses_semi.pas is allow-listed against TASK-102, as the dispatch allowed. AC #1 can be checked once TASK-102 removes that entry.

Verification: fmt_roundtrip_test 26/26 pass; full cargo test -p fmt4d green; clippy -D warnings and fmt --check clean. On the pre-TASK-26 formatter the oracle fails on the dropped uses comment.

Follow-ups filed:
- TASK-102: ';' inside each {$IFDEF} branch produces code that does not compile.
- TASK-103: var-list expansion duplicates or drops comments.
- TASK-108: sorting that leaves an {$IFDEF} block last gives a dangling comma.

Known limits:
- The oracle checks comment text and order only, not comment position relative to code.
- Both sides are parsed with parse_file, not the patched parse that format_source uses (unchanged from the old test).
- More than 4096 branch configurations in one clause is reported as an error rather than compared.
<!-- SECTION:FINAL_SUMMARY:END -->
