use super::*;
use crate::navigation::{NavigationIndex, SourceSpan};
use cfg_pascal::cfg_core::types::EdgeKind;
use lsp_types::Url;
use std::collections::HashSet;
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
            cfg_blocks: routine.cfg.as_ref().map_or(0, |cfg| cfg.graph.node_count()),
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

fn syntax_node_count(root: tree_sitter::Node<'_>) -> usize {
    let mut count = 0;
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        count += 1;
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    count
}

fn first_executable_routine(root: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "defProc" && node.child_by_field_name("body").is_some() {
            return node;
        }
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    panic!("expected executable routine in fixture")
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
fn flow_occurrences_keep_finally_statement_copies_in_each_cfg_block() {
    let source = concat!(
        "program FlowFinallyCopies;\n",
        "procedure Demo(A: Integer);\n",
        "var Value: Integer;\n",
        "begin\n",
        "  try\n",
        "    if A > 0 then Exit;\n",
        "    Value := 0;\n",
        "  finally\n",
        "    Value := Value + 1;\n",
        "  end;\n",
        "end;\n",
        "begin Demo(1); end.\n",
    );
    let uri = Url::parse("file:///tmp/flow-finally-copies.pas").expect("valid URI");
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("fixture parses");
    let document = index.documents.get(&uri).expect("indexed document");
    let cancel = AtomicBool::new(false);
    let mut budget = AssistanceBudget::new(100_000, 8 * 1024 * 1024, "flow test");
    let flow = routine_flows(&index, &uri, document, &cancel, &mut budget)
        .expect("flow model builds")
        .into_iter()
        .find(|flow| flow.name == "Demo")
        .expect("Demo flow");
    let cfg = flow.cfg.as_ref().expect("Demo CFG");

    let statement_start = source
        .rfind("Value := Value + 1;")
        .expect("finally statement");
    let statement_end = statement_start + "Value := Value + 1;".len();
    let value_spans = flow
        .occurrences
        .iter()
        .filter(|occurrence| {
            statement_start <= occurrence.span.start && occurrence.span.end <= statement_end
        })
        .map(|occurrence| occurrence.span)
        .collect::<HashSet<_>>();
    assert_eq!(value_spans.len(), 2, "finally read and write occurrences");

    for span in value_spans {
        let cfg_copies = cfg
            .graph
            .node_weights()
            .filter(|block| {
                block.stmts.iter().any(|statement| {
                    statement.byte_range.start <= span.start && span.end <= statement.byte_range.end
                })
            })
            .count();
        let occurrence_copies = flow
            .occurrences
            .iter()
            .filter(|occurrence| occurrence.span == span)
            .count();
        assert!(
            cfg_copies > 1,
            "fixture should duplicate finally statements"
        );
        assert_eq!(
            occurrence_copies, cfg_copies,
            "every CFG copy must retain the source occurrence at {span:?}"
        );
    }
}

fn assert_finally_copies_keep_occurrences_on_exception_paths(
    source: &str,
    routine_name: &str,
    exception_source: &str,
) {
    let uri = Url::parse("file:///tmp/flow-finally-exception-copies.pas").expect("valid URI");
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("fixture parses");
    let document = index.documents.get(&uri).expect("indexed document");
    let cancel = AtomicBool::new(false);
    let mut budget = AssistanceBudget::new(100_000, 8 * 1024 * 1024, "flow test");
    let flow = routine_flows(&index, &uri, document, &cancel, &mut budget)
        .expect("flow model builds")
        .into_iter()
        .find(|flow| flow.name == routine_name)
        .unwrap_or_else(|| panic!("{routine_name} flow"));
    let cfg = flow.cfg.as_ref().expect("routine CFG");

    let cleanup_start = source
        .find("Value := Value + 1;")
        .expect("finally statement");
    let cleanup_blocks = cfg
        .graph
        .node_indices()
        .filter(|&block| {
            cfg.graph[block].stmts.iter().any(|statement| {
                statement.byte_range.start <= cleanup_start
                    && cleanup_start < statement.byte_range.end
            })
        })
        .collect::<HashSet<_>>();
    assert!(
        cleanup_blocks.len() > 1,
        "normal and exceptional continuations should have separate finally copies; \
         cleanup blocks: {cleanup_blocks:?}; CFG blocks: {:?}; CFG edges: {:?}",
        cfg.graph
            .node_indices()
            .map(|block| (
                block.index(),
                &cfg.graph[block].kind,
                cfg.graph[block]
                    .stmts
                    .iter()
                    .map(|statement| (
                        statement.byte_range.clone(),
                        source[statement.byte_range.clone()].to_owned()
                    ))
                    .collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>(),
        cfg.graph
            .edge_indices()
            .filter_map(|edge| {
                let (from, to) = cfg.graph.edge_endpoints(edge)?;
                Some((from.index(), to.index(), cfg.graph[edge].clone()))
            })
            .collect::<Vec<_>>()
    );

    let exception_cleanup_blocks = cfg
        .graph
        .edge_indices()
        .filter_map(|edge| {
            let (source_block, target_block) = cfg.graph.edge_endpoints(edge)?;
            if cfg.graph[edge] != EdgeKind::ExceptionThrow
                || !cleanup_blocks.contains(&target_block)
            {
                return None;
            }
            let enters_from_expected_source =
                cfg.graph[source_block].stmts.iter().any(|statement| {
                    source
                        .get(statement.byte_range.clone())
                        .is_some_and(|text| text.contains(exception_source))
                });
            enters_from_expected_source.then_some((source_block, target_block))
        })
        .collect::<Vec<_>>();
    assert!(
        !exception_cleanup_blocks.is_empty(),
        "an exception from {exception_source:?} must enter a finally copy"
    );

    for span in [
        SourceSpan {
            start: cleanup_start,
            end: cleanup_start + "Value".len(),
        },
        SourceSpan {
            start: cleanup_start + "Value := ".len(),
            end: cleanup_start + "Value := ".len() + "Value".len(),
        },
    ] {
        let occurrence_blocks = flow
            .occurrences
            .iter()
            .filter(|occurrence| occurrence.span == span)
            .map(|occurrence| occurrence.block.index())
            .collect::<HashSet<_>>();
        assert_eq!(
            occurrence_blocks, cleanup_blocks,
            "every finally CFG copy must retain the occurrence at {span:?}"
        );
    }
}

#[test]
fn flow_occurrences_keep_finally_copies_for_raise_transfers() {
    let source = concat!(
        "program FlowFinallyRaise;\n",
        "procedure Demo(A: Integer);\n",
        "var Value: Integer;\n",
        "begin\n",
        "  try\n",
        "    if A > 0 then raise Exception.Create('boom');\n",
        "    Value := 0;\n",
        "  finally\n",
        "    Value := Value + 1;\n",
        "  end;\n",
        "end;\n",
        "begin Demo(1); end.\n",
    );

    assert_finally_copies_keep_occurrences_on_exception_paths(
        source,
        "Demo",
        "raise Exception.Create",
    );
}

#[test]
fn flow_occurrences_keep_finally_copies_for_call_exception_transfers() {
    let source = concat!(
        "program FlowFinallyCallException;\n",
        "procedure Demo(A: Integer);\n",
        "var Value: Integer;\n",
        "begin\n",
        "  try\n",
        "    if A > 0 then MayThrow();\n",
        "    Value := 0;\n",
        "  finally\n",
        "    Value := Value + 1;\n",
        "  end;\n",
        "end;\n",
        "begin Demo(1); end.\n",
    );

    assert_finally_copies_keep_occurrences_on_exception_paths(source, "Demo", "MayThrow");
}

#[test]
fn flow_call_effects_cover_compound_var_and_out_arguments() {
    let source = concat!(
        "program FlowCompoundCalls;\n",
        "type TBox = record Item: Integer; end; TIntArray = array[0..9] of Integer;\n",
        "procedure Mutate(var Ref: Integer; out Output: Integer);\n",
        "begin Ref := Output; end;\n",
        "procedure Demo;\n",
        "var Box: TBox; Values: TIntArray; Index: Integer;\n",
        "begin Mutate(Box.Item, Values[Index]); end;\n",
        "begin Demo; end.\n",
    );
    let flow = routine(source, "Demo");
    let at = |needle: &str| source.find(needle).expect("source anchor");
    let access_at = |variable: &str, offset: usize| {
        occurrences(&flow, variable)
            .find(|occurrence| occurrence.span.start == offset)
            .unwrap_or_else(|| panic!("{variable} occurrence at {offset}"))
            .access
    };

    assert_eq!(access_at("Box", at("Box.Item")), Access::VarArg);
    assert_eq!(access_at("Values", at("Values[Index]")), Access::OutArg);
    assert_eq!(
        access_at("Index", at("Index]")),
        Access::Read,
        "subscript expressions remain reads"
    );
}

#[test]
fn flow_call_effects_bind_repeated_arguments_by_argument_position() {
    let source = concat!(
        "program FlowRepeatedArguments;\n",
        "procedure F(var First: Integer; out Second: Integer);\n",
        "begin First := 1; Second := 2; end;\n",
        "procedure Demo;\n",
        "var X: Integer;\n",
        "begin F(X, X); end;\n",
        "begin Demo; end.\n",
    );
    let flow = routine(source, "Demo");
    let first_x = source.find("F(X").expect("first argument") + 2;
    let second_x = source.find(", X").expect("second argument") + 2;
    let access_at = |offset| {
        flow.occurrences
            .iter()
            .find(|occurrence| occurrence.span.start == offset)
            .unwrap_or_else(|| panic!("X occurrence at {offset}"))
            .access
    };

    assert_eq!(access_at(first_x), Access::VarArg);
    assert_eq!(access_at(second_x), Access::OutArg);
}

#[test]
fn unresolved_and_intrinsic_call_effects_are_classified_independently() {
    let unknown_source = concat!(
        "program FlowUnknownCall;\n",
        "procedure Demo;\n",
        "var Value: Integer;\n",
        "begin Mystery(Value); end;\n",
        "begin Demo; end.\n",
    );
    let unknown = routine(unknown_source, "Demo");
    let unknown_value =
        unknown_source.find("Mystery(Value)").expect("unknown call") + "Mystery(".len();
    assert_eq!(
        occurrences(&unknown, "Value")
            .find(|occurrence| occurrence.span.start == unknown_value)
            .expect("unknown call argument")
            .access,
        Access::ReadWrite
    );

    let intrinsic_source = concat!(
        "program FlowIntrinsicCall;\n",
        "procedure Demo;\n",
        "var Value: Integer;\n",
        "begin Inc(Value); end;\n",
        "begin Demo; end.\n",
    );
    let intrinsic = routine(intrinsic_source, "Demo");
    let intrinsic_value =
        intrinsic_source.find("Inc(Value)").expect("intrinsic call") + "Inc(".len();
    assert_eq!(
        occurrences(&intrinsic, "Value")
            .find(|occurrence| occurrence.span.start == intrinsic_value)
            .expect("intrinsic call argument")
            .access,
        Access::ReadWrite
    );
}

#[test]
fn flow_call_effects_conservatively_cover_ambiguous_compound_arguments() {
    let source = concat!(
        "program FlowAmbiguousCall;\n",
        "procedure Demo;\n",
        "var Value, Other: Integer;\n",
        "begin Mystery(Value + Other); end;\n",
        "begin Demo; end.\n",
    );
    let flow = routine(source, "Demo");
    let argument = source.find("Mystery(Value + Other)").expect("unknown call") + "Mystery(".len();
    let value = occurrences(&flow, "Value")
        .find(|occurrence| occurrence.span.start == argument)
        .expect("first variable in ambiguous expression");
    let other = occurrences(&flow, "Other")
        .find(|occurrence| occurrence.span.start > argument)
        .expect("second variable in ambiguous expression");

    assert_eq!(value.access, Access::ReadWrite);
    assert_eq!(other.access, Access::ReadWrite);
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
fn flow_model_only_binds_bare_unqualified_result_as_the_synthetic_result() {
    let source = concat!(
        "program FlowQualifiedResult;\n",
        "type TBox = record Result: Integer; end;\n",
        "function Demo: Integer;\n",
        "var Box: TBox;\n",
        "begin Box.Result := 1; Result := Box.Result; end;\n",
        "begin end.\n",
    );
    let flow = routine(source, "Demo");
    let result = flow
        .locals
        .iter()
        .position(|local| local.kind == LocalKind::Result)
        .expect("synthetic function result");
    let bare_result = source.rfind("Result :=").expect("bare result assignment");
    let result_spans = flow
        .occurrences
        .iter()
        .filter(|occurrence| occurrence.var == result)
        .map(|occurrence| occurrence.span.start)
        .collect::<Vec<_>>();

    assert_eq!(result_spans, vec![bare_result]);
}

#[test]
fn flow_budget_exhaustion_is_reported_as_a_coverage_gap() {
    let source = concat!(
        "program FlowTinyBudget;\n",
        "procedure Demo;\n",
        "var Value: Integer;\n",
        "begin Value := 1; end;\n",
        "begin Demo; end.\n",
    );
    let uri = Url::parse("file:///tmp/flow-tiny-budget.pas").expect("valid URI");
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("fixture parses");
    let document = index.documents.get(&uri).expect("indexed document");
    let cancel = AtomicBool::new(false);
    let mut budget = AssistanceBudget::new(1, 8 * 1024 * 1024, "flow test");
    let result =
        super::flow_diagnostics_with_budget(&index, &uri, document, &[], 10, &cancel, &mut budget);
    let mut report = super::super::SemanticDiagnosticReport::default();
    assert!(
        report
            .checked_candidate(result, &budget)
            .expect("budget exhaustion is not cancellation")
            .is_none()
    );
    assert_eq!(report.gaps, vec![super::super::SemanticCoverageGap::Budget]);
}

#[test]
fn flow_statement_mapping_observes_work_budget_and_cancellation() {
    let source = concat!(
        "program FlowStatementBudget;\n",
        "procedure Demo;\n",
        "var Value: Integer;\n",
        "begin Value := 1; end;\n",
        "begin Demo; end.\n",
    );
    let uri = Url::parse("file:///tmp/flow-statement-budget.pas").expect("valid URI");
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("fixture parses");
    let document = index.documents.get(&uri).expect("indexed document");
    let cfg = cfg_pascal::build_file_cfgs(&document.tree, source.as_bytes())
        .into_iter()
        .find(|cfg| cfg.proc_name == "Demo")
        .expect("Demo CFG");
    let start = source.find("Value :=").expect("assignment");
    let span = SourceSpan {
        start,
        end: start + "Value".len(),
    };
    let mut budget = AssistanceBudget::new(0, 8 * 1024 * 1024, "flow statement mapping");
    assert!(
        super::model::statement_locations(&cfg, span, &AtomicBool::new(false), &mut budget)
            .expect_err("mapping must stop when the work limit is empty")
            .contains("node traversal limit")
    );
    assert!(budget.exhausted());

    let cancel = AtomicBool::new(true);
    let mut budget = AssistanceBudget::new(100, 8 * 1024 * 1024, "flow statement mapping");
    assert_eq!(
        super::model::statement_locations(&cfg, span, &cancel, &mut budget)
            .expect_err("mapping must honor cancellation"),
        "request cancelled"
    );
}

#[test]
fn flow_with_target_scan_reports_budget_exclusion() {
    let source = concat!(
        "program FlowWithBudget;\n",
        "type TRecord = record Field: Integer; end;\n",
        "procedure Demo;\n",
        "var R: TRecord; Value: Integer;\n",
        "begin with R do Value := 1; end;\n",
        "begin Demo; end.\n",
    );
    let uri = Url::parse("file:///tmp/flow-with-budget.pas").expect("valid URI");
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("fixture parses");
    let document = index.documents.get(&uri).expect("indexed document");
    let cancel = AtomicBool::new(false);
    let routine_node = first_executable_routine(document.tree.root_node());
    let mut collection_budget = AssistanceBudget::new(10_000, 8 * 1024 * 1024, "flow test");
    let body_nodes = super::model::collect_nodes(routine_node, &cancel, &mut collection_budget)
        .expect("routine syntax nodes are collected");
    let with_node = body_nodes
        .iter()
        .copied()
        .find(|node| node.kind() == "with")
        .expect("fixture with node");
    let with_body = with_node.child_by_field_name("body").expect("with body");
    let value_identifier = body_nodes
        .iter()
        .copied()
        .find(|node| {
            node.kind() == "identifier"
                && with_body.start_byte() <= node.start_byte()
                && node.end_byte() <= with_body.end_byte()
                && source.get(node.start_byte()..node.end_byte()) == Some("Value")
        })
        .expect("Value identifier inside with body");
    let scan_nodes = [with_node, value_identifier];
    let mut locals = vec![LocalVar {
        name: "Value".to_string(),
        decl_span: SourceSpan { start: 0, end: 0 },
        kind: LocalKind::Local,
        ty: VarType::Scalar,
        type_resolved: true,
        address_taken: false,
        captured: false,
        absolute: false,
        possible_with_target: false,
    }];
    let local_names = HashSet::from(["value".to_string()]);
    let mut excluded = None;
    let mut scan_budget = AssistanceBudget::new(2, 8 * 1024 * 1024, "flow test");

    assert!(
        !super::model::mark_possible_with_targets(
            document,
            &scan_nodes,
            &mut locals,
            &local_names,
            &mut excluded,
            &cancel,
            &mut scan_budget,
        )
        .expect("budget exhaustion in a with scan excludes the routine")
    );
    assert_eq!(excluded, Some(FlowExclusion::Budget));
    assert!(!locals[0].possible_with_target);
    assert!(scan_budget.exhausted());
}

#[test]
fn flow_cfg_build_precharge_excludes_a_routine_when_it_cannot_fit() {
    let source = concat!(
        "program FlowCfgBudget;\n",
        "procedure Demo;\n",
        "var Value: Integer;\n",
        "begin Value := 1; end;\n",
        "begin Demo; end.\n",
    );
    let uri = Url::parse("file:///tmp/flow-cfg-budget.pas").expect("valid URI");
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("fixture parses");
    let document = index.documents.get(&uri).expect("indexed document");
    let cancel = AtomicBool::new(false);
    let root = document.tree.root_node();
    let work_limit = syntax_node_count(root)
        .saturating_mul(2)
        .saturating_add(
            first_executable_routine(root)
                .descendant_count()
                .saturating_add(1),
        )
        .saturating_sub(1);
    let mut budget = AssistanceBudget::new(work_limit, 8 * 1024 * 1024, "flow test");
    let flows = routine_flows(&index, &uri, document, &cancel, &mut budget)
        .expect("an over-budget routine is excluded instead of built");
    let flow = flows
        .into_iter()
        .find(|flow| flow.name == "Demo")
        .expect("Demo flow");

    assert_eq!(flow.excluded, Some(FlowExclusion::Budget));
    assert!(flow.cfg.is_none(), "the CFG builder must skip the routine");
    assert!(budget.exhausted());
}

#[test]
fn flow_document_precharge_excludes_all_routines_before_cfg_building() {
    let source = concat!(
        "program FlowDocumentBudget;\n",
        "procedure First; begin end;\n",
        "procedure Second; begin end;\n",
        "begin First; Second; end.\n",
    );
    let uri = Url::parse("file:///tmp/flow-document-budget.pas").expect("valid URI");
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("fixture parses");
    let document = index.documents.get(&uri).expect("indexed document");
    let node_count = syntax_node_count(document.tree.root_node());
    let work_limit = node_count.saturating_mul(2).saturating_sub(1);
    let cancel = AtomicBool::new(false);
    let mut budget = AssistanceBudget::new(work_limit, 8 * 1024 * 1024, "flow test");
    super::model::reset_test_cfg_builder_call_count();
    let flows = routine_flows(&index, &uri, document, &cancel, &mut budget)
        .expect("a document precharge miss excludes routines");

    assert_eq!(flows.len(), 2, "both executable routines are represented");
    assert!(
        flows
            .iter()
            .all(|flow| flow.excluded == Some(FlowExclusion::Budget) && flow.cfg.is_none()),
        "no routine should retain a CFG or partial flow facts: {flows:?}"
    );
    assert_eq!(
        flows
            .iter()
            .map(|flow| flow.name.as_str())
            .collect::<HashSet<_>>(),
        HashSet::from(["First", "Second"])
    );
    assert_eq!(
        super::model::test_cfg_builder_call_count(),
        0,
        "document precharge must stop before invoking cfg-pascal"
    );
    assert!(budget.exhausted());

    let mut gap_budget = AssistanceBudget::new(work_limit, 8 * 1024 * 1024, "flow test");
    let result = super::flow_diagnostics_with_budget(
        &index,
        &uri,
        document,
        &[],
        10,
        &cancel,
        &mut gap_budget,
    );
    let mut report = super::super::SemanticDiagnosticReport::default();
    assert!(
        report
            .checked_candidate(result, &gap_budget)
            .expect("budget exhaustion is not cancellation")
            .is_none()
    );
    assert_eq!(report.gaps, vec![super::super::SemanticCoverageGap::Budget]);
    assert_eq!(
        super::model::test_cfg_builder_call_count(),
        0,
        "coverage-gap reporting must not invoke cfg-pascal either"
    );
}

#[test]
fn flow_model_cancellation_before_document_preprocessing_is_reported() {
    let source = concat!(
        "program FlowDocumentCancel;\n",
        "procedure First; begin end;\n",
        "procedure Second; begin end;\n",
        "begin First; Second; end.\n",
    );
    let uri = Url::parse("file:///tmp/flow-document-cancel.pas").expect("valid URI");
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("fixture parses");
    let document = index.documents.get(&uri).expect("indexed document");
    let cancel = AtomicBool::new(true);
    let mut budget = AssistanceBudget::new(100_000, 8 * 1024 * 1024, "flow test");

    assert_eq!(
        routine_flows(&index, &uri, document, &cancel, &mut budget)
            .expect_err("pre-cancelled routine modeling must stop"),
        "request cancelled"
    );
}

#[test]
fn flow_model_returns_cancellation_during_with_target_work() {
    let source = concat!(
        "program FlowWithCancel;\n",
        "type TRecord = record Field: Integer; end;\n",
        "procedure Demo;\n",
        "var R: TRecord; Value: Integer;\n",
        "begin with R do Value := 1; end;\n",
        "begin Demo; end.\n",
    );
    let uri = Url::parse("file:///tmp/flow-with-cancel.pas").expect("valid URI");
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("fixture parses");
    let document = index.documents.get(&uri).expect("indexed document");
    let setup_cancel = AtomicBool::new(false);
    let mut setup_budget = AssistanceBudget::new(1_000, 8 * 1024 * 1024, "flow test");
    let with_node =
        super::model::collect_nodes(document.tree.root_node(), &setup_cancel, &mut setup_budget)
            .expect("collect fixture nodes")
            .into_iter()
            .find(|node| node.kind() == "with")
            .expect("with statement");

    let cancel = AtomicBool::new(false);
    let mut budget = AssistanceBudget::new(100, 8 * 1024 * 1024, "flow test");
    budget.cancel_after_work(2);
    let mut locals = Vec::new();
    let mut excluded = None;

    assert_eq!(
        super::model::mark_possible_with_targets(
            document,
            &[with_node],
            &mut locals,
            &HashSet::new(),
            &mut excluded,
            &cancel,
            &mut budget,
        )
        .expect_err("cancellation during the with-body scan must stop flow modeling"),
        "request cancelled"
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
fn flow_diagnostics_report_a_written_only_local_as_h2077() {
    let source = concat!(
        "program FlowWrittenOnly;\n",
        "procedure Demo;\n",
        "var WrittenOnly: Integer;\n",
        "begin WrittenOnly := 1; end;\n",
        "begin Demo; end.\n",
    );
    let diagnostics = flow_diagnostics(source);
    assert_eq!(
        flow_finding_lines(source, &diagnostics),
        vec![(
            "H2077".to_owned(),
            4,
            "Value assigned to 'WrittenOnly' never used".to_owned(),
        )]
    );
}

fn flow_diagnostics(source: &str) -> Vec<SemanticDiagnostic> {
    let uri = Url::parse("file:///tmp/flow-diagnostics.pas").expect("valid URI");
    let mut index = NavigationIndex::new();
    index
        .update(uri.clone(), source.to_owned())
        .expect("fixture parses");
    let document = index.documents.get(&uri).expect("indexed document");
    let cancel = AtomicBool::new(false);
    let mut budget = AssistanceBudget::new(100_000, 8 * 1024 * 1024, "flow test");
    super::flow_diagnostics_with_budget(&index, &uri, document, &[], 100, &cancel, &mut budget)
        .expect("flow diagnostics build")
}

#[test]
fn flow_diagnostics_withhold_findings_when_a_routine_has_an_unresolved_identifier() {
    let source = concat!(
        "program FlowUnresolved;\n",
        "procedure Demo;\n",
        "var Value: Integer;\n",
        "begin\n",
        "  Value := 1;\n",
        "  Value := Missing;\n",
        "end;\n",
        "begin Demo; end.\n",
    );

    assert!(
        flow_diagnostics(source).is_empty(),
        "flow analysis must not publish findings for a routine with an unresolved identifier"
    );
}

#[test]
fn flow_diagnostics_withhold_findings_for_duplicate_local_identities() {
    let sources = [
        concat!(
            "program FlowDuplicateLocals;\n",
            "procedure Demo;\n",
            "var Value: Integer;\n",
            "    Value: string;\n",
            "begin end;\n",
            "begin end.\n",
        ),
        concat!(
            "program FlowDuplicateParameter;\n",
            "procedure Demo(Value: Integer);\n",
            "var Value: string;\n",
            "begin end;\n",
            "begin end.\n",
        ),
    ];

    for source in sources {
        assert!(
            flow_diagnostics(source).is_empty(),
            "duplicate local identities must not produce flow findings: {source}"
        );
    }
}

#[test]
fn flow_diagnostics_withhold_findings_for_an_unresolved_local_type() {
    let source = concat!(
        "program FlowUnknownLocalType;\n",
        "procedure Demo;\n",
        "var Value: MissingType;\n",
        "begin end;\n",
        "begin end.\n",
    );

    assert!(
        flow_diagnostics(source).is_empty(),
        "an unresolved local type must make the routine fail closed"
    );
}

fn flow_finding_lines(
    source: &str,
    diagnostics: &[SemanticDiagnostic],
) -> Vec<(String, usize, String)> {
    let mut findings = diagnostics
        .iter()
        .map(|diagnostic| {
            let line = source[..diagnostic.span.start]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
                + 1;
            (
                diagnostic.kind.code().to_owned(),
                line,
                diagnostic.message.clone(),
            )
        })
        .collect::<Vec<_>>();
    findings.sort();
    findings
}

#[test]
fn flow_diagnostics_report_unused_locals_and_dead_assignments_at_dcc_locations() {
    let source = concat!(
        "program FlowUsage;\n",
        "procedure Demo;\n",
        "var NeverRead, WrittenOnly, Reassigned, Used: Integer;\n",
        "begin\n",
        "  WrittenOnly := 1;\n",
        "  Reassigned := 1;\n",
        "  Reassigned := 2;\n",
        "  if Reassigned = 0 then WriteLn('used');\n",
        "  Used := 3;\n",
        "  WriteLn(Used);\n",
        "end;\n",
        "begin Demo; end.\n",
    );
    let expected = vec![
        (
            "H2077".to_owned(),
            5,
            "Value assigned to 'WrittenOnly' never used".to_owned(),
        ),
        (
            "H2077".to_owned(),
            6,
            "Value assigned to 'Reassigned' never used".to_owned(),
        ),
        (
            "H2164".to_owned(),
            3,
            "Variable 'NeverRead' is declared but never used in 'Demo'".to_owned(),
        ),
    ];
    let diagnostics = flow_diagnostics(source);
    assert_eq!(flow_finding_lines(source, &diagnostics), expected);
    assert!(
        diagnostics
            .iter()
            .all(|finding| { finding.kind.severity() == crate::navigation::DccSeverity::Hint })
    );
}

#[test]
fn flow_diagnostics_keep_only_the_initial_dead_write_before_a_nonempty_constant_loop() {
    let source = concat!(
        "program FlowConstantLoop;\n",
        "procedure Demo;\n",
        "var I, Value: Integer;\n",
        "begin\n",
        "  Value := 1;\n",
        "  for I := 1 to 2 do\n",
        "    Value := I;\n",
        "  WriteLn(Value);\n",
        "end;\n",
        "begin Demo; end.\n",
    );

    assert_eq!(
        flow_finding_lines(source, &flow_diagnostics(source)),
        vec![(
            "H2077".to_owned(),
            5,
            "Value assigned to 'Value' never used".to_owned(),
        )]
    );
}

#[test]
fn flow_diagnostics_fail_closed_for_address_captured_absolute_and_with_locals() {
    let source = concat!(
        "program FlowExclusionDiagnostics;\n",
        "type TBox = record WithTarget: Integer; end;\n",
        "procedure TouchAddress(Value: Pointer); begin end;\n",
        "procedure Demo;\n",
        "var Addressed, Captured, WithTarget, SafeDead: Integer;\n",
        "    Box: TBox;\n",
        "    AbsoluteValue: Integer absolute $1000;\n",
        "  procedure Inner; begin Captured := 1; end;\n",
        "begin\n",
        "  Addressed := 1;\n",
        "  TouchAddress(@Addressed);\n",
        "  Addressed := 2;\n",
        "  AbsoluteValue := 1;\n",
        "  with Box do WithTarget := 1;\n",
        "  SafeDead := 1;\n",
        "end;\n",
        "begin Demo; end.\n",
    );
    let diagnostics = flow_diagnostics(source);
    assert_eq!(
        flow_finding_lines(source, &diagnostics),
        vec![(
            "H2077".to_owned(),
            15,
            "Value assigned to 'SafeDead' never used".to_owned(),
        )]
    );
}

#[test]
fn flow_diagnostics_withhold_findings_for_excluded_routines() {
    let positive = concat!(
        "program FlowUsagePositive;\n",
        "procedure Demo; var Value: Integer; begin Value := 1; end;\n",
        "begin Demo; end.\n",
    );
    assert!(
        flow_diagnostics(positive)
            .iter()
            .any(|finding| finding.kind.code() == "H2077"),
        "the positive control must produce a flow hint"
    );

    let excluded = [
        "program Recovery; procedure Demo; var Value: Integer; begin Value := ; end; begin end.",
        "program UnknownBranch; procedure Demo; var Value: Integer; begin {$IFDEF MAYBE} Value := 1; {$ENDIF} end; begin end.",
        "program Asm; procedure Demo; var Value: Integer; begin Value := 1; asm nop end; end; begin end.",
        "program Goto; procedure Demo; label Done; var Value: Integer; begin Value := 1; goto Done; Done: end; begin end.",
    ];
    for source in excluded {
        assert!(
            flow_diagnostics(source).is_empty(),
            "an excluded routine must not publish partial flow findings: {source}"
        );
    }
}
