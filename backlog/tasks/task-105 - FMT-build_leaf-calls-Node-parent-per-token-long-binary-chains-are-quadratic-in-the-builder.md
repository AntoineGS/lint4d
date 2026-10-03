---
id: TASK-105
title: >-
  FMT: build_leaf calls Node::parent() per token; long binary chains are
  quadratic in the builder
status: To Do
assignee: []
created_date: '2026-10-03 03:21'
labels:
  - fmt
  - runtime
  - arch-review
dependencies:
  - TASK-62
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
