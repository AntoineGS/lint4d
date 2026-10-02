---
id: TASK-1.1
title: Fix the tests and clippy lints failing on master
status: To Do
assignee: []
created_date: '2026-10-02 23:28'
labels:
  - agent-todos
  - workspace-perf
  - build
dependencies: []
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
