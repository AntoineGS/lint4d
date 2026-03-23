use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BdsInfo {
    pub bds_version: &'static str,
    pub product_name: &'static str,
}

/// Known ProjectVersion → BDS version mappings.
/// Order matters: exact matches first, then ranges checked by prefix.
const VERSION_TABLE: &[(&str, BdsInfo)] = &[
    ("19.5", BdsInfo { bds_version: "23.0", product_name: "Delphi 12 Athens" }),
    ("19.4", BdsInfo { bds_version: "22.0", product_name: "Delphi 11 Alexandria" }),
    ("19.3", BdsInfo { bds_version: "21.0", product_name: "Delphi 10.4 Sydney" }),
    ("18.8", BdsInfo { bds_version: "20.0", product_name: "Delphi 10.3 Rio" }),
];

/// Range fallbacks: major version prefix → BDS version.
const VERSION_RANGE_TABLE: &[(&str, BdsInfo)] = &[
    ("18.", BdsInfo { bds_version: "19.0", product_name: "Delphi 10.2 Tokyo" }),
];

/// Map a dproj `<ProjectVersion>` value to BDS installation info.
///
/// Tries exact match first, then prefix-based range match.
pub fn bds_version_for_project(project_version: &str) -> Option<BdsInfo> {
    // Exact match
    for (ver, info) in VERSION_TABLE {
        if *ver == project_version {
            return Some(info.clone());
        }
    }
    // Range/prefix match
    for (prefix, info) in VERSION_RANGE_TABLE {
        if project_version.starts_with(prefix) {
            return Some(info.clone());
        }
    }
    None
}

/// Construct the path to `rsvars.bat` from a BDS root directory.
pub fn rsvars_bat_path(bds_root: &Path) -> PathBuf {
    bds_root.join("bin").join("rsvars.bat")
}

/// Attempt to find the BDS root directory for a given BDS version string.
///
/// Resolution order:
/// 1. Windows registry: `HKCU\Software\Embarcadero\BDS\<ver>\RootDir`
/// 2. Common filesystem paths
///
/// Returns `None` if not found.
pub fn find_bds_root(bds_version: &str) -> Option<PathBuf> {
    if let Some(root) = find_bds_root_from_registry(bds_version) {
        return Some(root);
    }
    find_bds_root_from_filesystem(bds_version)
}

/// Try the Windows registry for the BDS root directory.
#[cfg(target_os = "windows")]
fn find_bds_root_from_registry(bds_version: &str) -> Option<PathBuf> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key_path = format!("Software\\Embarcadero\\BDS\\{}", bds_version);
    let key = hkcu.open_subkey(&key_path).ok()?;
    let root_dir: String = key.get_value("RootDir").ok()?;
    let path = PathBuf::from(root_dir);
    if path.is_dir() {
        Some(path)
    } else {
        None
    }
}

#[cfg(not(target_os = "windows"))]
fn find_bds_root_from_registry(_bds_version: &str) -> Option<PathBuf> {
    None
}

/// Enumerate all BDS registry entries and return the highest version with a
/// valid `rsvars.bat`.
#[cfg(target_os = "windows")]
pub fn find_any_bds_root() -> Option<PathBuf> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let bds_key = hkcu.open_subkey("Software\\Embarcadero\\BDS").ok()?;

    let mut versions: Vec<String> = bds_key
        .enum_keys()
        .filter_map(|k| k.ok())
        .collect();
    // Sort descending so highest version is first.
    versions.sort_by(|a, b| b.cmp(a));

    for ver in &versions {
        let Ok(sub) = bds_key.open_subkey(ver) else { continue };
        let Ok(root_dir) = sub.get_value::<String, _>("RootDir") else { continue };
        let root = PathBuf::from(&root_dir);
        if rsvars_bat_path(&root).is_file() {
            return Some(root);
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
pub fn find_any_bds_root() -> Option<PathBuf> {
    None
}

/// Scan common installation directories for a BDS version.
fn find_bds_root_from_filesystem(bds_version: &str) -> Option<PathBuf> {
    let candidates = [
        format!("C:\\Program Files (x86)\\Embarcadero\\Studio\\{}", bds_version),
        format!("C:\\Program Files\\Embarcadero\\Studio\\{}", bds_version),
    ];
    for candidate in &candidates {
        let path = PathBuf::from(candidate);
        if rsvars_bat_path(&path).is_file() {
            return Some(path);
        }
    }
    None
}

/// Full BDS discovery chain: explicit path → registry (versioned) → registry
/// (any) → filesystem scan.
pub fn discover_bds_root(
    explicit_bds_path: Option<&Path>,
    project_version: Option<&str>,
) -> Option<PathBuf> {
    // 1. Explicit path from CLI/config
    if let Some(path) = explicit_bds_path {
        if rsvars_bat_path(path).is_file() {
            return Some(path.to_path_buf());
        }
        eprintln!(
            "lint4d: warning: --bds-path {} does not contain bin/rsvars.bat",
            path.display()
        );
        return None;
    }

    // 2. Registry lookup using project version
    if let Some(pv) = project_version {
        if let Some(info) = bds_version_for_project(pv) {
            if let Some(root) = find_bds_root(info.bds_version) {
                return Some(root);
            }
        }
    }

    // 3. Registry fallback: any BDS installation
    if let Some(root) = find_any_bds_root() {
        return Some(root);
    }

    // 4. Filesystem scan for common versions
    for (_, info) in VERSION_TABLE {
        if let Some(root) = find_bds_root_from_filesystem(info.bds_version) {
            return Some(root);
        }
    }

    None
}
