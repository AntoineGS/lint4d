---
id: TASK-8
title: 'LSP-9: Make the interactive slot actually isolate interactive latency'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - lsp
  - runtime
  - agent-todos
milestone: m-1
dependencies: []
priority: high
type: enhancement
ordinal: 8000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-9).

Severity: high. Cost: runtime latency.

**Problem.** `server.rs:62-65` reserves one of three slots for interactive
work, but an interactive worker can wait indefinitely on a cache
`Computing` claim owned by a bulk job or the warmer
(`project_cache.rs:1335-1339`, `:1838-1847`, `warmer.rs:709-719`). The claim
key excludes the input hash, so a newer document revision waits behind the
older computation. Repository project listing and broad hierarchy/call
operations are classified interactive (`server.rs:953-979`, `:7774-7786`).
`AGENT_TODOS.md` also notes Neovim never cancels superseded requests.

**Approach.**
1. Include the input hash in the claim key; a mismatching claim is not
   waited on.
2. Interactive workers wait on a claim at most N ms (start with 50), then
   compute locally without caching.
3. Reclassify workspace-mode `prepareRename`, project listing, subtypes and
   incoming calls as bulk.
4. Cancel a queued request when a newer request of the same method and
   document arrives (server-side supersession).

**Tests first.** The protocol test above using the `test-support` barrier
harness so it is deterministic, not sleep-based.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A protocol test starts a bulk references request on a large fixture, then sends `textDocument/definition`; definition answers within 200 ms on the test machine.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From `AGENT_TODOS.md` (section "Workspace-Wide Performance"), folded into this task:

Scheduling: long workspace requests can occupy all analysis workers,
including the slot reserved for interactive requests (`prepareRename`
counts as interactive). Neovim never cancels superseded requests, so
repeated keypresses pile up. Consider classifying workspace-mode
`prepareRename` as bulk, cancelling superseded duplicates, and a work
budget per request.
<!-- SECTION:NOTES:END -->
