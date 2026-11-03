//! Which documents a workspace index covers (l4d-33 U1, A9, A10): the
//! compilable units of one context, read from the shared directory
//! catalogues (l4d-20) so that a second context, or a second request, lists
//! no directory the first already listed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lsp_types::Url;
use pascal_cache::{ContextKey as CacheContextKey, Hint};
use pascal_core::facts::{
    FactBudget, FactError, IncludeSlotOutcome, NotPersistable, include_slots,
};
use pascal_project::ProjectContext;

use super::baseline::HintReuse;
use super::catalogues::CatalogueScope;
use super::eligible::{ProvenRecord, RecordInputs, Unindexed, prove_existing, publish_proven};
use super::facts::SourceFacts;
use super::persist::{Reuse, ReuseMiss};
use super::rename::{
    CANCELLATION_MESSAGE, IndexedSources, WorkspaceInput, is_safe_source_path, path_key,
    project_metadata_context, snapshot_context_for_uri,
};
use super::{
    ContextKey, PathStamp, Workspace, absolute_path, check_workspace_cancel, disk_stamp,
    path_stamp, path_stamp_from_project_read, path_starts_with_ci,
};
use crate::catalogue::{Catalogue, CatalogueKey, MAX_CATALOGUE_ENTRIES};
use crate::project_cache::{IndexAnchor, IndexMembership, PublishOutcome, ReservationKind};
use crate::workspace_index::{
    CandidateQuery, Candidates, ContextIndex, IndexCoverage, IndexedSymbol,
    MAX_UNVERIFIED_CANDIDATES, MAX_UNVERIFIED_SYMBOL_CANDIDATES, MemberFacts, MemberStamp,
    MemberState, RETRYING_REFUSAL, SymbolKey,
};

/// Directories whose owner one call may discover; the rest stay pending and
/// the membership says so.
pub(crate) const MAX_REQUEST_OWNER_DISCOVERY: usize = 256;

/// Directories whose owner one interactive request (references, workspace
/// symbols) may discover for its outside sources (A8 item 5); the rest are
/// the outside-source gap and are discovered by later requests.
pub(crate) const MAX_INTERACTIVE_OWNER_DISCOVERY: usize = 32;

/// At most this many contexts are searched by one indexed request.
pub(crate) const MAX_TARGET_CONTEXTS: usize = 16;

/// What one request has spent on owner discovery: the directories it
/// discovered (at most `cap`), and which owner contexts it already found
/// fresh or stale.
struct OwnerDiscovery {
    cap: usize,
    discovered: usize,
    fresh: HashMap<ContextKey, bool>,
}

impl OwnerDiscovery {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            discovered: 0,
            fresh: HashMap::new(),
        }
    }
}

/// A workspace with at most this many Pascal sources and descriptors under
/// its roots still answers `workspace/symbol` the old way (A16).
pub(crate) const SMALL_WORKSPACE_SOURCES: usize = 2_048;
pub(crate) const SMALL_WORKSPACE_PROJECTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetPurpose {
    References,
    WorkspaceSymbols,
}

/// The contexts one indexed request searches, in priority order.
#[derive(Debug, Default)]
pub(crate) struct Targets {
    pub(crate) contexts: Vec<(ContextKey, IndexAnchor)>,
    pub(crate) gaps: Vec<String>,
    /// Targets that came from the requesting document, open documents or the
    /// configured project (A8 items 1-3).
    pub(crate) from_open_or_configured: usize,
    pub(crate) small_workspace: bool,
    /// The target the requesting document resolved to.
    pub(crate) requesting: Option<ContextKey>,
}

impl Targets {
    fn holds(&self, key: &ContextKey) -> bool {
        self.contexts.iter().any(|(held, _)| held == key)
    }

    /// Whether the target at `position` is searched in full: its membership
    /// computed, its unverified members read, its index requested. The
    /// others (A8 items 4 and 5) contribute only the members their existing
    /// index holds Fresh, unless the workspace is small enough to walk.
    fn searched(&self, position: usize) -> bool {
        self.small_workspace || position < self.from_open_or_configured
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct CapOverrides {
    owner_discovery: Option<usize>,
    interactive_discovery: Option<usize>,
    unverified_symbols: Option<usize>,
    small_sources: Option<usize>,
    small_projects: Option<usize>,
    max_targets: Option<usize>,
    unverified: Option<usize>,
}

#[cfg(test)]
thread_local! {
    static CAP_OVERRIDES: std::cell::Cell<CapOverrides> = const {
        std::cell::Cell::new(CapOverrides {
            owner_discovery: None,
            interactive_discovery: None,
            unverified_symbols: None,
            small_sources: None,
            small_projects: None,
            max_targets: None,
            unverified: None,
        })
    };
}

#[cfg(test)]
thread_local! {
    static DESCRIPTOR_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn descriptor_attempts() -> usize {
    DESCRIPTOR_ATTEMPTS.with(std::cell::Cell::get)
}

#[cfg(test)]
thread_local! {
    static MEMBERSHIP_COMPUTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Memberships computed on this thread since the last call.
#[cfg(test)]
fn take_membership_computes() -> usize {
    MEMBERSHIP_COMPUTES.with(|count| count.replace(0))
}

fn owner_discovery_cap() -> usize {
    #[cfg(test)]
    if let Some(cap) = CAP_OVERRIDES.with(|caps| caps.get().owner_discovery) {
        return cap;
    }
    MAX_REQUEST_OWNER_DISCOVERY
}

fn interactive_discovery_cap() -> usize {
    #[cfg(test)]
    if let Some(cap) = CAP_OVERRIDES.with(|caps| caps.get().interactive_discovery) {
        return cap;
    }
    MAX_INTERACTIVE_OWNER_DISCOVERY
}

fn small_workspace_sources() -> usize {
    #[cfg(test)]
    if let Some(cap) = CAP_OVERRIDES.with(|caps| caps.get().small_sources) {
        return cap;
    }
    SMALL_WORKSPACE_SOURCES
}

fn small_workspace_projects() -> usize {
    #[cfg(test)]
    if let Some(cap) = CAP_OVERRIDES.with(|caps| caps.get().small_projects) {
        return cap;
    }
    SMALL_WORKSPACE_PROJECTS
}

fn max_target_contexts() -> usize {
    #[cfg(test)]
    if let Some(cap) = CAP_OVERRIDES.with(|caps| caps.get().max_targets) {
        return cap;
    }
    MAX_TARGET_CONTEXTS
}

/// Sets the unverified-candidate bound for the tests of other modules.
#[cfg(test)]
pub(crate) fn test_set_unverified_cap(cap: Option<usize>) {
    CAP_OVERRIDES.with(|caps| {
        let mut value = caps.get();
        value.unverified = cap;
        caps.set(value);
    });
}

/// Sets the workspace-symbol unverified bound for the tests of other modules.
#[cfg(test)]
pub(crate) fn test_set_unverified_symbol_cap(cap: Option<usize>) {
    CAP_OVERRIDES.with(|caps| {
        let mut value = caps.get();
        value.unverified_symbols = cap;
        caps.set(value);
    });
}

/// Sets the small-workspace source cap (A16) for the tests of other modules.
#[cfg(test)]
pub(crate) fn test_set_small_workspace_sources(cap: Option<usize>) {
    CAP_OVERRIDES.with(|caps| {
        let mut value = caps.get();
        value.small_sources = cap;
        caps.set(value);
    });
}

#[cfg(test)]
thread_local! {
    static TARGET_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes `index_targets` fail, for the tests of other modules.
#[cfg(test)]
pub(crate) fn test_set_target_error(fail: bool) {
    TARGET_ERROR.with(|slot| slot.set(fail));
}

fn max_unverified_candidates() -> usize {
    #[cfg(test)]
    if let Some(cap) = CAP_OVERRIDES.with(|caps| caps.get().unverified) {
        return cap;
    }
    MAX_UNVERIFIED_CANDIDATES
}

fn max_unverified_symbol_candidates() -> usize {
    #[cfg(test)]
    if let Some(cap) = CAP_OVERRIDES.with(|caps| caps.get().unverified_symbols) {
        return cap;
    }
    MAX_UNVERIFIED_SYMBOL_CANDIDATES
}

/// What an index batch may do for a member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexMode {
    /// Reuse only: never loads or parses (the pass before the crawl, D33).
    Reuse,
    /// Reuse, then extract what reuse could not prove.
    Full,
}

/// How many pending members one `advance_workspace_index` call takes, for
/// how long, and what it may do with them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IndexBound {
    units: usize,
    time: Option<Duration>,
    mode: IndexMode,
}

impl IndexBound {
    /// `Full`, without a time bound.
    pub(crate) fn units(units: usize) -> Self {
        Self {
            units,
            time: None,
            mode: IndexMode::Full,
        }
    }

    /// The reuse pass: 512 members or 200 ms, never a parse.
    #[cfg_attr(not(test), allow(dead_code))] // the warmer's reuse pass (l4d-zf1b.8)
    pub(crate) fn reuse_pass() -> Self {
        Self {
            units: 512,
            time: Some(Duration::from_millis(200)),
            mode: IndexMode::Reuse,
        }
    }

    /// An extraction batch: 64 members or 2 s.
    #[cfg_attr(not(test), allow(dead_code))] // the warmer's extraction batches (l4d-zf1b.8)
    pub(crate) fn extraction() -> Self {
        Self {
            units: 64,
            time: Some(Duration::from_secs(2)),
            mode: IndexMode::Full,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // the warmer's reuse pass (l4d-zf1b.8)
    pub(crate) fn with_mode(self, mode: IndexMode) -> Self {
        Self { mode, ..self }
    }

    #[cfg(test)]
    pub(crate) fn all() -> Self {
        Self::units(usize::MAX)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexProgress {
    /// The context the batch indexed; `None` only before it was resolved.
    pub(crate) key: Option<ContextKey>,
    pub(crate) mode: IndexMode,
    pub(crate) indexed: usize,
    pub(crate) refreshed: usize,
    pub(crate) refused: usize,
    /// Members still `Unindexed` or `Stale` after the batch.
    pub(crate) pending: usize,
    pub(crate) generation: u64,
    /// No closed member is pending and the share has room.
    pub(crate) complete: bool,
    /// Pending members open in the editor: never published, always candidates
    /// of a query (A4), so they do not hold `complete` back.
    pub(crate) skipped_open: usize,
    /// Members left pending because their load failed for a reason that may
    /// pass (a source cache limit, a missing context). A batch with
    /// `deferred > 0` and no `indexed`, `refreshed` or `refused` made no
    /// progress: the caller must not requeue it in a loop.
    pub(crate) deferred: usize,
    /// The batch stopped because the index share has no room left; asking
    /// again cannot help until memory is released.
    pub(crate) memory: bool,
    /// Members whose facts were limited: unverified candidates of every
    /// request until they change.
    pub(crate) limited: usize,
    /// Pending closed members no batch has deferred yet and this batch did
    /// not try (a member skipped without a load stays untried otherwise).
    pub(crate) untried: usize,
    /// Members whose facts came from the store.
    pub(crate) reused: usize,
    /// Members the store had nothing for (now tried for reuse).
    pub(crate) reuse_missed: usize,
    /// Pending closed members not tried for reuse since they last changed.
    pub(crate) reuse_pending: usize,
    /// Retryable reuse misses this batch (left untried for reuse).
    pub(crate) reuse_deferred: usize,
    /// Members whose artifact and record this batch published.
    pub(crate) persisted: usize,
    /// Members a held snapshot lists without an artifact, deferred to be
    /// retried after every other member.
    pub(crate) retrying: usize,
}

impl IndexProgress {
    fn new(key: ContextKey, mode: IndexMode) -> Self {
        Self {
            key: Some(key),
            mode,
            indexed: 0,
            refreshed: 0,
            refused: 0,
            pending: 0,
            generation: 0,
            complete: false,
            skipped_open: 0,
            deferred: 0,
            memory: false,
            limited: 0,
            untried: 0,
            reused: 0,
            reuse_missed: 0,
            reuse_pending: 0,
            reuse_deferred: 0,
            persisted: 0,
            retrying: 0,
        }
    }

    /// Another batch can gain something: this one settled a member, or
    /// members no batch has tried yet are still pending. A batch that only
    /// deferred members already deferred is not requeued (no livelock); a
    /// request or invalidation asks for the anchor again. A reuse batch is
    /// requeued while members are left to try and it tried one for good, so
    /// a run of retryable misses alone does not loop either.
    pub(crate) fn worth_requeue(&self) -> bool {
        !self.complete
            && !self.memory
            && (self.indexed + self.refreshed + self.refused + self.limited + self.reused > 0
                || (self.deferred > 0 && self.untried > 0)
                || (self.mode == IndexMode::Reuse
                    && self.reuse_pending > 0
                    && self.reuse_missed > 0))
    }
}

/// A read failure that repeats for the same bytes or path (A6): a file over
/// `maxFileBytes`, an unreadable or irregular file, a symlink or path outside
/// the roots, an include expansion over a compile-time limit (a request
/// reading it directly fails the same way). Cancellation, budgets and
/// internal errors are transient.
pub(crate) fn is_stable_load_refusal(message: &str) -> bool {
    if message == CANCELLATION_MESSAGE || message.to_ascii_lowercase().contains("budget") {
        return false;
    }
    message.starts_with("cannot read ")
        || (message.starts_with("include expansion failed: ") && message.contains("limit"))
        || message.starts_with("cannot inspect ")
        || message.starts_with("source path is outside the effective project read roots")
        || message.contains("the configured per-file limit")
        || message.contains("is a symlink and not an authorized workspace source")
        || message.ends_with(" is not a regular file")
}

fn is_unit_source(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pas"))
}

#[cfg(test)]
type LoadHook = Box<dyn FnMut(&Path)>;

#[cfg(test)]
thread_local! {
    static AFTER_LOAD: std::cell::RefCell<Option<LoadHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_after_load_hook(hook: Option<LoadHook>) {
    AFTER_LOAD.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(test)]
thread_local! {
    /// Runs after each reuse attempt, once its facts are dropped.
    static AFTER_REUSE: std::cell::RefCell<Option<LoadHook>> =
        const { std::cell::RefCell::new(None) };
    /// Runs after an extracted member is indexed, before it is persisted.
    static BEFORE_PERSIST: std::cell::RefCell<Option<LoadHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_after_reuse_hook(hook: Option<LoadHook>) {
    AFTER_REUSE.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(test)]
fn set_before_persist_hook(hook: Option<LoadHook>) {
    BEFORE_PERSIST.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(test)]
fn run_hook(
    slot: &'static std::thread::LocalKey<std::cell::RefCell<Option<LoadHook>>>,
    path: &Path,
) {
    if let Some(mut hook) = slot.with(|slot| slot.borrow_mut().take()) {
        hook(path);
        slot.with(|slot| *slot.borrow_mut() = Some(hook));
    }
}

#[cfg(test)]
thread_local! {
    /// Members whose indexing fails with an error that is no cancellation.
    static MEMBER_ERROR: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn set_member_error(path: Option<PathBuf>) {
    set_member_errors(path.into_iter().collect());
}

#[cfg(test)]
thread_local! {
    /// Members whose stamp fails, as for a file the listing returns but a
    /// stat cannot reach.
    static UNSTAMPABLE: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn set_unstampable(paths: Vec<PathBuf>) {
    UNSTAMPABLE.with(|slot| *slot.borrow_mut() = paths);
}

#[cfg(test)]
fn set_member_errors(paths: Vec<PathBuf>) {
    MEMBER_ERROR.with(|slot| *slot.borrow_mut() = paths);
}

#[cfg(test)]
type CacheHook = Box<dyn FnMut(&crate::project_cache::ProjectCache)>;

#[cfg(test)]
thread_local! {
    /// Runs after `indexed_sources` read one target's index, before it looks
    /// up the membership's catalogues.
    static AFTER_TARGET_VIEW: std::cell::RefCell<Option<CacheHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_after_target_view_hook(hook: Option<CacheHook>) {
    AFTER_TARGET_VIEW.with(|slot| *slot.borrow_mut() = hook);
}

enum MemberOutcome {
    /// Indexed; `persisted` when its artifact and record were published.
    Published {
        persisted: bool,
    },
    /// The facts were limited: an unverified candidate until it changes.
    Limited,
    /// The load failed for a reason that may pass: still pending.
    Deferred(String),
    Refused,
    /// Not a member any more, or an unsaved buffer: nothing to publish.
    Skipped,
    Memory,
}

/// What asking the store for one member's facts gave.
enum ReuseOutcome {
    /// Indexed from the store.
    Reused,
    /// The store cannot prove facts for the current inputs: extract.
    Missed,
    /// A miss that may pass (budget, cancellation, a store bypass): the
    /// member stays untried for reuse.
    Retry,
    /// The index share refused the member.
    Memory,
    /// No baseline view, already tried, or no store: nothing was asked.
    NoStore,
    /// The held snapshot records a previous refusal (no artifact).
    Retrying,
    /// The request was cancelled while the store was asked.
    Cancelled,
}

/// Why an extracted member was not persisted. None is an error: the member
/// stays memory-only, and a later process extracts it again. The reasons are
/// diagnostics (traces).
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum PersistSkip {
    /// No baseline view, a context built from unsaved project input, or no
    /// store.
    Unavailable,
    Prepare(super::persist::PersistError),
    /// `maxCacheBytes` had no room for the publication's transient bytes.
    Memory,
    Ineligible(super::eligible::Ineligible),
    Store(pascal_cache::Bypass),
}

/// The slot paths and stamps of reused facts: slot 0 is the member (its
/// stamp taken before reuse read it), then one slot per include in slot
/// order, normalized as `source_slot_paths` does. An include's stamp is
/// the one reuse took before validating its bytes; without one the member
/// stays unverified on every request, as for an unresolved include (A7).
fn reused_slots(
    path: &Path,
    own_stamp: &PathStamp,
    facts: &pascal_core::facts::ArtifactFacts,
    include_stamps: &[(PathBuf, PathStamp)],
) -> (Vec<Option<PathBuf>>, Vec<MemberStamp>) {
    let mut includes = facts.includes.iter().collect::<Vec<_>>();
    includes.sort_by_key(|include| include.slot);
    let mut slots = vec![None];
    slots.extend(
        includes
            .iter()
            .map(|include| include.source.clone().map(absolute_path)),
    );
    let stamps = slot_stamps(
        path,
        own_stamp.clone(),
        &slots,
        include_stamps.iter().cloned(),
    );
    (slots, stamps)
}

/// The stamps of a member's slots: its own, then each resolved include's
/// from `observed` (the stamps taken before its bytes were read), matched
/// by `path_key`; an include without one keeps no stamp (A7).
fn slot_stamps(
    path: &Path,
    own_stamp: PathStamp,
    slots: &[Option<PathBuf>],
    observed: impl IntoIterator<Item = (PathBuf, PathStamp)>,
) -> Vec<MemberStamp> {
    let observed: HashMap<String, PathStamp> = observed
        .into_iter()
        .map(|(held, stamp)| (path_key(&absolute_path(held)), stamp))
        .collect();
    let mut stamps = vec![MemberStamp {
        path: path.to_path_buf(),
        stamp: Some(own_stamp),
    }];
    stamps.extend(slots.iter().skip(1).flatten().map(|slot| MemberStamp {
        path: slot.clone(),
        stamp: observed.get(&path_key(slot)).cloned(),
    }));
    stamps
}

impl Workspace {
    /// The paths behind the slots of the indexed `uri`: slot 0 is `None`
    /// (`MemberFacts::from_facts` fills it), then one entry per include slot,
    /// `None` for an include that did not resolve.
    pub(crate) fn source_slot_paths(&self, uri: &Url) -> Vec<Option<PathBuf>> {
        let mut paths = vec![None];
        let Some(parsed) = self.index.parsed_document(uri) else {
            return paths;
        };
        let Some(session) = parsed.compile_time_session().map(|analysis| &**analysis) else {
            return paths;
        };
        let Ok(slots) = include_slots(session) else {
            return paths;
        };
        paths.extend(slots.iter().map(|slot| match &slot.outcome {
            IncludeSlotOutcome::Expanded { source, .. } => Some(absolute_path(source.path.clone())),
            IncludeSlotOutcome::Unresolved(_) => None,
        }));
        paths
    }

    /// Indexes up to `bound` pending members of the context `anchor` names
    /// (l4d-33 U3). Each member is committed as it is published, so a failed
    /// or cancelled batch keeps its work. Parses are not published to the
    /// cache (A13). A transient failure returns `Err` with the member still
    /// pending; a stable one refuses it (A6).
    pub(crate) fn advance_workspace_index(
        &mut self,
        anchor: &IndexAnchor,
        bound: IndexBound,
        before_io: &mut dyn FnMut() -> Result<(), String>,
        cancel: &AtomicBool,
    ) -> Result<IndexProgress, String> {
        let previous = std::mem::replace(&mut self.index_build, true);
        let result = self.advance_index_batch(anchor, bound, before_io, cancel);
        self.index_build = previous;
        self.pinned_context = None;
        result
    }

    fn advance_index_batch(
        &mut self,
        anchor: &IndexAnchor,
        bound: IndexBound,
        before_io: &mut dyn FnMut() -> Result<(), String>,
        cancel: &AtomicBool,
    ) -> Result<IndexProgress, String> {
        let key = self.index_context_for(anchor, cancel)?;
        // Skipped files prune the contexts no document uses, and a build
        // view holds a document only while it loads one.
        self.pinned_context = Some(key.clone());
        self.ensure_index_membership(&key, anchor, cancel)?;
        if self.is_baseline() && self.persistent_cache().is_enabled() {
            self.release_evicted_holds();
            self.adopt_baseline_hint_once(&key);
        }
        let mut progress = IndexProgress::new(key.clone(), bound.mode);
        let started = Instant::now();
        // Open members are skipped, so ask for enough to fill the bound.
        let wanted = bound.units.saturating_add(self.open_documents.len());
        let pending = match bound.mode {
            IndexMode::Reuse => self.project_cache.reuse_pending_index_members(&key, wanted),
            IndexMode::Full => self
                .project_cache
                .read_index(&key, |index, _| index.pending(wanted))
                .unwrap_or_default(),
        };
        // Members this batch tried, skipped ones included, are never untried.
        let mut tried = HashSet::new();
        for path in pending {
            if tried.len() >= bound.units
                || bound.time.is_some_and(|limit| started.elapsed() >= limit)
            {
                break;
            }
            if self.is_open_member(&path) {
                continue;
            }
            tried.insert(path_key(&path));
            // The stamp precedes the bytes it describes (A7); `before_io`
            // runs between them.
            let own_stamp = path_stamp(&path);
            #[cfg(test)]
            let own_stamp =
                own_stamp.filter(|_| !UNSTAMPABLE.with(|slot| slot.borrow().contains(&path)));
            before_io()?;
            let Some(own_stamp) = own_stamp else {
                // Gone behind the catalogues' back: relist on the next look.
                self.project_cache.invalidate_path(&path);
                continue;
            };
            // A persisting view proves a member without a record again
            // through the store (stat and hash, no parse), so the snapshot
            // can still list it after an invalidation.
            let reprove = self.may_publish(&key)
                && self.persistent_cache().is_enabled()
                && !self.project_cache.index_member_has_record(&key, &path);
            if !reprove && self.refresh_unchanged_member(&key, &path, &own_stamp) {
                progress.refreshed += 1;
                continue;
            }
            let reuse = self.reuse_index_member(&key, &path, &own_stamp, cancel);
            #[cfg(test)]
            run_hook(&AFTER_REUSE, &path);
            let asked = !matches!(reuse, ReuseOutcome::NoStore);
            match reuse {
                ReuseOutcome::Reused => {
                    progress.reused += 1;
                    continue;
                }
                ReuseOutcome::Missed => {
                    self.project_cache.mark_index_reuse_tried(&key, &path);
                    progress.reuse_missed += 1;
                }
                ReuseOutcome::Retry => progress.reuse_deferred += 1,
                // A previous refusal is retried after every other member, by
                // a later batch (with no hint, members keep path order).
                ReuseOutcome::Retrying => {
                    self.project_cache.mark_index_reuse_tried(&key, &path);
                    self.project_cache
                        .defer_index_member(&key, &path, RETRYING_REFUSAL);
                    progress.retrying += 1;
                    continue;
                }
                ReuseOutcome::Memory => {
                    progress.memory = true;
                    break;
                }
                ReuseOutcome::NoStore => {}
                ReuseOutcome::Cancelled => return Err(CANCELLATION_MESSAGE.to_owned()),
            }
            if bound.mode == IndexMode::Reuse {
                continue;
            }
            // The store work above may have taken a while; without it the
            // member's first check still holds.
            if asked {
                before_io()?;
            }
            // One member's failure never stops the batch: it stays pending.
            let outcome = match self.index_member(&key, &path, own_stamp, cancel) {
                Ok(outcome) => outcome,
                Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
                Err(error) => MemberOutcome::Deferred(error),
            };
            match outcome {
                MemberOutcome::Published { persisted } => {
                    progress.indexed += 1;
                    progress.persisted += usize::from(persisted);
                }
                MemberOutcome::Refused => progress.refused += 1,
                MemberOutcome::Limited => progress.limited += 1,
                MemberOutcome::Skipped => {}
                MemberOutcome::Deferred(reason) => {
                    crate::trace::trace!("  index member {} deferred: {reason}", path.display());
                    self.project_cache.defer_index_member(&key, &path, &reason);
                    progress.deferred += 1;
                }
                MemberOutcome::Memory => {
                    progress.memory = true;
                    break;
                }
            }
        }
        self.ensure_index_membership(&key, anchor, cancel)?;
        let (all_pending, untried, reuse_pending, generation, retrying) = self
            .project_cache
            .read_index(&key, |index, _| {
                let pending = index.pending(usize::MAX);
                let retrying = pending
                    .iter()
                    .filter(|path| index.retrying(path))
                    .cloned()
                    .collect::<Vec<_>>();
                (
                    pending,
                    index.untried(),
                    index.reuse_pending(usize::MAX),
                    index.generation(),
                    retrying,
                )
            })
            .unwrap_or_default();
        progress.reuse_pending = reuse_pending
            .iter()
            .filter(|path| !self.is_open_member(path))
            .count();
        let open = all_pending
            .iter()
            .filter(|path| self.is_open_member(path))
            .count();
        progress.untried = untried
            .iter()
            .filter(|path| !tried.contains(&path_key(path)) && !self.is_open_member(path))
            .count();
        progress.pending = all_pending.len() - open;
        progress.skipped_open = open;
        progress.generation = generation;
        // Members retrying a previous refusal are coverage gaps, not
        // pending work (H4/R8-3).
        let retrying = retrying
            .iter()
            .filter(|path| !self.is_open_member(path))
            .count();
        progress.complete = progress.pending == retrying && !progress.memory;
        Ok(progress)
    }

    /// An open document (rejected ones included) is answered from the
    /// editor's buffer, never from the index.
    pub(super) fn is_open_member(&self, path: &Path) -> bool {
        Url::from_file_path(path).is_ok_and(|uri| {
            self.open_documents.contains_key(&uri)
                || self
                    .open_documents
                    .contains_key(&super::canonical_file_uri(&uri))
        })
    }

    fn ensure_index_membership(
        &mut self,
        key: &ContextKey,
        anchor: &IndexAnchor,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        self.ensure_index_membership_computed(key, anchor, cancel)
            .map(drop)
    }

    /// Like `ensure_index_membership`; returns the membership when it had to
    /// be computed.
    fn ensure_index_membership_computed(
        &mut self,
        key: &ContextKey,
        anchor: &IndexAnchor,
        cancel: &AtomicBool,
    ) -> Result<Option<IndexMembership>, String> {
        if self.project_cache.index_membership_current(key) {
            return Ok(None);
        }
        let membership = self.index_membership(key, cancel)?;
        self.project_cache.set_index_membership(
            key.clone(),
            anchor.clone(),
            membership.clone(),
            self.options.limits.max_workspace_units,
        );
        Ok(Some(membership))
    }

    /// The directories of the catalogues `scopes` name, while each is still
    /// stored under its recorded generation; `None` otherwise.
    fn membership_listing(&self, scopes: &[(CatalogueKey, u64)]) -> Option<MembershipListing> {
        let mut listing = MembershipListing::default();
        for (scope, generation) in scopes {
            let shared = self
                .project_cache
                .catalogue(scope)
                .filter(|shared| *generation != 0 && shared.generation == *generation)?;
            let catalogue = &shared.catalogue;
            listing.incomplete |= !catalogue.is_complete()
                || catalogue.limit().is_some()
                || !catalogue.unreadable().is_empty();
            listing
                .directories
                .extend(catalogue.directories().iter().cloned());
        }
        Some(listing)
    }

    /// A stale member whose own file and every include still prove unchanged
    /// keeps its facts: only the stamps are renewed. The stamp paths are the
    /// stored ones, so the charge stays fixed; a member with an unresolved
    /// include slot is always read again (a created file must be noticed).
    fn refresh_unchanged_member(
        &self,
        key: &ContextKey,
        path: &Path,
        own_stamp: &crate::workspace::PathStamp,
    ) -> bool {
        let Some((stored, resolved)) = self
            .project_cache
            .read_index(key, |index, _| {
                if index.state_of(path) != Some(MemberState::Stale) {
                    return None;
                }
                let resolved = index
                    .slot_paths_of(path)?
                    .iter()
                    .skip(1)
                    .all(Option::is_some);
                Some((index.stamps_of(path)?.to_vec(), resolved))
            })
            .flatten()
        else {
            return false;
        };
        if !resolved || stored.first().is_none_or(|first| first.path != path) {
            return false;
        }
        let mut current = Vec::with_capacity(stored.len());
        for (position, old) in stored.iter().enumerate() {
            let now = if position == 0 {
                Some(own_stamp.clone())
            } else {
                path_stamp(&old.path)
            };
            let unchanged =
                matches!((&old.stamp, &now), (Some(old), Some(now)) if old.proves_unchanged(now));
            if !unchanged {
                return false;
            }
            current.push(MemberStamp {
                path: old.path.clone(),
                stamp: now,
            });
        }
        self.project_cache.refresh_index_member(key, path, current)
    }

    fn index_member(
        &mut self,
        key: &ContextKey,
        path: &Path,
        own_stamp: crate::workspace::PathStamp,
        cancel: &AtomicBool,
    ) -> Result<MemberOutcome, String> {
        let Ok(uri) = Url::from_file_path(path) else {
            return Ok(self.refuse_member(
                key,
                path,
                "the path has no file URI".to_owned(),
                cancel,
            ));
        };
        #[cfg(test)]
        if MEMBER_ERROR.with(|slot| slot.borrow().iter().any(|failing| failing == path)) {
            return Err("an injected member error".to_owned());
        }
        let held = self.index.contains(&uri);
        self.load_refusal = None;
        let loaded = self.load_source_with_cancel(&uri, key, &HashSet::new(), Some(cancel));
        #[cfg(test)]
        run_hook(&AFTER_LOAD, path);
        let outcome = match loaded {
            Err(error) if is_stable_load_refusal(&error) => {
                Ok(self.refuse_unless_vanished(key, path, error, cancel))
            }
            Err(error) => Err(error),
            Ok(false) => match self.load_refusal.take() {
                Some(reason) if is_stable_load_refusal(&reason) => {
                    Ok(self.refuse_unless_vanished(key, path, reason, cancel))
                }
                // No stable reason: the member stays pending (a file that
                // vanished is relisted by the membership instead).
                _ if path_stamp(path).is_none() => {
                    self.project_cache.invalidate_path(path);
                    Ok(MemberOutcome::Skipped)
                }
                reason => {
                    Ok(MemberOutcome::Deferred(reason.unwrap_or_else(|| {
                        "the source could not be loaded".to_owned()
                    })))
                }
            },
            Ok(true) => self.publish_member(key, path, &uri, own_stamp, cancel),
        };
        if !held {
            let _ = self.remove_indexed_with_control(&uri, None, None, false);
        }
        outcome
    }

    /// A file that is gone is not a refusal: its directory listing changed.
    fn refuse_unless_vanished(
        &self,
        key: &ContextKey,
        path: &Path,
        reason: String,
        cancel: &AtomicBool,
    ) -> MemberOutcome {
        if path_stamp(path).is_none() {
            self.project_cache.invalidate_path(path);
            return MemberOutcome::Skipped;
        }
        self.refuse_member(key, path, reason, cancel)
    }

    fn publish_member(
        &mut self,
        key: &ContextKey,
        path: &Path,
        uri: &Url,
        own_stamp: crate::workspace::PathStamp,
        cancel: &AtomicBool,
    ) -> Result<MemberOutcome, String> {
        let facts = match self.source_facts(
            uri,
            &mut FactBudget::new(pascal_cache::MAX_ARTIFACT_CONTENT_BYTES),
            cancel,
        ) {
            Ok(facts) => facts,
            Err(FactError::Cancelled) => return Err(CANCELLATION_MESSAGE.to_owned()),
            Err(error) => return Ok(self.refuse_member(key, path, error.to_string(), cancel)),
        };
        match facts.extracted.persistable {
            Ok(()) => {}
            // Limited facts cannot prove a member irrelevant (§8): it stays a
            // candidate, read directly, until its file changes.
            Err(NotPersistable::Transient) => {
                check_workspace_cancel(Some(cancel))?;
                self.project_cache.limit_index_member(key, path);
                self.prove_unindexed(key, path, Unindexed::NotRecorded, cancel);
                return Ok(MemberOutcome::Limited);
            }
            // An unsaved buffer is not what the disk holds; the index
            // describes saved files only.
            Err(NotPersistable::OverlayRoot | NotPersistable::OverlayInclude { .. }) => {
                return Ok(MemberOutcome::Skipped);
            }
        }
        let slots = self.source_slot_paths(uri);
        // The include stamps are the ones the load observed when it read the
        // include (A7); a stat now could describe newer bytes.
        let observed = self
            .expansions
            .get(uri)
            .into_iter()
            .flat_map(|record| &record.include_observations)
            .filter_map(|observation| {
                Some((
                    observation.path.clone(),
                    path_stamp_from_project_read(observation.stamp.as_ref()?),
                ))
            });
        let stamps = slot_stamps(path, own_stamp, &slots, observed);
        let member = MemberFacts::from_facts(path, &facts.extracted.facts, slots);
        let hash = facts.root_content_hash.unwrap_or(0);
        Ok(
            match self
                .project_cache
                .publish_index_member(key, path, member, stamps, hash)
            {
                PublishOutcome::Indexed => {
                    #[cfg(test)]
                    run_hook(&BEFORE_PERSIST, path);
                    // The one extraction pass serves the index and the store.
                    let persisted = match self.persist_extracted(key, uri, facts, cancel) {
                        Ok(proof) => self.set_member_record(key, &proof),
                        Err(skip) => {
                            if !matches!(skip, PersistSkip::Unavailable) {
                                crate::trace::trace!(
                                    "  index member {} not persisted: {skip:?}",
                                    path.display()
                                );
                            }
                            false
                        }
                    };
                    MemberOutcome::Published { persisted }
                }
                PublishOutcome::NotAMember => MemberOutcome::Skipped,
                PublishOutcome::Memory => MemberOutcome::Memory,
            },
        )
    }

    fn refuse_member(
        &self,
        key: &ContextKey,
        path: &Path,
        reason: String,
        cancel: &AtomicBool,
    ) -> MemberOutcome {
        self.project_cache.refuse_index_member(key, path, reason);
        // Decided as `eligible_member` does: by the saved root's size, so
        // every producer derives the same record.
        let over_size = self
            .contexts
            .get(key)
            .is_some_and(|state| self.root_over_size_limit(path, &state.context));
        let unindexed = if over_size {
            Unindexed::OverSize
        } else {
            Unindexed::NotRecorded
        };
        self.prove_unindexed(key, path, unindexed, cancel);
        MemberOutcome::Refused
    }

    /// Only a baseline view whose context came from saved project files
    /// publishes (spec §4.4, D37).
    fn may_publish(&self, key: &ContextKey) -> bool {
        self.is_baseline() && self.context_inputs_are_disk_only(key)
    }

    /// Record `proof` as `key`'s record of its member.
    fn set_member_record(&self, key: &ContextKey, proof: &ProvenRecord) -> bool {
        self.contexts.get(key).is_some_and(|state| {
            self.project_cache.set_index_record(
                key,
                proof,
                CacheContextKey::new(&state.context).digest(),
            )
        })
    }

    /// Persist an extracted member: its artifact (with the evidence that
    /// proves it), then its record, in a baseline view whose context came
    /// from saved files. The transient bytes are reserved first; a refusal
    /// is no error, the next process extracts the member again (D41).
    pub(crate) fn persist_extracted(
        &self,
        key: &ContextKey,
        uri: &Url,
        source: SourceFacts,
        cancel: &AtomicBool,
    ) -> Result<ProvenRecord, PersistSkip> {
        if !self.may_publish(key) || !self.persistent_cache().is_enabled() {
            return Err(PersistSkip::Unavailable);
        }
        let path = uri
            .to_file_path()
            .map_err(|()| PersistSkip::Prepare(super::persist::PersistError::NotIndexed))?;
        let prepared = self
            .prepare_from_facts(uri, source, cancel)
            .map_err(PersistSkip::Prepare)?;
        let eligible = self
            .eligible_indexed_member(
                key,
                &RecordInputs {
                    path: &path,
                    facts: Some(&prepared.facts),
                    artifact: Some(prepared.artifact_id()),
                    member_evidence: prepared.member_evidence_id(),
                    origin: prepared.origin,
                    unindexed: None,
                },
            )
            .map_err(PersistSkip::Ineligible)?;
        let _transient = self
            .project_cache
            .reserve(ReservationKind::Scratch, prepared.transient_bytes())
            .ok_or(PersistSkip::Memory)?;
        let job = self
            .persistent_cache()
            .job()
            .ok_or(PersistSkip::Unavailable)?;
        let stopped = || job.is_cancelled() || cancel.load(Ordering::Relaxed);
        if stopped() {
            return Err(PersistSkip::Unavailable);
        }
        match prepared.publish(job.store()) {
            Ok(_) => {}
            Err(super::persist::PersistError::Store(bypass)) => {
                job.observe(&bypass);
                return Err(PersistSkip::Store(bypass));
            }
            Err(error) => return Err(PersistSkip::Prepare(error)),
        }
        // A published artifact without its record is still reusable: a
        // later proof publishes the record.
        if stopped() {
            return Err(PersistSkip::Unavailable);
        }
        publish_proven(job.store(), &eligible).map_err(|bypass| {
            job.observe(&bypass);
            PersistSkip::Store(bypass)
        })
    }

    /// Prove the record `inputs` derive for a member the index holds: found
    /// live in the store, else published. Without a proof the member only
    /// delays the context's snapshot.
    fn prove_member(
        &self,
        key: &ContextKey,
        inputs: &RecordInputs<'_>,
        cancel: &AtomicBool,
    ) -> bool {
        if !self.may_publish(key) {
            return false;
        }
        let eligible = match self.eligible_indexed_member(key, inputs) {
            Ok(eligible) => eligible,
            Err(ineligible) => {
                crate::trace::trace!(
                    "  index member {} not proven: {ineligible:?}",
                    inputs.path.display()
                );
                return false;
            }
        };
        let Some(job) = self.persistent_cache().job() else {
            return false;
        };
        let stopped = || job.is_cancelled() || cancel.load(Ordering::Relaxed);
        if stopped() {
            return false;
        }
        let proof = match prove_existing(job.store(), &eligible) {
            Ok(Some(proof)) => Ok(proof),
            Ok(None) if stopped() => return false,
            Ok(None) => publish_proven(job.store(), &eligible),
            Err(bypass) => Err(bypass),
        };
        let proof = match proof {
            Ok(proof) => proof,
            Err(bypass) => {
                job.observe(&bypass);
                return false;
            }
        };
        drop(job);
        self.set_member_record(key, &proof)
    }

    /// A refused or limited member is listed by an artifact-less record, so
    /// a complete membership can still be sealed; the indexer retries it.
    fn prove_unindexed(
        &self,
        key: &ContextKey,
        path: &Path,
        unindexed: Unindexed,
        cancel: &AtomicBool,
    ) {
        let Some(state) = self.contexts.get(key) else {
            return;
        };
        self.prove_member(
            key,
            &RecordInputs {
                path,
                facts: None,
                artifact: None,
                member_evidence: None,
                origin: state.context.source_origin(path),
                unindexed: Some(unindexed),
            },
            cancel,
        );
    }

    /// Index `path` from the store's facts, if current inputs prove them
    /// (spec §7), and prove its record. Nothing is loaded or parsed; the
    /// decoded facts are charged while they live, and dropped here (D41).
    fn reuse_index_member(
        &self,
        key: &ContextKey,
        path: &Path,
        own_stamp: &PathStamp,
        cancel: &AtomicBool,
    ) -> ReuseOutcome {
        if !self.is_baseline()
            || !self
                .project_cache
                .read_index(key, |index, _| !index.reuse_tried(path))
                .unwrap_or(false)
        {
            return ReuseOutcome::NoStore;
        }
        let Ok(uri) = Url::from_file_path(path) else {
            return ReuseOutcome::Missed;
        };
        let Some(job) = self.persistent_cache().job() else {
            return ReuseOutcome::NoStore;
        };
        let work = super::ReconciliationBudget::new(Arc::new(AtomicBool::new(false)));
        // The held snapshot first: its record skips the candidate search.
        let mut hinted = None;
        if let Some(hint) = self.project_cache.baseline_hint(key) {
            match self.reuse_from_hint(
                &uri,
                path,
                key,
                &hint,
                job.store(),
                &work,
                pascal_cache::MAX_ARTIFACT_CONTENT_BYTES,
                job.cancel_flag(),
            ) {
                Ok(HintReuse::Done(reuse)) => hinted = Some(Ok(reuse)),
                Ok(HintReuse::Retrying) => return ReuseOutcome::Retrying,
                Ok(HintReuse::Gone) => {
                    let id = hint.header().id();
                    if self
                        .project_cache
                        .drop_baseline_hint(key, id)
                        .is_some_and(|dropped| dropped.held())
                        && let Err(bypass) = job.store().release_snapshot(id)
                    {
                        job.observe(&bypass);
                    }
                }
                Ok(HintReuse::Fallback) => {}
                Err(bypass) => {
                    job.observe(&bypass);
                    return ReuseOutcome::Retry;
                }
            }
        }
        let result = hinted.unwrap_or_else(|| {
            self.reuse_in_context(
                &uri,
                key,
                job.store(),
                &work,
                pascal_cache::MAX_ARTIFACT_CONTENT_BYTES,
                job.cancel_flag(),
            )
        });
        job.record_reuse(&result);
        if cancel.load(Ordering::Relaxed) {
            return ReuseOutcome::Cancelled;
        }
        let (facts, reads, artifact, evidence, include_stamps, root_hash) = match result {
            Ok(Reuse::Hit {
                facts,
                reads,
                artifact,
                evidence,
                include_stamps,
                root_hash,
                ..
            }) => (facts, reads, artifact, evidence, include_stamps, root_hash),
            Ok(Reuse::Miss(ReuseMiss::Unproven(_))) => return ReuseOutcome::Retry,
            Ok(Reuse::Miss(ReuseMiss::Stale(_) | ReuseMiss::Store(_) | ReuseMiss::Open)) => {
                return ReuseOutcome::Missed;
            }
            // `record_reuse` observed the bypass.
            Err(super::persist::PersistError::Store(_)) => return ReuseOutcome::Retry,
            Err(_) => return ReuseOutcome::Missed,
        };
        job.store().hint_used(match evidence {
            Some(id) => Hint::Evidence(id),
            None => Hint::Artifact(artifact),
        });
        drop(job);
        self.project_cache
            .add_index_validation_reads(key, u64::try_from(reads).unwrap_or(u64::MAX));
        let (slots, stamps) = reused_slots(path, own_stamp, &facts.facts, &include_stamps);
        let member = MemberFacts::from_facts(path, &facts.facts, slots);
        let outcome = match self
            .project_cache
            .publish_index_member(key, path, member, stamps, root_hash)
        {
            PublishOutcome::Indexed => {
                self.project_cache.mark_index_reused(key, path);
                if let Some(state) = self.contexts.get(key) {
                    self.prove_member(
                        key,
                        &RecordInputs {
                            path,
                            facts: Some(&facts.facts),
                            artifact: Some(artifact),
                            member_evidence: evidence,
                            origin: state.context.source_origin(path),
                            unindexed: None,
                        },
                        cancel,
                    );
                }
                ReuseOutcome::Reused
            }
            PublishOutcome::Memory => ReuseOutcome::Memory,
            PublishOutcome::NotAMember => ReuseOutcome::Missed,
        };
        drop(facts);
        outcome
    }

    /// The context `anchor` names, resolved and retained.
    pub(crate) fn index_context_for(
        &mut self,
        anchor: &IndexAnchor,
        cancel: &AtomicBool,
    ) -> Result<ContextKey, String> {
        match anchor {
            IndexAnchor::Document(uri) => snapshot_context_for_uri(self, uri, cancel, false),
            IndexAnchor::Project(descriptor) => {
                check_workspace_cancel(Some(cancel))?;
                let uri = Url::from_file_path(descriptor)
                    .map_err(|()| format!("could not create a file URI for {descriptor:?}"))?;
                let roots = self.workspace_root_paths();
                let mut options = self.project_options();
                options.project_file = Some(descriptor.clone());
                let (key, project) =
                    self.readonly_context_for_uri(&uri, descriptor, &roots, &options)?;
                self.install_context(
                    key.clone(),
                    project.context,
                    project.observations,
                    Vec::new(),
                    HashMap::new(),
                    descriptor,
                    Some(cancel),
                    None,
                )?;
                Ok(key)
            }
        }
    }

    /// The compilable units of `key`, from disk only. The unit cap is applied
    /// where the membership is stored.
    pub(crate) fn index_membership(
        &mut self,
        key: &ContextKey,
        cancel: &AtomicBool,
    ) -> Result<IndexMembership, String> {
        #[cfg(test)]
        MEMBERSHIP_COMPUTES.with(|count| count.set(count.get() + 1));
        let context = self
            .contexts
            .get(key)
            .map(|state| state.context.clone())
            .ok_or_else(|| "index context was not resolved".to_string())?;
        let mut before_io = || check_workspace_cancel(Some(cancel));
        let mut membership = IndexMembership {
            complete: true,
            packages: !context.packages().is_empty(),
            ..IndexMembership::default()
        };
        let mut paths = Vec::new();
        if context.project_file().is_some() {
            let mut directories = Vec::new();
            if let Some(main) = context.main_source() {
                paths.push(absolute_path(main.to_path_buf()));
                directories.extend(main.parent().map(Path::to_path_buf));
            }
            for units in context.explicit_units().into_values() {
                for unit in units {
                    directories.extend(unit.parent().map(Path::to_path_buf));
                    paths.push(absolute_path(unit));
                }
            }
            directories.extend(context.search_paths());
            let mut seen = HashSet::new();
            for directory in directories {
                let scope = CatalogueScope::listing(&directory);
                if !seen.insert(scope.key().clone()) {
                    continue;
                }
                let catalogue =
                    self.shared_catalogue(&scope, MAX_CATALOGUE_ENTRIES, &mut before_io)?;
                self.note_scope(&mut membership, &scope, &catalogue);
                paths.extend(
                    catalogue
                        .pascal_files()
                        .filter(|path| is_unit_source(path))
                        .map(Path::to_path_buf),
                );
            }
        } else {
            self.projectless_members(key, &context, cancel, &mut paths, &mut membership)?;
        }
        paths.retain(|path| self.index_accepts(key, path));
        paths.sort_by_cached_key(|path| path_key(path));
        paths.dedup_by(|left, right| path_key(left) == path_key(right));
        membership.complete &= !paths.is_empty();
        membership.paths = paths;
        Ok(membership)
    }

    /// The acceptance today's enumeration applies: workspace sources must be
    /// safe source paths, mapped library sources only readable (mapped roots
    /// are deliberately not workspace source roots).
    pub(super) fn index_accepts(&self, key: &ContextKey, path: &Path) -> bool {
        (self.accepts_path(path) && is_safe_source_path(self, path))
            || self.mapped_path_is_readable(path, key)
    }

    /// Records the catalogue `membership` was read from, with the generation
    /// `index_membership_current` compares. A catalogue the store did not keep
    /// (or replaced meanwhile) gets generation 0, which is never current.
    fn note_scope(
        &self,
        membership: &mut IndexMembership,
        scope: &CatalogueScope,
        catalogue: &std::sync::Arc<Catalogue>,
    ) {
        let generation = self
            .project_cache
            .catalogue(scope.key())
            .filter(|shared| std::sync::Arc::ptr_eq(&shared.catalogue, catalogue))
            .map_or(0, |shared| shared.generation);
        membership.scopes.push((scope.key().clone(), generation));
        if !catalogue.is_complete()
            || catalogue.limit().is_some()
            || !catalogue.unreadable().is_empty()
        {
            membership.complete = false;
        }
    }

    /// The recursive scope of `root` under the exclusions of the workspace
    /// root containing it, the scope `filename_catalogue_with_cancel` uses.
    fn recursive_scope(&self, root: &Path) -> CatalogueScope {
        let root = absolute_path(root.to_path_buf());
        let matcher = self
            .roots
            .iter()
            .find(|workspace_root| path_starts_with_ci(&root, &workspace_root.path))
            .map(|workspace_root| workspace_root.excludes.clone());
        CatalogueScope::recursive(&root, matcher.as_ref())
    }

    /// A projectless context resolves units recursively, so it holds the
    /// recursive catalogues' files whose directory it owns (A9).
    fn projectless_members(
        &mut self,
        key: &ContextKey,
        context: &ProjectContext,
        cancel: &AtomicBool,
        paths: &mut Vec<PathBuf>,
        membership: &mut IndexMembership,
    ) -> Result<(), String> {
        let mut scopes = self
            .roots
            .iter()
            .map(|root| CatalogueScope::recursive(&root.path, Some(&root.excludes)))
            .collect::<Vec<_>>();
        for search_path in context.search_paths() {
            scopes.push(self.recursive_scope(&search_path));
        }
        let mut seen = HashSet::new();
        let mut before_io = || check_workspace_cancel(Some(cancel));
        let mut owned = HashMap::<PathBuf, Option<bool>>::new();
        let mut discovery = OwnerDiscovery::new(owner_discovery_cap());
        for scope in scopes {
            if !seen.insert(scope.key().clone()) {
                continue;
            }
            let catalogue = self.shared_catalogue(&scope, MAX_CATALOGUE_ENTRIES, &mut before_io)?;
            self.note_scope(membership, &scope, &catalogue);
            let mut files = catalogue
                .pascal_files()
                .filter(|path| is_unit_source(path))
                .collect::<Vec<_>>();
            files.sort_by_cached_key(|path| path_key(path));
            for file in files {
                let Some(directory) = file.parent() else {
                    continue;
                };
                if !owned.contains_key(directory) {
                    let verdict =
                        self.directory_owned_by(key, directory, file, cancel, &mut discovery)?;
                    if verdict.is_none() {
                        membership.owner_pending += 1;
                        membership.complete = false;
                    }
                    owned.insert(directory.to_path_buf(), verdict);
                }
                if owned[directory] == Some(true) {
                    paths.push(file.to_path_buf());
                }
            }
        }
        Ok(())
    }

    /// Whether `key` owns `directory`; `None` while its owner is unknown (the
    /// per-call discovery cap, or a source no context accepts).
    fn directory_owned_by(
        &mut self,
        key: &ContextKey,
        directory: &Path,
        sample: &Path,
        cancel: &AtomicBool,
        discovery: &mut OwnerDiscovery,
    ) -> Result<Option<bool>, String> {
        Ok(self
            .discover_directory_owner(directory, sample, cancel, discovery)?
            .map(|owner| &owner == key))
    }

    /// The remembered owner of `directory`, only while its context is still
    /// retained and fresh and still the one selected for `sample`, as the
    /// sibling reuse of `discover_enumerated_contexts` requires.
    fn fresh_owner_of(
        &self,
        directory: &Path,
        sample: &Path,
        cancel: &AtomicBool,
        discovery: &mut OwnerDiscovery,
    ) -> Result<Option<ContextKey>, String> {
        let Some(owner) = self.project_cache.owner_of(directory) else {
            return Ok(None);
        };
        let fresh = match discovery.fresh.get(&owner) {
            Some(fresh) => *fresh,
            None => {
                let fresh = self.context_is_fresh_with_cancel(&owner, Some(cancel), None)?
                    && self.context_matches_current_selection_with_cancel_and_budget(
                        sample,
                        &owner,
                        Some(cancel),
                        None,
                    )?;
                discovery.fresh.insert(owner.clone(), fresh);
                fresh
            }
        };
        Ok(fresh.then_some(owner))
    }

    /// The owner of `directory` from the owner map while fresh, else
    /// discovered through `sample` and remembered, while the request is under
    /// the discovery cap.
    fn discover_directory_owner(
        &mut self,
        directory: &Path,
        sample: &Path,
        cancel: &AtomicBool,
        discovery: &mut OwnerDiscovery,
    ) -> Result<Option<ContextKey>, String> {
        if let Some(owner) = self.fresh_owner_of(directory, sample, cancel, discovery)? {
            return Ok(Some(owner));
        }
        if discovery.discovered >= discovery.cap {
            return Ok(None);
        }
        discovery.discovered += 1;
        let Ok(uri) = Url::from_file_path(sample) else {
            return Ok(None);
        };
        match snapshot_context_for_uri(self, &uri, cancel, false) {
            Ok(owner) => {
                self.project_cache
                    .remember_owner(directory.to_path_buf(), owner.clone());
                discovery.fresh.insert(owner.clone(), true);
                Ok(Some(owner))
            }
            Err(_) => {
                check_workspace_cancel(Some(cancel))?;
                Ok(None)
            }
        }
    }

    /// The contexts an indexed request searches (A8): the requesting owner,
    /// the owners of open documents (newest first), the configured project,
    /// then, to fill the slots left, project descriptors (symbols only) and
    /// the owners of sources no target holds.
    pub(crate) fn index_targets(
        &mut self,
        input: &WorkspaceInput,
        requesting: Option<&Url>,
        purpose: TargetPurpose,
        cancel: &AtomicBool,
    ) -> Result<Targets, String> {
        #[cfg(test)]
        if TARGET_ERROR.with(std::cell::Cell::get) {
            return Err("an injected target error".to_owned());
        }
        let started = std::time::Instant::now();
        let maximum = max_target_contexts();
        let mut targets = Targets::default();
        let push = |targets: &mut Targets, key: ContextKey, anchor: IndexAnchor| {
            if targets.contexts.len() < maximum && !targets.holds(&key) {
                targets.contexts.push((key, anchor));
            }
        };

        let mut projects = HashMap::<String, Option<ContextKey>>::new();
        if let Some(uri) = requesting {
            let known = input
                .document_owners
                .get(uri)
                .or_else(|| input.document_owners.get(&super::canonical_file_uri(uri)));
            let owner = match known {
                Some(owner) => Some((
                    owner.key.clone(),
                    owner.state.context.project_file().map(Path::to_path_buf),
                )),
                None => match self.index_context_for(&IndexAnchor::Document(uri.clone()), cancel) {
                    Ok(key) => {
                        let project = self
                            .contexts
                            .get(&key)
                            .and_then(|state| state.context.project_file().map(Path::to_path_buf));
                        Some((key, project))
                    }
                    Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
                    Err(error) => {
                        crate::trace::trace!("  index target: {uri} did not resolve: {error}");
                        None
                    }
                },
            };
            if let Some((owner, project)) = owner
                && let Some((key, anchor)) =
                    self.owner_target(owner, project, uri, &mut projects, cancel)?
            {
                targets.requesting = Some(key.clone());
                push(&mut targets, key, anchor);
            }
        }

        let mut open = input.document_versions.iter().collect::<Vec<_>>();
        open.sort_by(|left, right| {
            right
                .1
                .cmp(left.1)
                .then_with(|| left.0.as_str().cmp(right.0.as_str()))
        });
        for (uri, _) in open {
            if let Some(owner) = input.document_owners.get(uri) {
                let project = owner.state.context.project_file().map(Path::to_path_buf);
                if let Some((key, anchor)) =
                    self.owner_target(owner.key.clone(), project, uri, &mut projects, cancel)?
                {
                    push(&mut targets, key, anchor);
                }
            }
        }

        if let Some(project) = self.configured_project_path()
            && let Some(key) = self.project_target(&project, &mut projects, cancel)?
        {
            push(&mut targets, key, IndexAnchor::Project(project));
        }
        targets.from_open_or_configured = targets.contexts.len();
        crate::trace::trace!(
            "  index targets: {} open or configured after {:.1} ms",
            targets.contexts.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );

        let files = self.root_files(cancel)?;
        targets.small_workspace = files.small();
        let RootFiles {
            sources,
            descriptors,
            ..
        } = files;
        crate::trace::trace!(
            "  index targets: {} root sources, {} descriptors after {:.1} ms",
            sources.len(),
            descriptors.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );

        if purpose == TargetPurpose::WorkspaceSymbols {
            let roots = self.workspace_root_paths();
            let mut options = self.project_options();
            let projects = descriptors
                .iter()
                .filter(|(_, path)| is_project_file(path))
                .filter_map(|(_, path)| sibling_stem(path))
                .collect::<HashSet<_>>();
            let mut attempts = 0usize;
            for (_, descriptor) in &descriptors {
                if targets.contexts.len() >= maximum || attempts >= 2 * maximum {
                    break;
                }
                if !is_project_file(descriptor)
                    && sibling_stem(descriptor).is_some_and(|stem| projects.contains(&stem))
                {
                    continue;
                }
                // Without an index a descriptor contributes nothing: it is
                // neither resolved nor requested (M1).
                let anchor = IndexAnchor::Project(descriptor.clone());
                if !targets.small_workspace && !self.project_cache.holds_index_for(&anchor) {
                    continue;
                }
                check_workspace_cancel(Some(cancel))?;
                attempts += 1;
                #[cfg(test)]
                DESCRIPTOR_ATTEMPTS.with(|count| count.set(count.get() + 1));
                match project_metadata_context(self, descriptor, &roots, &mut options, cancel) {
                    Ok((key, _)) => push(&mut targets, key, anchor),
                    Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
                    Err(error) => {
                        crate::trace::trace!(
                            "  index target: {} did not resolve: {error}",
                            descriptor.display()
                        );
                    }
                }
            }
            crate::trace::trace!(
                "  index targets: {attempts} descriptors tried, {} targets after {:.1} ms",
                targets.contexts.len(),
                started.elapsed().as_secs_f64() * 1000.0
            );
        }

        let mut held = HashSet::<String>::new();
        for (key, _) in &targets.contexts {
            self.project_cache.read_index(key, |_, membership| {
                held.extend(membership.paths.iter().map(|path| path_key(path)));
            });
        }
        let mut owners = HashMap::<PathBuf, Option<ContextKey>>::new();
        let mut discovery = OwnerDiscovery::new(interactive_discovery_cap());
        let mut outside = 0usize;
        for (file_key, file) in sources.iter().filter(|(_, path)| is_unit_source(path)) {
            if held.contains(file_key) {
                continue;
            }
            let Some(directory) = file.parent() else {
                continue;
            };
            let owner = match owners.get(directory) {
                Some(owner) => owner.clone(),
                None => {
                    let owner =
                        match self.fresh_owner_of(directory, file, cancel, &mut discovery)? {
                            Some(owner) => Some(owner),
                            None if targets.contexts.len() < maximum => self
                                .discover_directory_owner(
                                    directory,
                                    file,
                                    cancel,
                                    &mut discovery,
                                )?,
                            None => None,
                        };
                    owners.insert(directory.to_path_buf(), owner.clone());
                    owner
                }
            };
            match owner {
                Some(owner) if targets.holds(&owner) => {}
                // An owner without an index contributes nothing (M1).
                Some(owner)
                    if !targets.small_workspace && !self.project_cache.holds_index(&owner) =>
                {
                    outside += 1;
                }
                Some(owner) if targets.contexts.len() < maximum => {
                    let project = self
                        .contexts
                        .get(&owner)
                        .and_then(|state| state.context.project_file().map(Path::to_path_buf));
                    let target = match Url::from_file_path(file) {
                        Ok(uri) => {
                            self.owner_target(owner, project, &uri, &mut projects, cancel)?
                        }
                        Err(()) => None,
                    };
                    match target {
                        Some((key, anchor))
                            if targets.small_workspace || self.project_cache.holds_index(&key) =>
                        {
                            push(&mut targets, key, anchor);
                        }
                        _ => outside += 1,
                    }
                }
                _ => outside += 1,
            }
        }
        crate::trace::trace!(
            "  index targets: {} directories, {} owners discovered, {outside} outside, {} targets after {:.1} ms",
            owners.len(),
            discovery.discovered,
            targets.contexts.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        if outside > 0 {
            targets.gaps.push(format!(
                "{outside} sources under the workspace roots belong to contexts that were not searched"
            ));
        }
        Ok(targets)
    }

    /// The Pascal sources and project descriptors the root catalogues list.
    fn root_files(&mut self, cancel: &AtomicBool) -> Result<RootFiles, String> {
        let scopes = self
            .roots
            .iter()
            .map(|root| CatalogueScope::recursive(&root.path, Some(&root.excludes)))
            .collect::<Vec<_>>();
        let mut before_io = || check_workspace_cancel(Some(cancel));
        let mut files = RootFiles {
            complete: true,
            ..RootFiles::default()
        };
        for scope in &scopes {
            let catalogue = self.shared_catalogue(scope, MAX_CATALOGUE_ENTRIES, &mut before_io)?;
            files.complete &= catalogue.is_complete() && catalogue.limit().is_none();
            files.sources.extend(catalogue.pascal_files().map(keyed));
            files.descriptors.extend(
                catalogue
                    .files()
                    .iter()
                    .filter(|path| is_descriptor(path))
                    .map(|path| keyed(path)),
            );
        }
        for list in [&mut files.sources, &mut files.descriptors] {
            list.sort_by(|left, right| left.0.cmp(&right.0));
            list.dedup_by(|left, right| left.0 == right.0);
        }
        Ok(files)
    }

    /// Whether the workspace is small enough to walk (A16): its complete
    /// root catalogues hold at most `SMALL_WORKSPACE_SOURCES` sources and
    /// `SMALL_WORKSPACE_PROJECTS` descriptors.
    pub(crate) fn is_small_workspace(&mut self, cancel: &AtomicBool) -> Result<bool, String> {
        Ok(self.root_files(cancel)?.small())
    }

    /// The target an owner context stands for. A context with a project
    /// file is searched under the key its Project anchor resolves to, the
    /// key the warmer indexes: a document owner can hold an older
    /// resolution of the same project. `None` when the project no longer
    /// resolves.
    fn owner_target(
        &mut self,
        owner: ContextKey,
        project: Option<PathBuf>,
        uri: &Url,
        projects: &mut HashMap<String, Option<ContextKey>>,
        cancel: &AtomicBool,
    ) -> Result<Option<(ContextKey, IndexAnchor)>, String> {
        let Some(project) = project else {
            return Ok(Some((owner, IndexAnchor::Document(uri.clone()))));
        };
        Ok(self
            .project_target(&project, projects, cancel)?
            .map(|key| (key, IndexAnchor::Project(project))))
    }

    /// The key `project`'s anchor resolves to, once per request.
    fn project_target(
        &mut self,
        project: &Path,
        projects: &mut HashMap<String, Option<ContextKey>>,
        cancel: &AtomicBool,
    ) -> Result<Option<ContextKey>, String> {
        let project_key = path_key(project);
        if let Some(key) = projects.get(&project_key) {
            return Ok(key.clone());
        }
        let started = std::time::Instant::now();
        let key = match self.index_context_for(&IndexAnchor::Project(project.to_path_buf()), cancel)
        {
            Ok(key) => {
                crate::trace::trace!(
                    "  index target: {} resolved in {:.1} ms",
                    project.display(),
                    started.elapsed().as_secs_f64() * 1000.0
                );
                Some(key)
            }
            Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
            Err(error) => {
                crate::trace::trace!(
                    "  index target: {} did not resolve: {error}",
                    project.display()
                );
                None
            }
        };
        projects.insert(project_key, key.clone());
        Ok(key)
    }

    /// The sources an indexed snapshot reads for `targets` (l4d-33 §6): per
    /// target, the members `query` selects, then its unverified members
    /// within one request-wide bound (A5), then `must_include` (declaring
    /// documents and open buffers, A4). A member left out is excluded only
    /// while every stamp it was indexed under still proves it unchanged
    /// (A7); those stamps and the membership's directories become the
    /// baseline. `query: None` (workspace symbols) reads the unverified
    /// members only. Client-deleted files are never sources. With
    /// `requesting`, the members of every other target that lie outside the
    /// workspace roots are a gap, not candidates: the walk reads only the
    /// requesting context's mapped roots.
    pub(crate) fn indexed_sources(
        &mut self,
        input: &WorkspaceInput,
        targets: &Targets,
        query: Option<&CandidateQuery>,
        must_include: &[Url],
        requesting: Option<&ContextKey>,
        cancel: &AtomicBool,
    ) -> Result<IndexedSources, String> {
        self.select_sources(
            input,
            targets,
            Selection::Candidates(query),
            must_include,
            requesting,
            cancel,
        )
        .map(|(sources, _)| sources)
    }

    /// Workspace symbols of `targets` (§6.2): the declarations matching
    /// `query` from the facts of members whose stamps still prove them
    /// unchanged (A7), and the sources the snapshot reads for the rest:
    /// members that are unverified or failed verification, within the bound
    /// (A5), and members holding an open or client-deleted file (A4).
    /// Without `direct`, unverified members are only counted.
    pub(crate) fn indexed_symbols(
        &mut self,
        input: &WorkspaceInput,
        targets: &Targets,
        query: &str,
        direct: bool,
        must_include: &[Url],
        cancel: &AtomicBool,
    ) -> Result<(IndexedSources, Vec<(ContextKey, IndexedSymbol)>), String> {
        self.select_sources(
            input,
            targets,
            Selection::Symbols { query, direct },
            must_include,
            None,
            cancel,
        )
    }

    fn select_sources(
        &mut self,
        input: &WorkspaceInput,
        targets: &Targets,
        selection: Selection<'_>,
        must_include: &[Url],
        requesting: Option<&ContextKey>,
        cancel: &AtomicBool,
    ) -> Result<(IndexedSources, Vec<(ContextKey, IndexedSymbol)>), String> {
        let mut result = IndexedSources {
            gaps: targets.gaps.clone(),
            ..IndexedSources::default()
        };
        let mut symbols = Vec::new();
        let mut seen = HashSet::<SymbolKey>::new();
        let cap = crate::navigation::workspace_symbol_cap();
        let mut taken = HashSet::<String>::new();
        let mut stamps = HashMap::<String, Option<PathStamp>>::new();
        let mut unverified_left = match selection {
            Selection::Symbols { direct: false, .. } => 0,
            Selection::Symbols { direct: true, .. } => max_unverified_symbol_candidates(),
            Selection::Candidates(_) => max_unverified_candidates(),
        };
        let mut refused = 0usize;
        let mut first_refusal = None::<Box<str>>;
        let mut memory = false;
        let open = input
            .overlays
            .keys()
            .filter_map(|uri| uri.to_file_path().ok())
            .map(|path| path_key(&absolute_path(path)))
            .collect::<HashSet<_>>();
        // Disk facts never answer for open buffers or client deletes (A4).
        let unserved = input
            .overlays
            .keys()
            .chain(input.deleted_overrides.keys())
            .filter_map(|uri| uri.to_file_path().ok())
            .map(absolute_path)
            .collect::<Vec<_>>();
        let deleted = |path: &Path| {
            Url::from_file_path(path).is_ok_and(|uri| {
                [&uri, &super::canonical_file_uri(&uri)]
                    .into_iter()
                    .any(|uri| {
                        input
                            .deleted_overrides
                            .get(uri)
                            .is_some_and(|stamp| disk_stamp(path) == *stamp)
                    })
            })
        };
        // Open buffers stay candidates (A4); deleted files are not counted.
        let drop_outside = |workspace: &Self, key: &ContextKey, paths: &mut Vec<PathBuf>| {
            if requesting.is_none_or(|requesting| requesting == key) {
                return 0;
            }
            let mut outside = 0;
            paths.retain(|path| {
                let keep = workspace.accepts_path(path) || open.contains(&path_key(path));
                if !keep && !deleted(path) {
                    outside += 1;
                }
                keep
            });
            outside
        };
        let mut unowned = 0usize;
        for (position, (key, anchor)) in targets.contexts.iter().enumerate() {
            check_workspace_cancel(Some(cancel))?;
            let searched = targets.searched(position);
            let target_started = std::time::Instant::now();
            let sources_before = result.sources.len();
            if !self.contexts.contains_key(key) {
                let known = input
                    .document_owners
                    .values()
                    .find(|known| known.key == *key)
                    .map(|known| super::ContextState::clone(&known.state));
                if let Some(state) = known {
                    self.contexts.insert(key.clone(), state);
                } else {
                    let resolved = match self.index_context_for(anchor, cancel) {
                        Ok(resolved) => Some(resolved),
                        Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
                        Err(_) => None,
                    };
                    // The anchor now names another context: this target is
                    // a gap, never an error.
                    if resolved.as_ref() != Some(key) {
                        result.gaps.push(format!(
                            "{} changed while the request ran; it was not searched",
                            anchor_display(anchor)
                        ));
                        continue;
                    }
                }
            }
            let display = anchor_display(anchor);
            // A target searched only for its facts never computes a
            // membership: that is the warmer's work for a searched one.
            if !searched && !self.project_cache.index_membership_current(key) {
                result
                    .gaps
                    .push(format!("{display} is not indexed yet; it was not searched"));
                continue;
            }
            let mut computed = if searched {
                self.ensure_index_membership_computed(key, anchor, cancel)?
            } else {
                None
            };
            crate::trace::trace!(
                "  index target {display}: membership {} after {:.1} ms",
                if computed.is_some() {
                    "computed"
                } else {
                    "current"
                },
                target_started.elapsed().as_secs_f64() * 1000.0
            );
            if let Some(state) = self.contexts.get(key) {
                result.contexts.insert(key.clone(), state.clone());
            }
            // The directories baseline the membership only while its
            // catalogues are the ones it was computed from; otherwise the
            // membership is computed once more.
            let mut retried = false;
            let (view, listing) = loop {
                let view = self.project_cache.read_index(key, |index, membership| {
                    target_view(index, membership, selection, &unserved, &mut seen, cap)
                });
                #[cfg(test)]
                if let Some(mut hook) = AFTER_TARGET_VIEW.with(|slot| slot.borrow_mut().take()) {
                    hook(&self.project_cache);
                    AFTER_TARGET_VIEW.with(|slot| *slot.borrow_mut() = Some(hook));
                }
                let scopes = match (&view, &computed) {
                    (Some(view), _) => view.scopes.clone(),
                    (None, Some(membership)) => membership.scopes.clone(),
                    (None, None) => Vec::new(),
                };
                let listing = self.membership_listing(&scopes);
                if listing.is_some() || retried || !searched {
                    break (view, listing);
                }
                // The view is read again: its declarations are not seen yet.
                for symbol in view.iter().flat_map(|view| &view.symbols) {
                    if let Some((_, range)) = symbol.declaration.location {
                        seen.remove(&(
                            path_key(&symbol.path),
                            range,
                            symbol.declaration.display.clone(),
                        ));
                    }
                }
                retried = true;
                let membership = self.index_membership(key, cancel)?;
                self.project_cache.set_index_membership(
                    key.clone(),
                    anchor.clone(),
                    membership.clone(),
                    self.options.limits.max_workspace_units,
                );
                computed = Some(membership);
            };
            let listing_incomplete = match listing {
                Some(listing) => {
                    result.baseline.extend(listing.directories);
                    listing.incomplete
                }
                None if !searched => {
                    result
                        .gaps
                        .push(format!("{display} is not indexed yet; it was not searched"));
                    continue;
                }
                None => {
                    result.gaps.push(format!(
                        "the directory listings of {display} could not be kept; changes to them may be missed"
                    ));
                    false
                }
            };
            let stored = view.filter(|view| {
                !view
                    .limit
                    .as_deref()
                    .is_some_and(|limit| !limit.starts_with("more than "))
            });
            let Some(view) = stored else {
                if !searched {
                    result
                        .gaps
                        .push(format!("{display} is not indexed yet; it was not searched"));
                    continue;
                }
                // The store holds no members (no room for the membership):
                // read every member as today's walk would, within the bound.
                let membership = match computed {
                    Some(membership) => membership,
                    None => self.index_membership(key, cancel)?,
                };
                memory = true;
                let mut paths = membership.paths;
                let outside = drop_outside(self, key, &mut paths);
                outside_gap(&mut result.gaps, outside, &display);
                let (skipped, searched) = take_unverified(
                    paths,
                    key,
                    &open,
                    &deleted,
                    &mut taken,
                    &mut unverified_left,
                    &mut result.sources,
                );
                if skipped > 0 {
                    result.gaps.push(format!(
                        "{skipped} units of {display} are not indexed yet; {searched} were searched directly"
                    ));
                }
                self.project_cache.request_index(anchor.clone());
                continue;
            };
            let TargetView {
                candidates:
                    Candidates {
                        selected,
                        mut unverified,
                        refused: refused_members,
                        excluded,
                    },
                recorded,
                symbols: view_symbols,
                exceeded,
                coverage,
                complete,
                limit,
                packages,
                owner_pending,
                ..
            } = view;

            if exceeded {
                return Err(
                    crate::navigation::too_many_workspace_symbols(cap.saturating_add(1))
                        .unwrap_or_default(),
                );
            }
            let mut proven = HashSet::new();
            for (path, (resolved, recorded)) in excluded.into_iter().zip(recorded) {
                check_workspace_cancel(Some(cancel))?;
                let unchanged = recorded.first().is_some_and(|first| first.path == path)
                    && recorded.iter().all(|member| {
                        let now = stamps
                            .entry(path_key(&member.path))
                            .or_insert_with(|| path_stamp(&member.path));
                        matches!((&member.stamp, &*now), (Some(old), Some(now)) if old.proves_unchanged(now))
                    });
                if !unchanged {
                    self.project_cache.mark_index_stale(key, &path);
                }
                if resolved && unchanged {
                    proven.insert(path_key(&path));
                    result.baseline.extend(
                        recorded
                            .into_iter()
                            .map(|member| (member.path, member.stamp)),
                    );
                } else {
                    unverified.push(path);
                }
            }
            symbols.extend(
                view_symbols
                    .into_iter()
                    .filter(|symbol| proven.contains(&path_key(&symbol.member)))
                    .map(|symbol| (key.clone(), symbol)),
            );
            unverified.sort_by_cached_key(|path| path_key(path));
            let mut selected = selected;
            let outside =
                drop_outside(self, key, &mut selected) + drop_outside(self, key, &mut unverified);
            outside_gap(&mut result.gaps, outside, &display);

            for path in selected {
                if !deleted(&path) && taken.insert(path_key(&path)) {
                    result.sources.push((path, key.clone()));
                }
            }
            // A target searched for its facts reads no unverified member.
            let mut none_left = 0;
            let (skipped, read) = take_unverified(
                unverified,
                key,
                &open,
                &deleted,
                &mut taken,
                if searched {
                    &mut unverified_left
                } else {
                    &mut none_left
                },
                &mut result.sources,
            );
            if skipped > 0 {
                result.gaps.push(format!(
                    "{skipped} units of {display} are not indexed yet; {read} were searched directly"
                ));
            }
            if searched && let Some(reason) = coverage.first_deferral.as_deref() {
                result.gaps.push(format!(
                    "{} units of {display} failed to load for the index and are retried later: {reason}",
                    coverage.deferred
                ));
            }
            refused += refused_members.len();
            if first_refusal.is_none() {
                first_refusal = refused_members.into_iter().next().map(|(_, reason)| reason);
            }

            match limit.as_deref() {
                Some(_) => {
                    result.gaps.push(format!(
                        "{display} has more than {} units; the rest were not searched",
                        coverage.members
                    ));
                }
                // Directories whose owner is still pending are already the
                // outside-source gap of the targets.
                None if !complete && (listing_incomplete || owner_pending == 0) => {
                    result.gaps.push(format!(
                        "{display} membership is incomplete; some units were not searched"
                    ));
                }
                None => {}
            }
            memory |= self.project_cache.index_memory_exhausted(key);
            if packages {
                result.gaps.push(format!(
                    "units provided through runtime packages of {display} were not searched"
                ));
            }
            if searched && (coverage.pending > 0 || !complete) {
                self.project_cache.request_index(anchor.clone());
            }
            crate::trace::trace!(
                "  index target {display}: {} sources after {:.1} ms",
                result.sources.len() - sources_before,
                target_started.elapsed().as_secs_f64() * 1000.0
            );
        }

        for uri in must_include {
            let Ok(path) = uri.to_file_path().map(absolute_path) else {
                continue;
            };
            let path_key = path_key(&path);
            if deleted(&path) || taken.contains(&path_key) {
                continue;
            }
            let member_of = targets.contexts.iter().map(|(key, _)| key).find(|key| {
                self.project_cache
                    .read_index(key, |index, _| index.state_of(&path).is_some())
                    .unwrap_or(false)
            });
            let known = input
                .document_owners
                .get(uri)
                .or_else(|| input.document_owners.get(&super::canonical_file_uri(uri)));
            let owner = match (member_of, known) {
                (Some(key), _) => Some(key.clone()),
                (None, Some(known)) => {
                    result
                        .contexts
                        .entry(known.key.clone())
                        .or_insert_with(|| super::ContextState::clone(&known.state));
                    Some(known.key.clone())
                }
                // A declaring document must be bound: the first target that
                // would read it owns it.
                (None, None) => targets
                    .contexts
                    .iter()
                    .map(|(key, _)| key)
                    .find(|key| self.index_accepts(key, &path))
                    .cloned(),
            };
            match owner {
                Some(owner) => {
                    taken.insert(path_key);
                    result.sources.push((path, owner));
                }
                // An open buffer is still read by the snapshot's overlay step.
                None if open.contains(&path_key) => {}
                None => unowned += 1,
            }
        }
        if unowned > 0 {
            result.gaps.push(format!(
                "{unowned} requested sources belong to no searched context and were not searched"
            ));
        }

        if refused > 0 {
            result.gaps.push(format!(
                "{refused} units could not be indexed: {}",
                first_refusal.as_deref().unwrap_or("unknown reason")
            ));
        }
        if memory {
            result
                .gaps
                .push("the workspace index memory share is exhausted".to_owned());
        }
        Ok((result, symbols))
    }

    /// The configured project file; a relative one is looked up under the
    /// workspace roots.
    fn configured_project_path(&self) -> Option<PathBuf> {
        let project = self.configured_project_file()?;
        if project.is_absolute() {
            return Some(project.to_path_buf());
        }
        self.roots
            .iter()
            .map(|root| root.path.join(project))
            .find(|candidate| candidate.is_file())
    }
}

/// What the root catalogues list, keyed by `path_key` and deduplicated.
#[derive(Default)]
struct RootFiles {
    complete: bool,
    sources: Vec<(String, PathBuf)>,
    descriptors: Vec<(String, PathBuf)>,
}

impl RootFiles {
    fn small(&self) -> bool {
        self.complete
            && self.sources.len() <= small_workspace_sources()
            && self.descriptors.len() <= small_workspace_projects()
    }
}

/// What one target's index says, copied out under the index lock.
struct TargetView {
    candidates: Candidates,
    /// Per excluded member: whether every include slot resolved, and the
    /// stamps it was indexed under.
    recorded: Vec<(bool, Vec<MemberStamp>)>,
    /// The matching declarations of the excluded members (symbols only).
    symbols: Vec<IndexedSymbol>,
    /// More declarations matched than the request may return.
    exceeded: bool,
    coverage: IndexCoverage,
    scopes: Vec<(CatalogueKey, u64)>,
    complete: bool,
    limit: Option<String>,
    packages: bool,
    owner_pending: usize,
}

/// What a request reads of each target's index.
#[derive(Clone, Copy)]
enum Selection<'a> {
    /// References: the members `query` selects; `None` selects none.
    Candidates(Option<&'a CandidateQuery>),
    /// Workspace symbols: the members with declarations matching `query`
    /// are excluded, so they are verified and answered from their facts.
    Symbols { query: &'a str, direct: bool },
}

fn target_view(
    index: &ContextIndex,
    membership: &IndexMembership,
    selection: Selection<'_>,
    unserved: &[PathBuf],
    seen: &mut HashSet<SymbolKey>,
    limit: usize,
) -> TargetView {
    let every = || {
        index.candidates(&CandidateQuery {
            names: Vec::new(),
            top_level_units: None,
            aliases: Vec::new(),
        })
    };
    let mut symbols = Vec::new();
    let mut exceeded = false;
    let candidates = match selection {
        Selection::Candidates(Some(query)) => index.candidates(query),
        Selection::Candidates(None) => {
            let mut all = every();
            all.selected.clear();
            all.excluded.clear();
            all
        }
        Selection::Symbols { query, .. } => {
            // A member reading an open or deleted file is read directly.
            let mut direct = HashSet::new();
            for path in unserved {
                direct.insert(path_key(path));
                direct.extend(
                    index
                        .include_dependents(path)
                        .iter()
                        .map(|member| path_key(member)),
                );
            }
            let scan = index.symbol_scan(query, &direct, seen, limit);
            exceeded = scan.exceeded;
            symbols = scan.symbols;
            Candidates {
                selected: scan.direct,
                unverified: scan.unverified,
                refused: scan.refused,
                excluded: scan.contributing,
            }
        }
    };
    let recorded = candidates
        .excluded
        .iter()
        .map(|path| {
            let resolved = index
                .slot_paths_of(path)
                .is_some_and(|slots| slots.iter().skip(1).all(Option::is_some));
            let stamps = index.stamps_of(path).map(<[_]>::to_vec).unwrap_or_default();
            (resolved, stamps)
        })
        .collect();
    TargetView {
        candidates,
        recorded,
        symbols,
        exceeded,
        coverage: index.coverage(),
        scopes: membership.scopes.clone(),
        complete: membership.complete,
        limit: membership.limit.clone(),
        packages: membership.packages,
        owner_pending: membership.owner_pending,
    }
}

/// The directories a membership's catalogues listed, with their stamps.
#[derive(Default)]
struct MembershipListing {
    directories: Vec<(PathBuf, Option<PathStamp>)>,
    /// A catalogue stopped at a limit or could not read a directory.
    incomplete: bool,
}

/// Adds `unverified` (path order) to `sources` while the request-wide bound
/// lasts; open buffers are always read (A4) and do not use it. Returns how
/// many were skipped and how many used the bound.
fn take_unverified(
    unverified: Vec<PathBuf>,
    key: &ContextKey,
    open: &HashSet<String>,
    deleted: &dyn Fn(&Path) -> bool,
    taken: &mut HashSet<String>,
    left: &mut usize,
    sources: &mut Vec<(PathBuf, ContextKey)>,
) -> (usize, usize) {
    let (mut skipped, mut searched) = (0, 0);
    for path in unverified {
        let path_key = path_key(&path);
        if deleted(&path) || taken.contains(&path_key) {
            continue;
        }
        if !open.contains(&path_key) {
            if *left == 0 {
                skipped += 1;
                continue;
            }
            *left -= 1;
            searched += 1;
        }
        taken.insert(path_key);
        sources.push((path, key.clone()));
    }
    (skipped, searched)
}

fn outside_gap(gaps: &mut Vec<String>, outside: usize, display: &str) {
    if outside > 0 {
        gaps.push(format!(
            "{outside} units of {display} outside the workspace roots were not searched"
        ));
    }
}

/// How coverage notes name a context: its project file, or the document
/// that anchors a projectless one.
fn anchor_display(anchor: &IndexAnchor) -> String {
    match anchor {
        IndexAnchor::Project(path) => path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        ),
        IndexAnchor::Document(uri) => uri
            .to_file_path()
            .map_or_else(|()| uri.to_string(), |path| path.display().to_string()),
    }
}

fn keyed(path: &Path) -> (String, PathBuf) {
    (path_key(path), path.to_path_buf())
}

fn is_project_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("dproj"))
}

/// The directory and lowercase stem a `.dpr`/`.dpk` shares with its `.dproj`.
fn sibling_stem(path: &Path) -> Option<(PathBuf, String)> {
    Some((
        path.parent()?.to_path_buf(),
        path.file_stem()?.to_string_lossy().to_lowercase(),
    ))
}

fn is_descriptor(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        ["dproj", "dpr", "dpk"]
            .iter()
            .any(|descriptor| extension.eq_ignore_ascii_case(descriptor))
    })
}

#[cfg(test)]
pub(in crate::workspace) mod tests {
    use super::*;
    use crate::catalogue::{Catalogue, Depth, test_listings, test_reset_listings};
    use crate::project_cache::{IndexAnchor, IndexMembership};
    use crate::workspace::WorkspaceOptions;
    use crate::workspace::rename::CANCELLATION_MESSAGE;
    use crate::workspace_index::{CandidateQuery, MemberState};
    use pascal_project::delphi_overrides::OverrideSession;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    pub(in crate::workspace) fn workspace(root: &Path, options: WorkspaceOptions) -> Workspace {
        Workspace::with_override_session(
            vec![root.to_path_buf()],
            options,
            OverrideSession::new(None),
        )
    }

    pub(in crate::workspace) fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn unit(path: &Path) {
        write(path, "unit X; interface implementation end.\n");
    }

    fn project(root: &Path, properties: &str, items: &str) -> PathBuf {
        let descriptor = root.join("App.dproj");
        write(
            &descriptor,
            &format!(
                "<Project><PropertyGroup><MainSource>App.dpr</MainSource>{properties}</PropertyGroup><ItemGroup>{items}</ItemGroup></Project>"
            ),
        );
        write(&root.join("App.dpr"), "program App; begin end.\n");
        descriptor
    }

    fn context(workspace: &mut Workspace, anchor: IndexAnchor) -> ContextKey {
        workspace
            .index_context_for(&anchor, &AtomicBool::new(false))
            .expect("context")
    }

    fn membership(workspace: &mut Workspace, key: &ContextKey) -> IndexMembership {
        workspace
            .index_membership(key, &AtomicBool::new(false))
            .expect("membership")
    }

    fn relative(membership: &IndexMembership, root: &Path) -> Vec<String> {
        membership
            .paths
            .iter()
            .map(|path| {
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    pub(in crate::workspace) fn project_workspace(
        root: &Path,
        properties: &str,
        items: &str,
        options: WorkspaceOptions,
    ) -> (Workspace, ContextKey) {
        let descriptor = project(root, properties, items);
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, options);
        let key = context(&mut workspace, IndexAnchor::Project(descriptor));
        (workspace, key)
    }

    #[test]
    fn a_project_membership_lists_search_paths_without_recursing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        unit(&root.join("lib/A.pas"));
        unit(&root.join("lib/Nested/Deep.pas"));
        let (mut workspace, key) = project_workspace(
            root,
            "<DCC_UnitSearchPath>lib</DCC_UnitSearchPath>",
            "",
            WorkspaceOptions::default(),
        );
        let found = membership(&mut workspace, &key);
        assert_eq!(relative(&found, root), ["App.dpr", "lib/A.pas"]);
        assert!(found.complete);
        assert!(
            found
                .scopes
                .iter()
                .all(|(scope, _)| scope.depth == Depth::Listing)
        );
    }

    #[test]
    fn explicit_units_outside_search_paths_are_members_with_their_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        unit(&root.join("ext/Extra.pas"));
        unit(&root.join("ext/Sibling.pas"));
        let (mut workspace, key) = project_workspace(
            root,
            "",
            "<DCCReference Include=\"ext/Extra.pas\"/>",
            WorkspaceOptions::default(),
        );
        let found = membership(&mut workspace, &key);
        assert_eq!(
            relative(&found, root),
            ["App.dpr", "ext/Extra.pas", "ext/Sibling.pas"]
        );
        assert!(
            found
                .scopes
                .iter()
                .any(|(scope, _)| scope.root == root.join("ext") && scope.depth == Depth::Listing)
        );
    }

    #[test]
    fn a_projectless_membership_is_the_recursive_roots_filtered_by_owner() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        unit(&root.join("A.pas"));
        unit(&root.join("proj/B.pas"));
        write(
            &root.join("proj/P.dproj"),
            "<Project><PropertyGroup><MainSource>B.pas</MainSource></PropertyGroup></Project>",
        );
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let uri = Url::from_file_path(root.join("A.pas")).unwrap();
        let key = context(&mut workspace, IndexAnchor::Document(uri));
        let found = membership(&mut workspace, &key);
        assert_eq!(relative(&found, root), ["A.pas"]);
        assert!(found.complete);
        assert_eq!(found.owner_pending, 0);
        assert!(
            found
                .scopes
                .iter()
                .any(|(scope, _)| scope.depth == Depth::Recursive)
        );
    }

    #[test]
    fn members_failing_acceptance_are_left_out() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        unit(&root.join("lib/A.pas"));
        unit(&root.join("skipped/B.pas"));
        let options = WorkspaceOptions {
            exclude: vec!["skipped".to_owned()],
            ..WorkspaceOptions::default()
        };
        let (mut workspace, key) = project_workspace(
            root,
            "<DCC_UnitSearchPath>lib;skipped</DCC_UnitSearchPath>",
            "",
            options,
        );
        let found = membership(&mut workspace, &key);
        assert_eq!(relative(&found, root), ["App.dpr", "lib/A.pas"]);
    }

    #[test]
    fn packages_are_flagged_not_enumerated() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        unit(&root.join("pkg/Provided.pas"));
        write(
            &root.join("pkg/Shared.dpk"),
            "package Shared;\ncontains\n  Provided in 'Provided.pas';\nend.\n",
        );
        let (mut workspace, key) = project_workspace(
            root,
            "<DCC_UsePackage>Shared</DCC_UsePackage>",
            "",
            WorkspaceOptions::default(),
        );
        let found = membership(&mut workspace, &key);
        assert!(found.packages);
        assert_eq!(relative(&found, root), ["App.dpr"]);
    }

    #[test]
    fn an_empty_membership_is_never_complete() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        unit(&root.join("lib/A.pas"));
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let uri = Url::from_file_path(root.join("lib/A.pas")).unwrap();
        let key = context(&mut workspace, IndexAnchor::Document(uri));
        fs::remove_file(root.join("lib/A.pas")).unwrap();
        workspace
            .project_cache
            .invalidate_path(&root.join("lib/A.pas"));
        let found = membership(&mut workspace, &key);
        assert!(found.paths.is_empty());
        assert!(!found.complete);
    }

    #[test]
    fn membership_is_capped_in_path_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for name in ["A", "B", "C", "D"] {
            unit(&root.join(format!("lib/{name}.pas")));
        }
        let (mut workspace, key) = project_workspace(
            root,
            "<DCC_UnitSearchPath>lib</DCC_UnitSearchPath>",
            "",
            WorkspaceOptions::default(),
        );
        let found = membership(&mut workspace, &key);
        assert_eq!(found.paths.len(), 5);
        let all = found.paths.clone();
        workspace.project_cache.set_index_unit_limit_for_test(3);
        let anchor = IndexAnchor::Project(root.join("App.dproj"));
        workspace
            .project_cache
            .set_index_membership(key.clone(), anchor, found, usize::MAX);
        let (stored, limited, complete) = workspace
            .project_cache
            .read_index(&key, |index, membership| {
                (
                    index.member_paths().len(),
                    membership.limit.is_some(),
                    membership.complete,
                )
            })
            .unwrap();
        assert_eq!((stored, limited, complete), (3, true, false));
        let kept = workspace
            .project_cache
            .read_index(&key, |index, _| index.member_paths())
            .unwrap();
        let mut expected = all;
        expected.truncate(3);
        assert_eq!(kept, expected);
    }

    #[test]
    fn membership_records_its_catalogue_scopes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        unit(&root.join("lib/A.pas"));
        let (mut workspace, key) = project_workspace(
            root,
            "<DCC_UnitSearchPath>lib</DCC_UnitSearchPath>",
            "",
            WorkspaceOptions::default(),
        );
        let found = membership(&mut workspace, &key);
        assert!(found.scopes.iter().all(|(_, generation)| *generation > 0));
        let anchor = IndexAnchor::Project(root.join("App.dproj"));
        workspace
            .project_cache
            .set_index_membership(key.clone(), anchor, found, usize::MAX);
        assert!(workspace.project_cache.index_membership_current(&key));
        workspace
            .project_cache
            .invalidate_path(&root.join("lib/New.pas"));
        assert!(!workspace.project_cache.index_membership_current(&key));
    }

    #[test]
    fn a_second_membership_lists_no_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        unit(&root.join("lib/A.pas"));
        let (mut workspace, key) = project_workspace(
            root,
            "<DCC_UnitSearchPath>lib</DCC_UnitSearchPath>",
            "",
            WorkspaceOptions::default(),
        );
        let first = membership(&mut workspace, &key);
        test_reset_listings();
        let second = membership(&mut workspace, &key);
        assert_eq!(first.paths, second.paths);
        assert!(test_listings().is_empty());
    }

    #[test]
    fn owner_discovery_is_capped_per_call_and_resumes_from_the_owner_map() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let directories = MAX_REQUEST_OWNER_DISCOVERY + 2;
        for index in 0..directories {
            unit(&root.join(format!("d{index:03}/U.pas")));
        }
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let uri = Url::from_file_path(root.join("d000/U.pas")).unwrap();
        let key = context(&mut workspace, IndexAnchor::Document(uri));
        let first = membership(&mut workspace, &key);
        assert_eq!(first.owner_pending, 2);
        assert!(!first.complete);
        for index in 0..directories {
            let directory = root.join(format!("d{index:03}"));
            let remembered = workspace.project_cache.owner_of(&directory).is_some();
            assert_eq!(remembered, index < directories - 2, "d{index:03}");
        }
        let second = membership(&mut workspace, &key);
        for index in 0..directories {
            let directory = root.join(format!("d{index:03}"));
            assert!(workspace.project_cache.owner_of(&directory).is_some());
        }
        assert_eq!(second.owner_pending, 0);
        assert!(second.complete);
        assert!(second.paths.contains(&root.join("d000/U.pas")));
    }

    #[test]
    fn a_missing_search_path_is_a_complete_empty_listing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        unit(&root.join("lib/A.pas"));
        let (mut workspace, key) = project_workspace(
            root,
            "<DCC_UnitSearchPath>lib;gone</DCC_UnitSearchPath>",
            "",
            WorkspaceOptions::default(),
        );
        let found = membership(&mut workspace, &key);
        assert_eq!(relative(&found, root), ["App.dpr", "lib/A.pas"]);
        assert!(found.complete);
        let gone = found
            .scopes
            .iter()
            .find(|(scope, _)| scope.root == root.join("gone"))
            .expect("the missing directory is a recorded scope");
        assert_eq!(gone.0.depth, Depth::Listing);
        assert!(gone.1 > 0);
    }

    pub(in crate::workspace) fn named_unit(path: &Path, identifier: &str) {
        let unit = path.file_stem().unwrap().to_string_lossy().into_owned();
        write(
            path,
            &format!(
                "unit {unit};\ninterface\nprocedure {identifier};\nimplementation\nprocedure {identifier};\nbegin\nend;\nend.\n"
            ),
        );
    }

    /// App.dpr plus `count - 1` units `lib/U<n>.pas`, each declaring `P<n>`.
    pub(in crate::workspace) fn indexed_project(
        root: &Path,
        count: usize,
        options: WorkspaceOptions,
    ) -> (Workspace, ContextKey, IndexAnchor) {
        for index in 0..count - 1 {
            named_unit(
                &root.join(format!("lib/U{index}.pas")),
                &format!("P{index}"),
            );
        }
        let (workspace, key) = project_workspace(
            root,
            "<DCC_UnitSearchPath>lib</DCC_UnitSearchPath>",
            "",
            options,
        );
        (workspace, key, IndexAnchor::Project(root.join("App.dproj")))
    }

    pub(in crate::workspace) fn advance(
        workspace: &mut Workspace,
        anchor: &IndexAnchor,
        bound: IndexBound,
    ) -> IndexProgress {
        workspace
            .advance_workspace_index(anchor, bound, &mut || Ok(()), &AtomicBool::new(false))
            .expect("batch")
    }

    pub(in crate::workspace) fn state(
        workspace: &Workspace,
        key: &ContextKey,
        path: &Path,
    ) -> Option<MemberState> {
        workspace
            .project_cache
            .read_index(key, |index, _| index.state_of(path))
            .flatten()
    }

    pub(in crate::workspace) fn members(workspace: &Workspace, key: &ContextKey) -> Vec<PathBuf> {
        workspace
            .project_cache
            .read_index(key, |index, _| index.member_paths())
            .unwrap_or_default()
    }

    pub(in crate::workspace) fn selected(
        workspace: &Workspace,
        key: &ContextKey,
        name: &str,
    ) -> Vec<PathBuf> {
        workspace
            .project_cache
            .read_index(key, |index, _| {
                index
                    .candidates(&CandidateQuery {
                        names: vec![name.to_owned()],
                        top_level_units: None,
                        aliases: Vec::new(),
                    })
                    .selected
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_batch_indexes_at_most_its_bound_and_resumes() {
        let temp = tempfile::tempdir().unwrap();
        let (mut workspace, key, anchor) =
            indexed_project(temp.path(), 10, WorkspaceOptions::default());
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(advance(&mut workspace, &anchor, IndexBound::units(4)));
        }
        let pending: Vec<_> = seen.iter().map(|progress| progress.pending).collect();
        assert_eq!(pending, [6, 2, 0]);
        let indexed: Vec<_> = seen.iter().map(|progress| progress.indexed).collect();
        assert_eq!(indexed, [4, 4, 2]);
        let complete: Vec<_> = seen.iter().map(|progress| progress.complete).collect();
        assert_eq!(complete, [false, false, true]);
        assert!(seen.iter().all(|progress| !progress.memory));
        for path in members(&workspace, &key) {
            assert_eq!(state(&workspace, &key, &path), Some(MemberState::Fresh));
        }
        assert!(!workspace.index_build);
    }

    #[test]
    fn a_cancelled_batch_keeps_what_it_published() {
        let temp = tempfile::tempdir().unwrap();
        let (mut workspace, key, anchor) =
            indexed_project(temp.path(), 6, WorkspaceOptions::default());
        let mut calls = 0;
        let result = workspace.advance_workspace_index(
            &anchor,
            IndexBound::all(),
            &mut || {
                calls += 1;
                if calls == 3 {
                    Err(CANCELLATION_MESSAGE.to_owned())
                } else {
                    Ok(())
                }
            },
            &AtomicBool::new(false),
        );
        assert_eq!(result.unwrap_err(), CANCELLATION_MESSAGE);
        assert!(!workspace.index_build);
        let all = members(&workspace, &key);
        let states: Vec<_> = all
            .iter()
            .map(|path| state(&workspace, &key, path).unwrap())
            .collect();
        assert_eq!(&states[..2], [MemberState::Fresh, MemberState::Fresh]);
        assert!(
            states[2..]
                .iter()
                .all(|state| *state == MemberState::Unindexed)
        );
        let resumed = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!((resumed.indexed, resumed.pending), (all.len() - 2, 0));
    }

    #[test]
    fn index_batches_do_not_publish_parses() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, key, anchor) = indexed_project(temp.path(), 4, WorkspaceOptions::default());
        let mut view = Workspace::from_analysis_input(&workspace.analysis_input());
        advance(&mut view, &anchor, IndexBound::all());
        assert_eq!(view.project_cache.stats().units, 0);
        let first = Url::from_file_path(temp.path().join("lib/U0.pas")).unwrap();
        assert!(view.index.parsed_document(&first).is_none());
        for path in members(&view, &key) {
            assert!(!view.index.contains(&Url::from_file_path(path).unwrap()));
        }
    }

    #[test]
    fn a_cached_parse_is_still_used_by_a_batch() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, key, anchor) = indexed_project(temp.path(), 4, WorkspaceOptions::default());
        let cancel = AtomicBool::new(false);
        let mut seeder = Workspace::from_analysis_input(&workspace.analysis_input());
        assert!(context(&mut seeder, anchor.clone()) == key);
        seeder
            .advance_workspace_index(&anchor, IndexBound::units(0), &mut || Ok(()), &cancel)
            .unwrap();
        assert!(context(&mut seeder, anchor.clone()) == key);
        seeder.pinned_context = Some(key.clone());
        let all = members(&seeder, &key);
        for path in &all {
            let uri = Url::from_file_path(path).unwrap();
            assert!(
                seeder
                    .load_source_with_cancel(&uri, &key, &HashSet::new(), Some(&cancel))
                    .unwrap()
            );
        }
        let seeded = seeder.project_cache.stats().units;
        assert_eq!(seeded, all.len());
        let mut view = Workspace::from_analysis_input(&workspace.analysis_input());
        crate::navigation::test_reset_document_parse_count();
        let progress = advance(&mut view, &anchor, IndexBound::all());
        assert_eq!(progress.indexed, all.len());
        assert_eq!(crate::navigation::test_document_parse_count(), 0);
        assert_eq!(view.project_cache.stats().units, seeded);
    }

    #[test]
    fn an_edited_member_is_reindexed_on_the_next_batch() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 3, WorkspaceOptions::default());
        advance(&mut workspace, &anchor, IndexBound::all());
        let edited = root.join("lib/U0.pas");
        assert_eq!(
            selected(&workspace, &key, "P0"),
            std::slice::from_ref(&edited)
        );
        named_unit(&edited, "BrandNewIdentifier");
        workspace.project_cache.invalidate_file_contents(&edited);
        assert_eq!(state(&workspace, &key, &edited), Some(MemberState::Stale));
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!((progress.indexed, progress.refreshed), (1, 0));
        assert_eq!(
            selected(&workspace, &key, "BrandNewIdentifier"),
            std::slice::from_ref(&edited)
        );
        assert!(selected(&workspace, &key, "P0").is_empty());
    }

    #[test]
    fn a_stale_member_with_an_unchanged_stamp_is_refreshed_without_reading() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 3, WorkspaceOptions::default());
        advance(&mut workspace, &anchor, IndexBound::all());
        let member = root.join("lib/U0.pas");
        workspace.project_cache.mark_index_stale(&key, &member);
        assert_eq!(state(&workspace, &key, &member), Some(MemberState::Stale));
        crate::navigation::test_reset_document_parse_count();
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!(
            (progress.indexed, progress.refreshed, progress.pending),
            (0, 1, 0)
        );
        assert_eq!(crate::navigation::test_document_parse_count(), 0);
        assert_eq!(state(&workspace, &key, &member), Some(MemberState::Fresh));
    }

    #[test]
    fn a_renamed_file_leaves_and_its_new_path_joins() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 3, WorkspaceOptions::default());
        advance(&mut workspace, &anchor, IndexBound::all());
        let old = root.join("lib/U1.pas");
        let new = root.join("lib/Renamed.pas");
        fs::rename(&old, &new).unwrap();
        workspace.project_cache.invalidate_path(&old);
        workspace.project_cache.invalidate_path(&new);
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!((progress.indexed, progress.pending), (1, 0));
        assert_eq!(state(&workspace, &key, &old), None);
        assert_eq!(state(&workspace, &key, &new), Some(MemberState::Fresh));
    }

    #[test]
    fn a_deleted_member_is_removed_and_uncharged() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 4, WorkspaceOptions::default());
        advance(&mut workspace, &anchor, IndexBound::all());
        let before = workspace.project_cache.stats().reserved_index_bytes;
        let gone = root.join("lib/U2.pas");
        fs::remove_file(&gone).unwrap();
        workspace.project_cache.invalidate_path(&gone);
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!((progress.pending, progress.complete), (0, true));
        assert_eq!(state(&workspace, &key, &gone), None);
        assert!(selected(&workspace, &key, "P2").is_empty());
        assert!(workspace.project_cache.stats().reserved_index_bytes < before);
    }

    #[test]
    fn a_changed_include_restales_its_dependents() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("lib/x.inc"), "procedure FromInclude;\n");
        write(
            &root.join("lib/A.pas"),
            "unit A;\ninterface\n{$I x.inc}\nimplementation\nprocedure FromInclude;\nbegin\nend;\nend.\n",
        );
        let (mut workspace, key, anchor) = indexed_project(root, 2, WorkspaceOptions::default());
        advance(&mut workspace, &anchor, IndexBound::all());
        let member = root.join("lib/A.pas");
        let include = root.join("lib/x.inc");
        assert_eq!(
            selected(&workspace, &key, "FromInclude"),
            std::slice::from_ref(&member)
        );
        let slots = workspace
            .project_cache
            .read_index(&key, |index, _| {
                index.slot_paths_of(&member).map(<[_]>::to_vec)
            })
            .flatten()
            .expect("slots");
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].as_deref(), Some(member.as_path()));
        assert_eq!(slots[1].as_deref(), Some(include.as_path()));
        write(&include, "procedure ChangedByEdit;\n");
        workspace.project_cache.invalidate_file_contents(&include);
        assert_eq!(state(&workspace, &key, &member), Some(MemberState::Stale));
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!((progress.indexed, progress.refreshed), (1, 0));
        assert_eq!(selected(&workspace, &key, "ChangedByEdit"), [member]);
    }

    #[test]
    fn an_oversized_member_is_refused_with_a_stable_reason() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let mut options = WorkspaceOptions::default();
        options.limits.max_file_bytes = 200;
        let (mut workspace, key, anchor) = indexed_project(root, 3, options);
        let big = root.join("lib/Big.pas");
        write(
            &big,
            &format!(
                "unit Big;\n{}\ninterface implementation end.\n",
                "{ filler }\n".repeat(60)
            ),
        );
        pascal_project::backdate_tree_for_test(root);
        workspace.project_cache.invalidate_path(&big);

        let cancel = AtomicBool::new(false);
        let cancelled = workspace.advance_workspace_index(
            &anchor,
            IndexBound::all(),
            &mut || {
                cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            &cancel,
        );
        assert_eq!(cancelled.unwrap_err(), CANCELLATION_MESSAGE);
        assert_eq!(state(&workspace, &key, &big), Some(MemberState::Unindexed));

        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!(progress.refused, 1);
        assert_eq!(progress.pending, 0);
        assert!(progress.complete);
        match state(&workspace, &key, &big) {
            Some(MemberState::Refused(reason)) => {
                assert!(reason.contains("per-file limit"), "{reason}");
            }
            other => panic!("expected a refusal, found {other:?}"),
        }
    }

    #[test]
    fn the_stamp_is_taken_before_the_bytes_are_read() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let member = root.join("A.pas");
        named_unit(&member, "Original");
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let anchor = IndexAnchor::Document(Url::from_file_path(&member).unwrap());
        let key = context(&mut workspace, anchor.clone());
        workspace
            .advance_workspace_index(
                &anchor,
                IndexBound::units(0),
                &mut || Ok(()),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(members(&workspace, &key), std::slice::from_ref(&member));
        let rewrite = member.clone();
        let progress = workspace
            .advance_workspace_index(
                &anchor,
                IndexBound::all(),
                &mut || {
                    named_unit(&rewrite, "RewrittenAfterTheStamp");
                    Ok(())
                },
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(progress.indexed, 1);
        let proves = workspace
            .project_cache
            .read_index(&key, |index, _| {
                let stored = index.stamps_of(&member).expect("stamps")[0]
                    .stamp
                    .clone()
                    .expect("a stamp");
                let current = crate::workspace::path_stamp(&member).expect("current");
                stored.proves_unchanged(&current)
            })
            .unwrap();
        assert!(!proves, "the stored stamp must predate the rewrite");
    }

    #[test]
    fn a_full_index_share_stops_the_batch_without_a_requeue_loop() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut probe, _, anchor) = indexed_project(root, 8, WorkspaceOptions::default());
        advance(&mut probe, &anchor, IndexBound::units(0));
        let membership = probe.project_cache.stats().reserved_index_bytes;
        advance(&mut probe, &anchor, IndexBound::all());
        let total = probe.project_cache.stats().reserved_index_bytes;

        let (mut workspace, key, anchor) = indexed_project(root, 8, WorkspaceOptions::default());
        workspace.project_cache =
            crate::project_cache::ProjectCache::new(4 * (membership + (total - membership) / 2));
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert!(progress.memory);
        assert!(!progress.complete);
        assert!(progress.pending > 0);
        assert!(progress.indexed > 0);
        assert!(!workspace.index_build);
        assert!(
            workspace
                .project_cache
                .read_index(&key, |index, _| index.coverage().fresh > 0)
                .unwrap()
        );
    }

    #[test]
    fn an_include_stamp_is_the_one_the_load_observed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let include = root.join("lib/x.inc");
        let original = "procedure FromInclude;\n";
        write(&include, original);
        write(
            &root.join("lib/A.pas"),
            "unit A;\ninterface\n{$I x.inc}\nimplementation\nprocedure FromInclude;\nbegin\nend;\nend.\n",
        );
        let (mut workspace, key, anchor) = indexed_project(root, 2, WorkspaceOptions::default());
        let rewritten = include.clone();
        set_after_load_hook(Some(Box::new(move |loaded| {
            if loaded.ends_with("A.pas") {
                write(
                    &rewritten,
                    "procedure RewrittenAfterTheLoad_ThatIsLonger;\n",
                );
            }
        })));
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        set_after_load_hook(None);
        assert_eq!(progress.indexed, members(&workspace, &key).len());
        let member = root.join("lib/A.pas");
        let stored = workspace
            .project_cache
            .read_index(&key, |index, _| index.stamps_of(&member).map(<[_]>::to_vec))
            .flatten()
            .expect("stamps");
        assert_eq!(stored.len(), 2);
        let observed = stored[1].stamp.as_ref().expect("an observed stamp");
        assert_eq!(observed.bytes, original.len() as u64);
        let current = crate::workspace::path_stamp(&include).expect("current");
        assert!(!observed.proves_unchanged(&current));
    }

    #[test]
    fn a_load_that_cannot_retain_leaves_the_member_pending() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let mut options = WorkspaceOptions::default();
        options.limits.max_total_bytes = 10;
        let (mut workspace, key, anchor) = indexed_project(root, 3, options);
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!((progress.indexed, progress.refused), (0, 0));
        assert!(progress.deferred > 0);
        assert_eq!(progress.pending, members(&workspace, &key).len());
        assert!(!progress.complete);
        for path in members(&workspace, &key) {
            assert_eq!(state(&workspace, &key, &path), Some(MemberState::Unindexed));
        }
    }

    /// A unit including an include that includes itself: the session stops
    /// at the include depth limit, so its facts are limited
    /// (`NotPersistable::Transient`) every time it is read.
    fn limited_unit(path: &Path) {
        write(&path.with_file_name("x.inc"), "{$I x.inc}\n");
        write(
            path,
            "unit U0;\ninterface\n{$I x.inc}\nimplementation\nend.\n",
        );
    }

    #[test]
    fn a_member_with_limited_facts_stays_an_unverified_candidate_and_the_batch_continues() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        limited_unit(&root.join("lib/U0.pas"));
        for index in 1..3 {
            named_unit(
                &root.join(format!("lib/U{index}.pas")),
                &format!("P{index}"),
            );
        }
        let (mut workspace, key) = project_workspace(
            root,
            "<DCC_UnitSearchPath>lib</DCC_UnitSearchPath>",
            "",
            WorkspaceOptions::default(),
        );
        let anchor = IndexAnchor::Project(root.join("App.dproj"));
        let limited = root.join("lib/U0.pas");
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!((progress.indexed, progress.limited), (3, 1), "{progress:?}");
        assert_eq!(progress.pending, 0);
        assert!(progress.complete);
        assert_eq!(
            state(&workspace, &key, &limited),
            Some(MemberState::Limited)
        );
        let again = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!((again.indexed, again.limited, again.pending), (0, 0, 0));

        let input = workspace.analysis_input();
        let query = CandidateQuery {
            names: vec!["P1".to_owned()],
            top_level_units: None,
            aliases: Vec::new(),
        };
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            &[],
        );
        assert_eq!(source_names(&found), ["U0.pas", "U1.pas"]);

        workspace.project_cache.invalidate_path(&limited);
        assert_eq!(state(&workspace, &key, &limited), Some(MemberState::Stale));
    }

    /// A unit whose include expansion exceeds a compile-time limit fails the
    /// same way for the same bytes, in a batch as in a request (A6).
    #[test]
    fn a_member_over_a_compile_time_limit_is_refused_with_its_reason() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 3, WorkspaceOptions::default());
        let heavy = root.join("lib/U0.pas");
        write(
            &root.join("lib/big.inc"),
            &format!("{}\n", "{ padding }\n".repeat(4_000)),
        );
        write(
            &heavy,
            "unit U0;\ninterface\n{$I big.inc}\nprocedure P0;\nimplementation\nprocedure P0;\nbegin\nend;\nend.\n",
        );
        let _cap = super::super::resolver::test_byte_work_cap(4_096);
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!(
            (progress.indexed, progress.refused, progress.deferred),
            (2, 1, 0),
            "{progress:?}"
        );
        assert!(progress.complete, "{progress:?}");
        match state(&workspace, &key, &heavy) {
            Some(MemberState::Refused(reason)) => {
                assert!(reason.contains("limit"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// Deferred members go behind the members never tried, so a run of
    /// failures at the head of the path order cannot stall the index; once
    /// the condition clears they are indexed too.
    #[test]
    fn deferred_members_go_behind_untried_ones_and_are_retried_last() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 7, WorkspaceOptions::default());
        // App.dpr and U0, U1 head the path order and keep failing.
        let failing = vec![
            root.join("App.dpr"),
            root.join("lib/U0.pas"),
            root.join("lib/U1.pas"),
        ];
        set_member_errors(failing.clone());
        let first = advance(&mut workspace, &anchor, IndexBound::units(3));
        assert_eq!((first.indexed, first.deferred), (0, 3), "{first:?}");
        assert!(first.untried > 0 && first.worth_requeue(), "{first:?}");
        let second = advance(&mut workspace, &anchor, IndexBound::units(3));
        assert_eq!((second.indexed, second.deferred), (3, 0), "{second:?}");
        let third = advance(&mut workspace, &anchor, IndexBound::units(3));
        assert_eq!((third.indexed, third.deferred), (1, 2), "{third:?}");
        assert!(third.worth_requeue(), "{third:?}");
        // Only the deferred are left: the batch retries them, fails again
        // and stops, telling why.
        let stalled = advance(&mut workspace, &anchor, IndexBound::units(3));
        assert_eq!(
            (stalled.indexed, stalled.deferred, stalled.untried),
            (0, 3, 0)
        );
        assert!(!stalled.worth_requeue(), "{stalled:?}");
        let coverage = workspace
            .project_cache
            .read_index(&key, |index, _| index.coverage())
            .expect("an index");
        assert_eq!(coverage.deferred, 3);
        assert_eq!(
            coverage.first_deferral.as_deref(),
            Some("an injected member error")
        );
        let input = workspace.analysis_input();
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            None,
            &[],
        );
        assert!(
            found
                .gaps
                .contains(&"3 units of App.dproj failed to load for the index and are retried later: an injected member error".to_owned()),
            "{:?}",
            found.gaps
        );
        set_member_errors(Vec::new());
        let cleared = advance(&mut workspace, &anchor, IndexBound::units(3));
        assert_eq!((cleared.indexed, cleared.pending), (3, 0), "{cleared:?}");
        assert!(cleared.complete);
        let coverage = workspace
            .project_cache
            .read_index(&key, |index, _| index.coverage())
            .expect("an index");
        assert_eq!((coverage.deferred, coverage.first_deferral), (0, None));
        for path in failing {
            assert_eq!(state(&workspace, &key, &path), Some(MemberState::Fresh));
        }
    }

    /// A member that cannot be stamped (listed, but its stat fails) is
    /// skipped every batch without a load; beside a deferred member it must
    /// not keep the anchor requeued.
    #[test]
    fn an_unstampable_member_beside_a_deferred_one_does_not_requeue_forever() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 4, WorkspaceOptions::default());
        let broken = root.join("lib/U2.pas");
        let failing = root.join("lib/U0.pas");
        set_unstampable(vec![broken.clone()]);
        set_member_errors(vec![failing.clone()]);
        let mut batches = Vec::new();
        for _ in 0..6 {
            let progress = advance(&mut workspace, &anchor, IndexBound::all());
            batches.push(progress.clone());
            if !progress.worth_requeue() {
                break;
            }
        }
        set_member_errors(Vec::new());
        set_unstampable(Vec::new());
        assert!(
            state(&workspace, &key, &broken).is_some(),
            "the unstampable file is still a member"
        );
        let last = batches.last().unwrap();
        assert!(!last.worth_requeue(), "{batches:?}");
        assert!(batches.len() <= 2, "{batches:?}");
        assert!(last.deferred > 0 && last.pending >= 2, "{last:?}");
    }

    #[test]
    fn a_member_error_is_deferred_and_the_batch_continues() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 4, WorkspaceOptions::default());
        let failing = root.join("lib/U0.pas");
        set_member_error(Some(failing.clone()));
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        set_member_error(None);
        assert_eq!(
            (progress.indexed, progress.deferred),
            (3, 1),
            "{progress:?}"
        );
        assert_eq!(progress.pending, 1);
        assert_eq!(
            state(&workspace, &key, &failing),
            Some(MemberState::Unindexed)
        );
    }

    #[test]
    fn open_members_are_skipped_and_do_not_hold_the_index_incomplete() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 4, WorkspaceOptions::default());
        let open = root.join("lib/U1.pas");
        let uri = Url::from_file_path(&open).unwrap();
        workspace
            .open_document(
                uri,
                "unit U1; interface implementation end.\n".to_owned(),
                1,
            )
            .unwrap();
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!(progress.skipped_open, 1);
        assert_eq!(progress.indexed, members(&workspace, &key).len() - 1);
        assert_eq!(progress.pending, 0);
        assert!(progress.complete);
        assert_eq!(state(&workspace, &key, &open), Some(MemberState::Unindexed));
    }

    const EDITED: &str = "unit U0;\ninterface\nprocedure Edited;\nimplementation\nprocedure Edited; begin end;\nend.\n";

    /// D37: nothing of the editor reaches the producer view.
    #[test]
    fn a_baseline_view_holds_no_editor_state() {
        let temp = tempfile::tempdir().unwrap();
        let (mut workspace, key, anchor) =
            indexed_project(temp.path(), 3, WorkspaceOptions::default());
        let open = Url::from_file_path(temp.path().join("lib/U0.pas")).unwrap();
        workspace
            .open_document(open.clone(), EDITED.to_owned(), 1)
            .unwrap();
        workspace.remember_deleted(&Url::from_file_path(temp.path().join("lib/U1.pas")).unwrap());
        let session_input = workspace.analysis_input();
        assert!(!session_input.baseline && !session_input.overlays.is_empty());
        let input = session_input.disk_baseline();
        assert!(input.baseline && input.overlays.is_empty() && input.cached_documents.is_empty());
        assert!(input.rejected_documents.is_empty() && input.rejection_reasons.is_empty());
        assert!(input.deleted_overrides.is_empty() && input.document_versions.is_empty());
        assert_eq!(input.roots, vec![temp.path().to_path_buf()]);
        let mut view = Workspace::from_analysis_input(&input);
        assert!(view.is_baseline() && !workspace.is_baseline());
        assert!(!view.root_is_session_private(&open));
        assert!(view.open_documents.is_empty() && view.deleted_overrides.is_empty());
        assert_eq!(
            view.index_context_for(&anchor, &AtomicBool::new(false))
                .unwrap(),
            key,
            "same context key"
        );
    }

    /// D32: an open, edited member is indexed from its saved file.
    #[test]
    fn an_open_edited_member_is_indexed_and_persisted_from_disk() {
        use super::super::eligible::{RecordInputs, Unindexed};
        let temp = tempfile::tempdir().unwrap();
        let (mut workspace, key, anchor) =
            indexed_project(temp.path(), 3, WorkspaceOptions::default());
        let path = temp.path().join("lib/U0.pas");
        let open = Url::from_file_path(&path).unwrap();
        workspace
            .open_document(open.clone(), EDITED.to_owned(), 1)
            .unwrap();
        let session = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!(session.skipped_open, 1);
        assert_eq!(state(&workspace, &key, &path), Some(MemberState::Unindexed));

        let mut view = Workspace::from_analysis_input(&workspace.analysis_input().disk_baseline());
        let progress = advance(&mut view, &anchor, IndexBound::all());
        assert_eq!(progress.skipped_open, 0);
        assert!(progress.complete);
        assert_eq!(state(&view, &key, &path), Some(MemberState::Fresh));
        assert_eq!(selected(&view, &key, "P0"), vec![path.clone()]);
        assert!(selected(&view, &key, "Edited").is_empty());
        let record = view
            .eligible_indexed_member(
                &key,
                &RecordInputs {
                    path: &path,
                    facts: None,
                    artifact: None,
                    member_evidence: None,
                    origin: pascal_project::SourceOrigin::ProjectCompiled,
                    unindexed: Some(Unindexed::NotRecorded),
                },
            )
            .expect("eligible in the baseline view");
        assert_eq!(record.value().header.source.canonical, path);
        assert!(
            workspace
                .eligible_indexed_member(
                    &key,
                    &RecordInputs {
                        path: &path,
                        facts: None,
                        artifact: None,
                        member_evidence: None,
                        origin: pascal_project::SourceOrigin::ProjectCompiled,
                        unindexed: Some(Unindexed::NotRecorded),
                    },
                )
                .is_err(),
            "the session view refuses its open member"
        );
    }

    #[test]
    fn a_rejected_open_document_is_skipped_too() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let mut options = WorkspaceOptions::default();
        options.limits.max_file_bytes = 300;
        let (mut workspace, key, anchor) = indexed_project(root, 4, options);
        let open = root.join("lib/U1.pas");
        let uri = Url::from_file_path(&open).unwrap();
        let oversized = format!(
            "unit U1; {}\ninterface implementation end.\n",
            " ".repeat(400)
        );
        workspace.open_document(uri.clone(), oversized, 1).unwrap();
        assert!(workspace.open_documents[&uri].rejection.is_some());
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!(progress.skipped_open, 1);
        assert_eq!(progress.indexed, members(&workspace, &key).len() - 1);
        assert!(progress.complete);
    }

    #[test]
    fn a_file_that_vanishes_before_its_read_is_not_refused() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 3, WorkspaceOptions::default());
        let doomed = root.join("lib/U1.pas");
        let removed = doomed.clone();
        let mut calls = 0;
        let progress = workspace
            .advance_workspace_index(
                &anchor,
                IndexBound::all(),
                &mut || {
                    calls += 1;
                    // Members run in path order: App.dpr, U0, then U1.
                    if calls == 3 {
                        fs::remove_file(&removed).unwrap();
                    }
                    Ok(())
                },
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(progress.refused, 0);
        assert!(!matches!(
            state(&workspace, &key, &doomed),
            Some(MemberState::Refused(_))
        ));
        let again = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!(state(&workspace, &key, &doomed), None);
        assert!(again.complete);
    }

    #[test]
    fn stable_load_refusals_are_listed_exhaustively() {
        for stable in [
            "/w/A.pas is 9000000 bytes; the configured per-file limit is 8388608",
            "decoded contents of /w/A.pas exceed the configured per-file limit 8388608",
            "cannot read /w/A.pas: Permission denied (os error 13)",
            "cannot inspect /w/A.pas: No such file or directory (os error 2)",
            "/w/A.pas is a symlink and not an authorized workspace source",
            "/w/A.pas is not a regular file",
            "source path is outside the effective project read roots: /w/A.pas",
            "include expansion failed: resolver limit max_work exceeded: 9354837 > 1999998",
            "include expansion failed: mapped expansion work limit reached",
        ] {
            assert!(is_stable_load_refusal(stable), "{stable}");
        }
        for transient in [
            CANCELLATION_MESSAGE,
            "reconciliation budget exhausted",
            "cannot read /w/A.pas: the request budget was exceeded",
            "project context was not retained for file:///w/A.pas",
            "source cache limit: 3 files, 10 bytes retained",
            "include expansion failed: request cancelled",
            "",
        ] {
            assert!(!is_stable_load_refusal(transient), "{transient}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn mapped_library_sources_are_members() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let sdk = temp.path().join("sdk");
        unit(&sdk.join("Lib.pas"));
        write(
            &root.join(".delphi-tools.local.toml"),
            &format!(
                "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
                sdk.display()
            ),
        );
        let (mut workspace, key) = project_workspace(
            &root,
            "<DCC_UnitSearchPath>C:\\SDK</DCC_UnitSearchPath>",
            "",
            WorkspaceOptions::default(),
        );
        let found = membership(&mut workspace, &key);
        assert!(found.paths.contains(&sdk.join("Lib.pas")), "{found:?}");
        assert!(found.paths.contains(&root.join("App.dpr")));
    }

    struct CapGuard;

    impl Drop for CapGuard {
        fn drop(&mut self) {
            CAP_OVERRIDES.with(|caps| {
                caps.set(CapOverrides {
                    owner_discovery: None,
                    interactive_discovery: None,
                    unverified_symbols: None,
                    small_sources: None,
                    small_projects: None,
                    max_targets: None,
                    unverified: None,
                })
            });
        }
    }

    fn override_caps(change: impl FnOnce(&mut CapOverrides)) -> CapGuard {
        CAP_OVERRIDES.with(|caps| {
            let mut value = caps.get();
            change(&mut value);
            caps.set(value);
        });
        CapGuard
    }

    fn named_project(root: &Path, name: &str, with_unit: bool) -> PathBuf {
        let descriptor = root.join(name).join(format!("{name}.dproj"));
        write(
            &descriptor,
            &format!(
                "<Project><PropertyGroup><MainSource>{name}.dpr</MainSource></PropertyGroup></Project>"
            ),
        );
        write(
            &root.join(name).join(format!("{name}.dpr")),
            &format!("program {name}; begin end.\n"),
        );
        if with_unit {
            unit(&root.join(name).join("U.pas"));
        }
        descriptor
    }

    fn targets(
        workspace: &mut Workspace,
        requesting: Option<&Url>,
        purpose: TargetPurpose,
    ) -> Targets {
        let input = workspace.analysis_input();
        workspace
            .index_targets(&input, requesting, purpose, &AtomicBool::new(false))
            .expect("targets")
    }

    fn open(workspace: &mut Workspace, path: &Path, version: i32) -> Url {
        let uri = Url::from_file_path(path).unwrap();
        workspace
            .open_document(
                uri.clone(),
                "unit U; interface implementation end.\n".to_owned(),
                version,
            )
            .unwrap();
        uri
    }

    fn keys(targets: &Targets) -> Vec<ContextKey> {
        targets
            .contexts
            .iter()
            .map(|(key, _)| key.clone())
            .collect()
    }

    #[test]
    fn targets_follow_the_documented_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for name in ["A", "B", "C"] {
            named_project(root, name, true);
        }
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        open(&mut workspace, &root.join("B/U.pas"), 1);
        open(&mut workspace, &root.join("A/U.pas"), 2);
        let requesting = Url::from_file_path(root.join("C/U.pas")).unwrap();
        let a = context(
            &mut workspace,
            IndexAnchor::Document(Url::from_file_path(root.join("A/U.pas")).unwrap()),
        );
        let b = context(
            &mut workspace,
            IndexAnchor::Document(Url::from_file_path(root.join("B/U.pas")).unwrap()),
        );
        let c = context(&mut workspace, IndexAnchor::Document(requesting.clone()));
        let found = targets(&mut workspace, Some(&requesting), TargetPurpose::References);
        assert_eq!(keys(&found), [c, a, b]);
        assert_eq!(found.from_open_or_configured, 3);
        assert!(found.gaps.is_empty(), "{:?}", found.gaps);
    }

    #[test]
    fn the_configured_project_is_a_target_and_never_a_duplicate() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let descriptor = named_project(root, "D", true);
        pascal_project::backdate_tree_for_test(root);
        let options = WorkspaceOptions {
            project_file: Some(PathBuf::from("D/D.dproj")),
            ..WorkspaceOptions::default()
        };
        let mut workspace = workspace(root, options);
        let configured = context(&mut workspace, IndexAnchor::Project(descriptor.clone()));
        let uri = open(&mut workspace, &root.join("D/U.pas"), 1);
        let found = targets(&mut workspace, Some(&uri), TargetPurpose::References);
        assert_eq!(keys(&found), [configured]);
        assert_eq!(found.from_open_or_configured, 1);
    }

    #[test]
    fn workspace_symbols_add_project_descriptor_contexts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let first = named_project(root, "A", false);
        let second = named_project(root, "B", false);
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let a = context(&mut workspace, IndexAnchor::Project(first));
        let b = context(&mut workspace, IndexAnchor::Project(second));
        let symbols = targets(&mut workspace, None, TargetPurpose::WorkspaceSymbols);
        let found = keys(&symbols);
        assert_eq!(found.len(), 2);
        assert!(found.contains(&a) && found.contains(&b));
        assert_eq!(symbols.from_open_or_configured, 0);
        let references = targets(&mut workspace, None, TargetPurpose::References);
        assert!(references.contexts.is_empty());
    }

    #[test]
    fn outside_sources_get_their_owners_discovered() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        named_project(root, "P1", true);
        named_project(root, "P2", true);
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let uri = open(&mut workspace, &root.join("P2/U.pas"), 1);
        let p2 = context(&mut workspace, IndexAnchor::Document(uri));
        let p1 = context(
            &mut workspace,
            IndexAnchor::Document(Url::from_file_path(root.join("P1/U.pas")).unwrap()),
        );
        let found = targets(&mut workspace, None, TargetPurpose::References);
        assert_eq!(keys(&found), [p2, p1]);
        assert_eq!(found.from_open_or_configured, 1);
        assert!(found.gaps.is_empty(), "{:?}", found.gaps);
        assert!(workspace.project_cache.owner_of(&root.join("P1")).is_some());
    }

    #[test]
    fn outside_sources_beyond_the_discovery_cap_are_one_gap() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for index in 0..5 {
            unit(&root.join(format!("d{index}/U.pas")));
        }
        pascal_project::backdate_tree_for_test(root);
        let _caps = override_caps(|caps| caps.interactive_discovery = Some(2));
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let found = targets(&mut workspace, None, TargetPurpose::References);
        assert_eq!(
            found.gaps,
            ["3 sources under the workspace roots belong to contexts that were not searched"]
        );
        let remembered = (0..5)
            .filter(|index| {
                workspace
                    .project_cache
                    .owner_of(&root.join(format!("d{index}")))
                    .is_some()
            })
            .count();
        assert_eq!(remembered, 2);
    }

    #[test]
    fn targets_are_capped_at_sixteen() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for index in 0..20 {
            named_project(root, &format!("P{index:02}"), true);
        }
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let found = targets(&mut workspace, None, TargetPurpose::WorkspaceSymbols);
        assert_eq!(found.contexts.len(), MAX_TARGET_CONTEXTS);
        let distinct = keys(&found).into_iter().collect::<HashSet<_>>();
        assert_eq!(distinct.len(), MAX_TARGET_CONTEXTS);
        assert!(!found.gaps.is_empty());
    }

    #[test]
    fn a_small_workspace_is_detected_from_the_root_catalogues() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        named_project(root, "A", true);
        named_project(root, "B", true);
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        assert!(targets(&mut workspace, None, TargetPurpose::WorkspaceSymbols).small_workspace);
        {
            let _caps = override_caps(|caps| caps.small_sources = Some(1));
            assert!(
                !targets(&mut workspace, None, TargetPurpose::WorkspaceSymbols).small_workspace
            );
        }
        let _caps = override_caps(|caps| caps.small_projects = Some(3));
        assert!(!targets(&mut workspace, None, TargetPurpose::WorkspaceSymbols).small_workspace);
    }

    #[test]
    fn a_standalone_dpr_is_still_a_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("S/S.dpr"), "program S; begin end.\n");
        named_project(root, "A", false);
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let found = targets(&mut workspace, None, TargetPurpose::WorkspaceSymbols);
        assert_eq!(found.contexts.len(), 2);
        assert!(found.contexts.iter().any(|(_, anchor)| {
            matches!(anchor, IndexAnchor::Project(path) if path.ends_with("S/S.dpr"))
        }));
        assert!(found.contexts.iter().any(|(_, anchor)| {
            matches!(anchor, IndexAnchor::Project(path) if path.ends_with("A/A.dproj"))
        }));
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_attempts_are_capped_at_twice_the_target_cap() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for index in 0..6 {
            let descriptor = root.join(format!("P{index}/P{index}.dproj"));
            write(&descriptor, "<Project/>");
        }
        pascal_project::backdate_tree_for_test(root);
        for index in 0..6 {
            let descriptor = root.join(format!("P{index}/P{index}.dproj"));
            let mut permissions = fs::metadata(&descriptor).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o000);
            fs::set_permissions(&descriptor, permissions).unwrap();
        }
        let _caps = override_caps(|caps| caps.max_targets = Some(1));
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let before = descriptor_attempts();
        let _ = targets(&mut workspace, None, TargetPurpose::WorkspaceSymbols);
        assert_eq!(descriptor_attempts() - before, 2);
    }

    #[test]
    fn a_stale_owner_map_entry_is_rediscovered_not_trusted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        named_project(root, "P1", true);
        named_project(root, "P2", true);
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let uri = open(&mut workspace, &root.join("P2/U.pas"), 1);
        let p2 = context(&mut workspace, IndexAnchor::Document(uri));
        let p1 = context(
            &mut workspace,
            IndexAnchor::Document(Url::from_file_path(root.join("P1/U.pas")).unwrap()),
        );
        let mut stale = p1.clone();
        stale.config = Some("not retained".to_owned());
        workspace
            .project_cache
            .remember_owner(root.join("P1"), stale.clone());
        let found = targets(&mut workspace, None, TargetPurpose::References);
        assert_eq!(keys(&found), [p2, p1.clone()]);
        assert_eq!(workspace.project_cache.owner_of(&root.join("P1")), Some(p1));
    }

    const SHARED: &str =
        "unit Shared;\ninterface\nconst\n  SharedValue = 1;\nimplementation\nend.\n";
    const USER: &str = "unit User;\ninterface\nuses Shared;\nimplementation\nprocedure Run;\nbegin\n  WriteLn(SharedValue);\nend;\nend.\n";
    const OTHER: &str = "unit Other;\ninterface\nimplementation\nprocedure Run;\nvar\n  SharedValue: Integer;\nbegin\n  SharedValue := 1;\nend;\nend.\n";
    const PLAIN: &str = "unit Plain;\ninterface\nimplementation\nend.\n";

    /// App.dpr and lib/{Shared,User,Other,Plain}.pas, fully indexed: User
    /// imports Shared and names SharedValue, Other only names it.
    fn shared_project(
        root: &Path,
        options: WorkspaceOptions,
    ) -> (Workspace, ContextKey, IndexAnchor) {
        write(&root.join("lib/Shared.pas"), SHARED);
        write(&root.join("lib/User.pas"), USER);
        write(&root.join("lib/Other.pas"), OTHER);
        write(&root.join("lib/Plain.pas"), PLAIN);
        let (mut workspace, key) = project_workspace(
            root,
            "<DCC_UnitSearchPath>lib</DCC_UnitSearchPath>",
            "",
            options,
        );
        let anchor = IndexAnchor::Project(root.join("App.dproj"));
        advance(&mut workspace, &anchor, IndexBound::all());
        (workspace, key, anchor)
    }

    fn one_target(key: &ContextKey, anchor: &IndexAnchor) -> Targets {
        Targets {
            contexts: vec![(key.clone(), anchor.clone())],
            from_open_or_configured: 1,
            ..Targets::default()
        }
    }

    fn shared_query(units: Option<&[&str]>) -> CandidateQuery {
        CandidateQuery {
            names: vec!["SharedValue".to_owned()],
            top_level_units: units
                .map(|units| units.iter().map(|unit| (*unit).to_owned()).collect()),
            aliases: Vec::new(),
        }
    }

    fn indexed(
        workspace: &mut Workspace,
        input: &WorkspaceInput,
        targets: &Targets,
        query: Option<&CandidateQuery>,
        must_include: &[Url],
    ) -> IndexedSources {
        workspace
            .indexed_sources(
                input,
                targets,
                query,
                must_include,
                None,
                &AtomicBool::new(false),
            )
            .expect("indexed sources")
    }

    fn source_names(sources: &IndexedSources) -> Vec<String> {
        let mut names = sources
            .sources
            .iter()
            .map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn indexed_sources_hold_importers_with_the_name_and_the_declaring_unit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = shared_project(root, WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let query = shared_query(Some(&["shared"]));
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            &[],
        );
        assert_eq!(source_names(&found), ["Shared.pas", "User.pas"]);
        assert!(found.sources.iter().all(|(_, owner)| *owner == key));
        assert!(found.contexts.contains_key(&key));
        assert!(found.gaps.is_empty(), "{:?}", found.gaps);
        for excluded in ["App.dpr", "lib/Other.pas", "lib/Plain.pas"] {
            let path = root.join(excluded);
            assert!(
                found
                    .baseline
                    .iter()
                    .any(|(baseline, stamp)| *baseline == path && stamp.is_some()),
                "{excluded} is in the baseline"
            );
        }
        assert!(
            found
                .baseline
                .iter()
                .any(|(path, _)| *path == root.join("lib")),
            "the membership directories are in the baseline"
        );
        assert!(workspace.project_cache.take_index_requests().is_empty());
    }

    #[test]
    fn an_excluded_member_changed_since_indexing_becomes_a_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = shared_project(root, WorkspaceOptions::default());
        let plain = root.join("lib/Plain.pas");
        write(&plain, USER.replace("unit User", "unit Plain").as_str());
        let input = workspace.analysis_input();
        let query = shared_query(Some(&["shared"]));
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            &[],
        );
        assert_eq!(
            source_names(&found),
            ["Plain.pas", "Shared.pas", "User.pas"]
        );
        assert!(!found.baseline.iter().any(|(path, _)| *path == plain));
        assert_eq!(state(&workspace, &key, &plain), Some(MemberState::Stale));
    }

    #[test]
    fn an_unresolved_include_makes_a_member_unverified_every_time() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            &root.join("lib/Inc.pas"),
            "unit Inc;\ninterface\n{$I missing.inc}\nimplementation\nend.\n",
        );
        let (mut workspace, key, anchor) = shared_project(root, WorkspaceOptions::default());
        let member = root.join("lib/Inc.pas");
        assert_eq!(state(&workspace, &key, &member), Some(MemberState::Fresh));
        let query = shared_query(Some(&["shared"]));
        for _ in 0..2 {
            let input = workspace.analysis_input();
            let found = indexed(
                &mut workspace,
                &input,
                &one_target(&key, &anchor),
                Some(&query),
                &[],
            );
            assert_eq!(source_names(&found), ["Inc.pas", "Shared.pas", "User.pas"]);
        }
        assert_eq!(state(&workspace, &key, &member), Some(MemberState::Fresh));
    }

    #[test]
    fn unverified_members_are_bounded_and_the_rest_are_a_gap() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 5, WorkspaceOptions::default());
        let _caps = override_caps(|caps| caps.unverified = Some(3));
        let input = workspace.analysis_input();
        let query = CandidateQuery {
            names: vec!["P0".to_owned()],
            top_level_units: None,
            aliases: Vec::new(),
        };
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            &[],
        );
        assert_eq!(source_names(&found), ["App.dpr", "U0.pas", "U1.pas"]);
        assert_eq!(
            found.gaps,
            ["2 units of App.dproj are not indexed yet; 3 were searched directly"]
        );
        assert_eq!(workspace.project_cache.take_index_requests(), [anchor]);
    }

    #[test]
    fn refused_members_are_a_gap_not_a_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let big = root.join("lib/Big.pas");
        write(
            &big,
            &format!(
                "unit Big;\n{}\ninterface implementation end.\n",
                "{ SharedValue }\n".repeat(60)
            ),
        );
        let mut options = WorkspaceOptions::default();
        options.limits.max_file_bytes = 300;
        let (mut workspace, key, anchor) = shared_project(root, options);
        assert!(matches!(
            state(&workspace, &key, &big),
            Some(MemberState::Refused(_))
        ));
        let input = workspace.analysis_input();
        let query = shared_query(None);
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            &[],
        );
        assert!(!source_names(&found).contains(&"Big.pas".to_owned()));
        assert_eq!(found.gaps.len(), 1, "{:?}", found.gaps);
        assert!(
            found.gaps[0].starts_with("1 units could not be indexed: ")
                && found.gaps[0].contains("per-file limit"),
            "{}",
            found.gaps[0]
        );
    }

    #[test]
    fn an_open_buffer_is_a_candidate_even_when_its_disk_facts_exclude_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = shared_project(root, WorkspaceOptions::default());
        let plain = Url::from_file_path(root.join("lib/Plain.pas")).unwrap();
        workspace
            .open_document(plain.clone(), USER.replace("unit User", "unit Plain"), 1)
            .unwrap();
        let input = workspace.analysis_input();
        let query = shared_query(Some(&["shared"]));
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            std::slice::from_ref(&plain),
        );
        assert_eq!(
            source_names(&found),
            ["Plain.pas", "Shared.pas", "User.pas"]
        );
        let owner = found
            .sources
            .iter()
            .find(|(path, _)| path.ends_with("Plain.pas"))
            .map(|(_, owner)| owner);
        assert_eq!(owner, Some(&key));
    }

    #[test]
    fn client_deleted_files_are_never_sources() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = shared_project(root, WorkspaceOptions::default());
        let user = root.join("lib/User.pas");
        let mut input = workspace.analysis_input();
        input.deleted_overrides.insert(
            Url::from_file_path(&user).unwrap(),
            super::super::disk_stamp(&user),
        );
        let query = shared_query(Some(&["shared"]));
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            &[Url::from_file_path(&user).unwrap()],
        );
        assert_eq!(source_names(&found), ["Shared.pas"]);
    }

    #[test]
    fn phase_one_failure_falls_back_to_the_member_rule() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = shared_project(root, WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let query = shared_query(None);
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            &[],
        );
        assert_eq!(
            source_names(&found),
            ["Other.pas", "Shared.pas", "User.pas"]
        );
    }

    #[test]
    fn a_symbols_request_reads_only_unverified_members() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = shared_project(root, WorkspaceOptions::default());
        workspace
            .project_cache
            .mark_index_stale(&key, &root.join("lib/Plain.pas"));
        let input = workspace.analysis_input();
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            None,
            &[],
        );
        assert_eq!(source_names(&found), ["Plain.pas"]);
        assert_eq!(workspace.project_cache.take_index_requests(), [anchor]);
    }

    fn lib_scope(root: &Path) -> crate::catalogue::CatalogueKey {
        CatalogueScope::listing(&root.join("lib")).key().clone()
    }

    #[test]
    fn a_catalogue_replaced_after_the_membership_is_recomputed_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = shared_project(root, WorkspaceOptions::default());
        let scope = lib_scope(root);
        let fired = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = fired.clone();
        set_after_target_view_hook(Some(Box::new(move |cache| {
            if count.get() == 0
                && let Some(shared) = cache.catalogue(&scope)
            {
                cache.drop_catalogue(&scope, shared.generation);
            }
            count.set(count.get() + 1);
        })));
        let input = workspace.analysis_input();
        let query = shared_query(Some(&["shared"]));
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            &[],
        );
        set_after_target_view_hook(None);
        assert_eq!(
            fired.get(),
            2,
            "the target was read again after the recompute"
        );
        assert!(found.gaps.is_empty(), "{:?}", found.gaps);
        assert!(
            found
                .baseline
                .iter()
                .any(|(path, _)| *path == root.join("lib"))
        );
        assert_eq!(source_names(&found), ["Shared.pas", "User.pas"]);
    }

    #[test]
    fn a_catalogue_that_stays_unavailable_is_a_gap() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = shared_project(root, WorkspaceOptions::default());
        let scope = lib_scope(root);
        set_after_target_view_hook(Some(Box::new(move |cache| {
            if let Some(shared) = cache.catalogue(&scope) {
                cache.drop_catalogue(&scope, shared.generation);
            }
        })));
        let input = workspace.analysis_input();
        let query = shared_query(Some(&["shared"]));
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            &[],
        );
        set_after_target_view_hook(None);
        assert_eq!(
            found.gaps,
            [
                "the directory listings of App.dproj could not be kept; changes to them may be missed"
            ]
        );
    }

    #[test]
    fn an_incomplete_membership_is_a_gap() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("lib/Shared.pas"), SHARED);
        write(&root.join("lib/User.pas"), USER);
        let (mut workspace, key) = project_workspace(
            root,
            "<DCC_UnitSearchPath>lib</DCC_UnitSearchPath>",
            "",
            WorkspaceOptions::default(),
        );
        let scope = CatalogueScope::listing(&root.join("lib"));
        let mut stopped =
            Catalogue::new(&root.join("lib"), Depth::Listing, &mut || Ok(())).unwrap();
        stopped
            .advance(
                crate::catalogue::Bound::directories(1),
                &|_| false,
                &mut || Ok(()),
            )
            .unwrap();
        stopped.test_stop(crate::catalogue::Limit::Entries);
        workspace
            .project_cache
            .publish_catalogue(scope.key().clone(), None, std::sync::Arc::new(stopped))
            .expect("published");
        let anchor = IndexAnchor::Project(root.join("App.dproj"));
        advance(&mut workspace, &anchor, IndexBound::all());
        let input = workspace.analysis_input();
        let query = shared_query(Some(&["shared"]));
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            &[],
        );
        assert_eq!(
            found.gaps,
            ["App.dproj membership is incomplete; some units were not searched"]
        );
    }

    #[test]
    fn a_changed_member_with_an_unresolved_include_is_marked_stale() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let member = root.join("lib/Inc.pas");
        write(
            &member,
            "unit Inc;\ninterface\n{$I missing.inc}\nimplementation\nend.\n",
        );
        let (mut workspace, key, anchor) = shared_project(root, WorkspaceOptions::default());
        assert_eq!(state(&workspace, &key, &member), Some(MemberState::Fresh));
        write(
            &member,
            "unit Inc;\ninterface\n{$I missing.inc}\nimplementation\n{ edited }\nend.\n",
        );
        let input = workspace.analysis_input();
        let query = shared_query(Some(&["shared"]));
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            &[],
        );
        assert!(source_names(&found).contains(&"Inc.pas".to_owned()));
        assert_eq!(state(&workspace, &key, &member), Some(MemberState::Stale));
    }

    #[test]
    fn a_required_document_outside_every_membership_is_bound_or_reported() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let elsewhere = temp.path().join("elsewhere/Far.pas");
        write(&elsewhere, PLAIN);
        let extra = root.join("ext/Extra.pas");
        write(&extra, USER.replace("unit User", "unit Extra").as_str());
        let (mut workspace, key, anchor) = shared_project(&root, WorkspaceOptions::default());
        let input = workspace.analysis_input();
        let query = shared_query(Some(&["shared"]));
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            &[
                Url::from_file_path(&extra).unwrap(),
                Url::from_file_path(&elsewhere).unwrap(),
            ],
        );
        assert_eq!(
            source_names(&found),
            ["Extra.pas", "Shared.pas", "User.pas"]
        );
        assert!(found.sources.iter().all(|(_, owner)| *owner == key));
        assert_eq!(
            found.gaps,
            ["1 requested sources belong to no searched context and were not searched"]
        );
    }

    /// `app/` with two project candidates, `B.dproj` selected for the open
    /// `app/Main.pas`, and an open `lib/Other.pas` from the projects' search
    /// path outside their directory.
    fn selected_project(root: &Path) -> (Workspace, PathBuf, Url, Url) {
        let app = root.join("app");
        for name in ["A", "B"] {
            write(
                &app.join(format!("{name}.dproj")),
                &format!(
                    "<Project><PropertyGroup><MainSource>{name}.dpr</MainSource><DCC_Define>DEBUG</DCC_Define><DCC_UnitSearchPath>..\\lib</DCC_UnitSearchPath></PropertyGroup><ItemGroup><DCCReference Include=\"Main.pas\"/></ItemGroup></Project>"
                ),
            );
            write(
                &app.join(format!("{name}.dpr")),
                &format!("program {name}; begin end.\n"),
            );
        }
        let main_text = USER.replace("unit User", "unit Main");
        write(&app.join("Main.pas"), &main_text);
        write(&root.join("lib/Shared.pas"), SHARED);
        write(&root.join("lib/Other.pas"), OTHER);
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let main = Url::from_file_path(app.join("Main.pas")).unwrap();
        let other = Url::from_file_path(root.join("lib/Other.pas")).unwrap();
        workspace.open_document(main.clone(), main_text, 1).unwrap();
        let descriptor = app.join("B.dproj");
        workspace
            .select_project(&main, Some(&Url::from_file_path(&descriptor).unwrap()))
            .expect("select");
        workspace
            .open_document(other.clone(), OTHER.to_owned(), 2)
            .unwrap();
        (workspace, descriptor, main, other)
    }

    /// A request works on a workspace built from its input, whose document
    /// owners may hold an older resolution of their project (here: the
    /// descriptor changed after they were recorded). The project is still one
    /// target, under the key its anchor resolves to (the key the warmer
    /// indexes), and a mismatch is never an error.
    #[test]
    fn a_selected_project_is_one_target_under_its_anchors_key() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (workspace, descriptor, main, other) = selected_project(root);
        let text = fs::read_to_string(&descriptor).unwrap();
        write(&descriptor, &text.replace("DEBUG", "DEBUG;EXTRA"));
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let anchor = IndexAnchor::Project(descriptor.clone());
        let resolved = Workspace::from_analysis_input(&input)
            .index_context_for(&anchor, &cancel)
            .expect("the project resolves");
        assert_ne!(
            input.document_owners[&main].key, resolved,
            "the owner must hold an older resolution"
        );

        let mut request = Workspace::from_analysis_input(&input);
        let found = request
            .index_targets(&input, Some(&main), TargetPurpose::References, &cancel)
            .expect("targets");
        let projects = found
            .contexts
            .iter()
            .filter(|(_, target)| *target == anchor)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        assert_eq!(projects, std::slice::from_ref(&resolved));
        assert_eq!(found.requesting.as_ref(), Some(&resolved));
        assert!(
            found
                .contexts
                .iter()
                .any(|(key, _)| *key == input.document_owners[&other].key)
        );
        let query = shared_query(Some(&["shared"]));
        let sources = request
            .indexed_sources(
                &input,
                &found,
                Some(&query),
                std::slice::from_ref(&main),
                None,
                &cancel,
            )
            .expect("indexed sources");
        assert!(
            sources
                .sources
                .iter()
                .any(|(path, key)| *path == root.join("app/Main.pas") && *key == resolved)
        );
    }

    #[test]
    fn an_index_target_that_resolves_to_another_context_is_a_gap() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (workspace, descriptor, main, _) = selected_project(root);
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let anchor = IndexAnchor::Project(descriptor);
        let mut stale = input.document_owners[&main].key.clone();
        stale.config = Some("not retained".to_owned());
        let mut request = Workspace::from_analysis_input(&input);
        let sources = request
            .indexed_sources(
                &input,
                &one_target(&stale, &anchor),
                Some(&shared_query(None)),
                &[],
                None,
                &cancel,
            )
            .expect("a mismatch is not an error");
        assert!(sources.sources.iter().all(|(_, key)| *key != stale));
        assert_eq!(
            sources.gaps,
            ["B.dproj changed while the request ran; it was not searched"]
        );
    }

    #[test]
    fn without_room_for_the_index_every_member_is_read_within_the_bound() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 5, WorkspaceOptions::default());
        let open = Url::from_file_path(root.join("lib/U3.pas")).unwrap();
        workspace
            .open_document(
                open,
                "unit U3; interface implementation end.\n".to_owned(),
                1,
            )
            .unwrap();
        workspace.project_cache = crate::project_cache::ProjectCache::new(64);
        let _caps = override_caps(|caps| caps.unverified = Some(2));
        let input = workspace.analysis_input();
        let query = CandidateQuery {
            names: vec!["P0".to_owned()],
            top_level_units: None,
            aliases: Vec::new(),
        };
        let found = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            Some(&query),
            &[],
        );
        assert_eq!(source_names(&found), ["App.dpr", "U0.pas", "U3.pas"]);
        assert!(
            found.gaps.contains(
                &"2 units of App.dproj are not indexed yet; 2 were searched directly".to_owned()
            ),
            "{:?}",
            found.gaps
        );
        assert!(
            found
                .gaps
                .contains(&"the workspace index memory share is exhausted".to_owned()),
            "{:?}",
            found.gaps
        );
    }

    #[test]
    fn a_request_discovers_at_most_the_interactive_owner_cap() {
        assert_eq!(MAX_INTERACTIVE_OWNER_DISCOVERY, 32);
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let directories = MAX_INTERACTIVE_OWNER_DISCOVERY + 3;
        for index in 0..directories {
            unit(&root.join(format!("d{index:02}/U.pas")));
        }
        pascal_project::backdate_tree_for_test(root);
        let _caps = override_caps(|caps| caps.small_sources = Some(0));
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let found = targets(&mut workspace, None, TargetPurpose::References);
        let remembered = (0..directories)
            .filter(|index| {
                workspace
                    .project_cache
                    .owner_of(&root.join(format!("d{index:02}")))
                    .is_some()
            })
            .count();
        assert_eq!(remembered, MAX_INTERACTIVE_OWNER_DISCOVERY);
        // No discovered owner has an index, so every source is outside.
        assert_eq!(
            found.gaps,
            ["35 sources under the workspace roots belong to contexts that were not searched"]
        );
    }

    #[test]
    fn workspace_symbols_read_at_most_the_interactive_unverified_bound() {
        assert_eq!(MAX_UNVERIFIED_SYMBOL_CANDIDATES, 256);
        let temp = tempfile::tempdir().unwrap();
        let (mut workspace, key, anchor) =
            indexed_project(temp.path(), 5, WorkspaceOptions::default());
        let _caps = override_caps(|caps| caps.unverified_symbols = Some(2));
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let (sources, _) = workspace
            .indexed_symbols(&input, &one_target(&key, &anchor), "P", true, &[], &cancel)
            .expect("indexed symbols");
        assert_eq!(source_names(&sources), ["App.dpr", "U0.pas"]);
        assert_eq!(
            sources.gaps,
            ["3 units of App.dproj are not indexed yet; 2 were searched directly"]
        );
        let references = indexed(
            &mut workspace,
            &input,
            &one_target(&key, &anchor),
            None,
            &[],
        );
        assert_eq!(references.sources.len(), 5, "references keep their bound");
    }

    /// `P1` (its `U.pas` open) and `P2`, each with `U`, `V` and `W` naming
    /// `Target`, in a workspace too large for the walk (A16).
    fn two_projects(root: &Path) -> (Workspace, Url, IndexAnchor, IndexAnchor) {
        for name in ["P1", "P2"] {
            write(
                &root.join(name).join(format!("{name}.dproj")),
                &format!(
                    "<Project><PropertyGroup><MainSource>{name}.dpr</MainSource></PropertyGroup></Project>"
                ),
            );
            write(
                &root.join(name).join(format!("{name}.dpr")),
                &format!("program {name}; begin end.\n"),
            );
            for unit in ["U", "V", "W"] {
                named_unit(&root.join(name).join(format!("{unit}.pas")), "Target");
            }
        }
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let path = root.join("P1/U.pas");
        let uri = Url::from_file_path(&path).unwrap();
        workspace
            .open_document(uri.clone(), fs::read_to_string(&path).unwrap(), 1)
            .unwrap();
        (
            workspace,
            uri,
            IndexAnchor::Project(root.join("P1/P1.dproj")),
            IndexAnchor::Project(root.join("P2/P2.dproj")),
        )
    }

    fn target_query() -> CandidateQuery {
        CandidateQuery {
            names: vec!["Target".to_owned()],
            top_level_units: None,
            aliases: Vec::new(),
        }
    }

    fn references_of_target(workspace: &mut Workspace, requesting: &Url) -> IndexedSources {
        let input = workspace.analysis_input();
        let cancel = AtomicBool::new(false);
        let found = workspace
            .index_targets(&input, Some(requesting), TargetPurpose::References, &cancel)
            .expect("targets");
        workspace
            .indexed_sources(&input, &found, Some(&target_query()), &[], None, &cancel)
            .expect("indexed sources")
    }

    fn relative_sources(sources: &IndexedSources, root: &Path) -> Vec<String> {
        let mut found = sources
            .sources
            .iter()
            .map(|(path, _)| {
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect::<Vec<_>>();
        found.sort();
        found
    }

    #[test]
    fn an_outside_owner_without_an_index_is_a_gap_and_never_requested() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, requesting, p1, _) = two_projects(root);
        let _caps = override_caps(|caps| caps.small_sources = Some(0));
        let sources = references_of_target(&mut workspace, &requesting);
        assert_eq!(
            relative_sources(&sources, root),
            ["P1/P1.dpr", "P1/U.pas", "P1/V.pas", "P1/W.pas"]
        );
        assert!(
            sources.gaps.contains(
                &"3 sources under the workspace roots belong to contexts that were not searched"
                    .to_owned()
            ),
            "{:?}",
            sources.gaps
        );
        assert_eq!(workspace.project_cache.take_index_requests(), [p1]);
    }

    #[test]
    fn an_outside_owner_with_an_index_contributes_only_its_fresh_members() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, requesting, p1, p2) = two_projects(root);
        context(&mut workspace, p2.clone());
        // P2.dpr and P2/U.pas are indexed; V and W are not.
        advance(&mut workspace, &p2, IndexBound::units(2));
        let _ = workspace.project_cache.take_index_requests();
        let _caps = override_caps(|caps| caps.small_sources = Some(0));
        let sources = references_of_target(&mut workspace, &requesting);
        assert_eq!(
            relative_sources(&sources, root),
            ["P1/P1.dpr", "P1/U.pas", "P1/V.pas", "P1/W.pas", "P2/U.pas"]
        );
        assert!(
            sources.gaps.contains(
                &"2 units of P2.dproj are not indexed yet; 0 were searched directly".to_owned()
            ),
            "{:?}",
            sources.gaps
        );
        assert_eq!(workspace.project_cache.take_index_requests(), [p1]);
    }

    #[test]
    fn a_small_workspace_still_reads_and_requests_outside_owners() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, requesting, p1, p2) = two_projects(root);
        let sources = references_of_target(&mut workspace, &requesting);
        assert_eq!(
            relative_sources(&sources, root),
            [
                "P1/P1.dpr",
                "P1/U.pas",
                "P1/V.pas",
                "P1/W.pas",
                "P2/P2.dpr",
                "P2/U.pas",
                "P2/V.pas",
                "P2/W.pas"
            ]
        );
        let requests = workspace.project_cache.take_index_requests();
        assert!(
            requests.contains(&p1) && requests.contains(&p2),
            "{requests:?}"
        );
    }

    #[test]
    fn descriptor_targets_without_an_index_are_neither_searched_nor_requested() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let first = named_project(root, "A", true);
        let second = named_project(root, "B", true);
        pascal_project::backdate_tree_for_test(root);
        let mut workspace = workspace(root, WorkspaceOptions::default());
        let a = context(&mut workspace, IndexAnchor::Project(first.clone()));
        context(&mut workspace, IndexAnchor::Project(second));
        advance(
            &mut workspace,
            &IndexAnchor::Project(first.clone()),
            IndexBound::all(),
        );
        let _ = workspace.project_cache.take_index_requests();
        let _caps = override_caps(|caps| caps.small_sources = Some(0));
        let found = targets(&mut workspace, None, TargetPurpose::WorkspaceSymbols);
        assert_eq!(keys(&found), [a]);
        let input = workspace.analysis_input();
        let (sources, _) = workspace
            .indexed_symbols(&input, &found, "", false, &[], &AtomicBool::new(false))
            .expect("indexed symbols");
        assert!(sources.sources.is_empty());
        assert!(workspace.project_cache.take_index_requests().is_empty());
    }

    /// Multidev run 2: each references request computed the membership of
    /// every outside owner (a projectless one walks the whole root catalogue
    /// and discovers up to 256 directory owners), stored them, and so evicted
    /// the requesting context's index (16 targets, 8 retained contexts); the
    /// next request computed them all again.
    #[test]
    fn repeated_requests_compute_no_membership_again_and_keep_the_requesting_index() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, requesting, p1, _) = two_projects(root);
        for index in 0..10 {
            named_project(root, &format!("Q{index}"), true);
        }
        pascal_project::backdate_tree_for_test(root);
        let _caps = override_caps(|caps| caps.small_sources = Some(0));
        let _ = take_membership_computes();
        references_of_target(&mut workspace, &requesting);
        assert_eq!(take_membership_computes(), 1, "only P1 is searched");
        references_of_target(&mut workspace, &requesting);
        assert_eq!(take_membership_computes(), 0, "nothing is computed again");
        assert!(workspace.project_cache.holds_index_for(&p1));
        assert_eq!(workspace.project_cache.indexed_context_count(), 1);
    }

    /// Index batches that reuse persisted facts and publish what they
    /// extract (D31). Every test opens a store.
    #[cfg(unix)]
    pub(in crate::workspace) mod persisting {
        use super::*;
        use crate::persistent_cache::{Ack, CacheOptions, CacheState, PersistentCache};
        use crate::workspace::persist::acceptance::rewrite_keeping_the_stamp;
        use crate::workspace::persist::tests::{PROVIDER_MAIN, SHAPES, private_dir};
        use pascal_cache::{Digest, MaintenanceLimits, Namespace, RetireStep};
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;
        use std::sync::Arc;
        use std::time::Duration;

        fn start(workspace: &Workspace, directory: &Path, enabled: bool) -> PersistentCache {
            PersistentCache::start_unscheduled_for_test(
                CacheOptions {
                    enabled,
                    directory: Some(directory.to_path_buf()),
                    ..CacheOptions::default()
                },
                workspace.project_cache().clone(),
                Arc::default(),
                Namespace::new(Digest::from_bytes([11; 32])),
            )
        }

        /// The baseline view of `workspace` under `cache`.
        pub(in crate::workspace) fn view_with(
            workspace: &Workspace,
            cache: &PersistentCache,
        ) -> Workspace {
            let mut input = workspace.analysis_input().disk_baseline();
            input.persistent_cache = cache.clone();
            Workspace::from_analysis_input(&input)
        }

        /// A baseline view of `workspace` whose handle has an open store
        /// under `directory`.
        pub(in crate::workspace) fn persisting_view(
            workspace: &Workspace,
            directory: &Path,
        ) -> (Workspace, PersistentCache) {
            let cache = start(workspace, directory, true);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            (view_with(workspace, &cache), cache)
        }

        /// A new process: a fresh workspace and project cache (no
        /// witnesses) over the same files, and the same store. Nothing is
        /// rewritten or backdated, so edits made while closed keep their
        /// bytes and stamps.
        pub(in crate::workspace) fn restarted_view(
            root: &Path,
            directory: &Path,
        ) -> (Workspace, PersistentCache) {
            persisting_view(&workspace(root, WorkspaceOptions::default()), directory)
        }

        pub(in crate::workspace) fn records(
            workspace: &Workspace,
            key: &ContextKey,
        ) -> Vec<(PathBuf, Option<Digest>)> {
            workspace
                .project_cache
                .index_records(key)
                .unwrap_or_default()
        }

        fn recorded(workspace: &Workspace, key: &ContextKey) -> Vec<PathBuf> {
            records(workspace, key)
                .into_iter()
                .filter_map(|(path, record)| record.map(|_| path))
                .collect()
        }

        fn first_session(root: &Path, directory: &Path, count: usize) -> (ContextKey, IndexAnchor) {
            let (workspace, key, anchor) =
                indexed_project(root, count, WorkspaceOptions::default());
            let (mut first, cache) = persisting_view(&workspace, directory);
            let progress = advance(&mut first, &anchor, IndexBound::all());
            assert!(progress.complete, "{progress:?}");
            assert_eq!(progress.persisted, count, "{progress:?}");
            assert_eq!(recorded(&first, &key).len(), count);
            cache.shutdown();
            (key, anchor)
        }

        /// Records every member a batch loads.
        fn record_loads() -> Rc<RefCell<Vec<PathBuf>>> {
            let loaded = Rc::new(RefCell::new(Vec::new()));
            let sink = Rc::clone(&loaded);
            set_after_load_hook(Some(Box::new(move |path| {
                sink.borrow_mut().push(path.to_path_buf());
            })));
            loaded
        }

        #[test]
        fn index_bounds_carry_their_budgets_and_mode() {
            let reuse = IndexBound::reuse_pass();
            assert_eq!(
                (reuse.units, reuse.time, reuse.mode),
                (512, Some(Duration::from_millis(200)), IndexMode::Reuse)
            );
            let extraction = IndexBound::extraction();
            assert_eq!(
                (extraction.units, extraction.time, extraction.mode),
                (64, Some(Duration::from_secs(2)), IndexMode::Full)
            );
            let units = IndexBound::units(3).with_mode(IndexMode::Reuse);
            assert_eq!(
                (units.units, units.time, units.mode),
                (3, None, IndexMode::Reuse)
            );
        }

        #[test]
        fn a_restart_fills_the_index_from_the_store_without_parsing() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (key, anchor) = first_session(temp.path(), store.path(), 40);
            let (mut second, cache) = restarted_view(temp.path(), store.path());
            crate::navigation::test_reset_document_parse_count();
            let progress = advance(
                &mut second,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert_eq!(
                crate::navigation::test_document_parse_count(),
                0,
                "no member was parsed"
            );
            assert!(progress.complete, "{progress:?}");
            assert_eq!(progress.reused, 40);
            assert_eq!((progress.indexed, progress.persisted), (0, 0));
            assert_eq!(progress.reuse_pending, 0);
            assert_eq!(progress.key.as_ref(), Some(&key));
            assert_eq!(members(&second, &key).len(), 40);
            assert_eq!(
                selected(&second, &key, "P3"),
                vec![temp.path().join("lib/U3.pas")],
                "the index answers"
            );
            assert_eq!(recorded(&second, &key).len(), 40, "every reuse is proven");
            assert_eq!(cache.counters().unwrap().snapshot().hits, 40);
            cache.shutdown();
        }

        /// H1: an invalidation drops the record, so an unchanged member is
        /// proven again through the store instead of only refreshed.
        #[test]
        fn an_invalidated_unchanged_member_is_proven_again_without_a_parse() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), 4, WorkspaceOptions::default());
            let (mut view, cache) = persisting_view(&workspace, store.path());
            assert_eq!(advance(&mut view, &anchor, IndexBound::all()).persisted, 4);
            let member = temp.path().join("lib/U1.pas");
            view.project_cache.mark_index_stale(&key, &member);
            assert!(!view.project_cache.index_member_has_record(&key, &member));
            crate::navigation::test_reset_document_parse_count();
            let progress = advance(&mut view, &anchor, IndexBound::all());
            assert_eq!(crate::navigation::test_document_parse_count(), 0);
            assert_eq!(
                (progress.refreshed, progress.reused),
                (0, 1),
                "{progress:?}"
            );
            assert!(view.project_cache.index_member_has_record(&key, &member));
            assert_eq!(recorded(&view, &key).len(), 4);
            cache.shutdown();
        }

        /// A member refused for the per-file limit is listed by an
        /// artifact-less record, not counted as persisted.
        #[test]
        fn a_refused_member_gets_an_artifact_less_record() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let mut options = WorkspaceOptions::default();
            options.limits.max_file_bytes = 300;
            let (workspace, key, anchor) = indexed_project(temp.path(), 3, options);
            let big = temp.path().join("lib/U1.pas");
            named_unit(&big, &"Long".repeat(60));
            let (mut view, cache) = persisting_view(&workspace, store.path());
            let progress = advance(&mut view, &anchor, IndexBound::all());
            assert_eq!((progress.refused, progress.indexed), (1, 2), "{progress:?}");
            assert_eq!(progress.persisted, 2);
            assert!(matches!(
                state(&view, &key, &big),
                Some(MemberState::Refused(_))
            ));
            assert!(view.project_cache.index_member_has_record(&key, &big));
            assert_eq!(recorded(&view, &key).len(), 3);
            cache.shutdown();
        }

        #[test]
        fn a_cancelled_first_session_resumes_from_published_members() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), 40, WorkspaceOptions::default());
            let (mut first, cache) = persisting_view(&workspace, store.path());
            // Two checks per member in a Full batch: before its reuse and
            // before its extraction.
            let mut calls = 0;
            let result = first.advance_workspace_index(
                &anchor,
                IndexBound::all(),
                &mut || {
                    calls += 1;
                    if calls > 30 {
                        Err(CANCELLATION_MESSAGE.to_owned())
                    } else {
                        Ok(())
                    }
                },
                &AtomicBool::new(false),
            );
            assert_eq!(result.unwrap_err(), CANCELLATION_MESSAGE);
            let published = recorded(&first, &key);
            assert_eq!(published.len(), 15);
            cache.shutdown();
            let (mut second, cache) = restarted_view(temp.path(), store.path());
            let loaded = record_loads();
            crate::navigation::test_reset_document_parse_count();
            let progress = advance(&mut second, &anchor, IndexBound::all());
            set_after_load_hook(None);
            assert!(progress.complete, "{progress:?}");
            assert_eq!((progress.reused, progress.indexed), (15, 25));
            assert_eq!(progress.persisted, 25);
            let mut unpublished = members(&second, &key);
            unpublished.retain(|path| !published.contains(path));
            assert_eq!(
                *loaded.borrow(),
                unpublished,
                "no published member is loaded"
            );
            assert_eq!(crate::navigation::test_document_parse_count(), 25);
            cache.shutdown();
        }

        #[test]
        fn an_edit_while_closed_is_extracted_and_its_neighbours_reused() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (key, anchor) = first_session(temp.path(), store.path(), 40);
            let edited = temp.path().join("lib/U2.pas");
            named_unit(&edited, "Renamed2");
            let (mut second, cache) = restarted_view(temp.path(), store.path());
            let loaded = record_loads();
            let progress = advance(&mut second, &anchor, IndexBound::all());
            set_after_load_hook(None);
            assert!(progress.complete, "{progress:?}");
            assert_eq!((progress.reused, progress.indexed), (39, 1));
            assert_eq!(*loaded.borrow(), vec![edited.clone()]);
            assert_eq!(selected(&second, &key, "Renamed2"), vec![edited]);
            assert!(selected(&second, &key, "P2").is_empty());
            cache.shutdown();
        }

        #[test]
        fn a_same_size_same_mtime_rewrite_while_closed_is_not_reused() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (key, anchor) = first_session(temp.path(), store.path(), 10);
            let edited = temp.path().join("lib/U2.pas");
            let rewritten = fs::read_to_string(&edited).unwrap().replace("P2", "Q2");
            rewrite_keeping_the_stamp(&edited, rewritten.as_bytes());
            let (mut second, cache) = restarted_view(temp.path(), store.path());
            let loaded = record_loads();
            let progress = advance(&mut second, &anchor, IndexBound::all());
            set_after_load_hook(None);
            assert!(progress.complete, "{progress:?}");
            assert_eq!((progress.reused, progress.indexed), (9, 1));
            assert_eq!(*loaded.borrow(), vec![edited.clone()]);
            assert_eq!(selected(&second, &key, "Q2"), vec![edited]);
            assert!(selected(&second, &key, "P2").is_empty());
            cache.shutdown();
        }

        #[test]
        fn units_shared_by_two_project_contexts_are_extracted_once() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            for name in ["A", "B"] {
                write(
                    &root.join(format!("{name}.dproj")),
                    &format!(
                        "<Project><PropertyGroup><MainSource>{name}.dpr</MainSource><DCC_UnitSearchPath>lib</DCC_UnitSearchPath></PropertyGroup></Project>"
                    ),
                );
                write(
                    &root.join(format!("{name}.dpr")),
                    &format!("program {name}; begin end.\n"),
                );
            }
            for index in 0..5 {
                named_unit(
                    &root.join(format!("lib/U{index}.pas")),
                    &format!("P{index}"),
                );
            }
            pascal_project::backdate_tree_for_test(root);
            let (mut first, cache) =
                persisting_view(&workspace(root, WorkspaceOptions::default()), store.path());
            let a = IndexAnchor::Project(root.join("A.dproj"));
            let progress = advance(&mut first, &a, IndexBound::all());
            assert!(progress.complete && progress.persisted == 6, "{progress:?}");
            cache.shutdown();
            let (mut second, cache) = restarted_view(root, store.path());
            let b = IndexAnchor::Project(root.join("B.dproj"));
            let loaded = record_loads();
            let progress = advance(&mut second, &b, IndexBound::all());
            set_after_load_hook(None);
            assert!(progress.complete, "{progress:?}");
            assert_eq!((progress.reused, progress.indexed), (5, 1));
            assert_eq!(*loaded.borrow(), vec![root.join("B.dpr")]);
            cache.shutdown();
        }

        const EDITED_SHAPES: &str = "unit Shapes;\ninterface\ntype TPair = record A, B, C: Integer; end;\nimplementation\nend.\n";

        fn provider_project(root: &Path) -> (Workspace, ContextKey, IndexAnchor) {
            write(
                &root.join("lib/Main.pas"),
                std::str::from_utf8(PROVIDER_MAIN).unwrap(),
            );
            write(
                &root.join("lib/Shapes.pas"),
                std::str::from_utf8(SHAPES).unwrap(),
            );
            let (workspace, key) = project_workspace(
                root,
                "<DCC_UnitSearchPath>lib</DCC_UnitSearchPath>",
                "",
                WorkspaceOptions::default(),
            );
            (workspace, key, IndexAnchor::Project(root.join("App.dproj")))
        }

        #[test]
        fn an_overlay_provider_never_reaches_a_persisted_member() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let main = root.join("lib/Main.pas");
            let (mut session, key, anchor) = provider_project(root);
            session
                .open_document(
                    Url::from_file_path(root.join("lib/Shapes.pas")).unwrap(),
                    EDITED_SHAPES.to_owned(),
                    1,
                )
                .unwrap();
            let (mut view, cache) = persisting_view(&session, store.path());
            let progress = advance(&mut view, &anchor, IndexBound::all());
            assert!(progress.complete, "{progress:?}");
            assert_eq!(progress.persisted, 3, "{progress:?}");
            assert_eq!(selected(&view, &key, "Wide"), vec![main.clone()]);
            cache.shutdown();

            let (mut restarted, cache) = restarted_view(root, store.path());
            crate::navigation::test_reset_document_parse_count();
            let progress = advance(
                &mut restarted,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert_eq!(crate::navigation::test_document_parse_count(), 0);
            assert!(progress.complete && progress.reused == 3, "{progress:?}");
            let reused = selected(&restarted, &key, "Wide");
            cache.shutdown();

            let other = tempfile::tempdir().unwrap();
            let (mut fresh, fresh_key, fresh_anchor) = provider_project(other.path());
            assert!(advance(&mut fresh, &fresh_anchor, IndexBound::all()).complete);
            let disk_only = selected(&fresh, &fresh_key, "Wide");
            assert_eq!(disk_only, vec![other.path().join("lib/Main.pas")]);
            assert_eq!(
                reused,
                vec![main],
                "the reused facts are the disk-only ones"
            );
        }

        #[test]
        fn an_overlaid_project_file_context_publishes_nothing() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (mut session, key, anchor) =
                indexed_project(temp.path(), 4, WorkspaceOptions::default());
            // The main source is project metadata (its `uses ... in`
            // clauses name explicit units); `.dproj` buffers are not held.
            let main = temp.path().join("App.dpr");
            session
                .open_document(
                    Url::from_file_path(&main).unwrap(),
                    "program App; { unsaved } begin end.\n".to_owned(),
                    1,
                )
                .unwrap();
            assert!(!session.context_inputs_are_disk_only(&key));
            let (mut view, cache) = persisting_view(&session, store.path());
            assert!(!view.context_inputs_are_disk_only(&key));
            let progress = advance(&mut view, &anchor, IndexBound::all());
            assert!(progress.complete, "{progress:?}");
            assert_eq!((progress.indexed, progress.persisted), (4, 0));
            assert!(recorded(&view, &key).is_empty());
            cache.shutdown();
            let (mut restarted, cache) = restarted_view(temp.path(), store.path());
            let progress = advance(
                &mut restarted,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert!(restarted.context_inputs_are_disk_only(&key));
            assert_eq!(progress.reused, 0, "nothing was in the store");
            assert_eq!(progress.reuse_pending, 0, "every member was tried");
            cache.shutdown();
        }

        /// Ruling (fix round 1): an open project file whose buffer equals
        /// its saved bytes is no session-private input; an edited one is.
        #[test]
        fn an_open_unedited_main_source_does_not_block_publication() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (mut session, key, anchor) =
                indexed_project(temp.path(), 4, WorkspaceOptions::default());
            let main = temp.path().join("App.dpr");
            let saved = fs::read_to_string(&main).unwrap();
            session
                .open_document(Url::from_file_path(&main).unwrap(), saved, 1)
                .unwrap();
            assert!(session.context_inputs_are_disk_only(&key));
            let (mut view, cache) = persisting_view(&session, store.path());
            let progress = advance(&mut view, &anchor, IndexBound::all());
            assert!(view.context_inputs_are_disk_only(&key));
            assert_eq!((progress.indexed, progress.persisted), (4, 4));
            assert_eq!(recorded(&view, &key).len(), 4);
            cache.shutdown();
        }

        /// Review finding 2: project discovery reads saved files only, so a
        /// context the session discovers while a project file is edited
        /// is a disk context, and a baseline view adopting it from the
        /// shared discoveries publishes nothing of the buffer.
        #[test]
        fn a_baseline_view_adopting_a_session_discovery_sees_no_edited_project_file() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            named_unit(&root.join("ext/Extra.pas"), "ExtraProc");
            let (session, key, _) = indexed_project(root, 3, WorkspaceOptions::default());
            let member = Url::from_file_path(root.join("lib/U0.pas")).unwrap();
            let anchor = IndexAnchor::Document(member.clone());
            // The view exists before the edit, so its session-private set
            // does not name the edited main source.
            let (mut view, cache) = persisting_view(&session, store.path());
            let mut session = session;
            session
                .open_document(
                    Url::from_file_path(root.join("App.dpr")).unwrap(),
                    "program App;\nuses Extra in 'ext/Extra.pas';\nbegin end.\n".to_owned(),
                    1,
                )
                .unwrap();
            let discovered = context(&mut session, anchor.clone());
            assert_eq!(discovered, key);
            let units = |workspace: &Workspace| {
                workspace.contexts[&key]
                    .context
                    .explicit_units()
                    .into_values()
                    .flatten()
                    .collect::<Vec<_>>()
            };
            assert!(units(&session).is_empty(), "discovery ignored the buffer");
            let progress = advance(&mut view, &anchor, IndexBound::all());
            assert_eq!(progress.key.as_ref(), Some(&key));
            assert!(units(&view).is_empty());
            assert!(!members(&view, &key).contains(&root.join("ext/Extra.pas")));
            assert!(selected(&view, &key, "ExtraProc").is_empty());
            assert_eq!(progress.persisted, 3, "{progress:?}");
            cache.shutdown();
        }

        /// The record kind comes from the root's size, not the message, as
        /// `eligible_member` decides it.
        #[test]
        fn a_refusal_names_over_size_only_for_a_root_over_the_limit() {
            use crate::workspace::eligible::member_record;
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), 3, WorkspaceOptions::default());
            let (mut view, cache) = persisting_view(&workspace, store.path());
            advance(&mut view, &anchor, IndexBound::all());
            let small = temp.path().join("lib/U0.pas");
            view.refuse_member(
                &key,
                &small,
                format!(
                    "include owner {small:?} directive summary exceeds the configured per-file limit"
                ),
                &AtomicBool::new(false),
            );
            let expected = member_record(&RecordInputs {
                path: &small,
                facts: None,
                artifact: None,
                member_evidence: None,
                origin: view.contexts[&key].context.source_origin(&small),
                unindexed: Some(Unindexed::NotRecorded),
            })
            .id()
            .unwrap();
            let recorded = records(&view, &key)
                .into_iter()
                .find(|(path, _)| *path == small)
                .and_then(|(_, record)| record);
            assert_eq!(recorded, Some(expected));
            cache.shutdown();
        }

        /// Review finding 1: a cancellation between the artifact and the
        /// record stops the publication.
        #[test]
        fn a_cancellation_after_the_artifact_publishes_no_record() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), 2, WorkspaceOptions::default());
            let member = temp.path().join("lib/U0.pas");
            let (mut view, cache) = persisting_view(&workspace, store.path());
            let cancel = Arc::new(AtomicBool::new(false));
            let (handle, flag, target) = (cache.clone(), Arc::clone(&cancel), member.clone());
            set_before_persist_hook(Some(Box::new(move |path| {
                if path == target {
                    let flag = Arc::clone(&flag);
                    let job = handle.job().expect("job");
                    job.store()
                        .set_before_commit_for_test(move || flag.store(true, Ordering::Relaxed));
                }
            })));
            let result =
                view.advance_workspace_index(&anchor, IndexBound::all(), &mut || Ok(()), &cancel);
            set_before_persist_hook(None);
            cache
                .job()
                .expect("job")
                .store()
                .set_before_commit_for_test(|| {});
            assert!(cancel.load(Ordering::Relaxed), "the artifact committed");
            // U0 is the last member, so the batch ends without another check.
            let progress = result.expect("batch");
            assert_eq!((progress.indexed, progress.persisted), (2, 1));
            assert_eq!(state(&view, &key, &member), Some(MemberState::Fresh));
            assert_eq!(recorded(&view, &key), vec![temp.path().join("App.dpr")]);
            cache.shutdown();
            // The artifact alone was published: a restart reuses it and
            // proves the record then.
            let (mut second, cache) = restarted_view(temp.path(), store.path());
            let progress = advance(
                &mut second,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert_eq!(progress.reused, 2, "{progress:?}");
            cache.shutdown();
        }

        #[test]
        fn a_disable_mid_batch_continues_memory_only_without_store_calls() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), 10, WorkspaceOptions::default());
            let (mut view, cache) = persisting_view(&workspace, store.path());
            let jobs_at_ack = Cell::new(None);
            let mut calls = 0;
            let progress = view
                .advance_workspace_index(
                    &anchor,
                    IndexBound::all(),
                    &mut || {
                        calls += 1;
                        // The 5th member's first check.
                        if calls == 9 {
                            let ack = cache.reconfigure(CacheOptions {
                                enabled: false,
                                directory: Some(store.path().to_path_buf()),
                                ..CacheOptions::default()
                            });
                            assert_eq!(ack, Ack::Done);
                            assert_eq!(cache.state(), CacheState::Disabled);
                            jobs_at_ack.set(Some(cache.jobs_issued_for_test()));
                        }
                        Ok(())
                    },
                    &AtomicBool::new(false),
                )
                .expect("batch");
            let jobs = jobs_at_ack.get().expect("the cache was disabled mid-batch");
            assert!(jobs > 0, "the first members used the store");
            assert_eq!(
                cache.jobs_issued_for_test(),
                jobs,
                "no store call after the ack"
            );
            assert!(progress.complete, "{progress:?}");
            assert_eq!((progress.indexed, progress.persisted), (10, 4));
            for path in members(&view, &key) {
                assert_eq!(state(&view, &key, &path), Some(MemberState::Fresh));
            }
            assert_eq!(recorded(&view, &key).len(), 4);
            cache.shutdown();
        }

        #[test]
        fn persistence_off_indexes_exactly_as_before() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), 10, WorkspaceOptions::default());
            let cache = start(&workspace, store.path(), false);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Disabled);
            let mut view = view_with(&workspace, &cache);
            let progress = advance(&mut view, &anchor, IndexBound::all());
            assert_eq!(
                (
                    progress.indexed,
                    progress.refreshed,
                    progress.refused,
                    progress.pending
                ),
                (10, 0, 0, 0)
            );
            assert!(progress.complete && !progress.memory);
            assert_eq!(
                (progress.reused, progress.persisted, progress.reuse_deferred),
                (0, 0, 0)
            );
            assert!(recorded(&view, &key).is_empty());
            assert_eq!(
                fs::read_dir(store.path()).unwrap().count(),
                0,
                "no store file"
            );
            assert_eq!(cache.jobs_issued_for_test(), 0);
            cache.shutdown();
        }

        #[test]
        fn a_refused_publication_reservation_keeps_the_member_memory_only() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), 2, WorkspaceOptions::default());
            let member = temp.path().join("lib/U0.pas");
            let (mut view, cache) = persisting_view(&workspace, store.path());
            let project_cache = workspace.project_cache().clone();
            let target = member.clone();
            let shrunk = Rc::new(Cell::new(false));
            let flag = Rc::clone(&shrunk);
            set_before_persist_hook(Some(Box::new(move |path| {
                if path == target {
                    let stats = project_cache.stats();
                    // No room is left for anything new.
                    project_cache.set_max_bytes(stats.outstanding_bytes + stats.reserved_bytes());
                    flag.set(true);
                }
            })));
            let progress = advance(&mut view, &anchor, IndexBound::all());
            set_before_persist_hook(None);
            assert!(shrunk.get());
            workspace
                .project_cache()
                .set_max_bytes(crate::project_cache::DEFAULT_MAX_CACHE_BYTES);
            assert!(progress.complete, "{progress:?}");
            assert_eq!((progress.indexed, progress.persisted), (2, 1));
            assert_eq!(state(&view, &key, &member), Some(MemberState::Fresh));
            assert_eq!(recorded(&view, &key), vec![temp.path().join("App.dpr")]);
            cache.shutdown();
            let (mut second, cache) = restarted_view(temp.path(), store.path());
            let loaded = record_loads();
            let progress = advance(&mut second, &anchor, IndexBound::all());
            set_after_load_hook(None);
            assert_eq!((progress.reused, progress.indexed), (1, 1));
            assert_eq!(*loaded.borrow(), vec![member]);
            cache.shutdown();
        }

        #[test]
        fn reuse_holds_one_members_facts_at_a_time() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (_, anchor) = first_session(temp.path(), store.path(), 12);
            let (mut second, cache) = restarted_view(temp.path(), store.path());
            let project_cache = second.project_cache().clone();
            let samples = Rc::new(RefCell::new(Vec::new()));
            let sink = Rc::clone(&samples);
            set_after_reuse_hook(Some(Box::new(move |_| {
                sink.borrow_mut()
                    .push(project_cache.stats().reserved_facts_bytes);
            })));
            let progress = advance(
                &mut second,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            set_after_reuse_hook(None);
            assert_eq!(progress.reused, 12);
            assert_eq!(
                *samples.borrow(),
                vec![0; 12],
                "each member's facts were dropped"
            );
            let snapshot = cache.counters().unwrap().snapshot();
            assert_eq!(snapshot.hits, 12);
            assert!(
                snapshot.decoded_bytes > 0,
                "the facts were decoded and charged"
            );
            cache.shutdown();
        }

        /// Maintenance in another process: retire every unreferenced row
        /// (no snapshot lists the records) and, with `reclaim`, delete them.
        fn retire_everything(root: &Path, directory: &Path, reclaim: bool) -> usize {
            let (_, cache) = restarted_view(root, directory);
            let job = cache.job().expect("job");
            let limits = MaintenanceLimits {
                recency_window: 0,
                ..MaintenanceLimits::DEFAULT
            };
            let mut retired = 0;
            for step in [
                RetireStep::Records,
                RetireStep::Evidence,
                RetireStep::Artifacts,
            ] {
                loop {
                    let rows = job.store().retire_step(step, &limits).unwrap();
                    if rows == 0 {
                        break;
                    }
                    retired += rows;
                }
            }
            while reclaim && !job.store().reclaim(256).unwrap().complete {}
            drop(job);
            cache.shutdown();
            retired
        }

        #[test]
        fn a_retired_artifact_is_extracted_again() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (key, anchor) = first_session(temp.path(), store.path(), 6);
            assert_eq!(retire_everything(temp.path(), store.path(), false), 12);
            // Retired but not deleted yet: never reused, and a publication
            // is refused (`Retiring`), so the member stays memory-only.
            let (mut second, cache) = restarted_view(temp.path(), store.path());
            let progress = advance(&mut second, &anchor, IndexBound::all());
            assert!(progress.complete, "{progress:?}");
            assert_eq!((progress.reused, progress.indexed), (0, 6));
            assert_eq!(progress.persisted, 0);
            cache.shutdown();
            assert_eq!(retire_everything(temp.path(), store.path(), true), 0);
            // Deleted: extracted and published again.
            let (mut third, cache) = restarted_view(temp.path(), store.path());
            let progress = advance(&mut third, &anchor, IndexBound::all());
            assert_eq!((progress.reused, progress.indexed), (0, 6));
            assert_eq!(progress.persisted, 6, "republished");
            cache.shutdown();
            let (mut fourth, cache) = restarted_view(temp.path(), store.path());
            let progress = advance(
                &mut fourth,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert!(progress.complete && progress.reused == 6, "{progress:?}");
            assert_eq!(recorded(&fourth, &key).len(), 6);
            cache.shutdown();
        }

        #[test]
        fn two_sessions_index_one_store_concurrently() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let (first_workspace, key, anchor) =
                indexed_project(root, 12, WorkspaceOptions::default());
            let (mut first, cache) = persisting_view(&first_workspace, store.path());
            let second_workspace = workspace(root, WorkspaceOptions::default());
            let mut second = view_with(&second_workspace, &cache);
            let mut done = (false, false);
            done.0 |= advance(&mut first, &anchor, IndexBound::units(1)).complete;
            assert!(
                second.project_cache().read_index(&key, |_, _| ()).is_none(),
                "two project caches, as two processes have"
            );
            for _ in 0..64 {
                done.0 |= advance(&mut first, &anchor, IndexBound::units(1)).complete;
                done.1 |= advance(&mut second, &anchor, IndexBound::units(1)).complete;
                if done == (true, true) {
                    break;
                }
            }
            assert_eq!(done, (true, true));
            let (left, right) = (records(&first, &key), records(&second, &key));
            assert_eq!(left.len(), 12);
            assert!(left.iter().all(|(_, record)| record.is_some()), "{left:?}");
            assert_eq!(left, right, "identical records for every member");
            cache.shutdown();
            let (mut third, cache) = restarted_view(root, store.path());
            crate::navigation::test_reset_document_parse_count();
            let progress = advance(
                &mut third,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert_eq!(crate::navigation::test_document_parse_count(), 0);
            assert!(progress.complete && progress.reused == 12, "{progress:?}");
            cache.shutdown();
        }
    }
}
