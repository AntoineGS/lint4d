---
id: TASK-126
title: >-
  LSP: reconcile file batches through a ChangeSet and dispatch new requests
  during reconciliation
status: To Do
assignee: []
created_date: '2026-10-03 23:37'
labels:
  - lsp
  - runtime
  - workspace-perf
milestone: m-2
dependencies:
  - TASK-15
priority: medium
type: enhancement
ordinal: 128000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Deferred remainder of TASK-15 (LSP-10), branch perf/lsp-protocol-commits (012f7a0). TASK-15 now delivers completed read-only results whose generations match the pre-batch revision while a watcher/notification batch reconciles. Two parts of its Approach were not done:
1. A pure ChangeSet reconciler: compute the batch's effect without holding the whole workspace, then apply it in a short commit.
2. New requests that arrive during reconciliation are still queued (server.rs, around the reconcile worker loop) instead of being dispatched against the pre-batch revision or deferred individually.

See the TASK-15 notes ('Known limits') and the review in .superpowers/sdd/day-2026-10-03 (LSP-S).
<!-- SECTION:DESCRIPTION:END -->
