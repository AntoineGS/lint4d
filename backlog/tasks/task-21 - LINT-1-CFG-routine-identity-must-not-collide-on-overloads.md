---
id: TASK-21
title: 'LINT-1: CFG routine identity must not collide on overloads'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:22'
updated_date: '2026-10-04 02:39'
labels:
  - arch-review
  - lint
  - correctness
  - cross-repo
milestone: m-3
dependencies:
  - TASK-3
modified_files:
  - crates/cfg-core/src/summary.rs
  - crates/cfg-core/src/call_graph.rs
  - crates/cfg-core/src/compute.rs
  - crates/lint4d/src/engine/mod.rs
  - crates/lint4d/src/rules/helpers.rs
  - crates/lint4d/src/rules/nil_check.rs
  - crates/lint4d/src/rules/transaction.rs
  - crates/lint4d/tests/rules_use_after_free_test.rs
  - crates/lint4d/tests/rules_transaction_test.rs
  - crates/lint4d/tests/rules_resource_leak_test.rs
  - crates/lint4d/tests/rules_nil_check_test.rs
  - >-
    crates/lint4d/tests/fixtures/use_after_free/bad_double_free_in_first_overload.pas
  - >-
    crates/lint4d/tests/fixtures/use_after_free/bad_double_free_in_overloaded_method.pas
  - >-
    crates/lint4d/tests/fixtures/use_after_free/bad_double_free_in_forward_overload.pas
  - >-
    crates/lint4d/tests/fixtures/transaction/bad_no_commit_in_second_overload.pas
  - crates/lint4d/tests/fixtures/resource_leak/bad_no_try_in_first_overload.pas
  - >-
    crates/lint4d/tests/fixtures/nil_check/bad_overloaded_function_return_nil.pas
  - >-
    crates/lint4d/tests/fixtures/nil_check/good_ambiguous_overloaded_function_return.pas
priority: high
type: bug
ordinal: 21000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-1). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: correctness.

**Problem.** `crates/lint4d/src/engine/mod.rs:126-132` collects CFGs into a
`HashMap<ProcId, _>` where `ProcId` is unit name plus qualified name
(`crates/cfg-core/src/summary.rs:3-14`). Overloaded routines overwrite each
other silently. `crates/lint4d/src/rules/transaction.rs:654-680` then searches the AST by
that same non-unique name with an unqualified fallback.

**Approach.** Add `scope_range: (usize, usize)` (byte range of the routine
body) to `ProcId` in cfg-core, set it in `crates/cfg-pascal/src/pascal_builder.rs:124-138`,
and look routines up by range in transaction analysis. Bump the cfg-core and
cfg-pascal pins.

**Tests first.** That fixture in `crates/lint4d/tests/rules_resource_leak_test.rs`
(fails today: one CFG lost).

**Depends on.** nothing. Three-repo pin bump.

(Citations point at eee0751, the TASK-3 monorepo commit; before TASK-3 the two cfg files lived in ../cfg-core and ../cfg-pascal. Since TASK-3 there are no pins to bump.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A unit with two `procedure Foo` overloads, one leaking a resource, reports the leak on the correct overload.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree /home/a.simard@multidev.local/gits/lint4d-overloads, branch fix/cfg-overload-identity (stacked on chore/monorepo eee0751).

Root cause: ProcId = (unit_name, qualified_name); crates/lint4d/src/engine/mod.rs:126-132 collects CFGs into HashMap<ProcId, Cfg>, so overloads (same proc_name, e.g. two "Foo" or two "TFoo.Bar") overwrite each other: only the last one in source order survives. Transaction analysis (crates/lint4d/src/rules/transaction.rs:654-680) also maps each CFG back to the AST by name, so it always picks the first same-named defProc.

Steps:
1. Tests first (RED at eee0751): lint4d integration fixtures with overloads where the earlier overload has the bug — use-after-free (free procedures and class methods TFoo.Bar(A: Integer)/TFoo.Bar(S: string), with forward-declared overloads), transaction (second overload misses a commit), and the resource-leak fixture from the task (records old behaviour; resource-leak rules are AST-based). cfg-core unit test that ProcIds of same name but different ranges are distinct.
2. cfg-core: add byte_range: Range<usize> to ProcId (constructor takes it) — the routine's defProc range, same value as Cfg::byte_range which cfg-pascal already sets.
3. lint4d engine: build ProcId from cfg.byte_range. transaction.rs: look the defProc up by the CFG's byte range. nil_check.rs: keep name lookup for call resolution (semantically a name lookup), made deterministic across overloads.
4. Run cfg-core, cfg-pascal and lint4d suites; fmt + clippy for touched crates.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision (user, 2026-10-03): wait for the TASK-3 monorepo; then TASK-21 is a single in-repo change with no sibling-repo pushes.

Ruling: the identity field is `byte_range: Range<usize>` rather than `scope_range: (usize, usize)` — it is the same value as `Cfg::byte_range` (the whole defProc), and matching that name/type avoids a second spelling of the same range; summary.rs already imported Range.

Ruling: no change in crates/cfg-pascal — pascal_builder.rs:172-190 (build_proc_cfg) already sets `Cfg::byte_range` to the defProc range; the engine builds `ProcId::new(unit, proc_name, cfg.byte_range.clone())` (crates/lint4d/src/engine/mod.rs:129). `ProcId::new` now takes the range so no caller can build a name-only identity by accident.

Ruling: the "Tests first" resource-leak fixture records old behaviour rather than failing — resource-leak-unprotected/-no-try are AST walkers that ignore `analysis.cfgs` (crates/lint4d/src/rules/resource_leak.rs:61-71, 248-258), so the lost CFG never affected them. The RED evidence comes from the CFG consumers: use-after-free (iterates analysis.cfgs, loses the earlier overload) and transaction (keeps the later CFG but maps it to the first same-named defProc by name).

Ruling: transaction analysis now maps a CFG to its defProc by start byte (`find_def_proc_at`, moved from nil_check.rs to rules/helpers.rs and shared); the name-based extract_defproc_name/find_proc_node_for_cfg were deleted.

Ruling: unchecked-nil's `can_function_return_nil` is a call resolution by name, so it stays a name lookup (compares unit_name/qualified_name, ignores the range). With overloads now all present it considers every same-named candidate and answers only when they all agree (any procedure candidate or any disagreement → None, i.e. "can't be analyzed", which the rule already treats as safe). This is order-independent over the HashMap, so it stays deterministic; it changes one old behaviour: before, the last overload in source order silently decided (recorded by unchecked_nil_treats_call_to_disagreeing_overloads_as_unanalyzable, which flagged at eee0751).

Ruling: CallGraph/compute (cfg-core) key by the full ProcId; nothing in the workspace populates the call graph today (engine passes CallGraph::new()), so a future resolver must resolve a call to a specific definition. pascal-lsp does not use ProcId (grep), only builds against lint4d.

Finding: can_function_return_nil leaves its "can return nil" sentinel in the cache on every None exit, so the second call to an unanalyzable function in a file is flagged. Pre-existing, out of scope: TASK-116.

Finding: the full pascal-lsp run under load had one timeout failure, source_bearing_mixed_roots_and_repeated_includes_deduplicate_physical_results (protocol.rs:1208 "receive LSP message before timeout"); it passed 3/3 in isolation. Not related to this change (no overloads/CFG rules involved).

Implemented on branch fix/cfg-overload-identity (worktree /home/a.simard@multidev.local/gits/lint4d-overloads), awaiting review and merge. It is stacked on chore/monorepo (TASK-3, eee0751) and must merge after it.

Merged into master as 7b69aac (branch fix/cfg-overload-identity, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: ProcId was (unit_name, qualified_name), so overloaded routines (two `procedure Foo`, or `TFoo.Bar(A: Integer)` / `TFoo.Bar(S: string)`) collided in the per-file HashMap<ProcId, Cfg> built by the engine: only the last overload's CFG survived. use-after-free never analyzed the earlier overloads; transaction analysis kept the last CFG but mapped it to the first same-named defProc by name, so the later overload was never analyzed either.

Change (commit d7d64ac on fix/cfg-overload-identity, stacked on chore/monorepo eee0751):
- crates/cfg-core/src/summary.rs: ProcId gains `byte_range: Range<usize>` (the defProc range, same as Cfg::byte_range); ProcId::new takes it. cfg-core tests updated; new call_graph_keeps_overloads_apart.
- crates/lint4d/src/engine/mod.rs: ProcId built from cfg.byte_range.
- crates/lint4d/src/rules/transaction.rs: CFG → defProc by start byte via helpers::find_def_proc_at (moved from nil_check.rs); name-based lookup removed.
- crates/lint4d/src/rules/nil_check.rs: by-name return-nil lookup considers all same-named overloads and answers only when they agree.
- No cfg-pascal change needed (it already sets Cfg::byte_range); no pins exist any more after TASK-3.

Tests (written first; RED at eee0751, GREEN now):
- rules_use_after_free_test: use_after_free_flags_double_free_in_first_of_two_overloads / _in_first_of_two_overloaded_methods / _in_forward_declared_overload — RED: left [] right [14]/[18]/[15]; GREEN.
- rules_transaction_test: transaction_no_commit_flags_the_overload_that_misses_it — RED: got []; GREEN.
- rules_resource_leak_test: resource_leak_no_try_flags_the_leaking_overload — passes at eee0751 too (the rule is AST-based; recorded old behaviour) and now.
- rules_nil_check_test: unchecked_nil_flags_call_when_every_overload_can_return_nil (passes both), unchecked_nil_treats_call_to_disagreeing_overloads_as_unanalyzable — RED: flagged line 21 (last overload decided); GREEN.
Verified: cargo fmt --all --check clean; CARGO_BUILD_JOBS=2 cargo clippy -p cfg-core -p cfg-pascal -p lint4d -p pascal-lsp --all-targets -- -D warnings clean; cargo test -p cfg-core -p cfg-pascal -p lint4d --no-fail-fast: 59 binaries, 607 passed, 0 failed; cargo test -p pascal-lsp --no-fail-fast: 1929 passed, 1 failed (load timeout in protocol.rs source_bearing_mixed_roots_and_repeated_includes_deduplicate_physical_results, passes 3/3 alone).

Known limits: by-name call resolution in unchecked-nil cannot pick an overload (no arity/type matching), so disagreeing overloads are not analyzed. Follow-up TASK-116 (pre-existing nil-check sentinel bug).
<!-- SECTION:FINAL_SUMMARY:END -->
