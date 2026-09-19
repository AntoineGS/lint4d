# Task 17 independent spec and quality review

Date: 2026-09-17  
Reviewed worktree: `/home/antoinegs/gits/lint4d/.worktrees/full-lsp`  
Reviewed HEAD: `bd409d549039f019e1d4c62363c22f8f34666296`  
Base: `9c9a522`

## Verdict

**Spec: CHANGES REQUIRED. Quality: CHANGES REQUIRED.**

The implementation substantially covers negotiation, real deferred rendering,
bounded server-owned identity state, stable-field restoration, and scheduler
integration. However, two independently reproduced freshness defects can return
metadata for a different declaration while retaining the original label and edit.
One defect also changes the shared source-validation invariant used outside
completion. These violate the exact-identity and stale-validation requirements.

Required test coverage is also incomplete. The implementation report explicitly
does not preserve historical pre-implementation RED evidence; that is a process
verification limitation, not evidence that RED occurred and not something this
review's later probes can repair retroactively.

## Findings

### F1 — P1: Bind the rebuilt resolve snapshot to the original observations

**Locations:**

- `crates/pascal-lsp/src/workspace/queries.rs:216-225`
- `crates/pascal-lsp/src/workspace/queries.rs:257-258,279-289`
- Related final validation: `crates/pascal-lsp/src/server.rs:2112-2117`

`completion_metadata_from_input` validates the original completion's records,
then builds a new assistance snapshot. The returned records and the final worker
validation describe only that new snapshot. Nothing checks that the new snapshot
consumed the same source observations as the original completion. A provider
write between the first validation and snapshot construction therefore becomes
the new accepted baseline. Matching only declaration URI/index is insufficient:
the same index can now belong to another declaration.

**Reproduced with the unchanged debug binary and a debugger-controlled file-write
interleaving:**

1. `Main.pas` imports `Provider` and requests completion at `Doc`.
2. Provider exports documented `function DocOld: Integer;`. Retain its issued item.
3. Send `completionItem/resolve`.
4. Pause at `queries.rs:220`, after original-record validation but before the
   rebuilt snapshot. Change the provider to `DocNew` with different documentation,
   keeping the declaration at the same index. Do not send a watcher notification.
5. Continue the worker.

The server **successfully returns**, rather than rejecting as stale:

```json
{
  "label": "DocOld",
  "textEdit": {"newText": "DocOld", "range": {"start": {"line": 6, "character": 2}, "end": {"line": 6, "character": 5}}},
  "detail": "function DocNew: Integer;",
  "documentation": {"kind": "markdown", "value": "NEW DOCUMENTATION."}
}
```

No timestamp restoration is necessary for this reproduction. A normal provider
write during resolve is sufficient; the debugger only makes the timing
deterministic. A no-write control returns `DocOld` metadata correctly.

**Requested change:** Validate the rebuilt snapshot's consumed observations
against the retained original identity, rather than simply accepting fresh
records after an earlier check. Keep the original dependencies involved in final
worker validation as well. The check must bind metadata to the original source
revision, not merely perform a new same-index lookup. Add an in-flight disk-change
test at this boundary that expects conservative rejection and no mixed item.

### F2 — P1: Do not replace parsed-source equality with a separately sampled disk hash

**Locations:**

- `crates/pascal-lsp/src/workspace/rename.rs:3074-3103,3112-3123`
- `crates/pascal-lsp/src/workspace/rename.rs:851-859`
- Parallel validation change: `crates/pascal-lsp/src/workspace/rename.rs:777-781`
- Hash sampling: `crates/pascal-lsp/src/workspace/rename.rs:3417-3457`

For an imported dependency, `record.text` comes from the already-loaded index.
The added assignment at line 3121 attaches a hash obtained by separately reading
the disk later. The modified validators now use **hash instead of text equality**
whenever a hash is present. Previously the full-record validation required both
the decoded source equality and any supplied content-hash equality.

If the file changes after parsing but before the later fingerprint, the record
can contain old parsed text and a hash of new bytes. With unchanged size/mtime,
the modified validator accepts this inconsistent record. The new hash does not
prove that the current disk file is the source actually used for analysis.

**Independent reproduction, during initial completion rather than resolve:**

1. Use the same two-file fixture as F1.
2. Pause the initial completion at `rename.rs:3112`, after provider source was
   obtained from the loader's index and before the later baseline hash read.
3. Replace `DocOld`/`OLD DOCUMENTATION` with equal-length
   `DocNew`/`NEW DOCUMENTATION` in the temporary fixture, preserving its mtime.
4. Continue and disable the breakpoint. Make no further writes during resolve.

Initial completion incorrectly succeeds with `DocOld`; subsequent resolve also
succeeds and returns the mixed `DocOld` label/edit plus `DocNew` metadata shown
above. This does **not** depend on F1's resolve-time interleaving: the inconsistent
baseline has already been accepted and stored by the initial completion.

**Requested change:** Keep full-source records' existing text-equality validation
in addition to raw-byte checks. Distinguish compact hash-only observations from
full records explicitly, or ensure every content identity is captured from the
same read that supplies the parsed source. Do not transplant a later disk hash
onto earlier parsed text and treat it as sufficient proof. Preserve the existing
same-stamp-change defenses for shared rename/reference consumers.

The runtime probe exercised completion. The impact on shared validation is
established by the common code change; this review does not claim to have run a
separate rename-edit reproduction. Add shared-validator regression coverage as
well as a completion identity test before accepting this change.

### F3 — P2: Complete the feature-specific identity and stale-delivery test matrix

**Locations:** `crates/pascal-lsp/tests/protocol.rs:3569-4106`,
`crates/pascal-lsp/src/navigation/assistance.rs:4224-4262`, and
`crates/pascal-lsp/src/server.rs:4568-4635`.

The added protocol tests cover seven normal cases and one cancellation barrier.
They do not exercise deferred resolve for an exact overloaded declaration or a
helper declaration. The only new generic resolve case is a cross-unit generic
field. There is no feature-specific related-provider-overlay test or stale
non-cancelled resolve-worker delivery test. The deferred unit test asserts absent
fields, not instrumentation proving metadata rendering was skipped; code
inspection does show the rendering branches are skipped.

The store tests cover token/foreign/oversized rejection and entry-count eviction,
but not the byte-budget/context-observation boundaries. Existing navigation
tests are valuable for binding, but do not test the newly reconstructed identity
across completion and resolve. The two successful mixed-identity reproductions
demonstrate why the distinction matters.

**Requested change:** Add deterministic regressions for F1/F2 plus the explicitly
required overload/helper resolution, provider-overlay invalidation, stale worker
delivery, and byte/context bounds. Assert exact documentation/detail and stable
fields, not just successful responses or a same-name label. Retain independent
documentation/detail negotiation and plaintext/Markdown checks.

## Spec/quality assessment of the remaining implementation

| Area | Assessment |
| --- | --- |
| Standard capability and dispatch | `resolveProvider` is advertised; `completionItem/resolve` uses the analysis scheduler. |
| Negotiation | Documentation and detail are independently gated by `resolveSupport.properties`; unsupported fields are eager. The no-support path does not issue resolve tokens. |
| Actual deferral | `assistance.rs:417-436` skips declaration-display construction and documentation rendering for deferred fields. This is not merely constructing metadata and hiding JSON fields. Existing Task16 document-index documentation remains cached. |
| Rendering | Resolve shares `declaration_display` and bounded Task16 documentation rendering; it returns negotiated Markdown or plaintext. |
| Identity | Opaque session-local token/proof, strict data schema, server-owned request context, URI/index, and generations are retained. Client paths do not authorize reads. Recomputed binding uses the existing completion logic and generic substitution, but F1/F2 break its required source-revision premise. The report's “preserved exact seed” description should not imply the whole seed is retained: registration stores URI/index and resolve rederives the seed. |
| Stable fields and edits | `finish` clones the server's originally issued item and only fills negotiated metadata fields. Caller-provided label/edit/filter/sort/insert changes are not trusted. No new edits are introduced by this path. README documents this restoration policy. |
| Retained-state architecture | Fixed entry, total-accounting, data, item, dependency-record and context limits; shared contexts rather than per-item snapshots; oldest-entry eviction. The targeted store tests pass. This is bounded-accounting code, not a measured proof of exact allocator usage. |
| Ordinary freshness | Existing targeted tests pass for changed provider disk, changed requester overlay, project selection switch, and unrelated overlay acceptance. F1/F2 remain blocking exceptions. |
| Worker/cancellation | Interactive scheduling, bounded existing queues, worker-side parsing/I/O and validation, and cancellation integration follow existing conventions. The cancellation barrier returns once and passes. No new filesystem I/O on the protocol thread was identified in the added store/dispatch paths. |
| UTF-16/CRLF and case handling | Existing replacement-range and case-insensitive candidate-selection code is retained; resolve restores the original edit. No separate coordinate regression was identified by inspection. This review did not rerun all coordinate suites. |
| Ownership/read policy | Original observations retain authorization metadata; worker snapshot creation and payload checks reuse existing ownership/read policy. No client-path read authorization was added. |
| Scope | Changes are feature-related, but changes to shared rename validation require the stronger regression protection described in F2. No unrelated audit or source modifications were performed. |

## Verification performed in this review

Read the brief, implementation report, supplied review diff, actual source, and
feature tests. Confirmed the reviewed worktree HEAD and clean working state.
Did not rerun full protocol/workspace suites: the supplied report already records
fresh 345-protocol/384-barrier results and broader checks.

Focused existing tests run from the reviewed worktree:

```text
cargo test -p pascal-lsp --lib completion_resolution -- --nocapture
2 passed, 0 failed

cargo test -p pascal-lsp --test protocol_barriers --features test-support completion_resolution -- --nocapture
8 passed, 0 failed; 376 filtered out
```

External, read-only-to-the-repository probes:

```text
python /tmp/opencode/task17-review-probe.py
Control: DocOld resolves to DocOld / OLD DOCUMENTATION.

python /tmp/opencode/task17-review-probe.py --race
F1 reproduced: successful DocOld item with DocNew / NEW DOCUMENTATION.

python /tmp/opencode/task17-review-probe.py --hash-race
F2 reproduced: initial stale item accepted, then successfully resolved to mixed metadata.
```

Both defect probes were repeated successfully after the targeted test runs.
Latest fixture/debugger artifacts are under
`/tmp/opencode/task17-review-776tw1ye` (F1) and
`/tmp/opencode/task17-review-gf2nmqmx` (F2). The probe script, temporary fixtures,
and debugger command files are outside the repository. The debugger pauses only
at the identified boundaries; it does not patch executable instructions or
repository source. Fixture writes model concurrent disk changes.

These are post-implementation review reproductions, **not historical TDD RED
evidence**. No feature source/test/README files were edited and no commits were
created by this review.

---

## Review round 1 — correction `0d4ce7b`

Reviewed HEAD: `0d4ce7b0168ad2954940e19f5f2b0cfdbc151ade`  
Scope: `bd409d5..0d4ce7b`, the three prior findings, their regressions, and
new breakage attributable to this correction only.

### Round 1 gate

**Spec implementation gate: PASS. Quality gate: PASS.**

**F1: addressed. F2: addressed. F3: addressed.** No new blocking finding was
identified in this scoped correction review. This supersedes the original
changes-required verdict for the reviewed HEAD, without erasing the original
findings or their reproduction evidence.

The historical evidence limitation remains: correction-time RED evidence is
not proof of the original feature's pre-implementation TDD sequence. The earlier
review established that the original sequence was not preserved. The updated
report documents the correction's failing probes separately; no historical
feature RED evidence is inferred from them. This is a disclosed process-history
limitation, not an outstanding functional defect in this correction.

### F1 — Addressed: rebuilt observations are bound to the issued revision

At `workspace/queries.rs:234`, the rebuilt snapshot is checked against the
original dependency observations before candidate reconstruction and metadata
rendering. `completion_observations_match` / `completion_observations_equal`
(`queries.rs:1324-1370`) match original records without reusing a rebuilt record
and compare source provenance, versions/stamps, dependency identity, authorization
and project observations. Original records are revalidated after rebuilding and
are also appended to the final result at `queries.rs:306`, keeping them in the
worker's final validation and delivery read set.

The original deterministic `--race` probe now receives:

```json
{"error":{"code":-32803,"message":"completion dependency observations changed while resolving; retry the request"}}
```

No mixed `DocOld`/`DocNew` item is returned. A no-write control still resolves
`DocOld` to `function DocOld: Integer;` and `OLD DOCUMENTATION.` The new
`completion_resolution_rejects_provider_change_after_original_validation`
unit test (`queries.rs:2093`) also passes. The existing unrelated-overlay
acceptance test remains green, so the fix has not simply replaced dependency
validation with unconditional generation invalidation.

### F2 — Addressed: parsed provenance is independent of later raw hashes

`SourceRecord.parsed_text_hash` is captured from the decoded source actually
used by the snapshot, including the imported-source branch at
`workspace/rename.rs:3098-3120`. Compaction retains that provenance independently
of the raw content hash. Both shared validation paths now additionally call
`parsed_source_changed` (`rename.rs:1001`), preventing a later raw-byte hash from
masking a different parsed source. Open-overlay delivery validation also checks
the parsed provenance. Metadata-only records do not acquire fictitious parsed
source observations.

The original `--hash-race` probe now rejects **initial completion**, before an
inconsistent identity can be registered:

```text
-32803: closed source changed while resolving file:///.../Provider.pas; retry the request
```

The old reproducer assumes initial completion succeeds and consequently raises
`KeyError: 'result'` after printing this expected rejection. That is a harness
assumption, not a server failure: a separate assertion wrapper explicitly checked
the protocol error and absence of a resolve response and exited successfully.
The unchanged breakpoint at `rename.rs:3112` still occurs after imported source
has been copied from the index and before its later disk fingerprint, so it
continues to test the relevant interleaving despite shifted line numbers.

The new `full_source_revalidation_rejects_inconsistent_parsed_text_and_hash`
regression (`queries.rs:2162`) exercises both `Workspace::revalidate_records`
and `revalidate_input`, and passes. The shared-validator regression from F2 is
therefore covered directly, not just hidden behind the new resolve guard.

### F3 — Addressed: requested feature coverage added

| Prior gap | New coverage and result |
| --- | --- |
| Exact overloaded declaration | `protocol.rs:4004`: cross-unit overloads with distinct documentation; asserts the exact Integer signature and documentation. Passed. |
| Exact helper declaration | `protocol.rs:4063`: resolves the helper method and asserts exact declaration detail/documentation. Passed. |
| Related provider overlay | `protocol.rs:4172`: changes the provider's authoritative overlay after issuing completion; requires `-32803`. Passed. |
| Stale non-cancelled worker delivery | `protocol.rs:4295`: pauses a dispatched resolve, changes workspace overlay state, then requires the delivery-side stale-result error, without cancellation. Passed. |
| Context record/byte limits | `server.rs:4665`: accepts 1,024 observations, rejects 1,025, and rejects a context exceeding 2 MiB. Passed. Existing entry eviction, foreign/tamper/oversized-data tests also passed. |
| Original P1 defects | The two new unit regressions and both external deterministic reproductions verify rejection at the relevant boundaries. Passed. |

Previously passing generic-specialization, independent metadata negotiation,
plaintext/Markdown, stable-field restoration, requester/context/provider-disk
freshness, unrelated-overlay, and cancellation tests also passed in the focused
feature run. This assessment does not claim that the new bounds test measures
actual allocator usage or exhaustively tests every aggregate-store byte value;
the store's existing total-accounting/eviction implementation was not changed by
the correction.

### Fix-diff quality assessment

- The correction preserves the worker/protocol division: snapshot comparison and
  disk revalidation are worker-side. No new protocol-thread filesystem I/O was
  introduced.
- Original and rebuilt observations are both retained only for the in-flight
  resolve result; this is not a new per-item retained snapshot store.
- Added `SourceRecord` initializers distinguish parsed sources from metadata-only
  dependencies. The changes to code-action/workspace record constructors support
  the shared record field rather than adding unrelated behavior.
- Source validation is strengthened without changing reference/rename discovery
  or completeness rules. The correction does not touch completion edits,
  coordinate conversion, case-insensitive binding, or negotiated rendering.
- No new correctness regression was identified within this fix diff. No
  repository source edits, subagents, or unrelated audit were performed.

### Fresh round 1 verification

Confirmed HEAD and clean implementation worktree; `git diff --check
bd409d5..0d4ce7b` succeeded. Read the updated report, unchanged brief, prior review,
supplied correction diff, and corresponding source/test locations.

```text
cargo test -p pascal-lsp --lib completion_resolution -- --nocapture
4 passed, 0 failed

cargo test -p pascal-lsp --lib full_source_revalidation_rejects_inconsistent_parsed_text_and_hash -- --nocapture
1 passed, 0 failed

cargo test -p pascal-lsp --test protocol_barriers --features test-support completion_resolution -- --nocapture
12 passed, 0 failed; 376 filtered out
```

Replayed `/tmp/opencode/task17-review-probe.py` in control, `--race`, and
`--hash-race` modes, then repeated all three with explicit protocol-output
assertions. Wrapper result: **3/3 PASS**, exit code 0. Latest artifacts:

- Control: `/tmp/opencode/task17-review-4q42yig9`
- F1 rejection: `/tmp/opencode/task17-review-l4mx01hu`
- F2 rejection: `/tmp/opencode/task17-review-7ovgoey1`

No full suites were repeated. The updated report's 269 library / 291 navigation /
348 protocol / 388 barriers / 70 rename results and formatting/Clippy checks are
recorded as supplied implementation evidence, not claimed as rerun by this review.
