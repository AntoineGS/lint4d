use std::path::PathBuf;
use std::sync::Arc;

use lint4d::cfg::project_snapshot::{
    CfgSnapshotOptions, CfgSnapshotStatus, to_cfg_project_snapshot,
};
use pascal_core::resolver::{
    ImportSection, ImportSite, LoadedSource, ResolutionReport, ResolutionTarget, ResolvedImport,
    ResolvedInclude, ResolvedProject, ResolvedUnit, SourceId, SourceRevision,
};

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
