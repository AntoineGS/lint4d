---
id: TASK-10
title: 'LSP-4: Make memory accounting cover owned heap and outstanding snapshots'
status: Deferred
assignee:
  - '@claude'
created_date: '2026-10-02 23:22'
updated_date: '2026-10-04 02:39'
labels:
  - arch-review
  - lsp
  - runtime
  - agent-todos
milestone: m-2
dependencies: []
modified_files:
  - crates/pascal-lsp/src/project_cache.rs
  - crates/pascal-lsp/src/workspace.rs
priority: high
type: enhancement
ordinal: 10000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-4). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime memory.

**Problem.** Admission counts source bytes (`workspace.rs:452-465`) while
parsed retention is 48 bytes per source byte
(`project_cache.rs:19-24`, `RETAINED_BYTES_PER_SOURCE_BYTE`, marked
`dead_code`). A 256 MiB source allowance therefore permits a multi-gigabyte
index outside the 2 GiB cache budget. Import accounting omits
report/probes/context (`project_cache.rs:1325-1332`, `:1927-1941`), interface
accounting omits probes (`:1991-2010`), pinned entries may exceed the budget,
and evicted `Arc` values stay resident while workers hold them.

**Approach.**
1. Give every cached value a `retained_bytes()` that includes probes,
   reports and contexts; add a unit test that compares it to
   `RETAINED_BYTES_PER_SOURCE_BYTE * source_len` within 20 percent on a
   large RTL fixture.
2. Track outstanding snapshot bytes (values handed to workers) in the same
   budget; eviction counts only when the last `Arc` drops (use a drop guard
   that decrements the counter).
3. Admission for `max_total_bytes` uses the 48x estimate, not raw bytes.

**Tests first.** Unit tests for `retained_bytes()` on each cached type and
for the drop-guard counter.

**Depends on.** nothing. Pairs with LSP-6.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `max_cache_bytes = 512 MiB` on the 24k-file repo keeps RSS under roughly 1 GiB during a references request (measure with `/proc/self/status` VmRSS logged at debug level). Document the number.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Branch perf/lsp-workspace-memory (worktree .worktrees/lsp-workspace-memory), stacked after TASK-13/18.
Root cause (d55c0f6): cache charges are caller estimates — imports count only dependency payloads (import_value_bytes), interface entries ignore probes (interface_value_bytes), every entry clones its own ProjectContext; evicted values that workers still hold are no longer charged anywhere.
Steps:
1. Tests first: retained_bytes() for UnitValue / ImportValue / InterfaceImportsValue (probes, report observations and warnings, watch dirs, expansion, unshared disk text); interface entry charge includes probes (assert via stats); entries of one context share one Arc<ProjectContext>; an evicted value held by a worker stays charged (has_room false, newly stored entry evicted) until the worker drops it.
2. retained_bytes() on each value; workspace store sites pass it; put_interface_imports uses it.
3. Intern contexts per fingerprint in the cache (State.contexts) so contexts cost O(distinct contexts), not O(entries).
4. Outstanding accounting: removing a ready entry whose value Arc is still held records a Weak + bytes; budget checks (evict_to_budget, has_room, stats) prune dead Weaks and include live outstanding bytes.
5. Item 3 of the Approach (48x admission against maxTotalBytes) — evaluate against the documented maxTotalBytes contract; ruling in notes.
AC#1 needs the 24k-file repository; not available here.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From `AGENT_TODOS.md` (section "Workspace-Wide Performance"), folded into this task:

Workspace-wide indexing phase still grows RSS by ~3 GB on `multidev`
for references on a public symbol; consider bounding it.

RED (original code + new tests, `cargo test -p pascal-lsp --lib -- project_cache::cache_tests::interface_entries_charge project_cache::cache_tests::entries_of_one project_cache::cache_tests::evicted_values`): interface_entries_charge_their_probes panicked 'probe paths are retained with the entry'; entries_of_one_project_share_its_context panicked 'one context copy per project, not per entry'; evicted_values_held_by_a_worker_stay_charged_until_dropped panicked 'the worker's copy of A still fills the budget'. The retained_bytes() tests (unit/import/interface) failed to compile on the original code (E0599 no method `retained_bytes`).

Ruling: outstanding snapshot bytes use Weak handles recorded when an entry is removed while its value Arc is still held, pruned at every budget decision (evict_to_budget, has_room, stats), instead of a drop-guard field inside each value. Same accounting at every decision without adding a guard field to every value constructor. Tracked at the cached value (what workers receive from hits), not the ParsedDocument inside a unit, which the main index may legitimately share.

Ruling: contexts are interned per fingerprint in the cache (State.contexts) instead of being charged per entry: entries used to clone the full ProjectContext each (three layers per unit); now they cost O(distinct contexts). Their bytes are not added to the per-entry charge, which keeps entry charges independent of context size.

Ruling: Approach item 3 (admission for maxTotalBytes by the 48x estimate) is not done: README documents maxTotalBytes as source bytes, and 48x against the 256 MiB default admits ~5.3 MiB of source. Split into TASK-131 for a user decision.

AC#1 not verified: needs the 24k-file production repository and a VmRSS measurement there; not available in this environment. No debug VmRSS logging was added (server.rs is being changed on other branches).

Implemented on branch perf/lsp-workspace-memory (worktree .worktrees/lsp-workspace-memory), awaiting review and merge.

Fix round 1 (commit bf8fa0e). Review finding: the Weak<value> outstanding accounting tracked Arc<UnitValue>/ImportValue/InterfaceImportsValue, which production drops right after a hit, so outstanding_bytes stayed ~0, and a racing transient hit was charged. Replaced with leases: request workspaces (cache_epoch is Some: Workspace::from_analysis_input and the rename/snapshot loader) call ProjectCache::lease_unit for each cached parse they keep in their index (index_source_with_budget unit hits, closure insert_provider). A lease holds the Arc<ParsedDocument>; the cache tracks which parse addresses entries hold (cached_parses) and charges a leased parse as outstanding only when no entry holds it, until the last lease drops. Leases move with the loader's index into RenameSnapshot (_cache_leases). The long-lived workspace never leases, so main-index holds are excluded. Transient hits are not charged.

Ruling: import payloads (Arc<ResolvedImports>, Arc<ResolutionReport>) are not leased: load_imports_with_session_cache holds them only for the call, and dependency texts are copied into the index, so nothing keeps them after the request returns. Interface values from the closure walk are also read only during the walk.

RED (fix round 1): parses_a_snapshot_took_from_the_cache_stay_charged_after_eviction (InheritedFixture: warm Main+Derived, build_snapshot inserts Base's cached parse via the closure walk, then invalidate_after_overflow) on bf8fa0e^: 'the snapshot still holds Base's parse: 0 bytes charged'. GREEN: charged while the snapshot lives, 0 after drop. Cache-level tests rewritten to hold a lease (a_leased_parse_stays_charged_after_eviction_until_the_lease_drops), plus transient_hits_are_not_charged_after_eviction and a_re_cached_parse_is_charged_by_its_entry_again. The old test holding a Lookup::Hit Arc was removed.

Fix round 1: the unit retained_bytes test no longer claims 'within 20% of the measured factor' (circular: retained_bytes is defined as the 48x estimate plus extras). The 48x factor is calibrated only by the RSS-based ignored test measure_retained_bytes_per_source_byte; tree-sitter trees live in C allocations that no in-process count sees. The test now checks only what the estimate adds (probes, unshared disk text) and that a bare parse is exactly the estimate plus the struct.

Limit: a lease lives until its request workspace or snapshot drops, even if that index removed the parse earlier (over-charge, bounded by the request's lifetime). A NavigationIndex rebuilt from a snapshot (rebind_with_replaced_source_for_fix_all) that outlives the snapshot is not charged.

Fix round 2 (commit 678ee55): leases hold a Weak<ParsedDocument> (address stability only), so a parse the request's index and the cache both dropped is freed; prune_dead_leases (evict_to_budget/has_room/stats) stops charging it. Workspace::cache_leases is keyed by URI: replaced on every (re)index of the URI, dropped at the index.remove sites, cleared when the index is replaced (also in the rename loader). Cold path: after store_unit a request leases the stored parse via lease_stored_parse (only if the cache took it). lease_unit on an already-evicted parse charges it and runs evict_to_budget at once. Correction to round 1: an eviction racing a hit is charged once the request keeps (leases) that parse; hits not kept are never charged.

RED (fix round 2): leasing_a_parse_evicted_after_its_hit_keeps_the_budget 'the late charge evicts to budget'; parses_a_cold_request_stores_stay_charged_while_it_keeps_them 'the worker still holds the parses it stored'; a_request_that_drops_a_leased_parse_releases_it_and_its_charge 'nothing keeps a parse both the request and the cache dropped'. GREEN: all pass; pascal-lsp lib 721 passed; protocol definition 9 / closed 22 / references 43; navigation 317; fmt and clippy clean.

Review minors after fix round 2 (controller), to pick up with TASK-131 or separately: (1) release_parse (project_cache.rs ~2884-2898) with the evict_to_budget loop (~3044) can briefly over-evict when a stale lease exists (sources: field drop order index before cache_leases in workspace.rs ~2732/~2741 and rename.rs ~545; discard_reconstructible_state replaces the index without clearing leases; the cancellation return at workspace.rs ~7928 before the old lease is removed at ~7932) — mark a lease evicted only if the parse has holders beyond the entry, or prune inside the loop; (2) workspace.rs ~8011-8016: a gap between store_unit and lease_stored_parse lets a concurrent eviction leave a stored parse unleased — have store_unit return the lease under the same lock.

Merged into master as 16139a2 (branch perf/lsp-workspace-memory, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.

Deferred: code merged; AC #2 met (evicted parses held by request workspaces/snapshots stay charged via weak URI-keyed leases). AC #1 (max_cache_bytes = 512 MiB keeps RSS under ~1 GiB on the 24k-file multidev repo) needs a measurement on that repo, which is not available on the dev machine; the maxTotalBytes semantics question moved to TASK-131. Unblock: the user runs the RSS measurement on multidev and records it here.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: cache charges were caller estimates that ignored probes, reports, watch dirs and expansions; each entry cloned its own ProjectContext; values evicted while workers held them were charged nowhere.

Change (commit f09f96e): UnitValue/ImportValue/InterfaceImportsValue::retained_bytes() (parse at the measured 48x factor plus expansion, probes, unshared disk text; dependency payloads plus bindings, report observations/warnings, probes, watch dirs; bindings plus probes). Workspace store sites and put_interface_imports charge retained_bytes(). Contexts are interned per fingerprint. Removing a ready entry whose value is still held records it as outstanding; evict_to_budget/has_room/stats prune released ones and count live outstanding bytes in the budget (CacheStats.outstanding_bytes).

Tests: retained_bytes for each type (compile-RED on the original), interface probe charge, shared context, evicted-but-held value stays charged until dropped (assertion-RED on the original). pascal-lsp lib 707 passed; fmt and clippy clean.

Not done: AC#1 (RSS on the 24k-file repo) needs that repository; admission by 48x estimate split into TASK-131 for a decision on the documented maxTotalBytes meaning.

Fix round 1 (bf8fa0e): outstanding accounting now leases the parses that request workspaces and snapshots keep (closure walk, unit hits in index_source), not the transient value Arcs; the main workspace never leases. The test holds the real snapshot path (Base inserted via the closure walk; 0 bytes charged before, charged after eviction until the snapshot drops). The circular 20% retained_bytes claim was removed. AC#1 is still unverified (needs the 24k-file repo).

Fix round 2 (678ee55): leases no longer keep parses alive (Weak, URI-keyed, dropped when the request's index removes or replaces the parse); parses a cold request stores are leased; a late lease of an evicted parse evicts to budget at once. AC#1 is still unverified.
<!-- SECTION:FINAL_SUMMARY:END -->
