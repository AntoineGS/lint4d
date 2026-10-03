---
id: TASK-97
title: >-
  Replace the permanent fence for unattributable file notifications with cache
  invalidation
status: To Do
assignee: []
created_date: '2026-10-03 02:21'
labels:
  - lsp
  - runtime
milestone: m-1
dependencies: []
priority: medium
type: bug
ordinal: 99000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-up to TASK-5 (LSP-3), which stopped refused or interrupted notification recovery from fencing the workspace permanently.

**Problem.** `permanently_fence_file_notification_analysis` in `crates/pascal-lsp/src/server.rs` (around line 13566) still latches `rejected_open_fence_permanent` before invalidating. It is called 4 times from `invalidate_malformed_file_notification` and 14 times from `handle_notification_with_control_inner`, when a file notification's endpoint evidence was not inspected or does not fit the recovery envelope. After that, every request fails with -32803 "analysis is disabled" until restart.

**Question to settle first.** Is the unattributable evidence reconstructible? The derived caches are, and TASK-5's `discard_reconstructible_state` already discards them. Open questions:
- Which of the 18 call sites lose authoritative state: deletions that cannot be stamped, or rename transitions whose endpoints are unknown?
- Can rejecting every open overlay at rename endpoints, plus a whole-workspace discard, stand in for the fence?

**Tests first.** A protocol test that sends a malformed or over-envelope didChangeWatchedFiles or didRenameFiles notification and checks that the next textDocument/definition answers (or record why the fence must stay).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each permanently_fence_file_notification_analysis call site is classified as reconstructible or authoritative, with the reasoning recorded in the task
- [ ] #2 Reconstructible call sites discard derived state without the permanent fence, and a protocol test shows textDocument/definition answering after such a notification; the test failed on the code before the change
- [ ] #3 Call sites that keep the fence have a test pinning that behaviour and a comment naming the authoritative state at risk
<!-- AC:END -->
