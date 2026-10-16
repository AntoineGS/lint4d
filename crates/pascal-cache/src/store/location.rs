use super::outcome::{Bypass, LocationIssue};
use crate::Digest;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const DATABASE_FILE: &str = "analysis.sqlite3";
const NAMESPACE_LOCKS: &str = "namespaces";

/// The default cache directory: the platform user cache directory plus
/// `lint4d/analysis` (spec §11.1).
pub fn default_directory() -> Result<PathBuf, Bypass> {
    default_directory_from(|name| std::env::var_os(name))
}

fn default_directory_from(var: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, Bypass> {
    let absolute = |name: &str| {
        var(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let base = if cfg!(windows) {
        absolute("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        absolute("HOME").map(|home| home.join("Library").join("Caches"))
    } else {
        absolute("XDG_CACHE_HOME").or_else(|| absolute("HOME").map(|home| home.join(".cache")))
    };
    base.map(|base| base.join("lint4d").join("analysis"))
        .ok_or(Bypass::Location(LocationIssue::NoDefaultDirectory))
}

/// A cache directory that is absolute, owned by this user, private, on a
/// local filesystem and not a symlink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLocation {
    directory: PathBuf,
}

impl StoreLocation {
    /// Create `directory` and its missing parents (mode `0700`), then check
    /// it. An existing directory is checked, never modified.
    pub fn prepare(directory: &Path) -> Result<Self, Bypass> {
        if !directory.is_absolute() {
            return Err(Bypass::Location(LocationIssue::NotAbsolute));
        }
        platform::prepare(directory)?;
        Ok(Self {
            directory: directory.to_path_buf(),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn database(&self) -> PathBuf {
        self.directory.join(DATABASE_FILE)
    }

    /// The database and the WAL and shared-memory files SQLite keeps beside it.
    pub fn database_files(&self) -> [PathBuf; 3] {
        [
            self.database(),
            self.directory.join(format!("{DATABASE_FILE}-wal")),
            self.directory.join(format!("{DATABASE_FILE}-shm")),
        ]
    }

    /// The lock file a process holds shared while it uses `namespace`.
    pub fn namespace_lock(&self, namespace: Digest) -> PathBuf {
        self.directory
            .join(NAMESPACE_LOCKS)
            .join(format!("{}.lock", namespace.to_hex()))
    }

    pub fn prepare_namespace_locks(&self) -> Result<(), Bypass> {
        platform::prepare_subdirectory(&self.directory.join(NAMESPACE_LOCKS))
    }

    /// Accept a store file that is absent, or a regular file of this user.
    pub fn check_file(path: &Path) -> Result<(), Bypass> {
        platform::check_file(path)
    }
}

#[cfg(unix)]
fn check_directory_metadata(
    is_symlink: bool,
    is_dir: bool,
    owner: u32,
    mode: u32,
    user: u32,
) -> Result<(), LocationIssue> {
    if is_symlink {
        return Err(LocationIssue::Symlink);
    }
    if !is_dir {
        return Err(LocationIssue::NotADirectory);
    }
    if owner != user {
        return Err(LocationIssue::NotOwned);
    }
    if mode & 0o077 != 0 {
        return Err(LocationIssue::NotPrivate {
            mode: mode & 0o7777,
        });
    }
    Ok(())
}

/// Filesystems known to be local disks or memory, by `statfs` magic.
/// Anything else, including NFS, SMB, FUSE and 9p, is not local.
#[cfg(target_os = "linux")]
const LOCAL_LINUX_FILESYSTEMS: &[u64] = &[
    0xEF53,      // ext2/ext3/ext4
    0x5846_5342, // xfs
    0x9123_683E, // btrfs
    0x0102_1994, // tmpfs
    0x8584_58F6, // ramfs
    0xF2F5_2010, // f2fs
    0x2FC1_2FC1, // zfs
    0xCA45_1A4E, // bcachefs
    0x794C_7630, // overlayfs
    0x3153_464A, // jfs
    0x5265_4973, // reiserfs
    0x3434,      // nilfs2
    0xF15F,      // ecryptfs
];

#[cfg(target_os = "linux")]
fn linux_filesystem_is_local(magic: u64) -> bool {
    LOCAL_LINUX_FILESYSTEMS.contains(&magic)
}

#[cfg(unix)]
mod platform {
    use super::{Bypass, LocationIssue, check_directory_metadata};
    use std::fs::{self, DirBuilder};
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::path::Path;

    fn effective_uid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    pub(super) fn prepare(directory: &Path) -> Result<(), Bypass> {
        if let Err(error) = DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
        {
            check_directory(directory)?;
            return Err(Bypass::Io(error.kind()));
        }
        check_directory(directory)?;
        check_locality(directory)
    }

    pub(super) fn prepare_subdirectory(directory: &Path) -> Result<(), Bypass> {
        match DirBuilder::new().mode(0o700).create(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(Bypass::Io(error.kind())),
        }
        check_directory(directory)
    }

    fn check_directory(directory: &Path) -> Result<(), Bypass> {
        let metadata = fs::symlink_metadata(directory).map_err(|error| Bypass::Io(error.kind()))?;
        check_directory_metadata(
            metadata.file_type().is_symlink(),
            metadata.is_dir(),
            metadata.uid(),
            metadata.mode(),
            effective_uid(),
        )
        .map_err(Bypass::Location)
    }

    pub(super) fn check_file(path: &Path) -> Result<(), Bypass> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Bypass::Io(error.kind())),
        };
        if metadata.file_type().is_symlink() {
            return Err(Bypass::Location(LocationIssue::Symlink));
        }
        if !metadata.is_file() {
            return Err(Bypass::Location(LocationIssue::FileNotRegular));
        }
        if metadata.uid() != effective_uid() {
            return Err(Bypass::Location(LocationIssue::NotOwned));
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn statfs(directory: &Path) -> Result<libc::statfs, Bypass> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let path = CString::new(directory.as_os_str().as_bytes())
            .map_err(|_| Bypass::Io(io::ErrorKind::InvalidInput))?;
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: `path` is NUL-terminated and `stat` is a valid out-pointer.
        if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(Bypass::Io(io::Error::last_os_error().kind()));
        }
        // SAFETY: statfs returned 0, so it initialized `stat`.
        Ok(unsafe { stat.assume_init() })
    }

    #[cfg(target_os = "linux")]
    #[allow(clippy::unnecessary_cast)]
    fn check_locality(directory: &Path) -> Result<(), Bypass> {
        let magic = statfs(directory)?.f_type as u64 & 0xFFFF_FFFF;
        if super::linux_filesystem_is_local(magic) {
            Ok(())
        } else {
            Err(Bypass::Location(LocationIssue::NotLocal {
                filesystem: magic,
            }))
        }
    }

    #[cfg(target_os = "macos")]
    #[allow(clippy::unnecessary_cast)]
    fn check_locality(directory: &Path) -> Result<(), Bypass> {
        let stat = statfs(directory)?;
        if stat.f_flags as u64 & libc::MNT_LOCAL as u64 != 0 {
            Ok(())
        } else {
            Err(Bypass::Location(LocationIssue::NotLocal {
                filesystem: stat.f_type as u64,
            }))
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn check_locality(_: &Path) -> Result<(), Bypass> {
        Err(Bypass::Location(
            LocationIssue::PlatformChecksNotImplemented,
        ))
    }
}

#[cfg(not(unix))]
mod platform {
    use super::{Bypass, LocationIssue};
    use std::path::Path;

    const UNSUPPORTED: Bypass = Bypass::Location(LocationIssue::PlatformChecksNotImplemented);

    pub(super) fn prepare(_: &Path) -> Result<(), Bypass> {
        Err(UNSUPPORTED)
    }

    pub(super) fn prepare_subdirectory(_: &Path) -> Result<(), Bypass> {
        Err(UNSUPPORTED)
    }

    pub(super) fn check_file(_: &Path) -> Result<(), Bypass> {
        Err(UNSUPPORTED)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn default_directory_follows_xdg_then_home() {
        assert_eq!(
            default_directory_from(env(&[("XDG_CACHE_HOME", "/x/cache"), ("HOME", "/h")])),
            Ok(PathBuf::from("/x/cache/lint4d/analysis"))
        );
        // A relative XDG_CACHE_HOME is invalid by the XDG spec and ignored.
        assert_eq!(
            default_directory_from(env(&[("XDG_CACHE_HOME", "cache"), ("HOME", "/h")])),
            Ok(PathBuf::from("/h/.cache/lint4d/analysis"))
        );
        assert_eq!(
            default_directory_from(env(&[])),
            Err(Bypass::Location(LocationIssue::NoDefaultDirectory))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn default_directory_uses_library_caches() {
        assert_eq!(
            default_directory_from(env(&[("HOME", "/Users/u")])),
            Ok(PathBuf::from("/Users/u/Library/Caches/lint4d/analysis"))
        );
    }

    #[test]
    fn prepare_creates_a_private_directory() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("a/b/analysis");
        let location = StoreLocation::prepare(&directory).unwrap();
        assert_eq!(location.directory(), directory);
        assert_eq!(location.database(), directory.join(DATABASE_FILE));
        let mode = fs::metadata(&directory).unwrap().mode();
        assert_eq!(mode & 0o777, 0o700);
        // Preparing again accepts the existing directory.
        assert!(StoreLocation::prepare(&directory).is_ok());
    }

    #[test]
    fn relative_directories_are_rejected() {
        assert_eq!(
            StoreLocation::prepare(Path::new("cache")),
            Err(Bypass::Location(LocationIssue::NotAbsolute))
        );
    }

    #[test]
    fn a_file_is_not_a_directory() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert_eq!(
            StoreLocation::prepare(&file),
            Err(Bypass::Location(LocationIssue::NotADirectory))
        );
    }

    #[test]
    fn a_symlinked_directory_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.path().join("link");
        symlink(&target, &link).unwrap();
        assert_eq!(
            StoreLocation::prepare(&link),
            Err(Bypass::Location(LocationIssue::Symlink))
        );
    }

    #[test]
    fn a_shared_directory_is_not_private() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("shared");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            StoreLocation::prepare(&directory),
            Err(Bypass::Location(LocationIssue::NotPrivate { mode: 0o755 }))
        );
    }

    #[test]
    fn ownership_and_kind_are_checked_from_metadata() {
        assert_eq!(check_directory_metadata(false, true, 7, 0o40700, 7), Ok(()));
        assert_eq!(
            check_directory_metadata(false, true, 8, 0o40700, 7),
            Err(LocationIssue::NotOwned)
        );
        assert_eq!(
            check_directory_metadata(true, false, 7, 0o120777, 7),
            Err(LocationIssue::Symlink)
        );
        assert_eq!(
            check_directory_metadata(false, false, 7, 0o100600, 7),
            Err(LocationIssue::NotADirectory)
        );
        assert_eq!(
            check_directory_metadata(false, true, 7, 0o40770, 7),
            Err(LocationIssue::NotPrivate { mode: 0o770 })
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn local_linux_filesystems_are_recognized() {
        for magic in [0xEF53, 0x5846_5342, 0x9123_683E, 0x0102_1994, 0x2FC1_2FC1] {
            assert!(linux_filesystem_is_local(magic), "{magic:#x}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn network_and_fuse_filesystems_are_not_local() {
        // NFS, SMB, CIFS, SMB2, FUSE (sshfs, ntfs-3g), 9p (WSL /mnt/c), Ceph, AFS.
        for magic in [
            0x6969,
            0x517B,
            0xFF53_4D42,
            0xFE53_4D42,
            0x6573_5546,
            0x0102_1997,
            0x00C3_6400,
            0x5346_414F,
        ] {
            assert!(!linux_filesystem_is_local(magic), "{magic:#x}");
        }
    }

    #[test]
    fn database_files_must_be_regular_files_of_this_user() {
        let root = tempfile::tempdir().unwrap();
        let absent = root.path().join("absent");
        assert_eq!(StoreLocation::check_file(&absent), Ok(()));
        let file = root.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert_eq!(StoreLocation::check_file(&file), Ok(()));
        let link = root.path().join("link");
        symlink(&file, &link).unwrap();
        assert_eq!(
            StoreLocation::check_file(&link),
            Err(Bypass::Location(LocationIssue::Symlink))
        );
        let dangling = root.path().join("dangling");
        symlink(root.path().join("nowhere"), &dangling).unwrap();
        assert_eq!(
            StoreLocation::check_file(&dangling),
            Err(Bypass::Location(LocationIssue::Symlink))
        );
        assert_eq!(
            StoreLocation::check_file(root.path()),
            Err(Bypass::Location(LocationIssue::FileNotRegular))
        );
    }

    #[test]
    fn namespace_locks_live_in_a_private_subdirectory() {
        let root = tempfile::tempdir().unwrap();
        let location = StoreLocation::prepare(&root.path().join("cache")).unwrap();
        location.prepare_namespace_locks().unwrap();
        let digest = crate::Digest::from_bytes([0xab; 32]);
        let lock = location.namespace_lock(digest);
        assert_eq!(lock.parent().unwrap(), root.path().join("cache/namespaces"));
        assert_eq!(
            lock.file_name().unwrap(),
            format!("{}.lock", digest.to_hex()).as_str()
        );
        let mode = fs::metadata(lock.parent().unwrap()).unwrap().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(location.prepare_namespace_locks().is_ok());
        assert_eq!(
            location.database_files(),
            [
                root.path().join("cache/analysis.sqlite3"),
                root.path().join("cache/analysis.sqlite3-wal"),
                root.path().join("cache/analysis.sqlite3-shm"),
            ]
        );
    }
}
