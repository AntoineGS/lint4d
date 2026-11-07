//! Which documents a workspace index covers (l4d-33 U1, A9, A10): the
//! compilable units of one context, read from the shared directory
//! catalogues (l4d-20) so that a second context, or a second request, lists
//! no directory the first already listed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
use crate::project_cache::{IndexAnchor, IndexMembership, PublishOutcome};
pub(crate) use crate::workspace_index::MAX_INTERACTIVE_UNVERIFIED_CANDIDATES;
use crate::workspace_index::{
    CandidateQuery, Candidates, ContextIndex, IndexCoverage, IndexedSymbol, MemberFacts,
    MemberStamp, MemberState, RETRYING_REFUSAL, SymbolKey,
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
    MAX_INTERACTIVE_UNVERIFIED_CANDIDATES
}

fn max_unverified_symbol_candidates() -> usize {
    #[cfg(test)]
    if let Some(cap) = CAP_OVERRIDES.with(|caps| caps.get().unverified_symbols) {
        return cap;
    }
    MAX_INTERACTIVE_UNVERIFIED_CANDIDATES
}

/// Pending members one extraction batch takes.
pub(crate) const EXTRACTION_BATCH_UNITS: usize = 64;
/// How long a batch waits for the maintenance it woke before proving
/// members refused for room (the worker's pressure interval, twice).
const ROOM_WAIT: Duration = Duration::from_secs(2);

/// How long an index batch lets one member's extraction run (13b-5, A4):
/// five extraction batch bounds. A member's compile-time session is the
/// cost it bounds, and that session runs in milliseconds to about a second
/// for ordinary units once observations are deduplicated (13b-2), so only a
/// pathological unit reaches it. Such a unit costs at most this once per
/// start: it is limited, listed by an artifact-less record and retried last
/// by the next start (R8-3).
pub(crate) const MEMBER_DEADLINE: Duration = Duration::from_secs(10);

/// Why a compile-time session stopped at `Workspace::member_deadline`. It is
/// no stable load refusal (`is_stable_load_refusal`): the member is limited,
/// never stored as refused.
pub(crate) const MEMBER_DEADLINE_MESSAGE: &str = "the member's extraction passed its deadline";

/// The largest member file a parallel extraction worker extracts (13d-2).
/// A larger one is extracted by the batch's own thread, in member order, as
/// the serial loop does. An extraction's transient memory (parse, session,
/// facts) is about a hundred times its source and is charged nowhere; with
/// workers, every worker could hold a large member's at once, and each
/// worker thread's allocator arena keeps its high-water mark resident
/// afterwards. Kept on the batch's thread, large members cost what they cost
/// without a store, and workers add at most `threads` times about 25 MiB.
/// About 1 % of multidev's units are larger.
pub(crate) const WORKER_MEMBER_BYTES: u64 = 256 * 1024;

#[cfg(test)]
thread_local! {
    /// The member whose extraction deadline has passed when it starts, as
    /// for a member slower than `MEMBER_DEADLINE`.
    static SLOW_MEMBER: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Clears `slow_member_for_test` when dropped.
#[cfg(test)]
pub(crate) struct SlowMember;

#[cfg(test)]
impl Drop for SlowMember {
    fn drop(&mut self) {
        SLOW_MEMBER.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Makes `path` a member whose extraction outlasts its deadline on this
/// thread, until the guard drops.
#[cfg(test)]
pub(crate) fn slow_member_for_test(path: &Path) -> SlowMember {
    SLOW_MEMBER.with(|slot| *slot.borrow_mut() = Some(path.to_path_buf()));
    SlowMember
}

/// When the extraction of `path`, starting now, must be over.
fn member_deadline(path: &Path) -> Instant {
    #[cfg(test)]
    if SLOW_MEMBER.with(|slot| slot.borrow().as_deref() == Some(path)) {
        return Instant::now();
    }
    #[cfg(not(test))]
    let _ = path;
    Instant::now() + MEMBER_DEADLINE
}

/// Runs where a member's extraction starts, on whichever thread extracts
/// it, with the flag its load polls.
#[cfg(test)]
type ExtractHook = Arc<dyn Fn(&Path, &AtomicBool) + Send + Sync>;

#[cfg(test)]
thread_local! {
    static EXTRACT_HOOK: std::cell::RefCell<Option<ExtractHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_worker_extract_hook(hook: Option<ExtractHook>) {
    EXTRACT_HOOK.with(|slot| *slot.borrow_mut() = hook);
}

/// The batch thread's test seams, which its extraction workers run under
/// (13c-3): thread-locals do not reach a worker by themselves.
#[cfg(test)]
#[derive(Clone)]
struct WorkerSeams {
    slow: Option<PathBuf>,
    errors: Vec<PathBuf>,
    unstampable: Vec<PathBuf>,
    hook: Option<ExtractHook>,
}

#[cfg(test)]
impl WorkerSeams {
    fn capture() -> Self {
        Self {
            slow: SLOW_MEMBER.with(|slot| slot.borrow().clone()),
            errors: MEMBER_ERROR.with(|slot| slot.borrow().clone()),
            unstampable: UNSTAMPABLE.with(|slot| slot.borrow().clone()),
            hook: EXTRACT_HOOK.with(|slot| slot.borrow().clone()),
        }
    }

    /// On a worker thread, which has none of its own.
    fn install(self) {
        SLOW_MEMBER.with(|slot| *slot.borrow_mut() = self.slow);
        MEMBER_ERROR.with(|slot| *slot.borrow_mut() = self.errors);
        UNSTAMPABLE.with(|slot| *slot.borrow_mut() = self.unstampable);
        EXTRACT_HOOK.with(|slot| *slot.borrow_mut() = self.hook);
    }
}

/// Report-only heap measurements (13d-2): what each view dropped after a
/// batch freed, with how many members it took.
#[cfg(test)]
static VIEW_FREED: std::sync::Mutex<Vec<(&'static str, usize, usize)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn note_view_freed(kind: &'static str, freed: usize, members: usize) {
    VIEW_FREED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((kind, freed, members));
}

#[cfg(test)]
fn take_view_freed() -> Vec<(&'static str, usize, usize)> {
    std::mem::take(
        &mut *VIEW_FREED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
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
    #[cfg(test)]
    pub(crate) fn units(units: usize) -> Self {
        Self {
            units,
            time: None,
            mode: IndexMode::Full,
        }
    }

    /// The reuse pass: 512 members or 200 ms, never a parse.
    pub(crate) fn reuse_pass() -> Self {
        Self {
            units: 512,
            time: Some(Duration::from_millis(200)),
            mode: IndexMode::Reuse,
        }
    }

    /// An extraction batch: 64 members or 2 s.
    pub(crate) fn extraction() -> Self {
        Self {
            units: EXTRACTION_BATCH_UNITS,
            time: Some(Duration::from_secs(2)),
            mode: IndexMode::Full,
        }
    }

    #[cfg(test)]
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
    /// Of `reuse_missed`, members the store held nothing for.
    pub(crate) reuse_absent: usize,
    /// Members the held snapshot lists artifact-less (a previous refusal),
    /// now tried for reuse: they are retried last by extraction.
    pub(crate) reuse_retrying: usize,
    /// Pending closed members not tried for reuse since they last changed.
    pub(crate) reuse_pending: usize,
    /// Retryable reuse misses this batch (left untried for reuse).
    pub(crate) reuse_deferred: usize,
    /// Members whose artifact and record this batch published.
    pub(crate) persisted: usize,
    /// Closed members still pending only to retry a previous refusal (a
    /// held snapshot lists them without an artifact); they do not hold
    /// `complete` back.
    pub(crate) retrying: usize,
    /// Members retrying a previous refusal whose retry this batch ended
    /// (indexed, refused, limited or deferred for another reason).
    pub(crate) retries_attempted: usize,
    /// Members whose publication was refused for room (WAL threshold,
    /// disk budget, free space), to prove once the store has room; they
    /// hold `complete` back.
    pub(crate) awaiting_room: usize,
    /// Members over `WORKER_MEMBER_BYTES` that the parallel workers left to
    /// the batch's thread to extract (13d-2), and their source bytes.
    pub(crate) left_large: usize,
    pub(crate) left_large_bytes: u64,
}

impl IndexProgress {
    pub(crate) fn new(key: ContextKey, mode: IndexMode) -> Self {
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
            reuse_absent: 0,
            reuse_retrying: 0,
            reuse_pending: 0,
            reuse_deferred: 0,
            persisted: 0,
            retrying: 0,
            retries_attempted: 0,
            awaiting_room: 0,
            left_large: 0,
            left_large_bytes: 0,
        }
    }

    /// Another batch can gain something: this one settled a member, or
    /// members no batch has tried yet are still pending. A batch that only
    /// deferred members already deferred is not requeued (no livelock); a
    /// request or invalidation asks for the anchor again. A reuse batch is
    /// requeued while members are left to try and it tried one for good (a
    /// miss, or a member the snapshot lists as refused), so a run of
    /// retryable misses alone does not loop either (the warmer retries such
    /// a batch a bounded number of times). A Full batch
    /// that left members retrying a previous refusal (they go last, so its
    /// bound may end before them) is requeued for them although the index
    /// is `complete`, if it settled or retried anything: trying one clears
    /// its retry, so this ends.
    pub(crate) fn worth_requeue(&self) -> bool {
        let settled = self.indexed + self.refreshed + self.refused + self.limited + self.reused > 0;
        if self.memory {
            return false;
        }
        if self.complete {
            return self.mode == IndexMode::Full
                && self.retrying > 0
                && (settled || self.retries_attempted > 0);
        }
        settled
            || (self.deferred > 0 && self.untried > 0)
            || (self.mode == IndexMode::Reuse
                && self.reuse_pending > 0
                && self.reuse_missed + self.reuse_retrying > 0)
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

/// The compile-time admission bounds fixed for a session, whatever else
/// the process holds: one that refuses a member's session refuses it again
/// with the same inputs (the expression bounds are constants of
/// pascal-core's operand parser). `max_retained_bytes` is not one (the
/// resolver narrows it by what it already retains), nor is
/// `max_provider_stack_bytes` (it depends on the caller's stack).
const DETERMINISTIC_ADMISSION_LIMITS: [&str; 13] = [
    "max_definitions",
    "max_fields",
    "max_variant_alternatives",
    "max_pending_bytes",
    "max_scope_depth",
    "max_type_depth",
    "max_array_dimensions",
    "max_branch_states",
    "max_provider_queries",
    "max_work",
    "max_byte_work",
    "max_expression_bytes",
    "max_expression_tokens",
];

/// The fixed compile-time bound a load failure names (13d-1), as
/// `pascal_core::compile_time::resolver_error` words a structural
/// admission failure: such a member is limited after one attempt, not
/// deferred. `None` for anything that may pass (a budget, a cancellation,
/// any other reason).
pub(crate) fn deterministic_admission_limit(message: &str) -> Option<&'static str> {
    const ADMISSION: &str = "compile-time admission: Limit { name: \"";
    if message == CANCELLATION_MESSAGE || message.to_ascii_lowercase().contains("budget") {
        return None;
    }
    let rest = &message[message.find(ADMISSION)? + ADMISSION.len()..];
    let name = &rest[..rest.find('"')?];
    DETERMINISTIC_ADMISSION_LIMITS
        .into_iter()
        .find(|bound| *bound == name)
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

/// The phases of a member's reuse a test can time (R8-1).
#[derive(Debug, Clone, Copy)]
pub(super) enum ReusePhase {
    /// The snapshot's member entry, record and evidence.
    Lookup,
    /// The saved root bytes, their hash and the local artifact key.
    ReadHash,
    Validation,
    Decode,
    /// The decoded facts into the index.
    Insert,
    /// The member's record, proven again for this snapshot.
    Prove,
}

const REUSE_PHASES: usize = 6;
const REUSE_PHASE_NAMES: [&str; REUSE_PHASES] = [
    "lookup",
    "readHash",
    "validation",
    "decode",
    "insert",
    "prove",
];

#[cfg(test)]
thread_local! {
    static REUSE_PHASE_TIMES: std::cell::RefCell<[(Duration, usize); REUSE_PHASES]> =
        const { std::cell::RefCell::new([(Duration::ZERO, 0); REUSE_PHASES]) };
}

/// Process-wide time and calls of each reuse phase, summed over every
/// thread (the parallel workers included); recorded only while
/// `phase_metrics_on`.
static PHASE_NANOS: [AtomicU64; REUSE_PHASES] = [const { AtomicU64::new(0) }; REUSE_PHASES];
static PHASE_CALLS: [AtomicU64; REUSE_PHASES] = [const { AtomicU64::new(0) }; REUSE_PHASES];

#[cfg(test)]
static PHASE_METRICS_FORCED: AtomicBool = AtomicBool::new(false);

/// Tests turn the phase counters on without the trace file (never off:
/// tests run in parallel and the counters are process-wide).
#[cfg(test)]
fn set_phase_metrics_for_test(on: bool) {
    PHASE_METRICS_FORCED.store(on, Ordering::Relaxed);
}

/// The phase counters run in release builds behind the trace switch
/// (`PASCAL_LSP_TRACE`), which also receives their report.
fn phase_metrics_on() -> bool {
    #[cfg(test)]
    if PHASE_METRICS_FORCED.load(Ordering::Relaxed) {
        return true;
    }
    crate::trace::enabled()
}

/// A reading of the process-wide phase counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct ReusePhaseTotals {
    nanos: [u64; REUSE_PHASES],
    calls: [u64; REUSE_PHASES],
}

impl ReusePhaseTotals {
    fn snapshot() -> Self {
        let mut totals = Self::default();
        for at in 0..REUSE_PHASES {
            totals.nanos[at] = PHASE_NANOS[at].load(Ordering::Relaxed);
            totals.calls[at] = PHASE_CALLS[at].load(Ordering::Relaxed);
        }
        totals
    }

    /// What was recorded after `earlier`.
    fn since(&self, earlier: &Self) -> Self {
        let mut totals = Self::default();
        for at in 0..REUSE_PHASES {
            totals.nanos[at] = self.nanos[at].saturating_sub(earlier.nanos[at]);
            totals.calls[at] = self.calls[at].saturating_sub(earlier.calls[at]);
        }
        totals
    }

    #[cfg(test)]
    fn calls(&self, phase: ReusePhase) -> usize {
        usize::try_from(self.calls[phase as usize]).unwrap_or(usize::MAX)
    }

    fn is_empty(&self) -> bool {
        self.calls.iter().all(|calls| *calls == 0)
    }
}

/// One batch's reuse phases: the trace line that splits a reuse pass's
/// time between read+hash, validation, decode, insert and prove.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReusePhaseReport {
    mode: IndexMode,
    /// Validation threads the batch used (1 when serial).
    threads: usize,
    /// Members the batch tried.
    members: usize,
    totals: ReusePhaseTotals,
}

impl std::fmt::Display for ReusePhaseReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "reuse phases: mode={:?} threads={} members={}",
            self.mode, self.threads, self.members
        )?;
        for (at, name) in REUSE_PHASE_NAMES.iter().enumerate() {
            write!(
                formatter,
                " {name}={:.1}ms/{}",
                self.totals.nanos[at] as f64 / 1e6,
                self.totals.calls[at]
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static LAST_PHASE_REPORT: std::cell::RefCell<Option<ReusePhaseReport>> =
        const { std::cell::RefCell::new(None) };
}

/// The report the last batch on this thread emitted.
#[cfg(test)]
fn take_phase_report() -> Option<ReusePhaseReport> {
    LAST_PHASE_REPORT.with(|report| report.borrow_mut().take())
}

/// Traces what the batch added to the phase counters since `before`.
fn report_reuse_phases(before: &ReusePhaseTotals, mode: IndexMode, threads: usize, members: usize) {
    if !phase_metrics_on() {
        return;
    }
    let totals = ReusePhaseTotals::snapshot().since(before);
    if totals.is_empty() {
        return;
    }
    let report = ReusePhaseReport {
        mode,
        threads,
        members,
        totals,
    };
    crate::trace::trace!("  {report}");
    #[cfg(test)]
    LAST_PHASE_REPORT.with(|last| *last.borrow_mut() = Some(report));
}

/// `run`, timed as `phase` on this thread in tests, and in the
/// process-wide counters while phase metrics are on.
#[inline]
pub(super) fn timed<T>(phase: ReusePhase, run: impl FnOnce() -> T) -> T {
    let record = phase_metrics_on();
    let started = (cfg!(test) || record).then(Instant::now);
    let value = run();
    if let Some(started) = started {
        let elapsed = started.elapsed();
        #[cfg(test)]
        REUSE_PHASE_TIMES.with(|times| {
            let slot = &mut times.borrow_mut()[phase as usize];
            slot.0 += elapsed;
            slot.1 += 1;
        });
        if record {
            PHASE_NANOS[phase as usize].fetch_add(
                u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            PHASE_CALLS[phase as usize].fetch_add(1, Ordering::Relaxed);
        }
    }
    value
}

#[cfg(test)]
thread_local! {
    /// WAL bytes added by artifact (0) and record (1) publications, and
    /// how many of each were measured.
    static PUBLISH_WAL: std::cell::RefCell<[(u64, usize); 2]> =
        const { std::cell::RefCell::new([(0, 0); 2]) };
}

/// The WAL's growth across one publication (a checkpoint in between
/// counts as none).
#[cfg(test)]
struct WalGrowth {
    path: PathBuf,
    before: u64,
}

#[cfg(test)]
impl WalGrowth {
    fn new(store: &pascal_cache::Store) -> Self {
        let path = store.location().database_files()[1].clone();
        let before = std::fs::metadata(&path).map_or(0, |metadata| metadata.len());
        Self { path, before }
    }

    fn note(self, slot: usize) -> Self {
        let after = std::fs::metadata(&self.path).map_or(0, |metadata| metadata.len());
        PUBLISH_WAL.with(|totals| {
            let total = &mut totals.borrow_mut()[slot];
            total.0 += after.saturating_sub(self.before);
            total.1 += 1;
        });
        Self {
            before: after,
            ..self
        }
    }
}

#[cfg(test)]
fn take_publish_wal() -> [(u64, usize); 2] {
    PUBLISH_WAL.with(|totals| std::mem::take(&mut *totals.borrow_mut()))
}

/// The time and number of calls of each phase since the last take.
#[cfg(test)]
fn take_reuse_phases() -> [(Duration, usize); REUSE_PHASES] {
    REUSE_PHASE_TIMES.with(|times| std::mem::take(&mut *times.borrow_mut()))
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
    /// `absent` when it held nothing for the member.
    Missed { absent: bool },
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

/// `validate_reuse`'s answer: an outcome, or proven facts to apply.
enum Validated {
    Outcome(ReuseOutcome),
    Hit(ReuseHit),
}

/// Facts the store proved for a member, charged until applied or dropped.
struct ReuseHit {
    facts: super::persist::AdmittedFacts,
    reads: usize,
    artifact: pascal_cache::Digest,
    evidence: Option<pascal_cache::Digest>,
    include_stamps: Vec<(PathBuf, PathStamp)>,
    root_hash: u64,
    /// The held snapshot and the live record it listed for the member
    /// (13f).
    record: Option<(pascal_cache::SnapshotId, pascal_cache::Digest)>,
}

/// Bounded parallel validation of a reuse batch (R8-1): up to `threads`
/// analysis threads validate members ahead of the batch, which applies
/// their results in member order. Each worker checks `may_run` (interactive
/// work, cancellation) before each member it starts, and worker `n` takes a
/// member only while `workers()` (the free general analysis slots) exceeds
/// `n`; worker 0 always may. The proof rule is unchanged: a worker runs the
/// serial validation of one member.
#[derive(Clone, Copy)]
pub(crate) struct ParallelValidation<'a> {
    pub(crate) threads: usize,
    pub(crate) may_run: &'a (dyn Fn() -> bool + Sync),
    pub(crate) workers: &'a (dyn Fn() -> usize + Sync),
}

/// What a worker found for one member of a parallel reuse batch.
enum Prefetched {
    /// Open in the editor: skipped.
    Open,
    /// The worker found interactive work or cancellation before it started
    /// this member: the batch stops here.
    Preempted,
    /// `stamp` is `None` when the member is gone. `epoch` is the cache's
    /// invalidation epoch before the stamp: a watcher event since makes the
    /// batch prove the member again itself.
    Member {
        epoch: u64,
        stamp: Option<PathStamp>,
        reuse: Option<Box<Validated>>,
        /// A Full batch's extraction after a reuse miss (13c-3).
        extracted: Option<Box<Extraction>>,
    },
}

/// The hand-off between a parallel reuse batch and its workers. Members are
/// taken in order and at most `window` past the one the batch applies, so
/// at most `window` members' results (decoded facts, or a prepared
/// publication and its reservation) are alive at once, the one the batch
/// applies included: a worker takes member `n` only while
/// `n < done + window` (D41).
struct Pipeline {
    state: std::sync::Mutex<PipelineState>,
    changed: std::sync::Condvar,
    window: usize,
    /// Set once the batch stops or interactive work arrives: the
    /// validations in flight stop through their work budget (M5).
    tripped: Arc<AtomicBool>,
    #[cfg(test)]
    takes: std::sync::Mutex<Vec<(usize, usize)>>,
    #[cfg(test)]
    live: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    peak: std::sync::Mutex<ValidationPeak>,
}

#[derive(Default)]
struct PipelineState {
    /// The next member a worker takes.
    next: usize,
    /// The member the batch is at; those before it are done.
    done: usize,
    results: HashMap<usize, Prefetched>,
    stopped: bool,
    /// No member is taken any more; those taken still arrive (13c-3).
    closed: bool,
    /// Workers started and not yet exited: with none, a member never
    /// taken or never handed over cannot arrive (N2).
    workers: usize,
}

/// The most facts a parallel batch held at once (test measure).
#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ValidationPeak {
    pub(crate) held: usize,
    pub(crate) reserved_bytes: usize,
}

#[cfg(test)]
thread_local! {
    static VALIDATION_PEAK: std::cell::Cell<ValidationPeak> =
        const { std::cell::Cell::new(ValidationPeak { held: 0, reserved_bytes: 0 }) };
}

/// The peak of the last parallel batch on this thread.
#[cfg(test)]
pub(crate) fn take_validation_peak() -> ValidationPeak {
    VALIDATION_PEAK.with(std::cell::Cell::take)
}

#[cfg(test)]
thread_local! {
    /// `(worker, workers())` for each member taken, in take order.
    static VALIDATION_TAKES: std::cell::RefCell<Vec<(usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Runs in the batch before it applies a worker's result.
    static BEFORE_APPLY: std::cell::RefCell<Option<LoadHook>> =
        const { std::cell::RefCell::new(None) };
}

/// The takes of the last parallel batch on this thread.
#[cfg(test)]
pub(crate) fn take_validation_takes() -> Vec<(usize, usize)> {
    VALIDATION_TAKES.with(|takes| std::mem::take(&mut *takes.borrow_mut()))
}

#[cfg(test)]
fn set_before_apply_hook(hook: Option<LoadHook>) {
    BEFORE_APPLY.with(|slot| *slot.borrow_mut() = hook);
}

impl Pipeline {
    fn new(window: usize) -> Self {
        Self {
            state: std::sync::Mutex::default(),
            changed: std::sync::Condvar::new(),
            window,
            tripped: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            takes: std::sync::Mutex::default(),
            #[cfg(test)]
            live: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            peak: std::sync::Mutex::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PipelineState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait<'a>(
        &self,
        state: std::sync::MutexGuard<'a, PipelineState>,
    ) -> std::sync::MutexGuard<'a, PipelineState> {
        self.changed
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The next member for `worker`, once the window has room; `None` when
    /// every member is taken, the batch stopped or the worker's slot was
    /// taken (`workers()` is `worker` or less).
    fn take_next(
        &self,
        members: usize,
        worker: usize,
        workers: &(dyn Fn() -> usize + Sync),
    ) -> Option<(usize, usize)> {
        let mut state = self.lock();
        loop {
            if state.stopped || state.closed || state.next >= members {
                return None;
            }
            let allowed = workers();
            if worker > 0 && worker >= allowed {
                return None;
            }
            if state.next < state.done + self.window {
                let position = state.next;
                state.next += 1;
                return Some((position, allowed));
            }
            state = self.wait(state);
        }
    }

    /// A worker starts; `worker_exited` must follow.
    fn worker_started(&self) {
        self.lock().workers += 1;
    }

    fn worker_exited(&self) {
        self.lock().workers -= 1;
        self.changed.notify_all();
    }

    #[cfg(test)]
    fn note_take(&self, worker: usize, allowed: usize) {
        self.takes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((worker, allowed));
    }

    fn put(&self, position: usize, prefetched: Prefetched) {
        let mut state = self.lock();
        if matches!(prefetched, Prefetched::Preempted) {
            state.stopped = true;
            self.tripped.store(true, Ordering::Relaxed);
        }
        state.results.insert(position, prefetched);
        drop(state);
        self.changed.notify_all();
    }

    /// The batch is at `position`: what a worker found for it, once found.
    /// The members before it are done, which makes room in the window.
    /// While it waits, interactive work (`may_run` false) trips the
    /// validations in flight.
    fn take(&self, position: usize, may_run: &(dyn Fn() -> bool + Sync)) -> Prefetched {
        let mut state = self.lock();
        state.done = position;
        self.changed.notify_all();
        loop {
            if let Some(prefetched) = state.results.remove(&position) {
                return prefetched;
            }
            // Never taken: a worker stopped before it. With no worker
            // left, nothing else can arrive.
            if (state.stopped && state.next <= position) || state.workers == 0 {
                return Prefetched::Preempted;
            }
            // Interactive work: stop the batch here, as a failing
            // `before_io` does, and trip the validations in flight. The
            // trip lasts as long as this pipeline, which ends with the
            // batch (N1).
            if !may_run() {
                state.stopped = true;
                self.tripped.store(true, Ordering::Relaxed);
                self.changed.notify_all();
                return Prefetched::Preempted;
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(25))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// The batch reached its time bound: the workers take no member, and
    /// the ones they took (before the returned position) still arrive, to
    /// be applied. Nothing in flight is tripped.
    fn close(&self) -> usize {
        let mut state = self.lock();
        state.closed = true;
        let taken = state.next;
        drop(state);
        self.changed.notify_all();
        taken
    }

    fn stop(&self) {
        self.lock().stopped = true;
        self.tripped.store(true, Ordering::Relaxed);
        self.changed.notify_all();
    }

    #[cfg(test)]
    fn note_hit(&self, reserved_bytes: usize) {
        let held = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        let mut peak = self
            .peak
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        peak.held = peak.held.max(held);
        peak.reserved_bytes = peak.reserved_bytes.max(reserved_bytes);
    }

    #[cfg(test)]
    fn note_applied(&self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A member a worker took and has not handed over: dropped (an early
/// return, a panic) it stops the batch there.
struct InFlight<'a> {
    pipeline: &'a Pipeline,
    position: usize,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.pipeline.put(self.position, Prefetched::Preempted);
    }
}

/// A worker exits, however it ends.
struct WorkerExit<'a>(&'a Pipeline);

impl Drop for WorkerExit<'_> {
    fn drop(&mut self) {
        self.0.worker_exited();
    }
}

/// Stops the workers however the batch ends.
struct StopOnDrop<'a>(&'a Pipeline);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// A worker's loop (R8-1, 13c-3): it takes members in order through
/// `pipeline` and starts each only while `may_run` holds and the request is
/// not cancelled; `find` gives what it found for the member.
fn prefetch_loop(
    members: &[PathBuf],
    pipeline: &Pipeline,
    worker: usize,
    validation: ParallelValidation<'_>,
    cancel: &AtomicBool,
    mut find: impl FnMut(&Path) -> Prefetched,
) {
    // Its slot claims yield to interactive work, as the warmer's do, and
    // count as cancelled once the batch stops or interactive work trips it:
    // an interactive lookup takes them over rather than wait (13c-3 I1).
    crate::project_cache::set_claim_role_on_this_thread(crate::project_cache::ClaimRole::yielding(
        Arc::clone(&pipeline.tripped),
    ));
    while let Some((position, allowed)) =
        pipeline.take_next(members.len(), worker, validation.workers)
    {
        // A worker that panics stops the batch at its member rather than
        // leave it waiting; the scope then propagates the panic.
        let in_flight = InFlight { pipeline, position };
        if cancel.load(Ordering::Relaxed) || !(validation.may_run)() {
            drop(in_flight);
            return;
        }
        #[cfg(test)]
        pipeline.note_take(worker, allowed);
        #[cfg(not(test))]
        let _ = allowed;
        let prefetched = find(&members[position]);
        std::mem::forget(in_flight);
        pipeline.put(position, prefetched);
    }
}

/// A stable refusal, unless the file is gone: then its directory listing
/// changed, which is no refusal.
fn refused_unless_vanished(path: &Path, reason: String) -> Extraction {
    if path_stamp(path).is_none() {
        Extraction::Vanished
    } else {
        Extraction::Refused(reason)
    }
}

/// What extracting one member gave, before anything reaches the index or
/// the store (13c-3): a worker computes it on its own view, and the batch
/// applies it in member order (`apply_extraction`), as the serial loop
/// does both at once (`index_member`).
enum Extraction {
    /// `index_member`'s `Err`: a cancellation, or a reason the member
    /// stays pending.
    Failed(String),
    Refused(String),
    /// The file is gone: its directory listing changed. Seen by the
    /// worker after the load, as the serial loop sees it; the batch only
    /// invalidates the path, so a file back by then is relisted and
    /// extracted by a later batch.
    Vanished,
    Deferred(String),
    PastDeadline,
    /// The facts were limited.
    Limited,
    /// The compile-time session was refused by this fixed admission bound
    /// (13d-1): the same inputs refuse again.
    OverAdmission(&'static str),
    /// An unsaved buffer: nothing to publish.
    Overlay,
    Facts(Box<ExtractedMember>),
}

/// A loaded member's facts, slots and stamps, and its prepared publication.
struct ExtractedMember {
    member: MemberFacts,
    stamps: Vec<MemberStamp>,
    hash: u64,
    /// `None` when the view could not publish as it extracted.
    prepared: Option<Result<PreparedPublication, super::persist::PersistError>>,
    /// What the session read without a capture, when that refused the
    /// evidence (13c-1).
    unprovable: Option<(&'static str, String)>,
}

/// One `prepare_artifact` pass and the reservation of its transient bytes.
struct PreparedPublication {
    artifact: super::persist::PreparedArtifact,
    /// Reserved from the publication pool as a worker prepared it, so the
    /// bytes are charged while it waits for the batch (`Some(None)`:
    /// refused, D41); `None` when the serial loop reserves them as it
    /// publishes, as before 13c-3.
    transient: Option<Option<crate::project_cache::Reservation>>,
}

/// Why an extracted member was not persisted. None is an error: the member
/// stays memory-only, and a later process extracts it again. The reasons are
/// diagnostics (traces).
#[derive(Debug)]
pub(crate) enum PersistSkip {
    /// No baseline view, a context built from unsaved project input, or no
    /// store.
    Unavailable,
    Prepare(super::persist::PersistError),
    /// Neither the publication pool nor `maxCacheBytes` had room for the
    /// publication's transient bytes.
    Memory,
    #[allow(dead_code)] // read through `Debug` by the "not persisted" trace
    Ineligible(super::eligible::Ineligible),
    Store(pascal_cache::Bypass),
}

impl PersistSkip {
    /// The publication exceeds a fixed limit its inputs decide: more
    /// distinct reads than a capture holds, or a store admission limit.
    /// Publishing the same member again fails the same way.
    fn over_fixed_limit(&self) -> bool {
        use super::evidence::EvidenceGap;
        use super::persist::PersistError;
        matches!(
            self,
            Self::Prepare(
                PersistError::Evidence(EvidenceGap::Overflowed)
                    | PersistError::Store(pascal_cache::Bypass::TooLarge(_))
            ) | Self::Store(pascal_cache::Bypass::TooLarge(_))
        )
    }
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
    #[cfg(test)]
    pub(crate) fn advance_workspace_index(
        &mut self,
        anchor: &IndexAnchor,
        bound: IndexBound,
        before_io: &mut dyn FnMut() -> Result<(), String>,
        cancel: &AtomicBool,
    ) -> Result<IndexProgress, String> {
        self.advance_workspace_index_with(anchor, bound, before_io, None, cancel)
    }

    /// `advance_workspace_index`, a Reuse batch of a persisting baseline
    /// view validating its members on `validation`'s workers (R8-1).
    pub(crate) fn advance_workspace_index_with(
        &mut self,
        anchor: &IndexAnchor,
        bound: IndexBound,
        before_io: &mut dyn FnMut() -> Result<(), String>,
        validation: Option<ParallelValidation<'_>>,
        cancel: &AtomicBool,
    ) -> Result<IndexProgress, String> {
        let previous = std::mem::replace(&mut self.index_build, true);
        let result = self.advance_index_batch(anchor, bound, before_io, validation, cancel);
        self.index_build = previous;
        self.pinned_context = None;
        result
    }

    fn advance_index_batch(
        &mut self,
        anchor: &IndexAnchor,
        bound: IndexBound,
        before_io: &mut dyn FnMut() -> Result<(), String>,
        validation: Option<ParallelValidation<'_>>,
        cancel: &AtomicBool,
    ) -> Result<IndexProgress, String> {
        let (key, anchor) = self.index_target_for(anchor, cancel)?;
        // Skipped files prune the contexts no document uses, and a build
        // view holds a document only while it loads one.
        self.pinned_context = Some(key.clone());
        self.ensure_index_membership(&key, &anchor, cancel)?;
        let persisting = self.persistent_cache().is_enabled();
        // A publication refused for room: the batch waits once at most for
        // the maintenance it woke (here, or before its next extraction).
        let mut waited_for_room = false;
        if self.is_baseline() && persisting {
            self.release_evicted_holds();
            self.adopt_baseline_hint_once(&key, cancel);
            // Members an earlier batch indexed while the store was not
            // enabled get their records now, without a restart.
            if self.may_publish(&key) {
                let restaled = self.project_cache.restale_memory_only_members(&key);
                if restaled > 0 {
                    crate::trace::trace!("  {restaled} members indexed memory-only to prove");
                }
                if self.project_cache.index_awaiting_room(&key) > 0 {
                    waited_for_room = true;
                    // After a wait that found no room, a batch with other
                    // members to extract does not wait again; one left with
                    // nothing else does, which paces the retries.
                    let others = self
                        .project_cache
                        .read_index(&key, |index, _| !index.pending(1).is_empty())
                        .unwrap_or(false);
                    let may_wait = !others || !self.project_cache.index_room_wait_failed(&key);
                    if self.store_has_room(&key, may_wait, before_io)? {
                        let restaled = self.project_cache.restale_awaiting_room(&key);
                        crate::trace::trace!("  {restaled} members refused for room to prove");
                    }
                }
            }
        }
        let mut progress = IndexProgress::new(key.clone(), bound.mode);
        let phases_before = ReusePhaseTotals::snapshot();
        // Workers that validated this batch's members: the requested count
        // capped by the members queued, 1 when the batch ran serially.
        let mut workers_used = 1;
        let started = Instant::now();
        // Open members are skipped, so ask for enough to fill the bound.
        let wanted = bound.units.saturating_add(self.open_documents.len());
        let pending = match bound.mode {
            IndexMode::Reuse => self.project_cache.reuse_pending_index_members(&key, wanted),
            // A member retrying a previous refusal goes after every other
            // pending member (H4/R8-3); without a hint, path order stays.
            IndexMode::Full => self
                .project_cache
                .read_index(&key, |index, _| {
                    let (retrying, others): (Vec<_>, Vec<_>) = index
                        .pending(wanted)
                        .into_iter()
                        .partition(|path| index.retrying(path));
                    others.into_iter().chain(retrying).collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        };
        let mut pending = std::collections::VecDeque::from(pending);
        // Members this batch tried, skipped ones included, are never untried.
        let mut tried = HashSet::new();
        let mut context_key = super::persist::ContextKeyMemo::default();
        // Workers validate a Reuse batch's members ahead of it (R8-1), and
        // extract a Full batch's (13c-3); a batch without a store stays
        // serial.
        let parallel = validation.filter(|validation| {
            validation.threads > 1 && persisting && self.is_baseline() && pending.len() > 1
        });
        if let Some(validation) = parallel {
            let members = pending.len();
            // No worker could start: the batch runs serially.
            match bound.mode {
                IndexMode::Reuse => {
                    if self.reuse_in_parallel(
                        &key,
                        &mut pending,
                        bound,
                        started,
                        before_io,
                        validation,
                        &mut progress,
                        &mut tried,
                        cancel,
                    )? {
                        pending.clear();
                        workers_used = validation.threads.min(members).max(1);
                    }
                }
                // Members retrying an earlier refusal come back at the
                // batch's end, serially, as in the serial loop.
                IndexMode::Full => {
                    if let Some(later) = self.extract_in_parallel(
                        &key,
                        &pending,
                        bound,
                        started,
                        before_io,
                        validation,
                        &mut progress,
                        &mut tried,
                        &mut waited_for_room,
                        cancel,
                    )? {
                        pending = later.into();
                        workers_used = validation.threads.min(members).max(1);
                    }
                }
            }
        }
        while let Some(path) = pending.pop_front() {
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
            let reuse = self.reuse_index_member(&key, &path, &own_stamp, &mut context_key, cancel);
            #[cfg(test)]
            run_hook(&AFTER_REUSE, &path);
            let asked = !matches!(reuse, ReuseOutcome::NoStore);
            match reuse {
                ReuseOutcome::Reused => {
                    progress.reused += 1;
                    continue;
                }
                ReuseOutcome::Missed { absent } => {
                    self.project_cache.mark_index_reuse_tried(&key, &path);
                    progress.reuse_missed += 1;
                    progress.reuse_absent += usize::from(absent);
                }
                ReuseOutcome::Retry => progress.reuse_deferred += 1,
                // A previous refusal is retried after every other member: a
                // Full batch comes back to it at its end, within its bound,
                // when it is tried for reuse no more.
                ReuseOutcome::Retrying => {
                    progress.reuse_retrying += 1;
                    self.project_cache.mark_index_reuse_tried(&key, &path);
                    self.project_cache
                        .defer_index_member(&key, &path, RETRYING_REFUSAL);
                    if bound.mode == IndexMode::Full {
                        pending.push_back(path);
                    }
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
            self.wait_for_room_once(&key, &mut waited_for_room, before_io)?;
            let retrying = self
                .project_cache
                .read_index(&key, |index, _| index.retrying(&path))
                .unwrap_or(false);
            // One member's failure never stops the batch: it stays pending.
            // Its extraction runs under its own deadline (A4); the batch
            // bound is checked between members.
            self.member_deadline = Some(member_deadline(&path));
            let indexed = self.index_member(&key, &path, own_stamp, cancel);
            self.member_deadline = None;
            if !self.note_member_outcome(&key, &path, indexed, retrying, &mut progress)? {
                break;
            }
        }
        // The index moved to the key a new selection gives (A8) while the
        // batch ran: storing a membership here would bring the old key back.
        if !self.project_cache.index_moved_by_selection(&key) {
            self.ensure_index_membership(&key, &anchor, cancel)?;
        }
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
        progress.retrying = retrying
            .iter()
            .filter(|path| !self.is_open_member(path))
            .count();
        progress.awaiting_room = self.project_cache.index_awaiting_room(&key);
        progress.complete = progress.pending == progress.retrying
            && progress.awaiting_room == 0
            && !progress.memory;
        report_reuse_phases(&phases_before, bound.mode, workers_used, tried.len());
        Ok(progress)
    }

    /// The members of a Reuse batch, validated by `validation.threads`
    /// workers ahead of the batch and applied here in member order, with
    /// every check the serial loop makes (R8-1). `before_io` runs before
    /// each member applied; a worker that finds `may_run` false stops the
    /// batch at that member, as a failing `before_io` does.
    #[allow(clippy::too_many_arguments)]
    fn reuse_in_parallel(
        &self,
        key: &ContextKey,
        pending: &mut std::collections::VecDeque<PathBuf>,
        bound: IndexBound,
        started: Instant,
        before_io: &mut dyn FnMut() -> Result<(), String>,
        validation: ParallelValidation<'_>,
        progress: &mut IndexProgress,
        tried: &mut HashSet<String>,
        cancel: &AtomicBool,
    ) -> Result<bool, String> {
        // Workers take no member the bound leaves untried (M1); skipped
        // open members do not count towards it. Members validated past the
        // time bound or a stop are dropped unapplied, so the store's hit
        // count may exceed `reused` by up to the window.
        let untried = bound.units.saturating_sub(tried.len());
        let members = Vec::from_iter(
            pending
                .iter()
                .take(untried.saturating_add(self.open_documents.len()))
                .cloned(),
        );
        let threads = validation.threads.min(members.len()).max(1);
        let pipeline = Pipeline::new(threads);
        let result = std::thread::scope(|scope| {
            let _stop = StopOnDrop(&pipeline);
            let (pipeline, members) = (&pipeline, &members);
            // Worker ids follow spawn order, so worker 0 (which always
            // may run) exists whenever any worker does (N2).
            let mut spawned = 0;
            for _ in 0..threads {
                let worker = spawned;
                pipeline.worker_started();
                let started = crate::server::analysis_thread(&format!("PascalLspReuse{worker}"))
                    .spawn_scoped(scope, move || {
                        let _exit = WorkerExit(pipeline);
                        self.prefetch_members(key, members, pipeline, worker, validation, cancel);
                    });
                if started.is_ok() {
                    spawned += 1;
                } else {
                    pipeline.worker_exited();
                }
            }
            if spawned == 0 {
                return Ok(false);
            }
            self.apply_prefetched(
                key, members, pipeline, bound, started, before_io, validation, progress, tried,
                cancel,
            )
            .map(|()| true)
        });
        #[cfg(test)]
        VALIDATION_PEAK.with(|peak| {
            peak.set(
                *pipeline
                    .peak
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        });
        #[cfg(test)]
        VALIDATION_TAKES.with(|takes| {
            *takes.borrow_mut() = std::mem::take(
                &mut *pipeline
                    .takes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        });
        result
    }

    /// One worker of `reuse_in_parallel`.
    fn prefetch_members(
        &self,
        key: &ContextKey,
        members: &[PathBuf],
        pipeline: &Pipeline,
        worker: usize,
        validation: ParallelValidation<'_>,
        cancel: &AtomicBool,
    ) {
        let mut context_key = super::persist::ContextKeyMemo::default();
        prefetch_loop(members, pipeline, worker, validation, cancel, |path| {
            self.prefetch_member(key, path, pipeline, &mut context_key, cancel)
        });
    }

    /// What a worker finds for one member before the batch applies it: its
    /// stamp (A7), then the store's proof of its facts, unless it is open
    /// or a stale member the batch may refresh from its stamps.
    fn prefetch_member(
        &self,
        key: &ContextKey,
        path: &Path,
        pipeline: &Pipeline,
        context_key: &mut super::persist::ContextKeyMemo,
        cancel: &AtomicBool,
    ) -> Prefetched {
        if self.is_open_member(path) {
            return Prefetched::Open;
        }
        let epoch = self.project_cache.invalidation_epoch();
        // The stamp precedes the bytes it describes (A7).
        let stamp = path_stamp(path);
        let reprove = self.may_publish(key)
            && self.persistent_cache().is_enabled()
            && !self.project_cache.index_member_has_record(key, path);
        // A stale member may be refreshed from its stamps: the batch
        // decides, in order.
        let refresh = !reprove
            && self
                .project_cache
                .read_index(key, |index, _| {
                    index.state_of(path) == Some(MemberState::Stale)
                })
                .unwrap_or(false);
        let reuse = (stamp.is_some() && !refresh).then(|| {
            Box::new(self.validate_reuse(key, path, context_key, &pipeline.tripped, cancel))
        });
        #[cfg(test)]
        if matches!(reuse.as_deref(), Some(Validated::Hit(_))) {
            pipeline.note_hit(self.project_cache.stats().reserved_facts_bytes);
        }
        Prefetched::Member {
            epoch,
            stamp,
            reuse,
            extracted: None,
        }
    }

    /// The batch's side of `reuse_in_parallel`: the serial loop's checks
    /// and outcomes, Reuse mode, in member order.
    #[allow(clippy::too_many_arguments)]
    fn apply_prefetched(
        &self,
        key: &ContextKey,
        members: &[PathBuf],
        pipeline: &Pipeline,
        bound: IndexBound,
        started: Instant,
        before_io: &mut dyn FnMut() -> Result<(), String>,
        validation: ParallelValidation<'_>,
        progress: &mut IndexProgress,
        tried: &mut HashSet<String>,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let mut context_key = super::persist::ContextKeyMemo::default();
        for (position, path) in members.iter().enumerate() {
            if tried.len() >= bound.units
                || bound.time.is_some_and(|limit| started.elapsed() >= limit)
            {
                break;
            }
            let (epoch, mut stamp, mut reuse) = match pipeline.take(position, validation.may_run) {
                Prefetched::Preempted => return Err(CANCELLATION_MESSAGE.to_owned()),
                Prefetched::Open => continue,
                Prefetched::Member {
                    epoch,
                    stamp,
                    reuse,
                    ..
                } => (epoch, stamp, reuse),
            };
            tried.insert(path_key(path));
            #[cfg(test)]
            run_hook(&BEFORE_APPLY, path);
            // A watcher event since the worker's stamp may concern what it
            // read: the batch proves the member again itself (M2).
            if self.project_cache.invalidation_epoch() != epoch {
                #[cfg(test)]
                if matches!(reuse.as_deref(), Some(Validated::Hit(_))) {
                    pipeline.note_applied();
                }
                reuse = None;
                stamp = path_stamp(path);
            }
            #[cfg(test)]
            let stamp = stamp.filter(|_| !UNSTAMPABLE.with(|slot| slot.borrow().contains(path)));
            before_io()?;
            let Some(own_stamp) = stamp else {
                self.project_cache.invalidate_path(path);
                continue;
            };
            let reuse = match reuse.map(|reuse| *reuse) {
                Some(Validated::Hit(hit)) => {
                    let outcome = self.apply_reuse(key, path, &own_stamp, hit, cancel);
                    #[cfg(test)]
                    pipeline.note_applied();
                    outcome
                }
                Some(Validated::Outcome(outcome)) => outcome,
                None => {
                    let reprove = self.may_publish(key)
                        && self.persistent_cache().is_enabled()
                        && !self.project_cache.index_member_has_record(key, path);
                    if !reprove && self.refresh_unchanged_member(key, path, &own_stamp) {
                        progress.refreshed += 1;
                        continue;
                    }
                    self.reuse_index_member(key, path, &own_stamp, &mut context_key, cancel)
                }
            };
            #[cfg(test)]
            run_hook(&AFTER_REUSE, path);
            match reuse {
                ReuseOutcome::Reused => progress.reused += 1,
                ReuseOutcome::Missed { absent } => {
                    self.project_cache.mark_index_reuse_tried(key, path);
                    progress.reuse_missed += 1;
                    progress.reuse_absent += usize::from(absent);
                }
                ReuseOutcome::Retry => progress.reuse_deferred += 1,
                ReuseOutcome::Retrying => {
                    progress.reuse_retrying += 1;
                    self.project_cache.mark_index_reuse_tried(key, path);
                    self.project_cache
                        .defer_index_member(key, path, RETRYING_REFUSAL);
                }
                ReuseOutcome::Memory => {
                    progress.memory = true;
                    break;
                }
                ReuseOutcome::NoStore => {}
                ReuseOutcome::Cancelled => return Err(CANCELLATION_MESSAGE.to_owned()),
            }
        }
        Ok(())
    }

    /// The members of a Full batch, extracted by `validation.threads`
    /// workers ahead of the batch, each on its own view of the batch's
    /// input, and applied here in member order with every check the serial
    /// loop makes (13c-3). A worker tries the store first, as the serial
    /// loop does, and extracts after a miss: load, facts and the prepared
    /// publication, nothing that reaches the index or the store. The batch
    /// publishes, records and counts. `None` when no worker could start;
    /// otherwise the members retrying an earlier refusal, which the batch
    /// extracts last.
    ///
    /// Memory: each worker's view holds what a serial batch's view holds
    /// for the members it loads (their dependencies' parses and sessions,
    /// under the parse budget's reservations and leases), so the views hold
    /// at most `threads` times a serial batch's; the views themselves (the
    /// owners copied, maps) are not charged to `maxCacheBytes`, as the
    /// batch's own view is not. They drop with the batch.
    #[allow(clippy::too_many_arguments)]
    fn extract_in_parallel(
        &mut self,
        key: &ContextKey,
        pending: &std::collections::VecDeque<PathBuf>,
        bound: IndexBound,
        started: Instant,
        before_io: &mut dyn FnMut() -> Result<(), String>,
        validation: ParallelValidation<'_>,
        progress: &mut IndexProgress,
        tried: &mut HashSet<String>,
        waited_for_room: &mut bool,
        cancel: &AtomicBool,
    ) -> Result<Option<Vec<PathBuf>>, String> {
        let Some(state) = self.contexts.get(key).cloned() else {
            return Ok(None);
        };
        // As for reuse (M1): no member the unit bound leaves untried.
        let untried = bound.units.saturating_sub(tried.len());
        let members = Vec::from_iter(
            pending
                .iter()
                .take(untried.saturating_add(self.open_documents.len()))
                .cloned(),
        );
        let threads = validation.threads.min(members.len()).max(1);
        let pipeline = Pipeline::new(threads);
        let input = self.worker_view_input();
        #[cfg(test)]
        let seams = WorkerSeams::capture();
        // While the batch waits for a member, interactive work or a
        // cancellation trips the extractions in flight.
        let may_wait = || (validation.may_run)() && !cancel.load(Ordering::Relaxed);
        let result = std::thread::scope(|scope| {
            let _stop = StopOnDrop(&pipeline);
            let (pipeline, members, input, state) = (&pipeline, &members, &input, &state);
            let mut spawned = 0;
            for _ in 0..threads {
                let worker = spawned;
                #[cfg(test)]
                let seams = seams.clone();
                pipeline.worker_started();
                let started = crate::server::analysis_thread(&format!("PascalLspExtract{worker}"))
                    .spawn_scoped(scope, move || {
                        let _exit = WorkerExit(pipeline);
                        #[cfg(test)]
                        seams.install();
                        let mut view = Workspace::extraction_view(input, key, state);
                        let mut context_key = super::persist::ContextKeyMemo::default();
                        #[cfg(test)]
                        let mut taken = 0;
                        prefetch_loop(members, pipeline, worker, validation, cancel, |path| {
                            #[cfg(test)]
                            {
                                taken += 1;
                            }
                            view.prefetch_extraction(
                                key,
                                path,
                                pipeline,
                                &mut context_key,
                                validation,
                                cancel,
                            )
                        });
                        #[cfg(test)]
                        if crate::heap_count::counting() {
                            let freed = crate::heap_count::freed_during(|| drop(view));
                            note_view_freed("worker", freed, taken);
                        }
                    });
                if started.is_ok() {
                    spawned += 1;
                } else {
                    pipeline.worker_exited();
                }
            }
            if spawned == 0 {
                return Ok(None);
            }
            self.apply_extractions(
                key,
                members,
                pipeline,
                bound,
                started,
                before_io,
                &may_wait,
                progress,
                tried,
                waited_for_room,
                cancel,
            )
            .map(Some)
        });
        #[cfg(test)]
        VALIDATION_PEAK.with(|peak| {
            peak.set(
                *pipeline
                    .peak
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        });
        #[cfg(test)]
        VALIDATION_TAKES.with(|takes| {
            *takes.borrow_mut() = std::mem::take(
                &mut *pipeline
                    .takes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        });
        result
    }

    /// An extraction worker's view: the batch's input and its context,
    /// loading as an index build does.
    fn extraction_view(
        input: &WorkspaceInput,
        key: &ContextKey,
        state: &super::ContextState,
    ) -> Workspace {
        let mut view = Workspace::from_analysis_input(input);
        view.contexts.insert(key.clone(), state.clone());
        view.index_build = true;
        view.pinned_context = Some(key.clone());
        view
    }

    /// `prefetch_member`, then, after a miss, the member's extraction on
    /// this worker's view under its own deadline (A4). Interactive work
    /// found between the two stops the batch there.
    fn prefetch_extraction(
        &mut self,
        key: &ContextKey,
        path: &Path,
        pipeline: &Pipeline,
        context_key: &mut super::persist::ContextKeyMemo,
        validation: ParallelValidation<'_>,
        cancel: &AtomicBool,
    ) -> Prefetched {
        let prefetched = self.prefetch_member(key, path, pipeline, context_key, cancel);
        let Prefetched::Member {
            epoch,
            stamp: Some(stamp),
            reuse: Some(reuse),
            ..
        } = prefetched
        else {
            return prefetched;
        };
        if !matches!(
            *reuse,
            Validated::Outcome(
                ReuseOutcome::Missed { .. } | ReuseOutcome::Retry | ReuseOutcome::NoStore
            )
        ) || stamp.bytes > WORKER_MEMBER_BYTES
        {
            // A large member is extracted by the batch (13d-2).
            return Prefetched::Member {
                epoch,
                stamp: Some(stamp),
                reuse: Some(reuse),
                extracted: None,
            };
        }
        if cancel.load(Ordering::Relaxed) || !(validation.may_run)() {
            return Prefetched::Preempted;
        }
        self.member_deadline = Some(member_deadline(path));
        // The pipeline's trip cancels the load in flight.
        let extracted = self.extract_member(key, path, stamp.clone(), true, &pipeline.tripped);
        self.member_deadline = None;
        #[cfg(test)]
        if matches!(&extracted, Extraction::Facts(facts) if matches!(facts.prepared, Some(Ok(_)))) {
            pipeline.note_hit(self.project_cache.stats().reserved_publication_bytes);
        }
        Prefetched::Member {
            epoch,
            stamp: Some(stamp),
            reuse: Some(reuse),
            extracted: Some(Box::new(extracted)),
        }
    }

    /// The batch's side of `extract_in_parallel`: the serial loop's checks
    /// and outcomes, Full mode, in member order. At the time bound the
    /// workers take no more members, and the ones they took are applied
    /// (each is done or under way), as the serial loop finishes the member
    /// it started.
    #[allow(clippy::too_many_arguments)]
    fn apply_extractions(
        &mut self,
        key: &ContextKey,
        members: &[PathBuf],
        pipeline: &Pipeline,
        bound: IndexBound,
        started: Instant,
        before_io: &mut dyn FnMut() -> Result<(), String>,
        may_wait: &(dyn Fn() -> bool + Sync),
        progress: &mut IndexProgress,
        tried: &mut HashSet<String>,
        waited_for_room: &mut bool,
        cancel: &AtomicBool,
    ) -> Result<Vec<PathBuf>, String> {
        let mut context_key = super::persist::ContextKeyMemo::default();
        let mut later = Vec::new();
        let mut taken = None;
        for (position, path) in members.iter().enumerate() {
            if taken.is_some_and(|taken| position >= taken) {
                break;
            }
            if taken.is_none() {
                if tried.len() >= bound.units {
                    break;
                }
                if bound.time.is_some_and(|limit| started.elapsed() >= limit) {
                    let end = pipeline.close();
                    if position >= end {
                        break;
                    }
                    taken = Some(end);
                }
            }
            let (epoch, mut stamp, mut reuse, mut extracted) =
                match pipeline.take(position, may_wait) {
                    Prefetched::Preempted => return Err(CANCELLATION_MESSAGE.to_owned()),
                    Prefetched::Open => continue,
                    Prefetched::Member {
                        epoch,
                        stamp,
                        reuse,
                        extracted,
                    } => (epoch, stamp, reuse, extracted),
                };
            #[cfg(test)]
            if matches!(reuse.as_deref(), Some(Validated::Hit(_)))
                || matches!(extracted.as_deref(), Some(Extraction::Facts(facts)) if matches!(facts.prepared, Some(Ok(_))))
            {
                pipeline.note_applied();
            }
            // A member the worker left for its size (13d-2), noted before
            // the epoch check may drop the worker's results.
            let left_large = stamp.as_ref().map(|stamp| stamp.bytes).filter(|bytes| {
                *bytes > WORKER_MEMBER_BYTES
                    && extracted.is_none()
                    && matches!(
                        reuse.as_deref(),
                        Some(Validated::Outcome(
                            ReuseOutcome::Missed { .. }
                                | ReuseOutcome::Retry
                                | ReuseOutcome::NoStore
                        ))
                    )
            });
            tried.insert(path_key(path));
            #[cfg(test)]
            run_hook(&BEFORE_APPLY, path);
            // A watcher event since the worker's stamp may concern what it
            // read: the batch indexes the member again itself (M2).
            if self.project_cache.invalidation_epoch() != epoch {
                reuse = None;
                extracted = None;
                stamp = path_stamp(path);
            }
            #[cfg(test)]
            let stamp = stamp.filter(|_| !UNSTAMPABLE.with(|slot| slot.borrow().contains(path)));
            before_io()?;
            let Some(own_stamp) = stamp else {
                self.project_cache.invalidate_path(path);
                continue;
            };
            let reuse = match reuse.map(|reuse| *reuse) {
                Some(Validated::Hit(hit)) => self.apply_reuse(key, path, &own_stamp, hit, cancel),
                Some(Validated::Outcome(outcome)) => outcome,
                None => {
                    let reprove = self.may_publish(key)
                        && self.persistent_cache().is_enabled()
                        && !self.project_cache.index_member_has_record(key, path);
                    if !reprove && self.refresh_unchanged_member(key, path, &own_stamp) {
                        progress.refreshed += 1;
                        continue;
                    }
                    self.reuse_index_member(key, path, &own_stamp, &mut context_key, cancel)
                }
            };
            #[cfg(test)]
            run_hook(&AFTER_REUSE, path);
            let asked = !matches!(reuse, ReuseOutcome::NoStore);
            match reuse {
                ReuseOutcome::Reused => {
                    progress.reused += 1;
                    continue;
                }
                ReuseOutcome::Missed { absent } => {
                    self.project_cache.mark_index_reuse_tried(key, path);
                    progress.reuse_missed += 1;
                    progress.reuse_absent += usize::from(absent);
                }
                ReuseOutcome::Retry => progress.reuse_deferred += 1,
                ReuseOutcome::Retrying => {
                    progress.reuse_retrying += 1;
                    self.project_cache.mark_index_reuse_tried(key, path);
                    self.project_cache
                        .defer_index_member(key, path, RETRYING_REFUSAL);
                    later.push(path.clone());
                    continue;
                }
                ReuseOutcome::Memory => {
                    progress.memory = true;
                    return Ok(Vec::new());
                }
                ReuseOutcome::NoStore => {}
                ReuseOutcome::Cancelled => return Err(CANCELLATION_MESSAGE.to_owned()),
            }
            if asked {
                before_io()?;
            }
            self.wait_for_room_once(key, waited_for_room, before_io)?;
            let retrying = self
                .project_cache
                .read_index(key, |index, _| index.retrying(path))
                .unwrap_or(false);
            let indexed = match extracted {
                Some(extraction) => self.apply_extraction(key, path, *extraction, cancel),
                // Proven again by the batch (M2), or a member the worker
                // left to it.
                None => {
                    if let Some(bytes) = left_large {
                        progress.left_large += 1;
                        progress.left_large_bytes += bytes;
                    }
                    self.member_deadline = Some(member_deadline(path));
                    let indexed = self.index_member(key, path, own_stamp, cancel);
                    self.member_deadline = None;
                    indexed
                }
            };
            if !self.note_member_outcome(key, path, indexed, retrying, progress)? {
                return Ok(Vec::new());
            }
        }
        Ok(later)
    }

    /// A refusal for room in this batch: wait once for the maintenance it
    /// woke, so the members after it publish rather than being refused in
    /// turn.
    fn wait_for_room_once(
        &self,
        key: &ContextKey,
        waited_for_room: &mut bool,
        before_io: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if !*waited_for_room && self.project_cache.index_is_awaiting_room(key) {
            *waited_for_room = true;
            let may_wait = !self.project_cache.index_room_wait_failed(key);
            if self.store_has_room(key, may_wait, before_io)? {
                self.project_cache.restale_awaiting_room(key);
            }
        }
        Ok(())
    }

    /// Counts one member's extraction; `false` when the batch stops there
    /// (the index share is full). A cancellation is the batch's `Err`.
    fn note_member_outcome(
        &self,
        key: &ContextKey,
        path: &Path,
        indexed: Result<MemberOutcome, String>,
        retrying: bool,
        progress: &mut IndexProgress,
    ) -> Result<bool, String> {
        let outcome = match indexed {
            Ok(outcome) => outcome,
            Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
            Err(error) => MemberOutcome::Deferred(error),
        };
        // Only an outcome that ends the retry counts: a skipped member
        // (an unsaved overlay, no member any more) or a memory stop
        // keeps it, and counting it would requeue the batch forever.
        if retrying && !matches!(outcome, MemberOutcome::Skipped | MemberOutcome::Memory) {
            progress.retries_attempted += 1;
        }
        match outcome {
            MemberOutcome::Published { persisted } => {
                progress.indexed += 1;
                progress.persisted += usize::from(persisted);
                if !persisted && !self.persistent_cache().is_enabled() {
                    self.project_cache.note_index_memory_only(key, path);
                }
            }
            MemberOutcome::Refused => progress.refused += 1,
            MemberOutcome::Limited => progress.limited += 1,
            MemberOutcome::Skipped => {}
            MemberOutcome::Deferred(reason) => {
                crate::trace::trace!("  index member {} deferred: {reason}", path.display());
                self.project_cache.defer_index_member(key, path, &reason);
                progress.deferred += 1;
            }
            MemberOutcome::Memory => {
                progress.memory = true;
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether the store takes writes again. Under pressure a maintenance
    /// step can clear (the WAL threshold, the budget's headroom) and with
    /// `may_wait`, the cache's maintenance is woken and given one step
    /// (`check` runs while waiting), then the pressure is read again; a
    /// wait that found no room is noted for `key`, and room clears it.
    /// Low free space returns at once.
    fn store_has_room(
        &self,
        key: &ContextKey,
        may_wait: bool,
        check: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<bool, String> {
        let pressure = || {
            #[cfg(test)]
            if let Some(pressure) = self.persistent_cache().pressure_for_test() {
                return Some(pressure);
            }
            let job = self.persistent_cache().job()?;
            match job.store().pressure() {
                Ok(pressure) => Some(pressure),
                Err(bypass) => {
                    job.observe(&bypass);
                    None
                }
            }
        };
        let room = |pressure: Option<pascal_cache::Pressure>| {
            let room = pressure == Some(pascal_cache::Pressure::None);
            if room {
                self.project_cache.set_index_room_wait_failed(key, false);
            }
            room
        };
        match pressure() {
            Some(pascal_cache::Pressure::WalPressure | pascal_cache::Pressure::OverHeadroom)
                if may_wait => {}
            other => return Ok(room(other)),
        }
        if self
            .persistent_cache()
            .await_maintenance(ROOM_WAIT, check)?
            && room(pressure())
        {
            return Ok(true);
        }
        self.project_cache.set_index_room_wait_failed(key, true);
        Ok(false)
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
        let extraction = self.extract_member(key, path, own_stamp, false, cancel);
        self.apply_extraction(key, path, extraction, cancel)
    }

    /// The half of `index_member` that touches neither the index nor the
    /// store (13c-3): load, facts, slots and stamps, and the prepared
    /// publication. An extraction worker runs it on its own view, and
    /// reserves the publication's transient bytes there (`reserve`).
    fn extract_member(
        &mut self,
        key: &ContextKey,
        path: &Path,
        own_stamp: crate::workspace::PathStamp,
        reserve: bool,
        cancel: &AtomicBool,
    ) -> Extraction {
        let Ok(uri) = Url::from_file_path(path) else {
            return Extraction::Refused("the path has no file URI".to_owned());
        };
        #[cfg(test)]
        if MEMBER_ERROR.with(|slot| slot.borrow().iter().any(|failing| failing == path)) {
            return Extraction::Failed("an injected member error".to_owned());
        }
        #[cfg(test)]
        if let Some(hook) = EXTRACT_HOOK.with(|slot| slot.borrow().clone()) {
            hook(path, cancel);
        }
        let held = self.index.contains(&uri);
        self.load_refusal = None;
        let loaded = self.load_source_with_cancel(&uri, key, &HashSet::new(), Some(cancel));
        #[cfg(test)]
        run_hook(&AFTER_LOAD, path);
        let past_deadline = match &loaded {
            Err(error) => error.contains(MEMBER_DEADLINE_MESSAGE),
            Ok(false) => self
                .load_refusal
                .as_deref()
                .is_some_and(|reason| reason.contains(MEMBER_DEADLINE_MESSAGE)),
            Ok(true) => false,
        };
        let extraction = if past_deadline {
            self.load_refusal = None;
            Extraction::PastDeadline
        } else {
            self.member_loaded(key, path, &uri, loaded, own_stamp, reserve, cancel)
        };
        if !held {
            let _ = self.remove_indexed_with_control(&uri, None, None, false);
        }
        extraction
    }

    /// The half of `index_member` that applies an extraction, in member
    /// order: the index, the store and the member's record.
    fn apply_extraction(
        &self,
        key: &ContextKey,
        path: &Path,
        extraction: Extraction,
        cancel: &AtomicBool,
    ) -> Result<MemberOutcome, String> {
        match extraction {
            Extraction::Failed(error) => Err(error),
            Extraction::Refused(reason) => Ok(self.refuse_member(key, path, reason, cancel)),
            // Not a refusal: the membership relists it.
            Extraction::Vanished => {
                self.project_cache.invalidate_path(path);
                Ok(MemberOutcome::Skipped)
            }
            Extraction::Deferred(reason) => Ok(MemberOutcome::Deferred(reason)),
            Extraction::PastDeadline => self.limit_past_deadline(key, path, cancel),
            Extraction::OverAdmission(bound) => self.limit_over_admission(key, path, bound, cancel),
            // Limited facts cannot prove a member irrelevant (§8): it stays a
            // candidate, read directly, until its file changes.
            Extraction::Limited => {
                check_workspace_cancel(Some(cancel))?;
                self.project_cache.limit_index_member(key, path);
                self.prove_unindexed(key, path, Unindexed::NotRecorded, cancel);
                Ok(MemberOutcome::Limited)
            }
            Extraction::Overlay => Ok(MemberOutcome::Skipped),
            Extraction::Facts(extracted) => Ok(self.publish_member(key, path, *extracted, cancel)),
        }
    }

    /// The member's extraction passed its deadline (A4): limited, an
    /// unverified candidate until it changes, listed by an artifact-less
    /// record so the context still seals; the next start retries it last
    /// (R8-3). A cancellation that came meanwhile wins: the member stays
    /// pending and the batch is requeued.
    fn limit_past_deadline(
        &self,
        key: &ContextKey,
        path: &Path,
        cancel: &AtomicBool,
    ) -> Result<MemberOutcome, String> {
        check_workspace_cancel(Some(cancel))?;
        crate::trace::trace!(
            "  index member {} limited: extraction over {} s",
            path.display(),
            MEMBER_DEADLINE.as_secs()
        );
        self.project_cache
            .limit_index_member_past_deadline(key, path);
        self.prove_unindexed(key, path, Unindexed::NotRecorded, cancel);
        Ok(MemberOutcome::Limited)
    }

    /// The member's compile-time session was refused by a fixed admission
    /// bound (13d-1): limited, like a member past its deadline, so the
    /// context completes and seals with it as a gap, and the next start
    /// retries it last (R8-3) rather than this process deferring it again.
    fn limit_over_admission(
        &self,
        key: &ContextKey,
        path: &Path,
        bound: &'static str,
        cancel: &AtomicBool,
    ) -> Result<MemberOutcome, String> {
        check_workspace_cancel(Some(cancel))?;
        crate::trace::trace!(
            "  index member {} limited: compile-time limit {bound}",
            path.display()
        );
        self.project_cache
            .limit_index_member_over_admission(key, path, bound);
        self.prove_unindexed(key, path, Unindexed::NotRecorded, cancel);
        Ok(MemberOutcome::Limited)
    }

    #[allow(clippy::too_many_arguments)]
    fn member_loaded(
        &self,
        key: &ContextKey,
        path: &Path,
        uri: &Url,
        loaded: Result<bool, String>,
        own_stamp: crate::workspace::PathStamp,
        reserve: bool,
        cancel: &AtomicBool,
    ) -> Extraction {
        match loaded {
            Err(error) if let Some(bound) = deterministic_admission_limit(&error) => {
                Extraction::OverAdmission(bound)
            }
            Err(error) if is_stable_load_refusal(&error) => refused_unless_vanished(path, error),
            Err(error) => Extraction::Failed(error),
            Ok(false) => match self.load_refusal.clone() {
                Some(reason) if let Some(bound) = deterministic_admission_limit(&reason) => {
                    Extraction::OverAdmission(bound)
                }
                Some(reason) if is_stable_load_refusal(&reason) => {
                    refused_unless_vanished(path, reason)
                }
                // No stable reason: the member stays pending (a file that
                // vanished is relisted by the membership instead).
                _ if path_stamp(path).is_none() => Extraction::Vanished,
                reason => Extraction::Deferred(
                    reason.unwrap_or_else(|| "the source could not be loaded".to_owned()),
                ),
            },
            Ok(true) => self.extract_loaded(key, path, uri, own_stamp, reserve, cancel),
        }
    }

    /// The facts of a loaded member, and its publication prepared when this
    /// view may publish: the one extraction pass serves the index and the
    /// store.
    fn extract_loaded(
        &self,
        key: &ContextKey,
        path: &Path,
        uri: &Url,
        own_stamp: crate::workspace::PathStamp,
        reserve: bool,
        cancel: &AtomicBool,
    ) -> Extraction {
        let facts = match self.source_facts(
            uri,
            &mut FactBudget::new(pascal_cache::MAX_ARTIFACT_CONTENT_BYTES),
            cancel,
        ) {
            Ok(facts) => facts,
            Err(FactError::Cancelled) => {
                return Extraction::Failed(CANCELLATION_MESSAGE.to_owned());
            }
            Err(error) => return Extraction::Refused(error.to_string()),
        };
        match facts.extracted.persistable {
            Ok(()) => {}
            Err(NotPersistable::Transient) => return Extraction::Limited,
            // An unsaved buffer is not what the disk holds; the index
            // describes saved files only.
            Err(NotPersistable::OverlayRoot | NotPersistable::OverlayInclude { .. }) => {
                return Extraction::Overlay;
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
        // Facts known over a fixed limit are never written: the batch
        // records them artifact-less without a prepared publication.
        let (prepared, unprovable) = if self.may_publish(key)
            && self.persistent_cache().is_enabled()
            && !self.known_over_limit(key, path, hash)
        {
            let prepared = self.prepare_from_facts(uri, facts, cancel).map(|artifact| {
                let transient = reserve.then(|| {
                    self.project_cache
                        .reserve_publication(artifact.transient_bytes())
                });
                PreparedPublication {
                    artifact,
                    transient,
                }
            });
            let unprovable = matches!(
                prepared,
                Err(super::persist::PersistError::Evidence(
                    super::evidence::EvidenceGap::Uncaptured
                ))
            )
            .then(|| {
                self.uncaptured_input(uri)
                    .unwrap_or(("unknown", String::new()))
            });
            (Some(prepared), unprovable)
        } else {
            (None, None)
        };
        Extraction::Facts(Box::new(ExtractedMember {
            member,
            stamps,
            hash,
            prepared,
            unprovable,
        }))
    }

    fn publish_member(
        &self,
        key: &ContextKey,
        path: &Path,
        extracted: ExtractedMember,
        cancel: &AtomicBool,
    ) -> MemberOutcome {
        let ExtractedMember {
            member,
            stamps,
            hash,
            prepared,
            unprovable,
        } = extracted;
        match self
            .project_cache
            .publish_index_member(key, path, member, stamps, hash)
        {
            PublishOutcome::Indexed => {
                #[cfg(test)]
                run_hook(&BEFORE_PERSIST, path);
                // These facts exceeded a fixed limit already: the write
                // would fail the same way.
                if self.known_over_limit(key, path, hash) {
                    self.record_over_limit(key, path, hash, cancel);
                    return MemberOutcome::Published { persisted: false };
                }
                let persisted = match self.persist_extracted(key, path, prepared, cancel) {
                    Ok(proof) => self.set_member_record(key, &proof),
                    Err(skip) => {
                        if !matches!(skip, PersistSkip::Unavailable) {
                            crate::trace::trace!(
                                "  index member {} not persisted: {skip:?}",
                                path.display()
                            );
                        }
                        // Another process's writes pass: the next batch
                        // proves the member again (reuse, else
                        // extraction), so no extracted member stays
                        // without its record and the snapshot can be
                        // sealed (D33, §6.3).
                        match skip {
                            PersistSkip::Store(
                                pascal_cache::Bypass::Busy | pascal_cache::Bypass::Recovering,
                            ) => self.project_cache.mark_index_stale(key, path),
                            // Only maintenance makes this room (the
                            // store woke it): the member waits for a
                            // batch that finds no pressure rather than
                            // being extracted into the same refusal.
                            // `LowDiskSpace` and this store's own
                            // `ScratchExhausted` are not pressure
                            // maintenance clears: memory-only.
                            PersistSkip::Store(
                                pascal_cache::Bypass::WalPressure
                                | pascal_cache::Bypass::OverBudget,
                            ) => self.project_cache.note_index_awaiting_room(key, path),
                            // The same inputs exceed the same fixed limit
                            // every time: the facts stay indexed here and
                            // an artifact-less record lists the member, so
                            // the snapshot seals and the next process
                            // retries it last instead of failing the same
                            // write in path order (l4d-kqbb, A1).
                            skip if skip.over_fixed_limit() => {
                                self.record_over_limit(key, path, hash, cancel);
                            }
                            // The session read an input its capture did
                            // not record: no evidence proves it to
                            // another process. Listed artifact-less and
                            // counted, never left memory-only, so the
                            // snapshot seals and the next process
                            // retries it last (13c-1, l4d-kqbb).
                            PersistSkip::Prepare(super::persist::PersistError::Evidence(
                                super::evidence::EvidenceGap::Uncaptured,
                            )) => self.record_unprovable(key, path, unprovable, cancel),
                            // Not even the publication pool had room: the
                            // member stays memory-only (a retryable miss,
                            // never a refusal record), counted with a gap.
                            PersistSkip::Memory => {
                                self.project_cache.note_index_publication_refused(key);
                            }
                            _ => {}
                        }
                        false
                    }
                };
                MemberOutcome::Published { persisted }
            }
            PublishOutcome::NotAMember => MemberOutcome::Skipped,
            PublishOutcome::Memory => MemberOutcome::Memory,
        }
    }

    fn known_over_limit(&self, key: &ContextKey, path: &Path, hash: u64) -> bool {
        self.contexts.get(key).is_some_and(|state| {
            self.project_cache.over_limit(
                path,
                hash,
                self.cache_context_key(&state.context).digest(),
            )
        })
    }

    /// `path`'s facts, indexed from the bytes hashed `hash`, exceed a fixed
    /// limit of the store: the same inputs fail the same way every time.
    /// The facts stay indexed in this process, an artifact-less record
    /// lists the member so the snapshot seals, and the next process
    /// retries it last (l4d-kqbb, A1); status reports it (`overLimit`).
    fn record_over_limit(&self, key: &ContextKey, path: &Path, hash: u64, cancel: &AtomicBool) {
        if let Some(state) = self.contexts.get(key) {
            self.project_cache.note_over_limit(
                path,
                hash,
                self.cache_context_key(&state.context).digest(),
            );
        }
        self.prove_unindexed(key, path, Unindexed::NotRecorded, cancel);
        self.project_cache.mark_index_over_limit(key, path);
    }

    /// `input` is what the extraction found its session read without a
    /// capture (`uncaptured_input`).
    fn record_unprovable(
        &self,
        key: &ContextKey,
        path: &Path,
        input: Option<(&'static str, String)>,
        cancel: &AtomicBool,
    ) {
        let (kind, input) = input.unwrap_or(("unknown", String::new()));
        crate::trace::trace!(
            "  index member {} unprovable: {kind} {input}",
            path.display()
        );
        self.prove_unindexed(key, path, Unindexed::NotRecorded, cancel);
        self.project_cache.mark_index_unprovable(key, path, kind);
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
                self.cache_context_key(&state.context).digest(),
            )
        })
    }

    /// Persist an extracted member: its artifact (with the evidence that
    /// proves it), then its record, in a baseline view whose context came
    /// from saved files. The transient bytes are reserved first; a refusal
    /// is no error, the next process extracts the member again (D41).
    fn persist_extracted(
        &self,
        key: &ContextKey,
        path: &Path,
        prepared: Option<Result<PreparedPublication, super::persist::PersistError>>,
        cancel: &AtomicBool,
    ) -> Result<ProvenRecord, PersistSkip> {
        if !self.may_publish(key) || !self.persistent_cache().is_enabled() {
            return Err(PersistSkip::Unavailable);
        }
        // The store was not enabled as the member was extracted: it is
        // memory-only, and proven once a batch finds the store enabled.
        let Some(prepared) = prepared else {
            self.project_cache.note_index_memory_only(key, path);
            return Err(PersistSkip::Unavailable);
        };
        let PreparedPublication {
            artifact,
            transient,
        } = prepared.map_err(PersistSkip::Prepare)?;
        self.publish_prepared(key, path, artifact, transient, cancel)
            .map(|(proof, _)| proof)
    }

    /// `persist_extracted` once the caller proved its view may publish
    /// `key`'s facts: a baseline view (`may_publish`), or the warm crawl's
    /// per-unit disk proof (`publish_crawled_unit`). Also returns the
    /// paths the published evidence names (`PreparedArtifact::input_paths`).
    fn publish_extracted(
        &self,
        key: &ContextKey,
        uri: &Url,
        source: SourceFacts,
        cancel: &AtomicBool,
    ) -> Result<(ProvenRecord, Vec<PathBuf>), PersistSkip> {
        let path = uri
            .to_file_path()
            .map_err(|()| PersistSkip::Prepare(super::persist::PersistError::NotIndexed))?;
        let prepared = self
            .prepare_from_facts(uri, source, cancel)
            .map_err(PersistSkip::Prepare)?;
        self.publish_prepared(key, &path, prepared, None, cancel)
    }

    /// Publish one prepared artifact and the record it proves for `path`;
    /// `transient` holds its transient bytes when a worker reserved them
    /// (`Some(None)`: refused), else they are reserved here.
    fn publish_prepared(
        &self,
        key: &ContextKey,
        path: &Path,
        prepared: super::persist::PreparedArtifact,
        transient: Option<Option<crate::project_cache::Reservation>>,
        cancel: &AtomicBool,
    ) -> Result<(ProvenRecord, Vec<PathBuf>), PersistSkip> {
        let eligible = self
            .eligible_indexed_member(
                key,
                &RecordInputs {
                    path,
                    facts: Some(&prepared.facts),
                    artifact: Some(prepared.artifact_id()),
                    member_evidence: prepared.member_evidence_id(),
                    origin: prepared.origin,
                    unindexed: None,
                },
            )
            .map_err(PersistSkip::Ineligible)?;
        let inputs = prepared.input_paths();
        // From the publication pool, not the parse budget the crawl's pins
        // and leases fill (13b-3, A2).
        let _transient = transient
            .unwrap_or_else(|| {
                self.project_cache
                    .reserve_publication(prepared.transient_bytes())
            })
            .ok_or(PersistSkip::Memory)?;
        let job = self
            .persistent_cache()
            .job()
            .ok_or(PersistSkip::Unavailable)?;
        let stopped = || job.is_cancelled() || cancel.load(Ordering::Relaxed);
        if stopped() {
            return Err(PersistSkip::Unavailable);
        }
        #[cfg(test)]
        if let Some(bypass) = self.persistent_cache().publish_fault_for_test(path) {
            job.observe(&bypass);
            return Err(PersistSkip::Store(bypass));
        }
        #[cfg(test)]
        let wal = WalGrowth::new(job.store());
        let published = prepared.publish(job.store());
        #[cfg(test)]
        let wal = wal.note(0);
        match published {
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
        let proven = publish_proven(job.store(), &eligible);
        #[cfg(test)]
        wal.note(1);
        proven.map(|proof| (proof, inputs)).map_err(|bypass| {
            job.observe(&bypass);
            PersistSkip::Store(bypass)
        })
    }

    /// R8-2/D42: publish a unit the warm crawl just indexed in the session
    /// view, so a first run need not wait for the crawl before it persists.
    /// Only a unit proven disk-only is published: read from disk and no
    /// open, overlaid or deleted root (1); facts with no buffer root or
    /// include (2); evidence and keys `prepare_from_facts` accepts (3); a
    /// context built from saved project files (4). The facts come from the
    /// crawl's own parse. Nothing enters the index; a failure leaves the
    /// unit unpublished and is never an error to the crawl.
    pub(super) fn publish_crawled_unit(
        &mut self,
        uri: &Url,
        revision: &pascal_core::SourceRevision,
        cancel: &AtomicBool,
    ) {
        // Considered once per job, before the proofs: a unit skipped for a
        // transient reason (a refused reservation, a store bypass,
        // cancellation) is not retried by the same crawl. The Full batch
        // extracts it, or a later crawl job publishes it.
        if !self.crawl_persists
            || !self.persistent_cache().is_enabled()
            || !self.crawl_considered.insert(uri.clone())
        {
            return;
        }
        let Some(key) = self.document_contexts.get(uri).cloned() else {
            return;
        };
        if !self.crawl_context_is_disk_only(&key) {
            self.crawl_private.store(true, Ordering::Relaxed);
            return;
        }
        if !matches!(revision, pascal_core::SourceRevision::Disk { .. })
            || self.root_is_session_private(uri)
        {
            return;
        }
        let Ok(path) = uri.to_file_path().map(absolute_path) else {
            return;
        };
        let Some(hash) = self.indexed_content_hashes.get(uri).copied() else {
            return;
        };
        let Some(context) = self
            .contexts
            .get(&key)
            .map(|state| self.cache_context_key(&state.context).digest())
        else {
            return;
        };
        // Already in the store: this process published it from the same
        // bytes under the same context (an earlier crawl job), or indexed it
        // so and proved its record.
        if self.project_cache.crawl_published(&path, hash, context)
            || self.project_cache.over_limit(&path, hash, context)
            || self.project_cache.index_member_is_proven(&path, hash)
        {
            return;
        }
        let source = match self.source_facts(
            uri,
            &mut FactBudget::new(pascal_cache::MAX_ARTIFACT_CONTENT_BYTES),
            cancel,
        ) {
            Ok(source) if source.extracted.persistable.is_ok() => source,
            _ => return,
        };
        match self.publish_extracted(&key, uri, source, cancel) {
            Ok((_, inputs)) => self
                .project_cache
                .note_published(&path, hash, context, &inputs),
            // No room even in the publication pool. The crawl publishes
            // nothing for the unit, and it stays pending in every context
            // index that has it as a member, so the next Full batch extracts
            // and publishes it. It stays considered: this job does not
            // prepare it again on each edge to it (13b-3, A2).
            Err(PersistSkip::Memory) => {
                self.project_cache.note_crawl_deferred(&path);
            }
            Err(skip) => {
                crate::trace::trace!("  crawl unit {} not persisted: {skip:?}", path.display());
                // No later crawl job tries the same write; the index batch
                // indexes the member and records it artifact-less.
                if skip.over_fixed_limit() {
                    self.project_cache.note_over_limit(&path, hash, context);
                }
            }
        }
    }

    fn crawl_context_is_disk_only(&mut self, key: &ContextKey) -> bool {
        if let Some(known) = self.crawl_disk_contexts.get(key) {
            return *known;
        }
        let disk_only = self.context_inputs_are_disk_only(key);
        self.crawl_disk_contexts.insert(key.clone(), disk_only);
        disk_only
    }

    /// Prove the record `inputs` derive for a member the index holds: found
    /// live in the store (`live` when it is the held snapshot's record the
    /// batch already opened), else published. Without a proof the member
    /// only delays the context's snapshot.
    fn prove_member(
        &self,
        key: &ContextKey,
        inputs: &RecordInputs<'_>,
        live: Option<pascal_cache::Digest>,
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
        let proof = match prove_existing(job.store(), &eligible, live) {
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
            None,
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
        context_key: &mut super::persist::ContextKeyMemo,
        cancel: &AtomicBool,
    ) -> ReuseOutcome {
        let stop = Arc::new(AtomicBool::new(false));
        match self.validate_reuse(key, path, context_key, &stop, cancel) {
            Validated::Outcome(outcome) => outcome,
            Validated::Hit(hit) => self.apply_reuse(key, path, own_stamp, hit, cancel),
        }
    }

    /// The store half of `reuse_index_member`: find the member's facts and
    /// prove them against current inputs. It changes no member's index
    /// state, so workers run it ahead of the batch (R8-1); the caller takes
    /// the member's stamp before, so it precedes every byte read (A7).
    fn validate_reuse(
        &self,
        key: &ContextKey,
        path: &Path,
        context_key: &mut super::persist::ContextKeyMemo,
        stop: &Arc<AtomicBool>,
        cancel: &AtomicBool,
    ) -> Validated {
        if !self.is_baseline()
            || !self
                .project_cache
                .read_index(key, |index, _| !index.reuse_tried(path))
                .unwrap_or(false)
        {
            return Validated::Outcome(ReuseOutcome::NoStore);
        }
        let Ok(uri) = Url::from_file_path(path) else {
            return Validated::Outcome(ReuseOutcome::Missed { absent: false });
        };
        let Some(job) = self.persistent_cache().job() else {
            return Validated::Outcome(ReuseOutcome::NoStore);
        };
        // `stop` ends the validation within: a parallel batch's trip (M5).
        let work = super::ReconciliationBudget::new(Arc::clone(stop));
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
                Ok(HintReuse::Retrying) => return Validated::Outcome(ReuseOutcome::Retrying),
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
                    return Validated::Outcome(ReuseOutcome::Retry);
                }
            }
        }
        let result = hinted.unwrap_or_else(|| {
            self.reuse_in_context_with(
                &uri,
                key,
                context_key,
                job.store(),
                &work,
                pascal_cache::MAX_ARTIFACT_CONTENT_BYTES,
                job.cancel_flag(),
            )
        });
        job.record_reuse(&result);
        if cancel.load(Ordering::Relaxed) {
            return Validated::Outcome(ReuseOutcome::Cancelled);
        }
        let (facts, reads, artifact, evidence, include_stamps, root_hash, record) = match result {
            Ok(Reuse::Hit {
                facts,
                via,
                reads,
                artifact,
                evidence,
                include_stamps,
                root_hash,
            }) => {
                let record = match via {
                    super::persist::ReuseVia::Snapshot { record } => record,
                    _ => None,
                };
                (
                    facts,
                    reads,
                    artifact,
                    evidence,
                    include_stamps,
                    root_hash,
                    record,
                )
            }
            Ok(Reuse::Miss(ReuseMiss::Unproven(_))) => {
                return Validated::Outcome(ReuseOutcome::Retry);
            }
            Ok(Reuse::Miss(ReuseMiss::Store(pascal_cache::Miss::Absent))) => {
                return Validated::Outcome(ReuseOutcome::Missed { absent: true });
            }
            Ok(Reuse::Miss(ReuseMiss::Stale(_) | ReuseMiss::Store(_) | ReuseMiss::Open)) => {
                return Validated::Outcome(ReuseOutcome::Missed { absent: false });
            }
            // `record_reuse` observed the bypass.
            Err(super::persist::PersistError::Store(_)) => {
                return Validated::Outcome(ReuseOutcome::Retry);
            }
            Err(_) => return Validated::Outcome(ReuseOutcome::Missed { absent: false }),
        };
        job.store().hint_used(match evidence {
            Some(id) => Hint::Evidence(id),
            None => Hint::Artifact(artifact),
        });
        drop(job);
        Validated::Hit(ReuseHit {
            facts,
            reads,
            artifact,
            evidence,
            include_stamps,
            root_hash,
            record,
        })
    }

    /// The index half of `reuse_index_member`, in member order: index the
    /// proven facts and prove the member's record; the facts drop here.
    fn apply_reuse(
        &self,
        key: &ContextKey,
        path: &Path,
        own_stamp: &PathStamp,
        hit: ReuseHit,
        cancel: &AtomicBool,
    ) -> ReuseOutcome {
        let ReuseHit {
            facts,
            reads,
            artifact,
            evidence,
            include_stamps,
            root_hash,
            record,
        } = hit;
        self.project_cache
            .add_index_validation_reads(key, u64::try_from(reads).unwrap_or(u64::MAX));
        let published = timed(ReusePhase::Insert, || {
            let (slots, stamps) = reused_slots(path, own_stamp, &facts.facts, &include_stamps);
            let member = MemberFacts::from_facts(path, &facts.facts, slots);
            self.project_cache
                .publish_reused_index_member(key, path, member, stamps, root_hash)
        });
        // The record proves the member only while the snapshot that
        // listed it is still held (a dropped hint released its hold).
        let live = record
            .filter(|(snapshot, _)| {
                self.project_cache
                    .baseline_hint(key)
                    .is_some_and(|hint| hint.held() && hint.header().id() == *snapshot)
            })
            .map(|(_, record)| record);
        let outcome = match published {
            PublishOutcome::Indexed => {
                if let Some(state) = self.contexts.get(key) {
                    timed(ReusePhase::Prove, || {
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
                            live,
                            cancel,
                        );
                    });
                }
                ReuseOutcome::Reused
            }
            PublishOutcome::Memory => ReuseOutcome::Memory,
            PublishOutcome::NotAMember => ReuseOutcome::Missed { absent: false },
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
        self.index_target_for(anchor, cancel).map(|(key, _)| key)
    }

    /// The context `anchor` names and the anchor that names it once: a
    /// document whose context has a project file is indexed under its
    /// project's anchor (A8), the key searches use (`owner_target`). Its
    /// owner may hold an older resolution of the same project, for instance
    /// one made before `pascal/selectProject` selected it. When the project
    /// no longer resolves, the document's own context stays.
    fn index_target_for(
        &mut self,
        anchor: &IndexAnchor,
        cancel: &AtomicBool,
    ) -> Result<(ContextKey, IndexAnchor), String> {
        let uri = match anchor {
            IndexAnchor::Project(descriptor) => {
                return Ok((
                    self.project_anchor_context(descriptor, cancel)?,
                    anchor.clone(),
                ));
            }
            IndexAnchor::Document(uri) => uri,
        };
        let key = snapshot_context_for_uri(self, uri, cancel, false)?;
        let Some(project) = self
            .contexts
            .get(&key)
            .and_then(|state| state.context.project_file().map(Path::to_path_buf))
        else {
            return Ok((key, anchor.clone()));
        };
        match self.project_anchor_context(&project, cancel) {
            Ok(project_key) => Ok((project_key, IndexAnchor::Project(project))),
            Err(error) if error == CANCELLATION_MESSAGE => Err(error),
            Err(error) => {
                crate::trace::trace!("  index anchor {uri}: its project did not resolve: {error}");
                Ok((key, anchor.clone()))
            }
        }
    }

    /// The context of the Project anchor `descriptor`, resolved and
    /// retained. An index stored under an earlier resolution of the same
    /// context, made under another runtime selection, moves to its key
    /// (A8).
    fn project_anchor_context(
        &mut self,
        descriptor: &Path,
        cancel: &AtomicBool,
    ) -> Result<ContextKey, String> {
        check_workspace_cancel(Some(cancel))?;
        let uri = Url::from_file_path(descriptor)
            .map_err(|()| format!("could not create a file URI for {descriptor:?}"))?;
        let roots = self.workspace_root_paths();
        let mut options = self.project_options();
        options.project_file = Some(descriptor.to_path_buf());
        let (key, project) = self.readonly_context_for_uri(&uri, descriptor, &roots, &options)?;
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
        if self.project_cache.index_moved_by_selection(&key)
            && let Some(state) = self.contexts.get(&key)
            && self
                .project_cache
                .rekey_index_selection(&key, self.cache_context_key(&state.context).digest())
        {
            crate::trace::trace!("  index of {descriptor:?} moved to the current selection");
        }
        Ok(key)
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
            context: Some(CacheContextKey::new(&context).digest()),
            ..IndexMembership::default()
        };
        let mut paths = Vec::new();
        if context.project_file().is_some() {
            let mut directories = Vec::new();
            if let Some(main) = context.main_source() {
                paths.push(absolute_path(main.to_path_buf()));
                directories.extend(main.parent().map(Path::to_path_buf));
            }
            // `explicit_units` is a `HashMap`; the order-sensitive discovery
            // scope (`baseline::discovery_scope`) needs the same directory
            // order in every process, so the units go by path key.
            let mut explicit = context
                .explicit_units()
                .into_values()
                .flatten()
                .collect::<Vec<_>>();
            explicit.sort_by_cached_key(|unit| path_key(unit));
            for unit in explicit {
                directories.extend(unit.parent().map(Path::to_path_buf));
                paths.push(absolute_path(unit));
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
    pub(super) fn recursive_scope(&self, root: &Path) -> CatalogueScope {
        let root = absolute_path(root.to_path_buf());
        let matcher = self
            .roots
            .iter()
            .find(|workspace_root| path_starts_with_ci(&root, &workspace_root.path))
            .map(|workspace_root| workspace_root.excludes.clone())
            .or_else(|| self.cache_only_excludes(&root));
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
                let claimable = self
                    .document_owners
                    .get(&uri)
                    .is_some_and(|known| known.claimed && known.key == owner)
                    || self
                        .contexts
                        .get(&owner)
                        .is_some_and(|state| state.context.project_file().is_none());
                // N4: a claim decided against an older claimant list (a
                // request built before a close or a selection) is not
                // recorded over the owners the main workspace reassigned.
                if !claimable
                    || self.claimant_list.generation == self.project_cache.claim_generation()
                {
                    self.project_cache.remember_owner_claimable(
                        directory.to_path_buf(),
                        owner.clone(),
                        claimable,
                    );
                }
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
                None => match self.index_target_for(&IndexAnchor::Document(uri.clone()), cancel) {
                    Ok((key, anchor)) => {
                        let project = self
                            .contexts
                            .get(&key)
                            .and_then(|state| state.context.project_file().map(Path::to_path_buf));
                        // The document resolved through its project's anchor:
                        // `owner_target` need not resolve it again.
                        if let IndexAnchor::Project(project) = &anchor {
                            projects.insert(path_key(project), Some(key.clone()));
                        }
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
        let mut ranking = match selection {
            Selection::Candidates(Some(query)) => Some(UnverifiedRanking::new(query)),
            _ => None,
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
                if let Some(ranking) = ranking.as_mut() {
                    ranking.rank(
                        &mut paths,
                        unverified_left,
                        &|path: &Path| {
                            let key = path_key(path);
                            open.contains(&key) || taken.contains(&key) || deleted(path)
                        },
                        cancel,
                    );
                }
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
            if searched && let Some(ranking) = ranking.as_mut() {
                ranking.rank(
                    &mut unverified,
                    unverified_left,
                    &|path: &Path| {
                        let key = path_key(path);
                        open.contains(&key) || taken.contains(&key) || deleted(path)
                    },
                    cancel,
                );
            }

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
    pub(crate) fn configured_project_path(&self) -> Option<PathBuf> {
        let project = self.configured_project_file()?;
        if project.is_absolute() {
            return Some(project.to_path_buf());
        }
        // One match only: a project found under several roots is ambiguous,
        // as discovery treats it.
        let mut found = self
            .roots
            .iter()
            .map(|root| absolute_path(root.path.join(project)))
            .filter(|candidate| candidate.is_file());
        let first = found.next()?;
        found
            .all(|other| path_key(&other) == path_key(&first))
            .then_some(first)
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

/// The bytes one references request may read to rank its unverified
/// members (l4d-jeh4); members past it keep path order behind the ranked.
const MAX_UNVERIFIED_RANKING_BYTES: u64 = 128 * 1024 * 1024;
const MAX_UNVERIFIED_RANKING_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// What one request has left for ranking unverified members.
struct UnverifiedRanking {
    names: Vec<String>,
    units: Vec<String>,
    bytes_left: u64,
}

impl UnverifiedRanking {
    fn new(query: &CandidateQuery) -> Self {
        let tail = |name: &str| {
            crate::navigation::canonical_name(name)
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .to_owned()
        };
        let mut names = query
            .names
            .iter()
            .map(|name| tail(name))
            .collect::<Vec<_>>();
        let mut units = query
            .top_level_units
            .iter()
            .flatten()
            .flat_map(|unit| crate::workspace_index::import_names_for(unit, &query.aliases))
            .map(|name| tail(&name))
            .collect::<Vec<_>>();
        for list in [&mut names, &mut units] {
            list.retain(|name| !name.is_empty());
            list.sort();
            list.dedup();
        }
        Self {
            names,
            units,
            bytes_left: MAX_UNVERIFIED_RANKING_BYTES,
        }
    }

    /// Orders `paths` (in path order) so the members likely to hold an
    /// occurrence come first: those whose text spells a name and, for
    /// top-level members, a name of a declaring unit (an importer); then
    /// those spelling a name; then the unread; then the rest. Only the
    /// order changes, never the set, so the bound alone decides what is
    /// left out. Nothing is read when `left` already covers every path.
    /// Paths `unread` selects (open, deleted or already taken) do not use
    /// the bound, so they are neither read nor counted.
    ///
    /// The scan is a byte search over the raw file: a UTF-16 file, or an
    /// importer spelling the name only through an include, ranks last.
    fn rank(
        &mut self,
        paths: &mut Vec<PathBuf>,
        left: usize,
        unread: &dyn Fn(&Path) -> bool,
        cancel: &AtomicBool,
    ) {
        if self.names.is_empty() || paths.iter().filter(|path| !unread(path)).count() <= left {
            return;
        }
        let started = std::time::Instant::now();
        let mut likely = 0usize;
        let mut tiers = Vec::with_capacity(paths.len());
        for path in paths.iter() {
            let tier = if unread(path)
                || likely >= left
                || cancel.load(std::sync::atomic::Ordering::Relaxed)
            {
                2
            } else {
                match self.read(path) {
                    None => 2,
                    Some(text) => {
                        let named = self
                            .names
                            .iter()
                            .any(|name| crate::navigation::text_mentions_name(&text, name));
                        let imports = self.units.is_empty()
                            || self
                                .units
                                .iter()
                                .any(|unit| crate::navigation::text_mentions_name(&text, unit));
                        match (named, imports) {
                            (true, true) => 0,
                            (true, false) => 1,
                            (false, _) => 3,
                        }
                    }
                }
            };
            if tier == 0 {
                likely += 1;
            }
            tiers.push(tier);
        }
        let mut ranked = std::mem::take(paths)
            .into_iter()
            .zip(tiers)
            .collect::<Vec<_>>();
        ranked.sort_by_key(|(_, tier)| *tier);
        crate::trace::trace!(
            "  unverified ranking: {} members, {likely} likely, bound {left}, {:.1} ms",
            ranked.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        paths.extend(ranked.into_iter().map(|(path, _)| path));
    }

    fn read(&mut self, path: &Path) -> Option<Vec<u8>> {
        use std::io::Read;
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        if len > MAX_UNVERIFIED_RANKING_FILE_BYTES || len > self.bytes_left {
            return None;
        }
        self.bytes_left -= len;
        let mut text = Vec::with_capacity(len as usize);
        file.take(len).read_to_end(&mut text).ok()?;
        Some(text)
    }
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

    /// l4d-jeh4 fix round 1: a path the bound will not read (taken, open
    /// or deleted) is neither read nor counted as a likely member, so the
    /// ranking does not stop before the bound holds a likely member.
    #[test]
    fn unverified_ranking_ignores_paths_the_bound_does_not_read() {
        let temp = tempfile::tempdir().unwrap();
        let named =
            "unit X;\ninterface\nuses Shared;\nconst Y = SharedValue;\nimplementation\nend.\n";
        let a = temp.path().join("A.pas");
        let b = temp.path().join("B.pas");
        let c = temp.path().join("C.pas");
        write(&a, named);
        write(&b, "unit B;\ninterface\nimplementation\nend.\n");
        write(&c, named);
        let mut ranking = UnverifiedRanking::new(&CandidateQuery {
            names: vec!["sharedvalue".to_owned()],
            top_level_units: Some(vec!["shared".to_owned()]),
            aliases: Vec::new(),
        });
        let mut paths = vec![a.clone(), b.clone(), c.clone()];
        let taken = path_key(&a);
        ranking.rank(
            &mut paths,
            1,
            &|path: &Path| path_key(path) == taken,
            &AtomicBool::new(false),
        );
        let c_at = paths.iter().position(|path| *path == c).unwrap();
        let b_at = paths.iter().position(|path| *path == b).unwrap();
        assert!(c_at < b_at, "{paths:?}");
    }

    #[test]
    fn text_mentions_name_edges() {
        use crate::navigation::text_mentions_name;
        assert!(text_mentions_name(b"", ""));
        assert!(text_mentions_name(b"abc", ""));
        assert!(!text_mentions_name(b"foo", "foobar"));
        assert!(!text_mentions_name(b"", "a"));
        assert!(text_mentions_name(b"x := FOO", "foo"));
        assert!(text_mentions_name(b"Foo", "foo"));
        assert!(text_mentions_name(b"&fOo;", "foo"));
        assert!(!text_mentions_name(b"x := FO", "foo"));
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

    /// `<DCCReference>` items for `count` units `d<n>/E<n>.pas`, each in its
    /// own directory outside every search path. The names count down while
    /// the paths count up, so neither name nor declaration order is sorted
    /// by path.
    pub(in crate::workspace) fn explicit_unit_items(root: &Path, count: usize) -> String {
        (0..count)
            .rev()
            .map(|index| {
                let path = format!("d{index}/E{}.pas", count - index);
                named_unit(&root.join(&path), &format!("Q{index}"));
                format!("<DCCReference Include=\"{path}\"/>")
            })
            .collect()
    }

    /// 13e-1: the discovery-scope digest is order-sensitive, so the
    /// membership's directory order must not come from a `HashMap`: the
    /// explicit units' directories follow the main source's, sorted by
    /// path key, and every computation, in any process, gives the same
    /// order.
    #[test]
    fn explicit_unit_directories_are_scanned_in_path_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        unit(&root.join("lib/A.pas"));
        let items = explicit_unit_items(root, 8);
        let (mut workspace, key) = project_workspace(
            root,
            "<DCC_UnitSearchPath>lib</DCC_UnitSearchPath>",
            &items,
            WorkspaceOptions::default(),
        );
        let roots = |found: &IndexMembership| {
            found
                .scopes
                .iter()
                .map(|(scope, _)| scope.root.clone())
                .collect::<Vec<_>>()
        };
        let mut expected = vec![root.to_path_buf()];
        let mut explicit = (0..8)
            .map(|index| root.join(format!("d{index}")))
            .collect::<Vec<_>>();
        explicit.sort_by_cached_key(|path| path_key(path));
        expected.extend(explicit);
        expected.push(root.join("lib"));
        let first = roots(&membership(&mut workspace, &key));
        assert_eq!(first, expected);
        assert_eq!(roots(&membership(&mut workspace, &key)), first);
        let (mut fresh, fresh_key) = project_workspace(
            root,
            "<DCC_UnitSearchPath>lib</DCC_UnitSearchPath>",
            &items,
            WorkspaceOptions::default(),
        );
        assert_eq!(roots(&membership(&mut fresh, &fresh_key)), first);
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

    /// A unit that reads an include, so its load runs a compile-time
    /// session (the work a member deadline bounds).
    pub(in crate::workspace) fn session_unit(path: &Path) {
        write(&path.with_file_name("x.inc"), "const X = 1;\n");
        let unit = path.file_stem().unwrap().to_string_lossy().into_owned();
        write(
            path,
            &format!("unit {unit};\ninterface\n{{$I x.inc}}\nimplementation\nend.\n"),
        );
    }

    /// 13b-5 (A4): a member whose extraction passes its deadline is aborted
    /// through its session's control and limited. The batch goes on, the
    /// other members are not affected, the context completes and reports
    /// the member as a gap, and no later batch of this process extracts it
    /// again.
    #[test]
    fn a_member_past_its_extraction_deadline_is_limited_and_the_batch_continues() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 4, WorkspaceOptions::default());
        let slow = root.join("lib/U0.pas");
        session_unit(&slow);
        // A normal member with a session of its own keeps the default
        // deadline.
        let normal = root.join("lib/U1.pas");
        session_unit(&normal);
        pascal_project::backdate_tree_for_test(root);
        let _slow = slow_member_for_test(&slow);
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!(
            (
                progress.indexed,
                progress.limited,
                progress.deferred,
                progress.refused
            ),
            (3, 1, 0, 0),
            "{progress:?}"
        );
        assert!(progress.complete, "{progress:?}");
        assert_eq!(state(&workspace, &key, &slow), Some(MemberState::Limited));
        for path in members(&workspace, &key)
            .into_iter()
            .filter(|path| *path != slow)
        {
            assert_eq!(
                state(&workspace, &key, &path),
                Some(MemberState::Fresh),
                "{path:?}"
            );
        }
        let status = workspace.project_cache.index_status(&|_| false);
        assert_eq!(status[0].counts.limited, 1, "{:?}", status[0]);
        assert_eq!(status[0].counts.over_deadline, 1, "{:?}", status[0]);
        assert!(
            status[0]
                .gaps
                .iter()
                .any(|gap| gap.starts_with("1 units took longer than")),
            "{:?}",
            status[0]
        );
        assert!(
            !status[0]
                .gaps
                .iter()
                .any(|gap| gap.contains("exceed the index limits")),
            "{:?}",
            status[0]
        );
        let json = crate::project_cache::index_status_json(&status, Default::default());
        assert_eq!(json["contexts"][0]["overDeadline"], 1, "{json}");
        let again = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!((again.indexed, again.limited, again.pending), (0, 0, 0));
        // A change makes it a pending member again, extracted under a
        // fresh deadline.
        workspace.project_cache.invalidate_path(&slow);
        assert_eq!(state(&workspace, &key, &slow), Some(MemberState::Stale));
        let status = workspace.project_cache.index_status(&|_| false);
        assert_eq!(status[0].counts.over_deadline, 0, "{:?}", status[0]);
    }

    /// 13b-5 (A4): a member's compile-time session does not copy the
    /// workspace's document owners, which grow with every member a batch
    /// loads, so a member's cost does not grow with the members before it.
    #[test]
    fn a_member_session_copies_no_document_owners() {
        const COUNT: usize = 41;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) =
            indexed_project(root, COUNT, WorkspaceOptions::default());
        for index in 0..COUNT - 1 {
            session_unit(&root.join(format!("lib/U{index}.pas")));
        }
        pascal_project::backdate_tree_for_test(root);
        super::super::rename::take_owners_copied_for_test();
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        let copied = super::super::rename::take_owners_copied_for_test();
        assert_eq!(progress.indexed, COUNT, "{progress:?}");
        assert!(members(&workspace, &key).len() == COUNT);
        // Quadratic before: each session copied the owners of every member
        // loaded before it.
        assert!(copied <= COUNT, "{copied} owner entries copied");
    }

    /// 13b-5 (A4): cancellation of the batch (interactive work, a new
    /// generation) during a member whose deadline also passed stops the
    /// batch; the member stays pending for the next batch, never limited.
    #[test]
    fn a_cancelled_slow_member_stays_pending_rather_than_limited() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 3, WorkspaceOptions::default());
        let slow = root.join("lib/U0.pas");
        session_unit(&slow);
        pascal_project::backdate_tree_for_test(root);
        let _slow = slow_member_for_test(&slow);
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let (flag, target) = (std::sync::Arc::clone(&cancel), slow.clone());
        set_after_load_hook(Some(Box::new(move |path| {
            if path == target {
                flag.store(true, Ordering::Relaxed);
            }
        })));
        let result =
            workspace.advance_workspace_index(&anchor, IndexBound::all(), &mut || Ok(()), &cancel);
        set_after_load_hook(None);
        assert_eq!(result.unwrap_err(), CANCELLATION_MESSAGE);
        assert_eq!(state(&workspace, &key, &slow), Some(MemberState::Unindexed));
        let resumed = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!(resumed.limited, 1, "{resumed:?}");
        assert!(resumed.complete, "{resumed:?}");
        assert_eq!(state(&workspace, &key, &slow), Some(MemberState::Limited));
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

    /// A unit whose compile-time session asks whether a name is declared,
    /// so it compiles the interface of the unit it uses, `BigProvider`,
    /// whose one declaration is longer than the session's
    /// `max_pending_bytes` (1 MiB): the same inputs fail its admission on
    /// every attempt. `BigProvider` itself is a member, limited by its own
    /// incomplete session.
    pub(in crate::workspace) fn over_pending_bytes_unit(path: &Path) {
        write(&path.with_file_name("x.inc"), "const X = 1;\n");
        write(
            &path.with_file_name("BigProvider.pas"),
            &format!(
                "unit BigProvider;\ninterface\n{{$I x.inc}}\nconst S = '{}';\nimplementation\nend.\n",
                "x".repeat(1_100_000)
            ),
        );
        let unit = path.file_stem().unwrap().to_string_lossy().into_owned();
        write(
            path,
            &format!(
                "unit {unit};\ninterface\nuses BigProvider;\n{{$I x.inc}}\n{{$IF Declared(Zed)}}const Y = 1;{{$IFEND}}\nimplementation\nend.\n"
            ),
        );
    }

    /// 13d-1: a member whose compile-time admission refuses a fixed bound
    /// (`max_pending_bytes`) is limited after one attempt, not deferred:
    /// the same inputs refuse again, so retrying it in this process cannot
    /// pass. The context completes, a gap names the bound, and no later
    /// batch of this process extracts it again.
    #[test]
    fn a_member_over_a_compile_time_admission_bound_is_limited_after_one_attempt() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 4, WorkspaceOptions::default());
        let over = root.join("lib/U0.pas");
        over_pending_bytes_unit(&over);
        pascal_project::backdate_tree_for_test(root);
        let progress = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!(
            (
                progress.indexed,
                progress.limited,
                progress.deferred,
                progress.refused
            ),
            (3, 2, 0, 0),
            "{progress:?}"
        );
        assert!(
            progress.complete && !progress.worth_requeue(),
            "{progress:?}"
        );
        assert_eq!(state(&workspace, &key, &over), Some(MemberState::Limited));
        assert_eq!(
            state(&workspace, &key, &root.join("lib/BigProvider.pas")),
            Some(MemberState::Limited)
        );
        let status = workspace.project_cache.index_status(&|_| false);
        assert_eq!(
            (
                status[0].counts.limited,
                status[0].counts.deferred,
                status[0].counts.over_deadline,
                status[0].counts.over_admission
            ),
            (2, 0, 0, 1),
            "{:?}",
            status[0]
        );
        assert!(status[0].complete, "{:?}", status[0]);
        assert!(
            status[0].gaps.iter().any(|gap| gap
                == "1 units exceed a compile-time limit (first: max_pending_bytes) and are not indexed; the next start retries them last"),
            "{:?}",
            status[0]
        );
        assert!(
            !status[0]
                .gaps
                .iter()
                .any(|gap| gap.contains("retried later")),
            "{:?}",
            status[0]
        );
        // `BigProvider`'s own limit is the other limited member.
        assert!(
            status[0]
                .gaps
                .contains(&"1 units exceed the index limits and are not indexed".to_owned()),
            "{:?}",
            status[0]
        );
        let json = crate::project_cache::index_status_json(&status, Default::default());
        assert_eq!(json["contexts"][0]["overAdmission"], 1, "{json}");
        let again = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!(
            (again.indexed, again.limited, again.deferred, again.pending),
            (0, 0, 0, 0),
            "{again:?}"
        );
        // A change makes it pending again; the gap goes with it.
        workspace.project_cache.invalidate_path(&over);
        let status = workspace.project_cache.index_status(&|_| false);
        assert!(
            !status[0]
                .gaps
                .iter()
                .any(|gap| gap.contains("compile-time limit")),
            "{:?}",
            status[0]
        );
    }

    /// 13d-1: a deferral whose cause may pass (here an injected load
    /// error, as a memory budget or a busy store gives) keeps today's
    /// behaviour: the member stays deferred and a later batch retries it.
    #[test]
    fn a_transient_deferral_stays_deferred_and_is_retried() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, key, anchor) = indexed_project(root, 3, WorkspaceOptions::default());
        let failing = root.join("lib/U0.pas");
        set_member_error(Some(failing.clone()));
        let first = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!(
            (first.indexed, first.deferred, first.limited),
            (2, 1, 0),
            "{first:?}"
        );
        assert!(!first.complete, "{first:?}");
        let status = workspace.project_cache.index_status(&|_| false);
        assert_eq!(
            (status[0].counts.deferred, status[0].counts.limited),
            (1, 0),
            "{:?}",
            status[0]
        );
        assert!(!status[0].complete, "{:?}", status[0]);
        set_member_error(None);
        let second = advance(&mut workspace, &anchor, IndexBound::all());
        assert_eq!(
            (second.indexed, second.deferred, second.pending),
            (1, 0, 0),
            "{second:?}"
        );
        assert!(second.complete, "{second:?}");
        assert_eq!(state(&workspace, &key, &failing), Some(MemberState::Fresh));
    }

    /// 13d-1: only a named fixed compile-time bound is deterministic for
    /// a member's inputs; retained bytes depend on what else the resolver
    /// holds, and a budget or cancellation may pass.
    #[test]
    fn only_fixed_compile_time_admission_bounds_are_deterministic() {
        let message = |name: &str| {
            format!(
                "include expansion failed: invalid resolver request: compile-time admission: Limit {{ name: \"{name}\", maximum: 1048576 }}"
            )
        };
        assert_eq!(
            deterministic_admission_limit(&message("max_pending_bytes")),
            Some("max_pending_bytes")
        );
        assert_eq!(
            deterministic_admission_limit(&message("max_branch_states")),
            Some("max_branch_states")
        );
        for fixed in ["max_expression_bytes", "max_expression_tokens"] {
            assert_eq!(deterministic_admission_limit(&message(fixed)), Some(fixed));
        }
        assert_eq!(
            deterministic_admission_limit(&message("max_retained_bytes")),
            None
        );
        assert_eq!(deterministic_admission_limit(&message("unknown")), None);
        assert_eq!(
            deterministic_admission_limit("the source could not be loaded"),
            None
        );
        assert_eq!(deterministic_admission_limit(CANCELLATION_MESSAGE), None);
        assert_eq!(
            deterministic_admission_limit(
                "compile-time admission: Limit { name: \"max_pending_bytes\", maximum: 1 } over the memory budget"
            ),
            None
        );
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

    /// `App.dproj` (main source `App.dpr`, search path `lib`) beside
    /// `Other.dproj` (main source `Other.dpr`, no search path), with
    /// `lib/U0..U2.pas`; `projectFile` is the absolute `App.dproj`.
    fn selectable_projects(root: &Path) -> (Workspace, PathBuf) {
        let app = project(root, "<DCC_UnitSearchPath>lib</DCC_UnitSearchPath>", "");
        let other = root.join("Other.dproj");
        write(
            &other,
            "<Project><PropertyGroup><MainSource>Other.dpr</MainSource></PropertyGroup><ItemGroup></ItemGroup></Project>",
        );
        write(&root.join("Other.dpr"), "program Other; begin end.\n");
        for index in 0..3 {
            named_unit(
                &root.join(format!("lib/U{index}.pas")),
                &format!("P{index}"),
            );
        }
        pascal_project::backdate_tree_for_test(root);
        let options = WorkspaceOptions {
            project_file: Some(app.clone()),
            ..WorkspaceOptions::default()
        };
        (workspace(root, options), app)
    }

    fn open_from_disk(workspace: &mut Workspace, path: &Path) -> Url {
        let uri = Url::from_file_path(path).unwrap();
        workspace
            .open_document(uri.clone(), fs::read_to_string(path).unwrap(), 1)
            .unwrap();
        uri
    }

    fn select(workspace: &mut Workspace, document: &Path, project: &Path) {
        workspace
            .select_project(
                &Url::from_file_path(document).unwrap(),
                Some(&Url::from_file_path(project).unwrap()),
            )
            .expect("select project");
    }

    fn status(workspace: &Workspace) -> Vec<crate::project_cache::IndexStatusEntry> {
        workspace.project_cache.index_status(&|_| false)
    }

    /// A8 (13b-6): an open document's owner resolved from the absolute
    /// `projectFile` before `pascal/selectProject` selects that project has
    /// no selection, the Project anchor's has one. Both anchors still name
    /// one context: one key, one index, one status entry, every member
    /// extracted once, under the persisted key the owner's context gives.
    #[test]
    fn a_selected_configured_project_and_its_documents_share_one_context() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, app) = selectable_projects(root);
        let document = open_from_disk(&mut workspace, &root.join("lib/U0.pas"));
        let owner_context = workspace.document_owners[&document].state.context.clone();
        select(&mut workspace, &root.join("App.dpr"), &app);

        let project = IndexAnchor::Project(app.clone());
        let by_document = context(&mut workspace, IndexAnchor::Document(document.clone()));
        let by_project = context(&mut workspace, project.clone());
        assert_eq!(by_document, by_project);

        let first = advance(&mut workspace, &project, IndexBound::all());
        assert!(first.complete, "{first:?}");
        let second = advance(
            &mut workspace,
            &IndexAnchor::Document(document),
            IndexBound::all(),
        );
        assert_eq!(second.key.as_ref(), Some(&by_project));
        assert_eq!((second.indexed, second.reused), (0, 0), "{second:?}");
        let entries = status(&workspace);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(entries[0].anchor_is_project, "{entries:?}");
        let persisted = workspace
            .project_cache
            .read_index(&by_project, |_, membership| membership.context)
            .flatten();
        assert_eq!(
            persisted,
            Some(CacheContextKey::new(&owner_context).digest())
        );
    }

    /// A8 (13b-6): the Project anchor indexed before the selection keeps its
    /// index once the same project is selected: the index moves to the key
    /// the selection gives, nothing is indexed again.
    #[test]
    fn selecting_the_configured_project_later_keeps_its_index() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, app) = selectable_projects(root);
        let project = IndexAnchor::Project(app.clone());
        let before = context(&mut workspace, project.clone());
        let first = advance(&mut workspace, &project, IndexBound::all());
        assert!(first.complete, "{first:?}");
        select(&mut workspace, &root.join("App.dpr"), &app);
        let after = context(&mut workspace, project.clone());
        assert!(
            before.differs_only_in_selection(&after),
            "{before:?} {after:?}"
        );
        assert!(!workspace.project_cache.holds_index(&before));
        let again = advance(&mut workspace, &project, IndexBound::all());
        assert_eq!(again.key.as_ref(), Some(&after));
        assert_eq!((again.indexed, again.reused), (0, 0), "{again:?}");
        assert!(again.complete, "{again:?}");
        assert_eq!(status(&workspace).len(), 1);
    }

    /// A8 (13b-6 fix round 1): a search re-keys the Project anchor's index
    /// for a new selection while a batch still runs on the old key; the
    /// batch's closing membership must not store the old key again.
    #[test]
    fn a_batch_on_a_rekeyed_index_does_not_bring_the_old_key_back() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let (mut workspace, app) = selectable_projects(&root);
        let project = IndexAnchor::Project(app.clone());
        let old = context(&mut workspace, project.clone());
        let selected = old.clone().with_selection_for_test(&root, &app);
        let cache = workspace.project_cache.clone();
        let hook_old = old.clone();
        let hook_selected = selected.clone();
        set_after_reuse_hook(Some(Box::new(move |_| {
            if let Some(Some(context)) =
                cache.read_index(&hook_old, |_, membership| membership.context)
            {
                assert!(cache.rekey_index_selection(&hook_selected, context));
            }
        })));
        let progress = advance(&mut workspace, &project, IndexBound::all());
        set_after_reuse_hook(None);
        assert_eq!(progress.key.as_ref(), Some(&old));
        assert!(!workspace.project_cache.holds_index(&old));
        assert!(workspace.project_cache.holds_index(&selected));
        assert_eq!(status(&workspace).len(), 1);
    }

    /// A8 (13b-6 fix round 1): a project index whose membership was computed
    /// for another persisted context is not moved to the new selection.
    #[test]
    fn a_rekey_needs_the_same_persisted_context() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, app) = selectable_projects(root);
        let project = IndexAnchor::Project(app.clone());
        let before = context(&mut workspace, project.clone());
        advance(&mut workspace, &project, IndexBound::all());
        workspace
            .project_cache
            .with_index_membership_context_for_test(
                &before,
                pascal_cache::Digest::from_bytes([9; 32]),
            );
        select(&mut workspace, &root.join("App.dpr"), &app);
        let after = context(&mut workspace, project);
        assert!(before.differs_only_in_selection(&after));
        assert!(workspace.project_cache.holds_index(&before));
        assert!(!workspace.project_cache.holds_index(&after));
    }

    /// 13b-6 fix round 1: a relative `projectFile` is normalized, and one
    /// found under two roots is ambiguous.
    #[test]
    fn a_relative_configured_project_is_normalized_and_unique() {
        let temp = tempfile::tempdir().unwrap();
        let (one, two) = (temp.path().join("one"), temp.path().join("two"));
        write(&one.join("App.dproj"), "<Project/>");
        fs::create_dir_all(one.join("lib")).unwrap();
        let options = |project: &str| WorkspaceOptions {
            project_file: Some(PathBuf::from(project)),
            ..WorkspaceOptions::default()
        };
        let single = Workspace::with_override_session(
            vec![one.clone(), two.clone()],
            options("lib/../App.dproj"),
            OverrideSession::new(None),
        );
        assert_eq!(
            single.configured_project_path(),
            Some(one.join("App.dproj"))
        );
        write(&two.join("App.dproj"), "<Project/>");
        let ambiguous = Workspace::with_override_session(
            vec![one, two],
            options("App.dproj"),
            OverrideSession::new(None),
        );
        assert_eq!(ambiguous.configured_project_path(), None);
    }

    /// A8 (13b-6): a document whose selected project is not the configured
    /// one keeps its own context beside the configured project's.
    #[test]
    fn selecting_another_project_keeps_two_contexts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let configured = named_project(root, "A", true);
        let other = named_project(root, "B", true);
        pascal_project::backdate_tree_for_test(root);
        let options = WorkspaceOptions {
            project_file: Some(configured.clone()),
            ..WorkspaceOptions::default()
        };
        let mut workspace = workspace(root, options);
        let document = open_from_disk(&mut workspace, &root.join("B/U.pas"));
        select(&mut workspace, &root.join("B/U.pas"), &other);
        let project = IndexAnchor::Project(configured.clone());
        let by_project = context(&mut workspace, project.clone());
        let by_document = context(&mut workspace, IndexAnchor::Document(document.clone()));
        assert_ne!(by_project, by_document);
        assert_eq!(
            by_project.project_file.as_deref(),
            Some(configured.as_path())
        );
        assert_eq!(by_document.project_file.as_deref(), Some(other.as_path()));
        advance(&mut workspace, &project, IndexBound::all());
        advance(
            &mut workspace,
            &IndexAnchor::Document(document),
            IndexBound::all(),
        );
        assert_eq!(status(&workspace).len(), 2);
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
    /// path outside their directory, which keeps the selected project's
    /// context (D43).
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

    /// An open document whose own project is separate from the requesting
    /// one (it names its unit itself, so no D43 claim applies) is a second
    /// target under its own key.
    #[test]
    fn an_open_document_with_a_separate_owner_is_another_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (mut workspace, _, main, _) = selected_project(root);
        let solo = root.join("solo/Solo.pas");
        write(
            &root.join("solo/Solo.dproj"),
            "<Project><PropertyGroup><MainSource>Solo.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"Solo.pas\"/></ItemGroup></Project>",
        );
        write(&root.join("solo/Solo.dpr"), "program Solo; begin end.\n");
        write(&solo, OTHER.replace("unit Other", "unit Solo").as_str());
        pascal_project::backdate_tree_for_test(root);
        let solo_uri = Url::from_file_path(&solo).unwrap();
        workspace
            .open_document(solo_uri.clone(), fs::read_to_string(&solo).unwrap(), 3)
            .unwrap();
        let input = workspace.analysis_input();
        let solo_key = input.document_owners[&solo_uri].key.clone();
        assert_eq!(
            solo_key.project_file.as_deref(),
            Some(root.join("solo/Solo.dproj").as_path())
        );
        assert_ne!(solo_key, input.document_owners[&main].key);

        let mut request = Workspace::from_analysis_input(&input);
        let found = request
            .index_targets(
                &input,
                Some(&main),
                TargetPurpose::References,
                &AtomicBool::new(false),
            )
            .expect("targets");

        assert!(
            found.contexts.iter().any(|(key, _)| *key == solo_key),
            "the separate owner must be a target: {:?}",
            found.contexts
        );
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
        assert_eq!(
            input.document_owners[&other].key, input.document_owners[&main].key,
            "the search-path unit keeps the selected project's context (D43)"
        );
        assert!(
            found
                .contexts
                .iter()
                .all(|(key, _)| *key != input.document_owners[&other].key),
            "the older resolution must not be a second target"
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
        assert_eq!(MAX_INTERACTIVE_UNVERIFIED_CANDIDATES, 256);
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
            provider_project_with(root, WorkspaceOptions::default())
        }

        fn provider_project_with(
            root: &Path,
            options: WorkspaceOptions,
        ) -> (Workspace, ContextKey, IndexAnchor) {
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
                options,
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

        /// Another process's writes made a publication `Busy`: the member
        /// keeps its facts and the next batch proves it, so this process
        /// can still seal its snapshot (protocol test
        /// `two_servers_index_the_same_workspace`, l4d-zf1b.8).
        #[test]
        fn a_busy_publication_is_proven_by_the_next_batch() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), 3, WorkspaceOptions::default());
            let member = temp.path().join("lib/U0.pas");
            let (mut view, cache) = persisting_view(&workspace, store.path());
            let (handle, target) = (cache.clone(), member.clone());
            set_before_persist_hook(Some(Box::new(move |path| {
                if path == target {
                    handle
                        .job()
                        .expect("job")
                        .store()
                        .fail_next_for_test(pascal_cache::Bypass::Busy);
                }
            })));
            let first = advance(&mut view, &anchor, IndexBound::all());
            set_before_persist_hook(None);
            assert_eq!((first.indexed, first.persisted), (3, 2), "{first:?}");
            assert!(!view.project_cache.index_member_has_record(&key, &member));
            assert!(!first.complete, "the member is still to prove: {first:?}");
            let second = advance(&mut view, &anchor, IndexBound::all());
            assert!(second.complete, "{second:?}");
            assert_eq!(second.indexed + second.reused, 1, "{second:?}");
            assert_eq!(recorded(&view, &key).len(), 3);
            cache.shutdown();
        }

        /// Task 11 fix round 1: a publication refused for room (the WAL at
        /// its threshold) leaves the member memory-only; once the store
        /// reports no pressure, a later batch proves it and the snapshot
        /// seals, without a restart.
        #[test]
        fn a_publication_refused_for_wal_pressure_is_proven_by_a_later_batch() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), 3, WorkspaceOptions::default());
            let member = temp.path().join("lib/U0.pas");
            let (mut view, cache) = persisting_view(&workspace, store.path());
            let (handle, target) = (cache.clone(), member.clone());
            set_before_persist_hook(Some(Box::new(move |path| {
                if path == target {
                    handle
                        .job()
                        .expect("job")
                        .store()
                        .fail_next_for_test(pascal_cache::Bypass::WalPressure);
                }
            })));
            let first = advance(&mut view, &anchor, IndexBound::all());
            set_before_persist_hook(None);
            assert_eq!((first.indexed, first.persisted), (3, 2), "{first:?}");
            assert!(!view.project_cache.index_member_has_record(&key, &member));
            assert!(!first.complete, "the member is still to prove: {first:?}");
            assert!(first.worth_requeue(), "{first:?}");
            let second = advance(&mut view, &anchor, IndexBound::all());
            assert!(second.complete, "{second:?}");
            assert_eq!(second.indexed + second.reused, 1, "{second:?}");
            assert_eq!(recorded(&view, &key).len(), 3);
            assert_eq!(
                view.seal_baseline_if_due(&key, &mut || Ok(())),
                crate::workspace::baseline::SealOutcome::Sealed
            );
            cache.shutdown();
        }

        /// Batches until one is not worth requeueing; at most `limit`.
        fn until_settled(
            view: &mut Workspace,
            anchor: &IndexAnchor,
            bound: IndexBound,
            limit: usize,
        ) -> IndexProgress {
            for _ in 0..limit {
                let progress = advance(view, anchor, bound);
                if !progress.worth_requeue() {
                    return progress;
                }
            }
            panic!("the batches never end ({limit} batches)");
        }

        /// Fix round 2 (A): `ScratchExhausted` is this store's own scratch
        /// budget, not pressure maintenance clears: a member that exhausts
        /// it every time is extracted once and stays memory-only, and the
        /// batches end.
        #[test]
        fn a_publication_that_always_exhausts_scratch_is_extracted_once() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), 4, WorkspaceOptions::default());
            let member = temp.path().join("lib/U0.pas");
            let (mut view, cache) = persisting_view(&workspace, store.path());
            let target = member.clone();
            cache.set_publish_fault_for_test(Some(Box::new(move |path| {
                (path == target).then_some(pascal_cache::Bypass::ScratchExhausted)
            })));
            let loaded = record_loads();
            let last = until_settled(&mut view, &anchor, IndexBound::all(), 10);
            set_after_load_hook(None);
            assert_eq!(
                loaded
                    .borrow()
                    .iter()
                    .filter(|path| **path == member)
                    .count(),
                1,
                "{last:?}"
            );
            assert!(!view.project_cache.index_member_has_record(&key, &member));
            assert_eq!(recorded(&view, &key).len(), 3);
            cache.shutdown();
        }

        /// Rootcause A1 fix 4 (l4d-kqbb): a member whose evidence or
        /// artifact exceeds a fixed store limit keeps its facts in this
        /// process's index and gets an artifact-less record; it is never a
        /// write that fails the same way each batch, and the context still
        /// completes and seals.
        #[test]
        fn a_publication_over_a_fixed_limit_is_an_artifact_less_member() {
            const COUNT: usize = 4;
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), COUNT, WorkspaceOptions::default());
            let member = temp.path().join("lib/U0.pas");
            let (mut view, cache) = persisting_view(&workspace, store.path());
            let target = member.clone();
            cache.set_publish_fault_for_test(Some(Box::new(move |path| {
                (path == target).then_some(pascal_cache::Bypass::TooLarge(
                    pascal_cache::TooLarge::Observations { count: 70_000 },
                ))
            })));
            let loaded = record_loads();
            let (mut indexed, mut persisted) = (0, 0);
            let mut last = None;
            for _ in 0..10 {
                let progress = advance(&mut view, &anchor, IndexBound::all());
                indexed += progress.indexed;
                persisted += progress.persisted;
                let settled = !progress.worth_requeue();
                last = Some(progress);
                if settled {
                    break;
                }
            }
            set_after_load_hook(None);
            let last = last.unwrap();
            assert!(last.complete, "{last:?}");
            assert_eq!(
                loaded
                    .borrow()
                    .iter()
                    .filter(|path| **path == member)
                    .count(),
                1,
                "{last:?}"
            );
            assert_eq!((indexed, persisted), (COUNT, COUNT - 1), "{last:?}");
            // Reported, not hidden: a distinct count and a gap.
            let status = view.project_cache.index_status(&|_| false);
            let entry = &status[0];
            assert_eq!(entry.counts.over_limit, 1, "{entry:?}");
            assert_eq!(entry.counts.limited, 0, "{entry:?}");
            assert!(
                entry
                    .gaps
                    .iter()
                    .any(|gap| gap.starts_with("1 members are too large to store")),
                "{entry:?}"
            );
            let json = crate::project_cache::index_status_json(&status, Default::default());
            assert_eq!(json["contexts"][0]["overLimit"], 1, "{json}");
            let records = records(&view, &key);
            assert_eq!(records.len(), COUNT, "{records:?}");
            assert!(view.project_cache.index_member_has_record(&key, &member));
            assert_eq!(
                view.seal_baseline_if_due(&key, &mut || Ok(())),
                crate::workspace::baseline::SealOutcome::Sealed
            );
            cache.shutdown();
        }

        /// 13b-2 fix round 1 (M1): a session that reads more distinct
        /// inputs than a capture holds fails evidence with `Overflowed`,
        /// the member's own fixed limit: it is recorded artifact-less and
        /// reported, not re-extracted into the same failure.
        #[test]
        fn a_member_whose_capture_overflows_is_an_artifact_less_member() {
            const COUNT: usize = 3;
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            // U0 reads an include, so its session captures reads; with a
            // bound of 1 it captures more than it may hold.
            let member = temp.path().join("lib/U0.pas");
            let (workspace, key, anchor) =
                indexed_project(temp.path(), COUNT, WorkspaceOptions::default());
            write(&member.with_file_name("x.inc"), "const X = 1;\n");
            write(&member.with_file_name("y.inc"), "const Y = 1;\n");
            write(
                &member,
                "unit U0;\ninterface\n{$I x.inc}\n{$I y.inc}\nimplementation\nend.\n",
            );
            pascal_project::backdate_tree_for_test(temp.path());
            let (mut view, cache) = persisting_view(&workspace, store.path());
            crate::workspace::capture::set_capture_bound_for_test(Some(1));
            let loaded = record_loads();
            let first = until_settled(&mut view, &anchor, IndexBound::all(), 10);
            let again = advance(&mut view, &anchor, IndexBound::all());
            set_after_load_hook(None);
            crate::workspace::capture::set_capture_bound_for_test(None);
            assert!(first.complete, "{first:?}");
            assert_eq!(loaded.borrow().len(), COUNT, "each member loaded once");
            assert_eq!((again.indexed, again.persisted), (0, 0), "{again:?}");
            assert_eq!(recorded(&view, &key).len(), COUNT);
            let status = view.project_cache.index_status(&|_| false);
            assert_eq!(status[0].counts.over_limit, 1, "{:?}", status[0]);
            assert!(view.project_cache.index_member_has_record(&key, &member));
            cache.shutdown();
        }

        /// 13c-1 (rootcause2 U1): a session whose evidence names an input
        /// the capture did not record (`Uncaptured`) is never left
        /// memory-only: it is recorded artifact-less, counted `unprovable`
        /// (not `persisted`, not `overLimit`) with a gap naming the input's
        /// kind, it is not extracted again, and the context seals.
        #[test]
        fn a_member_with_an_uncaptured_input_is_an_unprovable_artifact_less_member() {
            const COUNT: usize = 3;
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let member = temp.path().join("lib/U0.pas");
            let (workspace, key, anchor) =
                indexed_project(temp.path(), COUNT, WorkspaceOptions::default());
            write(&member.with_file_name("x.inc"), "const X = 1;\n");
            write(
                &member,
                "unit U0;\ninterface\n{$I x.inc}\nimplementation\nend.\n",
            );
            pascal_project::backdate_tree_for_test(temp.path());
            let (mut view, cache) = persisting_view(&workspace, store.path());
            crate::workspace::capture::set_uncaptured_payload_for_test(Some("x.inc"));
            let loaded = record_loads();
            let first = until_settled(&mut view, &anchor, IndexBound::all(), 10);
            let again = advance(&mut view, &anchor, IndexBound::all());
            set_after_load_hook(None);
            crate::workspace::capture::set_uncaptured_payload_for_test(None);
            assert!(first.complete, "{first:?}");
            assert_eq!(loaded.borrow().len(), COUNT, "each member loaded once");
            assert_eq!((again.indexed, again.persisted), (0, 0), "{again:?}");
            assert_eq!(recorded(&view, &key).len(), COUNT);
            assert!(view.project_cache.index_member_has_record(&key, &member));
            let status = view.project_cache.index_status(&|_| false);
            let entry = &status[0];
            assert_eq!(entry.counts.unprovable, 1, "{entry:?}");
            assert_eq!(entry.counts.over_limit, 0, "{entry:?}");
            assert_eq!(entry.counts.persisted, COUNT - 1, "{entry:?}");
            assert!(
                // The include's search marker is proven by its hit, the
                // payload the capture missed: it is the first unproven input.
                entry.gaps.iter().any(|gap| gap.starts_with(
                    "1 members read an input the cache cannot prove (first: a directory search)"
                )),
                "{entry:?}"
            );
            let json = crate::project_cache::index_status_json(&status, Default::default());
            assert_eq!(json["contexts"][0]["unprovable"], 1, "{json}");
            assert_eq!(
                view.seal_baseline_if_due(&key, &mut || Ok(())),
                crate::workspace::baseline::SealOutcome::Sealed
            );
            cache.shutdown();
        }

        /// 13b-5 (A4) measurement, not a gate: per-member extraction cost
        /// on a multidev-shaped project (`DIRS` search-path directories,
        /// `L4D_MEASURE_UNITS` (600) units whose interfaces use lower units
        /// (`L4D_MEASURE_SHAPE=tree`: only one, half its index) and test a
        /// provider constant, so each session compiles a provider closure,
        /// and one shared include found through the search path). Run with
        /// `cargo test -p pascal-lsp --release --lib -- --ignored
        /// measure_member_cost_on_a_search_path_shape --nocapture`.
        #[test]
        #[ignore]
        fn measure_member_cost_on_a_search_path_shape() {
            const DIRS: usize = 40;
            let units = std::env::var("L4D_MEASURE_UNITS")
                .ok()
                .and_then(|units| units.parse().ok())
                .unwrap_or(600_usize);
            let tree = std::env::var("L4D_MEASURE_SHAPE").is_ok_and(|shape| shape == "tree");
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let root = temp.path();
            write(
                &root.join("lib/d0/Defines.inc"),
                "{$DEFINE SHARED}\nconst Shared = 1;\n",
            );
            for unit in 0..units {
                let path = root.join(format!("lib/d{}/U{unit}.pas", unit % DIRS));
                // `tree`: only `unit / 2`, a closure of log depth.
                let uses = if tree {
                    vec![unit / 2]
                } else {
                    vec![unit.saturating_sub(1), unit.saturating_sub(2), unit / 2]
                };
                let uses = uses
                    .into_iter()
                    .filter(|used| *used < unit)
                    .collect::<std::collections::BTreeSet<_>>();
                let mut text = format!("unit U{unit};\ninterface\n");
                if let Some(first) = uses.first() {
                    let names = uses
                        .iter()
                        .map(|used| format!("U{used}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    text.push_str(&format!(
                        "uses {names};\n{{$IF U{first}.Flag = 1}}\nconst Flag = 1;\n{{$ELSE}}\nconst Flag = 2;\n{{$ENDIF}}\n"
                    ));
                } else {
                    text.push_str("const Flag = 1;\n");
                }
                text.push_str(&format!(
                    "{{$I Defines.inc}}\nconst C{unit} = {unit};\nprocedure P{unit};\nimplementation\nprocedure P{unit};\nbegin\nend;\nend.\n"
                ));
                write(&path, &text);
            }
            let search = (0..DIRS)
                .map(|dir| format!("lib/d{dir}"))
                .collect::<Vec<_>>()
                .join(";");
            let (session, _key, anchor) = {
                let (workspace, key) = project_workspace(
                    root,
                    &format!("<DCC_UnitSearchPath>{search}</DCC_UnitSearchPath>"),
                    "",
                    WorkspaceOptions::default(),
                );
                (workspace, key, IndexAnchor::Project(root.join("App.dproj")))
            };
            let (mut view, cache) = persisting_view(&session, store.path());
            // Per member: the reuse attempt's end, then the load's end.
            type Mark = (PathBuf, Instant, Option<Instant>);
            let marks: Rc<RefCell<Vec<Mark>>> = Rc::default();
            let reuse_marks = Rc::clone(&marks);
            set_after_reuse_hook(Some(Box::new(move |path| {
                reuse_marks
                    .borrow_mut()
                    .push((path.to_path_buf(), Instant::now(), None));
            })));
            let load_marks = Rc::clone(&marks);
            set_after_load_hook(Some(Box::new(move |path| {
                if let Some(last) = load_marks.borrow_mut().last_mut()
                    && last.0 == path
                {
                    last.2 = Some(Instant::now());
                }
            })));
            let started = Instant::now();
            let progress = advance(&mut view, &anchor, IndexBound::all());
            let ended = Instant::now();
            set_after_reuse_hook(None);
            set_after_load_hook(None);
            let marks = marks.borrow();
            let mut loads = Vec::new();
            let mut totals = Vec::new();
            for (at, (_, reused, loaded)) in marks.iter().enumerate() {
                let next = marks.get(at + 1).map_or(ended, |next| next.1);
                totals.push(next - *reused);
                if let Some(loaded) = loaded {
                    loads.push(*loaded - *reused);
                }
            }
            let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
            let summary = |name: &str, values: &[Duration]| {
                let mut sorted = values.to_vec();
                sorted.sort();
                let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize];
                let tenth = values.len() / 10;
                let mean = |part: &[Duration]| {
                    ms(part.iter().sum::<Duration>()) / part.len().max(1) as f64
                };
                println!(
                    "{name}: n={} median={:.2} ms p95={:.2} ms max={:.2} ms first-10%-mean={:.2} ms last-10%-mean={:.2} ms",
                    values.len(),
                    ms(at(0.5)),
                    ms(at(0.95)),
                    ms(at(1.0)),
                    mean(&values[..tenth]),
                    mean(&values[values.len() - tenth..]),
                );
            };
            println!(
                "batch: indexed={} persisted={} awaiting_room={} limited={} refused={} in {:.0} ms; providerGraphsRefused={}",
                progress.indexed,
                progress.persisted,
                progress.awaiting_room,
                progress.limited,
                progress.refused,
                ms(ended - started),
                view.project_cache
                    .index_process_counts()
                    .provider_graphs_refused,
            );
            summary("load (session + parse)", &loads);
            summary("member total (load + facts + publish)", &totals);
            assert_eq!(progress.limited, 0, "{progress:?}");
            cache.shutdown();
        }

        /// One `WalPressure` refusal of the first publication.
        fn refuse_first_publication(cache: &PersistentCache) {
            let mut refused = false;
            cache.set_publish_fault_for_test(Some(Box::new(move |_| {
                (!std::mem::replace(&mut refused, true))
                    .then_some(pascal_cache::Bypass::WalPressure)
            })));
        }

        /// Fix round 2 (B): free disk space is not something a maintenance
        /// step makes, so no batch waits for one.
        #[test]
        fn no_batch_waits_for_room_under_low_disk_space() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, _, anchor) =
                indexed_project(temp.path(), 8, WorkspaceOptions::default());
            let (mut view, cache) = persisting_view(&workspace, store.path());
            refuse_first_publication(&cache);
            cache.set_pressure_for_test(Some(Box::new(|_| pascal_cache::Pressure::LowDiskSpace)));
            let last = until_settled(&mut view, &anchor, IndexBound::units(3), 10);
            assert_eq!(last.awaiting_room, 1, "{last:?}");
            assert_eq!(cache.room_waits_for_test(), 0);
            cache.shutdown();
        }

        /// Fix round 2 (B): under WAL pressure a batch waits once, and once
        /// a wait found no room, the batches that still have members to
        /// extract do not wait again; only the one left with nothing but
        /// the refused member does.
        #[test]
        fn a_wait_for_room_that_found_none_is_not_repeated_by_each_batch() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, _, anchor) =
                indexed_project(temp.path(), 10, WorkspaceOptions::default());
            let (mut view, cache) = persisting_view(&workspace, store.path());
            refuse_first_publication(&cache);
            cache.set_pressure_for_test(Some(Box::new(|_| pascal_cache::Pressure::WalPressure)));
            let last = until_settled(&mut view, &anchor, IndexBound::units(3), 10);
            assert_eq!(last.awaiting_room, 1, "{last:?}");
            assert_eq!(cache.room_waits_for_test(), 2);
            cache.shutdown();
        }

        /// 13c-2 (rootcause2 U2): a refusal for room restales exactly the
        /// refused member. A member that stays memory-only for another
        /// reason (here, no free disk space) is neither extracted again nor
        /// counted as awaiting room.
        #[test]
        fn a_refusal_for_room_restales_only_the_refused_member() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), 6, WorkspaceOptions::default());
            // The unrelated member comes before the refused one, so the
            // restale at the next member's start finds it unrecorded.
            let (other, refused) = (
                temp.path().join("lib/U1.pas"),
                temp.path().join("lib/U2.pas"),
            );
            let (mut view, cache) = persisting_view(&workspace, store.path());
            let (target, unrelated) = (refused.clone(), other.clone());
            let mut once = false;
            cache.set_publish_fault_for_test(Some(Box::new(move |path| {
                if path == unrelated {
                    Some(pascal_cache::Bypass::LowDiskSpace)
                } else {
                    (path == target && !std::mem::replace(&mut once, true))
                        .then_some(pascal_cache::Bypass::WalPressure)
                }
            })));
            let loaded = record_loads();
            let last = until_settled(&mut view, &anchor, IndexBound::all(), 10);
            set_after_load_hook(None);
            let count = |path: &PathBuf| loaded.borrow().iter().filter(|p| *p == path).count();
            assert_eq!(count(&refused), 2, "refused member: {last:?}");
            assert_eq!(count(&other), 1, "unrelated member: {last:?}");
            assert_eq!((last.awaiting_room, last.complete), (0, true), "{last:?}");
            let unrecorded = records(&view, &key)
                .into_iter()
                .filter(|(_, record)| record.is_none())
                .map(|(path, _)| path)
                .collect::<Vec<_>>();
            assert_eq!(unrecorded, vec![other.clone()]);
            assert_eq!(state(&view, &key, &other), Some(MemberState::Fresh));
            cache.shutdown();
        }

        /// 13c-2 fix round 1: a member refused for room whose later
        /// extraction ends memory-only for another reason is no longer
        /// awaiting room.
        #[test]
        fn a_member_indexed_again_leaves_the_awaiting_room_set() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), 4, WorkspaceOptions::default());
            let member = temp.path().join("lib/U2.pas");
            let (mut view, cache) = persisting_view(&workspace, store.path());
            let target = member.clone();
            cache.set_publish_fault_for_test(Some(Box::new(move |path| {
                (path == target).then_some(pascal_cache::Bypass::LowDiskSpace)
            })));
            let first = advance(&mut view, &anchor, IndexBound::all());
            assert!(first.complete, "{first:?}");
            view.project_cache.note_index_awaiting_room(&key, &member);
            view.project_cache.note_index_memory_only(&key, &member);
            assert_eq!(view.project_cache.index_awaiting_room(&key), 1);
            // The member changes and is extracted again.
            fs::write(
                &member,
                "unit U2;\ninterface\nconst Changed = 1;\nimplementation\nend.\n",
            )
            .unwrap();
            view.project_cache.invalidate_path(&member);
            let second = advance(&mut view, &anchor, IndexBound::all());
            assert_eq!(second.indexed, 1, "{second:?}");
            assert_eq!(second.persisted, 0, "{second:?}");
            assert_eq!(second.awaiting_room, 0, "{second:?}");
            assert!(second.complete, "{second:?}");
            assert!(!view.project_cache.index_is_awaiting_room(&key));
            assert_eq!(view.project_cache.restale_awaiting_room(&key), 0);
            assert_eq!(view.project_cache.restale_memory_only_members(&key), 0);
            cache.shutdown();
        }

        /// 13c-2: the awaiting-room and memory-only sets hold the members
        /// that were refused or indexed memory-only, nothing else
        /// unrecorded.
        #[test]
        fn the_awaiting_room_and_memory_only_sets_hold_only_their_members() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), 4, WorkspaceOptions::default());
            let (a, b) = (
                temp.path().join("lib/U1.pas"),
                temp.path().join("lib/U2.pas"),
            );
            let (mut view, cache) = persisting_view(&workspace, store.path());
            let pair = [a.clone(), b.clone()];
            cache.set_publish_fault_for_test(Some(Box::new(move |path| {
                pair.contains(&path.to_path_buf())
                    .then_some(pascal_cache::Bypass::LowDiskSpace)
            })));
            let progress = advance(&mut view, &anchor, IndexBound::all());
            assert_eq!(progress.awaiting_room, 0, "{progress:?}");
            assert!(progress.complete, "{progress:?}");
            let cache_of = &view.project_cache;
            assert!(!cache_of.index_is_awaiting_room(&key));
            assert_eq!(cache_of.restale_awaiting_room(&key), 0);
            assert_eq!(cache_of.restale_memory_only_members(&key), 0);
            cache_of.note_index_awaiting_room(&key, &a);
            assert!(cache_of.index_is_awaiting_room(&key));
            assert_eq!(cache_of.index_awaiting_room(&key), 1);
            assert_eq!(cache_of.restale_awaiting_room(&key), 1);
            assert_eq!(state(&view, &key, &a), Some(MemberState::Stale));
            assert_eq!(state(&view, &key, &b), Some(MemberState::Fresh));
            assert_eq!(cache_of.index_awaiting_room(&key), 0);
            cache_of.note_index_memory_only(&key, &b);
            assert_eq!(cache_of.restale_memory_only_members(&key), 1);
            assert_eq!(state(&view, &key, &b), Some(MemberState::Stale));
            assert_eq!(cache_of.restale_memory_only_members(&key), 0);
            cache.shutdown();
        }

        /// Task 10b: a batch that ran memory-only (the store opened after
        /// `MAX_OPEN_WAIT`, or was enabled later) leaves members without a
        /// record. Once the store is enabled, the next batch proves them, so
        /// they get records and a snapshot without a restart.
        #[test]
        fn members_indexed_memory_only_are_proven_once_the_store_is_enabled() {
            const COUNT: usize = 4;
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (workspace, key, anchor) =
                indexed_project(temp.path(), COUNT, WorkspaceOptions::default());
            let cache = start(&workspace, store.path(), false);
            assert_eq!(cache.state(), CacheState::Disabled);
            let mut view = view_with(&workspace, &cache);
            let first = advance(&mut view, &anchor, IndexBound::all());
            assert!(first.complete, "{first:?}");
            assert_eq!((first.indexed, first.persisted), (COUNT, 0), "{first:?}");

            let ack = cache.reconfigure(CacheOptions {
                enabled: true,
                directory: Some(store.path().to_path_buf()),
                ..CacheOptions::default()
            });
            assert_eq!(ack, Ack::Done);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            let second = advance(&mut view, &anchor, IndexBound::all());
            assert!(second.complete, "{second:?}");
            assert_eq!(second.persisted, COUNT, "{second:?}");
            assert_eq!(recorded(&view, &key).len(), COUNT);
            let third = advance(&mut view, &anchor, IndexBound::all());
            assert_eq!(
                (third.indexed, third.reused, third.persisted),
                (0, 0, 0),
                "proven once: {third:?}"
            );
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
            let pool = Rc::new(RefCell::new(None));
            let held = Rc::clone(&pool);
            set_before_persist_hook(Some(Box::new(move |path| {
                if path == target {
                    let stats = project_cache.stats();
                    // No room is left for anything new, not even in the
                    // publication pool (13b-3).
                    project_cache.set_max_bytes(stats.outstanding_bytes + stats.reserved_bytes());
                    *held.borrow_mut() =
                        project_cache.reserve_publication(project_cache.publication_pool_bytes());
                    flag.set(true);
                }
            })));
            let progress = advance(&mut view, &anchor, IndexBound::all());
            set_before_persist_hook(None);
            assert!(shrunk.get());
            assert!(
                pool.borrow_mut().take().is_some(),
                "the whole pool was held"
            );
            workspace
                .project_cache()
                .set_max_bytes(crate::project_cache::DEFAULT_MAX_CACHE_BYTES);
            assert!(progress.complete, "{progress:?}");
            assert_eq!((progress.indexed, progress.persisted), (2, 1));
            assert_eq!(state(&view, &key, &member), Some(MemberState::Fresh));
            assert_eq!(recorded(&view, &key), vec![temp.path().join("App.dpr")]);
            // 13b-3 fix round 1 (M3): counted and explained, not re-queued.
            let status = view.project_cache.index_status(&|_| false);
            assert_eq!(status[0].publications_refused, 1, "{status:?}");
            assert!(
                status[0]
                    .gaps
                    .iter()
                    .any(|gap| gap.starts_with("1 members found no memory to publish")),
                "{status:?}"
            );
            let json = crate::project_cache::index_status_json(&status, Default::default());
            assert_eq!(json["contexts"][0]["publicationsRefused"], 1, "{json}");
            cache.shutdown();
            let (mut second, cache) = restarted_view(temp.path(), store.path());
            let loaded = record_loads();
            let progress = advance(&mut second, &anchor, IndexBound::all());
            set_after_load_hook(None);
            assert_eq!((progress.reused, progress.indexed), (1, 1));
            assert_eq!(*loaded.borrow(), vec![member]);
            cache.shutdown();
        }

        /// 13c-2 (fix #5): a batch reports its reuse phases without the test
        /// timers, once phase metrics are on (the trace switch in release).
        #[test]
        fn a_reuse_batch_reports_its_phase_timings() {
            let temp = tempfile::tempdir().unwrap();
            let store = private_dir();
            let (_, anchor) = first_session(temp.path(), store.path(), 12);
            let (mut second, cache) = restarted_view(temp.path(), store.path());
            set_phase_metrics_for_test(true);
            take_phase_report();
            let before = ReusePhaseTotals::snapshot();
            let progress = advance(
                &mut second,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            let totals = ReusePhaseTotals::snapshot().since(&before);
            let report = take_phase_report().expect("the batch reported");
            assert_eq!(progress.reused, 12);
            assert_eq!(report.threads, 1);
            assert_eq!(report.mode, IndexMode::Reuse);
            // Insert and prove are timed on every reuse path; the others
            // only on the snapshot path.
            for phase in [ReusePhase::Insert, ReusePhase::Prove] {
                assert!(report.totals.calls(phase) >= 12, "{phase:?}: {report:?}");
                assert!(totals.calls(phase) >= 12, "{phase:?}: {totals:?}");
            }
            let line = report.to_string();
            for name in [
                "lookup",
                "readHash",
                "validation",
                "decode",
                "insert",
                "prove",
                "threads=1",
            ] {
                assert!(line.contains(name), "{name} in {line}");
            }
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

        /// R8-1: bounded parallel validation of reuse batches.
        mod parallel_reuse {
            use super::*;
            use std::sync::atomic::AtomicUsize;
            use std::time::Instant;

            /// Search-path directories `d00..`, and `units` units spread over
            /// them; every unit includes `Shared.inc` from the last one, so
            /// each member's evidence names the whole search path
            /// (multidev's shape: many members over ~40 shared directories).
            pub(super) fn shared_directories_project(
                root: &Path,
                directories: usize,
                units: usize,
            ) -> (Workspace, ContextKey, IndexAnchor) {
                let names = (0..directories)
                    .map(|index| format!("d{index:02}"))
                    .collect::<Vec<_>>();
                write(
                    &root.join(names.last().unwrap()).join("Shared.inc"),
                    "const Shared = 1;\n",
                );
                for index in 0..units - 1 {
                    let directory = &names[index % directories];
                    write(
                        &root.join(directory).join(format!("U{index:04}.pas")),
                        &format!(
                            "unit U{index:04};\ninterface\n{{$I Shared.inc}}\nprocedure P{index};\nimplementation\nprocedure P{index};\nbegin\nend;\nend.\n"
                        ),
                    );
                }
                let (workspace, key) = project_workspace(
                    root,
                    &format!(
                        "<DCC_UnitSearchPath>{}</DCC_UnitSearchPath><DCC_IncludePath>{}</DCC_IncludePath>",
                        names.join(";"),
                        names.join(";")
                    ),
                    "",
                    WorkspaceOptions::default(),
                );
                (workspace, key, IndexAnchor::Project(root.join("App.dproj")))
            }

            /// A first process indexes and publishes every member.
            pub(super) fn published(
                root: &Path,
                store: &Path,
                directories: usize,
                units: usize,
            ) -> (ContextKey, IndexAnchor) {
                let (workspace, key, anchor) = shared_directories_project(root, directories, units);
                // The production maintenance schedule checkpoints the WAL
                // as the extraction batches fill it.
                let (mut first, cache) = super::synthetic::scheduled_view(&workspace, store);
                let mut totals = super::synthetic::Totals::default();
                super::synthetic::batches(
                    &mut first,
                    &anchor,
                    IndexBound::extraction(),
                    None,
                    &mut || Ok(()),
                    &mut totals,
                )
                .unwrap();
                assert_eq!(recorded(&first, &key).len(), units, "{totals:?}");
                cache.shutdown();
                (key, anchor)
            }

            pub(super) fn reuse_pass(
                view: &mut Workspace,
                anchor: &IndexAnchor,
                threads: usize,
                may_run: &(dyn Fn() -> bool + Sync),
            ) -> Result<IndexProgress, String> {
                view.advance_workspace_index_with(
                    anchor,
                    IndexBound::all().with_mode(IndexMode::Reuse),
                    &mut || Ok(()),
                    Some(ParallelValidation {
                        threads,
                        may_run,
                        workers: &move || threads,
                    }),
                    &AtomicBool::new(false),
                )
            }

            /// Everything a reuse pass leaves in the index, per member.
            fn snapshot(view: &Workspace, key: &ContextKey) -> Vec<String> {
                let mut rows = members(view, key)
                    .into_iter()
                    .map(|path| {
                        let detail = view
                            .project_cache
                            .read_index(key, |index, _| {
                                format!(
                                    "{:?} {:?} {:?} {}",
                                    index.state_of(&path),
                                    index.stamps_of(&path),
                                    index.slot_paths_of(&path),
                                    index.reuse_tried(&path),
                                )
                            })
                            .unwrap_or_default();
                        format!("{} {detail}", path.display())
                    })
                    .collect::<Vec<_>>();
                rows.extend(
                    records(view, key)
                        .into_iter()
                        .map(|(path, record)| format!("record {} {record:?}", path.display())),
                );
                rows
            }

            #[test]
            fn parallel_and_serial_validation_give_the_same_index_and_records() {
                const UNITS: usize = 30;
                let temp = tempfile::tempdir().unwrap();
                let store = private_dir();
                let (key, anchor) = published(temp.path(), store.path(), 6, UNITS);
                let mut runs = Vec::new();
                let mut reads = Vec::new();
                for threads in [1, 3] {
                    let (mut view, cache) = restarted_view(temp.path(), store.path());
                    let progress = reuse_pass(&mut view, &anchor, threads, &|| true).unwrap();
                    assert_eq!(progress.reused, UNITS, "{threads} threads: {progress:?}");
                    let hits = cache.counters().unwrap().snapshot().hits;
                    let names = (0..UNITS - 1)
                        .step_by(7)
                        .map(|index| selected(&view, &key, &format!("P{index}")))
                        .collect::<Vec<_>>();
                    runs.push((progress, hits, snapshot(&view, &key), names));
                    reads.push(view.project_cache.index_validation_reads(&key));
                    cache.shutdown();
                }
                assert_eq!(runs[0], runs[1]);
                // A cost measure, not state: two workers may both read a
                // directory before either's witness is recorded.
                assert!(reads[0] >= 1 && reads[0] <= reads[1], "{reads:?}");
            }

            /// 13c-2 fix round 1: without a held snapshot (the member-by-member
            /// store path) a batch still reports non-zero lookup, read+hash,
            /// validation and decode calls, for hits and misses alike.
            #[test]
            fn a_snapshotless_batch_reports_every_reuse_phase() {
                const UNITS: usize = 30;
                let temp = tempfile::tempdir().unwrap();
                let store = private_dir();
                let (_, anchor) = published(temp.path(), store.path(), 6, UNITS);
                set_phase_metrics_for_test(true);
                // One member now misses: its saved root changed.
                write(
                    &temp.path().join("d00/U0000.pas"),
                    "unit U0000;\ninterface\n{$I Shared.inc}\nprocedure Changed;\nimplementation\nprocedure Changed;\nbegin\nend;\nend.\n",
                );
                let (mut view, cache) = restarted_view(temp.path(), store.path());
                take_phase_report();
                let progress = reuse_pass(&mut view, &anchor, 1, &|| true).unwrap();
                let report = take_phase_report().expect("reported");
                assert_eq!(progress.reused, UNITS - 1, "{progress:?}");
                assert_eq!(progress.reuse_missed, 1, "{progress:?}");
                for phase in [
                    ReusePhase::Lookup,
                    ReusePhase::ReadHash,
                    ReusePhase::Validation,
                    ReusePhase::Decode,
                ] {
                    assert!(report.totals.calls(phase) > 0, "{phase:?}: {report}");
                }
                // Every member reads and hashes its root, hit or miss.
                assert!(
                    report.totals.calls(ReusePhase::ReadHash) >= UNITS,
                    "{report}"
                );
                cache.shutdown();
            }

            /// 13c-2: the report of a parallel batch names its worker threads.
            #[test]
            fn a_parallel_batch_reports_its_threads() {
                const UNITS: usize = 30;
                let temp = tempfile::tempdir().unwrap();
                let store = private_dir();
                let (_, anchor) = published(temp.path(), store.path(), 6, UNITS);
                set_phase_metrics_for_test(true);
                for threads in [1, 3] {
                    let (mut view, cache) = restarted_view(temp.path(), store.path());
                    take_phase_report();
                    let progress = reuse_pass(&mut view, &anchor, threads, &|| true).unwrap();
                    let report = take_phase_report().expect("reported");
                    assert_eq!(progress.reused, UNITS);
                    assert_eq!(report.threads, threads, "{report:?}");
                    assert!(
                        report.totals.calls(ReusePhase::Prove) >= UNITS,
                        "{report:?}"
                    );
                    cache.shutdown();
                }
            }

            #[test]
            fn interactive_work_stops_the_workers_between_members() {
                const UNITS: usize = 24;
                const PASSING: usize = 5;
                const THREADS: usize = 3;
                let temp = tempfile::tempdir().unwrap();
                let store = private_dir();
                let (key, anchor) = published(temp.path(), store.path(), 4, UNITS);
                let (mut view, cache) = restarted_view(temp.path(), store.path());
                let (checks, closed) = (AtomicUsize::new(0), AtomicUsize::new(0));
                // Interactive work arrives after `PASSING` passing checks
                // (by the workers, or the batch while it waits).
                let gate = || {
                    let open = checks.fetch_add(1, Ordering::SeqCst) < PASSING;
                    closed.fetch_add(usize::from(!open), Ordering::SeqCst);
                    open
                };
                let result = reuse_pass(&mut view, &anchor, THREADS, &gate);
                assert_eq!(result.unwrap_err(), CANCELLATION_MESSAGE);
                let hits = cache.counters().unwrap().snapshot().hits as usize;
                assert!(hits <= PASSING, "{hits}");
                // Each worker stops at its next member, and the batch trips
                // the validations in flight once: one closed check each.
                assert!(
                    closed.load(Ordering::SeqCst) <= THREADS + 1,
                    "{} closed checks",
                    closed.load(Ordering::SeqCst)
                );
                // No member started after the gate closed is applied: the
                // applied ones are among the first members in path order.
                let mut order = members(&view, &key);
                order.sort_by_key(|path| path_key(path));
                let fresh = order
                    .iter()
                    .filter(|path| state(&view, &key, path) == Some(MemberState::Fresh))
                    .count();
                assert!(fresh <= hits, "{fresh} of {hits}");
                assert!(
                    order[PASSING + THREADS..]
                        .iter()
                        .all(|path| state(&view, &key, path) != Some(MemberState::Fresh)),
                    "a member past the stop was applied"
                );
                let progress = reuse_pass(&mut view, &anchor, THREADS, &|| true).unwrap();
                assert_eq!(progress.reused, UNITS - fresh, "{progress:?}");
                assert!(progress.complete, "{progress:?}");
                cache.shutdown();
            }

            #[test]
            fn cancellation_stops_a_parallel_pass() {
                let temp = tempfile::tempdir().unwrap();
                let store = private_dir();
                let (_, anchor) = published(temp.path(), store.path(), 4, 12);
                let (mut view, cache) = restarted_view(temp.path(), store.path());
                let cancel = AtomicBool::new(false);
                let checks = AtomicUsize::new(0);
                // The request is cancelled once two members started.
                let gate = || {
                    if checks.fetch_add(1, Ordering::SeqCst) == 1 {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    true
                };
                let result = view.advance_workspace_index_with(
                    &anchor,
                    IndexBound::all().with_mode(IndexMode::Reuse),
                    &mut || Ok(()),
                    Some(ParallelValidation {
                        threads: 3,
                        may_run: &gate,
                        workers: &|| 3,
                    }),
                    &cancel,
                );
                assert_eq!(result.unwrap_err(), CANCELLATION_MESSAGE);
                assert!(
                    checks.load(Ordering::SeqCst) <= 2 + 3,
                    "{} checks",
                    checks.load(Ordering::SeqCst)
                );
                // At most one member a worker, started before it saw the flag.
                assert!(cache.counters().unwrap().snapshot().hits <= 3);
                cache.shutdown();
            }

            #[test]
            fn parallel_validation_holds_at_most_one_members_facts_per_thread() {
                const THREADS: usize = 3;
                let temp = tempfile::tempdir().unwrap();
                let store = private_dir();
                let (_, anchor) = published(temp.path(), store.path(), 4, 24);
                let (mut view, cache) = restarted_view(temp.path(), store.path());
                let progress = reuse_pass(&mut view, &anchor, THREADS, &|| true).unwrap();
                assert_eq!(progress.reused, 24);
                let peak = take_validation_peak();
                assert!(peak.held <= THREADS, "{peak:?}");
                assert!(peak.held >= 1, "{peak:?}");
                assert!(
                    peak.reserved_bytes <= THREADS * pascal_cache::MAX_ARTIFACT_CONTENT_BYTES,
                    "{peak:?}"
                );
                assert_eq!(view.project_cache.stats().reserved_facts_bytes, 0);
                cache.shutdown();
            }

            #[test]
            fn without_a_store_a_parallel_pass_changes_nothing() {
                let temp = tempfile::tempdir().unwrap();
                let (workspace, key, anchor) = shared_directories_project(temp.path(), 4, 12);
                let mut serial = workspace.analysis_input().disk_baseline();
                serial.persistent_cache = Default::default();
                let mut serial = Workspace::from_analysis_input(&serial);
                let mut parallel =
                    Workspace::from_analysis_input(&workspace.analysis_input().disk_baseline());
                let one = reuse_pass(&mut serial, &anchor, 1, &|| true).unwrap();
                let checks = AtomicUsize::new(0);
                let two = reuse_pass(&mut parallel, &anchor, 3, &|| {
                    checks.fetch_add(1, Ordering::SeqCst);
                    true
                })
                .unwrap();
                assert_eq!(one, two);
                assert_eq!(checks.load(Ordering::SeqCst), 0, "no worker started");
                assert_eq!(snapshot(&serial, &key), snapshot(&parallel, &key));
            }

            /// I1: worker `n` takes no member once the free general slots
            /// are `n` or fewer (diagnostics started); worker 0 goes on.
            #[test]
            fn workers_shrink_when_diagnostics_take_the_slots() {
                const UNITS: usize = 24;
                let temp = tempfile::tempdir().unwrap();
                let store = private_dir();
                let (_, anchor) = published(temp.path(), store.path(), 4, UNITS);
                let (mut view, cache) = restarted_view(temp.path(), store.path());
                let free = Arc::new(AtomicUsize::new(3));
                let applied = Rc::new(Cell::new(0));
                let (slots, count) = (Arc::clone(&free), Rc::clone(&applied));
                // Diagnostics start once four members are applied.
                set_after_reuse_hook(Some(Box::new(move |_| {
                    count.set(count.get() + 1);
                    if count.get() == 4 {
                        slots.store(1, Ordering::SeqCst);
                    }
                })));
                let progress = view.advance_workspace_index_with(
                    &anchor,
                    IndexBound::all().with_mode(IndexMode::Reuse),
                    &mut || Ok(()),
                    Some(ParallelValidation {
                        threads: 3,
                        may_run: &|| true,
                        workers: &|| free.load(Ordering::SeqCst),
                    }),
                    &AtomicBool::new(false),
                );
                set_after_reuse_hook(None);
                let progress = progress.unwrap();
                assert_eq!(progress.reused, UNITS, "{progress:?}");
                let takes = take_validation_takes();
                assert_eq!(takes.len(), UNITS);
                assert!(
                    takes
                        .iter()
                        .all(|&(worker, free)| worker == 0 || worker < free),
                    "{takes:?}"
                );
                let first_shrunk = takes
                    .iter()
                    .position(|&(_, free)| free == 1)
                    .expect("members taken after the diagnostics started");
                assert!(
                    takes[first_shrunk..].iter().all(|&(worker, _)| worker == 0),
                    "only worker 0 takes members once one slot is free: {takes:?}"
                );
                cache.shutdown();
            }

            /// M1: workers take no member past the batch's unit bound.
            #[test]
            fn workers_take_no_member_past_the_unit_bound() {
                let temp = tempfile::tempdir().unwrap();
                let store = private_dir();
                let (_, anchor) = published(temp.path(), store.path(), 4, 24);
                let (mut view, cache) = restarted_view(temp.path(), store.path());
                let progress = view
                    .advance_workspace_index_with(
                        &anchor,
                        IndexBound::units(5).with_mode(IndexMode::Reuse),
                        &mut || Ok(()),
                        Some(ParallelValidation {
                            threads: 3,
                            may_run: &|| true,
                            workers: &|| 3,
                        }),
                        &AtomicBool::new(false),
                    )
                    .unwrap();
                assert_eq!(progress.reused, 5, "{progress:?}");
                assert_eq!(take_validation_takes().len(), 5);
                assert_eq!(cache.counters().unwrap().snapshot().hits, 5);
                cache.shutdown();
            }

            /// M2: a watcher event between a worker's validation and the
            /// batch's apply makes the batch prove the member again itself:
            /// the worker's facts of the old bytes are never indexed.
            #[test]
            fn an_event_after_a_workers_validation_is_proven_again_by_the_batch() {
                const UNITS: usize = 12;
                let temp = tempfile::tempdir().unwrap();
                let store = private_dir();
                let (key, anchor) = published(temp.path(), store.path(), 4, UNITS);
                let (mut view, cache) = restarted_view(temp.path(), store.path());
                let edited = temp.path().join("d03/U0003.pas");
                let target = edited.clone();
                let project_cache = view.project_cache().clone();
                set_before_apply_hook(Some(Box::new(move |path| {
                    if path == target {
                        write(
                            &target,
                            "unit U0003;\ninterface\nprocedure Q3;\nimplementation\nprocedure Q3;\nbegin\nend;\nend.\n",
                        );
                        project_cache.invalidate_path(&target);
                    }
                })));
                let progress = reuse_pass(&mut view, &anchor, 3, &|| true);
                set_before_apply_hook(None);
                let progress = progress.unwrap();
                assert_eq!(progress.reused, UNITS - 1, "{progress:?}");
                assert_eq!(progress.reuse_missed, 1, "{progress:?}");
                assert!(selected(&view, &key, "P3").is_empty());
                assert_ne!(state(&view, &key, &edited), Some(MemberState::Fresh));
                cache.shutdown();
            }

            /// M5/N1: interactive work found while the batch waits stops the
            /// batch at that member and trips the validations in flight.
            #[test]
            fn interactive_work_while_the_batch_waits_stops_it_and_trips_the_workers() {
                let pipeline = Pipeline::new(2);
                pipeline.worker_started();
                // A worker took member 0; the batch waits for it.
                assert_eq!(pipeline.take_next(2, 0, &|| 2), Some((0, 2)));
                assert!(matches!(pipeline.take(0, &|| false), Prefetched::Preempted));
                let budget =
                    crate::workspace::ReconciliationBudget::new(Arc::clone(&pipeline.tripped));
                assert!(pascal_project::ProjectWorkBudget::check_cancelled(&budget).is_err());
                assert_eq!(pipeline.take_next(2, 0, &|| 2), None, "the workers stop");
            }

            /// N2: with no worker left, the batch never waits for a member.
            #[test]
            fn the_batch_does_not_wait_once_every_worker_exited() {
                let pipeline = Pipeline::new(2);
                pipeline.worker_started();
                assert_eq!(pipeline.take_next(2, 0, &|| 2), Some((0, 2)));
                pipeline.worker_exited();
                assert!(matches!(pipeline.take(0, &|| true), Prefetched::Preempted));
            }

            /// N1: a gate closed for one check while the batch waits ends
            /// the batch with a cancellation (the normal yield), not with
            /// every later member a retryable miss; the next batch reuses
            /// everything.
            #[test]
            fn a_short_interactive_request_while_the_batch_waits_yields_the_batch() {
                const UNITS: usize = 12;
                let temp = tempfile::tempdir().unwrap();
                let store = private_dir();
                let (_, anchor) = published(temp.path(), store.path(), 4, UNITS);
                let (mut view, cache) = restarted_view(temp.path(), store.path());
                let batch = std::thread::current().id();
                let closed = AtomicBool::new(false);
                // The batch's first check (waiting for member 0) finds the
                // gate closed; workers wait for it before their first member,
                // so the batch does wait.
                let gate = || {
                    if std::thread::current().id() == batch {
                        // Closed once, open again after.
                        closed.swap(true, Ordering::SeqCst)
                    } else {
                        while !closed.load(Ordering::SeqCst) {
                            std::thread::yield_now();
                        }
                        true
                    }
                };
                let result = reuse_pass(&mut view, &anchor, 3, &gate);
                assert_eq!(result.unwrap_err(), CANCELLATION_MESSAGE);
                let progress = reuse_pass(&mut view, &anchor, 3, &|| true).unwrap();
                assert_eq!(progress.reused, UNITS, "{progress:?}");
                assert_eq!(progress.reuse_deferred, 0, "{progress:?}");
                cache.shutdown();
            }

            /// Release measurement (report-only; numbers go into the task
            /// report):
            /// TMPDIR=$HOME/.cache/l4d-tmp cargo test -p pascal-lsp --release --lib parallel_validation_speedup -- --ignored --nocapture
            #[test]
            #[ignore = "release measurement, run explicitly"]
            fn parallel_validation_speedup_on_shared_directories() {
                const DIRECTORIES: usize = 40;
                const UNITS: usize = 2_000;
                let temp = tempfile::tempdir().unwrap();
                let store = private_dir();
                let (_, anchor) = published(temp.path(), store.path(), DIRECTORIES, UNITS);
                let threads = crate::server::validation_threads(0);
                let mut times = Vec::new();
                for threads in [1, threads] {
                    let (mut view, cache) = restarted_view(temp.path(), store.path());
                    let started = Instant::now();
                    let progress = reuse_pass(&mut view, &anchor, threads, &|| true).unwrap();
                    let elapsed = started.elapsed();
                    assert_eq!(progress.reused, UNITS, "{progress:?}");
                    eprintln!(
                        "R8-1 shared directories: {UNITS} members over {DIRECTORIES} \
                         directories, {threads} threads: reuse pass {elapsed:?}"
                    );
                    times.push(elapsed);
                    cache.shutdown();
                }
                eprintln!(
                    "R8-1 shared directories: speedup {:.2}x",
                    times[0].as_secs_f64() / times[1].as_secs_f64()
                );
            }

            /// 13c-3: Full (extraction) batches on the free general slots.
            mod extraction {
                use super::*;

                const BIG: &str = "d00/Big.pas";
                const LIMITED: &str = "d01/Lim.pas";
                const OPEN: &str = "d02/U0002.pas";

                /// `shared_directories_project`'s shape with extraction's
                /// outcomes: every unit includes `Shared.inc`, every third
                /// uses its predecessor (a dependency load), `Big.pas` is
                /// over `maxFileBytes` (refused), `Lim.pas` includes itself
                /// (limited), and `U0002` is open with an edited buffer in
                /// the session.
                fn extraction_project(
                    root: &Path,
                    directories: usize,
                    units: usize,
                ) -> (Workspace, ContextKey, IndexAnchor) {
                    let names = directory_names(directories);
                    write(
                        &root.join(names.last().unwrap()).join("Shared.inc"),
                        "const Shared = 1;\n",
                    );
                    for index in 0..units {
                        let directory = &names[index % directories];
                        let uses = if index % 3 == 1 {
                            format!("uses U{:04};\n", index - 1)
                        } else {
                            String::new()
                        };
                        write(
                            &root.join(directory).join(format!("U{index:04}.pas")),
                            &format!(
                                "unit U{index:04};\ninterface\n{uses}{{$I Shared.inc}}\nprocedure P{index};\nimplementation\nprocedure P{index};\nbegin\nend;\nend.\n"
                            ),
                        );
                    }
                    write(
                        &root.join(BIG),
                        &format!(
                            "unit Big;\n{}\ninterface implementation end.\n",
                            "{ filler }\n".repeat(400)
                        ),
                    );
                    write(&root.join("d01/x.inc"), "{$I x.inc}\n");
                    write(
                        &root.join(LIMITED),
                        "unit Lim;\ninterface\n{$I x.inc}\nimplementation\nend.\n",
                    );
                    project(
                        root,
                        &format!(
                            "<DCC_UnitSearchPath>{}</DCC_UnitSearchPath><DCC_IncludePath>{}</DCC_IncludePath>",
                            names.join(";"),
                            names.join(";")
                        ),
                        "",
                    );
                    pascal_project::backdate_tree_for_test(root);
                    extraction_session(root)
                }

                fn directory_names(directories: usize) -> Vec<String> {
                    (0..directories)
                        .map(|index| format!("d{index:02}"))
                        .collect()
                }

                /// A new session (its own project cache, as a new process
                /// has) over `extraction_project`'s files.
                fn extraction_session(root: &Path) -> (Workspace, ContextKey, IndexAnchor) {
                    let mut options = WorkspaceOptions::default();
                    options.limits.max_file_bytes = 2_000;
                    let mut workspace = workspace(root, options);
                    let anchor = IndexAnchor::Project(root.join("App.dproj"));
                    let key = context(&mut workspace, anchor.clone());
                    let open = Url::from_file_path(root.join(OPEN)).unwrap();
                    workspace
                        .open_document(
                            open,
                            "unit U0002;\ninterface\nprocedure Edited;\nimplementation\nprocedure Edited;\nbegin\nend;\nend.\n".to_owned(),
                            1,
                        )
                        .unwrap();
                    (workspace, key, anchor)
                }

                #[derive(Debug, Default, PartialEq, Eq)]
                struct Sums {
                    indexed: usize,
                    refreshed: usize,
                    refused: usize,
                    limited: usize,
                    deferred: usize,
                    reused: usize,
                    reuse_missed: usize,
                    reuse_retrying: usize,
                    persisted: usize,
                    retries_attempted: usize,
                }

                impl Sums {
                    fn add(&mut self, progress: &IndexProgress) {
                        self.indexed += progress.indexed;
                        self.refreshed += progress.refreshed;
                        self.refused += progress.refused;
                        self.limited += progress.limited;
                        self.deferred += progress.deferred;
                        self.reused += progress.reused;
                        self.reuse_missed += progress.reuse_missed;
                        self.reuse_retrying += progress.reuse_retrying;
                        self.persisted += progress.persisted;
                        self.retries_attempted += progress.retries_attempted;
                    }
                }

                fn extraction_batch(
                    view: &mut Workspace,
                    anchor: &IndexAnchor,
                    bound: IndexBound,
                    threads: usize,
                    may_run: &(dyn Fn() -> bool + Sync),
                    cancel: &AtomicBool,
                ) -> Result<IndexProgress, String> {
                    view.advance_workspace_index_with(
                        anchor,
                        bound,
                        &mut || Ok(()),
                        Some(ParallelValidation {
                            threads,
                            may_run,
                            workers: &move || threads,
                        }),
                        cancel,
                    )
                }

                /// Full batches of 8 units until the index is complete.
                fn extract_all(
                    view: &mut Workspace,
                    anchor: &IndexAnchor,
                    threads: usize,
                ) -> (Sums, IndexProgress) {
                    let mut sums = Sums::default();
                    for _ in 0..64 {
                        let progress = extraction_batch(
                            view,
                            anchor,
                            IndexBound::units(8),
                            threads,
                            &|| true,
                            &AtomicBool::new(false),
                        )
                        .expect("batch");
                        sums.add(&progress);
                        if progress.complete {
                            return (sums, progress);
                        }
                        assert!(progress.worth_requeue(), "{progress:?}");
                    }
                    panic!("never complete: {sums:?}");
                }

                fn store_content(cache: &PersistentCache) -> Vec<String> {
                    cache
                        .job()
                        .expect("job")
                        .store()
                        .content_for_test()
                        .expect("content")
                }

                /// What a run leaves: its sums, the index and records, a
                /// few names' candidates, and the store's content.
                type Run = (Sums, Vec<String>, Vec<Vec<PathBuf>>, Vec<String>);

                fn run(root: &Path, store: &Path, threads: usize) -> Run {
                    let (session, key, anchor) = extraction_session(root);
                    let (key, anchor) = (&key, &anchor);
                    let (mut view, cache) = persisting_view(&session, store);
                    let (sums, _) = extract_all(&mut view, anchor, threads);
                    // Sealed, so the next process holds the snapshot and
                    // retries its refused and limited members last.
                    let _ = view.seal_baseline_if_due(key, &mut || Ok(()));
                    if threads > 1 {
                        assert!(
                            !take_validation_takes().is_empty(),
                            "the batches ran on workers"
                        );
                    }
                    let names = ["P0", "P1", "P3", "P7", "Edited", "Lim"]
                        .iter()
                        .map(|name| selected(&view, key, name))
                        .collect();
                    let run = (sums, snapshot(&view, key), names, store_content(&cache));
                    assert_eq!(view.project_cache.stats().reserved_facts_bytes, 0);
                    assert_eq!(view.project_cache.stats().reserved_publication_bytes, 0);
                    cache.shutdown();
                    run
                }

                /// The store, the index, the records and every outcome
                /// (refused, limited, an open member from disk, dependency
                /// loads) are the serial ones; so is a second process,
                /// which reuses and retries the limited member last.
                #[test]
                fn parallel_and_serial_extraction_give_the_same_index_records_and_store() {
                    const UNITS: usize = 30;
                    let temp = tempfile::tempdir().unwrap();
                    extraction_project(temp.path(), 6, UNITS);
                    let stores = [private_dir(), private_dir()];
                    let serial = run(temp.path(), stores[0].path(), 1);
                    let parallel = run(temp.path(), stores[1].path(), 3);
                    assert_eq!(serial.0.refused, 1, "{:?}", serial.0);
                    assert_eq!(serial.0.limited, 1, "{:?}", serial.0);
                    assert_eq!(serial.0.indexed, UNITS + 1, "{:?}", serial.0);
                    assert_eq!(serial.2[4], Vec::<PathBuf>::new(), "disk, not the buffer");
                    assert!(!serial.3.is_empty());
                    assert_eq!(serial.0, parallel.0);
                    assert_eq!(serial.1, parallel.1);
                    assert_eq!(serial.2, parallel.2);
                    assert_eq!(serial.3, parallel.3, "the same store content");
                    // A second process: reuse first, then the member
                    // retrying its earlier refusal, last.
                    let again = [
                        run(temp.path(), stores[0].path(), 1),
                        run(temp.path(), stores[1].path(), 3),
                    ];
                    assert!(again[0].0.reused >= UNITS, "{:?}", again[0].0);
                    assert_eq!(again[0].0.retries_attempted, 2, "{:?}", again[0].0);
                    assert_eq!(again[0], again[1]);
                }

                /// Without a store a Full batch stays serial: no worker
                /// starts, and the index is the serial one.
                #[test]
                fn without_a_store_extraction_stays_serial() {
                    let temp = tempfile::tempdir().unwrap();
                    let (_, key, anchor) = extraction_project(temp.path(), 4, 12);
                    let memory_only = || {
                        let (session, _, _) = extraction_session(temp.path());
                        let mut input = session.analysis_input().disk_baseline();
                        input.persistent_cache = Default::default();
                        Workspace::from_analysis_input(&input)
                    };
                    let (mut serial, mut parallel) = (memory_only(), memory_only());
                    let one = advance(&mut serial, &anchor, IndexBound::all());
                    let checks = AtomicUsize::new(0);
                    let two = extraction_batch(
                        &mut parallel,
                        &anchor,
                        IndexBound::all(),
                        3,
                        &|| {
                            checks.fetch_add(1, Ordering::SeqCst);
                            true
                        },
                        &AtomicBool::new(false),
                    )
                    .unwrap();
                    assert_eq!(one, two);
                    assert_eq!(checks.load(Ordering::SeqCst), 0, "no worker started");
                    assert!(take_validation_takes().is_empty());
                    assert_eq!(snapshot(&serial, &key), snapshot(&parallel, &key));
                }

                /// Interactive work stops the workers at their next member;
                /// nothing started after it is applied, and the next batches
                /// complete the index.
                #[test]
                fn interactive_work_stops_extraction_between_members() {
                    const UNITS: usize = 24;
                    const PASSING: usize = 4;
                    const THREADS: usize = 3;
                    let temp = tempfile::tempdir().unwrap();
                    let (session, key, anchor) = extraction_project(temp.path(), 4, UNITS);
                    let store = private_dir();
                    let (mut view, cache) = persisting_view(&session, store.path());
                    let checks = AtomicUsize::new(0);
                    let gate = || checks.fetch_add(1, Ordering::SeqCst) < PASSING;
                    let result = extraction_batch(
                        &mut view,
                        &anchor,
                        IndexBound::all(),
                        THREADS,
                        &gate,
                        &AtomicBool::new(false),
                    );
                    assert_eq!(result.unwrap_err(), CANCELLATION_MESSAGE);
                    let mut order = members(&view, &key);
                    order.sort_by_key(|path| path_key(path));
                    let fresh = order
                        .iter()
                        .filter(|path| state(&view, &key, path) == Some(MemberState::Fresh))
                        .count();
                    assert!(fresh <= PASSING, "{fresh} applied");
                    assert!(
                        order[PASSING + THREADS..]
                            .iter()
                            .all(|path| state(&view, &key, path) != Some(MemberState::Fresh)),
                        "a member past the stop was applied"
                    );
                    let (sums, progress) = extract_all(&mut view, &anchor, THREADS);
                    assert!(progress.complete, "{progress:?}");
                    // The units and `App.dpr`, each indexed once.
                    assert_eq!(sums.indexed + fresh, UNITS + 1, "{sums:?}");
                    cache.shutdown();
                }

                /// Fix round 1 (I1): an extraction worker's slot claims yield
                /// to interactive work, as the warmer's do: an interactive
                /// lookup of a slot a worker computes takes the claim over
                /// rather than waiting or computing it detached.
                #[test]
                fn an_interactive_lookup_takes_over_an_extraction_workers_claim() {
                    use crate::project_cache::{ClaimRole, Lookup, set_claim_role_on_this_thread};
                    let temp = tempfile::tempdir().unwrap();
                    let (session, key, anchor) = extraction_project(temp.path(), 4, 6);
                    let store = private_dir();
                    let (mut view, cache) = persisting_view(&session, store.path());
                    let context = Arc::clone(&session.contexts.get(&key).unwrap().context);
                    let project_cache = view.project_cache().clone();
                    let probe = Url::from_file_path(temp.path().join("Probe.pas")).unwrap();
                    let taken = Arc::new(std::sync::Mutex::new(None));
                    let (once, outcome) = (Arc::new(AtomicBool::new(false)), Arc::clone(&taken));
                    let batch = std::thread::current().id();
                    set_worker_extract_hook(Some(Arc::new(move |_, _| {
                        if std::thread::current().id() == batch || once.swap(true, Ordering::SeqCst)
                        {
                            return;
                        }
                        // The worker claims a slot it computes...
                        let no = AtomicBool::new(false);
                        let Lookup::Compute(claim) =
                            project_cache.unit(&probe, &context, 1, &HashMap::new(), &no)
                        else {
                            panic!("the worker claims the probe slot");
                        };
                        assert!(!claim.is_detached());
                        // ...and an interactive lookup meets the claim.
                        let (cache, context, probe) =
                            (project_cache.clone(), Arc::clone(&context), probe.clone());
                        let detached = std::thread::spawn(move || {
                            set_claim_role_on_this_thread(ClaimRole::interactive(
                                Arc::new(AtomicBool::new(false)),
                                Duration::ZERO,
                            ));
                            match cache.unit(
                                &probe,
                                &context,
                                1,
                                &HashMap::new(),
                                &AtomicBool::new(false),
                            ) {
                                Lookup::Compute(claim) => Some(claim.is_detached()),
                                _ => None,
                            }
                        })
                        .join()
                        .unwrap();
                        *outcome.lock().unwrap() = Some(detached);
                        drop(claim);
                    })));
                    let progress = extraction_batch(
                        &mut view,
                        &anchor,
                        IndexBound::all(),
                        2,
                        &|| true,
                        &AtomicBool::new(false),
                    );
                    set_worker_extract_hook(None);
                    assert!(progress.unwrap().complete);
                    assert_eq!(
                        *taken.lock().unwrap(),
                        Some(Some(false)),
                        "the interactive lookup took the worker's claim over"
                    );
                    cache.shutdown();
                }

                /// Fix round 1 (I2), M2 on the extraction path: a member a
                /// worker extracted, rewritten (another size) and invalidated
                /// before the batch applies it, is extracted again by the
                /// batch: the index and the record hold the new content, and
                /// the store holds what a serial run of the new tree does.
                #[test]
                fn an_event_after_a_workers_extraction_is_extracted_again_by_the_batch() {
                    let temp = tempfile::tempdir().unwrap();
                    let (_, key, anchor) = extraction_project(temp.path(), 4, 12);
                    let edited = temp.path().join("d03/U0003.pas");
                    let (session, _, _) = extraction_session(temp.path());
                    let store = private_dir();
                    let (mut view, cache) = persisting_view(&session, store.path());
                    let project_cache = view.project_cache().clone();
                    let target = edited.clone();
                    let rewritten = Rc::new(Cell::new(false));
                    let flag = Rc::clone(&rewritten);
                    set_before_apply_hook(Some(Box::new(move |path| {
                        if path == target && !flag.replace(true) {
                            write(
                                &target,
                                "unit U0003;\ninterface\n{$I Shared.inc}\nprocedure Rewritten3;\nimplementation\nprocedure Rewritten3;\nbegin\nend;\nend.\n",
                            );
                            project_cache.invalidate_path(&target);
                        }
                    })));
                    let progress = extraction_batch(
                        &mut view,
                        &anchor,
                        IndexBound::all(),
                        3,
                        &|| true,
                        &AtomicBool::new(false),
                    );
                    set_before_apply_hook(None);
                    let progress = progress.unwrap();
                    assert!(rewritten.get());
                    assert!(
                        !take_validation_takes().is_empty(),
                        "the batch ran on workers"
                    );
                    assert!(progress.complete, "{progress:?}");
                    assert_eq!(state(&view, &key, &edited), Some(MemberState::Fresh));
                    assert_eq!(selected(&view, &key, "Rewritten3"), vec![edited.clone()]);
                    assert!(selected(&view, &key, "P3").is_empty());
                    assert!(
                        records(&view, &key)
                            .iter()
                            .any(|(path, record)| *path == edited && record.is_some()),
                        "the new content is recorded"
                    );
                    let parallel = store_content(&cache);
                    cache.shutdown();
                    // A serial run of the rewritten tree publishes the same
                    // store: nothing of the old bytes was published.
                    let (session, _, _) = extraction_session(temp.path());
                    let other = private_dir();
                    let (mut serial, cache) = persisting_view(&session, other.path());
                    assert!(advance(&mut serial, &anchor, IndexBound::all()).complete);
                    assert_eq!(parallel, store_content(&cache));
                    cache.shutdown();
                }

                /// A member whose extraction is in flight when interactive
                /// work arrives is cancelled there (its session sees the
                /// trip), not run to its end: it stays pending.
                #[test]
                fn interactive_work_cancels_the_extraction_in_flight() {
                    let temp = tempfile::tempdir().unwrap();
                    let (session, key, anchor) = extraction_project(temp.path(), 4, 12);
                    let store = private_dir();
                    let (mut view, cache) = persisting_view(&session, store.path());
                    let closed = Arc::new(AtomicBool::new(false));
                    let tripped = Arc::new(AtomicBool::new(false));
                    let first = Arc::new(std::sync::Mutex::new(None));
                    let (gate, seen, target) = (
                        Arc::clone(&closed),
                        Arc::clone(&tripped),
                        Arc::clone(&first),
                    );
                    // The first extraction to start closes the gate, then
                    // waits (bounded, no sleep) until its load's flag trips.
                    set_worker_extract_hook(Some(Arc::new(move |path, cancel| {
                        if gate.swap(true, Ordering::SeqCst) {
                            return;
                        }
                        *target.lock().unwrap() = Some(path.to_path_buf());
                        let started = Instant::now();
                        while !cancel.load(Ordering::SeqCst)
                            && started.elapsed() < Duration::from_secs(30)
                        {
                            std::thread::yield_now();
                        }
                        seen.store(cancel.load(Ordering::SeqCst), Ordering::SeqCst);
                    })));
                    let result = extraction_batch(
                        &mut view,
                        &anchor,
                        IndexBound::all(),
                        2,
                        &|| !closed.load(Ordering::SeqCst),
                        &AtomicBool::new(false),
                    );
                    set_worker_extract_hook(None);
                    assert_eq!(result.unwrap_err(), CANCELLATION_MESSAGE);
                    assert!(tripped.load(Ordering::SeqCst), "the load saw the trip");
                    let first = first
                        .lock()
                        .unwrap()
                        .clone()
                        .expect("an extraction started");
                    assert_eq!(state(&view, &key, &first), Some(MemberState::Unindexed));
                    let (_, progress) = extract_all(&mut view, &anchor, 2);
                    assert!(progress.complete, "{progress:?}");
                    assert_ne!(state(&view, &key, &first), Some(MemberState::Unindexed));
                    cache.shutdown();
                }

                /// Cancellation of the batch stops the workers too.
                #[test]
                fn cancellation_stops_parallel_extraction() {
                    let temp = tempfile::tempdir().unwrap();
                    let (session, key, anchor) = extraction_project(temp.path(), 4, 16);
                    let store = private_dir();
                    let (mut view, cache) = persisting_view(&session, store.path());
                    let cancel = AtomicBool::new(false);
                    let checks = AtomicUsize::new(0);
                    let gate = || {
                        if checks.fetch_add(1, Ordering::SeqCst) == 1 {
                            cancel.store(true, Ordering::SeqCst);
                        }
                        true
                    };
                    let result =
                        extraction_batch(&mut view, &anchor, IndexBound::all(), 3, &gate, &cancel);
                    assert_eq!(result.unwrap_err(), CANCELLATION_MESSAGE);
                    let fresh = members(&view, &key)
                        .iter()
                        .filter(|path| state(&view, &key, path) == Some(MemberState::Fresh))
                        .count();
                    assert!(fresh <= 2, "{fresh} applied after the cancellation");
                    cache.shutdown();
                }

                /// Each worker's view has its own per-member deadline: the
                /// slow member is limited (over its deadline) and the
                /// members extracted beside it are not.
                #[test]
                fn each_worker_extracts_under_its_own_member_deadline() {
                    let temp = tempfile::tempdir().unwrap();
                    let (session, key, anchor) = extraction_project(temp.path(), 4, 12);
                    let slow = temp.path().join("d03/U0003.pas");
                    let store = private_dir();
                    let (mut view, cache) = persisting_view(&session, store.path());
                    let _slow = slow_member_for_test(&slow);
                    let (sums, progress) = extract_all(&mut view, &anchor, 3);
                    assert!(progress.complete, "{progress:?}");
                    assert_eq!(sums.limited, 2, "Lim and the slow member: {sums:?}");
                    assert_eq!(state(&view, &key, &slow), Some(MemberState::Limited));
                    let status = view.project_cache.index_status(&|_| false);
                    assert_eq!(status[0].counts.over_deadline, 1, "{:?}", status[0]);
                    let neighbour = temp.path().join("d00/U0004.pas");
                    assert_eq!(state(&view, &key, &neighbour), Some(MemberState::Fresh));
                    cache.shutdown();
                }

                /// 13d-2: a member over `WORKER_MEMBER_BYTES` is extracted by
                /// the batch's thread, as the serial loop (and a run without
                /// a store) does, so no worker holds the transient memory
                /// of a large member, in flight or kept free by its thread's
                /// allocator arena; the others still run on workers, and the
                /// index and store are the serial ones.
                #[test]
                fn a_member_over_the_worker_bound_is_extracted_by_the_batch() {
                    let temp = tempfile::tempdir().unwrap();
                    let root = temp.path();
                    extraction_project(root, 4, 12);
                    let large = root.join("d01/Large.pas");
                    write(
                        &large,
                        &format!(
                            "unit Large;\ninterface\n{{$I Shared.inc}}\nprocedure PLarge;\nimplementation\n{}procedure PLarge;\nbegin\nend;\nend.\n",
                            "{ filler }\n".repeat(WORKER_MEMBER_BYTES as usize / 10 + 1)
                        ),
                    );
                    pascal_project::backdate_tree_for_test(root);
                    let anchor = IndexAnchor::Project(root.join("App.dproj"));
                    let run = |threads: usize, store: &Path| {
                        let mut session = workspace(root, WorkspaceOptions::default());
                        let key = context(&mut session, anchor.clone());
                        let (mut view, cache) = persisting_view(&session, store);
                        let threads_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
                        let seen = Arc::clone(&threads_seen);
                        set_worker_extract_hook(Some(Arc::new(move |path, _| {
                            let name = std::thread::current().name().map(str::to_owned);
                            seen.lock().unwrap().push((path.to_path_buf(), name));
                        })));
                        let mut sums = Sums::default();
                        let mut left = (0, 0);
                        loop {
                            let progress = extraction_batch(
                                &mut view,
                                &anchor,
                                IndexBound::units(8),
                                threads,
                                &|| true,
                                &AtomicBool::new(false),
                            )
                            .expect("batch");
                            sums.add(&progress);
                            left.0 += progress.left_large;
                            left.1 += progress.left_large_bytes;
                            if progress.complete {
                                break;
                            }
                        }
                        set_worker_extract_hook(None);
                        let outcome = (sums, snapshot(&view, &key), store_content(&cache));
                        cache.shutdown();
                        let seen = std::mem::take(&mut *threads_seen.lock().unwrap());
                        (outcome, seen, left)
                    };
                    let stores = [private_dir(), private_dir()];
                    let (serial, _, serial_left) = run(1, stores[0].path());
                    let (parallel, seen, left) = run(3, stores[1].path());
                    // Counted where a worker left it, never by the serial loop.
                    assert_eq!(serial_left, (0, 0));
                    assert_eq!(left, (1, std::fs::metadata(&large).unwrap().len()));
                    let this = std::thread::current().name().map(str::to_owned);
                    let on = |path: &Path| {
                        seen.iter()
                            .filter(|(seen, _)| seen == path)
                            .map(|(_, thread)| thread.clone())
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(on(&large), vec![this.clone()], "{seen:?}");
                    assert!(
                        seen.iter().any(|(_, thread)| thread != &this),
                        "the other members ran on workers: {seen:?}"
                    );
                    assert!(serial.0.indexed >= 13, "{:?}", serial.0);
                    assert_eq!(serial, parallel);
                }

                /// The window bounds what the workers hold: at most one
                /// prepared publication (and its reservation) per thread.
                #[test]
                fn parallel_extraction_holds_at_most_one_publication_per_thread() {
                    const THREADS: usize = 3;
                    let temp = tempfile::tempdir().unwrap();
                    let (session, _key, anchor) = extraction_project(temp.path(), 4, 24);
                    let store = private_dir();
                    let (mut view, cache) = persisting_view(&session, store.path());
                    let progress = extraction_batch(
                        &mut view,
                        &anchor,
                        IndexBound::all(),
                        THREADS,
                        &|| true,
                        &AtomicBool::new(false),
                    )
                    .unwrap();
                    assert!(progress.complete, "{progress:?}");
                    let peak = take_validation_peak();
                    assert!(peak.held >= 1 && peak.held <= THREADS, "{peak:?}");
                    assert!(
                        peak.reserved_bytes <= THREADS * pascal_cache::MAX_ARTIFACT_CONTENT_BYTES,
                        "{peak:?}"
                    );
                    let stats = view.project_cache.stats();
                    assert_eq!(stats.reserved_publication_bytes, 0);
                    assert_eq!(stats.reserved_facts_bytes, 0);
                    cache.shutdown();
                }

                /// Release measurement (report-only):
                /// TMPDIR=$HOME/.cache/l4d-tmp cargo test -p pascal-lsp --release --lib parallel_extraction_speedup -- --ignored --nocapture
                #[test]
                #[ignore = "release measurement, run explicitly"]
                fn parallel_extraction_speedup_on_a_synthetic_project() {
                    const DIRECTORIES: usize = 40;
                    const UNITS: usize = 600;
                    let temp = tempfile::tempdir().unwrap();
                    extraction_project(temp.path(), DIRECTORIES, UNITS);
                    let mut times = Vec::new();
                    let free = crate::server::validation_threads(0);
                    for threads in [1, free, 3] {
                        // A new process each: no shared cache entry.
                        let (session, _, anchor) = extraction_session(temp.path());
                        let store = private_dir();
                        let (mut view, cache) = persisting_view(&session, store.path());
                        let started = Instant::now();
                        let mut batches = 0;
                        loop {
                            let progress = extraction_batch(
                                &mut view,
                                &anchor,
                                IndexBound::extraction(),
                                threads,
                                &|| true,
                                &AtomicBool::new(false),
                            )
                            .unwrap();
                            batches += 1;
                            if progress.complete || !progress.worth_requeue() {
                                break;
                            }
                        }
                        let elapsed = started.elapsed();
                        eprintln!(
                            "13c-3 extraction: {UNITS} members over {DIRECTORIES} directories, \
                             {threads} threads: {elapsed:?} in {batches} batches"
                        );
                        times.push(elapsed);
                        cache.shutdown();
                    }
                    for (index, threads) in [(1, free), (2, 3)] {
                        eprintln!(
                            "13c-3 extraction: speedup with {threads} threads {:.2}x",
                            times[0].as_secs_f64() / times[index].as_secs_f64()
                        );
                    }
                }

                /// A multidev-shaped project (13d-2): `core` provider units
                /// in chains of five (each reads its predecessor's constant in
                /// a conditional) spread over the directories, and `units`
                /// members, each reading eight providers' constants in
                /// conditionals and using a member in another directory,
                /// with a shared and a per-directory include.
                fn multidev_shaped_project(
                    root: &Path,
                    directories: usize,
                    core: usize,
                    units: usize,
                ) {
                    // Providers per member, and the length of each chain.
                    const PROVIDERS: usize = 8;
                    const CHAIN: usize = 5;
                    const MEMBER_METHODS: usize = 120;
                    const HEAVY_EVERY: usize = 40;
                    const HEAVY_METHODS: usize = 5_000;
                    let names = directory_names(directories);
                    write(
                        &root.join(names.last().unwrap()).join("Shared.inc"),
                        "const Shared = 1;\n",
                    );
                    for name in &names {
                        write(
                            &root.join(name).join(format!("{name}.inc")),
                            &format!("const Dir_{name} = 2;\n"),
                        );
                    }
                    let methods = |prefix: &str, count: usize| {
                        let declarations = (0..count)
                            .map(|m| {
                                format!(
                                    "    procedure {prefix}M{m}(A: Integer; const B: string);\n"
                                )
                            })
                            .collect::<String>();
                        let bodies = (0..count)
                            .map(|m| {
                                format!(
                                    "procedure T{prefix}.{prefix}M{m}(A: Integer; const B: string);\nvar X: Integer;\nbegin\n  X := A + {m};\n  if X > 1 then X := X * 2;\nend;\n"
                                )
                            })
                            .collect::<String>();
                        (declarations, bodies)
                    };
                    for index in 0..core {
                        let directory = &names[index % directories];
                        let uses = if index % CHAIN > 0 {
                            format!("uses C{:03};\n", index - 1)
                        } else {
                            String::new()
                        };
                        let prefix = format!("C{index:03}");
                        let (declarations, bodies) = methods(&prefix, 30);
                        let functions = (0..20)
                            .map(|f| format!("function {prefix}F{f}(A: Integer): Integer;\n"))
                            .collect::<String>();
                        let function_bodies = (0..20)
                            .map(|f| {
                                format!("function {prefix}F{f}(A: Integer): Integer;\nbegin\n  Result := A + {f};\nend;\n")
                            })
                            .collect::<String>();
                        // A provider chain: each interface's conditional
                        // reads its predecessor's constant.
                        let chain = if index % CHAIN > 0 {
                            format!(
                                "{{$IF C{0:03}.Flag{0:03} >= 0}}\nconst Chain{index:03} = 1;\n{{$ENDIF}}\n",
                                index - 1
                            )
                        } else {
                            String::new()
                        };
                        let constants = (0..200)
                            .map(|k| format!("  {prefix}K{k} = {k};\n"))
                            .collect::<String>();
                        write(
                            &root.join(directory).join(format!("{prefix}.pas")),
                            &format!(
                                "unit {prefix};\ninterface\n{uses}{{$I Shared.inc}}\n{chain}const\n  Flag{prefix_number:03} = {index};\n{constants}type\n  T{prefix} = class\n  public\n    FValue: Integer;\n{declarations}  end;\n{functions}implementation\n{bodies}{function_bodies}end.\n",
                                prefix_number = index
                            ),
                        );
                    }
                    for index in 0..units {
                        let directory = &names[index % directories];
                        let mut providers = (0..PROVIDERS)
                            .map(|n| (index * (2 * n + 1) + n * 17) % core)
                            .collect::<Vec<_>>();
                        providers.sort_unstable();
                        providers.dedup();
                        let mut uses = providers
                            .iter()
                            .map(|provider| format!("C{provider:03}"))
                            .collect::<Vec<_>>();
                        if index >= directories {
                            uses.push(format!("U{:04}", index - directories));
                        }
                        let prefix = format!("U{index:04}");
                        // A few heavy members, as multidev's large units.
                        let count = if index % HEAVY_EVERY == HEAVY_EVERY - 1 {
                            HEAVY_METHODS
                        } else {
                            MEMBER_METHODS
                        };
                        let (declarations, bodies) = methods(&prefix, count);
                        let conditionals = providers
                            .iter()
                            .map(|provider| {
                                format!(
                                    "{{$IF C{provider:03}.Flag{provider:03} >= 0}}\nconst {prefix}From{provider:03} = 1;\n{{$ENDIF}}\n"
                                )
                            })
                            .collect::<String>();
                        write(
                            &root.join(directory).join(format!("{prefix}.pas")),
                            &format!(
                                "unit {prefix};\ninterface\nuses {};\n{{$I Shared.inc}}\n{{$I {directory}.inc}}\n{conditionals}type\n  T{prefix} = class\n{declarations}  end;\nimplementation\n{bodies}end.\n",
                                uses.join(", ")
                            ),
                        );
                    }
                    project(
                        root,
                        &format!(
                            "<DCC_UnitSearchPath>{}</DCC_UnitSearchPath><DCC_IncludePath>{}</DCC_IncludePath>",
                            names.join(";"),
                            names.join(";")
                        ),
                        "",
                    );
                    pascal_project::backdate_tree_for_test(root);
                }

                /// Peak and retained heap of one indexing run, over the
                /// live heap before it.
                #[derive(Debug)]
                struct HeapRun {
                    peak: isize,
                    /// The largest batch peak over the live heap before it.
                    transient: isize,
                    retained: isize,
                    batches: usize,
                    sums: Sums,
                }

                /// Indexes the whole context in production batches
                /// (`IndexBound::extraction()` or the reuse pass), each on a
                /// new view as the warmer runs them.
                fn heap_run(
                    session: &Workspace,
                    anchor: &IndexAnchor,
                    cache: Option<&PersistentCache>,
                    mode: IndexMode,
                    threads: usize,
                ) -> HeapRun {
                    let base = crate::heap_count::live();
                    let mut overall = base;
                    let mut sums = Sums::default();
                    let mut batches = 0;
                    let bound = match mode {
                        IndexMode::Full => IndexBound::extraction(),
                        IndexMode::Reuse => IndexBound::reuse_pass(),
                    };
                    let mut transient = 0;
                    loop {
                        let before = crate::heap_count::live();
                        crate::heap_count::reset_peak();
                        let mut view = match cache {
                            Some(cache) => view_with(session, cache),
                            None => {
                                let mut input = session.analysis_input().disk_baseline();
                                input.persistent_cache = Default::default();
                                Workspace::from_analysis_input(&input)
                            }
                        };
                        let progress = extraction_batch(
                            &mut view,
                            anchor,
                            bound,
                            threads,
                            &|| true,
                            &AtomicBool::new(false),
                        )
                        .unwrap();
                        let freed = crate::heap_count::freed_during(|| drop(view));
                        note_view_freed("batch", freed, progress.indexed + progress.reused);
                        // No maintenance runs in this harness: checkpoint
                        // as it would, so no publication meets WalPressure.
                        if let Some(job) = cache.and_then(PersistentCache::job) {
                            let _ = job.store().checkpoint();
                        }
                        sums.add(&progress);
                        batches += 1;
                        transient = transient.max(crate::heap_count::peak() - before);
                        overall = overall.max(crate::heap_count::peak());
                        if progress.complete || !progress.worth_requeue() {
                            break;
                        }
                    }
                    let views = take_view_freed();
                    for kind in ["batch", "worker"] {
                        let of_kind = views.iter().filter(|view| view.0 == kind);
                        let (count, bytes, members, max) =
                            of_kind.fold((0, 0, 0, 0), |(count, bytes, members, max), view| {
                                (count + 1, bytes + view.1, members + view.2, max.max(view.1))
                            });
                        if count > 0 {
                            eprintln!(
                                "13d-2 heap:   {count} {kind} views: mean {:.2} MiB, max {:.2} MiB, {:.1} KiB per member",
                                bytes as f64 / count as f64 / 1048576.0,
                                max as f64 / 1048576.0,
                                bytes as f64 / members.max(1) as f64 / 1024.0
                            );
                        }
                    }
                    HeapRun {
                        peak: overall - base,
                        transient,
                        retained: crate::heap_count::live() - base,
                        batches,
                        sums,
                    }
                }

                /// Release measurement (report-only, 13d-2):
                /// TMPDIR=$HOME/.cache/l4d-tmp cargo test -p pascal-lsp --release --lib indexing_heap_by_configuration -- --ignored --nocapture --test-threads=1
                #[test]
                #[ignore = "release measurement, run explicitly"]
                fn indexing_heap_by_configuration() {
                    const DIRECTORIES: usize = 40;
                    const CORE: usize = 120;
                    const UNITS: usize = 1_200;
                    crate::heap_count::start();
                    let temp = tempfile::tempdir().unwrap();
                    multidev_shaped_project(temp.path(), DIRECTORIES, CORE, UNITS);
                    let anchor = IndexAnchor::Project(temp.path().join("App.dproj"));
                    let session = || {
                        let mut workspace = workspace(temp.path(), WorkspaceOptions::default());
                        context(&mut workspace, anchor.clone());
                        workspace
                    };
                    let mib = |bytes: isize| bytes as f64 / (1024.0 * 1024.0);
                    let report = |name: &str, run: &HeapRun| {
                        eprintln!(
                            "13d-2 heap: {name}: peak {:.1} MiB, batch transient {:.1} MiB, retained {:.1} MiB, {} batches, {:?}",
                            mib(run.peak),
                            mib(run.transient),
                            mib(run.retained),
                            run.batches,
                            run.sums
                        );
                    };
                    // One configuration per process (`L4D_13D2_CONFIG` = off,
                    // serial, parallel or reuse), for its RSS; all by default.
                    let only = std::env::var("L4D_13D2_CONFIG").ok();
                    let wanted = |name: &str| only.as_deref().is_none_or(|only| only == name);
                    let rss = |field: &str| {
                        std::fs::read_to_string("/proc/self/status")
                            .unwrap()
                            .lines()
                            .find_map(|line| line.strip_prefix(field).map(str::to_owned))
                            .unwrap_or_default()
                            .trim()
                            .to_owned()
                    };
                    eprintln!(
                        "13d-2 heap: before: VmRSS {}, VmHWM {}",
                        rss("VmRSS:"),
                        rss("VmHWM:")
                    );
                    let threads = 2;
                    if wanted("off") {
                        let session = session();
                        let run = heap_run(&session, &anchor, None, IndexMode::Full, threads);
                        report("serial, persistence off", &run);
                    }
                    if wanted("serial") {
                        let session = session();
                        let store = private_dir();
                        let (_, cache) = persisting_view(&session, store.path());
                        let run = heap_run(&session, &anchor, Some(&cache), IndexMode::Full, 1);
                        report("serial, persistence on", &run);
                        cache.shutdown();
                    }
                    let store = private_dir();
                    if wanted("parallel") || wanted("reuse") {
                        let session = session();
                        let (_, cache) = persisting_view(&session, store.path());
                        let run =
                            heap_run(&session, &anchor, Some(&cache), IndexMode::Full, threads);
                        if wanted("parallel") {
                            report("parallel (2), persistence on", &run);
                        }
                        cache.shutdown();
                    }
                    if wanted("reuse") {
                        if only.is_some() {
                            eprintln!(
                                "13d-2 heap: after the extraction that fills the store: VmRSS {}, VmHWM {}",
                                rss("VmRSS:"),
                                rss("VmHWM:")
                            );
                        }
                        let session = session();
                        let (_, cache) = persisting_view(&session, store.path());
                        let run =
                            heap_run(&session, &anchor, Some(&cache), IndexMode::Reuse, threads);
                        report("reuse only (2)", &run);
                        cache.shutdown();
                    }
                    eprintln!(
                        "13d-2 heap: after: VmRSS {}, VmHWM {}, live {:.1} MiB",
                        rss("VmRSS:"),
                        rss("VmHWM:"),
                        mib(crate::heap_count::live())
                    );
                    // What the allocator keeps free in its arenas.
                    #[cfg(all(target_os = "linux", target_env = "gnu"))]
                    {
                        // SAFETY: malloc_trim only returns free pages to the system.
                        unsafe {
                            libc::malloc_trim(0);
                        }
                        eprintln!("13d-2 heap: after malloc_trim: VmRSS {}", rss("VmRSS:"));
                    }
                }
            }
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

        /// The open-file crawl of `open` in `session`'s own view, with its
        /// overlays, under `cache`, as the warmer runs it (R8-2/D42).
        fn crawl_view(session: &Workspace, cache: &PersistentCache, persists: bool) -> Workspace {
            let mut input = session.analysis_input();
            input.persistent_cache = cache.clone();
            let mut crawl = Workspace::from_analysis_input(&input);
            crawl.set_crawl_persists(persists);
            crawl
        }

        fn crawl(session: &Workspace, cache: &PersistentCache, open: &Url) -> Workspace {
            let mut crawl = crawl_view(session, cache, true);
            crawl
                .warm_with_cancel(open, &AtomicBool::new(false))
                .expect("crawl");
            crawl
        }

        const ROOT_UNITS: usize = 6;

        /// `lib/Root.pas`, whose interface uses `U0..U5`, in the project of
        /// `indexed_project`, configured; `Root` is open, as saved.
        fn crawled_project(root: &Path) -> (Workspace, ContextKey, IndexAnchor, Url) {
            let uses = (0..ROOT_UNITS)
                .map(|index| format!("U{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let text = format!("unit Root;\ninterface\nuses {uses};\nimplementation\nend.\n");
            write(&root.join("lib/Root.pas"), &text);
            let (mut session, key, anchor) = indexed_project(
                root,
                ROOT_UNITS + 1,
                WorkspaceOptions {
                    project_file: Some(root.join("App.dproj")),
                    ..WorkspaceOptions::default()
                },
            );
            let open = Url::from_file_path(root.join("lib/Root.pas")).unwrap();
            session.open_document(open.clone(), text, 1).unwrap();
            (session, key, anchor, open)
        }

        fn units(root: &Path, count: usize) -> Vec<PathBuf> {
            (0..count)
                .map(|index| root.join(format!("lib/U{index}.pas")))
                .collect()
        }

        fn fresh(workspace: &Workspace, key: &ContextKey) -> Vec<PathBuf> {
            let mut fresh = members(workspace, key)
                .into_iter()
                .filter(|path| state(workspace, key, path) == Some(MemberState::Fresh))
                .collect::<Vec<_>>();
            fresh.sort();
            fresh
        }

        fn sorted(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
            paths.sort();
            paths
        }

        #[test]
        fn a_crawl_publishes_the_disk_units_it_analyses_for_a_restart() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let (session, key, anchor, open) = crawled_project(root);
            let cache = start(&session, store.path(), true);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            let crawled = crawl(&session, &cache, &open);
            assert!(crawled.index.parsed_document(&open).is_some());
            assert_eq!(
                sorted(session.project_cache.published_by_crawl_for_test()),
                units(root, ROOT_UNITS),
                "every disk unit, never the open root"
            );
            cache.shutdown();

            let (mut restarted, cache) = restarted_view(root, store.path());
            crate::navigation::test_reset_document_parse_count();
            let progress = advance(
                &mut restarted,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert_eq!(crate::navigation::test_document_parse_count(), 0);
            assert_eq!(progress.reused, ROOT_UNITS, "{progress:?}");
            assert_eq!(fresh(&restarted, &key), units(root, ROOT_UNITS));
            assert_eq!(recorded(&restarted, &key).len(), ROOT_UNITS);
            cache.shutdown();
        }

        /// Holds every byte of `cache`'s budget that pins, leases and held
        /// reservations leave, standing in for the crawl's pins and leases
        /// of Task 13 R1 (A2): no ordinary reservation fits until it is
        /// dropped.
        fn fill_parse_budget(
            cache: &crate::project_cache::ProjectCache,
        ) -> crate::project_cache::Reservation {
            let stats = cache.stats();
            let used = stats
                .pinned_bytes
                .saturating_add(stats.outstanding_bytes)
                .saturating_add(stats.reserved_bytes())
                // Held from the publication pool, outside the budget.
                .saturating_sub(stats.reserved_publication_bytes);
            let filler = cache
                .reserve(
                    crate::project_cache::ReservationKind::Facts,
                    cache.max_bytes() - used,
                )
                .expect("the rest of the budget");
            assert!(
                cache
                    .reserve(crate::project_cache::ReservationKind::Scratch, 1)
                    .is_none()
            );
            filler
        }

        /// 13b-3 (A2): a parse budget with no room left (held here by one
        /// reservation, by the crawl's pins and leases in Task 13 R1) does
        /// not stop the crawl publishing: a publication's transients draw
        /// on their own pool.
        #[test]
        fn a_crawl_publishes_while_the_parse_budget_is_full() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let (session, _key, anchor, open) = crawled_project(root);
            let cache = start(&session, store.path(), true);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            let filler = fill_parse_budget(&session.project_cache);
            crawl(&session, &cache, &open);
            drop(filler);
            assert_eq!(
                sorted(session.project_cache.published_by_crawl_for_test()),
                units(root, ROOT_UNITS),
            );
            let counts = session.project_cache.index_process_counts();
            assert_eq!(
                (counts.crawl_published, counts.crawl_deferred),
                (ROOT_UNITS as u64, 0),
                "{counts:?}"
            );
            cache.shutdown();
            let (mut restarted, cache) = restarted_view(root, store.path());
            let progress = advance(
                &mut restarted,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert_eq!(progress.reused, ROOT_UNITS, "{progress:?}");
            cache.shutdown();
        }

        /// 13b-3 (A2): with no room even in the publication pool, the crawl
        /// defers each unit to the index batch, counted once per job and
        /// not prepared again by it, instead of dropping it: the next Full
        /// batch extracts and publishes every one.
        #[test]
        fn a_refused_crawl_publication_is_published_by_the_next_full_batch() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let (session, key, anchor, open) = crawled_project(root);
            let (mut view, cache) = persisting_view(&session, store.path());
            let pool = session
                .project_cache
                .reserve_publication(session.project_cache.publication_pool_bytes())
                .expect("the whole pool");
            let filler = fill_parse_budget(&session.project_cache);
            let crawled = crawl(&session, &cache, &open);
            drop((filler, pool));
            assert!(
                session
                    .project_cache
                    .published_by_crawl_for_test()
                    .is_empty()
            );
            let counts = session.project_cache.index_process_counts();
            assert_eq!(
                (counts.crawl_published, counts.crawl_deferred),
                (0, ROOT_UNITS as u64),
                "{counts:?}"
            );
            // Each unit is deferred once: the index batch is its queue, so
            // the job does not prepare it again on every edge to it.
            for unit in units(root, ROOT_UNITS) {
                let uri = Url::from_file_path(&unit).unwrap();
                assert!(
                    crawled.crawl_considered.contains(&uri),
                    "{} is not tried again by this job",
                    unit.display()
                );
            }

            let loaded = record_loads();
            let progress = advance(&mut view, &anchor, IndexBound::all());
            set_after_load_hook(None);
            assert!(progress.complete, "{progress:?}");
            let mut expected = units(root, ROOT_UNITS);
            expected.extend([root.join("App.dpr"), root.join("lib/Root.pas")]);
            assert_eq!(sorted(loaded.borrow().clone()), sorted(expected));
            assert_eq!(recorded(&view, &key).len(), ROOT_UNITS + 2);
            cache.shutdown();

            let (mut restarted, cache) = restarted_view(root, store.path());
            crate::navigation::test_reset_document_parse_count();
            let progress = advance(
                &mut restarted,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert_eq!(crate::navigation::test_document_parse_count(), 0);
            assert_eq!(progress.reused, ROOT_UNITS + 2, "{progress:?}");
            cache.shutdown();
        }

        #[test]
        fn a_crawl_without_persistence_publishes_nothing() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let (session, _key, anchor, open) = crawled_project(root);
            let cache = start(&session, store.path(), true);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            let mut crawled = crawl_view(&session, &cache, false);
            crawled
                .warm_with_cancel(&open, &AtomicBool::new(false))
                .expect("crawl");
            assert!(
                session
                    .project_cache
                    .published_by_crawl_for_test()
                    .is_empty()
            );
            assert!(!crawled.crawl_private_signal().load(Ordering::Relaxed));
            cache.shutdown();
            let (mut restarted, cache) = restarted_view(root, store.path());
            let progress = advance(
                &mut restarted,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert_eq!(progress.reused, 0, "{progress:?}");
            cache.shutdown();

            // A disabled store: the crawl flag alone publishes nothing.
            let other_root = tempfile::tempdir().unwrap();
            let other_store = private_dir();
            let (other, _, _, other_open) = crawled_project(other_root.path());
            let disabled = start(&other, other_store.path(), false);
            let crawled = crawl(&other, &disabled, &other_open);
            assert!(crawled.index.parsed_document(&other_open).is_some());
            assert!(other.project_cache.published_by_crawl_for_test().is_empty());
        }

        #[test]
        fn a_crawl_never_publishes_an_open_or_overlay_dependent_unit() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let root_text = "unit Root;\ninterface\nuses Main, Other;\nimplementation\nend.\n";
            write(&root.join("lib/Root.pas"), root_text);
            named_unit(&root.join("lib/Other.pas"), "FromOther");
            let (mut session, key, anchor) = provider_project_with(
                root,
                WorkspaceOptions {
                    project_file: Some(root.join("App.dproj")),
                    ..WorkspaceOptions::default()
                },
            );
            let (shapes, main, other) = (
                root.join("lib/Shapes.pas"),
                root.join("lib/Main.pas"),
                root.join("lib/Other.pas"),
            );
            let open = Url::from_file_path(root.join("lib/Root.pas")).unwrap();
            session
                .open_document(open.clone(), root_text.to_owned(), 1)
                .unwrap();
            session
                .open_document(
                    Url::from_file_path(&shapes).unwrap(),
                    EDITED_SHAPES.to_owned(),
                    1,
                )
                .unwrap();
            let cache = start(&session, store.path(), true);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            let crawled = crawl(&session, &cache, &open);
            for path in [&main, &shapes, &other] {
                let uri = Url::from_file_path(path).unwrap();
                assert!(
                    crawled.index.parsed_document(&uri).is_some(),
                    "{} was analysed",
                    path.display()
                );
            }
            assert_eq!(
                session.project_cache.published_by_crawl_for_test(),
                vec![other.clone()],
                "only the unit that reads nothing from a buffer"
            );
            cache.shutdown();

            let (mut restarted, cache) = restarted_view(root, store.path());
            let progress = advance(
                &mut restarted,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert_eq!(progress.reused, 1, "{progress:?}");
            assert_eq!(fresh(&restarted, &key), vec![other]);
            for path in [&main, &shapes, &root.join("lib/Root.pas")] {
                assert!(
                    !restarted.project_cache.index_member_has_record(&key, path),
                    "{} has no record",
                    path.display()
                );
            }
            cache.shutdown();
        }

        #[test]
        fn a_crawl_under_an_overlaid_project_file_publishes_nothing() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let (mut session, key, anchor, open) = crawled_project(root);
            // `.dproj` buffers are not held; the main source is metadata.
            session
                .open_document(
                    Url::from_file_path(root.join("App.dpr")).unwrap(),
                    "program App; { unsaved } begin end.\n".to_owned(),
                    1,
                )
                .unwrap();
            let cache = start(&session, store.path(), true);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            let crawled = crawl(&session, &cache, &open);
            assert!(!crawled.context_inputs_are_disk_only(&key));
            assert!(crawled.crawl_private_signal().load(Ordering::Relaxed));
            assert!(
                session
                    .project_cache
                    .published_by_crawl_for_test()
                    .is_empty()
            );
            cache.shutdown();
            let (mut restarted, cache) = restarted_view(root, store.path());
            let progress = advance(
                &mut restarted,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert_eq!(progress.reused, 0, "{progress:?}");
            cache.shutdown();
        }

        #[test]
        fn extraction_after_the_crawl_reuses_what_the_crawl_published() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let (session, key, anchor, open) = crawled_project(root);
            let (mut view, cache) = persisting_view(&session, store.path());
            let progress = advance(
                &mut view,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert_eq!(progress.reused, 0, "a cold store: {progress:?}");
            assert_eq!(progress.reuse_pending, 0, "every member was tried");

            crawl(&session, &cache, &open);
            assert_eq!(
                sorted(session.project_cache.published_by_crawl_for_test()),
                units(root, ROOT_UNITS)
            );

            let loaded = record_loads();
            let progress = advance(&mut view, &anchor, IndexBound::all());
            set_after_load_hook(None);
            assert!(progress.complete, "{progress:?}");
            assert_eq!(progress.reused, ROOT_UNITS, "{progress:?}");
            assert_eq!(
                sorted(loaded.borrow().clone()),
                vec![root.join("App.dpr"), root.join("lib/Root.pas")],
                "only what the crawl did not publish is loaded"
            );
            assert_eq!(recorded(&view, &key).len(), ROOT_UNITS + 2);
            cache.shutdown();
        }

        /// `App.dpr` using `U0..U5` in `lib`, `App.dproj` configured, and a
        /// session that has resolved no index context yet: what a server
        /// sees at its first `didOpen` of the main source.
        fn configured_main_source(root: &Path) -> (Workspace, Url) {
            for index in 0..ROOT_UNITS {
                named_unit(
                    &root.join(format!("lib/U{index}.pas")),
                    &format!("P{index}"),
                );
            }
            let uses = (0..ROOT_UNITS)
                .map(|index| format!("U{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let text = format!("program App;\nuses {uses};\nbegin\nend.\n");
            write(
                &root.join("App.dproj"),
                "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_UnitSearchPath>lib</DCC_UnitSearchPath></PropertyGroup><ItemGroup></ItemGroup></Project>",
            );
            write(&root.join("App.dpr"), &text);
            pascal_project::backdate_tree_for_test(root);
            let mut session = workspace(root, configured(root));
            let open = Url::from_file_path(root.join("App.dpr")).unwrap();
            session.open_document(open.clone(), text, 1).unwrap();
            (session, open)
        }

        fn configured(root: &Path) -> WorkspaceOptions {
            WorkspaceOptions {
                project_file: Some(root.join("App.dproj")),
                ..WorkspaceOptions::default()
            }
        }

        /// I-2 (Task 10b): the crawl of a configured project's main source
        /// and a later `Project` anchor of that project derive one context,
        /// so the anchor's extraction reuses what the crawl published.
        #[test]
        fn a_project_anchor_reuses_what_the_crawl_of_its_main_source_published() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let (session, open) = configured_main_source(root);
            let cache = start(&session, store.path(), true);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            let crawled = crawl(&session, &cache, &open);
            assert_eq!(
                sorted(session.project_cache.published_by_crawl_for_test()),
                units(root, ROOT_UNITS)
            );
            let unit = Url::from_file_path(root.join("lib/U0.pas")).unwrap();
            let crawl_context = crawled
                .document_contexts
                .get(&unit)
                .and_then(|key| crawled.contexts.get(key))
                .map(|state| state.context.clone())
                .expect("the crawl's context");
            cache.shutdown();

            let (mut restarted, cache) =
                persisting_view(&workspace(root, configured(root)), store.path());
            let anchor = IndexAnchor::Project(root.join("App.dproj"));
            let key = restarted
                .index_context_for(&anchor, &AtomicBool::new(false))
                .expect("context");
            let anchor_context = restarted.contexts[&key].context.clone();
            assert_eq!(
                CacheContextKey::new(&anchor_context).digest(),
                CacheContextKey::new(&crawl_context).digest(),
                "crawl: {crawl_context:#?}\nanchor: {anchor_context:#?}"
            );
            let loaded = record_loads();
            let progress = advance(&mut restarted, &anchor, IndexBound::all());
            set_after_load_hook(None);
            assert!(progress.complete, "{progress:?}");
            assert_eq!(progress.reused, ROOT_UNITS, "{progress:?}");
            assert_eq!(
                *loaded.borrow(),
                vec![root.join("App.dpr")],
                "only the open main source is extracted"
            );
            cache.shutdown();
        }

        /// 13b-2 fix round 1 (I1): a crawl unit whose publication exceeds
        /// a fixed limit is not tried again by a later crawl job, and the
        /// index batch indexes it without the failing write and records it
        /// artifact-less.
        #[test]
        fn a_crawl_unit_over_a_fixed_limit_is_published_once_and_recorded_by_the_index() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let (session, key, anchor, open) = crawled_project(root);
            let (mut view, cache) = persisting_view(&session, store.path());
            let member = root.join("lib/U0.pas");
            let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (target, counted) = (member.clone(), Arc::clone(&attempts));
            cache.set_publish_fault_for_test(Some(Box::new(move |path| {
                (path == target).then(|| {
                    counted.fetch_add(1, Ordering::Relaxed);
                    pascal_cache::Bypass::TooLarge(pascal_cache::TooLarge::Observations {
                        count: 70_000,
                    })
                })
            })));
            crawl(&session, &cache, &open);
            crawl(&session, &cache, &open);
            assert_eq!(attempts.load(Ordering::Relaxed), 1, "one failing write");
            assert_eq!(
                session.project_cache.published_by_crawl_for_test().len(),
                ROOT_UNITS - 1
            );
            let progress = until_settled(&mut view, &anchor, IndexBound::all(), 10);
            assert!(progress.complete, "{progress:?}");
            assert_eq!(attempts.load(Ordering::Relaxed), 1, "the index skips it");
            assert_eq!(state(&view, &key, &member), Some(MemberState::Fresh));
            assert!(view.project_cache.index_member_has_record(&key, &member));
            let status = view.project_cache.index_status(&|_| false);
            assert_eq!(status[0].counts.over_limit, 1, "{:?}", status[0]);
            cache.shutdown();
        }

        /// Fix round 1: a crawl paused and dispatched again, in a new job
        /// workspace, does not publish the units it published before.
        #[test]
        fn a_crawl_dispatched_again_publishes_each_unit_once() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let (session, _key, _anchor, open) = crawled_project(root);
            let cache = start(&session, store.path(), true);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            let mut paused = crawl_view(&session, &cache, true);
            paused.set_dependency_hook(Arc::new(|_, done, _| {
                if done >= 3 {
                    Err(CANCELLATION_MESSAGE.to_owned())
                } else {
                    Ok(())
                }
            }));
            assert!(
                paused
                    .warm_with_cancel(&open, &AtomicBool::new(false))
                    .is_err()
            );
            assert_eq!(session.project_cache.published_by_crawl_for_test().len(), 3);
            crawl(&session, &cache, &open);
            assert_eq!(
                sorted(session.project_cache.published_by_crawl_for_test()),
                units(root, ROOT_UNITS),
                "each unit is published once"
            );
            // An edit while open makes the unit publishable again.
            let edited = root.join("lib/U0.pas");
            named_unit(&edited, "Edited");
            session.project_cache.invalidate_path(&edited);
            crawl(&session, &cache, &open);
            let published = session.project_cache.published_by_crawl_for_test();
            assert_eq!(published.len(), ROOT_UNITS + 1, "{published:?}");
            assert_eq!(published.last(), Some(&edited));
            cache.shutdown();
        }

        /// Fix round 2: a watcher overflow loses which paths changed, so the
        /// crawl publishes every unit again.
        #[test]
        fn a_crawl_after_a_watcher_overflow_publishes_again() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let (session, _key, _anchor, open) = crawled_project(root);
            let cache = start(&session, store.path(), true);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            crawl(&session, &cache, &open);
            assert_eq!(
                session.project_cache.published_by_crawl_for_test().len(),
                ROOT_UNITS
            );
            crawl(&session, &cache, &open);
            assert_eq!(
                session.project_cache.published_by_crawl_for_test().len(),
                ROOT_UNITS,
                "nothing again before the overflow"
            );
            session.project_cache.invalidate_after_overflow();
            crawl(&session, &cache, &open);
            let published = session.project_cache.published_by_crawl_for_test();
            assert_eq!(published.len(), 2 * ROOT_UNITS, "{published:?}");
            assert_eq!(
                sorted(published[ROOT_UNITS..].to_vec()),
                units(root, ROOT_UNITS)
            );
            cache.shutdown();
        }

        /// Fix round 2: editing a file a published unit's evidence names (an
        /// include, a provider) makes that unit publishable again too.
        #[test]
        fn a_crawl_publishes_again_the_dependents_of_a_changed_input() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let root_text =
                "unit Root;\ninterface\nuses Main, IncUnit, Other;\nimplementation\nend.\n";
            write(&root.join("lib/Root.pas"), root_text);
            write(
                &root.join("lib/IncUnit.pas"),
                "unit IncUnit;\ninterface\n{$I Part.inc}\nimplementation\nend.\n",
            );
            write(&root.join("lib/Part.inc"), "const FromPart = 1;\n");
            named_unit(&root.join("lib/Other.pas"), "FromOther");
            let (mut session, _key, _anchor) = provider_project_with(
                root,
                WorkspaceOptions {
                    project_file: Some(root.join("App.dproj")),
                    ..WorkspaceOptions::default()
                },
            );
            let open = Url::from_file_path(root.join("lib/Root.pas")).unwrap();
            session
                .open_document(open.clone(), root_text.to_owned(), 1)
                .unwrap();
            let lib = root.join("lib");
            let (main, shapes, inc_unit, part, other) = (
                lib.join("Main.pas"),
                lib.join("Shapes.pas"),
                lib.join("IncUnit.pas"),
                lib.join("Part.inc"),
                lib.join("Other.pas"),
            );
            let cache = start(&session, store.path(), true);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            crawl(&session, &cache, &open);
            assert_eq!(
                sorted(session.project_cache.published_by_crawl_for_test()),
                sorted(vec![
                    main.clone(),
                    shapes.clone(),
                    inc_unit.clone(),
                    other.clone()
                ])
            );

            // The include's bytes change (no listing does): its unit is
            // published again, nothing else.
            write(&part, "const FromPart = 2;\n");
            session.project_cache.invalidate_file_contents(&part);
            crawl(&session, &cache, &open);
            assert_eq!(
                session.project_cache.published_by_crawl_for_test()[4..],
                *std::slice::from_ref(&inc_unit)
            );

            // The provider changes: it and the unit whose facts read it are
            // published again; `Other` is not.
            write(&shapes, EDITED_SHAPES);
            session.project_cache.invalidate_file_contents(&shapes);
            crawl(&session, &cache, &open);
            assert_eq!(
                sorted(session.project_cache.published_by_crawl_for_test()[5..].to_vec()),
                sorted(vec![main.clone(), shapes])
            );

            // A new file changes `lib`'s listing, which Main's provider
            // search read: Main is published again; `Other`, which read no
            // listing, is not.
            named_unit(&lib.join("New.pas"), "FromNew");
            session.project_cache.invalidate_path(&lib.join("New.pas"));
            crawl(&session, &cache, &open);
            let again = session.project_cache.published_by_crawl_for_test()[7..].to_vec();
            assert!(again.contains(&main), "{again:?}");
            assert!(!again.contains(&other), "{again:?}");
            cache.shutdown();
        }

        /// Fix round 1 (depth 2): `X`'s compile-time expression reads
        /// `Main`, whose own conditional reads the edited `Shapes` buffer.
        #[test]
        fn a_crawl_never_publishes_a_unit_two_providers_from_an_overlay() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let root_text = "unit Root;\ninterface\nuses X;\nimplementation\nend.\n";
            write(&root.join("lib/Root.pas"), root_text);
            write(
                &root.join("lib/X.pas"),
                "unit X;\ninterface\nuses Main;\n{$IF Declared(Wide)}\nconst XWide = 1;\n{$ENDIF}\nimplementation\nend.\n",
            );
            let (mut session, _key, _anchor) = provider_project_with(
                root,
                WorkspaceOptions {
                    project_file: Some(root.join("App.dproj")),
                    ..WorkspaceOptions::default()
                },
            );
            let open = Url::from_file_path(root.join("lib/Root.pas")).unwrap();
            session
                .open_document(open.clone(), root_text.to_owned(), 1)
                .unwrap();
            session
                .open_document(
                    Url::from_file_path(root.join("lib/Shapes.pas")).unwrap(),
                    EDITED_SHAPES.to_owned(),
                    1,
                )
                .unwrap();
            let cache = start(&session, store.path(), true);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            let crawled = crawl(&session, &cache, &open);
            for name in ["X", "Main"] {
                let uri = Url::from_file_path(root.join(format!("lib/{name}.pas"))).unwrap();
                assert!(crawled.index.parsed_document(&uri).is_some(), "{name}");
            }
            let x = Url::from_file_path(root.join("lib/X.pas")).unwrap();
            let facts = crawled
                .source_facts(&x, &mut FactBudget::new(1 << 26), &AtomicBool::new(false))
                .unwrap();
            assert!(
                facts.extracted.provider_dependent,
                "X's facts depend on its providers"
            );
            assert!(
                matches!(
                    crawled.source_evidence(&x, &AtomicBool::new(false)),
                    Err(crate::workspace::evidence::EvidenceGap::Overlay)
                ),
                "X's session read the Shapes buffer through Main"
            );
            let published = session.project_cache.published_by_crawl_for_test();
            assert!(
                !published.contains(&root.join("lib/X.pas"))
                    && !published.contains(&root.join("lib/Main.pas")),
                "{published:?}"
            );
            cache.shutdown();
        }

        /// A unit this process already indexed from the same bytes, with a
        /// proven record, is in the store: the crawl does not publish it
        /// again.
        #[test]
        fn a_crawl_skips_units_the_index_already_proved() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let (session, key, anchor, open) = crawled_project(root);
            let (mut view, cache) = persisting_view(&session, store.path());
            assert!(advance(&mut view, &anchor, IndexBound::all()).complete);
            assert_eq!(recorded(&view, &key).len(), ROOT_UNITS + 2);
            let crawled = crawl(&session, &cache, &open);
            assert!(crawled.index.parsed_document(&open).is_some());
            assert!(
                session
                    .project_cache
                    .published_by_crawl_for_test()
                    .is_empty(),
                "every unit was proven by the index"
            );
            cache.shutdown();
        }

        #[test]
        fn a_crawl_cancelled_after_n_units_resumes_from_them() {
            const DONE: usize = 3;
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let store = private_dir();
            let (session, key, anchor, open) = crawled_project(root);
            let cache = start(&session, store.path(), true);
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            let mut crawled = crawl_view(&session, &cache, true);
            crawled.set_dependency_hook(Arc::new(|_, done, _| {
                if done >= DONE {
                    Err(CANCELLATION_MESSAGE.to_owned())
                } else {
                    Ok(())
                }
            }));
            assert!(
                crawled
                    .warm_with_cancel(&open, &AtomicBool::new(false))
                    .is_err()
            );
            let published = sorted(session.project_cache.published_by_crawl_for_test());
            assert_eq!(published.len(), DONE, "{published:?}");
            assert!(
                published
                    .iter()
                    .all(|path| units(root, ROOT_UNITS).contains(path))
            );
            cache.shutdown();

            let (mut restarted, cache) = restarted_view(root, store.path());
            let progress = advance(
                &mut restarted,
                &anchor,
                IndexBound::all().with_mode(IndexMode::Reuse),
            );
            assert_eq!(progress.reused, DONE, "{progress:?}");
            assert_eq!(fresh(&restarted, &key), published);
            cache.shutdown();
        }

        /// Spec §12.2 amendment 2026-11-03: restart and resume over a
        /// synthetic project.
        mod synthetic {
            use super::*;
            use crate::workspace::baseline::SealOutcome;
            use std::time::Instant;

            const SHARED_INCLUDE: &str = "const\n  SharedA = 1;\n  SharedB = 2;\n  SharedC = 3;\n  SharedD = 4;\n  SharedE = 5;\n  SharedF = 6;\n  SharedG = 7;\n  SharedH = 8;\n  SharedI = 9;\n";

            /// A unit of about 60 lines: three `uses` of its predecessors, a
            /// record type, a constant block and two routines with bodies;
            /// every tenth includes `Shared.inc`.
            fn synthetic_unit(index: usize) -> String {
                let name = format!("U{index:04}");
                let uses = (1..=3)
                    .filter_map(|back| index.checked_sub(back))
                    .map(|other| format!("  U{other:04}"))
                    .collect::<Vec<_>>();
                let uses = if uses.is_empty() {
                    String::new()
                } else {
                    format!("uses\n{};\n\n", uses.join(",\n"))
                };
                let include = if index.is_multiple_of(10) {
                    "{$I Shared.inc}\n\n"
                } else {
                    ""
                };
                format!(
                    "unit {name};

interface

{uses}{include}type
  TRecord{index} = record
    Id: Integer;
    Name: string;
    Left: Double;
    Right: Double;
    Next: Integer;
  end;

const
  Base{index} = {index};
  Step{index} = 3;
  Label{index} = '{name}';

function Compute{index}(Value: Integer): Integer;
procedure Fill{index}(var Item: TRecord{index});

implementation

function Compute{index}(Value: Integer): Integer;
var
  Index: Integer;
begin
  Result := Base{index};
  for Index := 0 to Value do
  begin
    if Index mod Step{index} = 0 then
      Result := Result + Index
    else
      Result := Result - 1;
  end;
end;

procedure Fill{index}(var Item: TRecord{index});
begin
  Item.Id := Compute{index}(Base{index});
  Item.Name := Label{index};
  Item.Left := Item.Id * 0.5;
  Item.Right := Item.Left + Step{index};
  Item.Next := Item.Id + 1;
  if Item.Next > 100 then
  begin
    Item.Next := 0;
  end;
end;

end.
"
                )
            }

            /// `App.dproj`, `App.dpr` and `units - 1` units under `lib`:
            /// `units` members.
            fn synthetic_project(
                root: &Path,
                units: usize,
            ) -> (Workspace, ContextKey, IndexAnchor) {
                write(&root.join("lib/Shared.inc"), SHARED_INCLUDE);
                for index in 0..units - 1 {
                    write(
                        &root.join(format!("lib/U{index:04}.pas")),
                        &synthetic_unit(index),
                    );
                }
                let (workspace, key) = project_workspace(
                    root,
                    "<DCC_UnitSearchPath>lib</DCC_UnitSearchPath>",
                    "",
                    WorkspaceOptions::default(),
                );
                (workspace, key, IndexAnchor::Project(root.join("App.dproj")))
            }

            #[derive(Debug, Default)]
            pub(super) struct Totals {
                batches: usize,
                indexed: usize,
                reused: usize,
                persisted: usize,
                refused: usize,
                limited: usize,
                deferred: usize,
                /// The largest WAL seen between batches.
                wal_peak: u64,
            }

            /// A baseline view of `workspace` whose store under `directory`
            /// has the production maintenance schedule (the worker's steps
            /// and its wake on pressure), unlike `persisting_view`.
            pub(super) fn scheduled_view(
                workspace: &Workspace,
                directory: &Path,
            ) -> (Workspace, PersistentCache) {
                let cache = PersistentCache::start_with(
                    CacheOptions {
                        enabled: true,
                        directory: Some(directory.to_path_buf()),
                        ..CacheOptions::default()
                    },
                    workspace.project_cache().clone(),
                    Arc::default(),
                    Namespace::new(Digest::from_bytes([11; 32])),
                );
                assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
                (view_with(workspace, &cache), cache)
            }

            fn wal_pressure_bypasses(cache: &PersistentCache) -> u64 {
                cache.counters().unwrap().snapshot().bypasses
                    [crate::persistent_cache::names::bypass_index(
                        &pascal_cache::Bypass::WalPressure,
                    )]
            }

            /// Batches of `bound` until the index is complete; the first
            /// `Err` (a cancellation) ends them. With `store`, the WAL's
            /// size is sampled after each batch.
            pub(super) fn batches(
                view: &mut Workspace,
                anchor: &IndexAnchor,
                bound: IndexBound,
                store: Option<&Path>,
                before_io: &mut dyn FnMut() -> Result<(), String>,
                totals: &mut Totals,
            ) -> Result<(), String> {
                loop {
                    let progress = view.advance_workspace_index(
                        anchor,
                        bound,
                        before_io,
                        &AtomicBool::new(false),
                    )?;
                    totals.batches += 1;
                    totals.indexed += progress.indexed;
                    totals.reused += progress.reused;
                    totals.persisted += progress.persisted;
                    totals.refused += progress.refused;
                    totals.limited += progress.limited;
                    totals.deferred += progress.deferred;
                    if let Some(store) = store {
                        totals.wal_peak = totals.wal_peak.max(store_bytes(store).1);
                    }
                    if progress.complete {
                        return Ok(());
                    }
                    assert!(progress.worth_requeue(), "{progress:?}");
                }
            }

            /// The database and its WAL under `directory`.
            fn store_bytes(directory: &Path) -> (u64, u64) {
                let (mut database, mut wal) = (0, 0);
                let mut stack = vec![directory.to_path_buf()];
                while let Some(directory) = stack.pop() {
                    for entry in fs::read_dir(&directory).unwrap() {
                        let entry = entry.unwrap();
                        let metadata = entry.metadata().unwrap();
                        if metadata.is_dir() {
                            stack.push(entry.path());
                        } else if entry.file_name() == "analysis-v6.sqlite3" {
                            database += metadata.len();
                        } else if entry.file_name() == "analysis-v6.sqlite3-wal" {
                            wal += metadata.len();
                        }
                    }
                }
                (database, wal)
            }

            fn report_phases(session: &str, phases: [(Duration, usize); REUSE_PHASES]) {
                let names = [
                    "lookup",
                    "read+hash",
                    "evidence validation",
                    "fact decode",
                    "index insert",
                    "record proof",
                ];
                for (name, (time, calls)) in names.iter().zip(phases) {
                    eprintln!("{session}: {name}: {time:?} over {calls} calls");
                }
            }

            /// Task 11 fix round 1: past the WAL threshold, the production
            /// schedule (the worker woken by the refusal) checkpoints, and
            /// the refused members are proven by later batches: a cold run
            /// persists every member and seals, with no hand-run step. Sized
            /// so container 6 rows (13d-3) still pass the 8 MiB minimum.
            #[test]
            fn a_cold_index_past_the_wal_threshold_persists_every_member_and_seals() {
                const UNITS: usize = 90;
                let temp = tempfile::tempdir().unwrap();
                let root = temp.path();
                let store = private_dir();
                let (session, key, anchor) = synthetic_project(root, UNITS);
                let (mut view, cache) = scheduled_view(&session, store.path());
                {
                    let job = cache.job().expect("job");
                    let revision = job.store().policy().unwrap().revision;
                    job.store()
                        .set_policy(
                            revision,
                            pascal_cache::StorePolicy {
                                wal_threshold_bytes: pascal_cache::MIN_WAL_THRESHOLD_BYTES,
                                ..pascal_cache::StorePolicy::default()
                            },
                        )
                        .unwrap();
                }
                let mut totals = Totals::default();
                batches(
                    &mut view,
                    &anchor,
                    IndexBound::extraction(),
                    Some(store.path()),
                    &mut || Ok(()),
                    &mut totals,
                )
                .unwrap();
                assert!(
                    wal_pressure_bypasses(&cache) > 0,
                    "the WAL reached its threshold: {totals:?}"
                );
                // A member whose artifact got in before its record was
                // refused is proven by reuse, the others by extraction.
                assert!(totals.persisted + totals.reused >= UNITS, "{totals:?}");
                assert_eq!(recorded(&view, &key).len(), UNITS);
                assert_eq!(
                    view.seal_baseline_if_due(&key, &mut || Ok(())),
                    SealOutcome::Sealed
                );
                cache.shutdown();
            }

            /// Spec §12.2 amendment 2026-11-03: second session, unchanged
            /// tree, zero parses. Ordinary test at 300 units; the
            /// 5,000-unit measurement is below.
            #[test]
            fn an_unchanged_restart_parses_nothing() {
                run(300, false);
            }

            /// 13f: on a restart through a held snapshot, each member's
            /// record is opened once (the hint's lookup authorizes it, and
            /// proves the identical rebuilt record), and the context's
            /// store key is encoded once, not once per member.
            #[test]
            fn a_hinted_restart_opens_each_record_once() {
                const UNITS: usize = 40;
                let temp = tempfile::tempdir().unwrap();
                let root = temp.path();
                let store = private_dir();
                let (session, key, anchor) = synthetic_project(root, UNITS);
                let (mut first, cache) = scheduled_view(&session, store.path());
                let mut totals = Totals::default();
                batches(
                    &mut first,
                    &anchor,
                    IndexBound::extraction(),
                    None,
                    &mut || Ok(()),
                    &mut totals,
                )
                .unwrap();
                assert_eq!(
                    first.seal_baseline_if_due(&key, &mut || Ok(())),
                    SealOutcome::Sealed
                );
                let expected = recorded(&first, &key);
                cache.shutdown();
                drop(first);

                let (mut second, cache) =
                    scheduled_view(&workspace(root, WorkspaceOptions::default()), store.path());
                let cancel = AtomicBool::new(false);
                second.index_context_for(&anchor, &cancel).unwrap();
                second
                    .ensure_index_membership(&key, &anchor, &cancel)
                    .unwrap();
                assert_eq!(second.adopt_baseline_hint(&key, &cancel), Some(()));
                assert!(second.project_cache.baseline_hint(&key).unwrap().held());
                let opens = pascal_cache::record_opens_on_this_thread_for_test();
                let encodings = second.cache_context_key_encodings();
                let mut two = Totals::default();
                batches(
                    &mut second,
                    &anchor,
                    IndexBound::reuse_pass(),
                    None,
                    &mut || Ok(()),
                    &mut two,
                )
                .unwrap();
                assert_eq!(two.reused, UNITS, "{two:?}");
                assert_eq!(
                    pascal_cache::record_opens_on_this_thread_for_test() - opens,
                    UNITS as u64,
                    "one record open per member"
                );
                assert_eq!(
                    second.cache_context_key_encodings() - encodings,
                    1,
                    "the context's key is encoded once"
                );
                assert_eq!(recorded(&second, &key), expected);
                assert_eq!(
                    second.seal_baseline_if_due(&key, &mut || Ok(())),
                    SealOutcome::Unchanged
                );
                cache.shutdown();
            }

            /// Release measurement (run explicitly, numbers go into the
            /// task report):
            /// TMPDIR=$HOME/.cache/l4d-tmp cargo test -p pascal-lsp --release --lib synthetic_five_thousand -- --ignored --nocapture
            #[test]
            #[ignore = "release measurement, run explicitly"]
            fn synthetic_five_thousand_unit_restart_and_resume() {
                run(5_000, true);
            }

            fn run(units: usize, measure: bool) {
                let temp = tempfile::tempdir().unwrap();
                let root = temp.path();
                let store = private_dir();
                let (session, key, anchor) = synthetic_project(root, units);

                // 1. First session: extraction batches publish every member,
                // then the baseline is sealed.
                let (mut first, cache) = scheduled_view(&session, store.path());
                crate::navigation::test_reset_document_parse_count();
                let started = Instant::now();
                let mut one = Totals::default();
                batches(
                    &mut first,
                    &anchor,
                    IndexBound::extraction(),
                    Some(store.path()),
                    &mut || Ok(()),
                    &mut one,
                )
                .unwrap();
                let indexing1 = started.elapsed();
                let seal = first.seal_baseline_if_due(&key, &mut || Ok(()));
                let t1 = started.elapsed();
                let parses1 = crate::navigation::test_document_parse_count();
                assert_eq!(
                    seal,
                    SealOutcome::Sealed,
                    "{one:?} {:?}",
                    cache.counters().unwrap().snapshot()
                );
                assert_eq!(
                    (one.refused, one.limited, one.deferred),
                    (0, 0, 0),
                    "{one:?}"
                );
                // A publication refused for room is proven again by a later
                // batch: extracted again, or reused when its artifact got in.
                assert!(one.persisted + one.reused >= units, "{one:?}");
                assert!(one.indexed + one.reused >= units, "{one:?}");
                assert_eq!(parses1, one.indexed);
                assert_eq!(recorded(&first, &key).len(), units);
                let pressure1 = wal_pressure_bypasses(&cache);
                let [artifacts, records] = take_publish_wal();
                eprintln!(
                    "synthetic {units}: WAL growth: artifact publications {} bytes over {} \
                     ({} per member), record publications {} bytes over {} ({} per member)",
                    artifacts.0,
                    artifacts.1,
                    artifacts.0 / artifacts.1.max(1) as u64,
                    records.0,
                    records.1,
                    records.0 / records.1.max(1) as u64,
                );
                let size1 = store_bytes(store.path());
                let reserved1 = first.project_cache.stats().reserved_index_bytes;
                cache.shutdown();
                drop(first);
                let _ = take_reuse_phases();

                // 2. Second session: a fresh project cache adopts the
                // snapshot, then reuse batches fill the index from the
                // store; every member is still read and hashed (R8-1).
                let (mut second, cache) =
                    scheduled_view(&workspace(root, WorkspaceOptions::default()), store.path());
                crate::navigation::test_reset_document_parse_count();
                let started = Instant::now();
                let cancel = AtomicBool::new(false);
                let resumed = second.index_context_for(&anchor, &cancel).unwrap();
                assert_eq!(resumed, key);
                second
                    .ensure_index_membership(&key, &anchor, &cancel)
                    .unwrap();
                assert_eq!(second.adopt_baseline_hint(&key, &cancel), Some(()));
                let adoption = started.elapsed();
                let mut two = Totals::default();
                batches(
                    &mut second,
                    &anchor,
                    IndexBound::reuse_pass(),
                    None,
                    &mut || Ok(()),
                    &mut two,
                )
                .unwrap();
                let indexing2 = started.elapsed();
                let seal = second.seal_baseline_if_due(&key, &mut || Ok(()));
                let t2 = started.elapsed();
                let parses2 = crate::navigation::test_document_parse_count();
                let reads = second.project_cache.index_validation_reads(&key);
                assert_eq!(parses2, 0, "no member is parsed again");
                assert_eq!(
                    (two.refused, two.limited, two.deferred),
                    (0, 0, 0),
                    "{two:?}"
                );
                assert_eq!((two.reused, two.indexed, two.persisted), (units, 0, 0));
                assert_eq!(members(&second, &key).len(), units);
                assert_eq!(seal, SealOutcome::Unchanged);
                assert_eq!(cache.counters().unwrap().snapshot().hits, units as u64);
                // Only `Shared.inc` is validated through evidence, and it is
                // read once: the process witness of its stamp proves it for
                // the other includers (root bytes are read+hash, not
                // validation reads).
                assert!(reads >= 1, "{reads}");
                let size2 = store_bytes(store.path());
                let reserved2 = second.project_cache.stats().reserved_index_bytes;
                eprintln!(
                    "synthetic {units}: t1 {t1:?} (indexing {indexing1:?}, {} batches), \
                     parses1 {parses1}, indexed {} reused {} persisted {}, WAL peak {} \
                     (between batches), walPressure bypasses {pressure1}; t2 {t2:?} \
                     (adoption {adoption:?}, reuse to {indexing2:?}, {} batches), parses2 \
                     {parses2}, ratio {:.4}",
                    one.batches,
                    one.indexed,
                    one.reused,
                    one.persisted,
                    one.wal_peak,
                    two.batches,
                    t2.as_secs_f64() / t1.as_secs_f64(),
                );
                eprintln!(
                    "synthetic {units}: validation reads {reads}; store after first \
                     (db, wal) {size1:?}, after second {size2:?}; reserved index bytes \
                     {reserved1} then {reserved2}"
                );
                let phases = take_reuse_phases();
                assert_eq!(phases[ReusePhase::ReadHash as usize].1, units, "{phases:?}");
                // Every tenth unit includes `Shared.inc`: its member evidence
                // is validated; the others are proven by their local key.
                assert_eq!(
                    phases[ReusePhase::Validation as usize].1,
                    (units - 1).div_ceil(10),
                    "{phases:?}"
                );
                report_phases(&format!("synthetic {units} second session"), phases);
                cache.shutdown();
                drop(second);
                if measure {
                    assert!(
                        t2.as_secs_f64() < 0.1 * t1.as_secs_f64(),
                        "t1 {t1:?} t2 {t2:?}"
                    );
                }

                // 3. Kill and resume: a fresh store; the first session is
                // cancelled once 40% of the members are published, and the
                // restart extracts only the others.
                let target = units * 2 / 5;
                let store = private_dir();
                let (mut first, cache) =
                    scheduled_view(&workspace(root, WorkspaceOptions::default()), store.path());
                // The hook runs after each member's reuse attempt, before its
                // extraction: the attempt for member `target + 1` cancels.
                let attempts = Rc::new(Cell::new(0));
                let counter = Rc::clone(&attempts);
                set_after_reuse_hook(Some(Box::new(move |_| counter.set(counter.get() + 1))));
                let started = Instant::now();
                let mut killed = Totals::default();
                let result = batches(
                    &mut first,
                    &anchor,
                    IndexBound::extraction(),
                    Some(store.path()),
                    &mut || {
                        if attempts.get() > target {
                            Err(CANCELLATION_MESSAGE.to_owned())
                        } else {
                            Ok(())
                        }
                    },
                    &mut killed,
                );
                set_after_reuse_hook(None);
                let cancelled_after = started.elapsed();
                assert_eq!(result.unwrap_err(), CANCELLATION_MESSAGE);
                let published = recorded(&first, &key);
                // Fewer when a publication refused for room was not proven
                // again before the kill.
                assert!(
                    published.len() <= target && published.len() >= target * 9 / 10,
                    "{} of {target}: {killed:?}",
                    published.len()
                );
                let pressure_killed = wal_pressure_bypasses(&cache);
                cache.shutdown();
                drop(first);

                let (mut resumed, cache) =
                    scheduled_view(&workspace(root, WorkspaceOptions::default()), store.path());
                let loaded = record_loads();
                crate::navigation::test_reset_document_parse_count();
                let started = Instant::now();
                let mut resume = Totals::default();
                let result = batches(
                    &mut resumed,
                    &anchor,
                    IndexBound::extraction(),
                    Some(store.path()),
                    &mut || Ok(()),
                    &mut resume,
                );
                set_after_load_hook(None);
                result.unwrap();
                let resumed_in = started.elapsed();
                let parses = crate::navigation::test_document_parse_count();
                assert_eq!((resume.refused, resume.limited), (0, 0), "{resume:?}");
                // Every published member is reused; one whose artifact was
                // published without its record is reused too.
                assert!(resume.reused >= published.len(), "{resume:?}");
                // More when publications refused for room were proven
                // again by a later batch.
                assert!(resume.reused + resume.indexed >= units, "{resume:?}");
                assert_eq!(parses, resume.indexed);
                let loaded = loaded.borrow();
                assert_eq!(loaded.len(), resume.indexed);
                assert!(
                    loaded.iter().all(|path| !published.contains(path)),
                    "no published member is extracted again"
                );
                assert_eq!(recorded(&resumed, &key).len(), units);
                eprintln!(
                    "synthetic {units}: kill at {target} attempts, {} published after \
                     {cancelled_after:?} (walPressure bypasses {pressure_killed}); resume \
                     reused {}, parsed {parses}, persisted {} in {resumed_in:?} ({} batches, \
                     walPressure bypasses {})",
                    published.len(),
                    resume.reused,
                    resume.persisted,
                    resume.batches,
                    wal_pressure_bypasses(&cache),
                );
                cache.shutdown();
            }
        }
    }
}
