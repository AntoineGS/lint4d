---
id: TASK-74
title: Handle macOS case-insensitive paths in project and resolver identity
status: To Do
assignee: []
created_date: '2026-10-02 23:26'
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
<!-- AC:END -->
