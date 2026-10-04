---
id: TASK-116
title: >-
  unchecked-nil: an unanalyzable function call leaves a 'can return nil'
  sentinel, so the second call to it is flagged
status: To Do
assignee: []
created_date: '2026-10-03 22:34'
labels:
  - lint
  - correctness
dependencies:
  - TASK-21
priority: medium
type: bug
ordinal: 118000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/lint4d/src/rules/nil_check.rs can_function_return_nil inserts a recursion sentinel (true = can return nil) into FN_RETURN_CACHE before looking the function up, and only overwrites it on the success path. Every None exit (function not defined in this file, a procedure, overloads that disagree since TASK-21) leaves the sentinel behind, so the first call is treated as safe and every later call to the same name in the file is treated as "can return nil".

Repro (unchecked-nil enabled, project with System.TObject as a class, as in crates/lint4d/tests/rules_nil_check_test.rs):

    procedure Test;
    var Obj: TObject; Obj2: TObject;
    begin
      Obj := ExternalGet(1);
      Obj.ClassName;      // not flagged
      Obj2 := ExternalGet(2);
      Obj2.ClassName;     // flagged (line 14) - inconsistent
    end;

Probed on branch fix/cfg-overload-identity (TASK-21); the not-found path is unchanged from master. Fix idea: remove the sentinel (or cache the None outcome) on every None exit; regression test with two calls to an external function and to disagreeing overloads.
<!-- SECTION:DESCRIPTION:END -->
