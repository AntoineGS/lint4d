pub mod bds;
pub mod dproj;
mod glob;
pub mod msbuild;
pub use glob::discover_files;

use std::path::PathBuf;

/// Resolve DCU directories using the priority cascade:
/// CLI paths > config paths > MSBuild auto-discovery.
pub fn resolve_dcu_dirs(
    cli_paths: &[PathBuf],
    config_paths: &[String],
    discovered_paths: Vec<PathBuf>,
) -> Vec<PathBuf> {
    if !cli_paths.is_empty() {
        return cli_paths.to_vec();
    }
    if !config_paths.is_empty() {
        return config_paths.iter().map(PathBuf::from).collect();
    }
    discovered_paths
}
