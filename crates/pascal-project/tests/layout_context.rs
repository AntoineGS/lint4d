use pascal_project::delphi_overrides::OverrideSession;
use pascal_project::{
    CompilerVersion, ConditionalContext, ConditionalFact as F, LayoutContext, LayoutPlatform,
    LayoutSettings, ProjectContext, ProjectOptions, SourceOrigin, TargetPlatform,
};
use std::fs;
use tempfile::{TempDir, tempdir};

fn discover(properties: &str, options: ProjectOptions) -> (TempDir, ProjectContext) {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::create_dir(temp.path().join("library")).unwrap();
    fs::write(project.join("App.dpr"), "program App; begin end.").unwrap();
    fs::write(
        temp.path().join("library/External.pas"),
        "unit External; interface implementation end.",
    )
    .unwrap();
    fs::write(project.join("App.dproj"), format!(
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Config>Debug</Config><Platform>Win32</Platform>{properties}</PropertyGroup><ProjectExtensions><BorlandProject><Platforms><Platform value=\"Win32\">True</Platform></Platforms></BorlandProject></ProjectExtensions></Project>"
    )).unwrap();
    let context = ProjectContext::discover_with_overrides(
        &project.join("App.dpr"),
        &[temp.path().to_path_buf()],
        &options,
        &OverrideSession::default(),
    )
    .unwrap();
    (temp, context)
}

fn options(version: Option<CompilerVersion>) -> ProjectOptions {
    ProjectOptions {
        conditional_context: ConditionalContext {
            compiler_version: version,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn target_facts_are_not_defines() {
    let version = Some(CompilerVersion::new(37, 0));
    let win32 = LayoutContext::for_target(version, Some(&TargetPlatform::Win32));
    let win64 = LayoutContext::for_target(version, Some(&TargetPlatform::Win64));
    let mut ctx = ConditionalContext::default()
        .with_compiler_version(version.unwrap())
        .with_layout(win32.clone());
    let before = ctx.fingerprint();
    ctx.set_define("WIN64", F::True);
    assert_eq!(ctx.layout, win32);
    assert_ne!(before, ctx.fingerprint());
    assert_ne!(
        ctx.fingerprint(),
        ctx.clone().with_layout(win64).fingerprint()
    );
    assert_eq!(LayoutContext::default().platform, None);
    assert_eq!(
        pascal_project::layout::LayoutContext::default().defaults,
        LayoutSettings::default()
    );
}

#[test]
fn only_referenced_defaults_are_admitted() {
    for target in [TargetPlatform::Win32, TargetPlatform::Win64] {
        let athens = LayoutContext::for_target(Some(CompilerVersion::new(36, 0)), Some(&target));
        assert_eq!(athens.defaults.record_alignment, Some(8));
        assert_eq!(athens.defaults.long_strings, F::True);
        assert_eq!(athens.defaults.minimum_enum_size, None);
        assert_eq!(athens.defaults.old_type_layout, F::Unknown);
        assert_eq!(athens.defaults.real_compatibility, F::Unknown);
        for version in [
            None,
            Some(CompilerVersion::new(37, 0)),
            Some(CompilerVersion::new(35, 0)),
            Some(CompilerVersion::new(36, 1)),
            Some(CompilerVersion::with_patch(36, 0, 1)),
        ] {
            assert_eq!(
                LayoutContext::for_target(version, Some(&target)).defaults,
                LayoutSettings::default()
            );
        }
    }
    let other = LayoutContext::for_target(
        Some(CompilerVersion::new(36, 0)),
        Some(&TargetPlatform::Other("Linux64".into())),
    );
    assert_eq!(
        other.platform,
        Some(LayoutPlatform::Other("Linux64".into()))
    );
    assert_eq!(other.defaults, LayoutSettings::default());
    assert_eq!(
        LayoutContext::for_target(Some(CompilerVersion::new(36, 0)), None),
        LayoutContext::default()
    );
}

#[test]
fn rf_library_defaults() {
    let (temp, context) = discover(
        "<DCC_Alignment>1</DCC_Alignment><DCC_MinimumEnumSize>4</DCC_MinimumEnumSize><DCC_LongStrings>false</DCC_LongStrings>",
        options(Some(CompilerVersion::new(36, 0))),
    );
    let project_path = temp.path().join("project/App.dpr");
    let library_path = temp.path().join("library/External.pas");
    assert_eq!(
        context.source_origin(&project_path),
        SourceOrigin::ProjectCompiled
    );
    assert_eq!(context.source_origin(&library_path), SourceOrigin::Library);
    let project = context.conditional_context_for(&project_path);
    let library = context.conditional_context_for(&library_path);
    assert_eq!(project.layout.platform, Some(LayoutPlatform::Win32));
    assert_eq!(library.layout.platform, Some(LayoutPlatform::Win32));
    assert_eq!(project.layout.defaults.record_alignment, Some(1));
    assert_eq!(project.layout.defaults.minimum_enum_size, Some(4));
    assert_eq!(project.layout.defaults.long_strings, F::False);
    assert_eq!(library.layout.defaults.record_alignment, Some(8));
    assert_eq!(library.layout.defaults.minimum_enum_size, None);
    assert_eq!(library.layout.defaults.long_strings, F::True);
    assert!(!project.layout_explicit);
    assert!(!library.layout_explicit);
}

#[test]
fn unverified_compiler_defaults_are_not_project_property_defaults() {
    for version in [None, Some(CompilerVersion::new(37, 0))] {
        let (_temp, context) = discover("", options(version));
        assert_eq!(
            context.conditional_context.layout.defaults,
            LayoutSettings::default()
        );
        assert_eq!(
            context.library_conditional_context.layout.defaults,
            LayoutSettings::default()
        );
        let (_temp, context) = discover(
            "<DCC_Alignment>2</DCC_Alignment><DCC_MinimumEnumSize>1</DCC_MinimumEnumSize><DCC_LongStrings>on</DCC_LongStrings>",
            options(version),
        );
        assert_eq!(
            context.conditional_context.layout.defaults.record_alignment,
            Some(2)
        );
        assert_eq!(
            context
                .conditional_context
                .layout
                .defaults
                .minimum_enum_size,
            Some(1)
        );
        assert_eq!(
            context.conditional_context.layout.defaults.long_strings,
            F::True
        );
        assert_eq!(
            context.library_conditional_context.layout.defaults,
            LayoutSettings::default()
        );
    }
}

#[test]
fn invalid_selection_does_not_establish_target_or_compiler_defaults() {
    let mut opts = options(Some(CompilerVersion::new(36, 0)));
    opts.platform = Some("Win64".into());
    let (_temp, context) = discover("", opts);
    // Legacy global options retain their selection mode even when undeclared;
    // layout admission must independently respect the declared candidate set.
    assert_eq!(
        context.platform_selection.selected.as_deref(),
        Some("Win64")
    );
    assert_eq!(context.platform_selection.candidates, ["Win32"]);
    assert_eq!(context.conditional_context.layout, LayoutContext::default());
    assert_eq!(
        context.library_conditional_context.layout,
        LayoutContext::default()
    );
}

#[test]
fn unresolved_or_invalid_properties_invalidate_only_affected_settings() {
    for alignment in ["$(Missing)", "3", "16", "", "8oops"] {
        let (_temp, context) = discover(
            &format!("<DCC_Alignment>{alignment}</DCC_Alignment>"),
            options(Some(CompilerVersion::new(36, 0))),
        );
        assert_eq!(
            context.conditional_context.layout.defaults.record_alignment,
            None
        );
        assert_eq!(
            context.conditional_context.layout.defaults.long_strings,
            F::True
        );
        assert_eq!(
            context
                .library_conditional_context
                .layout
                .defaults
                .record_alignment,
            Some(8)
        );
    }
    let (_temp, context) = discover(
        "<DCC_LongStrings>$(Missing)</DCC_LongStrings><DCC_MinimumEnumSize>3</DCC_MinimumEnumSize>",
        options(Some(CompilerVersion::new(36, 0))),
    );
    assert_eq!(
        context.conditional_context.layout.defaults.long_strings,
        F::Unknown
    );
    assert_eq!(
        context
            .conditional_context
            .layout
            .defaults
            .minimum_enum_size,
        None
    );
    assert_eq!(
        context.conditional_context.layout.defaults.record_alignment,
        Some(8)
    );
}

#[test]
fn explicit_unknown_layout_cannot_be_filled_by_metadata() {
    let mut opts = options(Some(CompilerVersion::new(36, 0)));
    opts.conditional_context = opts
        .conditional_context
        .with_layout(LayoutContext::default());
    let (_temp, context) = discover("<DCC_Alignment>1</DCC_Alignment>", opts);
    for ctx in [
        &context.conditional_context,
        &context.library_conditional_context,
    ] {
        assert_eq!(ctx.layout, LayoutContext::default());
        assert!(ctx.layout_explicit);
    }
}

#[test]
fn explicit_layout_overrides_properties_but_not_conflicting_target() {
    let supplied = LayoutContext {
        platform: Some(LayoutPlatform::Win32),
        defaults: LayoutSettings {
            record_alignment: Some(4),
            ..Default::default()
        },
    };
    let mut opts = options(Some(CompilerVersion::new(36, 0)));
    opts.conditional_context = opts.conditional_context.with_layout(supplied.clone());
    let (_temp, context) = discover("<DCC_Alignment>1</DCC_Alignment>", opts.clone());
    assert_eq!(context.conditional_context.layout, supplied);
    assert_eq!(context.library_conditional_context.layout, supplied);
    opts.conditional_context.layout.platform = Some(LayoutPlatform::Win64);
    let (_temp, context) = discover("", opts);
    assert_eq!(context.conditional_context.layout, LayoutContext::default());
    assert_eq!(
        context.library_conditional_context.layout,
        LayoutContext::default()
    );
    assert!(context.conditional_context.layout_explicit);
}

#[test]
fn fingerprints_and_recovery_account_for_layout_and_provenance() {
    let base = ConditionalContext::default();
    let explicit = base.clone().with_layout(LayoutContext::default());
    assert_ne!(base.fingerprint(), explicit.fingerprint());
    let custom = base.with_layout(LayoutContext {
        platform: Some(LayoutPlatform::Other("CustomTarget".into())),
        ..Default::default()
    });
    let mut sizes = Vec::new();
    custom
        .visit_recovery_payload(&mut |size| {
            sizes.push(size);
            Ok(())
        })
        .unwrap();
    assert_eq!(sizes, vec!["CustomTarget".len()]);
    assert!(
        custom
            .visit_recovery_payload(&mut |_| Err("budget".into()))
            .is_err()
    );
    let mut changed = custom.clone();
    changed.layout.defaults.real_compatibility = F::True;
    assert_ne!(custom.fingerprint(), changed.fingerprint());
    let hash = |context: &ConditionalContext| {
        use std::hash::{Hash, Hasher};
        let mut state = std::collections::hash_map::DefaultHasher::new();
        context.hash(&mut state);
        state.finish()
    };
    assert_ne!(hash(&custom), hash(&changed));
    assert_ne!(hash(&ConditionalContext::default()), hash(&explicit));
    for defaults in [
        LayoutSettings {
            record_alignment: Some(1),
            ..Default::default()
        },
        LayoutSettings {
            minimum_enum_size: Some(2),
            ..Default::default()
        },
        LayoutSettings {
            long_strings: F::False,
            ..Default::default()
        },
        LayoutSettings {
            old_type_layout: F::True,
            ..Default::default()
        },
        LayoutSettings {
            real_compatibility: F::False,
            ..Default::default()
        },
    ] {
        let mut changed = custom.clone();
        changed.layout.defaults = defaults;
        assert_ne!(custom.fingerprint(), changed.fingerprint());
        assert_ne!(hash(&custom), hash(&changed));
    }
}

#[test]
fn unknown_conditions_and_imports_do_not_supply_defaults() {
    let (_temp, context) = discover(
        "<DCC_Alignment Condition=\"Unsupported()\">1</DCC_Alignment>",
        options(Some(CompilerVersion::new(36, 0))),
    );
    assert_eq!(
        context.conditional_context.layout.defaults.record_alignment,
        None
    );
    assert_eq!(
        context.conditional_context.layout.defaults.long_strings,
        F::True
    );

    let mut opts = options(Some(CompilerVersion::new(36, 0)));
    opts.platform = Some("Win32".into());
    for later_assignment in ["", "<DCC_Alignment>4</DCC_Alignment>"] {
        let (_temp, context) = discover(
            &format!(
                "</PropertyGroup><Import Project=\"unknown.props\"/><PropertyGroup>{later_assignment}"
            ),
            opts.clone(),
        );
        assert_eq!(
            context.conditional_context.layout.platform,
            Some(LayoutPlatform::Win32)
        );
        assert_eq!(
            context.conditional_context.layout.defaults.record_alignment,
            if later_assignment.is_empty() {
                None
            } else {
                Some(4)
            }
        );
        assert_eq!(
            context.conditional_context.layout.defaults.long_strings,
            F::Unknown
        );
        assert_eq!(
            context
                .library_conditional_context
                .layout
                .defaults
                .record_alignment,
            Some(8)
        );
        assert_eq!(
            context
                .library_conditional_context
                .layout
                .defaults
                .long_strings,
            F::True
        );
    }
}

#[test]
fn bounded_property_values_are_data_without_compiler_defaults() {
    for (alignment, enum_size, long_strings, expected) in [
        ("1", "1", "true", F::True),
        ("2", "2", "0", F::False),
        ("4", "4", "+", F::True),
        ("8", "2", "off", F::False),
    ] {
        let (_temp, context) = discover(
            &format!(
                "<DCC_Alignment>{alignment}</DCC_Alignment><DCC_MinimumEnumSize>{enum_size}</DCC_MinimumEnumSize><DCC_LongStrings>{long_strings}</DCC_LongStrings>"
            ),
            options(None),
        );
        assert_eq!(
            context.conditional_context.layout.defaults.record_alignment,
            alignment.parse().ok()
        );
        assert_eq!(
            context
                .conditional_context
                .layout
                .defaults
                .minimum_enum_size,
            enum_size.parse().ok()
        );
        assert_eq!(
            context.conditional_context.layout.defaults.long_strings,
            expected
        );
    }
}

#[test]
fn standalone_layout_uses_only_explicit_target_facts() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("Alone.pas");
    fs::write(
        &source,
        "unit Alone; interface {$DEFINE WIN64} implementation end.",
    )
    .unwrap();
    let mut opts = options(Some(CompilerVersion::new(36, 0)));
    let discover_source = |opts: &ProjectOptions| {
        ProjectContext::discover_with_overrides(
            &source,
            &[temp.path().to_path_buf()],
            opts,
            &OverrideSession::default(),
        )
        .unwrap()
    };
    assert_eq!(
        discover_source(&opts).conditional_context.layout,
        LayoutContext::default()
    );
    opts.platform = Some("x64".into());
    let context = discover_source(&opts);
    assert_eq!(
        context.conditional_context.layout.platform,
        Some(LayoutPlatform::Win64)
    );
    assert_eq!(
        context.conditional_context.layout.defaults.record_alignment,
        Some(8)
    );
    assert_eq!(
        context.conditional_context.layout,
        context.library_conditional_context.layout
    );
    opts.platform = Some("Linux64".into());
    let context = discover_source(&opts);
    assert_eq!(
        context.conditional_context.layout.platform,
        Some(LayoutPlatform::Other("Linux64".into()))
    );
    assert_eq!(
        context.conditional_context.layout.defaults,
        LayoutSettings::default()
    );
    opts.conditional_context = opts
        .conditional_context
        .with_layout(LayoutContext::default());
    assert_eq!(
        discover_source(&opts).conditional_context.layout,
        LayoutContext::default()
    );
}
