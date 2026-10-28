//! Helpers shared by the store's integration tests.

/// `tempfile::tempdir` honours the umask; the store requires a private root.
pub fn private_tempdir() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        root.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    root
}

/// Member record and snapshot fixtures; not every test file uses them all.
#[allow(dead_code)]
pub mod records;
