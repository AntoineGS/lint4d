use super::{PropertyMap, RelocatedEnvironment};
use crate::delphi_overrides::{EffectiveOverrides, PathMapping, ResolvedPath};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const INDEPENDENT_ROOTS: &[&str] = &["bds", "bdslib", "bdsinclude", "bdsbin"];

pub(crate) fn relocate_environment(
    imported: &PropertyMap,
    configured: &EffectiveOverrides,
) -> Result<RelocatedEnvironment, String> {
    // APPDATA in this configuration points at the per-version IDE directory,
    // not the roaming Windows APPDATA root, so it is never an inferred root.
    let mut inferred_by_source = BTreeMap::<String, PathMapping>::new();
    for name in INDEPENDENT_ROOTS {
        let (Some(local), Some(original)) = (configured.properties.get(*name), imported.get(*name))
        else {
            continue;
        };
        if !is_windows_absolute(original) || !Path::new(local).is_absolute() {
            continue;
        }
        let mapping = PathMapping {
            from: original.replace('\\', "/"),
            to: PathBuf::from(local),
            config_file: configured
                .property_origins
                .get(*name)
                .cloned()
                .unwrap_or_default(),
        };
        let canonical = canonical_root(&mapping.from);
        if let Some(previous) = inferred_by_source.get(&canonical) {
            if previous.to != mapping.to {
                return Err(format!(
                    "conflicting inferred translations for original root `{}`",
                    mapping.from
                ));
            }
        } else {
            inferred_by_source.insert(canonical, mapping);
        }
    }
    let mut inferred_mappings = inferred_by_source.into_values().collect::<Vec<_>>();
    inferred_mappings.sort_by_key(|mapping| std::cmp::Reverse(component_count(&mapping.from)));

    let mut properties = imported.clone();
    for (name, value) in imported {
        if configured.properties.contains_key(name) {
            continue;
        }
        if is_windows_absolute(value) {
            if let Ok(resolved) = resolve_from_tier(&inferred_mappings, value) {
                if !resolved.path.as_os_str().is_empty() {
                    properties.insert(name.clone(), resolved.path.to_string_lossy().into_owned());
                }
            }
        }
    }
    // Explicit profile properties are authoritative and remain a separate tier
    // from imported defaults, rather than mutating the captured configuration.
    for (name, value) in &configured.properties {
        properties.insert(name.clone(), value.clone());
    }

    // Only roots provided by the selected profile/mappings authorize reads;
    // unrelated roots in the imported environment do not.
    let mut read_roots = Vec::new();
    for (name, value) in &configured.properties {
        if is_root_property(name) {
            let root = PathBuf::from(value);
            if root.is_absolute() && !read_roots.contains(&root) {
                read_roots.push(root);
            }
        }
    }
    for mapping in &configured.path_mappings {
        if !read_roots.contains(&mapping.to) {
            read_roots.push(mapping.to.clone());
        }
    }
    Ok(RelocatedEnvironment {
        properties,
        inferred_mappings,
        read_roots,
    })
}

pub(crate) fn resolve_path_with_inferred(
    configured: &EffectiveOverrides,
    inferred: &[PathMapping],
    raw: &str,
    base: &Path,
) -> Result<ResolvedPath, String> {
    match configured.resolve_path(raw, base) {
        Ok(resolved) => Ok(resolved),
        Err(explicit_error) => {
            // The existing resolver reports unmapped Windows paths as errors.
            // Give the derived tier a chance, but preserve native explicit-path
            // errors (which cannot be interpreted by this tier).
            let mut derived = configured.clone();
            derived.path_mappings = inferred.to_vec();
            derived.resolve_path(raw, base).map_err(|_| explicit_error)
        }
    }
}

fn resolve_from_tier(mappings: &[PathMapping], raw: &str) -> Result<ResolvedPath, String> {
    let tier = EffectiveOverrides {
        path_mappings: mappings.to_vec(),
        ..EffectiveOverrides::default()
    };
    tier.resolve_path(raw, Path::new("/"))
}

fn is_root_property(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "bds" | "bdslib" | "bdsinclude" | "bdsbin" | "bdscommondir" | "bdsuserdir" | "appdata"
    )
}

fn is_windows_absolute(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn canonical_root(root: &str) -> String {
    root.trim_end_matches(['/', '\\'])
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn component_count(root: &str) -> usize {
    root.split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .count()
}

#[cfg(test)]
mod tests {
    use super::super::PropertyMap;
    use super::{relocate_environment, resolve_path_with_inferred};
    use crate::delphi_overrides::{EffectiveOverrides, OverrideLayer};
    use std::path::{Path, PathBuf};

    fn configured(toml: &str) -> EffectiveOverrides {
        let layer = OverrideLayer::parse(toml, Path::new("local.toml")).unwrap();
        EffectiveOverrides::merge(&[layer])
    }

    #[test]
    fn imported_descendant_uses_configured_local_root() {
        let configured = configured("[properties]\nBDS = '/local/sdk'\n");
        let imported = PropertyMap::from([
            ("bds".into(), r"C:\Old\37.0".into()),
            ("bdslib".into(), r"C:\Old\37.0\lib".into()),
        ]);
        let relocated = relocate_environment(&imported, &configured).unwrap();
        assert_eq!(relocated.properties["bdslib"], "/local/sdk/lib");
    }

    #[test]
    fn explicit_local_child_property_wins() {
        let configured = configured("[properties]\nBDS = '/local/sdk'\nBDSLIB = '/custom/lib'\n");
        let imported = PropertyMap::from([
            ("bds".into(), r"C:\Old\37.0".into()),
            ("bdslib".into(), r"C:\Old\37.0\lib".into()),
        ]);
        let relocated = relocate_environment(&imported, &configured).unwrap();
        assert_eq!(relocated.properties["bdslib"], "/custom/lib");
    }

    #[test]
    fn configured_mapping_tier_wins_over_more_specific_inferred_root() {
        let configured = configured(
            "[properties]\nBDS = '/local/sdk'\n[[path_mappings]]\nfrom = 'C:/Old/37.0/lib'\nto = '/mapped/lib'\n",
        );
        let imported = PropertyMap::from([
            ("bds".into(), r"C:\Old\37.0".into()),
            ("bdslib".into(), r"C:\Old\37.0\lib".into()),
        ]);
        let relocated = relocate_environment(&imported, &configured).unwrap();
        let resolved = resolve_path_with_inferred(
            &configured,
            &relocated.inferred_mappings,
            r"C:\Old\37.0\lib\Windows",
            Path::new("/project"),
        )
        .unwrap();
        assert_eq!(resolved.path, PathBuf::from("/mapped/lib/Windows"));
    }

    #[test]
    fn inferred_roots_match_case_slash_variants_at_component_boundaries() {
        let configured = configured("[properties]\nBDS = '/local/SDK'\n");
        let imported = PropertyMap::from([("bds".into(), r"C:\Old\SDK".into())]);
        let relocated = relocate_environment(&imported, &configured).unwrap();
        let resolve = |path| {
            resolve_path_with_inferred(
                &configured,
                &relocated.inferred_mappings,
                path,
                Path::new("/project"),
            )
            .unwrap()
            .path
        };
        assert_eq!(resolve(r"c:/old/sdk/Lib"), PathBuf::from("/local/SDK/Lib"));
        assert!(
            resolve_path_with_inferred(
                &configured,
                &relocated.inferred_mappings,
                r"C:\Old\SDK2\Lib",
                Path::new("/project"),
            )
            .is_err()
        );
    }

    #[test]
    fn configured_independent_root_does_not_relocate_from_another_root() {
        let configured = configured(
            "[properties]\nBDS = '/local/sdk'\nBDSCOMMONDIR = '/local/common'\nAPPDATA = '/ide/config'\n",
        );
        let imported = PropertyMap::from([
            ("bds".into(), r"C:\Old\SDK".into()),
            ("bdslib".into(), r"C:\Old\SDK\lib".into()),
            ("bdscommondir".into(), r"D:\Shared\Common".into()),
            ("appdata".into(), r"C:\Users\me\AppData\Roaming".into()),
        ]);
        let relocated = relocate_environment(&imported, &configured).unwrap();
        assert_eq!(relocated.properties["bdslib"], "/local/sdk/lib");
        assert_eq!(relocated.properties["bdscommondir"], "/local/common");
        assert_eq!(relocated.properties["appdata"], "/ide/config");
        assert!(relocated.read_roots.contains(&PathBuf::from("/ide/config")));
        assert!(
            !relocated
                .read_roots
                .contains(&PathBuf::from(r"D:\Shared\Common"))
        );
    }

    #[test]
    fn conflicting_original_roots_are_rejected() {
        let configured =
            configured("[properties]\nBDS = '/local/sdk'\nBDSLIB = '/different/lib'\n");
        let imported = PropertyMap::from([
            ("bds".into(), r"C:\Old\SDK".into()),
            ("bdslib".into(), r"c:/old/sdk/".into()),
        ]);
        assert!(
            relocate_environment(&imported, &configured)
                .unwrap_err()
                .contains("conflicting inferred translations")
        );
    }

    #[test]
    fn explicitly_configured_bdsinclude_is_relocated() {
        let configured =
            configured("[properties]\nBDS = '/local/sdk'\nBDSINCLUDE = '/custom/include'\n");
        let imported = PropertyMap::from([
            ("bds".into(), r"C:\Old\SDK".into()),
            ("bdsinclude".into(), r"C:\Old\SDK\include".into()),
        ]);
        let relocated = relocate_environment(&imported, &configured).unwrap();
        assert_eq!(relocated.properties["bdsinclude"], "/custom/include");
        assert!(
            relocated
                .inferred_mappings
                .iter()
                .any(|mapping| { mapping.from.eq_ignore_ascii_case(r"C:/Old/SDK/include") })
        );
    }

    #[test]
    fn imported_path_property_does_not_authorize_an_arbitrary_read_root() {
        let configured = configured("[properties]\nBDS = '/local/sdk'\n");
        let imported = PropertyMap::from([
            ("bds".into(), r"C:\Old\SDK".into()),
            ("path".into(), r"D:\Unrelated\tools".into()),
        ]);
        let relocated = relocate_environment(&imported, &configured).unwrap();
        assert_eq!(relocated.read_roots, vec![PathBuf::from("/local/sdk")]);
    }
}
