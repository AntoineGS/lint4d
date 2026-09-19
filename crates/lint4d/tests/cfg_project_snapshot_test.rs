use std::path::PathBuf;
use std::sync::Arc;

use lint4d::cfg::project_snapshot::{
    CfgSnapshotOptions, CfgSnapshotStatus, to_cfg_project_snapshot,
};
use pascal_core::resolver::{
    ImportSection, ImportSite, LoadedSource, ResolutionReport, ResolutionTarget, ResolvedImport,
    ResolvedInclude, ResolvedProject, ResolvedUnit, SourceId, SourceRevision,
};
use pascal_project::{CompilerVersion, ConditionalContext};

fn source(id: &str, path: &str, bytes: &[u8]) -> LoadedSource {
    LoadedSource {
        id: SourceId::new(id),
        path: PathBuf::from(path),
        bytes: Arc::from(bytes.to_vec()),
        decoded_text: None,
        revision: SourceRevision::Overlay {
            version: 1,
            content_hash: 1,
        },
    }
}

fn report(complete: bool) -> ResolutionReport {
    ResolutionReport {
        observations: Vec::new(),
        warnings: Vec::new(),
        complete,
        incomplete_reasons: (!complete)
            .then(|| vec!["fixture incomplete".to_string()])
            .unwrap_or_default(),
    }
}

fn project_with_import(target: ResolutionTarget<SourceId>) -> ResolvedProject {
    let root_bytes = b"unit App; interface uses Errors; implementation end.";
    let dependency_bytes = b"unit Errors; interface implementation end.";
    let root = source(
        "source:/workspace/App.pas",
        "/workspace/App.pas",
        root_bytes,
    );
    let dependency = source(
        "source:/workspace/Errors.pas",
        "/workspace/Errors.pas",
        dependency_bytes,
    );
    let start = root_bytes
        .windows(b"Errors".len())
        .position(|window| window == b"Errors")
        .expect("fixture uses site");
    ResolvedProject {
        root: ResolvedUnit {
            requested_name: "App".to_string(),
            declared_name: "App".to_string(),
            source: root,
        },
        units: vec![ResolvedUnit {
            requested_name: "Errors".to_string(),
            declared_name: "Errors".to_string(),
            source: dependency,
        }],
        imports: vec![ResolvedImport {
            importer_source_id: SourceId::new("source:/workspace/App.pas"),
            site: ImportSite {
                byte_range: start..start + b"Errors".len(),
                requested_name: "Errors".to_string(),
                section: ImportSection::Interface,
            },
            target,
            authorized_qualifiers: vec!["Vendor.Errors".to_string(), "Errors".to_string()],
        }],
        includes: Vec::new(),
        include_sources: Vec::new(),
        complete: true,
        report: report(true),
    }
}

fn raw_snapshot_options() -> CfgSnapshotOptions {
    CfgSnapshotOptions {
        prepare_configured_sources: false,
        configuration_id: None,
        conditional_context: ConditionalContext::default(),
        preparation_environment: cfg_pascal::PreparationEnvironment::Complete,
        initial_defined_symbols: Vec::new(),
        initial_undefined_symbols: Vec::new(),
        preparation_limits: cfg_pascal::PreparationLimits::default(),
    }
}

#[test]
fn adapter_uses_stable_source_ids_for_unit_ids() {
    let snapshot = to_cfg_project_snapshot(
        project_with_import(ResolutionTarget::Found(SourceId::new(
            "source:/workspace/Errors.pas",
        ))),
        raw_snapshot_options(),
    )
    .expect("snapshot conversion");
    let ids = snapshot
        .snapshot
        .units()
        .iter()
        .map(|unit| unit.id().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec!["source:/workspace/App.pas", "source:/workspace/Errors.pas"]
    );
    assert_eq!(snapshot.target_unit.as_str(), "source:/workspace/App.pas");
}

#[test]
fn adapter_forwards_ambiguous_import_without_loading_a_target() {
    let snapshot = to_cfg_project_snapshot(
        project_with_import(ResolutionTarget::Ambiguous),
        raw_snapshot_options(),
    )
    .expect("snapshot conversion");
    assert!(matches!(
        snapshot.snapshot.imports()[0].target(),
        cfg_pascal::ImportTarget::Ambiguous
    ));
    assert!(matches!(snapshot.status, CfgSnapshotStatus::Complete));
}

#[test]
fn incomplete_import_target_marks_the_snapshot_incomplete_even_with_a_complete_report() {
    let snapshot = to_cfg_project_snapshot(
        project_with_import(ResolutionTarget::Incomplete),
        raw_snapshot_options(),
    )
    .expect("snapshot conversion");
    assert!(matches!(
        snapshot.status,
        CfgSnapshotStatus::Incomplete { .. }
    ));
}

#[test]
fn adapter_forwards_exact_authorized_qualifiers() {
    let snapshot = to_cfg_project_snapshot(
        project_with_import(ResolutionTarget::Found(SourceId::new(
            "source:/workspace/Errors.pas",
        ))),
        raw_snapshot_options(),
    )
    .expect("snapshot conversion");
    assert_eq!(
        snapshot.snapshot.imports()[0].authorized_qualifiers(),
        &["Vendor.Errors".to_string(), "Errors".to_string()]
    );
}

#[test]
fn incomplete_resolution_does_not_claim_prepared_source() {
    let mut project = project_with_import(ResolutionTarget::Incomplete);
    project.complete = false;
    project.report = report(false);
    let options = CfgSnapshotOptions {
        prepare_configured_sources: true,
        configuration_id: Some("debug".to_string()),
        ..raw_snapshot_options()
    };
    let snapshot = to_cfg_project_snapshot(project, options).expect("raw fallback");
    assert!(matches!(
        snapshot.status,
        CfgSnapshotStatus::Incomplete { .. }
    ));
    assert!(
        snapshot
            .snapshot
            .units()
            .iter()
            .all(|unit| !unit.is_prepared())
    );
}

#[test]
fn adapter_accepts_original_ranges_for_raw_project_inputs() {
    let snapshot = to_cfg_project_snapshot(
        project_with_import(ResolutionTarget::Unavailable),
        raw_snapshot_options(),
    )
    .expect("snapshot conversion");
    let site = snapshot.snapshot.imports()[0].site();
    assert_eq!(site.byte_range(), 25..31);
    assert_eq!(site.unit_id().as_str(), "source:/workspace/App.pas");
}

#[test]
fn adapter_prepares_configured_includes_and_preserves_their_origin() {
    let root_bytes = b"program App;\nbegin\n  {$I body.inc}\nend.\n";
    let include_bytes = b"  Writeln('included');\n";
    let root_id = SourceId::new("source:/workspace/App.pas");
    let include_id = SourceId::new("source:/workspace/body.inc");
    let directive = b"{$I body.inc}";
    let directive_start = root_bytes
        .windows(directive.len())
        .position(|window| window == directive)
        .expect("include directive");
    let project = ResolvedProject {
        root: ResolvedUnit {
            requested_name: "App".to_string(),
            declared_name: "App".to_string(),
            source: source(root_id.as_str(), "/workspace/App.pas", root_bytes),
        },
        units: Vec::new(),
        imports: Vec::new(),
        includes: vec![ResolvedInclude {
            including_source_id: root_id.clone(),
            byte_range: directive_start..directive_start + directive.len(),
            requested_name: "body.inc".to_string(),
            target: ResolutionTarget::Found(include_id.clone()),
        }],
        include_sources: vec![source(
            include_id.as_str(),
            "/workspace/body.inc",
            include_bytes,
        )],
        complete: true,
        report: report(true),
    };
    let options = CfgSnapshotOptions {
        prepare_configured_sources: true,
        configuration_id: Some("debug".to_string()),
        ..raw_snapshot_options()
    };

    let snapshot = to_cfg_project_snapshot(project, options).expect("prepared snapshot");
    assert!(matches!(snapshot.status, CfgSnapshotStatus::Complete));
    let target = snapshot.snapshot.units().first().expect("target unit");
    assert!(target.is_prepared());
    let included_start = target
        .source()
        .windows(b"Writeln('included');".len())
        .position(|window| window == b"Writeln('included');")
        .expect("expanded include");
    let mapped = target
        .source_map()
        .expect("prepared source map")
        .map_range(included_start..included_start + b"Writeln('included');".len())
        .expect("include origin mapping");
    assert_eq!(mapped.len(), 1);
    assert_eq!(
        mapped[0]
            .original()
            .expect("include origin")
            .source_id()
            .as_str(),
        include_id.as_str()
    );
}

#[test]
fn adapter_routes_compiler_context_into_configured_cfg_preparation() {
    let root_bytes = b"unit App; interface {$IF CompilerVersion >= 24} const Enabled = 1; {$ENDIF} implementation end.";
    let root_id = SourceId::new("source:/workspace/App.pas");
    let project = ResolvedProject {
        root: ResolvedUnit {
            requested_name: "App".to_string(),
            declared_name: "App".to_string(),
            source: source(root_id.as_str(), "/workspace/App.pas", root_bytes),
        },
        units: Vec::new(),
        imports: Vec::new(),
        includes: Vec::new(),
        include_sources: Vec::new(),
        complete: true,
        report: report(true),
    };
    let options = CfgSnapshotOptions {
        prepare_configured_sources: true,
        configuration_id: Some("debug".to_string()),
        conditional_context: ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(24, 0)),
        ..raw_snapshot_options()
    };

    let snapshot = to_cfg_project_snapshot(project, options).expect("prepared snapshot");
    assert!(matches!(snapshot.status, CfgSnapshotStatus::Complete));
    let target = snapshot.snapshot.units().first().expect("target unit");
    assert!(target.is_prepared());
    assert!(
        target
            .source()
            .windows(b"Enabled".len())
            .any(|window| { window == b"Enabled" })
    );
}

#[test]
fn adapter_projects_compiler_context_in_cross_unit_cfg_inputs() {
    let mut project = project_with_import(ResolutionTarget::Found(SourceId::new(
        "source:/workspace/Errors.pas",
    )));
    let dependency_source = b"unit Errors; interface {$IF CompilerVersion >= 24} procedure Enabled; {$ENDIF} implementation {$IF CompilerVersion >= 24} procedure Enabled; begin end; {$ENDIF} end.";
    project.units[0].source = source(
        "source:/workspace/Errors.pas",
        "/workspace/Errors.pas",
        dependency_source,
    );

    let options = CfgSnapshotOptions {
        prepare_configured_sources: true,
        configuration_id: Some("debug".to_string()),
        conditional_context: ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(24, 0)),
        ..raw_snapshot_options()
    };
    let snapshot = to_cfg_project_snapshot(project, options).expect("prepared project snapshot");
    assert!(matches!(snapshot.status, CfgSnapshotStatus::Complete));
    let dependency = snapshot
        .snapshot
        .units()
        .iter()
        .find(|unit| unit.id().as_str().ends_with("Errors.pas"))
        .expect("prepared dependency unit");
    assert!(dependency.is_prepared());
    assert!(
        dependency
            .source()
            .windows(b"procedure Enabled".len())
            .any(|window| window == b"procedure Enabled")
    );
    let cfgs = cfg_pascal::build_file_cfgs_in_project(&snapshot.snapshot, dependency.id())
        .expect("context-projected dependency CFG");
    assert!(
        !cfgs.is_empty(),
        "enabled cross-unit procedure should have a CFG"
    );
}

#[test]
fn adapter_projects_root_define_into_the_include_cfg_context() {
    let root_bytes = b"unit App; interface {$DEFINE ENABLED}{$I condition.inc} implementation end.";
    let include_bytes =
        b"{$IF Defined(ENABLED)}const CorrectBranch = 1;{$ELSE}const WrongBranch = 1;{$ENDIF}";
    let root_id = SourceId::new("source:/workspace/App.pas");
    let include_id = SourceId::new("source:/workspace/condition.inc");
    let directive = b"{$I condition.inc}";
    let directive_start = root_bytes
        .windows(directive.len())
        .position(|window| window == directive)
        .expect("include directive");
    let project = ResolvedProject {
        root: ResolvedUnit {
            requested_name: "App".to_string(),
            declared_name: "App".to_string(),
            source: source(root_id.as_str(), "/workspace/App.pas", root_bytes),
        },
        units: Vec::new(),
        imports: Vec::new(),
        includes: vec![ResolvedInclude {
            including_source_id: root_id.clone(),
            byte_range: directive_start..directive_start + directive.len(),
            requested_name: "condition.inc".to_string(),
            target: ResolutionTarget::Found(include_id.clone()),
        }],
        include_sources: vec![source(
            include_id.as_str(),
            "/workspace/condition.inc",
            include_bytes,
        )],
        complete: true,
        report: report(true),
    };
    let mut conditional_context = ConditionalContext::default();
    conditional_context.set_define("ENABLED", pascal_project::ConditionalFact::False);
    let options = CfgSnapshotOptions {
        prepare_configured_sources: true,
        configuration_id: Some("debug".to_string()),
        conditional_context,
        ..raw_snapshot_options()
    };

    let snapshot = to_cfg_project_snapshot(project, options).expect("prepared snapshot");
    assert!(matches!(snapshot.status, CfgSnapshotStatus::Complete));
    let target = snapshot.snapshot.units().first().expect("prepared root");
    assert!(
        target
            .source()
            .windows(b"CorrectBranch".len())
            .any(|window| window == b"CorrectBranch"),
        "root DEFINE must make the active include branch available to CFG"
    );
    assert!(
        !target
            .source()
            .windows(b"WrongBranch".len())
            .any(|window| window == b"WrongBranch"),
        "inactive include branch must not survive prepared CFG source"
    );
}

#[test]
fn adapter_projects_root_option_transition_into_the_include_cfg_context() {
    let root_bytes = b"unit App; interface {$R-}{$I condition.inc} implementation end.";
    let include_bytes = b"{$IFOPT R+}const WrongBranch = 1;{$ELSE}const CorrectBranch = 1;{$ENDIF}";
    let root_id = SourceId::new("source:/workspace/App.pas");
    let include_id = SourceId::new("source:/workspace/condition.inc");
    let directive = b"{$I condition.inc}";
    let directive_start = root_bytes
        .windows(directive.len())
        .position(|window| window == directive)
        .expect("include directive");
    let project = ResolvedProject {
        root: ResolvedUnit {
            requested_name: "App".to_string(),
            declared_name: "App".to_string(),
            source: source(root_id.as_str(), "/workspace/App.pas", root_bytes),
        },
        units: Vec::new(),
        imports: Vec::new(),
        includes: vec![ResolvedInclude {
            including_source_id: root_id.clone(),
            byte_range: directive_start..directive_start + directive.len(),
            requested_name: "condition.inc".to_string(),
            target: ResolutionTarget::Found(include_id.clone()),
        }],
        include_sources: vec![source(
            include_id.as_str(),
            "/workspace/condition.inc",
            include_bytes,
        )],
        complete: true,
        report: report(true),
    };
    let options = CfgSnapshotOptions {
        prepare_configured_sources: true,
        configuration_id: Some("debug".to_string()),
        conditional_context: ConditionalContext::default()
            .with_option("R", pascal_project::ConditionalFact::True),
        ..raw_snapshot_options()
    };

    let snapshot = to_cfg_project_snapshot(project, options).expect("prepared snapshot");
    assert!(matches!(snapshot.status, CfgSnapshotStatus::Complete));
    let target = snapshot.snapshot.units().first().expect("prepared root");
    assert!(
        target
            .source()
            .windows(b"CorrectBranch".len())
            .any(|window| window == b"CorrectBranch")
    );
    assert!(
        !target
            .source()
            .windows(b"WrongBranch".len())
            .any(|window| window == b"WrongBranch")
    );
}

#[test]
fn adapter_rejects_one_physical_include_reached_with_two_entry_contexts() {
    let root_bytes = concat!(
        "unit App; interface {$DEFINE ENABLED}{$I condition.inc}",
        "{$UNDEF ENABLED}{$I condition.inc} implementation end."
    )
    .as_bytes();
    let include_bytes =
        b"{$IFDEF ENABLED}const EnabledBranch = 1;{$ELSE}const DisabledBranch = 1;{$ENDIF}";
    let root_id = SourceId::new("source:/workspace/App.pas");
    let include_id = SourceId::new("source:/workspace/condition.inc");
    let directive = b"{$I condition.inc}";
    let ranges = root_bytes
        .windows(directive.len())
        .enumerate()
        .filter_map(|(index, window)| {
            (window == directive).then_some(index..index + directive.len())
        })
        .collect::<Vec<_>>();
    assert_eq!(ranges.len(), 2);
    let project = ResolvedProject {
        root: ResolvedUnit {
            requested_name: "App".to_string(),
            declared_name: "App".to_string(),
            source: source(root_id.as_str(), "/workspace/App.pas", root_bytes),
        },
        units: Vec::new(),
        imports: Vec::new(),
        includes: ranges
            .into_iter()
            .map(|byte_range| ResolvedInclude {
                including_source_id: root_id.clone(),
                byte_range,
                requested_name: "condition.inc".to_string(),
                target: ResolutionTarget::Found(include_id.clone()),
            })
            .collect(),
        include_sources: vec![source(
            include_id.as_str(),
            "/workspace/condition.inc",
            include_bytes,
        )],
        complete: true,
        report: report(true),
    };
    let options = CfgSnapshotOptions {
        prepare_configured_sources: true,
        configuration_id: Some("debug".to_string()),
        ..raw_snapshot_options()
    };

    let snapshot = to_cfg_project_snapshot(project, options).expect("raw fallback snapshot");
    assert!(matches!(
        snapshot.status,
        CfgSnapshotStatus::Incomplete { .. }
    ));
}

#[test]
fn adapter_prepares_short_nested_conditionals_and_includes_with_exact_spans() {
    let root_bytes = concat!(
        "unit App; interface ",
        "{$UNDEF X}",
        "{$IFDEF X}const Inactive = 1;{$ELSE}",
        "{$IF 1=1}const NestedActive = 1;{$ENDIF}",
        "{$ENDIF}",
        "{$I body.inc} implementation end."
    )
    .as_bytes();
    let include_bytes =
        b"{$IF 1=0}const IncludedInactive = 1;{$ELSE}const IncludedActive = 1;{$ENDIF}";
    let root_id = SourceId::new("source:/workspace/App.pas");
    let include_id = SourceId::new("source:/workspace/body.inc");
    let include_directive = b"{$I body.inc}";
    let include_start = root_bytes
        .windows(include_directive.len())
        .position(|window| window == include_directive)
        .expect("include directive");
    let project = ResolvedProject {
        root: ResolvedUnit {
            requested_name: "App".to_string(),
            declared_name: "App".to_string(),
            source: source(root_id.as_str(), "/workspace/App.pas", root_bytes),
        },
        units: Vec::new(),
        imports: Vec::new(),
        includes: vec![ResolvedInclude {
            including_source_id: root_id.clone(),
            byte_range: include_start..include_start + include_directive.len(),
            requested_name: "body.inc".to_string(),
            target: ResolutionTarget::Found(include_id.clone()),
        }],
        include_sources: vec![source(
            include_id.as_str(),
            "/workspace/body.inc",
            include_bytes,
        )],
        complete: true,
        report: report(true),
    };
    let options = CfgSnapshotOptions {
        prepare_configured_sources: true,
        configuration_id: Some("debug".to_string()),
        ..raw_snapshot_options()
    };

    let snapshot = to_cfg_project_snapshot(project, options).expect("prepared snapshot");
    assert!(matches!(snapshot.status, CfgSnapshotStatus::Complete));
    let target = snapshot.snapshot.units().first().expect("prepared root");
    assert!(target.is_prepared());
    assert!(
        target
            .source()
            .windows(b"NestedActive".len())
            .any(|window| window == b"NestedActive")
    );
    assert!(
        target
            .source()
            .windows(b"IncludedActive".len())
            .any(|window| window == b"IncludedActive")
    );
    assert!(
        !target
            .source()
            .windows(b"Inactive".len())
            .any(|window| window == b"Inactive")
    );
    assert!(
        !target
            .source()
            .windows(b"IncludedInactive".len())
            .any(|window| window == b"IncludedInactive")
    );

    let nested_start = root_bytes
        .windows(b"NestedActive".len())
        .position(|window| window == b"NestedActive")
        .expect("nested declaration");
    let prepared_nested_start = target
        .source()
        .windows(b"NestedActive".len())
        .position(|window| window == b"NestedActive")
        .expect("prepared nested declaration");
    let nested_mapping = target
        .source_map()
        .expect("prepared source map")
        .map_range(prepared_nested_start..prepared_nested_start + b"NestedActive".len())
        .expect("nested mapping");
    assert_eq!(nested_mapping.len(), 1);
    assert_eq!(
        nested_mapping[0]
            .original()
            .expect("nested origin")
            .source_id()
            .as_str(),
        root_id.as_str()
    );
    assert_eq!(
        nested_mapping[0]
            .original()
            .expect("nested origin")
            .byte_range(),
        nested_start..nested_start + b"NestedActive".len()
    );

    let included_start = include_bytes
        .windows(b"IncludedActive".len())
        .position(|window| window == b"IncludedActive")
        .expect("included declaration");
    let prepared_included_start = target
        .source()
        .windows(b"IncludedActive".len())
        .position(|window| window == b"IncludedActive")
        .expect("prepared included declaration");
    let included_mapping = target
        .source_map()
        .expect("prepared source map")
        .map_range(prepared_included_start..prepared_included_start + b"IncludedActive".len())
        .expect("included mapping");
    assert_eq!(included_mapping.len(), 1);
    assert_eq!(
        included_mapping[0]
            .original()
            .expect("included origin")
            .source_id()
            .as_str(),
        include_id.as_str()
    );
    assert_eq!(
        included_mapping[0]
            .original()
            .expect("included origin")
            .byte_range(),
        included_start..included_start + b"IncludedActive".len()
    );
}
