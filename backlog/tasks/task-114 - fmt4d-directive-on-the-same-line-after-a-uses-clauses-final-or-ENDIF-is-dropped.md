---
id: TASK-114
title: >-
  fmt4d: directive on the same line after a uses clause's final ; or {$ENDIF} is
  dropped
status: To Do
assignee: []
created_date: '2026-10-03 22:18'
labels:
  - fmt
  - correctness
dependencies: []
priority: high
ordinal: 116000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while working on TASK-104 (pre-existing; not caused by the uses-clause punctuation fixes).

Repro (default config):
```pascal
unit T;
interface
uses A; {$I d.inc}
implementation
end.
```
Output: `uses\n  A;\n\nimplementation` — the `{$I d.inc}` is gone. Same for a directive after the {$ENDIF} of a clause that is one ppUsesBlockWithSemi (`uses {$IFDEF X} A; {$ELSE} C; {$ENDIF} {$I d.inc}`). On its own line after the clause the directive is kept.

Likely cause: DirectiveMap attaches a same-line directive as trailing trivia of the clause's last leaf (`;` or ppEndIf). build_uses (crates/fmt4d/src/doc_builder.rs, fn build_uses) rebuilds the clause from UsesItem values and only reads comments from CommentMap (uses.rs punctuation_texts / trailing_texts), never trailing directives of leaves inside the clause, so the directive has no emission path. Dropping an {$I} silently changes the program.

Expected: the directive is kept (e.g. on the `;` line or the next line), output parses and is idempotent; a regression test in fmt_bugs_test.rs and the round-trip oracle (which compares directive texts) cover it.
<!-- SECTION:DESCRIPTION:END -->
