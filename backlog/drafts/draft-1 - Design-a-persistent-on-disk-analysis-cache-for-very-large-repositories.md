---
id: DRAFT-1
title: Design a persistent on-disk analysis cache for very large repositories
status: Draft
assignee: []
created_date: '2026-10-03 22:04'
labels:
  - lsp
  - workspace-perf
  - design
dependencies: []
references:
  - TASK-83
  - TASK-33
  - TASK-6
priority: medium
type: spike
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Raised by the user while deciding TASK-83 (2026-10-03). On multidev (24,050 .pas files, 230+ projects) workspace-wide requests cannot complete within a session. Idea: persist analysis results (symbols, occurrences, reverse dependencies, project contexts) on disk, as files or SQLite, keyed by content and project-context fingerprints, so the server can serve complete or near-complete workspace-wide results across restarts.

Not designed yet. Needs a brainstorming session with the user first: storage format (files vs SQLite), invalidation (content hashes, mtimes, project/config fingerprints, grammar/analyzer version), what to persist, interaction with TASK-33 (LSP-16 versioned workspace index) and TASK-6 (partial results), concurrency between several server instances, and disk budget.
<!-- SECTION:DESCRIPTION:END -->
