#[path = "support/compile_time.rs"]
mod support;
use pascal_core::facts::{
    IncludeSlot, IncludeSlotOutcome, NotPersistable, UnresolvedInclude, include_slots,
    session_persistability,
};
use pascal_core::{Resolution, ResolutionCandidate};
use pascal_project::{ConditionalFact, LayoutPlatform};
use std::path::PathBuf;
use support::{Fixture, context};

fn written<'a>(slots: &[IncludeSlot<'a>]) -> Vec<(u32, &'a str)> {
    slots
        .iter()
        .map(|slot| (slot.parent, slot.written))
        .collect()
}

#[test]
fn slots_follow_expansion_order_with_repetition_nesting_and_unresolved_barriers() {
    let mut conditional = context(LayoutPlatform::Win32);
    conditional.set_define("NEVER", ConditionalFact::False);
    let mut f = Fixture::new(
        "unit T; interface {$I a.inc} {$I a.inc} {$IFDEF NEVER}{$I gone.inc}{$ENDIF} {$I missing.inc} implementation end.",
        conditional,
    );
    f.disk_include("a.inc", "{$I b.inc} const A = 1;");
    f.disk_include("b.inc", "const B = 2;");
    let analysis = f.analyze().unwrap();
    let slots = include_slots(&analysis).unwrap();
    assert_eq!(
        written(&slots),
        vec![
            (0, "{$I a.inc}"),
            (1, "{$I b.inc}"),
            (0, "{$I a.inc}"),
            (3, "{$I b.inc}"),
            (0, "{$I missing.inc}"),
        ]
    );
    let expanded: Vec<_> = slots
        .iter()
        .filter_map(|slot| match &slot.outcome {
            IncludeSlotOutcome::Expanded { occurrence, source } => {
                Some((*occurrence, source.path.clone()))
            }
            IncludeSlotOutcome::Unresolved(_) => None,
        })
        .collect();
    assert_eq!(expanded.len(), 4);
    assert_ne!(
        expanded[0].0, expanded[2].0,
        "a repeated include is its own occurrence"
    );
    assert_eq!(expanded[1].1, PathBuf::from("/fixture/b.inc"));
    assert!(matches!(
        slots[4].outcome,
        IncludeSlotOutcome::Unresolved(UnresolvedInclude::Unavailable)
    ));
    let root = f.root.decoded_text.as_deref().unwrap();
    assert_eq!(&root[slots[0].directive.clone()], "{$I a.inc}");
    assert!(!f.read_names().iter().any(|name| name == "gone.inc"));
    assert_eq!(session_persistability(&analysis, &slots), Ok(()));
}

/// The session stops at the first include it cannot resolve, so each
/// failure kind needs its own session.
fn only_unresolved(
    text: &str,
    name: &str,
    failure: Resolution<pascal_core::LoadedSource>,
) -> UnresolvedInclude {
    let mut f = Fixture::new(text, context(LayoutPlatform::Win32));
    f.fail_include(name, failure);
    let analysis = f.analyze().unwrap();
    let slots = include_slots(&analysis).unwrap();
    assert_eq!(slots.len(), 1);
    match &slots[0].outcome {
        IncludeSlotOutcome::Unresolved(reason) => reason.clone(),
        IncludeSlotOutcome::Expanded { .. } => panic!("{:?} must be unresolved", slots[0]),
    }
}

#[test]
fn ambiguous_and_incomplete_includes_keep_typed_candidates_without_resolver_text() {
    let candidate = |path: &str| ResolutionCandidate {
        path: path.into(),
        declared_name: None,
    };
    assert_eq!(
        only_unresolved(
            "unit T; interface {$I two.inc} implementation end.",
            "two.inc",
            Resolution::Ambiguous {
                candidates: vec![
                    candidate("/fixture/x/two.inc"),
                    candidate("/fixture/y/two.inc")
                ],
            },
        ),
        UnresolvedInclude::Ambiguous {
            candidates: vec!["/fixture/x/two.inc".into(), "/fixture/y/two.inc".into()],
        }
    );
    assert_eq!(
        only_unresolved(
            "unit T; interface {$I part.inc} implementation end.",
            "part.inc",
            Resolution::Incomplete {
                reason: "Permission denied (os error 13)".into(),
                candidates: vec![candidate("/fixture/x/part.inc")],
            },
        ),
        UnresolvedInclude::Incomplete {
            candidates: vec!["/fixture/x/part.inc".into()],
        }
    );
}

#[test]
fn overlay_includes_are_not_persistable() {
    let mut f = Fixture::new(
        "unit T; interface {$I a.inc} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("a.inc", "const A = 1;");
    let analysis = f.analyze().unwrap();
    let slots = include_slots(&analysis).unwrap();
    assert_eq!(
        session_persistability(&analysis, &slots),
        Err(NotPersistable::OverlayInclude { slot: 1 })
    );
}

#[test]
fn provider_lookups_are_not_persistable() {
    let mut f = Fixture::new(
        "unit Main; interface uses Other; {$IF Other.Early}{$DEFINE EARLY}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Other",
        "unit Other; interface const Early=SizeOf(Integer)=4; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let analysis = f.analyze().unwrap();
    assert!(analysis.provider_queries > 0);
    assert_eq!(
        session_persistability(&analysis, &[]),
        Err(NotPersistable::ProviderDependent)
    );

    let mut f = Fixture::new(
        "unit T; interface {$IFDEF X}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    let analysis = f.analyze().unwrap();
    assert_eq!(analysis.provider_queries, 0);
    assert_eq!(session_persistability(&analysis, &[]), Ok(()));
}

#[test]
fn a_limited_session_is_transient() {
    let mut f = Fixture::new(
        "unit T; interface {$I prefix.inc}} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.disk_include("prefix.inc", &format!("{{$IF {}", "x".repeat(200)));
    f.limits.max_pending_bytes = 64;
    let analysis = f.analyze().unwrap();
    let slots = include_slots(&analysis).unwrap();
    assert_eq!(
        session_persistability(&analysis, &slots),
        Err(NotPersistable::Transient)
    );
}
