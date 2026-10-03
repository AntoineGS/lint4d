---
id: TASK-1.1
title: Fix the tests and clippy lints failing on master
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:28'
updated_date: '2026-10-03 21:55'
labels:
  - agent-todos
  - workspace-perf
  - build
dependencies: []
modified_files:
  - crates/pascal-lsp/src/workspace.rs
  - crates/pascal-lsp/src/server.rs
  - crates/pascal-lsp/tests/protocol.rs
parent_task_id: TASK-1
priority: high
type: bug
ordinal: 87000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

Failing on `master` (`2b75a51`) in every full run so far, unrelated to
the above:
`selection_ranges_keep_line_comment_ranges_valid_for_lf_and_crlf`,
`selection_ranges_keep_non_bmp_comment_endpoints_and_mixed_positions_valid`,
`selection_ranges_accept_crlf_line_comments_in_mixed_position_batches`,
`assistance_retains_lazy_package_project_observation_at_the_read_boundary`,
`neovim_standard_symbol_reference_and_highlight_queries`,
`code_action_creation_rejects_overlong_identity_and_caps_serialized_output`,
`sixty_four_project_metadata_changes_share_the_notification_budget_and_stale_pull_results`,
`type_hierarchy_candidate_cap_refuses_partial_subtypes`. Clippy on the
current toolchain also fails in `pascal-core`, `lint4d` and `pascal-lsp`.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/master-tests-clippy, branch fix/master-tests-clippy (from master 81418f2).
1. Re-run the 8 listed tests by name and `cargo clippy --workspace --all-targets -- -D warnings` on 1.99.0; record what still fails.
2. For each failing test: find root cause; fix code if wrong, fix test only when expectation is stale (with recorded reasoning). No deleting/#[ignore].
3. Minimal behavior-preserving clippy fixes.
4. cargo fmt --check, clippy per touched crate, one `cargo test --workspace` at the end (TASK-1.2 timing tests excluded from scope).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Current truth on master 81418f2, toolchain 1.99.0 (2026-10-03):
- All 8 listed tests PASS when run by name (unmodified 81418f2 code; the only change before running them was a test-only clippy fix in a cfg-gated test helper that does not affect them):
  pascal-lsp --lib: selection_ranges_keep_line_comment_ranges_valid_for_lf_and_crlf, selection_ranges_keep_non_bmp_comment_endpoints_and_mixed_positions_valid, assistance_retains_lazy_package_project_observation_at_the_read_boundary (3 passed).
  pascal-lsp --test neovim: neovim_standard_symbol_reference_and_highlight_queries (ok).
  pascal-lsp --test protocol: selection_ranges_accept_crlf_line_comments_in_mixed_position_batches, type_hierarchy_candidate_cap_refuses_partial_subtypes, code_action_creation_rejects_overlong_identity_and_caps_serialized_output (3 passed).
  sixty_four_project_metadata_changes_share_the_notification_budget_and_stale_pull_results is #[cfg(feature = "test-support")] (protocol.rs:38289), so plain `cargo test --workspace` never compiles it; with `--features test-support --test protocol` it passes (13.9 s).
  They were fixed on master between 2b75a51 and 81418f2; no code change needed here.
- `cargo clippy --workspace --all-targets -- -D warnings` FAILED with one error: unused variable `should_recover` at crates/pascal-lsp/src/workspace.rs:15787. The binding was only read inside a #[cfg(feature = "test-support")] block. pascal-core and lint4d were already clean.
- With `--all-features` clippy also failed with 8 more lints in test-support-only code: collapsible_if at server.rs:371 and :6658; collapsible_if/collapsible_match at tests/protocol.rs:1829, :11723, :11830, :41104/:41105; manual_is_multiple_of at tests/protocol.rs:41041.
Ruling: also fixed the --all-features lints. CI runs clippy without features, but they are in a crate I touched, the fixes are mechanical let-chain/guard rewrites, and leaving them would keep TASK-30 (run the barrier target in CI) red on clippy.
Ruling: TDD does not apply to test code and clippy-only changes; the RED evidence is the failing clippy output above, GREEN is the clean run. I re-ran every feature-gated test whose body I touched, with test-support on.

Implemented on branch fix/master-tests-clippy (worktree .worktrees/master-tests-clippy), awaiting review and merge.

Merged into master as 7ef2efd (no-ff), 2026-10-03. Merged master tree is identical to the verified integration tree: cargo fmt --check clean, clippy --workspace --all-targets -D warnings clean, cargo test --workspace 3256 passed / 0 failed / 9 ignored.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: TASK-1.1 listed 8 tests and clippy (pascal-core, lint4d, pascal-lsp) failing on master 2b75a51.
Findings on 81418f2 / 1.99.0: all 8 tests already pass. sixty_four_project_metadata_changes_... only compiles with --features test-support. Workspace clippy failed on one pascal-lsp error: an unused `should_recover` binding in a test helper that was only read under cfg(feature = "test-support"). Clippy with --all-features failed on 8 more lints in test-support-only code.
Change (9f3a2f1 fix(lsp): clear clippy failures on the pinned toolchain): moved `should_recover` into the cfg block that reads it (workspace.rs). Collapsed the nested ifs into let-chains/match guards (server.rs x2, tests/protocol.rs x4 sites) and used is_multiple_of(2) (tests/protocol.rs). Behavior unchanged.
Verification: cargo fmt --check clean. `cargo clippy --workspace --all-targets -- -D warnings` and the same with --all-features both clean. The 8 listed tests pass by name (the sixty_four test with test-support). Re-ran the touched test-support tests (6 protocol + 2 lib exhausted/oversized recovery): all pass. A single `CARGO_BUILD_JOBS=3 cargo test --workspace --no-fail-fast` exited 0: 3217 passed, 0 failed, 9 ignored across 82 result lines, which include self-reexec sub-runs. configuration_revalidation_readers_do_not_block_on_fifo_replacement passed in that run.
Known limits: `cargo test --workspace` (and CI) never compiles #[cfg(feature = "test-support")] tests, including the two TASK-1.2 sixty_sixth tests and the protocol_barriers target. Noted on TASK-30, which owns running them in CI.
<!-- SECTION:FINAL_SUMMARY:END -->
