//! `--routine` / `--line` selection with suggestions on a miss.

use super::model::{FileDigest, RoutineDigest};

const MAX_SUGGESTIONS: usize = 10;

pub enum Selector {
    Index,
    All,
    Routine(String),
    Line(usize),
}

#[derive(Debug)]
pub struct SelectError {
    pub message: String,
    pub suggestions: Vec<String>,
}

pub fn select<'a>(
    file: &'a FileDigest,
    selector: &Selector,
) -> Result<Vec<&'a RoutineDigest>, SelectError> {
    match selector {
        Selector::Index | Selector::All => Ok(file.routines.iter().collect()),
        Selector::Routine(name) => {
            let exact: Vec<_> = file
                .routines
                .iter()
                .filter(|r| r.name.eq_ignore_ascii_case(name))
                .collect();
            if !exact.is_empty() {
                return Ok(exact);
            }
            let wanted = last_component(name);
            let by_last: Vec<_> = file
                .routines
                .iter()
                .filter(|r| last_component(&r.name).eq_ignore_ascii_case(wanted))
                .collect();
            if !by_last.is_empty() {
                return Ok(by_last);
            }
            Err(SelectError {
                message: format!("no routine named '{name}' in {}", file.path),
                suggestions: closest_by_name(file, name),
            })
        }
        Selector::Line(line) => file
            .routines
            .iter()
            .filter(|r| r.start_line <= *line && *line <= r.end_line)
            .min_by_key(|r| r.end_line - r.start_line)
            .map(|r| vec![r])
            .ok_or_else(|| SelectError {
                message: format!("no routine contains line {line} in {}", file.path),
                suggestions: closest_by_line(file, *line),
            }),
    }
}

fn last_component(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn suggestion(routine: &RoutineDigest) -> String {
    format!(
        "{} (L{}-{})",
        routine.name, routine.start_line, routine.end_line
    )
}

fn closest_by_name(file: &FileDigest, name: &str) -> Vec<String> {
    let query = name.to_ascii_lowercase();
    let query_last = last_component(&query).to_string();
    let mut scored: Vec<(usize, &RoutineDigest)> = file
        .routines
        .iter()
        .map(|r| {
            let candidate = r.name.to_ascii_lowercase();
            let distance = levenshtein(&query, &candidate)
                .min(levenshtein(&query_last, last_component(&candidate)));
            (distance, r)
        })
        .collect();
    scored.sort_by_key(|(distance, r)| (*distance, r.start_line));
    scored
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(_, r)| suggestion(r))
        .collect()
}

fn closest_by_line(file: &FileDigest, line: usize) -> Vec<String> {
    let mut scored: Vec<(usize, &RoutineDigest)> = file
        .routines
        .iter()
        .map(|r| {
            (
                if line < r.start_line {
                    r.start_line - line
                } else {
                    line.saturating_sub(r.end_line)
                },
                r,
            )
        })
        .collect();
    scored.sort_by_key(|(distance, r)| (*distance, r.start_line));
    scored
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(_, r)| suggestion(r))
        .collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut current = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != *cb);
            current.push(substitution.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        previous = current;
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routine(name: &str, start_line: usize, end_line: usize) -> RoutineDigest {
        RoutineDigest {
            name: name.into(),
            start_line,
            end_line,
            byte_range: 0..0,
            objects: Vec::new(),
            body: Vec::new(),
            degraded: Vec::new(),
        }
    }

    fn file() -> FileDigest {
        FileDigest {
            path: "t.pas".into(),
            routines: vec![
                routine("TFoo.Load", 5, 8),
                routine("TFoo.Load", 10, 13),
                routine("TBar.Save", 15, 30),
                routine("TBar.Save.Inner", 17, 20),
                routine("T.<initialization>", 32, 33),
            ],
        }
    }

    fn names(selected: &[&RoutineDigest]) -> Vec<(String, usize)> {
        selected
            .iter()
            .map(|r| (r.name.clone(), r.start_line))
            .collect()
    }

    #[test]
    fn index_and_all_select_everything() {
        assert_eq!(select(&file(), &Selector::Index).unwrap().len(), 5);
        assert_eq!(select(&file(), &Selector::All).unwrap().len(), 5);
    }

    #[test]
    fn qualified_names_match_case_insensitively_with_overloads() {
        let file = file();
        let selected = select(&file, &Selector::Routine("tfoo.load".into())).unwrap();
        assert_eq!(
            names(&selected),
            vec![("TFoo.Load".into(), 5), ("TFoo.Load".into(), 10)]
        );
    }

    #[test]
    fn last_component_is_the_fallback() {
        let file = file();
        for query in ["save", "TBaz.Save"] {
            let selected = select(&file, &Selector::Routine(query.into())).unwrap();
            assert_eq!(names(&selected), vec![("TBar.Save".into(), 15)], "{query}");
        }
    }

    #[test]
    fn line_selects_the_innermost_routine() {
        let file = file();
        assert_eq!(
            names(&select(&file, &Selector::Line(18)).unwrap()),
            vec![("TBar.Save.Inner".into(), 17)]
        );
        assert_eq!(
            names(&select(&file, &Selector::Line(32)).unwrap()),
            vec![("T.<initialization>".into(), 32)]
        );
    }

    #[test]
    fn misses_suggest_the_closest_routines() {
        let error = select(&file(), &Selector::Routine("Lod".into())).unwrap_err();
        assert!(
            error.message.contains("no routine named 'Lod'"),
            "{}",
            error.message
        );
        assert_eq!(error.suggestions[0], "TFoo.Load (L5-8)");
        let error = select(&file(), &Selector::Line(2)).unwrap_err();
        assert!(
            error.message.contains("no routine contains line 2"),
            "{}",
            error.message
        );
        assert_eq!(error.suggestions[0], "TFoo.Load (L5-8)");
        assert!(error.suggestions.len() <= 10);
    }
}
