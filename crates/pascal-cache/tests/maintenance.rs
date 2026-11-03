//! In-process maintenance tests (spec §10, amendment 2026-11-03).
#![cfg(unix)]

mod common;

use common::private_tempdir;
use common::records::open;
use pascal_cache::{
    ArtifactKeyRef, ArtifactRow, ArtifactWrite, DATABASE_FILE, LocalArtifactKey, SourceContent,
    Store,
};
use pascal_project::{ConditionalContext, SourceOrigin};
use std::path::Path;

fn key(text: &str) -> LocalArtifactKey {
    LocalArtifactKey::new(
        &SourceContent::from_bytes(text.as_bytes()),
        &ConditionalContext::default(),
        SourceOrigin::ProjectCompiled,
    )
}

/// Publish an artifact of `rows` rows of `row_bytes` bytes each.
fn publish_bulk(
    store: &Store,
    name: &str,
    rows: usize,
    row_bytes: usize,
) -> Result<pascal_cache::Published, pascal_cache::Bypass> {
    let key = key(name);
    let payload = vec![7u8; row_bytes];
    let rows: Vec<ArtifactRow<'_>> = (0..rows)
        .map(|_| ArtifactRow {
            row_kind: 1,
            index_key: None,
            payload: &payload,
        })
        .collect();
    store.publish_artifact(&ArtifactWrite {
        key: ArtifactKeyRef::local(&key),
        supplemental: b"",
        rows: &rows,
    })
}

fn file_len(directory: &Path, suffix: &str) -> u64 {
    std::fs::metadata(directory.join(format!("{DATABASE_FILE}{suffix}"))).map_or(0, |m| m.len())
}

#[test]
fn commits_never_checkpoint_in_the_writer() {
    let root = private_tempdir();
    let store = open(root.path());
    let main_before = file_len(root.path(), "");
    // About 5 MiB: more than SQLite's default 1000-page autocheckpoint.
    publish_bulk(&store, "unit Bulk;", 6, 900_000).unwrap();
    assert!(
        file_len(root.path(), "-wal") > 4 << 20,
        "the commit is in the WAL"
    );
    assert_eq!(
        file_len(root.path(), ""),
        main_before,
        "no checkpoint copied pages into the database on the commit path"
    );
}
