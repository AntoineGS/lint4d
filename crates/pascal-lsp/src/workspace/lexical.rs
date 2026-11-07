//! Lexical proofs over raw source bytes (R9-5): a member whose bytes, and
//! for references the bytes of the includes they resolve, cannot hold a
//! candidate name is excluded without binding it.

use crate::navigation::text_mentions_name;
use pascal_core::facts::{identifier_runs, is_identifier_byte};
use std::ops::Range;

/// UTF-8 encodings of the characters whose Unicode lowercase holds an
/// ASCII letter (`İ` → `i̇`, the Kelvin sign → `k`): a display spelling one
/// may match an ASCII query piece the bytes do not hold.
const ASCII_LOWERCASING: [&[u8]; 2] = ["\u{130}".as_bytes(), "\u{212A}".as_bytes()];

/// Whether raw source bytes prove that no candidate name occurs in their
/// own text (R9-5): not UTF-16, and no occurrence of any needle. `needles`
/// are non-empty ASCII last segments (references) or the identifier pieces
/// of the lowercased query (workspace symbols); an empty or non-ASCII
/// needle list proves nothing. References (`any_piece_missing = false`):
/// absent when no needle is a whole identifier (`holds_identifier`); the
/// caller must still follow the include directives (`include_requests`).
/// Symbols (`any_piece_missing = true`): absent when some piece is not a
/// substring; the symbol read parses the raw text without expanding its
/// includes, so they do not matter.
pub(crate) fn proves_absent(text: &[u8], needles: &[String], any_piece_missing: bool) -> bool {
    if needles.is_empty()
        || needles
            .iter()
            .any(|needle| needle.is_empty() || !needle.is_ascii())
        || !is_byte_text(text)
    {
        return false;
    }
    if any_piece_missing {
        if ASCII_LOWERCASING.iter().any(|spelling| {
            text.windows(spelling.len())
                .any(|window| window == *spelling)
        }) {
            return false;
        }
        needles
            .iter()
            .any(|needle| !text_mentions_name(text, needle))
    } else {
        !holds_identifier(text, needles)
    }
}

/// Not UTF-16 (no byte-order mark, no NUL byte): a byte search sees the
/// identifiers the analysis reads.
pub(crate) fn is_byte_text(text: &[u8]) -> bool {
    !(text.starts_with(&[0xff, 0xfe]) || text.starts_with(&[0xfe, 0xff]) || text.contains(&0))
}

/// Whether some identifier run of `text` (`identifier_runs`: every byte
/// that is not an ASCII letter, digit or `_` separates, non-ASCII bytes
/// included) equals a needle, ASCII case-insensitively. This is the scan
/// the snapshot itself filters its sources with; comments and strings
/// count.
pub(crate) fn holds_identifier(text: &[u8], needles: &[String]) -> bool {
    identifier_runs(text).any(|run| {
        needles
            .iter()
            .any(|needle| text[run.clone()].eq_ignore_ascii_case(needle.as_bytes()))
    })
}

/// An include directive of a member's raw text: its byte range and the file
/// name the resolver looks up (`pascal_core::resolver::include_name`).
#[derive(Debug)]
pub(crate) struct IncludeRequest {
    pub(crate) range: Range<usize>,
    pub(crate) name: String,
}

/// The include directives of `text`, or `None` when one cannot be followed.
/// Every `{$...}` and `(*$...*)` is read, also inside comments and strings
/// (more includes to follow, never fewer), and is an include when its
/// keyword is `I` or `INCLUDE` as the conditional scanner splits it
/// (`{$I+}`, `{$IFDEF}` are not). `None`: an include whose name is not a
/// literal file name (`{$I %DATE%}`, a macro, no name), an unterminated
/// one, a non-UTF-8 directive that may be one (decoded as ANSI, a leading
/// non-ASCII byte may be white space), or an include touching an
/// identifier byte, which the included text could extend.
pub(crate) fn include_requests(text: &[u8]) -> Option<Vec<IncludeRequest>> {
    let include = |keyword: &[u8]| {
        keyword.eq_ignore_ascii_case(b"I") || keyword.eq_ignore_ascii_case(b"INCLUDE")
    };
    let mut requests = Vec::new();
    let mut at = 0;
    while let Some(offset) = text[at..]
        .iter()
        .position(|byte| matches!(byte, b'{' | b'('))
    {
        let start = at + offset;
        at = start + 1;
        let (opener, closer): (&[u8], &[u8]) = if text[start] == b'{' {
            (b"{$", b"}")
        } else {
            (b"(*$", b"*)")
        };
        if !text[start..].starts_with(opener) {
            continue;
        }
        let body_start = start + opener.len();
        let body_end = text[body_start..]
            .windows(closer.len())
            .position(|window| window == closer)
            .map(|offset| body_start + offset);
        let body = &text[body_start..body_end.unwrap_or(text.len())];
        let Ok(body) = std::str::from_utf8(body) else {
            let keyword = body.trim_ascii_start();
            if keyword.first().is_some_and(|byte| !byte.is_ascii())
                || include(
                    keyword
                        .split(|byte| byte.is_ascii_whitespace() || *byte == b':')
                        .next()
                        .unwrap_or_default(),
                )
            {
                return None;
            }
            continue;
        };
        let keyword = body
            .trim_start()
            .split(|character: char| character.is_ascii_whitespace() || character == ':')
            .next()
            .unwrap_or_default();
        if !include(keyword.as_bytes()) {
            continue;
        }
        let end = body_end? + closer.len();
        let name =
            pascal_core::resolver::include_name(body).filter(|name| !name.starts_with('%'))?;
        let touches = |byte: Option<&u8>| byte.is_some_and(|byte| is_identifier_byte(*byte));
        if touches(start.checked_sub(1).and_then(|before| text.get(before)))
            || touches(text.get(end))
        {
            return None;
        }
        requests.push(IncludeRequest {
            range: start..end,
            name,
        });
    }
    Some(requests)
}

/// The needles of a workspace-symbol query: the non-empty pieces of the
/// lowercased query split at every ASCII character that is not a letter,
/// a digit or `_`. A matching display is source-spelled identifiers (and
/// the text between them), so each piece lies within one identifier.
pub(crate) fn symbol_needles(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|character: char| {
            character.is_ascii() && !(character.is_ascii_alphanumeric() || character == '_')
        })
        .filter(|piece| !piece.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absence_requires_every_condition() {
        let needles = vec!["tshared".to_owned()];
        assert!(proves_absent(
            b"unit A; interface implementation end.",
            &needles,
            false
        ));
        assert!(
            !proves_absent(b"var x: TSHARED;", &needles, false),
            "case-insensitive"
        );
        assert!(
            proves_absent(b"var x: TSharedEx;", &needles, false),
            "a longer identifier does not hold it"
        );
        assert!(
            !proves_absent(b"\xFF\xFEu\0n\0i\0t\0", &needles, false),
            "UTF-16"
        );
        assert!(
            !proves_absent(b"unit A;", &[], false),
            "no needle proves nothing"
        );
        assert!(
            !proves_absent(b"unit A;", &["ärger".to_owned()], false),
            "non-ASCII needle (A2)"
        );
    }

    #[test]
    fn a_symbol_query_is_absent_only_when_one_of_its_pieces_is() {
        let pieces = vec!["foo".to_owned(), "bar".to_owned()]; // query "Foo.Bar"
        assert!(!proves_absent(
            b"type TFoo = class procedure Bar; end;",
            &pieces,
            true
        ));
        assert!(proves_absent(
            b"type TFoo = class procedure Baz; end;",
            &pieces,
            true
        ));
    }

    #[test]
    fn symbol_pieces_split_at_every_non_identifier_ascii_character() {
        assert_eq!(symbol_needles("Vcl.Forms"), ["vcl", "forms"]);
        assert_eq!(symbol_needles("foo.bar"), ["foo", "bar"]);
        assert_eq!(symbol_needles("TList<T>"), ["tlist", "t"]);
        assert_eq!(symbol_needles("a_b9 c"), ["a_b9", "c"]);
        assert!(symbol_needles("").is_empty());
        assert!(symbol_needles(".<>").is_empty());
        assert_eq!(symbol_needles("Ärger"), ["ärger"]);
    }

    /// `Vcl.Forms` is the display of a dotted unit; a method's display is
    /// its name (`Bar`, owner `TFoo` apart), both spelled in the source.
    #[test]
    fn a_member_spelling_a_matching_display_is_never_absent() {
        let unit = b"unit Vcl.Forms;\ninterface\nimplementation\nend.\n";
        for query in ["vcl.forms", "Forms", "l.f", "VCL"] {
            assert!(
                !proves_absent(unit, &symbol_needles(query), true),
                "{query}"
            );
        }
        let method =
            b"unit U;\ninterface\ntype TFoo = class procedure Bar; end;\nimplementation\nprocedure TFoo.Bar; begin end;\nend.\n";
        for query in ["foo.bar", "TFoo.Bar", "bar", "Foo"] {
            assert!(
                !proves_absent(method, &symbol_needles(query), true),
                "{query}"
            );
        }
        assert!(proves_absent(unit, &symbol_needles("vcl.dialogs"), true));
    }

    /// `İ` and the Kelvin sign lowercase to text holding ASCII `i`/`k`, so
    /// a display spelling them may match an ASCII query the bytes lack.
    #[test]
    fn unicode_capitals_lowercasing_to_ascii_prove_nothing_for_symbols() {
        let pieces = symbol_needles("i");
        assert!(crate::workspace_index::name_matches("İx", "i"));
        assert!(proves_absent("const Ax = 1;".as_bytes(), &pieces, true));
        assert!(!proves_absent("const İx = 1;".as_bytes(), &pieces, true));
        let pieces = symbol_needles("kx");
        assert!(!proves_absent(
            "const \u{212A}x = 1;".as_bytes(),
            &pieces,
            true
        ));
    }

    #[test]
    fn a_nul_byte_proves_nothing() {
        assert!(!proves_absent(b"u\0n\0i\0t\0", &["x".to_owned()], false));
    }

    /// References match whole identifiers: runs of ASCII letters, digits
    /// and `_`, every other byte (non-ASCII ones included) separating them;
    /// comments and strings still count.
    #[test]
    fn references_match_whole_identifiers_only() {
        let needles = vec!["tdatabaseinfo".to_owned()];
        assert!(proves_absent(
            b"function GetDatabaseInfo: Integer;",
            &needles,
            false
        ));
        assert!(proves_absent(b"TDatabaseInfo_2 := 1;", &needles, false));
        for spelled in [
            "var x: TDatabaseInfo;",
            "x := (TDatabaseInfo)",
            "&TDatabaseInfo := 1;",
            "{ tdatabaseinfo in a comment }",
            "s := 'TDATABASEINFO';",
            "TDatabaseInfo\u{c4}",
            "\u{c4}TDatabaseInfo",
        ] {
            assert!(
                !proves_absent(spelled.as_bytes(), &needles, false),
                "{spelled}"
            );
        }
    }

    /// The symbol path reads raw text without expanding includes, so an
    /// include directive does not keep its proof.
    #[test]
    fn symbol_mode_ignores_include_directives() {
        let pieces = symbol_needles("vcl.dialogs");
        assert!(proves_absent(
            b"unit A;\n{$I Defs.inc}\ninterface\nimplementation\nend.\n",
            &pieces,
            true
        ));
    }

    fn requested(text: &str) -> Option<Vec<String>> {
        include_requests(text.as_bytes())
            .map(|requests| requests.into_iter().map(|request| request.name).collect())
    }

    #[test]
    fn include_requests_name_every_literal_include() {
        assert_eq!(
            requested("{$I Defs.inc}\n(*$INCLUDE 'a b.inc'*) {$i ..\\x.inc} {$ I Spaced.inc}"),
            Some(vec![
                "Defs.inc".to_owned(),
                "a b.inc".to_owned(),
                "../x.inc".to_owned(),
                "Spaced.inc".to_owned(),
            ])
        );
        assert_eq!(
            requested("{$I+} {$I-} {$IFDEF X}{$ENDIF} {$IF A}{$IFEND} {$IMPLICITBUILD OFF}"),
            Some(Vec::new())
        );
        let request = &include_requests(b"ab {$I x.inc} cd").unwrap()[0];
        assert_eq!(request.range, 3..13);
    }

    /// A directive the proof cannot follow refuses it: a computed or
    /// macro name, no name, an unterminated or non-UTF-8 directive, or one
    /// touching an identifier byte that the included text could extend.
    #[test]
    fn include_requests_refuse_what_they_cannot_follow() {
        for text in [
            "{$I %DATE%}",
            "{$I $(Root)\\x.inc}",
            "{$I}",
            "{$INCLUDE x.inc",
            "Foo{$I x.inc}",
            "{$I x.inc}Bar",
        ] {
            assert_eq!(requested(text), None, "{text}");
        }
        assert_eq!(include_requests(b"{$I \xff.inc}").map(|r| r.len()), None);
        // Decoded as ANSI, a leading non-ASCII byte may be white space.
        assert_eq!(include_requests(b"{$\xa0I x.inc}").map(|r| r.len()), None);
        assert_eq!(include_requests(b"{$R \xff.res}").map(|r| r.len()), Some(0));
    }
}
