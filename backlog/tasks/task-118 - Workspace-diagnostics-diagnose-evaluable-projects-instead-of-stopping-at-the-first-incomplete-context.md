---
id: TASK-118
title: >-
  Workspace diagnostics: diagnose evaluable projects instead of stopping at the
  first incomplete context
status: To Do
assignee: []
created_date: '2026-10-03 22:48'
labels:
  - lsp
  - workspace-perf
dependencies:
  - TASK-6
priority: medium
type: enhancement
ordinal: 120000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
After TASK-6, workspace/diagnostic answers with the proven subset plus a window/logMessage instead of failing when the snapshot is incomplete. But build_rejectable_workspace_snapshot (crates/pascal-lsp/src/workspace/rename.rs, stop_at_incomplete_context in build_snapshot / discover_enumerated_contexts) still stops discovery at the first ambiguous or incomplete project context and returns an empty snapshot, so on a repo with one unevaluable project (multidev) the report is empty: units discovered before and after the bad project are never diagnosed.

Repro: protocol test workspace_diagnostics_answer_and_log_when_a_project_cannot_be_evaluated (Ambiguous/ with two .dproj, Clean/Clean.pas): the response has no item for Clean.pas.

Under the partial policy (TASK-83 decision) discovery should continue past an incomplete context, diagnose units whose context is complete, skip units listed in RenameSnapshot.incomplete_context_sources (already filtered in workspace_diagnostics_from_input) and record them as gaps. Watch runtime on very large repos: the early stop was added to bound the cost of a request that used to fail anyway (see workspace.rs test workspace_diagnostics_stop_discovery_at_the_first_incomplete_context).
<!-- SECTION:DESCRIPTION:END -->
