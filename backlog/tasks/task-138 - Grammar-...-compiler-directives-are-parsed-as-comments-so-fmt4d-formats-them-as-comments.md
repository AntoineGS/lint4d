---
id: TASK-138
title: >-
  Grammar: (*$ ... *) compiler directives are parsed as comments, so fmt4d
  formats them as comments
status: Done
assignee:
  - '@claude'
created_date: '2026-10-05 00:57'
updated_date: '2026-10-05 13:23'
labels:
  - fmt
  - grammar
  - correctness
milestone: m-3
dependencies:
  - TASK-137
modified_files:
  - crates/tree-sitter-pascal/grammar.js
  - crates/tree-sitter-pascal/src/grammar.json
  - crates/tree-sitter-pascal/src/parser.c
  - crates/tree-sitter-pascal/src/scanner.c
  - crates/tree-sitter-pascal/test/corpus/preprocessor.txt
  - crates/pascal-core/src/directive_fragment_rewrite.rs
  - crates/cfg-pascal/src/pascal_builder.rs
  - crates/cfg-pascal/tests/structured_flow_test.rs
  - crates/fmt4d/src/uses.rs
  - crates/fmt4d/tests/fmt_bugs_test.rs
  - crates/fmt4d/tests/fmt_roundtrip_test.rs
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

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Decision: fix the grammar (root cause; CLAUDE.md: fix the grammar directly rather than work around it). Delphi and FPC read '(*$ ... *)' exactly like '{$ ... }'. No file in the repo or the fmtrev corpus uses the spelling, so the parse impact on real code is nil; the fix is for correctness.

Branch fix/grammar-paren-star-directives.

1. crates/tree-sitter-pascal/grammar.js: every directive token accepts both spellings: ppDirective, _ppIf/_ppElse/_ppEndIf and the inline ppIf/ppElse/ppEndIf tokens of ppUsesBlock / ppUsesBlockWithSemi. The '(*' comment alternative no longer matches '(*$' (so '(**)' and '(**$x*)' stay comments). Regenerate with tree-sitter CLI 0.24.7 (npx), commit src/.
2. Out of scope, unchanged: the external scanner's single-line fragments (ppFragmentExpr/Stmt) and pascal-core's directive_fragment_rewrite stay '{$'-only. A '(*$IFDEF*)' fragment in an expression position lexes as a ppDirective extra, as it did as a comment.
3. Consumers that read directive text: cfg-pascal is_unconditional_preprocessor_else strips '{$' / '}' only, so '(*$ELSE*)' would not count as an else: strip '(*$' / '*)' too. fmt4d uses.rs is_include_directive accepts '(*$'. Audit other text parsers of ppIf/ppElse/ppEndIf/ppDirective nodes.
4. Tests first: grammar corpus tests in test/corpus/preprocessor.txt for '(*$I x*)' as ppDirective, '(*$IFDEF X*)...(*$ELSE*)...(*$ENDIF*)' as a ppBlock (declarations) and in a uses clause as ppUsesBlock, and '(**)' still a comment; fmt_bugs_test.rs: 'uses B, A, (*$I u.inc*), C;' keeps its order and punctuation (sorting on and off), parses, idempotent; a cfg-pascal test that a '(*$ELSE*)' branch counts as an else.
5. Verify: tree-sitter test, cargo test --workspace, clippy, fmt, fmt corpus gate.

Revision (differential probe before commit): with the grammar alone, single-line conditional fragments in the (*$ spelling stop parsing where they used to pass as comments: the external scanner (ppFragmentExpr/Stmt) and pascal-core directive_fragment_rewrite (partial if/while headers, opaque {$IF} blocks) only know '{$'. E.g. '(*$IFNDEF X*) if c then (*$ENDIF*)' and '(*$IFDEF X*)System.(*$ENDIF*)SysUtils' went from formatted to parse error. So step 2 is in scope after all: both learn the (*$ ... *) spelling. Oracle: fmt_roundtrip_test every_fixture_formats_alike_in_either_directive_spelling formats every fixture holding '{$' as is and respelled '(*$x*)', and requires the same output after respelling back (RED now on the 11 ppFragment fixtures). Also uses.rs wrote a synthetic '{$ELSE}' for a uses block's else branch; it now keeps the source text (IfDefBlock.else_directive), so a lowercase '{$else}' is kept as written, like '{$ifdef}' and '{$endif}' already were.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED:
- tree-sitter test: new corpus cases 'Parenthesis-star directives' and '... in uses clause' failed on master (comments instead of ppDirective / ppBlock / ppUsesBlock); 'Parenthesis-star directive fragments' failed with the grammar change but the old scanner.
- fmt_bugs_test paren_star_directives_in_a_uses_clause_are_directives failed on master ('uses B, A, (*$I u.inc*), C;' -> 'A, (*$I u.inc*)' / 'B,' / 'C;'); with the grammar alone it showed the synthetic '{$ELSE}' in a uses block.
- fmt_roundtrip_test every_fixture_formats_alike_in_either_directive_spelling failed on master and with the grammar alone (11 ppFragment fixtures).
- cfg-pascal structured_flow_test preprocessor_else_closes_the_no_match_path_in_either_spelling failed with the grammar change ('(*$ELSE*)' not seen as an else, so code after a conditional whose branches all exit stayed reachable). It passes on master only because the directives were comments there.
- pascal-core directive_fragment_rewrite rewrite_paren_star_spelling / f_u10_paren_star_spelling failed.
Change:
- grammar.js: ppKeyword() builds the ppIf/ppElse/ppEndIf tokens for both spellings (shared by _ppIf/_ppElse/_ppEndIf and the ppUsesBlock tokens). ppDirective accepts '(*$...*)'. The '(*' comment no longer matches '(*$'. Regenerated with CLI 0.24.7.
- scanner.c: fragments open and nest in either spelling (skip_to_directive_end).
- pascal-core directive_fragment_rewrite: directive_keyword_start / find_directive_end for both spellings; '(*$' is not skipped as a comment, and a trailing '(*$...*)' is not trimmed as one.
- cfg-pascal is_unconditional_preprocessor_else strips either spelling.
- fmt4d uses.rs: is_include_directive accepts '(*$'; IfDefBlock.else_directive keeps the else text instead of writing '{$ELSE}'. Slot::Pinned boxes its item (clippy large_enum_variant after the new field).
GREEN: tree-sitter test 174/174, examples parse as before (same two pre-existing ERRORs); cargo test --workspace --no-fail-fast 3619 passed / 0 failed / 9 ignored; clippy --workspace --all-targets -D warnings and fmt --check clean. fmt corpus gate (348 files, base = master fmt4d): no output differences, none non-idempotent. Spelling differential over the 83 corpus files that hold '{$': 82 identical, 1 differs only because the longer spelling pushes a line past 120 columns (x_DUnitX.Exceptions.pas).
Not covered, unchanged: textual scanners that only know '{$' (pascal-core conditional.rs directive collection, pascal-core directives.rs {$FMT.OFF}/{$FMT.ON}, pascal-lsp queries.rs include expansion from the raw directive).

Review round 1 (code-reviewer subagent on 49e6365..b9b67a6): one Important finding. The fragment scanner now counted '(*$ENDIF*)' text inside a string or comment within a '{$' fragment as a directive, so 'a := {$IFDEF X}b { (*$ENDIF*) } {$ELSE}c{$ENDIF};' and 'a := {$IFDEF X}'(*$ENDIF*)'{$ELSE}'b'{$ENDIF};' went from parsing to ERROR (fmt4d refused the file). The old scanner had the same blind spot for '{$...}' text in strings. RED: corpus case 'Directive text in strings and comments inside fragments' (both spellings, plus '{$ENDIF}' in a string). Fix (scanner.c): the fragment walk skips strings, '//' comments and '{ }' / '(* *)' comments (skip_to_close, noting line breaks), so neither directives nor ';' inside them count. GREEN: tree-sitter test 175/175; cargo test --workspace 3619/0/9; clippy and fmt clean; fmt corpus gate vs master: no differences, none non-idempotent; spelling differential unchanged (82/83, the other a line-width break).

Merged into master by fast-forward to bde03da (branch fix/grammar-paren-star-directives, 2026-10-05). Merged tree verified: cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean; cargo test --workspace --no-fail-fast 3619 passed / 0 failed / 9 ignored; tree-sitter test 175/175.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: the grammar parsed the '(*$ ... *)' spelling of compiler directives as comments. fmt4d therefore sorted a '(*$I u.inc*)' along with a unit in a uses clause, '(*$IFDEF X*)...(*$ENDIF*)' was not a conditional anywhere, and the CFG treated such code as plain statements.

Fix (branch fix/grammar-paren-star-directives, commits b9b67a6 and 9c0264e): the grammar's directive tokens (ppDirective, ppIf/ppElse/ppEndIf, the ppUsesBlock tokens) accept both spellings, and the '(*' comment no longer matches '(*$'. The external scanner's single-line fragments and pascal-core's fragment rewrite also accept both spellings. The scanner now skips strings and comments inside a fragment, which also fixes '{$...}' text inside a string ending a fragment early. cfg-pascal recognises '(*$ELSE*)'. fmt4d treats '(*$I file*)' as an include, and a uses block keeps its else directive's own text (a lowercase '{$else}' is no longer uppercased).

Tests: grammar corpus (4 cases); fmt_bugs_test paren_star_directives_in_a_uses_clause_are_directives; fmt_roundtrip_test every_fixture_formats_alike_in_either_directive_spelling (every fixture with directives formats the same in both spellings); cfg-pascal preprocessor_else_closes_the_no_match_path_in_either_spelling; pascal-core rewrite tests.

Verification: tree-sitter test 175/175; cargo test --workspace 3619 passed, 0 failed, 9 ignored; clippy -D warnings and fmt clean; fmt corpus gate against master shows no output differences and nothing non-idempotent.

Not covered: textual scanners that only know '{$': pascal-core conditional.rs, the {$FMT.OFF}/{$FMT.ON} detection, and pascal-lsp include expansion.
<!-- SECTION:FINAL_SUMMARY:END -->
