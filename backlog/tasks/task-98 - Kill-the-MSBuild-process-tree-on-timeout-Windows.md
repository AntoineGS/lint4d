---
id: TASK-98
title: Kill the MSBuild process tree on timeout (Windows)
status: To Do
assignee: []
created_date: '2026-10-03 03:00'
labels:
  - core
  - runtime
dependencies:
  - TASK-27
priority: low
ordinal: 100000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-27 drains pipes and kills+reaps the `cmd` child on timeout, but descendants (MSBuild, node reuse) can outlive it. Needs a Windows job object or `taskkill /T /F /PID` in `wait_with_timeout` (crates/pascal-core/src/discovery_msbuild.rs). Deferred from TASK-27: no Windows target is installed on the Linux dev machine so it cannot be compiled or tested here. Note: surviving descendants hold the pipe write ends, so reader threads stay detached until they exit.
<!-- SECTION:DESCRIPTION:END -->
