---
id: TASK-124
title: 'unchecked-nil: resolve overloaded callees by argument count'
status: To Do
assignee: []
created_date: '2026-10-03 23:31'
labels:
  - lint
dependencies:
  - TASK-21
priority: low
type: enhancement
ordinal: 126000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-up from the TASK-21 review (branch fix/cfg-overload-identity, d7d64ac). After TASK-21, a call to an overload set whose members disagree on whether they can return nil is treated as unanalyzable (no report), because calls are resolved by name only. That misses real reports when the nil-returning overload is the one called. Resolve the callee among same-named routines by argument count (and simple literal types where cheap) before falling back to 'unanalyzable'. See crates/lint4d/src/rules/nil_check.rs (can_function_return_nil and its overload loop). Related: TASK-116 (placeholder left in FN_RETURN_CACHE on None exits).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A call whose argument count selects exactly one overload uses that overload's nil behaviour
- [ ] #2 Ambiguous calls stay unanalyzable; tests cover both
<!-- AC:END -->
