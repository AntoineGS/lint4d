---
id: TASK-94
title: Make source-project resolution work on Windows
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:40'
updated_date: '2026-10-04 02:39'
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
modified_files:
  - .github/workflows/ci.yml
  - crates/lint4d/src/engine/mod.rs
  - crates/lint4d/tests/cli_test.rs
  - crates/pascal-core/src/resolver.rs
  - crates/pascal-core/src/resolver_store.rs
  - crates/pascal-core/tests/delphi_overrides.rs
  - crates/pascal-core/tests/unit_resolver.rs
  - crates/pascal-lsp/README.md
  - crates/pascal-lsp/src/include_expansion.rs
  - crates/pascal-lsp/src/project_cache.rs
  - crates/pascal-lsp/src/server.rs
  - crates/pascal-lsp/src/server/project_prompts.rs
  - crates/pascal-lsp/src/server/uri_spelling.rs
  - crates/pascal-lsp/src/workspace.rs
  - crates/pascal-lsp/src/workspace/code_lenses.rs
  - crates/pascal-lsp/src/workspace/codeactions.rs
  - crates/pascal-lsp/src/workspace/project_catalogue.rs
  - crates/pascal-lsp/src/workspace/queries.rs
  - crates/pascal-lsp/src/workspace/rename.rs
  - crates/pascal-lsp/tests/project.rs
  - crates/pascal-lsp/tests/protocol.rs
  - crates/pascal-project/README.md
  - crates/pascal-project/src/configuration.rs
  - crates/pascal-project/src/delphi_overrides.rs
  - crates/pascal-project/src/installations/ide_paths.rs
  - crates/pascal-project/src/installations/roots.rs
  - crates/pascal-project/src/lib.rs
  - crates/pascal-project/src/path_identity.rs
  - crates/pascal-project/tests/installations.rs
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
- [x] #1 Test (windows-latest) passes, and continue-on-error and the Windows-only --no-fail-fast are removed from .github/workflows/ci.yml
- [x] #2 lint4d --project resolves the source project on Windows (the cli_test project_* tests pass there)
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/windows-paths, branch fix/windows-paths (from master d55c0f6); iterate through a draft PR's Windows CI job (user decision 2026-10-03).

1. Baseline: failure list from master CI run 37157098603 (d55c0f6), grouped by root cause (notes).
2. Shared host path identity in pascal-project (path_identity module): is_case_insensitive(path) (cfg(windows) today, takes the path so TASK-74 can probe the volume), components_equal/paths_equal/path_starts_with/path_key, walk_root (prefix + root), without_verbatim_prefix.
3. Fix root causes in batches:
   a. component walks start at the bare separator and drop the drive (pascal-project resolve_existing_path_status, pascal-lsp resolve_case_insensitive_path / include lookup);
   b. pascal-core canonical_path lowercases paths on Windows, and lowercased paths flow into plain comparisons / reported paths (lint4d "CFG target path ... does not match");
   c. user config needs HOME: fall back to USERPROFILE;
   d. 8.3 short names: walk maps an unlisted alias to its long name; CI uses a long-name TEMP so fixture paths agree with resolved long names (mirrors the macOS TMPDIR step);
   e. verbatim \\?\ paths from canonicalize turned back into plain paths.
4. Each batch: regression tests (host-independent where possible, cfg(windows) otherwise), Linux fmt/clippy/focused tests, push, read the Windows job, regroup.
5. When Windows is green: drop continue-on-error and --no-fail-fast from ci.yml. Stop after ~8 pushes if a residue remains (Deferred, residue grouped in notes).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Baseline from the first complete Windows run (CI run 37080784330, commit 070e8ae, --no-fail-fast): 431 failed tests across 12 test binaries; the largest are 204/634 (pascal-lsp protocol suite) and 85/650 (pascal-lsp unit tests). Expect several root causes to account for most of them; fix those first and recount.

Decision (user, 2026-10-03): iterate through CI: push fix branches to origin and open a draft PR against master for each (CI only runs on master pushes and PRs). Never push master; nothing merges without the user.

Baseline at master d55c0f6 (CI run 37157098603, job 111302612822): 435 failing tests ("---- stdout" blocks). By binary: pascal-lsp protocol 205/637, pascal-lsp unit 88/659, pascal-core unit_resolver 34/52, pascal-project unit 31/99, pascal-lsp project 30/58, local_installations 14/30, public_api 11/22, build_selection 8/53, cli_test 6/23, navigation 5/317, build_context_freshness 2/2, pascal-core unit 1/102.
Grouping by first warning/message (a test can show several): workspace root could not be resolved (drive dropped by the component walk) 77; project has no resolvable MainSource 40 and main source path does not exist 38 (same walk); pascal-core lowercased canonical paths reaching plain comparisons (e.g. "c:\users\...\errors.pas" vs "C:\Users\...\Errors.pas", lint4d "CFG target path ... does not match", filename catalogue LimitExceeded with observed 0 = catalogue root outside the read policy) ~50; verbatim \\?\ paths from canonicalize vs fixture paths (configuration tests) 3+; 8.3 short names (fixtures under C:\Users\RUNNER~1, resolution reports runneradmin) affects most assert_eq path comparisons; HOME unset 4; remaining (Unix-only fixture paths like /mapped/lib, code-lens/URI) to be regrouped after the first batch.

CI iterations on draft PR #6 (https://github.com/AntoineGS/lint4d/pull/6):
- push 1 (a0b3fd2): Windows Check failed to compile (cfg(windows) test module lacked imports); no test data.
- push 2 (d4c7a0f-ish, "test: give Windows fixture URIs a drive..."): 435 -> 70 failing. cli_test project_* now pass. Remaining groups: case-distinct-directory tests (7, cannot exist on NTFS), relocation of foreign Windows paths onto Unix roots (roots.rs module 11, ide_paths 3, installations.rs 1), MemoryStore fixtures with driveless /workspace paths (unit_resolver 29 + resolver unit 1), Unix fixture URIs in pascal-lsp unit tests (uri_spelling 3, project_prompts, warm progress, fix-all, code lenses), verbatim \\?\ canonicalize results in document links / test keys (4), mixed-separator fixture expectations (3), host-dependent accounting in two pascal-project tests (2), include found under the requested spelling instead of the on-disk one (1, the TASK-74 symptom on Windows), stamp tick granularity in a cache probe test (1).
- push 3: 70 -> 2 (two more tests whose premise needs a case-sensitive volume).
- push 4: continue-on-error removed.

Ruling: canonical_path in pascal-core keeps the spelling instead of lowercasing on Windows — those paths are opened, reported and compared with callers' paths; case-insensitive identity is path_key / path_identity's job. Source IDs therefore keep the spelling, matching pascal-lsp's own source_id_for_path.

Ruling: the user configuration home falls back to USERPROFILE when HOME is unset or empty, keeping the documented ~/.config/delphi-tools/config.toml layout rather than switching Windows to %APPDATA% — one layout on every host, and XDG_CONFIG_HOME still wins.

Ruling: CI points TEMP/TMP at RUNNER_TEMP (a long-name path) on Windows, mirroring the macOS TMPDIR step: resolution reports long names, so fixtures compared with resolved paths must live under a long name. Short-name inputs are covered by the walk's alias step and a Windows-only test that resolves a real 8.3 path.

Ruling: tests that relocate imported Windows paths onto Unix roots (installations/roots.rs tests, three ide_paths tests, installations.rs profiles_merge) and tests that need case-distinct directories are skipped on Windows (cfg(not(windows))), with a comment each; on Windows the imported paths are native and such directories cannot coexist.

Ruling: --no-fail-fast stays: 1be9a70 made it apply to every OS on purpose ("A failing test binary stopped the rest of the workspace"), so there is no Windows-only flag left to remove; only continue-on-error is dropped.

Ruling: the case-insensitive rule lives in pascal_project::path_identity::is_case_insensitive(path), which takes the path so TASK-74 can probe the volume; on_disk_spelling uses canonicalize, which reports the on-disk spelling on Windows only — TASK-74 needs a listing-based variant for macOS.

- push 5 (05e587d): Windows green (3049 passed / 0 failed / 9 ignored, run 37161385253) after fixing a second test that rewrites a file within one mtime tick (cache_tests::peek_unit_misses_when_a_probe_fails; it passed in run 3, so tick-flaky).
- push 6 (30919b0): Windows green again (3051 passed / 0 failed / 9 ignored, run 37161909281, all 11 jobs success) with continue-on-error removed. The new system-volume 8.3 test ran (it fails on CI instead of skipping when no short name exists).

Follow-ups created: TASK-122 (ProjectReadStamp misses same-size edits within one mtime tick; seen as two flaky Windows tests), TASK-123 (decide installation relocation on Windows hosts; its tests are Unix-host only).

For TASK-74: pascal_project::path_identity is the shared rule (is_case_insensitive(path), components_equal, paths_equal, path_starts_with, path_key, on_disk_spelling). Older cfg(windows) case checks were not migrated: project_components_equal / ProjectPathIdentity / is_default_excluded_component / compile_exclude_patterns (pascal-project lib.rs), configuration.rs components_equal, and pascal-lsp's own path helpers. on_disk_spelling relies on canonicalize, which fixes letter case on Windows only.

Implemented on branch fix/windows-paths (worktree .worktrees/windows-paths), awaiting review and merge. Draft PR #6 (not marked ready, not merged).

Fix round 1 (review of d55c0f6..30919b0): (1) end-to-end 8.3 coverage. New Windows cli_tests build a project under %LOCALAPPDATA%\Temp (C:\Users\runneradmin\..., which has 8.3 names on the runner; under CI they fail instead of skipping when no short name exists): --project through a short-name path, and a .dproj whose MainSource/DCCReference name Main.pas by an absolute short path. RED on CI run 37162714361 (d274728): the second failed with "could not resolve source project for C:/Users/RUNNER~1/.../LONGPR~1/Main.pas: ... outside the requester-scoped project roots; using file-local CFG"; the first already passed, because lint4d canonicalizes the --project path at entry (main.rs canonical_project_path). Fix (10774ed): path_identity::on_disk_spelling falls back to the directory walk for aliases other than letter case (short names map to long ones, symlinked components keep their name); UnitResolver::load_source maps the request path through it before path_entry_for, and lint4d's CFG target/lint file comparison compares on-disk spellings. Linux unchanged (on_disk_spelling returns None on case-sensitive volumes). (2) overlay_for's case-insensitive fallback now returns the overlay's own path for an unsaved file (a file on disk keeps the on-disk spelling load found), so every spelling shares one SourceId; RED in the same run (unsaved_overlay_has_one_identity_under_every_spelling: source:...\new.pas vs ...\New.pas). GREEN: CI run 37163072689 on 10774ed, all 11 jobs success, Windows 3054 passed / 0 failed / 9 ignored.

Ruling (fix round 1): no extra CI job without the TEMP override — the new cli_tests place their fixtures under the runner's stock user temp directory (C:\Users\runneradmin\AppData\Local\Temp, reached as RUNNER~1) explicitly, which covers the short-name case end to end without doubling the Windows test job; the TEMP override stays so path-comparing fixtures elsewhere keep long names.

TASK-122 now has acceptance criteria.

Merged into master as d43f9ae (branch fix/windows-paths, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: source-project resolution failed on Windows (435 failing tests on master d55c0f6's Windows job): component walks started at the bare separator and dropped the drive; pascal-core lowercased canonical paths on Windows and those reached plain comparisons and reports (lint4d "CFG target path c:\users\... does not match"); user configuration required HOME; 8.3 short names (RUNNER~1) were not in directory listings; canonicalize's verbatim \\?\ paths leaked into comparisons; a file found under another letter case kept the requested spelling.

Change:
- New pascal_project::path_identity: is_case_insensitive(path) (cfg(windows) today; takes the path so TASK-74 can probe volumes), components_equal/paths_equal/path_starts_with/path_key, walk_root (prefix + root), without_verbatim_prefix, on_disk_spelling.
- pascal-project: resolve_existing_path_status walks from walk_root, maps an unlisted 8.3 alias to its listed long name, and on Windows skips the walk when canonicalize already reports the requested spelling; configuration paths drop the verbatim prefix; user config home falls back to USERPROFILE (user_home_dir, user_config_path_from_env, used by pascal-project and pascal-lsp; READMEs updated).
- pascal-core: canonical_path keeps the spelling (and the form of driveless rooted paths); path_key/path_starts_with/relative_components use path_identity; the filesystem store reports and authorizes the on-disk spelling and finds overlays under another letter case on case-insensitive volumes.
- lint4d: CFG target/lint file comparison uses path_identity::paths_equal.
- pascal-lsp: case-insensitive walks keep the drive; document links, project catalogue and a test key drop verbatim prefixes.
- Tests: Unix-only fixture URIs/paths made host-neutral; tests needing case-distinct directories or relocating foreign Windows paths onto Unix roots skip Windows; two host-dependent accounting tests compute the host value; two cache tests move the mtime explicitly.
- CI: Windows TEMP/TMP point at RUNNER_TEMP (long names), continue-on-error removed. --no-fail-fast kept (all-OS since 1be9a70).

Commits: a0b3fd2, 7e38e21, 5f013e1, ece72fd, 05e587d, 30919b0 (branch fix/windows-paths, draft PR #6).

Tests: CI run 37161909281 on 30919b0: all jobs green; Test (windows-latest) 3051 passed / 0 failed / 9 ignored, including cli_test project_* (AC #2) and new Windows tests (drive kept, letter case fixed, 8.3 names in temp and on the system volume, case-variant overlay, USERPROFILE fallback, on_disk_spelling). Linux locally: fmt, clippy -D warnings for pascal-project, pascal-core, pascal-lsp, lint4d; cargo test -p pascal-project -p pascal-core -p lint4d -p pascal-lsp green (one load-timing protocol flake passed on rerun).

Known limits: tests skipped on Windows are listed in the notes; relocation on Windows hosts is TASK-123; stamp tick granularity is TASK-122; older cfg(windows) case checks remain for TASK-74 to migrate; LSP include lookups outside the pascal-core store may still report the requested letter case on Windows.

Fix round 1: commits d274728 (RED tests), 10774ed (fix). 8.3 short-name request paths (e.g. a .dproj naming its sources by absolute short paths) now resolve the source project: on_disk_spelling walks for short-name aliases, and load_source and lint4d's target check use it. Unsaved overlays keep one identity under every spelling. CI run 37163072689: all green, Windows 3054 passed / 0 failed / 9 ignored.
<!-- SECTION:FINAL_SUMMARY:END -->
