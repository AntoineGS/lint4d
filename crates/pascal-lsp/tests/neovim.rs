//! Exercises the shipped configuration through a real Neovim client, when installed.
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn configure_neovim_environment(command: &mut Command, environment: &TempDir) {
    for (name, directory) in [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        command.env(name, environment.path().join(directory));
    }
}

fn neovim_is_available() -> bool {
    let environment = tempfile::tempdir().expect("isolated Neovim environment");
    let mut command = Command::new("nvim");
    configure_neovim_environment(&mut command, &environment);
    command.arg("--version").output().is_ok()
}

#[test]
fn neovim_example_navigates_and_applies_workspace_edits_without_saving() {
    if !neovim_is_available() {
        eprintln!("Neovim integration skipped: nvim is not installed");
        return;
    }
    let directory = tempfile::Builder::new()
        .prefix("pascal lsp neovim ")
        .tempdir()
        .unwrap();
    fs::write(directory.path().join(".lint4d.toml"), "").unwrap();
    fs::write(
        directory.path().join("Provider.pas"),
        "unit Provider;\ninterface\nconst\n  badConst = 1;\n  unrelatedConst = 2;\nimplementation\nend.\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("Main.pas"),
        "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Log(badConst);\n  Log(unrelatedConst);\nend;\nend.\n",
    )
    .unwrap();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let environment = tempfile::tempdir().expect("isolated Neovim environment");
    let mut command = Command::new("nvim");
    command
        .args(["--headless", "-u", "NONE", "-l"])
        .arg(manifest.join("tests/neovim_smoke.lua"))
        .env("PASCAL_LSP_BIN", env!("CARGO_BIN_EXE_pascal-lsp"))
        .env("PASCAL_LSP_SMOKE_ROOT", directory.path())
        .env("PASCAL_LSP_CONFIG", manifest.join("examples/neovim.lua"));
    configure_neovim_environment(&mut command, &environment);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "Neovim timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Neovim integration failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("NEOVIM_WORKSPACE_EDIT_OK"));
}

#[test]
fn neovim_example_navigates_and_tracks_unsaved_documents() {
    if !neovim_is_available() {
        eprintln!("Neovim integration skipped: nvim is not installed");
        return;
    }
    let directory = tempfile::Builder::new()
        .prefix("pascal lsp neovim navigation ")
        .tempdir()
        .unwrap();
    fs::write(directory.path().join(".lint4d.toml"), "").unwrap();
    fs::write(
        directory.path().join("Provider.pas"),
        "unit Provider;\ninterface\nprocedure Greet;\nimplementation\nprocedure Greet;\nbegin\nend;\nend.\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("Main.pas"),
        "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Greet;\nend;\nend.\n",
    )
    .unwrap();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let environment = tempfile::tempdir().expect("isolated Neovim environment");
    let mut command = Command::new("nvim");
    command
        .args(["--headless", "-u", "NONE", "-l"])
        .arg(manifest.join("tests/neovim_navigation_smoke.lua"))
        .env("PASCAL_LSP_BIN", env!("CARGO_BIN_EXE_pascal-lsp"))
        .env("PASCAL_LSP_SMOKE_ROOT", directory.path())
        .env("PASCAL_LSP_CONFIG", manifest.join("examples/neovim.lua"));
    configure_neovim_environment(&mut command, &environment);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "Neovim navigation smoke timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Neovim navigation smoke failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("NEOVIM_LSP_OK"));
}

#[test]
fn neovim_project_selection() {
    if !neovim_is_available() {
        eprintln!("Neovim integration skipped: nvim is not installed");
        return;
    }
    let directory = tempfile::Builder::new()
        .prefix("pascal lsp neovim project selection ")
        .tempdir()
        .unwrap();
    let root = directory.path();
    fs::write(root.join(".lint4d.toml"), "").unwrap();
    fs::create_dir_all(root.join("projects/A")).unwrap();
    fs::create_dir_all(root.join("projects/B")).unwrap();
    fs::create_dir_all(root.join("other")).unwrap();
    fs::write(
        root.join("other/Startup.dproj"),
        "<Project><PropertyGroup><MainSource>../projects/Main.pas</MainSource></PropertyGroup></Project>",
    )
    .unwrap();
    fs::write(
        root.join("other/Startup.dpr"),
        "program Startup; begin end.\n",
    )
    .unwrap();
    fs::write(
        root.join("projects/Main.pas"),
        "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n",
    )
    .unwrap();
    fs::write(
        root.join("other/Other.pas"),
        "unit Other;\ninterface\nimplementation\nend.\n",
    )
    .unwrap();
    for project in ["A", "B"] {
        fs::write(
            root.join(format!("projects/{project}/Provider.pas")),
            format!(
                "unit Provider;\ninterface\nconst\n  ProjectName = '{project}';\nimplementation\nend.\n"
            ),
        )
        .unwrap();
        fs::write(
            root.join(format!("projects/{project}.dproj")),
            format!(
                "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"{project}/Provider.pas\" /></ItemGroup></Project>"
            ),
        )
        .unwrap();
    }

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let environment = tempfile::tempdir().expect("isolated Neovim environment");
    let mut command = Command::new("nvim");
    command
        .args(["--headless", "-u", "NONE", "-l"])
        .arg(manifest.join("tests/neovim_project_smoke.lua"))
        .env("PASCAL_LSP_BIN", env!("CARGO_BIN_EXE_pascal-lsp"))
        .env("PASCAL_LSP_SMOKE_ROOT", root)
        .env("PASCAL_LSP_CONFIG", manifest.join("examples/neovim.lua"));
    configure_neovim_environment(&mut command, &environment);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "Neovim project selection smoke timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Neovim project selection smoke failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("NEOVIM_PROJECT_SELECTION_OK"));
}

#[test]
fn neovim_delphi_overrides_navigate_to_native_source() {
    if !neovim_is_available() {
        eprintln!("Neovim integration skipped: nvim is not installed");
        return;
    }
    let directory = tempfile::Builder::new()
        .prefix("pascal lsp neovim delphi overrides ")
        .tempdir()
        .unwrap();
    let root = directory.path().join("project");
    let sdk = directory.path().join("sdk");
    let provider = sdk.join("source/Provider.pas");
    let main_source = "unit Main;\ninterface\nuses Provider;\nimplementation\nend.\n";
    let provider_source = "unit Provider;\ninterface\nprocedure ProviderRoutine;\nimplementation\nprocedure ProviderRoutine; begin end;\nend.\n";
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(".lint4d.toml"), "").unwrap();
    fs::write(root.join("Main.pas"), main_source).unwrap();
    fs::write(
        root.join("Main.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitSearchPath>$(BDS)\\source</DCC_UnitSearchPath></PropertyGroup></Project>",
    )
    .unwrap();
    fs::create_dir_all(provider.parent().unwrap()).unwrap();
    fs::write(&provider, provider_source).unwrap();
    let main_bytes = fs::read(root.join("Main.pas")).unwrap();
    let provider_bytes = fs::read(&provider).unwrap();

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let environment = tempfile::tempdir().expect("isolated Neovim environment");
    let user_config = environment.path().join("config/delphi-tools/config.toml");
    fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    fs::write(
        &user_config,
        format!(
            "[properties]\nBDS = 'C:\\SDK'\n[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            sdk.display()
        ),
    )
    .unwrap();

    let mut command = Command::new("nvim");
    command
        .args(["--headless", "-u", "NONE", "-l"])
        .arg(manifest.join("tests/neovim_overrides_smoke.lua"))
        .env("PASCAL_LSP_BIN", env!("CARGO_BIN_EXE_pascal-lsp"))
        .env("PASCAL_LSP_SMOKE_ROOT", &root)
        .env("PASCAL_LSP_EXPECTED_PROVIDER", &provider)
        .env("PASCAL_LSP_CONFIG", manifest.join("examples/neovim.lua"));
    configure_neovim_environment(&mut command, &environment);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "Neovim Delphi override smoke timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Neovim Delphi override smoke failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("NEOVIM_DELPHI_OVERRIDES_OK"),
        "Neovim override smoke did not print its marker:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(root.join("Main.pas")).unwrap(), main_bytes);
    assert_eq!(fs::read(&provider).unwrap(), provider_bytes);
}

#[test]
fn neovim_watches_repository_parent_configuration_fallback() {
    if !neovim_is_available() {
        eprintln!("Neovim integration skipped: nvim is not installed");
        return;
    }
    let directory = tempfile::Builder::new()
        .prefix("pascal_lsp_neovim_external_watcher_")
        .tempdir()
        .unwrap();
    let repository = directory.path().join("repository");
    let workspace = repository.join("nested-workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(repository.join(".git"), "gitdir: /outside/worktree\n").unwrap();
    fs::write(
        workspace.join("Main.pas"),
        "unit Main;\ninterface\nconst\n  badConst = 1;\nimplementation\nend.\n",
    )
    .unwrap();

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let environment = tempfile::tempdir().expect("isolated Neovim environment");
    let mut command = Command::new("nvim");
    command
        .args(["--headless", "-u", "NONE", "-l"])
        .arg(manifest.join("tests/neovim_watch_smoke.lua"))
        .env("PASCAL_LSP_BIN", env!("CARGO_BIN_EXE_pascal-lsp"))
        .env("PASCAL_LSP_SMOKE_ROOT", &repository);
    configure_neovim_environment(&mut command, &environment);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "Neovim external watcher smoke timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Neovim external watcher smoke failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("NEOVIM_EXTERNAL_WATCH_OK"));
}

#[test]
fn neovim_standard_symbol_reference_and_highlight_queries() {
    if !neovim_is_available() {
        eprintln!("Neovim integration skipped: nvim is not installed");
        return;
    }
    let directory = tempfile::Builder::new()
        .prefix("pascal lsp neovim queries ")
        .tempdir()
        .unwrap();
    fs::write(directory.path().join(".lint4d.toml"), "").unwrap();
    let provider_source = "unit Provider;\ninterface\ntype\n  TWidget = class\n  private\n    FValue: Integer;\n  public\n    procedure Run;\n    property Value: Integer read FValue;\n  end;\nconst\n  SharedValue = 1;\n  DiskOnly = 2;\nprocedure PublicRoutine;\nimplementation\nprocedure TWidget.Run;\nbegin\n  FValue := 1;\nend;\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";
    let consumer_source = "unit Consumer;\ninterface\nuses Provider;\nprocedure ConsumerOnly;\nimplementation\nprocedure ConsumerOnly;\nbegin\n  Log(SharedValue);\n  Log(Provider.SharedValue);\nend;\nend.\n";
    fs::write(directory.path().join("Provider.pas"), provider_source).unwrap();
    fs::write(directory.path().join("Consumer.pas"), consumer_source).unwrap();

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let environment = tempfile::tempdir().expect("isolated Neovim environment");
    let mut command = Command::new("nvim");
    command
        .args(["--headless", "-u", "NONE", "-l"])
        .arg(manifest.join("tests/neovim_queries_smoke.lua"))
        .env("PASCAL_LSP_BIN", env!("CARGO_BIN_EXE_pascal-lsp"))
        .env("PASCAL_LSP_SMOKE_ROOT", directory.path())
        .env("PASCAL_LSP_CONFIG", manifest.join("examples/neovim.lua"));
    configure_neovim_environment(&mut command, &environment);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "Neovim query smoke timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Neovim query smoke failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("NEOVIM_QUERY_SMOKE_OK"));
    assert_eq!(
        fs::read(directory.path().join("Provider.pas")).unwrap(),
        provider_source.as_bytes()
    );
    assert_eq!(
        fs::read(directory.path().join("Consumer.pas")).unwrap(),
        consumer_source.as_bytes()
    );
}

#[test]
fn neovim_standard_assistance_requests_use_overlays_without_saving() {
    if Command::new("nvim").arg("--version").output().is_err() {
        eprintln!("Neovim integration skipped: nvim is not installed");
        return;
    }
    let directory = tempfile::Builder::new()
        .prefix("pascal lsp neovim assistance ")
        .tempdir()
        .unwrap();
    let provider_source = "unit Provider;\ninterface\ntype\n  TWidget = class\n  private\n    Hidden: Integer;\n  public\n    Member: Integer;\n  end;\nprocedure PublicRoutine(A, B: Integer; C: string; D: Integer); overload;\nprocedure PublicRoutine(A: string); overload;\nimplementation\nprocedure PublicRoutine(A, B: Integer; C: string; D: Integer);\nbegin\nend;\nprocedure PublicRoutine(A: string);\nbegin\nend;\nend.\n";
    let main_source = "unit Main;\ninterface\nuses Provider;\nvar\n  GlobalName: Integer;\nimplementation\nprocedure Other(X, Y: Integer);\nvar\n  LocalName: Integer;\n  Shadowed: Integer;\nbegin\n  LocalName := X;\nend;\nprocedure Caller;\nvar\n  Obj: TWidget;\n  DiskName: Integer;\nbegin\n  Loc;\n  Obj.Me;\n  PublicRoutine(Other(1, 2), 'a,b', [1,2], 4);\nend;\nend.\n";
    fs::write(directory.path().join(".lint4d.toml"), "").unwrap();
    fs::write(directory.path().join("Provider.pas"), provider_source).unwrap();
    fs::write(directory.path().join("Main.pas"), main_source).unwrap();

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut child = Command::new("nvim")
        .args(["--headless", "-u", "NONE", "-l"])
        .arg(manifest.join("tests/neovim_assistance_smoke.lua"))
        .env("PASCAL_LSP_BIN", env!("CARGO_BIN_EXE_pascal-lsp"))
        .env("PASCAL_LSP_SMOKE_ROOT", directory.path())
        .env("PASCAL_LSP_CONFIG", manifest.join("examples/neovim.lua"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "Neovim assistance smoke timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Neovim assistance smoke failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("NEOVIM_ASSISTANCE_OK"));
    assert_eq!(
        fs::read(directory.path().join("Provider.pas")).unwrap(),
        provider_source.as_bytes()
    );
    assert_eq!(
        fs::read(directory.path().join("Main.pas")).unwrap(),
        main_source.as_bytes()
    );
}
