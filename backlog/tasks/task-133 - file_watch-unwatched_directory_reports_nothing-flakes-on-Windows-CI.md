---
id: TASK-133
title: file_watch unwatched_directory_reports_nothing flakes on Windows CI
status: To Do
assignee: []
created_date: '2026-10-04 00:53'
updated_date: '2026-10-04 01:25'
labels:
  - lsp
  - test
dependencies: []
priority: low
ordinal: 135000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen once on PR #7 (CI run 37164776274, Test (windows-latest), commit 87119ea): pascal-lsp file_watch::tests::unwatched_directory_reports_nothing failed with "assertion failed: events.receiver.recv_timeout(Duration::from_millis(500)).is_err()" (crates/pascal-lsp/src/file_watch.rs:217). The next two runs on the same code passed. The test watches a temp directory, unwatches it and writes New.pas, expecting no event within 500 ms; on Windows an event queued before unwatch (or ReadDirectoryChangesW delivering late) can still arrive. file_watch.rs was not changed on that branch.

Repro: rerun the Windows test job; the failure is intermittent.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cause is identified (event queued before unwatch, late delivery, or an unwatch race in NotifyWatcher) and recorded
- [ ] #2 unwatched_directory_reports_nothing passes 20 consecutive Windows CI runs (or a looped run of the test on Windows) without weakening what it asserts
- [ ] #3 If the backend can deliver events after unwatch, the watcher drops events for unwatched roots and a test covers it
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Second sighting: PR #7 CI run 37167338491 attempt 1 (commit e443d6a), same assertion at file_watch.rs:217; the failed-job rerun (attempt 2) passed. 2 failures in 6 Windows runs on that branch; file_watch.rs unchanged there.
<!-- SECTION:NOTES:END -->
