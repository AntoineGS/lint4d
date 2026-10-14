use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn lint4d() -> Command {
    Command::cargo_bin("lint4d").unwrap()
}

#[test]
fn version_flag() {
    lint4d()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("lint4d"));
}

#[test]
fn no_args_shows_help() {
    lint4d()
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn lint_clean_file_exits_zero() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("Clean.pas"),
        "unit Clean;\ninterface\nimplementation\nend.\n",
    )
    .unwrap();

    lint4d()
        .arg(dir.path().join("Clean.pas"))
        .assert()
        .success();
}

#[test]
fn lint_with_json_format() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("Clean.pas"),
        "unit Clean;\ninterface\nimplementation\nend.\n",
    )
    .unwrap();

    lint4d()
        .arg("--format")
        .arg("json")
        .arg(dir.path().join("Clean.pas"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"version\": 1"));
}

#[test]
fn init_creates_config_file() {
    let dir = TempDir::new().unwrap();
    lint4d()
        .arg("--init")
        .current_dir(dir.path())
        .assert()
        .success();
    assert!(dir.path().join(".lint4d.toml").exists());
}

#[test]
fn list_rules_shows_all_rules() {
    lint4d()
        .arg("--list-rules")
        .assert()
        .success()
        .stdout(predicate::str::contains("empty-except"))
        .stdout(predicate::str::contains("resource-leak-unprotected"));
}

#[test]
fn explain_known_rule() {
    lint4d()
        .arg("--explain")
        .arg("empty-except")
        .assert()
        .success()
        .stdout(predicate::str::contains("empty-except"))
        .stdout(predicate::str::contains("Empty Except Block"));
}

#[test]
fn explain_unknown_rule() {
    lint4d()
        .arg("--explain")
        .arg("nonexistent-rule")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Unknown rule"));
}

#[test]
fn lint_directory_recursively() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(
        sub.join("Unit1.pas"),
        "unit Unit1;\ninterface\nimplementation\nend.\n",
    )
    .unwrap();

    lint4d().arg(dir.path()).assert().success();
}

#[test]
fn fail_on_error_ignores_warnings() {
    let dir = TempDir::new().unwrap();
    // with-statement triggers a warning, not an error
    fs::write(
        dir.path().join("WithTest.pas"),
        "unit WithTest;\ninterface\nimplementation\nprocedure Foo;\nvar Obj: TObject;\nbegin\n  with Obj do\n    Writeln('hi');\nend;\nend.\n",
    )
    .unwrap();

    lint4d()
        .arg("--fail-on")
        .arg("error")
        .arg(dir.path().join("WithTest.pas"))
        .assert()
        .success();
}

#[test]
fn fail_on_warning_catches_warnings() {
    let dir = TempDir::new().unwrap();
    // with-statement triggers a warning
    fs::write(
        dir.path().join("WithTest.pas"),
        "unit WithTest;\ninterface\nimplementation\nprocedure Foo;\nvar Obj: TObject;\nbegin\n  with Obj do\n    Writeln('hi');\nend;\nend.\n",
    )
    .unwrap();

    lint4d()
        .arg("--fail-on")
        .arg("warning")
        .arg(dir.path().join("WithTest.pas"))
        .assert()
        .code(1);
}

#[test]
fn generate_baseline_creates_file_and_suppresses() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("Bad.pas"),
        "unit Bad;\ninterface\nimplementation\nprocedure X;\nbegin\ntry\n  WriteLn('x');\nexcept\nend;\nend;\nend.\n",
    )
    .unwrap();

    // Generate baseline
    lint4d()
        .arg("--generate-baseline")
        .arg(dir.path().join("Bad.pas"))
        .current_dir(dir.path())
        .assert()
        .success();

    assert!(dir.path().join(".lint4d-baseline.json").exists());

    // Lint with baseline — should suppress existing violations
    lint4d()
        .arg(dir.path().join("Bad.pas"))
        .current_dir(dir.path())
        .assert()
        .success(); // exit 0 because all violations are baselined
}

#[test]
fn project_flag_lints_dproj_files() {
    let dir = TempDir::new().unwrap();
    // Create a simple .pas file
    fs::write(
        dir.path().join("Unit1.pas"),
        "unit Unit1;\ninterface\nimplementation\nend.\n",
    )
    .unwrap();
    // Create a .dproj referencing it
    fs::write(
        dir.path().join("MyProject.dproj"),
        r#"<?xml version="1.0" encoding="utf-8"?>
<Project>
  <ItemGroup>
    <DCCReference Include="Unit1.pas"/>
  </ItemGroup>
</Project>"#,
    )
    .unwrap();

    lint4d()
        .arg("--project")
        .arg(dir.path().join("MyProject.dproj"))
        .current_dir(dir.path())
        .assert()
        .success();
}

#[test]
fn project_resolution_reports_incomplete_imports_but_keeps_linting() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("Main.pas"),
        "unit Main;\ninterface\nuses MissingUnit;\nimplementation\nend.\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("MyProject.dproj"),
        r#"<?xml version="1.0" encoding="utf-8"?>
<Project>
  <ItemGroup>
    <DCCReference Include="Main.pas"/>
  </ItemGroup>
</Project>"#,
    )
    .unwrap();

    lint4d()
        .arg("--project")
        .arg(dir.path().join("MyProject.dproj"))
        .current_dir(dir.path())
        .assert()
        .success()
        .stderr(
            predicate::str::contains("source-project CFG")
                .and(predicate::str::contains("incomplete")),
        );
}

// macOS reaches its temporary directories through symlinks (/var, /tmp).
#[cfg(unix)]
#[test]
fn project_given_through_a_symlinked_directory_is_resolved() {
    let dir = TempDir::new().unwrap();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::write(
        real.join("Main.pas"),
        "unit Main;\ninterface\nuses MissingUnit;\nimplementation\nend.\n",
    )
    .unwrap();
    fs::write(
        real.join("MyProject.dproj"),
        r#"<Project><ItemGroup><DCCReference Include="Main.pas"/></ItemGroup></Project>"#,
    )
    .unwrap();
    let linked = dir.path().join("linked");
    std::os::unix::fs::symlink(&real, &linked).unwrap();

    lint4d()
        .arg("--project")
        .arg(linked.join("MyProject.dproj"))
        .current_dir(&linked)
        .assert()
        .success()
        .stderr(
            predicate::str::contains("source-project CFG")
                .and(predicate::str::contains("incomplete"))
                .and(predicate::str::contains("not authorized").not()),
        );
}

#[test]
fn project_cfg_preserves_crlf_diagnostics_and_original_scope() {
    let dir = TempDir::new().unwrap();
    let main = dir.path().join("Main.pas");
    let pad = dir.path().join("pad.inc");
    let project = dir.path().join("App.dproj");
    let lf_source = "unit Main;\ninterface\nimplementation\n{$I pad.inc}\nprocedure Test;\nvar X: TObject;\nbegin\n  X.Free;\n  X.Foo;\nend;\nend.\n";
    let crlf_source = lf_source.replace('\n', "\r\n");
    fs::write(&pad, "\n\n\n\n\n\n\n\n\n\n").unwrap();
    fs::write(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"Main.pas\"/></ItemGroup></Project>",
    )
    .unwrap();

    fs::write(&main, lf_source).unwrap();
    let lint = |expected_source: &str| -> Value {
        fs::write(&main, expected_source).unwrap();
        let output = lint4d()
            .arg("--project")
            .arg(&project)
            .arg("--format")
            .arg("json")
            .output()
            .unwrap();
        assert!(
            output.status.success() || output.status.code() == Some(1),
            "lint4d failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    let lf = lint(lf_source);
    let crlf = lint(&crlf_source);
    let lf_diagnostics = &lf["files"][0]["diagnostics"];
    let crlf_diagnostics = &crlf["files"][0]["diagnostics"];
    assert_eq!(lf_diagnostics, crlf_diagnostics);
    assert_eq!(crlf_diagnostics[0]["rule_id"], "use-after-free");
    assert_eq!(crlf_diagnostics[0]["line"], 9);
    assert_eq!(crlf_diagnostics[0]["column"], 3);
    assert_eq!(crlf_diagnostics[0]["scope"], "Test");
}

#[test]
fn project_root_alias_does_not_replace_the_selected_main_source() {
    let dir = TempDir::new().unwrap();
    let main = dir.path().join("Main.pas");
    let other = dir.path().join("Other.pas");
    let project = dir.path().join("App.dproj");
    fs::write(
        &main,
        "unit Main;\ninterface\nimplementation\nprocedure Test;\nvar X: TObject;\nbegin\n  X.Free;\n  X.Foo;\nend;\nend.\n",
    )
    .unwrap();
    fs::write(&other, "unit Other;\ninterface\nimplementation\nend.\n").unwrap();
    fs::write(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitAlias>Main=Other</DCC_UnitAlias></PropertyGroup><ItemGroup><DCCReference Include=\"Main.pas\"/></ItemGroup></Project>",
    )
    .unwrap();

    let output = lint4d()
        .arg("--project")
        .arg(&project)
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "lint4d failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostics = &json["files"][0]["diagnostics"];
    assert_eq!(diagnostics[0]["rule_id"], "use-after-free");
    assert_eq!(diagnostics[0]["line"], 8);
    assert_eq!(diagnostics[0]["column"], 3);
}

#[test]
fn project_dpr_root_is_loaded_by_selected_full_path() {
    let dir = TempDir::new().unwrap();
    let main = dir.path().join("App.dpr");
    let other = dir.path().join("Other.pas");
    let project = dir.path().join("App.dproj");
    fs::write(
        &main,
        "program App;\nprocedure Test;\nvar X: TObject;\nbegin\n  X.Free;\n  X.Foo;\nend;\nbegin\n  Test;\nend.\n",
    )
    .unwrap();
    fs::write(&other, "unit Other;\ninterface\nimplementation\nend.\n").unwrap();
    fs::write(
        &project,
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_UnitAlias>App=Other</DCC_UnitAlias></PropertyGroup><ItemGroup><DCCReference Include=\"App.dpr\"/></ItemGroup></Project>",
    )
    .unwrap();

    let output = lint4d()
        .arg("--project")
        .arg(&project)
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "lint4d failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostics = &json["files"][0]["diagnostics"];
    assert_eq!(diagnostics[0]["rule_id"], "use-after-free");
    assert_eq!(diagnostics[0]["line"], 6);
    assert_eq!(diagnostics[0]["column"], 3);
}

#[test]
fn project_latin1_root_include_and_dependency_match_decoded_positions() {
    let dir = TempDir::new().unwrap();
    let main = dir.path().join("Main.pas");
    let provider = dir.path().join("Provider.pas");
    let include = dir.path().join("body.inc");
    let project = dir.path().join("App.dproj");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Test;\nvar X: TObject;\nbegin\n  // café\n  {$I body.inc}\n  X.Foo;\nend;\nend.\n";
    let provider_source = "unit Provider;\ninterface\n// café\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    let include_source = "// café\nX.Free;\n";
    fs::write(&main, latin1(main_source)).unwrap();
    fs::write(&provider, latin1(provider_source)).unwrap();
    fs::write(&include, latin1(include_source)).unwrap();
    fs::write(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"Main.pas\"/></ItemGroup></Project>",
    )
    .unwrap();

    let output = lint4d()
        .arg("--project")
        .arg(&project)
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "lint4d failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostics = &json["files"][0]["diagnostics"];
    assert_eq!(diagnostics[0]["rule_id"], "use-after-free");
    assert_eq!(diagnostics[0]["line"], 10);
    assert_eq!(diagnostics[0]["column"], 3);
}

fn latin1(source: &str) -> Vec<u8> {
    source
        .chars()
        .map(|character| {
            let codepoint = character as u32;
            assert!(codepoint <= 0xff, "test source is not Latin-1");
            codepoint as u8
        })
        .collect()
}

#[test]
fn fix_fmt_skips_public_type_rename_and_fixes_case_only_local_shadow() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("BadPrefix.pas");
    fs::write(
        &path,
        "unit BadPrefix;\n\ninterface\n\ntype\n  Foo = class(TObject)\n  end;\n  Printable = interface\n  end;\n\nimplementation\n\nprocedure Work;\nvar\n  foo: Integer;\nbegin\n  foo := 1;\nend;\n\nend.\n",
    )
    .unwrap();

    let output = lint4d().arg("--fix-fmt").arg(&path).output().unwrap();
    assert!(
        output.status.success(),
        "lint4d failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("\n  Foo = class(TObject)"),
        "public type was renamed:\n{content}"
    );
    assert!(
        content.contains("\n  Printable = interface"),
        "public interface was renamed:\n{content}"
    );
    assert!(!content.contains("TFoo = class"), "{content}");
    assert!(content.contains("Foo: Integer;"), "{content}");
    assert!(content.contains("Foo := 1;"), "{content}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Foo -> TFoo"),
        "missing skipped-rename warning: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Printable -> IPrintable"),
        "missing skipped-interface warning: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn fix_fmt_no_output_for_clean_file() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("Clean.pas"),
        "unit Clean;\n\ninterface\n\ntype\n  TMyClass = class\n  end;\n\nimplementation\n\nend.\n",
    )
    .unwrap();

    lint4d()
        .arg("--fix-fmt")
        .arg(dir.path().join("Clean.pas"))
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

#[test]
fn fix_fmt_mutually_exclusive_with_format() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("Test.pas"),
        "unit Test;\ninterface\nimplementation\nend.\n",
    )
    .unwrap();

    lint4d()
        .arg("--fix-fmt")
        .arg("--format")
        .arg("json")
        .arg(dir.path().join("Test.pas"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("--fix-fmt"));
}

#[test]
fn fix_fmt_mutually_exclusive_with_generate_baseline() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("Test.pas"),
        "unit Test;\ninterface\nimplementation\nend.\n",
    )
    .unwrap();

    lint4d()
        .arg("--fix-fmt")
        .arg("--generate-baseline")
        .arg(dir.path().join("Test.pas"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("--fix-fmt"));
}

#[test]
fn fix_fmt_skips_constant_rename_and_warns_with_suggestion() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("Const.pas");
    fs::write(
        &path,
        r#"unit &Const;

interface

const
  maxSize = 100;

implementation

procedure DoWork;
var
  x: Integer;
begin
  x := maxSize;
end;

end.
"#,
    )
    .unwrap();

    let output = lint4d().arg("--fix-fmt").arg(&path).output().unwrap();
    assert!(
        output.status.success(),
        "lint4d failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("maxSize = 100;"), "{content}");
    assert!(content.contains("x := maxSize;"), "{content}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("maxSize -> MAX_SIZE"),
        "missing skipped-rename warning: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn fix_fmt_nested_shadow_blocks_ancestor_but_capture_is_preserved() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("Nested.pas");
    fs::write(
        &path,
        r#"unit Nested;
interface
implementation
procedure Outer;
var
  myVar: Integer;
  procedure ShadowParameter(MYVAR: Integer);
  begin
    MYVAR := 3;
  end;
  procedure ShadowLocal;
  var
    MYVAR: Integer;
  begin
    MYVAR := 4;
  end;
  procedure Capture;
  begin
    myVar := 5;
  end;
begin
  myVar := 2;
end;
end.
"#,
    )
    .unwrap();

    lint4d().arg("--fix-fmt").arg(&path).assert().success();

    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("MyVar: Integer;"), "{content}");
    assert!(
        content.contains("ShadowParameter(MYVAR: Integer)"),
        "{content}"
    );
    assert!(content.contains("MYVAR := 3;"), "{content}");
    assert!(content.contains("MYVAR: Integer;"), "{content}");
    assert!(content.contains("MYVAR := 4;"), "{content}");
    assert!(
        !content.contains("myVar := 5;"),
        "capture not fixed:\n{content}"
    );
    assert!(content.contains("MyVar := 5;"), "{content}");
    assert!(content.contains("MyVar := 2;"), "{content}");
}

#[test]
fn fix_fmt_renames_case_only_parameter_and_usages() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("Parameter.pas");
    fs::write(
        &path,
        "unit Parameter;\ninterface\nimplementation\nprocedure Work(badParam: Integer);\nbegin\n  badParam := 1;\nend;\nend.\n",
    )
    .unwrap();

    lint4d().arg("--fix-fmt").arg(&path).assert().success();

    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("Work(BadParam: Integer)"), "{content}");
    assert!(content.contains("BadParam := 1;"), "{content}");
}

#[test]
fn fix_fmt_declines_non_case_local_rename_and_warns() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("NonCase.pas");
    let source = "unit NonCase;\ninterface\nimplementation\nprocedure Work;\nvar\n  bad_name: Integer;\nbegin\n  bad_name := 1;\nend;\nend.\n";
    fs::write(&path, source).unwrap();

    let output = lint4d().arg("--fix-fmt").arg(&path).output().unwrap();
    assert!(output.status.success());

    assert_eq!(fs::read_to_string(&path).unwrap(), source);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("bad_name -> BadName"),
        "missing non-case warning: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn fix_fmt_parse_error_leaves_file_unchanged_and_warns() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("Malformed.pas");
    let source = "unit Malformed;\ninterface\nimplementation\nprocedure Work;\nvar\n  badLocal: Integer;\nbegin\n  badLocal := ;\nend;\nend.\n";
    fs::write(&path, source).unwrap();

    let output = lint4d().arg("--fix-fmt").arg(&path).output().unwrap();
    assert!(output.status.success());

    assert_eq!(fs::read_to_string(&path).unwrap(), source);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("parse error"),
        "missing parse-error warning: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The 8.3 short form of `path`, when its volume generates short names.
#[cfg(windows)]
fn windows_short_path(path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::os::windows::process::CommandExt;
    // cmd does not parse the quoting `Command::arg` applies.
    let output = std::process::Command::new("cmd")
        .raw_arg(format!(
            "/C for %I in (\"{}\") do @echo %~sI",
            path.display()
        ))
        .output()
        .ok()?;
    let short = std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    (short.as_os_str() != path.as_os_str() && short.exists()).then_some(short)
}

/// A project under the user's own temporary directory on the system volume
/// (`C:\Users\<name>\AppData\Local\Temp`), which has 8.3 names, as `TEMP`
/// does on a stock Windows runner. Returns the directory and its short form.
#[cfg(windows)]
fn short_name_project(
    main_source: impl Fn(&std::path::Path) -> String,
) -> Option<(TempDir, std::path::PathBuf)> {
    let local_temp = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Temp");
    let dir = TempDir::new_in(&local_temp).unwrap();
    let project_dir = dir.path().join("Long Project Directory");
    fs::create_dir(&project_dir).unwrap();
    let main = project_dir.join("Main.pas");
    fs::write(
        &main,
        "unit Main;\ninterface\nimplementation\nprocedure Test;\nvar X: TObject;\nbegin\n  X.Free;\n  X.Foo;\nend;\nend.\n",
    )
    .unwrap();
    let Some(short_dir) = windows_short_path(&project_dir) else {
        // GitHub's Windows runners have 8.3 names on the system volume,
        // so CI must not skip this silently.
        assert!(
            std::env::var_os("CI").is_none(),
            "no 8.3 name for {}",
            project_dir.display()
        );
        return None;
    };
    let reference = main_source(&short_dir.join("Main.pas"));
    fs::write(
        project_dir.join("App.dproj"),
        format!(
            "<Project><PropertyGroup><MainSource>{reference}</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"{reference}\"/></ItemGroup></Project>"
        ),
    )
    .unwrap();
    Some((dir, short_dir))
}

#[cfg(windows)]
fn assert_project_cfg_for(project: &std::path::Path) {
    let output = lint4d()
        .arg("--project")
        .arg(project)
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "lint4d failed: {stderr}"
    );
    assert!(
        !stderr.contains("file-local CFG") && !stderr.contains("source-project resolution"),
        "source project was not resolved for {}: {stderr}",
        project.display()
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["files"][0]["diagnostics"][0]["rule_id"], "use-after-free",
        "{report}\n{stderr}"
    );
}

#[cfg(windows)]
#[test]
fn project_flag_resolves_the_source_project_through_a_short_name_path() {
    let Some((_dir, short_dir)) = short_name_project(|_| "Main.pas".to_string()) else {
        return;
    };
    assert_project_cfg_for(&short_dir.join("App.dproj"));
}

#[cfg(windows)]
#[test]
fn project_source_referenced_by_a_short_name_path_resolves_the_source_project() {
    let Some((dir, _short_dir)) = short_name_project(|short_main| short_main.display().to_string())
    else {
        return;
    };
    assert_project_cfg_for(&dir.path().join("Long Project Directory").join("App.dproj"));
}
