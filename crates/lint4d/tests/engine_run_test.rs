use lint4d::cfg::project_snapshot::{CfgSnapshotOptions, to_cfg_project_snapshot};
use lint4d::config::Config;
use lint4d::engine::{FileInfo, run_lint, run_lint_with_cfg_project};
use lint4d::rules::RuleRegistry;
use pascal_core::resolver::{
    LoadedSource, ResolutionReport, ResolutionTarget, ResolvedInclude, ResolvedProject,
    ResolvedUnit, SourceId, SourceRevision,
};
use std::path::PathBuf;
use std::sync::Arc;

#[test]
fn identifier_casing_flags_mismatched_casing() {
    let source = std::fs::read("tests/fixtures/naming/bad_identifier_casing.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "identifier-casing")
        .collect();
    // fvalue (should be FValue), localobj (should be LocalObj), counter (should be Counter), my_const (should be MY_CONST)
    assert_eq!(hits.len(), 4, "Should flag 4 casing mismatches: {:?}", hits);
}

#[test]
fn identifier_casing_passes_consistent_casing() {
    let source = std::fs::read("tests/fixtures/naming/good_identifier_casing.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "identifier-casing")
        .collect();
    assert!(
        hits.is_empty(),
        "No identifier-casing diagnostics expected: {:?}",
        hits
    );
}

#[test]
fn identifier_casing_scopes_fields_per_class() {
    let source =
        std::fs::read("tests/fixtures/naming/bad_identifier_casing_multiclass.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "identifier-casing")
        .collect();
    // TClassA.Run: fdata should be FData (1 hit)
    // TClassB.Run: Fdata should be fData (1 hit)
    assert_eq!(
        hits.len(),
        2,
        "Should flag 2 casing mismatches (one per class): {:?}",
        hits
    );
}

#[test]
fn engine_runs_on_valid_file_with_no_issues() {
    let source = b"unit Clean;\ninterface\nimplementation\nend.\n";
    let file = FileInfo::new(PathBuf::from("Clean.pas"));
    let config = "version = 1".parse::<Config>().unwrap();

    let diagnostics = run_lint(&file, source, &config);
    assert!(diagnostics.is_empty());
}

#[test]
fn engine_reports_parse_errors() {
    let source = b"unit Bad;\n@@@\nend.\n";
    let file = FileInfo::new(PathBuf::from("Bad.pas"));
    let config = "version = 1".parse::<Config>().unwrap();

    let diagnostics = run_lint(&file, source, &config);
    assert!(diagnostics.iter().any(|d| d.rule_id == "parse-error"));
}

#[test]
fn engine_skips_parse_errors_in_dpr_files() {
    // The `in 'path'` syntax in uses clauses is valid Delphi but unsupported
    // by tree-sitter-pascal, producing spurious parse errors. These should be
    // suppressed for .dpr/.dpk files.
    let source = b"program Test;\nuses\n  MyUnit in 'path\\MyUnit.pas';\nbegin\nend.\n";
    let file = FileInfo::new(PathBuf::from("Test.dpr"));
    let config = "version = 1".parse::<Config>().unwrap();

    let diagnostics = run_lint(&file, source, &config);
    let parse_errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "parse-error")
        .collect();
    assert!(
        parse_errors.is_empty(),
        "Parse errors should be suppressed for .dpr files, got: {:?}",
        parse_errors
    );
}

#[test]
fn engine_skips_bare_raise_parse_errors() {
    // Bare `raise;` (re-raise) is valid Delphi but produces ERROR nodes in
    // tree-sitter-pascal. These should be suppressed.
    let source = b"unit Test;\ninterface\nimplementation\nprocedure Foo;\nbegin\n  try\n    DoWork;\n  except\n    raise;\n  end;\nend;\nend.\n";
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1".parse::<Config>().unwrap();

    let diagnostics = run_lint(&file, source, &config);
    let raise_errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "parse-error" && d.message.contains("raise"))
        .collect();
    assert!(
        raise_errors.is_empty(),
        "Bare raise; should not produce parse errors, got: {:?}",
        raise_errors
    );
}

#[test]
fn engine_keeps_parse_errors_in_pas_files() {
    // Regular .pas files should still report parse errors.
    let source = b"unit Bad;\n@@@\nend.\n";
    let file = FileInfo::new(PathBuf::from("Bad.pas"));
    let config = "version = 1".parse::<Config>().unwrap();

    let diagnostics = run_lint(&file, source, &config);
    assert!(
        diagnostics.iter().any(|d| d.rule_id == "parse-error"),
        "Parse errors should still be reported for .pas files"
    );
}

#[test]
fn shared_runner_without_project_matches_file_local_wrapper() {
    let source = std::fs::read("tests/fixtures/naming/bad_identifier_casing.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1".parse::<Config>().unwrap();
    let expected = run_lint(&file, &source, &config);
    let actual =
        run_lint_with_cfg_project(&file, &source, &config, None, None, &RuleRegistry::new());
    assert_eq!(
        serde_json::to_value(&actual).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
}

#[test]
fn prepared_include_diagnostic_falls_back_without_publishing_a_fake_location() {
    let source = b"unit App;\ninterface\n{$I decls.inc}\nimplementation\nend.\n";
    let include = b"const badConst = 1;\n";
    let root_id = SourceId::new("source:/workspace/App.pas");
    let include_id = SourceId::new("source:/workspace/decls.inc");
    let directive = b"{$I decls.inc}";
    let directive_start = source
        .windows(directive.len())
        .position(|window| window == directive)
        .expect("include directive");
    let project = ResolvedProject {
        root: ResolvedUnit {
            requested_name: "App".to_string(),
            declared_name: "App".to_string(),
            source: LoadedSource {
                id: root_id.clone(),
                path: PathBuf::from("/workspace/App.pas"),
                bytes: Arc::from(source.to_vec()),
                decoded_text: None,
                revision: SourceRevision::Overlay {
                    version: 1,
                    content_hash: 1,
                },
            },
        },
        units: Vec::new(),
        imports: Vec::new(),
        includes: vec![ResolvedInclude {
            including_source_id: root_id.clone(),
            byte_range: directive_start..directive_start + directive.len(),
            requested_name: "decls.inc".to_string(),
            target: ResolutionTarget::Found(include_id.clone()),
        }],
        include_sources: vec![LoadedSource {
            id: include_id,
            path: PathBuf::from("/workspace/decls.inc"),
            bytes: Arc::from(include.to_vec()),
            decoded_text: None,
            revision: SourceRevision::Overlay {
                version: 1,
                content_hash: 1,
            },
        }],
        complete: true,
        report: ResolutionReport {
            observations: Vec::new(),
            warnings: Vec::new(),
            complete: true,
            incomplete_reasons: Vec::new(),
        },
    };
    let snapshot = to_cfg_project_snapshot(
        project,
        CfgSnapshotOptions {
            prepare_configured_sources: true,
            configuration_id: Some("debug".to_string()),
            ..CfgSnapshotOptions::default()
        },
    )
    .expect("prepared snapshot");
    let config = "version = 1\n[rules.naming]\nconstant_style = \"PascalCase\""
        .parse::<Config>()
        .unwrap();
    let diagnostics = run_lint_with_cfg_project(
        &FileInfo::new(PathBuf::from("/workspace/App.pas")),
        source,
        &config,
        None,
        Some(&snapshot),
        &RuleRegistry::new(),
    );

    assert!(diagnostics.iter().all(|diagnostic| {
        diagnostic.rule_id != "constant-naming" && diagnostic.rule_id != "lint4d-error"
    }));
}

#[test]
fn prepared_include_diagnostic_scope_uses_original_coordinates() {
    let source = b"unit Main;\ninterface\nimplementation\n{$I pad.inc}\nprocedure Test;\nvar X: TObject;\nbegin\n  X.Free;\n  X.Foo;\nend;\nend.\n";
    let pad = b"\n\n\n\n\n\n\n\n\n\n";
    let root_id = SourceId::new("source:/workspace/Main.pas");
    let pad_id = SourceId::new("source:/workspace/pad.inc");
    let directive = b"{$I pad.inc}";
    let directive_start = source
        .windows(directive.len())
        .position(|window| window == directive)
        .expect("include directive");
    let project = ResolvedProject {
        root: ResolvedUnit {
            requested_name: "Main".to_string(),
            declared_name: "Main".to_string(),
            source: LoadedSource {
                id: root_id.clone(),
                path: PathBuf::from("/workspace/Main.pas"),
                bytes: Arc::from(source.to_vec()),
                decoded_text: None,
                revision: SourceRevision::Overlay {
                    version: 1,
                    content_hash: 1,
                },
            },
        },
        units: Vec::new(),
        imports: Vec::new(),
        includes: vec![ResolvedInclude {
            including_source_id: root_id.clone(),
            byte_range: directive_start..directive_start + directive.len(),
            requested_name: "pad.inc".to_string(),
            target: ResolutionTarget::Found(pad_id.clone()),
        }],
        include_sources: vec![LoadedSource {
            id: pad_id,
            path: PathBuf::from("/workspace/pad.inc"),
            bytes: Arc::from(pad.to_vec()),
            decoded_text: None,
            revision: SourceRevision::Overlay {
                version: 1,
                content_hash: 2,
            },
        }],
        complete: true,
        report: ResolutionReport {
            observations: Vec::new(),
            warnings: Vec::new(),
            complete: true,
            incomplete_reasons: Vec::new(),
        },
    };
    let snapshot = to_cfg_project_snapshot(
        project,
        CfgSnapshotOptions {
            prepare_configured_sources: true,
            configuration_id: Some("debug".to_string()),
            ..CfgSnapshotOptions::default()
        },
    )
    .expect("prepared snapshot");
    let config = "version = 1".parse::<Config>().unwrap();
    let diagnostics = run_lint_with_cfg_project(
        &FileInfo::new(PathBuf::from("/workspace/Main.pas")),
        source,
        &config,
        None,
        Some(&snapshot),
        &RuleRegistry::new(),
    );
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.rule_id == "use-after-free")
        .expect("prepared use-after-free diagnostic");
    assert_eq!(diagnostic.line, 9);
    assert_eq!(diagnostic.column, 3);
    assert_eq!(diagnostic.scope.as_deref(), Some("Test"));
}

#[test]
fn engine_filters_suppressed_diagnostics() {
    let source = b"unit Test;\n// lint4d:ignore parse-error\n@@@\nend.\n";
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1".parse::<Config>().unwrap();

    let diagnostics = run_lint(&file, source, &config);
    // Parse error on line 3 should be suppressed by comment on line 2
    let parse_errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "parse-error" && d.line == 3)
        .collect();
    assert!(
        parse_errors.is_empty(),
        "Parse error on line 3 should be suppressed, got: {:?}",
        parse_errors
    );
}

#[test]
fn local_variable_naming_flags_pascal_case_in_camel_mode() {
    let source = std::fs::read("tests/fixtures/naming/bad_local_variable_camel.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1\n[rules.naming]\nlocal_variable_style = \"camelCase\""
        .parse::<Config>()
        .unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "local-variable-naming")
        .collect();
    assert_eq!(
        hits.len(),
        4,
        "Should flag BadParam, AnotherParam, MyCounter and AnotherBadName but not x: {:?}",
        hits
    );
}

#[test]
fn local_variable_naming_passes_camel_case() {
    let source = std::fs::read("tests/fixtures/naming/good_local_variable_camel.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1\n[rules.naming]\nlocal_variable_style = \"camelCase\""
        .parse::<Config>()
        .unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "local-variable-naming")
        .collect();
    assert!(
        hits.is_empty(),
        "No local-variable-naming diagnostics expected: {:?}",
        hits
    );
}

#[test]
fn local_variable_naming_flags_camel_case_in_pascal_mode() {
    let source = std::fs::read("tests/fixtures/naming/bad_local_variable_pascal.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1\n[rules.naming]\nlocal_variable_style = \"PascalCase\""
        .parse::<Config>()
        .unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "local-variable-naming")
        .collect();
    assert_eq!(
        hits.len(),
        4,
        "Should flag badParam, anotherParam, myCounter and anotherBad but not x: {:?}",
        hits
    );
}

#[test]
fn local_variable_naming_passes_pascal_case() {
    let source = std::fs::read("tests/fixtures/naming/good_local_variable_pascal.pas").unwrap();
    let file = FileInfo::new(PathBuf::from("Test.pas"));
    let config = "version = 1\n[rules.naming]\nlocal_variable_style = \"PascalCase\""
        .parse::<Config>()
        .unwrap();
    let diagnostics = run_lint(&file, &source, &config);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "local-variable-naming")
        .collect();
    assert!(
        hits.is_empty(),
        "No local-variable-naming diagnostics expected: {:?}",
        hits
    );
}
