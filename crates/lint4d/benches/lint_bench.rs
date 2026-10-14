use cfg_pascal::PreparationEnvironment;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use lint4d::cfg::{CfgSnapshotOptions, CfgSnapshotStatus, to_cfg_project_snapshot};
use lint4d::config::Config;
use lint4d::dcu::{
    DcuPlatform, DcuUnit, DcuVersion, ProjectContext as DcuProjectContext, TypeInfo, TypeKind,
};
use lint4d::discovery::dproj::parse_dproj;
use lint4d::engine::{FileInfo, run_lint, run_lint_with_cfg_project};
use lint4d::fix::fix_file_restricted;
use lint4d::rules::RuleRegistry;
use pascal_core::resolver::{FilesystemSourceStore, NoCancellation, ResolverLimits, UnitResolver};
use pascal_project::delphi_overrides::OverrideSession;
use pascal_project::{ProjectContext, ProjectOptions};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

const PROJECT_UNIT_COUNT: usize = 200;
const FIX_FMT_EDIT_COUNT: usize = 1_000;

fn bench_lint_single_file(c: &mut Criterion) {
    let file = FileInfo::new(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/resource_leak/bad_unprotected.pas"),
    );
    let source = fs::read(&file.path).expect("fixture not found");
    let config: Config = "version = 1".parse().unwrap();

    c.bench_function("lint_single_file", |b| {
        b.iter(|| run_lint(black_box(&file), black_box(&source), black_box(&config)))
    });
}

fn bench_parse_only(c: &mut Criterion) {
    let file = FileInfo::new(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/resource_leak/bad_unprotected.pas"),
    );
    let source = fs::read(&file.path).expect("fixture not found");

    c.bench_function("parse_only", |b| {
        b.iter(|| lint4d::engine::parse_file(black_box(&file), black_box(&source)).unwrap())
    });
}

struct SyntheticProject {
    _directory: TempDir,
    files: Vec<FileInfo>,
    source_roots: Vec<PathBuf>,
    context: ProjectContext,
    dcu_context: DcuProjectContext,
    config: Config,
    registry: RuleRegistry,
}

fn bench_project_lint_200_units(c: &mut Criterion) {
    let project = synthetic_project();
    let proof = lint_project(&project);
    assert_eq!(project.files.len(), PROJECT_UNIT_COUNT);
    assert_eq!(proof.resolved_unit_count, PROJECT_UNIT_COUNT * 2 - 1);
    assert_eq!(proof.field_not_freed_count, 1);

    c.bench_function("project_lint_200_units", |b| {
        b.iter(|| black_box(lint_project(black_box(&project))))
    });
}

struct ProjectLintProof {
    resolved_unit_count: usize,
    field_not_freed_count: usize,
}

fn lint_project(project: &SyntheticProject) -> ProjectLintProof {
    let snapshot_options = CfgSnapshotOptions {
        prepare_configured_sources: true,
        configuration_id: Some("criterion-benchmark".to_string()),
        preparation_environment: PreparationEnvironment::Partial,
        ..CfgSnapshotOptions::default()
    };
    let mut resolved_unit_count = 0;
    let mut field_not_freed_count = 0;

    for file in &project.files {
        let raw_source = fs::read(&file.path).expect("synthetic project unit should be readable");
        let source = pascal_core::decode_bytes(&raw_source)
            .into_owned()
            .into_bytes();
        let unit_name = file
            .path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("synthetic unit filename should be UTF-8");
        let mut resolver = UnitResolver::new(
            project.context.clone(),
            project.source_roots.clone(),
            FilesystemSourceStore::new(),
            ResolverLimits::default(),
        );
        let resolved = resolver
            .resolve_project_from_source(&file.path, unit_name, &[], None, &NoCancellation)
            .expect("synthetic project graph should resolve");
        assert!(resolved.complete, "project resolution must be complete");
        resolved_unit_count += resolved.units.len() + 1;

        let cfg_project = to_cfg_project_snapshot(resolved, snapshot_options.clone())
            .expect("resolved project should produce a CFG snapshot");
        assert_eq!(cfg_project.status, CfgSnapshotStatus::Complete);
        let diagnostics = run_lint_with_cfg_project(
            file,
            &source,
            &project.config,
            Some(&project.dcu_context),
            Some(&cfg_project),
            &project.registry,
        );
        field_not_freed_count += diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.rule_id == "field-not-freed")
            .count();
    }

    assert_eq!(
        resolved_unit_count,
        PROJECT_UNIT_COUNT * 2 - 1,
        "project lint should resolve the shared dependency for every non-leaf unit"
    );
    assert_eq!(
        field_not_freed_count, 1,
        "project lint should report the synthetic root field leak"
    );
    ProjectLintProof {
        resolved_unit_count,
        field_not_freed_count,
    }
}

fn synthetic_project() -> SyntheticProject {
    let directory = tempfile::tempdir().expect("temporary project directory");
    let root = directory.path().to_path_buf();
    let mut references = String::new();

    for index in 0..PROJECT_UNIT_COUNT {
        let unit_name = format!("Unit{index:03}");
        let source = if index == 0 {
            field_leak_source(&unit_name, Some("Unit199"))
        } else {
            simple_unit_source(
                &unit_name,
                (index + 1 < PROJECT_UNIT_COUNT).then_some("Unit199"),
            )
        };
        fs::write(root.join(format!("{unit_name}.pas")), source)
            .expect("write synthetic project unit");
        references.push_str(&format!(
            "    <DCCReference Include=\"{unit_name}.pas\"/>\n"
        ));
    }

    let project_file = root.join("Benchmark.dproj");
    fs::write(
        &project_file,
        format!(
            "<Project xmlns=\"http://schemas.microsoft.com/developer/msbuild/2003\">\n  <ItemGroup>\n{references}  </ItemGroup>\n</Project>\n"
        ),
    )
    .expect("write synthetic Delphi project");
    let files = parse_dproj(&project_file).expect("synthetic .dproj should parse");
    assert_eq!(files.len(), PROJECT_UNIT_COUNT);
    assert!(files.iter().all(|file| file.path.is_file()));

    let options = ProjectOptions {
        project_file: Some(project_file),
        ..ProjectOptions::default()
    };
    let context = ProjectContext::discover_with_overrides(
        &files[0].path,
        std::slice::from_ref(&root),
        &options,
        &OverrideSession::new(None),
    )
    .expect("synthetic project context should be discoverable");
    assert!(context.discovery_complete);
    assert_eq!(context.project_file, options.project_file);

    SyntheticProject {
        _directory: directory,
        files,
        source_roots: vec![root],
        context,
        dcu_context: benchmark_dcu_context(),
        config: config_for_rules(&["field-not-freed"]),
        registry: RuleRegistry::new(),
    }
}

fn simple_unit_source(unit_name: &str, imported_unit: Option<&str>) -> String {
    let uses = imported_unit
        .map(|unit| format!("uses {unit};\n"))
        .unwrap_or_default();
    format!("unit {unit_name};\ninterface\n{uses}implementation\nend.\n")
}

fn field_leak_source(unit_name: &str, imported_unit: Option<&str>) -> String {
    let uses = imported_unit
        .map(|unit| format!("uses {unit};\n"))
        .unwrap_or_default();
    format!(
        "unit {unit_name};\ninterface\n{uses}type\n  TLeaky = class\n  private\n    FChild: TObject;\n  public\n    constructor Create;\n    destructor Destroy; override;\n  end;\nimplementation\nconstructor TLeaky.Create;\nbegin\n  inherited Create;\n  FChild := TObject.Create;\nend;\ndestructor TLeaky.Destroy;\nbegin\n  inherited;\nend;\nend.\n"
    )
}

fn bench_field_rules_with_dcu_context(c: &mut Criterion) {
    let file = FileInfo::new("criterion/FieldRules.pas".into());
    let source = field_rules_source().into_bytes();
    let config = config_for_rules(&[
        "field-not-freed",
        "field-reassign-leak",
        "resource-leak-no-try",
    ]);
    let registry = RuleRegistry::new();
    let dcu_context = benchmark_dcu_context();
    let dcu_uses = vec!["System".to_string()];
    assert!(
        dcu_context
            .is_class_type("TObject", &dcu_uses)
            .unwrap_or(false)
    );
    let diagnostics =
        run_lint_with_cfg_project(&file, &source, &config, Some(&dcu_context), None, &registry);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.rule_id == "field-not-freed"),
        "field-not-freed CFG path should produce a finding"
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.rule_id == "field-reassign-leak"),
        "field-reassign-leak CFG path should produce a finding"
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.rule_id == "resource-leak-no-try"),
        "DCU-context resource analysis should produce a finding"
    );

    c.bench_function("field_rules_with_dcu_context", |b| {
        b.iter(|| {
            let diagnostics = run_lint_with_cfg_project(
                black_box(&file),
                black_box(&source),
                black_box(&config),
                Some(black_box(&dcu_context)),
                None,
                black_box(&registry),
            );
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.rule_id == "field-not-freed")
            );
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.rule_id == "field-reassign-leak")
            );
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.rule_id == "resource-leak-no-try")
            );
            black_box(diagnostics)
        })
    });
}

fn field_rules_source() -> String {
    "unit FieldRules;\ninterface\nuses System;\ntype\n  TLeaky = class\n  private\n    FChild: TObject;\n  public\n    constructor Create;\n    procedure Replace;\n    destructor Destroy; override;\n  end;\nimplementation\nconstructor TLeaky.Create;\nbegin\n  inherited Create;\n  FChild := TObject.Create;\nend;\nprocedure TLeaky.Replace;\nvar\n  Temporary: TObject;\nbegin\n  Temporary := TObject.Create;\n  FChild := TObject.Create;\n  FChild := TObject.Create;\nend;\ndestructor TLeaky.Destroy;\nbegin\n  inherited;\nend;\nend.\n".to_string()
}

fn benchmark_dcu_context() -> DcuProjectContext {
    DcuProjectContext::from_units(vec![DcuUnit {
        name: "System".to_string(),
        version: DcuVersion::D13,
        platform: DcuPlatform::Win64,
        imported_units: Vec::new(),
        types: vec![TypeInfo {
            name: "TObject".to_string(),
            kind: TypeKind::Class,
            parent: None,
            fields: Vec::new(),
            methods: Vec::new(),
            interface_guid: None,
        }],
    }])
}

fn bench_fix_fmt_1000_edits(c: &mut Criterion) {
    let file = FileInfo::new("criterion/FixFmt1000.pas".into());
    let source = fix_fmt_source(FIX_FMT_EDIT_COUNT);
    let config = config_for_rules(&["local-variable-naming"]);
    let _ = run_fix_fmt_1000_edits(&file, &source, &config);

    c.bench_function("fix_fmt_1000_edits", |b| {
        b.iter(|| black_box(run_fix_fmt_1000_edits(&file, &source, &config)))
    });
}

fn run_fix_fmt_1000_edits(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
) -> (Vec<u8>, usize, Vec<String>) {
    let (fixed, edit_count, warnings) = fix_file_restricted(file, source, config)
        .expect("synthetic fix-fmt source should be accepted");
    assert_eq!(edit_count, FIX_FMT_EDIT_COUNT);
    assert!(warnings.is_empty());
    assert_ne!(fixed, source, "fix-fmt should change the synthetic source");
    (fixed, edit_count, warnings)
}

fn fix_fmt_source(edit_count: usize) -> Vec<u8> {
    let mut source =
        String::from("unit FixFmt1000;\ninterface\nimplementation\nprocedure Fix;\nvar\n");
    for index in 0..edit_count {
        source.push_str(&format!("  value{index:04}: Integer;\n"));
    }
    source.push_str("begin\nend;\nend.\n");
    source.into_bytes()
}

fn config_for_rules(enabled_rules: &[&str]) -> Config {
    let registry = RuleRegistry::new();
    let overrides = registry
        .all_rules()
        .iter()
        .map(|rule| {
            let severity = if enabled_rules.contains(&rule.meta().id) {
                rule.meta().default_severity.to_string()
            } else {
                "off".to_string()
            };
            format!("\"{}\" = \"{severity}\"", rule.meta().id)
        })
        .collect::<Vec<_>>()
        .join("\n");
    for enabled_rule in enabled_rules {
        assert!(
            registry.get(enabled_rule).is_some(),
            "unknown benchmark rule {enabled_rule}"
        );
    }
    format!("version = 1\n[rules]\n{overrides}")
        .parse()
        .expect("benchmark rule config should parse")
}

criterion_group!(
    benches,
    bench_lint_single_file,
    bench_parse_only,
    bench_project_lint_200_units,
    bench_field_rules_with_dcu_context,
    bench_fix_fmt_1000_edits
);
criterion_main!(benches);
