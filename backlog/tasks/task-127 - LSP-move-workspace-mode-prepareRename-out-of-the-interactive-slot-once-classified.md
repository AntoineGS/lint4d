---
id: TASK-127
title: >-
  LSP: move workspace-mode prepareRename out of the interactive slot once
  classified
status: To Do
assignee: []
created_date: '2026-10-03 23:37'
labels:
  - lsp
  - runtime
milestone: m-1
dependencies:
  - TASK-8
priority: medium
type: enhancement
ordinal: 129000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Deferred remainder of TASK-8 (LSP-9), branch perf/lsp-protocol-commits (7488fba). TASK-8's Approach item 3 asked to reclassify workspace-mode prepareRename. It stays Interactive (server.rs, request classification around line 972) because the workspace/local mode is only known inside the worker. A running workspace-mode prepareRename can therefore occupy the reserved interactive slot for a whole workspace scan: supersession only cancels queued requests, and the claim-wait limit doesn't shorten a scan already in progress. This was a user-visible pain point (AGENT_TODOS Neovim note).

Approach: once the worker classifies a prepareRename as workspace mode, hand it to the bulk lane (or cancel cooperatively and re-admit it as bulk).
<!-- SECTION:DESCRIPTION:END -->
