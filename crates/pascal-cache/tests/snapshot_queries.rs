#![cfg(unix)]

mod common;

use common::private_tempdir;
use common::records::{
    ambiguous, binding_evidence, context, member, open, raw, read_members, read_record, records,
    sealed,
};
use pascal_cache::{
    Binding, BindingKind, BindingOutcome, Bypass, Dependency, ImporterHit, IncompleteReason,
    Lookup, Malformed, Miss, POSTING_ROW, SourceIdentity, Store, path_key,
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
        let Lookup::Hit(page) = store
            .find_importers(header, Path::new(target), after, limit)
            .unwrap()
        else {
            panic!("a sealed snapshot answers")
        };
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
    let hits = all_importers(&store, &header, "/w/B0.pas", 10);
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

/// 13f: `find_member_record` answers what `find_member` then
/// `open_member_record` answer, in one transaction.
#[test]
fn a_member_and_its_record_are_found_together() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "target", "/w/Target0.pas");
    let target = records(&store, "Target", 1)[0];
    let importer = store
        .publish_member_record(&member("/w/Main.pas", &["/w/Target0.pas"], evidence))
        .unwrap()
        .record_id;
    let header = sealed(&store, &[target, importer]);
    for path in ["/w/Target0.pas", "/w/Main.pas"] {
        let Lookup::Hit(member) = store.find_member(&header, Path::new(path)).unwrap() else {
            panic!("{path} is a member")
        };
        let record = store.open_member_record(member.record_id).unwrap();
        assert!(matches!(record, Lookup::Hit(_)), "{path}");
        assert_eq!(
            store.find_member_record(&header, Path::new(path)).unwrap(),
            Lookup::Hit((member, record)),
            "{path}"
        );
    }
    assert_eq!(
        store
            .find_member_record(&header, Path::new("/lib/System.pas"))
            .unwrap(),
        Lookup::Miss(Miss::Absent)
    );
    // A stored header that disagrees with its columns: the member is
    // found, its record is malformed, as `open_member_record` says.
    raw(directory.path())
        .execute(
            "UPDATE member_records SET artifact_id = zeroblob(32) WHERE record_id = ?1",
            [importer.as_bytes().as_slice()],
        )
        .unwrap();
    let Lookup::Hit((found, record)) = store
        .find_member_record(&header, Path::new("/w/Main.pas"))
        .unwrap()
    else {
        panic!("the member is still listed")
    };
    assert_eq!(found.record_id, importer);
    assert_eq!(record, store.open_member_record(importer).unwrap());
    assert_eq!(record, Lookup::Miss(Miss::Malformed(Malformed::Member)));
    assert!(store.retire_snapshot(header.id()).unwrap());
    assert_eq!(
        store
            .find_member_record(&header, Path::new("/w/Target0.pas"))
            .unwrap(),
        Lookup::Miss(Miss::Retired)
    );
}

#[test]
fn queries_on_a_retired_or_vanished_snapshot_are_a_miss() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "target", "/w/Target0.pas");
    let target = records(&store, "Target", 1)[0];
    let importer = store
        .publish_member_record(&member("/w/Main.pas", &["/w/Target0.pas"], evidence))
        .unwrap()
        .record_id;
    let header = sealed(&store, &[target, importer]);
    let gone = sealed(&store, &[target, importer]);
    assert!(store.retire_snapshot(header.id()).unwrap());
    assert_eq!(
        store
            .find_member(&header, Path::new("/w/Target0.pas"))
            .unwrap(),
        Lookup::Miss(Miss::Retired)
    );
    assert_eq!(
        store
            .find_importers(&header, Path::new("/w/Target0.pas"), None, 10)
            .unwrap(),
        Lookup::Miss(Miss::Retired),
        "never an empty last page"
    );
    let connection = raw(directory.path());
    connection
        .execute(
            "DELETE FROM snapshot_members WHERE snapshot_id = ?1",
            [gone.id().as_bytes().as_slice()],
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM snapshots WHERE snapshot_id = ?1",
            [gone.id().as_bytes().as_slice()],
        )
        .unwrap();
    assert_eq!(
        store
            .find_member(&gone, Path::new("/w/Target0.pas"))
            .unwrap(),
        Lookup::Miss(Miss::Vanished)
    );
    assert_eq!(
        store
            .find_importers(&gone, Path::new("/w/Target0.pas"), None, 10)
            .unwrap(),
        Lookup::Miss(Miss::Vanished)
    );
}

#[test]
fn a_snapshot_retired_between_importer_pages_is_a_miss_not_a_last_page() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "target", "/w/Target0.pas");
    let target = records(&store, "Target", 1)[0];
    let importers: Vec<_> = (0..4)
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
    let Lookup::Hit(first) = store
        .find_importers(&header, Path::new("/w/Target0.pas"), None, 2)
        .unwrap()
    else {
        panic!("the snapshot is sealed")
    };
    assert_eq!(first.hits.len(), 2);
    assert!(first.next.is_some());
    assert!(store.retire_snapshot(header.id()).unwrap());
    assert_eq!(
        store
            .find_importers(&header, Path::new("/w/Target0.pas"), first.next, 2)
            .unwrap(),
        Lookup::Miss(Miss::Retired)
    );
}

#[test]
fn several_bindings_of_one_record_split_across_pages_without_gaps_or_repeats() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "target", "/w/Target0.pas");
    let target = records(&store, "Target", 1)[0];
    let targets = ["/w/Target0.pas"; 5];
    let importers: Vec<_> = ["/w/A.pas", "/w/B.pas"]
        .iter()
        .map(|path| {
            store
                .publish_member_record(&member(path, &targets, evidence))
                .unwrap()
                .record_id
        })
        .collect();
    let header = sealed(&store, &[target, importers[0], importers[1]]);
    let mut expected: Vec<_> = importers
        .iter()
        .flat_map(|record| (0..5).map(move |ordinal| (*record, ordinal)))
        .collect();
    expected.sort();
    for limit in [1, 2, 3, 4, 7] {
        let hits = all_importers(&store, &header, "/w/Target0.pas", limit);
        assert_eq!(
            hits.iter()
                .map(|hit| (hit.record_id, hit.binding_ordinal))
                .collect::<Vec<_>>(),
            expected,
            "limit {limit}"
        );
    }
}

#[test]
fn a_forged_cursor_past_every_ordinal_never_wraps() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "target", "/w/Target0.pas");
    let target = records(&store, "Target", 1)[0];
    let importers: Vec<_> = ["/w/A.pas", "/w/B.pas"]
        .iter()
        .map(|path| {
            store
                .publish_member_record(&member(path, &["/w/Target0.pas"], evidence))
                .unwrap()
                .record_id
        })
        .collect();
    let header = sealed(&store, &[target, importers[0], importers[1]]);
    let first = importers.iter().min().copied().unwrap();
    let forged = ImporterHit {
        record_id: first,
        binding_ordinal: u64::MAX,
        dependency: Dependency::Precise,
        member_ordinal: 0,
    };
    let Lookup::Hit(page) = store
        .find_importers(&header, Path::new("/w/Target0.pas"), Some(forged), 10)
        .unwrap()
    else {
        panic!("the snapshot is sealed")
    };
    assert_eq!(
        page.hits
            .iter()
            .map(|hit| hit.record_id)
            .collect::<Vec<_>>(),
        [importers.iter().max().copied().unwrap()],
        "nothing of the cursor's record repeats"
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
        importers
            .lines()
            .next()
            .is_some_and(|step| step.starts_with("SEARCH t ")
                && step.contains("INDEX binding_targets_by_target ")),
        "binding_targets drives through its target index: {importers}"
    );
    assert!(!importers.contains("SCAN"), "{importers}");
    assert!(!importers.contains("TEMP B-TREE"), "{importers}");
    let member = plan(directory.path(), pascal_cache::MEMBER_QUERY_FOR_TEST, 5);
    assert!(!member.contains("SCAN"), "{member}");
    assert!(!member.contains("TEMP B-TREE"), "{member}");
    // l4d-c8l2: one record lookup per matching row, never a walk of every
    // live record.
    for filter in [
        pascal_cache::CONTAINING_FILTER_FOR_TEST,
        pascal_cache::EQUAL_FILTER_FOR_TEST,
    ] {
        let rows = plan(
            directory.path(),
            &pascal_cache::members_with_rows_query_for_test(filter),
            8,
        );
        assert!(!rows.contains("SCAN"), "{rows}");
        assert!(rows.contains("INDEX artifact_rows_by_index_key "), "{rows}");
        assert!(rows.contains("INDEX member_records_by_artifact "), "{rows}");
        assert!(rows.contains("INDEX snapshot_members_by_record "), "{rows}");
    }
}

/// Spec §10: a query's result vector is charged to the scratch before it is
/// allocated; with no scratch left every query bypasses.
#[test]
fn queries_reserve_their_results_before_allocating_them() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let header = sealed(&store, &records(&store, "M", 2));
    let member = Path::new("/w/M0.pas");
    let free = store.limits().scratch_bytes - store.scratch_in_use();
    let hold = store.hold_scratch_for_test(free).unwrap();
    assert_eq!(
        store.find_member(&header, member).err(),
        Some(Bypass::ScratchExhausted)
    );
    assert_eq!(
        store.find_importers(&header, member, None, 10).err(),
        Some(Bypass::ScratchExhausted)
    );
    assert_eq!(
        store.find_by_index(POSTING_ROW, b"m", None, 10).err(),
        Some(Bypass::ScratchExhausted)
    );
    drop(hold);
    assert!(matches!(
        store.find_member(&header, member),
        Ok(Lookup::Hit(_))
    ));
    assert!(matches!(
        store.find_importers(&header, member, None, 10),
        Ok(Lookup::Hit(_))
    ));
    assert!(store.find_by_index(POSTING_ROW, b"m", None, 10).is_ok());
    assert_eq!(store.scratch_in_use(), 0, "nothing stays reserved");
}

/// Spec §10: the sealed-snapshot candidates are charged to the scratch
/// before their vector is allocated.
#[test]
fn snapshot_lookups_reserve_their_headers_before_allocating_them() {
    let directory = private_tempdir();
    let store = open(directory.path());
    sealed(&store, &records(&store, "M", 2));
    let free = store.limits().scratch_bytes - store.scratch_in_use();
    let hold = store.hold_scratch_for_test(free).unwrap();
    assert_eq!(
        store.find_snapshots(&context()).err(),
        Some(Bypass::ScratchExhausted)
    );
    drop(hold);
    assert!(matches!(
        store.find_snapshots(&context()),
        Ok(Lookup::Hit(_))
    ));
    assert_eq!(store.scratch_in_use(), 0, "nothing stays reserved");
}

/// A requested path whose key would exceed any key the store writes is
/// absent before its key is built or any scratch is reserved, even with a
/// (tampered) stored row of that key; the stored-key length guard stays as
/// defence in depth behind it.
#[test]
fn an_over_long_requested_key_is_absent_before_it_is_encoded() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let header = sealed(&store, &records(&store, "M", 2));
    let long = format!("/{}", "x".repeat(5000));
    let connection = raw(directory.path());
    connection
        .pragma_update(None, "ignore_check_constraints", "ON")
        .unwrap();
    connection
        .execute(
            "UPDATE snapshot_members SET source_key = ?1 WHERE source_key = ?2",
            [path_key(Path::new(&long)), path_key(Path::new("/w/M0.pas"))],
        )
        .unwrap();
    let free = store.limits().scratch_bytes - store.scratch_in_use();
    let hold = store.hold_scratch_for_test(free).unwrap();
    assert_eq!(
        store.find_member(&header, Path::new(&long)),
        Ok(Lookup::Miss(Miss::Absent))
    );
    assert_eq!(
        store.find_importers(&header, Path::new(&long), None, 10),
        Ok(Lookup::Miss(Miss::Absent))
    );
    drop(hold);
    assert_eq!(
        store.find_member(&header, Path::new(&long)),
        Ok(Lookup::Miss(Miss::Absent))
    );
    assert_eq!(
        store.find_importers(&header, Path::new(&long), None, 10),
        Ok(Lookup::Miss(Miss::Absent))
    );
}

/// A stored record ID of the wrong length is malformed, refused from its
/// length before the blob is read, by both queries.
#[test]
fn an_over_long_stored_record_id_is_a_malformed_snapshot() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "lib", "/w/Lib.pas");
    let importer = store
        .publish_member_record(&member("/w/Main.pas", &["/w/Lib.pas"], evidence))
        .unwrap()
        .record_id;
    let header = sealed(&store, &[importer]);
    let connection = raw(directory.path());
    connection
        .pragma_update(None, "ignore_check_constraints", "ON")
        .unwrap();
    let long = vec![7u8; 5000];
    for table in [
        "member_records",
        "member_bindings",
        "binding_targets",
        "snapshot_members",
    ] {
        connection
            .execute(
                &format!("UPDATE {table} SET record_id = ?1 WHERE record_id = ?2"),
                rusqlite::params![long, importer.as_bytes().as_slice()],
            )
            .unwrap();
    }
    assert_eq!(
        store
            .find_member(&header, Path::new("/w/Main.pas"))
            .unwrap(),
        Lookup::Miss(Miss::Malformed(Malformed::Snapshot))
    );
    assert_eq!(
        store
            .find_importers(&header, Path::new("/w/Lib.pas"), None, 10)
            .unwrap(),
        Lookup::Miss(Miss::Malformed(Malformed::Snapshot))
    );
    assert_eq!(store.scratch_in_use(), 0);
}

#[test]
fn ambiguous_importers_are_returned_for_every_candidate_flagged() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "t", "/w/x/T0.pas");
    let x = records(&store, "x/T", 1)[0];
    let precise = store
        .publish_member_record(&member("/w/P.pas", &["/w/x/T0.pas"], evidence))
        .unwrap()
        .record_id;
    let both = store
        .publish_member_record(&ambiguous(
            "/w/A.pas",
            &["/w/x/T0.pas", "/w/y/T0.pas"],
            false,
            evidence,
        ))
        .unwrap()
        .record_id;
    let header = sealed(&store, &[x, precise, both]);
    let hits = all_importers(&store, &header, "/w/x/T0.pas", 1);
    let kinds: Vec<_> = hits
        .iter()
        .map(|hit| (hit.record_id, hit.dependency))
        .collect();
    assert!(kinds.contains(&(precise, Dependency::Precise)), "{kinds:?}");
    assert!(kinds.contains(&(both, Dependency::Ambiguous)), "{kinds:?}");
    let other = all_importers(&store, &header, "/w/y/T0.pas", 10);
    assert_eq!(
        other
            .iter()
            .map(|hit| (hit.record_id, hit.dependency))
            .collect::<Vec<_>>(),
        [(both, Dependency::Ambiguous)],
        "the other candidate finds it too, without reading payloads"
    );
}

fn unknown_importer(
    store: &Store,
    path: &str,
    evidence: pascal_cache::Digest,
) -> pascal_cache::Digest {
    let mut record = member(path, &[], evidence);
    record.bindings.push(Binding {
        kind: BindingKind::Import,
        occurrence: 0,
        outcome: BindingOutcome::Incomplete(IncompleteReason::UnknownConditional),
        evidence: None,
    });
    store.publish_member_record(&record).unwrap().record_id
}

#[test]
fn unbounded_importers_follow_every_target_across_pages() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "t", "/w/T0.pas");
    let target = records(&store, "T", 1)[0];
    let mut members = vec![target];
    for index in 0..5 {
        members.push(
            store
                .publish_member_record(&member(
                    &format!("/w/P{index}.pas"),
                    &["/w/T0.pas"],
                    evidence,
                ))
                .unwrap()
                .record_id,
        );
    }
    let mut unknown = Vec::new();
    for index in 0..4 {
        let id = unknown_importer(&store, &format!("/w/N{index}.pas"), evidence);
        unknown.push(id);
        members.push(id);
    }
    let header = sealed(&store, &members);
    for limit in [1, 2, 3, 100] {
        let hits = all_importers(&store, &header, "/w/T0.pas", limit);
        assert_eq!(
            hits.len(),
            9,
            "limit {limit}: no gap or repeat at the phase boundary"
        );
        assert!(
            hits[..5]
                .iter()
                .all(|hit| hit.dependency == Dependency::Precise)
        );
        assert_eq!(
            hits[5..]
                .iter()
                .map(|hit| hit.record_id)
                .collect::<BTreeSet<_>>(),
            unknown.iter().copied().collect::<BTreeSet<_>>()
        );
        assert!(
            hits[5..]
                .iter()
                .all(|hit| hit.dependency == Dependency::Unbounded)
        );
    }
    assert_eq!(
        all_importers(&store, &header, "/w/Unrelated.pas", 10).len(),
        4,
        "unbounded importers are dependants of any target"
    );
}

#[test]
fn a_record_with_a_precise_and_an_unbounded_binding_is_a_hit_in_each_phase() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "t", "/w/T0.pas");
    let target = records(&store, "T", 1)[0];
    let mut record = member("/w/Both.pas", &["/w/T0.pas"], evidence);
    record.bindings.push(Binding {
        kind: BindingKind::Import,
        occurrence: 1,
        outcome: BindingOutcome::Incomplete(IncompleteReason::Unsupported),
        evidence: None,
    });
    let both = store.publish_member_record(&record).unwrap().record_id;
    let header = sealed(&store, &[target, both]);
    for limit in [1, 10] {
        let hits = all_importers(&store, &header, "/w/T0.pas", limit);
        assert_eq!(
            hits.iter()
                .map(|hit| (hit.record_id, hit.binding_ordinal, hit.dependency))
                .collect::<Vec<_>>(),
            [
                (both, 0, Dependency::Precise),
                (both, 1, Dependency::Unbounded)
            ],
            "limit {limit}"
        );
    }
}

#[test]
fn a_snapshot_retired_at_the_phase_boundary_is_a_miss() {
    let directory = private_tempdir();
    let store = open(directory.path());
    let evidence = binding_evidence(&store, "t", "/w/T0.pas");
    let target = records(&store, "T", 1)[0];
    let precise = store
        .publish_member_record(&member("/w/P.pas", &["/w/T0.pas"], evidence))
        .unwrap()
        .record_id;
    let u1 = unknown_importer(&store, "/w/N1.pas", evidence);
    let u2 = unknown_importer(&store, "/w/N2.pas", evidence);
    let header = sealed(&store, &[target, precise, u1, u2]);
    let Lookup::Hit(first) = store
        .find_importers(&header, Path::new("/w/T0.pas"), None, 1)
        .unwrap()
    else {
        panic!("the snapshot is sealed")
    };
    assert_eq!(first.hits.len(), 1);
    assert_eq!(first.hits[0].dependency, Dependency::Precise);
    assert!(first.next.is_some());
    assert!(store.retire_snapshot(header.id()).unwrap());
    assert_eq!(
        store
            .find_importers(&header, Path::new("/w/T0.pas"), first.next, 1)
            .unwrap(),
        Lookup::Miss(Miss::Retired)
    );
}

/// A member at `path` over a local artifact (unique `text`) with one row
/// of `row_kind` per key.
fn member_with_rows(
    store: &Store,
    path: &str,
    text: &str,
    row_kind: u8,
    keys: &[&[u8]],
) -> pascal_cache::Digest {
    let key = pascal_cache::LocalArtifactKey::new(
        &pascal_cache::SourceContent::from_bytes(text.as_bytes()),
        &pascal_project::ConditionalContext::default(),
        pascal_project::SourceOrigin::ProjectCompiled,
    );
    let rows: Vec<pascal_cache::ArtifactRow<'_>> = keys
        .iter()
        .map(|index_key| pascal_cache::ArtifactRow {
            row_kind,
            index_key: Some(index_key),
            payload: b"p",
        })
        .collect();
    store
        .publish_artifact(&pascal_cache::ArtifactWrite {
            key: pascal_cache::ArtifactKeyRef::local(&key),
            supplemental: b"",
            rows: &rows,
        })
        .unwrap();
    let mut record = member(path, &[], pascal_cache::Digest::from_bytes([0; 32]));
    record.header.artifact = Some(key.digest());
    store.publish_member_record(&record).unwrap().record_id
}

fn sources(paths: &[&str]) -> BTreeSet<Vec<u8>> {
    paths.iter().map(|path| path_key(Path::new(path))).collect()
}

/// l4d-c8l2: the members of one sealed snapshot whose artifact holds a row
/// of a kind whose key contains a needle (non-ASCII keys always pass) or
/// equals one of a list. A selection hint only, before re-validation.
#[test]
fn members_with_matching_rows_come_from_one_snapshot() {
    use pascal_cache::{DECLARATION_ROW, IndexKeyMatch};
    let directory = private_tempdir();
    let store = open(directory.path());
    let a = member_with_rows(
        &store,
        "/w/A.pas",
        "a",
        DECLARATION_ROW,
        &[b"tdatabaseinfo", b"x"],
    );
    let b = member_with_rows(
        &store,
        "/w/B.pas",
        "b",
        DECLARATION_ROW,
        &[b"getdatabaseinfo"],
    );
    let c = member_with_rows(&store, "/w/C.pas", "c", DECLARATION_ROW, &[b"other"]);
    let d = member_with_rows(
        &store,
        "/w/D.pas",
        "d",
        DECLARATION_ROW,
        &["café".as_bytes()],
    );
    let e = member_with_rows(&store, "/w/E.pas", "e", POSTING_ROW, &[b"tdatabaseinfo"]);
    let unindexed = records(&store, "U", 1)[0];
    member_with_rows(
        &store,
        "/w/Outside.pas",
        "f",
        DECLARATION_ROW,
        &[b"tdatabaseinfo"],
    );
    let header = sealed(&store, &[a, b, c, d, e, unindexed]);
    let present: [&[u8]; 2] = [b"tdatabaseinfo", b"zz"];
    let absent: [&[u8]; 1] = [b"databaseinfo"];
    let found = |row_kind, matching, limit| match store
        .find_members_with_rows(&header, row_kind, matching, limit)
        .unwrap()
    {
        Lookup::Hit(found) => found.map(|found| found.into_iter().collect::<BTreeSet<_>>()),
        Lookup::Miss(miss) => panic!("{miss:?}"),
    };
    assert_eq!(
        found(
            DECLARATION_ROW,
            IndexKeyMatch::Containing(b"databaseinfo"),
            10
        ),
        Some(sources(&["/w/A.pas", "/w/B.pas", "/w/D.pas"]))
    );
    assert_eq!(
        found(
            DECLARATION_ROW,
            IndexKeyMatch::Containing(b"tdatabaseinfo"),
            10
        ),
        Some(sources(&["/w/A.pas", "/w/B.pas", "/w/D.pas"]))
    );
    assert_eq!(
        found(POSTING_ROW, IndexKeyMatch::Equal(&present), 10),
        Some(sources(&["/w/E.pas"]))
    );
    assert_eq!(
        found(POSTING_ROW, IndexKeyMatch::Equal(&absent), 10),
        Some(BTreeSet::new())
    );
    assert_eq!(
        found(
            DECLARATION_ROW,
            IndexKeyMatch::Containing(b"databaseinfo"),
            2
        ),
        None,
        "more members than the limit"
    );
    assert!(store.retire_member_record(b).unwrap());
    assert_eq!(
        found(
            DECLARATION_ROW,
            IndexKeyMatch::Containing(b"databaseinfo"),
            10
        ),
        Some(sources(&["/w/A.pas", "/w/D.pas"])),
        "a retired record is not a member"
    );
    assert!(store.retire_snapshot(header.id()).unwrap());
    assert_eq!(
        store
            .find_members_with_rows(
                &header,
                DECLARATION_ROW,
                IndexKeyMatch::Containing(b"x"),
                10
            )
            .unwrap(),
        Lookup::Miss(Miss::Retired)
    );
    assert_eq!(store.scratch_in_use(), 0, "nothing stays reserved");
}
