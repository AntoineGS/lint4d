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
