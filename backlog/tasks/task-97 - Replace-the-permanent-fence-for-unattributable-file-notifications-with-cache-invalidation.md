---
id: TASK-97
title: >-
  Replace the permanent fence for unattributable file notifications with cache
  invalidation
status: Done
assignee:
  - '@claude'
created_date: '2026-10-03 02:21'
updated_date: '2026-10-04 02:39'
labels:
  - lsp
  - runtime
milestone: m-1
dependencies: []
modified_files:
  - crates/pascal-lsp/src/server.rs
  - crates/pascal-lsp/src/workspace.rs
  - crates/pascal-lsp/tests/protocol.rs
  - crates/pascal-lsp/README.md
  - crates/pascal-project/src/delphi_overrides.rs
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
- [x] #1 Each permanently_fence_file_notification_analysis call site is classified as reconstructible or authoritative, with the reasoning recorded in the task
- [x] #2 Reconstructible call sites discard derived state without the permanent fence, and a protocol test shows textDocument/definition answering after such a notification; the test failed on the code before the change
- [x] #3 Call sites that keep the fence have a test pinning that behaviour and a comment naming the authoritative state at risk
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Phase 2 (user decisions Q1-Q5 recorded in notes).
Worktree .worktrees/notification-fence, branch fix/notification-fence (from master d55c0f6).
Root cause: permanently_fence_file_notification_analysis latches rejected_open_fence_permanent for notifications whose evidence the server did not read or could not parse; under the LSP-spec ordering model (Q1) all of that state is reconstructible from disk + client overlays.
Steps:
1. RED protocol tests per trigger class (each fails on d55c0f6 with -32803): >64 watched events (git checkout) incl. a Delete; watched without changes / unparseable member; create/delete without files / >64 / unparseable; empty create/delete no-op; rename without files / >64 pairs / unparseable endpoint (overlay whose file is gone is rejected, others kept); override TOML re-read on discard; >256 Deletes do not fence.
2. Replace the fence with a recovery path: parse all well-formed members, record kinds/rename endpoints, force whole discard (discard_reconstructible_state), refresh captured override TOML on discard, reject open overlays absent on disk for unknown rename endpoints; empty create/delete/rename -> no-op; remember_deleted overflow -> discard instead of fence.
3. Delete permanently_fence_file_notification_analysis, invalidate_malformed_file_notification and any now-dead latch path (if nothing else uses it).
4. Flip existing tests encoding the fence / "Delete before unlink" model with recorded reasons; rewrite README fence section.
5. fmt, clippy -D warnings, pascal-lsp suites.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision (user, 2026-10-03): classify first, then stop. An agent classifies all call sites (AC #1) with reasoning recorded in the task, and stops for the user's review before changing any behaviour.

2026-10-03 classification, part 1 of 3 (AC #1 work, fix/notification-fence @ d55c0f6, read-only; full report .superpowers/sdd/day-2026-10-03/F97-TASK-97-report.md).

Facts:
- Count confirmed: 4 + 14 = 18 sites; function at server.rs:13575.
- invalidate_malformed_file_notification has one caller (server.rs:14111, empty create/delete `files: []`) which passes [] → sites :13539/:13549/:13558 are dead; :13530 is reachable only on try_reserve(128) failure (OOM). Vestige of 59e6ed8.
- Every fence site returns before record_file_event/record_rename_endpoint, and each worker has a fresh budget (server.rs:10600). So the fence records no tombstones/endpoints: it is just latch + invalidate_for_reconciliation_budget(empty budget). Dropping only the latch == invalidate_ambiguous_file_notification(.., [], ..).
- Watched/create/delete events never touch open overlays (workspace.rs:4425-4430); only rename does (workspace.rs:5311-5373).
- Not rebuildable from disk + client overlays:
  (a) a Deleted reported before unlink (README:1572/1580 and several tests model this);
  (b) identity of an open overlay at an unknown rename old-URI;
  (c) captured override TOML (OverrideSession is refreshed only by explicit events; discard_reconstructible_state does not re-capture it, but it could).
  (a) only exists under a stricter-than-LSP ordering model (spec: did* notifications are sent after the operation).
- Over-envelope batches still carry all their evidence (frame ≤ 8 MiB, server.rs:62); the 64-entry/32 KiB caps only bound per-entry reconciliation work.

Classes: R = reconstructible under any ordering model; R* = reconstructible if did* follow the disk change (LSP spec), authoritative under the current "delete may precede unlink" model; dead = unreachable.

Classification, part 2 of 3 (table; lines are server.rs @ d55c0f6)

| # | Site | Trigger | Class (conf.) | Reasoning | Proposed behaviour |
|---|---|---|---|---|---|
| M1 | :13530 | try_reserve(128) fails | dead/OOM (high) | no evidence involved | delete helper; empty create/delete → invalidate_ambiguous(..,[],..) or no-op |
| M2 | :13539 | endpoint URI > 4096 B | dead (high) | caller passes [] | delete |
| M3 | :13549 | endpoint byte sum overflow | dead (high) | caller passes [] | delete |
| M4 | :13558 | > 128 endpoints / > 512 KiB | dead (high) | caller passes [] | delete |
| W1 | :13993 | watched: no changes array | R* (high under spec) | unknown Deletes (a) + override TOML (c); no overlay at risk | spec: discard + re-capture overrides; strict: keep fence |
| W2 | :14006 | > 64 watched events | R (high) | all events are in the frame: tombstone Deleted (stamped), clear on Created/Changed, refresh override TOML. VERY realistic (git checkout/pull/stash, repo-wide fmt4d, probably folder moves; nvim _watchfiles and vscode-languageclient batch); today kills the session | parse all, record kinds, whole discard, no fence (> 256 Deletes still fence via remember_deleted) |
| W3 | :14050 | member with bad URI / non-file URI / unknown type | split: non-file R (high); known URI + unknown type R (high, tombstone conservatively); unparseable URI R* | non-file URIs never become workspace state | drop non-file; unknown type → Deleted; unparseable → as W1 |
| W4 | :14060 | ≤ 64 events, > 32 KiB URI bytes | R (high) | parsed_changes already complete | as W2 |
| F1 | :14104 | create/delete: no files array | create R (high); delete R* | no overlay at risk; Delete loses (a) | create: discard; delete: as W1 |
| F2 | :14122 | > 64 file-operation entries | R (high) | evidence in frame; medium realism (multi-select explorer ops) | parse all, record, whole discard, no fence |
| F3 | :14162 | member with missing/bad/non-file URI | create + non-file R (high); unparseable delete R* | kind is known | create: discard; non-file: drop; unparseable delete: as W1 |
| F4 | :14172 | ≤ 64 entries, > 32 KiB | R (high) | uris complete | as F2 |
| R1 | :14213 | rename: no files array | R* + overlay policy (medium) | (b) ghost overlay at an unknown old URI; (a) under the strict model | spec: discard + overlay policy (Q2); strict: keep fence |
| R2 | :14220 | files: [] | R (high) | no rename happened | no-op (like empty watched :13999) |
| R3 | :14230 | > 64 rename pairs | R (high) | evidence in frame; tombstone olds, Created news, reject overlays at all endpoints; pending plans already dropped by discard | parse all, record, whole discard, no fence. Don't reuse invalidate_ambiguous as-is: it tombstones new endpoints too and hides existing destinations |
| R4 | :14299 | endpoint URI > 4096 B | R (medium-high) | such a URI can never be an open overlay (open_document refuses it, workspace.rs:3833); preflight already refuses → discard | record like R3 |
| R5 | :14309 | missing/bad/non-file endpoint, or old == new | old == new + non-file R (high); unparseable R* | old == new is not a transition | skip old == new; non-file side → record the file side as Deleted/Created; unparseable → as R1 |
| R6 | :14333 | ≤ 64 pairs, > 32 KiB | R (high) | renames complete | as R3 |

Classification, part 3 of 3
Summary: no site is AUTHORITATIVE under LSP-spec ordering. Under the current strict model, only the unparseable-member cases of W1/W3/F1/F3 (Deletes) and R1/R5 (rename endpoints) are authoritative.

Recommendation:
- Phase 2: remove the fence from W2, W4, F2, F4, R2, R3, R4, R6 and from the safe sub-cases of W3/F3/R5; delete the dead helper. Add the step that refreshes captured override paths on discard.
- Keep the fence (AC #3, with a comment naming the state at risk) for unparseable Delete/rename members unless the user adopts the spec ordering model (Q1).
- Test plan and the list of existing tests whose expectations flip: see report. README:1533-1585 must be rewritten.

Ruling: no characterization tests added. 18+ existing protocol tests already pin every site; the 55 matching tests pass at d55c0f6 (cargo test -p pascal-lsp --features test-support --test protocol -- fence malformed oversized unattributable uri_byte_overflow sixty_four_entry sixty_four_short ambiguous_rename).
Ruling: introduced the R* class because the answer for unparseable members depends on an ordering assumption that README and tests make explicitly; that is a user decision.

Classification ready for user review.

Decisions (user, 2026-10-03) on the classification's open questions:
- Q1: follow the LSP spec. did*Files notifications and watcher Deleted events describe changes already on disk. Under that model all 18 call sites are reconstructible; update README and the tests that encode the old 'Delete may precede unlink' model.
- Q2: for unknown rename endpoints, reject only open overlays whose file no longer exists on disk (one stat per open document; never-saved buffers are rejected too and are re-sent by the client).
- Q3: yes, re-read the captured override TOML when derived state is discarded.
- Q4: yes, an empty didCreateFiles/didDeleteFiles is a no-op.
- Q5: yes, include it in this task: a batch of more than 256 Deletes must not fence via remember_deleted.

Final classification (phase 2, after the user's Q1 = LSP-spec ordering; supersedes the R/R* split above). All 18 permanently_fence_file_notification_analysis call sites are RECONSTRUCTIBLE. Disk plus the client's open documents rebuild every piece of state they touched. None keeps a fence, so AC #3 has nothing to pin. The fence helper and invalidate_malformed_file_notification are deleted (commit 473d9cf). Disposition by site:
- M1-M4 (invalidate_malformed_file_notification): dead/OOM-only, deleted. Its only caller (empty create/delete) is now a no-op (Q4).
- W1 (no changes array): whole discard (recover_unreconciled_file_notification with no events). The override TOML is re-read on discard (Q3). Watched events never touch overlays, so none are rejected.
- W2 (>64 events), W3 (bad member), W4 (>32 KiB): every parseable member is recorded (Deleted → stamped tombstone, max 256; Created/Changed clear it), then a whole discard. Bad members are skipped; disk decides.
- F1 (no files array): whole discard. F2/F3/F4: record the parsed URIs with the method's kind, then a whole discard.
- R1 (no files array): whole discard with unknown endpoints. Open documents whose file no longer exists on disk are rejected (Q2).
- R2 (files: []): no-op.
- R3 (>64 pairs), R4 (endpoint >4 KiB), R6 (>32 KiB): record old→Deleted and new→Created, reject overlays at all known endpoints, then a whole discard.
- R5 (malformed pair): known file sides are recorded as above, then the unknown-endpoint overlay rule (Q2). A pair with old == new is skipped as not a transition.
- Q5: remember_deleted no longer latches the fence past 256 tombstones; it invalidates the project-cache path and leaves the deletion to disk.
- Whole discard = project_cache.invalidate_after_overflow() + discard_reconstructible_state (TASK-5), which now also marks the captured override files dirty (OverrideSession::mark_all_dirty).

Ruling: a malformed watched member with an unknown change type is skipped rather than tombstoned as Deleted (the classification proposed Deleted). Why: under spec ordering a tombstone stamped on a file that still exists would hide it. The whole discard re-reads disk anyway.

Ruling: a non-file or unparseable member still triggers the whole discard rather than being dropped from an otherwise in-envelope batch. Why: one uniform path, and a discard is cheap and safe. Only the old == new rename pair is skipped outright.

Ruling: over-envelope batches go straight to the forced discard (discard_reconstructible_state); bounded per-entry recovery is not attempted first. Why: when evidence is missing (W1/R1), only a whole discard is sound, and one path for all 18 sites keeps it simple.

Ruling: Q3 is implemented lazily. mark_all_dirty makes the next effective_for/configuration_for re-read each captured file, so the discard does no override IO. captured_sources_match is unchanged (a version that skipped dirty paths was tried, but no test needed it, so it was dropped).

Ruling: in-envelope tests that report a Delete before the unlink (e.g. the project-descriptor test at protocol.rs ~30088 and the workspace unit test 'without the fence, the tombstone must keep...') are left as is. They still pass because stamped tombstones remain as a defensive mechanism, and only tests that asserted the fence encode the old model in a way that now fails. The 23 flipped protocol tests and the tombstone-capacity unit test unlink before notifying, with a TASK-97 comment giving the reason.

Follow-up created: TASK-132 (ambiguous rename recovery tombstones existing destinations; pre-existing, not a fence). Noted on TASK-1.2: sixty_four_project_metadata_changes_share_the_notification_budget_and_stale_pull_results is load-flaky on d55c0f6 too.

Implemented on branch fix/notification-fence (worktree .worktrees/notification-fence), awaiting review and merge.

Review minors to pick up when convenient (controller): add a test for a rename pair with old == new (now skipped silently instead of fencing), in-envelope and inside a malformed batch; add an untitled:/deleted-before-save overlay to the R1 test (Q2 never-saved buffers); README ~1573 should state the real recovery path for rejected documents (didClose/didOpen or a full-text didChange; incremental-sync clients don't re-send on their own); refresh stale comments (workspace.rs ~4512 discard doc, delphi_overrides.rs ~341, workspace.rs ~16299 test message, server.rs ~14199 log text); strengthen more_than_256_deletes_across_in_envelope_batches_do_not_fence to check a unit deleted past the tombstone capacity stops resolving.

Merged into master as aebf633 (branch fix/notification-fence, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Comments

<!-- COMMENTS:BEGIN -->
created: 2026-10-03 22:19
---
Open questions for the user before phase 2 (classification in the notes / report F97-TASK-97-report.md):
Q1 Ordering model: treat did*-notifications and watcher Deletes as AFTER the filesystem change (LSP spec)? Or keep the current "Delete may precede unlink" model encoded in README:1572-1585 and tests? This decides whether unparseable Delete/rename members (W1, F1-delete, R1, and the unparseable sub-cases of W3/F3/R5) can drop the fence.
Q2 (spec model only) Overlay policy for unknown rename endpoints:
  (a) reject every open overlay;
  (b) reject overlays whose file is now absent on disk;
  (c) reject none and rely on the client's didClose/didOpen (VS Code does that around a move; nvim depends on the plugin, not verified).
Q3 OK to re-read the captured override TOML (OverrideSession) during a discard? Today it is refreshed only by explicit events.
Q4 Should empty didCreate/DeleteFiles become a no-op (like empty watched and the proposed empty rename) instead of a global discard?
Q5 A batch with > 256 Deletes (branch switch) still fences via remember_deleted (TASK-5 rule). Accept for now, or open a follow-up?
Recommendation: approve phase 2 for the 8 over-envelope / trivially-safe sites (W2 is the critical one: git checkout or a repo-wide fmt4d run currently disables analysis until restart), and keep the fence with an AC #3 pin for unparseable members unless Q1 = spec.
---
<!-- COMMENTS:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: for a malformed or over-envelope file notification, permanently_fence_file_notification_analysis latched rejected_open_fence_permanent at 18 call sites. Every later request then failed with -32803 until restart. A git checkout or repo-wide format run (>64 watched events) was enough to kill the session.

Change (473d9cf on fix/notification-fence): under the LSP-spec ordering (notifications follow the disk change), all 18 sites are reconstructible. They now go through recover_unreconciled_file_notification: record the parseable events (Deleted → stamped tombstone, Created/Changed clear it; rename old → Deleted, new → Created, plus rename endpoints for overlay rejection), then Workspace::discard_for_unreconciled_file_notification (project cache overflow-invalidated plus TASK-5's discard_reconstructible_state). For unknown rename endpoints, only open documents whose file no longer exists are rejected (Q2). discard_reconstructible_state marks the captured override TOMLs dirty for re-read (Q3, OverrideSession::mark_all_dirty). Empty create/delete/rename batches are no-ops (Q4), and a rename pair with old == new is skipped. remember_deleted no longer fences past 256 tombstones (Q5). The fence helper, invalidate_malformed_file_notification, Workspace::permanently_fence_notification_analysis, fail_closed_notification_recovery and the MAX_FILE_OPERATION_RECOVERY_* constants are removed. The README file-notification section is rewritten for the spec model.

Tests: 23 existing protocol tests that asserted the fence now assert the new behaviour, with TASK-97 comments and, where the fixture used the old ordering, the unlink moved before the notification. 5 new protocol tests: git_checkout_sized_watched_batch_keeps_analysis_answering (302 events, 300 Deletes), more_than_256_deletes_across_in_envelope_batches_do_not_fence, notifications_without_an_event_array_discard_without_fencing (W1/F1/R1 + Q2), empty_file_operation_batches_are_no_ops, discard_re_reads_captured_override_files. One workspace unit test was flipped and one pascal-project unit test added. RED: all 28 new or flipped protocol tests fail on d55c0f6 sources (mostly -32803). The override test also fails with only mark_all_dirty disabled. GREEN: pascal-lsp lib 692/692, navigation 317, rename 94, project 88, neovim 9, protocol 906/909 (the 3 failures are load-sensitive and reproduce on d55c0f6 under the same load); pascal-project 115 passed. cargo fmt --check is clean, and clippy -D warnings is clean for pascal-lsp and pascal-project with and without test-support.

Known limits: ambiguous rename recovery still tombstones new endpoints (TASK-132). In-envelope tests that report a Delete before the unlink were not rewritten, since tombstones remain a defensive mechanism.
<!-- SECTION:FINAL_SUMMARY:END -->
