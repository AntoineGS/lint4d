//! Explicit, filename-only browsing of Delphi project files.

use super::{Workspace, absolute_path, check_workspace_cancel, path_starts_with_native};
use lsp_types::Url;
use serde::Serialize;
use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

const MAX_CATALOGUE_VISITS: usize = 10_000;
const MAX_CATALOGUE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CATALOGUE_TIME: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCatalogueItem {
    pub project_uri: Url,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCatalogue {
    pub projects: Vec<ProjectCatalogueItem>,
    pub complete: bool,
    pub warnings: Vec<String>,
}

impl Workspace {
    /// Enumerate `.dproj` filenames under the workspace root containing `anchor`.
    /// Project files are not opened; a listed candidate must be revalidated when selected.
    pub fn list_projects(
        &self,
        anchor: &Url,
        cancel: &AtomicBool,
    ) -> Result<ProjectCatalogue, String> {
        self.list_projects_with_limits(
            anchor,
            cancel,
            MAX_CATALOGUE_VISITS,
            MAX_CATALOGUE_BYTES,
            MAX_CATALOGUE_TIME,
        )
    }

    fn list_projects_with_limits(
        &self,
        anchor: &Url,
        cancel: &AtomicBool,
        visit_limit: usize,
        byte_limit: usize,
        time_limit: Duration,
    ) -> Result<ProjectCatalogue, String> {
        check_workspace_cancel(Some(cancel))?;
        let anchor_path = anchor
            .to_file_path()
            .map_err(|_| format!("project catalogue requires a file URI: {anchor}"))?;
        let anchor_path = fs::canonicalize(absolute_path(anchor_path))
            .map_err(|error| format!("project catalogue anchor is unavailable: {error}"))?;
        let (root, root_path) = self
            .roots
            .iter()
            .filter_map(|root| {
                fs::canonicalize(&root.path)
                    .ok()
                    .filter(|physical_root| path_starts_with_native(&anchor_path, physical_root))
                    .map(|physical_root| (root, physical_root))
            })
            .max_by_key(|(_, physical_root)| physical_root.components().count())
            .ok_or_else(|| {
                format!(
                    "project catalogue anchor is outside configured workspace roots: {}",
                    anchor_path.display()
                )
            })?;

        let started = Instant::now();
        let visited = Cell::new(0usize);
        let filter_cancelled = Cell::new(false);
        let filter_stop = Cell::new(None);
        let mut bytes = 0usize;
        let mut projects = Vec::new();
        let mut warnings = Vec::new();
        let mut complete = true;
        let walker = WalkDir::new(&root_path)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                visited.set(visited.get().saturating_add(1));
                if check_workspace_cancel(Some(cancel)).is_err() {
                    filter_cancelled.set(true);
                    // Let `next` return so the outer loop can stop immediately.
                    return true;
                }
                if started.elapsed() >= time_limit {
                    filter_stop.set(Some("time"));
                    return true;
                }
                if visited.get() > visit_limit {
                    filter_stop.set(Some("entry"));
                    return true;
                }
                if entry.depth() == 0 {
                    return true;
                }
                let path = entry.path();
                !entry.file_type().is_symlink() && !root.excludes.is_excluded(path, &root_path)
            });

        let mut walker = walker;
        loop {
            check_workspace_cancel(Some(cancel))?;
            if started.elapsed() >= time_limit {
                complete = false;
                warnings.push("project catalogue time limit reached".to_owned());
                break;
            }
            let Some(entry) = walker.next() else {
                break;
            };
            if filter_cancelled.get() {
                return Err("project catalogue request cancelled".to_owned());
            }
            if let Some(reason) = filter_stop.replace(None) {
                complete = false;
                warnings.push(match reason {
                    "time" => "project catalogue time limit reached".to_owned(),
                    _ => "project catalogue entry limit reached".to_owned(),
                });
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    visited.set(visited.get().saturating_add(1));
                    if check_workspace_cancel(Some(cancel)).is_err() {
                        return Err("project catalogue request cancelled".to_owned());
                    }
                    if started.elapsed() >= time_limit {
                        complete = false;
                        warnings.push("project catalogue time limit reached".to_owned());
                        break;
                    }
                    if visited.get() > visit_limit {
                        complete = false;
                        warnings.push("project catalogue entry limit reached".to_owned());
                        break;
                    }
                    complete = false;
                    if warnings.is_empty() {
                        warnings.push("project catalogue could not visit some entries".to_owned());
                    }
                    continue;
                }
            };
            if started.elapsed() >= time_limit {
                complete = false;
                warnings.push("project catalogue time limit reached".to_owned());
                break;
            }
            if !entry.file_type().is_file()
                || !entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("dproj"))
            {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&root_path)
                .unwrap_or(entry.path());
            let label = relative_label(relative);
            let project_uri = Url::from_file_path(entry.path()).map_err(|_| {
                format!(
                    "could not convert project path to URI: {}",
                    entry.path().display()
                )
            })?;
            let retained = label.len().saturating_add(project_uri.as_str().len());
            if bytes.saturating_add(retained) > byte_limit {
                complete = false;
                warnings.push("project catalogue retained-byte limit reached".to_owned());
                break;
            }
            bytes += retained;
            projects.push(ProjectCatalogueItem { project_uri, label });
        }
        check_workspace_cancel(Some(cancel))?;
        projects.sort_by(|left, right| left.label.cmp(&right.label));
        Ok(ProjectCatalogue {
            projects,
            complete,
            warnings,
        })
    }
}

fn relative_label(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(label_component(part)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(unix)]
fn label_component(component: &std::ffi::OsStr) -> String {
    use std::os::unix::ffi::OsStrExt;

    fn escape_valid(text: &str, output: &mut String) {
        output.push_str(&text.replace('%', "%25").replace('\\', "%5C"));
    }

    let mut remaining = component.as_bytes();
    let mut output = String::new();
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(text) => {
                escape_valid(text, &mut output);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    // `valid_up_to` is guaranteed to identify a UTF-8 boundary.
                    escape_valid(
                        std::str::from_utf8(&remaining[..valid]).unwrap(),
                        &mut output,
                    );
                }
                let invalid = error.error_len().unwrap_or(remaining.len() - valid).max(1);
                for byte in &remaining[valid..valid + invalid] {
                    output.push_str(&format!("%{byte:02X}"));
                }
                remaining = &remaining[valid + invalid..];
            }
        }
    }
    output
}

#[cfg(windows)]
fn label_component(component: &std::ffi::OsStr) -> String {
    use std::os::windows::ffi::OsStrExt;

    let wide = component.encode_wide().collect::<Vec<_>>();
    if let Some(text) = component.to_str() {
        return text.replace('%', "%25");
    }
    wide.iter().map(|unit| format!("%u{unit:04X}")).collect()
}

// Targets other than Unix and Windows do not expose a lossless OsStr encoding
// suitable for a portable label. Keep a readable fallback; collision-free
// labels on those targets are deferred until their native encoding is handled.
#[cfg(not(any(unix, windows)))]
fn label_component(component: &std::ffi::OsStr) -> String {
    component.to_string_lossy().replace('%', "%25")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::WorkspaceOptions;
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn write(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"not read by catalogue").unwrap();
    }

    #[test]
    fn lists_projects_by_distinct_relative_path_and_honors_exclusions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("apps/Two/App.dproj"));
        write(&root.join("apps/One/App.dproj"));
        write(&root.join("ignored/App.dproj"));
        let options = WorkspaceOptions {
            exclude: vec!["ignored/**".to_owned()],
            ..WorkspaceOptions::default()
        };
        let workspace = Workspace::new(vec![root.to_path_buf()], options);
        let catalogue = workspace
            .list_projects(&Url::from_file_path(root).unwrap(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(
            catalogue
                .projects
                .iter()
                .map(|p| p.label.as_str())
                .collect::<Vec<_>>(),
            vec!["apps/One/App.dproj", "apps/Two/App.dproj"]
        );
        assert!(catalogue.complete);
    }

    #[test]
    fn excluded_siblings_are_charged_before_filtering_can_hide_them() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let ignored = root.join("ignored");
        fs::create_dir_all(&ignored).unwrap();
        for index in 0..10_100 {
            fs::write(ignored.join(format!("entry-{index:05}.skip")), b"").unwrap();
        }
        write(&root.join("App.dproj"));
        let workspace = Workspace::new(
            vec![root.to_path_buf()],
            WorkspaceOptions {
                exclude: vec!["ignored/*.skip".to_owned()],
                ..WorkspaceOptions::default()
            },
        );
        let catalogue = workspace
            .list_projects_with_limits(
                &Url::from_file_path(root).unwrap(),
                &AtomicBool::new(false),
                32,
                1024,
                Duration::from_secs(10),
            )
            .unwrap();
        assert!(
            !catalogue.complete,
            "filtered entries still consume the visit budget"
        );
        assert!(
            catalogue.projects.len() <= 1,
            "retain only visited projects"
        );
        assert!(
            catalogue
                .warnings
                .iter()
                .any(|warning| warning.contains("entry limit"))
        );
    }

    #[test]
    fn cancellation_is_checked_while_filtering_excluded_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let ignored = root.join("ignored");
        fs::create_dir_all(&ignored).unwrap();
        for index in 0..20_000 {
            fs::write(ignored.join(format!("entry-{index:05}.skip")), b"").unwrap();
        }
        let workspace = Workspace::new(
            vec![root.to_path_buf()],
            WorkspaceOptions {
                exclude: vec!["ignored/*.skip".to_owned()],
                ..WorkspaceOptions::default()
            },
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_after_delay = Arc::clone(&cancel);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1));
            cancel_after_delay.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let result = workspace.list_projects_with_limits(
            &Url::from_file_path(root).unwrap(),
            &cancel,
            100_000,
            1024,
            Duration::from_secs(10),
        );
        canceller.join().unwrap();
        assert!(result.unwrap_err().contains("cancel"));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn cancellation_is_an_explicit_error() {
        let temp = tempfile::tempdir().unwrap();
        let workspace =
            Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
        let cancel = AtomicBool::new(true);
        assert!(
            workspace
                .list_projects(&Url::from_file_path(temp.path()).unwrap(), &cancel)
                .unwrap_err()
                .contains("cancel")
        );
    }

    #[test]
    fn dproj_is_not_an_analyzable_document_path() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("App.dproj");
        write(&project);
        let mut workspace =
            Workspace::new(vec![temp.path().to_path_buf()], WorkspaceOptions::default());
        assert!(
            workspace
                .context_for_uri(&Url::from_file_path(project).unwrap())
                .is_err()
        );
    }

    #[test]
    fn anchor_must_be_existing_and_physically_inside_the_authorized_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("root-sibling");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let workspace = Workspace::new(vec![root.clone()], WorkspaceOptions::default());
        let missing = root.join("missing.txt");
        assert!(
            workspace
                .list_projects(
                    &Url::from_file_path(missing).unwrap(),
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        assert!(
            workspace
                .list_projects(
                    &Url::from_file_path(&outside).unwrap(),
                    &AtomicBool::new(false)
                )
                .unwrap_err()
                .contains("outside")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_anchor_cannot_escape_the_authorized_root() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        write(&outside.join("outside.dproj"));
        symlink(&outside, root.join("escape")).unwrap();
        let workspace = Workspace::new(vec![root.clone()], WorkspaceOptions::default());
        assert!(
            workspace
                .list_projects(
                    &Url::from_file_path(root.join("escape/outside.dproj")).unwrap(),
                    &AtomicBool::new(false)
                )
                .unwrap_err()
                .contains("outside")
        );
    }

    // macOS filesystems reject file names that are not valid UTF-8.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn labels_preserve_unix_component_identity() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("apps/One/App.dproj"));
        write(&root.join("apps/One\\App.dproj"));
        write(&root.join(OsString::from_vec(b"bad\xff.dproj".to_vec())));
        let workspace = Workspace::new(vec![root.to_path_buf()], WorkspaceOptions::default());
        let catalogue = workspace
            .list_projects(&Url::from_file_path(root).unwrap(), &AtomicBool::new(false))
            .unwrap();
        let labels = catalogue
            .projects
            .iter()
            .map(|project| project.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels.len(), 3);
        assert_eq!(
            labels,
            vec!["apps/One%5CApp.dproj", "apps/One/App.dproj", "bad%FF.dproj"]
        );
        let uris = catalogue
            .projects
            .iter()
            .map(|project| project.project_uri.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(uris.len(), 3);
    }

    #[test]
    fn reaching_visit_limit_is_complete_only_if_iterator_is_exhausted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("One.dproj"));
        write(&root.join("Two.dproj"));
        let workspace = Workspace::new(vec![root.to_path_buf()], WorkspaceOptions::default());
        let anchor = Url::from_file_path(root).unwrap();
        let exact = workspace
            .list_projects_with_limits(
                &anchor,
                &AtomicBool::new(false),
                3,
                1024,
                Duration::from_secs(30),
            )
            .unwrap();
        assert!(
            exact.complete,
            "root + two files exhausts exactly three entries"
        );
        let truncated = workspace
            .list_projects_with_limits(
                &anchor,
                &AtomicBool::new(false),
                2,
                1024,
                Duration::from_secs(30),
            )
            .unwrap();
        assert!(!truncated.complete);
        assert!(
            truncated
                .warnings
                .iter()
                .any(|warning| warning.contains("entry limit"))
        );
    }

    #[test]
    fn huge_single_directory_observes_time_limit_without_sorting_every_sibling() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for index in 0..50_000 {
            fs::write(root.join(format!("entry-{index:05}.txt")), b"").unwrap();
        }
        let workspace = Workspace::new(vec![root.to_path_buf()], WorkspaceOptions::default());
        let started = Instant::now();
        let catalogue = workspace
            .list_projects_with_limits(
                &Url::from_file_path(root).unwrap(),
                &AtomicBool::new(false),
                8,
                1024,
                Duration::from_millis(1),
            )
            .unwrap();
        assert!(!catalogue.complete);
        assert!(catalogue.projects.len() <= 8);
        assert!(
            catalogue.warnings.iter().any(|warning| {
                warning.contains("time limit") || warning.contains("entry limit")
            })
        );
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "bounded catalogue traversal took {:?} despite its 1ms work deadline",
            started.elapsed()
        );
    }

    #[test]
    fn cancellation_interrupts_traversal_of_a_large_single_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for index in 0..20_000 {
            fs::write(root.join(format!("entry-{index:05}.txt")), b"").unwrap();
        }
        let workspace = Workspace::new(vec![root.to_path_buf()], WorkspaceOptions::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_after_delay = Arc::clone(&cancel);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2));
            cancel_after_delay.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let result = workspace.list_projects_with_limits(
            &Url::from_file_path(root).unwrap(),
            &cancel,
            100_000,
            1024,
            Duration::from_secs(10),
        );
        canceller.join().unwrap();
        assert!(result.unwrap_err().contains("cancel"));
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
