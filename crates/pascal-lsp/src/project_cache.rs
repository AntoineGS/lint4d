//! Project-scoped cache shared by analysis workers and the warmer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
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
    resolved
        .dependencies
        .iter()
        .map(|unit| unit.source.bytes.len())
        .sum::<usize>()
        .saturating_add(4096)
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
                Ok(Some(_)) => std::fs::read(path)
                    .is_ok_and(|bytes| pascal_project::content_hash_bytes(&bytes) == *content_hash),
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
            resolved: pascal_core::ResolvedImports {
                bindings: vec![],
                dependencies: vec![],
                complete: true,
            },
            report: pascal_core::ResolutionReport {
                observations: vec![],
                warnings: vec![],
                complete: true,
                incomplete_reasons: vec![],
            },
            probes,
            watch_dirs: vec![],
        }
    }

    fn uri(name: &str) -> Url {
        Url::parse(&format!("file:///ws/{name}")).unwrap()
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
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
    fn drop_imports_forces_resolution_again() {
        let cache = ProjectCache::new(1 << 20);
        let ctx = context("A.dproj");
        fill(&cache, "Main.pas", &ctx, 1, 1);
        cache.drop_imports();
        assert!(matches!(
            cache.imports(&uri("Main.pas"), &ctx, 1, &HashMap::new(), &no_cancel()),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Layer {
    Unit,
    Import,
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
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct ImportValue {
    #[allow(dead_code)]
    pub(crate) resolved: pascal_core::ResolvedImports,
    #[allow(dead_code)]
    pub(crate) report: pascal_core::ResolutionReport,
    pub(crate) probes: Vec<Probe>,
    pub(crate) watch_dirs: Vec<PathBuf>,
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
    watch_counts: HashMap<PathBuf, usize>,
    watcher: Option<Box<dyn DirectoryWatch>>,
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
            watch_counts: HashMap::new(),
            watcher: None,
        }
    }
}

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    #[cfg(test)]
    validation_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
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
    fn set_validation_hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self.inner.validation_hook.lock().unwrap() = Some(Box::new(hook));
    }

    fn run_validation_hook(&self) {
        let hook = self.inner.validation_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
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
    pub pinned_over_budget: bool,
}

fn lock(inner: &Inner) -> MutexGuard<'_, State> {
    match inner.state.lock() {
        Ok(state) => state,
        Err(poisoned) => {
            let mut state = PoisonError::into_inner(poisoned);
            state.slots.clear();
            state.generation = state.generation.wrapping_add(1);
            state.bytes = 0;
            let watched_directories = state.watch_counts.keys().cloned().collect::<Vec<_>>();
            if let Some(watcher) = state.watcher.as_mut() {
                for directory in &watched_directories {
                    watcher.unwatch(directory);
                }
            }
            state.watch_counts.clear();
            inner.state.clear_poison();
            state
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
                Value::Unit(unit) => Some((unit.clone(), unit.probes.clone())),
                Value::Import(_) => None,
            },
        )
    }

    /// Declared unit name of a cached parse of exactly these bytes, under any
    /// project context. The declared name does not depend on project settings.
    pub(crate) fn declared_unit_name(
        &self,
        uri: &Url,
        content_hash: u64,
    ) -> Option<Option<String>> {
        let state = lock(&self.inner);
        state.slots.iter().find_map(|(key, slot)| match slot {
            Slot::Ready(Entry {
                value: Value::Unit(unit),
                input_hash,
                ..
            }) if key.uri == *uri && *input_hash == content_hash => {
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
        self.store(
            claim,
            Value::Unit(Arc::new(value)),
            bytes,
            Vec::new(),
            cancel,
        );
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
                Value::Import(imports) => Some((imports.clone(), imports.probes.clone())),
                Value::Unit(_) => None,
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
        let watch_dirs = value.watch_dirs.clone();
        self.store(
            claim,
            Value::Import(Arc::new(value)),
            bytes,
            watch_dirs,
            cancel,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn lookup<V>(
        &self,
        layer: Layer,
        uri: &Url,
        context: &ProjectContext,
        input_hash: u64,
        snapshot_epoch: Option<u64>,
        overlays: &HashMap<Url, OverlayInput>,
        cancel: &AtomicBool,
        extract: impl Fn(&Value) -> Option<(Arc<V>, Vec<Probe>)>,
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
                    state = self
                        .inner
                        .changed
                        .wait_timeout(state, WAIT_SLICE)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                Some(Slot::Ready(entry))
                    if entry.context.as_ref() == context && entry.input_hash == input_hash =>
                {
                    let Some((value, probes)) = extract(&entry.value) else {
                        break;
                    };
                    drop(state);
                    let probes_hold = probes_hold(&probes, overlays);
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
                                .is_some_and(|(current, _)| Arc::ptr_eq(&current, &value))
                        }
                        _ => false,
                    };
                    if !same_entry {
                        continue;
                    }
                    if probes_hold {
                        state.clock += 1;
                        let clock = state.clock;
                        if let Some(Slot::Ready(entry)) = state.slots.get_mut(&key) {
                            entry.last_used = clock;
                        }
                        return Lookup::Hit(value);
                    }
                    break;
                }
                _ => break,
            }
        }
        if let Some(Slot::Ready(entry)) = state.slots.remove(&key) {
            state.bytes = state.bytes.saturating_sub(entry.bytes);
            release_watches(&mut state, &entry);
        }
        let generation = state.generation;
        state
            .slots
            .insert(key.clone(), Slot::Computing { generation });
        Lookup::Compute(Claim {
            inner: self.inner.clone(),
            key,
            generation,
            context: Arc::new(context.clone()),
            input_hash,
            invalidation_epoch: snapshot_epoch.unwrap_or(state.invalidation_epoch),
        })
    }
}

fn is_pinned(state: &State, key: &Key) -> bool {
    state
        .pins
        .values()
        .any(|pins| pins.contains(&(key.uri.clone(), key.fingerprint)))
}

fn pinned_bytes(state: &State) -> usize {
    state
        .slots
        .iter()
        .filter_map(|(key, slot)| match slot {
            Slot::Ready(entry) if is_pinned(state, key) => Some(entry.bytes),
            _ => None,
        })
        .fold(0usize, |bytes, entry_bytes| {
            bytes.saturating_add(entry_bytes)
        })
}

fn evict_to_budget(state: &mut State) {
    while state.bytes > state.max_bytes {
        let victim = state
            .slots
            .iter()
            .filter_map(|(key, slot)| match slot {
                Slot::Ready(entry) if !is_pinned(state, key) => {
                    Some((entry.last_used, key.clone()))
                }
                _ => None,
            })
            .min_by_key(|(last_used, _)| *last_used)
            .map(|(_, key)| key);
        let Some(key) = victim else {
            return;
        };
        if let Some(Slot::Ready(entry)) = state.slots.remove(&key) {
            state.bytes = state.bytes.saturating_sub(entry.bytes);
            release_watches(state, &entry);
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
        if let Some(watcher) = state.watcher.as_mut() {
            let _ = watcher.watch(directory);
        }
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
            if let Some(watcher) = state.watcher.as_mut() {
                watcher.unwatch(directory);
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl ProjectCache {
    fn store(
        &self,
        claim: Claim,
        value: Value,
        bytes: usize,
        watch_dirs: Vec<PathBuf>,
        cancel: &AtomicBool,
    ) {
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
            for directory in &watch_dirs {
                acquire_watch(&mut state, directory);
            }
            state.slots.insert(claim.key.clone(), Slot::Ready(entry));
            state.bytes = state.bytes.saturating_add(bytes);
            evict_to_budget(&mut state);
        }
        drop(state);
        drop(claim); // Removes the slot only when it is still Computing.
    }

    pub(crate) fn pin(&self, owner: &Url, keys: Vec<(Url, u64)>) {
        lock(&self.inner)
            .pins
            .insert(owner.clone(), keys.into_iter().collect());
    }

    pub(crate) fn unpin(&self, owner: &Url) {
        let mut state = lock(&self.inner);
        state.pins.remove(owner);
        evict_to_budget(&mut state);
    }

    pub(crate) fn clear_pins(&self) {
        let mut state = lock(&self.inner);
        state.pins.clear();
        evict_to_budget(&mut state);
    }

    /// Evicts every entry that depends on `path` and returns the affected URIs.
    pub(crate) fn invalidate_path(&self, path: &Path) -> Vec<Url> {
        let mut state = lock(&self.inner);
        state.invalidation_epoch = state.invalidation_epoch.wrapping_add(1);
        let parent = path.parent();
        let doomed = state
            .slots
            .iter()
            .filter_map(|(key, slot)| {
                let Slot::Ready(entry) = slot else {
                    return None;
                };
                let probes = match &entry.value {
                    Value::Unit(unit) => &unit.probes,
                    Value::Import(imports) => &imports.probes,
                };
                let own_path = key.uri.to_file_path().ok();
                let hit = own_path.as_deref() == Some(path)
                    || probes.iter().any(|probe| match probe {
                        Probe::Stamp { path: observed, .. }
                        | Probe::Content { path: observed, .. } => {
                            observed == path || Some(observed.as_path()) == parent
                        }
                        Probe::Overlay { uri, .. } => {
                            uri.to_file_path().ok().as_deref() == Some(path)
                        }
                    });
                hit.then(|| key.clone())
            })
            .collect::<Vec<_>>();
        let mut affected = Vec::new();
        for key in doomed {
            if let Some(Slot::Ready(entry)) = state.slots.remove(&key) {
                state.bytes = state.bytes.saturating_sub(entry.bytes);
                release_watches(&mut state, &entry);
                if !affected.contains(&key.uri) {
                    affected.push(key.uri);
                }
            }
        }
        affected
    }

    /// Watcher overflow: every resolution must list its directories again.
    pub(crate) fn drop_imports(&self) {
        let mut state = lock(&self.inner);
        let doomed = state
            .slots
            .iter()
            .filter(|(key, slot)| key.layer == Layer::Import && matches!(slot, Slot::Ready(_)))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in doomed {
            if let Some(Slot::Ready(entry)) = state.slots.remove(&key) {
                state.bytes = state.bytes.saturating_sub(entry.bytes);
                release_watches(&mut state, &entry);
            }
        }
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
            if let Some(Slot::Ready(entry)) = state.slots.remove(&key) {
                state.bytes = state.bytes.saturating_sub(entry.bytes);
                release_watches(&mut state, &entry);
            }
        }
        drop(state);
        self.inner.changed.notify_all();
    }

    pub(crate) fn has_room(&self) -> bool {
        let state = lock(&self.inner);
        pinned_bytes(&state) < state.max_bytes
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
            pinned_over_budget: pinned_bytes(&state) > state.max_bytes,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_watcher(&self, watcher: Box<dyn DirectoryWatch>) {
        lock(&self.inner).watcher = Some(watcher);
    }
}
