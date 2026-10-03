---
id: TASK-109
title: URI canonicalisation edge cases in the transport spelling table
status: To Do
assignee: []
created_date: '2026-10-03 03:37'
labels:
  - lsp
dependencies:
  - TASK-91
priority: medium
ordinal: 111000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-up from the TASK-91 review (branch fix/uri-at-sign, crates/pascal-lsp/src/server/uri_spelling.rs, canonical_file_uri in crates/pascal-lsp/src/workspace.rs). Not fixed in TASK-91:

(a) canonical_file_uri does not normalise the Windows drive-letter case. VS Code sends file:///c%3A/..., which canonicalises to file:///c:/..., while source records built from disk paths may spell C:. Check on Windows whether lookups still mismatch, and normalise the drive letter if they do.
(b) The url crate's file_url_segments_to_pathbuf decodes whole segments. file:///a%2Fb/X.pas therefore canonicalises to file:///a/b/X.pas, which aliases a different document (%5C on Windows likewise). A malformed %zz becomes %25zz. Decide whether such URIs should pass through unchanged instead of being canonicalised.
(c) Canonicalisation strips trailing slashes from folder URIs (rootUri, workspaceFolders, scopeUri). That records spellings even for clients that never percent-encode, so the writer walks every outbound message. Avoid recording when the only difference is a trailing slash, or treat that as canonical.
(d) The writer thread walks outbound JSON trees while holding the mutex the reader thread needs. Consider copying the needed entries out, using an RwLock, or a cheaper pre-check so inbound reads are not serialised behind large outbound responses.
<!-- SECTION:DESCRIPTION:END -->
