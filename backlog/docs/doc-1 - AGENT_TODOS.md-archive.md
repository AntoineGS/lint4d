---
id: doc-1
title: AGENT_TODOS.md archive
type: other
created_date: '2026-10-02 23:28'
updated_date: '2026-10-02 23:28'
---
Converted into Backlog.md tasks on 2026-10-02. Tasks carry the `agent-todos` label. New work goes in the backlog, not in a separate TODO file.

## Item to task mapping

| Original item (first line) | Backlog |
|---|---|
| Treat parameters of `interface`/forward-declared routines and class | TASK-75 (Done) |
| Resolve case-insensitive project paths without listing every directory | TASK-76 (Done) |
| Reuse project discovery for sibling sources within one request, with | TASK-77 (Done) |
| Stop `workspace/diagnostic` discovery at the first source with an | TASK-78 (Done) |
| Share one `ContextState` per context across document owners | TASK-79 (Done) |
| `sourcePaths` such as `C:/DelphiSources/rtl/sys` were joined onto the | TASK-80 (Done) |
| `ReadPolicy::new_with_installation_roots` (`pascal-project`) has the | TASK-81 (To Do) |
| Workspace-wide indexing phase still grows RSS by ~3 GB on `multidev` | note on TASK-10 (LSP-4) |
| Find out why many sources still miss discovery reuse (profiling at | TASK-82 (To Do) |
| Decide what workspace-wide requests should do on very large or | TASK-83 (To Do) |
| Scheduling: long workspace requests can occupy all analysis workers, | note on TASK-8 (LSP-9) |
| Add cancellation checks inside `resolve_existing_path_status`; a | TASK-84 (To Do) |
| Merge `fix/discovery-reuse` into master (fast-forward to `5bdf66b`, | TASK-85 (Done) |
| With the user's OK, drop the redundant agent stashes (fixes #1+#2 | note on TASK-1 (BUILD-6) |
| Merge `fix/semantic-tokens-includes` into master (fast-forward to | TASK-86 (Done) |
| Failing on `master` (`2b75a51`) in every full run so far, unrelated to | TASK-1.1 (subtask of TASK-1) |
| Timing-sensitive protocol tests fail under machine load on `master` | TASK-1.2 (subtask of TASK-1) |
| Treat C++Builder header, linker and layout directives (`NOINCLUDE`, | TASK-87 (Done) |
| Standalone sources inside exactly one configured installation's `BDS` | TASK-88 (Done) |
| Grammar gaps in `../tree-sitter-pascal` exposed by Classes.pas: | TASK-89 (Done) |
| lint4d's bare `raise;` ERROR suppression (`is_bare_raise_error` in | TASK-90 (To Do) |
| The server treats `file:///...%40...` and `file:///...@...` as | TASK-91 (To Do) |
| Find out why the user's nvim log shows frequent `analysis result became | TASK-92 (To Do) |
| Inherited owners: go to definition into an RTL unit did not leave | TASK-93 (To Do) |

## Original file (verbatim)

````markdown
# Agent TODOs

Owned and maintained by the coding agent. The user's own list is `TODOS.md`.

## Workspace-Wide Performance

Context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide
requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on
public symbols) burn a core for 8+ minutes and then fail, because the snapshot
is always incomplete. Neovim sends `workspace/diagnostic` automatically.
Done items below are merged into master (`05c2c79`..`5bdf66b`).

- [x] Treat parameters of `interface`/forward-declared routines and class
      methods as local, so references, rename and highlighting stay in the
      file. (`05c2c79`)
- [x] Resolve case-insensitive project paths without listing every directory
      from `/` per path: exact-path fast path plus a per-discovery listing
      cache, shared via `Arc`. (`1645002`)
- [x] Reuse project discovery for sibling sources within one request, with
      freshness checks, and stop pruning contexts once per file. (`baf5774`;
      still misses for many files, see below)
- [x] Stop `workspace/diagnostic` discovery at the first source with an
      incomplete project context (`build_rejectable_workspace_snapshot`).
      On `multidev` it now fails in ~1 s instead of 8+ minutes. The
      `references`/`rename` early exit for an incomplete requested-file
      context already existed. (`41876a3`)
- [x] Share one `ContextState` per context across document owners
      (`Arc`, reused only while equal to the current context, released with
      the last owner), and stop deep-cloning contexts per source in
      `discover_enumerated_contexts`. RSS at 30 s: 2.4 GB -> 1.2 GB; at
      270 s: 6.2 GB -> 4.8 GB (references on a public symbol). Remaining
      growth is the indexing phase. (`20f25ad`)
- [x] `sourcePaths` such as `C:/DelphiSources/rtl/sys` were joined onto the
      workspace root by the workspace walker (`.../multidev/C:/DelphiSources/
      ...`), so every workspace-wide snapshot on the user's setup was
      incomplete before discovery started. On non-Windows hosts the walker
      now skips drive-letter/UNC entries; project discovery already maps
      them per installation and `enumerate_mapped_sources` walks them per
      context. `workspace/diagnostic` on `multidev` now reports the real
      reason (an ambiguous project context) in 0.8 s. (`5bdf66b`)
- [ ] `ReadPolicy::new_with_installation_roots` (`pascal-project`) has the
      same `is_absolute()` check and adds `<root>/C:/...` as a configured
      read root. Harmless (the directory never exists), but inconsistent.
- [ ] Workspace-wide indexing phase still grows RSS by ~3 GB on `multidev`
      for references on a public symbol; consider bounding it.
- [ ] Find out why many sources still miss discovery reuse (profiling at
      120 s and 300 s still shows per-file `build_project_context`).
- [ ] Decide what workspace-wide requests should do on very large or
      partially evaluable repos: partial results marked incomplete, treating
      unknown projects differently, or making `workspace/diagnostic` opt-in.
      Today references/rename on public symbols cannot work on `multidev`.
- [ ] Scheduling: long workspace requests can occupy all analysis workers,
      including the slot reserved for interactive requests (`prepareRename`
      counts as interactive). Neovim never cancels superseded requests, so
      repeated keypresses pile up. Consider classifying workspace-mode
      `prepareRename` as bulk, cancelling superseded duplicates, and a work
      budget per request.
- [ ] Add cancellation checks inside `resolve_existing_path_status`; a
      cancelled request can keep a worker busy for up to ~5 s.
- [x] Merge `fix/discovery-reuse` into master (fast-forward to `5bdf66b`,
      history rewritten from the `wip:` checkpoints).
- [ ] With the user's OK, drop the redundant agent stashes (fixes #1+#2
      and the two temporary barrier baseline diffs). The agent worktrees and
      branches are already gone.
- [x] Merge `fix/semantic-tokens-includes` into master (fast-forward to
      `575e497`; release binary not rebuilt yet): semantic tokens of a source
      with a resolved `{$I ...}` were encoded against the include-expanded
      text, painting `.inc` comments over the file (MDIBDatabase.pas with
      `MDDatabaseXE3.dproj` selected). Tokens are now mapped back through the
      expansion source map.
- [ ] Failing on `master` (`2b75a51`) in every full run so far, unrelated to
      the above:
      `selection_ranges_keep_line_comment_ranges_valid_for_lf_and_crlf`,
      `selection_ranges_keep_non_bmp_comment_endpoints_and_mixed_positions_valid`,
      `selection_ranges_accept_crlf_line_comments_in_mixed_position_batches`,
      `assistance_retains_lazy_package_project_observation_at_the_read_boundary`,
      `neovim_standard_symbol_reference_and_highlight_queries`,
      `code_action_creation_rejects_overlong_identity_and_caps_serialized_output`,
      `sixty_four_project_metadata_changes_share_the_notification_budget_and_stale_pull_results`,
      `type_hierarchy_candidate_cap_refuses_partial_subtypes`. Clippy on the
      current toolchain also fails in `pascal-core`, `lint4d` and `pascal-lsp`.
- [ ] Timing-sensitive protocol tests fail under machine load on `master`
      too: `shutdown_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline`
      and `cancel_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline`
      (750 ms shutdown window). Consider making them robust to load.
      `configuration_revalidation_readers_do_not_block_on_fifo_replacement`
      takes 2.9-4.2 s alone against its 5 s deadline and fails under the
      full parallel suite.

## Delphi RTL Units (Classes.pas)

Context: Classes.pas from the D2010 installation reported "include expansion
was incomplete; lint diagnostics were withheld" and navigation inside it was
unavailable.

- [x] Treat C++Builder header, linker and layout directives (`NOINCLUDE`,
      `EXTERNALSYM`, `HPPEMIT`, `NODEFINE`, `WEAKPACKAGEUNIT`, `{$L file}`,
      `ALIGN`, `MINENUMSIZE`, ...) as harmless; `SCOPEDENUMS` and
      `POINTERMATH` stay unsupported. (`fb472e0`)
- [x] Standalone sources inside exactly one configured installation's `BDS`
      tree take that installation's compiler version and profile properties
      (`Platform`). Classes.pas now gets 150 lint diagnostics. (`09eb6d8`,
      merged in `04c5ec9`)
- [x] Grammar gaps in `../tree-sitter-pascal` exposed by Classes.pas:
      subrange constant-expression bounds, `raise E at ReturnAddr`, and
      routine attribute keywords (`Default`, `Local`, ...) as variable
      names. Merged in tree-sitter-pascal `2167e95`; Classes.pas has no
      parse errors with it. Generate with tree-sitter-cli 0.24.7 (the
      locked version): the system 0.26.9 CLI rewrites every generated file.
- [ ] lint4d's bare `raise;` ERROR suppression (`is_bare_raise_error` in
      `pascal-core/src/parser.rs`) is stale: the grammar parses bare
      `raise;` without errors.
- [ ] The server treats `file:///...%40...` and `file:///...@...` as
      different documents: an open document whose URI encodes `@` is reported
      as "disappeared" by `dependency_scoped_result_is_fresh`, so every pull
      is stale. nvim sends a raw `@`, so it is unaffected.
- [ ] Find out why the user's nvim log shows frequent `analysis result became
      stale` for semantic tokens and `include source-map work limit (1000000)
      reached`.
- [ ] Inherited owners: go to definition into an RTL unit did not leave
      the unit with its importer's project context when opened afterwards
      (not reproduced end to end; definition requests were stale in the
      probe because of the URI issue above).
````
