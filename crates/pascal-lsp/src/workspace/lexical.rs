//! Lexical proofs over raw source bytes (R9-5): a member whose bytes cannot
//! hold a candidate name is excluded without binding it.

use crate::navigation::text_mentions_name;

/// UTF-8 encodings of the characters whose Unicode lowercase holds an
/// ASCII letter (`İ` → `i̇`, the Kelvin sign → `k`): a display spelling one
/// may match an ASCII query piece the bytes do not hold.
const ASCII_LOWERCASING: [&[u8]; 2] = ["\u{130}".as_bytes(), "\u{212A}".as_bytes()];

/// Whether raw source bytes prove that no candidate name occurs in the
/// analysed text (R9-5): no include directive, not UTF-16, and no
/// case-insensitive ASCII occurrence of any needle. `needles` are
/// non-empty ASCII last segments (references) or the identifier pieces of
/// the lowercased query (workspace symbols); an empty or non-ASCII needle
/// list proves nothing. References: absent when no needle occurs. Symbols:
/// the caller passes `any_piece_missing = true` (absent when some piece
/// does not occur).
pub(crate) fn proves_absent(text: &[u8], needles: &[String], any_piece_missing: bool) -> bool {
    if needles.is_empty()
        || needles
            .iter()
            .any(|needle| needle.is_empty() || !needle.is_ascii())
        || text.starts_with(&[0xff, 0xfe])
        || text.starts_with(&[0xfe, 0xff])
        || text.contains(&0)
        || has_include_directive(text)
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
        !needles
            .iter()
            .any(|needle| text_mentions_name(text, needle))
    }
}

/// `{$I name}`, `{$INCLUDE name}`, `(*$I name*)`, `(*$INCLUDE name*)`,
/// case-insensitive; `{$I+}` and `{$I-}` (the I/O-check switch) and longer
/// directive names (`{$IF`, `{$IFDEF`, `{$IMPLICITBUILD`) are not includes.
pub(crate) fn has_include_directive(text: &[u8]) -> bool {
    let mut at = 0;
    while at < text.len() {
        let start = match text[at..]
            .iter()
            .position(|byte| matches!(byte, b'{' | b'('))
        {
            Some(offset) => at + offset,
            None => return false,
        };
        let opener: &[u8] = if text[start] == b'{' { b"{$" } else { b"(*$" };
        at = start + 1;
        if !text[start..].starts_with(opener) {
            continue;
        }
        let name_start = start + opener.len();
        let name_end = text[name_start..]
            .iter()
            .position(|byte| !byte.is_ascii_alphabetic())
            .map_or(text.len(), |offset| name_start + offset);
        let name = &text[name_start..name_end];
        if name.eq_ignore_ascii_case(b"INCLUDE")
            || (name.eq_ignore_ascii_case(b"I") && !matches!(text.get(name_end), Some(b'+' | b'-')))
        {
            return true;
        }
    }
    false
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
    fn include_directives_are_recognised_and_switches_are_not() {
        assert!(has_include_directive(b"{$I Spring.inc}"));
        assert!(has_include_directive(b"{$i spring.inc}"));
        assert!(has_include_directive(b"(*$INCLUDE a.inc*)"));
        assert!(has_include_directive(b"{$INCLUDE 'a b.inc'}"));
        assert!(!has_include_directive(
            b"{$I+} {$I-} {$IFDEF X}{$ENDIF} {$IF CompilerVersion > 20}{$IFEND} {$IMPLICITBUILD OFF}"
        ));
    }

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
            !proves_absent(b"var x: TSharedEx;", &needles, false),
            "substring, no word boundary"
        );
        assert!(
            !proves_absent(b"{$I x.inc} unit A;", &needles, false),
            "an include may spell it"
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
}
