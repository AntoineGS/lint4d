---
id: TASK-9
title: 'LSP-20: Fix-all naming validates one batch, not growing prefixes'
status: Deferred
assignee:
  - '@claude'
created_date: '2026-10-02 23:22'
updated_date: '2026-10-04 02:39'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-1
dependencies: []
modified_files:
  - crates/pascal-lsp/src/workspace/codeactions.rs
priority: high
type: enhancement
ordinal: 9000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-20). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime.

**Problem.** `workspace/codeactions.rs:1713-1733`, `:1749-1796`: for every
proven candidate, fix-all pushes it onto `retained` and calls
`validate_fix_all_subset` again, which normalizes cumulative edits,
rebuilds the transformed source, builds offset maps and rebinds the index.
Then `:2023-2088`, `:2138-2164` parse, lint and search the final source
again. At least quadratic in candidate count.

**Approach.** Build a conflict graph over candidates keyed by binding id
(two candidates conflict when their new spellings collide in a scope);
select a maximal non-conflicting set; validate once.

**Tests first.** Golden test capturing current output on existing fixtures;
call-count test.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Fix-all with 500 naming candidates performs one `validate_fix_all_subset` (count via test hook); the resulting edit equals the current implementation's edit on the existing codeaction fixtures.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/lsp-partial-results, branch feat/lsp-partial-results, stacked on the TASK-6 commit 2831402.

Root cause: fix_all_plan_from_input (crates/pascal-lsp/src/workspace/codeactions.rs) selects candidates greedily: for each proven candidate it pushes onto retained and calls validate_fix_all_subset on the whole retained prefix (normalize cumulative edits, rebuild transformed source, offset map, rebind index, per-edit navigation). N candidates => N validations of growing size: quadratic.

Plan: validate the whole proven set once. If it passes, keep it (one validation). Otherwise split the set in halves and recurse (each half validated together with what is already retained); a single failing candidate is dropped. Accepted chunks are appended in candidate order, and the last successful validation always covers the final retained set, so the plan's edits are still one combined proof. Cost: 1 validation when all candidates are compatible; O(k log n) when k candidates fail, instead of n.

Ruling to record: validation is not monotone (e.g. a->b plus b->c passes together while a->b alone fails), so batching can keep a sound superset of what the old greedy kept in such cases; the AC only requires equal edits on the existing fixtures.

Tests first: (1) golden test in the codeactions test module capturing the current fix-all output (applied source) on the existing conflict/independent fixtures; must pass before and after. (2) call-count test via a #[cfg(test)] thread-local counter in validate_fix_all_subset: 500 constant-naming candidates => exactly one validation; fails on the original code.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Ruling: selection algorithm — validate the whole proven set once; when a batch fails, split it in halves and retry each half with what is already retained; a failing single candidate is dropped. Batches are consumed in candidate order and every trial includes the retained set, so the last successful validation proves the final edit (same guarantee as before). Compatible candidates cost one validation; k incompatible ones cost O(k log n). Validation is not monotone (a->b with b->c can pass together while a->b alone fails), so in such cases batching keeps a sound superset of what the old greedy kept; on the existing fixtures the output is identical (golden test). The conflict-graph pre-filter suggested in the task was not built: keying conflicts by new spelling without scope would drop valid same-name fixes in independent routines (see protocol test source_fix_all_withholds_combined_binding_conflicts_in_eager_and_deferred_paths), and the bisection already gives one validation for compatible sets.

Finding: the 500-candidate AC cannot be met by this change alone. With 500 constants the per-candidate rename proofs (rename_edits_with_cancel_and_work_budget per candidate, each walking every identifier) exhaust MAX_FIX_ALL_WORK before the first validation, on the old and new code alike. The call-count test therefore uses 150 candidates, the largest that fits the proof phase. Follow-up: TASK-120.

TDD: RED (prefix validation restored, new tests in place): cargo test -p pascal-lsp --lib -- fix_all_validates_compatible fix_all_output_matches → fix_all_validates_compatible_candidates_once failed "left: 56 right: 1" (budget ran out after 56 growing validations, no action); fix_all_output_matches_the_prefix_validation_on_existing_fixtures passed (golden capture). GREEN after the change: both pass.

Implemented on branch feat/lsp-partial-results (worktree .worktrees/lsp-partial-results), awaiting review and merge.

Review note (controller): bisection can cost up to 2n-1 validations when nearly every candidate fails (old greedy: n). Near MAX_FIX_ALL_WORK this could exhaust the budget where the old loop did not; consider a linear fallback for small failing batches in TASK-120.

Merged into master as 4553ad3 (branch feat/lsp-partial-results, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.

Deferred: code merged; AC #2 met. AC #1 (500 candidates -> one validate_fix_all_subset) is not met because the per-candidate rename proofs that run before validation are themselves quadratic and exhaust the fix-all budget on old and new code alike. Continues in TASK-120; set Done when TASK-120 lands and AC #1 is checked there.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: fix-all naming pushed each proven candidate onto the retained set and re-ran validate_fix_all_subset (normalize, rebuild source, offset map, rebind, per-edit navigation) on the growing prefix: quadratic in candidates. With 150 constants the fix-all work budget ran out after 56 validations and no action was offered.

Change (commit 2f21b69, stacked on TASK-6's 2831402, crates/pascal-lsp/src/workspace/codeactions.rs): the proven set is validated as one batch; a failing batch is bisected, each half validated together with what is already retained, and a failing singleton dropped. Compatible candidates take one validation. A #[cfg(test)] thread-local counter in validate_fix_all_subset counts validations.

Tests: fix_all_output_matches_the_prefix_validation_on_existing_fixtures (golden applied-source output for the same-scope conflict, nested capture, independent routines and a mixed fixture; passed on the old code and after); fix_all_validates_compatible_candidates_once (150 constants → exactly 1 validation and the full action; old code: 56 validations, no action). cargo test -p pascal-lsp all green (lib 696, protocol 721, navigation 317, rename 94, ...); --features test-support protocol and protocol_barriers 905/907 each, only the TASK-1.2 timing tests failing; fmt and clippy -D warnings clean.

Known limits: AC #1 (500 candidates) is not met: the per-candidate rename proofs before validation are themselves quadratic and exhaust MAX_FIX_ALL_WORK at about 300+ candidates (follow-up task created). Batching can keep a sound superset of the old greedy selection when validation is non-monotone; identical on existing fixtures.
<!-- SECTION:FINAL_SUMMARY:END -->
