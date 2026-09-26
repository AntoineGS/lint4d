#[cfg(test)]
mod tests {
    use super::super::{evaluate_ide_paths, load_installation};
    use crate::delphi_overrides::EffectiveOverrides;
    use crate::installation_config::ResolvedInstallation;
    use crate::{ProjectReadTracker, ProjectWorkBudget, ReadPolicy};
    use std::fs;
    use std::path::Path;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn ide_paths_preserve_platform_order_and_relocate_original_bdslib() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let ide = temp.path().join("ide");
        write(&sdk.join("bin/rsvars.bat"), "SET BDS=C:\\Original\r\n");
        write(
            &ide.join("environment.proj"),
            "<Project><PropertyGroup><BDSLIB>C:\\Original\\lib</BDSLIB></PropertyGroup></Project>",
        );
        write(
            &ide.join("EnvOptions.proj"),
            r#"<Project>
  <PropertyGroup Condition="'$(Platform)'=='Win32'"><DelphiLibraryPath>$(BDS)\wrong-platform</DelphiLibraryPath></PropertyGroup>
  <PropertyGroup Condition="'$(Platform)'=='Linux64'"><DelphiLibraryPath>$(BDSLIB)\base</DelphiLibraryPath><DelphiBrowsingPath>$(BDS)\source\rtl</DelphiBrowsingPath></PropertyGroup>
  <PropertyGroup Condition="'$(Platform)'=='Linux64'"><DelphiLibraryPath>$(BDS)\addon;$(DelphiLibraryPath)</DelphiLibraryPath></PropertyGroup>
</Project>"#,
        );

        let overrides = EffectiveOverrides {
            properties: [
                ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                ("appdata".to_owned(), ide.to_string_lossy().into_owned()),
            ]
            .into_iter()
            .collect(),
            ..EffectiveOverrides::default()
        };
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides,
        };
        let roots = [sdk.clone(), ide.clone()];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();

        let environment = load_installation(&profile, "Debug", "Linux64", &mut tracker, &policy)
            .expect("installation environment loads");
        let paths = evaluate_ide_paths(
            &environment,
            &profile,
            "Debug",
            "Linux64",
            &mut tracker,
            &policy,
        )
        .expect("IDE paths evaluate");

        assert_eq!(
            paths
                .library
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            [sdk.join("addon"), sdk.join("lib/base")]
        );
        assert!(paths.library.iter().all(|entry| {
            matches!(&entry.provenance, crate::ProjectPathProvenance::Configured)
        }));
        assert_eq!(
            paths
                .browsing
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            [sdk.join("source/rtl")]
        );
        assert!(
            paths
                .library
                .iter()
                .all(|entry| !entry.path.ends_with("wrong-platform"))
        );
        assert_eq!(
            environment.properties.get("bdslib"),
            Some(&sdk.join("lib").to_string_lossy().into_owned())
        );
    }

    #[test]
    fn legacy_keys_and_explicit_envoptions_locator_are_scoped_to_win32() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("local sdk");
        fs::create_dir_all(&sdk).unwrap();
        let options = temp.path().join("external IDE options.proj");
        write(
            &options,
            r#"<Project><PropertyGroup Condition="'$(Platform)'=='Win32'"><Win32LibraryPath>$(BDS)\lib;</Win32LibraryPath><Win32BrowsingPath>$(BDS)\source</Win32BrowsingPath><DelphiNamespaceSearchPath>System;Vcl</DelphiNamespaceSearchPath></PropertyGroup></Project>"#,
        );
        let profile = ResolvedInstallation {
            id: "7.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [
                    ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                    (
                        "envoptions".to_owned(),
                        options.to_string_lossy().into_owned(),
                    ),
                ]
                .into_iter()
                .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let roots = [sdk.clone(), options.clone()];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();
        let environment =
            load_installation(&profile, "Debug", "Win32", &mut tracker, &policy).unwrap();
        let paths = evaluate_ide_paths(
            &environment,
            &profile,
            "Debug",
            "Win32",
            &mut tracker,
            &policy,
        )
        .unwrap();
        assert_eq!(paths.library[0].path, sdk.join("lib"));
        assert_eq!(paths.browsing[0].path, sdk.join("source"));
        assert_eq!(paths.namespaces, ["System", "Vcl"]);

        let other_platform = evaluate_ide_paths(
            &environment,
            &profile,
            "Debug",
            "Linux64",
            &mut tracker,
            &policy,
        )
        .unwrap();
        assert!(other_platform.library.is_empty());

        let unknown_platform =
            evaluate_ide_paths(&environment, &profile, "Debug", "", &mut tracker, &policy).unwrap();
        assert!(unknown_platform.library.is_empty());
    }

    #[test]
    fn missing_optional_environment_file_is_not_a_load_failure() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let ide = temp.path().join("ide");
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [
                    ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                    ("appdata".to_owned(), ide.to_string_lossy().into_owned()),
                ]
                .into_iter()
                .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let roots = [sdk, ide.clone()];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();
        let environment = load_installation(&profile, "Debug", "Win32", &mut tracker, &policy)
            .expect("missing optional environment.proj is a default layer");
        assert!(
            environment
                .warnings
                .iter()
                .any(|warning| warning.contains("rsvars"))
        );
        assert!(
            environment
                .metadata_observations
                .iter()
                .any(|observation| observation.path().ends_with("environment.proj"))
        );

        write(&ide.join("environment.proj"), "<Project><PropertyGroup>");
        let mut tracker = ProjectReadTracker::default();
        let malformed =
            load_installation(&profile, "Debug", "Win32", &mut tracker, &policy).unwrap();
        assert!(
            malformed
                .warnings
                .iter()
                .any(|warning| warning.contains("malformed environment settings"))
        );
    }

    #[test]
    fn installation_loading_propagates_budget_cancellation() {
        struct Cancelled;
        impl ProjectWorkBudget for Cancelled {
            fn check_cancelled(&self) -> Result<(), String> {
                Err("request cancelled".to_owned())
            }
            fn charge_path_visits(&self, _: usize) -> Result<(), String> {
                Ok(())
            }
            fn ensure_file_read_fits(&self, _: usize) -> Result<(), String> {
                Ok(())
            }
            fn charge_file_bytes(&self, _: usize) -> Result<(), String> {
                Ok(())
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [("bds".to_owned(), temp.path().to_string_lossy().into_owned())]
                    .into_iter()
                    .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let policy = ReadPolicy::new_with_installation_roots(
            &[],
            &[],
            &[],
            &profile.overrides,
            &[temp.path().to_path_buf()],
        );
        let budget = Cancelled;
        let mut tracker = ProjectReadTracker::with_budget(Some(&budget), &[]);
        assert_eq!(
            load_installation(&profile, "Debug", "Win32", &mut tracker, &policy).unwrap_err(),
            "request cancelled"
        );
    }

    #[test]
    fn unsupported_import_makes_dependent_ide_paths_uncertain() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let ide = temp.path().join("ide");
        write(
            &ide.join("EnvOptions.proj"),
            r#"<Project><Import Project="unsupported.targets"/><PropertyGroup Condition="'$(Platform)'=='Linux64'"><DelphiLibraryPath>$(BDS)\lib</DelphiLibraryPath></PropertyGroup></Project>"#,
        );
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [
                    ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                    ("appdata".to_owned(), ide.to_string_lossy().into_owned()),
                ]
                .into_iter()
                .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let roots = [sdk, ide];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();
        let environment =
            load_installation(&profile, "Debug", "Linux64", &mut tracker, &policy).unwrap();
        let error = evaluate_ide_paths(
            &environment,
            &profile,
            "Debug",
            "Linux64",
            &mut tracker,
            &policy,
        )
        .unwrap_err();
        assert!(error.contains("unsupported MSBuild import"));
    }

    #[test]
    fn imported_absolute_ide_paths_cannot_authorize_external_roots() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let ide = temp.path().join("ide");
        let external = temp.path().join("unconfigured/external-lib");
        write(
            &ide.join("environment.proj"),
            &format!(
                "<Project><PropertyGroup><DelphiLibraryPath>{}</DelphiLibraryPath></PropertyGroup></Project>",
                external.display()
            ),
        );
        write(&ide.join("EnvOptions.proj"), "<Project/>");
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [
                    ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                    ("appdata".to_owned(), ide.to_string_lossy().into_owned()),
                ]
                .into_iter()
                .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let roots = [sdk, ide];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();
        let environment =
            load_installation(&profile, "Debug", "Linux64", &mut tracker, &policy).unwrap();
        let paths = evaluate_ide_paths(
            &environment,
            &profile,
            "Debug",
            "Linux64",
            &mut tracker,
            &policy,
        )
        .unwrap();
        assert!(
            paths.library.is_empty(),
            "unconfigured imported path escaped its read roots"
        );
    }

    #[test]
    fn explicit_bdslib_override_preserves_original_environment_root_for_relocation() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let ide = temp.path().join("ide");
        let local_lib = temp.path().join("custom lib");
        write(&sdk.join("bin/rsvars.bat"), "SET BDS=C:\\Original\r\n");
        write(
            &ide.join("environment.proj"),
            "<Project><PropertyGroup><BDSLIB>C:\\Original\\lib</BDSLIB><BDSINCLUDE>C:\\Original\\lib\\include</BDSINCLUDE></PropertyGroup></Project>",
        );
        write(&ide.join("EnvOptions.proj"), "<Project/>");
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [
                    ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                    ("appdata".to_owned(), ide.to_string_lossy().into_owned()),
                    (
                        "bdslib".to_owned(),
                        local_lib.to_string_lossy().into_owned(),
                    ),
                ]
                .into_iter()
                .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let roots = [sdk, ide];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();
        let environment =
            load_installation(&profile, "Debug", "Linux64", &mut tracker, &policy).unwrap();
        assert_eq!(
            environment.properties["bdslib"],
            local_lib.to_string_lossy()
        );
        assert_eq!(
            environment.properties["bdsinclude"],
            local_lib.join("include").to_string_lossy()
        );
    }

    #[test]
    fn invalid_explicit_environment_locator_does_not_fall_back_to_appdata() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let ide = temp.path().join("ide");
        write(
            &ide.join("environment.proj"),
            "<Project><PropertyGroup><Marker>default-was-read</Marker></PropertyGroup></Project>",
        );
        write(&ide.join("EnvOptions.proj"), "<Project/>");
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [
                    ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                    ("appdata".to_owned(), ide.to_string_lossy().into_owned()),
                    (
                        "environmentsettings".to_owned(),
                        "C:\\unmapped\\environment.proj".to_owned(),
                    ),
                ]
                .into_iter()
                .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let roots = [sdk, ide];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();
        let error =
            load_installation(&profile, "Debug", "Linux64", &mut tracker, &policy).unwrap_err();
        assert!(error.contains("invalid configured environmentsettings locator"));

        let mut invalid_options_profile = profile.clone();
        invalid_options_profile
            .overrides
            .properties
            .remove("environmentsettings");
        invalid_options_profile.overrides.properties.insert(
            "envoptions".to_owned(),
            "C:\\unmapped\\EnvOptions.proj".to_owned(),
        );
        let mut tracker = ProjectReadTracker::default();
        let environment = load_installation(
            &invalid_options_profile,
            "Debug",
            "Linux64",
            &mut tracker,
            &policy,
        )
        .unwrap();
        assert!(
            evaluate_ide_paths(
                &environment,
                &invalid_options_profile,
                "Debug",
                "Linux64",
                &mut tracker,
                &policy,
            )
            .unwrap_err()
            .contains("invalid configured envoptions locator")
        );
    }

    #[test]
    fn nested_import_cancellation_is_returned_to_the_caller() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CancelOnNestedRead(AtomicUsize);
        impl ProjectWorkBudget for CancelOnNestedRead {
            fn check_cancelled(&self) -> Result<(), String> {
                if self.0.load(Ordering::SeqCst) >= 4 {
                    Err("request cancelled".to_owned())
                } else {
                    Ok(())
                }
            }
            fn charge_path_visits(&self, amount: usize) -> Result<(), String> {
                self.0.fetch_add(amount, Ordering::SeqCst);
                Ok(())
            }
            fn ensure_file_read_fits(&self, _: usize) -> Result<(), String> {
                Ok(())
            }
            fn charge_file_bytes(&self, _: usize) -> Result<(), String> {
                Ok(())
            }
            fn is_transient_error(&self, error: &str) -> bool {
                error == "request cancelled"
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let ide = temp.path().join("ide");
        write(&ide.join("settings.props"), "<Project/>");
        write(
            &ide.join("EnvOptions.proj"),
            r#"<Project><Import Project="settings.props"/><PropertyGroup><DelphiLibraryPath>$(BDS)\lib</DelphiLibraryPath></PropertyGroup></Project>"#,
        );
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [
                    ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                    ("appdata".to_owned(), ide.to_string_lossy().into_owned()),
                ]
                .into_iter()
                .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let roots = [sdk, ide];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();
        let environment =
            load_installation(&profile, "Debug", "Linux64", &mut tracker, &policy).unwrap();
        let budget = CancelOnNestedRead(AtomicUsize::new(0));
        let mut tracker = ProjectReadTracker::with_budget(Some(&budget), &[]);
        assert_eq!(
            evaluate_ide_paths(
                &environment,
                &profile,
                "Debug",
                "Linux64",
                &mut tracker,
                &policy,
            )
            .unwrap_err(),
            "request cancelled"
        );
    }

    #[test]
    fn unsupported_environment_import_fails_closed_instead_of_using_partial_roots() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let ide = temp.path().join("ide");
        write(
            &ide.join("environment.proj"),
            r#"<Project><Import Project="unsupported.targets"/><PropertyGroup><BDSLIB>C:\Original\lib</BDSLIB></PropertyGroup></Project>"#,
        );
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [
                    ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                    ("appdata".to_owned(), ide.to_string_lossy().into_owned()),
                ]
                .into_iter()
                .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let roots = [sdk, ide];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();
        let error =
            load_installation(&profile, "Debug", "Linux64", &mut tracker, &policy).unwrap_err();
        assert!(error.contains("unsupported MSBuild import"));
    }

    #[test]
    fn nested_environment_import_cancellation_is_propagated() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CancelAtNestedImport(AtomicUsize);
        impl ProjectWorkBudget for CancelAtNestedImport {
            fn check_cancelled(&self) -> Result<(), String> {
                if self.0.load(Ordering::SeqCst) >= 5 {
                    Err("request cancelled".to_owned())
                } else {
                    Ok(())
                }
            }
            fn charge_path_visits(&self, amount: usize) -> Result<(), String> {
                self.0.fetch_add(amount, Ordering::SeqCst);
                Ok(())
            }
            fn ensure_file_read_fits(&self, _: usize) -> Result<(), String> {
                Ok(())
            }
            fn charge_file_bytes(&self, _: usize) -> Result<(), String> {
                Ok(())
            }
            fn is_transient_error(&self, error: &str) -> bool {
                error == "request cancelled"
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let ide = temp.path().join("ide");
        write(&ide.join("settings.props"), "<Project/>");
        write(
            &ide.join("environment.proj"),
            r#"<Project><Import Project="settings.props"/><PropertyGroup><BDSLIB>C:\Original\lib</BDSLIB></PropertyGroup></Project>"#,
        );
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [
                    ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                    ("appdata".to_owned(), ide.to_string_lossy().into_owned()),
                ]
                .into_iter()
                .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let roots = [sdk, ide];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let budget = CancelAtNestedImport(AtomicUsize::new(0));
        let mut tracker = ProjectReadTracker::with_budget(Some(&budget), &[]);
        assert_eq!(
            load_installation(&profile, "Debug", "Linux64", &mut tracker, &policy).unwrap_err(),
            "request cancelled"
        );
    }

    #[test]
    fn inactive_unsupported_platform_import_does_not_fail_selected_platform() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let ide = temp.path().join("ide");
        fs::create_dir_all(&sdk).unwrap();
        write(
            &ide.join("EnvOptions.proj"),
            r#"<Project><Import Project="win32.targets" Condition="'$(Platform)'=='Win32'"/><PropertyGroup Condition="'$(Platform)'=='Linux64'"><DelphiLibraryPath>$(BDS)\lib</DelphiLibraryPath></PropertyGroup></Project>"#,
        );
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [
                    ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                    ("appdata".to_owned(), ide.to_string_lossy().into_owned()),
                ]
                .into_iter()
                .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let roots = [sdk.clone(), ide];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();
        let environment =
            load_installation(&profile, "Debug", "Linux64", &mut tracker, &policy).unwrap();
        let paths = evaluate_ide_paths(
            &environment,
            &profile,
            "Debug",
            "Linux64",
            &mut tracker,
            &policy,
        )
        .unwrap();
        assert_eq!(paths.library[0].path, sdk.join("lib"));
    }

    #[test]
    fn copied_envoptions_source_path_resolves_case_insensitively_on_linux() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let ide = temp.path().join("ide");
        let physical = sdk.join("source/Win32/rtl/sys");
        fs::create_dir_all(&physical).unwrap();
        write(
            &ide.join("EnvOptions.proj"),
            r#"<Project><PropertyGroup Condition="'$(Platform)'=='Win32'"><DelphiLibraryPath>$(BDS)\SOURCE\Win32\rtl\sys</DelphiLibraryPath></PropertyGroup></Project>"#,
        );
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [
                    ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                    ("appdata".to_owned(), ide.to_string_lossy().into_owned()),
                ]
                .into_iter()
                .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let roots = [sdk, ide];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();
        let environment =
            load_installation(&profile, "Debug", "Win32", &mut tracker, &policy).unwrap();
        let paths = evaluate_ide_paths(
            &environment,
            &profile,
            "Debug",
            "Win32",
            &mut tracker,
            &policy,
        )
        .unwrap();

        assert_eq!(paths.library.len(), 1);
        assert_eq!(paths.library[0].path, physical);
    }

    #[cfg(unix)]
    #[test]
    fn reconciliation_rejects_symlink_components() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sdk");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("source")).unwrap();
        let mut warnings = Vec::new();

        let result = super::reconcile_ide_path(
            &root.join("SOURCE/unit"),
            std::slice::from_ref(&root),
            &mut ProjectReadTracker::default(),
            &mut warnings,
            "delphilibrarypath",
            "$(BDS)\\SOURCE\\unit",
        )
        .unwrap();

        assert!(result.is_none());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("physical directory"))
        );
    }

    #[test]
    fn reconciliation_rejects_ambiguous_case_insensitive_components() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sdk");
        fs::create_dir_all(root.join("source/unit")).unwrap();
        fs::create_dir_all(root.join("SOURCE/unit")).unwrap();
        let mut warnings = Vec::new();

        let result = super::reconcile_ide_path(
            &root.join("SoUrCe/unit"),
            std::slice::from_ref(&root),
            &mut ProjectReadTracker::default(),
            &mut warnings,
            "delphilibrarypath",
            "$(BDS)\\SoUrCe\\unit",
        )
        .unwrap();

        assert!(result.is_none());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("ambiguous case-insensitive"))
        );
    }

    #[test]
    fn reconciliation_enforces_a_fixed_directory_scan_budget() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sdk");
        fs::create_dir_all(&root).unwrap();
        for index in 0..300 {
            fs::create_dir(root.join(format!("entry-{index}"))).unwrap();
        }
        let error = super::reconcile_ide_path(
            &root.join("missing"),
            std::slice::from_ref(&root),
            &mut ProjectReadTracker::default(),
            &mut Vec::new(),
            "delphilibrarypath",
            "$(BDS)\\missing",
        )
        .unwrap_err();

        assert!(error.contains("fixed work limit"));
    }

    #[test]
    fn reconciliation_propagates_cancellation() {
        struct Cancelled;
        impl ProjectWorkBudget for Cancelled {
            fn check_cancelled(&self) -> Result<(), String> {
                Err("request cancelled".to_owned())
            }
            fn charge_path_visits(&self, _: usize) -> Result<(), String> {
                Ok(())
            }
            fn ensure_file_read_fits(&self, _: usize) -> Result<(), String> {
                Ok(())
            }
            fn charge_file_bytes(&self, _: usize) -> Result<(), String> {
                Ok(())
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sdk");
        fs::create_dir_all(&root).unwrap();
        let budget = Cancelled;
        let mut tracker = ProjectReadTracker::with_budget(Some(&budget), &[]);
        let error = super::reconcile_ide_path(
            &root.join("source/unit"),
            std::slice::from_ref(&root),
            &mut tracker,
            &mut Vec::new(),
            "delphilibrarypath",
            "$(BDS)\\source\\unit",
        )
        .unwrap_err();

        assert_eq!(error, "request cancelled");
    }
}
use super::{
    InstallationEnvironment, PropertyMap, relocate_environment, resolve_path_with_inferred,
};
use crate::delphi_overrides::EffectiveOverrides;
use crate::installation_config::ResolvedInstallation;
use crate::{
    MetadataObservation, ProjectBuilder, ProjectOptions, ProjectPathEntry, ProjectPathProvenance,
    ProjectReadTracker, ReadPolicy,
};
use std::fs;
use std::path::{Path, PathBuf};

const MAX_INSTALLATION_XML_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RS_VARS_BYTES: u64 = 4 * 1024 * 1024;
const MAX_IDE_PATH_RECONCILIATION_VISITS: usize = 256;

#[derive(Debug, Clone, Default)]
pub(crate) struct IdePaths {
    pub library: Vec<ProjectPathEntry>,
    pub browsing: Vec<ProjectPathEntry>,
    pub debug_dcu: Vec<ProjectPathEntry>,
    pub namespaces: Vec<String>,
    pub warnings: Vec<String>,
}

pub(crate) fn load_installation(
    profile: &ResolvedInstallation,
    config: &str,
    platform: &str,
    tracker: &mut ProjectReadTracker<'_>,
    policy: &ReadPolicy,
) -> Result<InstallationEnvironment, String> {
    let bds = property_path(&profile.overrides, "bds", Path::new("/"));
    let appdata = property_path(&profile.overrides, "appdata", Path::new("/"));
    let mut warnings = Vec::new();
    let mut observations = Vec::new();
    let mut metadata_files = Vec::new();
    let mut imported = PropertyMap::new();
    let mut original_bds = None;

    if let Some(root) = bds.as_ref() {
        let rsvars = root.join("bin/rsvars.bat");
        if let Some(contents) = read_installation_file(
            &rsvars,
            false,
            MAX_RS_VARS_BYTES,
            policy,
            tracker,
            &mut observations,
            &mut metadata_files,
            &mut warnings,
        )? {
            match super::parse_rsvars(&contents, &PropertyMap::new()) {
                Ok(properties) => {
                    original_bds = properties.get("bds").cloned();
                    imported = properties;
                }
                Err(error) => {
                    warnings.push(format!("could not parse {}: {error}", rsvars.display()))
                }
            }
        }
    } else {
        warnings.push("selected Delphi installation has no configured BDS root".to_owned());
    }

    if let Some(original_bds) = original_bds.as_ref() {
        imported.insert("bds".to_owned(), original_bds.clone());
    }

    if let Some(path) = locator_path(
        &profile.overrides,
        "environmentsettings",
        appdata.as_deref(),
        "environment.proj",
    )? {
        if let Some(contents) = read_installation_file(
            &path,
            true,
            MAX_INSTALLATION_XML_BYTES,
            policy,
            tracker,
            &mut observations,
            &mut metadata_files,
            &mut warnings,
        )? {
            let environment_profile = environment_input_profile(profile);
            let mut builder = installation_builder(
                &environment_profile,
                config,
                platform,
                policy,
                &path,
                warnings.clone(),
            );
            builder.seed_installation_properties(&imported);
            match builder.process_installation_file(
                &contents,
                &path,
                tracker,
                &ProjectPathProvenance::Configured,
            ) {
                Ok(()) => {
                    imported = builder
                        .properties
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    if let Some(original_bds) = original_bds.as_ref() {
                        imported.insert("bds".to_owned(), original_bds.clone());
                    }
                    warnings = builder.warnings;
                }
                Err(error) => {
                    if tracker
                        .work_budget
                        .is_some_and(|budget| budget.is_transient_error(&error))
                        || !error.starts_with("malformed installation XML:")
                    {
                        return Err(error);
                    }
                    warnings.push(format!(
                        "malformed environment settings {}: {error}",
                        path.display()
                    ));
                    // A present but malformed file cannot safely supply any of its properties.
                    observations.push(MetadataObservation::Stat { path: path.clone() });
                }
            }
        }
    }

    let relocated = relocate_environment(&imported, &profile.overrides)?;
    let mut properties = relocated.properties;
    if let Some(root) = bds.as_ref() {
        properties
            .entry("bds".to_owned())
            .or_insert_with(|| root.to_string_lossy().into_owned());
    }
    derive_roots(&mut properties);
    let mut read_roots = relocated.read_roots;
    for path in [bds, appdata].into_iter().flatten() {
        if !read_roots.contains(&path) {
            read_roots.push(path);
        }
    }
    Ok(InstallationEnvironment {
        properties,
        read_roots,
        inferred_mappings: relocated.inferred_mappings,
        installation_config_files: metadata_files.clone(),
        metadata_files,
        metadata_observations: observations,
        path_issues: Vec::new(),
        warnings,
    })
}

pub(crate) fn evaluate_ide_paths(
    environment: &InstallationEnvironment,
    profile: &ResolvedInstallation,
    config: &str,
    platform: &str,
    tracker: &mut ProjectReadTracker<'_>,
    policy: &ReadPolicy,
) -> Result<IdePaths, String> {
    let mut warnings = Vec::new();
    let appdata = property_path(&profile.overrides, "appdata", Path::new("/"));
    let Some(path) = locator_path(
        &profile.overrides,
        "envoptions",
        appdata.as_deref(),
        "EnvOptions.proj",
    )?
    else {
        return Ok(IdePaths {
            warnings,
            ..IdePaths::default()
        });
    };
    let mut observations = Vec::new();
    let mut files = Vec::new();
    let Some(contents) = read_installation_file(
        &path,
        false,
        MAX_INSTALLATION_XML_BYTES,
        policy,
        tracker,
        &mut observations,
        &mut files,
        &mut warnings,
    )?
    else {
        return Ok(IdePaths {
            warnings,
            ..IdePaths::default()
        });
    };

    let options = ProjectOptions {
        build_config: Some(config.to_owned()),
        platform: Some(platform.to_owned()),
        ..ProjectOptions::default()
    };
    let mut builder = ProjectBuilder::new(
        &options,
        &profile.overrides,
        Vec::new(),
        path.parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf(),
        policy.clone(),
    );
    builder.seed_installation_properties(
        &environment
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    builder.process_installation_file(
        &contents,
        &path,
        tracker,
        &ProjectPathProvenance::Configured,
    )?;
    let legacy = profile.id == "7.0" && platform.eq_ignore_ascii_case("Win32");
    let (library_key, browsing_key, debug_key) = if legacy {
        ("win32librarypath", "win32browsingpath", "win32debugdcupath")
    } else {
        (
            "delphilibrarypath",
            "delphibrowsingpath",
            "delphidebugdcupath",
        )
    };
    let library = property_paths(
        &builder,
        library_key,
        profile,
        environment,
        policy,
        tracker,
        &mut warnings,
    )?;
    let browsing = property_paths(
        &builder,
        browsing_key,
        profile,
        environment,
        policy,
        tracker,
        &mut warnings,
    )?;
    let debug_dcu = property_paths(
        &builder,
        debug_key,
        profile,
        environment,
        policy,
        tracker,
        &mut warnings,
    )?;
    let namespaces = builder
        .property_list_with_provenance("delphinamespacesearchpath")
        .into_iter()
        .map(|(value, _)| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect();
    Ok(IdePaths {
        library,
        browsing,
        debug_dcu,
        namespaces,
        warnings,
    })
}

fn installation_builder(
    profile: &ResolvedInstallation,
    config: &str,
    platform: &str,
    policy: &ReadPolicy,
    file: &Path,
    warnings: Vec<String>,
) -> ProjectBuilder {
    let options = ProjectOptions {
        build_config: Some(config.to_owned()),
        platform: Some(platform.to_owned()),
        ..ProjectOptions::default()
    };
    ProjectBuilder::new(
        &options,
        &profile.overrides,
        warnings,
        file.parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf(),
        policy.clone(),
    )
}

fn property_path(overrides: &EffectiveOverrides, name: &str, base: &Path) -> Option<PathBuf> {
    let value = overrides.properties.get(name)?;
    overrides
        .resolve_path(value, base)
        .ok()
        .map(|resolved| resolved.path)
}

fn locator_path(
    overrides: &EffectiveOverrides,
    name: &str,
    appdata: Option<&Path>,
    filename: &str,
) -> Result<Option<PathBuf>, String> {
    if let Some(value) = overrides.properties.get(name) {
        return overrides
            .resolve_path(value, Path::new("/"))
            .map(|resolved| Some(resolved.path))
            .map_err(|error| format!("invalid configured {name} locator `{value}`: {error}"));
    }
    Ok(appdata.map(|root| root.join(filename)))
}

fn environment_input_profile(profile: &ResolvedInstallation) -> ResolvedInstallation {
    let mut profile = profile.clone();
    // Preserve the imported BDSLIB value as relocation evidence while still
    // applying the configured local BDSLIB as the final authoritative value.
    profile.overrides.properties.remove("bdslib");
    profile.overrides.property_origins.remove("bdslib");
    profile
}

#[allow(clippy::too_many_arguments)]
fn read_installation_file(
    path: &Path,
    optional_absence: bool,
    limit: u64,
    policy: &ReadPolicy,
    tracker: &mut ProjectReadTracker<'_>,
    observations: &mut Vec<MetadataObservation>,
    metadata_files: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
) -> Result<Option<String>, String> {
    if let Some(budget) = tracker.work_budget {
        budget.check_cancelled()?;
        budget.charge_path_visits(1)?;
    }
    let entry = policy
        .entry_for_path(path)
        .unwrap_or_else(|| ProjectPathEntry {
            path: path.to_path_buf(),
            provenance: ProjectPathProvenance::Configured,
        });
    tracker.record_metadata_path(path.to_path_buf());
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            observations.push(MetadataObservation::Stat {
                path: path.to_path_buf(),
            });
            tracker.record_metadata_observation(MetadataObservation::Stat {
                path: path.to_path_buf(),
            });
            if !optional_absence {
                warnings.push(format!(
                    "installation metadata is absent: {}",
                    path.display()
                ));
            }
            return Ok(None);
        }
        Err(error) => {
            warnings.push(format!(
                "could not inspect installation metadata {}: {error}",
                path.display()
            ));
            return Ok(None);
        }
        Ok(_) => {}
    }
    match crate::read_payload_with_tracker(policy, &entry, limit, tracker) {
        Ok((contents, observation)) => {
            metadata_files.push(path.to_path_buf());
            observations.push(observation.clone());
            tracker.record_metadata_observation(observation);
            Ok(Some(contents))
        }
        Err(error) => {
            if error == "request cancelled" || error.contains("budget") {
                return Err(error);
            }
            warnings.push(format!(
                "could not read installation metadata {}: {error}",
                path.display()
            ));
            observations.push(MetadataObservation::Stat {
                path: path.to_path_buf(),
            });
            tracker.record_metadata_observation(MetadataObservation::Stat {
                path: path.to_path_buf(),
            });
            Ok(None)
        }
    }
}

fn derive_roots(properties: &mut PropertyMap) {
    if let Some(bds) = properties.get("bds").cloned() {
        for (name, suffix) in [
            ("bdslib", "lib"),
            ("bdsinclude", "include"),
            ("bdsbin", "bin"),
        ] {
            properties
                .entry(name.to_owned())
                .or_insert_with(|| Path::new(&bds).join(suffix).to_string_lossy().into_owned());
        }
    }
}

fn property_paths(
    builder: &ProjectBuilder,
    key: &str,
    profile: &ResolvedInstallation,
    environment: &InstallationEnvironment,
    policy: &ReadPolicy,
    tracker: &mut ProjectReadTracker<'_>,
    warnings: &mut Vec<String>,
) -> Result<Vec<ProjectPathEntry>, String> {
    let mut entries = Vec::new();
    for (raw, _provenance) in builder.property_list_with_provenance(key) {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let normalized;
        let raw = if !is_windows_absolute(raw) {
            normalized = raw.replace('\\', "/");
            normalized.as_str()
        } else {
            raw
        };
        match resolve_path_with_inferred(
            &profile.overrides,
            &environment.inferred_mappings,
            raw,
            Path::new("/"),
        ) {
            Ok(resolved) => {
                if !environment
                    .read_roots
                    .iter()
                    .any(|root| crate::project_path_starts_with(&resolved.path, root))
                {
                    warnings.push(format!(
                        "ignored IDE {key} entry outside selected installation read roots: {raw}"
                    ));
                    continue;
                }
                let provisional =
                    ProjectPathEntry::resolved(resolved.path.clone(), &resolved, true);
                if !policy.allows_location(&provisional) {
                    warnings.push(format!(
                        "ignored IDE {key} entry not authorized by the read policy: {raw}"
                    ));
                    continue;
                }
                let physical_path = match reconcile_ide_path(
                    &resolved.path,
                    &environment.read_roots,
                    tracker,
                    warnings,
                    key,
                    raw,
                ) {
                    Ok(Some(path)) => path,
                    Ok(None) => continue,
                    Err(error) if error.contains("fixed work limit") => {
                        warnings.push(format!("IDE {key} entry `{raw}` is uncertain: {error}"));
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let entry = ProjectPathEntry::resolved(physical_path, &resolved, true);
                if !policy.allows_location(&entry) {
                    warnings.push(format!(
                        "ignored IDE {key} entry not authorized by the read policy: {raw}"
                    ));
                    continue;
                }
                if entries.iter().all(|existing: &ProjectPathEntry| {
                    !crate::project_paths_equal(&existing.path, &entry.path)
                }) {
                    entries.push(entry);
                }
            }
            Err(error) => warnings.push(format!("could not resolve {key} entry `{raw}`: {error}")),
        }
    }
    Ok(entries)
}

fn reconcile_ide_path(
    candidate: &Path,
    roots: &[PathBuf],
    tracker: &mut ProjectReadTracker<'_>,
    warnings: &mut Vec<String>,
    key: &str,
    raw: &str,
) -> Result<Option<PathBuf>, String> {
    let Some(root) = roots
        .iter()
        .filter(|root| crate::project_path_starts_with(candidate, root))
        .max_by_key(|root| root.components().count())
    else {
        warnings.push(format!(
            "ignored IDE {key} entry outside selected installation read roots: {raw}"
        ));
        return Ok(None);
    };

    let mut visits = 0usize;
    let mut current = PathBuf::new();
    for component in root.components() {
        current.push(component.as_os_str());
        let metadata = match checked_symlink_metadata(&current, tracker, &mut visits)? {
            Some(metadata) => metadata,
            None => {
                warnings.push(format!(
                    "IDE {key} entry `{raw}` is uncertain: selected installation root is absent ({})",
                    root.display()
                ));
                return Ok(None);
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            warnings.push(format!(
                "IDE {key} entry `{raw}` is uncertain: selected installation root is not a physical directory ({})",
                current.display()
            ));
            return Ok(None);
        }
    }

    let Ok(relative) = candidate.strip_prefix(root) else {
        warnings.push(format!(
            "ignored IDE {key} entry outside selected installation read roots: {raw}"
        ));
        return Ok(None);
    };
    let mut components = relative.components().peekable();
    while let Some(wanted) = components.next() {
        use std::path::Component;
        let Component::Normal(wanted) = wanted else {
            warnings.push(format!(
                "IDE {key} entry `{raw}` is uncertain: invalid path component"
            ));
            return Ok(None);
        };
        let exact = current.join(wanted);
        let selected = match checked_symlink_metadata(&exact, tracker, &mut visits)? {
            Some(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    warnings.push(format!(
                        "IDE {key} entry `{raw}` is uncertain: path component is not a physical directory ({})",
                        exact.display()
                    ));
                    return Ok(None);
                }
                exact
            }
            None => {
                let directory = match fs::read_dir(&current) {
                    Ok(directory) => directory,
                    Err(error) => {
                        warnings.push(format!(
                            "IDE {key} entry `{raw}` is uncertain: could not inspect {}: {error}",
                            current.display()
                        ));
                        return Ok(None);
                    }
                };
                let wanted_text = wanted.to_string_lossy();
                let mut matches = Vec::new();
                for entry in directory {
                    check_reconciliation_budget(tracker, &mut visits)?;
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            warnings.push(format!(
                                "IDE {key} entry `{raw}` is uncertain: directory scan failed under {}: {error}",
                                current.display()
                            ));
                            return Ok(None);
                        }
                    };
                    if entry
                        .file_name()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&wanted_text)
                    {
                        matches.push(entry.path());
                    }
                }
                if matches.len() > 1 {
                    warnings.push(format!(
                        "IDE {key} entry `{raw}` is uncertain: ambiguous case-insensitive component {wanted_text:?} under {}",
                        current.display()
                    ));
                    return Ok(None);
                }
                let Some(path) = matches.pop() else {
                    // IDE library roots are often generated later (for example,
                    // add-on output folders). Keep the safely rooted lexical
                    // path when no physical component exists to disambiguate.
                    current.push(wanted);
                    for remaining in components {
                        current.push(remaining.as_os_str());
                    }
                    return Ok(Some(current));
                };
                let metadata = match checked_symlink_metadata(&path, tracker, &mut visits)? {
                    Some(metadata) => metadata,
                    None => return Ok(None),
                };
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    warnings.push(format!(
                        "IDE {key} entry `{raw}` is uncertain: path component is not a physical directory ({})",
                        path.display()
                    ));
                    return Ok(None);
                }
                path
            }
        };
        current = selected;
    }
    Ok(Some(current))
}

fn checked_symlink_metadata(
    path: &Path,
    tracker: &mut ProjectReadTracker<'_>,
    visits: &mut usize,
) -> Result<Option<fs::Metadata>, String> {
    check_reconciliation_budget(tracker, visits)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Ok(None),
    }
}

fn check_reconciliation_budget(
    tracker: &mut ProjectReadTracker<'_>,
    visits: &mut usize,
) -> Result<(), String> {
    *visits += 1;
    if *visits > MAX_IDE_PATH_RECONCILIATION_VISITS {
        return Err("IDE path reconciliation exceeded its fixed work limit".to_owned());
    }
    if let Some(budget) = tracker.work_budget {
        budget.check_cancelled()?;
        budget.charge_path_visits(1)?;
    }
    Ok(())
}

fn is_windows_absolute(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\'))
        || raw.starts_with("\\\\")
        || raw.starts_with("//")
}
