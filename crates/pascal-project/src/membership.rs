#[derive(Debug)]
pub(super) enum PascalToken {
    Word(String),
    String(String),
    Comma,
    Semicolon,
    Dot,
    Other,
}

#[derive(Debug, Default)]
pub(super) struct UnitMembership {
    pub(super) explicit_paths: Vec<(String, String)>,
    pub(super) exhaustive: bool,
}

pub(super) fn parse_unit_membership(source: &str) -> UnitMembership {
    let tokens = lex_pascal(source);
    let mut result = UnitMembership {
        exhaustive: true,
        ..UnitMembership::default()
    };
    if contains_conditional_compiler_directive(source) {
        // The lexer deliberately removes directives, so a conditional uses or
        // contains clause cannot be proven exhaustive from the remaining
        // tokens. Keep explicit paths for positive lookup, but never use them
        // as negative ownership evidence.
        result.exhaustive = false;
    }
    let mut cursor = 0;
    while cursor < tokens.len() {
        let is_clause = matches!(
            tokens.get(cursor),
            Some(PascalToken::Word(word))
                if word.eq_ignore_ascii_case("uses") || word.eq_ignore_ascii_case("contains")
        );
        if !is_clause {
            cursor += 1;
            continue;
        }
        cursor += 1;
        let clause_start = cursor;
        while cursor < tokens.len() && !matches!(tokens[cursor], PascalToken::Semicolon) {
            cursor += 1;
        }
        parse_unit_clause(
            &tokens[clause_start..cursor],
            &mut result.explicit_paths,
            &mut result.exhaustive,
        );
        cursor = cursor.saturating_add(1);
    }
    result
}

fn contains_conditional_compiler_directive(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    ["{$if", "{$else", "{$endif", "(*$if", "(*$else", "(*$endif"]
        .iter()
        .any(|marker| lower.contains(marker))
}

fn parse_unit_clause(
    tokens: &[PascalToken],
    result: &mut Vec<(String, String)>,
    exhaustive: &mut bool,
) {
    let mut segment_start = 0;
    for index in 0..=tokens.len() {
        if index == tokens.len() || matches!(tokens[index], PascalToken::Comma) {
            if !parse_unit_segment(&tokens[segment_start..index], result) {
                *exhaustive = false;
            }
            segment_start = index.saturating_add(1);
        }
    }
}

fn parse_unit_segment(tokens: &[PascalToken], result: &mut Vec<(String, String)>) -> bool {
    if tokens.is_empty() {
        return true;
    }
    let Some(in_index) = tokens.iter().position(
        |token| matches!(token, PascalToken::Word(word) if word.eq_ignore_ascii_case("in")),
    ) else {
        return false;
    };
    let Some(PascalToken::String(path)) = tokens.get(in_index + 1) else {
        return false;
    };
    let mut unit_name = String::new();
    for token in &tokens[..in_index] {
        match token {
            PascalToken::Word(word) => unit_name.push_str(word),
            PascalToken::Dot => unit_name.push('.'),
            PascalToken::Other
            | PascalToken::String(_)
            | PascalToken::Comma
            | PascalToken::Semicolon => {}
        }
    }
    let unit_name = unit_name.trim().to_string();
    if !unit_name.is_empty() && !path.is_empty() {
        result.push((unit_name, path.clone()));
        true
    } else {
        false
    }
}

pub(super) fn lex_pascal(source: &str) -> Vec<PascalToken> {
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < source.len() {
        let rest = &source[cursor..];
        let Some(first) = rest.chars().next() else {
            break;
        };
        if first.is_whitespace() {
            cursor += first.len_utf8();
            continue;
        }
        if rest.starts_with("//") {
            cursor += 2;
            while cursor < source.len() {
                let current = source[cursor..]
                    .chars()
                    .next()
                    .expect("cursor is inside source");
                cursor += current.len_utf8();
                if current == '\n' {
                    break;
                }
            }
            continue;
        }
        if first == '{' {
            cursor += 1;
            while cursor < source.len() {
                let current = source[cursor..]
                    .chars()
                    .next()
                    .expect("cursor is inside source");
                cursor += current.len_utf8();
                if current == '}' {
                    break;
                }
            }
            continue;
        }
        if rest.starts_with("(*") {
            cursor += 2;
            while cursor < source.len() {
                if source[cursor..].starts_with("*)") {
                    cursor += 2;
                    break;
                }
                let current = source[cursor..]
                    .chars()
                    .next()
                    .expect("cursor is inside source");
                cursor += current.len_utf8();
            }
            continue;
        }
        if first == '\'' {
            cursor += 1;
            let mut value = String::new();
            while cursor < source.len() {
                let current = source[cursor..]
                    .chars()
                    .next()
                    .expect("cursor is inside source");
                cursor += current.len_utf8();
                if current != '\'' {
                    value.push(current);
                    continue;
                }
                if source[cursor..].starts_with('\'') {
                    value.push('\'');
                    cursor += 1;
                } else {
                    break;
                }
            }
            tokens.push(PascalToken::String(value));
            continue;
        }
        if first.is_ascii_alphabetic() || first == '_' {
            let start = cursor;
            cursor += first.len_utf8();
            while cursor < source.len() {
                let current = source[cursor..]
                    .chars()
                    .next()
                    .expect("cursor is inside source");
                if current.is_ascii_alphanumeric() || current == '_' {
                    cursor += current.len_utf8();
                } else {
                    break;
                }
            }
            tokens.push(PascalToken::Word(source[start..cursor].to_string()));
            continue;
        }
        let token = match first {
            ',' => PascalToken::Comma,
            ';' => PascalToken::Semicolon,
            '.' => PascalToken::Dot,
            _ => PascalToken::Other,
        };
        tokens.push(token);
        cursor += first.len_utf8();
    }
    tokens
}
