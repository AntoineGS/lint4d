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

fn analyze(text: &str) -> (Result<bool, ResolverError>, BudgetSnapshot) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("T.pas");
    std::fs::write(&path, text).unwrap();
    let mut ctx = ProjectContext::discover(&path, &[dir.path().into()], &ProjectOptions::default())
        .unwrap()
        .context;
    let mut parts = std::sync::Arc::unwrap_or_clone(std::mem::take(&mut ctx)).into_parts();
    parts.conditional_context = context(LayoutPlatform::Win32);
    ctx = ProjectContext::from_parts(parts).into();
    let mut resolver = UnitResolver::new(
        ctx,
        vec![dir.path().into()],
        FilesystemSourceStore::new(),
        ResolverLimits {
            max_source_bytes: 16 * 1024 * 1024,
            // The LSP selects a retention cap derived from its file limits.
            max_compile_time_retained_bytes: 64 * 1024 * 1024,
            ..ResolverLimits::default()
        },
    );
    let mut source = support::loaded("T.pas", text);
    source.path = path;
    let result = resolver
        .analyze_compile_time(&source, &NoCancellation)
        .map(|analysis| {
            if !analysis.root.complete {
                eprintln!("incomplete: {:?}", analysis.reasons);
            }
            analysis.root.complete
        });
    (result, resolver.compile_time_usage())
}

fn is_work_limit(result: &Result<bool, ResolverError>) -> bool {
    matches!(result, Err(ResolverError::LimitExceeded { limit, .. }) if *limit == "max_work")
}

/// About SynCommons.pas' size and shape: an interface of declarations and a
/// large implementation of routine bodies, comments and conditional code.
fn synthetic_library(bytes: usize) -> String {
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
             case Count mod 4 of\n    0: Result := Result + 1;\n    1: Result := Result - 1;\n    2: Result := Result * 2;\n  else\n    Result := 0;\n  end;\n\
             if Text <> '' then\n    Result := Result + Ord(Text[1]);\n{body}end;\n\n"
        ));
        n += 1;
    }
    text.push_str("end.\n");
    text
}

#[test]
fn a_large_ordinary_source_with_directives_is_analysed() {
    let text = synthetic_library(2_300_000);
    let (result, usage) = analyze(&text);
    assert!(
        !is_work_limit(&result),
        "{} bytes: {result:?} {usage:?}",
        text.len()
    );
    assert!(matches!(result, Ok(true)), "{result:?} {usage:?}");
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
