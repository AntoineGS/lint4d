---
id: TASK-105
title: >-
  FMT: build_leaf calls Node::parent() per token; long binary chains are
  quadratic in the builder
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 03:21'
updated_date: '2026-10-04 02:39'
labels:
  - fmt
  - runtime
  - arch-review
dependencies:
  - TASK-62
modified_files:
  - crates/fmt4d/src/doc_builder.rs
  - crates/fmt4d/tests/fmt_perf_test.rs
priority: medium
ordinal: 107000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while working TASK-62. Formatting a unit containing one `X := V0 + V1 + ... + V9999;` statement takes ~4.0s (release; 10k operands), ~18s at 20k, ~120s at 40k: quadratic. Phase timing shows parse 17ms, comment/directive maps 10ms, **DocBuilder::build 4.0s**, render 2ms, normalize <1ms.

Root cause (gdb samples): `DocBuilder::build_leaf` (crates/fmt4d/src/doc_builder.rs, `node.parent()`; same in `build_verbatim_leaf`) calls tree-sitter `Node::parent()`, which is O(depth) (`ts_node_parent` -> `ts_node_child_with_descendant`) and a left-nested binary chain has depth ~ operand count. Called once per leaf from `build_binary_chain_doc` (doc_builder_expressions.rs).

Repro: generate unit with a 20,000-operand `+` chain, `fmt4d --stdin`; or a #[test] with 20,000 operands in a worker thread with a 512 MB stack (debug builds overflow the default test stack). Takes ~18-27s today.

Approach: pass the parent kind down from the caller (flatten_binary_chain already knows the parent of each operand), or cache the parent kind while walking the left spine, instead of calling parent() per leaf. Other per-node parent() / prev_sibling() calls in comments.rs may share the shape; check before fixing. Note: comments.rs and directive_map.rs were under concurrent edit when this was filed.

TASK-62's acceptance #1 (5,000-element chain) is met at the renderer level only; this task is what makes the end-to-end chain linear.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/fmt-chain-perf, branch fix/fmt-chain-perf (base d55c0f6).
Root cause: build_leaf/build_verbatim_leaf call Node::parent() (O(depth)) per token; left-nested binary chains have depth ~ operand count.
Steps: 1) snapshot formatter output over all fixtures (default + aligned) 2) end-to-end chain timing test in fmt_perf_test.rs, RED on master 3) pass/derive parent kind instead of parent() per leaf 4) diff snapshots, tests, clippy, fmt 5) check TASK-62 AC#1, set Done.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Ruling: parent kinds are indexed once per build (one TreeCursor walk in DocBuilder::build, HashMap id->kind) rather than threaded through doc_for_node — touches no call sites, so no conflict with the concurrent uses/var-list edits; parent_kind() falls back to Node::parent() when the index has no entry. No other parent()/prev_sibling() calls exist in fmt4d/src (comments.rs included).

Ruling: timing test uses 20,000 operands (not 5,000): at 5,000 master takes only 1.2 s debug, too close to a safe bound; 20,000 is a superset. RED on master: 19.3 s (bound 10 s). GREEN: 0.24 s. Snapshot of all 144 .pas files in the repo, default and aligned (288 outputs), byte-identical before/after. Snapshot harness not committed: it needs 288 stored goldens; the existing fixture roundtrip tests cover idempotence.

Implemented on branch fix/fmt-chain-perf (worktree .worktrees/fmt-chain-perf), awaiting review and merge.

Merged into master as 559838c (branch fix/fmt-chain-perf, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: DocBuilder::build_leaf/build_verbatim_leaf (and PP fragment tokens) called Node::parent() per token, O(depth); a left-nested binary chain makes the builder quadratic (20,000 operands: 19.3 s).
Change: DocBuilder::build indexes every node's parent kind in one cursor walk; parent_kind() looks it up (fallback to parent()). Commit eefb9e9 perf(fmt4d): index parent kinds once instead of Node::parent() per leaf.
Tests: fmt_perf_test::long_binary_chain_formats_in_linear_time (20,000 operands, 512 MB stack thread, bound 10 s): RED 19.3 s, GREEN 0.24 s. Output byte-identical on 144 files, default+aligned. cargo test -p fmt4d all green; fmt and clippy -D warnings clean.
Limits: memory for a HashMap of all nodes per build; snapshot harness not committed.
<!-- SECTION:FINAL_SUMMARY:END -->
