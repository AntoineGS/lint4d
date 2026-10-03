---
id: TASK-74
title: Handle macOS case-insensitive paths in project and resolver identity
status: To Do
assignee: []
created_date: '2026-10-02 23:26'
updated_date: '2026-10-03 00:08'
labels:
  - core
  - correctness
milestone: m-3
dependencies: []
references:
  - crates/pascal-core/tests/unit_resolver.rs
  - crates/pascal-project/src/lib.rs
priority: medium
type: bug
ordinal: 74000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
macOS's default filesystem (APFS) ignores letter case, but path identity, comparisons and restrictions in pascal-project and the resolver are case-insensitive only under cfg(windows); macOS gets the case-sensitive Linux behavior.

Observed in CI (PR #5, fix/ci-green), where these two tests failed on macOS and were gated to Linux:
- authorized_parent_relative_and_absolute_include_case_adjustment_is_preserved: an include requested as ../shared/BODY.INC resolves under the requested spelling instead of the on-disk shared/body.inc, so one file can surface under two spellings.
- case_adjusted_include_reapplies_exact_restricted_provenance: a Configured (strict) restriction on Errors.pas is not reapplied when the same file is requested as errors.pas through the legacy route; on a case-insensitive filesystem that is the same file, so the restriction can be bypassed by changing case.

Case sensitivity is per volume on macOS (APFS can be case-sensitive), so a fix should probe the filesystem rather than key off target_os.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Both tests run on macOS again (remove the Linux-only gate) and pass
- [ ] #2 A restriction on a path applies to every spelling of it on a case-insensitive volume
- [ ] #3 Resolved includes and units report the on-disk spelling on macOS
- [ ] #4 The tests skipped on macOS for TASK-74 run there again (remove the not(target_os = "macos") gates)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Tests skipped on macOS for this gap (grep for TASK-74 to find them all): pascal-core/tests/unit_resolver.rs (2), pascal-lsp/tests/project.rs (5), pascal-lsp/tests/protocol.rs (2), pascal-lsp/src/workspace.rs (1), pascal-project/src/configuration.rs (module), pascal-project/src/installations/ide_paths.rs (3), pascal-project/src/lib.rs (3).

Performance: off Linux, resolve_existing_path_status (pascal-project/src/lib.rs) lists every directory along a path to find the on-disk spelling, even for exact paths. On macOS CI that was 240 listings for 24 files (path_resolution_does_not_list_directories_for_exact_paths). A case-aware fix should keep exact paths cheap on case-insensitive volumes too.
<!-- SECTION:NOTES:END -->
