#![allow(dead_code)]

pub(crate) fn form_object_classes(bytes: &[u8]) -> Option<Vec<String>> {
    if bytes.starts_with(b"TPF0") {
        return None;
    }
    let text = std::str::from_utf8(bytes)
        .ok()?
        .trim_start_matches('\u{feff}');
    let mut lexical = LexicalState::default();
    let mut frames = Vec::new();
    let mut collections = Vec::new();
    let mut classes = Vec::new();
    let mut roots = 0usize;
    let mut awaiting_collection = false;

    for line in text.lines() {
        let visible = visible_line(line, &mut lexical)?;
        let line = visible.trim();
        if line.is_empty() {
            continue;
        }

        if awaiting_collection {
            if line == "<" {
                collections.push(frames.len());
                awaiting_collection = false;
                continue;
            }
            awaiting_collection = false;
        }

        if line == ">" {
            if collections.pop()? != frames.len() {
                return None;
            }
            continue;
        }

        let (keyword, rest) = first_word(line);
        if keyword.eq_ignore_ascii_case("object")
            || keyword.eq_ignore_ascii_case("inherited")
            || keyword.eq_ignore_ascii_case("inline")
        {
            let class_name = parse_object_header(rest)?;
            if frames.is_empty() {
                roots = roots.checked_add(1)?;
                if roots > 1 {
                    return None;
                }
            }
            classes.push(class_name);
            frames.push(Frame::Object);
            continue;
        }
        if keyword.eq_ignore_ascii_case("item") {
            if !rest.is_empty() || collections.is_empty() || frames.is_empty() {
                return None;
            }
            frames.push(Frame::Item);
            continue;
        }
        if keyword.eq_ignore_ascii_case("end") {
            if !rest.is_empty() {
                return None;
            }
            frames.pop()?;
            continue;
        }

        if let Some((_, value)) = line.split_once('=') {
            match value.trim() {
                "<" => collections.push(frames.len()),
                "" => awaiting_collection = true,
                _ => {}
            }
        }
    }

    if lexical.in_string
        || lexical.brace_depth != 0
        || !frames.is_empty()
        || !collections.is_empty()
        || roots != 1
    {
        return None;
    }
    Some(classes)
}

#[derive(Default)]
struct LexicalState {
    in_string: bool,
    brace_depth: usize,
}

#[derive(Clone, Copy)]
enum Frame {
    Object,
    Item,
}

fn visible_line(line: &str, state: &mut LexicalState) -> Option<String> {
    let mut visible = String::with_capacity(line.len());
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        if state.brace_depth > 0 {
            match character {
                '{' => state.brace_depth = state.brace_depth.checked_add(1)?,
                '}' => state.brace_depth -= 1,
                _ => {}
            }
            visible.push(' ');
            continue;
        }
        if state.in_string {
            if character == '\'' {
                if characters.peek() == Some(&'\'') {
                    characters.next();
                    visible.push_str("  ");
                    continue;
                }
                state.in_string = false;
            }
            visible.push(' ');
            continue;
        }
        match character {
            '\'' => {
                state.in_string = true;
                visible.push(' ');
            }
            '{' => {
                state.brace_depth = 1;
                visible.push(' ');
            }
            '}' => return None,
            '/' if characters.peek() == Some(&'/') => break,
            _ => visible.push(character),
        }
    }
    Some(visible)
}

fn first_word(line: &str) -> (&str, &str) {
    let end = line
        .char_indices()
        .find_map(|(index, character)| character.is_ascii_whitespace().then_some(index))
        .unwrap_or(line.len());
    (&line[..end], line[end..].trim_start())
}

fn parse_object_header(header: &str) -> Option<String> {
    let header = header.trim_start();
    let (_, after_name) = identifier(header)?;
    let after_name = after_name.trim_start().strip_prefix(':')?.trim_start();
    let (first_class_part, mut rest) = identifier(after_name)?;
    let mut class_name = first_class_part.to_owned();
    loop {
        rest = rest.trim_start();
        let Some(after_dot) = rest.strip_prefix('.') else {
            break;
        };
        let (part, after_part) = identifier(after_dot.trim_start())?;
        class_name.push('.');
        class_name.push_str(part);
        rest = after_part;
    }

    let rest = rest.trim();
    if rest.is_empty() {
        return Some(class_name);
    }
    let index = rest.strip_prefix('[')?.strip_suffix(']')?.trim();
    if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(class_name)
}

fn identifier(text: &str) -> Option<(&str, &str)> {
    let mut characters = text.char_indices();
    let (_, first) = characters.next()?;
    if !identifier_start(first) {
        return None;
    }
    let mut end = first.len_utf8();
    for (index, character) in characters {
        if !identifier_continue(character) {
            break;
        }
        end = index + character.len_utf8();
    }
    Some((&text[..end], &text[end..]))
}

fn identifier_start(character: char) -> bool {
    character == '_' || character.is_alphabetic()
}

fn identifier_continue(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::form_object_classes;

    #[test]
    fn parses_nested_inherited_inline_and_collection_objects() {
        let form = br#"object MainForm: TMainForm
  Caption = 'object Phantom: TPhantom'#13
    'end'
  Items = <
    item
      Caption = 'collection item'
      Data = {
        00 01 02 03
      }
    end
  >
  object Button1: TButton
  end
  inherited ParentControl: TControl
  end
  inline SharedFrame: TFrame [0]
  end
end
"#;

        assert_eq!(
            form_object_classes(form),
            Some(vec![
                "TMainForm".to_owned(),
                "TButton".to_owned(),
                "TControl".to_owned(),
                "TFrame".to_owned(),
            ])
        );
    }

    #[test]
    fn binary_dfm_signature_is_not_text() {
        assert_eq!(form_object_classes(b"TPF0\x01\x02\x03"), None);
    }

    #[test]
    fn malformed_text_form_is_rejected() {
        assert_eq!(
            form_object_classes(b"object MainForm TMainForm\nend\n"),
            None
        );
    }
}
