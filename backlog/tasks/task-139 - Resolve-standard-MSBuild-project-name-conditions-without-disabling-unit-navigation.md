---
id: TASK-139
title: >-
  Resolve standard MSBuild project-name conditions without disabling unit
  navigation
status: Done
assignee:
  - OpenCode
created_date: '2026-10-08 13:51'
updated_date: '2026-10-08 14:10'
labels: []
dependencies: []
modified_files:
  - crates/pascal-project/src/lib.rs
  - crates/pascal-project/tests/public_api.rs
  - crates/pascal-project/README.md
  - crates/pascal-lsp/tests/project.rs
priority: high
type: bug
ordinal: 141000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The configured pascal-lsp release returns no definitions for any of the 13 uses entries in multidev/Projects/ChainDriveAPI/src/Database/CDAPI.Database.Centrale.pas. The correct ChainDriveAPI.dproj and Delphi 37.0 profile are selected. ChainDriveAPI.dproj:1111 contains Exists('$(MSBuildProjectName).deployproj'); the evaluator does not provide the standard MSBuildProjectName built-in, reports an unknown project condition, and ProjectContext.can_resolve_units becomes false. pascal-core/src/resolver.rs:1155 consequently rejects every unit before explicit-reference/search-path lookup. Supplying MSBuildProjectName='ChainDriveAPI' in an isolated copy of user configuration restored 10 of 13 definition requests with the original source/project files unchanged. This is a language-server/project-evaluator defect rather than a gd mapping or source syntax issue. Diagnostic probe and outputs from this session are in /tmp/opencode/probe-centrale-lsp.py and probe-centrale-lsp*.out; these temporary files are supplementary, not required reproduction dependencies.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A project using Exists('$(MSBuildProjectName).deployproj') evaluates that condition without a manually configured project-name property.
- [x] #2 Unit-definition navigation remains available for an otherwise resolvable project containing the standard deployment-import condition.
- [x] #3 Regression coverage proves the failure before the fix and covers both present and absent deployment files while preserving safe unsupported-import handling.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Approved bounded design; user chose current checkout.
- [x] Verify existing workspace baseline and inspect evaluator/import handling (3619 passed, 0 failed, 9 ignored).
- [x] Add and observe failing regression tests: absent/present deployment imports, immutable root identity in option sets, source-backed unit-definition navigation.
- [x] Seed MSBuildProjectName at root .dproj metadata evaluation using existing property accounting/provenance; preserve explicit configured global precedence and keep installation-only metadata unchanged.
- [x] Run full workspace tests, fmt and clippy; complete one bounded code-reviewer review with no blocking findings.
- [x] Rebuild target/release/pascal-lsp used by Neovim and rerun all 13 imports against the actual file with unchanged real configuration: 10 resolve, 3 remain TASK-140.
- [x] Finalize TASK-139 with evidence. TASK-140 and cdapi-n3p remain separate as approved.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED confirmed: all three new pascal-project public-API regressions fail for unknown Exists('$(MSBuildProjectName).deployproj') or unresolved MainSource expansion. New source-backed LSP regression also fails with 0 definitions instead of 1. Initial full cargo test baseline was terminated at its 120-second timeout without observed test failures; restarted all existing tests with --skip msbuild_project_name and no timeout. Approved current checkout is used; no user configuration/source edits.

GREEN: three pascal-project public API regressions and the real LSP navigation regression now pass. cargo fmt --check passes. The full baseline run completed successfully: 3619 passed, 0 failed, 9 ignored across 103 test groups. Post-change full workspace tests, clippy, and release build launched; bounded code review is running under the requesting-code-review skill.

Acceptance evidence: both absent and present Exists deployment-import public API cases now produce complete project contexts without a configured MSBuildProjectName. The imported-properties case retains Shop.Service root identity and expanded MainSource even when Nested.props assigns Wrong. The real LSP regression resolves Provider's unit header with both absent and present unsupported deployment files. All four regressions were observed failing before the fix. cargo clippy --workspace --all-targets -- -D warnings completed successfully with no warnings. Full suite, release build, and reviewer are still pending; task not yet finalized.

Bounded comprehensive-review__code-reviewer completed with no blocking findings; independently verified all four regression tests. Optional direct configured-MSBuildProjectName coverage was suggested. The existing generic immutable-configured-property integration regression was rerun and passed, and the new seed routes through that same guarded setter; no production changes requested by review. Only this reviewer was delegated; relevant correctness/safety/tests/documentation dimensions were covered, with wider unrelated review phases not launched. Release build and full-suite result still await completion before the actual-file probe/finalization.

Post-change cargo test --workspace --no-fail-fast completed successfully: 3623 passed, 0 failed, 9 ignored across 103 test groups. All workspace tests and doctests completed; no observed failures. Full-suite, fmt, clippy and bounded correctness review are satisfied. Waiting for the release build completion before retesting the real Centrale file and finalizing.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Fixed the project-evaluator blocker that disabled every unit jump from ChainDriveAPI's Centrale unit. Root .dproj evaluation now derives MSBuildProjectName from its filename, uses existing bounded property accounting/provenance, and protects that root identity against subsequent XML/option-set assignment. Explicit configured globals retain existing precedence. Installation-only metadata handling is unchanged, and unsupported deployment imports remain unread and unexecuted. Added documentation, three public-API regression tests, and one actual source-backed LSP navigation regression (both absent/present deployment cases). All four new tests were observed failing before the production fix and passing after.

Verification: cargo test --workspace --no-fail-fast completed with 3623 passed, 0 failed, 9 ignored; cargo fmt --check and cargo clippy --workspace --all-targets -- -D warnings passed; git diff --check passed. One bounded comprehensive-review__code-reviewer found no blocking issues. Release build of the Neovim-configured target/release/pascal-lsp completed successfully. A fresh stdio probe using the rebuilt binary and original real user/workspace configuration selects ChainDriveAPI.dproj and installation 37.0 with no unknown-project-condition warnings and resolves 10/13 imports (previously 0/13). FireDAC.Phys.IB, SysUtils, and mormot.core.base remain unresolved under TASK-140; stale project references remain cdapi-n3p. No real configuration or ChainDriveAPI source files were modified. Changes are uncommitted in the user-selected current checkout. Restart Neovim/Pascal LSP to load the rebuilt binary.
<!-- SECTION:FINAL_SUMMARY:END -->
