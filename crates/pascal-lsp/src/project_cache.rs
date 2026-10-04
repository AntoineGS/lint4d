//! Project-scoped cache shared by analysis workers and the warmer.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, WaitTimeoutResult};
use std::time::Duration;

use lsp_types::Url;
use pascal_project::{ProjectContext, ProjectReadStamp};

use crate::file_watch::DirectoryWatch;
use crate::include_expansion::ExpansionResult;
use crate::navigation::ParsedDocument;
use crate::navigation::compiled_dcu::project_context_fingerprint;
use crate::workspace::rename::OverlayInput;

/// Default for the `maxCacheBytes` limit.
pub(crate) const DEFAULT_MAX_CACHE_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Retained heap bytes per indexed source byte, measured by
/// `measure_retained_bytes_per_source_byte` on RAD Studio 7.0 RTL/VCL units.
#[allow(dead_code)]
pub(crate) const RETAINED_BYTES_PER_SOURCE_BYTE: usize = 48; // measured 2026-09-29: 46, 45, 45 on RAD Studio 7.0 RTL/VCL

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

fn expansion_bytes(expansion: &ExpansionResult) -> usize {
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
    use pascal_core::ResolutionObservation as O;

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
    let mut dirs = Vec::new();

    for unit in &resolved.dependencies {
        push_revision(&mut probes, &unit.source.path, &unit.source.revision);
    }
    for observation in &report.observations {
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
            } => push_revision(&mut probes, path, source_revision),
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
    (probes, dirs)
}

/// Every include candidate observed while resolving an expansion, plus the
/// content revision of each include source that was read.
pub(crate) fn expansion_probes(expansion: &ExpansionResult) -> Vec<Probe> {
    expansion
        .dependencies
        .iter()
        .flat_map(|include| include.observations.iter())
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
    probes.iter().all(|probe| match probe {
        Probe::Stamp { path, expected } => current_stamp(path).is_ok_and(|now| now == *expected),
        Probe::Content {
            path,
            stamp,
            content_hash,
        } => {
            if Url::from_file_path(path).is_ok_and(|uri| overlays.contains_key(&uri)) {
                return false;
            }
            match current_stamp(path) {
                Ok(Some(now)) if Some(&now) == stamp.as_ref() => true,
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
    })
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
    fn disk_content_probe_fails_once_the_file_is_open() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.pas");
        std::fs::write(&file, "unit A;").unwrap();
        let probes = vec![Probe::Content {
            path: file.clone(),
            stamp: stamp(&file),
            content_hash: pascal_project::content_hash_bytes(b"unit A;"),
        }];
        let overlays = HashMap::from([(
            Url::from_file_path(&file).unwrap(),
            OverlayInput {
                text: "unit A;".into(),
                version: 1,
            },
        )]);
        assert!(!probes_hold(&probes, &overlays));
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

    fn context(name: &str) -> ProjectContext {
        ProjectContext {
            project_file: Some(PathBuf::from(name)),
            ..ProjectContext::default()
        }
    }

    fn import_value(probes: Vec<Probe>) -> ImportValue {
        ImportValue {
            resolved: Arc::new(pascal_core::ResolvedImports {
                bindings: vec![],
                dependencies: vec![],
                complete: true,
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

    fn uri(name: &str) -> Url {
        Url::parse(&format!("file:///ws/{name}")).unwrap()
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
        cache.invalidate_path(Path::new("/ws/Other.pas"));
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
                    revision: pascal_core::SourceRevision::Overlay {
                        version: 1,
                        content_hash: 2,
                    },
                },
            }],
            complete: true,
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
        pin_names(&cache, "Open1.pas", &["A.pas"], &ctx);
        pin_names(&cache, "Open2.pas", &["A.pas"], &ctx);
        fill(&cache, "A.pas", &ctx, 1, 10);
        fill_unit(&cache, "A.pas", &ctx, 1, vec![]);
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
        cache.invalidate_path(Path::new("/ws/A.pas"));
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
        cache.invalidate_path(Path::new("/ws/Main.pas"));
        cache.invalidate_path(Path::new("/ws/Other.pas"));
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
            len: 1,
            modified: std::time::SystemTime::UNIX_EPOCH,
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
    }

    #[test]
    fn transient_hits_are_not_charged_after_eviction() {
        let cache = ProjectCache::new(usize::MAX);
        let ctx = context("A.dproj");
        store_unit_bytes(&cache, "A.pas", &ctx, 10);
        let unit = cache
            .peek_unit(&uri("A.pas"), &ctx, 1, &HashMap::new())
            .expect("hit");
        cache.invalidate_path(Path::new("/ws/A.pas"));
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
        cache.invalidate_path(Path::new("/ws/A.pas"));
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
        cache.pin(
            &uri("Open.pas"),
            vec![(uri("A.pas"), project_context_fingerprint(&ctx))],
        );
        fill(&cache, "A.pas", &ctx, 1, 10);

        assert_eq!(cache.stats().bytes, 10);
        assert!(!cache.has_room());
        assert!(!cache.stats().pinned_over_budget);
    }

    #[test]
    fn pinned_entries_over_budget_report_no_room() {
        let cache = ProjectCache::new(5);
        let ctx = context("A.dproj");
        cache.pin(
            &uri("Open.pas"),
            vec![(uri("A.pas"), project_context_fingerprint(&ctx))],
        );
        fill(&cache, "A.pas", &ctx, 1, 10);
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
}

const WAIT_SLICE: Duration = Duration::from_millis(20);
const MAX_CLOSURE_CRAWL_REQUESTS: usize = 1024;
const MAX_CLOSURE_MISS_ROOTS: usize = 1024;

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

/// A closed source file as it was read. A later load whose `stat` still
/// shows this length and modification time takes `text` instead of reading
/// and decoding the file again.
#[derive(Debug)]
pub(crate) struct DiskOrigin {
    pub(crate) len: u64,
    pub(crate) modified: std::time::SystemTime,
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
    Computing { generation: u64 },
}

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
    watch_counts: HashMap<PathBuf, usize>,
    /// Directories whose watch count reached or left zero. `sync_watches`
    /// applies them to the watcher once the state lock is released.
    watch_changes: Vec<PathBuf>,
    closure_crawl_requests: Vec<Url>,
    /// Each root's uncached closure units at its last snapshot walk.
    closure_misses: HashMap<Url, HashSet<Url>>,
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
            observed: HashMap::new(),
            exact: HashMap::new(),
            by_uri: HashMap::new(),
            contexts: HashMap::new(),
            cached_parses: HashMap::new(),
            leases: HashMap::new(),
            outstanding_bytes: 0,
            watch_counts: HashMap::new(),
            watch_changes: Vec::new(),
            closure_crawl_requests: Vec::new(),
            closure_misses: HashMap::new(),
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
/// applies them after `state` is released.
#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    watcher: Mutex<WatcherState>,
    #[cfg(test)]
    validation_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    waiter_hook: Mutex<Option<std::sync::mpsc::Sender<()>>>,
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
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Claim {
    inner: Arc<Inner>,
    key: Key,
    generation: u64,
    invalidation_epoch: u64,
    context: Arc<ProjectContext>,
    input_hash: u64,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut state = lock(&self.inner);
        if matches!(state.slots.get(&self.key), Some(Slot::Computing { generation }) if *generation == self.generation)
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
    /// Bytes of removed values that workers still hold.
    pub outstanding_bytes: usize,
    pub pinned_over_budget: bool,
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
    inner.state.clear_poison();
    state
}

fn lock(inner: &Inner) -> MutexGuard<'_, State> {
    match inner.state.lock() {
        Ok(state) => state,
        Err(poisoned) => recover_poisoned_state(inner, poisoned.into_inner()),
    }
}

fn wait_timeout_recovering_poison<'a>(
    inner: &'a Inner,
    state: MutexGuard<'a, State>,
) -> (MutexGuard<'a, State>, WaitTimeoutResult) {
    match inner.changed.wait_timeout(state, WAIT_SLICE) {
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

    #[allow(dead_code)]
    pub(crate) fn set_max_bytes(&self, max_bytes: usize) {
        let mut state = lock(&self.inner);
        state.max_bytes = max_bytes;
        evict_to_budget(&mut state);
        self.release(state);
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
    /// so the parse stays charged if its entry is evicted first.
    pub(crate) fn lease_unit(&self, unit: &UnitValue) -> CacheLease {
        let payload = Arc::as_ptr(&unit.parsed) as *const () as usize;
        let mut state = lock(&self.inner);
        if let Some(lease) = state.leases.get_mut(&payload) {
            lease.holders += 1;
        } else {
            let cached = state.cached_parses.get(&payload).map(|(_, bytes)| *bytes);
            let bytes = cached.unwrap_or_else(|| unit.retained_bytes());
            if cached.is_none() {
                state.outstanding_bytes = state.outstanding_bytes.saturating_add(bytes);
            }
            state.leases.insert(
                payload,
                Lease {
                    holders: 1,
                    bytes,
                    evicted: cached.is_none(),
                },
            );
        }
        CacheLease {
            inner: self.inner.clone(),
            payload,
            _parsed: unit.parsed.clone(),
        }
    }

    /// A verified unit entry read from a closed file whose length and
    /// modification time are still `len` and `modified`, with the raw content
    /// hash that keys it. Lets a load skip reading the file; never claims.
    pub(crate) fn peek_unit_from_disk(
        &self,
        uri: &Url,
        context: &ProjectContext,
        len: u64,
        modified: std::time::SystemTime,
        overlays: &HashMap<Url, OverlayInput>,
    ) -> Option<(Arc<UnitValue>, u64)> {
        let same_file = |unit: &UnitValue| {
            unit.disk
                .as_ref()
                .is_some_and(|disk| disk.len == len && disk.modified == modified)
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
            })) if entry_context.as_ref() == context && same_file(unit) => *input_hash,
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
            return false;
        }
        let unchanged = matches!(
            state.slots.get(&key),
            Some(Slot::Ready(Entry {
                value: Value::Interface(old),
                context: old_context,
                input_hash,
                ..
            })) if old_context.as_ref() == context
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
                    if entry.context.as_ref() == context && entry.input_hash == input_hash =>
                {
                    extract(&entry.value)?
                }
                _ => return None,
            }
        };
        if !probes_hold(value.probes(), overlays) {
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
        self.store(claim, Value::Unit(Arc::new(value)), bytes, cancel);
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
        self.store(claim, Value::Import(Arc::new(value)), bytes, cancel);
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
        let mut state = lock(&self.inner);
        loop {
            match state.slots.get(&key) {
                Some(Slot::Computing { .. }) => {
                    if cancel.load(Ordering::Relaxed) {
                        return Lookup::Cancelled;
                    }
                    #[cfg(test)]
                    self.signal_waiter_hook();
                    state = wait_timeout_recovering_poison(&self.inner, state).0;
                }
                Some(Slot::Ready(entry))
                    if entry.context.as_ref() == context && entry.input_hash == input_hash =>
                {
                    let Some(value) = extract(&entry.value) else {
                        break;
                    };
                    drop(state);
                    let probes_hold = probes_hold(value.probes(), overlays);
                    #[cfg(test)]
                    if probes_hold {
                        self.run_validation_hook();
                    }
                    state = lock(&self.inner);
                    let same_entry = match state.slots.get(&key) {
                        Some(Slot::Ready(entry))
                            if entry.context.as_ref() == context
                                && entry.input_hash == input_hash =>
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
                        return Lookup::Hit(value);
                    }
                    break;
                }
                _ => break,
            }
        }
        remove_ready(&mut state, &key);
        let generation = state.generation;
        state
            .slots
            .insert(key.clone(), Slot::Computing { generation });
        let invalidation_epoch = snapshot_epoch.unwrap_or(state.invalidation_epoch);
        let context = intern_context(&mut state, key.fingerprint, context);
        self.release(state);
        Lookup::Compute(Claim {
            inner: self.inner.clone(),
            key,
            generation,
            context,
            input_hash,
            invalidation_epoch,
        })
    }
}

const LAYERS: [Layer; 3] = [Layer::Unit, Layer::Import, Layer::Interface];

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
}

/// A request's hold on a cached parse it put into its own index. While a
/// lease is alive and no entry holds the parse, the entry's charge stays in
/// the budget as outstanding bytes. Transient hits are not leased.
pub(crate) struct CacheLease {
    inner: Arc<Inner>,
    payload: usize,
    _parsed: Arc<ParsedDocument>,
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
        && shared.as_ref() == context
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

fn evict_to_budget(state: &mut State) {
    while state.bytes.saturating_add(state.outstanding_bytes) > state.max_bytes {
        let Some((_, key)) = state.lru.pop_first() else {
            return;
        };
        #[cfg(test)]
        note_examined(state);
        remove_ready(state, &key);
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
        let Some(count) = state.watch_counts.get_mut(directory) else {
            continue;
        };
        *count -= 1;
        if *count == 0 {
            state.watch_counts.remove(directory);
            state.watch_changes.push(directory.clone());
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl ProjectCache {
    fn store(&self, claim: Claim, value: Value, bytes: usize, cancel: &AtomicBool) {
        let mut state = lock(&self.inner);
        let current = !cancel.load(Ordering::Relaxed)
            && claim.invalidation_epoch == state.invalidation_epoch
            && matches!(state.slots.get(&claim.key),
            Some(Slot::Computing { generation }) if *generation == claim.generation && claim.generation == state.generation);
        if current {
            state.clock += 1;
            let entry = Entry {
                value,
                context: claim.context.clone(),
                input_hash: claim.input_hash,
                bytes,
                last_used: state.clock,
            };
            insert_ready(&mut state, claim.key.clone(), entry);
            evict_to_budget(&mut state);
        }
        self.release(state);
        drop(claim); // Removes the slot only when it is still Computing.
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
        let mut state = lock(&self.inner);
        state.invalidation_epoch = state.invalidation_epoch.wrapping_add(1);
        let parent = path.parent().filter(|_| include_parent);
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
        self.release(state);
        affected
    }

    /// Watcher overflow loses change details, so discard every verified cache
    /// entry and reject claims based on snapshots from before the overflow.
    pub(crate) fn invalidate_after_overflow(&self) {
        let mut state = lock(&self.inner);
        state.invalidation_epoch = state.invalidation_epoch.wrapping_add(1);
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
        self.release(state);
        self.inner.changed.notify_all();
    }

    pub(crate) fn retain_fingerprints(&self, keep: &HashSet<u64>) {
        let mut state = lock(&self.inner);
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

    pub(crate) fn has_room(&self) -> bool {
        let state = lock(&self.inner);
        state.pinned_bytes.saturating_add(state.outstanding_bytes) < state.max_bytes
    }

    pub(crate) fn invalidation_epoch(&self) -> u64 {
        lock(&self.inner).invalidation_epoch
    }

    pub(crate) fn stats(&self) -> CacheStats {
        let state = lock(&self.inner);
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
            outstanding_bytes: state.outstanding_bytes,
            pinned_over_budget: state.pinned_bytes > state.max_bytes,
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
    }
}
