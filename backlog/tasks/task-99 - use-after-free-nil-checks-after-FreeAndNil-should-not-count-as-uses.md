---
id: TASK-99
title: 'use-after-free: nil checks after FreeAndNil should not count as uses'
status: To Do
assignee: []
created_date: '2026-10-03 03:04'
labels:
  - lint
  - correctness
dependencies: []
priority: low
type: bug
ordinal: 101000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while doing TASK-24 (LINT-6). The use-after-free rule treats every identifier read of a freed variable as a use. After `FreeAndNil(X)` the variable is nil, so `if Assigned(X) then`, `if X <> nil then` and `if X = nil then` are legitimate checks, but the rule reports them as "Use after free". The old text-based rule did the same (word match), so TASK-24 kept the behaviour.

Repro (rule `use-after-free`, crates/lint4d/src/rules/use_after_free.rs):
```pascal
unit U;
interface
implementation
procedure Test;
var X: TObject;
begin
  X := TObject.Create;
  FreeAndNil(X);
  if Assigned(X) then
    X.Foo;
end;
end.
```
`lint4d U.pas` reports "Use after free: 'x'" at 9:3 on the `if Assigned(X)` header (checked on branch fix/use-after-free-effects, commit 51ed0c6).

Suggested direction: track FreeAndNil as "freed and nil" separately from `.Free`/`.Destroy`, and exclude identifiers that are the sole argument of `Assigned` or an operand of a comparison with `nil` (see `collect_node_effects` in use_after_free.rs). After `X.Free` (dangling, non-nil) `Assigned(X)` should still report.
<!-- SECTION:DESCRIPTION:END -->
