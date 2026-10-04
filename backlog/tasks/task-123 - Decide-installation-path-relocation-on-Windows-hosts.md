---
id: TASK-123
title: Decide installation path relocation on Windows hosts
status: To Do
assignee: []
created_date: '2026-10-03 23:12'
labels:
  - core
dependencies:
  - TASK-94
references:
  - crates/pascal-project/src/installations/roots.rs
  - crates/pascal-project/src/installations/ide_paths.rs
  - crates/pascal-project/tests/installations.rs
priority: low
type: task
ordinal: 125000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Installation relocation (crates/pascal-project/src/installations/roots.rs relocate_environment, ide_paths.rs) infers mappings from imported Windows paths (rsvars.bat, environment.proj, EnvOptions.proj from another machine, e.g. C:\Old\SDK) to locally configured roots. Its tests use Unix destinations and treat C:\ paths as foreign, so TASK-94 skips them on Windows (installations/roots.rs tests module, three ide_paths tests, installations.rs profiles_merge_properties_and_mapping_origins_by_layer_and_profile).

On a Windows host the imported paths are native, so relocation behaves differently (no inference; an unmapped locator is a valid missing path rather than an error). Decide whether copied installation metadata from another Windows machine (different drive or root) should be relocated on Windows too, and add Windows-host tests with native destinations either way.
<!-- SECTION:DESCRIPTION:END -->
