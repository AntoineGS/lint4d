//! l4d-onnm (D46): an imported unit whose interface is unknown cannot
//! declare a name that never occurs as an identifier in its interface text
//! or the includes it could expand there, so it does not stop
//! `CompilerVersion`, `RTLVersion` or a constant from being proven.
#[path = "support/compile_time.rs"]
mod support;
use pascal_core::conditional::Truth;
use pascal_project::LayoutPlatform;
use support::{Fixture, activity, context};

const ROOT: &str = "unit Main; interface uses P; \
    {$IF CompilerVersion >= 20}{$DEFINE HIT}{$ENDIF} implementation end.";

/// A provider interface past `max_branch_states`: seven independent
/// unknown conditionals that each declare something.
fn branch_limited(extra: &str) -> String {
    let mut text = String::from("unit P; interface ");
    for n in 0..7 {
        text.push_str(&format!("{{$IFDEF U{n}}} const C{n} = {n}; {{$ENDIF}} "));
    }
    text.push_str(extra);
    text.push_str(" implementation const Impl = 1; end.");
    text
}

fn fixture(root: &str, provider: &str) -> Fixture {
    let mut f = Fixture::new(root, context(LayoutPlatform::Win32));
    f.unit("P", provider, context(LayoutPlatform::Win32));
    f
}

fn hit(f: &mut Fixture) -> Truth {
    let a = f.analyze().unwrap();
    activity(&a, "DEFINE HIT")
}

#[test]
fn the_provider_fixture_is_past_the_branch_state_limit() {
    let mut f = fixture(ROOT, &branch_limited(""));
    let a = f.analyze().unwrap();
    assert!(
        a.warnings
            .iter()
            .any(|w| w.contains("P.pas is unknown") && w.contains("max_branch_states")),
        "{:?}",
        a.warnings
    );
}

#[test]
fn an_unknown_provider_that_never_mentions_compiler_version_leaves_it_known() {
    let mut f = fixture(
        ROOT,
        &branch_limited(
            "{$I p.inc} // CompilerVersion in a comment\n \
             { CompilerVersion } (* CompilerVersion *) \
             {$IF CompilerVersion >= 20} const S = 'CompilerVersion'; {$IFEND} \
             const CompilerVersionX = 1; XCompilerVersion = 2;",
        ),
    );
    f.include("p.inc", "const FromInclude = 1; {$I q.inc}");
    f.include("q.inc", "type TQ = Byte;");
    assert_eq!(hit(&mut f), Truth::True);
}

#[test]
fn an_unknown_provider_whose_include_mentions_compiler_version_keeps_it_unknown() {
    let mut f = fixture(ROOT, &branch_limited("{$I p.inc}"));
    f.include("p.inc", "{$IFDEF U9} const CompilerVersion = 1; {$ENDIF}");
    assert_eq!(hit(&mut f), Truth::Unknown);
}

#[test]
fn a_nested_include_that_mentions_compiler_version_keeps_it_unknown() {
    let mut f = fixture(ROOT, &branch_limited("{$I p.inc}"));
    f.include("p.inc", "{$INCLUDE q.inc}");
    f.include("q.inc", "const CompilerVersion = 1;");
    assert_eq!(hit(&mut f), Truth::Unknown);
}

#[test]
fn escaped_and_case_variants_of_the_name_keep_it_unknown() {
    for spelling in [
        "&CompilerVersion",
        "COMPILERVERSION",
        "compilerversion",
        "&compilerVERSION",
    ] {
        let mut f = fixture(
            ROOT,
            &branch_limited(&format!("{{$IFDEF U9}} const {spelling} = 1; {{$ENDIF}}")),
        );
        assert_eq!(hit(&mut f), Truth::Unknown, "{spelling}");
    }
}

#[test]
fn a_mention_after_the_implementation_keyword_does_not_count() {
    let provider = branch_limited("").replace(
        "implementation const Impl = 1;",
        "implementation const CompilerVersion = 1;",
    );
    let mut f = fixture(ROOT, &provider);
    assert_eq!(hit(&mut f), Truth::True);
}

#[test]
fn a_conditional_implementation_keyword_does_not_end_the_scan() {
    let provider = branch_limited(
        "{$IFDEF U8} implementation {$ELSE} const CompilerVersion = 1; implementation {$ENDIF}",
    )
    .replace(" implementation const Impl = 1; end.", " end.");
    let mut f = fixture(ROOT, &provider);
    assert_eq!(hit(&mut f), Truth::Unknown);
}

#[test]
fn an_unresolved_include_in_the_provider_keeps_it_unknown() {
    let mut f = fixture(ROOT, &branch_limited("{$I missing.inc}"));
    assert_eq!(hit(&mut f), Truth::Unknown);
    let mut f = fixture(ROOT, &branch_limited("{$I p.inc}"));
    f.include("p.inc", "{$I missing.inc}");
    assert_eq!(hit(&mut f), Truth::Unknown);
}

#[test]
fn an_unterminated_comment_or_string_in_the_provider_keeps_it_unknown() {
    for tail in ["{ never closed", "const S = 'never closed"] {
        let provider = branch_limited("").replace(" implementation const Impl = 1; end.", tail);
        let mut f = fixture(ROOT, &provider);
        assert_eq!(hit(&mut f), Truth::Unknown, "{tail}");
    }
}

#[test]
fn a_non_ascii_letter_that_folds_to_ascii_keeps_it_unknown() {
    // U+017F LATIN SMALL LETTER LONG S uppercases to `S`; U+212A KELVIN SIGN
    // lowercases to `k`. Accented letters that fold to no ASCII letter do not
    // block the proof.
    let mut f = fixture(
        ROOT,
        &branch_limited("{$IFDEF U9} const Compilerver\u{17F}ion = 1; {$ENDIF}"),
    );
    assert_eq!(hit(&mut f), Truth::Unknown);
    let mut f = fixture(ROOT, &branch_limited("const Été = 1; // déjà vu"));
    assert_eq!(hit(&mut f), Truth::True);
}

#[test]
fn rtl_version_from_system_is_known_past_an_unknown_provider_that_never_mentions_it() {
    const RTL_ROOT: &str = "unit Main; interface uses System, P; \
        {$IF RTLVersion >= 20}{$DEFINE HIT}{$ENDIF} implementation end.";
    let mut f = fixture(RTL_ROOT, &branch_limited(""));
    f.unit(
        "System",
        "unit System; interface const RTLVersion = 21.0; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(hit(&mut f), Truth::True);

    let mut f = fixture(
        RTL_ROOT,
        &branch_limited("{$IFDEF U9} const RTLVersion = 1; {$ENDIF}"),
    );
    f.unit(
        "System",
        "unit System; interface const RTLVersion = 21.0; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(hit(&mut f), Truth::Unknown);
}

#[test]
fn a_constant_is_known_past_an_unknown_provider_that_never_mentions_it() {
    const CONST_ROOT: &str = "unit Main; interface uses Q, P; \
        {$IF Value = 8}{$DEFINE HIT}{$ENDIF} implementation end.";
    let mut f = fixture(CONST_ROOT, &branch_limited(""));
    f.unit(
        "Q",
        "unit Q; interface const Value = 8; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(hit(&mut f), Truth::True);
}

#[test]
fn an_unknown_activity_provider_interface_is_proven_by_the_scan_too() {
    // Not past the branch limit: one unknown conditional makes the
    // interface graph unknown.
    let provider = "unit P; interface {$IFDEF U0} const C0 = 0; {$ENDIF} implementation end.";
    let mut f = fixture(ROOT, provider);
    assert_eq!(hit(&mut f), Truth::True);
    let provider =
        "unit P; interface {$IFDEF U0} const CompilerVersion = 0; {$ENDIF} implementation end.";
    let mut f = fixture(ROOT, provider);
    assert_eq!(hit(&mut f), Truth::Unknown);
}

/// The shape of OmniThreadLibrary's MDCompilers.inc: a chain of
/// `CompilerVersion` comparisons, in a unit that imports a provider past the
/// branch-state limit. Every comparison is decided, so nothing forks.
#[test]
fn a_compiler_version_chain_past_an_unknown_provider_does_not_fork() {
    let mut chain = String::new();
    for version in 14..60 {
        chain.push_str(&format!(
            "{{$IF CompilerVersion >= {version}}}{{$DEFINE VER{version}UP}}{{$IFEND}}\n\
             {{$IF CompilerVersion = {version}}}{{$DEFINE VER{version}}}{{$IFEND}}\n"
        ));
    }
    let root = "unit Main; interface uses P; {$I MDCompilers.inc} \
        {$IFDEF VER37UP}{$DEFINE HIT}{$ENDIF} implementation end.";
    let mut f = fixture(root, &branch_limited(""));
    f.include("MDCompilers.inc", &chain);
    let a = f.analyze().unwrap();
    assert!(a.root.complete, "{:?}", a.reasons);
    assert!(
        !format!("{:?}", a.reasons).contains("max_branch_states"),
        "{:?}",
        a.reasons
    );
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
}

/// A store that keeps compiled interfaces and checks each dependency's
/// overlay version against the current one, as a real store's probes do.
#[derive(Default)]
struct Store {
    graphs: std::sync::Mutex<
        std::collections::HashMap<
            pascal_core::compile_time::ProviderGraphKey,
            std::sync::Arc<pascal_core::compile_time::SharedProviderGraph>,
        >,
    >,
    current: std::sync::Mutex<std::collections::HashMap<String, i32>>,
}

impl pascal_core::compile_time::SharedProviderGraphs for Store {
    fn get(
        &self,
        key: &pascal_core::compile_time::ProviderGraphKey,
    ) -> Option<std::sync::Arc<pascal_core::compile_time::SharedProviderGraph>> {
        let current = self.current.lock().unwrap();
        self.graphs
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .filter(|graph| {
                graph.dependencies.iter().all(|d| {
                    current
                        .get(d.source.as_str())
                        .is_none_or(|version| d.overlay_version == Some(*version))
                })
            })
    }
    fn put(
        &self,
        key: pascal_core::compile_time::ProviderGraphKey,
        graph: std::sync::Arc<pascal_core::compile_time::SharedProviderGraph>,
    ) {
        self.graphs.lock().unwrap().insert(key, graph);
    }
}

#[test]
fn the_names_an_unknown_interface_holds_are_shared_and_validated_with_it() {
    let store = std::sync::Arc::new(Store::default());
    let shared = |include: &str, version: i32| {
        let mut f = fixture(ROOT, &branch_limited("{$I p.inc}"));
        f.include("p.inc", include);
        f.include_revision("p.inc", version);
        f.shared = Some(store.clone());
        f
    };
    let mut first = shared("const FromInclude = 1;", 1);
    assert_eq!(hit(&mut first), Truth::True);
    assert!(first.read_names().iter().any(|n| n == "p.inc"));
    let stored = store
        .graphs
        .lock()
        .unwrap()
        .values()
        .next()
        .cloned()
        .unwrap();
    assert!(stored.graph.is_none());
    assert!(
        stored.names.is_some(),
        "the names are stored with the interface"
    );
    assert!(
        stored
            .dependencies
            .iter()
            .any(|d| d.source.as_str() == "/fixture/p.inc"),
        "the include the scan read is a dependency: {:?}",
        stored.dependencies
    );

    let mut second = shared("const FromInclude = 1;", 1);
    assert_eq!(hit(&mut second), Truth::True);
    assert!(
        !second.read_names().iter().any(|n| n == "p.inc"),
        "a reuse does not scan again: {:?}",
        second.read_names()
    );

    // The include changes: the stored interface is stale, and the new text
    // mentions the name.
    store
        .current
        .lock()
        .unwrap()
        .insert("/fixture/p.inc".into(), 2);
    let mut third = shared("{$IFDEF U9} const CompilerVersion = 1; {$ENDIF}", 2);
    assert_eq!(hit(&mut third), Truth::Unknown);
}

/// Fix round 1 (I1): a conditional an include opens and the unit closes
/// hides the `implementation` that follows it, so the scan cannot stop there.
#[test]
fn an_implementation_inside_a_conditional_an_include_opened_does_not_end_the_scan() {
    let provider = "unit P; interface {$I open.inc} implementation {$ENDIF} \
        const CompilerVersion = 1; implementation end.";
    let mut f = fixture(ROOT, provider);
    f.include("open.inc", "{$IFDEF U1}");
    assert_eq!(hit(&mut f), Truth::Unknown);
}

/// Fix round 1 (minor): `implementation` after a `.` is not the section
/// keyword.
#[test]
fn a_qualified_implementation_word_does_not_end_the_scan() {
    let mut f = fixture(
        ROOT,
        &branch_limited(
            "const X = Foo.implementation; {$IFDEF U9} const CompilerVersion = 1; {$ENDIF}",
        ),
    );
    assert_eq!(hit(&mut f), Truth::Unknown);
}
