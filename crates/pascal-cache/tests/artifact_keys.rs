use pascal_cache::{
    ArtifactRef, ContextKey, ExpandedArtifactKey, IncludeOccurrence, IncludeOutcome, KeyError,
    KeyKind, LocalArtifactKey, ManifestHeader, MemberInputs, MemberKey, SOURCE_DIGEST_CONTEXT,
    SourceContent, SourceIdentity,
};
use pascal_core::compile_time::OccurrenceId;
use pascal_core::facts::{IncludeSlot, IncludeSlotOutcome, UnresolvedInclude};
use pascal_core::{LoadedSource, SourceEncoding, SourceId, SourceRevision};
use pascal_project::{
    ConditionalContext, ConditionalFact, ProjectContext, ProjectContextParts, ProjectPathEntry,
    SourceOrigin,
};
use std::path::Path;

const UNIT: &[u8] = b"unit Shared; interface implementation end.";

fn defines(value: ConditionalFact) -> ConditionalContext {
    let mut context = ConditionalContext::default();
    context.set_define("DEBUG", value);
    context
}

fn local(
    content: &[u8],
    conditional: &ConditionalContext,
    origin: SourceOrigin,
) -> LocalArtifactKey {
    LocalArtifactKey::new(&SourceContent::from_bytes(content), conditional, origin)
}

#[test]
fn source_content_records_the_digest_length_and_decoder() {
    let content = SourceContent::from_bytes(UNIT);
    assert_eq!(
        *content.digest().as_bytes(),
        blake3::derive_key(SOURCE_DIGEST_CONTEXT, UNIT)
    );
    assert_eq!(content.length(), UNIT.len() as u64);
    assert_eq!(content.encoding(), SourceEncoding::Utf8);
    assert_eq!(
        SourceContent::from_bytes(b"\xef\xbb\xbfunit A;").encoding(),
        SourceEncoding::Utf8Bom
    );
    assert_eq!(
        SourceContent::from_bytes(b"unit \xe9;").encoding(),
        SourceEncoding::Latin1
    );
}

#[test]
fn local_keys_depend_on_content_environment_and_origin_only() {
    let debug = defines(ConditionalFact::True);
    let base = local(UNIT, &debug, SourceOrigin::ProjectCompiled);
    assert_eq!(base, local(UNIT, &debug, SourceOrigin::ProjectCompiled));
    assert_eq!(
        ManifestHeader::parse(base.key().manifest()).unwrap().kind,
        KeyKind::LocalArtifact
    );
    for other in [
        local(
            b"unit Other; interface implementation end.",
            &debug,
            SourceOrigin::ProjectCompiled,
        ),
        local(
            UNIT,
            &defines(ConditionalFact::False),
            SourceOrigin::ProjectCompiled,
        ),
        local(
            UNIT,
            &defines(ConditionalFact::Unknown),
            SourceOrigin::ProjectCompiled,
        ),
        local(
            UNIT,
            &ConditionalContext::default(),
            SourceOrigin::ProjectCompiled,
        ),
        local(
            UNIT,
            &debug.clone().with_absent_define(ConditionalFact::False),
            SourceOrigin::ProjectCompiled,
        ),
        local(UNIT, &debug, SourceOrigin::Library),
    ] {
        assert_ne!(other, base);
    }
}

#[test]
fn for_source_uses_the_environment_the_context_assigns_to_the_path() {
    let context = ProjectContext::from_parts(ProjectContextParts {
        project_file: Some("/w/App.dproj".into()),
        project_source_roots: vec!["/w".into()],
        conditional_context: defines(ConditionalFact::True),
        conditional_closure: pascal_project::ConditionalClosure {
            closed: true,
            ..Default::default()
        },
        ..ProjectContextParts::default()
    });
    let content = SourceContent::from_bytes(UNIT);
    assert_eq!(
        LocalArtifactKey::for_source(&context, Path::new("/w/Unit.pas"), &content),
        LocalArtifactKey::new(
            &content,
            &defines(ConditionalFact::True).with_absent_define(ConditionalFact::False),
            SourceOrigin::ProjectCompiled,
        )
    );
    assert_eq!(
        LocalArtifactKey::for_source(&context, Path::new("/lib/Unit.pas"), &content),
        LocalArtifactKey::new(
            &content,
            &ConditionalContext::default(),
            SourceOrigin::Library
        )
    );
}

fn resolved(name: &str, content: &[u8]) -> IncludeOutcome {
    IncludeOutcome::Resolved {
        source: SourceIdentity {
            canonical: format!("/w/{name}").into(),
            logical: format!("/w/{name}").into(),
            spelling: Some(name.into()),
            uri: None,
        },
        entry: Some(ProjectPathEntry::configured("/w".into())),
        content: SourceContent::from_bytes(content),
    }
}

fn occurrence(parent: Option<usize>, start: u64, name: &str, content: &[u8]) -> IncludeOccurrence {
    IncludeOccurrence {
        parent,
        directive_range: start..start + 12,
        directive: name.into(),
        inherited: defines(ConditionalFact::True),
        outcome: resolved(name, content),
    }
}

fn expanded(occurrences: &[IncludeOccurrence]) -> ExpandedArtifactKey {
    let root = local(
        UNIT,
        &defines(ConditionalFact::True),
        SourceOrigin::ProjectCompiled,
    );
    ExpandedArtifactKey::new(&root, occurrences).unwrap()
}

#[test]
fn expansion_identity_keeps_order_repetition_and_nesting() {
    let a = occurrence(None, 10, "a.inc", b"const A = 1;");
    let b = occurrence(None, 40, "b.inc", b"const B = 1;");
    let base = expanded(&[a.clone(), b.clone()]);
    assert_eq!(
        ManifestHeader::parse(base.key().manifest()).unwrap().kind,
        KeyKind::ExpandedArtifact
    );
    assert_eq!(base, expanded(&[a.clone(), b.clone()]));

    let b_first = IncludeOccurrence {
        directive_range: 10..22,
        ..b.clone()
    };
    let a_second = IncludeOccurrence {
        directive_range: 40..52,
        ..a.clone()
    };
    let nested_b = IncludeOccurrence {
        parent: Some(0),
        directive_range: 0..12,
        ..b.clone()
    };
    let mut other_inherited = b.clone();
    other_inherited.inherited = defines(ConditionalFact::False);
    let unresolved = IncludeOccurrence {
        outcome: IncludeOutcome::Unresolved {
            reason: UnresolvedInclude::Unavailable,
        },
        ..b.clone()
    };
    let unresolved_elsewhere = IncludeOccurrence {
        outcome: IncludeOutcome::Unresolved {
            reason: UnresolvedInclude::Ambiguous {
                candidates: vec!["/w/b.inc".into(), "/v/b.inc".into()],
            },
        },
        ..b.clone()
    };
    for (name, other) in [
        ("reordered", expanded(&[b_first, a_second])),
        ("repeated", expanded(&[a.clone(), b.clone(), b.clone()])),
        ("single", expanded(std::slice::from_ref(&a))),
        ("nested", expanded(&[a.clone(), nested_b])),
        (
            "changed include",
            expanded(&[a.clone(), occurrence(None, 40, "b.inc", b"const B = 2;")]),
        ),
        ("inherited state", expanded(&[a.clone(), other_inherited])),
        ("unresolved", expanded(&[a.clone(), unresolved.clone()])),
    ] {
        assert_ne!(other, base, "{name}");
    }
    assert_ne!(
        expanded(&[a.clone(), unresolved]),
        expanded(&[a, unresolved_elsewhere])
    );
}

#[test]
#[allow(clippy::reversed_empty_ranges)]
fn malformed_topology_is_rejected() {
    let root = local(UNIT, &ConditionalContext::default(), SourceOrigin::Library);
    let own_parent = occurrence(Some(0), 0, "a.inc", b"");
    assert_eq!(
        ExpandedArtifactKey::new(&root, &[own_parent]),
        Err(KeyError::ParentNotEarlier {
            occurrence: 0,
            parent: 0
        })
    );
    let inverted = IncludeOccurrence {
        directive_range: 9..3,
        ..occurrence(None, 0, "a.inc", b"")
    };
    assert_eq!(
        ExpandedArtifactKey::new(&root, &[inverted]),
        Err(KeyError::InvertedRange { occurrence: 0 })
    );
}

#[test]
fn members_carry_paths_spelling_uri_fallback_and_context() {
    let context = ContextKey::new(&ProjectContext::from_parts(ProjectContextParts {
        project_file: Some("/w/A.dproj".into()),
        ..ProjectContextParts::default()
    }));
    let other_context = ContextKey::new(&ProjectContext::from_parts(ProjectContextParts {
        project_file: Some("/w/B.dproj".into()),
        ..ProjectContextParts::default()
    }));
    let artifact = local(
        UNIT,
        &ConditionalContext::default(),
        SourceOrigin::ProjectCompiled,
    );
    let other_artifact = local(UNIT, &ConditionalContext::default(), SourceOrigin::Library);
    let expansion = ExpandedArtifactKey::new(&artifact, &[]).unwrap();
    let source = SourceIdentity {
        canonical: "/w/Unit1.pas".into(),
        logical: "/w/Unit1.pas".into(),
        spelling: Some("Unit1".into()),
        uri: Some("file:///w/Unit1.pas".into()),
    };
    let entry = ProjectPathEntry::configured("/w".into());
    let inputs = MemberInputs {
        context: &context,
        source: &source,
        entry: Some(&entry),
        origin: SourceOrigin::ProjectCompiled,
        fallback_unit_name: None,
        artifact: Some(ArtifactRef::Local(&artifact)),
    };
    let base = MemberKey::new(&inputs);
    assert_eq!(
        ManifestHeader::parse(base.key().manifest()).unwrap().kind,
        KeyKind::Member
    );

    let logical = SourceIdentity {
        logical: "/w/link/Unit1.pas".into(),
        ..source.clone()
    };
    let case = SourceIdentity {
        canonical: "/w/UNIT1.pas".into(),
        ..source.clone()
    };
    let spelling = SourceIdentity {
        spelling: Some("unit1".into()),
        ..source.clone()
    };
    let uri = SourceIdentity {
        uri: Some("file:///w/unit1.pas".into()),
        ..source.clone()
    };
    let legacy = ProjectPathEntry::legacy("/w".into());
    for (name, other) in [
        (
            "context",
            MemberInputs {
                context: &other_context,
                ..inputs
            },
        ),
        (
            "logical path",
            MemberInputs {
                source: &logical,
                ..inputs
            },
        ),
        (
            "case spelling",
            MemberInputs {
                source: &case,
                ..inputs
            },
        ),
        (
            "reference spelling",
            MemberInputs {
                source: &spelling,
                ..inputs
            },
        ),
        (
            "uri",
            MemberInputs {
                source: &uri,
                ..inputs
            },
        ),
        (
            "entry provenance",
            MemberInputs {
                entry: Some(&legacy),
                ..inputs
            },
        ),
        (
            "no entry",
            MemberInputs {
                entry: None,
                ..inputs
            },
        ),
        (
            "origin",
            MemberInputs {
                origin: SourceOrigin::Library,
                ..inputs
            },
        ),
        (
            "fallback name",
            MemberInputs {
                fallback_unit_name: Some("Unit1"),
                ..inputs
            },
        ),
        (
            "other artifact",
            MemberInputs {
                artifact: Some(ArtifactRef::Local(&other_artifact)),
                ..inputs
            },
        ),
        (
            "expanded artifact",
            MemberInputs {
                artifact: Some(ArtifactRef::Expanded(&expansion)),
                ..inputs
            },
        ),
        (
            "no artifact",
            MemberInputs {
                artifact: None,
                ..inputs
            },
        ),
    ] {
        assert_ne!(MemberKey::new(&other), base, "{name}");
    }
}

#[test]
fn expanded_keys_remember_their_root() {
    let root = local(
        UNIT,
        &ConditionalContext::default(),
        SourceOrigin::ProjectCompiled,
    );
    let expanded = ExpandedArtifactKey::new(&root, &[]).unwrap();
    assert_eq!(expanded.root_digest(), root.digest());
}

fn source(path: &str, text: &str, revision: Option<SourceRevision>) -> LoadedSource {
    LoadedSource {
        id: SourceId::new(path),
        path: path.into(),
        bytes: text.as_bytes().into(),
        decoded_text: Some(text.into()),
        revision: revision.unwrap_or_else(|| SourceRevision::Disk {
            stamp: pascal_project::ProjectReadStamp {
                bytes: text.len() as u64,
                modified: None,
                is_dir: false,
                is_symlink: false,
                change: None,
                settled: true,
            },
            content_hash: pascal_project::content_hash_bytes(text.as_bytes()),
            read_policy: Default::default(),
            path_entry: ProjectPathEntry::configured("/w".into()),
        }),
        compile_time_backing: None,
    }
}

#[test]
fn session_slots_key_like_the_equivalent_occurrences() {
    let inherited = defines(ConditionalFact::True);
    let root = local(UNIT, &inherited, SourceOrigin::ProjectCompiled);
    let a = source("/w/a.inc", "{$I b.inc} const A = 1;", None);
    let b = source("/w/b.inc", "const B = 1;", None);
    let slots = [
        IncludeSlot {
            parent: 0,
            directive: 10..20,
            written: "{$I a.inc}",
            outcome: IncludeSlotOutcome::Expanded {
                occurrence: OccurrenceId(1),
                source: &a,
            },
        },
        IncludeSlot {
            parent: 1,
            directive: 0..10,
            written: "{$I b.inc}",
            outcome: IncludeSlotOutcome::Expanded {
                occurrence: OccurrenceId(2),
                source: &b,
            },
        },
        IncludeSlot {
            parent: 0,
            directive: 30..46,
            written: "{$I missing.inc}",
            outcome: IncludeSlotOutcome::Unresolved(UnresolvedInclude::Unavailable),
        },
    ];
    let resolved = |path: &str, text: &str| IncludeOutcome::Resolved {
        source: SourceIdentity {
            canonical: path.into(),
            logical: path.into(),
            spelling: None,
            uri: None,
        },
        entry: Some(ProjectPathEntry::configured("/w".into())),
        content: SourceContent::from_bytes(text.as_bytes()),
    };
    let occurrences = [
        IncludeOccurrence {
            parent: None,
            directive_range: 10..20,
            directive: "{$I a.inc}".into(),
            inherited: inherited.clone(),
            outcome: resolved("/w/a.inc", "{$I b.inc} const A = 1;"),
        },
        IncludeOccurrence {
            parent: Some(0),
            directive_range: 0..10,
            directive: "{$I b.inc}".into(),
            inherited: inherited.clone(),
            outcome: resolved("/w/b.inc", "const B = 1;"),
        },
        IncludeOccurrence {
            parent: None,
            directive_range: 30..46,
            directive: "{$I missing.inc}".into(),
            inherited: inherited.clone(),
            outcome: IncludeOutcome::Unresolved {
                reason: UnresolvedInclude::Unavailable,
            },
        },
    ];
    let from_slots = ExpandedArtifactKey::from_slots(&root, &inherited, &slots).unwrap();
    assert_eq!(
        from_slots,
        ExpandedArtifactKey::new(&root, &occurrences).unwrap()
    );
    assert_eq!(from_slots.root_digest(), root.digest());
}

#[test]
fn an_overlay_include_has_no_persistent_expansion_key() {
    let root = local(
        UNIT,
        &ConditionalContext::default(),
        SourceOrigin::ProjectCompiled,
    );
    let edited = source(
        "/w/a.inc",
        "const A = 1;",
        Some(SourceRevision::Overlay {
            version: 3,
            content_hash: 0,
        }),
    );
    let slots = [IncludeSlot {
        parent: 0,
        directive: 0..10,
        written: "{$I a.inc}",
        outcome: IncludeSlotOutcome::Expanded {
            occurrence: OccurrenceId(1),
            source: &edited,
        },
    }];
    assert_eq!(
        ExpandedArtifactKey::from_slots(&root, &ConditionalContext::default(), &slots),
        Err(KeyError::OverlayInclude { occurrence: 0 })
    );
}

#[test]
fn unresolved_reasons_are_typed_parts_of_the_identity() {
    let root = local(UNIT, &ConditionalContext::default(), SourceOrigin::Library);
    let key = |reason: UnresolvedInclude| {
        ExpandedArtifactKey::new(
            &root,
            &[IncludeOccurrence {
                parent: None,
                directive_range: 0..10,
                directive: "{$I x.inc}".into(),
                inherited: ConditionalContext::default(),
                outcome: IncludeOutcome::Unresolved { reason },
            }],
        )
        .unwrap()
    };
    let keys = [
        key(UnresolvedInclude::Unavailable),
        key(UnresolvedInclude::NotExpanded),
        key(UnresolvedInclude::Ambiguous {
            candidates: vec!["/a/x.inc".into(), "/b/x.inc".into()],
        }),
        key(UnresolvedInclude::Ambiguous {
            candidates: vec!["/a/x.inc".into(), "/c/x.inc".into()],
        }),
        key(UnresolvedInclude::Incomplete {
            candidates: vec!["/a/x.inc".into(), "/b/x.inc".into()],
        }),
    ];
    for (index, left) in keys.iter().enumerate() {
        for right in &keys[index + 1..] {
            assert_ne!(left, right);
        }
    }
    assert_eq!(keys[0], key(UnresolvedInclude::Unavailable));
}
