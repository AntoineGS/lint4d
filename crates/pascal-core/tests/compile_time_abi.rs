//! Literal ABI expectations: changing a scalar mapping, guessing legacy alignment,
//! or admitting over-budget payloads must fail these tests.
use pascal_core::compile_time::*;
use pascal_core::{NoCancellation, SourceId};
use pascal_project::{
    CompilerVersion, ConditionalContext, ConditionalFact, LayoutContext, LayoutPlatform,
    LayoutSettings, TargetPlatform,
};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

fn context(version: CompilerVersion, platform: TargetPlatform) -> ConditionalContext {
    ConditionalContext::default()
        .with_compiler_version(version)
        .with_layout(LayoutContext::for_target(Some(version), Some(&platform)))
}

fn registry() -> Vec<(&'static str, BuiltinType)> {
    use BuiltinType::*;
    vec![
        ("Byte", Byte),
        ("ShortInt", ShortInt),
        ("AnsiChar", AnsiChar),
        ("Boolean", Boolean),
        ("Word", Word),
        ("SmallInt", SmallInt),
        ("WideChar", WideChar),
        ("Integer", Integer),
        ("LongInt", LongInt),
        ("Cardinal", Cardinal),
        ("LongWord", LongWord),
        ("Single", Single),
        ("Int64", Int64),
        ("UInt64", UInt64),
        ("Double", Double),
        ("Currency", Currency),
        ("Pointer", Pointer),
        ("NativeInt", NativeInt),
        ("NativeUInt", NativeUInt),
        ("DynamicArrayReference", DynamicArrayReference),
        ("Extended", Extended),
        ("Char", Char),
        ("String", String),
        ("ShortString", ShortString),
        ("ByteBool", ByteBool),
        ("WordBool", WordBool),
        ("LongBool", LongBool),
        ("Real", Real),
        ("Real48", Real48),
        ("AnsiString", AnsiString),
        ("UnicodeString", UnicodeString),
        ("WideString", WideString),
    ]
}

#[derive(Deserialize)]
struct Fixtures {
    fixture: Vec<Fixture>,
    references: HashMap<String, String>,
}
#[derive(Deserialize)]
struct Fixture {
    compiler: String,
    platform: String,
    switches: String,
    expression: String,
    expected_size: u64,
    expected_alignment: Option<u32>,
    reference: String,
}

#[test]
fn referenced_abi_fixtures_cover_each_scalar_on_both_windows_targets() {
    let fixtures: Fixtures =
        toml::from_str(include_str!("fixtures/compile_time/abi.toml")).unwrap();
    let types = registry();
    let mut covered = HashSet::new();
    for fixture in fixtures.fixture {
        let version = match fixture.compiler.as_str() {
            "18.5" => CompilerVersion::new(18, 5),
            "36.0" => CompilerVersion::new(36, 0),
            "37.0" => CompilerVersion::new(37, 0),
            other => panic!("unrecognized fixture compiler {other}"),
        };
        let target = match fixture.platform.as_str() {
            "Win32" => TargetPlatform::Win32,
            "Win64" => TargetPlatform::Win64,
            other => panic!("unrecognized fixture target {other}"),
        };
        let ctx = context(version, target);
        let mut settings = ctx.layout.defaults.clone();
        match fixture.switches.as_str() {
            "unknown" => {}
            "H+" => settings.long_strings = ConditionalFact::True,
            "H-" => settings.long_strings = ConditionalFact::False,
            "REALCOMPATIBILITY OFF" => settings.real_compatibility = ConditionalFact::False,
            "REALCOMPATIBILITY ON" => settings.real_compatibility = ConditionalFact::True,
            other => panic!("unrecognized switches {other}"),
        }
        let name = fixture
            .expression
            .strip_prefix("SizeOf(")
            .unwrap()
            .strip_suffix(')')
            .unwrap();
        let ty = types
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .unwrap()
            .1;
        let reference = fixtures.references.get(&fixture.reference).unwrap();
        assert!(reference.contains("http"), "missing traceable reference");
        let actual = builtin_layout(ty, &ctx, &settings);
        if let Some(alignment) = fixture.expected_alignment {
            assert_eq!(
                actual,
                Fact::Known(StorageLayout {
                    size: fixture.expected_size,
                    alignment
                }),
                "{} {} {}",
                fixture.compiler,
                fixture.platform,
                fixture.expression
            );
        } else {
            assert_eq!(
                actual,
                Fact::Unknown(UnknownReason::UnsupportedLayout),
                "unverified alignment"
            );
        }
        if fixture.compiler == "37.0" {
            covered.insert((fixture.platform, name.to_owned()));
        }
    }
    for (name, _) in types {
        for platform in ["Win32", "Win64"] {
            assert!(
                covered.contains(&(platform.to_owned(), name.to_owned())),
                "missing {platform} {name}"
            );
        }
    }
}

#[test]
fn independent_widths_do_not_require_irrelevant_layout_switches_or_mutable_defines() {
    let mut ctx = context(CompilerVersion::new(37, 0), TargetPlatform::Win32);
    assert_eq!(ctx.layout.defaults, LayoutSettings::default());
    ctx.defines.insert("WIN64".into(), ConditionalFact::True);
    for (ty, size, alignment) in [
        (BuiltinType::Byte, 1, 1),
        (BuiltinType::Extended, 10, 8),
        (BuiltinType::Pointer, 4, 4),
    ] {
        assert_eq!(
            builtin_layout(ty, &ctx, &ctx.layout.defaults).known(),
            Some(StorageLayout { size, alignment })
        );
    }
    assert_eq!(
        builtin_layout(BuiltinType::String, &ctx, &ctx.layout.defaults),
        Fact::Unknown(UnknownReason::UnsupportedLayout)
    );
    assert_eq!(
        builtin_layout(BuiltinType::Real, &ctx, &ctx.layout.defaults),
        Fact::Unknown(UnknownReason::UnsupportedLayout)
    );
}

#[test]
fn absent_invalid_future_or_unsupported_build_facts_never_produce_precise_layouts() {
    let settings = LayoutSettings::default();
    let empty = ConditionalContext::default();
    assert_eq!(
        builtin_layout(BuiltinType::Char, &empty, &settings),
        Fact::Unknown(UnknownReason::UnsupportedVersion)
    );
    for version in [
        CompilerVersion::new(38, 0),
        CompilerVersion::new(37, 1),
        CompilerVersion::with_patch(37, 0, 1),
    ] {
        let ctx = context(version, TargetPlatform::Win32);
        assert_eq!(
            builtin_layout(BuiltinType::Byte, &ctx, &settings),
            Fact::Unknown(UnknownReason::UnsupportedVersion)
        );
    }
    let mut ctx = context(CompilerVersion::new(37, 0), TargetPlatform::Win64);
    ctx.layout.platform = Some(LayoutPlatform::Other("Linux64".into()));
    for (_, ty) in registry() {
        assert_eq!(
            builtin_layout(ty, &ctx, &settings),
            Fact::Unknown(UnknownReason::UnsupportedTarget)
        );
    }
    ctx.layout.platform = None;
    assert_eq!(
        builtin_layout(BuiltinType::Pointer, &ctx, &settings),
        Fact::Unknown(UnknownReason::MissingTarget)
    );
    // The version proves these target-independent ordinal facts without a target.
    assert_eq!(
        builtin_layout(BuiltinType::Byte, &ctx, &settings).known(),
        Some(StorageLayout {
            size: 1,
            alignment: 1
        })
    );
    assert_eq!(
        builtin_layout(BuiltinType::Char, &ctx, &settings).known(),
        Some(StorageLayout {
            size: 2,
            alignment: 2
        })
    );
    let old = context(CompilerVersion::new(18, 5), TargetPlatform::Win64);
    assert_eq!(
        builtin_layout(BuiltinType::Byte, &old, &settings),
        Fact::Unknown(UnknownReason::UnsupportedVersion)
    );
    let old = context(CompilerVersion::new(18, 5), TargetPlatform::Win32);
    assert_eq!(
        builtin_layout(BuiltinType::UnicodeString, &old, &settings),
        Fact::Unknown(UnknownReason::UnsupportedVersion)
    );
}

#[test]
fn pre_unicode_native_integer_storage_is_not_assumed_to_match_pointer_storage() {
    // NativeInt/NativeUInt were64-bit in Delphi2007, before the2009 redefinition.
    // https://docs.devart.com/odac/work-rad-studio-xe2.htm
    // https://delphidabbler.com/notes/version-features (footnote4)
    let old = context(CompilerVersion::new(18, 5), TargetPlatform::Win32);
    for ty in [
        BuiltinType::NativeInt,
        BuiltinType::NativeUInt,
        BuiltinType::UInt64,
    ] {
        assert_eq!(
            builtin_layout(ty, &old, &old.layout.defaults).known(),
            Some(StorageLayout {
                size: 8,
                alignment: 8
            })
        );
    }
    assert_eq!(
        builtin_layout(BuiltinType::Pointer, &old, &old.layout.defaults).known(),
        Some(StorageLayout {
            size: 4,
            alignment: 4
        })
    );
}

#[test]
fn facts_and_identity_carriers_preserve_unknowns_dependencies_and_occurrences() {
    assert_eq!(Fact::<u64>::Unknown(UnknownReason::Overflow).known(), None);
    assert_eq!(
        Fact::<u64>::Incomplete(IncompleteReason::Cancelled).known(),
        None
    );
    assert_eq!(Fact::Known(42).known(), Some(42));
    let source = SourceId::new("unit");
    let anchor = SourceAnchor {
        source: source.clone(),
        occurrence: OccurrenceId(1),
        range: 3..7,
    };
    let key = DefinitionKey {
        anchor: anchor.clone(),
        scope: ScopeId(2),
    };
    let mut keys = HashSet::new();
    keys.insert(key.clone());
    let mut second = key.clone();
    second.anchor.occurrence = OccurrenceId(2);
    assert!(keys.insert(second));
    let known = KnownLayout {
        storage: StorageLayout {
            size: 4,
            alignment: 4,
        },
        identity: TypeIdentity::Declared(key),
        dependencies: Vec::new(),
    };
    assert_eq!(known.clone(), known);
    let fragment = SourceFragment {
        source: &source,
        occurrence: anchor.occurrence,
        physical_start: anchor.range.start,
        text: "Byte",
    };
    assert_eq!(fragment.clone(), fragment);
    let site = BindingSite {
        scope: ScopeId(2),
        sequence: Sequence(9),
    };
    assert_eq!(site.clone(), site);
}

#[test]
fn checked_alignment_rejects_invalid_and_overflowing_rounding() {
    assert_eq!(checked_align_up(9, 8), Some(16));
    assert_eq!(checked_align_up(16, 8), Some(16));
    assert_eq!(checked_align_up(0, 8), Some(0));
    assert_eq!(checked_align_up(u64::MAX, 1), Some(u64::MAX));
    assert_eq!(checked_align_up(u64::MAX, 8), None);
    assert_eq!(checked_align_up(7, 0), None);
    assert_eq!(checked_align_up(7, 3), None);
}

#[test]
fn cancellation_and_retained_limits_reject_before_payload_processing() {
    let cancelled = AtomicBool::new(true);
    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &cancelled);
    let mut processed = false;
    let result = budget.charge(1, 4, 4).map(|()| {
        processed = true;
        "Byte".to_owned()
    });
    assert_eq!(result, Err(IncompleteReason::Cancelled));
    assert!(!processed);
    assert_eq!(
        budget.snapshot(),
        BudgetSnapshot {
            work: 0,
            byte_work: 0,
            retained_bytes: 0
        }
    );
    let limits = CompileTimeLimits {
        max_retained_bytes: 0,
        ..CompileTimeLimits::default()
    };
    let mut budget = CompileTimeBudget::new(limits, &NoCancellation);
    let result = budget.charge(1, 4, 4).map(|()| {
        processed = true;
        "Byte".to_owned()
    });
    assert_eq!(
        result,
        Err(IncompleteReason::Limit {
            name: "max_retained_bytes",
            maximum: 0
        })
    );
    assert!(!processed);
    assert_eq!(budget.snapshot().retained_bytes, 0);
}

#[test]
fn budget_admission_is_checked_transactional_and_release_does_not_refund_work() {
    let limits = CompileTimeLimits {
        max_work: 2,
        max_byte_work: 4,
        max_retained_bytes: 4,
        max_pending_bytes: 0,
        ..CompileTimeLimits::default()
    };
    let token = AtomicBool::new(false);
    let mut budget = CompileTimeBudget::new(limits, &token);
    // Generic retained storage is not pending-fragment storage.
    budget.charge(1, 4, 4).unwrap();
    let before = budget.snapshot();
    assert_eq!(
        budget.charge(0, 1, 0),
        Err(IncompleteReason::Limit {
            name: "max_byte_work",
            maximum: 4
        })
    );
    assert_eq!(
        budget.charge(0, 0, 1),
        Err(IncompleteReason::Limit {
            name: "max_retained_bytes",
            maximum: 4
        })
    );
    assert_eq!(budget.snapshot(), before);
    budget.release(4);
    assert_eq!(
        budget.snapshot(),
        BudgetSnapshot {
            work: 1,
            byte_work: 4,
            retained_bytes: 0
        }
    );
    budget.charge(1, 0, 4).unwrap();
    assert_eq!(
        budget.charge(1, 0, 0),
        Err(IncompleteReason::Limit {
            name: "max_work",
            maximum: 2
        })
    );
    token.store(true, Ordering::Relaxed);
    assert_eq!(budget.charge(0, 0, 0), Err(IncompleteReason::Cancelled));
    for dimension in 0..3 {
        let limits = CompileTimeLimits {
            max_work: usize::MAX,
            max_byte_work: usize::MAX,
            max_retained_bytes: usize::MAX,
            ..CompileTimeLimits::default()
        };
        let mut budget = CompileTimeBudget::new(limits, &NoCancellation);
        let mut charges = [0; 3];
        charges[dimension] = usize::MAX;
        budget.charge(charges[0], charges[1], charges[2]).unwrap();
        let before = budget.snapshot();
        charges[dimension] = 1;
        assert!(matches!(
            budget.charge(charges[0], charges[1], charges[2]),
            Err(IncompleteReason::Limit { .. })
        ));
        assert_eq!(budget.snapshot(), before);
    }
}
