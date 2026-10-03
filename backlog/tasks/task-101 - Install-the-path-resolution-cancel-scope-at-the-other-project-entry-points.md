---
id: TASK-101
title: Install the path-resolution cancel scope at the other project entry points
status: To Do
assignee: []
created_date: '2026-10-03 03:12'
labels:
  - core
  - runtime
dependencies:
  - TASK-84
priority: low
ordinal: 103000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-84 added `with_path_resolution_cancel` (crates/pascal-project/src/lib.rs) and installs it only in `discover_context_with_selections`. Other public entry points that call `resolve_existing_path_status` indirectly (e.g. `project_candidates_with_work_budget*`, `ReadPolicy::new_with_installation_roots` callers, installation/IDE path resolution) do not, so a cancelled request there can still walk large directories. Wrap them (or move to an explicit token via TASK-43's DirectoryProvider).
<!-- SECTION:DESCRIPTION:END -->
