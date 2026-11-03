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
    CANCELLATION_MESSAGE, is_safe_source_path, path_key, snapshot_context_for_uri,
};
use super::{
    ContextKey, Workspace, absolute_path, check_workspace_cancel, path_stamp, path_starts_with_ci,
};
use crate::catalogue::{Catalogue, MAX_CATALOGUE_ENTRIES};
use crate::project_cache::{IndexAnchor, IndexMembership, PublishOutcome};
use crate::workspace_index::{MemberFacts, MemberStamp, MemberState};

/// Directories whose owner one call may discover; the rest stay pending and
/// the membership says so.
pub(crate) const MAX_REQUEST_OWNER_DISCOVERY: usize = 256;

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
    pub(crate) complete: bool,
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

enum MemberOutcome {
    Published,
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
        let previous = std::mem::replace(&mut self.retain_units, false);
        let result = self.advance_index_batch(anchor, bound, before_io, cancel);
        self.retain_units = previous;
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
            memory: false,
        };
        let pending = self
            .project_cache
            .read_index(&key, |index, _| index.pending(bound.units))
            .unwrap_or_default();
        for path in pending {
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
                MemberOutcome::Memory => {
                    progress.memory = true;
                    break;
                }
            }
        }
        self.ensure_index_membership(&key, anchor, cancel)?;
        let (pending, generation) = self
            .project_cache
            .read_index(&key, |index, _| {
                (index.pending(usize::MAX).len(), index.generation())
            })
            .unwrap_or_default();
        progress.pending = pending;
        progress.generation = generation;
        progress.complete = pending == 0 && !progress.memory;
        Ok(progress)
    }

    fn ensure_index_membership(
        &mut self,
        key: &ContextKey,
        anchor: &IndexAnchor,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if !self.project_cache.index_membership_current(key) {
            let membership = self.index_membership(key, cancel)?;
            self.project_cache
                .set_index_membership(key.clone(), anchor.clone(), membership);
        }
        Ok(())
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
        let outcome = match loaded {
            Err(error) if is_stable_load_refusal(&error) => {
                Ok(self.refuse_member(key, path, error))
            }
            Err(error) => Err(error),
            Ok(false) => {
                let reason = self
                    .load_refusal
                    .take()
                    .unwrap_or_else(|| "the document could not be loaded".to_owned());
                Ok(self.refuse_member(key, path, reason))
            }
            Ok(true) => self.publish_member(key, path, &uri, own_stamp, cancel),
        };
        if !held {
            let _ = self.remove_indexed_with_control(&uri, None, None, false);
        }
        outcome
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
        stamps.extend(slots.iter().skip(1).flatten().map(|slot| MemberStamp {
            path: slot.clone(),
            stamp: path_stamp(slot),
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
    fn index_accepts(&self, key: &ContextKey, path: &Path) -> bool {
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
        let mut discovered = 0usize;
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
                        self.directory_owned_by(key, directory, file, cancel, &mut discovered)?;
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
        discovered: &mut usize,
    ) -> Result<Option<bool>, String> {
        if let Some(owner) = self.project_cache.owner_of(directory) {
            return Ok(Some(&owner == key));
        }
        if *discovered >= MAX_REQUEST_OWNER_DISCOVERY {
            return Ok(None);
        }
        *discovered += 1;
        let Ok(uri) = Url::from_file_path(sample) else {
            return Ok(None);
        };
        match snapshot_context_for_uri(self, &uri, cancel, false) {
            Ok(owner) => {
                self.project_cache
                    .remember_owner(directory.to_path_buf(), owner.clone());
                Ok(Some(&owner == key))
            }
            Err(_) => {
                check_workspace_cancel(Some(cancel))?;
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::{Depth, test_listings, test_reset_listings};
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
        assert!(workspace.retain_units);
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
        assert!(workspace.retain_units);
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
        assert!(workspace.retain_units);
        assert!(
            workspace
                .project_cache
                .read_index(&key, |index, _| index.coverage().fresh > 0)
                .unwrap()
        );
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
}
