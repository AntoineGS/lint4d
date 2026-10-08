//! Regression boundary for layout transport into the conditional evaluator.
use pascal_core::NoCancellation;
use pascal_core::conditional::{
    ConditionalEnvironment, IncludeTransition, Truth, analyze_with_include_callback_context,
};
use pascal_project::{
    CompilerVersion, ConditionalContext, ConditionalFact, LayoutContext, LayoutPlatform,
    LayoutSettings,
};

fn assert_transport(context: &ConditionalContext) {
    let mut environment = ConditionalEnvironment::from_context(context);
    assert_eq!(environment.to_context().layout, context.layout);
    assert_eq!(
        environment.to_context().layout_explicit,
        context.layout_explicit
    );
    let mut seen = 0;
    let analysis = analyze_with_include_callback_context(
        "{$IF Mystery}{$DEFINE A}{$ELSE}{$DEFINE B}{$ENDIF}{$I child.inc}",
        &mut environment,
        &NoCancellation,
        &mut |_, environment| {
            seen += 1;
            let child_context = environment.to_context();
            assert_eq!(child_context.layout, context.layout);
            assert_eq!(child_context.layout_explicit, context.layout_explicit);
            // Exercise the public environment/context transition, not only a
            // direct clone: this used to erase source-entry layout provenance.
            *environment = ConditionalEnvironment::from_context(&child_context);
            IncludeTransition {
                complete: true,
                environment_known: true,
            }
        },
    );
    assert!(analysis.complete);
    assert_eq!(seen, 1);
    assert_eq!(environment.to_context().layout, context.layout);
    assert_eq!(
        environment.to_context().layout_explicit,
        context.layout_explicit
    );
}

#[test]
fn source_specific_project_and_library_layout_survive_branch_and_include_transitions() {
    for platform in [LayoutPlatform::Win32, LayoutPlatform::Win64] {
        // Source-specific contexts are supplied by pascal-project. The core
        // evaluator must transport each independently, never rederive defaults.
        for (alignment, enums, strings) in [
            (Some(1), Some(4), ConditionalFact::False),
            (Some(8), None, ConditionalFact::True),
        ] {
            let context = ConditionalContext {
                compiler_version: Some(CompilerVersion::new(36, 0)),
                layout: LayoutContext {
                    platform: Some(platform.clone()),
                    defaults: LayoutSettings {
                        record_alignment: alignment,
                        minimum_enum_size: enums,
                        long_strings: strings,
                        ..Default::default()
                    },
                },
                layout_explicit: false,
                ..Default::default()
            };
            assert_transport(&context);
        }
    }
}

#[test]
fn explicit_unknown_layout_survives_environment_context_round_trip() {
    let context = ConditionalContext::default()
        .with_compiler_version(CompilerVersion::new(36, 0))
        .with_layout(LayoutContext::default());
    assert!(context.layout_explicit);
    assert_transport(&context);
}

#[test]
fn environment_identity_includes_target_settings_and_explicit_provenance() {
    let plain = ConditionalContext::default();
    let explicit_unknown = plain.clone().with_layout(LayoutContext::default());
    let win32 = plain.clone().with_layout(LayoutContext {
        platform: Some(LayoutPlatform::Win32),
        ..Default::default()
    });
    let win64 = plain.clone().with_layout(LayoutContext {
        platform: Some(LayoutPlatform::Win64),
        ..Default::default()
    });
    let aligned = plain.clone().with_layout(LayoutContext {
        defaults: LayoutSettings {
            record_alignment: Some(1),
            ..Default::default()
        },
        ..Default::default()
    });
    let base = ConditionalEnvironment::from_context(&plain).fingerprint();
    for context in [&explicit_unknown, &win32, &win64, &aligned] {
        let environment = ConditionalEnvironment::from_context(context);
        assert!(environment.has_facts());
        assert_ne!(base, environment.fingerprint());
        assert_eq!(environment.fingerprint(), environment.clone().fingerprint());
    }
    assert_ne!(
        ConditionalEnvironment::from_context(&win32).fingerprint(),
        ConditionalEnvironment::from_context(&win64).fingerprint()
    );
}

#[test]
fn unsupported_platform_payload_is_admitted_before_environment_clone() {
    let oversized = ConditionalContext::default().with_layout(LayoutContext {
        platform: Some(LayoutPlatform::Other("x".repeat(1024 * 1024 + 1))),
        ..Default::default()
    });
    assert!(ConditionalEnvironment::try_from_context(&oversized).is_none());
    let small = ConditionalContext::default().with_layout(LayoutContext {
        platform: Some(LayoutPlatform::Other("Unverified".into())),
        ..Default::default()
    });
    assert_transport(&small);
}

#[test]
fn unavailable_include_does_not_erase_immutable_source_entry_layout() {
    let context = ConditionalContext::default().with_layout(LayoutContext {
        platform: Some(LayoutPlatform::Win32),
        ..Default::default()
    });
    let mut environment = ConditionalEnvironment::from_context(&context);
    let analysis = analyze_with_include_callback_context(
        "{$I missing.inc}{$IF Defined(A)}{$DEFINE B}{$ENDIF}",
        &mut environment,
        &NoCancellation,
        &mut |_, _| IncludeTransition {
            complete: true,
            environment_known: false,
        },
    );
    assert!(analysis.complete);
    assert_eq!(analysis.directives[1].condition, Some(Truth::Unknown));
    assert_eq!(environment.to_context().layout, context.layout);
    assert_eq!(
        environment.to_context().layout_explicit,
        context.layout_explicit
    );
}
