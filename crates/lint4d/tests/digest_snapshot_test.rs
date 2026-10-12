use std::fs;
use std::path::{Path, PathBuf};

use lint4d::digest::{Degradation, FileDigest, digest_source, render};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/digest")
}

fn digest(name: &str) -> FileDigest {
    let source = fs::read(fixtures().join(name)).unwrap();
    digest_source(Path::new(name), &source).unwrap()
}

fn all_text(name: &str) -> String {
    let file = digest(name);
    let routines: Vec<String> = file
        .routines
        .iter()
        .map(|r| render::render_routine(r, name, None))
        .collect();
    format!("{}\n{}", render::render_index(&file), routines.join("\n"))
}

#[test]
fn snapshot_lifecycle_text() {
    insta::assert_snapshot!(all_text("lifecycle.pas"));
}

#[test]
fn snapshot_lifecycle_json() {
    let file = digest("lifecycle.pas");
    let routines: Vec<_> = file.routines.iter().collect();
    insta::assert_snapshot!(render::render_json(&file.path, &routines));
}

#[test]
fn snapshot_compaction_text() {
    insta::assert_snapshot!(all_text("compaction.pas"));
}

#[test]
fn snapshot_compaction_json() {
    let file = digest("compaction.pas");
    let routines: Vec<_> = file.routines.iter().collect();
    insta::assert_snapshot!(render::render_json(&file.path, &routines));
}

#[test]
fn snapshot_constructs_text() {
    insta::assert_snapshot!(all_text("constructs.pas"));
}

#[test]
fn degraded_input_is_reported() {
    let text = all_text("degraded.pas");
    assert!(text.contains("! hidden directive block at L9"), "{text}");
    assert!(
        text.contains("L TStringList L8 unknown (hidden directive block at L9)"),
        "{text}"
    );
    let file = digest("degraded.pas");
    let broken = file
        .routines
        .iter()
        .find(|r| r.name == "Broken")
        .expect("Broken routine");
    assert!(
        broken
            .degraded
            .iter()
            .any(|d| matches!(d, Degradation::ParseErrors { .. })),
        "{text}"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn collapse_fits_max_lines() {
    let file = digest("collapse.pas");
    assert_eq!(
        render::render_routine(&file.routines[0], "collapse.pas", Some(8)),
        "Deep  collapse.pas:4-22 (19 lines)
L6 calls Start
L7 if A > 0
  L9 calls One
  L10 if A > 1
    L12 calls Two
    L13-18 if A > 2 … (6 lines, calls Three, Four)
L21 calls Finish
"
    );
}

/// The fact text after `Lnnn `, also behind an inline arm label (`1: L12 …`)
/// and at the end of a folded chain (`L5 if A → L6 …`).
fn fact_text(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let line = line.rsplit_once(" → ").map_or(line, |(_, last)| last);
    let line = match line.find(": L") {
        Some(index) if !line.starts_with('L') => &line[index + 2..],
        _ => line,
    };
    let (number, fact) = line.strip_prefix('L')?.split_once(' ')?;
    number.chars().all(|c| c.is_ascii_digit()).then_some(fact)
}

#[test]
fn index_markers_match_the_full_digest() {
    for entry in fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "pas") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        for routine in &digest(&name).routines {
            let markers = routine.markers();
            let text = render::render_routine(routine, &name, None);
            let facts: Vec<&str> = text.lines().filter_map(fact_text).collect();
            let context = format!("{name} {}:\n{text}", routine.name);
            assert_eq!(
                markers.raises,
                facts
                    .iter()
                    .filter(|f| f.starts_with("raise ") || **f == "re-raise")
                    .count(),
                "{context}"
            );
            assert_eq!(
                markers.exits,
                facts
                    .iter()
                    .filter(|f| **f == "exit" || f.starts_with("exit ("))
                    .count(),
                "{context}"
            );
            assert_eq!(
                markers.tx,
                facts.iter().any(|f| f.starts_with("tx ")),
                "{context}"
            );
            assert_eq!(
                markers.escape,
                text.contains("NOT freed on all paths"),
                "{context}"
            );
            let objects = text
                .lines()
                .find(|l| l.starts_with("objects:"))
                .unwrap_or_default();
            assert_eq!(markers.unknown, objects.contains(" unknown ("), "{context}");
        }
    }
}
