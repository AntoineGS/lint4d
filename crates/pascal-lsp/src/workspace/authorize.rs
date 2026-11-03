//! Authorization at consumption (spec §7 amendment 2026-11-02). Member
//! records are context-free and a store lookup never authorizes a read, so
//! every record or query hit passes here before use: its source identity
//! must map to a path entry of the requesting context and pass that
//! context's current read policy. A wider session's rows present in the
//! database are refused to a narrower requester. Authorization is not
//! freshness: member and binding evidence are still validated, and the
//! validator authorizes every payload it reads.
#![cfg_attr(not(test), allow(dead_code))]

use pascal_cache::{
    Binding, BindingOutcome, Bypass, Digest, ImporterHit, Lookup, MemberRecord, MemberRecordHeader,
    Miss, SnapshotMember, SourceIdentity, Store,
};
use pascal_project::{ProjectContext, ProjectPathEntry};
use std::path::{Path, PathBuf};

/// Store output that names a source.
pub(crate) trait RecordSource {
    fn source(&self) -> &SourceIdentity;

    /// The bindings the value carries, whose resolved paths are consumed
    /// with it. A header carries none: its bindings stay unusable until the
    /// full record is authorized.
    fn bindings(&self) -> &[Binding] {
        &[]
    }
}

impl RecordSource for MemberRecord {
    fn source(&self) -> &SourceIdentity {
        &self.header.source
    }

    fn bindings(&self) -> &[Binding] {
        &self.bindings
    }
}

impl RecordSource for MemberRecordHeader {
    fn source(&self) -> &SourceIdentity {
        &self.member().source
    }
}

/// A query hit, which names only its record.
pub(crate) trait RecordHit {
    fn record_id(&self) -> Digest;
}

impl RecordHit for SnapshotMember {
    fn record_id(&self) -> Digest {
        self.record_id
    }
}

impl RecordHit for ImporterHit {
    fn record_id(&self) -> Digest {
        self.record_id
    }
}

/// A value the requesting context may read, with the entry that allows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Authorized<T> {
    value: T,
    entry: ProjectPathEntry,
}

impl<T> Authorized<T> {
    pub(crate) fn value(&self) -> &T {
        &self.value
    }

    /// The requester's own entry for the source, under which its payload
    /// is read and validated.
    pub(crate) fn entry(&self) -> &ProjectPathEntry {
        &self.entry
    }

    pub(crate) fn into_inner(self) -> T {
        self.value
    }
}

/// Why a requester may not read a record. Consumers must never show the
/// refused path to the requester: it would reveal a wider session's file
/// (spec §8.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refused {
    /// No path entry of the requesting context names the source.
    NoPathEntry(PathBuf),
    /// The requesting context's current read policy does not allow it.
    Policy(PathBuf),
    /// A binding's `Precise` target or `Ambiguous` candidate is refused;
    /// `ordinal` is the binding's index in the record.
    Binding {
        ordinal: usize,
        reason: Box<Refused>,
    },
    /// The hit's record is no longer readable.
    Store(Miss),
    Bypass(Bypass),
}

/// The requester's entry for `path`, if its context names the path and its
/// current read policy allows it.
fn authorize_path(path: &Path, context: &ProjectContext) -> Result<ProjectPathEntry, Refused> {
    let Some(entry) = context.path_entry_for(path) else {
        return Err(Refused::NoPathEntry(path.to_path_buf()));
    };
    let policy = context.read_policy();
    if !policy.allows_location(&entry) || !policy.allows_entry(&entry) {
        return Err(Refused::Policy(path.to_path_buf()));
    }
    Ok(entry)
}

/// `value`, if `context` may read its source now and, for a record, every
/// path its bindings resolved to: a refused one refuses the whole record.
pub(crate) fn authorize_record<T: RecordSource>(
    value: T,
    context: &ProjectContext,
) -> Result<Authorized<T>, Refused> {
    let entry = authorize_path(&value.source().canonical, context)?;
    for (ordinal, binding) in value.bindings().iter().enumerate() {
        let paths: &[SourceIdentity] = match &binding.outcome {
            BindingOutcome::Precise { target } => std::slice::from_ref(target),
            BindingOutcome::Ambiguous { candidates, .. } => candidates,
            BindingOutcome::Unavailable | BindingOutcome::Incomplete(_) => &[],
        };
        for path in paths {
            authorize_path(&path.canonical, context).map_err(|reason| Refused::Binding {
                ordinal,
                reason: Box::new(reason),
            })?;
        }
    }
    Ok(Authorized { value, entry })
}

/// A query hit, authorized through the record it names: the record's
/// header is opened (its source is part of the verified header) and its
/// source authorized. Its bindings are unusable until the full record is
/// read and passed to `authorize_record`.
pub(crate) fn authorize_hit<H: RecordHit>(
    store: &Store,
    hit: &H,
    context: &ProjectContext,
) -> Result<Authorized<MemberRecordHeader>, Refused> {
    match store
        .open_member_record(hit.record_id())
        .map_err(Refused::Bypass)?
    {
        Lookup::Hit(header) => authorize_record(header, context),
        Lookup::Miss(miss) => Err(Refused::Store(miss)),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::workspace::persist::tests::{open_store, private_dir};
    use pascal_cache::{BindingKind, IncompleteReason};
    use pascal_cache::{
        ContextKey as CacheContextKey, CoverageStatus, EvidenceTarget, EvidenceWrite, ImporterHit,
        Lookup, LookupInputs, LookupKey, LookupKind, MemberCoverage, MemberHeader, Observation,
        SnapshotCoverage, SnapshotSeal,
    };
    use pascal_project::delphi_overrides::EffectiveOverrides;
    use pascal_project::{ProjectContextParts, ProjectPathEntry, ReadPolicy, SourceOrigin};
    use std::path::Path;

    /// A context whose path entries are `entries` and whose read policy
    /// allows `readable`.
    fn context(entries: &[&Path], readable: &[&Path]) -> ProjectContext {
        context_excluding(entries, readable, &[])
    }

    fn context_excluding(
        entries: &[&Path],
        readable: &[&Path],
        exclusions: &[&str],
    ) -> ProjectContext {
        ProjectContext::from_parts(ProjectContextParts {
            discovery_complete: true,
            search_path_entries: entries
                .iter()
                .map(|path| ProjectPathEntry::configured(path.to_path_buf()))
                .collect(),
            read_policy: ReadPolicy::new(
                &readable
                    .iter()
                    .map(|path| path.to_path_buf())
                    .collect::<Vec<_>>(),
                &[],
                &exclusions
                    .iter()
                    .map(|glob| glob.to_string())
                    .collect::<Vec<_>>(),
                &EffectiveOverrides::default(),
            ),
            ..ProjectContextParts::default()
        })
    }

    fn identity(path: &Path) -> SourceIdentity {
        SourceIdentity {
            canonical: path.to_path_buf(),
            logical: path.to_path_buf(),
            spelling: None,
            uri: None,
        }
    }

    fn binding(occurrence: u64, outcome: BindingOutcome) -> Binding {
        Binding {
            kind: BindingKind::Import,
            occurrence,
            outcome,
            evidence: None,
        }
    }

    fn record_binding(path: &Path, bindings: Vec<Binding>) -> MemberRecord {
        MemberRecord {
            bindings,
            ..record(path)
        }
    }

    fn record(path: &Path) -> MemberRecord {
        let complete = CoverageStatus::Complete;
        MemberRecord {
            header: MemberHeader {
                source: SourceIdentity {
                    canonical: path.to_path_buf(),
                    logical: path.to_path_buf(),
                    spelling: None,
                    uri: None,
                },
                artifact: None,
                member_evidence: None,
                origin: SourceOrigin::Library,
                fallback_unit_name: None,
                coverage: MemberCoverage {
                    extraction: complete,
                    resolution: complete,
                },
            },
            bindings: Vec::new(),
        }
    }

    /// `root/app/Main.pas` and `root/lib/Lib.pas`, with `root` canonical so
    /// no symlink component (a macOS `/var`) refuses everything.
    fn tree() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for (name, text) in [
            ("app/Main.pas", "unit Main;"),
            ("app/Dep.pas", "unit Dep;"),
            ("lib/Lib.pas", "unit Lib;"),
            ("library/Lib.pas", "unit Lib;"),
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        (temp, root)
    }

    #[test]
    fn a_wider_policy_never_authorizes_a_narrower_requester() {
        let (_temp, root) = tree();
        let (app, lib) = (root.join("app"), root.join("lib/Lib.pas"));
        let wide = context(&[&root], &[&root]);
        let narrow = context(&[&app], &[&app]);
        let narrower_policy = context(&[&root], &[&app]);
        let authorized = authorize_record(record(&lib), &wide).unwrap();
        assert_eq!(authorized.entry().path, lib);
        assert_eq!(authorized.value().header.source.canonical, lib);
        assert_eq!(authorized.into_inner(), record(&lib));
        assert_eq!(
            authorize_record(record(&lib), &narrow).err(),
            Some(Refused::NoPathEntry(lib.clone())),
            "no entry of the narrower context names it"
        );
        assert_eq!(
            authorize_record(record(&lib), &narrower_policy).err(),
            Some(Refused::Policy(lib.clone())),
            "an entry exists but the current policy does not allow it"
        );
        let missing = root.join("lib/Gone.pas");
        assert_eq!(
            authorize_record(record(&missing), &wide).err(),
            Some(Refused::Policy(missing)),
            "the policy reads only regular files"
        );
    }

    #[test]
    fn query_hits_are_authorized_through_their_record() {
        let (_temp, root) = tree();
        let lib = root.join("lib/Lib.pas");
        let directory = private_dir();
        let store = open_store(directory.path());
        let record_id = store
            .publish_member_record(&record(&lib))
            .unwrap()
            .record_id;
        let cache_context = CacheContextKey::new(&ProjectContext::default());
        let mut builder = store.begin_snapshot(&cache_context).unwrap();
        store.add_members(&mut builder, &[record_id]).unwrap();
        let complete = CoverageStatus::Complete;
        let sealed = store
            .seal_snapshot(
                builder,
                &SnapshotSeal {
                    discovery: None,
                    coverage: SnapshotCoverage {
                        discovery: complete,
                        membership: complete,
                    },
                },
            )
            .unwrap();
        let Lookup::Hit(member) = store.find_member(&sealed, &lib).unwrap() else {
            panic!("the member is found by its canonical path")
        };
        let wide = context(&[&root], &[&root]);
        let narrow = context(&[&root.join("app")], &[&root.join("app")]);
        let authorized = authorize_hit(&store, &member, &wide).unwrap();
        assert_eq!(authorized.value().member().source.canonical, lib);
        assert_eq!(
            authorize_hit(&store, &member, &narrow).err(),
            Some(Refused::NoPathEntry(lib.clone())),
            "the rows exist, but not for this requester"
        );
        let importer = ImporterHit {
            record_id,
            binding_ordinal: 0,
            member_ordinal: 0,
        };
        assert!(authorize_hit(&store, &importer, &wide).is_ok());
        let unknown = ImporterHit {
            record_id: Digest::from_bytes([42; 32]),
            ..importer
        };
        assert_eq!(
            authorize_hit(&store, &unknown, &wide).err(),
            Some(Refused::Store(Miss::Absent))
        );
    }

    #[test]
    fn a_binding_target_outside_the_requester_refuses_the_whole_record() {
        let (_temp, root) = tree();
        let app = root.join("app");
        let (main, dep, lib) = (
            app.join("Main.pas"),
            app.join("Dep.pas"),
            root.join("lib/Lib.pas"),
        );
        let narrow = context(&[&app], &[&app]);
        let inside = record_binding(
            &main,
            vec![binding(
                0,
                BindingOutcome::Precise {
                    target: identity(&dep),
                },
            )],
        );
        assert!(authorize_record(inside, &narrow).is_ok());
        let outside = record_binding(
            &main,
            vec![
                binding(
                    0,
                    BindingOutcome::Precise {
                        target: identity(&dep),
                    },
                ),
                binding(
                    1,
                    BindingOutcome::Precise {
                        target: identity(&lib),
                    },
                ),
            ],
        );
        assert_eq!(
            authorize_record(outside, &narrow).err(),
            Some(Refused::Binding {
                ordinal: 1,
                reason: Box::new(Refused::NoPathEntry(lib.clone())),
            }),
            "the source is inside, but a resolved target is not"
        );
        let ambiguous = |candidates: Vec<&Path>| {
            record_binding(
                &main,
                vec![binding(
                    0,
                    BindingOutcome::Ambiguous {
                        candidates: candidates.into_iter().map(identity).collect(),
                        overflow: false,
                    },
                )],
            )
        };
        assert!(authorize_record(ambiguous(vec![&main, &dep]), &narrow).is_ok());
        assert_eq!(
            authorize_record(ambiguous(vec![&dep, &lib]), &narrow).err(),
            Some(Refused::Binding {
                ordinal: 0,
                reason: Box::new(Refused::NoPathEntry(lib.clone())),
            }),
            "one candidate outside refuses the record"
        );
        let unresolved = record_binding(
            &main,
            vec![
                binding(0, BindingOutcome::Unavailable),
                binding(1, BindingOutcome::Incomplete(IncompleteReason::Unsupported)),
            ],
        );
        assert!(authorize_record(unresolved, &narrow).is_ok());
        let policy_only = context(&[&root], &[&app]);
        let wide_target = record_binding(
            &main,
            vec![binding(
                0,
                BindingOutcome::Precise {
                    target: identity(&lib),
                },
            )],
        );
        assert_eq!(
            authorize_record(wide_target, &policy_only).err(),
            Some(Refused::Binding {
                ordinal: 0,
                reason: Box::new(Refused::Policy(lib)),
            })
        );
    }

    /// Header authorization covers the source only: the stored header of a
    /// record whose binding target is outside the requester is authorized,
    /// while the same record read in full is refused for that binding.
    #[test]
    fn an_authorized_header_does_not_authorize_its_bindings() {
        let (_temp, root) = tree();
        let app = root.join("app");
        let (main, lib) = (app.join("Main.pas"), root.join("lib/Lib.pas"));
        let narrow = context(&[&app], &[&app]);
        let directory = private_dir();
        let store = open_store(directory.path());
        let lookup = LookupKey::new(&LookupInputs {
            kind: LookupKind::Unit,
            normalized_name: "lib",
            path_expression: None,
            importer_directory: None,
        });
        let evidence = store
            .publish_evidence(
                EvidenceTarget::Binding(&lookup),
                &EvidenceWrite {
                    context: &CacheContextKey::new(&ProjectContext::default()),
                    observations: &[Observation::Presence {
                        path: lib.clone(),
                        entry: None,
                        present: true,
                    }],
                },
            )
            .unwrap()
            .evidence_id;
        let outside = record_binding(
            &main,
            vec![Binding {
                evidence: Some(evidence),
                ..binding(
                    0,
                    BindingOutcome::Precise {
                        target: identity(&lib),
                    },
                )
            }],
        );
        let record_id = store.publish_member_record(&outside).unwrap().record_id;
        let Lookup::Hit(header) = store.open_member_record(record_id).unwrap() else {
            panic!("published")
        };
        assert_eq!(header.member(), &outside.header);
        let authorized = authorize_record(header, &narrow).unwrap();
        assert_eq!(authorized.entry().path, main);
        assert_eq!(
            authorize_record(outside, &narrow).err(),
            Some(Refused::Binding {
                ordinal: 0,
                reason: Box::new(Refused::NoPathEntry(lib)),
            })
        );
    }

    #[test]
    fn paths_outside_a_root_by_prefix_dotdot_or_exclusion_are_refused() {
        let (_temp, root) = tree();
        let lib = root.join("lib");
        let narrow = context(&[&lib], &[&lib]);
        let sibling = root.join("library/Lib.pas");
        assert!(sibling.is_file());
        assert!(
            authorize_record(record(&sibling), &narrow).is_err(),
            "root/library is not under root/lib"
        );
        let escaping = lib.join("../app/Main.pas");
        assert!(
            authorize_record(record(&escaping), &narrow).is_err(),
            "a .. segment does not stay inside the root"
        );
        let excluded = context_excluding(&[&root], &[&root], &["lib/*.pas"]);
        assert!(authorize_record(record(&root.join("app/Main.pas")), &excluded).is_ok());
        assert!(
            authorize_record(record(&lib.join("Lib.pas")), &excluded).is_err(),
            "an excluded path is refused"
        );
    }

    #[test]
    fn a_retired_record_is_refused_as_retired() {
        let (_temp, root) = tree();
        let directory = private_dir();
        let store = open_store(directory.path());
        let record_id = store
            .publish_member_record(&record(&root.join("lib/Lib.pas")))
            .unwrap()
            .record_id;
        assert!(store.retire_member_record(record_id).unwrap());
        let hit = ImporterHit {
            record_id,
            binding_ordinal: 0,
            member_ordinal: 0,
        };
        assert_eq!(
            authorize_hit(&store, &hit, &context(&[&root], &[&root])).err(),
            Some(Refused::Store(Miss::Retired))
        );
    }
}
