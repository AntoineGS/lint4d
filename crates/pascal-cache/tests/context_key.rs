use pascal_cache::{ContextKey, KeyKind, ManifestHeader};
use pascal_project::build_selection::BuildSelectionMode;
use pascal_project::delphi_overrides::EffectiveOverrides;
use pascal_project::{
    CompilerVersion, ConditionalFact, InstallationSelection, ProjectContext, ProjectContextParts,
    ProjectPathEntry, ProjectPathIssue, ProjectPathIssueKind, ProjectPathProvenance, ReadPolicy,
};
use std::path::PathBuf;

type Mutation = fn(&mut ProjectContextParts);

fn key(parts: ProjectContextParts) -> ContextKey {
    ContextKey::new(&ProjectContext::from_parts(parts))
}

fn with(mutate: impl FnOnce(&mut ProjectContextParts)) -> ContextKey {
    let mut parts = ProjectContextParts::default();
    mutate(&mut parts);
    key(parts)
}

#[test]
fn every_context_field_changes_the_key() {
    // Adding a field to ProjectContext breaks this pattern until the field is
    // encoded in ContextKey and given a mutation below.
    let ProjectContextParts {
        discovery_complete: _,
        binding_metadata_complete: _,
        path_issues: _,
        project_file: _,
        installation_evidence: _,
        installation_selection: _,
        search_path_entries: _,
        browsing_path_entries: _,
        debug_dcu_path_entries: _,
        main_source_entry: _,
        explicit_unit_entries: _,
        include_path_entries: _,
        unit_namespaces: _,
        unit_aliases: _,
        conditional_context: _,
        conditional_closure: _,
        library_conditional_context: _,
        project_source_roots: _,
        config: _,
        platform: _,
        config_selection: _,
        platform_selection: _,
        overrides: _,
        read_policy: _,
        packages: _,
        installation_config_files: _,
        override_error: _,
    } = ProjectContextParts::default();
    let mutations: Vec<(&str, Mutation)> = vec![
        ("discovery_complete", |parts| {
            parts.discovery_complete = true
        }),
        ("binding_metadata_complete", |parts| {
            parts.binding_metadata_complete = true
        }),
        ("path_issues", |parts| {
            parts.path_issues.push(ProjectPathIssue {
                kind: ProjectPathIssueKind::MissingDirectory,
                source_file: "/w/App.dproj".into(),
                property: "DCC_UnitSearchPath".into(),
                raw: "lib".into(),
                path: Some("/w/lib".into()),
                unit_name: None,
                provenance: ProjectPathProvenance::Configured,
            })
        }),
        ("project_file", |parts| {
            parts.project_file = Some("/w/App.dproj".into())
        }),
        ("installation_evidence", |parts| {
            parts.installation_evidence.conflicting = true
        }),
        ("installation_selection", |parts| {
            parts.installation_selection = Some(InstallationSelection::Legacy)
        }),
        ("search_path_entries", |parts| {
            parts
                .search_path_entries
                .push(ProjectPathEntry::configured("/w/src".into()))
        }),
        ("browsing_path_entries", |parts| {
            parts
                .browsing_path_entries
                .push(ProjectPathEntry::configured("/ide/source".into()))
        }),
        ("debug_dcu_path_entries", |parts| {
            parts
                .debug_dcu_path_entries
                .push(ProjectPathEntry::configured("/ide/debug".into()))
        }),
        ("main_source_entry", |parts| {
            parts.main_source_entry = Some(ProjectPathEntry::configured("/w/App.dpr".into()))
        }),
        ("explicit_unit_entries", |parts| {
            parts.explicit_unit_entries.insert(
                "unit1".into(),
                vec![ProjectPathEntry::configured("/w/Unit1.pas".into())],
            );
        }),
        ("include_path_entries", |parts| {
            parts
                .include_path_entries
                .push(ProjectPathEntry::configured("/w/inc".into()))
        }),
        ("unit_namespaces", |parts| {
            parts.unit_namespaces.push("System".into())
        }),
        ("unit_aliases", |parts| {
            parts
                .unit_aliases
                .insert("WinTypes".into(), "Winapi.Windows".into());
        }),
        ("conditional_context", |parts| {
            parts
                .conditional_context
                .set_define("DEBUG", ConditionalFact::True)
        }),
        ("conditional_closure", |parts| {
            parts.conditional_closure.closed = true
        }),
        ("library_conditional_context", |parts| {
            parts
                .library_conditional_context
                .set_define("DEBUG", ConditionalFact::True)
        }),
        ("project_source_roots", |parts| {
            parts.project_source_roots.push("/w".into())
        }),
        ("config", |parts| parts.config = Some("Debug".into())),
        ("platform", |parts| parts.platform = Some("Win32".into())),
        ("config_selection", |parts| {
            parts.config_selection.candidates.push("Debug".into())
        }),
        ("platform_selection", |parts| {
            parts.platform_selection.mode = BuildSelectionMode::Session
        }),
        ("overrides", |parts| {
            parts
                .overrides
                .properties
                .insert("BDS".into(), "/opt/bds".into());
        }),
        ("read_policy", |parts| {
            parts.read_policy = ReadPolicy::new(
                &[PathBuf::from("/w")],
                &[],
                &[],
                &EffectiveOverrides::default(),
            )
        }),
        ("packages", |parts| parts.packages.push("rtl".into())),
        ("installation_config_files", |parts| {
            parts
                .installation_config_files
                .push("/cfg/EnvOptions.proj".into())
        }),
        ("override_error", |parts| {
            parts.override_error = Some("bad override".into())
        }),
    ];
    assert_eq!(mutations.len(), 27);
    let base = key(ProjectContextParts::default());
    let mut seen = vec![base.digest()];
    for (name, mutate) in mutations {
        let changed = with(mutate);
        assert_ne!(changed, base, "{name} does not change the context key");
        assert!(
            !seen.contains(&changed.digest()),
            "{name} collides with another field"
        );
        seen.push(changed.digest());
    }
}

#[test]
fn unordered_maps_do_not_depend_on_insertion_order() {
    let names = (0..64)
        .map(|index| format!("Unit{index}"))
        .collect::<Vec<_>>();
    let build = |order: Vec<&String>| {
        with(|parts| {
            for name in order {
                parts.explicit_unit_entries.insert(
                    name.to_ascii_lowercase(),
                    vec![ProjectPathEntry::configured(
                        format!("/w/{name}.pas").into(),
                    )],
                );
                parts
                    .unit_aliases
                    .insert(format!("Alias{name}"), name.clone());
                parts
                    .overrides
                    .properties
                    .insert(format!("P{name}"), name.clone());
            }
        })
    };
    assert_eq!(
        build(names.iter().collect()),
        build(names.iter().rev().collect())
    );
}

#[test]
fn search_path_order_is_part_of_the_key() {
    let first = ProjectPathEntry::configured("/w/a".into());
    let second = ProjectPathEntry::configured("/w/b".into());
    let forward = with(|parts| parts.search_path_entries = vec![first.clone(), second.clone()]);
    let backward = with(|parts| parts.search_path_entries = vec![second.clone(), first.clone()]);
    assert_ne!(forward, backward);
}

#[test]
fn provenance_distinguishes_the_same_path() {
    let legacy =
        with(|parts| parts.search_path_entries = vec![ProjectPathEntry::legacy("/w".into())]);
    let configured =
        with(|parts| parts.search_path_entries = vec![ProjectPathEntry::configured("/w".into())]);
    let mapped = with(|parts| {
        parts.search_path_entries = vec![ProjectPathEntry {
            path: "/w".into(),
            provenance: ProjectPathProvenance::Mapped { root: "/w".into() },
        }]
    });
    assert_ne!(legacy, configured);
    assert_ne!(legacy, mapped);
    assert_ne!(configured, mapped);
}

#[test]
fn conditional_facts_keep_true_false_unknown_and_absence_distinct() {
    let fact =
        |value: ConditionalFact| with(|parts| parts.conditional_context.set_define("X", value));
    let absent = key(ProjectContextParts::default());
    let keys = [
        fact(ConditionalFact::True),
        fact(ConditionalFact::False),
        fact(ConditionalFact::Unknown),
        absent,
    ];
    for (index, left) in keys.iter().enumerate() {
        for right in &keys[index + 1..] {
            assert_ne!(left, right);
        }
    }
    let closed = with(|parts| {
        parts.conditional_context = parts
            .conditional_context
            .clone()
            .with_absent_define(ConditionalFact::False)
    });
    assert_ne!(closed, key(ProjectContextParts::default()));
}

#[test]
fn equal_compiler_versions_encode_equally() {
    let version = |value: CompilerVersion| {
        with(|parts| parts.conditional_context.compiler_version = Some(value))
    };
    let short = CompilerVersion::parse("18.5").unwrap();
    let long = CompilerVersion::parse("18.50").unwrap();
    assert_eq!(short, long);
    assert_eq!(version(short), version(long));
    assert_ne!(
        version(short),
        version(CompilerVersion::with_patch(18, 5, 1))
    );
    assert_ne!(
        version(short),
        version(CompilerVersion::parse("18.6").unwrap())
    );
}

#[test]
fn absent_and_empty_rtl_constant_lists_are_distinct() {
    let absent = with(|parts| parts.conditional_closure.rtl_constants = None);
    let empty = with(|parts| parts.conditional_closure.rtl_constants = Some(Vec::new()));
    assert_ne!(absent, empty);
}

#[test]
fn context_key_manifest_has_a_context_header() {
    let key = key(ProjectContextParts::default());
    assert_eq!(
        ManifestHeader::parse(key.key().manifest()).unwrap().kind,
        KeyKind::Context
    );
}
