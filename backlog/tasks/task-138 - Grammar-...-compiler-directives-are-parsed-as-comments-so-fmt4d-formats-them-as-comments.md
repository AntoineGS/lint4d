---
id: TASK-138
title: >-
  Grammar: (*$ ... *) compiler directives are parsed as comments, so fmt4d
  formats them as comments
status: To Do
assignee: []
created_date: '2026-10-05 00:57'
labels:
  - fmt
  - grammar
  - correctness
milestone: m-3
dependencies:
  - TASK-137
priority: medium
type: bug
ordinal: 140000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in review of TASK-137 (branch fix/fmt-uses-include-punctuation); pre-existing.

crates/tree-sitter-pascal parses the '(*$ ... *)' spelling of compiler directives as a 'comment' node, not a 'ppDirective' (and '(*$IFDEF X*)' ... '(*$ENDIF*)' is not a conditional). pascal-project (lib.rs ~4404, ~9618), cfg-pascal (prepared.rs ~417, ~574) and pascal-lsp already treat '(*$' as a directive textually; fmt4d relies on the tree, so it handles these as comments.

Consequence in uses clauses: a '(*$I u.inc*)' is not recognised as an include (crates/fmt4d/src/uses.rs is_include_directive only matches '{$'), attaches to a unit as a comment and moves with it under sorting:
  uses B, A, (*$I u.inc*), C;  ->  'A, (*$I u.inc*)' / 'B,' / 'C;'
which reads 'uses A, U B, C;' if u.inc lists U. Elsewhere, conditional blocks in this spelling are laid out as comments around code, not as pp blocks.

To do: decide whether the grammar should produce ppDirective / pp conditional nodes for '(*$' (scanner + grammar.js, regenerate with tree-sitter CLI 0.24 per CLAUDE.md, then check every consumer that matches on comment vs directive), or whether fmt4d should reclassify such comments itself. Then let is_include_directive accept the spelling. Tests first: grammar corpus tests for '(*$I x*)' and '(*$IFDEF X*)...(*$ENDIF*)'; fmt_bugs_test.rs for the uses-clause shape above (sorting on), parse and idempotency.
<!-- SECTION:DESCRIPTION:END -->
