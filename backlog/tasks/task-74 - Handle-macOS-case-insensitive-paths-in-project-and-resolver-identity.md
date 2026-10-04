---
id: TASK-74
title: Handle macOS case-insensitive paths in project and resolver identity
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:26'
updated_date: '2026-10-04 02:39'
labels:
  - core
  - correctness
milestone: m-3
dependencies: []
references:
  - crates/pascal-core/tests/unit_resolver.rs
  - crates/pascal-project/src/lib.rs
modified_files:
  - .github/workflows/ci.yml
  - Cargo.lock
  - crates/pascal-core/Cargo.toml
  - crates/pascal-core/src/resolver.rs
  - crates/pascal-core/tests/unit_resolver.rs
  - crates/pascal-lsp/src/server.rs
  - crates/pascal-lsp/src/workspace.rs
  - crates/pascal-lsp/src/workspace/projects.rs
  - crates/pascal-lsp/src/workspace/queries.rs
  - crates/pascal-lsp/src/workspace/rename.rs
  - crates/pascal-lsp/src/workspace/resolver.rs
  - crates/pascal-lsp/tests/project.rs
  - crates/pascal-lsp/tests/protocol.rs
  - crates/pascal-project/Cargo.toml
  - crates/pascal-project/README.md
  - crates/pascal-project/src/configuration.rs
  - crates/pascal-project/src/installations/ide_paths.rs
  - crates/pascal-project/src/lib.rs
  - crates/pascal-project/src/path_identity.rs
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
- [x] #1 Both tests run on macOS again (remove the Linux-only gate) and pass
- [x] #2 A restriction on a path applies to every spelling of it on a case-insensitive volume
- [x] #3 Resolved includes and units report the on-disk spelling on macOS
- [x] #4 The tests skipped on macOS for TASK-74 run there again (remove the not(target_os = "macos") gates)
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/macos-case-paths, branch fix/macos-case-paths, stacked on fix/windows-paths (10774ed, TASK-94, draft PR #6); iterate through a draft PR's macOS/Windows CI (user decision 2026-10-03).

Root cause: pascal_project::path_identity::is_case_insensitive is cfg!(windows), and several older checks (pascal-project lib.rs project_components_equal / ProjectPathIdentity / default excluded components / exclude globs, configuration.rs components_equal, pascal-lsp's own path helpers) are cfg(windows) too, so a case-insensitive macOS volume gets case-sensitive identity: a restriction on Errors.pas misses errors.pas, and includes keep the requested spelling.

Steps:
1. path_identity::is_case_insensitive probes the volume on macOS (pathconf _PC_CASE_SENSITIVE on the nearest existing ancestor), cached; Windows stays true, Linux stays false (no probe).
2. on_disk_spelling / resolve_existing_path_status: on case-insensitive volumes, an exact path whose canonical form is the requested spelling needs no listing (keeps path_resolution_does_not_list_directories_for_exact_paths cheap on macOS); other spellings walk.
3. Migrate the cfg(windows) case checks in pascal-project and pascal-lsp onto path_identity (one rule for Windows and macOS; Linux unchanged).
4. pascal-core: directory-listing caches and package_catalogues keyed by path_key; decide the driveless rooted path special case.
5. pascal-lsp include lookups / document links report the on-disk spelling (Windows and macOS).
6. Remove the TASK-74 macOS gates; tests that need case-distinct directories run on a case-sensitive volume on macOS CI (disk image) instead of being skipped.
7. README USERPROFILE wording.
8. Linux fmt/clippy/focused tests before each push; read macOS + Windows CI; stop after ~8 pushes if a residue remains (Deferred, residue grouped in notes).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Tests skipped on macOS for this gap (grep for TASK-74 to find them all): pascal-core/tests/unit_resolver.rs (2), pascal-lsp/tests/project.rs (5), pascal-lsp/tests/protocol.rs (2), pascal-lsp/src/workspace.rs (1), pascal-project/src/configuration.rs (module), pascal-project/src/installations/ide_paths.rs (3), pascal-project/src/lib.rs (3).

Performance: off Linux, resolve_existing_path_status (pascal-project/src/lib.rs) lists every directory along a path to find the on-disk spelling, even for exact paths. On macOS CI that was 240 listings for 24 files (path_resolution_does_not_list_directories_for_exact_paths). A case-aware fix should keep exact paths cheap on case-insensitive volumes too.

Decision (user, 2026-10-03): iterate through CI with a draft PR, as for TASK-94. Sequenced after TASK-94 because both change path identity in pascal-project.

From the TASK-94 review (branch fix/windows-paths, PR #6), for the stacked TASK-74 work on the shared pascal_project::path_identity module:
- pascal-lsp include lookups and document-link targets still report the requested spelling on Windows (e.g. Selected.inc vs selected.inc); the on-disk-spelling migration should cover Windows as well as macOS.
- Directory listing caches (resolver.rs ~1632) and package_catalogues (resolver.rs ~2213) are keyed by canonical_path, which no longer lowercases on Windows; two spellings of one directory produce two listings and count twice against max_package_catalogues. Key them by path_key (a case-folding key on case-insensitive volumes).
- resolver.rs ~3359 keeps driveless rooted paths (`\workspace`) only for MemoryStore fixtures; consider fixing the fixtures instead.
- README (pascal-project) says the USERPROFILE fallback is Windows-only, but delphi_overrides.rs user_home_dir applies it on every OS.

Draft PR #7 (https://github.com/AntoineGS/lint4d/pull/7), stacked on #6. Push 1: 87119ea.

Ruling: the macOS probe is pathconf(_PC_CASE_SENSITIVE) on the path or its nearest existing ancestor, cached per path (bounded, 4096 entries) rather than per directory or per volume root — a directory-keyed cache would give a mount point its parent volume's answer, and a device-keyed cache needs a stat per call anyway. Volumes that cannot answer count as case-insensitive (the macOS default). Linux keeps returning false with no probe; Windows true.

Ruling: tests that need case-distinct entries (Foo beside FOO) keep their Windows gate but run on macOS on a case-sensitive APFS volume that CI mounts (LINT4D_CASE_SENSITIVE_TMPDIR, path_identity::case_sensitive_test_dir); under CI a missing case-sensitive volume fails the test instead of skipping it. This also exercises the per-volume probe on both kinds of volume.

Ruling: the driveless rooted path special case in pascal-core canonical_path stays. Fixing the fixtures would touch ~111 /workspace paths in unit_resolver.rs alone; real Windows paths always carry a drive or UNC prefix, so the special case only affects in-memory fixtures.

Ruling: pathconf-based probing adds libc as a macOS-only dependency of pascal-project (already in the lockfile via pascal-lsp). path_identity::with_case_insensitive_volumes (test-support) forces the case-insensitive rule on one thread so identity logic can be tested on Linux; pascal-core enables pascal-project/test-support in dev-dependencies.

TDD: RED locally (forced case-insensitive rule) for resolver::tests::spellings_of_one_directory_share_a_listing_and_a_catalogue_on_case_insensitive_volumes: "incomplete_reasons: [package/source catalogue limit (1) reached]" — the second spelling counted as a new catalogue. GREEN after keying package_catalogues / session cache / directory cache / catalogue visited set by path_key. RED for the two named tests is the macOS failure recorded in this task's description (PR #5).

Push 1 (87119ea, run 37164776274): macOS 2 failures, Windows 1. macOS: two pascal-lsp unit tests assumed a case-sensitive temp volume (install_context_keeps_case_distinct_linux_metadata_paths: debug.optset overwrote Debug.optset; mapped_read_authorization_is_context_scoped_and_component_safe asserted "Linux containment must remain case-sensitive"). Windows: file_watch::tests::unwatched_directory_reports_nothing (untouched code; passed on the next two runs; follow-up TASK-133). Everything else passed on macOS, including both named tests, all formerly gated tests, path_resolution_does_not_list_directories_for_exact_paths (0 listings) and on_disk_spelling_corrects_letter_case_only_on_case_insensitive_volumes (so macOS realpath does report the on-disk letter case).

Push 2 (3d8e80f, run 37165297908): all 11 jobs green. Test (macos-latest) 3214 passed / 0 failed / 9 ignored; Test (windows-latest) 3061 / 0 / 9; ubuntu 3267 / 0 / 9. The macOS job mounts a case-sensitive APFS volume (hdiutil, /Users/runner/work/_temp/case-sensitive) for the case-distinct tests.

Push 3 (03116a3, run 37166026810): case_sensitive_test_dir behind the test-support feature; all 11 jobs green with the same counts.

Note: ambiguity_at_current_tier_blocks_search_path_fallback (unit_resolver, in-memory paths under /workspace) now returns early when /workspace's volume is case-insensitive (Windows and default macOS) instead of a Windows-only cfg gate; it was never a TASK-74 gate. overlay_under_another_letter_case_replaces_the_disk_file and unsaved_overlay_has_one_identity_under_every_spelling now run wherever the temp volume is case-insensitive (Windows and macOS).

Follow-up created: TASK-133 (Windows file_watch flake seen once on this PR).

Implemented on branch fix/macos-case-paths (worktree .worktrees/macos-case-paths), awaiting review and merge. Draft PR #7, stacked on #6 (merge after it).

Fix round 1 (review of 10774ed..03116a3): (1) the four cache keys moved to path_identity::path_lookup_key (lossless PathBuf, ASCII case folded per component only on case-insensitive volumes): resolver.rs DirectoryCacheKey.path, package_catalogues + ResolverSessionCache catalogues, the catalogue visited set, and pascal-lsp ProjectPathLookupKey. RED on Linux: resolver::tests::directories_that_only_print_alike_keep_their_own_listings_and_catalogues — catalogue units [backslash.pas, latiny.pas] instead of [backslash.pas, latinp.pas, latiny.pas, nested.pas] (pkg/a/b and lib\xfe skipped as already visited); workspace::tests::merged_read_observations_keep_paths_that_only_print_alike — 3 merged observations instead of 4 (lib\xff.dproj and lib\xfe.dproj collapsed to one U+FFFD key). GREEN after the change, plus path_identity tests lookup_keys_*. (2) mapped_read_authorization_is_case_insensitive_and_excludes_case_variants (was _on_windows, cfg(windows)) now runs wherever the temp volume is case-insensitive; it ran on macOS and Windows CI.

Fix round 1 CI: run 37167069275 (3b45e13) failed Check/Test (windows-latest) on dead code (the unix-only TreeStore fixture); fixed in e443d6a. Run 37167338491 (e443d6a): macOS 3219/0/9, ubuntu 3272/0/9; Windows attempt 1 failed only file_watch::tests::unwatched_directory_reports_nothing (TASK-133), attempt 2 green 3062/0/9. All 11 jobs green.

Fix round 1 follow-ups: TASK-134 created (probe cache cost, fallback docs, pathconf on stale mounts, untracked on_disk_spelling reads in reconcile_ide_path, eager double glob compile); TASK-133 now has acceptance criteria.

Merged into master as bbad3f1 (branch fix/macos-case-paths, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: path identity, comparisons and restrictions were case-insensitive only under cfg(windows), so on macOS's default case-insensitive APFS volume one file could surface under two spellings (an include requested as ../shared/BODY.INC kept that spelling) and a strict restriction on Errors.pas was bypassed by requesting errors.pas. Two resolver tests and 15 more were gated off macOS.

Change (stacked on TASK-94's path_identity, branch fix/macos-case-paths, draft PR #7 on top of #6):
- path_identity::is_case_insensitive asks the volume on macOS (pathconf _PC_CASE_SENSITIVE on the path or nearest existing ancestor, cached per path, bounded); Windows true, Linux false with no probe. libc is a macOS-only dependency of pascal-project.
- resolve_existing_path_status: on case-insensitive volumes an exact path whose canonical form is the requested spelling needs no listing (macOS: 0 listings for 24 exact files, was 240); exact entries on case-sensitive volumes take the symlink_metadata shortcut Linux had.
- The older cfg(windows) case checks now use path_identity: pascal-project project path identity/equality/prefix, default excluded components, exclusion globs (case-folding variant compiled on demand), configuration.rs; pascal-lsp exclusion globs, default excluded components, project lookup keys, package/native path comparisons, projects.rs, rename.rs and resolver.rs path keys, watcher keys.
- pascal-core: directory listing cache, package catalogues (and session cache, catalogue visited set) keyed by path_identity::path_lookup_key, a lossless PathBuf key that folds ASCII case per component only on case-insensitive volumes, so spellings of one directory share one listing and count once against max_package_catalogues while distinct directories (a\b vs a/b on Linux, non-UTF-8 names) never merge. pascal-lsp's ProjectPathLookupKey uses the same key.
- On-disk spelling: pascal-lsp include lookups (document links, expansion) and IDE path reconciliation report the on-disk spelling on Windows and macOS.
- Tests: TASK-74 gates removed. Tests needing case-distinct entries keep their Windows gate and run on macOS on a case-sensitive APFS volume mounted by CI (LINT4D_CASE_SENSITIVE_TMPDIR via path_identity::case_sensitive_test_dir, which fails under CI rather than skipping). New: case_rule_matches_what_the_volume_does, identity_rules_can_be_forced_case_insensitive_for_tests, spellings_of_one_directory_share_a_listing_and_a_catalogue_on_case_insensitive_volumes, include_document_link_targets_the_on_disk_spelling. path_identity::with_case_insensitive_volumes (test-support) lets identity logic be tested on Linux.
- README: USERPROFILE fallback applies on every host. The driveless rooted path special case in canonical_path stays (ruling in notes).

Commits: 87119ea, 3d8e80f, 03116a3; fix round 1: 17bd4d5, 3b45e13, e443d6a.

Tests: CI run 37167338491 (e443d6a, Windows job rerun once for the TASK-133 flake): all 11 jobs green; macOS 3219 passed / 0 failed / 9 ignored, Windows 3062 / 0 / 9, ubuntu 3272 / 0 / 9. (Before fix round 1: run 37166026810.) Linux locally: cargo fmt --check, clippy -D warnings for pascal-project, pascal-core, pascal-lsp, lint4d; cargo test -p pascal-project -p pascal-core -p lint4d (873 passed) and -p pascal-lsp all green (three load-timing protocol tests timed out once in a full run and passed alone; same timing as 10774ed).

Follow-ups: TASK-133 (Windows file_watch flake, now with acceptance criteria), TASK-134 (probe cache cost and fallback docs, untracked on_disk_spelling reads in IDE path reconciliation, eager double glob compile in pascal-lsp).

Known limits: the macOS probe treats volumes that cannot answer pathconf as case-insensitive; the per-path cache is not invalidated when a volume is mounted over an already-probed path; APFS folds Unicode case while the walk folds ASCII only (unchanged).
<!-- SECTION:FINAL_SUMMARY:END -->
