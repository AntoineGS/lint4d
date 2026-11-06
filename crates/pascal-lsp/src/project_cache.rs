//! Project-scoped cache shared by analysis workers and the warmer.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, WaitTimeoutResult};
use std::time::Duration;

use lsp_types::Url;
use pascal_project::{ProjectContext, ProjectReadStamp};

use crate::catalogue::{Catalogue, CatalogueKey, CatalogueStore, Depth, Removed, SharedCatalogue};
use crate::file_watch::DirectoryWatch;
use crate::include_expansion::ExpansionResult;
use crate::navigation::ParsedDocument;
use crate::navigation::compiled_dcu::project_context_fingerprint;
use crate::workspace::ContextKey;
use crate::workspace::rename::OverlayInput;
use crate::workspace_index::{
    ContextIndex, MAX_WORKSPACE_UNITS, MemberFacts, MemberStamp, membership_charge,
};

/// Default for the `maxCacheBytes` limit.
pub(crate) const DEFAULT_MAX_CACHE_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Retained heap bytes per indexed source byte, measured by
/// `measure_retained_bytes_per_source_byte` on RAD Studio 7.0 RTL/VCL units.
#[allow(dead_code)]
pub(crate) const RETAINED_BYTES_PER_SOURCE_BYTE: usize = 48; // measured 2026-09-29: 46, 45, 45 on RAD Studio 7.0 RTL/VCL

/// Shared directory catalogues (l4d-20) may hold at most this fraction of
/// `maxCacheBytes`. Like a reservation, they may evict unpinned entries to
/// make room, but never past that eighth of the budget.
const CATALOGUE_BUDGET_SHARE: usize = 8;
/// The workspace index may hold a quarter of `maxCacheBytes` (l4d-33).
const INDEX_BUDGET_SHARE: usize = 4;
/// Open-file crawl pins may hold half of `maxCacheBytes` (l4d-axsn), so the
/// index share, interactive parses and persistence transients keep the
/// other half.
const PIN_BUDGET_SHARE: usize = 2;
/// The room an index batch needs to dispatch: one small member's charge.
/// It publishes no parses (A13), so it needs no room for crawl entries.
const INDEX_BATCH_DISPATCH_BYTES: usize = 4 * 1024;
/// Compiled provider interfaces may hold a quarter of `maxCacheBytes`
/// (l4d-4nbs D29); past it their own least recently used are evicted.
const PROVIDER_GRAPH_BUDGET_SHARE: usize = 4;
/// A publication's transient buffers (its encoded facts and evidence) draw
/// on a pool of their own, the smaller of this and a sixteenth of
/// `maxCacheBytes` (13b-3, A2): the crawl's pins and leases filling the
/// parse budget never refuse them. One member's transients are at most
/// `MAX_ARTIFACT_CONTENT_BYTES` (16 MiB) plus its observations, so a few
/// publications fit at once; past the pool they ask the budget.
/// Pooled bytes are outside `maxCacheBytes`: they add at most the pool to
/// it, and never evict or refuse anything charged to it.
const PUBLICATION_POOL_BYTES: usize = 64 * 1024 * 1024;
const PUBLICATION_POOL_SHARE: usize = 16;
pub(crate) const MAX_INDEXED_CONTEXTS: usize = 8;
const MAX_INDEX_REQUESTS: usize = 64;
/// Paths `IndexStore::crawl_published` remembers; past it, it starts over.
/// Several large closures fit (multidev: about 7,000 units per context). An
/// entry is about 250 bytes (path, a few pairs, map overhead), so the map
/// stays under about 8 MiB, plus the reverse input links of the same units.
const MAX_CRAWL_PUBLISHED: usize = 32_768;
/// Publications remembered per path, newest last (contexts or contents a
/// path alternates between).
const MAX_CRAWL_PUBLICATIONS_PER_PATH: usize = 4;
const MAX_OWNERS: usize = 4_096;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Probe {
    /// The path must have exactly this stamp. `None` means it must not exist.
    Stamp {
        path: PathBuf,
        expected: Option<ProjectReadStamp>,
    },
    /// Closed file content. The stamp is a fast path; content decides.
    Content {
        path: PathBuf,
        stamp: Option<ProjectReadStamp>,
        content_hash: u64,
    },
    /// An open document must keep this version and text.
    Overlay {
        uri: Url,
        version: i32,
        content_hash: u64,
    },
}

/// What this process proved about one path while its stamp held (spec §7):
/// the digest of the bytes read, or of the complete directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Proven {
    Content(pascal_cache::SourceContent),
    Listing(pascal_cache::Digest),
}

#[derive(Debug, Clone)]
struct Witness {
    stamp: ProjectReadStamp,
    proven: Proven,
}

/// Witnesses past this are not recorded; validation reads instead.
pub(crate) const MAX_WITNESSES: usize = 131_072;

/// A complete directory listing this process read (R8-1): the names the
/// validator checks editor buffers and deletions against.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Listed {
    pub(crate) files: Vec<PathBuf>,
    pub(crate) directories: Vec<PathBuf>,
}

/// How a memoized listing was read: a listing is the same one only under
/// the same route form and at most the entry limit it was complete under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ListedUnder {
    pub(crate) legacy: bool,
    pub(crate) entry_limit: usize,
}

#[derive(Debug)]
struct ListedWitness {
    stamp: ProjectReadStamp,
    under: ListedUnder,
    listed: Arc<Listed>,
}

/// Listings memoized at once, in listed paths: about 100 bytes each.
pub(crate) const MAX_LISTED_PATHS: usize = 131_072;

#[derive(Debug, Default)]
struct Witnesses {
    proven: HashMap<PathBuf, Witness>,
    listings: HashMap<PathBuf, ListedWitness>,
    /// Paths the memoized listings hold, against `MAX_LISTED_PATHS`.
    listed_paths: usize,
}

impl Witnesses {
    fn remove_listing(&mut self, path: &Path) {
        if let Some(old) = self.listings.remove(path) {
            self.listed_paths -= old.listed.files.len() + old.listed.directories.len();
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn overlay_content_hash(text: &str) -> u64 {
    pascal_project::content_hash_bytes(text.as_bytes())
}

pub(crate) fn unit_value_bytes(indexed_len: usize) -> usize {
    indexed_len.saturating_mul(RETAINED_BYTES_PER_SOURCE_BYTE)
}

pub(crate) fn import_value_bytes(resolved: &pascal_core::ResolvedImports) -> usize {
    resolved.dependencies.iter().fold(4096usize, |total, unit| {
        total
            .saturating_add(unit.source.bytes.len())
            .saturating_add(
                unit.source
                    .decoded_text
                    .as_deref()
                    .map_or(0, |text| text.len()),
            )
    })
}

fn path_bytes(path: &Path) -> usize {
    path.as_os_str().len()
}

fn strings_bytes(strings: &[String]) -> usize {
    strings
        .iter()
        .fold(std::mem::size_of_val(strings), |total, text| {
            total.saturating_add(text.len())
        })
}

fn probes_bytes(probes: &[Probe]) -> usize {
    probes
        .iter()
        .fold(std::mem::size_of_val(probes), |total, probe| {
            total.saturating_add(match probe {
                Probe::Stamp { path, .. } | Probe::Content { path, .. } => path_bytes(path),
                Probe::Overlay { uri, .. } => uri.as_str().len(),
            })
        })
}

fn report_bytes(report: &pascal_core::ResolutionReport) -> usize {
    use pascal_core::ResolutionObservation as O;
    let observations = report.observations.iter().fold(
        std::mem::size_of_val(report.observations.as_slice()),
        |total, observation| {
            let path = match observation {
                O::Directory { path, .. }
                | O::Candidate { path, .. }
                | O::Payload { path, .. }
                | O::Metadata(
                    pascal_project::MetadataObservation::Stat { path }
                    | pascal_project::MetadataObservation::Payload { path, .. },
                ) => path,
                O::ProjectRead(read) => &read.path,
            };
            total.saturating_add(path_bytes(path))
        },
    );
    observations
        .saturating_add(strings_bytes(&report.warnings))
        .saturating_add(std::mem::size_of_val(report.incomplete_reasons.as_slice()))
}

pub(crate) fn expansion_bytes(expansion: &ExpansionResult) -> usize {
    let mut total = std::mem::size_of::<ExpansionResult>();
    let _ = expansion.expanded.visit_recovery_payload(&mut |bytes| {
        total = total.saturating_add(bytes);
        Ok(())
    });
    for include in &expansion.dependencies {
        total = include.observations.iter().fold(
            total
                .saturating_add(std::mem::size_of_val(include))
                .saturating_add(include.text.len())
                .saturating_add(include.uri.as_str().len())
                .saturating_add(std::mem::size_of_val(include.observations.as_slice())),
            |total, observation| total.saturating_add(path_bytes(&observation.path)),
        );
    }
    total.saturating_add(strings_bytes(&expansion.errors))
}

/// Converts resolver observations and dependency revisions into probes, and
/// lists the directories whose watches protect the entry.
pub(crate) fn report_probes(
    report: &pascal_core::ResolutionReport,
    resolved: &pascal_core::ResolvedImports,
) -> (Vec<Probe>, Vec<PathBuf>) {
    fn push_revision(probes: &mut Vec<Probe>, path: &Path, revision: &pascal_core::SourceRevision) {
        match revision {
            pascal_core::SourceRevision::Disk {
                stamp,
                content_hash,
                ..
            } => probes.push(Probe::Content {
                path: path.to_path_buf(),
                stamp: Some(stamp.clone()),
                content_hash: *content_hash,
            }),
            pascal_core::SourceRevision::Overlay {
                version,
                content_hash,
            } => {
                if let Ok(uri) = Url::from_file_path(path) {
                    probes.push(Probe::Overlay {
                        uri,
                        version: *version,
                        content_hash: *content_hash,
                    });
                }
            }
        }
    }

    let mut probes = Vec::new();
    for unit in &resolved.dependencies {
        push_revision(&mut probes, &unit.source.path, &unit.source.revision);
    }
    let (observed, dirs) = observation_probes(&report.observations, &mut |path, revision| {
        let mut probes = Vec::new();
        push_revision(&mut probes, path, revision);
        probes.pop()
    });
    probes.extend(observed);
    (dedup_probes(probes), dirs)
}

/// Probes for resolver observations, and the directories whose watches
/// protect them. `revision` turns a payload's revision into its probe.
pub(crate) fn observation_probes(
    observations: &[pascal_core::ResolutionObservation],
    revision: &mut dyn FnMut(&Path, &pascal_core::SourceRevision) -> Option<Probe>,
) -> (Vec<Probe>, Vec<PathBuf>) {
    use pascal_core::ResolutionObservation as O;
    let mut probes = Vec::new();
    let mut dirs = Vec::new();
    for observation in observations {
        match observation {
            O::Directory { path, stamp, .. } => {
                probes.push(Probe::Stamp {
                    path: path.clone(),
                    expected: stamp.clone(),
                });
                dirs.push(path.clone());
            }
            O::Candidate {
                path,
                stamp,
                present,
                ..
            } => probes.push(Probe::Stamp {
                path: path.clone(),
                expected: if *present { stamp.clone() } else { None },
            }),
            O::Payload {
                path,
                revision: source_revision,
                ..
            } => probes.extend(revision(path, source_revision)),
            O::Metadata(pascal_project::MetadataObservation::Stat { path }) => {
                probes.push(Probe::Stamp {
                    path: path.clone(),
                    expected: pascal_project::path_stamp_result(path).ok().flatten(),
                });
            }
            O::Metadata(pascal_project::MetadataObservation::Payload {
                path,
                stamp,
                content_hash,
                ..
            }) => probes.push(Probe::Content {
                path: path.clone(),
                stamp: stamp.clone(),
                content_hash: *content_hash,
            }),
            O::ProjectRead(read) => probes.push(Probe::Content {
                path: read.path.clone(),
                stamp: Some(read.stamp.clone()),
                content_hash: read.content_hash,
            }),
        }
    }
    dirs.sort();
    dirs.dedup();
    (dedup_probes(probes), dirs)
}

/// `probes` with each exact duplicate dropped, in first-seen order: a hit
/// checks every probe (`probes_hold`), and the resolver observes a
/// directory once per lookup that consults it (rootcause A4,
/// l4d-zf1b.8). Probes of one path that differ are all kept.
pub(crate) fn dedup_probes(probes: Vec<Probe>) -> Vec<Probe> {
    let mut seen: HashMap<&Path, Vec<usize>> = HashMap::new();
    let mut kept = Vec::with_capacity(probes.len());
    for (at, probe) in probes.iter().enumerate() {
        let path = match probe {
            Probe::Stamp { path, .. } | Probe::Content { path, .. } => path.as_path(),
            Probe::Overlay { uri, .. } => Path::new(uri.path()),
        };
        let same = seen.entry(path).or_default();
        if same.iter().any(|&known| probes[known] == *probe) {
            continue;
        }
        same.push(at);
        kept.push(at);
    }
    if kept.len() == probes.len() {
        return probes;
    }
    let mut keep = kept.into_iter().peekable();
    probes
        .into_iter()
        .enumerate()
        .filter_map(|(at, probe)| {
            (keep.peek() == Some(&at)).then(|| {
                keep.next();
                probe
            })
        })
        .collect()
}

/// Every include candidate observed while resolving an expansion, plus the
/// content revision of each include source that was read.
pub(crate) fn expansion_probes(expansion: &ExpansionResult) -> Vec<Probe> {
    expansion
        .dependencies
        .iter()
        .flat_map(|include| include.observations.iter())
        .chain(expansion.expanded.observations.iter())
        .filter_map(|observation| {
            let path = observation.path.clone();
            if let (Some(version), Some(content_hash)) =
                (observation.overlay_version, observation.content_hash)
            {
                return Url::from_file_path(&path).ok().map(|uri| Probe::Overlay {
                    uri,
                    version,
                    content_hash,
                });
            }

            Some(match observation.content_hash {
                Some(content_hash) => Probe::Content {
                    path,
                    stamp: observation.stamp.clone(),
                    content_hash,
                },
                None => Probe::Stamp {
                    path,
                    expected: observation.stamp.clone(),
                },
            })
        })
        .collect()
}

fn current_stamp(path: &Path) -> Result<Option<ProjectReadStamp>, ()> {
    pascal_project::path_stamp_result(path).map_err(|_| ())
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn probes_hold(probes: &[Probe], overlays: &HashMap<Url, OverlayInput>) -> bool {
    use pascal_project::analysis_metrics::{self, Metric};
    analysis_metrics::record(Metric::ProjectCacheProbeCalls, 1);
    // The raw `fs::read` in content probes below is not an authorized payload
    // read and is deliberately outside the payload counters.
    let started = analysis_metrics::enabled().then(std::time::Instant::now);
    let held = probes.iter().all(|probe| match probe {
        Probe::Stamp { path, expected } => current_stamp(path).is_ok_and(|now| now == *expected),
        Probe::Content {
            path,
            stamp,
            content_hash,
        } => {
            // An open document's overlay is authoritative over the disk file.
            if let Some(overlay) = Url::from_file_path(path)
                .ok()
                .and_then(|uri| overlays.get(&uri))
            {
                return overlay_content_hash(&overlay.text) == *content_hash;
            }
            match current_stamp(path) {
                Ok(Some(now))
                    if stamp
                        .as_ref()
                        .is_some_and(|stamp| stamp.proves_unchanged(&now)) =>
                {
                    true
                }
                Ok(Some(_)) => {
                    #[cfg(test)]
                    crate::workspace::resolver::record_test_disk_read(path);
                    std::fs::read(path).is_ok_and(|bytes| {
                        pascal_project::content_hash_bytes(&bytes) == *content_hash
                    })
                }
                _ => false,
            }
        }
        Probe::Overlay {
            uri,
            version,
            content_hash,
        } => overlays.get(uri).is_some_and(|overlay| {
            overlay.version == *version && overlay_content_hash(&overlay.text) == *content_hash
        }),
    });
    if let Some(started) = started {
        analysis_metrics::record(
            Metric::ProjectCacheProbeNanos,
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
        );
    }
    held
}

#[cfg(test)]
mod tests {
    fn resident_bytes() -> usize {
        let statm = std::fs::read_to_string("/proc/self/statm").expect("statm");
        let pages: usize = statm
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse().ok())
            .expect("resident pages");
        pages * 4096
    }

    /// Run manually:
    /// `PASCAL_RTL=~/.local/share/Headless-Delphi/files/RAD\ Studio/7.0/source/Win32 \
    ///  cargo test -p pascal-lsp --release --lib -- --ignored measure_retained --nocapture`
    #[test]
    #[ignore = "manual measurement against local Delphi sources"]
    fn measure_retained_bytes_per_source_byte() {
        let root = std::path::PathBuf::from(std::env::var("PASCAL_RTL").expect("PASCAL_RTL"));
        let files = [
            "rtl/win/Windows.pas",
            "rtl/sys/SysUtils.pas",
            "rtl/common/Classes.pas",
            "vcl/Controls.pas",
            "vcl/Forms.pas",
            "db/DB.pas",
        ];
        let mut index = crate::navigation::NavigationIndex::new();
        let context = pascal_core::conditional::ConditionalContext::default();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let before = resident_bytes();
        let mut source_bytes = 0usize;
        for file in files {
            let path = root.join(file);
            let bytes = std::fs::read(&path).expect("source");
            let text = crate::workspace::resolver::decode_source_bytes(&bytes);
            source_bytes += text.len();
            let uri = lsp_types::Url::from_file_path(&path).expect("uri");
            index
                .update_with_context_and_cached_with_cancel(uri, text, &context, None, &cancel)
                .expect("index");
        }
        let after = resident_bytes();
        let factor = (after.saturating_sub(before)) / source_bytes.max(1);
        println!(
            "source={source_bytes} retained={} factor={factor}",
            after - before
        );
        std::hint::black_box(&index);
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use std::collections::HashMap;

    fn stamp(path: &Path) -> Option<ProjectReadStamp> {
        pascal_project::path_stamp_result(path).ok().flatten()
    }

    #[test]
    fn metrics_probe_wrapper_preserves_outcome() {
        use pascal_project::analysis_metrics::{Metric, TestScope};
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.pas");
        std::fs::write(&file, "unit A;").unwrap();
        let removed = vec![Probe::Stamp {
            path: file.clone(),
            expected: stamp(&file),
        }];
        std::fs::remove_file(&file).unwrap();
        let scope = TestScope::new();
        assert!(probes_hold(&[], &HashMap::new()));
        let after_empty = scope.snapshot();
        assert!(!probes_hold(&removed, &HashMap::new()));
        let after_removed = scope.snapshot();
        assert_eq!(after_removed.value(Metric::ProjectCacheProbeCalls), 2);
        assert!(
            after_removed.value(Metric::ProjectCacheProbeNanos)
                >= after_empty.value(Metric::ProjectCacheProbeNanos)
        );
        after_removed.checked_delta(&after_empty).unwrap();
    }

    /// Rootcause A4: one Directory observation per lookup is one stamp
    /// probe per distinct directory and stamp; a directory seen with two
    /// stamps keeps both probes.
    #[test]
    fn repeated_observations_give_one_probe_each() {
        let temp = tempfile::tempdir().unwrap();
        let directory = |path: &Path, stamp: Option<ProjectReadStamp>| {
            pascal_core::ResolutionObservation::Directory {
                path: path.to_path_buf(),
                entry: pascal_project::ProjectPathEntry::configured(path.to_path_buf()),
                stamp,
                complete: true,
                listed: true,
            }
        };
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let mut observations = Vec::new();
        for _ in 0..50 {
            observations.push(directory(&a, stamp(&a)));
            observations.push(directory(&b, stamp(&b)));
        }
        observations.push(directory(&a, None));
        let (probes, dirs) = observation_probes(&observations, &mut |_, _| None);
        assert_eq!(
            probes,
            vec![
                Probe::Stamp {
                    path: a.clone(),
                    expected: stamp(&a),
                },
                Probe::Stamp {
                    path: b.clone(),
                    expected: stamp(&b),
                },
                Probe::Stamp {
                    path: a.clone(),
                    expected: None,
                },
            ]
        );
        assert_eq!(dirs.len(), 2);
    }

    #[test]
    fn unchanged_stamp_probe_holds() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.pas");
        std::fs::write(&file, "unit A;").unwrap();
        let probes = vec![Probe::Stamp {
            path: file.clone(),
            expected: stamp(&file),
        }];
        assert!(probes_hold(&probes, &HashMap::new()));
    }

    #[test]
    fn created_file_breaks_absence_probe() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.pas");
        let probes = vec![Probe::Stamp {
            path: file.clone(),
            expected: None,
        }];
        assert!(probes_hold(&probes, &HashMap::new()));
        std::fs::write(&file, "unit A;").unwrap();
        assert!(!probes_hold(&probes, &HashMap::new()));
    }

    #[test]
    fn stamp_change_with_identical_content_is_a_hit() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.pas");
        std::fs::write(&file, "unit A;").unwrap();
        let probes = vec![Probe::Content {
            path: file.clone(),
            stamp: stamp(&file),
            content_hash: pascal_project::content_hash_bytes(b"unit A;"),
        }];
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(probes_hold(&probes, &HashMap::new()));
    }

    #[test]
    fn changed_content_breaks_content_probe() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.pas");
        std::fs::write(&file, "unit A;").unwrap();
        let probes = vec![Probe::Content {
            path: file.clone(),
            stamp: stamp(&file),
            content_hash: pascal_project::content_hash_bytes(b"unit A;"),
        }];
        std::fs::write(&file, "unit B;").unwrap();
        assert!(!probes_hold(&probes, &HashMap::new()));
    }

    #[test]
    fn a_same_tick_rewrite_breaks_a_content_probe() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.pas");
        std::fs::write(&file, "unit A;").unwrap();
        let before = stamp(&file).unwrap();
        let probes = vec![Probe::Content {
            path: file.clone(),
            stamp: Some(before.clone()),
            content_hash: pascal_project::content_hash_bytes(b"unit A;"),
        }];
        std::fs::write(&file, "unit B;").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(before.modified.unwrap())
            .unwrap();
        assert!(!probes_hold(&probes, &HashMap::new()));
    }

    #[test]
    fn a_settled_unchanged_content_probe_holds_without_reading() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.pas");
        std::fs::write(&file, "unit A;").unwrap();
        pascal_project::backdate_for_test(&file);
        let probes = vec![Probe::Content {
            path: file.clone(),
            stamp: stamp(&file),
            content_hash: pascal_project::content_hash_bytes(b"unit A;"),
        }];
        crate::workspace::resolver::take_test_disk_reads();
        assert!(probes_hold(&probes, &HashMap::new()));
        assert!(
            crate::workspace::resolver::take_test_disk_reads().is_empty(),
            "a settled stamp is the fast path"
        );
    }

    #[test]
    fn disk_content_probe_follows_the_overlay_once_the_file_is_open() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.pas");
        std::fs::write(&file, "unit A;").unwrap();
        let probes = vec![Probe::Content {
            path: file.clone(),
            stamp: stamp(&file),
            content_hash: pascal_project::content_hash_bytes(b"unit A;"),
        }];
        let open = |text: &str| {
            HashMap::from([(
                Url::from_file_path(&file).unwrap(),
                OverlayInput {
                    text: text.into(),
                    version: 1,
                },
            )])
        };
        assert!(probes_hold(&probes, &open("unit A;")));
        // The overlay is authoritative even while the disk bytes still match.
        assert!(!probes_hold(&probes, &open("unit B;")));
    }

    #[test]
    fn overlay_probe_tracks_version_and_hash() {
        let uri = Url::parse("file:///tmp/A.pas").unwrap();
        let probes = vec![Probe::Overlay {
            uri: uri.clone(),
            version: 3,
            content_hash: overlay_content_hash("unit A;"),
        }];
        let same = HashMap::from([(
            uri.clone(),
            OverlayInput {
                text: "unit A;".into(),
                version: 3,
            },
        )]);
        let edited = HashMap::from([(
            uri.clone(),
            OverlayInput {
                text: "unit A; ".into(),
                version: 4,
            },
        )]);
        assert!(probes_hold(&probes, &same));
        assert!(!probes_hold(&probes, &edited));
        assert!(!probes_hold(&probes, &HashMap::new()));
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn crawl_publications_keep_the_newest_per_path_and_forget_dependents() {
        let cache = ProjectCache::default();
        let unit = Path::new("/p/lib/U.pas");
        let include = PathBuf::from("/p/inc/Part.inc");
        let context = pascal_cache::Digest::from_bytes([1; 32]);
        for hash in 0..=MAX_CRAWL_PUBLICATIONS_PER_PATH as u64 {
            cache.note_published(unit, hash, context, std::slice::from_ref(&include));
        }
        assert!(
            !cache.crawl_published(unit, 0, context),
            "the oldest is dropped"
        );
        for hash in 1..=MAX_CRAWL_PUBLICATIONS_PER_PATH as u64 {
            assert!(cache.crawl_published(unit, hash, context), "{hash}");
        }
        assert!(!cache.crawl_published(unit, 1, pascal_cache::Digest::from_bytes([2; 32])));
        // A bytes-only change of the include drops its dependent.
        cache.invalidate_file_contents(&include);
        assert!(!cache.crawl_published(unit, 1, context));
        // A new file in a directory a publication listed drops it too.
        cache.note_published(unit, 1, context, &[PathBuf::from("/p/lib")]);
        cache.invalidate_file_contents(Path::new("/p/lib/New.pas"));
        assert!(
            cache.crawl_published(unit, 1, context),
            "no listing changed"
        );
        cache.invalidate_path(Path::new("/p/lib/New.pas"));
        assert!(!cache.crawl_published(unit, 1, context));
        // An overflow forgets everything.
        cache.note_published(unit, 1, context, &[]);
        cache.invalidate_after_overflow();
        assert!(!cache.crawl_published(unit, 1, context));
    }

    fn context(name: &str) -> ProjectContext {
        ProjectContext::from_parts(pascal_project::ProjectContextParts {
            project_file: Some(PathBuf::from(name)),
            ..pascal_project::ProjectContextParts::default()
        })
    }

    fn import_value(probes: Vec<Probe>) -> ImportValue {
        ImportValue {
            resolved: Arc::new(pascal_core::ResolvedImports {
                bindings: vec![],
                dependencies: vec![],
                complete: true,
                settled: true,
            }),
            report: Arc::new(pascal_core::ResolutionReport {
                observations: vec![],
                warnings: vec![],
                complete: true,
                incomplete_reasons: vec![],
            }),
            probes,
            watch_dirs: vec![],
        }
    }

    /// A fixture source under the workspace root; Windows file URIs need a
    /// drive, or `to_file_path` fails and the entry's own path goes unindexed.
    fn ws_path(name: &str) -> PathBuf {
        let root = if cfg!(windows) { r"C:\ws" } else { "/ws" };
        Path::new(root).join(name)
    }

    fn uri(name: &str) -> Url {
        Url::from_file_path(ws_path(name)).unwrap()
    }

    #[test]
    fn closure_crawl_requests_are_deduplicated_and_drained() {
        let cache = ProjectCache::new(usize::MAX);
        cache.request_closure_crawl(&uri("Main.pas"));
        cache.request_closure_crawl(&uri("Main.pas"));
        cache.request_closure_crawl(&uri("Other.pas"));
        assert_eq!(
            cache.take_closure_crawl_requests(),
            vec![uri("Main.pas"), uri("Other.pas")]
        );
        assert!(cache.take_closure_crawl_requests().is_empty());
    }

    #[test]
    fn closure_misses_request_a_crawl_only_when_a_new_unit_misses() {
        let cache = ProjectCache::new(usize::MAX);
        let main = uri("Main.pas");
        let misses = |names: &[&str]| names.iter().map(|name| uri(name)).collect::<HashSet<_>>();

        cache.report_closure_misses(&main, misses(&["Sync.pas"]));
        assert_eq!(cache.take_closure_crawl_requests(), vec![main.clone()]);
        cache.report_closure_misses(&main, misses(&["Sync.pas"]));
        assert!(
            cache.take_closure_crawl_requests().is_empty(),
            "a persistent miss must not request another crawl"
        );

        cache.report_closure_misses(&main, misses(&["Sync.pas", "Sibling.pas"]));
        assert_eq!(cache.take_closure_crawl_requests(), vec![main.clone()]);
        cache.report_closure_misses(&main, misses(&["Sync.pas"]));
        assert!(
            cache.take_closure_crawl_requests().is_empty(),
            "a shrinking miss set must not request another crawl"
        );

        cache.report_closure_misses(&main, misses(&[]));
        cache.report_closure_misses(&main, misses(&["Sync.pas"]));
        assert_eq!(
            cache.take_closure_crawl_requests(),
            vec![main],
            "a miss that returns after a complete walk is new again"
        );
    }

    #[test]
    fn an_overflow_forgets_closure_misses() {
        let cache = ProjectCache::new(usize::MAX);
        let main = uri("Main.pas");
        let misses = || HashSet::from([uri("Sync.pas")]);
        cache.report_closure_misses(&main, misses());
        cache.report_closure_misses(&main, misses());
        assert_eq!(cache.take_closure_crawl_requests(), vec![main.clone()]);

        cache.invalidate_after_overflow();
        cache.report_closure_misses(&main, misses());

        assert_eq!(
            cache.take_closure_crawl_requests(),
            vec![main],
            "an overflow discarded what the last crawl stored"
        );
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn parsed_unit(name: &str) -> Arc<crate::navigation::ParsedDocument> {
        let mut index = crate::navigation::NavigationIndex::new();
        let unit = name.trim_end_matches(".pas");
        index
            .update(
                uri(name),
                format!("unit {unit};\ninterface\nimplementation\nend.\n"),
            )
            .unwrap();
        index.parsed_document(&uri(name)).unwrap()
    }

    fn fill_unit(
        cache: &ProjectCache,
        name: &str,
        ctx: &ProjectContext,
        hash: u64,
        probes: Vec<Probe>,
    ) {
        match cache.unit(&uri(name), ctx, hash, &HashMap::new(), &no_cancel()) {
            Lookup::Compute(claim) => cache.store_unit(
                claim,
                UnitValue {
                    parsed: parsed_unit(name),
                    expansion: None,
                    probes,
                    disk: None,
                },
                1,
                &no_cancel(),
            ),
            _ => panic!("expected a miss for {name}"),
        }
    }

    fn interface_value(targets: &[(&str, u64)], complete: bool) -> InterfaceImportsValue {
        InterfaceImportsValue {
            bindings: targets
                .iter()
                .map(|(name, hash)| InterfaceBinding {
                    name: name.trim_end_matches(".pas").to_ascii_lowercase(),
                    uri: uri(name),
                    revision: pascal_core::SourceRevision::Overlay {
                        version: 1,
                        content_hash: *hash,
                    },
                })
                .collect(),
            complete,
            probes: Vec::new(),
        }
    }

    #[test]
    fn peek_unit_returns_verified_entries_for_the_same_project_and_content() {
        let cache = ProjectCache::new(usize::MAX);
        let ctx = context("A.dproj");
        fill_unit(&cache, "Base.pas", &ctx, 7, vec![]);

        assert!(
            cache
                .peek_unit(&uri("Base.pas"), &ctx, 7, &HashMap::new())
                .is_some()
        );
        assert!(
            cache
                .peek_unit(&uri("Base.pas"), &ctx, 8, &HashMap::new())
                .is_none()
        );
        assert!(
            cache
                .peek_unit(&uri("Base.pas"), &context("B.dproj"), 7, &HashMap::new())
                .is_none()
        );
    }

    #[test]
    fn entries_are_not_shared_by_contexts_that_differ_in_analysis_inputs() {
        let cache = ProjectCache::new(usize::MAX);
        let ctx = context("A.dproj");
        fill_unit(&cache, "Base.pas", &ctx, 7, vec![]);
        // Include paths are not part of the fingerprint, so only the
        // context comparison tells these apart.
        let other = ProjectContext::from_parts(pascal_project::ProjectContextParts {
            include_path_entries: vec![pascal_project::ProjectPathEntry::configured(
                PathBuf::from("inc"),
            )],
            ..ctx.clone().into_parts()
        });

        assert!(
            cache
                .peek_unit(&uri("Base.pas"), &other, 7, &HashMap::new())
                .is_none()
        );
        assert!(matches!(
            cache.unit(&uri("Base.pas"), &other, 7, &HashMap::new(), &no_cancel()),
            Lookup::Compute(_)
        ));
    }

    #[test]
    fn peek_unit_misses_when_a_probe_fails() {
        let temp = tempfile::tempdir().unwrap();
        let include = temp.path().join("Defs.inc");
        std::fs::write(&include, "{$DEFINE A}").unwrap();
        let cache = ProjectCache::new(usize::MAX);
        let ctx = context("A.dproj");
        fill_unit(
            &cache,
            "Base.pas",
            &ctx,
            7,
            vec![Probe::Content {
                path: include.clone(),
                stamp: pascal_project::path_stamp_result(&include).ok().flatten(),
                content_hash: pascal_project::content_hash_bytes(b"{$DEFINE A}"),
            }],
        );
        assert!(
            cache
                .peek_unit(&uri("Base.pas"), &ctx, 7, &HashMap::new())
                .is_some()
        );

        std::fs::write(&include, "{$DEFINE B}").unwrap();
        assert!(
            cache
                .peek_unit(&uri("Base.pas"), &ctx, 7, &HashMap::new())
                .is_none()
        );
    }

    #[test]
    fn peek_never_claims_or_waits() {
        let cache = ProjectCache::new(usize::MAX);
        let ctx = context("A.dproj");
        assert!(
            cache
                .peek_unit(&uri("Base.pas"), &ctx, 7, &HashMap::new())
                .is_none()
        );

        // The miss left no computing slot behind, so a normal lookup claims.
        let Lookup::Compute(claim) =
            cache.unit(&uri("Base.pas"), &ctx, 7, &HashMap::new(), &no_cancel())
        else {
            panic!("expected a claim");
        };
        // While the claim is held, peeking returns at once instead of waiting.
        let started = std::time::Instant::now();
        assert!(
            cache
                .peek_unit(&uri("Base.pas"), &ctx, 7, &HashMap::new())
                .is_none()
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        drop(claim);
    }

    #[test]
    fn interface_entries_report_whether_they_are_new_or_changed() {
        let cache = ProjectCache::new(usize::MAX);
        let ctx = context("A.dproj");
        let derived = uri("Derived.pas");

        assert!(cache.put_interface_imports(
            &derived,
            &ctx,
            3,
            interface_value(&[("Base.pas", 7)], true),
            None
        ));
        assert!(
            !cache.put_interface_imports(
                &derived,
                &ctx,
                3,
                interface_value(&[("Base.pas", 7)], true),
                None
            ),
            "an identical entry is not new"
        );
        assert!(
            cache.put_interface_imports(
                &derived,
                &ctx,
                3,
                interface_value(&[("Base.pas", 9)], true),
                None
            ),
            "a changed binding is new"
        );

        let entry = cache
            .peek_interface_imports(&derived, &ctx, 3, &HashMap::new())
            .expect("stored entry");
        assert_eq!(entry.bindings[0].uri, uri("Base.pas"));
        assert_eq!(entry.bindings[0].content_hash(), 9);
        assert!(
            cache
                .peek_interface_imports(&derived, &ctx, 4, &HashMap::new())
                .is_none()
        );
        assert_eq!(
            cache.stats().imports,
            0,
            "interface entries are a separate layer"
        );
    }

    #[test]
    fn interface_entries_from_before_an_invalidation_are_dropped() {
        let cache = ProjectCache::new(usize::MAX);
        let epoch = cache.invalidation_epoch();
        cache.invalidate_path(&ws_path("Other.pas"));
        assert!(!cache.put_interface_imports(
            &uri("Derived.pas"),
            &context("A.dproj"),
            3,
            interface_value(&[], true),
            Some(epoch),
        ));
        assert!(
            cache
                .peek_interface_imports(
                    &uri("Derived.pas"),
                    &context("A.dproj"),
                    3,
                    &HashMap::new()
                )
                .is_none()
        );
    }

    #[test]
    fn interface_entries_are_invalidated_with_their_dependency_probes() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("Base.pas");
        std::fs::write(&base, "unit Base;").unwrap();
        let cache = ProjectCache::new(usize::MAX);
        let ctx = context("A.dproj");
        let mut value = interface_value(&[], true);
        value.probes = vec![Probe::Content {
            path: base.clone(),
            stamp: pascal_project::path_stamp_result(&base).ok().flatten(),
            content_hash: pascal_project::content_hash_bytes(b"unit Base;"),
        }];
        cache.put_interface_imports(&uri("Derived.pas"), &ctx, 3, value, None);

        cache.invalidate_file_contents(&base);
        assert!(
            cache
                .peek_interface_imports(&uri("Derived.pas"), &ctx, 3, &HashMap::new())
                .is_none()
        );
    }

    #[test]
    fn import_accounting_includes_raw_and_decoded_source_and_metadata() {
        let raw = Arc::<[u8]>::from(vec![b'x'; 13]);
        let decoded = Arc::<str>::from("decoded source text");
        let resolved = pascal_core::ResolvedImports {
            bindings: vec![],
            dependencies: vec![pascal_core::ResolvedUnit {
                requested_name: "Provider".to_string(),
                declared_name: "Provider".to_string(),
                source: pascal_core::LoadedSource {
                    id: pascal_core::SourceId::new("Provider.pas"),
                    path: PathBuf::from("/ws/Provider.pas"),
                    bytes: raw.clone(),
                    decoded_text: Some(decoded.clone()),
                    compile_time_backing: None,
                    revision: pascal_core::SourceRevision::Overlay {
                        version: 1,
                        content_hash: 2,
                    },
                },
            }],
            complete: true,
            settled: true,
        };

        assert_eq!(
            import_value_bytes(&resolved),
            4096usize
                .saturating_add(raw.len())
                .saturating_add(decoded.len()),
            "the estimate includes both retained source payloads and entry metadata"
        );
    }

    fn fill(cache: &ProjectCache, name: &str, ctx: &ProjectContext, hash: u64, bytes: usize) {
        match cache.imports(&uri(name), ctx, hash, &HashMap::new(), &no_cancel()) {
            Lookup::Compute(claim) => {
                cache.store_imports(claim, import_value(vec![]), bytes, &no_cancel())
            }
            _ => panic!("expected a miss for {name}"),
        }
    }

    #[test]
    fn stored_entry_is_a_hit_for_same_context_and_hash() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        fill(&cache, "Main.pas", &ctx, 7, 10);
        assert!(matches!(
            cache.imports(&uri("Main.pas"), &ctx, 7, &HashMap::new(), &no_cancel()),
            Lookup::Hit(_)
        ));
    }

    #[test]
    fn different_hash_or_context_is_a_miss() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        fill(&cache, "Main.pas", &ctx, 7, 10);
        assert!(matches!(
            cache.imports(&uri("Main.pas"), &ctx, 8, &HashMap::new(), &no_cancel()),
            Lookup::Compute(_)
        ));
        assert!(matches!(
            cache.imports(
                &uri("Main.pas"),
                &context("B.dproj"),
                7,
                &HashMap::new(),
                &no_cancel()
            ),
            Lookup::Compute(_)
        ));
    }

    #[test]
    fn failed_probe_is_a_miss() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Gone.pas");
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let Lookup::Compute(claim) =
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!()
        };
        cache.store_imports(
            claim,
            import_value(vec![Probe::Stamp {
                path: path.clone(),
                expected: None,
            }]),
            1,
            &no_cancel(),
        );
        std::fs::write(&path, "x").unwrap();
        assert!(matches!(
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Compute(_)
        ));
    }

    #[test]
    fn dropped_claim_lets_the_next_caller_compute() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let Lookup::Compute(claim) =
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!()
        };
        drop(claim);
        assert!(matches!(
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Compute(_)
        ));
    }

    #[test]
    fn cancelled_owner_result_is_not_cached() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let Lookup::Compute(claim) =
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!()
        };
        let cancel = AtomicBool::new(false);
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        cache.store_imports(claim, import_value(vec![]), 1, &cancel);
        assert!(matches!(
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Compute(_)
        ));
    }

    #[test]
    fn poisoned_cache_discards_entries_and_recovers() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        fill(&cache, "Main.pas", &ctx, 1, 1);
        cache.poison_state();

        let Lookup::Compute(claim) =
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!("poisoned cache must miss")
        };
        cache.store_imports(claim, import_value(vec![]), 1, &no_cancel());
        assert!(matches!(
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Hit(_)
        ));
    }

    #[test]
    fn waiter_receives_the_in_flight_result() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let Lookup::Compute(claim) =
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!()
        };
        let waiter = {
            let cache = cache.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                matches!(
                    cache.imports(
                        &uri("Main.pas"),
                        &ctx,
                        1,
                        &HashMap::new(),
                        &AtomicBool::new(false)
                    ),
                    Lookup::Hit(_)
                )
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        cache.store_imports(claim, import_value(vec![]), 1, &no_cancel());
        assert!(
            waiter.join().unwrap(),
            "the waiter must reuse the in-flight result"
        );
    }

    #[test]
    fn waiter_takes_over_when_the_computation_is_abandoned() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let Lookup::Compute(claim) =
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!()
        };
        let waiter = {
            let cache = cache.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                matches!(
                    cache.imports(
                        &uri("Main.pas"),
                        &ctx,
                        1,
                        &HashMap::new(),
                        &AtomicBool::new(false)
                    ),
                    Lookup::Compute(_)
                )
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(claim);
        assert!(waiter.join().unwrap());
    }

    #[test]
    fn a_claim_for_other_input_is_not_waited_on() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let main = uri("Main.pas");
        let Lookup::Compute(older) = cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!("expected initial miss")
        };
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let newer = {
            let cache = cache.clone();
            let ctx = ctx.clone();
            let main = main.clone();
            std::thread::spawn(move || {
                let Lookup::Compute(claim) =
                    cache.imports(&main, &ctx, 2, &HashMap::new(), &no_cancel())
                else {
                    panic!("a newer revision computes")
                };
                cache.store_imports(claim, import_value(vec![]), 1, &no_cancel());
                result_tx.send(()).unwrap();
            })
        };
        let result = result_rx.recv_timeout(Duration::from_secs(5));
        drop(older);
        newer.join().unwrap();
        result.expect("a newer revision must not wait behind an older claim");
        assert!(
            matches!(
                cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel()),
                Lookup::Compute(_)
            ),
            "the uncached computation must not have stored anything"
        );
    }

    fn claimed_unit(cache: &ProjectCache, main: &Url, ctx: &ProjectContext) -> Option<u64> {
        match lock(&cache.inner).slots.get(&Key {
            layer: Layer::Unit,
            uri: main.clone(),
            fingerprint: project_context_fingerprint(ctx),
        }) {
            Some(Slot::Computing { claim, .. }) => Some(*claim),
            _ => None,
        }
    }

    fn main_unit_value() -> UnitValue {
        UnitValue {
            parsed: parsed_unit("Main.pas"),
            expansion: None,
            probes: vec![],
            disk: None,
        }
    }

    /// Looks the unit `main` up on a new thread with `role`, and
    /// reports the outcome: `Hit`, `Cancelled`, or the claim to compute.
    fn spawn_lookup(
        cache: &ProjectCache,
        main: &Url,
        ctx: &ProjectContext,
        role: ClaimRole,
        cancel: Arc<AtomicBool>,
    ) -> std::sync::mpsc::Receiver<Lookup<UnitValue>> {
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let cache = cache.clone();
        let ctx = ctx.clone();
        let main = main.clone();
        std::thread::spawn(move || {
            set_claim_role_on_this_thread(role);
            let lookup = cache.unit(&main, &ctx, 1, &HashMap::new(), &cancel);
            result_tx.send(lookup).unwrap();
        });
        result_rx
    }

    /// Claims the unit `main` on a new thread with `role`, then holds
    /// the claim until told to store (`true`) or drop (`false`) it.
    fn spawn_owner(
        cache: &ProjectCache,
        main: &Url,
        ctx: &ProjectContext,
        role: ClaimRole,
    ) -> (std::sync::mpsc::Sender<bool>, std::thread::JoinHandle<()>) {
        let (claimed_tx, claimed_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel::<bool>();
        let cache = cache.clone();
        let ctx = ctx.clone();
        let main = main.clone();
        let owner = std::thread::spawn(move || {
            set_claim_role_on_this_thread(role);
            let Lookup::Compute(claim) = cache.unit(&main, &ctx, 1, &HashMap::new(), &no_cancel())
            else {
                panic!("the owner claims the slot")
            };
            assert!(!claim.is_detached());
            claimed_tx.send(()).unwrap();
            if finish_rx.recv().unwrap() {
                cache.store_unit(claim, main_unit_value(), 1, &no_cancel());
            }
        });
        claimed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the owner claimed the slot");
        (finish_tx, owner)
    }

    #[test]
    fn interactive_lookup_waits_for_a_unit_claim_instead_of_parsing_it_again() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let main = uri("Main.pas");
        let owner_cancel = Arc::new(AtomicBool::new(false));
        let (finish, owner) = spawn_owner(&cache, &main, &ctx, ClaimRole::job(owner_cancel));
        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        cache.set_every_wait_hook(waiting_tx);
        let interactive = spawn_lookup(
            &cache,
            &main,
            &ctx,
            ClaimRole::interactive(Arc::new(AtomicBool::new(false)), Duration::ZERO),
            Arc::new(AtomicBool::new(false)),
        );
        // Each wait lasts one slice at most; outlast any bounded wait the
        // claim may be given before it is computed a second time.
        let slices = 2 + (Duration::from_secs(1).as_millis() / WAIT_SLICE.as_millis()) as usize;
        for _ in 0..slices {
            waiting_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the interactive lookup keeps waiting for the claim");
        }
        finish.send(true).unwrap();
        owner.join().unwrap();
        let lookup = interactive
            .recv_timeout(Duration::from_secs(5))
            .expect("the interactive lookup answers once the claim is stored");
        assert!(
            matches!(lookup, Lookup::Hit(_)),
            "one computation serves both: the waiter must adopt the stored claim"
        );
    }

    #[test]
    fn cancelled_interactive_waiter_returns_while_the_claim_is_held() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let main = uri("Main.pas");
        let (finish, owner) = spawn_owner(
            &cache,
            &main,
            &ctx,
            ClaimRole::job(Arc::new(AtomicBool::new(false))),
        );
        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        cache.set_waiter_hook(waiting_tx);
        let cancel = Arc::new(AtomicBool::new(false));
        let interactive = spawn_lookup(
            &cache,
            &main,
            &ctx,
            ClaimRole::interactive(cancel.clone(), Duration::ZERO),
            cancel.clone(),
        );
        waiting_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the interactive lookup waits for the claim");
        cancel.store(true, Ordering::Relaxed);
        cache.inner.changed.notify_all();
        let lookup = interactive.recv_timeout(Duration::from_secs(5));
        let still_claimed = claimed_unit(&cache, &main, &ctx).is_some();
        finish.send(true).unwrap();
        owner.join().unwrap();
        assert!(
            matches!(lookup, Ok(Lookup::Cancelled)),
            "a cancelled waiter must return without waiting out the claim"
        );
        assert!(still_claimed, "the owner still held the claim");
    }

    #[test]
    fn a_failed_owner_lets_the_waiter_compute() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let main = uri("Main.pas");
        let (finish, owner) = spawn_owner(
            &cache,
            &main,
            &ctx,
            ClaimRole::job(Arc::new(AtomicBool::new(false))),
        );
        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        cache.set_waiter_hook(waiting_tx);
        let interactive = spawn_lookup(
            &cache,
            &main,
            &ctx,
            ClaimRole::interactive(Arc::new(AtomicBool::new(false)), Duration::ZERO),
            Arc::new(AtomicBool::new(false)),
        );
        waiting_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the interactive lookup waits for the claim");
        finish.send(false).unwrap();
        owner.join().unwrap();
        let Ok(Lookup::Compute(claim)) = interactive.recv_timeout(Duration::from_secs(5)) else {
            panic!("the waiter computes once the owner drops its claim")
        };
        assert!(!claim.is_detached(), "the waiter claims the freed slot");
        cache.store_unit(claim, main_unit_value(), 1, &no_cancel());
        assert!(matches!(
            cache.unit(&main, &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Hit(_)
        ));
    }

    #[test]
    fn a_cancelled_owners_claim_is_taken_over() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let main = uri("Main.pas");
        let owner_cancel = Arc::new(AtomicBool::new(false));
        let (finish, owner) =
            spawn_owner(&cache, &main, &ctx, ClaimRole::job(owner_cancel.clone()));
        let owner_claim = claimed_unit(&cache, &main, &ctx).expect("owner claim");
        owner_cancel.store(true, Ordering::Relaxed);
        let interactive = spawn_lookup(
            &cache,
            &main,
            &ctx,
            ClaimRole::interactive(Arc::new(AtomicBool::new(false)), Duration::ZERO),
            Arc::new(AtomicBool::new(false)),
        );
        let Ok(Lookup::Compute(claim)) = interactive.recv_timeout(Duration::from_secs(5)) else {
            panic!("the waiter takes over a cancelled owner's claim")
        };
        assert!(!claim.is_detached(), "the take-over claims the slot");
        let taken = claimed_unit(&cache, &main, &ctx).expect("taken-over claim");
        assert_ne!(taken, owner_claim);
        // The cancelled owner finishing late neither stores nor frees the slot.
        finish.send(true).unwrap();
        owner.join().unwrap();
        assert_eq!(claimed_unit(&cache, &main, &ctx), Some(taken));
        cache.store_unit(claim, main_unit_value(), 1, &no_cancel());
        assert!(matches!(
            cache.unit(&main, &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Hit(_)
        ));
    }

    #[test]
    fn interactive_lookup_takes_over_a_yielding_claim() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let main = uri("Main.pas");
        let (finish, owner) = spawn_owner(
            &cache,
            &main,
            &ctx,
            ClaimRole::yielding(Arc::new(AtomicBool::new(false))),
        );
        let interactive = spawn_lookup(
            &cache,
            &main,
            &ctx,
            ClaimRole::interactive(Arc::new(AtomicBool::new(false)), Duration::ZERO),
            Arc::new(AtomicBool::new(false)),
        );
        let Ok(Lookup::Compute(claim)) = interactive.recv_timeout(Duration::from_secs(5)) else {
            panic!("interactive work takes the warmer's claim over")
        };
        assert!(!claim.is_detached());
        cache.store_unit(claim, main_unit_value(), 1, &no_cancel());
        finish.send(true).unwrap();
        owner.join().unwrap();
        assert!(matches!(
            cache.unit(&main, &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Hit(_)
        ));
    }

    #[test]
    fn interactive_unit_waits_on_a_roleless_owner_are_bounded() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let main = uri("Main.pas");
        let (finish, owner) = spawn_owner(&cache, &main, &ctx, ClaimRole::default());
        let interactive = spawn_lookup(
            &cache,
            &main,
            &ctx,
            ClaimRole::interactive(Arc::new(AtomicBool::new(false)), Duration::from_millis(50)),
            Arc::new(AtomicBool::new(false)),
        );
        let lookup = interactive.recv_timeout(Duration::from_secs(5));
        finish.send(true).unwrap();
        owner.join().unwrap();
        let Ok(Lookup::Compute(claim)) = lookup else {
            panic!("a wait on a roleless owner gives up")
        };
        assert!(claim.is_detached());
    }

    #[test]
    fn interactive_lookup_waits_for_a_jobs_import_claim() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let main = uri("Main.pas");
        let (claimed_tx, claimed_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel::<()>();
        let owner = {
            let cache = cache.clone();
            let ctx = ctx.clone();
            let main = main.clone();
            std::thread::spawn(move || {
                set_claim_role_on_this_thread(ClaimRole::job(Arc::new(AtomicBool::new(false))));
                let Lookup::Compute(claim) =
                    cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel())
                else {
                    panic!("the owner claims the slot")
                };
                assert_eq!(claims_held_on_this_thread(), 1);
                claimed_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                cache.store_imports(claim, import_value(vec![]), 1, &no_cancel());
                assert_eq!(claims_held_on_this_thread(), 0);
            })
        };
        claimed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the owner claimed the slot");
        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        cache.set_every_wait_hook(waiting_tx);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let interactive = {
            let cache = cache.clone();
            let ctx = ctx.clone();
            let main = main.clone();
            std::thread::spawn(move || {
                set_claim_role_on_this_thread(ClaimRole::interactive(
                    Arc::new(AtomicBool::new(false)),
                    Duration::ZERO,
                ));
                let hit = matches!(
                    cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel()),
                    Lookup::Hit(_)
                );
                result_tx.send(hit).unwrap();
            })
        };
        // An import claim covers one resolution, like a unit claim one
        // parse: the interactive lookup keeps waiting for it.
        let slices = 2 + (Duration::from_secs(1).as_millis() / WAIT_SLICE.as_millis()) as usize;
        for _ in 0..slices {
            waiting_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the interactive lookup keeps waiting for the claim");
        }
        finish_tx.send(()).unwrap();
        owner.join().unwrap();
        assert!(
            result_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the interactive lookup answers once the claim is stored"),
            "the waiter adopts the stored resolution"
        );
        interactive.join().unwrap();
    }

    #[test]
    fn interactive_import_waits_are_bounded_and_leave_the_claim() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let main = uri("Main.pas");
        let Lookup::Compute(claim) = cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!("expected initial miss")
        };
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let interactive = {
            let cache = cache.clone();
            let ctx = ctx.clone();
            let main = main.clone();
            std::thread::spawn(move || {
                set_claim_role_on_this_thread(ClaimRole::interactive(
                    Arc::new(AtomicBool::new(false)),
                    Duration::from_millis(50),
                ));
                let Lookup::Compute(local) =
                    cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel())
                else {
                    panic!("a bounded import wait computes locally")
                };
                assert!(local.is_detached());
                cache.store_imports(local, import_value(vec![]), 1, &no_cancel());
                result_tx.send(()).unwrap();
            })
        };
        let result = result_rx.recv_timeout(Duration::from_secs(5));
        let still_claimed = matches!(
            lock(&cache.inner).slots.get(&Key {
                layer: Layer::Import,
                uri: main.clone(),
                fingerprint: project_context_fingerprint(&ctx),
            }),
            Some(Slot::Computing { .. })
        );
        cache.store_imports(claim, import_value(vec![]), 1, &no_cancel());
        interactive.join().unwrap();
        result.expect("a bounded import wait must give up on the claim");
        assert!(still_claimed, "the local computation must leave the claim");
        assert!(matches!(
            cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Hit(_)
        ));
    }

    #[test]
    fn a_wait_that_would_close_a_cycle_computes_detached() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let first = uri("First.pas");
        let second = uri("Second.pas");
        // This thread claims First; another claims Second and then waits
        // for First.
        let Lookup::Compute(first_claim) =
            cache.imports(&first, &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!("expected initial miss")
        };
        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        cache.set_waiter_hook(waiting_tx);
        let (claimed_tx, claimed_rx) = std::sync::mpsc::channel();
        let other = {
            let cache = cache.clone();
            let ctx = ctx.clone();
            let first = first.clone();
            let second = second.clone();
            std::thread::spawn(move || {
                let Lookup::Compute(second_claim) =
                    cache.imports(&second, &ctx, 1, &HashMap::new(), &no_cancel())
                else {
                    panic!("the other thread claims Second")
                };
                claimed_tx.send(()).unwrap();
                let first_lookup = cache.imports(&first, &ctx, 1, &HashMap::new(), &no_cancel());
                cache.store_imports(second_claim, import_value(vec![]), 1, &no_cancel());
                matches!(first_lookup, Lookup::Hit(_))
            })
        };
        claimed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the other thread claimed Second");
        waiting_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the other thread waits for First");
        let Lookup::Compute(detached) =
            cache.imports(&second, &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!("waiting for Second would close a cycle")
        };
        assert!(
            detached.is_detached(),
            "a cycle must not take the claim over"
        );
        drop(detached);
        cache.store_imports(first_claim, import_value(vec![]), 1, &no_cancel());
        assert!(other.join().unwrap(), "the other thread adopts First");
    }

    #[test]
    fn waiter_recovers_poisoned_state_when_condvar_reacquires() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let main = uri("Main.pas");
        let Lookup::Compute(claim) = cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!("expected initial miss")
        };

        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        cache.set_waiter_hook(waiting_tx);
        let waiter = {
            let cache = cache.clone();
            let ctx = ctx.clone();
            let main = main.clone();
            std::thread::spawn(move || {
                let was_compute =
                    match cache.imports(&main, &ctx, 1, &HashMap::new(), &AtomicBool::new(false)) {
                        Lookup::Compute(claim) => {
                            cache.store_imports(claim, import_value(vec![]), 1, &no_cancel());
                            true
                        }
                        Lookup::Hit(_) | Lookup::Cancelled => false,
                    };
                result_tx.send(was_compute).unwrap();
            })
        };

        waiting_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("waiter reached the Computing slot");
        cache.poison_state();
        cache.inner.changed.notify_all();
        let result = result_rx.recv_timeout(Duration::from_secs(2));
        drop(claim);
        let waiter_joined = waiter.join().is_ok();
        assert!(
            matches!(result, Ok(true)) && waiter_joined,
            "the woken waiter must recover poison and compute a miss"
        );
        assert!(matches!(
            cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Hit(_)
        ));
    }

    #[test]
    fn cancelled_waiter_returns_cancelled() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let Lookup::Compute(_claim) =
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!()
        };
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &cancelled),
            Lookup::Cancelled
        ));
    }

    #[test]
    fn lru_eviction_respects_budget_and_pins() {
        let cache = ProjectCache::new(25);
        let ctx = context("A.dproj");
        fill(&cache, "A.pas", &ctx, 1, 10);
        fill(&cache, "B.pas", &ctx, 1, 10);
        cache.pin(
            &uri("Open.pas"),
            vec![(uri("A.pas"), project_context_fingerprint(&ctx))],
        );
        fill(&cache, "C.pas", &ctx, 1, 10);
        assert!(
            matches!(
                cache.imports(&uri("A.pas"), &ctx, 1, &HashMap::new(), &no_cancel()),
                Lookup::Hit(_)
            ),
            "pinned entry survives"
        );
        assert!(
            matches!(
                cache.imports(&uri("B.pas"), &ctx, 1, &HashMap::new(), &no_cancel()),
                Lookup::Compute(_)
            ),
            "least recently used unpinned entry is evicted"
        );
        assert!(cache.stats().bytes <= 25);
    }

    fn ready(cache: &ProjectCache, name: &str) -> bool {
        !cache.ready_layers(&uri(name)).is_empty()
    }

    fn pin_names(cache: &ProjectCache, owner: &str, names: &[&str], ctx: &ProjectContext) {
        cache.pin(
            &uri(owner),
            names
                .iter()
                .map(|name| (uri(name), project_context_fingerprint(ctx)))
                .collect(),
        );
    }

    #[test]
    fn eviction_follows_last_use_across_hits_and_peeks() {
        let cache = ProjectCache::new(30);
        let ctx = context("A.dproj");
        fill(&cache, "A.pas", &ctx, 1, 10);
        fill(&cache, "B.pas", &ctx, 1, 10);
        fill_unit(&cache, "C.pas", &ctx, 1, vec![]);
        fill(&cache, "D.pas", &ctx, 1, 9);
        assert!(matches!(
            cache.imports(&uri("A.pas"), &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Hit(_)
        ));
        assert!(
            cache
                .peek_unit(&uri("C.pas"), &ctx, 1, &HashMap::new())
                .is_some()
        );

        fill(&cache, "E.pas", &ctx, 1, 11);
        assert!(!ready(&cache, "B.pas"), "B is the least recently used");
        assert!(!ready(&cache, "D.pas"), "D is next once B is gone");
        for name in ["A.pas", "C.pas", "E.pas"] {
            assert!(ready(&cache, name), "{name} was used more recently");
        }
        assert_eq!(cache.stats().bytes, 22);
    }

    #[test]
    fn a_pin_protects_every_layer_of_its_unit_until_the_last_owner_unpins() {
        let cache = ProjectCache::new(20);
        let ctx = context("A.dproj");
        // Pinned once ready: an entry stored after its pin is held to the
        // pin cap (half the budget), which A's two layers exceed.
        fill(&cache, "A.pas", &ctx, 1, 10);
        fill_unit(&cache, "A.pas", &ctx, 1, vec![]);
        pin_names(&cache, "Open1.pas", &["A.pas"], &ctx);
        pin_names(&cache, "Open2.pas", &["A.pas"], &ctx);
        fill(&cache, "B.pas", &ctx, 1, 9);

        cache.unpin(&uri("Open1.pas"));
        fill(&cache, "C.pas", &ctx, 1, 9);
        assert!(!ready(&cache, "B.pas"));
        assert_eq!(cache.ready_layers(&uri("A.pas")), vec!["Import", "Unit"]);

        cache.unpin(&uri("Open2.pas"));
        assert!(
            ready(&cache, "A.pas"),
            "unpinning alone stays within budget"
        );
        fill(&cache, "D.pas", &ctx, 1, 9);
        assert_eq!(
            cache.ready_layers(&uri("A.pas")),
            vec!["Unit"],
            "A's import is now the oldest unpinned entry"
        );
        assert!(ready(&cache, "C.pas") && ready(&cache, "D.pas"));
    }

    #[test]
    fn repinning_an_owner_replaces_its_previous_pins() {
        let cache = ProjectCache::new(20);
        let ctx = context("A.dproj");
        pin_names(&cache, "Open.pas", &["A.pas"], &ctx);
        fill(&cache, "A.pas", &ctx, 1, 10);
        fill(&cache, "B.pas", &ctx, 1, 10);
        pin_names(&cache, "Open.pas", &["B.pas"], &ctx);
        fill(&cache, "C.pas", &ctx, 1, 10);
        assert!(!ready(&cache, "A.pas"));
        assert!(ready(&cache, "B.pas") && ready(&cache, "C.pas"));
    }

    #[test]
    fn pins_only_cover_their_own_project_fingerprint() {
        let cache = ProjectCache::new(10);
        let a = context("A.dproj");
        let b = context("B.dproj");
        pin_names(&cache, "Open.pas", &["Main.pas"], &b);
        fill(&cache, "Main.pas", &a, 1, 10);
        fill(&cache, "Other.pas", &a, 1, 10);
        assert!(!ready(&cache, "Main.pas"));
        assert!(ready(&cache, "Other.pas"));
    }

    #[test]
    fn pinning_a_ready_entry_counts_it_as_pinned_until_unpinned() {
        let cache = ProjectCache::new(10);
        let ctx = context("A.dproj");
        fill(&cache, "A.pas", &ctx, 1, 10);
        assert!(cache.has_room());
        pin_names(&cache, "Open.pas", &["A.pas"], &ctx);
        assert!(!cache.has_room());
        cache.clear_pins();
        assert!(cache.has_room());
        pin_names(&cache, "Open.pas", &["A.pas"], &ctx);
        cache.invalidate_path(&ws_path("A.pas"));
        assert!(cache.has_room(), "an evicted pinned entry no longer counts");
    }

    #[test]
    fn invalidating_one_path_among_ten_thousand_entries_examines_one() {
        let cache = ProjectCache::new(usize::MAX);
        let ctx = context("A.dproj");
        for index in 0..10_000 {
            let name = format!("Main{index}.pas");
            let Lookup::Compute(claim) =
                cache.imports(&uri(&name), &ctx, 1, &HashMap::new(), &no_cancel())
            else {
                panic!("expected a miss for {name}");
            };
            let probe = Probe::Content {
                path: PathBuf::from(format!("/deps/Dep{index}.pas")),
                stamp: None,
                content_hash: 0,
            };
            cache.store_imports(claim, import_value(vec![probe]), 1, &no_cancel());
        }
        cache.reset_examined_entries();
        assert_eq!(
            cache.invalidate_path(Path::new("/deps/Dep5000.pas")),
            vec![uri("Main5000.pas")]
        );
        assert_eq!(cache.examined_entries(), 1);
        assert_eq!(cache.stats().imports, 9_999);
    }

    #[test]
    fn evicting_one_entry_among_ten_thousand_examines_one() {
        let cache = ProjectCache::new(10_000);
        let ctx = context("A.dproj");
        for index in 0..10_000 {
            fill(&cache, &format!("Main{index}.pas"), &ctx, 1, 1);
        }
        cache.reset_examined_entries();
        fill(&cache, "Last.pas", &ctx, 1, 1);
        assert!(!ready(&cache, "Main0.pas"));
        assert_eq!(cache.examined_entries(), 1);
    }

    type WatchCalls = Arc<Mutex<Vec<(&'static str, PathBuf, bool)>>>;

    /// Records each watcher call and whether the cache state was unlocked.
    struct RecordingWatch {
        inner: Arc<Inner>,
        calls: WatchCalls,
    }

    impl DirectoryWatch for RecordingWatch {
        fn watch(&mut self, directory: &Path) -> bool {
            let unlocked = self.inner.state.try_lock().is_ok();
            let call = ("watch", directory.to_path_buf(), unlocked);
            self.calls.lock().unwrap().push(call);
            true
        }

        fn unwatch(&mut self, directory: &Path) {
            let unlocked = self.inner.state.try_lock().is_ok();
            let call = ("unwatch", directory.to_path_buf(), unlocked);
            self.calls.lock().unwrap().push(call);
        }
    }

    fn recording_watcher(cache: &ProjectCache) -> WatchCalls {
        let calls = Arc::new(Mutex::new(Vec::new()));
        cache.set_watcher(Box::new(RecordingWatch {
            inner: cache.inner.clone(),
            calls: calls.clone(),
        }));
        calls
    }

    fn store_watching(cache: &ProjectCache, name: &str, ctx: &ProjectContext, dir: &Path) {
        let Lookup::Compute(claim) =
            cache.imports(&uri(name), ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!("expected a miss for {name}");
        };
        let mut value = import_value(vec![]);
        value.watch_dirs.push(dir.to_path_buf());
        cache.store_imports(claim, value, 1, &no_cancel());
    }

    #[test]
    fn watcher_calls_run_without_the_cache_lock() {
        let cache = ProjectCache::new(1 << 20);
        let calls = recording_watcher(&cache);
        let ctx = context("A.dproj");
        let dir = PathBuf::from("/ws/includes");
        store_watching(&cache, "Main.pas", &ctx, &dir);
        store_watching(&cache, "Other.pas", &ctx, &dir);
        cache.invalidate_path(&ws_path("Main.pas"));
        cache.invalidate_path(&ws_path("Other.pas"));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("watch", dir.clone(), true), ("unwatch", dir, true)],
            "one registration per directory, made with the cache unlocked"
        );
    }

    #[test]
    fn poison_recovery_releases_watches_without_the_cache_lock() {
        let cache = ProjectCache::new(1 << 20);
        let calls = recording_watcher(&cache);
        let ctx = context("A.dproj");
        let dir = PathBuf::from("/ws/includes");
        store_watching(&cache, "Main.pas", &ctx, &dir);
        cache.poison_state();
        fill(&cache, "Other.pas", &ctx, 1, 1);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("watch", dir.clone(), true), ("unwatch", dir, true)]
        );
    }

    #[test]
    fn a_published_catalogue_watches_its_directories_with_the_cache_unlocked() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("sub")).unwrap();
        let cache = ProjectCache::new(1 << 30);
        let calls = recording_watcher(&cache);
        let (key, catalogue) = super::listed_catalogue(temp.path());
        cache
            .publish_catalogue(key.clone(), None, catalogue)
            .unwrap();
        let root = temp.path().to_path_buf();
        let sub = root.join("sub");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("watch", root.clone(), true), ("watch", sub.clone(), true)]
        );
        let shared = cache.catalogue(&key).unwrap();
        assert!(shared.watch_registered);
        assert!(!shared.watched, "watched only after a later stamp check");
        cache.confirm_catalogue_watched(&key, shared.generation);
        assert!(cache.catalogue(&key).unwrap().watched);
        cache.drop_catalogue(&key, shared.generation);
        assert_eq!(
            calls.lock().unwrap()[2..],
            [("unwatch", root, true), ("unwatch", sub, true)]
        );
    }

    #[test]
    fn a_catalogue_beyond_the_watch_budget_is_not_watched() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("sub")).unwrap();
        let cache = ProjectCache::new(1 << 30);
        let calls = recording_watcher(&cache);
        cache.set_catalogue_watch_budget_for_test(1);
        let (key, catalogue) = super::listed_catalogue(temp.path());
        cache
            .publish_catalogue(key.clone(), None, catalogue)
            .unwrap();
        assert!(calls.lock().unwrap().is_empty());
        assert!(!cache.catalogue(&key).unwrap().watch_registered);
    }

    #[test]
    fn replacing_the_watcher_forgets_every_watched_catalogue() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ProjectCache::new(1 << 30);
        recording_watcher(&cache);
        let (key, catalogue) = super::listed_catalogue(temp.path());
        let generation = cache
            .publish_catalogue(key.clone(), None, catalogue)
            .unwrap();
        cache.confirm_catalogue_watched(&key, generation);
        assert!(cache.catalogue(&key).unwrap().watched);
        recording_watcher(&cache);
        let shared = cache.catalogue(&key).unwrap();
        assert!(!shared.watched && !shared.watch_registered);
    }

    #[test]
    fn poison_recovery_forgets_the_catalogue_watches_it_drained() {
        let temp = tempfile::tempdir().unwrap();
        let shared_directory = temp.path().join("sub");
        std::fs::create_dir_all(&shared_directory).unwrap();
        let cache = ProjectCache::new(1 << 30);
        let calls = recording_watcher(&cache);
        cache.set_catalogue_watch_budget_for_test(2);
        let (old_key, old) = super::listed_catalogue(temp.path());
        let old_generation = cache.publish_catalogue(old_key.clone(), None, old).unwrap();
        cache.poison_state();
        let (new_key, new) = super::listed_catalogue(&shared_directory);
        let new_generation = cache.publish_catalogue(new_key.clone(), None, new).unwrap();
        assert!(
            cache.catalogue(&new_key).unwrap().watch_registered,
            "drained watches no longer count against the budget"
        );
        cache.confirm_catalogue_watched(&new_key, new_generation);
        assert!(cache.catalogue(&new_key).unwrap().watched);
        let before_drop = calls.lock().unwrap().len();
        cache.drop_catalogue(&old_key, old_generation);
        assert_eq!(
            calls.lock().unwrap()[before_drop..],
            [],
            "the older catalogue's release must not unwatch what the newer one holds"
        );
        cache.drop_catalogue(&new_key, new_generation);
        assert_eq!(
            calls.lock().unwrap()[before_drop..],
            [("unwatch", shared_directory, true)]
        );
    }

    #[test]
    fn a_failed_watch_leaves_the_catalogue_unregistered() {
        struct Refuse;
        impl DirectoryWatch for Refuse {
            fn watch(&mut self, _: &Path) -> bool {
                false
            }
            fn unwatch(&mut self, _: &Path) {}
        }
        let temp = tempfile::tempdir().unwrap();
        let cache = ProjectCache::new(1 << 30);
        cache.set_watcher(Box::new(Refuse));
        let (key, catalogue) = super::listed_catalogue(temp.path());
        let generation = cache
            .publish_catalogue(key.clone(), None, catalogue)
            .unwrap();
        cache.confirm_catalogue_watched(&key, generation);
        let shared = cache.catalogue(&key).unwrap();
        assert!(!shared.watch_registered && !shared.watched);
    }

    fn long_probe(name: &str) -> Probe {
        Probe::Stamp {
            path: PathBuf::from(format!("/ws/{}/{name}", "deep".repeat(64))),
            expected: None,
        }
    }

    #[test]
    /// `retained_bytes` takes the parse at the measured factor; that factor
    /// is calibrated by RSS in `measure_retained_bytes_per_source_byte`, since
    /// tree-sitter's trees live in C allocations no in-process count sees.
    /// This checks only what the estimate adds on top of it.
    fn unit_retained_bytes_add_probes_and_unshared_text_to_the_parse_estimate() {
        let parsed = parsed_unit("Base.pas");
        let source_len = parsed.source_text().len();
        let unit = |probes, disk| UnitValue {
            parsed: parsed.clone(),
            expansion: None,
            probes,
            disk,
        };
        let bare = unit(vec![], None).retained_bytes();
        assert_eq!(
            bare,
            unit_value_bytes(source_len) + std::mem::size_of::<UnitValue>()
        );
        assert!(unit(vec![long_probe("A.inc")], None).retained_bytes() >= bare + 256);

        let origin = |text| DiskOrigin {
            stamp: crate::workspace::DiskStamp::from_project(&pascal_project::ProjectReadStamp {
                bytes: 1,
                modified: Some(std::time::SystemTime::UNIX_EPOCH),
                is_dir: false,
                is_symlink: false,
                change: None,
                settled: true,
            }),
            raw_bytes: 1,
            text,
        };
        let shared = unit(vec![], Some(origin(parsed.source_text().clone())));
        assert_eq!(
            shared.retained_bytes(),
            bare,
            "the parse's own text is free"
        );
        let copied = unit(vec![], Some(origin(Arc::from("x".repeat(1000)))));
        assert!(copied.retained_bytes() >= bare + 1000);
    }

    #[test]
    fn import_retained_bytes_cover_report_probes_and_watch_dirs() {
        let bare = import_value(vec![]);
        let base = bare.retained_bytes();
        assert!(base >= import_value_bytes(&bare.resolved));

        let probed = import_value(vec![long_probe("A.pas")]);
        assert!(probed.retained_bytes() >= base + 256);

        let mut watching = import_value(vec![]);
        watching.watch_dirs = vec![PathBuf::from("/ws/".to_string() + &"d".repeat(300))];
        assert!(watching.retained_bytes() >= base + 300);

        let mut reported = import_value(vec![]);
        reported.report = Arc::new(pascal_core::ResolutionReport {
            observations: vec![pascal_core::ResolutionObservation::Metadata(
                pascal_project::MetadataObservation::Stat {
                    path: PathBuf::from("/ws/".to_string() + &"m".repeat(300)),
                },
            )],
            warnings: vec!["w".repeat(300)],
            complete: true,
            incomplete_reasons: vec![],
        });
        assert!(reported.retained_bytes() >= base + 600);
    }

    #[test]
    fn interface_retained_bytes_cover_bindings_and_probes() {
        let bare = interface_value(&[("Base.pas", 1)], true);
        let base = bare.retained_bytes();
        assert!(base >= interface_value_bytes(&bare));
        let mut probed = interface_value(&[("Base.pas", 1)], true);
        probed.probes = vec![long_probe("Base.pas")];
        assert!(probed.retained_bytes() >= base + 256);
    }

    #[test]
    fn interface_entries_charge_their_probes() {
        let cache = ProjectCache::new(usize::MAX);
        let ctx = context("A.dproj");
        cache.put_interface_imports(&uri("Bare.pas"), &ctx, 1, interface_value(&[], true), None);
        let bare = cache.stats().bytes;
        let mut probed = interface_value(&[], true);
        probed.probes = vec![long_probe("A.pas"), long_probe("B.pas")];
        cache.put_interface_imports(&uri("Probed.pas"), &ctx, 1, probed, None);
        assert!(
            cache.stats().bytes - bare >= bare + 2 * 256,
            "probe paths are retained with the entry"
        );
    }

    #[test]
    fn entries_of_one_project_share_its_context() {
        let cache = ProjectCache::new(usize::MAX);
        fill(&cache, "A.pas", &context("A.dproj"), 1, 1);
        fill(&cache, "B.pas", &context("A.dproj"), 1, 1);
        fill_unit(&cache, "C.pas", &context("A.dproj"), 1, vec![]);
        let state = lock(&cache.inner);
        let contexts = state
            .slots
            .values()
            .filter_map(|slot| match slot {
                Slot::Ready(entry) => Some(entry.context.clone()),
                Slot::Computing { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(contexts.len(), 3);
        assert!(
            contexts
                .iter()
                .all(|context| Arc::ptr_eq(context, &contexts[0])),
            "one context copy per project, not per entry"
        );
    }

    fn store_unit_bytes(cache: &ProjectCache, name: &str, ctx: &ProjectContext, bytes: usize) {
        let Lookup::Compute(claim) = cache.unit(&uri(name), ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!("expected a miss for {name}");
        };
        let value = UnitValue {
            parsed: parsed_unit(name),
            expansion: None,
            probes: vec![],
            disk: None,
        };
        cache.store_unit(claim, value, bytes, &no_cancel());
    }

    #[test]
    fn a_leased_parse_stays_charged_after_eviction_until_the_lease_drops() {
        let cache = ProjectCache::new(10);
        let ctx = context("A.dproj");
        store_unit_bytes(&cache, "A.pas", &ctx, 10);
        let unit = cache
            .peek_unit(&uri("A.pas"), &ctx, 1, &HashMap::new())
            .expect("hit");
        let lease = cache.lease_unit(&unit);
        // What the request's index keeps.
        let kept = unit.parsed.clone();
        drop(unit);
        assert_eq!(
            cache.stats().outstanding_bytes,
            0,
            "a cached parse is charged once"
        );

        fill(&cache, "B.pas", &ctx, 1, 10);
        assert!(!ready(&cache, "A.pas"));
        assert!(
            !ready(&cache, "B.pas"),
            "the leased parse of A still fills the budget"
        );
        assert_eq!(cache.stats().outstanding_bytes, 10);
        assert!(!cache.has_room());

        drop(lease);
        assert_eq!(cache.stats().outstanding_bytes, 0);
        assert!(cache.has_room());
        fill(&cache, "B.pas", &ctx, 1, 10);
        assert!(ready(&cache, "B.pas"));
        drop(kept);
    }

    #[test]
    fn a_leased_parse_nothing_keeps_is_not_charged() {
        let cache = ProjectCache::new(usize::MAX);
        let ctx = context("A.dproj");
        store_unit_bytes(&cache, "A.pas", &ctx, 10);
        let unit = cache
            .peek_unit(&uri("A.pas"), &ctx, 1, &HashMap::new())
            .expect("hit");
        let lease = cache.lease_unit(&unit);
        let weak = Arc::downgrade(&unit.parsed);
        drop(unit);
        cache.invalidate_path(&ws_path("A.pas"));
        assert!(
            weak.upgrade().is_none(),
            "the lease does not keep the parse"
        );
        assert_eq!(cache.stats().outstanding_bytes, 0);
        drop(lease);
    }

    #[test]
    fn leasing_a_parse_evicted_after_its_hit_keeps_the_budget() {
        let cache = ProjectCache::new(10_000);
        let ctx = context("A.dproj");
        store_unit_bytes(&cache, "A.pas", &ctx, 10);
        let unit = cache
            .peek_unit(&uri("A.pas"), &ctx, 1, &HashMap::new())
            .expect("hit");
        cache.invalidate_path(&ws_path("A.pas"));
        fill(&cache, "B.pas", &ctx, 1, 10_000 - 10);

        let lease = cache.lease_unit(&unit);
        let stats = cache.stats();
        assert_eq!(stats.outstanding_bytes, unit.retained_bytes());
        assert!(!ready(&cache, "B.pas"), "the late charge evicts to budget");
        assert!(stats.bytes + stats.outstanding_bytes <= 10_000);
        drop(lease);
    }

    #[test]
    fn transient_hits_are_not_charged_after_eviction() {
        let cache = ProjectCache::new(usize::MAX);
        let ctx = context("A.dproj");
        store_unit_bytes(&cache, "A.pas", &ctx, 10);
        let unit = cache
            .peek_unit(&uri("A.pas"), &ctx, 1, &HashMap::new())
            .expect("hit");
        cache.invalidate_path(&ws_path("A.pas"));
        assert_eq!(cache.stats().outstanding_bytes, 0);
        drop(unit);
    }

    #[test]
    fn a_re_cached_parse_is_charged_by_its_entry_again() {
        let cache = ProjectCache::new(usize::MAX);
        let ctx = context("A.dproj");
        store_unit_bytes(&cache, "A.pas", &ctx, 10);
        let unit = cache
            .peek_unit(&uri("A.pas"), &ctx, 1, &HashMap::new())
            .expect("hit");
        let lease = cache.lease_unit(&unit);
        cache.invalidate_path(&ws_path("A.pas"));
        assert_eq!(cache.stats().outstanding_bytes, 10);

        let Lookup::Compute(claim) =
            cache.unit(&uri("A.pas"), &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!("expected a miss");
        };
        let value = UnitValue {
            parsed: unit.parsed.clone(),
            expansion: None,
            probes: vec![],
            disk: None,
        };
        cache.store_unit(claim, value, 10, &no_cancel());
        assert_eq!(cache.stats().outstanding_bytes, 0);
        drop(lease);
        assert_eq!(cache.stats().bytes, 10);
    }

    #[test]
    fn unpinned_entries_at_budget_still_allow_room_for_warming() {
        let cache = ProjectCache::new(10);
        let ctx = context("A.dproj");
        fill(&cache, "A.pas", &ctx, 1, 10);

        assert_eq!(cache.stats().bytes, 10);
        assert!(cache.has_room());
        assert!(!cache.stats().pinned_over_budget);
    }

    #[test]
    fn pinned_entries_at_budget_report_no_room() {
        let cache = ProjectCache::new(10);
        let ctx = context("A.dproj");
        fill(&cache, "A.pas", &ctx, 1, 10);
        cache.pin(
            &uri("Open.pas"),
            vec![(uri("A.pas"), project_context_fingerprint(&ctx))],
        );

        assert_eq!(cache.stats().bytes, 10);
        assert!(!cache.has_room());
        assert!(!cache.stats().pinned_over_budget);
    }

    #[test]
    fn capped_pins_stay_within_half_the_budget_unpinning_the_oldest_owner_first() {
        let cache = ProjectCache::new(40);
        let ctx = context("A.dproj");
        let fingerprint = project_context_fingerprint(&ctx);
        for name in ["A.pas", "B.pas", "C.pas", "D.pas", "E.pas"] {
            fill(&cache, name, &ctx, 1, 8);
        }
        let keys = |names: &[&str]| {
            names
                .iter()
                .map(|name| (uri(name), fingerprint))
                .collect::<Vec<_>>()
        };
        assert!(
            cache
                .pin_capped(&uri("Old.pas"), keys(&["A.pas", "B.pas"]), &[])
                .is_empty()
        );
        assert_eq!(cache.stats().pinned_bytes, 16);

        // 16 more would pass the cap of 20: the oldest owner goes first.
        let unpinned = cache.pin_capped(
            &uri("New.pas"),
            keys(&["C.pas", "D.pas"]),
            &[uri("Old.pas"), uri("New.pas")],
        );
        assert_eq!(unpinned, vec![uri("Old.pas")]);
        assert_eq!(cache.stats().pinned_bytes, 16);

        // A closure over the cap on its own keeps what fits, in order.
        let unpinned = cache.pin_capped(
            &uri("Big.pas"),
            keys(&["A.pas", "B.pas", "C.pas", "E.pas"]),
            &[uri("New.pas"), uri("Big.pas")],
        );
        assert_eq!(unpinned, vec![uri("New.pas")]);
        assert_eq!(cache.stats().pinned_bytes, 16);
        assert!(cache.stats().pinned_bytes <= cache.max_bytes() / 2);
        assert!(cache.has_room());
        fill(&cache, "F.pas", &ctx, 1, 24);
        assert!(ready(&cache, "A.pas") && ready(&cache, "B.pas"), "pinned");
        assert!(
            !ready(&cache, "C.pas"),
            "unpinned with New.pas, then evicted"
        );
    }

    #[test]
    fn pinned_entries_stored_after_their_pin_stay_within_the_cap() {
        let cache = ProjectCache::new(40);
        let ctx = context("A.dproj");
        let fingerprint = project_context_fingerprint(&ctx);
        let names = ["A.pas", "B.pas", "C.pas", "D.pas"];
        // Nothing is stored yet: every key is admitted at no cost.
        assert!(
            cache
                .pin_capped(
                    &uri("Open.pas"),
                    names.iter().map(|name| (uri(name), fingerprint)).collect(),
                    &[],
                )
                .is_empty()
        );
        for name in names {
            fill(&cache, name, &ctx, 1, 8);
            assert!(
                cache.stats().pinned_bytes <= cache.max_bytes() / 2,
                "after {name}: {:?}",
                cache.stats()
            );
        }
        assert_eq!(
            cache.stats().pinned_bytes,
            16,
            "A and B fit, C and D do not"
        );
        fill(&cache, "E.pas", &ctx, 1, 16);
        assert!(
            ready(&cache, "A.pas") && ready(&cache, "B.pas"),
            "still pinned"
        );
        assert!(
            !ready(&cache, "C.pas"),
            "C was left unpinned and evicted first"
        );
    }

    #[test]
    fn pinned_entries_over_budget_report_no_room() {
        let cache = ProjectCache::new(10);
        let ctx = context("A.dproj");
        fill(&cache, "A.pas", &ctx, 1, 10);
        cache.pin(
            &uri("Open.pas"),
            vec![(uri("A.pas"), project_context_fingerprint(&ctx))],
        );
        // A smaller budget cannot evict the pinned entry.
        cache.set_max_bytes(5);
        assert!(!cache.has_room());
        assert!(cache.stats().pinned_over_budget);
        cache.unpin(&uri("Open.pas"));
        fill(&cache, "B.pas", &ctx, 1, 1);
        assert!(cache.has_room());
    }

    #[test]
    fn project_switch_sweep_drops_other_fingerprints_and_stale_claims() {
        let cache = ProjectCache::new(1 << 20);
        let a = context("A.dproj");
        let b = context("B.dproj");
        fill(&cache, "Main.pas", &a, 1, 1);
        fill(&cache, "Other.pas", &b, 1, 1);
        let Lookup::Compute(stale) =
            cache.imports(&uri("Late.pas"), &a, 1, &HashMap::new(), &no_cancel())
        else {
            panic!()
        };
        cache.retain_fingerprints(&HashSet::from([project_context_fingerprint(&b)]));
        cache.store_imports(stale, import_value(vec![]), 1, &no_cancel());
        assert!(matches!(
            cache.imports(&uri("Main.pas"), &a, 1, &HashMap::new(), &no_cancel()),
            Lookup::Compute(_)
        ));
        assert!(
            matches!(
                cache.imports(&uri("Late.pas"), &a, 1, &HashMap::new(), &no_cancel()),
                Lookup::Compute(_)
            ),
            "results from before the switch are discarded"
        );
        assert!(matches!(
            cache.imports(&uri("Other.pas"), &b, 1, &HashMap::new(), &no_cancel()),
            Lookup::Hit(_)
        ));
    }

    #[test]
    fn invalidate_path_evicts_entries_that_observed_it() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let Lookup::Compute(claim) =
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!()
        };
        let stamp = pascal_project::path_stamp_result(&dir).ok().flatten();
        cache.store_imports(
            claim,
            import_value(vec![Probe::Stamp {
                path: dir.clone(),
                expected: stamp,
            }]),
            1,
            &no_cancel(),
        );
        let affected = cache.invalidate_path(&dir.join("New.pas"));
        assert_eq!(affected, vec![uri("Main.pas")]);
        assert!(matches!(
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Compute(_)
        ));
    }

    fn store_directory_observer(cache: &ProjectCache, dir: &Path) {
        let ctx = context("A.dproj");
        let Lookup::Compute(claim) =
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!()
        };
        let stamp = pascal_project::path_stamp_result(dir).ok().flatten();
        cache.store_imports(
            claim,
            import_value(vec![Probe::Stamp {
                path: dir.to_path_buf(),
                expected: stamp,
            }]),
            1,
            &no_cancel(),
        );
    }

    #[test]
    fn build_product_events_do_not_stale_the_cache() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        for name in ["Unit1.o", "build.lock", "settings.ini"] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let cache = ProjectCache::new(1 << 20);
        store_directory_observer(&cache, &dir);
        let epoch = cache.invalidation_epoch();

        for name in ["Unit1.o", "build.lock", "settings.ini"] {
            assert!(cache.invalidate_file_contents(&dir.join(name)).is_empty());
            // A create or delete in the observed directory changes its
            // listing, but not for a file no analysis reads.
            assert!(cache.invalidate_path(&dir.join(name)).is_empty());
        }

        assert_eq!(cache.invalidation_epoch(), epoch);
        assert!(matches!(
            cache.imports(
                &uri("Main.pas"),
                &context("A.dproj"),
                1,
                &HashMap::new(),
                &no_cancel()
            ),
            Lookup::Hit(_)
        ));
    }

    #[test]
    fn sources_under_an_opencode_ancestor_of_the_root_still_stale_the_cache() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".opencode/work/ws");
        std::fs::create_dir_all(&root).unwrap();
        let cache = ProjectCache::new(1 << 20);
        cache.set_artifact_roots(vec![root.clone()]);
        store_directory_observer(&cache, &root);

        for name in ["New.pas", "App.dpr", "App.dproj", "inc/x.inc"] {
            let epoch = cache.invalidation_epoch();
            cache.invalidate_path(&root.join(name));
            assert_ne!(cache.invalidation_epoch(), epoch, "{name}");
        }
        // Even a non-source file bumps: the ancestor is not tool state.
        let epoch = cache.invalidation_epoch();
        cache.invalidate_file_contents(&root.join("notes.txt"));
        assert_ne!(cache.invalidation_epoch(), epoch);
    }

    #[test]
    fn opencode_state_below_a_root_does_not_stale_the_cache() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("ws");
        std::fs::create_dir_all(root.join(".opencode/db")).unwrap();
        let cache = ProjectCache::new(1 << 20);
        cache.set_artifact_roots(vec![root.clone()]);
        let epoch = cache.invalidation_epoch();

        cache.invalidate_file_contents(&root.join(".opencode/db/x.sqlite"));
        cache.invalidate_path(&root.join(".opencode/db/y.bin"));
        assert_eq!(cache.invalidation_epoch(), epoch);

        // A source is never tool state, wherever it sits.
        cache.invalidate_path(&root.join(".opencode/db/Z.pas"));
        assert_ne!(cache.invalidation_epoch(), epoch);
    }

    #[test]
    fn a_crowded_invalidation_counts_as_touching_every_unit() {
        let cache = ProjectCache::new(1 << 30);
        let shared = PathBuf::from("/w/Shared.inc");
        let ctx = context("A.dproj");
        for index in 0..(MAX_AFFECTED_PER_RECORD + 5) {
            let main = uri(&format!("U{index}.pas"));
            let Lookup::Compute(claim) =
                cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel())
            else {
                panic!()
            };
            cache.store_imports(
                claim,
                import_value(vec![Probe::Stamp {
                    path: shared.clone(),
                    expected: None,
                }]),
                1,
                &no_cancel(),
            );
        }
        let before = cache.invalidation_epoch();
        let affected = cache.invalidate_file_contents(&shared);
        assert!(affected.len() > MAX_AFFECTED_PER_RECORD);

        let unrelated = HashSet::from([uri("Unrelated.pas")]);
        assert!(cache.units_invalidated_since(before, &unrelated));
        assert!(
            lock(&cache.inner)
                .invalidation_log
                .iter()
                .all(|record| record.affected.len() <= MAX_AFFECTED_PER_RECORD)
        );
    }

    #[test]
    fn a_dispatch_at_the_dropped_epoch_is_still_covered_by_the_log() {
        let cache = ProjectCache::new(1 << 20);
        let units = HashSet::from([uri("Sync.pas")]);
        for index in 0..=MAX_INVALIDATION_LOG {
            cache.invalidate_file_contents(Path::new(&format!("/w/Other{index}.pas")));
        }
        // Epoch 1's record was dropped. A dispatch at epoch 1 already saw it,
        // while one at epoch 0 may have missed it.
        assert!(!cache.units_invalidated_since(1, &units));
        assert!(cache.units_invalidated_since(0, &units));
    }

    #[test]
    fn a_source_created_in_an_observed_directory_still_stales_the_cache() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let cache = ProjectCache::new(1 << 20);
        store_directory_observer(&cache, &dir);
        let epoch = cache.invalidation_epoch();

        assert_eq!(
            cache.invalidate_path(&dir.join("New.pas")),
            vec![uri("Main.pas")]
        );
        assert_ne!(cache.invalidation_epoch(), epoch);
    }

    #[test]
    fn compiled_unit_and_unknown_kinds_still_stale_the_cache() {
        let cache = ProjectCache::new(1 << 20);
        for name in [
            "/w/Unit1.dcu",
            "/w/Pack.dcp",
            "/w/Pack.bpl",
            "/w/a.optset",
            "/w/x.txt",
        ] {
            let epoch = cache.invalidation_epoch();
            cache.invalidate_file_contents(Path::new(name));
            assert_ne!(cache.invalidation_epoch(), epoch, "{name}");
        }
    }

    #[test]
    fn an_observed_build_product_still_stales_the_cache() {
        let temp = tempfile::tempdir().unwrap();
        let object = temp.path().join("Unit1.o");
        std::fs::write(&object, "x").unwrap();
        let cache = ProjectCache::new(1 << 20);
        store_directory_observer(&cache, &object);
        let epoch = cache.invalidation_epoch();

        assert_eq!(
            cache.invalidate_file_contents(&object),
            vec![uri("Main.pas")]
        );
        assert_ne!(cache.invalidation_epoch(), epoch);
    }

    #[test]
    fn units_invalidated_since_sees_only_the_units_it_is_asked_about() {
        let cache = ProjectCache::new(1 << 20);
        let sync = uri("Sync.pas");
        let units = HashSet::from([sync.clone()]);
        let before = cache.invalidation_epoch();
        assert!(!cache.units_invalidated_since(before, &units));

        cache.invalidate_file_contents(Path::new("/w/Other.pas"));
        assert!(!cache.units_invalidated_since(before, &units));

        cache.invalidate_file_contents(&sync.to_file_path().unwrap());
        assert!(cache.units_invalidated_since(before, &units));
        assert!(!cache.units_invalidated_since(cache.invalidation_epoch(), &units));

        let overflowed = cache.invalidation_epoch();
        cache.invalidate_after_overflow();
        assert!(cache.units_invalidated_since(overflowed - 1, &HashSet::new()));
    }

    #[test]
    fn invalidation_after_claim_prevents_store() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let main = uri("Main.pas");
        let Lookup::Compute(claim) = cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!()
        };

        cache.invalidate_path(&temp.path().join("Provider.pas"));
        cache.store_imports(claim, import_value(vec![]), 1, &no_cancel());

        assert_eq!(cache.stats().imports, 0);
        assert!(matches!(
            cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Compute(_)
        ));
    }

    #[test]
    fn overflow_drops_imports_releases_watches_and_keeps_pins() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let main = uri("Main.pas");
        let watch_dir = PathBuf::from("/ws/includes");
        let owner = uri("Open.pas");
        cache.pin(
            &owner,
            vec![(main.clone(), project_context_fingerprint(&ctx))],
        );
        let Lookup::Compute(claim) = cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!("expected import miss")
        };
        let mut imports = import_value(vec![]);
        imports.watch_dirs.push(watch_dir.clone());
        cache.store_imports(claim, imports, 1, &no_cancel());
        assert_eq!(lock(&cache.inner).watch_counts.get(&watch_dir), Some(&1));

        cache.invalidate_after_overflow();
        assert!(matches!(
            cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Compute(_)
        ));
        let state = lock(&cache.inner);
        assert!(
            state.watch_counts.is_empty(),
            "overflow releases import watches"
        );
        assert!(
            state.pins.contains_key(&owner),
            "overflow keeps open-file pins"
        );
    }

    #[test]
    fn overflow_prevents_a_prior_import_claim_from_storing() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let main = uri("Main.pas");
        let snapshot_epoch = cache.invalidation_epoch();
        let Lookup::Compute(claim) = cache.imports_with_epoch(
            &main,
            &ctx,
            1,
            &HashMap::new(),
            Some(snapshot_epoch),
            &no_cancel(),
        ) else {
            panic!("expected import miss")
        };

        cache.invalidate_after_overflow();
        cache.store_imports(claim, import_value(vec![]), 1, &no_cancel());

        assert_eq!(cache.stats().imports, 0);
        assert!(matches!(
            cache.imports(&main, &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Compute(_)
        ));
    }

    #[test]
    fn invalidated_during_probe_verification_is_not_returned() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Observed.pas");
        std::fs::write(&path, "unit Observed;").unwrap();
        let expected = pascal_project::path_stamp_result(&path).unwrap().unwrap();
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        let Lookup::Compute(claim) =
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel())
        else {
            panic!()
        };
        cache.store_imports(
            claim,
            import_value(vec![Probe::Stamp {
                path: path.clone(),
                expected: Some(expected),
            }]),
            1,
            &no_cancel(),
        );

        let invalidating_cache = cache.clone();
        cache.set_validation_hook(move || {
            assert_eq!(
                invalidating_cache.invalidate_path(&path),
                vec![uri("Main.pas")]
            );
        });
        assert!(matches!(
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel()),
            Lookup::Compute(_)
        ));
    }

    #[test]
    fn a_reservation_evicts_unpinned_entries_least_recently_used_first() {
        let cache = ProjectCache::new(30);
        let ctx = context("A.dproj");
        fill(&cache, "A.pas", &ctx, 1, 10);
        fill(&cache, "B.pas", &ctx, 1, 10);
        fill(&cache, "C.pas", &ctx, 1, 10);
        let reservation = cache
            .reserve(ReservationKind::Facts, 15)
            .expect("two evictions make room");
        assert!(!ready(&cache, "A.pas") && !ready(&cache, "B.pas"));
        assert!(ready(&cache, "C.pas"), "only what was needed is evicted");
        let stats = cache.stats();
        assert_eq!((stats.bytes, stats.reserved_facts_bytes), (10, 15));
        assert_eq!(reservation.kind(), ReservationKind::Facts);
        assert_eq!(reservation.bytes(), 15);
    }

    #[test]
    fn a_reservation_that_pinned_entries_leave_no_room_for_is_refused_without_evicting() {
        let cache = ProjectCache::new(20);
        let ctx = context("A.dproj");
        pin_names(&cache, "Open.pas", &["A.pas"], &ctx);
        fill(&cache, "A.pas", &ctx, 1, 10);
        fill(&cache, "B.pas", &ctx, 1, 5);
        assert!(cache.reserve(ReservationKind::Pages, 11).is_none());
        assert!(ready(&cache, "A.pas"), "pinned entries are never evicted");
        assert!(ready(&cache, "B.pas"), "nothing is evicted for a refusal");
        assert_eq!(cache.stats().reserved_bytes(), 0);
        let fits = cache
            .reserve(ReservationKind::Pages, 10)
            .expect("B makes room");
        assert!(ready(&cache, "A.pas") && !ready(&cache, "B.pas"));
        drop(fits);
    }

    #[test]
    fn reservations_shrink_release_on_drop_and_are_totalled_per_kind() {
        let cache = ProjectCache::new(100);
        let mut facts = cache.reserve(ReservationKind::Facts, 10).unwrap();
        let scratch = cache.reserve(ReservationKind::Scratch, 3).unwrap();
        let pages = cache.reserve(ReservationKind::Pages, 2).unwrap();
        facts.shrink_to(4);
        facts.shrink_to(50);
        assert_eq!(facts.bytes(), 4, "a reservation never grows");
        let stats = cache.stats();
        assert_eq!(
            (
                stats.reserved_facts_bytes,
                stats.reserved_scratch_bytes,
                stats.reserved_pages_bytes,
                stats.reserved_bytes()
            ),
            (4, 3, 2, 9)
        );
        drop((facts, scratch, pages));
        assert_eq!(cache.stats().reserved_bytes(), 0);
    }

    fn provider_key(name: &str) -> pascal_core::compile_time::ProviderGraphKey {
        pascal_core::compile_time::ProviderGraphKey {
            source: pascal_core::SourceId::new(format!("/p/{name}.pas")),
            content_hash: 1,
            disk_stamp: None,
            overlay_version: Some(1),
            context: Default::default(),
            qualifiers: vec![],
        }
    }

    fn provider_graph(bytes: usize) -> Arc<pascal_core::compile_time::SharedProviderGraph> {
        Arc::new(pascal_core::compile_time::SharedProviderGraph {
            occurrence: pascal_core::compile_time::OccurrenceId(1),
            graph: None,
            observations: Default::default(),
            warnings: Default::default(),
            dependencies: Default::default(),
            scanned: Default::default(),
            children: Default::default(),
            trace: Default::default(),
            revision: pascal_core::SourceRevision::Overlay {
                version: 1,
                content_hash: 1,
            },
            lookup: (0, 0),
            retained_bytes: bytes,
        })
    }

    /// 13b-2 fix round 1 (M2): a provider interface whose reads may lack
    /// one it named is not shared, so no session inherits that overflow;
    /// each compiles it with its own reads instead.
    #[test]
    fn a_provider_interface_with_overflowed_reads_is_not_kept() {
        let cache = ProjectCache::new(1 << 20);
        let overflowed = crate::workspace::capture::CapturedReads {
            overflowed: true,
            ..Default::default()
        };
        assert!(!cache.put_provider_graph(
            7,
            provider_key("A"),
            provider_graph(1_000),
            vec![],
            overflowed
        ));
        assert_eq!(cache.provider_graph_count(), 0);
        assert_eq!(cache.provider_graph_bytes(), 0);
        // 13b-3 (N1): the refusal is counted for `pascal/indexStatus`.
        assert_eq!(cache.index_process_counts().provider_graphs_refused, 1);
        assert!(cache.put_provider_graph(
            7,
            provider_key("A"),
            provider_graph(1_000),
            vec![],
            Default::default()
        ));
    }

    /// l4d-4nbs D29: shared provider interfaces are charged to
    /// `maxCacheBytes` and evicted least recently used first past their
    /// share; reuse refreshes an entry.
    #[test]
    fn provider_interfaces_are_evicted_least_recently_used_first() {
        let cache = ProjectCache::new(4 * 3_000);
        let overlays = HashMap::new();
        for name in ["A", "B"] {
            assert!(cache.put_provider_graph(
                7,
                provider_key(name),
                provider_graph(1_000),
                vec![],
                Default::default()
            ));
        }
        let charged = cache.provider_graph_bytes();
        assert!(charged >= 2_000, "{charged}");
        assert!(
            cache
                .provider_graph(7, &provider_key("A"), &overlays)
                .is_some()
        );
        assert!(
            cache
                .provider_graph(8, &provider_key("A"), &overlays)
                .is_none(),
            "another resolver configuration shares nothing"
        );
        assert!(cache.put_provider_graph(
            7,
            provider_key("C"),
            provider_graph(1_000),
            vec![],
            Default::default()
        ));
        assert!(
            cache
                .provider_graph(7, &provider_key("B"), &overlays)
                .is_none()
        );
        assert!(
            cache
                .provider_graph(7, &provider_key("A"), &overlays)
                .is_some()
        );
        assert!(
            cache
                .provider_graph(7, &provider_key("C"), &overlays)
                .is_some()
        );
        assert_eq!(cache.provider_graph_bytes(), charged);
        cache.invalidate_after_overflow();
        assert_eq!(cache.provider_graph_count(), 0);
        assert_eq!(cache.provider_graph_bytes(), 0);
    }

    /// l4d-4nbs review I2: shared provider interfaces give way to every
    /// charge: a catalogue, the workspace index and unit stores evict them,
    /// by recency with unit entries; their share is a cap, not a guarantee.
    #[test]
    fn provider_interfaces_give_way_to_every_charge() {
        let cache = ProjectCache::new(24_000);
        let overlays = HashMap::new();
        assert!(cache.put_provider_graph(
            7,
            provider_key("A"),
            provider_graph(4_000),
            vec![],
            Default::default()
        ));
        let held = cache.reserve(ReservationKind::Scratch, 18_000).unwrap();
        assert!(
            cache.charge_catalogue(2_900),
            "a catalogue evicts the interface"
        );
        assert!(
            cache
                .provider_graph(7, &provider_key("A"), &overlays)
                .is_none()
        );
        assert_eq!(cache.provider_graph_bytes(), 0);
        cache.release_catalogues(vec![]);
        drop(held);

        let cache = ProjectCache::new(24_000);
        let held = cache.reserve(ReservationKind::Scratch, 18_000).unwrap();
        assert!(cache.put_provider_graph(
            7,
            provider_key("B"),
            provider_graph(4_000),
            vec![],
            Default::default()
        ));
        assert!(cache.charge_index(2_900), "the index evicts the interface");
        assert_eq!(cache.provider_graph_count(), 0);
        drop(held);

        let cache = ProjectCache::new(24_000);
        let ctx = context("A.dproj");
        assert!(cache.put_provider_graph(
            7,
            provider_key("C"),
            provider_graph(4_000),
            vec![],
            Default::default()
        ));
        fill(&cache, "Old.pas", &ctx, 1, 10_000);
        fill(&cache, "New.pas", &ctx, 1, 10_000);
        assert_eq!(
            cache.provider_graph_count(),
            0,
            "the older interface goes first"
        );
        assert!(ready(&cache, "Old.pas") && ready(&cache, "New.pas"));
    }

    /// An index batch dispatches when its first member's charge would fit
    /// once shared provider interfaces give way, as the charge itself does
    /// (`charge_index`, l4d-4nbs review I2).
    #[test]
    fn an_index_batch_dispatches_past_evictable_provider_interfaces() {
        let cache = ProjectCache::new(24_000);
        assert!(cache.put_provider_graph(
            7,
            provider_key("A"),
            provider_graph(4_000),
            vec![],
            Default::default()
        ));
        let held = cache.reserve(ReservationKind::Scratch, 18_000).unwrap();
        assert!(
            cache.index_batch_fits(),
            "the interface would give way to the member's charge"
        );
        assert!(cache.charge_index(INDEX_BATCH_DISPATCH_BYTES));
        drop(held);
    }

    /// 13b-3 (A2): `make_room` and `index_batch_fits` leave the shared
    /// provider interfaces out of what cannot give way, as `has_room` does:
    /// with pinned, outstanding and other reserved bytes leaving room, a
    /// small request evicts the interfaces instead of being refused while
    /// the budget is over (outstanding bytes counted past it).
    #[test]
    fn evictable_provider_interfaces_never_refuse_a_small_request() {
        let max = 24_000;
        let cache = ProjectCache::new(max);
        assert!(cache.put_provider_graph(
            7,
            provider_key("A"),
            provider_graph(5_000),
            vec![],
            Default::default()
        ));
        let graphs = cache.provider_graph_bytes();
        assert!(graphs > INDEX_BATCH_DISPATCH_BYTES + 100, "{graphs}");
        let held = cache.reserve(ReservationKind::Scratch, 16_000).unwrap();
        // Parses evicted while leased: their charge stays outstanding, and
        // with the interfaces the budget is over.
        lock(&cache.inner).outstanding_bytes = max - 16_000 - INDEX_BATCH_DISPATCH_BYTES - 100;
        assert!(cache.has_room(), "{:?}", cache.stats());
        assert!(cache.index_batch_fits(), "{:?}", cache.stats());
        let small = cache.reserve(ReservationKind::Scratch, 100);
        assert!(
            small.is_some(),
            "the interface gives way: {:?}",
            cache.stats()
        );
        assert_eq!(cache.provider_graph_count(), 0);
        drop((small, held));
    }

    /// 13b-3 fix round 1 (I1): pooled publication bytes are outside the
    /// budget everywhere: admitting them evicts nothing, and a reservation
    /// beside them is checked and made room for without them, so it never
    /// flushes the cache and is then refused.
    #[test]
    fn pooled_publication_bytes_are_outside_the_budget() {
        let max = 24_000;
        let cache = ProjectCache::new(max);
        let ctx = context("A.dproj");
        assert!(cache.put_provider_graph(
            7,
            provider_key("A"),
            provider_graph(4_000),
            vec![],
            Default::default()
        ));
        let graphs = cache.provider_graph_bytes();
        fill(&cache, "Old.pas", &ctx, 1, 1_000);
        let held = cache
            .reserve(ReservationKind::Facts, max - graphs - 1_000)
            .unwrap();
        assert_eq!(cache.provider_graph_count(), 1);
        let pool = cache.publication_pool_bytes();
        let pooled = cache.reserve_publication(pool).expect("from the pool");
        assert_eq!(cache.provider_graph_count(), 1, "admission evicts nothing");
        assert!(ready(&cache, "Old.pas"), "admission evicts nothing");
        assert!(
            cache
                .reserve(ReservationKind::Scratch, graphs + 1_001)
                .is_none(),
            "refused before eviction"
        );
        assert_eq!(cache.provider_graph_count(), 1, "nothing was flushed");
        assert!(ready(&cache, "Old.pas"), "nothing was flushed");
        drop(held);
        let small = cache
            .reserve(ReservationKind::Scratch, 1_000)
            .expect("fits beside the pool");
        assert_eq!(
            cache.provider_graph_count(),
            1,
            "nothing given way for the pool"
        );
        assert!(ready(&cache, "Old.pas"));
        drop((small, pooled));
    }

    /// 13b-3 (A2): a publication's transients draw on a small pool of their
    /// own, so they are admitted while pins, leases and other reservations
    /// fill the parse budget; past the pool they ask the budget as any
    /// reservation does. The crawl's and the index's room ignore them.
    #[test]
    fn publication_transients_draw_on_their_own_pool() {
        let max = 160_000;
        let cache = ProjectCache::new(max);
        let pool = cache.publication_pool_bytes();
        assert_eq!(pool, max / PUBLICATION_POOL_SHARE);
        let full = cache.reserve(ReservationKind::Facts, max).unwrap();
        assert!(cache.reserve(ReservationKind::Scratch, 1).is_none());
        let first = cache.reserve_publication(pool / 2).expect("from the pool");
        let second = cache.reserve_publication(pool / 2).expect("from the pool");
        assert_eq!(cache.stats().reserved_publication_bytes, pool);
        assert!(
            cache.reserve_publication(1).is_none(),
            "the pool is used up and the budget is full"
        );
        drop(full);
        assert!(cache.has_room() && cache.index_batch_fits());
        let past = cache
            .reserve_publication(1)
            .expect("past the pool, from the budget");
        drop((first, second, past));
        assert_eq!(cache.stats().reserved_publication_bytes, 0);
        let big = cache.reserve_publication(max / 2).expect("from the budget");
        assert_eq!(cache.stats().reserved_publication_bytes, max / 2);
        drop(big);
    }

    /// Evictable provider interfaces do not stop the crawl (l4d-axsn): with
    /// pins at their half, the index at its quarter and interfaces filling
    /// the rest, the crawl still has room, and what it stores evicts them.
    #[test]
    fn evictable_provider_interfaces_leave_the_crawl_room() {
        let max = 24_000;
        let cache = ProjectCache::new(max);
        let ctx = context("A.dproj");
        let fingerprint = project_context_fingerprint(&ctx);
        for name in ["A.pas", "B.pas"] {
            fill(&cache, name, &ctx, 1, max / 4);
        }
        assert!(
            cache
                .pin_capped(
                    &uri("Open.pas"),
                    vec![(uri("A.pas"), fingerprint), (uri("B.pas"), fingerprint)],
                    &[],
                )
                .is_empty()
        );
        assert_eq!(cache.stats().pinned_bytes, max / 2);
        assert!(cache.charge_index(max / 4));
        assert!(cache.put_provider_graph(
            7,
            provider_key("A"),
            provider_graph(4_000),
            vec![],
            Default::default()
        ));
        let graphs = cache.provider_graph_bytes();
        assert!(graphs > 0 && graphs <= max / 4, "{graphs}");
        // Whatever the interface's overhead, the quarter is full.
        let _filler = (graphs < max / 4)
            .then(|| cache.reserve(ReservationKind::Scratch, max / 4 - graphs))
            .flatten();
        assert!(
            cache.has_room(),
            "the interfaces would give way: {:?}",
            cache.stats()
        );

        fill(&cache, "C.pas", &ctx, 1, 1_000);
        assert!(ready(&cache, "C.pas"), "the crawl's entry is kept");
        assert_eq!(cache.provider_graph_count(), 0, "the interface gave way");
        assert!(ready(&cache, "A.pas") && ready(&cache, "B.pas"), "pinned");
    }

    #[test]
    fn reserved_bytes_count_in_eviction_and_room() {
        let cache = ProjectCache::new(20);
        let ctx = context("A.dproj");
        let reservation = cache.reserve(ReservationKind::Scratch, 15).unwrap();
        fill(&cache, "A.pas", &ctx, 1, 10);
        assert!(
            !ready(&cache, "A.pas"),
            "a store evicts to fit the reservation"
        );
        assert!(cache.has_room());
        let rest = cache.reserve(ReservationKind::Scratch, 5).unwrap();
        assert!(!cache.has_room(), "reservations alone fill the budget");
        drop((reservation, rest));
        assert!(cache.has_room());
    }

    #[test]
    fn reserving_several_kinds_is_all_or_nothing_and_evicts_only_when_all_fit() {
        let cache = ProjectCache::new(20);
        let ctx = context("A.dproj");
        fill(&cache, "A.pas", &ctx, 1, 5);
        assert!(
            cache
                .reserve_all(&[(ReservationKind::Pages, 12), (ReservationKind::Scratch, 9)])
                .is_none(),
            "the two do not fit together"
        );
        assert!(ready(&cache, "A.pas"), "nothing was evicted");
        assert_eq!(cache.stats().reserved_bytes(), 0);
        let held = cache
            .reserve_all(&[(ReservationKind::Pages, 12), (ReservationKind::Scratch, 8)])
            .unwrap();
        assert!(!ready(&cache, "A.pas"), "evicted to fit both");
        assert_eq!(
            held.iter()
                .map(|reservation| (reservation.kind(), reservation.bytes()))
                .collect::<Vec<_>>(),
            [(ReservationKind::Pages, 12), (ReservationKind::Scratch, 8)]
        );
        let stats = cache.stats();
        assert_eq!(
            (stats.reserved_pages_bytes, stats.reserved_scratch_bytes),
            (12, 8)
        );
        drop(held);
        assert_eq!(cache.stats().reserved_bytes(), 0);
    }

    #[test]
    fn a_lower_budget_keeps_held_reservations_and_refuses_new_ones() {
        let cache = ProjectCache::new(100);
        let held = cache.reserve(ReservationKind::Facts, 60).unwrap();
        cache.set_max_bytes(50);
        assert_eq!(
            cache.stats().reserved_facts_bytes,
            60,
            "held bytes stay charged"
        );
        assert!(cache.reserve(ReservationKind::Scratch, 1).is_none());
        drop(held);
        assert!(cache.reserve(ReservationKind::Scratch, 1).is_some());
    }

    #[test]
    fn leased_parses_evicted_for_a_reservation_stay_outstanding_and_refuse_it() {
        let cache = ProjectCache::new(15);
        let ctx = context("A.dproj");
        store_unit_bytes(&cache, "A.pas", &ctx, 10);
        let unit = cache
            .peek_unit(&uri("A.pas"), &ctx, 1, &HashMap::new())
            .expect("hit");
        let lease = cache.lease_unit(&unit);
        assert!(
            cache.reserve(ReservationKind::Facts, 10).is_none(),
            "evicting A moves its charge to outstanding, so nothing fits"
        );
        assert!(!ready(&cache, "A.pas"));
        assert_eq!(cache.stats().outstanding_bytes, 10);
        assert_eq!(cache.stats().reserved_bytes(), 0);
        drop((lease, unit));
        assert!(cache.reserve(ReservationKind::Facts, 10).is_some());
    }

    #[test]
    fn poison_recovery_keeps_held_reservations_charged_until_they_drop() {
        let cache = ProjectCache::new(20);
        let mut held = cache.reserve(ReservationKind::Pages, 12).unwrap();
        let other = cache.reserve(ReservationKind::Scratch, 3).unwrap();
        cache.poison_state();
        let stats = cache.stats();
        assert_eq!(
            (stats.reserved_pages_bytes, stats.reserved_scratch_bytes),
            (12, 3),
            "recovery keeps what holders still own"
        );
        assert!(cache.reserve(ReservationKind::Facts, 6).is_none());
        held.shrink_to(5);
        drop(other);
        assert_eq!(cache.stats().reserved_bytes(), 5);
        drop(held);
        assert_eq!(cache.stats().reserved_bytes(), 0);
        assert!(cache.reserve(ReservationKind::Facts, 20).is_some());
    }

    /// Spec §12.1 amendment 2026-11-02: reservations racing eviction. Every
    /// operation runs under the state lock, so each statistics snapshot
    /// either thread takes must be within the budget, and the pinned entry
    /// must still be ready. The barrier only lines the threads up; it does
    /// not force any particular interleaving, so this is a safety net, not
    /// a proof.
    #[test]
    fn reservations_racing_stores_never_exceed_the_budget() {
        const MAX: usize = 32;
        let cache = ProjectCache::new(MAX);
        let ctx = context("A.dproj");
        pin_names(&cache, "Open.pas", &["P.pas"], &ctx);
        fill(&cache, "P.pas", &ctx, 1, 8);
        let start = Arc::new(std::sync::Barrier::new(2));
        let within = |cache: &ProjectCache| {
            let stats = cache.stats();
            assert!(
                stats.bytes + stats.outstanding_bytes + stats.reserved_bytes() <= MAX,
                "{stats:?}"
            );
            assert!(ready(cache, "P.pas"), "pinned entries are never evicted");
        };
        let reserver = {
            let cache = cache.clone();
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                for _ in 0..500 {
                    let reservation = cache
                        .reserve(ReservationKind::Facts, 7)
                        .expect("pinned 8 + 7 fits 32");
                    within(&cache);
                    drop(reservation);
                    within(&cache);
                }
            })
        };
        start.wait();
        for index in 0..500 {
            fill(&cache, &format!("U{index}.pas"), &ctx, 1, 5);
            within(&cache);
        }
        reserver.join().unwrap();
        within(&cache);
        assert_eq!(cache.stats().reserved_bytes(), 0);
    }
}

const WAIT_SLICE: Duration = Duration::from_millis(20);
const MAX_CLOSURE_CRAWL_REQUESTS: usize = 1024;
const MAX_CLOSURE_MISS_ROOTS: usize = 1024;
const MAX_INVALIDATION_LOG: usize = 1024;
const MAX_AFFECTED_PER_RECORD: usize = 64;

/// Extensions of build products and tool state that no analysis reads. A
/// create or delete of such a file never changes what a directory listing
/// means to a cache entry.
const ARTIFACT_EXTENSIONS: &[&str] = &[
    "o", "a", "obj", "ppu", "so", "lock", "ini", "log", "tmp", "bak", "swp", "map", "rsm",
];

struct InvalidationRecord {
    epoch: u64,
    path: PathBuf,
    /// Empty with `crowded` set when more entries were evicted than the log
    /// keeps.
    affected: Vec<Url>,
    crowded: bool,
}

/// Whether `path` is a build product or tool state file. A source or project
/// metadata file never is. `.opencode` counts only as a component below one
/// of the workspace `roots`, never in the roots' own ancestry.
fn is_artifact_path(path: &Path, roots: &[PathBuf]) -> bool {
    if crate::catalogue::is_retained_file(path) {
        return false;
    }
    path.extension().is_some_and(|extension| {
        let extension = extension.to_string_lossy();
        ARTIFACT_EXTENSIONS
            .iter()
            .any(|artifact| extension.eq_ignore_ascii_case(artifact))
    }) || roots.iter().any(|root| {
        path.strip_prefix(root).is_ok_and(|below| {
            below
                .components()
                .any(|component| component.as_os_str() == ".opencode")
        })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Layer {
    Unit,
    Import,
    Interface,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct UnitValue {
    #[allow(dead_code)]
    pub(crate) parsed: Arc<ParsedDocument>,
    /// Reconciled expansion, reused only while every include probe holds.
    #[allow(dead_code)]
    pub(crate) expansion: Option<Arc<ExpansionResult>>,
    pub(crate) probes: Vec<Probe>,
    /// The closed file the unit was read from, when it was read from disk.
    pub(crate) disk: Option<DiskOrigin>,
}

/// A closed source file as it was read. A later load whose stamp this one
/// proves unchanged (`DiskStamp::proves_unchanged`) takes `text` instead of
/// reading and decoding the file again.
#[derive(Debug)]
pub(crate) struct DiskOrigin {
    pub(crate) stamp: crate::workspace::DiskStamp,
    /// Raw file size, before decoding.
    pub(crate) raw_bytes: usize,
    /// Decoded file text. Without include expansion this is the parse's own
    /// text, so it costs nothing extra.
    pub(crate) text: Arc<str>,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct ImportValue {
    /// Shared with the request that resolved it, so hits do not copy it.
    #[allow(dead_code)]
    pub(crate) resolved: Arc<pascal_core::ResolvedImports>,
    #[allow(dead_code)]
    pub(crate) report: Arc<pascal_core::ResolutionReport>,
    pub(crate) probes: Vec<Probe>,
    pub(crate) watch_dirs: Vec<PathBuf>,
}

/// One interface `uses` name of a unit, resolved to a source unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterfaceBinding {
    pub(crate) name: String,
    pub(crate) uri: Url,
    pub(crate) revision: pascal_core::SourceRevision,
}

impl InterfaceBinding {
    /// The bound unit's content hash, which keys its unit entry.
    pub(crate) fn content_hash(&self) -> u64 {
        match &self.revision {
            pascal_core::SourceRevision::Disk { content_hash, .. }
            | pascal_core::SourceRevision::Overlay { content_hash, .. } => *content_hash,
        }
    }
}

/// A unit's interface-section imports. Snapshots use these to bind members
/// inherited through their dependencies' imports without resolving anything.
#[derive(Debug)]
pub(crate) struct InterfaceImportsValue {
    pub(crate) bindings: Vec<InterfaceBinding>,
    /// Whether the unit's import graph resolved completely. Walks do not
    /// continue past an incomplete one.
    pub(crate) complete: bool,
    /// Copied from the unit's import entry. They include a content probe for
    /// every dependency, so a bound unit's hash is verified before use.
    pub(crate) probes: Vec<Probe>,
}

/// A cached value whose probes decide whether it may be served.
trait Probed {
    fn probes(&self) -> &[Probe];
}

impl Probed for UnitValue {
    fn probes(&self) -> &[Probe] {
        &self.probes
    }
}

impl Probed for ImportValue {
    fn probes(&self) -> &[Probe] {
        &self.probes
    }
}

impl Probed for InterfaceImportsValue {
    fn probes(&self) -> &[Probe] {
        &self.probes
    }
}

impl UnitValue {
    /// Estimated heap retained by the entry: the parse at the measured
    /// factor, its include expansion, probes, and file text it does not
    /// share with the parse.
    pub(crate) fn retained_bytes(&self) -> usize {
        let unshared_text = self
            .disk
            .as_ref()
            .filter(|disk| !Arc::ptr_eq(&disk.text, self.parsed.source_text()))
            .map_or(0, |disk| disk.text.len());
        unit_value_bytes(self.parsed.source_text().len())
            .saturating_add(std::mem::size_of::<Self>())
            .saturating_add(self.expansion.as_deref().map_or(0, expansion_bytes))
            .saturating_add(probes_bytes(&self.probes))
            .saturating_add(unshared_text)
    }
}

impl ImportValue {
    /// Estimated heap retained by the entry: dependency payloads, the
    /// resolution report, probes and watched directories.
    pub(crate) fn retained_bytes(&self) -> usize {
        import_value_bytes(&self.resolved)
            .saturating_add(std::mem::size_of_val(self.resolved.bindings.as_slice()))
            .saturating_add(std::mem::size_of_val(self.resolved.dependencies.as_slice()))
            .saturating_add(report_bytes(&self.report))
            .saturating_add(probes_bytes(&self.probes))
            .saturating_add(self.watch_dirs.iter().fold(
                std::mem::size_of_val(self.watch_dirs.as_slice()),
                |total, dir| total.saturating_add(path_bytes(dir)),
            ))
    }
}

impl InterfaceImportsValue {
    pub(crate) fn retained_bytes(&self) -> usize {
        interface_value_bytes(self).saturating_add(probes_bytes(&self.probes))
    }
}

fn interface_value_bytes(value: &InterfaceImportsValue) -> usize {
    value.bindings.iter().fold(256usize, |total, binding| {
        total
            .saturating_add(128)
            .saturating_add(binding.name.len())
            .saturating_add(binding.uri.as_str().len())
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    layer: Layer,
    uri: Url,
    fingerprint: u64,
}

#[allow(dead_code)]
#[derive(Debug)]
enum Value {
    Unit(Arc<UnitValue>),
    Import(Arc<ImportValue>),
    Interface(Arc<InterfaceImportsValue>),
}

#[derive(Debug)]
struct Entry {
    value: Value,
    context: Arc<ProjectContext>,
    input_hash: u64,
    bytes: usize,
    last_used: u64,
}

#[derive(Debug)]
enum Slot {
    Ready(Entry),
    Computing {
        generation: u64,
        input_hash: u64,
        /// Tells this claim from one that took the slot over.
        claim: u64,
        owner: ClaimOwner,
    },
}

/// The claim a thread computes and who it computes for, so that waiters can
/// tell when to stop waiting for it.
#[derive(Debug, Clone)]
struct ClaimOwner {
    thread: std::thread::ThreadId,
    role: ClaimRole,
}

/// How the work running on a thread takes part in cache claims.
#[derive(Debug, Clone, Default)]
pub(crate) struct ClaimRole {
    /// The work answers an interactive request.
    interactive: bool,
    /// The work's cancellation: once set, a waiter takes its claims over.
    cancel: Option<Arc<AtomicBool>>,
    /// Interactive work takes this work's claims over instead of waiting:
    /// the warmer pauses for interactive requests and runs at low priority.
    yields_to_interactive: bool,
    /// How long the work waits for a slot that a thread without a role
    /// (the main thread) computes, before computing it without caching:
    /// such a claim can be neither cancelled nor taken over. Claims of
    /// other owners cover one parse or one import resolution (the importer's
    /// dependency walk runs after the resolution is stored), so they are
    /// waited for as long as they are held.
    roleless_claim_wait: Option<Duration>,
}

impl ClaimRole {
    pub(crate) fn interactive(cancel: Arc<AtomicBool>, roleless_claim_wait: Duration) -> Self {
        Self {
            interactive: true,
            cancel: Some(cancel),
            yields_to_interactive: false,
            roleless_claim_wait: Some(roleless_claim_wait),
        }
    }

    pub(crate) fn job(cancel: Arc<AtomicBool>) -> Self {
        Self {
            interactive: false,
            cancel: Some(cancel),
            yields_to_interactive: false,
            roleless_claim_wait: None,
        }
    }

    pub(crate) fn yielding(cancel: Arc<AtomicBool>) -> Self {
        Self {
            interactive: false,
            cancel: Some(cancel),
            yields_to_interactive: true,
            roleless_claim_wait: None,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
    }
}

static NEXT_CLAIM: AtomicU64 = AtomicU64::new(0);

struct State {
    slots: HashMap<Key, Slot>,
    bytes: usize,
    clock: u64,
    generation: u64,
    invalidation_epoch: u64,
    max_bytes: usize,
    pins: HashMap<Url, HashSet<(Url, u64)>>,
    /// Number of owners in `pins` that pin each `(uri, fingerprint)`.
    pin_counts: HashMap<Url, HashMap<u64, usize>>,
    /// Bytes of the ready entries that are pinned.
    pinned_bytes: usize,
    /// Unpinned ready entries by `last_used`, oldest first. Clock values are
    /// unique, so each entry has its own slot.
    lru: BTreeMap<u64, Key>,
    /// Shared provider interfaces (l4d-4nbs D29), charged as the
    /// `ProviderGraphs` reservation and evicted with unit entries by recency
    /// (review I2).
    provider_graphs: ProviderGraphStore,
    /// Ready entries with a stamp or content probe on the path. A change to
    /// the path, or to a child for a directory probe, invalidates them.
    observed: HashMap<PathBuf, HashSet<Key>>,
    /// Ready entries for the source at the path or with an overlay probe on
    /// it. Only a change to the path itself invalidates them.
    exact: HashMap<PathBuf, HashSet<Key>>,
    by_uri: HashMap<Url, HashSet<Key>>,
    /// One shared copy of each project context, by fingerprint, so entries
    /// do not each hold their own.
    contexts: HashMap<u64, Arc<ProjectContext>>,
    /// Unit entries holding each parse, by `Arc` address, with the entry's
    /// charge.
    cached_parses: HashMap<usize, (usize, usize)>,
    /// Request leases on cached parses, by `Arc` address.
    leases: HashMap<usize, Lease>,
    /// Charges of leased parses that no entry holds any more. They stay in
    /// the budget until the last lease drops.
    outstanding_bytes: usize,
    /// Held reservations by `ReservationKind::index`. Poison recovery keeps
    /// them: their holders release them on drop.
    reserved: [usize; ReservationKind::COUNT],
    /// The part of `reserved[Publication]` drawn on the publication pool.
    publication_pooled: usize,
    watch_counts: HashMap<PathBuf, usize>,
    /// Advances when poison recovery drains `watch_counts`: a catalogue's
    /// watches acquired under an older one were drained and are not released.
    watch_count_epoch: u64,
    /// Directories whose watch count reached or left zero. `sync_watches`
    /// applies them to the watcher once the state lock is released.
    watch_changes: Vec<PathBuf>,
    closure_crawl_requests: Vec<Url>,
    /// Each root's uncached closure units at its last snapshot walk.
    closure_misses: HashMap<Url, HashSet<Url>>,
    /// Epoch bumps from path invalidations, oldest first, for asking whether
    /// given units were invalidated since an epoch.
    invalidation_log: std::collections::VecDeque<InvalidationRecord>,
    /// Epochs below this have no complete record in `invalidation_log`.
    invalidation_log_floor: u64,
    /// Workspace roots, for recognizing tool state below them.
    artifact_roots: Vec<PathBuf>,
    /// The slot each thread waits for, to find waits that would close a
    /// cycle.
    waiting: HashMap<std::thread::ThreadId, Key>,
    /// Ready entries looked at by eviction and invalidation.
    #[cfg(test)]
    examined: std::cell::Cell<usize>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            slots: HashMap::new(),
            bytes: 0,
            clock: 0,
            generation: 0,
            invalidation_epoch: 0,
            max_bytes: DEFAULT_MAX_CACHE_BYTES,
            pins: HashMap::new(),
            pin_counts: HashMap::new(),
            pinned_bytes: 0,
            lru: BTreeMap::new(),
            provider_graphs: ProviderGraphStore::default(),
            observed: HashMap::new(),
            exact: HashMap::new(),
            by_uri: HashMap::new(),
            contexts: HashMap::new(),
            cached_parses: HashMap::new(),
            leases: HashMap::new(),
            outstanding_bytes: 0,
            reserved: [0; ReservationKind::COUNT],
            publication_pooled: 0,
            watch_counts: HashMap::new(),
            watch_count_epoch: 0,
            watch_changes: Vec::new(),
            closure_crawl_requests: Vec::new(),
            closure_misses: HashMap::new(),
            invalidation_log: std::collections::VecDeque::new(),
            invalidation_log_floor: 0,
            artifact_roots: Vec::new(),
            waiting: HashMap::new(),
            #[cfg(test)]
            examined: std::cell::Cell::new(0),
        }
    }
}

#[cfg(test)]
fn note_examined(state: &State) {
    state.examined.set(state.examined.get() + 1);
}

/// The directory watcher and the directories it currently watches.
#[derive(Default)]
struct WatcherState {
    watcher: Option<Box<dyn DirectoryWatch>>,
    registered: HashSet<PathBuf>,
}

/// Lock order: `watcher` before `state`. Code holding `state` never takes
/// `watcher`, never calls the watcher and never touches the filesystem; it
/// queues watch-count changes in `State::watch_changes`, and `sync_watches`
/// applies them after `state` is released. `witnesses` is never held together
/// with either lock. `catalogues` is never held together with any other lock:
/// what it drops is released (charge, watches) after it is unlocked. `index`
/// is likewise never held together with `state`, `watcher`, `witnesses` or
/// `catalogues`: the charges of what it drops are released after it is
/// unlocked.
#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    watcher: Mutex<WatcherState>,
    /// Never held together with `state` or `watcher`.
    witnesses: Mutex<Witnesses>,
    /// Never held together with `state`, `watcher` or `witnesses`.
    catalogues: Mutex<CatalogueStore>,
    /// Never held together with `state`, `watcher`, `witnesses` or `catalogues`.
    index: Mutex<IndexStore>,
    /// Advances whenever the watcher's registrations are lost (a new watcher,
    /// poisoned-state recovery): catalogue watches of an older epoch are gone.
    watch_epoch: AtomicU64,
    /// Provider interfaces not shared because their reads overflowed
    /// (13b-2 follow-up N1), for `pascal/indexStatus`.
    provider_graphs_refused: AtomicU64,
    #[cfg(test)]
    validation_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    waiter_hook: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    /// Told each time a lookup starts waiting for a claim.
    #[cfg(test)]
    every_wait_hook: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Default)]
pub(crate) struct ProjectCache {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ProjectCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectCache")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
impl ProjectCache {
    /// The layers (`"Unit"`, `"Import"`, `"Interface"`) with a ready entry for `uri`.
    pub(crate) fn ready_layers(&self, uri: &Url) -> Vec<String> {
        let state = lock(&self.inner);
        let mut layers = state
            .slots
            .iter()
            .filter(|(key, slot)| &key.uri == uri && matches!(slot, Slot::Ready(_)))
            .map(|(key, _)| format!("{:?}", key.layer))
            .collect::<Vec<_>>();
        layers.sort();
        layers
    }

    fn reset_examined_entries(&self) {
        lock(&self.inner).examined.set(0);
    }

    fn examined_entries(&self) -> usize {
        lock(&self.inner).examined.get()
    }

    fn set_validation_hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self.inner.validation_hook.lock().unwrap() = Some(Box::new(hook));
    }

    fn run_validation_hook(&self) {
        let hook = self.inner.validation_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    fn set_waiter_hook(&self, sender: std::sync::mpsc::Sender<()>) {
        *self.inner.waiter_hook.lock().unwrap() = Some(sender);
    }

    fn signal_waiter_hook(&self) {
        if let Some(sender) = self.inner.waiter_hook.lock().unwrap().take() {
            let _ = sender.send(());
        }
        if let Some(sender) = self.inner.every_wait_hook.lock().unwrap().as_ref() {
            let _ = sender.send(());
        }
    }

    fn set_every_wait_hook(&self, sender: std::sync::mpsc::Sender<()>) {
        *self.inner.every_wait_hook.lock().unwrap() = Some(sender);
    }

    fn poison_state(&self) {
        let inner = self.inner.clone();
        let _ = std::thread::spawn(move || {
            let _state = inner.state.lock().unwrap();
            panic!("intentional test poison");
        })
        .join();
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum Lookup<V> {
    #[allow(dead_code)]
    Hit(Arc<V>),
    Compute(Claim),
    Cancelled,
}

/// Exclusive right to compute one slot. Dropping it without storing frees the
/// slot and wakes waiters so one of them can take over.
///
/// A claim must stay on the thread that obtained it: cycle detection between
/// waiters identifies a claim's owner by that thread.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Claim {
    inner: Arc<Inner>,
    key: Key,
    generation: u64,
    claim: u64,
    invalidation_epoch: u64,
    context: Arc<ProjectContext>,
    input_hash: u64,
    /// The slot is computed by another claim; this one computes without
    /// caching, so storing and dropping it leave the slot alone.
    detached: bool,
    started: std::time::Instant,
}

impl Claim {
    #[cfg_attr(not(feature = "test-support"), allow(dead_code))]
    pub(crate) fn is_detached(&self) -> bool {
        self.detached
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        HELD_CLAIMS.with(|held| held.set(held.get().saturating_sub(1)));
        let mut state = lock(&self.inner);
        if matches!(state.slots.get(&self.key), Some(Slot::Computing { generation, claim, .. })
            if *generation == self.generation && *claim == self.claim)
        {
            state.slots.remove(&self.key);
        }
        drop(state);
        self.inner.changed.notify_all();
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CacheStats {
    pub units: usize,
    pub imports: usize,
    pub bytes: usize,
    /// Bytes of ready entries some owner pins.
    pub pinned_bytes: usize,
    /// Bytes of removed values that workers still hold.
    pub outstanding_bytes: usize,
    /// Pinned bytes alone exceed the budget; reserved and outstanding bytes are reported separately.
    pub pinned_over_budget: bool,
    /// Held reservations, per kind (spec §10 amendment 2026-11-02).
    pub reserved_facts_bytes: usize,
    pub reserved_scratch_bytes: usize,
    pub reserved_pages_bytes: usize,
    pub reserved_catalogue_bytes: usize,
    pub reserved_index_bytes: usize,
    pub reserved_provider_graph_bytes: usize,
    pub reserved_publication_bytes: usize,
}

#[cfg_attr(not(test), allow(dead_code))]
impl CacheStats {
    pub(crate) fn reserved_bytes(&self) -> usize {
        self.reserved_facts_bytes
            .saturating_add(self.reserved_scratch_bytes)
            .saturating_add(self.reserved_pages_bytes)
            .saturating_add(self.reserved_catalogue_bytes)
            .saturating_add(self.reserved_index_bytes)
            .saturating_add(self.reserved_provider_graph_bytes)
            .saturating_add(self.reserved_publication_bytes)
    }
}

/// What a reservation charges to `maxCacheBytes` for (spec §10 amendment
/// 2026-11-02): one budget, not a second one.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ReservationKind {
    /// Decoded persisted facts a caller keeps.
    Facts,
    /// The persistent store's transient decode and encode buffers.
    Scratch,
    /// The persistent store's SQLite page caches and connection overhead.
    Pages,
    /// Shared directory catalogues (l4d-20). The catalogue store holds these
    /// bytes itself, not through a `Reservation`: a stored `Reservation`
    /// would keep `Inner` alive through its own `Arc`.
    Catalogues,
    /// The workspace index (l4d-33). Held by the index store like the
    /// catalogues, and released with `release_index`.
    WorkspaceIndex,
    /// Compiled provider interfaces (l4d-4nbs D29), held by their store like
    /// the catalogues.
    ProviderGraphs,
    /// A member publication's transient buffers (`reserve_publication`).
    Publication,
}

impl ReservationKind {
    const COUNT: usize = 7;

    fn index(self) -> usize {
        match self {
            Self::Facts => 0,
            Self::Scratch => 1,
            Self::Pages => 2,
            Self::Catalogues => 3,
            Self::WorkspaceIndex => 4,
            Self::ProviderGraphs => 5,
            Self::Publication => 6,
        }
    }
}

/// Bytes charged to the budget before they are allocated, held until
/// dropped. Eviction never releases a reservation: what it covers stays
/// charged for as long as its holder keeps it. It must never be stored in
/// `State` or an `Entry`: dropping it takes the state lock, which is not
/// reentrant, so dropping it under that lock would deadlock.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Reservation {
    inner: Arc<Inner>,
    kind: ReservationKind,
    bytes: usize,
    /// Drawn on the publication pool rather than made room for.
    pooled: bool,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Reservation")
            .field("kind", &self.kind)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl Reservation {
    pub(crate) fn kind(&self) -> ReservationKind {
        self.kind
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Keep only `bytes` (a reservation never grows).
    pub(crate) fn shrink_to(&mut self, bytes: usize) {
        let bytes = bytes.min(self.bytes);
        let mut state = lock(&self.inner);
        let reserved = &mut state.reserved[self.kind.index()];
        *reserved = reserved.saturating_sub(self.bytes - bytes);
        if self.pooled {
            state.publication_pooled = state.publication_pooled.saturating_sub(self.bytes - bytes);
        }
        self.bytes = bytes;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = lock(&self.inner);
        let reserved = &mut state.reserved[self.kind.index()];
        *reserved = reserved.saturating_sub(self.bytes);
        if self.pooled {
            state.publication_pooled = state.publication_pooled.saturating_sub(self.bytes);
        }
    }
}

fn publication_pool(state: &State) -> usize {
    PUBLICATION_POOL_BYTES.min(state.max_bytes / PUBLICATION_POOL_SHARE)
}

fn reserved_bytes(state: &State) -> usize {
    state
        .reserved
        .iter()
        .fold(0usize, |total, bytes| total.saturating_add(*bytes))
}

/// Reserved bytes charged to `maxCacheBytes`: all but those drawn on the
/// publication pool, a budget of its own (13b-3, A2).
fn budgeted_reserved_bytes(state: &State) -> usize {
    reserved_bytes(state).saturating_sub(state.publication_pooled)
}

/// Whether `retained` plus everything outstanding and reserved, plus
/// `request`, fits the budget.
fn fits(state: &State, retained: usize, request: usize) -> bool {
    retained
        .checked_add(state.outstanding_bytes)
        .and_then(|total| total.checked_add(budgeted_reserved_bytes(state)))
        .and_then(|total| total.checked_add(request))
        .is_some_and(|total| total <= state.max_bytes)
}

/// What no eviction frees: pinned, outstanding and budgeted reserved
/// bytes, less the shared provider interfaces, a cache that gives way to
/// every request (l4d-4nbs review I2).
fn committed_bytes(state: &State) -> usize {
    state
        .pinned_bytes
        .saturating_add(state.outstanding_bytes)
        .saturating_add(budgeted_reserved_bytes(state).saturating_sub(state.provider_graphs.bytes))
}

/// Whether `request` fits beside what no eviction frees.
fn fits_committed(state: &State, request: usize) -> bool {
    committed_bytes(state)
        .checked_add(request)
        .is_some_and(|total| total <= state.max_bytes)
}

/// Evicts unpinned entries, least recently used first, until `request` more
/// bytes fit. False when they cannot: pinned, outstanding and reserved bytes
/// alone leave no room, or nothing is left to evict.
fn make_room(state: &mut State, request: usize) -> bool {
    // The interfaces' charge is checked against `reserved`, not subtracted
    // from the request (13b-3, A2: a small request was refused before the
    // interfaces were evicted).
    if !fits_committed(state, request) {
        return false;
    }
    while !fits(state, state.bytes, request) {
        if !evict_oldest(state) {
            return false;
        }
    }
    true
}

/// Evicts the least recently used unpinned unit entry or shared provider
/// interface, whichever is older. False when neither is left.
fn evict_oldest(state: &mut State) -> bool {
    let unit = state.lru.first_key_value().map(|(tick, _)| *tick);
    let graph = state
        .provider_graphs
        .lru
        .first_key_value()
        .map(|(tick, _)| *tick);
    match (unit, graph) {
        (Some(unit), Some(graph)) if graph < unit => evict_oldest_graph(state),
        (None, Some(_)) => evict_oldest_graph(state),
        (Some(_), _) => {
            let Some((_, key)) = state.lru.pop_first() else {
                return false;
            };
            #[cfg(test)]
            note_examined(state);
            remove_ready(state, &key);
            true
        }
        (None, None) => false,
    }
}

fn evict_oldest_graph(state: &mut State) -> bool {
    let Some(id) = state
        .provider_graphs
        .lru
        .first_key_value()
        .map(|(_, id)| id.clone())
    else {
        return false;
    };
    let freed = state.provider_graphs.remove(&id);
    release_graph_bytes(state, freed);
    true
}

fn release_graph_bytes(state: &mut State, freed: usize) {
    let index = ReservationKind::ProviderGraphs.index();
    state.reserved[index] = state.reserved[index].saturating_sub(freed);
}

fn recover_poisoned_state<'a>(
    inner: &'a Inner,
    mut state: MutexGuard<'a, State>,
) -> MutexGuard<'a, State> {
    state.slots.clear();
    state.generation = state.generation.wrapping_add(1);
    state.bytes = 0;
    state.pinned_bytes = 0;
    state.cached_parses.clear();
    let mut orphaned = 0usize;
    for lease in state.leases.values_mut().filter(|lease| !lease.evicted) {
        lease.evicted = true;
        orphaned = orphaned.saturating_add(lease.bytes);
    }
    state.outstanding_bytes = state.outstanding_bytes.saturating_add(orphaned);
    state.lru.clear();
    state.observed.clear();
    state.exact.clear();
    state.by_uri.clear();
    let watched_directories = state.watch_counts.drain().map(|(directory, _)| directory);
    let watched_directories = watched_directories.collect::<Vec<_>>();
    state.watch_changes.extend(watched_directories);
    // The drained counts held the catalogue watches too.
    state.watch_count_epoch += 1;
    inner.watch_epoch.fetch_add(1, Ordering::SeqCst);
    inner.state.clear_poison();
    state
}

fn lock(inner: &Inner) -> MutexGuard<'_, State> {
    #[cfg(test)]
    assert!(
        !HELD_INDEX.with(std::cell::Cell::get),
        "the index lock is never held with the state lock"
    );
    match inner.state.lock() {
        Ok(state) => state,
        Err(poisoned) => recover_poisoned_state(inner, poisoned.into_inner()),
    }
}

thread_local! {
    static CLAIM_ROLE: std::cell::RefCell<ClaimRole> =
        std::cell::RefCell::new(ClaimRole::default());
    /// Claims this thread holds on slots, which other lookups may wait for.
    static HELD_CLAIMS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The number of slot claims the calling thread holds. Work that pauses for
/// other work must hold none, or a lookup waiting for its claims would wait
/// out the pause.
pub(crate) fn claims_held_on_this_thread() -> usize {
    HELD_CLAIMS.with(std::cell::Cell::get)
}

/// Sets how the work on the calling thread takes part in cache claims.
///
/// A lookup of a slot another thread computes for the same input waits for
/// that claim rather than computing the slot a second time, while its own
/// cancellation is clear. It takes the claim over instead once the owner's
/// work is cancelled, or when it is interactive and the owner yields to
/// interactive work. It computes without caching when waiting would close a
/// cycle of waits, or when an import slot outlasts the role's import wait.
pub(crate) fn set_claim_role_on_this_thread(role: ClaimRole) {
    CLAIM_ROLE.with(|cell| *cell.borrow_mut() = role);
}

#[cfg_attr(not(feature = "test-support"), allow(dead_code))]
pub(crate) fn claims_are_interactive_on_this_thread() -> bool {
    CLAIM_ROLE.with(|cell| cell.borrow().interactive)
}

/// Lets protocol tests see that an interactive lookup waits for a claim, and
/// for how many wait slices.
#[cfg(feature = "test-support")]
fn mark_interactive_claim_wait_for_tests() {
    use std::io::Write as _;

    let Some(path) = std::env::var_os("PASCAL_LSP_TEST_INTERACTIVE_CLAIM_WAIT_MARKER") else {
        return;
    };
    if let Ok(mut marker) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
    {
        let _ = marker.write_all(b"x");
    }
}

/// Whether `waiter` waiting for a slot `owner` computes would close a cycle
/// of threads each waiting for a slot the next one computes.
fn wait_closes_cycle(
    state: &State,
    waiter: std::thread::ThreadId,
    owner: std::thread::ThreadId,
) -> bool {
    let mut current = owner;
    for _ in 0..=state.waiting.len() {
        if current == waiter {
            return true;
        }
        let Some(key) = state.waiting.get(&current) else {
            return false;
        };
        let Some(Slot::Computing { owner, .. }) = state.slots.get(key) else {
            return false;
        };
        current = owner.thread;
    }
    false
}

fn wait_timeout_recovering_poison_for<'a>(
    inner: &'a Inner,
    state: MutexGuard<'a, State>,
    timeout: Duration,
) -> (MutexGuard<'a, State>, WaitTimeoutResult) {
    match inner.changed.wait_timeout(state, timeout) {
        Ok(result) => result,
        Err(poisoned) => {
            let (state, timeout) = poisoned.into_inner();
            (recover_poisoned_state(inner, state), timeout)
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl ProjectCache {
    pub(crate) fn new(max_bytes: usize) -> Self {
        let cache = Self::default();
        lock(&cache.inner).max_bytes = max_bytes;
        cache
    }

    /// Evicts to the new budget at once. Held reservations and leased
    /// outstanding bytes stay charged; new reservations that do not fit
    /// are refused until they are released.
    #[allow(dead_code)]
    pub(crate) fn set_max_bytes(&self, max_bytes: usize) {
        let mut state = lock(&self.inner);
        state.max_bytes = max_bytes;
        // Shared provider interfaces keep to their new share (a cap, not a
        // guarantee: they give way to everything else, l4d-4nbs review I2).
        let freed = state
            .provider_graphs
            .evict_until(max_bytes / PROVIDER_GRAPH_BUDGET_SHARE);
        release_graph_bytes(&mut state, freed);
        evict_to_budget(&mut state);
        self.release(state);
    }

    /// Charge `bytes` of `kind` before allocating them (spec §10 amendment
    /// 2026-11-02): evict unpinned entries, least recently used first, until
    /// retained, outstanding and reserved bytes plus `bytes` fit
    /// `maxCacheBytes`, or refuse. Pinned entries are never evicted for a
    /// reservation, so when pinned, outstanding and reserved bytes alone
    /// leave no room nothing is evicted. Evicting an entry whose parse a
    /// request leases moves its charge to outstanding, so a reservation can
    /// still be refused after evicting.
    pub(crate) fn reserve(&self, kind: ReservationKind, bytes: usize) -> Option<Reservation> {
        self.reserve_all(&[(kind, bytes)])?.pop()
    }

    /// `reserve` for several kinds at once, under one lock: their total is
    /// checked and made room for as one request, so nothing is evicted
    /// unless all of them fit, and either every reservation is made, in
    /// `requests` order, or none.
    pub(crate) fn reserve_all(
        &self,
        requests: &[(ReservationKind, usize)],
    ) -> Option<Vec<Reservation>> {
        let total = requests
            .iter()
            .try_fold(0usize, |total, (_, bytes)| total.checked_add(*bytes))?;
        let mut state = lock(&self.inner);
        prune_dead_leases(&mut state);
        if !make_room(&mut state, total) {
            self.release(state);
            return None;
        }
        for &(kind, bytes) in requests {
            state.reserved[kind.index()] += bytes;
        }
        self.release(state);
        Some(
            requests
                .iter()
                .map(|&(kind, bytes)| Reservation {
                    inner: Arc::clone(&self.inner),
                    kind,
                    bytes,
                    pooled: false,
                })
                .collect(),
        )
    }

    /// Reserve a member publication's transient bytes (13b-3, A2). Up to
    /// the publication pool they are admitted whatever pins, leases and
    /// other reservations hold, and evict nothing: the pool is outside
    /// `maxCacheBytes`, which no check or eviction counts it in. Past the
    /// pool they are made room for as `reserve` does, or refused.
    pub(crate) fn reserve_publication(&self, bytes: usize) -> Option<Reservation> {
        let kind = ReservationKind::Publication;
        let mut state = lock(&self.inner);
        prune_dead_leases(&mut state);
        let pooled = state
            .publication_pooled
            .checked_add(bytes)
            .is_some_and(|total| total <= publication_pool(&state));
        if pooled {
            state.publication_pooled += bytes;
            state.reserved[kind.index()] += bytes;
        } else if make_room(&mut state, bytes) {
            state.reserved[kind.index()] += bytes;
        } else {
            self.release(state);
            return None;
        }
        self.release(state);
        Some(Reservation {
            inner: Arc::clone(&self.inner),
            kind,
            bytes,
            pooled,
        })
    }

    /// The publication pool's size under the current budget.
    pub(crate) fn publication_pool_bytes(&self) -> usize {
        publication_pool(&lock(&self.inner))
    }

    /// Releases the state lock, then applies the watch changes it queued.
    fn release(&self, state: MutexGuard<'_, State>) {
        let changed = !state.watch_changes.is_empty();
        drop(state);
        if changed {
            self.sync_watches();
        }
    }

    /// Brings the watcher in line with the current watch counts of every
    /// directory queued in `watch_changes`. Changes are applied in the order
    /// threads take the watcher lock, each against the counts at that time,
    /// so the last application for a directory always reflects its count.
    fn sync_watches(&self) {
        let mut watcher = self
            .inner
            .watcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let changes = {
            let mut state = lock(&self.inner);
            let mut directories = std::mem::take(&mut state.watch_changes);
            directories.sort();
            directories.dedup();
            directories
                .into_iter()
                .map(|directory| {
                    let wanted = state.watch_counts.contains_key(&directory);
                    (directory, wanted)
                })
                .collect::<Vec<_>>()
        };
        let WatcherState {
            watcher: Some(watch),
            registered,
        } = &mut *watcher
        else {
            return;
        };
        for (directory, wanted) in changes {
            if wanted && !registered.contains(&directory) {
                if watch.watch(&directory) {
                    registered.insert(directory);
                }
            } else if !wanted && registered.remove(&directory) {
                watch.unwatch(&directory);
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn unit(
        &self,
        uri: &Url,
        context: &ProjectContext,
        source_hash: u64,
        overlays: &HashMap<Url, OverlayInput>,
        cancel: &AtomicBool,
    ) -> Lookup<UnitValue> {
        self.unit_with_epoch(uri, context, source_hash, overlays, None, cancel)
    }

    pub(crate) fn unit_with_epoch(
        &self,
        uri: &Url,
        context: &ProjectContext,
        source_hash: u64,
        overlays: &HashMap<Url, OverlayInput>,
        snapshot_epoch: Option<u64>,
        cancel: &AtomicBool,
    ) -> Lookup<UnitValue> {
        self.lookup(
            Layer::Unit,
            uri,
            context,
            source_hash,
            snapshot_epoch,
            overlays,
            cancel,
            |value| match value {
                Value::Unit(unit) => Some(unit.clone()),
                _ => None,
            },
        )
    }

    /// A verified unit entry, or `None`. Never claims a slot and never waits
    /// on a computation, so callers on the request path cannot stall.
    pub(crate) fn peek_unit(
        &self,
        uri: &Url,
        context: &ProjectContext,
        content_hash: u64,
        overlays: &HashMap<Url, OverlayInput>,
    ) -> Option<Arc<UnitValue>> {
        self.peek(
            Layer::Unit,
            uri,
            context,
            content_hash,
            overlays,
            |value| match value {
                Value::Unit(unit) => Some(unit.clone()),
                _ => None,
            },
        )
    }

    /// Leases the parse of a unit hit that a request keeps in its own index,
    /// so the parse stays charged if its entry is evicted first. A parse
    /// evicted between the hit and the lease is charged at once.
    pub(crate) fn lease_unit(&self, unit: &UnitValue) -> CacheLease {
        self.lease_parse(&unit.parsed, unit.retained_bytes(), false)
            .expect("an uncached parse is leased when not required to be cached")
    }

    fn lease_parse(
        &self,
        parsed: &Arc<ParsedDocument>,
        bytes: usize,
        require_cached: bool,
    ) -> Option<CacheLease> {
        let mut state = lock(&self.inner);
        let lease = self.lease_locked(&mut state, parsed, bytes, require_cached);
        if lease.as_ref().is_some_and(|(_, charged)| *charged) {
            evict_to_budget(&mut state);
        }
        self.release(state);
        lease.map(|(lease, _)| lease)
    }

    /// Takes a lease with the state lock held. The flag says whether the
    /// lease made new outstanding bytes, which the caller evicts for.
    fn lease_locked(
        &self,
        state: &mut State,
        parsed: &Arc<ParsedDocument>,
        bytes: usize,
        require_cached: bool,
    ) -> Option<(CacheLease, bool)> {
        let payload = Arc::as_ptr(parsed) as *const () as usize;
        let mut charged = false;
        if let Some(lease) = state.leases.get_mut(&payload) {
            lease.holders += 1;
        } else {
            let cached = state.cached_parses.get(&payload).map(|(_, bytes)| *bytes);
            if cached.is_none() && require_cached {
                return None;
            }
            let bytes = cached.unwrap_or(bytes);
            state.leases.insert(
                payload,
                Lease {
                    holders: 1,
                    bytes,
                    evicted: cached.is_none(),
                    parsed: Arc::downgrade(parsed),
                },
            );
            if cached.is_none() {
                state.outstanding_bytes = state.outstanding_bytes.saturating_add(bytes);
                charged = true;
            }
        }
        Some((
            CacheLease {
                inner: self.inner.clone(),
                payload,
                _parsed: Arc::downgrade(parsed),
            },
            charged,
        ))
    }

    /// A verified unit entry read from a closed file whose stamp the recorded
    /// one proves unchanged, with the raw content hash that keys it. Lets a
    /// load skip reading the file; never claims.
    pub(crate) fn peek_unit_from_disk(
        &self,
        uri: &Url,
        context: &ProjectContext,
        stamp: &crate::workspace::DiskStamp,
        overlays: &HashMap<Url, OverlayInput>,
    ) -> Option<(Arc<UnitValue>, u64)> {
        let same_file = |unit: &UnitValue| {
            unit.disk
                .as_ref()
                .is_some_and(|disk| disk.stamp.proves_unchanged(stamp))
        };
        let key = Key {
            layer: Layer::Unit,
            uri: uri.clone(),
            fingerprint: project_context_fingerprint(context),
        };
        let content_hash = match lock(&self.inner).slots.get(&key) {
            Some(Slot::Ready(Entry {
                value: Value::Unit(unit),
                context: entry_context,
                input_hash,
                ..
            })) if **entry_context == *context && same_file(unit) => *input_hash,
            _ => return None,
        };
        let unit = self.peek_unit(uri, context, content_hash, overlays)?;
        same_file(&unit).then_some((unit, content_hash))
    }

    /// A verified interface-imports entry, or `None`, under the same rules as
    /// `peek_unit`.
    pub(crate) fn peek_interface_imports(
        &self,
        uri: &Url,
        context: &ProjectContext,
        content_hash: u64,
        overlays: &HashMap<Url, OverlayInput>,
    ) -> Option<Arc<InterfaceImportsValue>> {
        self.peek(
            Layer::Interface,
            uri,
            context,
            content_hash,
            overlays,
            |value| match value {
                Value::Interface(interface) => Some(interface.clone()),
                _ => None,
            },
        )
    }

    /// Stores a unit's interface imports without a compute claim. Their
    /// probes protect every use, so a late store cannot serve stale bindings.
    /// Returns whether the entry is new or changed, so crawls know whether a
    /// semantic-token refresh is worthwhile. Results computed before an
    /// invalidation are dropped.
    pub(crate) fn put_interface_imports(
        &self,
        uri: &Url,
        context: &ProjectContext,
        content_hash: u64,
        value: InterfaceImportsValue,
        snapshot_epoch: Option<u64>,
    ) -> bool {
        let key = Key {
            layer: Layer::Interface,
            uri: uri.clone(),
            fingerprint: project_context_fingerprint(context),
        };
        let mut state = lock(&self.inner);
        if snapshot_epoch.is_some_and(|epoch| epoch != state.invalidation_epoch) {
            crate::trace::trace!(
                "  cache rejected Interface {} from epoch {snapshot_epoch:?} at epoch {}",
                uri.path().rsplit('/').next().unwrap_or_default(),
                state.invalidation_epoch
            );
            return false;
        }
        crate::trace::trace!(
            "  cache put Interface {} under {:x}",
            uri.path().rsplit('/').next().unwrap_or_default(),
            key.fingerprint
        );
        let unchanged = matches!(
            state.slots.get(&key),
            Some(Slot::Ready(Entry {
                value: Value::Interface(old),
                context: old_context,
                input_hash,
                ..
            })) if **old_context == *context
                && *input_hash == content_hash
                && old.bindings == value.bindings
                && old.complete == value.complete
        );
        let bytes = value.retained_bytes();
        state.clock += 1;
        let entry = Entry {
            value: Value::Interface(Arc::new(value)),
            context: intern_context(&mut state, key.fingerprint, context),
            input_hash: content_hash,
            bytes,
            last_used: state.clock,
        };
        // Replace even an unchanged entry: its probes may be fresher.
        insert_ready(&mut state, key, entry);
        evict_to_budget(&mut state);
        self.release(state);
        !unchanged
    }

    fn peek<V: Probed>(
        &self,
        layer: Layer,
        uri: &Url,
        context: &ProjectContext,
        input_hash: u64,
        overlays: &HashMap<Url, OverlayInput>,
        extract: impl Fn(&Value) -> Option<Arc<V>>,
    ) -> Option<Arc<V>> {
        let key = Key {
            layer,
            uri: uri.clone(),
            fingerprint: project_context_fingerprint(context),
        };
        let value = {
            let state = lock(&self.inner);
            match state.slots.get(&key) {
                Some(Slot::Ready(entry))
                    if *entry.context == *context && entry.input_hash == input_hash =>
                {
                    extract(&entry.value)?
                }
                slot => {
                    crate::trace::count(|counters| counters.peek_misses += 1);
                    if crate::trace::enabled() {
                        let reason = match slot {
                            None => {
                                let others = state
                                    .by_uri
                                    .get(uri)
                                    .into_iter()
                                    .flatten()
                                    .filter(|other| other.layer == layer)
                                    .map(|other| format!("{:x}", other.fingerprint))
                                    .collect::<Vec<_>>();
                                format!(
                                    "absent; same layer under fingerprints [{}]",
                                    others.join(", ")
                                )
                            }
                            Some(Slot::Computing { .. }) => "computing".to_string(),
                            Some(Slot::Ready(entry)) if entry.input_hash != input_hash => {
                                format!("hash {:x} != {:x}", entry.input_hash, input_hash)
                            }
                            Some(Slot::Ready(_)) => "context differs".to_string(),
                        };
                        crate::trace::trace!(
                            "  peek miss {layer:?} {} under {:x}: {reason}",
                            uri.path().rsplit('/').next().unwrap_or_default(),
                            key.fingerprint
                        );
                    }
                    return None;
                }
            }
        };
        let probing = std::time::Instant::now();
        let held = probes_hold(value.probes(), overlays);
        crate::trace::count(|counters| {
            counters.probes += probing.elapsed();
            if held {
                counters.peek_hits += 1;
            } else {
                counters.peek_misses += 1;
            }
        });
        if !held {
            crate::trace::trace!(
                "  peek miss {layer:?} {}: probes failed",
                uri.path().rsplit('/').next().unwrap_or_default()
            );
            return None;
        }
        let mut state = lock(&self.inner);
        if let Some(Slot::Ready(entry)) = state.slots.get(&key)
            && extract(&entry.value).is_some_and(|current| Arc::ptr_eq(&current, &value))
        {
            touch(&mut state, &key);
        }
        Some(value)
    }

    /// Declared unit name of a cached parse of exactly these bytes, under any
    /// project context. The declared name does not depend on project settings.
    pub(crate) fn declared_unit_name(
        &self,
        uri: &Url,
        content_hash: u64,
    ) -> Option<Option<String>> {
        let state = lock(&self.inner);
        let keys = state.by_uri.get(uri)?;
        keys.iter().find_map(|key| match state.slots.get(key) {
            Some(Slot::Ready(Entry {
                value: Value::Unit(unit),
                input_hash,
                ..
            })) if *input_hash == content_hash => {
                Some(Some(unit.parsed.unit_name().to_string()).filter(|name| !name.is_empty()))
            }
            _ => None,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn store_unit(
        &self,
        claim: Claim,
        value: UnitValue,
        bytes: usize,
        cancel: &AtomicBool,
    ) {
        self.store(claim, Value::Unit(Arc::new(value)), bytes, cancel, false);
    }

    /// `store_unit` for a request that keeps the parse in its own index: the
    /// stored parse is leased under the same lock, before eviction runs, so
    /// no eviction can fall between the store and the lease (l4d-10). `None`
    /// when the store was discarded.
    pub(crate) fn store_unit_leased(
        &self,
        claim: Claim,
        value: UnitValue,
        bytes: usize,
        cancel: &AtomicBool,
    ) -> Option<CacheLease> {
        self.store(claim, Value::Unit(Arc::new(value)), bytes, cancel, true)
    }

    pub(crate) fn imports(
        &self,
        importer: &Url,
        context: &ProjectContext,
        input_hash: u64,
        overlays: &HashMap<Url, OverlayInput>,
        cancel: &AtomicBool,
    ) -> Lookup<ImportValue> {
        self.imports_with_epoch(importer, context, input_hash, overlays, None, cancel)
    }

    pub(crate) fn imports_with_epoch(
        &self,
        importer: &Url,
        context: &ProjectContext,
        input_hash: u64,
        overlays: &HashMap<Url, OverlayInput>,
        snapshot_epoch: Option<u64>,
        cancel: &AtomicBool,
    ) -> Lookup<ImportValue> {
        self.lookup(
            Layer::Import,
            importer,
            context,
            input_hash,
            snapshot_epoch,
            overlays,
            cancel,
            |value| match value {
                Value::Import(imports) => Some(imports.clone()),
                _ => None,
            },
        )
    }

    pub(crate) fn store_imports(
        &self,
        claim: Claim,
        value: ImportValue,
        bytes: usize,
        cancel: &AtomicBool,
    ) {
        self.store(claim, Value::Import(Arc::new(value)), bytes, cancel, false);
    }

    #[allow(clippy::too_many_arguments)]
    fn lookup<V: Probed>(
        &self,
        layer: Layer,
        uri: &Url,
        context: &ProjectContext,
        input_hash: u64,
        snapshot_epoch: Option<u64>,
        overlays: &HashMap<Url, OverlayInput>,
        cancel: &AtomicBool,
        extract: impl Fn(&Value) -> Option<Arc<V>>,
    ) -> Lookup<V> {
        let key = Key {
            layer,
            uri: uri.clone(),
            fingerprint: project_context_fingerprint(context),
        };
        let role = CLAIM_ROLE.with(|cell| cell.borrow().clone());
        let thread = std::thread::current().id();
        let mut wait_started: Option<std::time::Instant> = None;
        let mut taking_over = false;
        #[cfg(feature = "test-support")]
        let mut wait_marked = false;
        let mut state = lock(&self.inner);
        loop {
            match state.slots.get(&key) {
                Some(Slot::Computing {
                    generation,
                    input_hash: claimed_input,
                    owner,
                    ..
                }) => {
                    let waiting_since = *wait_started.get_or_insert_with(std::time::Instant::now);
                    if cancel.load(Ordering::Relaxed) {
                        return Lookup::Cancelled;
                    }
                    // The owner's work was cancelled, so its result will be
                    // discarded, or it yields to this interactive work: take
                    // the slot over.
                    if *claimed_input == input_hash
                        && (owner.role.cancelled()
                            || (role.interactive && owner.role.yields_to_interactive))
                    {
                        taking_over = true;
                        break;
                    }
                    // Claims of threads without a role (the main thread, which
                    // parses opened documents) cannot be cancelled or taken
                    // over, so waits for them are bounded.
                    let bounded_wait = role
                        .roleless_claim_wait
                        .filter(|_| owner.role.cancel.is_none())
                        .map(|limit| limit.saturating_sub(waiting_since.elapsed()));
                    // The claim computes another revision of the input, this
                    // thread may not wait longer for a role-less owner, or
                    // waiting would close a cycle of waits: compute without
                    // caching instead of waiting on it.
                    if *claimed_input != input_hash
                        || bounded_wait.is_some_and(|remaining| remaining.is_zero())
                        || wait_closes_cycle(&state, thread, owner.thread)
                    {
                        let generation = *generation;
                        crate::trace::count(|counters| {
                            counters.detached += 1;
                            counters.wait +=
                                wait_started.map_or(Duration::ZERO, |since| since.elapsed());
                        });
                        return Lookup::Compute(Claim {
                            inner: self.inner.clone(),
                            key: key.clone(),
                            generation,
                            claim: NEXT_CLAIM.fetch_add(1, Ordering::Relaxed),
                            context: Arc::new(context.clone()),
                            input_hash,
                            invalidation_epoch: snapshot_epoch.unwrap_or(state.invalidation_epoch),
                            detached: true,
                            started: std::time::Instant::now(),
                        });
                    }
                    // One mark per wait slice, so tests can see a wait last.
                    #[cfg(feature = "test-support")]
                    if role.interactive && !wait_marked {
                        // The marker is a file: write it without the lock.
                        wait_marked = true;
                        drop(state);
                        mark_interactive_claim_wait_for_tests();
                        state = lock(&self.inner);
                        continue;
                    }
                    state.waiting.insert(thread, key.clone());
                    #[cfg(test)]
                    self.signal_waiter_hook();
                    let slice =
                        bounded_wait.map_or(WAIT_SLICE, |remaining| remaining.min(WAIT_SLICE));
                    state = wait_timeout_recovering_poison_for(&self.inner, state, slice).0;
                    state.waiting.remove(&thread);
                    #[cfg(feature = "test-support")]
                    {
                        wait_marked = false;
                    }
                }
                Some(Slot::Ready(entry))
                    if *entry.context == *context && entry.input_hash == input_hash =>
                {
                    let Some(value) = extract(&entry.value) else {
                        break;
                    };
                    drop(state);
                    let probing = std::time::Instant::now();
                    let probes_hold = probes_hold(value.probes(), overlays);
                    crate::trace::count(|counters| counters.probes += probing.elapsed());
                    #[cfg(test)]
                    if probes_hold {
                        self.run_validation_hook();
                    }
                    state = lock(&self.inner);
                    let same_entry = match state.slots.get(&key) {
                        Some(Slot::Ready(entry))
                            if *entry.context == *context && entry.input_hash == input_hash =>
                        {
                            extract(&entry.value)
                                .is_some_and(|current| Arc::ptr_eq(&current, &value))
                        }
                        _ => false,
                    };
                    if !same_entry {
                        continue;
                    }
                    if probes_hold {
                        touch(&mut state, &key);
                        crate::trace::count(|counters| {
                            counters.hits += 1;
                            counters.wait +=
                                wait_started.map_or(Duration::ZERO, |since| since.elapsed());
                        });
                        return Lookup::Hit(value);
                    }
                    break;
                }
                _ => break,
            }
        }
        crate::trace::count(|counters| {
            counters.computes += 1;
            counters.wait += wait_started.map_or(Duration::ZERO, |since| since.elapsed());
        });
        if crate::trace::enabled() {
            let reason = match state.slots.get(&key) {
                None => "absent".to_string(),
                Some(Slot::Ready(entry)) if entry.input_hash != input_hash => {
                    "input changed".to_string()
                }
                Some(Slot::Ready(entry)) if *entry.context != *context => {
                    format!(
                        "context differs in {}",
                        context_differences(&entry.context, context)
                    )
                }
                Some(Slot::Ready(_)) => "probes failed".to_string(),
                Some(Slot::Computing { .. }) if taking_over => "taken over".to_string(),
                Some(Slot::Computing { .. }) => "computing".to_string(),
            };
            crate::trace::trace!(
                "  cache compute {layer:?} {} under {:x}: {reason}",
                uri.path().rsplit('/').next().unwrap_or_default(),
                key.fingerprint
            );
        }
        remove_ready(&mut state, &key);
        let generation = state.generation;
        let claim = NEXT_CLAIM.fetch_add(1, Ordering::Relaxed);
        state.slots.insert(
            key.clone(),
            Slot::Computing {
                generation,
                input_hash,
                claim,
                owner: ClaimOwner { thread, role },
            },
        );
        let invalidation_epoch = snapshot_epoch.unwrap_or(state.invalidation_epoch);
        let context = intern_context(&mut state, key.fingerprint, context);
        self.release(state);
        HELD_CLAIMS.with(|held| held.set(held.get() + 1));
        Lookup::Compute(Claim {
            inner: self.inner.clone(),
            key,
            generation,
            claim,
            context,
            input_hash,
            invalidation_epoch,
            detached: false,
            started: std::time::Instant::now(),
        })
    }
}

/// Names the fields in which two contexts with one fingerprint differ.
fn context_differences(left: &ProjectContext, right: &ProjectContext) -> String {
    let mut fields = Vec::new();
    if left.discovery_complete() != right.discovery_complete() {
        fields.push("discovery_complete".to_string());
    }
    if left.binding_metadata_complete() != right.binding_metadata_complete() {
        fields.push("binding_metadata_complete".to_string());
    }
    if left.path_issues() != right.path_issues() {
        fields.push("path_issues".to_string());
    }
    if left.project_file() != right.project_file() {
        fields.push("project_file".to_string());
    }
    if left.installation_evidence() != right.installation_evidence() {
        fields.push("installation_evidence".to_string());
    }
    if left.installation_selection() != right.installation_selection() {
        fields.push("installation_selection".to_string());
    }
    if left.main_source() != right.main_source() {
        fields.push("main_source".to_string());
    }
    if left.search_paths() != right.search_paths() {
        fields.push("search_paths".to_string());
    }
    if left.search_path_entries() != right.search_path_entries() {
        fields.push("search_path_entries".to_string());
    }
    if left.browsing_path_entries() != right.browsing_path_entries() {
        fields.push("browsing_path_entries".to_string());
    }
    if left.debug_dcu_path_entries() != right.debug_dcu_path_entries() {
        fields.push("debug_dcu_path_entries".to_string());
    }
    if left.main_source_entry() != right.main_source_entry() {
        fields.push("main_source_entry".to_string());
    }
    if left.explicit_unit_entries() != right.explicit_unit_entries() {
        fields.push("explicit_unit_entries".to_string());
    }
    if left.include_paths() != right.include_paths() {
        fields.push("include_paths".to_string());
    }
    if left.include_path_entries() != right.include_path_entries() {
        fields.push("include_path_entries".to_string());
    }
    if left.explicit_units() != right.explicit_units() {
        fields.push("explicit_units".to_string());
    }
    if left.unit_namespaces() != right.unit_namespaces() {
        fields.push("unit_namespaces".to_string());
    }
    if left.unit_aliases() != right.unit_aliases() {
        fields.push("unit_aliases".to_string());
    }
    if left.defines() != right.defines() {
        fields.push("defines".to_string());
    }
    if left.conditional_context() != right.conditional_context() {
        fields.push("conditional_context".to_string());
    }
    if left.conditional_closure() != right.conditional_closure() {
        fields.push("conditional_closure".to_string());
    }
    if left.library_conditional_context() != right.library_conditional_context() {
        fields.push("library_conditional_context".to_string());
    }
    if left.project_source_roots() != right.project_source_roots() {
        fields.push("project_source_roots".to_string());
    }
    if left.project_unit_directories() != right.project_unit_directories() {
        fields.push("project_unit_directories".to_string());
    }
    if left.unit_directories_unknown() != right.unit_directories_unknown() {
        fields.push("unit_directories_unknown".to_string());
    }
    if left.config() != right.config() {
        fields.push("config".to_string());
    }
    if left.platform() != right.platform() {
        fields.push("platform".to_string());
    }
    if left.config_selection() != right.config_selection() {
        fields.push("config_selection".to_string());
    }
    if left.platform_selection() != right.platform_selection() {
        fields.push("platform_selection".to_string());
    }
    if left.overrides() != right.overrides() {
        fields.push("overrides".to_string());
    }
    if left.read_policy() != right.read_policy() {
        fields.push("read_policy".to_string());
    }
    if left.packages() != right.packages() {
        fields.push("packages".to_string());
    }
    if left.installation_config_files() != right.installation_config_files() {
        fields.push("installation_config_files".to_string());
    }
    if left.override_error() != right.override_error() {
        fields.push("override_error".to_string());
    }
    fields.join(", ")
}

const LAYERS: [Layer; 3] = [Layer::Unit, Layer::Import, Layer::Interface];

/// Removes `pin` from every owner's set, and drops owners left empty.
fn unpin_key(state: &mut State, pin: &(Url, u64)) {
    let mut removed = 0usize;
    state.pins.retain(|_, keys| {
        removed += usize::from(keys.remove(pin));
        !keys.is_empty()
    });
    for _ in 0..removed {
        remove_pin(state, pin);
    }
}

/// Whether some owner pins `(uri, fingerprint)`.
fn pin_counted(state: &State, (uri, fingerprint): &(Url, u64)) -> bool {
    state
        .pin_counts
        .get(uri)
        .is_some_and(|fingerprints| fingerprints.contains_key(fingerprint))
}

/// The bytes of the ready entries a pin of `(uri, fingerprint)` covers.
fn pin_key_bytes(state: &State, (uri, fingerprint): &(Url, u64)) -> usize {
    LAYERS
        .iter()
        .filter_map(|&layer| {
            match state.slots.get(&Key {
                layer,
                uri: uri.clone(),
                fingerprint: *fingerprint,
            }) {
                Some(Slot::Ready(entry)) => Some(entry.bytes),
                _ => None,
            }
        })
        .fold(0usize, usize::saturating_add)
}

fn is_pinned(state: &State, key: &Key) -> bool {
    state
        .pin_counts
        .get(&key.uri)
        .is_some_and(|fingerprints| fingerprints.contains_key(&key.fingerprint))
}

fn value_probes(value: &Value) -> &[Probe] {
    match value {
        Value::Unit(unit) => &unit.probes,
        Value::Import(imports) => &imports.probes,
        Value::Interface(interface) => &interface.probes,
    }
}

/// The paths whose change invalidates the entry: `true` marks a stamp or
/// content probe, which a change to a child also invalidates.
fn indexed_paths(key: &Key, entry: &Entry) -> Vec<(bool, PathBuf)> {
    let mut paths = Vec::new();
    if let Ok(own) = key.uri.to_file_path() {
        paths.push((false, own));
    }
    for probe in value_probes(&entry.value) {
        match probe {
            Probe::Stamp { path, .. } | Probe::Content { path, .. } => {
                paths.push((true, path.clone()));
            }
            Probe::Overlay { uri, .. } => {
                if let Ok(path) = uri.to_file_path() {
                    paths.push((false, path));
                }
            }
        }
    }
    paths
}

fn index_insert<K: std::hash::Hash + Eq>(map: &mut HashMap<K, HashSet<Key>>, at: K, key: &Key) {
    map.entry(at).or_default().insert(key.clone());
}

fn index_remove<K: std::hash::Hash + Eq>(map: &mut HashMap<K, HashSet<Key>>, at: &K, key: &Key) {
    if let Some(keys) = map.get_mut(at) {
        keys.remove(key);
        if keys.is_empty() {
            map.remove(at);
        }
    }
}

/// Stores a ready entry, replacing any ready entry under the key, and keeps
/// every index in step with it.
fn insert_ready(state: &mut State, key: Key, entry: Entry) {
    remove_ready(state, &key);
    state.bytes = state.bytes.saturating_add(entry.bytes);
    if let Some(address) = parse_address(&entry.value) {
        hold_parse(state, address, entry.bytes);
    }
    // A pinned key whose entry arrives after its pin keeps the pins within
    // their cap (l4d-axsn): over it, the key is unpinned for every owner.
    if is_pinned(state, &key)
        && state.pinned_bytes.saturating_add(entry.bytes) > state.max_bytes / PIN_BUDGET_SHARE
    {
        unpin_key(state, &(key.uri.clone(), key.fingerprint));
    }
    if is_pinned(state, &key) {
        state.pinned_bytes = state.pinned_bytes.saturating_add(entry.bytes);
    } else {
        state.lru.insert(entry.last_used, key.clone());
    }
    for (observed, path) in indexed_paths(&key, &entry) {
        let map = if observed {
            &mut state.observed
        } else {
            &mut state.exact
        };
        index_insert(map, path, &key);
    }
    index_insert(&mut state.by_uri, key.uri.clone(), &key);
    if let Value::Import(imports) = &entry.value {
        for directory in &imports.watch_dirs {
            acquire_watch(state, directory);
        }
    }
    state.slots.insert(key, Slot::Ready(entry));
}

#[derive(Debug)]
struct Lease {
    holders: usize,
    bytes: usize,
    /// Whether no cache entry holds the parse, so `bytes` are outstanding.
    evicted: bool,
    /// Outstanding bytes are charged only while the parse is alive.
    parsed: std::sync::Weak<ParsedDocument>,
}

/// A request's hold on a cached parse it put into its own index. While a
/// lease exists, no entry holds the parse and the parse is still alive, the
/// entry's charge stays in the budget as outstanding bytes. Transient hits
/// are not leased.
pub(crate) struct CacheLease {
    inner: Arc<Inner>,
    payload: usize,
    /// Keeps the address from being reused while the lease exists, without
    /// keeping the parse alive once the request's index drops it.
    _parsed: std::sync::Weak<ParsedDocument>,
}

impl std::fmt::Debug for CacheLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CacheLease")
            .field("payload", &self.payload)
            .finish_non_exhaustive()
    }
}

impl Drop for CacheLease {
    fn drop(&mut self) {
        let mut state = lock(&self.inner);
        let Some(lease) = state.leases.get_mut(&self.payload) else {
            return;
        };
        lease.holders -= 1;
        if lease.holders > 0 {
            return;
        }
        let Some(lease) = state.leases.remove(&self.payload) else {
            return;
        };
        if lease.evicted {
            state.outstanding_bytes = state.outstanding_bytes.saturating_sub(lease.bytes);
        }
    }
}

fn parse_address(value: &Value) -> Option<usize> {
    match value {
        Value::Unit(unit) => Some(Arc::as_ptr(&unit.parsed) as *const () as usize),
        _ => None,
    }
}

/// Records that an entry holds a parse; a leased parse stops being
/// outstanding.
fn hold_parse(state: &mut State, address: usize, bytes: usize) {
    let (refs, _) = state.cached_parses.entry(address).or_insert((0, bytes));
    *refs += 1;
    if let Some(lease) = state.leases.get_mut(&address)
        && lease.evicted
    {
        lease.evicted = false;
        state.outstanding_bytes = state.outstanding_bytes.saturating_sub(lease.bytes);
    }
}

/// Records that an entry released a parse; a leased parse no entry holds
/// becomes outstanding.
fn release_parse(state: &mut State, address: usize) {
    let Some((refs, _)) = state.cached_parses.get_mut(&address) else {
        return;
    };
    *refs -= 1;
    if *refs > 0 {
        return;
    }
    state.cached_parses.remove(&address);
    if let Some(lease) = state.leases.get_mut(&address)
        && !lease.evicted
    {
        lease.evicted = true;
        state.outstanding_bytes = state.outstanding_bytes.saturating_add(lease.bytes);
    }
}

/// The shared copy of `context`, so entries of one project hold one copy.
fn intern_context(
    state: &mut State,
    fingerprint: u64,
    context: &ProjectContext,
) -> Arc<ProjectContext> {
    if let Some(shared) = state.contexts.get(&fingerprint)
        && **shared == *context
    {
        return shared.clone();
    }
    let shared = Arc::new(context.clone());
    state.contexts.insert(fingerprint, shared.clone());
    shared
}

/// Forgets the shared context once no entry or claim holds it.
fn release_context(state: &mut State, fingerprint: u64) {
    if state
        .contexts
        .get(&fingerprint)
        .is_some_and(|shared| Arc::strong_count(shared) == 1)
    {
        state.contexts.remove(&fingerprint);
    }
}

/// Removes a ready entry and its index records. A computing slot stays.
fn remove_ready(state: &mut State, key: &Key) -> bool {
    if !matches!(state.slots.get(key), Some(Slot::Ready(_))) {
        return false;
    }
    let Some(Slot::Ready(entry)) = state.slots.remove(key) else {
        return false;
    };
    state.bytes = state.bytes.saturating_sub(entry.bytes);
    if is_pinned(state, key) {
        state.pinned_bytes = state.pinned_bytes.saturating_sub(entry.bytes);
    } else {
        state.lru.remove(&entry.last_used);
    }
    for (observed, path) in indexed_paths(key, &entry) {
        let map = if observed {
            &mut state.observed
        } else {
            &mut state.exact
        };
        index_remove(map, &path, key);
    }
    index_remove(&mut state.by_uri, &key.uri, key);
    release_watches(state, &entry);
    if let Some(address) = parse_address(&entry.value) {
        release_parse(state, address);
    }
    drop(entry);
    release_context(state, key.fingerprint);
    true
}

/// Marks a ready entry as the most recently used.
fn touch(state: &mut State, key: &Key) {
    state.clock += 1;
    let clock = state.clock;
    let pinned = is_pinned(state, key);
    let Some(Slot::Ready(entry)) = state.slots.get_mut(key) else {
        return;
    };
    let previous = std::mem::replace(&mut entry.last_used, clock);
    if !pinned {
        state.lru.remove(&previous);
        state.lru.insert(clock, key.clone());
    }
}

/// Moves the ready entries of a `(uri, fingerprint)` between the LRU order
/// and the pinned total when its first owner pins it or its last unpins it.
fn set_pinned(state: &mut State, uri: &Url, fingerprint: u64, pinned: bool) {
    for layer in LAYERS {
        let key = Key {
            layer,
            uri: uri.clone(),
            fingerprint,
        };
        let Some(Slot::Ready(entry)) = state.slots.get(&key) else {
            continue;
        };
        let (bytes, last_used) = (entry.bytes, entry.last_used);
        if pinned {
            state.lru.remove(&last_used);
            state.pinned_bytes = state.pinned_bytes.saturating_add(bytes);
        } else {
            state.lru.insert(last_used, key);
            state.pinned_bytes = state.pinned_bytes.saturating_sub(bytes);
        }
    }
}

fn add_pin(state: &mut State, (uri, fingerprint): &(Url, u64)) {
    let count = state
        .pin_counts
        .entry(uri.clone())
        .or_default()
        .entry(*fingerprint)
        .or_insert(0);
    *count += 1;
    if *count == 1 {
        set_pinned(state, uri, *fingerprint, true);
    }
}

fn remove_pin(state: &mut State, (uri, fingerprint): &(Url, u64)) {
    let Some(fingerprints) = state.pin_counts.get_mut(uri) else {
        return;
    };
    let Some(count) = fingerprints.get_mut(fingerprint) else {
        return;
    };
    *count -= 1;
    if *count > 0 {
        return;
    }
    fingerprints.remove(fingerprint);
    if fingerprints.is_empty() {
        state.pin_counts.remove(uri);
    }
    set_pinned(state, uri, *fingerprint, false);
}

/// Stops charging leased parses that nothing holds any more.
fn prune_dead_leases(state: &mut State) {
    let mut released = 0usize;
    state.leases.retain(|_, lease| {
        let alive = lease.parsed.strong_count() > 0;
        if !alive && lease.evicted {
            released = released.saturating_add(lease.bytes);
        }
        alive
    });
    state.outstanding_bytes = state.outstanding_bytes.saturating_sub(released);
}

fn evict_to_budget(state: &mut State) {
    prune_dead_leases(state);
    while state
        .bytes
        .saturating_add(state.outstanding_bytes)
        .saturating_add(budgeted_reserved_bytes(state))
        > state.max_bytes
    {
        if !evict_oldest(state) {
            return;
        }
    }
}

fn acquire_watch(state: &mut State, directory: &Path) {
    let count = state
        .watch_counts
        .entry(directory.to_path_buf())
        .or_insert(0);
    *count += 1;
    if *count == 1 {
        state.watch_changes.push(directory.to_path_buf());
    }
}

fn release_watches(state: &mut State, entry: &Entry) {
    let Value::Import(imports) = &entry.value else {
        return;
    };
    for directory in &imports.watch_dirs {
        release_watch(state, directory);
    }
}

fn release_watch(state: &mut State, directory: &Path) {
    let Some(count) = state.watch_counts.get_mut(directory) else {
        return;
    };
    *count -= 1;
    if *count == 0 {
        state.watch_counts.remove(directory);
        state.watch_changes.push(directory.to_path_buf());
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl ProjectCache {
    fn store(
        &self,
        claim: Claim,
        value: Value,
        bytes: usize,
        cancel: &AtomicBool,
        lease: bool,
    ) -> Option<CacheLease> {
        let mut state = lock(&self.inner);
        let current = !claim.detached
            && !cancel.load(Ordering::Relaxed)
            && claim.invalidation_epoch == state.invalidation_epoch
            && matches!(state.slots.get(&claim.key),
            Some(Slot::Computing { generation, claim: id, .. })
                if *generation == claim.generation
                    && claim.generation == state.generation
                    && *id == claim.claim);
        crate::trace::trace!(
            "  cache {} {:?} {} after {:.1} ms",
            if current { "stored" } else { "discarded" },
            claim.key.layer,
            claim.key.uri.path().rsplit('/').next().unwrap_or_default(),
            claim.started.elapsed().as_secs_f64() * 1000.0
        );
        let mut leased = None;
        if current {
            let parsed = match (&value, lease) {
                (Value::Unit(unit), true) => Some(unit.parsed.clone()),
                _ => None,
            };
            state.clock += 1;
            let entry = Entry {
                value,
                context: claim.context.clone(),
                input_hash: claim.input_hash,
                bytes,
                last_used: state.clock,
            };
            insert_ready(&mut state, claim.key.clone(), entry);
            // The entry holds the parse now, so the lease is not outstanding
            // unless the eviction below removes the entry itself.
            leased = parsed.and_then(|parsed| {
                self.lease_locked(&mut state, &parsed, bytes, true)
                    .map(|(lease, _)| lease)
            });
            evict_to_budget(&mut state);
        }
        self.release(state);
        drop(claim); // Removes the slot only when it is still Computing.
        leased
    }

    /// Records that a snapshot for `uri` found part of its interface closure
    /// uncached. The server drains these into warmer crawls.
    fn request_closure_crawl(&self, uri: &Url) {
        let mut state = lock(&self.inner);
        if state.closure_crawl_requests.len() < MAX_CLOSURE_CRAWL_REQUESTS
            && !state.closure_crawl_requests.contains(uri)
        {
            state.closure_crawl_requests.push(uri.clone());
        }
    }

    /// Records the units a snapshot walk for `root` found uncached, and
    /// requests a crawl only when one of them was not missed by the root's
    /// previous walk. A unit that can never be cached then stops requesting
    /// crawls, while one evicted since the last walk requests another.
    pub(crate) fn report_closure_misses(&self, root: &Url, missed: HashSet<Url>) {
        let mut state = lock(&self.inner);
        let new_miss = match state.closure_misses.get(root) {
            Some(previous) => missed.iter().any(|uri| !previous.contains(uri)),
            None => !missed.is_empty(),
        };
        if state.closure_misses.len() >= MAX_CLOSURE_MISS_ROOTS
            && !state.closure_misses.contains_key(root)
        {
            state.closure_misses.clear();
        }
        if new_miss {
            let previous = state.closure_misses.get(root);
            let fresh = missed
                .iter()
                .filter(|uri| previous.is_none_or(|previous| !previous.contains(uri)))
                .map(|uri| {
                    uri.path()
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .to_string()
                })
                .collect::<Vec<_>>();
            crate::trace::trace!(
                "closure misses for {root}: {} uncached, new: {}",
                missed.len(),
                crate::trace::short(fresh.join(", "), 400)
            );
        }
        state.closure_misses.insert(root.clone(), missed);
        drop(state);
        if new_miss {
            self.request_closure_crawl(root);
        }
    }

    /// Makes the root's next walk with misses request a crawl, because the
    /// crawl that followed its last report did not settle the closure.
    pub(crate) fn forget_closure_misses(&self, root: &Url) {
        lock(&self.inner).closure_misses.remove(root);
    }

    /// The uncached closure units the root's last walk reported.
    pub(crate) fn closure_misses(&self, root: &Url) -> HashSet<Url> {
        lock(&self.inner)
            .closure_misses
            .get(root)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether anything invalidated since `epoch` touched one of `units`: its
    /// file, or an entry of its URI. An epoch the log no longer covers counts
    /// as invalidated.
    pub(crate) fn units_invalidated_since(&self, epoch: u64, units: &HashSet<Url>) -> bool {
        let unit_paths = units
            .iter()
            .filter_map(|unit| unit.to_file_path().ok())
            .collect::<HashSet<_>>();
        let state = lock(&self.inner);
        if epoch == state.invalidation_epoch {
            return false;
        }
        if epoch < state.invalidation_log_floor {
            return true;
        }
        state
            .invalidation_log
            .iter()
            .filter(|record| record.epoch > epoch)
            .any(|record| {
                record.crowded
                    || unit_paths.contains(&record.path)
                    || record.affected.iter().any(|uri| units.contains(uri))
            })
    }

    pub(crate) fn set_artifact_roots(&self, roots: Vec<PathBuf>) {
        lock(&self.inner).artifact_roots = roots;
    }

    pub(crate) fn take_closure_crawl_requests(&self) -> Vec<Url> {
        std::mem::take(&mut lock(&self.inner).closure_crawl_requests)
    }

    pub(crate) fn pin(&self, owner: &Url, keys: Vec<(Url, u64)>) {
        let mut state = lock(&self.inner);
        let keys = keys.into_iter().collect::<HashSet<_>>();
        // Count the new set first so keys in both sets stay pinned throughout.
        for key in &keys {
            add_pin(&mut state, key);
        }
        if let Some(previous) = state.pins.insert(owner.clone(), keys) {
            for key in &previous {
                remove_pin(&mut state, key);
            }
        }
    }

    /// `pin` within half of `maxCacheBytes` (l4d-axsn): while `keys` do not
    /// fit, the owners in `oldest_first` (least recently opened first;
    /// `owner` is skipped) are unpinned whole, then `owner`'s keys are pinned
    /// in order, each only if it still fits. Returns the owners unpinned.
    pub(crate) fn pin_capped(
        &self,
        owner: &Url,
        keys: Vec<(Url, u64)>,
        oldest_first: &[Url],
    ) -> Vec<Url> {
        let mut state = lock(&self.inner);
        let cap = state.max_bytes / PIN_BUDGET_SHARE;
        if let Some(previous) = state.pins.remove(owner) {
            for key in &previous {
                remove_pin(&mut state, key);
            }
        }
        let mut seen = HashSet::new();
        let keys = keys
            .into_iter()
            .filter(|key| seen.insert(key.clone()))
            .collect::<Vec<_>>();
        let added = |state: &State, key: &(Url, u64)| {
            if pin_counted(state, key) {
                0
            } else {
                pin_key_bytes(state, key)
            }
        };
        let mut unpinned = Vec::new();
        for other in oldest_first.iter().filter(|other| *other != owner) {
            let wanted = keys.iter().fold(0usize, |total, key| {
                total.saturating_add(added(&state, key))
            });
            if state.pinned_bytes.saturating_add(wanted) <= cap {
                break;
            }
            if let Some(previous) = state.pins.remove(other) {
                for key in &previous {
                    remove_pin(&mut state, key);
                }
                unpinned.push(other.clone());
            }
        }
        let mut kept = HashSet::new();
        for key in keys {
            if state.pinned_bytes.saturating_add(added(&state, &key)) > cap {
                continue;
            }
            add_pin(&mut state, &key);
            kept.insert(key);
        }
        if !kept.is_empty() {
            state.pins.insert(owner.clone(), kept);
        }
        evict_to_budget(&mut state);
        self.release(state);
        unpinned
    }

    #[cfg(test)]
    pub(crate) fn pinned_owners_for_test(&self) -> Vec<Url> {
        let mut owners = lock(&self.inner).pins.keys().cloned().collect::<Vec<_>>();
        owners.sort();
        owners
    }

    pub(crate) fn max_bytes(&self) -> usize {
        lock(&self.inner).max_bytes
    }

    pub(crate) fn unpin(&self, owner: &Url) {
        let mut state = lock(&self.inner);
        if let Some(previous) = state.pins.remove(owner) {
            for key in &previous {
                remove_pin(&mut state, key);
            }
        }
        evict_to_budget(&mut state);
        self.release(state);
    }

    pub(crate) fn clear_pins(&self) {
        let mut state = lock(&self.inner);
        for (_, previous) in std::mem::take(&mut state.pins) {
            for key in &previous {
                remove_pin(&mut state, key);
            }
        }
        evict_to_budget(&mut state);
        self.release(state);
    }

    /// Evicts every entry that depends on `path` and returns the affected URIs.
    pub(crate) fn invalidate_path(&self, path: &Path) -> Vec<Url> {
        self.invalidate_path_with_parent(path, true)
    }

    /// A content or metadata write does not change the parent's directory
    /// listing. Keep entries that only observed that listing, while evicting
    /// exact source, include, and metadata dependencies regardless of extension.
    pub(crate) fn invalidate_file_contents(&self, path: &Path) -> Vec<Url> {
        self.invalidate_path_with_parent(path, false)
    }

    fn invalidate_path_with_parent(&self, path: &Path, include_parent: bool) -> Vec<Url> {
        self.forget_crawl_published(path, include_parent);
        let mut state = lock(&self.inner);
        let artifact = is_artifact_path(path, &state.artifact_roots);
        let parent = path.parent().filter(|_| include_parent && !artifact);
        // Only a build product or tool state file that no entry observes can
        // be skipped. No analysis reads such a file as a source (an include
        // named like one is a probe, and so observed, once an entry has it),
        // and creating or removing it does not change what an observed
        // listing means. Any other path stales the epoch, since a
        // computation in flight may read it before it registers the
        // dependency.
        let relevant =
            !artifact || state.exact.contains_key(path) || state.observed.contains_key(path);
        if relevant {
            state.invalidation_epoch = state.invalidation_epoch.wrapping_add(1);
        }
        crate::trace::trace!(
            "cache invalidate {} (parent: {include_parent}, relevant: {relevant}) -> epoch {}",
            path.display(),
            state.invalidation_epoch
        );
        let doomed = state
            .exact
            .get(path)
            .into_iter()
            .chain(state.observed.get(path))
            .chain(parent.and_then(|parent| state.observed.get(parent)))
            .flatten()
            .cloned()
            .collect::<HashSet<_>>();
        let mut affected = Vec::new();
        let mut seen = HashSet::new();
        for key in doomed {
            #[cfg(test)]
            note_examined(&state);
            if remove_ready(&mut state, &key) && seen.insert(key.uri.clone()) {
                affected.push(key.uri);
            }
        }
        if relevant {
            let epoch = state.invalidation_epoch;
            let crowded = affected.len() > MAX_AFFECTED_PER_RECORD;
            state.invalidation_log.push_back(InvalidationRecord {
                epoch,
                path: path.to_path_buf(),
                affected: if crowded {
                    Vec::new()
                } else {
                    affected.clone()
                },
                crowded,
            });
            if state.invalidation_log.len() > MAX_INVALIDATION_LOG
                && let Some(dropped) = state.invalidation_log.pop_front()
            {
                state.invalidation_log_floor = dropped.epoch;
            }
        }
        self.release(state);
        self.drop_witnesses(path, include_parent);
        let removed = self.catalogues().invalidate(path, include_parent);
        self.release_catalogues(removed);
        {
            let mut state = lock(&self.inner);
            let freed = state.provider_graphs.invalidate(path, include_parent);
            release_graph_bytes(&mut state, freed);
            self.release(state);
        }
        self.invalidate_index(path, include_parent);
        if !affected.is_empty() {
            crate::trace::trace!("  evicted entries of {} uris", affected.len());
        }
        affected
    }

    /// Watcher overflow loses change details, so discard every verified cache
    /// entry and reject claims based on snapshots from before the overflow.
    pub(crate) fn invalidate_after_overflow(&self) {
        let mut state = lock(&self.inner);
        state.invalidation_epoch = state.invalidation_epoch.wrapping_add(1);
        crate::trace::trace!(
            "cache invalidate all after overflow -> epoch {}",
            state.invalidation_epoch
        );
        let doomed = state
            .slots
            .iter()
            .filter(|(_, slot)| matches!(slot, Slot::Ready(_)))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in doomed {
            remove_ready(&mut state, &key);
        }
        state.closure_misses.clear();
        state.invalidation_log.clear();
        state.invalidation_log_floor = state.invalidation_epoch;
        self.release(state);
        self.inner.changed.notify_all();
        // Overflow lost which paths changed: no witness survives it.
        self.witnesses().clear();
        // Overflow lost which directories changed: no catalogue survives it.
        let removed = self.catalogues().clear();
        self.release_catalogues(removed);
        {
            let mut state = lock(&self.inner);
            let freed = state.provider_graphs.clear();
            release_graph_bytes(&mut state, freed);
            self.release(state);
        }
        // Overflow lost which paths changed: every member is suspect.
        self.invalidate_index_all();
    }

    /// Remember that `path`, while it has `stamp`, holds what `proven` says.
    /// Only a settled stamp with a change identity can witness anything
    /// (spec §7: "reliable native change identity and the l4d-122
    /// freshness contract"); other stamps are ignored.
    pub(crate) fn record_witness(&self, path: &Path, stamp: ProjectReadStamp, proven: Proven) {
        if !stamp.settled || !stamp.has_change_identity() {
            return;
        }
        let mut witnesses = self.witnesses();
        if witnesses.proven.len() >= MAX_WITNESSES && !witnesses.proven.contains_key(path) {
            return;
        }
        witnesses
            .proven
            .insert(path.to_path_buf(), Witness { stamp, proven });
    }

    /// What this process proved about `path`, if `current` (taken now)
    /// shows it unchanged since.
    pub(crate) fn witness(&self, path: &Path, current: &ProjectReadStamp) -> Option<Proven> {
        let witnesses = self.witnesses();
        let witness = witnesses.proven.get(path)?;
        witness
            .stamp
            .proves_unchanged(current)
            .then(|| witness.proven.clone())
    }

    /// Remember the complete listing of directory `path` read while it had
    /// `stamp` (R8-1). A witness like `record_witness`: only a settled stamp
    /// with a change identity memoizes, and a watcher event for the
    /// directory or a path in it, or an overflow, forgets it.
    pub(crate) fn record_listing(
        &self,
        path: &Path,
        stamp: ProjectReadStamp,
        under: ListedUnder,
        listed: Arc<Listed>,
    ) {
        if !stamp.settled || !stamp.has_change_identity() {
            return;
        }
        let paths = listed.files.len() + listed.directories.len();
        let mut witnesses = self.witnesses();
        witnesses.remove_listing(path);
        if witnesses.listed_paths + paths > MAX_LISTED_PATHS {
            return;
        }
        witnesses.listed_paths += paths;
        witnesses.listings.insert(
            path.to_path_buf(),
            ListedWitness {
                stamp,
                under,
                listed,
            },
        );
    }

    /// The listing of `path` this process read under `under`, if `current`
    /// (taken now) shows the directory unchanged since.
    pub(crate) fn listing(
        &self,
        path: &Path,
        current: &ProjectReadStamp,
        under: ListedUnder,
    ) -> Option<Arc<Listed>> {
        let witnesses = self.witnesses();
        let witness = witnesses.listings.get(path)?;
        (witness.under.legacy == under.legacy
            && witness.under.entry_limit <= under.entry_limit
            && witness.stamp.proves_unchanged(current))
        .then(|| Arc::clone(&witness.listed))
    }

    fn witnesses(&self) -> std::sync::MutexGuard<'_, Witnesses> {
        self.inner
            .witnesses
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn drop_witnesses(&self, path: &Path, include_parent: bool) {
        let mut witnesses = self.witnesses();
        witnesses.proven.remove(path);
        witnesses.remove_listing(path);
        if let Some(parent) = path.parent().filter(|_| include_parent) {
            witnesses.proven.remove(parent);
            witnesses.remove_listing(parent);
        }
    }

    pub(crate) fn retain_fingerprints(&self, keep: &HashSet<u64>) {
        let mut state = lock(&self.inner);
        crate::trace::trace!("cache retain fingerprints {keep:?}");
        state.generation = state.generation.wrapping_add(1);
        let doomed = state
            .slots
            .keys()
            .filter(|key| !keep.contains(&key.fingerprint))
            .cloned()
            .collect::<Vec<_>>();
        for key in doomed {
            if !remove_ready(&mut state, &key) {
                state.slots.remove(&key);
            }
        }
        state
            .contexts
            .retain(|fingerprint, _| keep.contains(fingerprint));
        self.release(state);
        self.inner.changed.notify_all();
    }

    /// Whether an index batch may dispatch: its first member's charge fits
    /// next to pinned, outstanding and reserved bytes, evicting unpinned
    /// entries (l4d-axsn). Unlike `has_room`, which gates the crawl, it
    /// asks for no room for parses (A13). Shared provider interfaces give
    /// way as they do to the charge itself (`make_room`).
    pub(crate) fn index_batch_fits(&self) -> bool {
        let mut state = lock(&self.inner);
        prune_dead_leases(&mut state);
        fits_committed(&state, INDEX_BATCH_DISPATCH_BYTES)
    }

    /// Whether the crawl may dispatch. Shared provider interfaces are not
    /// counted: what the crawl stores evicts them (`evict_to_budget`,
    /// l4d-4nbs review I2), so they never stop it (l4d-axsn).
    pub(crate) fn has_room(&self) -> bool {
        let mut state = lock(&self.inner);
        prune_dead_leases(&mut state);
        committed_bytes(&state) < state.max_bytes
    }

    pub(crate) fn invalidation_epoch(&self) -> u64 {
        lock(&self.inner).invalidation_epoch
    }

    pub(crate) fn stats(&self) -> CacheStats {
        let mut state = lock(&self.inner);
        prune_dead_leases(&mut state);
        let count = |layer| {
            state
                .slots
                .iter()
                .filter(|(key, slot)| key.layer == layer && matches!(slot, Slot::Ready(_)))
                .count()
        };
        CacheStats {
            units: count(Layer::Unit),
            imports: count(Layer::Import),
            bytes: state.bytes,
            pinned_bytes: state.pinned_bytes,
            outstanding_bytes: state.outstanding_bytes,
            pinned_over_budget: state.pinned_bytes > state.max_bytes,
            reserved_facts_bytes: state.reserved[ReservationKind::Facts.index()],
            reserved_scratch_bytes: state.reserved[ReservationKind::Scratch.index()],
            reserved_pages_bytes: state.reserved[ReservationKind::Pages.index()],
            reserved_catalogue_bytes: state.reserved[ReservationKind::Catalogues.index()],
            reserved_index_bytes: state.reserved[ReservationKind::WorkspaceIndex.index()],
            reserved_provider_graph_bytes: state.reserved[ReservationKind::ProviderGraphs.index()],
            reserved_publication_bytes: state.reserved[ReservationKind::Publication.index()],
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_watcher(&self, watcher: Box<dyn DirectoryWatch>) {
        let mut state = self
            .inner
            .watcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.watcher = Some(watcher);
        state.registered.clear();
        self.inner.watch_epoch.fetch_add(1, Ordering::SeqCst);
    }
}

/// Shared directory catalogues (l4d-20).
impl ProjectCache {
    pub(crate) fn catalogue(&self, key: &CatalogueKey) -> Option<SharedCatalogue> {
        let epoch = self.inner.watch_epoch.load(Ordering::SeqCst);
        self.catalogues().get(key, epoch)
    }

    /// Keeps `catalogue` for every view of this cache, charged to the
    /// `Catalogues` share of `maxCacheBytes`, when `base` is the generation
    /// stored now. Returns the new generation, or `None` when it was not kept
    /// (no room, or another view published or invalidated first); the caller
    /// may still use it for its own request.
    pub(crate) fn publish_catalogue(
        &self,
        key: CatalogueKey,
        base: Option<u64>,
        catalogue: Arc<Catalogue>,
    ) -> Option<u64> {
        let bytes = catalogue.retained_bytes();
        if !self.charge_catalogue(bytes) {
            return None;
        }
        let directories = catalogue
            .directories()
            .iter()
            .map(|(directory, _)| directory.clone())
            .collect::<Vec<_>>();
        let epoch = self.inner.watch_epoch.load(Ordering::SeqCst);
        let state = lock(&self.inner);
        let mut count_epoch = state.watch_count_epoch;
        self.release(state);
        let watch =
            self.has_watcher() && self.catalogues().can_watch(directories.len(), count_epoch);
        if watch {
            // Read under the lock that acquires: `lock` may have just
            // recovered from poison and advanced it.
            let mut state = lock(&self.inner);
            count_epoch = state.watch_count_epoch;
            for directory in &directories {
                acquire_watch(&mut state, directory);
            }
            self.release(state);
        }
        let held = if watch {
            directories.clone()
        } else {
            Vec::new()
        };
        let outcome =
            self.catalogues()
                .publish(key.clone(), base, catalogue, bytes, held, count_epoch);
        match outcome {
            Ok((generation, removed)) => {
                self.release_catalogues(removed);
                if watch && self.all_watched(&directories) {
                    self.catalogues()
                        .mark_watch_registered(&key, generation, epoch);
                }
                Some(generation)
            }
            Err(rejected) => {
                self.release_catalogues(vec![rejected]);
                None
            }
        }
    }

    /// Drops the catalogue of `key` if it is still the one published under
    /// `generation` (a stamp check found it stale).
    pub(crate) fn drop_catalogue(&self, key: &CatalogueKey, generation: u64) {
        let removed = self.catalogues().remove(key, generation);
        self.release_catalogues(removed.into_iter().collect());
    }

    /// Advances on every catalogue publication and invalidation (for the
    /// l4d-33 index and l4d-zf1b.8 coverage).
    #[cfg_attr(not(test), allow(dead_code))] // read by l4d-33 and l4d-zf1b.8
    pub(crate) fn catalogue_generation(&self) -> u64 {
        self.catalogues().generation()
    }

    /// A stamp check of the catalogue published under `generation`, begun
    /// after its watches were registered, passed.
    pub(crate) fn confirm_catalogue_watched(&self, key: &CatalogueKey, generation: u64) {
        self.catalogues().confirm_watched(key, generation);
    }

    #[cfg(test)]
    pub(crate) fn set_catalogue_watch_budget_for_test(&self, directories: usize) {
        self.catalogues().set_watch_budget_for_test(directories);
    }

    fn has_watcher(&self) -> bool {
        self.inner
            .watcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .watcher
            .is_some()
    }

    fn all_watched(&self, directories: &[PathBuf]) -> bool {
        let watcher = self
            .inner
            .watcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        watcher.watcher.is_some()
            && directories
                .iter()
                .all(|directory| watcher.registered.contains(directory))
    }

    #[cfg(test)]
    pub(crate) fn catalogue_count(&self) -> usize {
        self.catalogues().len()
    }

    /// A panic under the store lock leaves reverse-map keys whose catalogue
    /// is gone, which lookups ignore; a panic in `publish` or `invalidate`
    /// after `take` also leaks the charge and watch counts of the entries it
    /// took, since their `Removed` never reaches `release_catalogues`. Poison
    /// is cleared, not recovered from.
    fn catalogues(&self) -> MutexGuard<'_, CatalogueStore> {
        #[cfg(test)]
        assert!(
            !HELD_INDEX.with(std::cell::Cell::get),
            "the index lock is never held with the catalogues lock"
        );
        self.inner.catalogues.lock().unwrap_or_else(|poisoned| {
            self.inner.catalogues.clear_poison();
            poisoned.into_inner()
        })
    }

    fn charge_catalogue(&self, bytes: usize) -> bool {
        let mut state = lock(&self.inner);
        prune_dead_leases(&mut state);
        let index = ReservationKind::Catalogues.index();
        let share = state.max_bytes / CATALOGUE_BUDGET_SHARE;
        let admitted = state.reserved[index]
            .checked_add(bytes)
            .is_some_and(|total| total <= share)
            && make_room(&mut state, bytes);
        if admitted {
            state.reserved[index] += bytes;
        }
        self.release(state);
        admitted
    }

    fn release_catalogues(&self, removed: Vec<Removed>) {
        if removed.is_empty() {
            return;
        }
        let mut state = lock(&self.inner);
        let index = ReservationKind::Catalogues.index();
        for removed in &removed {
            state.reserved[index] = state.reserved[index].saturating_sub(removed.bytes);
            // Watches acquired before a poison recovery were drained with
            // the counts; releasing them would release a newer holder's.
            if removed.watch_count_epoch != state.watch_count_epoch {
                continue;
            }
            for directory in &removed.watch_directories {
                release_watch(&mut state, directory);
            }
        }
        self.release(state);
    }

    /// The provider interface compiled under resolver configuration
    /// `configuration` for `key`, if its probes still hold (l4d-4nbs D29). A
    /// stale entry is dropped.
    pub(crate) fn provider_graph(
        &self,
        configuration: u64,
        key: &pascal_core::compile_time::ProviderGraphKey,
        overlays: &HashMap<Url, OverlayInput>,
    ) -> Option<SharedProviderGraphHit> {
        let full = (configuration, key.clone());
        let hit = {
            let state = lock(&self.inner);
            let entry = state.provider_graphs.entries.get(&full)?;
            SharedProviderGraphHit {
                graph: Arc::clone(&entry.graph),
                reads: Arc::clone(&entry.reads),
                probes: Arc::clone(&entry.probes),
            }
        };
        let held = probes_hold(&hit.probes, overlays);
        let mut state = lock(&self.inner);
        if !state
            .provider_graphs
            .entries
            .get(&full)
            .is_some_and(|entry| Arc::ptr_eq(&entry.graph, &hit.graph))
        {
            self.release(state);
            return None;
        }
        if !held {
            let freed = state.provider_graphs.remove(&full);
            release_graph_bytes(&mut state, freed);
            self.release(state);
            return None;
        }
        state.clock += 1;
        let tick = state.clock;
        state.provider_graphs.touch(&full, tick);
        self.release(state);
        Some(hit)
    }

    /// Keep a compiled provider interface, charged to `maxCacheBytes` within
    /// its share. False when it does not fit (l4d-4nbs D29). The share is
    /// made, room evicted and the entry inserted under one lock.
    pub(crate) fn put_provider_graph(
        &self,
        configuration: u64,
        key: pascal_core::compile_time::ProviderGraphKey,
        graph: Arc<pascal_core::compile_time::SharedProviderGraph>,
        probes: Vec<Probe>,
        reads: crate::workspace::capture::CapturedReads,
    ) -> bool {
        let bytes = std::mem::size_of::<ProviderGraphEntry>()
            .saturating_add(key.source.as_str().len())
            .saturating_add(std::mem::size_of_val(key.qualifiers.as_slice()))
            .saturating_add(graph.retained_bytes)
            .saturating_add(probes_bytes(&probes))
            .saturating_add(reads.retained_bytes());
        // A read it named may be missing: a session reusing it could only
        // inherit the overflow, never prove its inputs. It compiles the
        // provider itself instead, capturing its own reads.
        if reads.overflowed {
            self.inner
                .provider_graphs_refused
                .fetch_add(1, Ordering::Relaxed);
            crate::trace::trace!(
                "  provider interface {} not shared: its reads overflowed",
                key.source.as_str()
            );
            return false;
        }
        let full = (configuration, key);
        let mut state = lock(&self.inner);
        let share = state.max_bytes / PROVIDER_GRAPH_BUDGET_SHARE;
        if bytes > share {
            self.release(state);
            return false;
        }
        prune_dead_leases(&mut state);
        let freed = state.provider_graphs.remove(&full);
        release_graph_bytes(&mut state, freed);
        let freed = state.provider_graphs.evict_until(share - bytes);
        release_graph_bytes(&mut state, freed);
        if !make_room(&mut state, bytes) {
            self.release(state);
            return false;
        }
        state.reserved[ReservationKind::ProviderGraphs.index()] += bytes;
        state.clock += 1;
        let tick = state.clock;
        state.provider_graphs.insert(
            full,
            ProviderGraphEntry {
                graph,
                probes: Arc::new(probes),
                reads: Arc::new(reads),
                bytes,
            },
            tick,
        );
        self.release(state);
        true
    }

    #[cfg(test)]
    pub(crate) fn provider_graph_count(&self) -> usize {
        lock(&self.inner).provider_graphs.entries.len()
    }

    /// What the shared provider interfaces are charged.
    #[cfg(test)]
    pub(crate) fn provider_graph_bytes(&self) -> usize {
        lock(&self.inner).reserved[ReservationKind::ProviderGraphs.index()]
    }
}

/// A compiled provider interface reused from the cache, with the reads its
/// compilation captured.
pub(crate) struct SharedProviderGraphHit {
    pub(crate) graph: Arc<pascal_core::compile_time::SharedProviderGraph>,
    pub(crate) reads: Arc<crate::workspace::capture::CapturedReads>,
    probes: Arc<Vec<Probe>>,
}

type ProviderGraphId = (u64, pascal_core::compile_time::ProviderGraphKey);

struct ProviderGraphEntry {
    graph: Arc<pascal_core::compile_time::SharedProviderGraph>,
    probes: Arc<Vec<Probe>>,
    reads: Arc<crate::workspace::capture::CapturedReads>,
    bytes: usize,
}

/// Compiled provider interfaces by resolver configuration and provider key,
/// least recently used first in `lru`.
#[derive(Default)]
struct ProviderGraphStore {
    entries: HashMap<ProviderGraphId, ProviderGraphEntry>,
    ticks: HashMap<ProviderGraphId, u64>,
    /// By the cache's clock, which unit entries share.
    lru: BTreeMap<u64, ProviderGraphId>,
    bytes: usize,
}

impl ProviderGraphStore {
    fn touch(&mut self, id: &ProviderGraphId, tick: u64) {
        if let Some(old) = self.ticks.insert(id.clone(), tick) {
            self.lru.remove(&old);
        }
        self.lru.insert(tick, id.clone());
    }

    /// The caller released whatever `id` held before.
    fn insert(&mut self, id: ProviderGraphId, entry: ProviderGraphEntry, tick: u64) {
        self.bytes = self.bytes.saturating_add(entry.bytes);
        self.entries.insert(id.clone(), entry);
        self.touch(&id, tick);
    }

    fn remove(&mut self, id: &ProviderGraphId) -> usize {
        if let Some(tick) = self.ticks.remove(id) {
            self.lru.remove(&tick);
        }
        let freed = self.entries.remove(id).map_or(0, |entry| entry.bytes);
        self.bytes = self.bytes.saturating_sub(freed);
        freed
    }

    /// Evicts least recently used entries until at most `limit` bytes remain.
    fn evict_until(&mut self, limit: usize) -> usize {
        let mut freed = 0usize;
        while self.bytes > limit {
            let Some((_, id)) = self.lru.pop_first() else {
                break;
            };
            self.ticks.remove(&id);
            if let Some(entry) = self.entries.remove(&id) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
                freed = freed.saturating_add(entry.bytes);
            }
        }
        freed
    }

    /// Drops every entry a probe of which names `path` or lies under it, or
    /// (`include_parent`) names its parent directory.
    fn invalidate(&mut self, path: &Path, include_parent: bool) -> usize {
        let parent = path.parent().filter(|_| include_parent);
        let doomed = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.probes.iter().any(|probe| {
                    let probed = match probe {
                        Probe::Stamp { path, .. } | Probe::Content { path, .. } => path.clone(),
                        Probe::Overlay { uri, .. } => match uri.to_file_path() {
                            Ok(path) => path,
                            Err(()) => return false,
                        },
                    };
                    probed.starts_with(path) || parent.is_some_and(|parent| probed == parent)
                })
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        doomed
            .iter()
            .fold(0usize, |freed, id| freed.saturating_add(self.remove(id)))
    }

    fn clear(&mut self) -> usize {
        let freed = self.bytes;
        self.entries.clear();
        self.ticks.clear();
        self.lru.clear();
        self.bytes = 0;
        freed
    }
}

#[cfg(test)]
thread_local! {
    /// Set while this thread holds the index guard.
    static HELD_INDEX: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What names the documents of an index: a document, or a project.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum IndexAnchor {
    Document(Url),
    Project(PathBuf),
}

/// The documents one context indexes, and what they were computed from.
#[derive(Debug, Clone, Default)]
pub(crate) struct IndexMembership {
    pub(crate) paths: Vec<PathBuf>,
    /// The catalogues the paths came from, with the generation each had.
    pub(crate) scopes: Vec<(CatalogueKey, u64)>,
    pub(crate) complete: bool,
    pub(crate) limit: Option<String>,
    pub(crate) packages: bool,
    pub(crate) owner_pending: usize,
    /// The cache digest of the context the members belong to: the crawl's
    /// publications under it count as the members' (`IndexCounts::persisted`).
    pub(crate) context: Option<pascal_cache::Digest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublishOutcome {
    Indexed,
    NotAMember,
    /// No room in the index share: the member stays unindexed.
    Memory,
}

struct StoredIndex {
    index: ContextIndex,
    membership: IndexMembership,
    anchor: IndexAnchor,
    dirty: bool,
    last_used: u64,
    /// The membership's own charge. The members' charges are
    /// `index.bytes()`.
    charged: usize,
    memory_exhausted: bool,
    /// Store reads spent validating members of this context.
    validation_reads: u64,
    /// The snapshot this context's index started from or last sealed (D40).
    baseline: Option<crate::workspace::baseline::BaselineHint>,
    /// When this process last sealed, or found nothing new to seal.
    last_seal: Option<std::time::Instant>,
    /// Whether this process looked for a sealed snapshot to start from.
    hint_tried: bool,
    /// A batch indexed members while the store was not enabled: they have
    /// no record until the next persisting batch proves them again.
    memory_only: bool,
    /// A publication was refused for room (the WAL at its threshold, the
    /// disk budget, free space): its members stay memory-only until the
    /// store reports no pressure, then a batch proves them again.
    awaiting_room: bool,
    /// A batch's wait for maintenance found no room; cleared by room.
    room_wait_failed: bool,
    /// Extracted members whose publication found no memory for its
    /// transient buffers in this process: they stay memory-only, without a
    /// record, and the next start extracts them again (13b-3).
    publications_refused: u64,
}

impl StoredIndex {
    fn total_charge(&self) -> usize {
        self.charged.saturating_add(self.index.bytes())
    }

    fn unrecorded_fresh_members(&self) -> Vec<PathBuf> {
        self.index
            .records()
            .into_iter()
            .filter(|(path, record)| {
                record.is_none()
                    && self.index.state_of(path) == Some(crate::workspace_index::MemberState::Fresh)
            })
            .map(|(path, _)| path)
            .collect()
    }
}

/// The longest anchor URI `pascal/indexStatus` reports.
const STATUS_ANCHOR_BYTES: usize = 512;
/// At most this many gaps per context, each at most `STATUS_GAP_BYTES`.
const STATUS_GAPS: usize = 4;
const STATUS_GAP_BYTES: usize = 160;
/// The whole `pascal/indexStatus` document.
const STATUS_DOCUMENT_BYTES: usize = 8192;

fn truncate_utf8(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// One context of `pascal/indexStatus`: counts, never member paths.
#[derive(Debug, Clone)]
pub(crate) struct IndexStatusEntry {
    /// The anchor URI, at most `STATUS_ANCHOR_BYTES` bytes.
    pub(crate) anchor: String,
    pub(crate) anchor_is_project: bool,
    pub(crate) anchor_truncated: bool,
    pub(crate) counts: crate::workspace_index::IndexCounts,
    pub(crate) membership_complete: bool,
    /// The membership is complete, current, and no member that can be
    /// indexed now is pending. Members retried after a refusal are not
    /// counted against it: they are reported as a gap. Neither are open
    /// members still pending, which are answered from their buffers.
    pub(crate) complete: bool,
    /// `"sealed"`: a snapshot this process sealed is the baseline. `"held"`:
    /// the baseline was adopted from an earlier process's snapshot. `"none"`:
    /// no baseline hint.
    pub(crate) snapshot: &'static str,
    pub(crate) validation_reads: u64,
    /// `StoredIndex::publications_refused`.
    pub(crate) publications_refused: u64,
    pub(crate) gaps: Vec<String>,
}

impl IndexStatusEntry {
    fn of(
        stored: &StoredIndex,
        crawl_published: &HashMap<PathBuf, Vec<(u64, pascal_cache::Digest)>>,
        is_open: &dyn Fn(&Path) -> bool,
    ) -> Self {
        let counts = match stored.membership.context {
            Some(context) => stored.index.counts_with(&|path, hash| {
                crawl_published.get(path).is_some_and(|published| {
                    published
                        .iter()
                        .any(|&(h, c)| c == context && hash.is_none_or(|hash| hash == h))
                })
            }),
            None => stored.index.counts(),
        };
        let (anchor_is_project, uri) = match &stored.anchor {
            IndexAnchor::Document(uri) => (false, uri.to_string()),
            IndexAnchor::Project(path) => (
                true,
                Url::from_file_path(path)
                    .map_or_else(|()| path.to_string_lossy().into_owned(), String::from),
            ),
        };
        let (anchor, anchor_truncated) = truncate_utf8(&uri, STATUS_ANCHOR_BYTES);
        let membership = &stored.membership;
        let membership_complete = membership.complete && !stored.dirty;
        // An open member is answered from the editor's buffer: while the
        // store is off it is never indexed (l4d-33), so it is not work the
        // index waits for. With the store on, the baseline view indexes it
        // from disk and it is counted as any other member.
        let open_pending = if counts.pending > counts.retrying {
            stored
                .index
                .pending(usize::MAX)
                .iter()
                .filter(|path| !stored.index.retrying(path) && is_open(path))
                .count()
        } else {
            0
        };
        let eligible_pending = counts
            .pending
            .saturating_sub(counts.retrying)
            .saturating_sub(open_pending);
        let mut gaps = Vec::new();
        match &membership.limit {
            Some(limit) => gaps.push(format!("{limit}; the rest are not indexed")),
            None if !membership.complete => {
                gaps.push("membership is incomplete; some units are not indexed".to_owned());
            }
            None => {}
        }
        if membership.packages {
            gaps.push("units provided through runtime packages are not indexed".to_owned());
        }
        if counts.refused > 0 {
            gaps.push(format!("{} units could not be indexed", counts.refused));
        }
        let over_index_limits = counts.limited - counts.over_deadline;
        if over_index_limits > 0 {
            gaps.push(format!(
                "{over_index_limits} units exceed the index limits and are not indexed"
            ));
        }
        if counts.over_deadline > 0 {
            gaps.push(format!(
                "{} units took longer than {} s to extract and are not indexed; the next start retries them last",
                counts.over_deadline,
                crate::workspace::indexing::MEMBER_DEADLINE.as_secs()
            ));
        }
        if counts.over_limit > 0 {
            gaps.push(format!(
                "{} members are too large to store and are extracted again by the next start",
                counts.over_limit
            ));
        }
        if stored.publications_refused > 0 {
            gaps.push(format!(
                "{} members found no memory to publish and are extracted again by the next start",
                stored.publications_refused
            ));
        }
        if counts.deferred > 0 {
            gaps.push(format!(
                "{} units failed to load for the index and are retried later",
                counts.deferred
            ));
        }
        if counts.retrying > 0 {
            gaps.push(format!(
                "{} members retried after a previous refusal",
                counts.retrying
            ));
        }
        if stored.memory_exhausted {
            gaps.push("the workspace index memory share is exhausted".to_owned());
        }
        gaps.truncate(STATUS_GAPS);
        for gap in &mut gaps {
            let (kept, _) = truncate_utf8(gap, STATUS_GAP_BYTES);
            let kept = kept.len();
            gap.truncate(kept);
        }
        Self {
            anchor: anchor.to_owned(),
            anchor_is_project,
            anchor_truncated,
            counts,
            membership_complete,
            complete: membership_complete && eligible_pending == 0,
            snapshot: match stored.baseline.as_ref() {
                None => "none",
                Some(hint) if hint.sealed() => "sealed",
                Some(_) => "held",
            },
            validation_reads: stored.validation_reads,
            publications_refused: stored.publications_refused,
            gaps,
        }
    }

    fn to_json(&self, anchor_bytes: usize) -> serde_json::Value {
        let (anchor, cut) = truncate_utf8(&self.anchor, anchor_bytes);
        let key = if self.anchor_is_project {
            "project"
        } else {
            "document"
        };
        let counts = &self.counts;
        serde_json::json!({
            "anchor": {key: anchor},
            "anchorTruncated": self.anchor_truncated || cut,
            "members": counts.members,
            "fresh": counts.fresh,
            "reused": counts.reused,
            "extracted": counts.extracted,
            "pending": counts.pending,
            "reusePending": counts.reuse_pending,
            "refused": counts.refused,
            "limited": counts.limited,
            "deferred": counts.deferred,
            "persisted": counts.persisted,
            "retrying": counts.retrying,
            "overLimit": counts.over_limit,
            "overDeadline": counts.over_deadline,
            "membershipComplete": self.membership_complete,
            "complete": self.complete,
            "snapshot": self.snapshot,
            "validationReads": self.validation_reads,
            "publicationsRefused": self.publications_refused,
            "gaps": self.gaps,
        })
    }
}

/// What `pascal/indexStatus` counts for the whole process, not per context.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct IndexProcessCounts {
    /// Units the warm crawl published (R8-2/D42).
    pub(crate) crawl_published: u64,
    /// Crawl publications that found no room, left to the index batch.
    pub(crate) crawl_deferred: u64,
    /// Provider interfaces not shared because their reads overflowed.
    pub(crate) provider_graphs_refused: u64,
}

/// The `pascal/indexStatus` result. It never exceeds
/// `STATUS_DOCUMENT_BYTES`: when the entries do not fit, gaps are dropped
/// from the least recently used contexts first, then anchors are cut.
pub(crate) fn index_status_json(
    entries: &[IndexStatusEntry],
    process: IndexProcessCounts,
) -> serde_json::Value {
    let mut entries = entries.to_vec();
    let mut anchor_bytes = STATUS_ANCHOR_BYTES;
    loop {
        let document = serde_json::json!({
            "schema": 1,
            "crawlPublished": process.crawl_published,
            "crawlDeferred": process.crawl_deferred,
            "providerGraphsRefused": process.provider_graphs_refused,
            "contexts": entries
                .iter()
                .map(|entry| entry.to_json(anchor_bytes))
                .collect::<Vec<_>>(),
        });
        if document.to_string().len() <= STATUS_DOCUMENT_BYTES {
            return document;
        }
        match entries
            .iter_mut()
            .rev()
            .find(|entry| !entry.gaps.is_empty())
        {
            Some(entry) => {
                entry.gaps.pop();
            }
            None if anchor_bytes > 16 => anchor_bytes /= 2,
            None => return document,
        }
    }
}

struct IndexStore {
    contexts: HashMap<ContextKey, StoredIndex>,
    clock: u64,
    owners: HashMap<PathBuf, ContextKey>,
    /// Owners that are D43 claims or projectless (see `remember_owner_claimable`).
    claimable_owners: HashSet<PathBuf>,
    /// The main workspace's D43 claim generation: claimable owners decided
    /// against another generation are not recorded.
    claim_generation: u64,
    requests: Vec<IndexAnchor>,
    unit_limit: usize,
    /// Holds of evicted contexts' snapshots, released by the next index
    /// batch that has a store job (D40): eviction runs under the index lock,
    /// where no store call may.
    released: Vec<pascal_cache::SnapshotId>,
    /// What the warm crawl published in this process: per path, the content
    /// hash and context digest of each publication. Bounded by
    /// `MAX_CRAWL_PUBLISHED` paths; an invalidated path is forgotten.
    crawl_published: HashMap<PathBuf, Vec<(u64, pascal_cache::Digest)>>,
    /// For each path a crawl publication's evidence names, the published
    /// units that read it: a change to it makes them publishable again.
    crawl_dependents: HashMap<PathBuf, HashSet<PathBuf>>,
    /// Per path, the content hash and context digest of each extraction in
    /// this process whose publication exceeded a fixed limit
    /// (`PersistSkip::over_fixed_limit`): the same write would fail again,
    /// so neither the crawl nor the index attempts it. Bounded like
    /// `crawl_published`; a change to the path forgets it. A change to
    /// another input does not: the member then stays unpublished in this
    /// process, which is never a wrong publication.
    over_limit: HashMap<PathBuf, Vec<(u64, pascal_cache::Digest)>>,
    /// Units the warm crawl published in this process (13b-3, A2).
    crawl_published_count: u64,
    /// Units whose crawl publication found no room and were left to the
    /// index batch, which extracts and publishes them (13b-3, A2).
    crawl_deferred_count: u64,
    /// The paths the warm crawl published, in publication order.
    #[cfg(test)]
    published_by_crawl: Vec<PathBuf>,
}

impl Default for IndexStore {
    fn default() -> Self {
        Self {
            contexts: HashMap::new(),
            clock: 0,
            owners: HashMap::new(),
            claimable_owners: HashSet::new(),
            claim_generation: 0,
            requests: Vec::new(),
            unit_limit: MAX_WORKSPACE_UNITS,
            released: Vec::new(),
            crawl_published: HashMap::new(),
            crawl_dependents: HashMap::new(),
            over_limit: HashMap::new(),
            crawl_published_count: 0,
            crawl_deferred_count: 0,
            #[cfg(test)]
            published_by_crawl: Vec::new(),
        }
    }
}

impl IndexStore {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn request(&mut self, anchor: &IndexAnchor) {
        if self.requests.len() < MAX_INDEX_REQUESTS && !self.requests.contains(anchor) {
            self.requests.push(anchor.clone());
        }
    }

    /// Removes the least recently used context other than `except` and
    /// returns the charge it held, for the caller to release once the store
    /// is unlocked.
    fn evict_lru(&mut self, except: &ContextKey) -> Option<usize> {
        let victim = self
            .contexts
            .iter()
            .filter(|(key, _)| *key != except)
            .min_by_key(|(_, stored)| stored.last_used)
            .map(|(key, _)| key.clone())?;
        let stored = self.contexts.remove(&victim)?;
        self.owners.retain(|_, owner| *owner != victim);
        if let Some(hint) = stored.baseline.as_ref().filter(|hint| hint.held()) {
            self.released.push(hint.header().id());
        }
        Some(stored.total_charge())
    }
}

/// The index lock; in tests it flags the thread so `lock` can refuse to run
/// under it.
struct IndexGuard<'a>(MutexGuard<'a, IndexStore>);

impl<'a> IndexGuard<'a> {
    fn new(guard: MutexGuard<'a, IndexStore>) -> Self {
        #[cfg(test)]
        HELD_INDEX.with(|held| held.set(true));
        Self(guard)
    }
}

impl std::ops::Deref for IndexGuard<'_> {
    type Target = IndexStore;

    fn deref(&self) -> &IndexStore {
        &self.0
    }
}

impl std::ops::DerefMut for IndexGuard<'_> {
    fn deref_mut(&mut self) -> &mut IndexStore {
        &mut self.0
    }
}

impl Drop for IndexGuard<'_> {
    fn drop(&mut self) {
        #[cfg(test)]
        HELD_INDEX.with(|held| held.set(false));
    }
}

fn membership_bytes(paths: &[PathBuf]) -> usize {
    paths.iter().map(|path| membership_charge(path)).sum()
}

/// The workspace index store (l4d-33): one `ContextIndex` per context, at
/// most `MAX_INDEXED_CONTEXTS`, charged to a quarter of `maxCacheBytes`.
impl ProjectCache {
    /// A panic under the index lock can leave a charge unreleased; poison
    /// is cleared, not recovered from.
    fn index(&self) -> IndexGuard<'_> {
        IndexGuard::new(self.inner.index.lock().unwrap_or_else(|poisoned| {
            self.inner.index.clear_poison();
            poisoned.into_inner()
        }))
    }

    /// Charge `bytes` to the index share, evicting cache entries (never
    /// pinned) like a reservation. The store holds the charge and releases
    /// it with `release_index`.
    fn charge_index(&self, bytes: usize) -> bool {
        let mut state = lock(&self.inner);
        prune_dead_leases(&mut state);
        let slot = ReservationKind::WorkspaceIndex.index();
        let share = state.max_bytes / INDEX_BUDGET_SHARE;
        let admitted = state.reserved[slot]
            .checked_add(bytes)
            .is_some_and(|total| total <= share)
            && make_room(&mut state, bytes);
        if admitted {
            state.reserved[slot] += bytes;
        }
        self.release(state);
        admitted
    }

    fn release_index(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let mut state = lock(&self.inner);
        let slot = ReservationKind::WorkspaceIndex.index();
        state.reserved[slot] = state.reserved[slot].saturating_sub(bytes);
        self.release(state);
    }

    /// `charge_index`, and when it fails evict the least recently used
    /// context other than `except` once and try again.
    fn charge_index_evicting(&self, bytes: usize, except: &ContextKey) -> bool {
        if self.charge_index(bytes) {
            return true;
        }
        let evicted = self.index().evict_lru(except);
        let Some(evicted) = evicted else {
            return false;
        };
        self.release_index(evicted);
        self.charge_index(bytes)
    }

    pub(crate) fn index_membership_current(&self, key: &ContextKey) -> bool {
        let scopes = {
            let store = self.index();
            match store.contexts.get(key) {
                Some(stored) if !stored.dirty => stored.membership.scopes.clone(),
                _ => return false,
            }
        };
        scopes.iter().all(|(scope, generation)| {
            self.catalogue(scope)
                .is_some_and(|shared| shared.generation == *generation)
        })
    }

    pub(crate) fn set_index_membership(
        &self,
        key: ContextKey,
        anchor: IndexAnchor,
        mut membership: IndexMembership,
        limit: usize,
    ) {
        let limit = limit.min(self.index().unit_limit);
        if membership.paths.len() > limit {
            membership.paths.truncate(limit);
            membership.complete = false;
            membership.limit = Some(format!("more than {limit} documents"));
        }
        let mut exhausted = false;
        for _ in 0..4 {
            let wanted = membership_bytes(&membership.paths);
            let held = self
                .index()
                .contexts
                .get(&key)
                .map_or(0, |stored| stored.charged);
            let paid = wanted.saturating_sub(held);
            if paid > 0 && !self.charge_index_evicting(paid, &key) {
                // A stored context keeps the membership and members it
                // holds; the gap stays visible until the share has room.
                if let Some(stored) = self.index().contexts.get_mut(&key) {
                    stored.memory_exhausted = true;
                    stored.dirty = true;
                    stored.membership.complete = false;
                    return;
                }
                exhausted = true;
                membership.paths.clear();
                membership.complete = false;
                membership.limit = Some("the index memory share is exhausted".to_owned());
                continue;
            }
            let mut store = self.index();
            let clock = store.tick();
            let current = store.contexts.get(&key).map_or(0, |stored| stored.charged);
            if current != held {
                drop(store);
                self.release_index(paid);
                continue;
            }
            let stored = store
                .contexts
                .entry(key.clone())
                .or_insert_with(|| StoredIndex {
                    index: ContextIndex::new(),
                    membership: IndexMembership::default(),
                    anchor: anchor.clone(),
                    dirty: false,
                    last_used: clock,
                    charged: 0,
                    memory_exhausted: false,
                    validation_reads: 0,
                    baseline: None,
                    last_seal: None,
                    hint_tried: false,
                    memory_only: false,
                    awaiting_room: false,
                    room_wait_failed: false,
                    publications_refused: 0,
                });
            let (_, removed) = stored.index.set_members(membership.paths.clone());
            stored.membership = membership;
            stored.anchor = anchor;
            stored.dirty = false;
            stored.last_used = clock;
            stored.charged = wanted;
            stored.memory_exhausted |= exhausted;
            let mut released = removed + held.saturating_sub(wanted);
            while store.contexts.len() > MAX_INDEXED_CONTEXTS {
                match store.evict_lru(&key) {
                    Some(evicted) => released += evicted,
                    None => break,
                }
            }
            drop(store);
            self.release_index(released);
            return;
        }
        // Retries lost to concurrent changes: leave the gap visible.
        if let Some(stored) = self.index().contexts.get_mut(&key) {
            stored.memory_exhausted = true;
        }
    }

    pub(crate) fn publish_index_member(
        &self,
        key: &ContextKey,
        path: &Path,
        facts: MemberFacts,
        stamps: Vec<MemberStamp>,
        content_hash: u64,
    ) -> PublishOutcome {
        self.publish_member(key, path, facts, stamps, content_hash, false)
    }

    /// `publish_index_member` for facts from the store: the member is
    /// marked reused under the same lock, so no status reads it as
    /// extracted.
    pub(crate) fn publish_reused_index_member(
        &self,
        key: &ContextKey,
        path: &Path,
        facts: MemberFacts,
        stamps: Vec<MemberStamp>,
        content_hash: u64,
    ) -> PublishOutcome {
        self.publish_member(key, path, facts, stamps, content_hash, true)
    }

    fn publish_member(
        &self,
        key: &ContextKey,
        path: &Path,
        facts: MemberFacts,
        stamps: Vec<MemberStamp>,
        content_hash: u64,
        reused: bool,
    ) -> PublishOutcome {
        // The charge covers the names the context does not hold yet, so it
        // is computed under the lock and checked again before inserting.
        let charge_for = |store: &IndexStore| {
            store
                .contexts
                .get(key)
                .filter(|stored| stored.index.stamps_of(path).is_some())
                .map(|stored| stored.index.charge_for(&facts, &stamps))
        };
        for _ in 0..4 {
            let Some(bytes) = charge_for(&self.index()) else {
                return PublishOutcome::NotAMember;
            };
            if !self.charge_index_evicting(bytes, key) {
                if let Some(stored) = self.index().contexts.get_mut(key) {
                    stored.memory_exhausted = true;
                }
                return PublishOutcome::Memory;
            }
            let mut store = self.index();
            if charge_for(&store).is_some_and(|needed| needed > bytes) {
                drop(store);
                self.release_index(bytes);
                continue;
            }
            let clock = store.tick();
            let old = store.contexts.get_mut(key).and_then(|stored| {
                stored.last_used = clock;
                let old = stored
                    .index
                    .insert_paid(path, facts, stamps, content_hash, bytes);
                if old.is_some() {
                    stored.memory_exhausted = false;
                    if reused {
                        stored.index.mark_reused(path);
                    }
                }
                old
            });
            drop(store);
            return match old {
                Some(old) => {
                    self.release_index(old);
                    PublishOutcome::Indexed
                }
                None => {
                    self.release_index(bytes);
                    PublishOutcome::NotAMember
                }
            };
        }
        // Retries lost to concurrent compactions: the member stays unindexed.
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.memory_exhausted = true;
        }
        PublishOutcome::Memory
    }

    pub(crate) fn refresh_index_member(
        &self,
        key: &ContextKey,
        path: &Path,
        stamps: Vec<MemberStamp>,
    ) -> bool {
        self.index()
            .contexts
            .get_mut(key)
            .is_some_and(|stored| stored.index.refresh(path, stamps))
    }

    pub(crate) fn refuse_index_member(&self, key: &ContextKey, path: &Path, reason: String) {
        let old = self
            .index()
            .contexts
            .get_mut(key)
            .and_then(|stored| stored.index.refuse(path, reason));
        if let Some(old) = old {
            self.release_index(old);
        }
    }

    pub(crate) fn limit_index_member(&self, key: &ContextKey, path: &Path) {
        self.limit_index_member_as(key, path, false);
    }

    /// `limit_index_member` for a member whose extraction passed its
    /// deadline (`IndexCounts::over_deadline`).
    pub(crate) fn limit_index_member_past_deadline(&self, key: &ContextKey, path: &Path) {
        self.limit_index_member_as(key, path, true);
    }

    fn limit_index_member_as(&self, key: &ContextKey, path: &Path, over_deadline: bool) {
        let old = self
            .index()
            .contexts
            .get_mut(key)
            .and_then(|stored| stored.index.limit(path, over_deadline));
        if let Some(old) = old {
            self.release_index(old);
        }
    }

    pub(crate) fn defer_index_member(&self, key: &ContextKey, path: &Path, reason: &str) {
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.index.defer(path, reason);
        }
    }

    pub(crate) fn reuse_pending_index_members(
        &self,
        key: &ContextKey,
        limit: usize,
    ) -> Vec<PathBuf> {
        self.index()
            .contexts
            .get(key)
            .map(|stored| stored.index.reuse_pending(limit))
            .unwrap_or_default()
    }

    pub(crate) fn mark_index_reuse_tried(&self, key: &ContextKey, path: &Path) {
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.index.mark_reuse_tried(path);
        }
    }

    /// The warm crawl published `path`'s facts (R8-2/D42): every context
    /// index asks the store for them again before it extracts the member.
    /// Nothing enters an index here.
    /// `hash` and `context` (the context's cache digest) say what was
    /// published, so the crawl does not publish it again in this process;
    /// `inputs` are the other paths its evidence names, whose change makes
    /// it publishable again.
    pub(crate) fn note_published(
        &self,
        path: &Path,
        hash: u64,
        context: pascal_cache::Digest,
        inputs: &[PathBuf],
    ) {
        let mut index = self.index();
        index.crawl_published_count = index.crawl_published_count.saturating_add(1);
        for stored in index.contexts.values_mut() {
            stored.index.clear_reuse_tried(path);
        }
        if index.crawl_published.len() >= MAX_CRAWL_PUBLISHED
            && !index.crawl_published.contains_key(path)
        {
            index.crawl_published.clear();
            index.crawl_dependents.clear();
        }
        let published = index.crawl_published.entry(path.to_path_buf()).or_default();
        if !published.contains(&(hash, context)) {
            if published.len() >= MAX_CRAWL_PUBLICATIONS_PER_PATH {
                published.remove(0);
            }
            published.push((hash, context));
        }
        for input in inputs {
            index
                .crawl_dependents
                .entry(input.clone())
                .or_default()
                .insert(path.to_path_buf());
        }
        #[cfg(test)]
        index.published_by_crawl.push(path.to_path_buf());
    }

    /// The warm crawl found no room to publish `path` (13b-3, A2). Nothing
    /// is dropped: each context index with `path` as a member still holds
    /// it pending, so its next Full batch extracts and publishes it; this
    /// counts the deferral.
    pub(crate) fn note_crawl_deferred(&self, path: &Path) {
        let mut index = self.index();
        index.crawl_deferred_count = index.crawl_deferred_count.saturating_add(1);
        crate::trace::trace!(
            "  crawl unit {} deferred to the index batch: no room for its publication",
            path.display()
        );
    }

    /// The process-wide counts `pascal/indexStatus` reports beside its
    /// contexts.
    pub(crate) fn index_process_counts(&self) -> IndexProcessCounts {
        let provider_graphs_refused = self.inner.provider_graphs_refused.load(Ordering::Relaxed);
        let index = self.index();
        IndexProcessCounts {
            crawl_published: index.crawl_published_count,
            crawl_deferred: index.crawl_deferred_count,
            provider_graphs_refused,
        }
    }

    /// Whether the crawl published `path` from the bytes hashed `hash` under
    /// `context` in this process.
    pub(crate) fn crawl_published(
        &self,
        path: &Path,
        hash: u64,
        context: pascal_cache::Digest,
    ) -> bool {
        self.index()
            .crawl_published
            .get(path)
            .is_some_and(|published| published.contains(&(hash, context)))
    }

    /// `path`'s facts from the bytes hashed `hash`, under the context with
    /// cache digest `context`, exceed a fixed limit of the store.
    pub(crate) fn note_over_limit(&self, path: &Path, hash: u64, context: pascal_cache::Digest) {
        let mut index = self.index();
        if index.over_limit.len() >= MAX_CRAWL_PUBLISHED && !index.over_limit.contains_key(path) {
            index.over_limit.clear();
        }
        let noted = index.over_limit.entry(path.to_path_buf()).or_default();
        if !noted.contains(&(hash, context)) {
            if noted.len() >= MAX_CRAWL_PUBLICATIONS_PER_PATH {
                noted.remove(0);
            }
            noted.push((hash, context));
        }
    }

    /// Whether `note_over_limit` noted these facts in this process.
    pub(crate) fn over_limit(&self, path: &Path, hash: u64, context: pascal_cache::Digest) -> bool {
        self.index()
            .over_limit
            .get(path)
            .is_some_and(|noted| noted.contains(&(hash, context)))
    }

    /// `key`'s Fresh member `path` is too large to store.
    pub(crate) fn mark_index_over_limit(&self, key: &ContextKey, path: &Path) {
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.index.mark_over_limit(path);
        }
    }

    #[cfg(test)]
    pub(crate) fn published_by_crawl_for_test(&self) -> Vec<PathBuf> {
        self.index().published_by_crawl.clone()
    }

    /// Whether some context index holds `path` indexed from the bytes
    /// hashed `hash`, with a record this process proved: its facts are in
    /// the store already.
    pub(crate) fn index_member_is_proven(&self, path: &Path, hash: u64) -> bool {
        self.index()
            .contexts
            .values()
            .any(|stored| stored.index.proven_with_hash(path, hash))
    }

    /// `context` is the cache digest of the context the caller indexes
    /// under (`CacheContextKey::new(&state.context)`); a proof made under
    /// another context is refused.
    pub(crate) fn set_index_record(
        &self,
        key: &ContextKey,
        proof: &crate::workspace::eligible::ProvenRecord,
        context: pascal_cache::Digest,
    ) -> bool {
        proof.context() == context
            && self
                .index()
                .contexts
                .get_mut(key)
                .is_some_and(|stored| stored.index.set_record(proof.path(), proof.record_id()))
    }

    pub(crate) fn index_member_has_record(&self, key: &ContextKey, path: &Path) -> bool {
        self.index()
            .contexts
            .get(key)
            .is_some_and(|stored| stored.index.record(path).is_some())
    }

    #[allow(dead_code)] // consumed by pascal/indexStatus and the snapshots (l4d-zf1b.8)
    pub(crate) fn index_records(
        &self,
        key: &ContextKey,
    ) -> Option<Vec<(PathBuf, Option<pascal_cache::Digest>)>> {
        self.index()
            .contexts
            .get(key)
            .map(|stored| stored.index.records())
    }

    pub(crate) fn add_index_validation_reads(&self, key: &ContextKey, reads: u64) {
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.validation_reads = stored.validation_reads.saturating_add(reads);
        }
    }

    #[allow(dead_code)] // consumed by pascal/indexStatus and the snapshots (l4d-zf1b.8)
    pub(crate) fn index_validation_reads(&self, key: &ContextKey) -> u64 {
        self.index()
            .contexts
            .get(key)
            .map_or(0, |stored| stored.validation_reads)
    }

    /// One entry per stored context, most recently used first (at most
    /// `MAX_INDEXED_CONTEXTS`), read under the index lock without IO or
    /// touching any context's recency.
    /// `is_open` names the open members a memory-only index leaves to
    /// their buffers.
    pub(crate) fn index_status(&self, is_open: &dyn Fn(&Path) -> bool) -> Vec<IndexStatusEntry> {
        let store = self.index();
        let mut stored: Vec<&StoredIndex> = store.contexts.values().collect();
        stored.sort_by_key(|stored| std::cmp::Reverse(stored.last_used));
        stored
            .into_iter()
            .take(MAX_INDEXED_CONTEXTS)
            .map(|stored| IndexStatusEntry::of(stored, &store.crawl_published, is_open))
            .collect()
    }

    /// A batch of `key` indexed members while the store was not enabled.
    pub(crate) fn note_index_memory_only(&self, key: &ContextKey) {
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.memory_only = true;
        }
    }

    /// After `note_index_memory_only`, once: marks every fresh member of
    /// `key` without a record stale, so the next batches prove it again
    /// (the reprove path). Returns how many.
    pub(crate) fn restale_memory_only_members(&self, key: &ContextKey) -> usize {
        let mut store = self.index();
        let Some(stored) = store
            .contexts
            .get_mut(key)
            .filter(|stored| stored.memory_only)
        else {
            return 0;
        };
        stored.memory_only = false;
        let unrecorded = stored.unrecorded_fresh_members();
        for path in &unrecorded {
            stored.index.mark_stale(path);
        }
        unrecorded.len()
    }

    /// A publication of `key`'s member was refused for room.
    pub(crate) fn note_index_awaiting_room(&self, key: &ContextKey) {
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.awaiting_room = true;
        }
    }

    /// An extracted member of `key` found no memory to publish (13b-3).
    pub(crate) fn note_index_publication_refused(&self, key: &ContextKey) {
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.publications_refused = stored.publications_refused.saturating_add(1);
        }
    }

    pub(crate) fn index_room_wait_failed(&self, key: &ContextKey) -> bool {
        self.index()
            .contexts
            .get(key)
            .is_some_and(|stored| stored.room_wait_failed)
    }

    pub(crate) fn set_index_room_wait_failed(&self, key: &ContextKey, failed: bool) {
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.room_wait_failed = failed;
        }
    }

    pub(crate) fn index_is_awaiting_room(&self, key: &ContextKey) -> bool {
        self.index()
            .contexts
            .get(key)
            .is_some_and(|stored| stored.awaiting_room)
    }

    /// After `note_index_awaiting_room`: the fresh members of `key` still
    /// without a record (0 otherwise).
    pub(crate) fn index_awaiting_room(&self, key: &ContextKey) -> usize {
        self.index()
            .contexts
            .get(key)
            .filter(|stored| stored.awaiting_room)
            .map_or(0, |stored| stored.unrecorded_fresh_members().len())
    }

    /// Once the store has room again: marks the members
    /// `index_awaiting_room` counts stale, so the next batches prove them
    /// (reuse, else extraction). Returns how many.
    pub(crate) fn restale_awaiting_room(&self, key: &ContextKey) -> usize {
        let mut store = self.index();
        let Some(stored) = store
            .contexts
            .get_mut(key)
            .filter(|stored| stored.awaiting_room)
        else {
            return 0;
        };
        stored.awaiting_room = false;
        let unrecorded = stored.unrecorded_fresh_members();
        for path in &unrecorded {
            stored.index.mark_stale(path);
        }
        unrecorded.len()
    }

    pub(crate) fn mark_index_stale(&self, key: &ContextKey, path: &Path) {
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.index.mark_stale(path);
        }
    }

    /// The membership is recomputed by the next batch.
    pub(crate) fn mark_index_membership_dirty(&self, key: &ContextKey) {
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.dirty = true;
        }
    }

    pub(crate) fn baseline_hint(
        &self,
        key: &ContextKey,
    ) -> Option<crate::workspace::baseline::BaselineHint> {
        self.index()
            .contexts
            .get(key)
            .and_then(|stored| stored.baseline.clone())
    }

    /// Install `hint` as `key`'s baseline and return the one it replaces;
    /// `None` when `key` holds no index any more, so the caller still owns
    /// the hint's hold.
    pub(crate) fn set_baseline_hint(
        &self,
        key: &ContextKey,
        hint: crate::workspace::baseline::BaselineHint,
    ) -> Option<Option<crate::workspace::baseline::BaselineHint>> {
        self.index()
            .contexts
            .get_mut(key)
            .map(|stored| stored.baseline.replace(hint))
    }

    /// Drop `key`'s baseline if it is still snapshot `id`; returns it.
    pub(crate) fn drop_baseline_hint(
        &self,
        key: &ContextKey,
        id: pascal_cache::SnapshotId,
    ) -> Option<crate::workspace::baseline::BaselineHint> {
        let mut store = self.index();
        let stored = store.contexts.get_mut(key)?;
        if stored
            .baseline
            .as_ref()
            .is_some_and(|hint| hint.header().id() == id)
        {
            stored.baseline.take()
        } else {
            None
        }
    }

    /// Whether `key` was not sealed (or found unchanged) within `interval`
    /// before `now`.
    pub(crate) fn baseline_seal_due(
        &self,
        key: &ContextKey,
        now: std::time::Instant,
        interval: std::time::Duration,
    ) -> bool {
        self.index().contexts.get(key).is_some_and(|stored| {
            stored
                .last_seal
                .is_none_or(|last| now.saturating_duration_since(last) >= interval)
        })
    }

    pub(crate) fn note_baseline_seal(&self, key: &ContextKey, now: std::time::Instant) {
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.last_seal = Some(now);
        }
    }

    /// Whether this process looked for a snapshot to start `key`'s index
    /// from; `true` when `key` holds no index (nothing to start).
    pub(crate) fn baseline_adoption_tried(&self, key: &ContextKey) -> bool {
        self.index()
            .contexts
            .get(key)
            .is_none_or(|stored| stored.hint_tried)
    }

    pub(crate) fn note_baseline_adoption_tried(&self, key: &ContextKey) {
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.hint_tried = true;
        }
    }

    /// The holds evicted contexts left, for a store job to release.
    pub(crate) fn take_released_holds(&self) -> Vec<pascal_cache::SnapshotId> {
        std::mem::take(&mut self.index().released)
    }

    /// `read` runs under the index lock: it must not call back into the cache.
    pub(crate) fn read_index<R>(
        &self,
        key: &ContextKey,
        read: impl FnOnce(&ContextIndex, &IndexMembership) -> R,
    ) -> Option<R> {
        let mut store = self.index();
        let clock = store.tick();
        let stored = store.contexts.get_mut(key)?;
        stored.last_used = clock;
        Some(read(&stored.index, &stored.membership))
    }

    /// Whether the store holds an index of `key`; its use is not recorded.
    pub(crate) fn holds_index(&self, key: &ContextKey) -> bool {
        self.index().contexts.contains_key(key)
    }

    #[cfg(test)]
    pub(crate) fn with_index_membership_context_for_test(
        &self,
        key: &ContextKey,
        context: pascal_cache::Digest,
    ) {
        if let Some(stored) = self.index().contexts.get_mut(key) {
            stored.membership.context = Some(context);
        }
    }

    /// Whether no index is stored under `key` but one is stored under a key
    /// that differs from it only in the runtime selection (A8): `key`'s
    /// context resolved again under another selection.
    pub(crate) fn index_moved_by_selection(&self, key: &ContextKey) -> bool {
        let store = self.index();
        !store.contexts.contains_key(key)
            && store
                .contexts
                .keys()
                .any(|old| old.differs_only_in_selection(key))
    }

    /// A8: when no index is stored under `key`, the most recently used
    /// index stored under a key that differs from it only in the runtime
    /// selection (the same project resolved before `pascal/selectProject`
    /// selected it) moves to `key` instead of being built again, provided
    /// its membership was computed for the persisted context `context`.
    /// Otherwise it is left to eviction. The directory owners recorded
    /// against the old key are forgotten, as on eviction. Returns whether
    /// an index moved.
    pub(crate) fn rekey_index_selection(
        &self,
        key: &ContextKey,
        context: pascal_cache::Digest,
    ) -> bool {
        let mut store = self.index();
        if store.contexts.contains_key(key) {
            return false;
        }
        let Some(old) = store
            .contexts
            .iter()
            .filter(|(old, _)| old.differs_only_in_selection(key))
            .max_by_key(|(_, stored)| stored.last_used)
            .filter(|(_, stored)| stored.membership.context == Some(context))
            .map(|(old, _)| old.clone())
        else {
            return false;
        };
        let Some(stored) = store.contexts.remove(&old) else {
            return false;
        };
        store.contexts.insert(key.clone(), stored);
        store.owners.retain(|_, owner| *owner != old);
        true
    }

    /// Whether the store holds an index built for `anchor`.
    pub(crate) fn holds_index_for(&self, anchor: &IndexAnchor) -> bool {
        self.index()
            .contexts
            .values()
            .any(|stored| stored.anchor == *anchor)
    }

    pub(crate) fn request_index(&self, anchor: IndexAnchor) {
        self.index().request(&anchor);
    }

    pub(crate) fn take_index_requests(&self) -> Vec<IndexAnchor> {
        std::mem::take(&mut self.index().requests)
    }

    pub(crate) fn index_memory_exhausted(&self, key: &ContextKey) -> bool {
        self.index()
            .contexts
            .get(key)
            .is_some_and(|stored| stored.memory_exhausted)
    }

    pub(crate) fn owner_of(&self, directory: &Path) -> Option<ContextKey> {
        self.index().owners.get(directory).cloned()
    }

    #[cfg(test)]
    pub(crate) fn remember_owner(&self, directory: PathBuf, key: ContextKey) {
        self.remember_owner_claimable(directory, key, false);
    }

    /// Remember `directory`'s owner; `claimable` when the owner is a D43
    /// claim or projectless, so a new claimant list reassigns it.
    pub(crate) fn remember_owner_claimable(
        &self,
        directory: PathBuf,
        key: ContextKey,
        claimable: bool,
    ) {
        let mut store = self.index();
        if store.owners.len() >= MAX_OWNERS && !store.owners.contains_key(&directory) {
            store.owners.clear();
            store.claimable_owners.clear();
        }
        if claimable {
            store.claimable_owners.insert(directory.clone());
        } else {
            store.claimable_owners.remove(&directory);
        }
        store.owners.insert(directory, key);
    }

    /// The main workspace's current D43 claim generation.
    pub(crate) fn claim_generation(&self) -> u64 {
        self.index().claim_generation
    }

    /// Record the main workspace's new D43 claim generation.
    pub(crate) fn set_claim_generation(&self, generation: u64) {
        self.index().claim_generation = generation;
    }

    /// D43: a new claimant list: forget the directory owners that were
    /// claims or projectless, dirtying the memberships listing them.
    pub(crate) fn reassign_claimable_owners(&self) {
        let directories = {
            let mut store = self.index();
            std::mem::take(&mut store.claimable_owners)
                .into_iter()
                .collect::<Vec<_>>()
        };
        self.reassign_owners(&directories);
    }

    /// D43: the owners of `directories` changed because a claimant project
    /// appeared or left. Forget them and dirty the memberships that list
    /// them, as a change to an entry of each directory would.
    pub(crate) fn reassign_owners(&self, directories: &[PathBuf]) {
        for directory in directories {
            // A child name no file has: only the listing and owner effects
            // of `invalidate_index` apply.
            self.invalidate_index(&directory.join("\u{0}d43-owner"), true);
        }
    }

    /// `path` changed: mark the members it is (or includes into) stale, and
    /// the memberships whose listing it may have changed dirty.
    fn invalidate_index(&self, path: &Path, include_parent: bool) {
        let mut store = self.index();
        let parent = path.parent();
        let mut touched = Vec::new();
        for stored in store.contexts.values_mut() {
            let stale = stored.index.mark_stale(path);
            let listed = include_parent
                && stored
                    .membership
                    .scopes
                    .iter()
                    .any(|(scope, _)| match scope.depth {
                        Depth::Listing => {
                            parent == Some(scope.root.as_path()) || path == scope.root
                        }
                        Depth::Recursive => path != scope.root && path.starts_with(&scope.root),
                    });
            let newly_dirty = listed && !stored.dirty;
            stored.dirty |= listed;
            if stale || newly_dirty {
                touched.push(stored.anchor.clone());
            }
        }
        for anchor in &touched {
            store.request(anchor);
        }
        if let Some(parent) = parent {
            store.owners.remove(parent);
        }
    }

    /// `path` changed: neither it nor a crawl publication whose evidence
    /// names it (or, for a listing change, its directory) is known
    /// published any more.
    fn forget_crawl_published(&self, path: &Path, include_parent: bool) {
        let mut store = self.index();
        let store = &mut *store;
        store.crawl_published.remove(path);
        store.over_limit.remove(path);
        let changed = std::iter::once(path).chain(path.parent().filter(|_| include_parent));
        for changed in changed {
            for dependent in store.crawl_dependents.remove(changed).unwrap_or_default() {
                store.crawl_published.remove(&dependent);
            }
        }
    }

    fn invalidate_index_all(&self) {
        let mut store = self.index();
        // Overflow lost which paths changed: nothing is known published.
        store.crawl_published.clear();
        store.crawl_dependents.clear();
        store.over_limit.clear();
        let mut anchors = Vec::new();
        for stored in store.contexts.values_mut() {
            stored.index.mark_all_stale();
            stored.dirty = true;
            anchors.push(stored.anchor.clone());
        }
        for anchor in &anchors {
            store.request(anchor);
        }
        store.owners.clear();
    }
}

#[cfg(test)]
impl ProjectCache {
    pub(crate) fn set_index_unit_limit_for_test(&self, units: usize) {
        self.index().unit_limit = units;
    }

    pub(crate) fn indexed_context_count(&self) -> usize {
        self.index().contexts.len()
    }
}

#[cfg(all(test, unix))]
mod witness_tests {
    use super::*;
    use pascal_cache::{Digest, SourceContent};

    fn settled(path: &Path, bytes: &[u8]) -> ProjectReadStamp {
        std::fs::write(path, bytes).unwrap();
        pascal_project::backdate_for_test(path);
        pascal_project::path_stamp_result(path).unwrap().unwrap()
    }

    fn content(bytes: &[u8]) -> Proven {
        Proven::Content(SourceContent::from_bytes(bytes))
    }

    #[test]
    fn a_witness_holds_while_its_stamp_holds() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.inc");
        let stamp = settled(&file, b"{A}");
        let cache = ProjectCache::default();
        cache.record_witness(&file, stamp.clone(), content(b"{A}"));
        assert_eq!(cache.witness(&file, &stamp), Some(content(b"{A}")));
        std::fs::write(&file, b"{B}").unwrap();
        let now = pascal_project::path_stamp_result(&file).unwrap().unwrap();
        assert_eq!(cache.witness(&file, &now), None);
    }

    #[test]
    fn an_unsettled_stamp_is_never_witnessed() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.inc");
        std::fs::write(&file, b"{A}").unwrap();
        let stamp = pascal_project::path_stamp_result(&file).unwrap().unwrap();
        let cache = ProjectCache::default();
        cache.record_witness(&file, stamp.clone(), content(b"{A}"));
        assert_eq!(cache.witness(&file, &stamp), None);
    }

    #[test]
    fn watcher_events_drop_the_witnesses_they_concern() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.inc");
        let file_stamp = settled(&file, b"{A}");
        pascal_project::backdate_for_test(temp.path());
        let directory_stamp = pascal_project::path_stamp_result(temp.path())
            .unwrap()
            .unwrap();
        let listing = Proven::Listing(Digest::from_bytes([1; 32]));
        let cache = ProjectCache::default();
        let record = || {
            cache.record_witness(&file, file_stamp.clone(), content(b"{A}"));
            cache.record_witness(temp.path(), directory_stamp.clone(), listing.clone());
        };
        record();
        cache.invalidate_file_contents(&file);
        assert_eq!(cache.witness(&file, &file_stamp), None);
        assert_eq!(
            cache.witness(temp.path(), &directory_stamp),
            Some(listing.clone()),
            "a content change keeps the parent's listing"
        );
        record();
        cache.invalidate_path(&file);
        assert_eq!(cache.witness(&file, &file_stamp), None);
        assert_eq!(cache.witness(temp.path(), &directory_stamp), None);
        record();
        cache.invalidate_after_overflow();
        assert_eq!(cache.witness(&file, &file_stamp), None);
        assert_eq!(cache.witness(temp.path(), &directory_stamp), None);
    }

    #[test]
    fn witnesses_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.inc");
        let stamp = settled(&file, b"{A}");
        let cache = ProjectCache::default();
        for index in 0..MAX_WITNESSES {
            cache.record_witness(
                &temp.path().join(format!("{index}.inc")),
                stamp.clone(),
                content(b"{A}"),
            );
        }
        cache.record_witness(&file, stamp.clone(), content(b"{A}"));
        assert_eq!(
            cache.witness(&file, &stamp),
            None,
            "past the bound nothing is recorded"
        );
    }
}

#[cfg(test)]
fn listed_catalogue(
    directory: &Path,
) -> (
    crate::catalogue::CatalogueKey,
    Arc<crate::catalogue::Catalogue>,
) {
    use crate::catalogue::{
        Bound, Catalogue, CatalogueKey, Depth, Exclusions, MAX_CATALOGUE_ENTRIES,
    };
    let mut io = || -> Result<(), String> { Ok(()) };
    let mut catalogue = Catalogue::new(directory, Depth::Recursive, &mut io).unwrap();
    catalogue
        .advance(Bound::entries(MAX_CATALOGUE_ENTRIES), &|_| false, &mut io)
        .unwrap();
    let key = CatalogueKey {
        root: directory.to_path_buf(),
        depth: Depth::Recursive,
        exclusions: Exclusions::Nothing,
    };
    (key, Arc::new(catalogue))
}

#[cfg(test)]
mod catalogue_tests {
    use super::*;
    use crate::catalogue::MAX_SHARED_CATALOGUES;

    fn tree() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("sub")).unwrap();
        std::fs::write(temp.path().join("A.pas"), b"").unwrap();
        std::fs::write(temp.path().join("sub/B.pas"), b"").unwrap();
        temp
    }

    #[test]
    fn a_published_catalogue_is_shared_by_every_clone_of_the_cache() {
        let temp = tree();
        let cache = ProjectCache::new(1 << 30);
        let (key, catalogue) = listed_catalogue(temp.path());
        let generation = cache
            .publish_catalogue(key.clone(), None, Arc::clone(&catalogue))
            .expect("retained");
        let shared = cache.clone().catalogue(&key).expect("shared");
        assert!(Arc::ptr_eq(&shared.catalogue, &catalogue));
        assert_eq!(shared.generation, generation);
    }

    #[test]
    fn a_catalogue_is_charged_until_it_is_removed() {
        let temp = tree();
        let cache = ProjectCache::new(1 << 30);
        let (key, catalogue) = listed_catalogue(temp.path());
        let bytes = catalogue.retained_bytes();
        let generation = cache
            .publish_catalogue(key.clone(), None, catalogue)
            .unwrap();
        assert_eq!(cache.stats().reserved_catalogue_bytes, bytes);
        assert_eq!(cache.stats().reserved_bytes(), bytes);
        cache.drop_catalogue(&key, generation.wrapping_add(1));
        assert!(
            cache.catalogue(&key).is_some(),
            "a stale generation drops nothing"
        );
        cache.drop_catalogue(&key, generation);
        assert!(cache.catalogue(&key).is_none());
        assert_eq!(cache.stats().reserved_catalogue_bytes, 0);
    }

    #[test]
    fn a_catalogue_over_its_share_of_the_budget_is_not_retained() {
        let temp = tree();
        let (key, catalogue) = listed_catalogue(temp.path());
        let bytes = catalogue.retained_bytes();
        let cache = ProjectCache::new(bytes * CATALOGUE_BUDGET_SHARE - CATALOGUE_BUDGET_SHARE);
        assert_eq!(cache.publish_catalogue(key.clone(), None, catalogue), None);
        assert!(cache.catalogue(&key).is_none());
        assert_eq!(cache.stats().reserved_catalogue_bytes, 0);
    }

    #[test]
    fn a_stale_base_never_replaces_the_stored_catalogue() {
        let temp = tree();
        let cache = ProjectCache::new(1 << 30);
        let (key, catalogue) = listed_catalogue(temp.path());
        let first = cache
            .publish_catalogue(key.clone(), None, Arc::clone(&catalogue))
            .unwrap();
        assert_eq!(
            cache.publish_catalogue(key.clone(), None, Arc::clone(&catalogue)),
            None,
            "a new build never replaces a stored one"
        );
        let second = cache
            .publish_catalogue(key.clone(), Some(first), Arc::clone(&catalogue))
            .expect("an extension of the stored catalogue replaces it");
        assert_eq!(
            cache.publish_catalogue(key.clone(), Some(first), Arc::clone(&catalogue)),
            None
        );
        cache.drop_catalogue(&key, second);
        assert_eq!(
            cache.publish_catalogue(key.clone(), Some(second), catalogue),
            None,
            "an extension of an invalidated catalogue is not stored"
        );
        assert_eq!(cache.stats().reserved_catalogue_bytes, 0);
    }

    #[test]
    fn the_store_keeps_the_most_recently_used_catalogues() {
        let temp = tree();
        let cache = ProjectCache::new(1 << 40);
        let (key, catalogue) = listed_catalogue(temp.path());
        let keyed = |index: usize| crate::catalogue::CatalogueKey {
            root: key.root.join(format!("view{index}")),
            ..key.clone()
        };
        for index in 0..=MAX_SHARED_CATALOGUES {
            cache
                .publish_catalogue(keyed(index), None, Arc::clone(&catalogue))
                .unwrap();
        }
        assert_eq!(cache.catalogue_count(), MAX_SHARED_CATALOGUES);
        assert!(cache.catalogue(&keyed(0)).is_none());
        assert!(cache.catalogue(&keyed(MAX_SHARED_CATALOGUES)).is_some());
        assert_eq!(
            cache.stats().reserved_catalogue_bytes,
            MAX_SHARED_CATALOGUES * catalogue.retained_bytes()
        );
    }

    #[test]
    fn a_listing_change_drops_the_catalogues_that_listed_the_parent() {
        let temp = tree();
        let cache = ProjectCache::new(1 << 30);
        let (key, catalogue) = listed_catalogue(temp.path());
        let before = cache.catalogue_generation();
        cache
            .publish_catalogue(key.clone(), None, catalogue)
            .unwrap();
        cache.invalidate_path(&temp.path().join("sub/New.pas"));
        assert!(cache.catalogue(&key).is_none());
        assert_eq!(cache.stats().reserved_catalogue_bytes, 0);
        assert!(cache.catalogue_generation() > before + 1);
    }

    #[test]
    fn a_content_change_keeps_catalogues_unless_it_names_a_listed_directory() {
        let temp = tree();
        let cache = ProjectCache::new(1 << 30);
        let (key, catalogue) = listed_catalogue(temp.path());
        cache
            .publish_catalogue(key.clone(), None, catalogue)
            .unwrap();
        cache.invalidate_file_contents(&temp.path().join("A.pas"));
        assert!(
            cache.catalogue(&key).is_some(),
            "a file's bytes are not membership"
        );
        cache.invalidate_file_contents(&temp.path().join("sub"));
        assert!(
            cache.catalogue(&key).is_none(),
            "a listed directory's metadata changed"
        );
    }

    #[test]
    fn a_change_inside_an_excluded_directory_keeps_the_catalogue() {
        use crate::catalogue::{Bound, Catalogue, Depth, MAX_CATALOGUE_ENTRIES};
        let temp = tree();
        std::fs::create_dir_all(temp.path().join("node_modules/pkg")).unwrap();
        let mut io = || -> Result<(), String> { Ok(()) };
        let mut catalogue = Catalogue::new(temp.path(), Depth::Recursive, &mut io).unwrap();
        catalogue
            .advance(
                Bound::entries(MAX_CATALOGUE_ENTRIES),
                &|path: &Path| path.ends_with("node_modules"),
                &mut io,
            )
            .unwrap();
        let (key, _) = listed_catalogue(temp.path());
        let cache = ProjectCache::new(1 << 30);
        cache
            .publish_catalogue(key.clone(), None, Arc::new(catalogue))
            .unwrap();
        cache.invalidate_path(&temp.path().join("node_modules/pkg/index.js"));
        assert!(cache.catalogue(&key).is_some());
        cache.invalidate_path(&temp.path().join("node_modules"));
        assert!(
            cache.catalogue(&key).is_none(),
            "the root's own listing changed"
        );
    }

    #[test]
    fn overflow_drops_every_catalogue_and_releases_its_charge() {
        let temp = tree();
        let cache = ProjectCache::new(1 << 30);
        let (key, catalogue) = listed_catalogue(temp.path());
        let other = crate::catalogue::CatalogueKey {
            root: key.root.join("other"),
            ..key.clone()
        };
        cache
            .publish_catalogue(key.clone(), None, Arc::clone(&catalogue))
            .unwrap();
        cache
            .publish_catalogue(other.clone(), None, catalogue)
            .unwrap();
        cache.invalidate_after_overflow();
        assert_eq!(cache.catalogue_count(), 0);
        assert_eq!(cache.stats().reserved_catalogue_bytes, 0);
    }
}

#[cfg(test)]
mod index_tests {
    use super::*;
    use crate::workspace::ContextKey;
    use crate::workspace_index::{CandidateQuery, MemberFacts, MemberStamp};
    use pascal_core::facts::{ArtifactFacts, PostingFact};

    fn key(name: &str) -> ContextKey {
        ContextKey::for_test(name)
    }

    fn anchor(name: &str) -> IndexAnchor {
        IndexAnchor::Project(PathBuf::from(format!("/anchors/{name}")))
    }

    fn membership(paths: &[&str]) -> IndexMembership {
        IndexMembership {
            paths: paths.iter().map(PathBuf::from).collect(),
            complete: true,
            ..IndexMembership::default()
        }
    }

    fn membership_cost(paths: &[&str]) -> usize {
        paths
            .iter()
            .map(|path| crate::workspace_index::membership_charge(Path::new(path)))
            .sum()
    }

    fn facts_with(path: &str, posting: &str, slots: Vec<Option<PathBuf>>) -> MemberFacts {
        let facts = ArtifactFacts {
            postings: vec![PostingFact {
                name: posting.to_owned(),
                occurrences: Vec::new(),
            }],
            ..ArtifactFacts::default()
        };
        MemberFacts::from_facts(Path::new(path), &facts, slots)
    }

    fn facts(path: &str) -> MemberFacts {
        facts_with(path, "thing", Vec::new())
    }

    fn charge(facts: &MemberFacts) -> usize {
        crate::workspace_index::ContextIndex::new().charge_for(facts, &[])
    }

    /// What a member's names cost its context, charged once.
    fn names(facts: &MemberFacts) -> usize {
        charge(facts) - crate::workspace_index::ContextIndex::member_charge(facts, &[])
    }

    fn stamps() -> Vec<MemberStamp> {
        Vec::new()
    }

    fn publish(cache: &ProjectCache, key: &ContextKey, path: &str) -> PublishOutcome {
        cache.publish_index_member(key, Path::new(path), facts(path), stamps(), 1)
    }

    fn unverified(cache: &ProjectCache, key: &ContextKey) -> Vec<PathBuf> {
        cache
            .read_index(key, |index, _| {
                index
                    .candidates(&CandidateQuery {
                        names: Vec::new(),
                        top_level_units: None,
                        aliases: Vec::new(),
                    })
                    .unverified
            })
            .expect("context is stored")
    }

    fn generation(cache: &ProjectCache, key: &ContextKey) -> u64 {
        cache
            .read_index(key, |index, _| index.generation())
            .expect("context is stored")
    }

    #[test]
    fn the_largest_index_status_fits_eight_kibibytes() {
        let cache = ProjectCache::new(1 << 30);
        for index in 0..MAX_INDEXED_CONTEXTS {
            let name = format!("k{index}");
            let mut limited = membership(&["/w/A.pas"]);
            limited.complete = false;
            limited.packages = true;
            limited.limit = Some("x".repeat(400));
            cache.set_index_membership(
                key(&name),
                IndexAnchor::Project(PathBuf::from(format!("/{}/{name}", "d".repeat(4096)))),
                limited,
                usize::MAX,
            );
        }
        let entries = cache.index_status(&|_| false);
        assert_eq!(entries.len(), MAX_INDEXED_CONTEXTS);
        for entry in &entries {
            assert!(entry.anchor_truncated);
            assert!(entry.gaps.len() <= 4, "{:?}", entry.gaps);
            assert!(entry.gaps.iter().all(|gap| gap.len() <= 160));
            assert!(!entry.complete);
        }
        let text = index_status_json(&entries, Default::default()).to_string();
        assert!(text.len() <= 8192, "{} bytes", text.len());
        assert!(text.contains("\"anchorTruncated\":true"), "{text}");
        assert!(!text.contains("/w/A.pas"));
    }

    /// 13b-3 (A2, N1): the process's crawl publications, crawl deferrals to
    /// the index batch and provider interfaces refused for overflowed reads
    /// are reported beside the contexts.
    #[test]
    fn index_status_reports_crawl_publications_and_deferrals() {
        let cache = ProjectCache::new(1 << 30);
        let digest = pascal_cache::Digest::from_bytes([3; 32]);
        cache.note_published(Path::new("/w/A.pas"), 1, digest, &[]);
        cache.note_published(Path::new("/w/B.pas"), 1, digest, &[]);
        cache.note_crawl_deferred(Path::new("/w/C.pas"));
        let counts = cache.index_process_counts();
        assert_eq!(
            (counts.crawl_published, counts.crawl_deferred),
            (2, 1),
            "{counts:?}"
        );
        let json = index_status_json(&cache.index_status(&|_| false), counts);
        assert_eq!(json["crawlPublished"], 2, "{json}");
        assert_eq!(json["crawlDeferred"], 1, "{json}");
        assert_eq!(json["providerGraphsRefused"], 0, "{json}");
    }

    fn membership_under(paths: &[&str], context: pascal_cache::Digest) -> IndexMembership {
        IndexMembership {
            context: Some(context),
            ..membership(paths)
        }
    }

    /// 13b-4 (A3): `persisted` counts the members of the context whose
    /// units the crawl published under its digest, before any index batch.
    #[test]
    fn persisted_counts_crawl_publications_of_the_context() {
        let cache = ProjectCache::new(1 << 30);
        let k = key("a");
        let mine = pascal_cache::Digest::from_bytes([3; 32]);
        let other = pascal_cache::Digest::from_bytes([4; 32]);
        let paths = ["/w/A.pas", "/w/B.pas", "/w/C.pas", "/w/D.pas"];
        cache.set_index_membership(
            k.clone(),
            anchor("a"),
            membership_under(&paths, mine),
            usize::MAX,
        );
        cache.note_published(Path::new("/w/A.pas"), 1, mine, &[]);
        cache.note_published(Path::new("/w/B.pas"), 1, mine, &[]);
        cache.note_published(Path::new("/w/C.pas"), 1, other, &[]);
        cache.note_published(Path::new("/w/NotMember.pas"), 1, mine, &[]);
        let counts = cache.index_status(&|_| false)[0].counts;
        assert_eq!(counts.persisted, 2, "{counts:?}");
        assert_eq!(counts.pending, 4, "{counts:?}");
    }

    /// 13b-4: a member published by both the index and the crawl counts
    /// once; a Fresh member whose crawl publication is of other bytes, and
    /// an over-limit member (no artifact), do not count.
    #[test]
    fn persisted_counts_each_member_once_and_only_with_an_artifact() {
        let cache = ProjectCache::new(1 << 30);
        let k = key("a");
        let mine = pascal_cache::Digest::from_bytes([3; 32]);
        let paths = ["/w/Both.pas", "/w/OtherBytes.pas", "/w/Big.pas"];
        cache.set_index_membership(
            k.clone(),
            anchor("a"),
            membership_under(&paths, mine),
            usize::MAX,
        );
        for path in paths {
            assert_eq!(publish(&cache, &k, path), PublishOutcome::Indexed);
        }
        {
            let mut store = cache.index();
            let index = &mut store.contexts.get_mut(&k).unwrap().index;
            assert!(index.set_record(
                Path::new("/w/Both.pas"),
                pascal_cache::Digest::from_bytes([9; 32])
            ));
            assert!(index.set_record(
                Path::new("/w/Big.pas"),
                pascal_cache::Digest::from_bytes([8; 32])
            ));
            index.mark_over_limit(Path::new("/w/Big.pas"));
        }
        cache.note_published(Path::new("/w/Both.pas"), 1, mine, &[]);
        cache.note_published(Path::new("/w/OtherBytes.pas"), 2, mine, &[]);
        let counts = cache.index_status(&|_| false)[0].counts;
        assert_eq!(counts.persisted, 1, "{counts:?}");
        assert_eq!(counts.over_limit, 1, "{counts:?}");
    }

    /// Task 10b: a member reused from the store is never counted as
    /// extracted, not even between its publication and its mark: the
    /// status reads the index between the two (protocol tests
    /// `a_second_server_fills_the_index_from_the_store_without_extracting`,
    /// `two_servers_index_the_same_workspace`).
    #[test]
    fn a_reused_member_is_published_reused() {
        let cache = ProjectCache::new(1 << 30);
        let k = key("a");
        cache.set_index_membership(
            k.clone(),
            anchor("a"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        assert_eq!(
            cache.publish_reused_index_member(
                &k,
                Path::new("/w/A.pas"),
                facts("/w/A.pas"),
                stamps(),
                1
            ),
            PublishOutcome::Indexed
        );
        let counts = cache.index_status(&|_| false)[0].counts;
        assert_eq!((counts.reused, counts.extracted), (1, 0), "{counts:?}");
    }

    /// A8 (13b-6 fix round 1): an index re-keyed for a new selection moves
    /// only when its membership was computed for the same persisted
    /// context, and the most recently used of several candidates moves.
    #[test]
    fn a_selection_rekey_checks_the_context_and_takes_the_latest_index() {
        let cache = ProjectCache::new(1 << 30);
        let base = key("a");
        let selected = base
            .clone()
            .with_selection_for_test(Path::new("/w"), Path::new("/w/A.dproj"));
        let older = base
            .clone()
            .with_selection_for_test(Path::new("/w"), Path::new("/w/Old.dproj"));
        let context = pascal_cache::Digest::from_bytes([1; 32]);
        let with_context = |paths: &[&str]| IndexMembership {
            context: Some(context),
            ..membership(paths)
        };
        cache.set_index_membership(
            older.clone(),
            anchor("a"),
            with_context(&["/w/O.pas"]),
            usize::MAX,
        );
        cache.set_index_membership(
            base.clone(),
            anchor("a"),
            with_context(&["/w/U.pas"]),
            usize::MAX,
        );
        assert!(cache.index_moved_by_selection(&selected));
        assert!(!cache.rekey_index_selection(&selected, pascal_cache::Digest::from_bytes([2; 32])));
        assert!(cache.holds_index(&base) && !cache.holds_index(&selected));
        assert!(cache.rekey_index_selection(&selected, context));
        assert!(
            !cache.holds_index(&base),
            "the most recently used index moved"
        );
        assert!(cache.holds_index(&older));
        let moved = cache
            .read_index(&selected, |index, _| index.member_paths())
            .unwrap();
        assert_eq!(moved, [PathBuf::from("/w/U.pas")]);
        assert!(!cache.index_moved_by_selection(&selected));
    }

    #[test]
    fn index_status_counts_members_by_state() {
        let cache = ProjectCache::new(1 << 30);
        let k = key("a");
        let paths = [
            "/w/Fresh.pas",
            "/w/Refused.pas",
            "/w/Limited.pas",
            "/w/Pending.pas",
        ];
        cache.set_index_membership(k.clone(), anchor("a"), membership(&paths), usize::MAX);
        assert_eq!(publish(&cache, &k, "/w/Fresh.pas"), PublishOutcome::Indexed);
        {
            let mut store = cache.index();
            let stored = store.contexts.get_mut(&k).unwrap();
            stored
                .index
                .refuse(Path::new("/w/Refused.pas"), "r".to_owned());
            stored.index.limit(Path::new("/w/Limited.pas"), false);
            stored.validation_reads = 7;
        }
        let entries = cache.index_status(&|_| false);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(
            (
                entry.counts.members,
                entry.counts.fresh,
                entry.counts.refused,
                entry.counts.limited,
                entry.counts.pending
            ),
            (4, 1, 1, 1, 1)
        );
        assert_eq!(entry.validation_reads, 7);
        assert_eq!(entry.snapshot, "none");
        assert!(entry.membership_complete);
        assert!(!entry.complete, "a pending member is not complete");
        assert!(
            entry
                .gaps
                .iter()
                .any(|gap| gap.starts_with("1 units could not be indexed")),
            "{:?}",
            entry.gaps
        );
        let json = index_status_json(&entries, Default::default());
        let context = &json["contexts"][0];
        assert_eq!(json["schema"], 1);
        assert_eq!(context["anchor"]["project"], "file:///anchors/a");
        assert_eq!(context["anchorTruncated"], false);
        assert_eq!(context["members"], 4);
        assert_eq!(context["pending"], 1);
        assert_eq!(context["refused"], 1);
        assert_eq!(context["limited"], 1);
        assert_eq!(context["snapshot"], "none");
        assert_eq!(context["validationReads"], 7);
        assert!(!json.to_string().contains("Fresh.pas"));
    }

    /// Without persistence an open member is answered from its buffer and
    /// never indexed (l4d-33): it does not hold `complete` back, or a
    /// memory-only index with an open member would never be complete
    /// (protocol test `runtime_disable_is_acknowledged_then_performs_no_cache_io`).
    #[test]
    fn index_status_does_not_wait_for_open_members() {
        let cache = ProjectCache::new(1 << 30);
        let k = key("a");
        let paths = ["/w/Fresh.pas", "/w/Open.pas", "/w/Pending.pas"];
        cache.set_index_membership(k.clone(), anchor("a"), membership(&paths), usize::MAX);
        assert_eq!(publish(&cache, &k, "/w/Fresh.pas"), PublishOutcome::Indexed);
        let is_open = |path: &Path| path == Path::new("/w/Open.pas");
        let entry = &cache.index_status(&is_open)[0];
        assert_eq!(entry.counts.pending, 2);
        assert!(!entry.complete, "Pending.pas is still to index");
        assert_eq!(
            publish(&cache, &k, "/w/Pending.pas"),
            PublishOutcome::Indexed
        );
        let entry = &cache.index_status(&is_open)[0];
        assert_eq!(entry.counts.pending, 1, "the count stays the index's own");
        assert!(entry.complete, "only the open member is pending");
        assert!(!cache.index_status(&|_| false)[0].complete);
    }

    #[test]
    fn index_status_lists_the_most_recently_used_context_first() {
        let cache = ProjectCache::new(1 << 30);
        for name in ["a", "b"] {
            cache.set_index_membership(
                key(name),
                anchor(name),
                membership(&["/w/A.pas"]),
                usize::MAX,
            );
        }
        cache.read_index(&key("a"), |_, _| ());
        let entries = cache.index_status(&|_| false);
        assert_eq!(entries[0].anchor, "file:///anchors/a");
        assert_eq!(entries[1].anchor, "file:///anchors/b");
        assert!(!entries[0].complete, "the member is still pending");
        assert_eq!(entries[0].counts.pending, 1);
        assert!(
            index_status_json(&[], Default::default())["contexts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn index_bytes_are_charged_before_insertion_and_released_on_removal() {
        let cache = ProjectCache::new(1 << 30);
        let a = key("a");
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas", "/w/B.pas"]),
            usize::MAX,
        );
        let base = cache.stats().reserved_index_bytes;
        assert_eq!(base, membership_cost(&["/w/A.pas", "/w/B.pas"]));
        let member = facts("/w/A.pas");
        let expected = charge(&member);
        assert_eq!(
            cache.publish_index_member(&a, Path::new("/w/A.pas"), member, stamps(), 1),
            PublishOutcome::Indexed
        );
        assert_eq!(cache.stats().reserved_index_bytes, base + expected);
        assert_eq!(cache.stats().reserved_bytes(), base + expected);
        // A's names stay with the context until its interner is compacted.
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/B.pas"]),
            usize::MAX,
        );
        assert_eq!(
            cache.stats().reserved_index_bytes,
            membership_cost(&["/w/B.pas"]) + names(&facts("/w/A.pas"))
        );
        cache.set_index_membership(a.clone(), anchor("a"), membership(&[]), usize::MAX);
        assert_eq!(cache.stats().reserved_index_bytes, 0);
    }

    #[test]
    fn a_member_over_the_quarter_share_is_left_unindexed_with_a_gap() {
        let cache = ProjectCache::new(4096);
        let a = key("a");
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        let held = cache.stats().reserved_index_bytes;
        let big = facts_with("/w/A.pas", &"x".repeat(2048), Vec::new());
        assert_eq!(
            cache.publish_index_member(&a, Path::new("/w/A.pas"), big, stamps(), 1),
            PublishOutcome::Memory
        );
        assert!(cache.index_memory_exhausted(&a));
        assert_eq!(cache.stats().reserved_index_bytes, held);
        assert_eq!(unverified(&cache, &a), vec![PathBuf::from("/w/A.pas")]);
    }

    #[test]
    fn the_least_recently_used_context_is_evicted_once_for_room() {
        let m = membership_cost(&["/w/A.pas"]);
        let c = charge(&facts("/w/A.pas"));
        let share = 3 * m + 2 * c + c / 2;
        let cache = ProjectCache::new(share * 4);
        let (a, b, third) = (key("a"), key("b"), key("c"));
        for (key, name) in [(&a, "a"), (&b, "b")] {
            cache.set_index_membership(
                key.clone(),
                anchor(name),
                membership(&["/w/A.pas"]),
                usize::MAX,
            );
            assert_eq!(publish(&cache, key, "/w/A.pas"), PublishOutcome::Indexed);
        }
        cache.read_index(&a, |_, _| ());
        cache.set_index_membership(
            third.clone(),
            anchor("c"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        assert_eq!(cache.indexed_context_count(), 3);
        assert_eq!(publish(&cache, &third, "/w/A.pas"), PublishOutcome::Indexed);
        assert_eq!(cache.indexed_context_count(), 2);
        assert!(cache.read_index(&a, |_, _| ()).is_some());
        assert!(cache.read_index(&b, |_, _| ()).is_none());
        assert_eq!(cache.stats().reserved_index_bytes, 2 * m + 2 * c);

        let big = facts_with("/w/A.pas", &"x".repeat(share), Vec::new());
        assert_eq!(
            cache.publish_index_member(&third, Path::new("/w/A.pas"), big, stamps(), 1),
            PublishOutcome::Memory
        );
        assert_eq!(cache.indexed_context_count(), 1, "one eviction, not all");
        assert!(cache.read_index(&third, |_, _| ()).is_some());
        assert!(cache.index_memory_exhausted(&third));
    }

    #[test]
    fn the_store_keeps_at_most_eight_contexts() {
        let cache = ProjectCache::new(1 << 30);
        for index in 0..=MAX_INDEXED_CONTEXTS {
            let name = format!("k{index}");
            cache.set_index_membership(
                key(&name),
                anchor(&name),
                membership(&["/w/A.pas"]),
                usize::MAX,
            );
        }
        assert_eq!(cache.indexed_context_count(), MAX_INDEXED_CONTEXTS);
        assert!(cache.read_index(&key("k0"), |_, _| ()).is_none());
        assert!(cache.read_index(&key("k8"), |_, _| ()).is_some());
        assert_eq!(
            cache.stats().reserved_index_bytes,
            MAX_INDEXED_CONTEXTS * membership_cost(&["/w/A.pas"])
        );
    }

    #[test]
    fn two_contexts_sharing_a_document_each_charge_their_member() {
        let cache = ProjectCache::new(1 << 30);
        let (a, b) = (key("a"), key("b"));
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        cache.set_index_membership(
            b.clone(),
            anchor("b"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        let base = cache.stats().reserved_index_bytes;
        let c = charge(&facts("/w/A.pas"));
        assert_eq!(publish(&cache, &a, "/w/A.pas"), PublishOutcome::Indexed);
        assert_eq!(publish(&cache, &b, "/w/A.pas"), PublishOutcome::Indexed);
        assert_eq!(cache.stats().reserved_index_bytes, base + 2 * c);
    }

    #[test]
    fn the_membership_charge_covers_what_a_member_retains() {
        let path = Path::new("/w/A.pas");
        let charge = crate::workspace_index::membership_charge(path);
        let retained = 3 * path.as_os_str().len() + crate::workspace_index::member_slot_bytes();
        assert!(charge >= retained, "{charge} < {retained}");
    }

    #[test]
    fn a_successful_publish_clears_the_memory_gap() {
        let cache = ProjectCache::new(4096);
        let a = key("a");
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        let big = facts_with("/w/A.pas", &"x".repeat(2048), Vec::new());
        assert_eq!(
            cache.publish_index_member(&a, Path::new("/w/A.pas"), big, stamps(), 1),
            PublishOutcome::Memory
        );
        assert!(cache.index_memory_exhausted(&a));
        assert_eq!(publish(&cache, &a, "/w/A.pas"), PublishOutcome::Indexed);
        assert!(!cache.index_memory_exhausted(&a));
    }

    #[test]
    fn a_membership_that_cannot_be_charged_records_the_gap() {
        let cache = ProjectCache::new(256);
        let a = key("a");
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        assert!(cache.index_memory_exhausted(&a));
        assert_eq!(cache.stats().reserved_index_bytes, 0);
    }

    #[test]
    fn a_grown_membership_that_cannot_be_charged_keeps_the_warm_index() {
        let m = membership_cost(&["/w/A.pas"]);
        let c = charge(&facts("/w/A.pas"));
        let cache = ProjectCache::new((m + c + 16) * INDEX_BUDGET_SHARE);
        let a = key("a");
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        assert_eq!(publish(&cache, &a, "/w/A.pas"), PublishOutcome::Indexed);
        let held = cache.stats().reserved_index_bytes;
        let grown = (0..20)
            .map(|index| format!("/w/U{index:02}.pas"))
            .chain(["/w/A.pas".to_owned()])
            .collect::<Vec<_>>();
        let grown = grown.iter().map(String::as_str).collect::<Vec<_>>();
        cache.set_index_membership(a.clone(), anchor("a"), membership(&grown), usize::MAX);
        let (paths, complete, members) = cache
            .read_index(&a, |index, membership| {
                (
                    membership.paths.clone(),
                    membership.complete,
                    index.member_paths(),
                )
            })
            .expect("the context stays stored");
        assert_eq!(paths, [PathBuf::from("/w/A.pas")]);
        assert_eq!(members, [PathBuf::from("/w/A.pas")]);
        assert!(!complete);
        assert!(unverified(&cache, &a).is_empty(), "the member stays fresh");
        assert!(cache.index_memory_exhausted(&a));
        assert!(!cache.index_membership_current(&a));
        assert_eq!(cache.stats().reserved_index_bytes, held);
    }

    #[test]
    fn a_path_outside_the_membership_is_not_charged() {
        let cache = ProjectCache::new(1 << 30);
        let a = key("a");
        assert_eq!(publish(&cache, &a, "/w/A.pas"), PublishOutcome::NotAMember);
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        let base = cache.stats().reserved_index_bytes;
        assert_eq!(publish(&cache, &a, "/w/Z.pas"), PublishOutcome::NotAMember);
        assert_eq!(cache.stats().reserved_index_bytes, base);
    }

    #[test]
    fn refusing_and_replacing_a_member_releases_its_old_charge() {
        let cache = ProjectCache::new(1 << 30);
        let a = key("a");
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        let base = cache.stats().reserved_index_bytes;
        publish(&cache, &a, "/w/A.pas");
        publish(&cache, &a, "/w/A.pas");
        assert_eq!(
            cache.stats().reserved_index_bytes,
            base + charge(&facts("/w/A.pas"))
        );
        cache.refuse_index_member(&a, Path::new("/w/A.pas"), "too big".into());
        assert_eq!(
            cache.stats().reserved_index_bytes,
            base + names(&facts("/w/A.pas"))
        );
    }

    #[test]
    fn members_sharing_names_charge_them_once_per_context() {
        let cache = ProjectCache::new(1 << 30);
        let a = key("a");
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas", "/w/B.pas"]),
            usize::MAX,
        );
        let base = cache.stats().reserved_index_bytes;
        publish(&cache, &a, "/w/A.pas");
        let first = cache.stats().reserved_index_bytes - base;
        assert_eq!(first, charge(&facts("/w/A.pas")));
        publish(&cache, &a, "/w/B.pas");
        let second = cache.stats().reserved_index_bytes - base - first;
        // B shares the posting; only its file stem is new.
        assert!(second < charge(&facts("/w/B.pas")), "{second}");
        assert_eq!(
            cache.stats().reserved_index_bytes,
            cache.read_index(&a, |index, _| index.bytes()).unwrap()
                + membership_cost(&["/w/A.pas", "/w/B.pas"])
        );
    }

    #[test]
    fn invalidating_a_member_path_marks_it_stale_and_bumps_the_generation() {
        let cache = ProjectCache::new(1 << 30);
        let a = key("a");
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        publish(&cache, &a, "/w/A.pas");
        assert!(unverified(&cache, &a).is_empty());
        cache.take_index_requests();
        let before = generation(&cache, &a);
        cache.invalidate_file_contents(Path::new("/w/A.pas"));
        cache.invalidate_file_contents(Path::new("/w/A.pas"));
        assert_eq!(unverified(&cache, &a), vec![PathBuf::from("/w/A.pas")]);
        assert!(generation(&cache, &a) > before);
        assert_eq!(cache.take_index_requests(), vec![anchor("a")]);
        assert!(cache.take_index_requests().is_empty());
    }

    #[test]
    fn invalidation_reaches_every_context_holding_the_path() {
        let cache = ProjectCache::new(1 << 30);
        let (a, b, other) = (key("a"), key("b"), key("o"));
        for (key, name, path) in [
            (&a, "a", "/w/A.pas"),
            (&b, "b", "/w/A.pas"),
            (&other, "o", "/w/O.pas"),
        ] {
            cache.set_index_membership(key.clone(), anchor(name), membership(&[path]), usize::MAX);
            publish(&cache, key, path);
        }
        cache.take_index_requests();
        cache.invalidate_path(Path::new("/w/A.pas"));
        assert_eq!(unverified(&cache, &a).len(), 1);
        assert_eq!(unverified(&cache, &b).len(), 1);
        assert!(unverified(&cache, &other).is_empty());
        let mut requests = cache.take_index_requests();
        requests.sort_by_key(|anchor| format!("{anchor:?}"));
        assert_eq!(requests, vec![anchor("a"), anchor("b")]);
    }

    fn listing_scope(directory: &Path) -> (crate::catalogue::CatalogueKey, Arc<Catalogue>) {
        use crate::catalogue::{Bound, Depth, Exclusions, MAX_CATALOGUE_ENTRIES};
        let mut io = || -> Result<(), String> { Ok(()) };
        let mut catalogue = Catalogue::new(directory, Depth::Listing, &mut io).unwrap();
        catalogue
            .advance(Bound::entries(MAX_CATALOGUE_ENTRIES), &|_| false, &mut io)
            .unwrap();
        let key = CatalogueKey {
            root: directory.to_path_buf(),
            depth: Depth::Listing,
            exclusions: Exclusions::Nothing,
        };
        (key, Arc::new(catalogue))
    }

    #[test]
    fn a_listing_change_dirties_the_membership_that_listed_the_parent() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("A.pas"), b"").unwrap();
        let cache = ProjectCache::new(1 << 30);
        let (scope, catalogue) = listing_scope(&dir);
        let generation = cache
            .publish_catalogue(scope.clone(), None, catalogue)
            .unwrap();
        let a = key("a");
        let mut recorded = membership(&["/w/A.pas"]);
        recorded.scopes = vec![(scope, generation)];
        cache.set_index_membership(a.clone(), anchor("a"), recorded, usize::MAX);
        assert!(cache.index_membership_current(&a));
        cache.invalidate_file_contents(&dir.join("New.pas"));
        assert!(
            cache.index_membership_current(&a),
            "a file's bytes are not membership"
        );
        cache.invalidate_path(&dir.join("New.pas"));
        assert!(!cache.index_membership_current(&a));
        assert!(!cache.index_membership_current(&key("absent")));
    }

    #[test]
    fn deleting_a_listed_directory_dirties_the_membership() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("dir");
        std::fs::create_dir_all(&dir).unwrap();
        let cache = ProjectCache::new(1 << 30);
        let (scope, catalogue) = listing_scope(&dir);
        let generation = cache
            .publish_catalogue(scope.clone(), None, catalogue)
            .unwrap();
        let a = key("a");
        let mut recorded = membership(&["/w/A.pas"]);
        recorded.scopes = vec![(scope, generation)];
        cache.set_index_membership(a.clone(), anchor("a"), recorded, usize::MAX);
        // Keep the catalogue current so only the membership's flag decides.
        cache.invalidate_index(&dir, true);
        assert!(!cache.index_membership_current(&a));
    }

    #[test]
    fn a_listing_change_leaves_unrelated_memberships_current() {
        let cache = ProjectCache::new(1 << 30);
        let a = key("a");
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        assert!(cache.index_membership_current(&a));
        cache.invalidate_path(Path::new("/elsewhere/New.pas"));
        assert!(cache.index_membership_current(&a));
    }

    #[test]
    fn a_changed_include_marks_its_dependents_stale() {
        let cache = ProjectCache::new(1 << 30);
        let a = key("a");
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas", "/w/B.pas"]),
            usize::MAX,
        );
        let slots = vec![None, Some(PathBuf::from("/w/shared.inc"))];
        let with_include = facts_with("/w/A.pas", "thing", slots);
        cache.publish_index_member(&a, Path::new("/w/A.pas"), with_include, stamps(), 1);
        publish(&cache, &a, "/w/B.pas");
        cache.invalidate_file_contents(Path::new("/w/shared.inc"));
        assert_eq!(unverified(&cache, &a), vec![PathBuf::from("/w/A.pas")]);
    }

    #[test]
    fn overflow_marks_every_member_stale_and_clears_owners() {
        let cache = ProjectCache::new(1 << 30);
        let (a, b) = (key("a"), key("b"));
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        cache.set_index_membership(
            b.clone(),
            anchor("b"),
            membership(&["/w/B.pas"]),
            usize::MAX,
        );
        publish(&cache, &a, "/w/A.pas");
        publish(&cache, &b, "/w/B.pas");
        cache.remember_owner(PathBuf::from("/w"), a.clone());
        assert_eq!(cache.owner_of(Path::new("/w")), Some(a.clone()));
        cache.invalidate_after_overflow();
        assert_eq!(unverified(&cache, &a).len(), 1);
        assert_eq!(unverified(&cache, &b).len(), 1);
        assert_eq!(cache.owner_of(Path::new("/w")), None);
        assert!(!cache.index_membership_current(&a));
    }

    #[test]
    fn a_listing_change_forgets_the_owner_of_the_directory() {
        let cache = ProjectCache::new(1 << 30);
        cache.remember_owner(PathBuf::from("/w"), key("a"));
        cache.remember_owner(PathBuf::from("/x"), key("a"));
        cache.invalidate_path(Path::new("/w/New.pas"));
        assert_eq!(cache.owner_of(Path::new("/w")), None);
        assert_eq!(cache.owner_of(Path::new("/x")), Some(key("a")));
    }

    #[test]
    fn requests_are_deduplicated() {
        let cache = ProjectCache::new(1 << 30);
        cache.request_index(anchor("a"));
        cache.request_index(anchor("a"));
        cache.request_index(anchor("b"));
        assert_eq!(cache.take_index_requests(), vec![anchor("a"), anchor("b")]);
    }

    #[test]
    fn the_unit_limit_truncates_the_membership_and_records_it() {
        let cache = ProjectCache::new(1 << 30);
        cache.set_index_unit_limit_for_test(2);
        let a = key("a");
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas", "/w/B.pas", "/w/C.pas"]),
            usize::MAX,
        );
        let (count, limited, complete) = cache
            .read_index(&a, |index, membership| {
                (
                    index.member_paths().len(),
                    membership.limit.is_some(),
                    membership.complete,
                )
            })
            .unwrap();
        assert_eq!((count, limited, complete), (2, true, false));
        assert_eq!(
            cache.stats().reserved_index_bytes,
            membership_cost(&["/w/A.pas", "/w/B.pas"])
        );
    }

    #[test]
    fn max_workspace_units_bounds_the_membership() {
        let cache = ProjectCache::new(1 << 30);
        let a = key("a");
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas", "/w/B.pas", "/w/C.pas"]),
            2,
        );
        let (paths, limit, complete) = cache
            .read_index(&a, |index, membership| {
                (
                    index.member_paths().len(),
                    membership.limit.clone(),
                    membership.complete,
                )
            })
            .unwrap();
        assert_eq!(paths, 2);
        assert!(!complete);
        assert_eq!(limit.as_deref(), Some("more than 2 documents"));
        cache.set_index_unit_limit_for_test(1);
        let b = key("b");
        cache.set_index_membership(
            b.clone(),
            anchor("b"),
            membership(&["/w/A.pas", "/w/B.pas"]),
            5,
        );
        assert_eq!(
            cache.read_index(&b, |index, _| index.member_paths().len()),
            Some(1)
        );
    }

    #[test]
    fn the_index_lock_is_never_held_with_state() {
        let cache = ProjectCache::new(1 << 20);
        let (a, b) = (key("a"), key("b"));
        cache.set_index_membership(
            a.clone(),
            anchor("a"),
            membership(&["/w/A.pas"]),
            usize::MAX,
        );
        assert_eq!(publish(&cache, &a, "/w/A.pas"), PublishOutcome::Indexed);
        cache.invalidate_path(Path::new("/w/A.pas"));
        cache.invalidate_after_overflow();
        for index in 0..=MAX_INDEXED_CONTEXTS {
            let name = format!("k{index}");
            cache.set_index_membership(
                key(&name),
                anchor(&name),
                membership(&["/w/A.pas"]),
                usize::MAX,
            );
        }
        cache.set_index_membership(b.clone(), anchor("b"), membership(&[]), usize::MAX);
        assert!(
            cache
                .read_index(&b, |_, _| HELD_INDEX.with(std::cell::Cell::get))
                .unwrap(),
            "the flag tracks the guard"
        );
        assert!(!HELD_INDEX.with(std::cell::Cell::get));
        let inside = cache
            .read_index(&b, |_, _| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cache.stats())).is_err()
            })
            .unwrap();
        assert!(inside, "state is refused while the index guard lives");
        assert!(!HELD_INDEX.with(std::cell::Cell::get));
        assert_eq!(
            cache.stats().reserved_index_bytes,
            cache.stats().reserved_bytes()
        );
    }
}
