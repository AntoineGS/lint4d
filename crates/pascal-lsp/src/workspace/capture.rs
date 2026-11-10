//! What one analysis read from disk, with durable digests, as the source
//! store saw it (spec §7). `workspace::evidence` builds evidence from it.
//!
//! A read is recorded once however often it is observed: the resolver
//! records one directory observation per lookup that consults it, and a
//! reused compilation replays its reads into every session that reuses it
//! (rootcause "common root", l4d-zf1b.8). Only exact duplicates are merged;
//! two different listings of one directory, or two different payloads at
//! one path, are both kept, so a change seen within a session still makes
//! its evidence `Conflicting` or stale.

use super::resolver::path_key;
use pascal_cache::{Digest, SourceContent, listing_digest};
use pascal_core::resolver::{
    DirectoryListing, LoadedSource, ResolutionObservation, SourceRevision,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// Distinct reads past this mark make the capture incomplete instead of
/// growing.
pub(crate) const MAX_CAPTURED_READS: usize = 4096;

#[cfg(test)]
thread_local! {
    static TEST_CAPTURE_BOUND: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Tests on this thread capture at most `bound` distinct reads.
#[cfg(test)]
pub(crate) fn set_capture_bound_for_test(bound: Option<usize>) {
    TEST_CAPTURE_BOUND.with(|cell| cell.set(bound));
}

#[cfg(test)]
thread_local! {
    static TEST_UNCAPTURED_PAYLOAD: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Tests on this thread do not capture payloads whose path ends with
/// `suffix`, as if the read had escaped the capture.
#[cfg(test)]
pub(crate) fn set_uncaptured_payload_for_test(suffix: Option<&str>) {
    TEST_UNCAPTURED_PAYLOAD.with(|cell| *cell.borrow_mut() = suffix.map(str::to_owned));
}

fn capture_bound() -> usize {
    #[cfg(test)]
    if let Some(bound) = TEST_CAPTURE_BOUND.with(std::cell::Cell::get) {
        return bound;
    }
    MAX_CAPTURED_READS
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedListing {
    pub(crate) directory: PathBuf,
    /// `listing_digest` of the disk listing, before editor buffers joined it.
    pub(crate) listing: Digest,
    pub(crate) complete: bool,
    /// Editor buffers added entries the disk does not have.
    pub(crate) overlay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedPayload {
    pub(crate) path: PathBuf,
    /// The revision's session hash, which names this read in observations.
    pub(crate) content_hash: u64,
    /// The durable digest of the raw bytes read (spec §4).
    pub(crate) content: SourceContent,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CapturedReads {
    pub(crate) listings: Vec<CapturedListing>,
    pub(crate) payloads: Vec<CapturedPayload>,
    /// Reads past `MAX_CAPTURED_READS` were not recorded.
    pub(crate) overflowed: bool,
    /// An editor buffer or deletion shaped an answer the store gave, even
    /// when no overlay payload was read (a refused buffer, a hidden file).
    pub(crate) editor_state: bool,
    /// The `path_key`s of the reads `overflowed` did not record, so a
    /// restriction to reads that were all recorded is not overflowed.
    /// `None` while overflowed: any read may be missing (too many were
    /// dropped to name, or the overflow came without names).
    pub(crate) dropped: Option<HashSet<String>>,
}

pub(crate) enum ListingCapture<'a> {
    Absent,
    One(&'a CapturedListing),
    /// The session listed the directory more than once and saw it change.
    Conflicting,
}

fn listing_capture<'a>(
    mut listings: impl Iterator<Item = &'a CapturedListing>,
) -> ListingCapture<'a> {
    let Some(first) = listings.next() else {
        return ListingCapture::Absent;
    };
    if listings.any(|listing| listing != first) {
        ListingCapture::Conflicting
    } else {
        ListingCapture::One(first)
    }
}

impl CapturedReads {
    #[cfg(test)]
    pub(crate) fn listing_of(&self, directory: &Path) -> ListingCapture<'_> {
        listing_capture(
            self.listings
                .iter()
                .filter(|listing| super::resolver::path_equivalent(&listing.directory, directory)),
        )
    }

    #[cfg(test)]
    pub(crate) fn payload_of(&self, path: &Path, content_hash: u64) -> Option<&SourceContent> {
        self.payloads
            .iter()
            .find(|payload| {
                payload.content_hash == content_hash
                    && super::resolver::path_equivalent(&payload.path, path)
            })
            .map(|payload| &payload.content)
    }

    /// Keyed lookups over these reads, for callers that look up many.
    pub(crate) fn indexed(&self) -> IndexedReads<'_> {
        IndexedReads {
            reads: self,
            index: ReadsIndex::of(self),
        }
    }

    /// `ReadCapture::restricted_to` over these reads.
    #[cfg(test)]
    pub(crate) fn restricted_to<'o>(
        &self,
        observations: impl IntoIterator<Item = &'o ResolutionObservation>,
    ) -> Self {
        restrict(self, &ReadsIndex::of(self), observations)
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        let listings = self.listings.iter().fold(
            std::mem::size_of_val(self.listings.as_slice()),
            |total, listing| total.saturating_add(listing.directory.as_os_str().len()),
        );
        let payloads = self.payloads.iter().fold(
            listings.saturating_add(std::mem::size_of_val(self.payloads.as_slice())),
            |total, payload| total.saturating_add(payload.path.as_os_str().len()),
        );
        self.dropped.iter().flatten().fold(payloads, |total, key| {
            total.saturating_add(std::mem::size_of::<String>() + key.len())
        })
    }

    /// Whether the read of `key` may be missing from these reads.
    fn may_have_dropped(&self, key: &str) -> bool {
        self.overflowed
            && self
                .dropped
                .as_ref()
                .is_none_or(|dropped| dropped.contains(key))
    }

    /// The read of `key` was not recorded.
    fn drop_read(&mut self, key: String) {
        if !self.overflowed {
            self.overflowed = true;
            self.dropped = Some(HashSet::new());
        }
        if let Some(dropped) = &mut self.dropped {
            if dropped.len() >= MAX_CAPTURED_READS {
                self.dropped = None;
            } else {
                dropped.insert(key);
            }
        }
    }

    /// Carry over the overflow of `other`, whose reads join these.
    fn merge_overflow(&mut self, other: &Self) {
        if !other.overflowed {
            return;
        }
        match &other.dropped {
            Some(dropped) => {
                for key in dropped {
                    self.drop_read(key.clone());
                }
                // An overflow that named nothing still dropped something.
                if dropped.is_empty() {
                    self.overflowed = true;
                    self.dropped = None;
                }
            }
            None => {
                self.overflowed = true;
                self.dropped = None;
            }
        }
    }

    fn total(&self) -> usize {
        self.listings.len() + self.payloads.len()
    }
}

/// Where each read of a `CapturedReads` is, by `path_key`.
#[derive(Debug, Default)]
struct ReadsIndex {
    listings: HashMap<String, Vec<usize>>,
    payloads: HashMap<(String, u64), Vec<usize>>,
}

impl ReadsIndex {
    fn of(reads: &CapturedReads) -> Self {
        let mut index = Self::default();
        for (at, listing) in reads.listings.iter().enumerate() {
            index
                .listings
                .entry(path_key(&listing.directory))
                .or_default()
                .push(at);
        }
        for (at, payload) in reads.payloads.iter().enumerate() {
            index
                .payloads
                .entry((path_key(&payload.path), payload.content_hash))
                .or_default()
                .push(at);
        }
        index
    }

    fn listings<'s, 'r>(
        &'s self,
        reads: &'r CapturedReads,
        key: &str,
    ) -> impl Iterator<Item = &'r CapturedListing> + use<'s, 'r> {
        self.listings
            .get(key)
            .into_iter()
            .flatten()
            .map(|&at| &reads.listings[at])
    }

    fn payloads<'s, 'r>(
        &'s self,
        reads: &'r CapturedReads,
        key: String,
        content_hash: u64,
    ) -> impl Iterator<Item = &'r CapturedPayload> + use<'s, 'r> {
        self.payloads
            .get(&(key, content_hash))
            .into_iter()
            .flatten()
            .map(|&at| &reads.payloads[at])
    }

    /// Record `listing` unless an identical one is recorded; a new one
    /// past the bound is dropped.
    fn insert_listing(&mut self, reads: &mut CapturedReads, listing: CapturedListing) {
        let key = path_key(&listing.directory);
        if self.listings(reads, &key).any(|known| *known == listing) {
            return;
        }
        if reads.total() >= capture_bound() {
            reads.drop_read(key);
            return;
        }
        self.listings
            .entry(key)
            .or_default()
            .push(reads.listings.len());
        reads.listings.push(listing);
    }

    fn insert_payload(&mut self, reads: &mut CapturedReads, payload: CapturedPayload) {
        let key = path_key(&payload.path);
        if self
            .payloads(reads, key.clone(), payload.content_hash)
            .any(|known| *known == payload)
        {
            return;
        }
        if reads.total() >= capture_bound() {
            reads.drop_read(key);
            return;
        }
        self.payloads
            .entry((key, payload.content_hash))
            .or_default()
            .push(reads.payloads.len());
        reads.payloads.push(payload);
    }
}

/// A `CapturedReads` with keyed lookups (`CapturedReads::indexed`).
pub(crate) struct IndexedReads<'a> {
    reads: &'a CapturedReads,
    index: ReadsIndex,
}

impl<'a> IndexedReads<'a> {
    pub(crate) fn listing_of(&self, directory: &Path) -> ListingCapture<'a> {
        listing_capture(self.index.listings(self.reads, &path_key(directory)))
    }

    pub(crate) fn payload_of(&self, path: &Path, content_hash: u64) -> Option<&'a SourceContent> {
        self.index
            .payloads(self.reads, path_key(path), content_hash)
            .next()
            .map(|payload| &payload.content)
    }
}

fn restrict<'o>(
    reads: &CapturedReads,
    index: &ReadsIndex,
    observations: impl IntoIterator<Item = &'o ResolutionObservation>,
) -> CapturedReads {
    let mut restricted = CapturedReads {
        editor_state: reads.editor_state,
        ..CapturedReads::default()
    };
    if reads.overflowed && reads.dropped.is_none() {
        restricted.merge_overflow(reads);
    }
    let mut listings = BTreeSet::new();
    let mut payloads = BTreeSet::new();
    let mut named = HashSet::new();
    for observation in observations {
        match observation {
            ResolutionObservation::Directory { path, .. } => {
                let key = path_key(path);
                if let Some(at) = index.listings.get(&key) {
                    listings.extend(at.iter().copied());
                }
                named.insert(key);
            }
            ResolutionObservation::Payload {
                path,
                revision: SourceRevision::Disk { content_hash, .. },
                ..
            } => {
                let key = path_key(path);
                if let Some(at) = index.payloads.get(&(key.clone(), *content_hash)) {
                    payloads.extend(at.iter().copied());
                }
                named.insert(key);
            }
            _ => {}
        }
    }
    for key in named {
        if reads.may_have_dropped(&key) {
            restricted.drop_read(key);
        }
    }
    restricted.listings = listings
        .into_iter()
        .map(|at| reads.listings[at].clone())
        .collect();
    restricted.payloads = payloads
        .into_iter()
        .map(|at| reads.payloads[at].clone())
        .collect();
    restricted
}

#[derive(Debug, Default)]
struct Captured {
    reads: CapturedReads,
    index: ReadsIndex,
}

/// Shared by every clone of the source store a session uses.
#[derive(Debug, Default)]
pub(crate) struct ReadCapture(Mutex<Captured>);

impl ReadCapture {
    fn lock(&self) -> std::sync::MutexGuard<'_, Captured> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The store listed `directory`; `disk` is what the disk returned.
    pub(crate) fn listed(&self, directory: &Path, disk: &DirectoryListing, overlay: bool) {
        let listing = CapturedListing {
            directory: directory.to_path_buf(),
            listing: listing_digest(&disk.files, &disk.directories),
            complete: disk.complete,
            overlay,
        };
        let captured = &mut *self.lock();
        captured.index.insert_listing(&mut captured.reads, listing);
    }

    /// The store read `source`. Buffers are not disk reads.
    pub(crate) fn loaded(&self, source: &LoadedSource) {
        let SourceRevision::Disk { content_hash, .. } = &source.revision else {
            return;
        };
        #[cfg(test)]
        if TEST_UNCAPTURED_PAYLOAD.with(|cell| {
            cell.borrow()
                .as_deref()
                .is_some_and(|suffix| source.path.to_string_lossy().ends_with(suffix))
        }) {
            return;
        }
        let payload = CapturedPayload {
            path: source.path.clone(),
            content_hash: *content_hash,
            content: SourceContent::from_bytes(&source.bytes),
        };
        let captured = &mut *self.lock();
        captured.index.insert_payload(&mut captured.reads, payload);
    }

    /// The store answered from, or refused because of, editor state.
    pub(crate) fn editor_state(&self) {
        self.lock().reads.editor_state = true;
    }

    /// Record what a reused compilation read, as if this session had read
    /// it (l4d-4nbs D29). Reads this session already holds are not added
    /// again.
    pub(crate) fn replay(&self, reads: &CapturedReads) {
        let captured = &mut *self.lock();
        captured.reads.editor_state |= reads.editor_state;
        captured.reads.merge_overflow(reads);
        for listing in &reads.listings {
            captured
                .index
                .insert_listing(&mut captured.reads, listing.clone());
        }
        for payload in &reads.payloads {
            captured
                .index
                .insert_payload(&mut captured.reads, payload.clone());
        }
    }

    /// The listings and payloads `observations` name, for a compilation
    /// another session reuses (l4d-4nbs D29), each once, in capture order,
    /// from this capture as it stands, without copying it. Editor state
    /// carries over, and so does an overflow that may have dropped a read
    /// the observations name: either makes evidence from the reuse refused
    /// as from the original. An overflow that dropped only reads the
    /// observations do not name stays with this capture.
    pub(crate) fn restricted_to<'o>(
        &self,
        observations: impl IntoIterator<Item = &'o ResolutionObservation>,
    ) -> CapturedReads {
        let captured = self.lock();
        restrict(&captured.reads, &captured.index, observations)
    }

    /// Whether the capture overflowed, and whether it met editor state.
    pub(crate) fn flags(&self) -> (bool, bool) {
        let captured = self.lock();
        (captured.reads.overflowed, captured.reads.editor_state)
    }

    pub(crate) fn snapshot(&self) -> CapturedReads {
        self.lock().reads.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::facts::tests::{index, index_with_buffers};
    use pascal_core::resolver::ResolutionObservation;
    use std::sync::Arc;

    const DPROJ: &[u8] = b"<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_IncludePath>include</DCC_IncludePath></PropertyGroup></Project>";
    const MAIN: &[u8] =
        b"unit Main;\ninterface\n{$I Shared.inc}\n{$I Missing.inc}\nimplementation\nend.\n";
    const SHARED: &[u8] = b"const Shared = 1;\n";

    fn disk_listing(directory: &Path) -> Digest {
        let (mut files, mut directories) = (Vec::new(), Vec::new());
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                directories.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
        listing_digest(&files, &directories)
    }

    #[test]
    fn a_session_captures_every_listing_and_payload_it_observed() {
        let indexed = index(
            &[
                ("App.dproj", DPROJ),
                ("Main.pas", MAIN),
                ("include/Shared.inc", SHARED),
            ],
            &["Main.pas"],
        );
        let expanded = &indexed.workspace.expansions[&indexed.uri("Main.pas")].expanded;
        let reads = expanded
            .reads
            .as_ref()
            .expect("a session captures its reads");
        let session = expanded.session.as_ref().unwrap();
        assert!(!reads.overflowed);
        assert!(
            reads
                .payloads
                .iter()
                .any(|payload| payload.path.ends_with("include/Shared.inc")
                    && payload.content == SourceContent::from_bytes(SHARED)),
            "{reads:?}"
        );
        let mut listed = 0;
        for observation in &session.observations {
            match observation {
                ResolutionObservation::Directory {
                    path,
                    complete: true,
                    ..
                } => {
                    let ListingCapture::One(captured) = reads.listing_of(path) else {
                        panic!("{path:?} was listed but not captured: {reads:?}")
                    };
                    assert!(captured.complete && !captured.overlay);
                    assert_eq!(captured.listing, disk_listing(path), "{path:?}");
                    listed += 1;
                }
                ResolutionObservation::Payload {
                    path,
                    revision: pascal_core::SourceRevision::Disk { content_hash, .. },
                    ..
                } => {
                    let content = reads
                        .payload_of(path, *content_hash)
                        .unwrap_or_else(|| panic!("{path:?} was read but not captured"));
                    assert_eq!(
                        *content,
                        SourceContent::from_bytes(&std::fs::read(path).unwrap())
                    );
                }
                _ => {}
            }
        }
        assert!(
            listed > 0,
            "the missing include was looked for in a listing"
        );
    }

    #[test]
    fn an_include_found_on_the_first_hit_captures_its_payload_and_every_listing() {
        let indexed = index(
            &[
                ("App.dproj", DPROJ),
                (
                    "Main.pas",
                    b"unit Main;\ninterface\n{$I Shared.inc}\nimplementation\nend.\n",
                ),
                ("include/Shared.inc", SHARED),
            ],
            &["Main.pas"],
        );
        let expanded = &indexed.workspace.expansions[&indexed.uri("Main.pas")].expanded;
        let reads = expanded.reads.as_ref().unwrap();
        let observations = &expanded.session.as_ref().unwrap().observations;
        let captured_hit_under = |directory: &Path| {
            observations.iter().any(|observation| {
                matches!(
                    observation,
                    ResolutionObservation::Payload {
                        path,
                        revision: pascal_core::SourceRevision::Disk { content_hash, .. },
                        ..
                    } if path.parent() == Some(directory)
                        && reads.payload_of(path, *content_hash).is_some()
                )
            })
        };
        let mut marked = 0;
        for observation in observations {
            match observation {
                ResolutionObservation::Directory {
                    path,
                    complete: true,
                    listed: true,
                    ..
                } => {
                    let ListingCapture::One(captured) = reads.listing_of(path) else {
                        panic!("{observation:?} was listed but not captured: {reads:?}")
                    };
                    assert_eq!(captured.listing, disk_listing(path), "{path:?}");
                }
                ResolutionObservation::Directory {
                    path,
                    listed: false,
                    ..
                } if path.ends_with("include") => {
                    assert!(captured_hit_under(path), "{observation:?}: {reads:?}");
                    marked += 1;
                }
                _ => {}
            }
        }
        assert_eq!(
            marked, 1,
            "the include directory was searched: {observations:?}"
        );
        // The owned compile-time path keeps the miss's marker and listing as one.
        let owner: Vec<bool> = observations
            .iter()
            .filter_map(|observation| match observation {
                ResolutionObservation::Directory { path, listed, .. }
                    if super::super::resolver::path_equivalent(path, &indexed.root) =>
                {
                    Some(*listed)
                }
                _ => None,
            })
            .collect();
        assert_eq!(owner, [true], "{observations:?}");
    }

    #[test]
    fn a_buffer_the_disk_lacks_marks_its_directory_listing() {
        let indexed = index_with_buffers(
            &[
                ("App.dproj", DPROJ),
                ("Main.pas", MAIN),
                ("include/Shared.inc", SHARED),
            ],
            &[("include/Extra.inc", "const Extra = 1;\n")],
            &["Main.pas"],
        );
        let reads = indexed.workspace.expansions[&indexed.uri("Main.pas")]
            .expanded
            .reads
            .clone()
            .unwrap();
        let include = indexed.root.join("include");
        assert!(
            reads
                .listings
                .iter()
                .any(|listing| listing.directory.ends_with("include") && listing.overlay),
            "{reads:?}"
        );
        // The digest is the disk's own listing, without the buffer.
        let captured = reads
            .listings
            .iter()
            .find(|listing| listing.directory.ends_with("include"))
            .unwrap();
        assert_eq!(captured.listing, disk_listing(&include));
    }

    #[test]
    fn captures_agree_or_conflict_and_stop_at_their_bound() {
        let capture = ReadCapture::default();
        let directory = Path::new("/w/include");
        let listing = |files: Vec<PathBuf>| DirectoryListing {
            files,
            directories: Vec::new(),
            stamp: None,
            complete: true,
        };
        assert!(matches!(
            capture.snapshot().listing_of(directory),
            ListingCapture::Absent
        ));
        capture.listed(directory, &listing(vec![directory.join("a.inc")]), false);
        capture.listed(directory, &listing(vec![directory.join("a.inc")]), false);
        assert!(matches!(
            capture.snapshot().listing_of(directory),
            ListingCapture::One(_)
        ));
        capture.listed(directory, &listing(vec![directory.join("b.inc")]), false);
        assert!(matches!(
            capture.snapshot().listing_of(directory),
            ListingCapture::Conflicting
        ));
        // An identical listing is recorded once; only distinct reads count.
        for _ in 0..MAX_CAPTURED_READS {
            capture.listed(Path::new("/w/other"), &listing(Vec::new()), false);
        }
        assert!(!capture.snapshot().overflowed);
        for n in 0..MAX_CAPTURED_READS {
            capture.listed(
                &PathBuf::from(format!("/w/other{n}")),
                &listing(Vec::new()),
                false,
            );
        }
        let reads = capture.snapshot();
        assert!(reads.overflowed);
        assert_eq!(
            reads.listings.len() + reads.payloads.len(),
            MAX_CAPTURED_READS
        );
    }

    fn listing_of_names(names: &[&str]) -> DirectoryListing {
        DirectoryListing {
            files: names.iter().map(PathBuf::from).collect(),
            directories: Vec::new(),
            stamp: None,
            complete: true,
        }
    }

    fn directory_observation(path: &Path) -> ResolutionObservation {
        ResolutionObservation::Directory {
            path: path.to_path_buf(),
            entry: pascal_project::ProjectPathEntry::configured(path.to_path_buf()),
            stamp: None,
            complete: true,
            listed: true,
            resolvable: None,
        }
    }

    /// The multidev shape (rootcause A1): `providers` provider graphs, each
    /// of whose compilations looked up `lookups` units over the same
    /// `search` directories, so each graph's observations name every search
    /// directory once per lookup, interleaved.
    struct SearchPathShape {
        search: Vec<PathBuf>,
        graphs: Vec<Vec<ResolutionObservation>>,
    }

    fn search_path_shape(search: usize, providers: usize, lookups: usize) -> SearchPathShape {
        let search: Vec<PathBuf> = (0..search)
            .map(|n| PathBuf::from(format!("/w/search{n}")))
            .collect();
        let graphs = (0..providers)
            .map(|_| {
                (0..lookups)
                    .flat_map(|_| search.iter().map(|path| directory_observation(path)))
                    .collect()
            })
            .collect();
        SearchPathShape { search, graphs }
    }

    impl SearchPathShape {
        /// The session that compiled every provider: it listed each search
        /// directory once (the resolver reuses its listing) and put each
        /// graph with the reads its observations name.
        fn compile(&self) -> Vec<CapturedReads> {
            let capture = ReadCapture::default();
            for (n, directory) in self.search.iter().enumerate() {
                capture.listed(directory, &listing_of_names(&[&format!("U{n}.pas")]), false);
            }
            self.graphs
                .iter()
                .map(|observations| capture.restricted_to(observations))
                .collect()
        }
    }

    #[test]
    fn a_graph_s_reads_hold_one_listing_per_directory_it_observed() {
        let shape = search_path_shape(8, 1, 50);
        let [graph] = shape.compile().try_into().unwrap();
        assert_eq!(graph.listings.len(), 8, "{graph:?}");
        assert!(!graph.overflowed);
        assert_eq!(
            graph
                .listings
                .iter()
                .map(|listing| listing.directory.clone())
                .collect::<Vec<_>>(),
            shape.search
        );
    }

    /// Rootcause A1 regression: sessions reusing many graphs over one
    /// search path capture each directory once and never overflow, while
    /// every distinct directory is still captured.
    #[test]
    fn many_sessions_reusing_graphs_over_one_search_path_stay_under_the_bound() {
        let shape = search_path_shape(40, 30, 60);
        let graphs = shape.compile();
        for _ in 0..8 {
            let session = ReadCapture::default();
            for graph in &graphs {
                session.replay(graph);
            }
            let reads = session.snapshot();
            assert!(!reads.overflowed, "{} captured", reads.listings.len());
            assert_eq!(reads.listings.len(), shape.search.len());
            for directory in &shape.search {
                assert!(matches!(
                    reads.listing_of(directory),
                    ListingCapture::One(_)
                ));
            }
        }
    }

    /// `restricted_to` as it was before l4d-zf1b.8 13b-2: one copy per
    /// observation, consecutive duplicates only removed. The baseline of
    /// `measure_search_path_shape`.
    fn legacy_restricted_to(
        reads: &CapturedReads,
        observations: &[ResolutionObservation],
    ) -> CapturedReads {
        let mut restricted = CapturedReads {
            overflowed: reads.overflowed,
            ..CapturedReads::default()
        };
        for observation in observations {
            if let ResolutionObservation::Directory { path, .. } = observation {
                restricted.listings.extend(
                    reads
                        .listings
                        .iter()
                        .filter(|listing| {
                            super::super::resolver::path_equivalent(&listing.directory, path)
                        })
                        .cloned(),
                );
            }
        }
        restricted.listings.dedup();
        restricted
    }

    fn legacy_replay(target: &mut CapturedReads, reads: &CapturedReads) {
        target.overflowed |= reads.overflowed;
        for listing in &reads.listings {
            if target.total() >= MAX_CAPTURED_READS {
                target.overflowed = true;
                return;
            }
            target.listings.push(listing.clone());
        }
    }

    /// Run: `cargo test -p pascal-lsp --release --lib -- --ignored
    /// measure_search_path_shape --nocapture`. Counts and per-put cost on
    /// the multidev shape (rootcause A1/A4), before and after.
    #[test]
    #[ignore = "manual measurement"]
    fn measure_search_path_shape() {
        use std::time::Instant;
        let (search, providers, lookups, sessions) = (40, 30, 60, 8);
        let shape = search_path_shape(search, providers, lookups);
        let observations: usize = shape.graphs.iter().map(Vec::len).sum();
        println!(
            "shape: {search} search dirs, {providers} provider graphs x {lookups} lookups \
             ({} observations per graph, {observations} total), {sessions} reusing sessions",
            shape.graphs[0].len()
        );

        // Before: the compiling session's capture, then each put.
        let mut compiling = CapturedReads::default();
        for directory in &shape.search {
            compiling.listings.push(CapturedListing {
                directory: directory.clone(),
                listing: listing_digest(&[], &[]),
                complete: true,
                overlay: false,
            });
        }
        let legacy: Vec<_> = shape
            .graphs
            .iter()
            .map(|graph| legacy_restricted_to(&compiling, graph))
            .collect();
        let mut session = CapturedReads::default();
        for graph in &legacy {
            legacy_replay(&mut session, graph);
        }
        println!(
            "before: {} listings per graph; a reusing session captures {} (overflowed {})",
            legacy[0].listings.len(),
            session.total(),
            session.overflowed
        );
        // A put from that saturated session: N graph observations x L entries.
        let started = Instant::now();
        let restricted = legacy_restricted_to(&session, &shape.graphs[0]);
        let legacy_put = started.elapsed();
        std::hint::black_box(restricted);

        // After.
        let graphs = shape.compile();
        let session = ReadCapture::default();
        for graph in &graphs {
            session.replay(graph);
        }
        let reads = session.snapshot();
        println!(
            "after: {} listings per graph; a reusing session captures {} (overflowed {})",
            graphs[0].listings.len(),
            reads.total(),
            reads.overflowed
        );
        let started = Instant::now();
        for _ in 0..sessions {
            std::hint::black_box(session.restricted_to(&shape.graphs[0]));
        }
        let put = started.elapsed() / sessions as u32;
        println!(
            "per put (restrict {} observations): before {legacy_put:?} against {} captured, \
             after {put:?} against {}",
            shape.graphs[0].len(),
            MAX_CAPTURED_READS,
            reads.total()
        );
        // Evidence of a session whose observations are every graph's.
        let all: Vec<_> = shape.graphs.iter().flatten().cloned().collect();
        let evidence = crate::workspace::evidence::session_evidence(
            &all,
            &reads,
            &pascal_project::ProjectObservations::default(),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        println!(
            "evidence: before {} observations (one per resolution observation), after {}",
            all.len(),
            evidence.len()
        );
        let (probes, _) = crate::project_cache::observation_probes(&all, &mut |_, _| None);
        println!(
            "probes: before {} (one per observation), after {}",
            all.len(),
            probes.len()
        );
    }

    #[test]
    fn a_replayed_listing_that_differs_still_conflicts() {
        let directory = Path::new("/w/include");
        let first = ReadCapture::default();
        first.listed(directory, &listing_of_names(&["a.inc"]), false);
        let second = ReadCapture::default();
        second.listed(directory, &listing_of_names(&["b.inc"]), false);
        let session = ReadCapture::default();
        session.replay(&first.snapshot());
        session.replay(&first.snapshot());
        session.replay(&second.snapshot());
        let reads = session.snapshot();
        assert_eq!(reads.listings.len(), 2, "{reads:?}");
        assert!(matches!(
            reads.listing_of(directory),
            ListingCapture::Conflicting
        ));
    }

    #[test]
    fn a_payload_read_twice_is_captured_once_and_a_changed_one_twice() {
        let capture = ReadCapture::default();
        let source = |bytes: &[u8], content_hash| LoadedSource {
            id: pascal_core::SourceId::new("source:/w/a.inc"),
            path: PathBuf::from("/w/a.inc"),
            bytes: bytes.to_vec().into(),
            decoded_text: None,
            revision: SourceRevision::Disk {
                stamp: pascal_project::ProjectReadStamp {
                    bytes: bytes.len() as u64,
                    modified: None,
                    is_dir: false,
                    is_symlink: false,
                    change: None,
                    settled: false,
                },
                content_hash,
                read_policy: Default::default(),
                path_entry: pascal_project::ProjectPathEntry::configured(PathBuf::from("/w/a.inc")),
            },
            compile_time_backing: None,
        };
        capture.loaded(&source(b"x", 1));
        capture.loaded(&source(b"x", 1));
        assert_eq!(capture.snapshot().payloads.len(), 1);
        capture.loaded(&source(b"y", 2));
        let reads = capture.snapshot();
        assert_eq!(reads.payloads.len(), 2);
        assert_eq!(
            reads.payload_of(Path::new("/w/a.inc"), 1),
            Some(&SourceContent::from_bytes(b"x"))
        );
        assert_eq!(
            reads.payload_of(Path::new("/w/a.inc"), 2),
            Some(&SourceContent::from_bytes(b"y"))
        );
    }

    /// Rootcause A1 secondary: a graph put from a session that overflowed
    /// on reads the graph never named does not carry that overflow; a
    /// graph that names a dropped read does.
    #[test]
    fn a_graph_carries_only_the_overflow_of_its_own_reads() {
        let capture = ReadCapture::default();
        let kept = Path::new("/w/kept");
        capture.listed(kept, &listing_of_names(&["a.pas"]), false);
        for n in 0..MAX_CAPTURED_READS {
            capture.listed(
                &PathBuf::from(format!("/w/other{n}")),
                &listing_of_names(&[]),
                false,
            );
        }
        let dropped = PathBuf::from(format!("/w/other{}", MAX_CAPTURED_READS - 1));
        assert!(capture.snapshot().overflowed);

        let own = capture.restricted_to(&[directory_observation(kept)]);
        assert!(!own.overflowed, "{:?}", own.listings);
        assert_eq!(own.listings.len(), 1);
        let session = ReadCapture::default();
        session.replay(&own);
        assert!(!session.snapshot().overflowed);

        let named =
            capture.restricted_to(&[directory_observation(kept), directory_observation(&dropped)]);
        assert!(named.overflowed, "a read it named was not recorded");
        let session = ReadCapture::default();
        session.replay(&named);
        assert!(session.snapshot().overflowed);
        // ... and the next graph put from that session names it too.
        assert!(
            session
                .restricted_to(&[directory_observation(&dropped)])
                .overflowed
        );
        // An overflow that recorded no path still poisons every restriction.
        let untracked = CapturedReads {
            overflowed: true,
            ..CapturedReads::default()
        };
        assert!(untracked.restricted_to(&[]).overflowed);
    }

    /// A store over a saved `Shared.inc` with a different buffer open.
    fn buffered_store() -> (
        tempfile::TempDir,
        PathBuf,
        Arc<pascal_project::ProjectContext>,
        Arc<ReadCapture>,
        super::super::resolver::LspSourceStore,
    ) {
        use pascal_project::{ProjectContext, ProjectOptions, delphi_overrides::OverrideSession};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Shared.inc");
        std::fs::write(&path, "const Shared = 1;\n").unwrap();
        let mut workspace = crate::workspace::Workspace::with_override_session(
            vec![dir.path().into()],
            Default::default(),
            OverrideSession::new(None),
        );
        workspace
            .open_document(
                lsp_types::Url::from_file_path(&path).unwrap(),
                "const Edited = 1;\n".to_owned(),
                1,
            )
            .unwrap();
        let context =
            ProjectContext::discover(&path, &[dir.path().into()], &ProjectOptions::default())
                .unwrap()
                .context;
        let capture = Arc::new(ReadCapture::default());
        let store = super::super::resolver::LspSourceStore::from_input(
            context.clone(),
            &workspace.analysis_input(),
        )
        .with_capture(Arc::clone(&capture));
        (dir, path, context, capture, store)
    }

    // Smaller than the buffer: the store refuses it, reading no payload.
    #[test]
    fn a_refused_buffer_still_marks_the_capture() {
        use pascal_core::resolver::{SourceKind, SourceStore};
        let (_dir, path, context, capture, mut store) = buffered_store();
        let entry = context.path_entry_for(&path).unwrap();
        let policy = context.read_policy().clone();
        let refused = store.load(
            pascal_core::resolver::SourceRequest {
                path: &path,
                entry: &entry,
                read_policy: &policy,
                legacy_route: None,
                kind: SourceKind::Include,
                max_bytes: 1,
            },
            &pascal_core::resolver::NoCancellation,
        );
        assert!(refused.is_err());
        let reads = capture.snapshot();
        assert!(reads.payloads.is_empty());
        assert!(reads.editor_state);
    }

    #[test]
    fn an_admitted_refused_buffer_still_marks_the_capture() {
        use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits};
        use pascal_core::resolver::{NoCancellation, SourceKind, SourceStore};
        let (_dir, path, context, capture, mut store) = buffered_store();
        let entry = context.path_entry_for(&path).unwrap();
        let policy = context.read_policy().clone();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let refused = store.load_admitted(
            pascal_core::resolver::SourceRequest {
                path: &path,
                entry: &entry,
                read_policy: &policy,
                legacy_route: None,
                kind: SourceKind::Include,
                max_bytes: 1,
            },
            &mut budget,
            &NoCancellation,
        );
        assert!(refused.is_err());
        let reads = capture.snapshot();
        assert!(reads.payloads.is_empty());
        assert!(reads.editor_state);
    }
}
