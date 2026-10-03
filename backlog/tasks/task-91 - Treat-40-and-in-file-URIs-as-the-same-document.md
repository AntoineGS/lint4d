---
id: TASK-91
title: Treat %40 and @ in file URIs as the same document
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:28'
updated_date: '2026-10-03 21:55'
labels:
  - agent-todos
  - rtl-units
  - lsp
dependencies: []
modified_files:
  - crates/pascal-lsp/src/server.rs
  - crates/pascal-lsp/src/server/uri_spelling.rs
  - crates/pascal-lsp/tests/protocol.rs
priority: medium
type: bug
ordinal: 93000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Delphi RTL Units (Classes.pas)".

Section context: Classes.pas from the D2010 installation reported "include expansion was incomplete; lint diagnostics were withheld" and navigation inside it was unavailable.

Original item:

The server treats `file:///...%40...` and `file:///...@...` as
different documents: an open document whose URI encodes `@` is reported
as "disappeared" by `dependency_scoped_result_is_fresh`, so every pull
is stale. nvim sends a raw `@`, so it is unaffected.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/uri-at-sign, branch fix/uri-at-sign.
Root cause (hypothesis): open_documents and related maps are keyed by the client's raw Url; SourceRecord URIs built from paths via Url::from_file_path spell '@' raw, so a client URI with %40 never matches (dependency_scoped_result_is_fresh reports "open document disappeared").
Steps:
1. RED protocol test: workspace dir containing '@', open Main/Provider with %40-encoded URIs, pull diagnostics + definition must succeed (not stale), responses use the client's URI string.
2. Fix at the boundary: one canonical form for file URIs (path-roundtrip so percent-encoding of unreserved/sub-delim chars is normalised), keeping the client's URI for responses.
3. Unit test for the canonicalisation.
4. fmt, clippy -p pascal-lsp, lib tests + URI/path/document protocol tests.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Root cause: the workspace keys open documents by the client's URI string, while SourceRecord URIs are canonical (LspSourceStore::from_input maps overlays through canonical_file_uri, crates/pascal-lsp/src/workspace/resolver.rs:95). A client URI with %40 never equals the canonical '@' spelling, so dependency_scoped_result_is_fresh (crates/pascal-lsp/src/workspace.rs:12966) found no open document and every pull/definition was stale. RED: percent_encoded_at_sign_uri_is_the_same_open_document failed on 81418f2 with -32802 "analysis result became stale; retry the request".

Ruling: canonicalise at the stdio transport (reader thread) and restore the client's spelling in the writer thread, via a bounded canonical->client spelling table (crates/pascal-lsp/src/server/uri_spelling.rs) — one canonical form everywhere internally, no per-call-site special cases, and clients still get back the URI string they sent. Only string values under keys ending in "uri" are canonicalised inbound (never document text); outbound also restores DocumentLink "target" and WorkspaceEdit "changes" keys. The latest client spelling for a canonical URI wins. This also covers VS Code's %3A drive-letter spelling on Windows (not tested on Windows).

Ruling: in-memory connections used by lib unit tests bypass the transport and are unaffected; the boundary is bounded_stdio, which is what real clients and the protocol tests use.

Implemented on branch fix/uri-at-sign (worktree .worktrees/uri-at-sign), awaiting review and merge.

Fix round 1 (review "Needs fixes"):
1. is_uri_key sliced the key at a byte offset and panicked on non-ASCII keys such as "éé" or "a😀". That killed the transport thread for any setting or data map with such a key. It now compares bytes. RED: unit test uri_keys_are_matched_by_bytes_without_char_boundary_panics panicked with "start byte index 1 is not a char boundary". Commit e849832.
2. relatedDocuments (document diagnostic report, built at server.rs:9777-9806) is a map keyed by canonical URIs. It now gets its keys restored like WorkspaceEdit changes (URI_KEYED_MAPS). Grepping for other outbound URI-keyed maps found only changes and relatedDocuments. RED: protocol test percent_encoded_at_sign_related_document_reports_use_the_client_uri returned the key file:///.../user@host/Shared.inc. A unit test was added as well. Commit 5db0736.
3. Edge cases (Windows drive case, %2F/%5C/%zz decoding, trailing-slash folder URIs, writer holding the mutex) are recorded as a follow-up task, not fixed here.

Merged into master as 82f5767 (no-ff), 2026-10-03. Merged master tree is identical to the verified integration tree: cargo fmt --check clean, clippy --workspace --all-targets -D warnings clean, cargo test --workspace 3256 passed / 0 failed / 9 ignored.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: a client that percent-encodes '@' (file:///...%40...) got every pull diagnostic and navigation request rejected as stale. Open documents were keyed by the client's spelling, while source records use the canonical URI.

Change on fix/uri-at-sign:
- fcc48dc adds crates/pascal-lsp/src/server/uri_spelling.rs. The stdio reader thread canonicalises every file URI under a "*uri" key (canonical_file_uri) and records the client's spelling in a bounded LRU table (8 MiB). The writer thread restores the client spelling in outgoing "*uri" values, DocumentLink target, and the keys of URI-keyed maps. Wired in bounded_stdio.
- Fix round 1, e849832: URI keys are matched by bytes, so non-ASCII object keys no longer panic the transport threads.
- Fix round 1, 5db0736: relatedDocuments map keys are restored like WorkspaceEdit changes.

Tests:
- Protocol tests percent_encoded_at_sign_uri_is_the_same_open_document (RED on 81418f2: -32802 stale) and percent_encoded_at_sign_related_document_reports_use_the_client_uri (RED: related key spelled with a raw '@').
- Unit tests server::uri_spelling::tests (4): canonicalisation, text untouched, restore, latest spelling wins, boundedness, non-ASCII keys, relatedDocuments.
- pascal-lsp lib tests (--features test-support): 692 passed, 1 ignored. Protocol uri/path/document filters passed in round 0. fmt is clean.

Known limits and follow-up: URIs the client never mentioned are emitted in canonical form. TASK-109 tracks the Windows drive-letter case, %2F/%5C/%zz decoding, trailing-slash folder URIs and mutex contention.

Pre-existing on master: pascal-lsp clippy collapsible_if at server.rs:373 and :6660, and lib tests that do not compile without test-support (unused should_recover, workspace.rs:15787).
<!-- SECTION:FINAL_SUMMARY:END -->
