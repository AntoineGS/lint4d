#![cfg(unix)]

mod common;

use common::private_tempdir;
use common::records::{
    binding_evidence, member, open, raw, read_members, read_record, records, sealed,
};
use pascal_cache::{
    Binding, BindingKind, BindingOutcome, ImporterHit, Lookup, Miss, SourceIdentity, Store,
};
use std::collections::BTreeSet;
use std::path::Path;

/// Every importer of `target` in the snapshot, page by page.
fn all_importers(
    store: &Store,
    header: &pascal_cache::SnapshotHeader,
    target: &str,
    limit: usize,
) -> Vec<ImporterHit> {
    let mut hits = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .find_importers(header, Path::new(target), after, limit)
            .unwrap();
        assert!(page.hits.len() <= limit);
        hits.extend_from_slice(&page.hits);
        match page.next {
            Some(next) => after = Some(next),
            None => return hits,
        }
    }
}

#[test]
fn importers_of_a_target_page_through_one_snapshot() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "target", "/w/Target0.pas");
    let target = records(&store, "Target", 1)[0];
    let importers: Vec<_> = (0..300)
        .map(|index| {
            store
                .publish_member_record(&member(
                    &format!("/w/I{index}.pas"),
                    &["/w/Target0.pas"],
                    evidence,
                ))
                .unwrap()
                .record_id
        })
        .collect();
    let outside = store
        .publish_member_record(&member("/w/Outside.pas", &["/w/Target0.pas"], evidence))
        .unwrap()
        .record_id;
    let mut members = vec![target];
    members.extend(&importers);
    let header = sealed(&store, &members);
    let hits = all_importers(&store, &header, "/w/Target0.pas", 100);
    assert_eq!(hits.len(), 300);
    assert!(
        hits.windows(2).all(|pair| pair[0] < pair[1]),
        "keyset order"
    );
    assert_eq!(
        hits.iter()
            .map(|hit| hit.record_id)
            .collect::<BTreeSet<_>>(),
        importers.iter().copied().collect::<BTreeSet<_>>(),
        "only this snapshot's members"
    );
    let listed = read_members(&store, &header).unwrap();
    for hit in &hits {
        assert_eq!(hit.binding_ordinal, 0);
        assert_eq!(listed[hit.member_ordinal as usize].record_id, hit.record_id);
    }
    assert_eq!(
        all_importers(&store, &header, "/w/Target0.pas", 1_000),
        hits
    );
    let Lookup::Hit(found) = store
        .find_member(&header, Path::new("/w/Target0.pas"))
        .unwrap()
    else {
        panic!("the target is a member")
    };
    assert_eq!((found.ordinal, found.record_id), (0, target));
    assert_eq!(
        store
            .find_member(&header, Path::new("/lib/System.pas"))
            .unwrap(),
        Lookup::Miss(Miss::Absent),
        "a target outside the snapshot is not an error"
    );
    let other = sealed(&store, &[target, outside]);
    let other_hits = all_importers(&store, &other, "/w/Target0.pas", 100);
    assert_eq!(
        other_hits
            .iter()
            .map(|hit| hit.record_id)
            .collect::<Vec<_>>(),
        [outside],
        "snapshots never mix"
    );
}

#[test]
fn binding_targets_resolve_by_canonical_path_not_spelling() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "b", "/w/B.pas");
    let target = records(&store, "B", 1)[0]; // `/w/B0.pas`
    let spelled = SourceIdentity {
        canonical: "/w/B0.pas".into(),
        logical: "/w/sub/../B0.pas".into(),
        spelling: Some("b0".into()),
        uri: Some("file:///w/sub/../B0.pas".into()),
    };
    let mut importer = member("/w/Main.pas", &[], evidence);
    importer.bindings.push(Binding {
        kind: BindingKind::Import,
        occurrence: 0,
        outcome: BindingOutcome::Precise {
            target: spelled.clone(),
        },
        evidence: Some(evidence),
    });
    let importer_id = store.publish_member_record(&importer).unwrap().record_id;
    // Another canonical spelling is another identity: never normalized.
    let other_case = store
        .publish_member_record(&member("/w/Other.pas", &["/w/b0.pas"], evidence))
        .unwrap()
        .record_id;
    let header = sealed(&store, &[target, importer_id, other_case]);
    let hits = store
        .find_importers(&header, &spelled.canonical, None, 10)
        .unwrap()
        .hits;
    assert_eq!(
        hits.iter().map(|hit| hit.record_id).collect::<Vec<_>>(),
        [importer_id]
    );
    let Lookup::Hit(found) = store.find_member(&header, &spelled.canonical).unwrap() else {
        panic!("the binding target resolves to the member")
    };
    assert_eq!(found.record_id, target);
    assert_eq!(
        store.find_member(&header, Path::new("/w/b0.pas")).unwrap(),
        Lookup::Miss(Miss::Absent)
    );
    let stored = read_record(&store, importer_id).unwrap();
    assert_eq!(
        stored.bindings[0].outcome.target(),
        Some(&spelled),
        "spelling kept"
    );
}

#[test]
fn queries_on_a_retired_snapshot_find_nothing() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "target", "/w/Target0.pas");
    let target = records(&store, "Target", 1)[0];
    let importer = store
        .publish_member_record(&member("/w/Main.pas", &["/w/Target0.pas"], evidence))
        .unwrap()
        .record_id;
    let header = sealed(&store, &[target, importer]);
    assert!(store.retire_snapshot(header.id()).unwrap());
    assert_eq!(
        store
            .find_member(&header, Path::new("/w/Target0.pas"))
            .unwrap(),
        Lookup::Miss(Miss::Absent)
    );
    assert!(
        store
            .find_importers(&header, Path::new("/w/Target0.pas"), None, 10)
            .unwrap()
            .hits
            .is_empty()
    );
}

#[test]
fn queries_exclude_members_whose_record_was_retired() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "target", "/w/Target0.pas");
    let target = records(&store, "Target", 1)[0];
    let importers: Vec<_> = (0..3)
        .map(|index| {
            store
                .publish_member_record(&member(
                    &format!("/w/I{index}.pas"),
                    &["/w/Target0.pas"],
                    evidence,
                ))
                .unwrap()
                .record_id
        })
        .collect();
    let mut members = vec![target];
    members.extend(&importers);
    let header = sealed(&store, &members);
    assert!(store.retire_member_record(importers[1]).unwrap());
    let hits = all_importers(&store, &header, "/w/Target0.pas", 10);
    assert_eq!(
        hits.iter()
            .map(|hit| hit.record_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([importers[0], importers[2]]),
        "a retired importer is gone, the others stay"
    );
    assert!(matches!(
        store.find_member(&header, Path::new("/w/I1.pas")).unwrap(),
        Lookup::Miss(Miss::Absent)
    ));
    let Lookup::Hit(found) = store.find_member(&header, Path::new("/w/I0.pas")).unwrap() else {
        panic!("a live member is still found")
    };
    assert_eq!(found.record_id, importers[0]);
    assert!(store.retire_member_record(target).unwrap());
    assert_eq!(
        store
            .find_member(&header, Path::new("/w/Target0.pas"))
            .unwrap(),
        Lookup::Miss(Miss::Absent)
    );
}

fn plan(directory: &Path, query: &str, parameters: usize) -> String {
    let connection = raw(directory);
    let mut statement = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
        .unwrap();
    let values: Vec<rusqlite::types::Value> = (0..parameters)
        .map(|_| rusqlite::types::Value::Blob(vec![0; 32]))
        .collect();
    let rows: Vec<String> = statement
        .query_map(rusqlite::params_from_iter(values), |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows.join("\n")
}

#[test]
fn snapshot_queries_use_their_indexes_without_sorting() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let ids = records(&store, "U", 2);
    sealed(&store, &ids);
    let importers = plan(directory.path(), pascal_cache::IMPORTERS_QUERY_FOR_TEST, 8);
    assert!(
        importers.contains("member_bindings_by_target"),
        "{importers}"
    );
    assert!(!importers.contains("TEMP B-TREE"), "{importers}");
    let member = plan(directory.path(), pascal_cache::MEMBER_QUERY_FOR_TEST, 5);
    assert!(!member.contains("SCAN"), "{member}");
    assert!(!member.contains("TEMP B-TREE"), "{member}");
}
