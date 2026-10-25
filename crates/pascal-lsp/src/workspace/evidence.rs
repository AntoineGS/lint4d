//! Durable evidence of what a compile-time session consulted (spec §7).

use super::Workspace;
use super::capture::{CapturedReads, ListingCapture};
use super::resolver::path_equivalent;
use lsp_types::Url;
use pascal_cache::Observation;
use pascal_core::resolver::{ResolutionObservation, SourceRevision};
use pascal_project::ProjectObservations;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Why a session's inputs cannot be proven to another process. The facts
/// stay usable in memory; they are only not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvidenceGap {
    NoSession,
    /// The session ran without a read capture.
    NoCapture,
    /// A directory scan or lookup did not finish.
    Incomplete,
    /// An input came from an editor buffer (spec §4.4).
    Overlay,
    /// The session consulted something the capture did not record, or
    /// metadata outside its context.
    Uncaptured,
    /// The session listed one directory twice and saw it change.
    Conflicting,
    /// The capture reached its bound.
    Overflowed,
    Cancelled,
}

/// The observations that prove a session's inputs, in session order.
/// Metadata the context was built from is proven by the context key
/// (spec §7 step 1), so it is skipped, not repeated.
pub(crate) fn session_evidence(
    observations: &[ResolutionObservation],
    reads: &CapturedReads,
    context: &ProjectObservations,
    cancel: &AtomicBool,
) -> Result<Vec<Observation>, EvidenceGap> {
    if reads.overflowed {
        return Err(EvidenceGap::Overflowed);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(EvidenceGap::Cancelled);
    }
    if reads.editor_state {
        return Err(EvidenceGap::Overlay);
    }
    let searched = SearchedDirectories::of(observations, reads);
    let mut evidence = Vec::with_capacity(observations.len());
    for observation in observations {
        if cancel.load(Ordering::Relaxed) {
            return Err(EvidenceGap::Cancelled);
        }
        evidence.push(match observation {
            ResolutionObservation::Directory {
                complete: false, ..
            } => return Err(EvidenceGap::Incomplete),
            ResolutionObservation::Directory {
                path,
                listed: false,
                ..
            } if searched.proves(path) => continue,
            ResolutionObservation::Directory { listed: false, .. } => {
                return Err(EvidenceGap::Uncaptured);
            }
            ResolutionObservation::Directory { path, entry, .. } => match reads.listing_of(path) {
                ListingCapture::One(listing) if listing.overlay => {
                    return Err(EvidenceGap::Overlay);
                }
                ListingCapture::One(listing) if !listing.complete => {
                    return Err(EvidenceGap::Incomplete);
                }
                ListingCapture::One(listing) => Observation::Listing {
                    path: path.clone(),
                    entry: entry.clone(),
                    listing: listing.listing,
                },
                ListingCapture::Absent => return Err(EvidenceGap::Uncaptured),
                ListingCapture::Conflicting => return Err(EvidenceGap::Conflicting),
            },
            ResolutionObservation::Candidate {
                path,
                entry,
                present,
                ..
            } => Observation::Presence {
                path: path.clone(),
                entry: entry.clone(),
                present: *present,
            },
            ResolutionObservation::Payload {
                revision: SourceRevision::Overlay { .. },
                ..
            } => return Err(EvidenceGap::Overlay),
            ResolutionObservation::Payload {
                path,
                revision:
                    SourceRevision::Disk {
                        content_hash,
                        path_entry,
                        ..
                    },
                ..
            } => match reads.payload_of(path, *content_hash) {
                Some(content) => Observation::Payload {
                    path: path.clone(),
                    entry: path_entry.clone(),
                    content: *content,
                },
                None => return Err(EvidenceGap::Uncaptured),
            },
            ResolutionObservation::Metadata(metadata)
                if context.metadata_observations.contains(metadata) =>
            {
                continue;
            }
            ResolutionObservation::Metadata(_) | ResolutionObservation::ProjectRead(_) => {
                return Err(EvidenceGap::Uncaptured);
            }
        });
    }
    Ok(evidence)
}

/// What proves an include search's stat-only marker of a directory it
/// consulted (`Directory { listed: false }`). After an exact hit the resolver
/// consults nothing else in that directory, so the hit's payload evidence,
/// reread and authorized at validation, proves what the session learned
/// there. Every directory where the exact name missed was listed by the
/// case-insensitive fallback; that listing is its own observation and is
/// proven by its capture.
struct SearchedDirectories<'a> {
    hit_parents: Vec<&'a Path>,
    listed: Vec<&'a Path>,
}

impl<'a> SearchedDirectories<'a> {
    fn of(observations: &'a [ResolutionObservation], reads: &CapturedReads) -> Self {
        let mut searched = Self {
            hit_parents: Vec::new(),
            listed: Vec::new(),
        };
        for observation in observations {
            match observation {
                ResolutionObservation::Directory {
                    path, listed: true, ..
                } => searched.listed.push(path),
                ResolutionObservation::Payload {
                    path,
                    revision: SourceRevision::Disk { content_hash, .. },
                    ..
                } if reads.payload_of(path, *content_hash).is_some() => {
                    searched.hit_parents.extend(path.parent());
                }
                _ => {}
            }
        }
        searched
    }

    fn proves(&self, directory: &Path) -> bool {
        self.hit_parents
            .iter()
            .chain(&self.listed)
            .any(|path| path_equivalent(path, directory))
    }
}

impl Workspace {
    /// Evidence of what `uri`'s indexed compile-time session consulted.
    pub(crate) fn source_evidence(
        &self,
        uri: &Url,
        cancel: &AtomicBool,
    ) -> Result<Vec<Observation>, EvidenceGap> {
        let record = self.expansions.get(uri).ok_or(EvidenceGap::NoSession)?;
        let session = record
            .expanded
            .session
            .as_ref()
            .ok_or(EvidenceGap::NoSession)?;
        let reads = record
            .expanded
            .reads
            .as_ref()
            .ok_or(EvidenceGap::NoCapture)?;
        let state = self
            .contexts
            .get(&record.context_key)
            .ok_or(EvidenceGap::NoSession)?;
        session_evidence(
            &session.observations,
            reads,
            &state.context_observations,
            cancel,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::capture::{CapturedListing, CapturedPayload};
    use crate::workspace::facts::tests::{index, index_with_buffers, index_with_edits};
    use pascal_cache::listing_digest;
    use pascal_cache::{Digest, SourceContent};
    use pascal_project::ProjectPathEntry;
    use std::path::PathBuf;

    const DPROJ: &[u8] = b"<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_IncludePath>include</DCC_IncludePath></PropertyGroup></Project>";
    const MAIN: &[u8] =
        b"unit Main;\ninterface\n{$I Shared.inc}\n{$I Missing.inc}\nimplementation\nend.\n";
    const SHARED: &[u8] = b"const Shared = 1;\n";

    fn files() -> [(&'static str, &'static [u8]); 3] {
        [
            ("App.dproj", DPROJ),
            ("Main.pas", MAIN),
            ("include/Shared.inc", SHARED),
        ]
    }

    #[test]
    fn a_session_s_evidence_names_every_read_and_lookup() {
        let indexed = index(&files(), &["Main.pas"]);
        let evidence = indexed
            .workspace
            .source_evidence(&indexed.uri("Main.pas"), &AtomicBool::new(false))
            .unwrap();
        assert!(evidence.iter().any(|observation| matches!(
            observation,
            Observation::Payload { path, content, .. }
                if path.ends_with("include/Shared.inc")
                    && *content == SourceContent::from_bytes(SHARED)
        )));
        // `Missing.inc` was looked for in the include directory: creating it
        // there must change something this evidence holds.
        assert!(
            evidence.iter().any(|observation| matches!(
                observation,
                Observation::Listing { path, .. } if path.ends_with("include")
            ) || matches!(
                observation,
                Observation::Presence { path, present: false, .. }
                    if path.ends_with("include/Missing.inc")
            )),
            "{evidence:?}"
        );
    }

    #[test]
    fn an_open_include_leaves_no_evidence() {
        let indexed = index_with_buffers(
            &files(),
            &[("include/Shared.inc", "const Edited = 1;\n")],
            &["Main.pas"],
        );
        assert_eq!(
            indexed
                .workspace
                .source_evidence(&indexed.uri("Main.pas"), &AtomicBool::new(false)),
            Err(EvidenceGap::Overlay)
        );
    }

    #[test]
    fn provider_lookups_are_evidence() {
        let indexed = index(
            &[
                (
                    "Main.pas",
                    b"unit Main;\ninterface\nuses Shapes;\n{$IF SizeOf(TPair) = 8}\nconst Wide = 1;\n{$ENDIF}\nimplementation\nend.\n".as_slice(),
                ),
                (
                    "Shapes.pas",
                    b"unit Shapes;\ninterface\ntype TPair = record A, B: Integer; end;\nimplementation\nend.\n".as_slice(),
                ),
            ],
            &["Main.pas"],
        );
        let evidence = indexed
            .workspace
            .source_evidence(&indexed.uri("Main.pas"), &AtomicBool::new(false))
            .unwrap();
        assert!(
            evidence.iter().any(|observation| matches!(
                observation,
                Observation::Payload { path, .. } if path.ends_with("Shapes.pas")
            )),
            "{evidence:?}"
        );
    }

    #[test]
    fn what_cannot_be_proven_is_a_gap() {
        let directory = PathBuf::from("/w/include");
        let entry = ProjectPathEntry::configured(directory.clone());
        let listed = |complete| ResolutionObservation::Directory {
            path: directory.clone(),
            entry: entry.clone(),
            stamp: None,
            complete,
            listed: true,
        };
        let captured = |listing: [u8; 32], overlay| CapturedListing {
            directory: directory.clone(),
            listing: Digest::from_bytes(listing),
            complete: true,
            overlay,
        };
        let reads = |listings: Vec<CapturedListing>, overflowed| CapturedReads {
            listings,
            payloads: Vec::new(),
            overflowed,
            ..CapturedReads::default()
        };
        let context = ProjectObservations::default();
        let gap = |observations: &[ResolutionObservation], reads: &CapturedReads| {
            session_evidence(observations, reads, &context, &AtomicBool::new(false)).unwrap_err()
        };
        assert_eq!(
            gap(&[listed(false)], &reads(vec![], false)),
            EvidenceGap::Incomplete
        );
        assert_eq!(
            gap(&[listed(true)], &reads(vec![], false)),
            EvidenceGap::Uncaptured
        );
        assert_eq!(
            gap(
                &[listed(true)],
                &reads(vec![captured([1; 32], true)], false)
            ),
            EvidenceGap::Overlay
        );
        assert_eq!(
            gap(
                &[listed(true)],
                &reads(
                    vec![captured([1; 32], false), captured([2; 32], false)],
                    false
                )
            ),
            EvidenceGap::Conflicting
        );
        assert_eq!(gap(&[], &reads(vec![], true)), EvidenceGap::Overflowed);
        let payload = ResolutionObservation::Payload {
            source_id: pascal_core::SourceId::new("source:/w/a.inc"),
            path: PathBuf::from("/w/a.inc"),
            revision: pascal_core::SourceRevision::Disk {
                stamp: pascal_project::ProjectReadStamp {
                    bytes: 1,
                    modified: None,
                    is_dir: false,
                    is_symlink: false,
                    change: None,
                    settled: false,
                },
                content_hash: 7,
                read_policy: Default::default(),
                path_entry: ProjectPathEntry::configured(PathBuf::from("/w/a.inc")),
            },
        };
        assert_eq!(
            gap(std::slice::from_ref(&payload), &reads(vec![], false)),
            EvidenceGap::Uncaptured
        );
        let with_payload = CapturedReads {
            payloads: vec![CapturedPayload {
                path: PathBuf::from("/w/a.inc"),
                content_hash: 7,
                content: SourceContent::from_bytes(b"x"),
            }],
            ..CapturedReads::default()
        };
        assert_eq!(
            session_evidence(&[payload], &with_payload, &context, &AtomicBool::new(false)),
            Ok(vec![Observation::Payload {
                path: PathBuf::from("/w/a.inc"),
                entry: ProjectPathEntry::configured(PathBuf::from("/w/a.inc")),
                content: SourceContent::from_bytes(b"x"),
            }])
        );
        assert_eq!(
            session_evidence(
                &[],
                &CapturedReads::default(),
                &context,
                &AtomicBool::new(true)
            ),
            Err(EvidenceGap::Cancelled)
        );
        let deleted = CapturedReads {
            editor_state: true,
            ..CapturedReads::default()
        };
        assert_eq!(
            session_evidence(&[], &deleted, &context, &AtomicBool::new(false)),
            Err(EvidenceGap::Overlay)
        );
    }

    #[test]
    fn a_search_marker_is_proven_by_the_hit_or_the_listing_in_its_directory() {
        let directory = PathBuf::from("/w/include");
        let entry = ProjectPathEntry::configured(directory.clone());
        let directory_observation = |listed| ResolutionObservation::Directory {
            path: directory.clone(),
            entry: entry.clone(),
            stamp: None,
            complete: true,
            listed,
        };
        let payload = |path: &str| ResolutionObservation::Payload {
            source_id: pascal_core::SourceId::new(format!("source:{path}")),
            path: PathBuf::from(path),
            revision: pascal_core::SourceRevision::Disk {
                stamp: pascal_project::ProjectReadStamp {
                    bytes: 1,
                    modified: None,
                    is_dir: false,
                    is_symlink: false,
                    change: None,
                    settled: false,
                },
                content_hash: 7,
                read_policy: Default::default(),
                path_entry: entry.clone(),
            },
        };
        let captured_payload = |path: &str| CapturedPayload {
            path: PathBuf::from(path),
            content_hash: 7,
            content: SourceContent::from_bytes(b"x"),
        };
        let context = ProjectObservations::default();
        let evidence = |observations: &[ResolutionObservation], reads: &CapturedReads| {
            session_evidence(observations, reads, &context, &AtomicBool::new(false))
        };
        let hit = CapturedReads {
            payloads: vec![captured_payload("/w/include/Shared.inc")],
            ..CapturedReads::default()
        };
        assert_eq!(
            evidence(
                &[
                    directory_observation(false),
                    payload("/w/include/Shared.inc")
                ],
                &hit
            ),
            Ok(vec![Observation::Payload {
                path: PathBuf::from("/w/include/Shared.inc"),
                entry: entry.clone(),
                content: SourceContent::from_bytes(b"x"),
            }]),
            "the exact hit proves its directory's marker"
        );
        assert_eq!(
            evidence(&[directory_observation(false)], &CapturedReads::default()),
            Err(EvidenceGap::Uncaptured)
        );
        // A hit elsewhere, or a payload the capture did not see, proves nothing here.
        let elsewhere = CapturedReads {
            payloads: vec![captured_payload("/w/Shared.inc")],
            ..CapturedReads::default()
        };
        assert_eq!(
            evidence(
                &[directory_observation(false), payload("/w/Shared.inc")],
                &elsewhere
            ),
            Err(EvidenceGap::Uncaptured)
        );
        assert_eq!(
            evidence(
                &[
                    directory_observation(false),
                    payload("/w/include/Shared.inc")
                ],
                &CapturedReads::default()
            ),
            Err(EvidenceGap::Uncaptured)
        );
        // A miss is listed by the fallback; that listing is the evidence.
        let listing = CapturedReads {
            listings: vec![CapturedListing {
                directory: directory.clone(),
                listing: Digest::from_bytes([1; 32]),
                complete: true,
                overlay: false,
            }],
            ..CapturedReads::default()
        };
        assert_eq!(
            evidence(
                &[directory_observation(false), directory_observation(true)],
                &listing
            ),
            Ok(vec![Observation::Listing {
                path: directory.clone(),
                entry: entry.clone(),
                listing: Digest::from_bytes([1; 32]),
            }])
        );
    }

    #[test]
    fn context_metadata_is_proven_by_the_context_key_not_repeated() {
        let stat = pascal_project::MetadataObservation::Stat {
            path: PathBuf::from("/w/App.dproj"),
        };
        let context = ProjectObservations {
            metadata_observations: vec![stat.clone()],
            ..ProjectObservations::default()
        };
        let observed = [ResolutionObservation::Metadata(stat)];
        assert_eq!(
            session_evidence(
                &observed,
                &CapturedReads::default(),
                &context,
                &AtomicBool::new(false)
            ),
            Ok(Vec::new())
        );
        let other = [ResolutionObservation::Metadata(
            pascal_project::MetadataObservation::Stat {
                path: PathBuf::from("/w/Package.dpk"),
            },
        )];
        assert_eq!(
            session_evidence(
                &other,
                &CapturedReads::default(),
                &context,
                &AtomicBool::new(false)
            ),
            Err(EvidenceGap::Uncaptured)
        );
    }

    #[test]
    fn an_include_deleted_in_the_editor_leaves_no_evidence() {
        let indexed = index_with_edits(&files(), &[], &["include/Shared.inc"], &["Main.pas"]);
        assert_eq!(
            indexed
                .workspace
                .source_evidence(&indexed.uri("Main.pas"), &AtomicBool::new(false)),
            Err(EvidenceGap::Overlay)
        );
    }

    #[test]
    fn a_missing_search_directory_is_the_empty_listing() {
        let dproj = b"<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_IncludePath>nowhere</DCC_IncludePath></PropertyGroup></Project>";
        let indexed = index(&[("App.dproj", dproj), ("Main.pas", MAIN)], &["Main.pas"]);
        let evidence = indexed
            .workspace
            .source_evidence(&indexed.uri("Main.pas"), &AtomicBool::new(false))
            .unwrap();
        assert!(
            evidence.iter().any(|observation| matches!(
                observation,
                Observation::Listing { path, listing, .. }
                    if path.ends_with("nowhere") && *listing == listing_digest(&[], &[])
            )),
            "{evidence:?}"
        );
    }

    #[test]
    fn evidence_uses_the_context_that_produced_the_session() {
        let mut indexed = index(&files(), &["Main.pas"]);
        let uri = indexed.uri("Main.pas");
        assert!(
            indexed
                .workspace
                .source_evidence(&uri, &AtomicBool::new(false))
                .is_ok()
        );
        // The document's mapping moves on; the expansion keeps its own key.
        indexed.workspace.document_contexts.remove(&uri);
        assert!(
            indexed
                .workspace
                .source_evidence(&uri, &AtomicBool::new(false))
                .is_ok()
        );
        let produced = indexed.workspace.expansions[&uri].context_key.clone();
        indexed.workspace.contexts.remove(&produced);
        assert_eq!(
            indexed
                .workspace
                .source_evidence(&uri, &AtomicBool::new(false)),
            Err(EvidenceGap::NoSession)
        );
    }
}
