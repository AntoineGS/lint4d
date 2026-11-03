//! l4d-9bxx: scanning costs about one step per source byte, so the step
//! budget must not refuse large but ordinary sources, while pathological
//! work on a small source still trips it.
#[path = "support/compile_time.rs"]
mod support;
use pascal_core::compile_time::{BudgetSnapshot, CompileTimeLimits};
use pascal_core::{
    FilesystemSourceStore, NoCancellation, ResolverError, ResolverLimits, UnitResolver,
};
use pascal_project::{LayoutPlatform, ProjectContext, ProjectOptions};
use support::context;

fn resolver_with(
    root: &str,
    files: &[(&str, &str)],
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    UnitResolver<FilesystemSourceStore>,
) {
    resolver_retaining(root, files, 64 * 1024 * 1024)
}

fn resolver_retaining(
    root: &str,
    files: &[(&str, &str)],
    retained: usize,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    UnitResolver<FilesystemSourceStore>,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("T.pas");
    std::fs::write(&path, root).unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let resolver = resolver_in(dir.path(), &path, retained);
    (dir, path, resolver)
}

/// A new request's resolver over the files of `dir`.
fn resolver_in(
    dir: &std::path::Path,
    path: &std::path::Path,
    retained: usize,
) -> UnitResolver<FilesystemSourceStore> {
    let mut ctx = ProjectContext::discover(path, &[dir.into()], &ProjectOptions::default())
        .unwrap()
        .context;
    let mut parts = std::sync::Arc::unwrap_or_clone(std::mem::take(&mut ctx)).into_parts();
    parts.conditional_context = context(LayoutPlatform::Win32);
    ctx = ProjectContext::from_parts(parts).into();
    UnitResolver::new(
        ctx,
        vec![dir.into()],
        FilesystemSourceStore::new(),
        ResolverLimits {
            max_source_bytes: 16 * 1024 * 1024,
            // The LSP selects a retention cap derived from its file limits.
            max_compile_time_retained_bytes: retained,
            ..ResolverLimits::default()
        },
    )
}

fn analyze_with(
    text: &str,
    files: &[(&str, &str)],
) -> (Result<bool, ResolverError>, BudgetSnapshot) {
    analyze_retaining(text, files, 64 * 1024 * 1024)
}

fn analyze_retaining(
    text: &str,
    files: &[(&str, &str)],
    retained: usize,
) -> (Result<bool, ResolverError>, BudgetSnapshot) {
    let (_dir, path, mut resolver) = resolver_retaining(text, files, retained);
    let mut source = support::loaded("T.pas", text);
    source.path = path;
    let result = resolver
        .analyze_compile_time(&source, &NoCancellation)
        .and_then(|analysis| {
            if !analysis.root.complete {
                eprintln!("incomplete: {:?}", analysis.reasons);
            }
            // A limit recorded as an incomplete reason is a refusal too.
            match analysis.reasons.iter().find_map(|reason| match reason {
                pascal_core::compile_time::IncompleteReason::Limit { name, maximum } => {
                    Some((*name, *maximum))
                }
                _ => None,
            }) {
                Some((limit, maximum)) if limit == "max_work" || limit == "max_byte_work" => {
                    Err(ResolverError::LimitExceeded {
                        limit,
                        observed: maximum + 1,
                        maximum,
                    })
                }
                _ => Ok(analysis.root.complete),
            }
        });
    (result, resolver.compile_time_usage())
}

fn analyze(text: &str) -> (Result<bool, ResolverError>, BudgetSnapshot) {
    analyze_with(text, &[])
}

fn is_work_limit(result: &Result<bool, ResolverError>) -> bool {
    matches!(result, Err(ResolverError::LimitExceeded { limit, .. }) if *limit == "max_work")
}

/// About SynCommons.pas' size and shape: an interface of declarations and a
/// large implementation of routine bodies, comments and conditional code.
/// `statements` repeats each routine's statement block, so larger values
/// make bigger routines with the same declarations.
fn synthetic_library(bytes: usize, statements: usize) -> String {
    let block = "  case Count mod 4 of\n    0: Result := Result + 1;\n    1: Result := Result - 1;\n    2: Result := Result * 2;\n  else\n    Result := 0;\n  end;\n"
        .repeat(statements);
    let mut text = String::from(
        "unit T;\n\ninterface\n\n{$DEFINE FAST}\n{$IFDEF CPU64}\nconst Bits = 64;\n{$ELSE}\nconst Bits = 32;\n{$ENDIF}\n\ntype\n",
    );
    for n in 0..1500 {
        text.push_str(&format!(
            "  TRecord{n} = record\n    Name: string;\n    Value: Integer;\n  end;\n"
        ));
    }
    for n in 0..1500 {
        text.push_str(&format!(
            "function Routine{n}(const Text: string; Count: Integer): Integer;\n"
        ));
    }
    text.push_str("\nimplementation\n\n");
    let mut n = 0;
    while text.len() < bytes {
        let body = if n % 5 == 0 {
            "  {$IFDEF FAST}\n  Result := Count;\n  {$ELSE}\n  Result := Count * 2;\n  {$ENDIF}\n"
        } else {
            "  Result := Count;\n"
        };
        text.push_str(&format!(
            "{{ Routine{n} returns a value derived from its arguments; the comment is\n  long enough to look like ordinary documentation of a library routine. }}\n\
             function Routine{n}(const Text: string; Count: Integer): Integer;\n\
             var\n  I: Integer;\nbegin\n  // Walk the text and accumulate the result.\n\
             for I := 1 to Length(Text) do\n    if Text[I] = 'x' then\n      Inc(Count)\n    else if Text[I] = 'y' then\n      Dec(Count)\n    else\n      Count := Count + Ord(Text[I]) mod 7;\n\
             if Count > 1000 then\n    Count := Count div 2\n  else\n    Count := Count * 3;\n\
             Result := Count + Length(Text) - 1; (* recompute the result from the state *)\n\
             {block}\
             if Text <> '' then\n    Result := Result + Ord(Text[1]);\n{body}end;\n\n"
        ));
        n += 1;
    }
    text.push_str("end.\n");
    text
}

#[test]
fn a_large_ordinary_source_with_directives_is_analysed() {
    let text = synthetic_library(2_300_000, 1);
    let (result, usage) = analyze(&text);
    assert!(
        !is_work_limit(&result),
        "{} bytes: {result:?} {usage:?}",
        text.len()
    );
    assert!(matches!(result, Ok(true)), "{result:?} {usage:?}");
}

/// Byte work, like steps, follows the input: an 8 MB ordinary unit (within
/// the 16 MiB maxFileBytes ceiling) scans, copies and maps more than the
/// fixed 64 MiB of byte work.
#[test]
fn a_larger_ordinary_source_fits_the_byte_budget() {
    let text = synthetic_library(8_000_000, 16);
    let (result, usage) = analyze(&text);
    assert!(matches!(result, Ok(true)), "{result:?} {usage:?}");
    assert!(
        usage.byte_work > CompileTimeLimits::default().max_byte_work,
        "{usage:?}"
    );
}

#[test]
fn pathological_work_on_a_small_source_still_trips_the_step_limit() {
    // Each condition looks a constant up among all earlier declarations:
    // conditions times declarations steps from about 120 KiB of source.
    let mut text = String::from("unit T;\ninterface\nconst\n");
    for n in 0..3000 {
        text.push_str(&format!("  C{n} = {n};\n"));
    }
    for _ in 0..3000 {
        text.push_str("{$IF C2999 = 2999}{$IFEND}\n");
    }
    text.push_str("implementation\nend.\n");
    assert!(text.len() < 128 * 1024);
    let (result, usage) = analyze(&text);
    assert!(
        is_work_limit(&result),
        "{} bytes: {result:?} {usage:?} of {}",
        text.len(),
        CompileTimeLimits::default().max_work
    );
}

/// Directive bodies are charged for the bytes their application reads, not
/// a multiple of each body byte.
#[test]
fn directive_bodies_are_charged_for_the_bytes_they_read() {
    let usage = |padding: usize| {
        let pad = " ".repeat(padding);
        let mut text = String::from("unit T;\ninterface\n");
        for _ in 0..1000 {
            text.push_str(&format!("{{$HINTS OFF{pad}}}{{$WARNINGS ON{pad}}}\n"));
        }
        text.push_str("implementation\nend.\n");
        let (result, usage) = analyze(&text);
        assert!(matches!(result, Ok(true)), "{result:?}");
        usage
    };
    let (short, long) = (usage(0), usage(100));
    let body_bytes = 2 * 1000 * 100;
    let work = (long.work - short.work) as f64 / body_bytes as f64;
    let bytes = (long.byte_work - short.byte_work) as f64 / body_bytes as f64;
    assert!(work <= 3.0, "{work} steps per directive body byte");
    assert!(bytes <= 24.0, "{bytes} byte work per directive body byte");
}

/// Windows.pas has 9,780 `{$EXTERNALSYM}` hints: directives that set no
/// conditional fact must not snapshot the whole conditional environment.
#[test]
fn inert_directives_do_not_copy_the_environment() {
    let mut text = String::from("unit T;\ninterface\n");
    for n in 0..100 {
        text.push_str(&format!("{{$DEFINE FEATURE_NUMBER_{n}}}\n"));
    }
    text.push_str("const\n");
    for n in 0..5000 {
        text.push_str(&format!("  {{$EXTERNALSYM X{n}}}\n  X{n} = {n};\n"));
    }
    text.push_str("implementation\nend.\n");
    let (result, usage) = analyze(&text);
    assert!(matches!(result, Ok(true)), "{result:?} {usage:?}");
    let mut plain = String::from("unit T;\ninterface\n");
    for n in 0..100 {
        plain.push_str(&format!("{{$DEFINE FEATURE_NUMBER_{n}}}\n"));
    }
    plain.push_str("const\n");
    for n in 0..5000 {
        plain.push_str(&format!("  X{n} = {n};\n"));
    }
    plain.push_str("implementation\nend.\n");
    let (_, without) = analyze(&plain);
    let per_directive = (usage.byte_work - without.byte_work) / 5000;
    assert!(
        per_directive < 1024,
        "{per_directive} byte work per directive"
    );
}

/// A unit whose header is conditional and whose body is about 2.2 MB.
fn big_provider(name: &str) -> String {
    let mut text = format!(
        "{{$IFDEF CPU64}}{{$ENDIF}}\nunit {name};\ninterface\nconst X = 1;\nimplementation\n"
    );
    let mut n = 0;
    while text.len() < 2_200_000 {
        text.push_str(&format!(
            "procedure P{n}(var V: Integer);\nbegin\n  V := V + {n}; // keep going\nend;\n"
        ));
        n += 1;
    }
    text.push_str("end.\n");
    text
}

/// Review of round 1: a header-only lookup scans a prefix, so it may grant
/// steps only for that prefix, not for the whole 2.2 MB unit.
#[test]
fn repeated_header_lookups_grant_only_what_they_scan() {
    let big = big_provider("Other");
    let (_dir, path, mut resolver) = resolver_with(
        "unit T; interface implementation end.",
        &[("Other.pas", &big)],
    );
    let mut previous = resolver.compile_time_work_allowance();
    for _ in 0..4 {
        let outcome = resolver
            .try_resolve_unit(
                pascal_core::UnitResolveRequest {
                    requested_name: "Other",
                    importer_path: &path,
                    legacy_route: None,
                },
                &NoCancellation,
            )
            .unwrap();
        assert!(
            matches!(outcome.result, pascal_core::Resolution::Found(_)),
            "{:?}",
            outcome.result
        );
        let allowance = resolver.compile_time_work_allowance();
        let grown = allowance - previous;
        assert!(
            grown <= 8 * 64 * 1024,
            "one header lookup granted {grown} steps for a {} byte unit",
            big.len()
        );
        previous = allowance;
    }
}

/// A small root's superlinear work cannot spend the grant of a large
/// provider whose implementation the interface-only provider scan skips.
#[test]
fn a_small_pathological_root_cannot_spend_a_large_providers_unscanned_grant() {
    let big = big_provider("Big");
    let mut text = String::from("unit T;\ninterface\nuses Big;\n{$IF Big.X = 1}{$IFEND}\nconst\n");
    for n in 0..3000 {
        text.push_str(&format!("  C{n} = {n};\n"));
    }
    for _ in 0..3000 {
        text.push_str("{$IF C2999 = 2999}{$IFEND}\n");
    }
    text.push_str("implementation\nend.\n");
    let (result, usage) = analyze_with(&text, &[("Big.pas", &big)]);
    // Refused, and by a limit not raised by the provider's unscanned body.
    let limits = CompileTimeLimits::default();
    let unscanned = big.len() / 2;
    assert!(
        matches!(
            &result,
            Err(ResolverError::LimitExceeded { limit, maximum, .. })
                if (*limit == "max_work"
                    && *maximum < limits.max_work + limits.work_per_source_byte * unscanned)
                    || (*limit == "max_byte_work"
                        && *maximum
                            < limits.max_byte_work + limits.byte_work_per_source_byte * unscanned)
        ),
        "{result:?} {usage:?}"
    );
}

/// Review of round 1: the import path's whole-source estimate had no grant,
/// so a plain 1 MB importer without includes was refused.
#[test]
fn a_large_importer_without_includes_resolves_its_imports() {
    let mut text = String::from("unit T;\ninterface\nuses U;\nimplementation\n");
    let mut n = 0;
    while text.len() < 1_000_000 {
        text.push_str(&format!(
            "procedure P{n}(var V: Integer);\nbegin\n  V := V + {n};\nend;\n"
        ));
        n += 1;
    }
    text.push_str("end.\n");
    let (_dir, path, mut resolver) =
        resolver_with(&text, &[("U.pas", "unit U; interface implementation end.")]);
    let mut source = support::loaded("T.pas", &text);
    source.path = path;
    let importer = pascal_core::ResolvedUnit {
        requested_name: "T".into(),
        declared_name: "T".into(),
        source,
    };
    let imports = resolver.resolve_imports_with_text(&importer, &[], &text, &NoCancellation);
    assert!(
        imports.as_ref().is_ok_and(|imports| imports.complete),
        "{imports:?}"
    );
}

/// Review of round 1: each short comment was charged a whole 512-byte search
/// piece; byte work must stay close to what comments actually read.
#[test]
fn short_comments_are_charged_about_what_they_read() {
    let routine = |comment: &str| {
        let mut text = String::from(
            "unit T;\ninterface\nimplementation\nprocedure P(var V: Integer);\nbegin\n",
        );
        for _ in 0..30_000 {
            text.push_str(&format!("  V := V + 1;{comment}\n"));
        }
        text.push_str("end;\nend.\n");
        text
    };
    let (plain_result, plain) = analyze(&routine(""));
    let (commented_result, commented) = analyze(&routine(" // c"));
    assert!(matches!(plain_result, Ok(true)), "{plain_result:?}");
    assert!(matches!(commented_result, Ok(true)), "{commented_result:?}");
    assert!(
        commented.byte_work <= plain.byte_work * 3,
        "{} byte work with a comment per line, {} without",
        commented.byte_work,
        plain.byte_work
    );
}

/// Round 3: funded whole-input charges (raw UTF-8 validation) and per-byte
/// grants are credited to the request once per distinct source; repeating
/// the analysis of the same raw source earns nothing more.
#[test]
fn repeated_analysis_of_one_raw_source_is_credited_once() {
    let text = synthetic_library(300_000, 1);
    let (_dir, path, mut resolver) = resolver_with(&text, &[]);
    let mut source = support::loaded("T.pas", &text);
    source.path = path;
    source.decoded_text = None;
    resolver
        .analyze_compile_time(&source, &NoCancellation)
        .unwrap();
    let credited = resolver.compile_time_work_allowance();
    let bytes = resolver.compile_time_byte_work_allowance();
    for _ in 0..4 {
        let _ = resolver.analyze_compile_time(&source, &NoCancellation);
    }
    assert_eq!(resolver.compile_time_work_allowance(), credited);
    assert_eq!(resolver.compile_time_byte_work_allowance(), bytes);
}

/// Round 3: an importer with an include is read by the shape probe and then
/// by the session; the request credits its text once.
#[test]
fn the_probe_and_session_credit_one_importer_once() {
    let mut text = String::from("unit T;\ninterface\nuses U;\n{$I small.inc}\nimplementation\n");
    let mut n = 0;
    while text.len() < 200_000 {
        text.push_str(&format!(
            "procedure P{n}(var V: Integer);\nbegin\n  V := V + {n};\nend;\n"
        ));
        n += 1;
    }
    text.push_str("end.\n");
    let include = "const Small = 1;";
    let (_dir, path, mut resolver) = resolver_with(
        &text,
        &[
            ("U.pas", "unit U; interface implementation end."),
            ("small.inc", include),
        ],
    );
    let mut source = support::loaded("T.pas", &text);
    source.path = path;
    let importer = pascal_core::ResolvedUnit {
        requested_name: "T".into(),
        declared_name: "T".into(),
        source,
    };
    let imports = resolver.resolve_imports_with_text(&importer, &[], &text, &NoCancellation);
    assert!(imports.is_ok(), "{imports:?}");
    let per_byte = CompileTimeLimits::default().work_per_source_byte;
    let distinct = text.len() + include.len() + "unit U; interface implementation end.".len();
    let credited = resolver.compile_time_work_allowance();
    // Each distinct byte once, plus the funded lexical estimate bound.
    assert!(
        credited <= distinct * (per_byte + 3),
        "{credited} credited for {distinct} distinct bytes"
    );
}

/// Round 4: an adapter keeping the default resolver hooks (no request-wide
/// credit) still credits each source byte at most once per session: 40
/// includes of one 4 KB file are refused within the cap plus the grant of
/// the distinct bytes (it spent 486,761 steps, about 24x a 20,000 cap).
#[test]
fn repeated_includes_through_a_default_hook_adapter_credit_once_per_session() {
    let body = "  V := V + 1;\n".repeat(4096 / 15);
    let root = format!(
        "unit T; interface implementation procedure P(var V: Integer); begin\n{}end; end.",
        "{$I body.inc}\n".repeat(40)
    );
    let mut fixture = support::Fixture::new(&root, context(LayoutPlatform::Win32));
    fixture.include("body.inc", &body);
    fixture.limits.max_work = 20_000;
    fixture.limits.max_retained_bytes = 64 * 1024 * 1024;
    let distinct = root.len() + body.len();
    let limits = CompileTimeLimits::default();
    let ceiling = 20_000 + distinct * (limits.work_per_source_byte + 3);
    let result = fixture.analyze();
    let refused = match &result {
        Err(ResolverError::LimitExceeded {
            limit: "max_work",
            maximum,
            ..
        }) => Some(*maximum),
        Ok(analysis) => analysis.reasons.iter().find_map(|reason| match reason {
            pascal_core::compile_time::IncompleteReason::Limit {
                name: "max_work",
                maximum,
            } => Some(*maximum),
            _ => None,
        }),
        _ => None,
    };
    assert!(
        refused.is_some_and(|maximum| maximum <= ceiling),
        "{:?} (ceiling {ceiling})",
        result
            .as_ref()
            .map(|analysis| (&analysis.reasons, analysis.budget.budget_snapshot()))
    );
}

/// l4d-4nbs R1: the legacy 1 MiB cap on the conditional environment bounds
/// its DEFINE/option facts only. The typed declaration state is admitted by
/// the compile-time budget, so a unit with more than 1 MiB of declarations
/// still applies later switches, DEFINEs and includes (SynCommons failed at
/// `{$M+}` with 1.3 MB of typed state and 2,877 scalar entries).
#[test]
fn directives_after_a_megabyte_of_declarations_are_applied() {
    let mut text = String::from("unit T;\ninterface\nconst\n");
    let value = "x".repeat(100);
    for n in 0..12_000 {
        text.push_str(&format!("  C{n} = '{value}';\n"));
    }
    text.push_str(
        "{$M+}\n{$DEFINE LATE}\n{$IFDEF LATE}\nconst Late = 1;\n{$ENDIF}\n{$I late.inc}\n{$IF Late = 1}{$DEFINE AFTER}{$IFEND}\nimplementation\nend.\n",
    );
    let (result, usage) = analyze_with(&text, &[("late.inc", "const FromInclude = 2;\n")]);
    assert!(matches!(result, Ok(true)), "{result:?} {usage:?}");
}

/// l4d-4nbs R2: the shape probe credits the request for the importer's
/// bytes, so the session that follows earns no per-byte grant of its own.
/// It must still get what a fresh session over the same text would get
/// (Windows.pas, 1.3 MB with includes, failed at `max_work 2000001 >
/// 2000000` while the request still had 10.5M steps of earned allowance).
#[test]
fn the_session_after_a_probe_keeps_the_grant_of_the_probed_bytes() {
    let mut text = String::from("unit T;\ninterface\nuses U;\n{$I small.inc}\nimplementation\n");
    let mut n = 0;
    while text.len() < 1_300_000 {
        text.push_str(&format!(
            "procedure P{n}(var V: Integer);\nbegin\n  case V mod 4 of\n    0: V := V + {n};\n    1: V := V - 1;\n  else\n    V := 0;\n  end;\nend;\n"
        ));
        n += 1;
    }
    text.push_str("end.\n");
    let (_dir, path, mut resolver) = resolver_with(
        &text,
        &[
            ("U.pas", "unit U; interface implementation end."),
            ("small.inc", "const Small = 1;"),
        ],
    );
    let mut source = support::loaded("T.pas", &text);
    source.path = path;
    let importer = pascal_core::ResolvedUnit {
        requested_name: "T".into(),
        declared_name: "T".into(),
        source,
    };
    let imports = resolver.resolve_imports_with_text(&importer, &[], &text, &NoCancellation);
    let report = resolver.report();
    assert!(
        imports.as_ref().is_ok_and(|imports| imports.complete),
        "{imports:?} {:?} {:?}",
        report.incomplete_reasons,
        resolver.compile_time_usage()
    );
}

/// l4d-4nbs R13: a unit lookup reads its candidate whole, though a provider
/// scan reads only its interface, so it is granted only that prefix.
/// BaseRECORDLOCK's session spent 38M of byte work on 42 such reads with no
/// grant. The first full read of each distinct source in the request is
/// funded, as raw UTF-8 validation is.
#[test]
fn full_reads_of_looked_up_providers_are_funded_once() {
    let units = 60;
    let body = (0..12_000)
        .map(|n| format!("procedure P{n}; begin end;\n"))
        .collect::<String>();
    let files = (0..units)
        .map(|u| {
            (
                format!("U{u}.pas"),
                format!("unit U{u}; interface const C{u} = {u}; implementation\n{body}end."),
            )
        })
        .collect::<Vec<_>>();
    let uses = (0..units)
        .map(|u| format!("U{u}"))
        .collect::<Vec<_>>()
        .join(", ");
    let root = format!(
        "unit T; interface uses {uses}; implementation {{$IF CompilerVersion > 21.0}}{{$DEFINE HIT}}{{$IFEND}} end."
    );
    let files = files
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect::<Vec<_>>();
    let (result, usage) = analyze_retaining(&root, &files, 512 * 1024 * 1024);
    assert!(matches!(result, Ok(true)), "{result:?} {usage:?}");
}

/// l4d-4nbs D19: raising the byte grant per source byte keeps superlinear
/// byte work on a small source refused: each layout query copies the whole
/// declaration state, so queries times declarations bytes from about 200 KiB.
#[test]
fn pathological_byte_work_on_a_small_source_still_trips_the_byte_limit() {
    let mut text = String::from("unit T;\ninterface\nuses U;\ntype\n");
    for n in 0..3000 {
        text.push_str(&format!("  R{n} = record A, B: Integer; end;\n"));
    }
    for _ in 0..3000 {
        text.push_str("{$IF SizeOf(R2999) = 8}{$IFEND}\n");
    }
    text.push_str("implementation\nend.\n");
    assert!(text.len() < 256 * 1024, "{}", text.len());
    let (result, usage) =
        analyze_with(&text, &[("U.pas", "unit U; interface implementation end.")]);
    assert!(
        matches!(&result, Err(ResolverError::LimitExceeded { limit, .. }) if *limit == "max_byte_work"),
        "{} bytes: {result:?} {usage:?}",
        text.len()
    );
}

/// Keeps every compiled provider interface it is given.
#[derive(Default)]
struct KeepAll(
    std::sync::Mutex<
        std::collections::HashMap<
            pascal_core::compile_time::ProviderGraphKey,
            std::sync::Arc<pascal_core::compile_time::SharedProviderGraph>,
        >,
    >,
);

impl pascal_core::compile_time::SharedProviderGraphs for KeepAll {
    fn get(
        &self,
        key: &pascal_core::compile_time::ProviderGraphKey,
    ) -> Option<std::sync::Arc<pascal_core::compile_time::SharedProviderGraph>> {
        self.0.lock().unwrap().get(key).cloned()
    }
    fn put(
        &self,
        key: pascal_core::compile_time::ProviderGraphKey,
        graph: std::sync::Arc<pascal_core::compile_time::SharedProviderGraph>,
    ) {
        self.0.lock().unwrap().insert(key, graph);
    }
}

/// l4d-4nbs D29: a request that reuses a provider interface another request
/// compiled scans none of its bytes, so it is granted nothing for them: its
/// allowance is what the root alone earns, and it spends less than the
/// compiling request.
#[test]
fn a_reused_provider_interface_grants_nothing() {
    let padding = "{".to_string() + &"x".repeat(200_000) + "}";
    let provider = format!("unit A; interface {padding} type TA = Int64; implementation end.");
    let root =
        "unit T; interface uses A; {$IF SizeOf(A.TA)=8}{$DEFINE HA}{$ENDIF} implementation end.";
    let store = std::sync::Arc::new(KeepAll::default());
    let (dir, path, _) = resolver_with(root, &[("A.pas", &provider)]);
    let request = |store: Option<&std::sync::Arc<KeepAll>>| {
        let resolver = resolver_in(dir.path(), &path, 64 * 1024 * 1024);
        let mut resolver = match store {
            Some(store) => resolver.with_shared_provider_graphs(store.clone()),
            None => resolver,
        };
        let mut source = support::loaded("T.pas", root);
        source.path = path.clone();
        let analysis = resolver
            .analyze_compile_time(&source, &NoCancellation)
            .unwrap();
        assert!(analysis.root.complete, "{:?}", analysis.reasons);
        (
            resolver.compile_time_work_allowance(),
            resolver.compile_time_usage(),
        )
    };
    let (alone, _) = request(None);
    let (compiling, first) = request(Some(&store));
    let (reusing, second) = request(Some(&store));
    assert_eq!(compiling, alone, "sharing changes nothing for the compiler");
    assert!(
        reusing + 200_000 <= compiling,
        "reuse granted the provider's bytes: {reusing} vs {compiling}"
    );
    assert!(second.work < first.work, "{first:?} then {second:?}");
    assert!(
        second.byte_work < first.byte_work,
        "{first:?} then {second:?}"
    );
}
