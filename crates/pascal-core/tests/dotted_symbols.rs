//! l4d-onnm: a conditional symbol is the identifier a DEFINE, UNDEF, IFDEF
//! or IFNDEF argument starts with; dcc ignores whatever follows it. Observed
//! with dcc32 21.0 and 24.0 (`~/.cache/l4d-tmp/d46-dccprobe/R.dpr`):
//! `{$define DELPHI.NET}` defines DELPHI, `{$ifdef DELPHI.OTHER}` and
//! `{$ifdef FOO junk}` test DELPHI and FOO, `{$undef FOO.X}` undefines FOO.
use pascal_core::conditional::{self, ConditionalContext, Truth};

fn closed() -> ConditionalContext {
    ConditionalContext::default().with_absent_define(Truth::False)
}

fn activities(source: &str) -> (bool, Vec<(String, Truth)>) {
    let analysis = conditional::analyze_with_context(source, &closed());
    (
        analysis.complete,
        analysis
            .directives
            .iter()
            .filter(|d| d.body.starts_with("DEFINE HIT"))
            .map(|d| (d.body.clone(), d.activity))
            .collect(),
    )
}

#[test]
fn a_dotted_define_defines_its_leading_identifier() {
    let (complete, hits) = activities(
        "{$define DELPHI.NET}\
         {$ifdef DELPHI}{$DEFINE HIT1}{$endif}\
         {$ifdef DELPHI.OTHER}{$DEFINE HIT2}{$endif}\
         {$ifdef NET}{$DEFINE HIT3}{$endif}\
         {$ifndef DELPHI.NET}{$DEFINE HIT4}{$endif}\
         {$undef DELPHI.X}\
         {$ifndef DELPHI}{$DEFINE HIT5}{$endif}",
    );
    assert!(complete);
    assert_eq!(
        hits,
        [
            ("DEFINE HIT1".into(), Truth::True),
            ("DEFINE HIT2".into(), Truth::True),
            ("DEFINE HIT3".into(), Truth::False),
            ("DEFINE HIT4".into(), Truth::False),
            ("DEFINE HIT5".into(), Truth::True),
        ]
    );
}

#[test]
fn text_after_the_symbol_is_ignored() {
    let (complete, hits) = activities(
        "{$define FOO}{$define X-Y}\
         {$ifdef FOO junk}{$DEFINE HIT1}{$endif}\
         {$ifdef FOO.}{$DEFINE HIT2}{$endif}\
         {$ifdef X}{$DEFINE HIT3}{$endif}",
    );
    assert!(complete);
    assert!(hits.iter().all(|(_, t)| *t == Truth::True), "{hits:?}");
}

/// The shape of MDCompilers.inc's .NET block: dotted symbols inside a
/// branch that is inactive on Win32, followed by CompilerVersion chains.
#[test]
fn an_inactive_dotted_block_and_version_chains_are_complete() {
    let context = closed().with_compiler_version(pascal_project::CompilerVersion::new(21, 0));
    let source = "{$ifdef CLR}\r\n  {$define DELPHI.NET}\r\n  {$ifdef VER160}\r\n    \
        {$define DELPHI.NET_1}\r\n  {$endif VER160}\r\n  {$ifdef DELPHI.NET_1}\r\n    \
        {$define DELHI.NET_1_UP}\r\n  {$endif DELPHI.NET_1}\r\n{$endif CLR}\r\n\
        {$ifndef VER120}\r\n  {$if CompilerVersion = 23 }\r\n    {$define C16}\r\n  \
        {$elseif CompilerVersion = 21 }\r\n    {$DEFINE HIT}\r\n  {$ifend}\r\n  \
        {$if CompilerVersion >= 14.0}  {$define D6UP}\r\n  {$ifend}\r\n{$endif}\r\n\
        {$ifdef DELPHI}{$define WRONG}{$endif}";
    let analysis = conditional::analyze_with_context(source, &context);
    assert!(analysis.complete);
    let activity = |body: &str| {
        analysis
            .directives
            .iter()
            .find(|d| d.body == body)
            .map(|d| d.activity)
            .unwrap()
    };
    assert_eq!(activity("DEFINE HIT"), Truth::True);
    assert_eq!(activity("define D6UP"), Truth::True);
    assert_eq!(activity("define DELPHI.NET"), Truth::False);
    assert_eq!(activity("define WRONG"), Truth::False);
}

/// Fix round 1 (I2): dcc rejects a dotted `Defined()` argument (E2029 ')'
/// expected but '.' found, dcc32 21.0 and 24.0), so it is unknown.
#[test]
fn a_dotted_defined_argument_is_unknown() {
    let analysis = conditional::analyze_with_context(
        "{$define DELPHI.NET}{$IF Defined(DELPHI.NET)}{$DEFINE HIT}{$IFEND}\
         {$IF Defined(DELPHI)}{$DEFINE HIT2}{$IFEND}",
        &closed(),
    );
    let activity = |body: &str| {
        analysis
            .directives
            .iter()
            .find(|d| d.body == body)
            .map(|d| d.activity)
            .unwrap()
    };
    assert_eq!(activity("DEFINE HIT"), Truth::Unknown);
    assert_eq!(activity("DEFINE HIT2"), Truth::True);
}
