---
id: TASK-129
title: >-
  LSP supersession: defer version-based cancels until admission and count
  replaced requests as freed capacity
status: To Do
assignee: []
created_date: '2026-10-03 23:46'
labels:
  - lsp
  - runtime
dependencies:
  - TASK-8
priority: low
type: bug
ordinal: 131000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the TASK-8 fix round review (branch perf/lsp-protocol-commits, cd102f3). Two leftovers in request supersession (crates/pascal-lsp/src/server.rs, request admission around the `replaced` / `is_superseded_by` logic):
1. The version-based supersession loop (`is_superseded_by`, a request for a newer document version) still cancels the older request before the newer one is admitted. If admission then rejects the newer request (queue full, recipient or partial-token limits), the client loses both answers. Pre-existing on master; cd102f3 fixed only the same-kind replacement path.
2. Since cd102f3 the replacement cancel runs after admission, so at saturation a replacement hover/prepareRename is rejected even though cancelling the replaced request would free exactly the capacity it needs. Count replaced requests as freed capacity in the bound checks.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A newer-version request rejected at admission does not cancel the older request
- [ ] #2 A replacement request at full capacity is admitted when the request it replaces frees the needed slot; tests cover both
<!-- AC:END -->
