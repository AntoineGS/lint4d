#[path = "src/build_fingerprint.rs"]
mod build_fingerprint;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_directory =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_directory
        .parent()
        .and_then(|crates| crates.parent())
        .expect("pascal-cache lives in crates/ below the workspace root")
        .to_path_buf();
    let rustc = env::var_os("RUSTC").expect("RUSTC");
    let output = Command::new(rustc)
        .arg("-V")
        .output()
        .expect("run rustc -V");
    assert!(output.status.success(), "rustc -V failed");
    let rustc_version = String::from_utf8(output.stdout)
        .expect("rustc -V prints UTF-8")
        .trim()
        .to_owned();
    let target = env::var("TARGET").expect("TARGET");

    let digest = build_fingerprint::fingerprint(&workspace_root, &rustc_version, &target)
        .unwrap_or_else(|error| panic!("cache build fingerprint: {error}"));
    for path in build_fingerprint::watched_paths(&workspace_root)
        .unwrap_or_else(|error| panic!("cache build fingerprint: {error}"))
    {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rustc-env=PASCAL_CACHE_RUSTC_VERSION={rustc_version}");
    println!("cargo:rustc-env=PASCAL_CACHE_TARGET={target}");

    let bytes = digest
        .iter()
        .map(|byte| format!("0x{byte:02x}"))
        .collect::<Vec<_>>()
        .join(", ");
    let output =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("build_fingerprint.rs");
    fs::write(output, format!("[{bytes}]\n")).expect("write build fingerprint");
}
