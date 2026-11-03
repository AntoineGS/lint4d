//! The compile-time declaration scanner on valid real-world shapes (l4d-zaf0).
mod support {
    pub mod compile_time;
}
use pascal_core::conditional::Truth;
use pascal_project::LayoutPlatform;
use support::compile_time::{Fixture, activity, context};

/// `body` is an implementation section; a probe after it must still see the
/// root-scope declarations on both sides of it.
fn assert_scans_completely(body: &str) {
    let source = format!(
        "unit T; interface type TFoo = class procedure Bar; end; \
         implementation {body} \
         const After = 1; \
         {{$IF Declared(After) and Declared(TFoo)}}{{$DEFINE SEEN}}{{$ENDIF}} \
         end."
    );
    let analysis = Fixture::new(&source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(analysis.root.complete, "{:?}\n{body}", analysis.reasons);
    assert!(analysis.reasons.is_empty(), "{:?}", analysis.reasons);
    assert_eq!(activity(&analysis, "DEFINE SEEN"), Truth::True, "{body}");
}

#[test]
fn anonymous_procedure_argument_in_a_routine_body() {
    assert_scans_completely(
        "procedure Run; begin TTask.Run(procedure begin Work; end); end; \
         procedure TFoo.Bar; begin end;",
    );
}

#[test]
fn anonymous_procedure_with_parameters_and_nested_blocks() {
    assert_scans_completely(
        "procedure Run; begin \
           Parallel.Async(procedure (const task: IOmniTask) \
             begin if Ready then begin Work; end; try Work; finally Done; end; end, \
             Parallel.TaskConfig.OnTerminated(procedure begin Done; end)); \
         end; \
         procedure TFoo.Bar; begin end;",
    );
}

#[test]
fn nested_anonymous_methods() {
    assert_scans_completely(
        "procedure Run; begin \
           TTask.Run(procedure begin \
             TThread.Queue(nil, procedure begin \
               TThread.Synchronize(nil, procedure begin Work; end); \
             end); \
           end); \
         end; \
         procedure TFoo.Bar; begin end;",
    );
}

#[test]
fn anonymous_function_assigned_to_a_variable() {
    assert_scans_completely(
        "procedure Run; var F: TFunc<Integer>; P: TProc; begin \
           F := function: Integer begin Result := 1; end; \
           P := procedure begin case F() of 1: Work; end; end; \
           P; \
         end; \
         procedure TFoo.Bar; begin end;",
    );
}

#[test]
fn anonymous_methods_inside_class_methods_and_nested_routines() {
    assert_scans_completely(
        "procedure TFoo.Bar; \
           procedure Local; begin TTask.Run(procedure begin Work; end); end; \
         var Count: Integer; \
         begin \
           Local; \
           Items.ForEach(function (const Item: TItem): Boolean \
             begin repeat Inc(Count); until Count > 1; Result := True; end); \
         end;",
    );
}

#[test]
fn anonymous_method_in_the_program_block() {
    let source = "program P; procedure Run; begin end; \
        begin TTask.Run(procedure begin Run; end); end.";
    let analysis = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
}

#[test]
fn control_named_routines_scan_completely() {
    assert_scans_completely(
        "procedure Run; procedure Local; begin end; begin Local; end; \
         procedure TFoo.Bar; begin end;",
    );
}

#[test]
fn program_uses_with_explicit_unit_paths() {
    let source = "program P;\nuses\n  Environment in 'src\\Environment.pas' {frmEnvironment: TFrame},\n  \
        Other in 'Other.pas';\n{$IF Ready = 1}{$DEFINE SEEN}{$ENDIF}\nbegin\nend.\n";
    let mut fixture = Fixture::new(source, context(LayoutPlatform::Win32));
    fixture.unit(
        "Environment",
        "unit Environment; interface const Ready = 1; implementation end.",
        context(LayoutPlatform::Win32),
    );
    fixture.unit(
        "Other",
        "unit Other; interface implementation end.",
        context(LayoutPlatform::Win32),
    );
    let analysis = fixture.analyze().unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert_eq!(activity(&analysis, "DEFINE SEEN"), Truth::True);
}

/// An anonymous method's local declarations are not admitted, so the scanner
/// leaves its state unknown after it.
const UNSCANNABLE: &str = "procedure Run; begin \
    TTask.Run(procedure var Local: Integer; begin Local := 1; end); end;";

#[test]
fn unscanned_declarations_leave_a_file_without_declaration_queries_complete() {
    let source = format!(
        "unit T; interface {{$DEFINE FLAG}} implementation {UNSCANNABLE} \
         {{$IFDEF FLAG}}{{$DEFINE AFTER_FLAG}}{{$ENDIF}} \
         {{$IF Defined(FLAG) and (1 + 1 = 2)}}{{$DEFINE AFTER_DEFINED}}{{$ENDIF}} \
         end."
    );
    let analysis = Fixture::new(&source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert!(analysis.reasons.is_empty(), "{:?}", analysis.reasons);
    assert_eq!(activity(&analysis, "DEFINE AFTER_FLAG"), Truth::True);
    assert_eq!(activity(&analysis, "DEFINE AFTER_DEFINED"), Truth::True);
}

#[test]
fn unscanned_declarations_make_only_later_declaration_queries_unknown() {
    let source = format!(
        "unit T; interface type R = Byte; const C = 1; implementation \
         {{$IF Declared(C)}}{{$DEFINE BEFORE}}{{$ENDIF}} \
         {UNSCANNABLE} \
         {{$IF Declared(C)}}{{$DEFINE DECLARED}}{{$ENDIF}} \
         {{$IF C = 1}}{{$DEFINE CONSTANT}}{{$ENDIF}} \
         {{$IF SizeOf(R) = 1}}{{$DEFINE SIZE}}{{$ENDIF}} \
         {{$IF Declared(Local)}}{{$DEFINE LOCAL}}{{$ENDIF}} \
         {{$IFDEF BEFORE}}{{$DEFINE INDEPENDENT}}{{$ENDIF}} \
         end."
    );
    let analysis = Fixture::new(&source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert_eq!(activity(&analysis, "DEFINE BEFORE"), Truth::True);
    for consumer in [
        "DEFINE DECLARED",
        "DEFINE CONSTANT",
        "DEFINE SIZE",
        "DEFINE LOCAL",
    ] {
        assert_eq!(activity(&analysis, consumer), Truth::Unknown, "{consumer}");
    }
    assert_eq!(activity(&analysis, "DEFINE INDEPENDENT"), Truth::True);
}

#[test]
fn inline_variable_initialized_by_an_anonymous_method_stays_balanced() {
    let source = "unit T; interface implementation \
        procedure Run; begin var P: TProc := procedure begin if A then begin B; end; end; P; end; \
        const After = 1; \
        {$IFDEF MSWINDOWS}{$ELSE}{$ENDIF} \
        {$IF Declared(After)}{$DEFINE SEEN}{$ENDIF} end.";
    let analysis = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert_eq!(activity(&analysis, "DEFINE SEEN"), Truth::Unknown);
}
