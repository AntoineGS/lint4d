use lsp_types::{Position, Url};
use pascal_lsp::{
    NavigationIndex,
    navigation::compiled_dcu::{
        CompiledUnitDocument, discover_compiled_units, virtual_unit_identity,
    },
};
use pascal_project::delphi_overrides::EffectiveOverrides;
use pascal_project::{ProjectContext, ProjectPathEntry, ReadPolicy};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

fn fixture() -> Vec<u8> {
    fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../lint4d/tests/fixtures/dcu/d13_win64/Win64/Debug/Lint4dFixture.Classes.dcu"),
    )
    .expect("checked-in Delphi 13 Win64 fixture")
}

fn selected_context(search_paths: &[PathBuf]) -> ProjectContext {
    let search_paths = search_paths
        .iter()
        .map(|path| path.canonicalize().expect("test search path exists"))
        .collect::<Vec<_>>();
    let search_path_entries = search_paths
        .iter()
        .cloned()
        .map(ProjectPathEntry::legacy)
        .collect();
    let read_policy = ReadPolicy::new(&search_paths, &[], &[], &EffectiveOverrides::default());
    ProjectContext {
        discovery_complete: true,
        search_paths,
        search_path_entries,
        read_policy,
        ..ProjectContext::default()
    }
}

#[test]
fn d13_win64_compiled_unit_is_read_only_navigation_completion_and_hover_provider() {
    let direct = CompiledUnitDocument::parse(&fixture()).expect("D13 Win64 fixture parse");
    assert!(direct.text().contains("TSimpleClass = class"));
    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../lint4d/tests/fixtures/dcu/d13_win64/Win64/Debug");
    let selected = selected_context(std::slice::from_ref(&fixture_root));
    assert!(
        selected
            .read_policy
            .allows_location(&selected.search_path_entries[0])
    );
    let fixture_path = selected.search_paths[0].join("Lint4dFixture.Classes.dcu");
    let fixture_entry = selected
        .read_policy
        .entry_for_path(&fixture_path)
        .expect("fixture is covered by policy");
    assert!(selected.read_policy.allows_location(&fixture_entry));
    let loaded = discover_compiled_units(
        &selected,
        &["Lint4dFixture.Classes".to_string()],
        &AtomicBool::new(false),
    )
    .expect("selected-context DCU provider");
    assert_eq!(loaded.len(), 1);
    let generated = &loaded[0].document;
    assert_eq!(generated.version_name(), "D13");
    assert_eq!(generated.platform_name(), "Win64");
    assert_eq!(
        virtual_unit_identity(generated.uri()).map(|(_, _, name)| name),
        Some("lint4dfixture.classes".to_string())
    );
    assert!(generated.text().contains("TSimpleClass = class"));
    assert!(!generated.text().contains("Create"));

    let mut index = NavigationIndex::new();
    index
        .update(generated.uri().clone(), generated.text().to_owned())
        .expect("parse generated unit declaration");
    let importer = Url::parse("file:///workspace/Consumer.pas").unwrap();
    let source = "unit Consumer; interface uses Lint4dFixture.Classes; type TAlias = TSimpleClass; implementation end.";
    index.update(importer.clone(), source.to_owned()).unwrap();

    let unit_position = Position::new(0, 31);
    let locations = index.navigate(
        &importer,
        unit_position,
        pascal_lsp::NavigationTarget::Definition,
    );
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, *generated.uri());

    let type_position = Position::new(0, source.find("TSimpleClass").unwrap() as u32 + 1);
    let locations = index.navigate(
        &importer,
        type_position,
        pascal_lsp::NavigationTarget::Definition,
    );
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, *generated.uri());

    let completion_source = "unit Consumer; interface uses Lint4dFixture.Classes; type TAlias = TSimple; implementation end.";
    index
        .update(importer.clone(), completion_source.to_owned())
        .unwrap();
    let completion_pos = Position::new(0, completion_source.find("TSimple").unwrap() as u32 + 7);
    let completion = index.completion(&importer, completion_pos).unwrap();
    assert!(
        completion
            .items
            .iter()
            .any(|item| item.label == "TSimpleClass")
    );

    let hover_source = "unit Consumer; interface uses Lint4dFixture.Classes; type TAlias = TSimpleClass; implementation end.";
    index
        .update(importer.clone(), hover_source.to_owned())
        .unwrap();
    let hover_pos = Position::new(0, hover_source.find("TSimpleClass").unwrap() as u32 + 2);
    assert!(index.hover(&importer, hover_pos).is_some());

    assert!(generated.text().len() <= CompiledUnitDocument::MAX_TEXT_BYTES);
    assert!(!generated.editable());
}

#[test]
fn virtual_unit_uri_parser_rejects_forged_or_noncanonical_uris() {
    for forged in [
        "lint4d-dcu://d13-win64/../../etc/passwd.pas",
        "lint4d-dcu://d13-win64/0000000000000000/0000000000000000/../../outside.pas",
        "lint4d-dcu://d13-win64/0000000000000000/0000000000000000/unit.pas?path=file:///etc/passwd",
        "lint4d-dcu://other/0000000000000000/0000000000000000/unit.pas",
        "lint4d-dcu://d13-win64/0000000000000000/0000000000000000/%2e%2e.pas",
    ] {
        let uri = Url::parse(forged).expect("well-formed URL used as a forged request");
        assert!(virtual_unit_identity(&uri).is_none(), "accepted {forged}");
    }
}

#[test]
fn compiled_unit_provider_rejects_unsupported_and_malformed_files() {
    let mut unsupported = fixture();
    unsupported[..4].copy_from_slice(&0u32.to_le_bytes());
    assert!(CompiledUnitDocument::parse(&unsupported).is_err());
    assert!(CompiledUnitDocument::parse(&fixture()[..24]).is_err());
}

#[test]
fn selected_context_loads_only_unique_authorized_dcu_without_source_shadowing() {
    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../lint4d/tests/fixtures/dcu/d13_win64/Win64/Debug");
    let selected = selected_context(std::slice::from_ref(&fixture_root));
    let units = discover_compiled_units(
        &selected,
        &["Lint4dFixture.Classes".to_string()],
        &AtomicBool::new(false),
    )
    .expect("read the selected project's supported DCU");
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].document.version_name(), "D13");
    assert!(units[0].is_current(&selected.read_policy));
}

#[test]
fn selected_context_discovery_honors_cancellation_and_import_caps() {
    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../lint4d/tests/fixtures/dcu/d13_win64/Win64/Debug");
    let selected = selected_context(std::slice::from_ref(&fixture_root));
    let cancelled = AtomicBool::new(true);
    assert_eq!(
        discover_compiled_units(
            &selected,
            &["Lint4dFixture.Classes".to_string()],
            &cancelled,
        )
        .expect_err("pre-cancelled discovery must stop"),
        "request cancelled"
    );

    let over_cap = (0..129)
        .map(|index| format!("Unit{index}"))
        .collect::<Vec<_>>();
    assert!(
        discover_compiled_units(&selected, &over_cap, &AtomicBool::new(false))
            .expect("cap refusal is not a partial error")
            .is_empty(),
        "over-cap import lists must not produce partial providers"
    );
}

#[test]
fn compiled_unit_content_observation_detects_same_size_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("Lint4dFixture.Classes.dcu");
    fs::write(&path, fixture()).unwrap();
    let selected = selected_context(std::slice::from_ref(&temp.path().to_path_buf()));
    let loaded = discover_compiled_units(
        &selected,
        &["Lint4dFixture.Classes".to_string()],
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].is_current(&selected.read_policy));

    let mut replacement = fixture();
    let last_byte = replacement.len() - 1;
    replacement[last_byte] ^= 1;
    assert_eq!(
        replacement.len(),
        fs::metadata(&path).unwrap().len() as usize
    );
    fs::write(&path, replacement).unwrap();
    assert!(!loaded[0].is_current(&selected.read_policy));
    let refreshed = discover_compiled_units(
        &selected,
        &["Lint4dFixture.Classes".to_string()],
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(refreshed.len(), 1);
    assert_ne!(loaded[0].document.uri(), refreshed[0].document.uri());
}

#[test]
fn selected_context_refuses_source_shadow_and_duplicate_dcu_unit_candidates() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    let name = "Lint4dFixture.Classes";
    fs::write(first.join(format!("{name}.dcu")), fixture()).unwrap();
    fs::write(second.join(format!("{name}.dcu")), fixture()).unwrap();
    let duplicate_context = selected_context(&[first.clone(), second.clone()]);
    assert!(
        discover_compiled_units(
            &duplicate_context,
            &[name.to_string()],
            &AtomicBool::new(false),
        )
        .unwrap()
        .is_empty()
    );

    fs::remove_file(second.join(format!("{name}.dcu"))).unwrap();
    fs::write(first.join(format!("{name}.pas")), "unit source; end.").unwrap();
    let source_context = selected_context(std::slice::from_ref(&first));
    assert!(
        discover_compiled_units(
            &source_context,
            &[name.to_string()],
            &AtomicBool::new(false),
        )
        .unwrap()
        .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn selected_context_refuses_symlinked_dcu_and_unapproved_search_paths() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let selected_root = temp.path().join("selected");
    fs::create_dir_all(&selected_root).unwrap();
    let name = "Lint4dFixture.Classes";
    symlink(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../lint4d/tests/fixtures/dcu/d13_win64/Win64/Debug")
            .join(format!("{name}.dcu")),
        selected_root.join(format!("{name}.dcu")),
    )
    .unwrap();
    let selected = selected_context(std::slice::from_ref(&selected_root));
    assert!(
        discover_compiled_units(&selected, &[name.to_string()], &AtomicBool::new(false),)
            .unwrap()
            .is_empty()
    );

    let mut unauthorized = selected;
    unauthorized.read_policy = ReadPolicy::default();
    assert!(
        discover_compiled_units(&unauthorized, &[name.to_string()], &AtomicBool::new(false),)
            .unwrap()
            .is_empty()
    );
}
