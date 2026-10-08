---
id: TASK-141
title: >-
  Share project-cache entries between workers whose project contexts differ only
  in recorded observations
status: Done
assignee:
  - '@OpenCode'
created_date: '2026-10-08 18:08'
updated_date: '2026-10-08 18:18'
labels:
  - lsp
  - runtime
  - performance
dependencies: []
priority: high
type: bug
ordinal: 141000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
In ChainDriveAPI (Projects/ChainDriveAPI/src/Database/CDAPI.Database.Centrale.pas), semantic tokens on a 134-line file took ~10 s and go-to-definition took ~9 s while the warmer was indexing, because the warmer and the request workers kept discarding each other's project-cache entries and re-parsing large units (mormot.core.os ~3.6 s, mormot.core.base ~2.4 s, System.SysUtils ~1.3 s).

Cause, established with the PASCAL_LSP_TRACE timeline (commit 141353e): project-cache entries are keyed by the project-context fingerprint but a hit also requires the stored ProjectContext to equal the caller's exactly. Import resolution appends observations to a worker's copy of the context as it reads project files (metadata_files, metadata_observations, warnings). The warmer had recorded two extra metadata files (CDAPI.Security.Auth.Types.pas, CDAPI.Security.Auth.Service.pas) that request workers had not, so contexts with the same fingerprint compared unequal ("context differs in metadata_files" / "metadata_observations") and every lookup recomputed and replaced the other worker's entry.

Measured with the same headless nvim scenario once the comparison ignored those fields: semantic tokens 9-10 s -> 0.7-1.3 s, gd during indexing 8.9-10.7 s -> 1.0-1.3 s, mormot.db.sql crawl 34 s -> 10 s.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A cache entry stored under one project context is a hit for a lookup whose context differs only in recorded resolution observations (metadata files, metadata observations, warnings); regression test fails on the original code
- [x] #2 A lookup whose context differs in any field that affects parsing or resolution (for example defines, search paths, platform) still misses
- [x] #3 pascal-lsp and pascal-project tests, cargo fmt and clippy pass
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. RED: project_cache cache_tests store a unit/interface entry under one context and look it up (lookup, peek_unit, peek_interface_imports) with a context that only adds metadata_files, metadata_observations and warnings; plus a negative test with a differing include path (not in the fingerprint, so only the comparison separates them).
2. GREEN: add ProjectContext::same_analysis_inputs in pascal-project, destructuring every field so new fields must be classified; ignore only metadata_files, metadata_observations, warnings.
3. Use it at every project_cache comparison site (lookup hit and re-check, peek, peek_unit_from_disk, put_interface_imports unchanged check, intern_context, trace compute reason). entry.context is only ever compared, never handed to consumers, so sharing an Arc with fewer observations is safe.
4. Direct pascal-project test of the method; fmt, clippy, pascal-lsp and pascal-project tests.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Regression test entries_are_shared_by_contexts_that_differ_only_in_recorded_observations failed on 141353e at the cache.unit Hit assertion (project_cache.rs:684) and passes with the fix; the negative test passed before and after, as expected. The pascal-project unit test for same_analysis_inputs was written alongside the new method. Full pascal-lsp + pascal-project test run green, fmt and clippy clean (load ~17 at the time). Earlier session measurement with the same change in the headless nvim ChainDriveAPI scenario: semantic tokens 9-10 s -> 0.7-1.3 s, gd during indexing 8.9-10.7 s -> 1.0-1.3 s.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Project-cache hits required the stored ProjectContext to equal the caller's exactly, including observations that import resolution appends to each worker's copy (metadata_files, metadata_observations, warnings). The warmer and request workers recorded different observations under one fingerprint, so they kept recomputing and replacing each other's entries and re-parsing large units.

Changes:
- pascal-project: ProjectContext::same_analysis_inputs, an exhaustive (destructured) comparison that ignores only those three observation fields.
- pascal-lsp project_cache: every context comparison (lookup, peeks, peek_unit_from_disk, interface put, intern_context, trace reason) uses it.

Tests: cache_tests::entries_are_shared_by_contexts_that_differ_only_in_recorded_observations (failed before the fix), cache_tests::entries_are_not_shared_by_contexts_that_differ_in_analysis_inputs, and same_analysis_inputs_ignores_only_recorded_observations in pascal-project. Full pascal-lsp and pascal-project suites, fmt and clippy pass.
<!-- SECTION:FINAL_SUMMARY:END -->
