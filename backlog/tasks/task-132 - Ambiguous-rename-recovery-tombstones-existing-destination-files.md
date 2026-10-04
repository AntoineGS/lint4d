---
id: TASK-132
title: Ambiguous rename recovery tombstones existing destination files
status: To Do
assignee: []
created_date: '2026-10-04 00:07'
updated_date: '2026-10-04 00:15'
labels:
  - lsp
  - runtime
dependencies: []
priority: low
type: bug
ordinal: 134000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found during TASK-97 (fix/notification-fence @ 473d9cf); also flagged in the TASK-97 classification (R3 row).

**Problem.** An ambiguous didRenameFiles batch (distinct pairs that reuse an endpoint, a chain, or a cycle) goes through `invalidate_ambiguous_file_notification` (crates/pascal-lsp/src/server.rs). That function records every endpoint, the *new* ones included, as `FileChange::Deleted`. Recovery then stamps a tombstone for each one with the file's current disk stamp (`remember_deleted`). Under the LSP-spec ordering adopted in TASK-97, the destination file already exists on disk, so `deletion_blocks_load_with_control` hides that existing file until its stamp changes. Analysis is not disabled; the moved unit simply cannot be resolved until the file is edited or a later Created/Changed event names it.

**Expected.** Record old endpoints as Deleted and new endpoints as Created (as `recover_unreconciled_file_notification` does for over-envelope renames). Keep rejecting open overlays at every endpoint.

**Repro idea.** Rename A→B and B→C in one batch, with C existing on disk afterwards. definition of a symbol in C should resolve; today it is empty.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Widened from the TASK-97 review (branch fix/notification-fence, 473d9cf): remember_deleted (workspace.rs ~12727) stamps a Deleted event with the file's CURRENT stamp even when the file exists again by processing time (unlink + recreate during a checkout, with a coalesced or stale Deleted). The tombstone then hides an existing file until it changes. TASK-97 now routes the discard path through this too. Under the LSP-spec ordering the user chose for TASK-97 (notifications describe changes already on disk), a Deleted whose file exists at processing time should not be tombstoned: tombstone only when the disk stamp is None. Cover both the rename-endpoint case and this one.
<!-- SECTION:NOTES:END -->
