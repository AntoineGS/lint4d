//! Exercises the shipped configuration through a real Neovim client, when installed.
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn neovim_example_navigates_and_tracks_unsaved_documents() {
    if Command::new("nvim").arg("--version").output().is_err() {
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
        "unit Provider;\ninterface\nprocedure Greet;\nimplementation\nprocedure Greet;\nbegin\nend;\nend.\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("Main.pas"),
        "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  Greet;\nend;\nend.\n",
    )
    .unwrap();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut child = Command::new("nvim")
        .args(["--headless", "-u", "NONE", "-l"])
        .arg(manifest.join("tests/neovim_smoke.lua"))
        .env("PASCAL_LSP_BIN", env!("CARGO_BIN_EXE_pascal-lsp"))
        .env("PASCAL_LSP_SMOKE_ROOT", directory.path())
        .env("PASCAL_LSP_CONFIG", manifest.join("examples/neovim.lua"))
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
    assert!(String::from_utf8_lossy(&output.stdout).contains("NEOVIM_LSP_OK"));
}
