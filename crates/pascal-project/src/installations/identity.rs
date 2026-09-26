use std::path::Path;

use crate::conditional::CompilerVersion;
use crate::installation_config::ProjectConfiguration;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallationEvidence {
    pub compiler_version: Option<CompilerVersion>,
    pub project_version: Option<String>,
    pub bds_root: Option<String>,
    pub conflicting: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallationOrigin {
    Session,
    Configured,
    Metadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallationSelection {
    Legacy,
    Selected {
        id: String,
        origin: InstallationOrigin,
    },
    NeedsChoice {
        candidates: Vec<String>,
    },
    Invalid {
        id: String,
    },
}

/// CompilerVersion values copied from Delphi's documented compiler constants
/// and the repository's checked-in version constants. The installation IDs
/// below are the corresponding copied BDS folder versions (RAD Studio/BDS
/// identity documentation); ProjectVersion remains a separate project-format
/// field and is deliberately not interpreted here.
pub fn compiler_version_for_installation(id: &str) -> Option<CompilerVersion> {
    let version = match id {
        "7.0" => (21, 0),
        "10.0" => (24, 0),
        "23.0" => (36, 0),
        "37.0" => (37, 0),
        _ => return None,
    };
    Some(CompilerVersion::new(version.0, version.1))
}

pub fn select_installation(
    config: &ProjectConfiguration,
    project: &Path,
    evidence: &InstallationEvidence,
    session: Option<&str>,
) -> InstallationSelection {
    let ids = config.installation_ids();
    if ids.is_empty() {
        return InstallationSelection::Legacy;
    }
    if let Some(id) = session {
        return choose_explicit(config, id, InstallationOrigin::Session);
    }
    if let Some(id) = config.configured_installation_for(project) {
        return choose_explicit(config, id, InstallationOrigin::Configured);
    }

    if evidence.conflicting {
        return InstallationSelection::NeedsChoice { candidates: ids };
    }

    let mut constraints = Vec::<Vec<String>>::new();
    if let Some(version) = evidence.compiler_version {
        let matches = ids
            .iter()
            .filter(|id| compiler_version_for_installation(id).as_ref() == Some(&version))
            .cloned()
            .collect::<Vec<_>>();
        if !matches.is_empty() {
            constraints.push(matches);
        }
    }
    if let Some(root) = evidence.bds_root.as_deref() {
        let matches = ids
            .iter()
            .filter(|id| {
                config.profile(id).ok().is_some_and(|profile| {
                    profile
                        .overrides
                        .properties
                        .get("bds")
                        .is_some_and(|configured| paths_equal(configured, root))
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if !matches.is_empty() {
            constraints.push(matches);
        }
    }

    if constraints.is_empty() {
        return InstallationSelection::NeedsChoice { candidates: ids };
    }
    let candidates = ids
        .iter()
        .filter(|id| constraints.iter().all(|constraint| constraint.contains(id)))
        .cloned()
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return InstallationSelection::NeedsChoice { candidates: ids };
    }
    match candidates.as_slice() {
        [id] => InstallationSelection::Selected {
            id: id.clone(),
            origin: InstallationOrigin::Metadata,
        },
        _ => InstallationSelection::NeedsChoice { candidates },
    }
}

fn choose_explicit(
    config: &ProjectConfiguration,
    id: &str,
    origin: InstallationOrigin,
) -> InstallationSelection {
    match config
        .installation_ids()
        .into_iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(id))
    {
        Some(id) => InstallationSelection::Selected { id, origin },
        None => InstallationSelection::Invalid { id: id.to_owned() },
    }
}

fn paths_equal(left: &str, right: &str) -> bool {
    let normalize = |value: &str| {
        value
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_ascii_lowercase()
    };
    normalize(left) == normalize(right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conditional::CompilerVersion;
    use crate::delphi_overrides::OverrideSession;
    use crate::installation_config::ProjectConfiguration;
    use std::fs;
    use std::path::Path;

    fn config_with_profiles(contents: &str) -> (tempfile::TempDir, ProjectConfiguration) {
        let directory = tempfile::tempdir().unwrap();
        let config_file = directory.path().join("config.toml");
        fs::write(&config_file, contents).unwrap();
        let config = OverrideSession::new(Some(config_file))
            .configuration_for(None, None)
            .unwrap();
        (directory, config)
    }

    #[test]
    fn compiler_version_is_not_the_installation_or_project_format_version() {
        let (_directory, config) =
            config_with_profiles("[installations.\"23.0\"]\n[installations.\"37.0\"]\n");
        assert_eq!(
            compiler_version_for_installation("7.0"),
            Some(CompilerVersion::new(21, 0))
        );
        assert_eq!(
            compiler_version_for_installation("10.0"),
            Some(CompilerVersion::new(24, 0))
        );
        assert_eq!(
            compiler_version_for_installation("23.0"),
            Some(CompilerVersion::new(36, 0))
        );
        assert_eq!(
            compiler_version_for_installation("37.0"),
            Some(CompilerVersion::new(37, 0))
        );
        assert_eq!(compiler_version_for_installation("custom"), None);

        let project = Path::new("/tmp/App.dproj");
        assert_eq!(
            select_installation(
                &config,
                project,
                &InstallationEvidence {
                    project_version: Some("20.3".into()),
                    ..Default::default()
                },
                None,
            ),
            InstallationSelection::NeedsChoice {
                candidates: vec!["23.0".into(), "37.0".into()]
            },
        );
    }

    #[test]
    fn session_and_configured_selections_precede_metadata_and_validate_ids() {
        let (_directory, config) = config_with_profiles(
            "[installations.\"7.0\"]\n[installations.\"10.0\"]\n[projects.\"/tmp/App.dproj\"]\ninstallation = '7.0'\n",
        );
        let project = Path::new("/tmp/App.dproj");
        let evidence = InstallationEvidence {
            compiler_version: Some(CompilerVersion::new(24, 0)),
            ..Default::default()
        };
        assert_eq!(
            select_installation(&config, project, &evidence, Some("10.0")),
            InstallationSelection::Selected {
                id: "10.0".into(),
                origin: InstallationOrigin::Session
            }
        );
        assert_eq!(
            select_installation(&config, project, &evidence, Some("removed")),
            InstallationSelection::Invalid {
                id: "removed".into()
            }
        );
        assert_eq!(
            select_installation(&config, project, &evidence, None),
            InstallationSelection::Selected {
                id: "7.0".into(),
                origin: InstallationOrigin::Configured
            }
        );
    }

    #[test]
    fn explicit_compiler_evidence_selects_and_conflicts_require_choice() {
        let (_directory, config) = config_with_profiles(
            "[installations.\"7.0\"]\n[installations.\"10.0\"]\n[installations.\"23.0\"]\n[installations.\"37.0\"]\n",
        );
        let project = Path::new("/tmp/Other.dproj");
        for (version, id) in [(21, "7.0"), (24, "10.0"), (36, "23.0"), (37, "37.0")] {
            assert_eq!(
                select_installation(
                    &config,
                    project,
                    &InstallationEvidence {
                        compiler_version: Some(CompilerVersion::new(version, 0)),
                        ..InstallationEvidence::default()
                    },
                    None,
                ),
                InstallationSelection::Selected {
                    id: id.into(),
                    origin: InstallationOrigin::Metadata,
                }
            );
        }
        assert_eq!(
            select_installation(
                &config,
                project,
                &InstallationEvidence {
                    compiler_version: Some(CompilerVersion::new(24, 0)),
                    ..Default::default()
                },
                None
            ),
            InstallationSelection::Selected {
                id: "10.0".into(),
                origin: InstallationOrigin::Metadata
            },
        );
        assert_eq!(
            select_installation(
                &config,
                project,
                &InstallationEvidence {
                    compiler_version: Some(CompilerVersion::new(24, 0)),
                    conflicting: true,
                    ..Default::default()
                },
                None
            ),
            InstallationSelection::NeedsChoice {
                candidates: vec!["10.0".into(), "23.0".into(), "37.0".into(), "7.0".into()]
            },
        );
    }

    #[test]
    fn unknown_metadata_prompts_even_when_exactly_one_profile_exists() {
        let (_directory, config) = config_with_profiles("[installations.\"37.0\"]\n");
        assert_eq!(
            select_installation(
                &config,
                Path::new("/tmp/App.dproj"),
                &InstallationEvidence::default(),
                None,
            ),
            InstallationSelection::NeedsChoice {
                candidates: vec!["37.0".into()]
            }
        );
    }
}
