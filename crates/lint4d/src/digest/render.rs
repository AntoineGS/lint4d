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
    let mut body = Vec::new();
    render_nodes(&routine.body, 0, usize::MAX, &mut body);
    if let Some(limit) = max_lines {
        let budget = limit.saturating_sub(head.len());
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
    let mut out = head.join("\n");
    out.push('\n');
    out
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
            DigestNode::Control { children, .. } if !children.is_empty() => {
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
                if depth >= collapse_at && !children.is_empty() {
                    let lines = end_line + 1 - line;
                    out.push(format!(
                        "{indent}L{line}-{end_line} {} … ({lines} line{}{})",
                        head_text(*kind, label),
                        if lines == 1 { "" } else { "s" },
                        calls_suffix(children)
                    ));
                } else if let Some(folded) = fold_chain(node, depth, collapse_at) {
                    out.push(format!("{indent}{folded}"));
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
/// A link that `--max-lines` would collapse ends the attempt.
fn fold_chain(node: &DigestNode, depth: usize, collapse_at: usize) -> Option<String> {
    let mut prefix = String::new();
    let mut parts: Vec<String> = Vec::new();
    let mut current = node;
    let mut level = depth;
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
                if !foldable(*kind) || level >= collapse_at {
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
                level += 1;
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
                NotFreedReason::Result => "returned as Result",
                NotFreedReason::NonLocal => "assigned to a non-local",
                NotFreedReason::NeverFreed => "no free found",
            };
            format!("not freed in this routine ({why})")
        }
        Verdict::Unknown { why } => format!("unknown ({why})"),
    };
    format!("{} {} L{} {verdict}", object.var, object.class, object.line)
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
                    verdict: Verdict::FreedAllPaths,
                },
                ObjectVerdict {
                    var: "Q".into(),
                    class: "TQuery".into(),
                    line: 13,
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
                    verdict: Verdict::NotFreedHere {
                        why: NotFreedReason::Result,
                    },
                },
                ObjectVerdict {
                    var: "M".into(),
                    class: "TList".into(),
                    line: 18,
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
objects: L TStringList L12 freed on all paths; Q TQuery L13 NOT freed on all paths (escapes via L14 exception, L15 exit, L16 falls off end, …); Result TList L17 not freed in this routine (returned as Result); M TList L18 unknown (reassigned at L20)
";

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
"
        );
        assert_eq!(render_routine(&sample(), "Foo.pas", None), expected);
    }

    #[test]
    fn collapses_deepest_controls_to_fit_max_lines() {
        let expected = format!(
            "{HEAD}L11 calls Log ×2, Prepare
L19-24 case A … (6 lines)
L25 exit (runs finally L28)
"
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
}
