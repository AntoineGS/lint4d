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
    Bypass, Digest, ImporterHit, Lookup, MemberRecord, MemberRecordHeader, Miss, SnapshotMember,
    SourceIdentity, Store,
};
use pascal_project::{ProjectContext, ProjectPathEntry};
use std::path::PathBuf;

/// Store output that names a source.
pub(crate) trait RecordSource {
    fn source(&self) -> &SourceIdentity;
}

impl RecordSource for MemberRecord {
    fn source(&self) -> &SourceIdentity {
        &self.header.source
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refused {
    /// No path entry of the requesting context names the source.
    NoPathEntry(PathBuf),
    /// The requesting context's current read policy does not allow it.
    Policy(PathBuf),
    /// The hit's record is no longer readable.
    Store(Miss),
    Bypass(Bypass),
}

/// `value`, if `context` may read its source now.
pub(crate) fn authorize_record<T: RecordSource>(
    value: T,
    context: &ProjectContext,
) -> Result<Authorized<T>, Refused> {
    let path = &value.source().canonical;
    let Some(entry) = context.path_entry_for(path) else {
        return Err(Refused::NoPathEntry(path.clone()));
    };
    let policy = context.read_policy();
    if !policy.allows_location(&entry) || !policy.allows_entry(&entry) {
        return Err(Refused::Policy(path.clone()));
    }
    Ok(Authorized { value, entry })
}

/// A query hit, authorized through the record it names: the record's
/// header is opened (its source is part of the verified header) and its
/// source authorized. The record's bindings are still unverified until
/// read.
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
    use pascal_cache::{
        ContextKey as CacheContextKey, CoverageStatus, ImporterHit, Lookup, MemberCoverage,
        MemberHeader, SnapshotCoverage, SnapshotSeal,
    };
    use pascal_project::delphi_overrides::EffectiveOverrides;
    use pascal_project::{ProjectContextParts, ProjectPathEntry, ReadPolicy, SourceOrigin};
    use std::path::Path;

    /// A context whose path entries are `entries` and whose read policy
    /// allows `readable`.
    fn context(entries: &[&Path], readable: &[&Path]) -> ProjectContext {
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
                &[],
                &EffectiveOverrides::default(),
            ),
            ..ProjectContextParts::default()
        })
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
        for (name, text) in [("app/Main.pas", "unit Main;"), ("lib/Lib.pas", "unit Lib;")] {
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
}
