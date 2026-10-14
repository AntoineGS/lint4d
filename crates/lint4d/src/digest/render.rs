//! Text, index and JSON views of a [`FileDigest`]. Pure functions of the model.

use std::path::Path;

use serde::Serialize;

use super::model::*;

/// The last path component, used in headers and the index.
pub fn file_name(path: &str) -> String {
    Path::new(path).file_name().map_or_else(
        || path.to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

pub fn render_index(file: &FileDigest) -> String {
    let count = file.routines.len();
    let mut out = format!(
        "{}  {count} routine{}\n",
        file_name(&file.path),
        if count == 1 { "" } else { "s" }
    );
    let spans: Vec<String> = file
        .routines
        .iter()
        .map(|r| format!("L{}-{}", r.start_line, r.end_line))
        .collect();
    let span_w = spans.iter().map(String::len).max().unwrap_or(0);
    let count_w = file
        .routines
        .iter()
        .map(|r| r.line_count().to_string().len())
        .max()
        .unwrap_or(0);
    let name_w = file
        .routines
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);
    for (routine, span) in file.routines.iter().zip(&spans) {
        let line = format!(
            "{span:<span_w$}  {:>count_w$}  {:<name_w$}  {}",
            routine.line_count(),
            routine.name,
            routine.markers().render()
        );
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Header, degradation lines, objects line, frequent calls line, then the body. With
/// `max_lines`, the deepest control subtrees collapse on a best-effort basis;
/// collapsing stops at the top level, so the output can still exceed
/// `max_lines`. The header lines are never collapsed.
pub fn render_routine(
    routine: &RoutineDigest,
    file_name: &str,
    max_lines: Option<usize>,
) -> String {
    let mut head = vec![format!(
        "{}  {file_name}:{}-{} ({} lines)",
        routine.name,
        routine.start_line,
        routine.end_line,
        routine.line_count()
    )];
    head.extend(
        routine
            .degraded
            .iter()
            .map(|d| format!("! {}", degradation_text(d))),
    );
    if !routine.objects.is_empty() {
        let objects: Vec<String> = routine.objects.iter().map(object_text).collect();
        head.push(format!("objects: {}", objects.join("; ")));
    }
    if !routine.frequent_calls.is_empty() {
        head.push(format!(
            "frequent calls: {}",
            routine
                .frequent_calls
                .iter()
                .map(|call| format!("{} ×{}", call.name, call.count))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let footer = has_objects_and_calls(routine).then_some(CALLS_NOTE);
    let mut body = Vec::new();
    render_nodes(&routine.body, 0, usize::MAX, &mut body);
    if let Some(limit) = max_lines {
        let budget = limit.saturating_sub(head.len() + usize::from(footer.is_some()));
        let mut depth = max_depth(&routine.body, 0);
        while body.len() > budget {
            body.clear();
            render_nodes(&routine.body, 0, depth, &mut body);
            if depth == 0 {
                break;
            }
            depth -= 1;
        }
    }
    head.extend(body);
    head.extend(footer.map(str::to_string));
    let mut out = head.join("\n");
    out.push('\n');
    out
}

const CALLS_NOTE: &str = "note: calls are names only; callees not analysed";

/// Whether the routine lists objects and shows any call.
fn has_objects_and_calls(routine: &RoutineDigest) -> bool {
    !routine.objects.is_empty() && (!routine.frequent_calls.is_empty() || any_calls(&routine.body))
}

fn any_calls(nodes: &[DigestNode]) -> bool {
    nodes.iter().any(|node| match node {
        DigestNode::Control { children, .. } => any_calls(children),
        DigestNode::Fact {
            fact: Fact::Calls { calls } | Fact::Lambda { calls },
            ..
        } => !calls.is_empty(),
        DigestNode::Fact { .. } => false,
    })
}

/// The uncollapsed body only.
pub fn render_tree(nodes: &[DigestNode]) -> String {
    let mut lines = Vec::new();
    render_nodes(nodes, 0, usize::MAX, &mut lines);
    lines.iter().map(|line| format!("{line}\n")).collect()
}

pub fn render_json(path: &str, routines: &[&RoutineDigest]) -> String {
    #[derive(Serialize)]
    struct Output<'a> {
        path: &'a str,
        routines: &'a [&'a RoutineDigest],
    }
    serde_json::to_string_pretty(&Output { path, routines }).expect("digest model serializes")
}

fn max_depth(nodes: &[DigestNode], depth: usize) -> usize {
    nodes
        .iter()
        .filter_map(|node| match node {
            DigestNode::Control { children, .. }
                if !children.is_empty() && fold_chain(node).is_none() =>
            {
                Some(max_depth(children, depth + 1).max(depth))
            }
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn render_nodes(nodes: &[DigestNode], depth: usize, collapse_at: usize, out: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    for node in nodes {
        match node {
            DigestNode::Fact { line, fact } => {
                out.push(format!("{indent}L{line} {}", fact_text(fact)))
            }
            DigestNode::Control {
                kind,
                line,
                end_line,
                label,
                children,
            } => {
                if let Some(folded) = fold_chain(node) {
                    out.push(format!("{indent}{folded}"));
                } else if depth >= collapse_at && !children.is_empty() {
                    let lines = end_line + 1 - line;
                    out.push(format!(
                        "{indent}L{line}-{end_line} {} … ({lines} line{}{})",
                        head_text(*kind, label),
                        if lines == 1 { "" } else { "s" },
                        calls_suffix(children)
                    ));
                } else if *kind == ControlKind::Arm {
                    out.push(format!("{indent}{label}:"));
                    render_nodes(children, depth + 1, collapse_at, out);
                } else {
                    out.push(format!("{indent}L{line} {}", head_text(*kind, label)));
                    render_nodes(children, depth + 1, collapse_at, out);
                }
            }
        }
    }
}

fn foldable(kind: ControlKind) -> bool {
    matches!(
        kind,
        ControlKind::If
            | ControlKind::Else
            | ControlKind::Arm
            | ControlKind::For
            | ControlKind::While
            | ControlKind::With
            | ControlKind::On
    )
}

/// One line for a chain of single-child controls ending in a single fact.
/// It is one line already, so `--max-lines` never collapses it.
fn fold_chain(node: &DigestNode) -> Option<String> {
    let mut prefix = String::new();
    let mut parts: Vec<String> = Vec::new();
    let mut current = node;
    loop {
        match current {
            DigestNode::Fact { line, fact } => {
                parts.push(format!("L{line} {}", fact_text(fact)));
                break;
            }
            DigestNode::Control {
                kind,
                line,
                label,
                children,
                ..
            } => {
                if !foldable(*kind) {
                    return None;
                }
                let [only] = children.as_slice() else {
                    return None;
                };
                if *kind == ControlKind::Arm {
                    prefix = format!("{label}: ");
                } else {
                    parts.push(format!("L{line} {}", head_text(*kind, label)));
                }
                current = only;
            }
        }
    }
    Some(format!("{prefix}{}", parts.join(" → ")))
}

fn calls_suffix(nodes: &[DigestNode]) -> String {
    let mut names: Vec<String> = Vec::new();
    collect_call_names(nodes, &mut names);
    if names.is_empty() {
        return String::new();
    }
    let more = names.len() > 5;
    names.truncate(5);
    format!(
        ", calls {}{}",
        names.join(", "),
        if more { ", …" } else { "" }
    )
}

fn collect_call_names(nodes: &[DigestNode], names: &mut Vec<String>) {
    for node in nodes {
        match node {
            DigestNode::Control { children, .. } => collect_call_names(children, names),
            DigestNode::Fact {
                fact: Fact::Calls { calls },
                ..
            } => {
                for call in calls {
                    if !names.contains(&call.name) {
                        names.push(call.name.clone());
                    }
                }
            }
            DigestNode::Fact { .. } => {}
        }
    }
}

fn head_text(kind: ControlKind, label: &str) -> String {
    match kind {
        ControlKind::If => format!("if {label}"),
        ControlKind::Else | ControlKind::ExceptElse => "else".to_string(),
        ControlKind::Case => format!("case {label}"),
        ControlKind::Arm => format!("{label}:"),
        ControlKind::For | ControlKind::While | ControlKind::Branch => label.to_string(),
        ControlKind::Repeat => format!("repeat until {label}"),
        ControlKind::Try => "try".to_string(),
        ControlKind::Finally => "finally".to_string(),
        ControlKind::Except => "except".to_string(),
        ControlKind::On => format!("on {label}"),
        ControlKind::With => format!("with {label}"),
        ControlKind::Conditional => "conditional".to_string(),
        ControlKind::Asm => "asm".to_string(),
    }
}

fn calls_text(calls: &[CallCount]) -> String {
    calls
        .iter()
        .map(|call| match call.count {
            1 => call.name.clone(),
            n => format!("{} ×{n}", call.name),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn fact_text(fact: &Fact) -> String {
    match fact {
        Fact::New {
            class,
            target: Some(target),
        } => format!("new {class} -> {target}"),
        Fact::New {
            class,
            target: None,
        } => format!("new {class}"),
        Fact::Free { var } => format!("free {var}"),
        Fact::Tx {
            op,
            receiver,
            method,
        } => {
            let op = match op {
                TxOp::Start => "start",
                TxOp::Commit => "commit",
                TxOp::Rollback => "rollback",
                TxOp::InTransaction => "in-transaction",
            };
            format!("tx {op} {receiver}.{method}")
        }
        Fact::Raise { text } => format!("raise {text}"),
        Fact::ReRaise => "re-raise".to_string(),
        Fact::Exit {
            runs_finally: Some(line),
        } => format!("exit (runs finally L{line})"),
        Fact::Exit { runs_finally: None } => "exit".to_string(),
        Fact::Calls { calls } => format!("calls {}", calls_text(calls)),
        Fact::Nested { name } => format!("nested routine {name}"),
        Fact::Lambda { calls } if calls.is_empty() => "anonymous method".to_string(),
        Fact::Lambda { calls } => format!("anonymous method calls {}", calls_text(calls)),
    }
}

fn degradation_text(degradation: &Degradation) -> String {
    match degradation {
        Degradation::ParseErrors { lines } => {
            let lines: Vec<String> = lines.iter().map(|line| format!("L{line}")).collect();
            format!("parse errors at {}", lines.join(", "))
        }
        Degradation::Conditional { regions } => {
            format!("conditional code: {regions} {{$IF…}} regions, all branches shown")
        }
        Degradation::HiddenDirective { line } => format!("hidden directive block at L{line}"),
        Degradation::AnonymousMethods => "anonymous methods not analysed".to_string(),
    }
}

fn line_range([first, last]: [usize; 2]) -> String {
    if first == last {
        format!("L{first}")
    } else {
        format!("L{first}-{last}")
    }
}

fn object_text(object: &ObjectVerdict) -> String {
    let verdict = match &object.verdict {
        Verdict::FreedAllPaths => "freed on all paths".to_string(),
        Verdict::Escapes { via, more } => {
            let mut items: Vec<String> = via
                .iter()
                .map(|escape| {
                    let how = match escape.how {
                        EscapeKind::Exception => "exception",
                        EscapeKind::Exit => "exit",
                        EscapeKind::FallsOffEnd => "falls off end",
                        EscapeKind::ReCreated => "re-created",
                    };
                    format!("L{} {how}", escape.line)
                })
                .collect();
            if *more {
                items.push("…".to_string());
            }
            format!("NOT freed on all paths (escapes via {})", items.join(", "))
        }
        Verdict::NotFreedHere { why } => {
            let why = match why {
                NotFreedReason::Result => "returned as Result: caller owns",
                NotFreedReason::NonLocal => "assigned to a non-local: owner frees",
                NotFreedReason::NeverFreed => "no free found: leak candidate",
            };
            format!("not freed in this routine ({why})")
        }
        Verdict::Unknown { why } => format!("unknown ({why})"),
    };
    let verify = object
        .verify_lines
        .map(|range| format!(" — verify {}", line_range(range)))
        .unwrap_or_default();
    format!(
        "{} {} L{} {verdict}{verify}",
        object.var, object.class, object.line
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(line: usize, fact: Fact) -> DigestNode {
        DigestNode::Fact { line, fact }
    }

    fn call(name: &str, count: usize) -> CallCount {
        CallCount {
            name: name.to_string(),
            count,
        }
    }

    fn arm(line: usize, end_line: usize, label: &str, children: Vec<DigestNode>) -> DigestNode {
        DigestNode::Control {
            kind: ControlKind::Arm,
            line,
            end_line,
            label: label.to_string(),
            children,
        }
    }

    fn sample() -> RoutineDigest {
        RoutineDigest {
            name: "TFoo.Run".to_string(),
            start_line: 10,
            end_line: 30,
            byte_range: 0..100,
            frequent_calls: Vec::new(),
            objects: vec![
                ObjectVerdict {
                    var: "L".into(),
                    class: "TStringList".into(),
                    line: 12,
                    verify_lines: None,
                    verdict: Verdict::FreedAllPaths,
                },
                ObjectVerdict {
                    var: "Q".into(),
                    class: "TQuery".into(),
                    line: 13,
                    verify_lines: Some([13, 16]),
                    verdict: Verdict::Escapes {
                        via: vec![
                            Escape {
                                line: 14,
                                how: EscapeKind::Exception,
                            },
                            Escape {
                                line: 15,
                                how: EscapeKind::Exit,
                            },
                            Escape {
                                line: 16,
                                how: EscapeKind::FallsOffEnd,
                            },
                        ],
                        more: true,
                    },
                },
                ObjectVerdict {
                    var: "Result".into(),
                    class: "TList".into(),
                    line: 17,
                    verify_lines: None,
                    verdict: Verdict::NotFreedHere {
                        why: NotFreedReason::Result,
                    },
                },
                ObjectVerdict {
                    var: "M".into(),
                    class: "TList".into(),
                    line: 18,
                    verify_lines: Some([18, 20]),
                    verdict: Verdict::Unknown {
                        why: "reassigned at L20".into(),
                    },
                },
            ],
            body: vec![
                fact(
                    11,
                    Fact::Calls {
                        calls: vec![call("Log", 2), call("Prepare", 1)],
                    },
                ),
                DigestNode::Control {
                    kind: ControlKind::Case,
                    line: 19,
                    end_line: 24,
                    label: "A".into(),
                    children: vec![
                        arm(
                            20,
                            20,
                            "1",
                            vec![fact(
                                20,
                                Fact::Tx {
                                    op: TxOp::Commit,
                                    receiver: "Db".into(),
                                    method: "Commit".into(),
                                },
                            )],
                        ),
                        arm(
                            21,
                            23,
                            "else",
                            vec![
                                fact(22, Fact::Free { var: "L".into() }),
                                fact(23, Fact::ReRaise),
                            ],
                        ),
                    ],
                },
                fact(
                    25,
                    Fact::Exit {
                        runs_finally: Some(28),
                    },
                ),
            ],
            degraded: vec![
                Degradation::Conditional { regions: 2 },
                Degradation::AnonymousMethods,
            ],
        }
    }

    const HEAD: &str = "TFoo.Run  Foo.pas:10-30 (21 lines)
! conditional code: 2 {$IF…} regions, all branches shown
! anonymous methods not analysed
objects: L TStringList L12 freed on all paths; Q TQuery L13 NOT freed on all paths (escapes via L14 exception, L15 exit, L16 falls off end, …) — verify L13-16; Result TList L17 not freed in this routine (returned as Result: caller owns); M TList L18 unknown (reassigned at L20) — verify L18-20
";
    const FOOTER: &str = "note: calls are names only; callees not analysed\n";

    #[test]
    fn renders_a_full_routine() {
        let expected = format!(
            "{HEAD}L11 calls Log ×2, Prepare
L19 case A
  1: L20 tx commit Db.Commit
  else:
    L22 free L
    L23 re-raise
L25 exit (runs finally L28)
{FOOTER}"
        );
        assert_eq!(render_routine(&sample(), "Foo.pas", None), expected);
    }

    #[test]
    fn collapses_deepest_controls_to_fit_max_lines() {
        let expected = format!(
            "{HEAD}L11 calls Log ×2, Prepare
L19-24 case A … (6 lines)
L25 exit (runs finally L28)
{FOOTER}"
        );
        assert_eq!(render_routine(&sample(), "Foo.pas", Some(8)), expected);
    }

    #[test]
    fn renders_the_index_from_markers() {
        let file = FileDigest {
            path: "src/Foo.pas".into(),
            routines: vec![sample()],
        };
        assert_eq!(
            render_index(&file),
            "Foo.pas  1 routine\nL10-30  21  TFoo.Run  tx raise1 exit1 !escape ?\n"
        );
    }

    #[test]
    fn json_uses_tagged_variants() {
        let routine = sample();
        let json: serde_json::Value =
            serde_json::from_str(&render_json("src/Foo.pas", &[&routine])).unwrap();
        assert_eq!(json["path"], "src/Foo.pas");
        let r = &json["routines"][0];
        assert_eq!(r["objects"][1]["verdict"]["status"], "escapes");
        assert_eq!(r["objects"][1]["verdict"]["via"][0]["how"], "exception");
        assert_eq!(r["body"][0]["node"], "fact");
        assert_eq!(r["body"][0]["fact"]["kind"], "calls");
        assert_eq!(r["body"][1]["kind"], "case");
        assert_eq!(r["degraded"][1]["kind"], "anonymous_methods");
    }

    fn verdict_text(verdict: Verdict, verify_lines: Option<[usize; 2]>) -> String {
        object_text(&ObjectVerdict {
            var: "L".into(),
            class: "TList".into(),
            line: 9,
            verify_lines,
            verdict,
        })
    }

    #[test]
    fn notices_follow_each_verdict() {
        let escapes = |lines: &[usize]| Verdict::Escapes {
            via: lines
                .iter()
                .map(|&line| Escape {
                    line,
                    how: EscapeKind::Exception,
                })
                .collect(),
            more: false,
        };
        assert_eq!(
            verdict_text(escapes(&[10, 11]), Some([9, 11])),
            "L TList L9 NOT freed on all paths (escapes via L10 exception, L11 exception) — verify L9-11"
        );
        assert_eq!(
            verdict_text(escapes(&[9]), Some([9, 9])),
            "L TList L9 NOT freed on all paths (escapes via L9 exception) — verify L9"
        );
        assert_eq!(
            verdict_text(
                Verdict::Unknown {
                    why: "reassigned at L12".into()
                },
                Some([9, 12])
            ),
            "L TList L9 unknown (reassigned at L12) — verify L9-12"
        );
        assert_eq!(
            verdict_text(
                Verdict::NotFreedHere {
                    why: NotFreedReason::Result
                },
                None
            ),
            "L TList L9 not freed in this routine (returned as Result: caller owns)"
        );
        assert_eq!(
            verdict_text(
                Verdict::NotFreedHere {
                    why: NotFreedReason::NonLocal
                },
                None
            ),
            "L TList L9 not freed in this routine (assigned to a non-local: owner frees)"
        );
        assert_eq!(
            verdict_text(
                Verdict::NotFreedHere {
                    why: NotFreedReason::NeverFreed
                },
                Some([9, 40])
            ),
            "L TList L9 not freed in this routine (no free found: leak candidate) — verify L9-40"
        );
        assert_eq!(
            verdict_text(Verdict::FreedAllPaths, None),
            "L TList L9 freed on all paths"
        );
    }

    fn with_body(objects: bool, body: Vec<DigestNode>, frequent: Vec<CallCount>) -> RoutineDigest {
        let mut routine = sample();
        if !objects {
            routine.objects.clear();
        }
        routine.body = body;
        routine.frequent_calls = frequent;
        routine
    }

    fn has_footer(routine: &RoutineDigest) -> bool {
        render_routine(routine, "Foo.pas", None).ends_with(FOOTER)
    }

    #[test]
    fn footer_needs_objects_and_calls() {
        let calls = || {
            vec![fact(
                11,
                Fact::Calls {
                    calls: vec![call("Log", 1)],
                },
            )]
        };
        assert!(has_footer(&with_body(true, calls(), Vec::new())));
        assert!(!has_footer(&with_body(false, calls(), Vec::new())));
        assert!(!has_footer(&with_body(
            true,
            vec![fact(11, Fact::ReRaise)],
            Vec::new()
        )));
        assert!(has_footer(&with_body(
            true,
            vec![fact(11, Fact::ReRaise)],
            vec![call("Log", 3)]
        )));
        assert!(has_footer(&with_body(
            true,
            vec![fact(
                11,
                Fact::Lambda {
                    calls: vec![call("Log", 1)]
                }
            )],
            Vec::new()
        )));
        assert!(!has_footer(&with_body(
            true,
            vec![fact(11, Fact::Lambda { calls: Vec::new() })],
            Vec::new()
        )));
    }

    #[test]
    fn footer_is_never_collapsed_away() {
        let text = render_routine(&sample(), "Foo.pas", Some(1));
        assert!(text.ends_with(FOOTER), "{text}");
    }

    /// One routine holding every variant of the model, so that a change to
    /// the JSON or text contract shows up in the snapshots.
    fn every_variant() -> RoutineDigest {
        let control = |kind, line, end_line, label: &str, children| DigestNode::Control {
            kind,
            line,
            end_line,
            label: label.to_string(),
            children,
        };
        let object = |var: &str, line, verify_lines, verdict| ObjectVerdict {
            var: var.into(),
            class: "TObj".into(),
            line,
            verify_lines,
            verdict,
        };
        let escape = |line, how| Escape { line, how };
        let tx = |op, method: &str| Fact::Tx {
            op,
            receiver: "Db".into(),
            method: method.into(),
        };
        RoutineDigest {
            name: "TAll.Every".to_string(),
            start_line: 1,
            end_line: 90,
            byte_range: 0..900,
            objects: vec![
                object("A", 5, None, Verdict::FreedAllPaths),
                object(
                    "B",
                    6,
                    Some([6, 20]),
                    Verdict::Escapes {
                        via: vec![
                            escape(10, EscapeKind::Exception),
                            escape(15, EscapeKind::Exit),
                            escape(18, EscapeKind::FallsOffEnd),
                            escape(20, EscapeKind::ReCreated),
                        ],
                        more: true,
                    },
                ),
                object(
                    "Result",
                    7,
                    None,
                    Verdict::NotFreedHere {
                        why: NotFreedReason::Result,
                    },
                ),
                object(
                    "FItems",
                    8,
                    None,
                    Verdict::NotFreedHere {
                        why: NotFreedReason::NonLocal,
                    },
                ),
                object(
                    "C",
                    9,
                    Some([9, 90]),
                    Verdict::NotFreedHere {
                        why: NotFreedReason::NeverFreed,
                    },
                ),
                object(
                    "D",
                    10,
                    Some([10, 12]),
                    Verdict::Unknown {
                        why: "reassigned at L12".into(),
                    },
                ),
                object(
                    "E",
                    11,
                    Some([11, 90]),
                    Verdict::Unknown {
                        why: "creation not in CFG".into(),
                    },
                ),
            ],
            frequent_calls: vec![call("Log", 4)],
            body: vec![
                fact(
                    20,
                    Fact::New {
                        class: "TObj".into(),
                        target: Some("A".into()),
                    },
                ),
                fact(
                    21,
                    Fact::New {
                        class: "TObj".into(),
                        target: None,
                    },
                ),
                fact(22, Fact::Free { var: "A".into() }),
                fact(23, tx(TxOp::Start, "StartTransaction")),
                fact(24, tx(TxOp::Commit, "Commit")),
                fact(25, tx(TxOp::Rollback, "Rollback")),
                fact(26, tx(TxOp::InTransaction, "InTransaction")),
                fact(
                    27,
                    Fact::Raise {
                        text: "Exception.Create('x')".into(),
                    },
                ),
                fact(28, Fact::ReRaise),
                fact(29, Fact::Exit { runs_finally: None }),
                fact(
                    30,
                    Fact::Exit {
                        runs_finally: Some(60),
                    },
                ),
                fact(
                    31,
                    Fact::Calls {
                        calls: vec![call("One", 1), call("Two", 3)],
                    },
                ),
                fact(
                    32,
                    Fact::Nested {
                        name: "Inner".into(),
                    },
                ),
                fact(33, Fact::Lambda { calls: Vec::new() }),
                fact(
                    34,
                    Fact::Lambda {
                        calls: vec![call("Cb", 2)],
                    },
                ),
                control(
                    ControlKind::If,
                    35,
                    36,
                    "A > 0",
                    vec![fact(36, Fact::ReRaise)],
                ),
                control(ControlKind::Else, 37, 38, "", vec![fact(38, Fact::ReRaise)]),
                control(
                    ControlKind::Case,
                    39,
                    44,
                    "N",
                    vec![
                        control(ControlKind::Arm, 40, 41, "1", vec![fact(41, Fact::ReRaise)]),
                        control(
                            ControlKind::Arm,
                            42,
                            44,
                            "else",
                            vec![fact(43, Fact::ReRaise)],
                        ),
                    ],
                ),
                control(
                    ControlKind::For,
                    45,
                    47,
                    "for I := 0 to 3",
                    vec![fact(46, Fact::ReRaise), fact(47, Fact::ReRaise)],
                ),
                control(
                    ControlKind::While,
                    48,
                    50,
                    "while X",
                    vec![fact(49, Fact::ReRaise), fact(50, Fact::ReRaise)],
                ),
                control(
                    ControlKind::Repeat,
                    51,
                    53,
                    "Done",
                    vec![fact(52, Fact::ReRaise), fact(53, Fact::ReRaise)],
                ),
                control(ControlKind::Try, 54, 55, "", vec![fact(55, Fact::ReRaise)]),
                control(
                    ControlKind::Finally,
                    56,
                    58,
                    "",
                    vec![fact(57, Fact::ReRaise), fact(58, Fact::ReRaise)],
                ),
                control(
                    ControlKind::Except,
                    59,
                    69,
                    "",
                    vec![
                        control(
                            ControlKind::On,
                            60,
                            62,
                            "E: EAbort",
                            vec![fact(61, Fact::ReRaise), fact(62, Fact::ReRaise)],
                        ),
                        control(
                            ControlKind::ExceptElse,
                            63,
                            65,
                            "",
                            vec![fact(64, Fact::ReRaise), fact(65, Fact::ReRaise)],
                        ),
                    ],
                ),
                control(
                    ControlKind::With,
                    71,
                    73,
                    "Obj",
                    vec![fact(72, Fact::ReRaise), fact(73, Fact::ReRaise)],
                ),
                control(
                    ControlKind::Conditional,
                    74,
                    76,
                    "",
                    vec![fact(75, Fact::ReRaise), fact(76, Fact::ReRaise)],
                ),
                control(
                    ControlKind::Branch,
                    77,
                    79,
                    "{$IF X}",
                    vec![fact(78, Fact::ReRaise), fact(79, Fact::ReRaise)],
                ),
                control(ControlKind::Asm, 80, 82, "", Vec::new()),
            ],
            degraded: vec![
                Degradation::ParseErrors { lines: vec![3, 4] },
                Degradation::Conditional { regions: 2 },
                Degradation::HiddenDirective { line: 12 },
                Degradation::AnonymousMethods,
            ],
        }
    }

    #[test]
    fn json_contract_covers_every_variant() {
        let routine = every_variant();
        insta::assert_snapshot!(render_json("src/All.pas", &[&routine]));
    }

    #[test]
    fn text_covers_every_variant() {
        insta::assert_snapshot!(render_routine(&every_variant(), "All.pas", None));
    }
}
