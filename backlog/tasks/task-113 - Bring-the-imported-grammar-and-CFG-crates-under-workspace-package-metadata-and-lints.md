---
id: TASK-113
title: >-
  Bring the imported grammar and CFG crates under workspace package metadata and
  lints
status: To Do
assignee: []
created_date: '2026-10-03 22:15'
labels:
  - build
  - maintenance
dependencies:
  - TASK-3
priority: low
type: chore
ordinal: 115000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
After TASK-3 (monorepo), crates/tree-sitter-pascal (edition 2018), crates/cfg-core and crates/cfg-pascal (edition 2021) keep their own [package] metadata and do not set `[lints] workspace = true`, so the workspace's `warnings = "deny"` and `clippy::dbg_macro = "deny"` do not apply to them and they compile with older editions than the rest of the workspace (2024).

Do: move cfg-core and cfg-pascal to `*.workspace = true` package fields and `[lints] workspace = true`, migrate their edition (cargo fix --edition), and decide whether the grammar crate (mostly generated C plus a small binding) should follow or stay as upstream ships it to keep `git subtree pull` from Isopod/tree-sitter-pascal conflict-free. Keep fmt, clippy -D warnings and cargo test --workspace green.
<!-- SECTION:DESCRIPTION:END -->
