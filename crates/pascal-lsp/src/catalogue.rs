//! Shared directory catalogues (l4d-20).
//!
//! A catalogue records what one root holds under one exclusion set: the
//! directories it listed, each with the stamp taken just before listing it,
//! the Pascal sources and project metadata files among their children, and
//! the directories still to list. It is built breadth-first, one whole
//! directory at a time, in bounded batches that resume where the previous
//! one stopped (`l4d-zf1b.8` drives them from the warmer). A catalogue holds
//! filesystem facts only; every consumer authorizes the paths it uses against
//! its own project context.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pascal_project::ProjectReadStamp;

/// Directories and Pascal sources one catalogue may count.
pub(crate) const MAX_CATALOGUE_ENTRIES: usize = 524_288;
/// Directory entries of any kind one catalogue may read.
pub(crate) const MAX_CATALOGUE_VISITS: usize = 1_048_576;
/// Unreadable paths a catalogue names; the rest are only counted.
const MAX_REPORTED_UNREADABLE: usize = 16;
/// Bytes charged per retained path or name beyond its own length.
const ENTRY_OVERHEAD_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Depth {
    /// The root directory's own files.
    Listing,
    /// Every directory under the root that is not excluded.
    Recursive,
}

/// What a walk excludes, as an identity: the matcher that produced it decides
/// the walk, this decides which catalogue it may share.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Exclusions {
    /// Nothing is excluded.
    Nothing,
    /// The default excluded components (`.git`, `node_modules`, ...).
    Defaults,
    /// The defaults plus client globs, matched relative to the walked root,
    /// `root` and `config_root`.
    Globs {
        root: PathBuf,
        config_root: PathBuf,
        patterns: std::sync::Arc<[String]>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CatalogueKey {
    pub(crate) root: PathBuf,
    pub(crate) depth: Depth,
    pub(crate) exclusions: Exclusions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootKind {
    Directory,
    /// Listed through the link, as `WalkDir` follows a root link.
    Symlink,
    Missing,
}

/// Why a catalogue stopped for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Limit {
    /// `MAX_CATALOGUE_ENTRIES` directories and Pascal sources.
    Entries,
    /// `MAX_CATALOGUE_VISITS` directory entries.
    Visits,
}

/// How far one `advance` call may go. Both bounds are checked between
/// directories: a directory is always listed whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bound {
    pub(crate) counted_at_least: usize,
    pub(crate) max_directories: usize,
}

impl Bound {
    /// Until the catalogue counts `counted` entries (a consumer's need).
    pub(crate) const fn entries(counted: usize) -> Self {
        Self {
            counted_at_least: counted,
            max_directories: usize::MAX,
        }
    }

    /// At most `directories` listings (a background batch).
    #[cfg_attr(not(test), allow(dead_code))] // l4d-zf1b.8 drives background batches
    pub(crate) const fn directories(directories: usize) -> Self {
        Self {
            counted_at_least: MAX_CATALOGUE_ENTRIES,
            max_directories: directories,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    entries: usize,
    visits: usize,
}

const LIMITS: Limits = Limits {
    entries: MAX_CATALOGUE_ENTRIES,
    visits: MAX_CATALOGUE_VISITS,
};

/// Files that count against the entry limit: analyzable Pascal sources.
pub(crate) fn is_counted_file(path: &Path) -> bool {
    crate::workspace::is_analyzable_source_path(path)
}

/// Files a catalogue keeps: analyzable sources and project metadata.
pub(crate) fn is_retained_file(path: &Path) -> bool {
    is_counted_file(path) || crate::workspace::rename::is_project_metadata_path(path)
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

fn stamp(path: &Path) -> Option<ProjectReadStamp> {
    pascal_project::path_stamp_result(path).ok().flatten()
}

fn lowercase(name: &std::ffi::OsStr) -> Box<str> {
    name.to_string_lossy().to_ascii_lowercase().into_boxed_str()
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Catalogue {
    root: PathBuf,
    depth: Depth,
    root_kind: RootKind,
    /// Listed directories with the stamp taken before listing them, root first.
    directories: Vec<(PathBuf, Option<ProjectReadStamp>)>,
    files: Vec<PathBuf>,
    /// Lowercase `.pas` file name to indices into `files`.
    by_name: HashMap<Box<str>, Vec<usize>>,
    /// Lowercase stem to the `.dpk` files of each directory, or its `.dproj`
    /// files where the directory has no `.dpk` of that stem.
    packages: HashMap<Box<str>, Vec<usize>>,
    frontier: VecDeque<PathBuf>,
    counted: usize,
    visits: usize,
    limit: Option<Limit>,
    unreadable: Vec<(PathBuf, String)>,
    unreadable_count: usize,
}

impl Catalogue {
    /// Stamps `root` and decides its kind; lists nothing yet.
    pub(crate) fn new(
        root: &Path,
        depth: Depth,
        before_io: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        before_io()?;
        let root_stamp = stamp(root);
        before_io()?;
        let root_kind = match fs::symlink_metadata(root) {
            Ok(metadata) if metadata.file_type().is_symlink() => RootKind::Symlink,
            Err(error) if error.kind() == io::ErrorKind::NotFound => RootKind::Missing,
            _ => RootKind::Directory,
        };
        let mut catalogue = Self::unlisted(root, depth);
        catalogue.root_kind = root_kind;
        catalogue.directories.push((root.to_path_buf(), root_stamp));
        if root_kind == RootKind::Missing {
            catalogue.frontier.clear();
            catalogue.counted = 0;
        }
        Ok(catalogue)
    }

    /// A catalogue of `root` that has done no filesystem work: incomplete.
    pub(crate) fn unlisted(root: &Path, depth: Depth) -> Self {
        Self {
            root: root.to_path_buf(),
            depth,
            root_kind: RootKind::Directory,
            directories: Vec::new(),
            files: Vec::new(),
            by_name: HashMap::new(),
            packages: HashMap::new(),
            frontier: VecDeque::from([root.to_path_buf()]),
            counted: 1,
            visits: 0,
            limit: None,
            unreadable: Vec::new(),
            unreadable_count: 0,
        }
    }

    /// Lists pending directories until `bound` is reached, the catalogue is
    /// complete, or a limit stops it. `excluded` is applied to every child
    /// before it is listed, kept or counted; `before_io` runs before every
    /// filesystem operation, and its error leaves `self` half-advanced, so a
    /// caller that gets an error must discard this value.
    pub(crate) fn advance(
        &mut self,
        bound: Bound,
        excluded: &dyn Fn(&Path) -> bool,
        before_io: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.advance_within(bound, LIMITS, excluded, before_io)
    }

    fn advance_within(
        &mut self,
        bound: Bound,
        limits: Limits,
        excluded: &dyn Fn(&Path) -> bool,
        before_io: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut listed = 0usize;
        while let Some(directory) = self.frontier.front().cloned() {
            if self.counted >= bound.counted_at_least || listed >= bound.max_directories {
                break;
            }
            listed += 1;
            if directory != self.root {
                before_io()?;
                let directory_stamp = stamp(&directory);
                self.directories.push((directory.clone(), directory_stamp));
            }
            before_io()?;
            #[cfg(test)]
            test_record_listing(&directory);
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) => {
                    self.note_unreadable(&directory, &error);
                    self.frontier.pop_front();
                    continue;
                }
            };
            let mut children = Vec::new();
            for entry in entries {
                before_io()?;
                self.visits += 1;
                if self.visits > limits.visits {
                    self.stop(Limit::Visits);
                    return Ok(());
                }
                match entry {
                    Ok(entry) => children.push(entry),
                    Err(error) => self.note_unreadable(&directory, &error),
                }
            }
            children.sort_by_key(fs::DirEntry::path);
            self.frontier.pop_front();
            if !self.admit_children(children, limits, excluded, before_io)? {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Returns false when the entry limit stopped the catalogue.
    fn admit_children(
        &mut self,
        children: Vec<fs::DirEntry>,
        limits: Limits,
        excluded: &dyn Fn(&Path) -> bool,
        before_io: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<bool, String> {
        let mut subdirectories = Vec::new();
        let mut descriptors: HashMap<Box<str>, (Vec<usize>, Vec<usize>)> = HashMap::new();
        let mut admitted = true;
        for entry in children {
            let path = entry.path();
            before_io()?;
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    self.note_unreadable(&path, &error);
                    continue;
                }
            };
            if file_type.is_symlink() || excluded(&path) {
                continue;
            }
            if file_type.is_dir() {
                if self.depth == Depth::Recursive {
                    if !self.admit(limits) {
                        admitted = false;
                        break;
                    }
                    subdirectories.push(path);
                }
                continue;
            }
            if !file_type.is_file() || !is_retained_file(&path) {
                continue;
            }
            if is_counted_file(&path) && !self.admit(limits) {
                admitted = false;
                break;
            }
            let index = self.files.len();
            if has_extension(&path, "pas")
                && let Some(name) = path.file_name()
            {
                self.by_name.entry(lowercase(name)).or_default().push(index);
            }
            let dpk = has_extension(&path, "dpk");
            if (dpk || has_extension(&path, "dproj"))
                && let Some(stem) = path.file_stem()
            {
                let slot = descriptors.entry(lowercase(stem)).or_default();
                if dpk {
                    slot.0.push(index);
                } else {
                    slot.1.push(index);
                }
            }
            self.files.push(path);
        }
        for (stem, (dpk, dproj)) in descriptors {
            let selected = if dpk.is_empty() { dproj } else { dpk };
            self.packages.entry(stem).or_default().extend(selected);
        }
        if admitted {
            self.frontier.extend(subdirectories);
        }
        Ok(admitted)
    }

    fn admit(&mut self, limits: Limits) -> bool {
        if self.counted >= limits.entries {
            self.stop(Limit::Entries);
            return false;
        }
        self.counted += 1;
        true
    }

    fn stop(&mut self, limit: Limit) {
        self.limit = Some(limit);
        self.frontier.clear();
    }

    fn note_unreadable(&mut self, path: &Path, error: &io::Error) {
        self.unreadable_count += 1;
        if self.unreadable.len() < MAX_REPORTED_UNREADABLE {
            self.unreadable
                .push((path.to_path_buf(), error.to_string()));
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn root_kind(&self) -> RootKind {
        self.root_kind
    }

    /// Every directory was listed and no limit stopped the walk. Unreadable
    /// directories do not make it incomplete; consumers decide.
    pub(crate) fn is_complete(&self) -> bool {
        self.frontier.is_empty() && self.limit.is_none()
    }

    pub(crate) fn limit(&self) -> Option<Limit> {
        self.limit
    }

    pub(crate) fn unreadable(&self) -> &[(PathBuf, String)] {
        &self.unreadable
    }

    pub(crate) fn unreadable_count(&self) -> usize {
        self.unreadable_count
    }

    pub(crate) fn counted(&self) -> usize {
        self.counted
    }

    pub(crate) fn visits(&self) -> usize {
        self.visits
    }

    pub(crate) fn frontier_len(&self) -> usize {
        self.frontier.len()
    }

    pub(crate) fn directories(&self) -> &[(PathBuf, Option<ProjectReadStamp>)] {
        &self.directories
    }

    pub(crate) fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// The retained `.pas` files.
    pub(crate) fn pascal_files(&self) -> impl Iterator<Item = &Path> {
        self.by_name
            .values()
            .flatten()
            .map(|&index| self.files[index].as_path())
    }

    pub(crate) fn pascal_files_named<'a>(
        &'a self,
        lowercase_name: &str,
    ) -> impl Iterator<Item = &'a Path> + use<'a> {
        self.by_name
            .get(lowercase_name)
            .into_iter()
            .flatten()
            .map(|&index| self.files[index].as_path())
    }

    pub(crate) fn package_descriptors<'a>(
        &'a self,
        lowercase_stem: &str,
    ) -> impl Iterator<Item = &'a Path> + use<'a> {
        self.packages
            .get(lowercase_stem)
            .into_iter()
            .flatten()
            .map(|&index| self.files[index].as_path())
    }

    /// What retaining this catalogue costs, for `maxCacheBytes`.
    pub(crate) fn retained_bytes(&self) -> usize {
        let path = |path: &PathBuf| path.as_os_str().len() + ENTRY_OVERHEAD_BYTES;
        let directories = self
            .directories
            .iter()
            .map(|(directory, _)| path(directory) + size_of::<Option<ProjectReadStamp>>())
            .sum::<usize>();
        let files = self
            .files
            .iter()
            .chain(&self.frontier)
            .map(path)
            .sum::<usize>();
        let names = self
            .by_name
            .iter()
            .chain(&self.packages)
            .map(|(name, indices)| {
                name.len() + ENTRY_OVERHEAD_BYTES + indices.len() * size_of::<usize>()
            })
            .sum::<usize>();
        let unreadable = self
            .unreadable
            .iter()
            .map(|(directory, error)| path(directory) + error.len())
            .sum::<usize>();
        size_of::<Self>() + directories + files + names + unreadable
    }
}

/// Catalogues one store keeps; the least recently used goes first.
pub(crate) const MAX_SHARED_CATALOGUES: usize = 1_024;
/// Native directory watches all catalogues together may hold.
pub(crate) const MAX_CATALOGUE_WATCH_DIRECTORIES: usize = 16_384;

/// A stored catalogue as a view sees it.
#[derive(Debug, Clone)]
pub(crate) struct SharedCatalogue {
    pub(crate) catalogue: Arc<Catalogue>,
    /// The store generation it was published under: the token that
    /// `publish` (as `base`) and `remove` compare.
    pub(crate) generation: u64,
    /// Watched natively and confirmed by a stamp check that began after the
    /// watches were registered: it needs no stamp check (Task 8).
    pub(crate) watched: bool,
    /// Every listed directory has a native watch; the next passing stamp
    /// check may confirm it (Task 8).
    pub(crate) watch_registered: bool,
}

/// What a store dropped: its charge and the directory watches it held, both
/// released by the caller once the store is unlocked.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Removed {
    pub(crate) bytes: usize,
    pub(crate) watch_directories: Vec<PathBuf>,
}

#[derive(Debug)]
struct Stored {
    catalogue: Arc<Catalogue>,
    generation: u64,
    last_used: u64,
    bytes: usize,
    watch_directories: Vec<PathBuf>,
    watch_epoch: Option<u64>,
    watched: bool,
}

/// Every catalogue a `ProjectCache` shares, with a reverse map from each
/// listed directory to the catalogues that listed it.
#[derive(Debug, Default)]
pub(crate) struct CatalogueStore {
    entries: HashMap<CatalogueKey, Stored>,
    by_directory: HashMap<PathBuf, HashSet<CatalogueKey>>,
    clock: u64,
    generation: u64,
    watched_directories: usize,
    /// `None` is `MAX_CATALOGUE_WATCH_DIRECTORIES`.
    watch_budget: Option<usize>,
}

impl CatalogueStore {
    /// Advances on every publication and invalidation.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// `watch_epoch` is the cache's current one: watches registered under an
    /// older epoch belong to a watcher that no longer exists.
    pub(crate) fn get(&mut self, key: &CatalogueKey, watch_epoch: u64) -> Option<SharedCatalogue> {
        self.clock += 1;
        let clock = self.clock;
        let stored = self.entries.get_mut(key)?;
        stored.last_used = clock;
        let registered = stored.watch_epoch == Some(watch_epoch);
        Some(SharedCatalogue {
            catalogue: Arc::clone(&stored.catalogue),
            generation: stored.generation,
            watched: registered && stored.watched,
            watch_registered: registered,
        })
    }

    /// Stores `catalogue` when `base` is the generation now stored (`None`
    /// when nothing is). Otherwise another view published or the stored one
    /// was invalidated since the caller read it, and `Err` hands back the
    /// caller's charge and watches.
    pub(crate) fn publish(
        &mut self,
        key: CatalogueKey,
        base: Option<u64>,
        catalogue: Arc<Catalogue>,
        bytes: usize,
        watch_directories: Vec<PathBuf>,
    ) -> Result<(u64, Vec<Removed>), Removed> {
        let current = self.entries.get(&key).map(|stored| stored.generation);
        if current != base {
            return Err(Removed {
                bytes,
                watch_directories,
            });
        }
        let mut removed = self.take(&key).into_iter().collect::<Vec<_>>();
        self.generation += 1;
        self.clock += 1;
        for (directory, _) in catalogue.directories() {
            self.by_directory
                .entry(directory.clone())
                .or_default()
                .insert(key.clone());
        }
        self.watched_directories += watch_directories.len();
        self.entries.insert(
            key,
            Stored {
                catalogue,
                generation: self.generation,
                last_used: self.clock,
                bytes,
                watch_directories,
                watch_epoch: None,
                watched: false,
            },
        );
        while self.entries.len() > MAX_SHARED_CATALOGUES {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, stored)| stored.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            removed.extend(self.take(&victim));
        }
        Ok((self.generation, removed))
    }

    /// Whether `directories` more native watches fit the catalogues' budget.
    pub(crate) fn can_watch(&self, directories: usize) -> bool {
        let budget = self.watch_budget.unwrap_or(MAX_CATALOGUE_WATCH_DIRECTORIES);
        self.watched_directories
            .checked_add(directories)
            .is_some_and(|total| total <= budget)
    }

    #[cfg(test)]
    pub(crate) fn set_watch_budget_for_test(&mut self, directories: usize) {
        self.watch_budget = Some(directories);
    }

    /// Every listed directory of the catalogue published under `generation`
    /// has a native watch of the watcher of `epoch`.
    pub(crate) fn mark_watch_registered(
        &mut self,
        key: &CatalogueKey,
        generation: u64,
        epoch: u64,
    ) {
        if let Some(stored) = self.entries.get_mut(key)
            && stored.generation == generation
        {
            stored.watch_epoch = Some(epoch);
        }
    }

    /// A stamp check that began after the watches were registered passed:
    /// every later change reaches `invalidate`.
    pub(crate) fn confirm_watched(&mut self, key: &CatalogueKey, generation: u64) {
        if let Some(stored) = self.entries.get_mut(key)
            && stored.generation == generation
            && stored.watch_epoch.is_some()
        {
            stored.watched = true;
        }
    }

    pub(crate) fn remove(&mut self, key: &CatalogueKey, generation: u64) -> Option<Removed> {
        if self.entries.get(key)?.generation != generation {
            return None;
        }
        self.generation += 1;
        self.take(key)
    }

    /// Drops the catalogues that listed `path`, and with `listing` (a
    /// membership change) the ones that listed its parent.
    pub(crate) fn invalidate(&mut self, path: &Path, listing: bool) -> Vec<Removed> {
        let mut keys = self.by_directory.get(path).cloned().unwrap_or_default();
        if listing
            && let Some(parent) = path.parent()
            && let Some(parents) = self.by_directory.get(parent)
        {
            keys.extend(parents.iter().cloned());
        }
        if keys.is_empty() {
            return Vec::new();
        }
        self.generation += 1;
        keys.iter().filter_map(|key| self.take(key)).collect()
    }

    pub(crate) fn clear(&mut self) -> Vec<Removed> {
        self.generation += 1;
        self.by_directory.clear();
        self.watched_directories = 0;
        self.entries
            .drain()
            .map(|(_, stored)| Removed {
                bytes: stored.bytes,
                watch_directories: stored.watch_directories,
            })
            .collect()
    }

    fn take(&mut self, key: &CatalogueKey) -> Option<Removed> {
        let stored = self.entries.remove(key)?;
        for (directory, _) in stored.catalogue.directories() {
            if let Some(keys) = self.by_directory.get_mut(directory) {
                keys.remove(key);
                if keys.is_empty() {
                    self.by_directory.remove(directory);
                }
            }
        }
        self.watched_directories = self
            .watched_directories
            .saturating_sub(stored.watch_directories.len());
        Some(Removed {
            bytes: stored.bytes,
            watch_directories: stored.watch_directories,
        })
    }
}

#[cfg(test)]
thread_local! {
    static TEST_LISTINGS: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Records a directory a catalogue walk is about to list.
#[cfg(test)]
pub(crate) fn test_record_listing(directory: &Path) {
    TEST_LISTINGS.with(|listings| listings.borrow_mut().push(directory.to_path_buf()));
}

/// The directories listed on this thread since the last reset.
#[cfg(test)]
pub(crate) fn test_listings() -> Vec<PathBuf> {
    TEST_LISTINGS.with(|listings| listings.borrow().clone())
}

#[cfg(test)]
pub(crate) fn test_reset_listings() {
    TEST_LISTINGS.with(|listings| listings.borrow_mut().clear());
}

#[cfg(test)]
thread_local! {
    static TEST_STAMP_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Records one per-request stamp validation of a stored catalogue.
#[cfg(test)]
pub(crate) fn test_record_stamp_check() {
    TEST_STAMP_CHECKS.with(|checks| checks.set(checks.get() + 1));
}

#[cfg(test)]
pub(crate) fn test_stamp_checks() -> usize {
    TEST_STAMP_CHECKS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn test_reset_stamp_checks() {
    TEST_STAMP_CHECKS.with(|checks| checks.set(0));
}

#[cfg(test)]
impl Catalogue {
    /// Stops the catalogue as `limit` would, without reaching it.
    pub(crate) fn test_stop(&mut self, limit: Limit) {
        self.stop(limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_io() -> impl FnMut() -> Result<(), String> {
        || Ok(())
    }

    fn build(root: &Path, depth: Depth, excluded: &dyn Fn(&Path) -> bool) -> Catalogue {
        let mut io = no_io();
        let mut catalogue = Catalogue::new(root, depth, &mut io).expect("catalogue root");
        catalogue
            .advance(Bound::entries(MAX_CATALOGUE_ENTRIES), excluded, &mut io)
            .expect("catalogue walk");
        catalogue
    }

    fn write(path: &Path) {
        fs::create_dir_all(path.parent().expect("parent")).expect("parent directory");
        fs::write(path, b"").expect("file");
    }

    fn in_node_modules(path: &Path) -> bool {
        path.components()
            .any(|component| component.as_os_str() == "node_modules")
    }

    fn nothing(_: &Path) -> bool {
        false
    }

    fn relative<'a>(paths: impl Iterator<Item = &'a Path>, root: &Path) -> Vec<PathBuf> {
        let mut paths = paths
            .map(|path| path.strip_prefix(root).expect("under root").to_path_buf())
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    #[test]
    fn excluded_directories_are_neither_listed_nor_counted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("Main.pas"));
        write(&root.join("node_modules/pkg/Hidden.pas"));
        test_reset_listings();
        let catalogue = build(root, Depth::Recursive, &in_node_modules);
        assert!(catalogue.is_complete());
        assert_eq!(
            relative(catalogue.pascal_files(), root),
            [PathBuf::from("Main.pas")]
        );
        assert_eq!(catalogue.counted(), 2, "the root and Main.pas");
        assert!(
            test_listings()
                .iter()
                .all(|listed| !in_node_modules(listed)),
            "{:?}",
            test_listings()
        );
    }

    #[test]
    fn only_directories_and_pascal_sources_count() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for name in [
            "A.pas",
            "B.inc",
            "C.dpr",
            "D.dpk",
            "App.dproj",
            "sub/E.pas",
            "sub/data.xml",
        ] {
            write(&root.join(name));
        }
        for index in 0..20 {
            write(&root.join(format!("Notes{index}.txt")));
        }
        let catalogue = build(root, Depth::Recursive, &nothing);
        assert_eq!(catalogue.counted(), 7, "root, A, B, C, D, sub, E");
        assert_eq!(catalogue.visits(), 28, "26 root entries and 2 in sub");
        assert_eq!(catalogue.files().len(), 7, "text files are not retained");
        assert_eq!(
            relative(catalogue.pascal_files(), root),
            [PathBuf::from("A.pas"), PathBuf::from("sub/E.pas")]
        );
        assert_eq!(
            relative(catalogue.package_descriptors("app"), root),
            [PathBuf::from("App.dproj")]
        );
        assert_eq!(
            relative(catalogue.pascal_files_named("e.pas"), root),
            [PathBuf::from("sub/E.pas")]
        );
    }

    #[test]
    fn a_directory_bound_resumes_to_the_one_shot_catalogue() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for name in [
            "R.pas",
            "a/A.pas",
            "a/x/X.pas",
            "b/B.dpk",
            "b/B.dproj",
            "c/C.inc",
        ] {
            write(&root.join(name));
        }
        let one_shot = build(root, Depth::Recursive, &nothing);
        let mut io = no_io();
        let mut batched = Catalogue::new(root, Depth::Recursive, &mut io).unwrap();
        let mut batches = 0;
        while !batched.is_complete() {
            batched
                .advance(Bound::directories(1), &nothing, &mut io)
                .unwrap();
            batches += 1;
            assert!(batches < 100, "the walk must make progress");
        }
        assert_eq!(batched, one_shot);
        assert_eq!(batches, 5, "root, a, b, c, a/x");
    }

    #[test]
    fn the_entry_bound_is_checked_between_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for index in 0..5 {
            write(&root.join(format!("U{index}.pas")));
        }
        write(&root.join("sub/S.pas"));
        let mut io = no_io();
        let mut catalogue = Catalogue::new(root, Depth::Recursive, &mut io).unwrap();
        catalogue
            .advance(Bound::entries(2), &nothing, &mut io)
            .unwrap();
        assert!(!catalogue.is_complete());
        assert_eq!(catalogue.limit(), None, "a bound is not a limit");
        assert_eq!(
            catalogue.counted(),
            7,
            "the root directory is admitted whole"
        );
        assert_eq!(catalogue.frontier_len(), 1);
        catalogue
            .advance(Bound::entries(MAX_CATALOGUE_ENTRIES), &nothing, &mut io)
            .unwrap();
        assert!(catalogue.is_complete());
        assert_eq!(catalogue.pascal_files().count(), 6);
    }

    #[test]
    fn package_descriptors_prefer_dpk_within_each_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for name in ["a/P.dpk", "a/P.dproj", "b/P.dproj"] {
            write(&root.join(name));
        }
        let catalogue = build(root, Depth::Recursive, &nothing);
        assert_eq!(
            relative(catalogue.package_descriptors("p"), root),
            [PathBuf::from("a/P.dpk"), PathBuf::from("b/P.dproj")]
        );
    }

    #[test]
    fn a_listing_holds_only_the_root_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("Top.pas"));
        write(&root.join("sub/Nested.pas"));
        let catalogue = build(root, Depth::Listing, &nothing);
        assert!(catalogue.is_complete());
        assert_eq!(
            relative(catalogue.pascal_files(), root),
            [PathBuf::from("Top.pas")]
        );
        assert_eq!(catalogue.counted(), 2, "the root and Top.pas");
        assert_eq!(catalogue.directories().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_neither_followed_nor_retained() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("real/A.pas"));
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        std::os::unix::fs::symlink(root.join("real/A.pas"), root.join("L.pas")).unwrap();
        test_reset_listings();
        let catalogue = build(root, Depth::Recursive, &nothing);
        assert_eq!(
            relative(catalogue.pascal_files(), root),
            [PathBuf::from("real/A.pas")]
        );
        assert!(!test_listings().contains(&root.join("link")));
    }

    #[test]
    fn a_missing_root_is_complete_and_empty() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("absent");
        let catalogue = build(&root, Depth::Recursive, &nothing);
        assert_eq!(catalogue.root_kind(), RootKind::Missing);
        assert!(catalogue.is_complete());
        assert_eq!(catalogue.counted(), 0);
        assert_eq!(catalogue.directories(), [(root, None)]);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directories_are_reported_and_the_walk_continues() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("locked/Hidden.pas"));
        write(&root.join("open/Seen.pas"));
        let locked = root.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&locked).is_ok() {
            // Running as root: permissions do not restrict listing.
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let catalogue = build(root, Depth::Recursive, &nothing);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(catalogue.is_complete(), "the traversal itself finished");
        assert_eq!(catalogue.unreadable_count(), 1);
        assert_eq!(catalogue.unreadable()[0].0, locked);
        assert_eq!(
            relative(catalogue.pascal_files(), root),
            [PathBuf::from("open/Seen.pas")]
        );
    }

    #[test]
    fn limits_stop_the_catalogue_for_good() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for index in 0..5 {
            write(&root.join(format!("U{index}.pas")));
        }
        write(&root.join("sub/S.pas"));
        let mut io = no_io();
        let mut entries = Catalogue::new(root, Depth::Recursive, &mut io).unwrap();
        entries
            .advance_within(
                Bound::entries(MAX_CATALOGUE_ENTRIES),
                Limits {
                    entries: 3,
                    visits: MAX_CATALOGUE_VISITS,
                },
                &nothing,
                &mut io,
            )
            .unwrap();
        assert_eq!(entries.limit(), Some(Limit::Entries));
        assert!(!entries.is_complete());
        assert_eq!(entries.counted(), 3);
        assert_eq!(entries.frontier_len(), 0);

        let mut visits = Catalogue::new(root, Depth::Recursive, &mut io).unwrap();
        visits
            .advance_within(
                Bound::entries(MAX_CATALOGUE_ENTRIES),
                Limits {
                    entries: MAX_CATALOGUE_ENTRIES,
                    visits: 2,
                },
                &nothing,
                &mut io,
            )
            .unwrap();
        assert_eq!(visits.limit(), Some(Limit::Visits));
        assert!(!visits.is_complete());
    }

    #[test]
    fn an_io_refusal_stops_before_the_next_operation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("a/A.pas"));
        let mut calls = 0usize;
        let mut refuse_fourth = || {
            calls += 1;
            if calls == 4 {
                Err("refused".to_string())
            } else {
                Ok(())
            }
        };
        let mut catalogue = Catalogue::new(root, Depth::Recursive, &mut refuse_fourth).unwrap();
        let error = catalogue
            .advance(
                Bound::entries(MAX_CATALOGUE_ENTRIES),
                &nothing,
                &mut refuse_fourth,
            )
            .unwrap_err();
        assert_eq!(error, "refused");
        assert_eq!(calls, 4, "no filesystem operation follows a refusal");
    }
}
