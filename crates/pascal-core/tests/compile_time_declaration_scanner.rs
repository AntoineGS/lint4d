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

/// A root-level declaration the scanner cannot admit leaves its state
/// unknown after it.
const UNSCANNABLE: &str = "function Local: ; begin end;";

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
    assert_eq!(activity(&analysis, "DEFINE SEEN"), Truth::True);
}

/// Declarations the scanner skips inside a statement scope hide only that
/// scope: queries outside it keep their proofs, queries inside it do not.
fn assert_scope_local(body: &str) {
    assert_inside(body, Truth::Unknown);
}

fn assert_inside(body: &str, inside: Truth) {
    let source = format!(
        "unit T; interface const C = 1; implementation {body} \
         const After = 1; \
         {{$IF Declared(After) and (C = 1)}}{{$DEFINE OUTSIDE}}{{$ENDIF}} \
         end."
    );
    let analysis = Fixture::new(&source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(analysis.root.complete, "{:?}\n{body}", analysis.reasons);
    assert_eq!(activity(&analysis, "DEFINE OUTSIDE"), Truth::True, "{body}");
    assert_eq!(activity(&analysis, "DEFINE INSIDE"), inside, "{body}");
}

#[test]
fn anonymous_method_local_declarations_hide_only_the_method() {
    assert_scope_local(
        "procedure Run; begin \
           Parallel.Async( \
             procedure (const task: IOmniTask) \
             var Local: Integer; \
             const C = 2; \
             begin {$IF C = 1}{$DEFINE INSIDE}{$ENDIF} Local := C; end); \
         end;",
    );
}

#[test]
fn inline_declarations_are_admitted_in_their_block_and_end_at_their_statement() {
    // The inline constant shadows the unit's C inside its block only.
    assert_inside(
        "procedure Run; begin \
           var X: Integer := 1; X := 2; \
           if X > 0 then begin const C = 2; X := C; {$IF C = 1}{$DEFINE INSIDE}{$ENDIF} end; \
           case X of 1: X := 3; end; \
         end;",
        Truth::False,
    );
}

#[test]
fn inline_declarations_in_an_anonymous_method_argument_hide_only_their_block() {
    assert_scope_local(
        "procedure Run; begin \
           Go(procedure begin var Y := 1; const C = 2; \
             if Y > 0 then begin Y := C; end; {$IF C = 1}{$DEFINE INSIDE}{$ENDIF} end); \
         end;",
    );
}

#[test]
fn inline_variable_initialized_by_an_anonymous_method_hides_only_its_block() {
    assert_scope_local(
        "procedure Run; begin \
           var C: TProc := procedure begin if A then begin B; end; end; \
           {$IF C = 1}{$DEFINE INSIDE}{$ENDIF} C; \
         end;",
    );
}

#[test]
fn method_reference_type_in_a_statement_is_not_an_anonymous_method() {
    assert_scope_local(
        "procedure Run; begin \
           var C: reference to procedure; \
           if A then begin {$IF C = 1}{$DEFINE INSIDE}{$ENDIF} end; \
         end;",
    );
}

/// Reduced from a real unit: a class method with a nested routine whose body
/// starts an OmniThread task with local variables and nested try blocks.
#[test]
fn background_task_with_locals_in_a_class_method() {
    let source = "unit T; interface type TEnv = class procedure Log(Id: Integer); end; \
        implementation \
        procedure TEnv.Log(Id: Integer); \
          procedure Convert(var Value: string); begin case Id of 1: Value := ''; else Value := 'x'; end; end; \
        begin \
          if Id <> 0 then begin \
            try \
              Parallel.Async( \
                procedure (const task: IOmniTask) \
                var Connection: IConnectionSet; \
                begin \
                  try \
                    Connection := Pool.Get; \
                    try Insert(Id); finally Pool.Release(Connection); end; \
                  except on e: Exception do \
                    begin if Pos('x', e.Message) > 0 then begin Reset := True; end; end; \
                  end \
                end); \
            except on e: Exception do begin Log(0); end; \
            end \
          end; \
        end; \
        {$IFDEF DEBUG}{$DEFINE TRACE}{$ENDIF} \
        const After = 1; \
        {$IF Declared(After) and Declared(TEnv)}{$DEFINE SEEN}{$ENDIF} \
        end.";
    let analysis = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert!(analysis.reasons.is_empty(), "{:?}", analysis.reasons);
    assert_eq!(activity(&analysis, "DEFINE SEEN"), Truth::True);
}

/// Composite types among an anonymous method's locals end with their own
/// `end`, which must not close the method's opaque scope.
#[test]
fn anonymous_method_local_record_type_keeps_the_method_scope() {
    assert_scope_local(
        "procedure Run; begin \
           Go(procedure type C = record A: Integer; end; \
             begin {$IF C = 1}{$DEFINE INSIDE}{$ENDIF} end); \
         end;",
    );
}

#[test]
fn anonymous_method_local_record_variable_keeps_the_method_scope() {
    assert_scope_local(
        "procedure Run; begin \
           Go(procedure var C: record A: Integer; end; \
             begin {$IF C = 1}{$DEFINE INSIDE}{$ENDIF} end); \
         end;",
    );
}

#[test]
fn anonymous_method_local_variant_record_keeps_the_method_scope() {
    assert_scope_local(
        "procedure Run; begin \
           Go(procedure type C = packed record case Tag: Integer of \
               0: (A: Integer); 1: (B: Byte; D: record E: Byte; end); end; \
             begin {$IF C = 1}{$DEFINE INSIDE}{$ENDIF} end); \
         end;",
    );
}

#[test]
fn anonymous_method_local_classes_keep_the_method_scope() {
    assert_scope_local(
        "procedure Run; begin \
           Go(procedure type \
               EFail = class(Exception); \
               TMeta = class of TObject; \
               IFoo = interface; \
               C = class(TObject) \
                 type TInner = class X: Integer; end; \
                 class function Make: C; \
                 F: procedure of object; \
               end; \
               IFoo = interface procedure P; end; \
             begin {$IF C = 1}{$DEFINE INSIDE}{$ENDIF} end); \
         end;",
    );
}

#[test]
fn anonymous_method_local_record_does_not_prove_absence() {
    let source = "unit T; interface implementation procedure Run; begin \
        Go(procedure type LocalRec = record A: Integer; end; \
          begin {$IF Declared(LocalRec)}{$ELSE}{$DEFINE ABSENT}{$ENDIF} end); \
        end; end.";
    let analysis = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert_eq!(activity(&analysis, "DEFINE ABSENT"), Truth::Unknown);
}

#[test]
fn anonymous_methods_in_initialization_and_finalization() {
    let source = "unit T; interface const C = 1; implementation \
        initialization TTask.Run(procedure begin Work; end); \
        {$IF C = 1}{$DEFINE INIT}{$ENDIF} \
        finalization TThread.Queue(nil, procedure begin Done; end); \
        {$IF Declared(C)}{$DEFINE FINAL}{$ENDIF} \
        end.";
    let analysis = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert_eq!(activity(&analysis, "DEFINE INIT"), Truth::True);
    assert_eq!(activity(&analysis, "DEFINE FINAL"), Truth::True);
}

/// l4d-4nbs R11: SysUtils.FloatToTextFmt has a `var` section and an `asm`
/// body whose labels (`@@1:`) looked like the type of a variable, so its
/// `end` was skipped and every later routine nested one scope deeper.
#[test]
fn assembler_routine_with_local_variables_and_labels() {
    assert_scans_completely(
        "function Low: Integer; var Count: Integer; \
         asm \
         @@1: MOV EAX, Count \
         DEC EAX \
         JNZ @@1 \
         end; \
         procedure TFoo.Bar; begin end;",
    );
}

/// An `asm` statement inside a `begin` block ends with its own `end`.
#[test]
fn assembler_statement_inside_a_routine_body() {
    assert_scans_completely(
        "procedure Wait; var Count: Integer; \
         begin \
           asm @@1: PAUSE \
           DEC Count \
           JNZ @@1 end; \
           if Count = 0 then asm NOP end; \
         end; \
         procedure TFoo.Bar; begin end;",
    );
}

/// l4d-4nbs R11: TypInfo.SortPropList's assembler labels `@@repeat` and
/// `@@endRepeat` are labels, not `repeat`; likewise `&`-escaped identifiers
/// such as `&begin` are not keywords.
#[test]
fn assembler_labels_and_escaped_identifiers_are_not_keywords() {
    assert_scans_completely(
        "procedure Sort; assembler; \
         asm \
         @@repeat: DEC EAX \
         JNZ @@repeat \
         @@case: JMP @@begin \
         @@begin: \
         end; \
         procedure Escaped; var &begin: Integer; begin &begin := 1; end; \
         procedure TFoo.Bar; begin end;",
    );
}

/// l4d-4nbs R11: ZLib.pas declares `EZLibError = class(Exception);`, a
/// class with a heritage list and no body, which left its scope open.
#[test]
fn bodiless_classes_with_a_heritage_list_close_their_scope() {
    let source = "unit T; interface \
         type \
           EBase = class(Exception); \
           EDerived = class(EBase); \
           TImpl = class(TInterfacedObject, IUnknown); \
           TFull = class(TObject) procedure Run; end; \
         const After = 1; \
         {$IF Declared(After) and Declared(EDerived) and Declared(TFull)}{$DEFINE SEEN}{$ENDIF} \
         implementation end.";
    let analysis = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(analysis.root.complete, "{:?}", analysis.reasons);
    assert_eq!(activity(&analysis, "DEFINE SEEN"), Truth::True);
}

/// Test-only harness (l4d-4nbs R11): analyse the real units listed in
/// `L4D_SCAN_FILES` (separated by `;`) and report every incomplete reason.
/// Includes are served from the unit's directory and `L4D_SCAN_INCLUDES`.
#[test]
#[ignore = "reads local Delphi sources named by L4D_SCAN_FILES"]
fn scan_real_units_from_the_environment() {
    let files = std::env::var("L4D_SCAN_FILES").unwrap_or_default();
    let extra = std::env::var("L4D_SCAN_INCLUDES").unwrap_or_default();
    let mut failures = Vec::new();
    for file in files.split(';').filter(|f| !f.is_empty()) {
        let path = std::path::Path::new(file);
        let text = String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned();
        let mut ctx = context(LayoutPlatform::Win32)
            .with_compiler_version(pascal_project::CompilerVersion::new(21, 0));
        for define in [
            "MSWINDOWS",
            "WIN32",
            "CPU386",
            "VER210",
            "CONDITIONALEXPRESSIONS",
        ] {
            ctx.set_define(define, pascal_project::ConditionalFact::True);
        }
        ctx = ctx.with_absent_define(pascal_project::ConditionalFact::False);
        let mut f = Fixture::new(&text, ctx);
        f.limits.max_retained_bytes = 256 * 1024 * 1024;
        let dirs = std::iter::once(path.parent().unwrap().to_path_buf())
            .chain(extra.split(';').filter(|d| !d.is_empty()).map(Into::into));
        for dir in dirs {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.to_ascii_lowercase().ends_with(".inc") {
                    let body =
                        String::from_utf8_lossy(&std::fs::read(entry.path()).unwrap()).into_owned();
                    // The fixture matches spellings exactly (`GETMEM.INC`).
                    let stem = &name[..name.len() - 4];
                    for spelling in [
                        name.clone(),
                        name.to_ascii_lowercase(),
                        name.to_ascii_uppercase(),
                        format!("{stem}.INC"),
                        format!("{stem}.inc"),
                    ] {
                        f.include(&spelling, &body);
                    }
                }
            }
        }
        match f.analyze() {
            Ok(analysis) => {
                eprintln!(
                    "{file}: complete={} {:?}",
                    analysis.root.complete, analysis.reasons
                );
                if !analysis.reasons.is_empty() {
                    failures.push(file.to_owned());
                }
            }
            Err(error) => {
                eprintln!("{file}: error {error}");
                failures.push(file.to_owned());
            }
        }
    }
    assert!(failures.is_empty(), "{failures:?}");
}
