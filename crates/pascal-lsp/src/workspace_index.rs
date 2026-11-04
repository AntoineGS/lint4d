//! The versioned workspace index (l4d-33): compact per-document facts of one
//! project context, inverted for candidate selection. No I/O: the workspace
//! side (`workspace::indexing`) supplies paths, stamps and facts.

use crate::navigation::canonical_name;
use crate::workspace::PathStamp;
use crate::workspace::rename::path_key;
use pascal_cache::Digest;
use pascal_core::facts::{
    ArtifactFacts, DeclarationKind, DeclarationShape, FactLocation, TextRange,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::mem::size_of;
use std::path::{Path, PathBuf};

pub(crate) const MAX_WORKSPACE_UNITS: usize = 100_000;
/// The unverified members one interactive request (references,
/// `workspace/symbol`) reads directly (A5): each costs a full include
/// expansion and nothing is kept, so the rest are a coverage gap.
pub(crate) const MAX_INTERACTIVE_UNVERIFIED_CANDIDATES: usize = 256;
pub(crate) const MAX_INDEX_CHANGES: usize = 4_096;
pub(crate) const IMPLICIT_UNITS: [&str; 2] = ["system", "sysinit"];

/// One interner entry beside its string (rounded to 16): the map slot (a
/// boxed string and an id, 25 bytes with its control byte) at the load a
/// table has just after doubling, and the string allocation's header.
const INTERNER_ENTRY_BYTES: usize = 80;
/// One key of an inverted list: the map slot (an id and a vector, 33 bytes
/// with its control byte) at the load a table has just after doubling, and
/// the vector's first allocation.
const INVERTED_KEY_BYTES: usize = 96;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct NameId(u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct MemberId(u32);

#[derive(Debug, Clone)]
pub(crate) struct MemberDeclaration {
    pub(crate) name: Box<str>,
    pub(crate) display: Box<str>,
    pub(crate) kind: DeclarationKind,
    pub(crate) shape: DeclarationShape,
    #[allow(dead_code)] // no l4d-33 query filters on visibility to importers
    pub(crate) exported: bool,
    pub(crate) unit_scope: bool,
    pub(crate) owner: Option<Box<str>>,
    /// (slot, range in that slot's decoded text): the extent when it lies in
    /// one slot, else the name (A14). `None`: both are `Expanded`; not listed.
    pub(crate) location: Option<(u32, TextRange)>,
}

#[derive(Debug, Clone)]
pub(crate) struct MemberFacts {
    unit_names: Box<[Box<str>]>,
    unit_display: Box<str>,
    imports: Box<[Box<str>]>,
    postings: Box<[Box<str>]>,
    declarations: Box<[MemberDeclaration]>,
    slot_paths: Box<[Option<PathBuf>]>,
}

impl MemberFacts {
    pub(crate) fn from_facts(
        path: &Path,
        facts: &ArtifactFacts,
        slot_paths: Vec<Option<PathBuf>>,
    ) -> Self {
        let stem = path
            .file_stem()
            .map(|stem| canonical_name(&stem.to_string_lossy()))
            .unwrap_or_default();
        let declared = facts
            .declarations
            .iter()
            .find(|declaration| declaration.kind == DeclarationKind::Unit);
        let mut unit_names = vec![stem.into_boxed_str()];
        if let Some(unit) = declared {
            unit_names.push(canonical_name(&unit.name).into_boxed_str());
        }
        unit_names.sort();
        unit_names.dedup();
        let mut imports: Vec<Box<str>> = facts
            .imports
            .iter()
            .map(|import| canonical_name(&import.name).into_boxed_str())
            .collect();
        imports.sort();
        imports.dedup();
        let postings = facts
            .postings
            .iter()
            .map(|posting| posting.name.as_str().into())
            .collect();
        let declarations = facts
            .declarations
            .iter()
            .map(|declaration| MemberDeclaration {
                name: declaration.name.as_str().into(),
                display: declaration.display.as_str().into(),
                kind: declaration.kind,
                shape: declaration.shape,
                exported: declaration.exported,
                unit_scope: declaration.unit_scope,
                owner: declaration.owner.as_deref().map(Into::into),
                location: match (declaration.extent, declaration.location) {
                    (FactLocation::Source { slot, range }, _) => Some((slot, range)),
                    (_, FactLocation::Source { slot, range }) => Some((slot, range)),
                    _ => None,
                },
            })
            .collect();
        let mut slot_paths = slot_paths;
        if slot_paths.is_empty() {
            slot_paths.push(Some(path.to_path_buf()));
        } else {
            slot_paths[0] = Some(path.to_path_buf());
        }
        Self {
            unit_names: unit_names.into(),
            unit_display: declared
                .map_or_else(
                    || {
                        path.file_stem()
                            .map(|stem| stem.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    },
                    |unit| unit.display.clone(),
                )
                .into(),
            imports: imports.into(),
            postings,
            declarations,
            slot_paths: slot_paths.into(),
        }
    }

    /// The bytes the facts keep once indexed, without the names the
    /// interner holds. The member charge (`ContextIndex::member_charge`)
    /// adds the list entries, include keys and stamps.
    fn own_bytes(&self) -> usize {
        size_of::<Self>()
            + self.unit_display.len()
            + self
                .declarations
                .iter()
                .map(|declaration| {
                    size_of::<MemberDeclaration>()
                        + declaration.name.len()
                        + declaration.display.len()
                        + declaration.owner.as_ref().map_or(0, |owner| owner.len())
                })
                .sum::<usize>()
            + self
                .slot_paths
                .iter()
                .map(|slot| {
                    size_of::<Option<PathBuf>>() + slot.as_ref().map_or(0, |p| p.as_os_str().len())
                })
                .sum::<usize>()
    }

    /// The names are interned once indexed; only their ids are kept.
    fn drop_names(&mut self) {
        self.unit_names = Box::default();
        self.imports = Box::default();
        self.postings = Box::default();
    }

    /// Import last segments, the short-name tier of the resolver (A1).
    fn import_tails(&self) -> Vec<Box<str>> {
        let mut tails: Vec<Box<str>> = self
            .imports
            .iter()
            .map(|import| import.rsplit('.').next().unwrap_or_default().into())
            .collect();
        tails.sort();
        tails.dedup();
        tails
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberState {
    Unindexed,
    Fresh,
    Stale,
    Refused(Box<str>),
    /// Its facts could not be completed (a compile-time limit): read
    /// directly by every request, and not indexed again until it changes.
    Limited,
}

#[derive(Debug, Clone)]
pub(crate) struct MemberStamp {
    pub(crate) path: PathBuf,
    pub(crate) stamp: Option<PathStamp>,
}

fn stamps_bytes(stamps: &[MemberStamp]) -> usize {
    stamps
        .iter()
        .map(|stamp| size_of::<MemberStamp>() + stamp.path.as_os_str().len())
        .sum()
}

/// Last dotted segment after `canonical_name`, or `None` when the name is
/// not ASCII or the segment is empty: postings prove nothing then (A2).
fn posting_key(name: &str) -> Option<String> {
    let name = canonical_name(name);
    let key = name.rsplit('.').next().unwrap_or_default();
    if !name.is_ascii() || key.is_empty() {
        return None;
    }
    Some(key.to_owned())
}

fn last_segment(name: &str) -> String {
    name.rsplit('.').next().unwrap_or_default().to_owned()
}

/// Every import spelling that can bind `unit` (A1): exact names, dotted
/// suffixes, aliases, and (matched by last segment in `candidates`) the
/// resolver's short-name tier.
pub(crate) fn import_names_for(unit: &str, aliases: &[(String, String)]) -> HashSet<String> {
    let unit = canonical_name(unit);
    let mut names = HashSet::new();
    let mut rest = unit.as_str();
    loop {
        names.insert(rest.to_owned());
        match rest.split_once('.') {
            Some((_, tail)) => rest = tail,
            None => break,
        }
    }
    let tails: HashSet<String> = names.iter().map(|name| last_segment(name)).collect();
    for (alias, target) in aliases {
        let target = canonical_name(target);
        if names.contains(&target) || tails.contains(&last_segment(&target)) {
            names.insert(canonical_name(alias));
        }
    }
    names
}

#[derive(Debug, Default)]
struct Interner {
    ids: HashMap<Box<str>, NameId>,
}

impl Interner {
    fn get(&self, name: &str) -> Option<NameId> {
        self.ids.get(name).copied()
    }

    fn intern(&mut self, name: &str) -> NameId {
        if let Some(id) = self.ids.get(name) {
            return *id;
        }
        let id = NameId(self.ids.len() as u32);
        self.ids.insert(name.into(), id);
        id
    }
}

#[derive(Debug, Default)]
struct Member {
    path: PathBuf,
    state: Option<MemberState>,
    stamps: Vec<MemberStamp>,
    hash: Option<u64>,
    facts: Option<MemberFacts>,
    posting_ids: Vec<NameId>,
    import_ids: Vec<NameId>,
    tail_ids: Vec<NameId>,
    unit_ids: Vec<NameId>,
    charged: usize,
    /// When its last load was deferred (a reason that may pass), in the
    /// context's deferral order; 0 when it was not.
    deferred: u64,
    /// The deferral was "retrying a previous refusal" (`RETRYING_REFUSAL`).
    retrying: bool,
    /// The store was asked for this member's facts since it was last
    /// invalidated.
    reuse_tried: bool,
    /// The facts came from the store, not from an extraction.
    reused: bool,
    /// The proven record of this member's state, set by this process.
    record: Option<Digest>,
}

/// The deferral reason of a member whose previous refusal is tried again.
pub(crate) const RETRYING_REFUSAL: &str = "retrying a previous refusal";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexCounts {
    pub(crate) members: usize,
    pub(crate) fresh: usize,
    pub(crate) reused: usize,
    pub(crate) extracted: usize,
    pub(crate) pending: usize,
    pub(crate) reuse_pending: usize,
    pub(crate) refused: usize,
    pub(crate) limited: usize,
    pub(crate) deferred: usize,
    pub(crate) retrying: usize,
    /// Members with a proven record set by this process.
    pub(crate) persisted: usize,
}

impl Member {
    fn state(&self) -> &MemberState {
        self.state.as_ref().unwrap_or(&MemberState::Unindexed)
    }
}

type Inverted = HashMap<NameId, Vec<MemberId>>;

fn list_insert(map: &mut Inverted, name: NameId, id: MemberId) {
    let list = map.entry(name).or_default();
    if let Err(at) = list.binary_search(&id) {
        list.insert(at, id);
    }
}

/// The key stays (it was charged with the name); a list that shrank far
/// gives its room back, since the members that grew it were refunded.
fn list_remove(map: &mut Inverted, name: NameId, id: MemberId) {
    if let Some(list) = map.get_mut(&name) {
        if let Ok(at) = list.binary_search(&id) {
            list.remove(at);
        }
        if list.len() * 4 <= list.capacity() {
            list.shrink_to_fit();
        }
    }
}

#[cfg(test)]
pub(crate) fn member_slot_bytes() -> usize {
    size_of::<Member>()
}

/// What listing one path in the membership costs: the path is kept three
/// times (the membership, `Member::path`, the `by_path` key) beside a whole
/// `Member` slot, plus a margin for map overhead.
pub(crate) fn membership_charge(path: &Path) -> usize {
    3 * path.as_os_str().len() + size_of::<Member>() + 64
}

#[derive(Debug, Default)]
pub(crate) struct ContextIndex {
    generation: u64,
    members: Vec<Member>,
    free: Vec<MemberId>,
    by_path: BTreeMap<String, MemberId>,
    names: Interner,
    postings: Inverted,
    importers: Inverted,
    importer_tails: Inverted,
    units: Inverted,
    include_dependents: HashMap<String, Vec<MemberId>>,
    changes: VecDeque<(u64, PathBuf)>,
    overflowed_through: Option<u64>,
    member_bytes: usize,
    /// The names the interner and the inverted-list keys hold, charged once
    /// per context and kept until compaction.
    name_bytes: usize,
    deferrals: u64,
    /// The reason of the latest deferral, reported while members are
    /// deferred.
    last_deferral: Option<Box<str>>,
}

pub(crate) struct CandidateQuery {
    pub(crate) names: Vec<String>,
    /// `Some(units)`: every binding member is top-level and declared in `units` (A3). `None`: member rule.
    pub(crate) top_level_units: Option<Vec<String>>,
    pub(crate) aliases: Vec<(String, String)>,
}

#[derive(Debug, Default)]
pub(crate) struct Candidates {
    pub(crate) selected: Vec<PathBuf>,
    pub(crate) unverified: Vec<PathBuf>,
    pub(crate) refused: Vec<(PathBuf, Box<str>)>,
    pub(crate) excluded: Vec<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct IndexedSymbol {
    /// The file of the declaration's slot (A14).
    pub(crate) path: PathBuf,
    /// The member whose facts hold the declaration.
    pub(crate) member: PathBuf,
    pub(crate) declaration: MemberDeclaration,
    pub(crate) container: Option<String>,
}

/// A declaration once per request: its slot file's path key, range and
/// display name. Names declared together share a range.
pub(crate) type SymbolKey = (String, TextRange, Box<str>);

/// What one context contributes to a workspace-symbol request.
#[derive(Debug, Default)]
pub(crate) struct SymbolScan {
    pub(crate) symbols: Vec<IndexedSymbol>,
    /// Fresh members with a matching declaration, in path order.
    pub(crate) contributing: Vec<PathBuf>,
    /// Fresh members read directly instead (open or deleted files).
    pub(crate) direct: Vec<PathBuf>,
    pub(crate) unverified: Vec<PathBuf>,
    pub(crate) refused: Vec<(PathBuf, Box<str>)>,
    /// More declarations matched than the request may return.
    pub(crate) exceeded: bool,
}

/// `display` contains `query` (already lowercased), ignoring case; ASCII
/// names are compared without allocating.
pub(crate) fn name_matches(display: &str, query: &str) -> bool {
    if display.is_ascii() && query.is_ascii() {
        let (display, query) = (display.as_bytes(), query.as_bytes());
        return query.is_empty()
            || display
                .windows(query.len())
                .any(|window| window.eq_ignore_ascii_case(query));
    }
    display.to_lowercase().contains(query)
}

pub(crate) struct IndexCoverage {
    pub(crate) members: usize,
    pub(crate) fresh: usize,
    pub(crate) pending: usize,
    pub(crate) refused: usize,
    pub(crate) first_refusal: Option<Box<str>>,
    /// Pending members whose last load failed for a reason that may pass.
    pub(crate) deferred: usize,
    pub(crate) first_deferral: Option<Box<str>>,
}

impl ContextIndex {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// `None`: the log no longer reaches back to `generation` (A18).
    #[allow(dead_code)] // l4d-36 validates witnesses against it (A18)
    pub(crate) fn changes_since(&self, generation: u64) -> Option<Vec<PathBuf>> {
        if self
            .overflowed_through
            .is_some_and(|dropped| generation <= dropped)
        {
            return None;
        }
        let mut paths: Vec<PathBuf> = self
            .changes
            .iter()
            .filter(|(at, _)| *at > generation)
            .map(|(_, path)| path.clone())
            .collect();
        paths.sort();
        paths.dedup();
        Some(paths)
    }

    fn log(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.generation += 1;
        for path in paths {
            self.changes.push_back((self.generation, path));
            if self.changes.len() > MAX_INDEX_CHANGES
                && let Some((dropped, _)) = self.changes.pop_front()
            {
                self.overflowed_through = Some(dropped);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn member_paths(&self) -> Vec<PathBuf> {
        self.by_path
            .values()
            .map(|id| self.members[id.0 as usize].path.clone())
            .collect()
    }

    fn id_of(&self, path: &Path) -> Option<MemberId> {
        self.by_path.get(&path_key(path)).copied()
    }

    /// Reconcile with `paths` (sorted, deduplicated by path_key). Returns
    /// (added, removed bytes).
    pub(crate) fn set_members(&mut self, paths: Vec<PathBuf>) -> (usize, usize) {
        let wanted: BTreeMap<String, PathBuf> = paths
            .into_iter()
            .map(|path| (path_key(&path), path))
            .collect();
        let mut changed = Vec::new();
        let mut removed_bytes = 0;
        let gone: Vec<String> = self
            .by_path
            .keys()
            .filter(|key| !wanted.contains_key(*key))
            .cloned()
            .collect();
        for key in gone {
            if let Some(id) = self.by_path.get(&key).copied() {
                changed.push(self.members[id.0 as usize].path.clone());
                removed_bytes += self.release(id);
                self.by_path.remove(&key);
                let member = &mut self.members[id.0 as usize];
                *member = Member::default();
                self.free.push(id);
            }
        }
        let mut added = 0;
        for (key, path) in wanted {
            if self.by_path.contains_key(&key) {
                continue;
            }
            let member = Member {
                path: path.clone(),
                ..Member::default()
            };
            let id = match self.free.pop() {
                Some(id) => {
                    self.members[id.0 as usize] = member;
                    id
                }
                None => {
                    self.members.push(member);
                    MemberId((self.members.len() - 1) as u32)
                }
            };
            self.by_path.insert(key, id);
            changed.push(path);
            added += 1;
        }
        if self.by_path.is_empty() {
            self.members = Vec::new();
            self.free = Vec::new();
            self.names = Interner::default();
            self.postings = HashMap::new();
            self.importers = HashMap::new();
            self.importer_tails = HashMap::new();
            self.units = HashMap::new();
            self.include_dependents = HashMap::new();
            self.member_bytes = 0;
            self.last_deferral = None;
            removed_bytes += std::mem::take(&mut self.name_bytes);
        }
        if !changed.is_empty() {
            self.log(changed);
        }
        (added, removed_bytes)
    }

    /// Drop a member's facts from every inverted list; returns the bytes it
    /// had charged.
    fn release(&mut self, id: MemberId) -> usize {
        let member = &mut self.members[id.0 as usize];
        let posting_ids = std::mem::take(&mut member.posting_ids);
        let import_ids = std::mem::take(&mut member.import_ids);
        let tail_ids = std::mem::take(&mut member.tail_ids);
        let unit_ids = std::mem::take(&mut member.unit_ids);
        let facts = member.facts.take();
        let charged = std::mem::take(&mut member.charged);
        member.stamps = Vec::new();
        member.deferred = 0;
        member.retrying = false;
        member.reused = false;
        member.record = None;
        for name in posting_ids {
            list_remove(&mut self.postings, name, id);
        }
        for name in import_ids {
            list_remove(&mut self.importers, name, id);
        }
        for name in tail_ids {
            list_remove(&mut self.importer_tails, name, id);
        }
        for name in unit_ids {
            list_remove(&mut self.units, name, id);
        }
        if let Some(facts) = facts {
            for slot in facts.slot_paths.iter().skip(1).flatten() {
                let key = path_key(slot);
                if let Some(list) = self.include_dependents.get_mut(&key) {
                    if let Ok(at) = list.binary_search(&id) {
                        list.remove(at);
                    }
                    if list.is_empty() {
                        self.include_dependents.remove(&key);
                    }
                }
            }
        }
        self.member_bytes -= charged;
        charged
    }

    fn pending_members(&self) -> impl Iterator<Item = &Member> {
        self.by_path
            .values()
            .map(|id| &self.members[id.0 as usize])
            .filter(|member| matches!(member.state(), MemberState::Unindexed | MemberState::Stale))
    }

    /// Unindexed | Stale: the members no load deferred in path order, then
    /// the deferred ones, longest deferred first, so a run of failing
    /// members never holds the others back.
    pub(crate) fn pending(&self, limit: usize) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .pending_members()
            .filter(|member| member.deferred == 0)
            .take(limit)
            .map(|member| member.path.clone())
            .collect();
        if paths.len() < limit {
            let mut deferred: Vec<&Member> = self
                .pending_members()
                .filter(|member| member.deferred != 0)
                .collect();
            deferred.sort_by_key(|member| member.deferred);
            paths.extend(
                deferred
                    .into_iter()
                    .take(limit - paths.len())
                    .map(|member| member.path.clone()),
            );
        }
        paths
    }

    /// Pending members no load has deferred since they were last indexed.
    pub(crate) fn untried(&self) -> Vec<PathBuf> {
        self.pending_members()
            .filter(|member| member.deferred == 0)
            .map(|member| member.path.clone())
            .collect()
    }

    /// Pending members the store was not asked about since they were last
    /// invalidated, in path order.
    pub(crate) fn reuse_pending(&self, limit: usize) -> Vec<PathBuf> {
        self.pending_members()
            .filter(|member| !member.reuse_tried)
            .take(limit)
            .map(|member| member.path.clone())
            .collect()
    }

    /// Whether the store was asked for `path`'s facts since it was last
    /// invalidated; `false` for a path that is no member.
    pub(crate) fn reuse_tried(&self, path: &Path) -> bool {
        self.id_of(path)
            .is_some_and(|id| self.members[id.0 as usize].reuse_tried)
    }

    /// Whether `path` is pending only to retry a previous refusal
    /// (`RETRYING_REFUSAL`), which does not hold `complete` back.
    pub(crate) fn retrying(&self, path: &Path) -> bool {
        self.id_of(path).is_some_and(|id| {
            let member = &self.members[id.0 as usize];
            member.retrying && matches!(member.state(), MemberState::Unindexed | MemberState::Stale)
        })
    }

    pub(crate) fn mark_reuse_tried(&mut self, path: &Path) {
        if let Some(id) = self.id_of(path) {
            self.members[id.0 as usize].reuse_tried = true;
        }
    }

    /// The store gained `path`'s facts since it was asked: ask again.
    pub(crate) fn clear_reuse_tried(&mut self, path: &Path) {
        if let Some(id) = self.id_of(path) {
            self.members[id.0 as usize].reuse_tried = false;
        }
    }

    /// Whether `path` is Fresh from the bytes hashed `hash`, with a proven
    /// record.
    pub(crate) fn proven_with_hash(&self, path: &Path, hash: u64) -> bool {
        self.id_of(path).is_some_and(|id| {
            let member = &self.members[id.0 as usize];
            matches!(member.state(), MemberState::Fresh)
                && member.record.is_some()
                && member.hash == Some(hash)
        })
    }

    /// The facts just inserted came from the store.
    pub(crate) fn mark_reused(&mut self, path: &Path) {
        if let Some(id) = self.id_of(path) {
            let member = &mut self.members[id.0 as usize];
            if matches!(member.state(), MemberState::Fresh) {
                member.reused = true;
            }
        }
    }

    /// Only a Fresh, Refused or Limited member holds a record.
    pub(crate) fn set_record(&mut self, path: &Path, record: Digest) -> bool {
        let Some(id) = self.id_of(path) else {
            return false;
        };
        let member = &mut self.members[id.0 as usize];
        match member.state() {
            MemberState::Fresh | MemberState::Refused(_) | MemberState::Limited => {
                member.record = Some(record);
                true
            }
            MemberState::Unindexed | MemberState::Stale => false,
        }
    }

    pub(crate) fn record(&self, path: &Path) -> Option<Digest> {
        self.id_of(path)
            .and_then(|id| self.members[id.0 as usize].record)
    }

    /// One entry per member, in path order.
    #[allow(dead_code)] // consumed by pascal/indexStatus and the snapshots (l4d-zf1b.8)
    pub(crate) fn records(&self) -> Vec<(PathBuf, Option<Digest>)> {
        self.by_path
            .values()
            .map(|id| {
                let member = &self.members[id.0 as usize];
                (member.path.clone(), member.record)
            })
            .collect()
    }

    pub(crate) fn counts(&self) -> IndexCounts {
        let mut counts = IndexCounts {
            members: self.by_path.len(),
            ..IndexCounts::default()
        };
        for member in self.by_path.values().map(|id| &self.members[id.0 as usize]) {
            counts.persisted += usize::from(member.record.is_some());
            match member.state() {
                MemberState::Fresh => {
                    counts.fresh += 1;
                    if member.reused {
                        counts.reused += 1;
                    } else {
                        counts.extracted += 1;
                    }
                }
                MemberState::Unindexed | MemberState::Stale => {
                    counts.pending += 1;
                    counts.reuse_pending += usize::from(!member.reuse_tried);
                    if member.retrying {
                        counts.retrying += 1;
                    } else if member.deferred != 0 {
                        counts.deferred += 1;
                    }
                }
                MemberState::Refused(_) => counts.refused += 1,
                MemberState::Limited => counts.limited += 1,
            }
        }
        counts
    }

    /// A pending member's load failed for a reason that may pass: it goes
    /// behind every other pending member. The facts do not change, so this
    /// neither bumps the generation nor logs a change.
    pub(crate) fn defer(&mut self, path: &Path, reason: &str) {
        let Some(id) = self.id_of(path) else {
            return;
        };
        let member = &mut self.members[id.0 as usize];
        if !matches!(member.state(), MemberState::Unindexed | MemberState::Stale) {
            return;
        }
        self.deferrals += 1;
        member.deferred = self.deferrals;
        member.retrying = reason == RETRYING_REFUSAL;
        if self.last_deferral.as_deref() != Some(reason) {
            self.last_deferral = Some(reason.into());
        }
    }

    /// The bytes `insert` will store for this member, computable before
    /// inserting (A12): its own charge, plus the names it brings that this
    /// context does not hold yet. Under one index lock the growth is exact;
    /// names interned meanwhile only make it an overcount.
    pub(crate) fn charge_for(&self, facts: &MemberFacts, stamps: &[MemberStamp]) -> usize {
        Self::member_charge(facts, stamps) + self.name_growth(facts)
    }

    /// What the member itself keeps: its facts without the names (the
    /// interner holds them), its stamps, its include keys, and each list
    /// entry twice (the inverted list, with room to grow, and the
    /// per-member list kept for removal).
    pub(crate) fn member_charge(facts: &MemberFacts, stamps: &[MemberStamp]) -> usize {
        let count = facts.unit_names.len()
            + facts.imports.len()
            + facts.postings.len()
            + facts.import_tails().len();
        let includes: usize = facts
            .slot_paths
            .iter()
            .skip(1)
            .flatten()
            .map(|slot| path_key(slot).len() + 24 + 2 * size_of::<MemberId>())
            .sum();
        facts.own_bytes()
            + stamps_bytes(stamps)
            + count * 2 * (size_of::<MemberId>() + size_of::<NameId>())
            + includes
    }

    /// The bytes the context grows by when it first holds the names of
    /// `facts`: each new interner entry with its string, and each new key
    /// of an inverted list. Both stay until the interner is compacted.
    fn name_growth(&self, facts: &MemberFacts) -> usize {
        let tails = facts.import_tails();
        let mut fresh = HashSet::new();
        let mut keys = 0;
        let roles: [(&[Box<str>], &Inverted); 4] = [
            (&facts.postings, &self.postings),
            (&facts.imports, &self.importers),
            (&tails, &self.importer_tails),
            (&facts.unit_names, &self.units),
        ];
        for (names, list) in roles {
            let mut role = HashSet::new();
            for name in names {
                if !role.insert(&**name) {
                    continue;
                }
                match self.names.get(name) {
                    Some(id) if list.contains_key(&id) => {}
                    Some(_) => keys += 1,
                    None => {
                        keys += 1;
                        fresh.insert(&**name);
                    }
                }
            }
        }
        fresh
            .iter()
            .map(|name| name.len().next_multiple_of(16) + INTERNER_ENTRY_BYTES)
            .sum::<usize>()
            + keys * INVERTED_KEY_BYTES
    }

    /// Returns the old member charge, `None` if `path` is not a member.
    #[cfg(test)]
    pub(crate) fn insert(
        &mut self,
        path: &Path,
        facts: MemberFacts,
        stamps: Vec<MemberStamp>,
        hash: u64,
    ) -> Option<usize> {
        let paid = self.charge_for(&facts, &stamps);
        self.insert_paid(path, facts, stamps, hash, paid)
    }

    /// `insert` once `paid` (at least `charge_for` under this lock) was
    /// charged: the member keeps its own charge, the rest pays for the
    /// names. Returns the old member charge.
    pub(crate) fn insert_paid(
        &mut self,
        path: &Path,
        mut facts: MemberFacts,
        stamps: Vec<MemberStamp>,
        hash: u64,
        paid: usize,
    ) -> Option<usize> {
        let id = self.id_of(path)?;
        let old = self.release(id);
        let posting_ids = facts
            .postings
            .iter()
            .map(|name| self.names.intern(name))
            .collect::<Vec<_>>();
        let import_ids = facts
            .imports
            .iter()
            .map(|name| self.names.intern(name))
            .collect::<Vec<_>>();
        let tail_ids = facts
            .import_tails()
            .iter()
            .map(|name| self.names.intern(name))
            .collect::<Vec<_>>();
        let unit_ids = facts
            .unit_names
            .iter()
            .map(|name| self.names.intern(name))
            .collect::<Vec<_>>();
        for name in &posting_ids {
            list_insert(&mut self.postings, *name, id);
        }
        for name in &import_ids {
            list_insert(&mut self.importers, *name, id);
        }
        for name in &tail_ids {
            list_insert(&mut self.importer_tails, *name, id);
        }
        for name in &unit_ids {
            list_insert(&mut self.units, *name, id);
        }
        for slot in facts.slot_paths.iter().skip(1).flatten() {
            let list = self.include_dependents.entry(path_key(slot)).or_default();
            if let Err(at) = list.binary_search(&id) {
                list.insert(at, id);
            }
        }
        let charged = Self::member_charge(&facts, &stamps);
        // The interner holds the names now; the ids replace them.
        facts.drop_names();
        let member = &mut self.members[id.0 as usize];
        member.posting_ids = posting_ids;
        member.import_ids = import_ids;
        member.tail_ids = tail_ids;
        member.unit_ids = unit_ids;
        member.facts = Some(facts);
        member.stamps = stamps;
        member.hash = Some(hash);
        member.state = Some(MemberState::Fresh);
        member.charged = charged;
        member.reuse_tried = false;
        let path = member.path.clone();
        self.member_bytes += charged;
        self.name_bytes += paid.saturating_sub(charged);
        self.log([path]);
        Some(old)
    }

    /// Re-stamp a member whose bytes were verified unchanged. The facts do
    /// not change, so this neither bumps the generation nor logs a change.
    /// The charge is fixed: `false` (nothing changed) unless `stamps` name
    /// the same paths as the stored ones.
    pub(crate) fn refresh(&mut self, path: &Path, stamps: Vec<MemberStamp>) -> bool {
        let Some(id) = self.id_of(path) else {
            return false;
        };
        let member = &mut self.members[id.0 as usize];
        if member.facts.is_none()
            || member.stamps.len() != stamps.len()
            || member
                .stamps
                .iter()
                .zip(&stamps)
                .any(|(old, new)| old.path != new.path)
        {
            return false;
        }
        member.stamps = stamps;
        member.state = Some(MemberState::Fresh);
        member.deferred = 0;
        member.retrying = false;
        true
    }

    /// Returns the old charge, `None` if `path` is not a member. The member
    /// keeps no facts and stays an unverified candidate until it is stale.
    pub(crate) fn limit(&mut self, path: &Path) -> Option<usize> {
        let id = self.id_of(path)?;
        let old = self.release(id);
        let member = &mut self.members[id.0 as usize];
        member.hash = None;
        member.state = Some(MemberState::Limited);
        let path = member.path.clone();
        self.log([path]);
        Some(old)
    }

    /// Returns the old charge, `None` if `path` is not a member. A refused
    /// member (a stable reason, A6) drops its facts, so it is no include
    /// dependent of anything until it is indexed again.
    pub(crate) fn refuse(&mut self, path: &Path, reason: String) -> Option<usize> {
        let id = self.id_of(path)?;
        let old = self.release(id);
        let member = &mut self.members[id.0 as usize];
        member.hash = None;
        member.state = Some(MemberState::Refused(reason.into()));
        let path = member.path.clone();
        self.log([path]);
        Some(old)
    }

    fn stale_one(&mut self, id: MemberId) -> bool {
        let member = &mut self.members[id.0 as usize];
        match member.state() {
            MemberState::Fresh | MemberState::Refused(_) | MemberState::Limited => {
                member.state = Some(MemberState::Stale);
                member.record = None;
                member.reuse_tried = false;
                member.reused = false;
                true
            }
            MemberState::Unindexed | MemberState::Stale => false,
        }
    }

    /// `path` is a member or an include dependency of members.
    pub(crate) fn mark_stale(&mut self, path: &Path) -> bool {
        let key = path_key(path);
        let mut ids: Vec<MemberId> = self.by_path.get(&key).copied().into_iter().collect();
        ids.extend(
            self.include_dependents
                .get(&key)
                .into_iter()
                .flatten()
                .copied(),
        );
        let mut changed = Vec::new();
        for id in ids {
            if self.stale_one(id) {
                changed.push(self.members[id.0 as usize].path.clone());
            }
        }
        let any = !changed.is_empty();
        if any {
            self.log(changed);
        }
        any
    }

    pub(crate) fn mark_all_stale(&mut self) {
        let ids: Vec<MemberId> = self.by_path.values().copied().collect();
        let mut changed = Vec::new();
        for id in ids {
            if self.stale_one(id) {
                changed.push(self.members[id.0 as usize].path.clone());
            }
        }
        if !changed.is_empty() {
            self.log(changed);
        }
    }

    pub(crate) fn include_dependents(&self, path: &Path) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .include_dependents
            .get(&path_key(path))
            .into_iter()
            .flatten()
            .map(|id| self.members[id.0 as usize].path.clone())
            .collect();
        paths.sort_by_cached_key(|path| path_key(path));
        paths
    }

    fn live_members_with_ids(&self) -> impl Iterator<Item = (MemberId, &Member)> {
        self.by_path
            .values()
            .map(|id| (*id, &self.members[id.0 as usize]))
    }

    pub(crate) fn candidates(&self, query: &CandidateQuery) -> Candidates {
        // None: no posting filter (a non-ASCII or absent name proves nothing).
        let with_posting: Option<HashSet<MemberId>> = query
            .names
            .iter()
            .map(|name| posting_key(name))
            .collect::<Option<Vec<_>>>()
            .filter(|keys| !keys.is_empty())
            .map(|keys| {
                keys.iter()
                    .filter_map(|key| self.names.get(key))
                    .flat_map(|id| self.postings.get(&id).into_iter().flatten().copied())
                    .collect()
            });
        let importers: Option<HashSet<MemberId>> = query.top_level_units.as_ref().map(|units| {
            let mut names = HashSet::new();
            for unit in units {
                names.extend(import_names_for(unit, &query.aliases));
            }
            if names
                .iter()
                .any(|name| IMPLICIT_UNITS.contains(&name.as_str()))
            {
                return self.by_path.values().copied().collect();
            }
            let tails: HashSet<String> = names.iter().map(|name| last_segment(name)).collect();
            let exact = names
                .iter()
                .filter_map(|name| self.names.get(name))
                .flat_map(|id| {
                    self.importers
                        .get(&id)
                        .into_iter()
                        .flatten()
                        .chain(self.units.get(&id).into_iter().flatten())
                        .copied()
                });
            let short = tails
                .iter()
                .filter_map(|tail| self.names.get(tail))
                .flat_map(|id| self.importer_tails.get(&id).into_iter().flatten().copied());
            exact.chain(short).collect()
        });
        let mut result = Candidates::default();
        for (id, member) in self.live_members_with_ids() {
            match member.state() {
                MemberState::Unindexed | MemberState::Stale | MemberState::Limited => {
                    result.unverified.push(member.path.clone());
                }
                MemberState::Refused(reason) => {
                    result.refused.push((member.path.clone(), reason.clone()));
                }
                MemberState::Fresh => {
                    let posting = with_posting.as_ref().is_none_or(|set| set.contains(&id));
                    let scope = importers.as_ref().is_none_or(|set| set.contains(&id));
                    if posting && scope {
                        result.selected.push(member.path.clone());
                    } else {
                        result.excluded.push(member.path.clone());
                    }
                }
            }
        }
        result
    }

    /// One pass over the members for workspace symbols (spec 6.2): the
    /// declarations of Fresh members whose display name contains `query`
    /// (case-insensitive), in path order, with the members they came from.
    /// Fresh members keyed in `direct` (open or deleted files and their
    /// include dependents) are listed for a direct read instead. A
    /// declaration already in `seen` (another context's copy) is not
    /// repeated; once `seen` holds more than `limit`, the scan stops.
    /// Eligibility and container mirror `workspace_eligible` and
    /// `workspace_container_name` (`navigation/symbols.rs`).
    pub(crate) fn symbol_scan(
        &self,
        query: &str,
        direct: &HashSet<String>,
        seen: &mut HashSet<SymbolKey>,
        limit: usize,
    ) -> SymbolScan {
        let query = query.to_lowercase();
        let mut scan = SymbolScan::default();
        for (key, id) in &self.by_path {
            let member = &self.members[id.0 as usize];
            let facts = match member.state() {
                MemberState::Unindexed | MemberState::Stale | MemberState::Limited => {
                    scan.unverified.push(member.path.clone());
                    continue;
                }
                MemberState::Refused(reason) => {
                    scan.refused.push((member.path.clone(), reason.clone()));
                    continue;
                }
                MemberState::Fresh => match &member.facts {
                    Some(facts) => facts,
                    None => continue,
                },
            };
            if !direct.is_empty() && direct.contains(key) {
                scan.direct.push(member.path.clone());
                continue;
            }
            let mut contributed = false;
            for declaration in facts.declarations.iter() {
                let eligible = match declaration.kind {
                    DeclarationKind::Variable | DeclarationKind::Constant => declaration.unit_scope,
                    _ => true,
                };
                if !eligible || !name_matches(&declaration.display, &query) {
                    continue;
                }
                let Some((slot, range)) = declaration.location else {
                    continue;
                };
                let Some(Some(path)) = facts.slot_paths.get(slot as usize) else {
                    continue;
                };
                let slot_key = if slot == 0 {
                    key.clone()
                } else {
                    let slot_key = path_key(path);
                    if direct.contains(&slot_key) {
                        continue;
                    }
                    slot_key
                };
                if !seen.insert((slot_key, range, declaration.display.clone())) {
                    continue;
                }
                if seen.len() > limit {
                    scan.exceeded = true;
                    return scan;
                }
                let container = if declaration.kind == DeclarationKind::Unit {
                    None
                } else {
                    declaration
                        .owner
                        .as_deref()
                        .map(str::to_owned)
                        .or_else(|| Some(facts.unit_display.to_string()))
                };
                scan.symbols.push(IndexedSymbol {
                    path: path.clone(),
                    member: member.path.clone(),
                    declaration: declaration.clone(),
                    container,
                });
                contributed = true;
            }
            if contributed {
                scan.contributing.push(member.path.clone());
            }
        }
        scan
    }

    pub(crate) fn stamps_of(&self, path: &Path) -> Option<&[MemberStamp]> {
        self.id_of(path)
            .map(|id| self.members[id.0 as usize].stamps.as_slice())
    }

    pub(crate) fn state_of(&self, path: &Path) -> Option<MemberState> {
        self.id_of(path)
            .map(|id| self.members[id.0 as usize].state().clone())
    }

    /// The slot paths of an indexed (or stale) member; `None` without facts.
    pub(crate) fn slot_paths_of(&self, path: &Path) -> Option<&[Option<PathBuf>]> {
        let id = self.id_of(path)?;
        self.members[id.0 as usize]
            .facts
            .as_ref()
            .map(|facts| &*facts.slot_paths)
    }

    pub(crate) fn coverage(&self) -> IndexCoverage {
        let mut coverage = IndexCoverage {
            members: self.by_path.len(),
            fresh: 0,
            pending: 0,
            refused: 0,
            first_refusal: None,
            deferred: 0,
            first_deferral: None,
        };
        for (_, member) in self.live_members_with_ids() {
            match member.state() {
                MemberState::Fresh => coverage.fresh += 1,
                MemberState::Unindexed | MemberState::Stale => {
                    coverage.pending += 1;
                    coverage.deferred += usize::from(member.deferred != 0);
                }
                MemberState::Limited => {}
                MemberState::Refused(reason) => {
                    coverage.refused += 1;
                    coverage.first_refusal.get_or_insert_with(|| reason.clone());
                }
            }
        }
        if coverage.deferred > 0 {
            coverage.first_deferral = self.last_deferral.clone();
        }
        coverage
    }

    pub(crate) fn bytes(&self) -> usize {
        self.member_bytes + self.name_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pascal_core::facts::*;

    fn certain() -> Certainty {
        Certainty {
            activity: Activity::Active,
            recovered: false,
        }
    }

    fn source(slot: u32, start: u32, end: u32) -> FactLocation {
        FactLocation::Source {
            slot,
            range: TextRange { start, end },
        }
    }

    fn facts(
        unit: &str,
        imports: &[&str],
        postings: &[&str],
        decls: &[(&str, DeclarationKind, DeclarationShape, bool)],
    ) -> ArtifactFacts {
        let mut declarations = vec![DeclarationFact {
            id: 0,
            name: unit.to_owned(),
            display: unit.to_owned(),
            kind: DeclarationKind::Unit,
            visibility: Visibility::Public,
            region: Region::Interface,
            origin: Origin::Declaration,
            exported: true,
            parent: None,
            location: source(0, 5, 5 + unit.len() as u32),
            certainty: certain(),
            extent: source(0, 0, 100),
            shape: DeclarationShape::Unit,
            unit_scope: true,
            owner: None,
        }];
        for (index, (name, kind, shape, unit_scope)) in decls.iter().enumerate() {
            declarations.push(DeclarationFact {
                id: index as u32 + 1,
                name: name.to_lowercase(),
                display: (*name).to_owned(),
                kind: *kind,
                visibility: Visibility::Public,
                region: Region::Interface,
                origin: Origin::Declaration,
                exported: true,
                parent: None,
                location: source(0, 10, 10 + name.len() as u32),
                certainty: certain(),
                extent: source(0, 10, 40),
                shape: *shape,
                unit_scope: *unit_scope,
                owner: None,
            });
        }
        ArtifactFacts {
            declarations,
            imports: imports
                .iter()
                .enumerate()
                .map(|(id, name)| ImportFact {
                    id: id as u32,
                    name: (*name).to_owned(),
                    display: (*name).to_owned(),
                    section: pascal_core::ImportSection::Interface,
                    location: source(0, 0, 1),
                    certainty: certain(),
                })
                .collect(),
            postings: postings
                .iter()
                .map(|name| PostingFact {
                    name: (*name).to_owned(),
                    occurrences: vec![PostingOccurrence {
                        slot: 0,
                        range: TextRange { start: 0, end: 1 },
                        activity: PostingActivity::Active,
                    }],
                })
                .collect(),
            ..ArtifactFacts::default()
        }
    }

    fn member(index: &mut ContextIndex, path: &str, facts: &ArtifactFacts) {
        let path = PathBuf::from(path);
        index.set_members({
            let mut paths = index.member_paths();
            paths.push(path.clone());
            paths.sort();
            paths
        });
        index.insert(
            &path,
            MemberFacts::from_facts(&path, facts, vec![]),
            vec![],
            1,
        );
    }

    fn facts_for(index: &mut ContextIndex, path: &str) {
        index.insert(
            Path::new(path),
            MemberFacts::from_facts(Path::new(path), &ArtifactFacts::default(), vec![]),
            vec![],
            1,
        );
    }

    fn digest(byte: u8) -> Digest {
        Digest::from_bytes([byte; 32])
    }

    #[test]
    fn reuse_pending_skips_members_tried_since_their_last_invalidation() {
        let mut index = ContextIndex::new();
        index.set_members(vec!["/p/A.pas".into(), "/p/B.pas".into()]);
        index.mark_reuse_tried(Path::new("/p/A.pas"));
        assert_eq!(index.reuse_pending(10), vec![PathBuf::from("/p/B.pas")]);
        assert_eq!(index.pending(10).len(), 2);
        facts_for(&mut index, "/p/A.pas");
        index.mark_stale(Path::new("/p/A.pas"));
        assert_eq!(index.reuse_pending(10).len(), 2);
    }

    #[test]
    fn reinserting_a_member_clears_its_reuse_flags() {
        let mut index = ContextIndex::new();
        index.set_members(vec!["/p/A.pas".into()]);
        index.mark_reuse_tried(Path::new("/p/A.pas"));
        facts_for(&mut index, "/p/A.pas");
        index.mark_reused(Path::new("/p/A.pas"));
        assert_eq!(index.counts().reused, 1);
        facts_for(&mut index, "/p/A.pas");
        let counts = index.counts();
        assert_eq!((counts.reused, counts.extracted), (0, 1));
        index.mark_all_stale();
        assert_eq!(index.reuse_pending(10).len(), 1);
    }

    #[test]
    fn an_invalidated_member_loses_its_record() {
        let mut index = ContextIndex::new();
        let a = Path::new("/p/A.pas");
        let b = Path::new("/p/B.pas");
        let c = Path::new("/p/C.pas");
        let d = Path::new("/p/D.pas");
        index.set_members(vec![a.into(), b.into(), c.into(), d.into()]);
        assert!(
            !index.set_record(a, digest(1)),
            "unindexed members hold none"
        );
        for path in [a, b, c, d] {
            facts_for(&mut index, path.to_str().unwrap());
        }
        for (path, byte) in [(a, 1), (b, 2), (c, 3), (d, 4)] {
            assert!(index.set_record(path, digest(byte)));
        }
        assert_eq!(index.record(a), Some(digest(1)));
        assert_eq!(index.counts().persisted, 4);
        index.mark_stale(a);
        index.refuse(b, "no".into());
        index.limit(c);
        assert_eq!(index.record(a), None);
        assert_eq!(index.record(b), None);
        assert_eq!(index.record(c), None);
        assert_eq!(index.record(d), Some(digest(4)));
        assert!(
            index.set_record(b, digest(9)),
            "a refused member can hold one"
        );
        assert!(
            index.set_record(c, digest(8)),
            "a limited member can hold one"
        );
        index.set_members(vec![a.into(), b.into(), c.into()]);
        assert_eq!(
            index.records(),
            vec![
                (a.to_path_buf(), None),
                (b.to_path_buf(), Some(digest(9))),
                (c.to_path_buf(), Some(digest(8))),
            ]
        );
    }

    #[test]
    fn counts_separate_reused_from_extracted() {
        let mut index = ContextIndex::new();
        index.set_members(vec![
            "/p/A.pas".into(),
            "/p/B.pas".into(),
            "/p/C.pas".into(),
            "/p/D.pas".into(),
            "/p/E.pas".into(),
        ]);
        facts_for(&mut index, "/p/A.pas");
        facts_for(&mut index, "/p/B.pas");
        index.mark_reused(Path::new("/p/A.pas"));
        index.mark_reuse_tried(Path::new("/p/C.pas"));
        index.defer(Path::new("/p/C.pas"), "busy");
        index.defer(Path::new("/p/D.pas"), RETRYING_REFUSAL);
        let counts = index.counts();
        assert_eq!(
            counts,
            IndexCounts {
                members: 5,
                fresh: 2,
                reused: 1,
                extracted: 1,
                pending: 3,
                reuse_pending: 2,
                refused: 0,
                limited: 0,
                deferred: 1,
                retrying: 1,
                persisted: 0,
            }
        );
    }

    #[test]
    fn a_proven_record_is_charged_with_its_member() {
        let path = Path::new("/p/A.pas");
        let slot = size_of::<Option<Digest>>();
        assert!(
            membership_charge(path) >= 3 * path.as_os_str().len() + slot + 64,
            "the slot is charged with the member"
        );
        assert!(member_slot_bytes() >= slot);
    }

    fn top_level(units: &[&str], names: &[&str]) -> CandidateQuery {
        CandidateQuery {
            names: names.iter().map(|n| n.to_string()).collect(),
            top_level_units: Some(units.iter().map(|u| u.to_string()).collect()),
            aliases: Vec::new(),
        }
    }

    fn member_rule(names: &[&str]) -> CandidateQuery {
        CandidateQuery {
            names: names.iter().map(|n| n.to_string()).collect(),
            top_level_units: None,
            aliases: Vec::new(),
        }
    }

    fn paths(values: &[&str]) -> Vec<PathBuf> {
        values.iter().map(PathBuf::from).collect()
    }

    fn abc(index: &mut ContextIndex) {
        member(
            index,
            "/w/Shared.pas",
            &facts("shared", &[], &["sharedvalue"], &[]),
        );
        member(
            index,
            "/w/A.pas",
            &facts("a", &["shared"], &["sharedvalue"], &[]),
        );
        member(index, "/w/B.pas", &facts("b", &["shared"], &[], &[]));
        member(index, "/w/C.pas", &facts("c", &[], &["sharedvalue"], &[]));
    }

    #[test]
    fn top_level_queries_select_importers_with_a_posting() {
        let mut index = ContextIndex::new();
        abc(&mut index);
        let candidates = index.candidates(&top_level(&["shared"], &["SharedValue"]));
        assert_eq!(candidates.selected, paths(&["/w/A.pas", "/w/Shared.pas"]));
        assert_eq!(candidates.excluded, paths(&["/w/B.pas", "/w/C.pas"]));
        assert!(candidates.unverified.is_empty());
    }

    #[test]
    fn member_queries_ignore_imports() {
        let mut index = ContextIndex::new();
        abc(&mut index);
        let candidates = index.candidates(&member_rule(&["SharedValue"]));
        assert_eq!(
            candidates.selected,
            paths(&["/w/A.pas", "/w/C.pas", "/w/Shared.pas"])
        );
        assert_eq!(candidates.excluded, paths(&["/w/B.pas"]));
    }

    #[test]
    fn import_names_cover_namespaces_and_aliases() {
        let names = import_names_for("system.sysutils", &[("sysutl".into(), "sysutils".into())]);
        let expected: HashSet<String> = ["system.sysutils", "sysutils", "sysutl"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(names, expected);
        let mut index = ContextIndex::new();
        member(
            &mut index,
            "/w/A.pas",
            &facts("a", &["sysutils"], &["x"], &[]),
        );
        member(&mut index, "/w/B.pas", &facts("b", &["other"], &["x"], &[]));
        let candidates = index.candidates(&top_level(&["system.sysutils"], &["x"]));
        assert_eq!(candidates.selected, paths(&["/w/A.pas"]));
        assert_eq!(candidates.excluded, paths(&["/w/B.pas"]));
    }

    #[test]
    fn implicit_units_are_imported_by_everyone() {
        let mut index = ContextIndex::new();
        abc(&mut index);
        let candidates = index.candidates(&top_level(&["system"], &["SharedValue"]));
        assert_eq!(
            candidates.selected,
            paths(&["/w/A.pas", "/w/C.pas", "/w/Shared.pas"])
        );
    }

    #[test]
    fn dotted_names_match_by_last_segment() {
        let mut index = ContextIndex::new();
        member(&mut index, "/w/A.pas", &facts("a", &[], &["thing"], &[]));
        member(&mut index, "/w/B.pas", &facts("b", &[], &["other"], &[]));
        let candidates = index.candidates(&member_rule(&["Vendor.Lib.Thing"]));
        assert_eq!(candidates.selected, paths(&["/w/A.pas"]));
        assert_eq!(candidates.excluded, paths(&["/w/B.pas"]));
    }

    #[test]
    fn non_ascii_names_skip_the_posting_filter() {
        let mut index = ContextIndex::new();
        abc(&mut index);
        let candidates = index.candidates(&member_rule(&["Ärger"]));
        assert_eq!(candidates.selected.len(), 4);
        let candidates = index.candidates(&top_level(&["shared"], &["Ärger"]));
        assert_eq!(
            candidates.selected,
            paths(&["/w/A.pas", "/w/B.pas", "/w/Shared.pas"])
        );
        assert_eq!(candidates.excluded, paths(&["/w/C.pas"]));
    }

    #[test]
    fn every_posting_activity_counts() {
        let mut inactive = facts("a", &[], &["thing"], &[]);
        inactive.postings[0].occurrences[0].activity = PostingActivity::Inactive;
        let mut directive = facts("b", &[], &["thing"], &[]);
        directive.postings[0].occurrences[0].activity = PostingActivity::Directive;
        let mut index = ContextIndex::new();
        member(&mut index, "/w/A.pas", &inactive);
        member(&mut index, "/w/B.pas", &directive);
        let candidates = index.candidates(&member_rule(&["thing"]));
        assert_eq!(candidates.selected, paths(&["/w/A.pas", "/w/B.pas"]));
    }

    #[test]
    fn a_member_without_a_posting_is_excluded_only_when_fresh() {
        let mut index = ContextIndex::new();
        abc(&mut index);
        member(&mut index, "/w/D.pas", &facts("d", &[], &[], &[]));
        index.set_members({
            let mut all = index.member_paths();
            all.push(PathBuf::from("/w/E.pas"));
            all.sort();
            all
        });
        assert!(index.mark_stale(Path::new("/w/B.pas")));
        assert!(
            index
                .refuse(Path::new("/w/D.pas"), "too large".into())
                .is_some()
        );
        let candidates = index.candidates(&member_rule(&["SharedValue"]));
        assert_eq!(candidates.unverified, paths(&["/w/B.pas", "/w/E.pas"]));
        assert_eq!(candidates.refused.len(), 1);
        assert_eq!(candidates.refused[0].0, PathBuf::from("/w/D.pas"));
        assert_eq!(&*candidates.refused[0].1, "too large");
        assert!(candidates.excluded.is_empty());
        assert_eq!(index.pending(10), paths(&["/w/B.pas", "/w/E.pas"]));
    }

    #[test]
    fn set_members_reconciles_added_and_removed() {
        let mut index = ContextIndex::new();
        abc(&mut index);
        let g0 = index.generation();
        let (added, removed) = index.set_members(paths(&["/w/A.pas", "/w/N.pas", "/w/Shared.pas"]));
        assert_eq!(added, 1);
        assert!(removed > 0);
        assert!(index.generation() > g0);
        let changes = index.changes_since(g0).unwrap();
        assert!(changes.contains(&PathBuf::from("/w/N.pas")));
        assert!(changes.contains(&PathBuf::from("/w/B.pas")));
        let candidates = index.candidates(&member_rule(&["SharedValue"]));
        assert_eq!(candidates.selected, paths(&["/w/A.pas", "/w/Shared.pas"]));
        assert_eq!(candidates.unverified, paths(&["/w/N.pas"]));
        assert!(candidates.excluded.is_empty());
        let candidates = index.candidates(&top_level(&["shared"], &["SharedValue"]));
        assert_eq!(candidates.selected, paths(&["/w/A.pas", "/w/Shared.pas"]));
        assert!(index.include_dependents(Path::new("/w/B.pas")).is_empty());
        index.set_members(Vec::new());
        assert_eq!(index.bytes(), 0);
        assert!(index.candidates(&member_rule(&["x"])).selected.is_empty());
    }

    #[test]
    fn insert_replaces_old_facts() {
        let mut index = ContextIndex::new();
        abc(&mut index);
        let before_facts = MemberFacts::from_facts(
            Path::new("/w/A.pas"),
            &facts("a", &["shared"], &["sharedvalue"], &[]),
            vec![],
        );
        let before = index.charge_for(&before_facts, &[]);
        let after = MemberFacts::from_facts(
            Path::new("/w/A.pas"),
            &facts("a", &["shared"], &[], &[]),
            vec![],
        );
        let old = index.insert(Path::new("/w/A.pas"), after, vec![], 2);
        assert_eq!(old, Some(before));
        let candidates = index.candidates(&member_rule(&["SharedValue"]));
        assert_eq!(candidates.selected, paths(&["/w/C.pas", "/w/Shared.pas"]));
        assert_eq!(
            index.insert(
                Path::new("/w/Z.pas"),
                MemberFacts::from_facts(Path::new("/w/Z.pas"), &facts("z", &[], &[], &[]), vec![]),
                vec![],
                1
            ),
            None
        );
    }

    #[test]
    fn include_dependents_follow_slot_paths() {
        let mut index = ContextIndex::new();
        let path = PathBuf::from("/w/A.pas");
        index.set_members(vec![path.clone()]);
        let slots = vec![Some(path.clone()), Some(PathBuf::from("/w/x.inc"))];
        index.insert(
            &path,
            MemberFacts::from_facts(&path, &facts("a", &[], &[], &[]), slots),
            vec![],
            1,
        );
        assert_eq!(
            index.include_dependents(Path::new("/w/x.inc")),
            vec![path.clone()]
        );
        assert!(index.mark_stale(Path::new("/w/x.inc")));
        assert_eq!(index.pending(10), vec![path.clone()]);
        index.set_members(Vec::new());
        assert!(index.include_dependents(Path::new("/w/x.inc")).is_empty());
    }

    #[test]
    fn the_change_log_is_bounded() {
        let mut index = ContextIndex::new();
        let path = PathBuf::from("/w/A.pas");
        index.set_members(vec![path.clone()]);
        for _ in 0..=MAX_INDEX_CHANGES {
            index.mark_all_stale();
            index.insert(
                &path,
                MemberFacts::from_facts(&path, &facts("a", &[], &[], &[]), vec![]),
                vec![],
                1,
            );
        }
        assert!(index.changes_since(0).is_none());
        let latest = index.generation();
        assert_eq!(index.changes_since(latest), Some(Vec::new()));
    }

    #[test]
    fn symbols_match_case_insensitively_and_skip_ineligible() {
        let mut with_owner = facts(
            "shared",
            &[],
            &[],
            &[
                (
                    "SharedConst",
                    DeclarationKind::Constant,
                    DeclarationShape::Constant,
                    true,
                ),
                (
                    "SharedType",
                    DeclarationKind::Type,
                    DeclarationShape::Class,
                    true,
                ),
                (
                    "SharedField",
                    DeclarationKind::Field,
                    DeclarationShape::Field,
                    true,
                ),
                (
                    "SharedVar",
                    DeclarationKind::Variable,
                    DeclarationShape::Variable,
                    false,
                ),
                (
                    "SharedLocalConst",
                    DeclarationKind::Constant,
                    DeclarationShape::Constant,
                    false,
                ),
                (
                    "SharedHidden",
                    DeclarationKind::Routine,
                    DeclarationShape::Function,
                    true,
                ),
                (
                    "Other",
                    DeclarationKind::Routine,
                    DeclarationShape::Function,
                    true,
                ),
            ],
        );
        with_owner.declarations[3].owner = Some("SharedType".into());
        with_owner.declarations[6].location = FactLocation::Expanded {
            range: TextRange { start: 0, end: 1 },
        };
        with_owner.declarations[6].extent = with_owner.declarations[6].location;
        let mut index = ContextIndex::new();
        member(&mut index, "/w/Shared.pas", &with_owner);
        let symbols = scan(&index, "sha", &[]).symbols;
        let names: Vec<(&str, Option<&str>)> = symbols
            .iter()
            .map(|symbol| (&*symbol.declaration.display, symbol.container.as_deref()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("shared", None),
                ("SharedConst", Some("shared")),
                ("SharedType", Some("shared")),
                ("SharedField", Some("SharedType")),
            ]
        );
    }

    #[test]
    fn symbols_skip_paths_the_caller_excludes() {
        let mut index = ContextIndex::new();
        member(&mut index, "/w/A.pas", &facts("a", &[], &[], &[]));
        member(&mut index, "/w/B.pas", &facts("b", &[], &[], &[]));
        let found = scan(&index, "", &["/w/A.pas"]);
        assert_eq!(found.symbols.len(), 1);
        assert_eq!(found.symbols[0].path, PathBuf::from("/w/B.pas"));
        assert_eq!(found.direct, paths(&["/w/A.pas"]));
        assert_eq!(found.contributing, paths(&["/w/B.pas"]));
    }

    #[test]
    fn removal_leaves_every_inverted_list() {
        let slots = |path: &str| {
            vec![None, Some(PathBuf::from("/w/x.inc"))]
                .into_iter()
                .enumerate()
                .map(|(i, p)| if i == 0 { Some(PathBuf::from(path)) } else { p })
                .collect::<Vec<_>>()
        };
        let build = |index: &mut ContextIndex, path: &str, unit: &str| {
            let path = PathBuf::from(path);
            index.insert(
                &path,
                MemberFacts::from_facts(
                    &path,
                    &facts(unit, &["shared"], &["thing"], &[]),
                    slots(path.to_str().unwrap()),
                ),
                vec![],
                1,
            );
        };
        let mut index = ContextIndex::new();
        index.set_members(paths(&["/w/A.pas", "/w/B.pas"]));
        build(&mut index, "/w/A.pas", "a");
        build(&mut index, "/w/B.pas", "b");
        let both = index.bytes();
        index.set_members(paths(&["/w/B.pas"]));
        assert_eq!(
            index.include_dependents(Path::new("/w/x.inc")),
            paths(&["/w/B.pas"])
        );
        assert_eq!(index.postings.values().map(Vec::len).sum::<usize>(), 1);
        assert_eq!(index.importers.values().map(Vec::len).sum::<usize>(), 1);
        assert_eq!(index.units.values().map(Vec::len).sum::<usize>(), 1);
        assert!(index.bytes() < both);
        let only_b = index.bytes();
        index.set_members(paths(&["/w/A2.pas", "/w/B.pas"]));
        assert_eq!(index.bytes(), only_b);
        assert_eq!(
            index
                .candidates(&top_level(&["shared"], &["thing"]))
                .selected,
            paths(&["/w/B.pas"])
        );
    }

    #[test]
    fn dotted_imports_match_by_last_segment() {
        let mut index = ContextIndex::new();
        member(
            &mut index,
            "/w/A.pas",
            &facts("a", &["vendor.foo"], &["x"], &[]),
        );
        member(&mut index, "/w/B.pas", &facts("b", &["foo"], &["x"], &[]));
        member(
            &mut index,
            "/w/C.pas",
            &facts("c", &["vendor.bar"], &["x"], &[]),
        );
        let candidates = index.candidates(&top_level(&["foo"], &["x"]));
        assert_eq!(candidates.selected, paths(&["/w/A.pas", "/w/B.pas"]));
        assert_eq!(candidates.excluded, paths(&["/w/C.pas"]));
        let candidates = index.candidates(&top_level(&["vendor.foo"], &["x"]));
        assert_eq!(candidates.selected, paths(&["/w/A.pas", "/w/B.pas"]));
        let aliased = CandidateQuery {
            names: vec!["x".into()],
            top_level_units: Some(vec!["foo".into()]),
            aliases: vec![("f".into(), "ns.foo".into())],
        };
        member(&mut index, "/w/D.pas", &facts("d", &["lib.f"], &["x"], &[]));
        assert!(
            index
                .candidates(&aliased)
                .selected
                .contains(&PathBuf::from("/w/D.pas"))
        );
    }

    #[test]
    fn symbols_serve_fresh_members_only() {
        let mut index = ContextIndex::new();
        member(&mut index, "/w/A.pas", &facts("a", &[], &[], &[]));
        member(&mut index, "/w/B.pas", &facts("b", &[], &[], &[]));
        index.mark_stale(Path::new("/w/A.pas"));
        let found = scan(&index, "", &[]);
        assert_eq!(found.symbols.len(), 1);
        assert_eq!(found.symbols[0].path, PathBuf::from("/w/B.pas"));
        assert_eq!(found.unverified, paths(&["/w/A.pas"]));
    }

    fn scan(index: &ContextIndex, query: &str, direct: &[&str]) -> SymbolScan {
        let direct = direct
            .iter()
            .map(|path| path_key(Path::new(path)))
            .collect();
        index.symbol_scan(query, &direct, &mut HashSet::new(), usize::MAX)
    }

    #[test]
    fn a_symbol_scan_stops_copying_past_the_limit() {
        let mut index = ContextIndex::new();
        for name in ["a", "b", "c", "d"] {
            member(
                &mut index,
                &format!("/w/{name}.pas"),
                &facts(name, &[], &[], &[]),
            );
        }
        let mut seen = HashSet::new();
        let found = index.symbol_scan("", &HashSet::new(), &mut seen, 2);
        assert!(found.exceeded);
        assert_eq!(found.symbols.len(), 2);
        let mut seen = HashSet::new();
        let found = index.symbol_scan("", &HashSet::new(), &mut seen, 4);
        assert!(!found.exceeded);
        assert_eq!(found.symbols.len(), 4);
        // A second context sharing the declarations adds nothing new.
        let again = index.symbol_scan("", &HashSet::new(), &mut seen, 4);
        assert!(again.symbols.is_empty() && !again.exceeded);
    }

    #[test]
    fn symbol_names_match_case_insensitively() {
        assert!(name_matches("SharedValue", "redval"));
        assert!(!name_matches("SharedValue", "shard"));
        assert!(name_matches("Ärger", "är"));
        assert!(name_matches("Größe", "grö"));
        assert!(name_matches("anything", ""));
    }

    #[test]
    fn charge_for_matches_the_stored_charge() {
        let mut index = ContextIndex::new();
        let a = PathBuf::from("/w/A.pas");
        let b = PathBuf::from("/w/B.pas");
        index.set_members(vec![a.clone(), b.clone()]);
        let slots = vec![Some(a.clone()), Some(PathBuf::from("/w/x.inc"))];
        let facts_a =
            MemberFacts::from_facts(&a, &facts("a", &["vendor.foo"], &["x", "y"], &[]), slots);
        let stamps = vec![MemberStamp {
            path: a.clone(),
            stamp: None,
        }];
        let charge_a = index.charge_for(&facts_a, &stamps);
        let own_a = ContextIndex::member_charge(&facts_a, &stamps);
        index.insert(&a, facts_a, stamps.clone(), 1);
        assert_eq!(index.bytes(), charge_a);
        let facts_b = MemberFacts::from_facts(&b, &facts("b", &["foo"], &["x"], &[]), vec![]);
        let charge_b = index.charge_for(&facts_b, &[]);
        index.insert(&b, facts_b, vec![], 1);
        assert_eq!(index.bytes(), charge_a + charge_b);
        assert!(index.refresh(&a, stamps));
        assert_eq!(index.bytes(), charge_a + charge_b);
        // A refused member gives back its own charge; its names stay
        // charged until the interner is compacted.
        assert_eq!(index.refuse(&a, "r".into()), Some(own_a));
        assert_eq!(index.bytes(), charge_a + charge_b - own_a);
        let (_, removed) = index.set_members(vec![]);
        assert_eq!(removed, charge_a + charge_b - own_a);
        assert_eq!(index.bytes(), 0);
    }

    #[test]
    fn empty_binding_names_skip_the_posting_filter() {
        let mut index = ContextIndex::new();
        member(&mut index, "/w/A.pas", &facts("a", &[], &[], &[]));
        assert_eq!(
            index.candidates(&member_rule(&["Vendor."])).selected.len(),
            1
        );
        assert_eq!(index.candidates(&member_rule(&[""])).selected.len(), 1);
    }

    #[test]
    fn names_are_charged_once_per_context() {
        let mut index = ContextIndex::new();
        let (a, b) = (PathBuf::from("/w/A.pas"), PathBuf::from("/w/B.pas"));
        index.set_members(vec![a.clone(), b.clone()]);
        let shared = |path: &Path, unit: &str| {
            MemberFacts::from_facts(path, &facts(unit, &["shared"], &["x", "y"], &[]), vec![])
        };
        let facts_a = shared(&a, "u");
        let first = index.charge_for(&facts_a, &[]);
        let own = ContextIndex::member_charge(&facts_a, &[]);
        assert!(first > own, "the first member pays for its names");
        index.insert(&a, facts_a, vec![], 1);
        // Only B's file stem is a new name (a new interner entry and a new
        // key of the unit list).
        let facts_b = shared(&b, "u");
        assert_eq!(
            index.charge_for(&facts_b, &[]) - ContextIndex::member_charge(&facts_b, &[]),
            "b".len().next_multiple_of(16) + INTERNER_ENTRY_BYTES + INVERTED_KEY_BYTES,
            "names already interned are not charged again"
        );
        index.insert(&b, facts_b, vec![], 1);
        let both = index.bytes();
        for _ in 0..10 {
            index.insert(&a, shared(&a, "u"), vec![], 1);
        }
        assert_eq!(index.bytes(), both, "reindexing charges no name twice");
        index.set_members(Vec::new());
        assert_eq!(index.bytes(), 0, "compaction releases the names");
    }

    /// `MEMBERS` members of 300 postings, 20 imports and 40 declarations;
    /// `unique` of the postings per member are names no other member uses,
    /// the rest come from a shared vocabulary.
    fn bytes_per_member(unique: usize) -> usize {
        const MEMBERS: usize = 1_000;
        let mut index = ContextIndex::new();
        let member_paths: Vec<PathBuf> = (0..MEMBERS)
            .map(|id| PathBuf::from(format!("/workspace/src/module{id:04}/Unit{id:04}.pas")))
            .collect();
        index.set_members(member_paths.clone());
        for (id, path) in member_paths.iter().enumerate() {
            let imports: Vec<String> = (0..20).map(|n| format!("vendor.core.unit{n}")).collect();
            let imports: Vec<&str> = imports.iter().map(String::as_str).collect();
            let postings: Vec<String> = (0..300)
                .map(|n| {
                    if n < unique {
                        format!("identifier{id:04}x{n:03}")
                    } else {
                        format!("sharedidentifier{:04}", (id * 7 + n) % 2_000)
                    }
                })
                .collect();
            let postings: Vec<&str> = postings.iter().map(String::as_str).collect();
            let declarations: Vec<(String, DeclarationKind, DeclarationShape, bool)> = (0..40)
                .map(|n| {
                    (
                        format!("TDeclaration{id:04}x{n:02}"),
                        DeclarationKind::Type,
                        DeclarationShape::Class,
                        true,
                    )
                })
                .collect();
            let declarations: Vec<(&str, DeclarationKind, DeclarationShape, bool)> = declarations
                .iter()
                .map(|(name, kind, shape, scope)| (name.as_str(), *kind, *shape, *scope))
                .collect();
            let unit = format!("unit{id:04}");
            let facts = MemberFacts::from_facts(
                path,
                &facts(&unit, &imports, &postings, &declarations),
                vec![],
            );
            let stamps = vec![MemberStamp {
                path: path.clone(),
                stamp: None,
            }];
            assert!(index.insert(path, facts, stamps, 1).is_some());
        }
        index.bytes() / MEMBERS
    }

    #[test]
    fn index_bytes_per_member_stay_within_the_estimate() {
        let distinct = bytes_per_member(300);
        let shared = bytes_per_member(30);
        println!(
            "workspace index bytes per member: {distinct} (distinct names), {shared} (shared names)"
        );
        // Every name distinct is the worst case: each member pays for all
        // of its names.
        assert!(
            distinct <= 96 * 1024,
            "{distinct} bytes per member exceeds the 96 KiB estimate"
        );
        assert!(
            shared * 2 < distinct,
            "members sharing names must not pay for them each: {shared} vs {distinct}"
        );
    }
}
