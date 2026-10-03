---
id: TASK-94
title: Make source-project resolution work on Windows
status: To Do
assignee: []
created_date: '2026-10-02 23:40'
updated_date: '2026-10-03 00:19'
labels:
  - core
  - lint
  - correctness
milestone: m-3
dependencies: []
references:
  - .github/workflows/ci.yml
  - crates/pascal-project/src/lib.rs
  - crates/lint4d/tests/cli_test.rs
priority: high
type: bug
ordinal: 96000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Source-project resolution has never worked on Windows: test code there had not compiled since April, and pascal-project refused every Windows absolute path until fix/ci-green (PR #5) made those checks host-aware. With that fixed, the Windows CI job shows more bugs. The Test (windows-latest) job is continue-on-error and runs with --no-fail-fast until this is done; its log lists every failure.

Seen in lint4d's cli_test alone (6 failures, e.g. project_flag_lints_dproj_files):
- A drive letter is dropped: "project has no resolvable MainSource: \Users\runneradmin\...\MyProject.dproj".
- Lowercased path identities reach a plain path comparison: "CFG target path c:\users\...\unit1.pas does not match lint file C:\Users\...\Unit1.pas".
- User configuration requires HOME, which Windows does not normally set: "HOME must be a non-empty absolute path" (use USERPROFILE / the platform config dir).
- Temp paths arrive as 8.3 short names (C:\Users\RUNNER~1\...), while canonicalized paths use long names; identities must agree.

No local Windows build is available on the Linux dev machine, so iterate through CI or a Windows host.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Test (windows-latest) passes, and continue-on-error and the Windows-only --no-fail-fast are removed from .github/workflows/ci.yml
- [ ] #2 lint4d --project resolves the source project on Windows (the cli_test project_* tests pass there)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Baseline from the first complete Windows run (CI run 37080784330, commit 070e8ae, --no-fail-fast): 431 failed tests across 12 test binaries; the largest are 204/634 (pascal-lsp protocol suite) and 85/650 (pascal-lsp unit tests). Expect several root causes to account for most of them; fix those first and recount.
<!-- SECTION:NOTES:END -->
