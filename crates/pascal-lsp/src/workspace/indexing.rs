//! Which documents a workspace index covers (l4d-33 U1, A9, A10): the
//! compilable units of one context, read from the shared directory
//! catalogues (l4d-20) so that a second context, or a second request, lists
//! no directory the first already listed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use lsp_types::Url;
use pascal_core::facts::{
    FactBudget, FactError, IncludeSlotOutcome, NotPersistable, include_slots,
};
use pascal_project::ProjectContext;

use super::catalogues::CatalogueScope;
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
use crate::workspace_index::{
    CandidateQuery, Candidates, ContextIndex, IndexCoverage, MAX_UNVERIFIED_CANDIDATES,
    MemberFacts, MemberStamp, MemberState,
};

/// Directories whose owner one call may discover; the rest stay pending and
/// the membership says so.
pub(crate) const MAX_REQUEST_OWNER_DISCOVERY: usize = 256;

/// At most this many contexts are searched by one indexed request.
#[allow(dead_code)] // read by the query tasks of l4d-33
pub(crate) const MAX_TARGET_CONTEXTS: usize = 16;

/// What one request has spent on owner discovery: the directories it
/// discovered, and which owner contexts it already found fresh or stale.
#[derive(Default)]
struct OwnerDiscovery {
    discovered: usize,
    fresh: HashMap<ContextKey, bool>,
}

/// A workspace with at most this many Pascal sources and descriptors under
/// its roots still answers `workspace/symbol` the old way (A16).
#[allow(dead_code)] // read by the query tasks of l4d-33
pub(crate) const SMALL_WORKSPACE_SOURCES: usize = 2_048;
#[allow(dead_code)] // read by the query tasks of l4d-33
pub(crate) const SMALL_WORKSPACE_PROJECTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // read by the query tasks of l4d-33
pub(crate) enum TargetPurpose {
    References,
    WorkspaceSymbols,
}

/// The contexts one indexed request searches, in priority order.
#[derive(Debug, Default)]
#[allow(dead_code)] // read by the query tasks of l4d-33
pub(crate) struct Targets {
    pub(crate) contexts: Vec<(ContextKey, IndexAnchor)>,
    pub(crate) gaps: Vec<String>,
    /// Targets that came from the requesting document, open documents or the
    /// configured project (A8 items 1-3).
    pub(crate) from_open_or_configured: usize,
    pub(crate) small_workspace: bool,
}

impl Targets {
    fn holds(&self, key: &ContextKey) -> bool {
        self.contexts.iter().any(|(held, _)| held == key)
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct CapOverrides {
    owner_discovery: Option<usize>,
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

fn owner_discovery_cap() -> usize {
    #[cfg(test)]
    if let Some(cap) = CAP_OVERRIDES.with(|caps| caps.get().owner_discovery) {
        return cap;
    }
    MAX_REQUEST_OWNER_DISCOVERY
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

fn max_unverified_candidates() -> usize {
    #[cfg(test)]
    if let Some(cap) = CAP_OVERRIDES.with(|caps| caps.get().unverified) {
        return cap;
    }
    MAX_UNVERIFIED_CANDIDATES
}

/// How many pending members one `advance_workspace_index` call takes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IndexBound {
    units: usize,
}

#[allow(dead_code)] // built by the warmer of l4d-33
impl IndexBound {
    pub(crate) fn units(units: usize) -> Self {
        Self { units }
    }

    pub(crate) fn all() -> Self {
        Self { units: usize::MAX }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexProgress {
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
}

/// A read failure that repeats for the same bytes or path (A6): a file over
/// `maxFileBytes`, an unreadable or irregular file, a symlink or path outside
/// the roots. Cancellation, budgets and internal errors are transient.
pub(crate) fn is_stable_load_refusal(message: &str) -> bool {
    if message == CANCELLATION_MESSAGE || message.to_ascii_lowercase().contains("budget") {
        return false;
    }
    message.starts_with("cannot read ")
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
    Published,
    /// The load failed for a reason that may pass: still pending.
    Deferred,
    Refused,
    /// Not a member any more, or an unsaved buffer: nothing to publish.
    Skipped,
    Memory,
}

#[allow(dead_code)] // read by the workspace index tasks of l4d-33
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
        let mut progress = IndexProgress {
            indexed: 0,
            refreshed: 0,
            refused: 0,
            pending: 0,
            generation: 0,
            complete: false,
            skipped_open: 0,
            deferred: 0,
            memory: false,
        };
        // Open members are skipped, so ask for enough to fill the bound.
        let wanted = bound.units.saturating_add(self.open_documents.len());
        let pending = self
            .project_cache
            .read_index(&key, |index, _| index.pending(wanted))
            .unwrap_or_default();
        let mut taken = 0;
        for path in pending {
            if taken >= bound.units {
                break;
            }
            if self.is_open_member(&path) {
                continue;
            }
            taken += 1;
            // The stamp precedes the bytes it describes (A7); `before_io`
            // runs between them.
            let own_stamp = path_stamp(&path);
            before_io()?;
            let Some(own_stamp) = own_stamp else {
                // Gone behind the catalogues' back: relist on the next look.
                self.project_cache.invalidate_path(&path);
                continue;
            };
            if self.refresh_unchanged_member(&key, &path, &own_stamp) {
                progress.refreshed += 1;
                continue;
            }
            match self.index_member(&key, &path, own_stamp, cancel)? {
                MemberOutcome::Published => progress.indexed += 1,
                MemberOutcome::Refused => progress.refused += 1,
                MemberOutcome::Skipped => {}
                MemberOutcome::Deferred => progress.deferred += 1,
                MemberOutcome::Memory => {
                    progress.memory = true;
                    break;
                }
            }
        }
        self.ensure_index_membership(&key, anchor, cancel)?;
        let (all_pending, generation) = self
            .project_cache
            .read_index(&key, |index, _| {
                (index.pending(usize::MAX), index.generation())
            })
            .unwrap_or_default();
        let open = all_pending
            .iter()
            .filter(|path| self.is_open_member(path))
            .count();
        progress.pending = all_pending.len() - open;
        progress.skipped_open = open;
        progress.generation = generation;
        progress.complete = progress.pending == 0 && !progress.memory;
        Ok(progress)
    }

    /// An open document (rejected ones included) is answered from the
    /// editor's buffer, never from the index.
    fn is_open_member(&self, path: &Path) -> bool {
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
        self.project_cache
            .set_index_membership(key.clone(), anchor.clone(), membership.clone());
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
            return Ok(self.refuse_member(key, path, "the path has no file URI".to_owned()));
        };
        let held = self.index.contains(&uri);
        self.load_refusal = None;
        let loaded = self.load_source_with_cancel(&uri, key, &HashSet::new(), Some(cancel));
        #[cfg(test)]
        if let Some(mut hook) = AFTER_LOAD.with(|slot| slot.borrow_mut().take()) {
            hook(path);
            AFTER_LOAD.with(|slot| *slot.borrow_mut() = Some(hook));
        }
        let outcome = match loaded {
            Err(error) if is_stable_load_refusal(&error) => {
                Ok(self.refuse_unless_vanished(key, path, error))
            }
            Err(error) => Err(error),
            Ok(false) => match self.load_refusal.take() {
                Some(reason) if is_stable_load_refusal(&reason) => {
                    Ok(self.refuse_unless_vanished(key, path, reason))
                }
                // No stable reason: the member stays pending (a file that
                // vanished is relisted by the membership instead).
                _ if path_stamp(path).is_none() => {
                    self.project_cache.invalidate_path(path);
                    Ok(MemberOutcome::Skipped)
                }
                _ => Ok(MemberOutcome::Deferred),
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
    ) -> MemberOutcome {
        if path_stamp(path).is_none() {
            self.project_cache.invalidate_path(path);
            return MemberOutcome::Skipped;
        }
        self.refuse_member(key, path, reason)
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
            Err(error) => return Ok(self.refuse_member(key, path, error.to_string())),
        };
        match facts.extracted.persistable {
            Ok(()) => {}
            Err(NotPersistable::Transient) => {
                return Err("the member's facts were limited; it stays pending".to_owned());
            }
            // An unsaved buffer is not what the disk holds; the index
            // describes saved files only.
            Err(NotPersistable::OverlayRoot | NotPersistable::OverlayInclude { .. }) => {
                return Ok(MemberOutcome::Skipped);
            }
        }
        let slots = self.source_slot_paths(uri);
        let mut stamps = vec![MemberStamp {
            path: path.to_path_buf(),
            stamp: Some(own_stamp),
        }];
        // The include stamps are the ones the load observed when it read the
        // include (A7); a stat now could describe newer bytes.
        let observed: HashMap<String, PathStamp> = self
            .expansions
            .get(uri)
            .map(|record| {
                record
                    .include_observations
                    .iter()
                    .filter_map(|observation| {
                        Some((
                            path_key(&absolute_path(observation.path.clone())),
                            path_stamp_from_project_read(observation.stamp.as_ref()?),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        stamps.extend(slots.iter().skip(1).flatten().map(|slot| MemberStamp {
            path: slot.clone(),
            stamp: observed.get(&path_key(slot)).cloned(),
        }));
        let member = MemberFacts::from_facts(path, &facts.extracted.facts, slots);
        let hash = facts.root_content_hash.unwrap_or(0);
        Ok(
            match self
                .project_cache
                .publish_index_member(key, path, member, stamps, hash)
            {
                PublishOutcome::Indexed => MemberOutcome::Published,
                PublishOutcome::NotAMember => MemberOutcome::Skipped,
                PublishOutcome::Memory => MemberOutcome::Memory,
            },
        )
    }

    fn refuse_member(&self, key: &ContextKey, path: &Path, reason: String) -> MemberOutcome {
        self.project_cache.refuse_index_member(key, path, reason);
        MemberOutcome::Refused
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
        let mut discovery = OwnerDiscovery::default();
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
        if discovery.discovered >= owner_discovery_cap() {
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
        let maximum = max_target_contexts();
        let mut targets = Targets::default();
        let push = |targets: &mut Targets, key: ContextKey, anchor: IndexAnchor| {
            if targets.contexts.len() < maximum && !targets.holds(&key) {
                targets.contexts.push((key, anchor));
            }
        };

        if let Some(uri) = requesting {
            let known = input
                .document_owners
                .get(uri)
                .or_else(|| input.document_owners.get(&super::canonical_file_uri(uri)));
            if let Some(owner) = known {
                let anchor = owner_anchor(owner.state.context.project_file(), uri);
                push(&mut targets, owner.key.clone(), anchor);
            } else {
                match self.index_context_for(&IndexAnchor::Document(uri.clone()), cancel) {
                    Ok(key) => {
                        let project = self
                            .contexts
                            .get(&key)
                            .and_then(|state| state.context.project_file().map(Path::to_path_buf));
                        push(&mut targets, key, owner_anchor(project.as_deref(), uri));
                    }
                    Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
                    Err(error) => {
                        crate::trace::trace!("  index target: {uri} did not resolve: {error}");
                    }
                }
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
                let anchor = owner_anchor(owner.state.context.project_file(), uri);
                push(&mut targets, owner.key.clone(), anchor);
            }
        }

        if let Some(project) = self.configured_project_path() {
            match self.index_context_for(&IndexAnchor::Project(project.clone()), cancel) {
                Ok(key) => push(&mut targets, key, IndexAnchor::Project(project)),
                Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
                Err(error) => {
                    crate::trace::trace!(
                        "  index target: configured project did not resolve: {error}"
                    );
                }
            }
        }
        targets.from_open_or_configured = targets.contexts.len();

        let scopes = self
            .roots
            .iter()
            .map(|root| CatalogueScope::recursive(&root.path, Some(&root.excludes)))
            .collect::<Vec<_>>();
        let mut before_io = || check_workspace_cancel(Some(cancel));
        let mut complete = true;
        let mut sources = Vec::<(String, PathBuf)>::new();
        let mut descriptors = Vec::<(String, PathBuf)>::new();
        for scope in &scopes {
            let catalogue = self.shared_catalogue(scope, MAX_CATALOGUE_ENTRIES, &mut before_io)?;
            complete &= catalogue.is_complete() && catalogue.limit().is_none();
            sources.extend(catalogue.pascal_files().map(keyed));
            descriptors.extend(
                catalogue
                    .files()
                    .iter()
                    .filter(|path| is_descriptor(path))
                    .map(|path| keyed(path)),
            );
        }
        for list in [&mut sources, &mut descriptors] {
            list.sort_by(|left, right| left.0.cmp(&right.0));
            list.dedup_by(|left, right| left.0 == right.0);
        }
        targets.small_workspace = complete
            && sources.len() <= small_workspace_sources()
            && descriptors.len() <= small_workspace_projects();

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
                check_workspace_cancel(Some(cancel))?;
                attempts += 1;
                #[cfg(test)]
                DESCRIPTOR_ATTEMPTS.with(|count| count.set(count.get() + 1));
                match project_metadata_context(self, descriptor, &roots, &mut options, cancel) {
                    Ok((key, _)) => {
                        push(&mut targets, key, IndexAnchor::Project(descriptor.clone()));
                    }
                    Err(error) if error == CANCELLATION_MESSAGE => return Err(error),
                    Err(error) => {
                        crate::trace::trace!(
                            "  index target: {} did not resolve: {error}",
                            descriptor.display()
                        );
                    }
                }
            }
        }

        let mut held = HashSet::<String>::new();
        for (key, _) in &targets.contexts {
            self.project_cache.read_index(key, |_, membership| {
                held.extend(membership.paths.iter().map(|path| path_key(path)));
            });
        }
        let mut owners = HashMap::<PathBuf, Option<ContextKey>>::new();
        let mut discovery = OwnerDiscovery::default();
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
                Some(owner) if targets.contexts.len() < maximum => {
                    let project = self
                        .contexts
                        .get(&owner)
                        .and_then(|state| state.context.project_file().map(Path::to_path_buf));
                    let anchor = match (project, Url::from_file_path(file)) {
                        (Some(project), _) => IndexAnchor::Project(project),
                        (None, Ok(uri)) => IndexAnchor::Document(uri),
                        (None, Err(())) => {
                            outside += 1;
                            continue;
                        }
                    };
                    push(&mut targets, owner, anchor);
                }
                _ => outside += 1,
            }
        }
        if outside > 0 {
            targets.gaps.push(format!(
                "{outside} sources under the workspace roots belong to contexts that were not searched"
            ));
        }
        Ok(targets)
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
        let mut result = IndexedSources {
            gaps: targets.gaps.clone(),
            ..IndexedSources::default()
        };
        let mut taken = HashSet::<String>::new();
        let mut stamps = HashMap::<String, Option<PathStamp>>::new();
        let mut unverified_left = max_unverified_candidates();
        let mut refused = 0usize;
        let mut first_refusal = None::<Box<str>>;
        let mut memory = false;
        let open = input
            .overlays
            .keys()
            .filter_map(|uri| uri.to_file_path().ok())
            .map(|path| path_key(&absolute_path(path)))
            .collect::<HashSet<_>>();
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
        for (key, anchor) in &targets.contexts {
            check_workspace_cancel(Some(cancel))?;
            if !self.contexts.contains_key(key) {
                let resolved = self.index_context_for(anchor, cancel)?;
                if &resolved != key {
                    return Err(format!(
                        "index target {} resolved to another context",
                        anchor_display(anchor)
                    ));
                }
            }
            let mut computed = self.ensure_index_membership_computed(key, anchor, cancel)?;
            if let Some(state) = self.contexts.get(key) {
                result.contexts.insert(key.clone(), state.clone());
            }
            let display = anchor_display(anchor);
            // The directories baseline the membership only while its
            // catalogues are the ones it was computed from; otherwise the
            // membership is computed once more.
            let mut retried = false;
            let (view, listing) = loop {
                let view = self.project_cache.read_index(key, |index, membership| {
                    target_view(index, membership, query)
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
                if listing.is_some() || retried {
                    break (view, listing);
                }
                retried = true;
                let membership = self.index_membership(key, cancel)?;
                self.project_cache.set_index_membership(
                    key.clone(),
                    anchor.clone(),
                    membership.clone(),
                );
                computed = Some(membership);
            };
            let listing_incomplete = match listing {
                Some(listing) => {
                    result.baseline.extend(listing.directories);
                    listing.incomplete
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
                coverage,
                complete,
                limit,
                packages,
                owner_pending,
                ..
            } = view;

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
                    result.baseline.extend(
                        recorded
                            .into_iter()
                            .map(|member| (member.path, member.stamp)),
                    );
                } else {
                    unverified.push(path);
                }
            }
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
            let (skipped, searched) = take_unverified(
                unverified,
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
            if coverage.pending > 0 || !complete {
                self.project_cache.request_index(anchor.clone());
            }
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
        Ok(result)
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

/// What one target's index says, copied out under the index lock.
struct TargetView {
    candidates: Candidates,
    /// Per excluded member: whether every include slot resolved, and the
    /// stamps it was indexed under.
    recorded: Vec<(bool, Vec<MemberStamp>)>,
    coverage: IndexCoverage,
    scopes: Vec<(CatalogueKey, u64)>,
    complete: bool,
    limit: Option<String>,
    packages: bool,
    owner_pending: usize,
}

fn target_view(
    index: &ContextIndex,
    membership: &IndexMembership,
    query: Option<&CandidateQuery>,
) -> TargetView {
    let candidates = match query {
        Some(query) => index.candidates(query),
        None => {
            let mut all = index.candidates(&CandidateQuery {
                names: Vec::new(),
                top_level_units: None,
                aliases: Vec::new(),
            });
            all.selected.clear();
            all.excluded.clear();
            all
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

fn owner_anchor(project: Option<&Path>, uri: &Url) -> IndexAnchor {
    match project {
        Some(project) => IndexAnchor::Project(project.to_path_buf()),
        None => IndexAnchor::Document(uri.clone()),
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
mod tests {
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

    fn workspace(root: &Path, options: WorkspaceOptions) -> Workspace {
        Workspace::with_override_session(
            vec![root.to_path_buf()],
            options,
            OverrideSession::new(None),
        )
    }

    fn write(path: &Path, text: &str) {
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

    fn project_workspace(
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
            .set_index_membership(key.clone(), anchor, found);
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
            .set_index_membership(key.clone(), anchor, found);
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

    fn named_unit(path: &Path, identifier: &str) {
        let unit = path.file_stem().unwrap().to_string_lossy().into_owned();
        write(
            path,
            &format!(
                "unit {unit};\ninterface\nprocedure {identifier};\nimplementation\nprocedure {identifier};\nbegin\nend;\nend.\n"
            ),
        );
    }

    /// App.dpr plus `count - 1` units `lib/U<n>.pas`, each declaring `P<n>`.
    fn indexed_project(
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

    fn advance(
        workspace: &mut Workspace,
        anchor: &IndexAnchor,
        bound: IndexBound,
    ) -> IndexProgress {
        workspace
            .advance_workspace_index(anchor, bound, &mut || Ok(()), &AtomicBool::new(false))
            .expect("batch")
    }

    fn state(workspace: &Workspace, key: &ContextKey, path: &Path) -> Option<MemberState> {
        workspace
            .project_cache
            .read_index(key, |index, _| index.state_of(path))
            .flatten()
    }

    fn members(workspace: &Workspace, key: &ContextKey) -> Vec<PathBuf> {
        workspace
            .project_cache
            .read_index(key, |index, _| index.member_paths())
            .unwrap_or_default()
    }

    fn selected(workspace: &Workspace, key: &ContextKey, name: &str) -> Vec<PathBuf> {
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
        ] {
            assert!(is_stable_load_refusal(stable), "{stable}");
        }
        for transient in [
            CANCELLATION_MESSAGE,
            "reconciliation budget exhausted",
            "cannot read /w/A.pas: the request budget was exceeded",
            "project context was not retained for file:///w/A.pas",
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
        let _caps = override_caps(|caps| caps.owner_discovery = Some(2));
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
}
