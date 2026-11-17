use super::*;
use crate::navigation::{NavigationIndex, SourceSpan};
use lsp_types::Url;
use std::sync::atomic::AtomicBool;

#[derive(Debug)]
struct OccurrenceSnapshot {
    var: usize,
    variable: String,
    span: SourceSpan,
    access: Access,
    block: usize,
    order: usize,
}

#[derive(Debug)]
struct FlowSnapshot {
    name: String,
    locals: Vec<LocalVar>,
    occurrences: Vec<OccurrenceSnapshot>,
    cfg_blocks: usize,
    excluded: Option<FlowExclusion>,
}

fn flows(source: &str) -> Vec<FlowSnapshot> {
    let uri = Url::parse("file:///tmp/flow-scaffolding.pas").expect("valid URI");
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("fixture parses");
    let document = index.documents.get(&uri).expect("indexed document");
    let cancel = AtomicBool::new(false);
    let mut budget = AssistanceBudget::new(100_000, 8 * 1024 * 1024, "flow test");
    routine_flows(&index, &uri, document, &cancel, &mut budget)
        .expect("flow model builds")
        .into_iter()
        .map(|routine| FlowSnapshot {
            name: routine.name,
            occurrences: routine
                .occurrences
                .iter()
                .map(|occurrence| OccurrenceSnapshot {
                    var: occurrence.var,
                    variable: routine.locals[occurrence.var].name.clone(),
                    span: occurrence.span,
                    access: occurrence.access,
                    block: occurrence.block.index().index(),
                    order: occurrence.order,
                })
                .collect(),
            locals: routine.locals,
            cfg_blocks: routine.cfg.graph.node_count(),
            excluded: routine.excluded,
        })
        .collect()
}

fn routine(source: &str, name: &str) -> FlowSnapshot {
    flows(source)
        .into_iter()
        .find(|flow| flow.name == name)
        .unwrap_or_else(|| panic!("routine model for {name:?}"))
}

fn occurrences<'a>(
    flow: &'a FlowSnapshot,
    variable: &str,
) -> impl Iterator<Item = &'a OccurrenceSnapshot> {
    flow.occurrences
        .iter()
        .filter(move |occurrence| occurrence.variable.eq_ignore_ascii_case(variable))
}

#[test]
fn flow_scaffolding_extracts_routine_locals_and_cfg() {
    let source = concat!(
        "program FlowFixture;\n",
        "procedure Demo;\n",
        "var Count: Integer; LabelText: string;\n",
        "begin Count := 1; end;\n",
        "begin Demo; end.\n",
    );

    let routines = flows(source);
    let routine = routines
        .iter()
        .find(|routine| routine.name == "Demo")
        .unwrap();

    assert_eq!(routine.locals.len(), 2);
    assert!(routine.cfg_blocks >= 3);
    assert_eq!(routine.excluded, None);
}

#[test]
fn flow_model_classifies_local_types_and_parameter_modes_conservatively() {
    let source = concat!(
        "program FlowTypes;\n",
        "type TRecord = record Value: Integer; end; TEnum = (One, Two); PInteger = ^Integer;\n",
        "procedure Demo(Value: Integer; var Ref: Integer; out Output: Integer; const ReadOnly: Integer);\n",
        "var Number: Integer; Text: string; R: TRecord; E: TEnum; P: PInteger;\n",
        "begin Ref := Value; Output := Number; end;\n",
        "begin end.\n",
    );
    let flow = routine(source, "Demo");
    let local = |name| {
        flow.locals
            .iter()
            .find(|local| local.name.eq_ignore_ascii_case(name))
            .unwrap_or_else(|| panic!("local {name:?}"))
    };

    assert_eq!(
        local("Value").kind,
        LocalKind::Parameter(LocalVarParameterMode::Value)
    );
    assert_eq!(
        local("Ref").kind,
        LocalKind::Parameter(LocalVarParameterMode::Var)
    );
    assert_eq!(
        local("Output").kind,
        LocalKind::Parameter(LocalVarParameterMode::Out)
    );
    assert_eq!(
        local("ReadOnly").kind,
        LocalKind::Parameter(LocalVarParameterMode::Const)
    );
    assert_eq!(local("Number").ty, VarType::Scalar);
    assert_eq!(local("Text").ty, VarType::Managed);
    assert_eq!(local("R").ty, VarType::Record);
    assert_eq!(local("E").ty, VarType::Unknown);
    assert_eq!(local("P").ty, VarType::Unknown);
}

#[test]
fn flow_occurrences_bind_reads_writes_and_keep_rhs_before_lhs_order() {
    let source = concat!(
        "program FlowAccess;\n",
        "procedure Demo;\n",
        "var Dest, Source: Integer;\n",
        "begin Dest := Source; end;\n",
        "begin Demo; end.\n",
    );
    let flow = routine(source, "Demo");
    let dest = occurrences(&flow, "Dest")
        .next()
        .expect("destination occurrence");
    let source_read = occurrences(&flow, "Source")
        .next()
        .expect("source occurrence");

    assert_eq!(dest.access, Access::Write);
    assert_eq!(source_read.access, Access::Read);
    assert_eq!(dest.block, source_read.block);
    assert!(source_read.order < dest.order);
    assert!(source_read.span.start > dest.span.start);
}

#[test]
fn flow_occurrences_map_if_loop_and_exception_statements_to_cfg_blocks() {
    let source = concat!(
        "program FlowBlocks;\n",
        "procedure Demo;\n",
        "var I, X, Y: Integer;\n",
        "begin\n",
        "  if X > 0 then Y := X;\n",
        "  for I := 1 to 2 do X := I;\n",
        "  try X := Y except Y := X end;\n",
        "end;\n",
        "begin Demo; end.\n",
    );
    let flow = routine(source, "Demo");
    let at = |needle: &str| source.find(needle).expect("source anchor");
    let occurrence = |variable: &str, offset: usize| {
        occurrences(&flow, variable)
            .find(|occurrence| occurrence.span.start == offset)
            .unwrap_or_else(|| panic!("{variable} at {offset}"))
    };

    let condition = occurrence("X", at("X > 0"));
    let then_read = occurrence("X", at("Y := X") + "Y := ".len());
    assert_ne!(condition.block, then_read.block);

    let loop_write = occurrence("I", at("I := 1"));
    let loop_read = occurrence("I", at("X := I") + "X := ".len());
    assert_eq!(
        flow.locals
            .iter()
            .find(|local| local.name == "I")
            .unwrap()
            .kind,
        LocalKind::LoopVar
    );
    assert_eq!(loop_write.access, Access::Write);
    assert_eq!(loop_read.access, Access::Read);
    assert_ne!(loop_write.block, loop_read.block);

    let try_rhs = occurrence("Y", at("X := Y") + "X := ".len());
    let except_rhs = occurrence("X", at("Y := X") + "Y := ".len());
    assert_ne!(try_rhs.block, except_rhs.block);
}

#[test]
fn flow_occurrences_classify_address_and_call_effects() {
    let source = concat!(
        "program FlowCalls;\n",
        "procedure Mutate(var Ref: Integer; out Output: Integer);\n",
        "begin Ref := Output; end;\n",
        "procedure Demo;\n",
        "var Value, Other: Integer; Address: Pointer;\n",
        "begin\n",
        "  Inc(Value);\n",
        "  Mutate(Value, Other);\n",
        "  Mystery(Value);\n",
        "  Address := @Other;\n",
        "  Address := Addr(Value);\n",
        "end;\n",
        "begin Demo; end.\n",
    );
    let flow = routine(source, "Demo");
    let accesses = |name: &str| {
        occurrences(&flow, name)
            .map(|occurrence| occurrence.access)
            .collect::<Vec<_>>()
    };

    assert!(accesses("Value").contains(&Access::ReadWrite));
    assert!(accesses("Value").contains(&Access::VarArg));
    assert!(accesses("Other").contains(&Access::OutArg));
    assert!(accesses("Other").contains(&Access::AddressOf));
    assert!(accesses("Value").contains(&Access::AddressOf));
    assert!(
        flow.locals
            .iter()
            .find(|local| local.name == "Value")
            .unwrap()
            .address_taken
    );
    assert!(
        flow.locals
            .iter()
            .find(|local| local.name == "Other")
            .unwrap()
            .address_taken
    );
}

#[test]
fn flow_model_marks_captures_absolute_storage_and_possible_with_targets() {
    let source = concat!(
        "program FlowExclusions;\n",
        "type TRecord = record A: Integer; end;\n",
        "procedure Outer;\n",
        "var Captured: Integer; AbsoluteValue: Integer absolute $1000; R: TRecord; A: Integer;\n",
        "  procedure Inner;\n",
        "  begin Captured := 1; end;\n",
        "begin\n",
        "  with R do A := 1;\n",
        "end;\n",
        "begin Outer; end.\n",
    );
    let flow = routine(source, "Outer");
    let local = |name| {
        flow.locals
            .iter()
            .find(|local| local.name.eq_ignore_ascii_case(name))
            .unwrap_or_else(|| panic!("local {name:?}"))
    };

    assert!(local("Captured").captured);
    assert!(local("AbsoluteValue").absolute);
    assert!(local("A").possible_with_target);
}

#[test]
fn flow_model_creates_result_local_and_binds_result_assignments() {
    let source = concat!(
        "program FlowResult;\n",
        "function Demo: Integer;\n",
        "begin Result := 1; Demo := Result; end;\n",
        "begin end.\n",
    );
    let flow = routine(source, "Demo");
    let result = flow
        .locals
        .iter()
        .position(|local| local.kind == LocalKind::Result)
        .expect("synthetic function result");

    assert_eq!(flow.locals[result].ty, VarType::Scalar);
    assert!(
        flow.occurrences
            .iter()
            .filter(|occurrence| occurrence.var == result)
            .any(|occurrence| occurrence.access == Access::Write)
    );
    assert!(
        flow.occurrences
            .iter()
            .filter(|occurrence| occurrence.var == result)
            .any(|occurrence| occurrence.access == Access::Read)
    );
}

#[test]
fn flow_model_fails_closed_for_recovery_unknown_conditionals_asm_goto_and_inline_vars() {
    let cases = [
        (
            "program Recovery; procedure Demo; var X: Integer; begin X := ; end; begin end.",
            "Demo",
            FlowExclusion::Recovery,
        ),
        (
            "program UnknownBranch; procedure Demo; var X: Integer; begin {$IFDEF MAYBE} X := 1; {$ENDIF} end; begin end.",
            "Demo",
            FlowExclusion::UnknownConditional,
        ),
        (
            "program Asm; procedure Demo; begin asm nop end; end; begin end.",
            "Demo",
            FlowExclusion::Asm,
        ),
        (
            "program GotoFixture; procedure Demo; label Done; begin goto Done; Done: end; begin end.",
            "Demo",
            FlowExclusion::Goto,
        ),
        (
            "program InlineVar; procedure Demo; begin var X: Integer; X := 1; end; begin end.",
            "Demo",
            FlowExclusion::InlineVariable,
        ),
    ];

    for (source, name, expected) in cases {
        let flow = routine(source, name);
        assert_eq!(flow.excluded, Some(expected), "{source}");
    }
}

#[test]
fn flow_diagnostics_pass_is_wired_but_emits_no_findings_yet() {
    let source = concat!(
        "program FlowNoFindings;\n",
        "procedure Demo;\n",
        "var NeverRead: Integer;\n",
        "begin NeverRead := 1; end;\n",
        "begin Demo; end.\n",
    );
    let uri = Url::parse("file:///tmp/flow-no-findings.pas").expect("valid URI");
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("fixture parses");
    let diagnostics = index
        .semantic_diagnostics_with_cancel(&uri, &AtomicBool::new(false))
        .expect("semantic report");
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("declared but never used")),
        "the scaffolding pass must not publish findings before analysis tasks: {diagnostics:?}"
    );
}
