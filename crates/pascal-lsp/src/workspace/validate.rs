//! Checks durable evidence against the current filesystem, under the
//! current read policy and editor state (spec §7 steps 2-3).
//!
//! Authorization mirrors the resolver (Decision 9), except that every
//! payload is read the strict way the resolver reads one without a legacy
//! route: an `Observation` does not record whether a route was used, and
//! the strict form can only turn a hit into a miss.
#![cfg_attr(not(test), allow(dead_code))]

use crate::project_cache::{ProjectCache, Proven};
use crate::workspace::rename::OverlayInput;
use crate::workspace::resolver::path_equivalent;
use lsp_types::Url;
use pascal_cache::{Observation, SourceContent, listing_digest};
use pascal_core::resolver::{
    DirectoryRequest, FilesystemSourceStore, SourceStore, SourceStoreError,
};
use pascal_project::{
    ProjectPathEntry, ProjectPathProvenance, ProjectWorkBudget, ReadPolicy, path_stamp_result,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// What the current session sees, which evidence is checked against.
pub(crate) struct CurrentInputs<'a> {
    pub(crate) read_policy: &'a ReadPolicy,
    pub(crate) overlays: &'a HashMap<Url, OverlayInput>,
    pub(crate) cache: &'a ProjectCache,
    pub(crate) max_file_bytes: usize,
    pub(crate) directory_entries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Check {
    Holds,
    Stale(Stale),
    /// Not decided; never a hit (spec §5: no permanent result from
    /// cancellation or exhausted budgets).
    Unproven(Unproven),
}

/// The input differs from what the evidence recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stale {
    Content(PathBuf),
    Presence(PathBuf),
    Listing(PathBuf),
    /// The current read policy no longer allows the access.
    Unauthorized(PathBuf),
    /// An editor buffer changes what this session sees there (spec §4.4).
    Overlay(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unproven {
    Cancelled,
    Budget,
}

pub(crate) struct Validator<'a> {
    inputs: CurrentInputs<'a>,
    budget: &'a dyn ProjectWorkBudget,
    cancel: &'a AtomicBool,
    disk: FilesystemSourceStore,
    reads: usize,
}

fn legacy(entry: &ProjectPathEntry) -> bool {
    matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
}

impl<'a> Validator<'a> {
    pub(crate) fn new(
        inputs: CurrentInputs<'a>,
        budget: &'a dyn ProjectWorkBudget,
        cancel: &'a AtomicBool,
    ) -> Self {
        // The session listed through `list_directory_admitted`, which bounds
        // entries but not the bytes of the files listed; an unbounded byte
        // scan here lists the same names (the budget bounds the work).
        let disk = FilesystemSourceStore::with_limits(inputs.directory_entries, usize::MAX);
        Self {
            inputs,
            budget,
            cancel,
            disk,
            reads: 0,
        }
    }

    /// Payloads and listings read so far rather than witnessed.
    pub(crate) fn reads(&self) -> usize {
        self.reads
    }

    pub(crate) fn check(&mut self, observation: &Observation) -> Check {
        if self.cancel.load(Ordering::Relaxed) || self.budget.check_cancelled().is_err() {
            return self.limited();
        }
        if self.budget.charge_path_visits(1).is_err() {
            return self.limited();
        }
        match observation {
            Observation::Payload {
                path,
                entry,
                content,
            } => self.payload(path, entry, content),
            Observation::Presence {
                path,
                entry,
                present,
            } => self.presence(path, entry.as_ref(), *present),
            Observation::Listing {
                path,
                entry,
                listing,
            } => self.listing(path, entry, *listing),
        }
    }

    fn limited(&self) -> Check {
        Check::Unproven(if self.cancel.load(Ordering::Relaxed) {
            Unproven::Cancelled
        } else {
            Unproven::Budget
        })
    }

    fn open(&self, path: &Path) -> bool {
        Url::from_file_path(path).is_ok_and(|uri| {
            self.inputs
                .overlays
                .contains_key(&crate::workspace::canonical_file_uri(&uri))
        })
    }

    fn payload(&mut self, path: &Path, entry: &ProjectPathEntry, content: &SourceContent) -> Check {
        let stale = |stale: fn(PathBuf) -> Stale| Check::Stale(stale(path.to_path_buf()));
        if self.open(path) {
            return stale(Stale::Overlay);
        }
        let policy = self.inputs.read_policy;
        if !policy.allows_location(entry) || !policy.allows_entry(entry) {
            return stale(Stale::Unauthorized);
        }
        let Ok(Some(before)) = path_stamp_result(path) else {
            return stale(Stale::Content);
        };
        if let Some(Proven::Content(proven)) = self.inputs.cache.witness(path, &before) {
            return if proven == *content {
                Check::Holds
            } else {
                stale(Stale::Content)
            };
        }
        let limit = self.inputs.max_file_bytes;
        let estimate = usize::try_from(before.bytes)
            .unwrap_or(usize::MAX)
            .min(limit);
        if self
            .budget
            .ensure_file_read_fits(estimate.saturating_add(1))
            .is_err()
        {
            return self.limited();
        }
        // Unreadable, a symlink, too large: not the input the evidence saw.
        let Ok(bytes) = policy.read_payload_bytes(entry, limit as u64) else {
            return stale(Stale::Content);
        };
        if self.budget.charge_file_bytes(bytes.len()).is_err() {
            return self.limited();
        }
        self.reads += 1;
        let current = SourceContent::from_bytes(&bytes);
        if path_stamp_result(path).ok().flatten().as_ref() == Some(&before) {
            self.inputs
                .cache
                .record_witness(path, before, Proven::Content(current));
        }
        if current == *content {
            Check::Holds
        } else {
            stale(Stale::Content)
        }
    }

    /// Mirrors the resolver: a candidate is visible only where the read
    /// policy allows its location.
    fn presence(&mut self, path: &Path, entry: Option<&ProjectPathEntry>, present: bool) -> Check {
        if !present && self.open(path) {
            return Check::Stale(Stale::Overlay(path.to_path_buf()));
        }
        let visible = entry.is_none_or(|entry| self.inputs.read_policy.allows_location(entry));
        let exists = visible && path_stamp_result(path).ok().flatten().is_some();
        if exists == present {
            Check::Holds
        } else {
            Check::Stale(Stale::Presence(path.to_path_buf()))
        }
    }

    fn listing(
        &mut self,
        path: &Path,
        entry: &ProjectPathEntry,
        listing: pascal_cache::Digest,
    ) -> Check {
        let stale = |stale: fn(PathBuf) -> Stale| Check::Stale(stale(path.to_path_buf()));
        let policy = self.inputs.read_policy;
        // The resolver lists a `LegacyNative` directory under the route form
        // whether or not a legacy route led there.
        let authorized = if legacy(entry) {
            policy.allows_legacy_route_entry(entry)
        } else {
            policy.allows_location(entry)
        };
        if !authorized {
            return stale(Stale::Unauthorized);
        }
        // A missing directory is the empty listing, as the session captured
        // it; it has no stamp, so nothing witnesses it.
        let Ok(before) = path_stamp_result(path) else {
            return stale(Stale::Listing);
        };
        let (files, directories) = match self.disk.list_directory(
            DirectoryRequest {
                directory: path,
                entry,
                read_policy: policy,
            },
            self.cancel,
        ) {
            Ok(disk) if disk.complete => (disk.files, disk.directories),
            Err(SourceStoreError::NotFound { .. }) => (Vec::new(), Vec::new()),
            Err(SourceStoreError::Cancelled) => return self.limited(),
            _ => return stale(Stale::Listing),
        };
        // A buffer for a file the disk lacks joins the session's listing.
        if self.inputs.overlays.keys().any(|uri| {
            uri.to_file_path().is_ok_and(|open| {
                open.parent()
                    .is_some_and(|parent| path_equivalent(parent, path))
                    && !files.iter().any(|file| path_equivalent(file, &open))
            })
        }) {
            return stale(Stale::Overlay);
        }
        let witnessed = before
            .as_ref()
            .and_then(|before| self.inputs.cache.witness(path, before));
        let current = match witnessed {
            Some(Proven::Listing(proven)) => proven,
            _ => {
                if self
                    .budget
                    .charge_path_visits(files.len() + directories.len())
                    .is_err()
                {
                    return self.limited();
                }
                self.reads += 1;
                let current = listing_digest(&files, &directories);
                if let Some(before) = before
                    && path_stamp_result(path).ok().flatten().as_ref() == Some(&before)
                {
                    self.inputs
                        .cache
                        .record_witness(path, before, Proven::Listing(current));
                }
                current
            }
        };
        if current == listing {
            Check::Holds
        } else {
            stale(Stale::Listing)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::ReconciliationBudget;
    use pascal_project::ProjectPathEntry;
    use pascal_project::delphi_overrides::EffectiveOverrides;
    use std::sync::Arc;

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        policy: ReadPolicy,
        cache: ProjectCache,
        overlays: HashMap<Url, OverlayInput>,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("w");
        std::fs::create_dir_all(root.join("include")).unwrap();
        std::fs::write(root.join("include/A.inc"), b"{$DEFINE A}").unwrap();
        std::fs::write(root.join("include/Notes.txt"), b"notes").unwrap();
        pascal_project::backdate_tree_for_test(&root);
        let policy = ReadPolicy::new(
            std::slice::from_ref(&root),
            &[],
            &[],
            &EffectiveOverrides::default(),
        );
        Fixture {
            _temp: temp,
            root,
            policy,
            cache: ProjectCache::default(),
            overlays: HashMap::new(),
        }
    }

    impl Fixture {
        fn payload(&self) -> Observation {
            let path = self.root.join("include/A.inc");
            Observation::Payload {
                entry: ProjectPathEntry::configured(path.clone()),
                content: SourceContent::from_bytes(&std::fs::read(&path).unwrap()),
                path,
            }
        }

        fn listing(&self) -> Observation {
            let path = self.root.join("include");
            let disk = |path: &Path| {
                let mut files: Vec<_> = std::fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect();
                files.sort();
                files
            };
            Observation::Listing {
                listing: listing_digest(&disk(&path), &[]),
                entry: ProjectPathEntry::configured(path.clone()),
                path,
            }
        }

        fn absent(&self, name: &str) -> Observation {
            let path = self.root.join(name);
            Observation::Presence {
                entry: Some(ProjectPathEntry::configured(path.clone())),
                path,
                present: false,
            }
        }

        /// Check `observations` with a fresh budget; also returns the reads.
        fn check(&self, observations: &[Observation]) -> (Vec<Check>, usize) {
            self.check_with(&self.policy, &self.cache, observations)
        }

        fn check_with(
            &self,
            policy: &ReadPolicy,
            cache: &ProjectCache,
            observations: &[Observation],
        ) -> (Vec<Check>, usize) {
            let cancel = Arc::new(AtomicBool::new(false));
            let budget = ReconciliationBudget::new(Arc::clone(&cancel));
            let mut validator = Validator::new(
                CurrentInputs {
                    read_policy: policy,
                    overlays: &self.overlays,
                    cache,
                    max_file_bytes: 1 << 20,
                    directory_entries: 1024,
                },
                &budget,
                &cancel,
            );
            let checks = observations
                .iter()
                .map(|observation| validator.check(observation))
                .collect();
            (checks, validator.reads())
        }
    }

    #[test]
    fn unchanged_inputs_hold() {
        let fixture = fixture();
        let (checks, _) = fixture.check(&[
            fixture.payload(),
            fixture.listing(),
            fixture.absent("include/B.inc"),
        ]);
        assert_eq!(checks, vec![Check::Holds; 3]);
    }

    #[test]
    fn a_same_size_same_mtime_rewrite_is_stale_without_a_witness() {
        let fixture = fixture();
        let payload = fixture.payload();
        let path = fixture.root.join("include/A.inc");
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, b"{$DEFINE B}").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(
            fixture.check(&[payload]).0,
            vec![Check::Stale(Stale::Content(path))]
        );
    }

    #[test]
    fn created_files_and_renames_are_stale_and_content_edits_keep_listings() {
        let fixture = fixture();
        let (listing, absent) = (fixture.listing(), fixture.absent("include/B.inc"));
        std::fs::write(fixture.root.join("include/Notes.txt"), b"other notes").unwrap();
        assert_eq!(
            fixture.check(std::slice::from_ref(&listing)).0,
            vec![Check::Holds],
            "a content edit keeps the names"
        );
        std::fs::write(fixture.root.join("include/B.inc"), b"{}").unwrap();
        assert_eq!(
            fixture.check(&[listing, absent]).0,
            vec![
                Check::Stale(Stale::Listing(fixture.root.join("include"))),
                Check::Stale(Stale::Presence(fixture.root.join("include/B.inc"))),
            ]
        );
    }

    #[test]
    fn a_missing_directory_is_the_empty_listing() {
        let fixture = fixture();
        let path = fixture.root.join("missing");
        let empty = Observation::Listing {
            listing: listing_digest(&[], &[]),
            entry: ProjectPathEntry::configured(path.clone()),
            path: path.clone(),
        };
        assert_eq!(
            fixture.check(std::slice::from_ref(&empty)).0,
            vec![Check::Holds]
        );
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("A.inc"), b"{}").unwrap();
        assert_eq!(
            fixture.check(&[empty]).0,
            vec![Check::Stale(Stale::Listing(path))]
        );
    }

    #[test]
    fn a_narrower_read_policy_or_a_symlink_is_stale() {
        let fixture = fixture();
        let payload = fixture.payload();
        let elsewhere = fixture.root.parent().unwrap().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let narrow = ReadPolicy::new(
            std::slice::from_ref(&elsewhere),
            &[],
            &[],
            &EffectiveOverrides::default(),
        );
        let path = fixture.root.join("include/A.inc");
        assert_eq!(
            fixture
                .check_with(&narrow, &fixture.cache, std::slice::from_ref(&payload))
                .0,
            vec![Check::Stale(Stale::Unauthorized(path.clone()))]
        );
        #[cfg(unix)]
        {
            let copy = elsewhere.join("A.inc");
            std::fs::copy(&path, &copy).unwrap();
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&copy, &path).unwrap();
            assert!(matches!(
                fixture.check(&[payload]).0[..],
                [Check::Stale(Stale::Content(_) | Stale::Unauthorized(_))]
            ));
        }
    }

    fn legacy_payload(path: PathBuf) -> Observation {
        Observation::Payload {
            entry: ProjectPathEntry::legacy(path.clone()),
            content: SourceContent::from_bytes(&std::fs::read(&path).unwrap()),
            path,
        }
    }

    #[test]
    fn an_unchanged_legacy_payload_holds() {
        let fixture = fixture();
        let payload = legacy_payload(fixture.root.join("include/A.inc"));
        assert_eq!(fixture.check(&[payload]).0, vec![Check::Holds]);
    }

    /// Without a legacy route the resolver reads a `LegacyNative` entry
    /// strictly; the evidence does not record a route, so neither may follow
    /// a symlink to the same bytes.
    #[cfg(unix)]
    #[test]
    fn a_legacy_payload_behind_a_symlink_is_stale() {
        let fixture = fixture();
        let path = fixture.root.join("include/A.inc");
        let payload = legacy_payload(path.clone());
        let copy = fixture.root.join("A.copy");
        std::fs::copy(&path, &copy).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&copy, &path).unwrap();
        assert_eq!(
            fixture.check(&[payload]).0,
            vec![Check::Stale(Stale::Unauthorized(path))]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_legacy_payload_under_a_symlinked_directory_is_stale() {
        let fixture = fixture();
        let path = fixture.root.join("include/A.inc");
        let payload = legacy_payload(path.clone());
        let include = fixture.root.join("include");
        let moved = fixture.root.join("real-include");
        std::fs::rename(&include, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &include).unwrap();
        assert_eq!(
            fixture.check(&[payload]).0,
            vec![Check::Stale(Stale::Unauthorized(path))]
        );
    }

    #[cfg(unix)]
    #[test]
    fn witnesses_spare_rereads_until_a_watcher_gap_or_a_restart() {
        let fixture = fixture();
        let observations = [fixture.payload(), fixture.listing()];
        let (checks, reads) = fixture.check(&observations);
        assert_eq!((checks, reads), (vec![Check::Holds; 2], 2));
        assert_eq!(fixture.check(&observations), (vec![Check::Holds; 2], 0));
        crate::file_watch::apply_watch_event(
            &fixture.cache,
            crate::file_watch::WatchEvent::Overflow,
        );
        assert_eq!(
            fixture.check(&observations).1,
            2,
            "overflow drops every witness"
        );
        let restarted = ProjectCache::default();
        assert_eq!(
            fixture
                .check_with(&fixture.policy, &restarted, &observations)
                .1,
            2,
            "a new process starts without witnesses"
        );
    }

    #[test]
    fn an_open_buffer_makes_disk_evidence_stale() {
        let mut fixture = fixture();
        let (payload, listing, absent) = (
            fixture.payload(),
            fixture.listing(),
            fixture.absent("include/B.inc"),
        );
        for name in ["include/A.inc", "include/B.inc"] {
            fixture.overlays.insert(
                crate::workspace::canonical_file_uri(
                    &Url::from_file_path(fixture.root.join(name)).unwrap(),
                ),
                OverlayInput {
                    text: "{}".into(),
                    version: 1,
                },
            );
        }
        let include = fixture.root.join("include");
        assert_eq!(
            fixture.check(&[payload, listing, absent]).0,
            vec![
                Check::Stale(Stale::Overlay(include.join("A.inc"))),
                Check::Stale(Stale::Overlay(include.clone())),
                Check::Stale(Stale::Overlay(include.join("B.inc"))),
            ]
        );
    }

    #[test]
    fn cancellation_and_budget_never_hold() {
        let fixture = fixture();
        let cancel = Arc::new(AtomicBool::new(true));
        let budget = ReconciliationBudget::new(Arc::clone(&cancel));
        let mut validator = Validator::new(
            CurrentInputs {
                read_policy: &fixture.policy,
                overlays: &fixture.overlays,
                cache: &fixture.cache,
                max_file_bytes: 1 << 20,
                directory_entries: 1024,
            },
            &budget,
            &cancel,
        );
        assert_eq!(
            validator.check(&fixture.payload()),
            Check::Unproven(Unproven::Cancelled)
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let budget = ReconciliationBudget::new(Arc::clone(&cancel));
        let _ = budget
            .charge_path_visits(crate::workspace::MAX_NOTIFICATION_RECONCILIATION_PATH_VISITS + 1);
        let mut validator = Validator::new(
            CurrentInputs {
                read_policy: &fixture.policy,
                overlays: &fixture.overlays,
                cache: &fixture.cache,
                max_file_bytes: 1 << 20,
                directory_entries: 1024,
            },
            &budget,
            &cancel,
        );
        assert_eq!(
            validator.check(&fixture.payload()),
            Check::Unproven(Unproven::Budget)
        );
    }
}
