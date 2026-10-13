# Shared persistent analysis cache

Date: 2026-10-12

Status: Written specification approved by the user ("looks good") on 2026-10-12.
Approved for implementation planning, not product implementation. Implementation
plan review and execution-method selection remain required.

Tracking: architecture gate `l4d-0jh`, delivery epic `l4d-zf1b`.

## 1. Intent and agreed scope

Reduce repeated analysis and make bounded workspace indexing useful across LSP
restarts on large Delphi/Pascal repositories. The motivating repository has
roughly 24,000 Pascal files and more than 230 projects. Share eligible facts
between projects and configurations without sharing incompatible meaning.

The cache is enabled by default, shared by the same OS user on the same machine,
and stored on local disk outside project directories. SQLite WAL is the initial
storage backend. Users can disable persistence without losing features.

The first delivery is LSP-focused. Its contracts permit later lint CLI adoption,
tracked separately in deferred `l4d-u14s`. Cache administration is included;
adoption by the lint analysis pipeline is not.

Success means less reparsing after restart, incremental/resumable indexing,
compact candidate queries, and lazy syntax-tree loading. It does not mean zero
startup reads, identical latency with persistence disabled, or removal of edit
proof requirements and source/request limits.

Excluded from this release:

- Network, cross-machine, cross-user, and daemon-backed caching.
- Persistence of unsaved overlays or anything derived from them.
- Native syntax trees, full source-text storage, and serialized Rust object graphs.
- A second analyzer, resolver, workspace index, or reference-binding engine.
- Selective keys containing only the defines an analyzer happened to consult.
- Persistent DCU analysis artifacts, rule diagnostics, and bound reference-location
  proofs. Existing compiled-provider handling remains available through normal
  analysis and contributes dependency evidence when consumed.
- Mandatory migration of old semantic artifacts or incompatible container schemas.

## 2. Existing boundaries and prerequisites

The current `pascal-lsp::project_cache::ProjectCache` retains process-local
`ParsedDocument` values, import results, pins, probes, and request leases. It
cannot be serialized as the persistent representation. Its historical retained
memory multiplier is not a measurement of the proposed compact representation.

`ProjectContext::same_analysis_inputs` compares parsing/resolution inputs
separately from appended observations. Existing `u64`/`DefaultHasher` keys and
stamp-only shortcuts are not durable identities or startup freshness proofs.
Conditional projection precedes parsing; existing parses are not define-neutral.

The shared resolver already exposes source revisions, path/read provenance,
`ResolutionObservation`, and `ResolutionReport`. Extend their durable projection
where needed; do not invent an independent lookup or authorization policy.

| Existing issue | Contract used by this design |
| --- | --- |
| `l4d-51` | Canonical immutable project inputs and complete discovery keys, including build selections |
| `l4d-122` | Same-size/same-mtime edits cannot pass content freshness probes solely because stamps match |
| `l4d-33` | One versioned workspace index, incremental updates, and candidate selection; add the catalogue/materialization separation below |
| `l4d-20` | Shared directory catalogues and excluded-tree handling; support bounded streaming reconciliation rather than a second crawler |
| `l4d-10` | Retained/outstanding memory accounting and leases; extend it to compact facts and storage overhead |
| `l4d-36` | Session-local freshness witnesses; no cross-process or restart portability is implied |
| `l4d-6` | Truthful incomplete coverage for read-only results; persistence does not relax edit strictness |

These issues retain their existing owners and milestone relationships. Their
requirements are not considered implemented by approving this design. Follow
repository coordination rules for the shared workspace/navigation files.

## 3. Components and optionality

Use a dedicated workspace crate, `pascal-cache`, rather than placing SQLite in
the LSP or making every `pascal-core` consumer depend on it.

| Component | Owns | Does not own |
| --- | --- | --- |
| `pascal-project` | Canonical configuration inputs, source-origin/effective conditional selection, project metadata and read provenance | Workspace membership, SQLite, editor revisions |
| `pascal-core` | LSP-neutral compact fact DTOs and domain evidence contracts; reusable extraction helpers where existing analysis permits | SQLite, LSP positions, session scheduling |
| `pascal-cache` | Canonical durable-key encoding, SQLite backend, immutable publication, policy and bounded maintenance | Reading project sources to authorize/validate them, semantic binding, editor overlays |
| `pascal-lsp` | Existing analysis/extraction adapter, workspace index, current verification, authorization, overlays, scheduling and tree hydration | Database-specific query semantics exposed to features |

`pascal-cache` may depend on `pascal-core` and `pascal-project`; neither depends
on `pascal-cache`. SQLite and BLAKE3 dependencies belong to `pascal-cache`.
Keep existing navigation traversals as producers of neutral facts rather than
moving the navigation monolith wholesale into core.

Consumers use a backend-neutral index/fact contract. The memory-only adapter
provides the same domain operations. SQLite adds restart/cross-session reuse and
disk-backed candidate access, not a distinct feature path.

Storage operations return an admitted result, a typed miss, or a bypass reason.
Absence of a cached artifact is never a semantic claim that a symbol/provider is
absent. Only the domain resolver/index coverage can make such claims.

With persistence disabled:

- Do not inspect/create the cache directory, read its policy, open SQLite, read
  artifacts, write records, or run maintenance during ordinary LSP operation.
- Retain normal in-memory indexing, navigation, references, rename, diagnostics,
  and other features, subject to their existing correctness/resource limits.
- Leave previously stored data untouched. No persistent-cache dependency is
  needed to perform a feature's authoritative analysis.
- Runtime disable fences new cache jobs, cancels queued work, and waits for
  admitted operations to finish/rollback before acknowledging the transition.
  After that acknowledgement, persistent-cache IO is zero. Editor state is not
  discarded. Explicit administration modes are separate user-requested operations.

The same fallback applies when persistence is unavailable. Already decoded facts
can remain usable in memory if independently verified and admitted.

## 4. Identities and canonical encoding

Use BLAKE3 256-bit digests over explicitly versioned, domain-separated canonical
encodings. Do not persist Rust `Hash`, `DefaultHasher`, pointer identities, or
process generations as durable keys.

Canonical encoding v1 uses explicit field/enum tags, fixed little-endian integer
encodings, length-prefixed sequences/strings, and explicit optional values.
Text is UTF-8 without implicit Unicode normalization. Order maps/sets by their
canonical encoded keys; preserve ordered search paths, provider precedence,
include occurrences and source-mapping topology. Distinguish true, false,
unknown, and the policy for absent conditional symbols.

Paths use tagged native encodings: Unix bytes or Windows UTF-16 code units.
Preserve spelling and provenance when semantic/path policy requires it; never
use lossy display strings as identity. Canonical resolved identity, logical path
and original spelling have distinct fields. Source encoding/BOM and the decoder
contract are explicit because analysis ranges use decoded UTF-8 coordinates.

Every input manifest is retained in canonical form and compared on lookup.
A digest narrows identity; it is not permission or freshness evidence. Adding a
consumed input requires classification and an encoding/version test. Semantic
versioning covers grammar, conditional analysis, extraction, resolution and
decoding behavior, not merely the executable's release label.

Compute durable source digests from the authorized original payload. Existing
`SourceRevision`/probe `u64` hashes may remain session compatibility fields; do
not promote them into strong identities by hashing the old hash value.

### 4.1 Local facts

`LocalArtifactKey` includes:

- Digest of original source bytes and source decoding contract.
- Effective per-source conditional environment: defines/undefines, absent-define
  closure, constants, compiler options/version, RTL facts, platform/ABI/layout.
- Source-origin classification where it affects analysis; do not substitute the
  project's define set for a library source's effective environment.
- Grammar, conditional-analysis and fact-extraction versions.

Do not include project names/paths solely to prevent sharing. Conversely, do not
omit a path-dependent input to manufacture equivalence. A path-inferred fallback
unit name is a member wrapper, not a path-neutral declaration fact.

The first version persists conditional local facts, not a new define-neutral
parse. A raw content digest need not imply a stored byte/lexical artifact.

### 4.2 Include-expanded facts

`ExpandedArtifactKey` includes the local inputs plus ordered include occurrences,
their content digests, inherited directive state, resolver inputs/evidence and
mapping topology. A different include resolution or source mapping is a
different expansion, even if the root source is unchanged.

Do not treat an unordered set of included files as an expansion identity.
Preserve repeated occurrences and nested parentage.

### 4.3 Context, members and bindings

`ContextKey` encodes complete project/configuration analysis inputs: project and
main-source identity; build/config/platform selection; installation evidence;
ordered search/include/browsing/provider paths with provenance; aliases,
namespaces, explicit mappings and packages; project/library conditional inputs;
overrides; read policy; and discovery/binding-input completeness.

Consumed metadata/environment values and their dependency observations are
explicit. Compare exact manifests; an incomplete context cannot be silently
upgraded by matching its selected strings to a complete one.

A member wraps an artifact with its logical/physical source identity, paths,
fallback name if needed, source-origin provenance and relevant evidence.
Resolved import/include relationships are scoped to an exact context snapshot
and evidence set. A/Debug, A/Release and B/Debug may share local facts but not
incompatible bindings or project membership.

### 4.4 Session-private inputs

If any root, include, provider, metadata input or transitive dependency came from
an unsaved overlay, the derived artifact/binding/snapshot is not publishable.
Do not persist it merely because its bytes equal saved bytes. Revision provenance
must follow the existing session source-store contract. After save, authoritative
disk input can establish a separate persistence-eligible result.

An open editor does not disable caching for unrelated saved units. The background
indexer may construct an independently authorized disk-only baseline, then compose
current overlays and their affected bindings privately in the session index.
Disk-only results require disk-only observations throughout their dependency graph;
do not relabel a live overlay-derived snapshot or merely drop its edited members
to make it publishable. Both baseline and private additions remain memory-admitted.

## 5. Compact DTO contracts

The following are logical contracts, not a requirement to expose SQLite rows
or LSP structures in public APIs.

| DTO | Required fields |
| --- | --- |
| `ArtifactHeader` | Kind/key/namespace, exact canonical inputs, encoding and source lengths, table counts, fact-content checksum, bounded supplemental-byte count, extraction coverage |
| `DeclarationFact` | Artifact-local declaration ID, normalized and display names, declaration kind/visibility, optional parent ID, owner source slot, decoded UTF-8 byte range, conditional certainty |
| `ImportFact` | Occurrence ID, spelling/normalized unit name, interface/implementation scope, optional explicit path expression, owner slot/range, conditional certainty |
| `IdentifierPosting` | Occurrence ID, normalized spelling, owner slot/range and conservative activity classification; no persisted semantic target |
| `IncludeMapping` | Occurrence and parent occurrence, owner/child source slots, directive range, expanded range, mapped source range and expansion certainty |
| `ObservationFact` | Kind, ordered ordinal when meaningful, native path/source identity, authorization provenance, content digest or presence/membership evidence, completeness |
| `MemberFact` | Context-snapshot member ID, path/source wrapper, optional artifact ID, evidence-set ID and member coverage |
| `BindingFact` | Import/include occurrence and owning member, resolver outcome, optional precise target/member, ambiguity candidates or incomplete reason, evidence-set ID |
| `CoverageFact` | Separate discovery/extraction/resolution status plus bounded typed reasons; no portable verified-at-current-time flag |

Artifact-local IDs are deterministic from extraction ordering and ranges, not
tree node addresses or global declarations. Resolve them through their owning
artifact/member. Validate range arithmetic, parent references, encoding, counts
and tags before consumption. Cap warnings/reasons using existing bounded report
behavior; a truncated reason list still retains an incomplete marker.

Names use the existing Pascal semantic normalization rules, versioned with the
extractor. Store/query the normalized spelling with binary SQL comparison, not
SQLite's `NOCASE` or locale-dependent folding. Display spelling remains separate.

Stable incomplete facts, such as unsupported syntax under exact inputs, may be
stored with their limitations. Cancellation, transient IO, insufficient budget
and incomplete work do not become permanent failed/empty artifacts. A historical
partial member can record such a reason, but the scheduler retries it.

## 6. SQLite container and relational schema

Use one private `analysis.sqlite3` container per selected cache directory, with
one physical container-format version and multiple semantic namespaces. A
namespace identifies canonical-encoding, grammar/analyzer and DTO versions.
Rebuild incompatible semantic namespaces; do not migrate them in place.

If the physical container schema is unsupported, bypass it. Do not replace or
migrate a file that another binary may have open. A user can select a fresh local
directory; incompatible-container recovery must not delete a live database.

Use a bundled SQLite through `rusqlite`, selected/pinned during implementation
planning to provide SQLite 3.51.3 or later with the WAL-reset fix. Assert the
runtime SQLite version. WAL mode is required and must be confirmed, not assumed.
Use foreign keys, constrained enum/count/range fields and bound SQL parameters.
No extensions, executable SQL from cache records, or attached project databases.
Reconstructible cache transactions may use `synchronous=NORMAL`; losing recent
cache commits after power failure is acceptable, losing editor state is not.

### 6.1 Tables

`Digest` below is a 32-byte BLOB; `Nonce` is a random 16-byte identifier. Required
fields are non-null unless explicitly optional. References stay within one
namespace. Ordinals/ranges/counts are non-negative checked integers.

Every semantic primary/foreign key below has an implicit prefix
`Scope = (namespace_id, namespace_reset_epoch)`. The two administrative tables,
`store_state` and `namespaces`, are exceptions. Thus an artifact's physical
address is `(Scope, artifact_id)`, not the digest alone. Queries use the current
epoch; old-epoch rows cannot collide with or satisfy new-epoch publication.
Reset does not change content equivalence, but it changes storage eligibility.

| Table | Primary key and columns |
| --- | --- |
| `store_state` | Singleton: container version, policy revision, reset epoch, maximum disk bytes, WAL threshold, committed admission/accounting metadata |
| `namespaces` | `namespace_id: Digest`; canonical version manifest, namespace reset epoch, creation metadata |
| `evidence_sets` | `evidence_id: Digest`; lifecycle (`live`/`retired`), canonical manifest checksum, observation count and historical completeness |
| `observations` | `(evidence_id, ordinal)`; kind, encoded source/path/provenance, optional content digest, presence/membership data and completeness |
| `artifacts` | `artifact_id: Digest`; lifecycle (`live`/`retired`), kind, canonical inputs, optional expansion evidence ID, fact checksum, per-table counts, source/analysis lengths, bounded supplemental payload, coverage |
| `declarations` | `(artifact_id, declaration_id)`; declaration DTO fields and row checksum |
| `imports` | `(artifact_id, occurrence_id)`; import DTO fields and row checksum |
| `identifier_postings` | `(artifact_id, occurrence_id)`; posting DTO fields and row checksum |
| `include_mappings` | `(artifact_id, occurrence_id, segment_ordinal)`; mapping DTO fields and row checksum |
| `contexts` | `context_id: Digest`; canonical project input manifest and its checksum |
| `snapshots` | `snapshot_id: Nonce`; context, state (`building`/`sealed`/`retired`), builder token, reset epochs, build revision, membership/discovery evidence ID, optional sealed manifest digest, counts/coverage and bounded recency metadata |
| `members` | `(snapshot_id, member_id)`; source/path wrapper, optional artifact, evidence ID, fallback name/origin and coverage |
| `bindings` | `(snapshot_id, member_id, binding_kind, occurrence_id)`; resolver outcome, optional target member, bounded outcome payload and evidence ID |

Nullable member artifacts represent recognized but currently unindexed members;
they cannot exclude a file from candidate searches. Ambiguous/unavailable/
incomplete bindings preserve their outcome rather than pretending to be precise
targets. Compiled-provider relationships are re-established by ordinary analysis
in this release; a persisted unsupported relationship keeps coverage incomplete.

Expanded artifacts require an expansion-evidence reference. Local artifacts do
not embed the publishing project's path-specific evidence; each consuming member
supplies its own root/context evidence. Artifact evidence references participate
in foreign-key and GC reachability checks as well as member/binding references.

Foreign keys preserve context/member/artifact/evidence relationships. Sealed
semantic rows cannot be rewritten by normal publishers. Lifecycle state,
administrative epochs and bounded recency hints are the only mutable control
metadata. Stored timestamps do not establish source freshness or semantic order.

Lookups and new reference insertion require a live artifact/evidence set in the
current scope. Retiring an unreferenced record and admitting a new reference
serialize through short write transactions; a builder cannot attach to a record
whose batched deletion has started. Snapshot retirement removes eligibility
before bindings/members are deleted. Namespace reset invalidates old scope
eligibility before asynchronous reclamation.

Required secondary indexes:

- Declarations: `(normalized_name, artifact_id)`.
- Imports: `(normalized_unit_name, artifact_id)`.
- Identifier postings: `(normalized_name, artifact_id, occurrence_id)`.
- Members: `(snapshot_id, artifact_id)` and `(snapshot_id, source_identity)`.
- Snapshots: `(context_id, state, recency_hint, snapshot_id)` for bounded candidate
  selection, never an authoritative latest-state lookup.
- Observation paths and artifact/member references used for invalidation/GC.
- Bindings: target/source member indexes for the existing reverse-dependency
  operations, scoped by snapshot and outcome.

Use keyset pagination, not growing offsets or unbounded `IN` lists. SQL must
restrict results to the requesting context's eligible membership. A name hit
in the global fact table is not authorization or evidence of project membership.

### 6.2 Integrity and atomic artifacts

An artifact header, its facts and required evidence publish in one bounded
transaction. A duplicate key requires exact input/format agreement; conflicting
content is an integrity failure, not permission to overwrite an immutable row.

The fact checksum covers canonical ordered row content and supplemental data.
Before persisted postings can exclude candidates, establish an integrity witness
by streaming the artifact's rows, validating counts/checksums and DTO invariants.
This does not require retaining all facts. Never accept a corrupted/missing row
as proof of absence. Sealed artifact rows are immutable until whole-artifact GC;
missing headers/references or SQL/integrity errors invalidate eligibility.

Apply equivalent checksum/count/size validation to canonical context and evidence
records and sealed snapshot manifests. A valid artifact with invalid lookup or
membership evidence is not a hit. Integrity witnesses assume application-enforced
immutable records, not permission for external tools to rewrite a live database.

No compression is required initially. Admission includes encoded bytes, row
counts, bounded decoded allocations and temporary buffers. Oversized artifacts
are not persisted; normal analysis remains available.

### 6.3 Snapshot construction

Create a random snapshot/builder token under the current reset epochs. Add
members/bindings in admitted batches. Each mutation increments the build revision
and is conditional on the builder token, epochs and `building` state.

After all intended membership work has outcomes, stream a canonical manifest
digest and counts outside a long transaction. Seal with a short compare-and-set
against the observed build revision, token and epochs. A seal succeeds only if
referenced records exist and the recorded counts/coverage agree. Missing references
or concurrent mutation force retry/miss, not a partial seal.

`sealed` means a structurally finished historical snapshot; it does not mean
complete analysis or current verified coverage. A sealed snapshot may honestly
record incomplete members/discovery. Unfinished builds are not query snapshots,
but already committed standalone artifacts remain useful after interruption.

## 7. Freshness, authorization and coverage

The LSP owns per-session verification witnesses for source contents, membership,
resolver evidence, context inputs and cache integrity. No portable SQLite flag,
timestamp or another process's generation supplies those witnesses.

On a potential hit:

1. Obtain the current canonical context and source origin through normal discovery.
2. Authorize the root and every consumed dependency/path under current read policy.
   A cached wider policy never authorizes a narrower requester.
3. Validate current source/include/config/environment inputs and successful,
   ambiguous and negative resolver candidates/membership.
4. Match exact canonical inputs and verify admitted artifact integrity.
5. Merge observations, report/coverage and accounting through the existing analysis
   paths, so result-delivery and edit freshness checks retain their inputs.

A matching file length/mtime alone is insufficient. Startup and uncertain change
coverage require reading/hashing applicable content. Within a verified covered
lifetime, reliable native change identity and the `l4d-122` freshness contract may
avoid rereading unchanged files. Watchers reinforce evidence; they do not make
stamp-only comparisons safe or prove offline history.

Restart, watcher overflow/gaps, source-origin changes and uncertain directory
evidence invalidate the relevant witnesses. Git HEAD is a hint, not an identity
for untracked/generated files or a filesystem freshness proof.

| Changed input | Reconcile |
| --- | --- |
| Root content/encoding | Local facts and dependent expanded facts/bindings |
| Include bytes/order/state/mapping | Affected expansion and dependants |
| Defines/options/compiler/RTL/platform/layout | Effective context variants and affected facts |
| Project, optset, installation XML, overrides or consumed environment | Context/discovery evidence and affected bindings |
| Provider appearance/deletion/rename or ambiguity | Successful and negative lookup evidence and affected consumers |
| Search precedence, aliases, namespaces, mappings, packages | Resolver evidence and bindings |
| Read policy, symlinks, path identity, exclusions or roots | Authorization, affected membership and coverage |
| Decoder/grammar/analyzer/format version | Select/rebuild the matching namespace |
| Restart or watcher gap | Re-establish session witnesses before authoritative use |

There is no claim of atomic filesystem snapshots supplied by SQLite. Concurrent
source edits are handled through existing observed-revision validation at result
delivery and edit execution. An observed change during validation rejects the
affected proof. Persisting a historical artifact never makes it current.

Before a workspace search is complete, reconcile its applicable discovery scope
and all members needed to justify candidate exclusion. Checking only old matching
postings can miss a file that gained a new identifier. An unverified/unindexed
member remains a candidate requiring analysis or contributes incomplete coverage.

While reconciliation is incomplete, return only proven read-only findings with
bounded reasons. Rename/code actions require the existing complete current proof.
Stored coverage never removes cancellation, resource, unknown-conditional or
unavailable-input limitations.

## 8. Large-workspace index and data flow

Add `maxWorkspaceUnits`, default and initial hard ceiling 100,000 distinct unit
members per project/configuration, configurable downward. This bounds compact
catalogue membership, not simultaneously loaded sources or trees. Include
occurrence and directory/metadata work retain their separate bounds.

Keep the existing `maxFiles` ceiling (10,000), `maxFileBytes` (2 MiB),
`maxTotalBytes` (256 MiB) and existing resolver/include/request limits on their
applicable materialization/work paths. Do not silently redefine existing settings.
`l4d-33` separates the compact catalogue from the currently retained document
index; `l4d-20` supplies streamed directory reconciliation within bounded work.
Other catalogue limits must be classified by retained batch size versus total
scope rather than bypassed to manufacture completeness.

Both memory-only and persisted adapters expose this larger catalogue. If it does
not fit an adapter's admitted capacity, coverage becomes incomplete; disabling
persistence does not disable the feature. Source files above existing size limits
and requests with too many necessary candidates remain limited.

### 8.1 Startup and background indexing

1. Admit/open persistence if enabled; otherwise select the memory-only backend.
2. Discover current project/context and membership under normal authorization.
3. Choose a bounded historical snapshot candidate as a hint, not a current index.
4. In resumable batches, rehash/reconcile relevant sources, includes, metadata,
   providers and directory membership; establish fact-integrity witnesses.
5. Reuse matching compact facts, compute changed/missing ones, and update the
   existing workspace index and affected dependency closure.
6. Commit eligible artifacts independently and construct/seal the historical
   snapshot with honest coverage. Publish current session index generations only
   from currently verified inputs.

Resume from current verified facts, not a trusted serialized crawler cursor.
Discovery may need to repeat after restart; committed per-unit analysis work need
not. A batch is bounded by existing source/work limits and the new IO/decode
budgets. Yield between units and poll cancellation during long operations.
Interactive jobs have priority; do not monopolize their execution slots with a
whole-workspace rebuild. Transient failures remain retryable.

Complete 24k-unit catalogue reconciliation can span many batches without one
24k-document request. An offline workspace still requires content/membership IO;
the benefit is avoided parsing/index extraction and on-demand tree retention.

### 8.2 Queries

Use `l4d-33` exports/importers/identifier candidate operations. Select from current
eligible members, then run existing semantic binding and source mapping on the
necessary candidate documents. Conservative dependencies may add candidates;
unproved candidate pruning is forbidden.

Declaration summaries can supply supported index queries, but lexical postings
are not final reference locations. Hydrate native trees only for consumers that
need them, charging ordinary tree/lease budgets. Cache presence does not widen
source access. Partial chunks retain their normal delivery-freshness rules.

An edit recomputes/reconciles the affected closure. An A/B/A configuration switch
can reuse old immutable artifacts only after current revalidation; it does not
restore another session's open-document state or freshness generation.

## 9. Multi-process concurrency and recovery

Duplicate cold computation is acceptable initially. There are no cross-process
single-flight work claims or daemon. Exact immutable keys converge duplicate
publishers; later coordination needs profiling and a separate design review.

Perform hashing, analysis and semantic binding outside SQLite transactions and
outside project-cache locks. Never hold a database transaction while invoking
filesystem discovery, resolver callbacks or result delivery. Use at most two
read connections and one write/maintenance connection per process; connection
queues, page caches and result batches are bounded.

Historical snapshots are not ordered semantic truth. There is no mutable global
latest-project pointer. A slow old worker can add history but cannot replace a
current session's selected/verified bindings. Builders and artifact publishers
check namespace/store reset epochs at commit to fence administrative resets.

Read bounded batches in short consistent transactions. Release SQL statements and
transactions before analysis. Between-batch GC can cause a missing snapshot or
artifact; retry/fall back and invalidate the relevant coverage instead of ignoring
the missing data. Do not introduce request-long read leases/transactions in v1.

Crashes at artifact publication expose either a committed structurally complete
artifact or no artifact. Crashes in membership construction leave an unsealed
build, not a current index. SQLite handles WAL recovery; a busy recovering store
can be bypassed by another process. Power loss may lose reconstructible commits.

## 10. Resource policy and maintenance

| Resource | Initial policy | Scope |
| --- | --- | --- |
| Retained analysis cache | Existing 2 GiB `maxCacheBytes`; compact facts, leases and SQLite page caches are charged, not added unchecked | Per session |
| SQLite page caches | 16 MiB aggregate, also within retained-cache admission | Per process |
| Cache transient decode/encode buffers | 16 MiB aggregate, additional separately bounded scratch | Per process |
| Managed disk | 8 GiB, including DB, WAL, temporary files, unfinished work and obsolete namespaces | Shared store |
| WAL pressure threshold | 256 MiB, included in disk policy | Shared store |
| Interactive database contention | At most 25 ms cumulative lock/connection wait per cache operation before bypass | Per operation |

Use bounded allocation admission before reading/retaining rows or decoding DTOs.
Page-cache settings are not a complete SQLite heap/RSS bound: account or measure
connection/statement allocations, avoid unbounded SQL sorts, disable database
page mmap, bound SQLite length/expression/parameter limits, and bound temporary
spill. WAL shared-memory/OS page accounting is reported separately from owned
Rust heap. A connection is not opened when its overhead cannot be admitted.

Initially admit at most 16 MiB of canonical fact content per artifact. Use row
batches bounded by both 256 rows and 1 MiB; a row over the batch allowance is a
cache miss, not an unbounded allocation. Actual decoder scratch must also fit
the aggregate allowance. Larger authoritative analyses can simply skip storage.
Canonical input/evidence/control payloads are also capped at 1 MiB per row;
stream multi-row manifests rather than accumulating unbounded metadata blobs.
Cancellation/progress checks and request work limits bound query/validation CPU;
the 25 ms limit concerns waiting, not a claim that all queries finish in 25 ms.

Store-wide disk/WAL policy is initialized once, then changed explicitly through
administration, not last-writer-wins LSP project settings. Check policy revisions
and reserve estimated admission transactionally before a write. Account physical
file sizes plus reserved in-flight growth; freed SQLite pages and physical file
length are not interchangeable. Reserve maintenance headroom inside the budget.
Reject overflow, impossible policies and writes without headroom.

Maintenance rules:

- Run bounded maintenance off interactive paths. Use background passive
  checkpoints and reader gaps; do not force blocking truncation on requests.
- At the WAL threshold, stop new writes unless checkpoint/reuse creates room.
  A stuck reader cannot authorize endless WAL growth.
- Retire unused historical snapshots and abandoned builds in batches. Builder
  expiry is a scheduling hint; conditional token/state checks make cleanup races
  recoverable even if a worker is still alive or the wall clock moved.
- Reclaim artifacts/evidence only when unreferenced by surviving snapshots/builds.
  Do not cascade a large artifact/snapshot deletion in an unbounded transaction:
  retire it from lookup, delete bounded row batches, then remove its header.
- Preserve required references for live staged builders until retirement. A
  publisher whose build was retired must retry rather than seal missing data.
- Batch/coalesce recency hints; do not write a timestamp for every cache read.
- Prefer reuse of freed pages and bounded incremental reclamation. Set the
  necessary incremental-vacuum policy when creating a new container; no full
  `VACUUM` on an interactive path.
- Count obsolete namespaces in shared usage. Reclaim only formats whose
  references/lifecycle the container understands; otherwise stop admission.

The disk budget is a managed target, not a filesystem-enforced hard quota. An
already oversized store, reservations/SQLite overhead and unexpected external
growth require write suspension and bounded reclamation. Never claim a physical
upper bound the implementation cannot enforce. Low free disk space can reject
writes before the configured budget is reached.

Corruption, missing permissions, unsupported filesystem, read-only state,
contention, incompatible container and full disk safely bypass persistence.
Do not automatically unlink/rename an open DB/WAL or perform expensive integrity
rebuilds during an interactive request. Recover compatible data through bounded
logical reset; for unreadable containers, report safe fresh-directory/offline
recovery instructions. Editor state and source files are never cache cleanup targets.

## 11. Configuration, location and administration

### 11.1 LSP options

Use existing initialization/runtime configuration conventions with these names:

| Option | Default and behavior |
| --- | --- |
| `persistentCacheEnabled` | `true`; runtime disable follows the zero-IO acknowledgement contract |
| `persistentCacheDirectory` | OS user-cache directory plus `lint4d/analysis`; user-controlled initialization setting, not a project-sidecar redirect; changing requires restart |
| `maxWorkspaceUnits` | 100,000; configurable downward for compact per-context catalogue scope |
| `persistentCachePageBytes` | 16 MiB; aggregate per-process page cache, configurable downward and subject to `maxCacheBytes` |
| `persistentCacheScratchBytes` | 16 MiB; aggregate per-process transient buffers, configurable downward |
| `persistentCacheWaitMs` | 25; configurable downward, cumulative contention allowance |

Existing source/materialization limits retain their documented meaning and hard
caps. Invalid new limits produce actionable configuration errors, not unchecked
casts or silent increases. Changing runtime budgets cancels/fences affected cache
jobs and re-admits retained state; disabling does not remove authoritative buffers.

Resolve platform defaults using the platform cache directory convention, including
XDG on Linux, local app data on Windows and user caches on macOS. A network-backed
home directory is not evidence that its cache subdirectory is local.

Before opening, positively establish local-filesystem suitability, user ownership,
private permissions/ACLs, regular database files and safe path handling. Do not
trust project-controlled cache redirects or database symlinks. Unsupported or
uncertain locality/security causes bypass with an explanation and a local-directory
override recommendation. Unix directories are private to the user; Windows ACLs
must provide equivalent protection. Validate paths without exposing source data.

### 11.2 Administration modes

`pascal-lsp` provides mutually exclusive explicit modes, which do not start the
language server:

- `--cache-status`: bounded aggregate health/usage/policy report; no source text or
  unrestricted project catalogue. A missing store is reported without creating it.
- `--cache-prune`: bounded reclamation; can be rerun until more space is reclaimed.
- `--cache-reset`: reset the current semantic namespace logically. Increment its
  reset epoch, fence builders/publishers, retire its lookup records, and reclaim
  physical data in batches. Other namespaces are not blindly deleted.
- `--cache-directory PATH`: select the same validated local directory as the LSP.
- `--cache-max-disk-bytes N` and `--cache-wal-threshold-bytes N`: explicit store-wide
  policy administration, with positive checked values, sufficient maintenance
  headroom, and a WAL threshold smaller than the disk budget.

Policy changes form a separate mode, cannot be combined with status/prune/reset,
and may initialize a missing store after the same private/local checks. They
report the accepted policy and exit. Status reports an absent store successfully;
prune/reset of an absent store are successful no-ops. Incompatible stores and
administration failures return a nonzero status with a bounded reason. Reset
never deletes source files, editor state or a DB/WAL file with live handles.
Namespace epoch changes invalidate cache eligibility in other sessions; their
already verified authoritative memory state remains usable. Subsequent fresh work
may repopulate the new epoch.

Normal LSP diagnostics aggregate hit/miss/bypass causes, validation work,
parse/bind counts, admitted/decoded bytes, disk/WAL usage and lock waits. Avoid
per-source hot logs, source text and exposing project membership to unrelated
queries. Diagnostics retention and repeated warnings are bounded.

## 12. Acceptance and measurement

No performance measurements have been made for this subsystem. The following
are user-approved release targets, not claims about implemented behavior.

### 12.1 Correctness matrix

Under identical effective inputs and sufficient budgets, cached and uncached
complete results, mappings, warnings and edit outcomes must agree. Under limited
budgets the available partial set can differ, but every finding must be proven
and coverage/edit strictness must remain truthful.

Test at least:

- A/Debug, A/Release, B/Debug and A/B/A switching, including project/library origin.
- Conditional unknown/false/absent closure, compiler options/constants/layout,
  decoding/BOM/non-UTF8 sources, repeated/nested includes and source mappings.
- Root/include/config/installation/environment/package changes; negative candidate
  appearance; provider deletion/rename/ambiguity; aliases/search-order changes.
- Same-size/same-mtime writes during the session and while the server is stopped.
- Branch/generator changes, symlinks/read-root changes and narrow/wide authorization.
- Two independent sessions with distinct private overlays, including overlay
  dependencies of otherwise saved roots and save transitions.
- Cancellation/restart mid-index; unverified postings; exclusion/membership changes;
  catalogue/source/request limits and RAM eviction/outstanding leases.
- Two-process old/new publishers, duplicate publishers, interrupted transactions,
  reset fencing and readers/publishers raced with pruning/checkpointing.
- Corrupt/truncated/malformed/oversized records, unsupported format, busy/read-only
  store, low free space/full disk and crashed WAL recovery.
- Disabled initial startup and acknowledged runtime disable with zero cache IO;
  feature availability and safe fallback under every storage failure.
- Stable canonical encoding vectors in independent processes, ordered search
  precedence and manifest/namespace rejection across analyzer/grammar changes.

Use deterministic barriers/fault injection, not sleep-dependent race tests.
Tests are written with each implementation slice; integration tests do not
replace test-first development. Bug fixes require failing regressions first.

### 12.2 Benchmarks and targets

Before implementation, `l4d-zf1b.1` fixes fixture revisions, a representative
reference symbol/query, project/compiler selections, hardware/build profile and
commands. Use the same selected inputs in before/after comparisons. Run enough
repeated samples to report median/p95 and spread: at least 30 measured warm-query
and disabled-path samples per fixed scenario, and five paired cold-index builds.
Elapsed-time overhead gates use the median paired ratio. Report cold-index spread
separately; do not select a better query after seeing results.

Separate comparisons:

1. Pre-change behavior: establishes the combined indexing/cache benefit.
2. The same compact-index implementation with persistence disabled: isolates
   storage overhead and optional-backend behavior.
3. A build of that implementation without the persistence path: isolates disabled
   dispatch overhead; its baseline excludes store IO identically.

| Scenario | Release gate |
| --- | --- |
| Unchanged restart indexing | At least 90% fewer source parse invocations than rebuilding the same index with persistence disabled |
| Fully reconciled warm references | p95 at most 5 seconds for the fixed representative query on the large repository, with necessary candidate work within existing request caps |
| First cold index to equivalent coverage | At most 20% elapsed-time overhead versus the same index with persistence disabled |
| Persistence disabled | At most 5% elapsed-time overhead versus the equivalent implementation without the persistence path on the fixed scenario suite |

Warm references are measured only after content/membership/integrity reconciliation.
Also report reconciliation time and time to useful partial/current complete results;
do not hide startup cost in warm numbers. Count query-demand tree hydration separately
from indexing parses. A 500-unit synthetic public-symbol test must bind the relevant
consumer set, not every unrelated unit. Add a greater-than-10k-unit fixture to prove
catalogue/materialization separation with both backends.

Record reads/stats/hash bytes, parse/bind counts, validation work, decoded/retained
and SQLite-owned byte peaks, RSS, DB/WAL/temp physical sizes, writes/lock waits and
write amplification. Exercise cold build, unchanged restart/request, configuration
switches, one source/include/metadata edit, branch change, eviction, two processes
and interrupted/recovered indexing. Verify admission/backpressure and selective
tree loading without inventing an unmeasured RSS reduction percentage.

An unavailable production corpus leaves its gate open. Failed/noisy targets require
investigation or explicit user-approved revision before release. Do not silently
switch the agreed default to opt-in or check off unmeasured criteria.

Affected-package tests, `cargo fmt --check` and
`cargo clippy --workspace --all-targets -- -D warnings` are required for product
changes. Record pre-existing failures rather than claiming a green suite. This
specification itself neither runs those product gates nor satisfies them.

## 13. Delivery mapping and approval boundary

| Epic child | Responsibility |
| --- | --- |
| `l4d-zf1b.1` | Baselines/counters/benchmark definitions |
| `l4d-zf1b.2` | Canonical identities, exact manifests and encoding vectors |
| `l4d-zf1b.3` | Dedicated backend, container/schema and bounded IO |
| `l4d-zf1b.4` | Compact DTO extraction and fact indexes |
| `l4d-zf1b.5` | Durable observations and current reconciliation |
| `l4d-zf1b.6` | Atomic publication, sealed snapshots and reset fencing |
| `l4d-zf1b.7` | Overlay/privacy/authorization and heap admission |
| `l4d-zf1b.8` | Resumable indexing and catalogue coverage with `l4d-33`/`l4d-20` |
| `l4d-zf1b.9` | Existing LSP query consumers and lazy hydration |
| `l4d-zf1b.10` | Shared policy, GC, checkpointing and recovery |
| `l4d-zf1b.11` | Settings, administration and bounded diagnostics |
| `l4d-zf1b.12` | Integrated differential/concurrency/performance acceptance |

Conceptual architecture approval allowed writing this specification only. User
approval of this written file permits creating the implementation plan, not
product code. The plan must name test-first slices, exact commands and measurable
criteria; the user reviews it and selects execution method before implementation.
No commit/push is authorized by this design workflow.

## References

- [SQLite WAL](https://www.sqlite.org/wal.html): local/same-host requirement,
  short-reader/checkpoint trade-offs and WAL-reset fix.
- [SQLite 3.51.3 release](https://www.sqlite.org/releaselog/3_51_3.html): fixed
  WAL-reset race. Both SQLite references checked on 2026-10-12.
- [Rust DefaultHasher](https://doc.rust-lang.org/std/collections/hash_map/struct.DefaultHasher.html):
  algorithm is not specified or stable across releases; prior investigation on
  `l4d-0jh` records this constraint.
- [Shared resolver architecture](../../shared-resolver-architecture.md): current
  source/observation/authorization ownership; historical dependency-pin prose
  does not override the monorepo path-dependency rules in `AGENTS.md`.
- [Project navigation cache design](2026-09-29-project-navigation-cache-design.md):
  earlier process-local cache. This design does not adopt its stamp-only matching
  rule as persistent freshness proof.
