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
    if !needles_are_provable(needles) || !is_byte_text(text) {
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

/// Whether a byte search can prove `needles` absent: some, none empty,
/// all ASCII (A2).
pub(crate) fn needles_are_provable(needles: &[String]) -> bool {
    !needles.is_empty()
        && needles
            .iter()
            .all(|needle| !needle.is_empty() && needle.is_ascii())
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
/// count. Like that filter it does not split a run a lexer would split
/// after a number (`1e5Name`, `$FFName`).
pub(crate) fn holds_identifier(text: &[u8], needles: &[String]) -> bool {
    identifier_runs(text).any(|run| {
        needles
            .iter()
            .any(|needle| text[run.clone()].eq_ignore_ascii_case(needle.as_bytes()))
    })
}

/// Whether some identifier run of `text` (as `holds_identifier` splits it)
/// starts with a prefix, ASCII case-insensitively (l4d-fpyu).
pub(crate) fn holds_identifier_prefix(text: &[u8], prefixes: &[String]) -> bool {
    identifier_runs(text).any(|run| {
        let run = &text[run];
        prefixes.iter().any(|prefix| {
            run.get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix.as_bytes()))
        })
    })
}

/// An include directive of a member's raw text: its byte range and the file
/// name the resolver looks up (`pascal_core::resolver::include_name`).
#[derive(Debug, Clone)]
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

/// The hash `TokenFilter` keys an identifier by: FNV-1a over its ASCII
/// lowercased bytes, then a splitmix64 finalizer.
fn token_hash(token: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in token {
        hash = (hash ^ u64::from(byte.to_ascii_lowercase())).wrapping_mul(0x0100_0000_01b3);
    }
    hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

/// The identifier hashes (`token_hash`) of some texts, distinct once
/// compacted: what a `TokenFilter` is sized and built from.
#[derive(Debug, Clone, Default)]
pub(crate) struct TokenHashes(Vec<u64>);

impl TokenHashes {
    /// Adds every identifier of `text` (`identifier_runs`).
    pub(crate) fn insert_text(&mut self, text: &[u8]) {
        self.0
            .extend(identifier_runs(text).map(|run| token_hash(&text[run])));
        self.compact();
    }

    pub(crate) fn union(&mut self, other: &Self) {
        self.0.extend_from_slice(&other.0);
        self.compact();
    }

    fn compact(&mut self) {
        self.0.sort_unstable();
        self.0.dedup();
    }

    pub(crate) fn into_filter(self) -> TokenFilter {
        TokenFilter::of(&self.0)
    }
}

/// The whole identifiers of some texts (ASCII lowercased) as a Bloom filter
/// of about `TOKEN_FILTER_BITS_PER_TOKEN` bits per distinct identifier
/// (a power of two, within `TOKEN_FILTER_MIN_BITS..=TOKEN_FILTER_MAX_BITS`):
/// a needle it does not hold occurs in none of them as an identifier
/// (exact), one it holds may.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TokenFilter(Box<[u64]>);

const TOKEN_FILTER_BITS_PER_TOKEN: usize = 16;
const TOKEN_FILTER_MIN_BITS: usize = 1 << 12;
const TOKEN_FILTER_MAX_BITS: usize = 1 << 21;
const TOKEN_FILTER_PROBES: u64 = 4;

impl std::fmt::Debug for TokenFilter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = self.0.iter().map(|word| word.count_ones()).sum::<u32>();
        write!(
            formatter,
            "TokenFilter({set} of {} bits)",
            self.0.len() * 64
        )
    }
}

impl Default for TokenFilter {
    fn default() -> Self {
        Self::of(&[])
    }
}

impl TokenFilter {
    /// The filter of `hashes`, sized by their count.
    fn of(hashes: &[u64]) -> Self {
        let bits = hashes
            .len()
            .saturating_mul(TOKEN_FILTER_BITS_PER_TOKEN)
            .next_power_of_two()
            .clamp(TOKEN_FILTER_MIN_BITS, TOKEN_FILTER_MAX_BITS);
        let mut filter = Self(vec![0; bits / 64].into_boxed_slice());
        for hash in hashes {
            for bit in filter.probes(*hash) {
                filter.0[bit / 64] |= 1 << (bit % 64);
            }
        }
        filter
    }

    pub(crate) fn bytes(&self) -> usize {
        self.0.len() * 8
    }

    fn probes(&self, hash: u64) -> impl Iterator<Item = usize> + use<> {
        let mask = (self.0.len() * 64 - 1) as u64;
        let (first, step) = (hash & 0xffff_ffff, (hash >> 32) | 1);
        (0..TOKEN_FILTER_PROBES)
            .map(move |probe| (first.wrapping_add(probe.wrapping_mul(step)) & mask) as usize)
    }

    /// Whether some needle may be an identifier of the texts.
    pub(crate) fn may_hold(&self, needles: &[String]) -> bool {
        needles.iter().any(|needle| {
            self.probes(token_hash(needle.as_bytes()))
                .all(|bit| self.0[bit / 64] & (1 << (bit % 64)) != 0)
        })
    }
}

/// What one references proof of a member found (Task 8d): the whole
/// identifiers of its raw text and of every include it reached, and what
/// proved them: the member's stamp, taken before its read, then every
/// include read (stamp before the read) and every resolver observation of
/// its walk, as the request baseline holds them. A later request may use
/// it only after every stamp proves its path unchanged
/// (`ProjectReadStamp::proves_unchanged`; a missing path still missing),
/// under the same binding and root route.
#[derive(Debug)]
pub(crate) struct LexicalSummary {
    pub(crate) tokens: TokenFilter,
    pub(crate) root: super::PathStamp,
    pub(crate) dependencies: Vec<(std::path::PathBuf, Option<super::PathStamp>)>,
    /// The legacy route the member's walk loaded its root with; `None` for
    /// a member without includes, which no walk follows.
    pub(crate) route: Option<Option<pascal_core::LegacyRoute>>,
}

impl LexicalSummary {
    pub(crate) fn bytes(&self) -> usize {
        let entry = std::mem::size_of::<(std::path::PathBuf, Option<super::PathStamp>)>();
        std::mem::size_of::<Self>()
            + self.tokens.bytes()
            + self
                .dependencies
                .iter()
                .map(|(path, _)| entry + path.as_os_str().len())
                .sum::<usize>()
            + self
                .route
                .iter()
                .flatten()
                .map(|route| {
                    route.source_path.as_os_str().len() + route.sibling_directory.as_os_str().len()
                })
                .sum::<usize>()
    }
}

/// What every summary of a context was proven under: its project context
/// (summaries of a context replaced under the same key are not used), the
/// workspace roots and the file size limit the walk's resolver read with.
#[derive(Debug, Clone)]
pub(crate) struct SummaryBinding {
    pub(crate) context: std::sync::Arc<pascal_project::ProjectContext>,
    pub(crate) roots: Vec<std::path::PathBuf>,
    pub(crate) max_file_bytes: usize,
}

impl SummaryBinding {
    fn matches(&self, other: &Self) -> bool {
        (std::sync::Arc::ptr_eq(&self.context, &other.context) || self.context == other.context)
            && self.roots == other.roots
            && self.max_file_bytes == other.max_file_bytes
    }
}

/// At most this many summaries, and bytes, per context.
pub(crate) const MAX_LEXICAL_SUMMARIES: usize = 8192;
pub(crate) const MAX_LEXICAL_SUMMARY_BYTES: usize = 64 * 1024 * 1024;

/// The workspace symbols of a source the symbol read parsed (Task 8d):
/// valid for bytes with the same content hash and decoded text hash,
/// parsed under the same conditional context without an expansion.
#[derive(Debug)]
pub(crate) struct ListedSymbols {
    pub(crate) content_hash: u64,
    pub(crate) parsed_text_hash: u64,
    pub(crate) conditional_context: pascal_project::ConditionalContext,
    pub(crate) list: crate::navigation::WorkspaceSymbolList,
}

impl ListedSymbols {
    pub(crate) fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.list.bytes()
            + std::mem::size_of_val(&self.conditional_context)
            + 64 * self.conditional_context.defines.len()
    }
}

/// One context's summaries and symbol lists, kept with its index state and
/// charged to the index share with it (`StoredIndex::total_charge`).
#[derive(Debug, Default)]
pub(crate) struct LexicalSummaries {
    binding: Option<SummaryBinding>,
    members: std::collections::HashMap<std::path::PathBuf, std::sync::Arc<LexicalSummary>>,
    symbols: std::collections::HashMap<std::path::PathBuf, std::sync::Arc<ListedSymbols>>,
    bytes: usize,
}

pub(crate) type SummaryMap =
    std::collections::HashMap<std::path::PathBuf, std::sync::Arc<LexicalSummary>>;

impl LexicalSummaries {
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.members.len()
    }

    pub(crate) fn symbols(&self, path: &std::path::Path) -> Option<std::sync::Arc<ListedSymbols>> {
        self.symbols.get(path).cloned()
    }

    /// Keeps `lists`, whose `paid` bytes the caller charged, within the
    /// bounds; returns the bytes to release.
    pub(crate) fn keep_symbols(
        &mut self,
        lists: Vec<(std::path::PathBuf, std::sync::Arc<ListedSymbols>)>,
        paid: usize,
    ) -> usize {
        let mut released = 0;
        let mut kept = 0;
        for (path, list) in lists {
            let bytes = list.bytes();
            if let Some(old) = self.symbols.remove(&path) {
                let old = old.bytes();
                self.bytes -= old;
                released += old;
            }
            if self.members.len() + self.symbols.len() >= MAX_LEXICAL_SUMMARIES
                || self.bytes + bytes > MAX_LEXICAL_SUMMARY_BYTES
            {
                continue;
            }
            self.bytes += bytes;
            kept += bytes;
            self.symbols.insert(path, list);
        }
        released + paid.saturating_sub(kept)
    }

    /// The summaries proven under `binding`.
    pub(crate) fn under(&self, binding: &SummaryBinding) -> Option<SummaryMap> {
        let held = self.binding.as_ref()?;
        (held.matches(binding) && !self.members.is_empty()).then(|| self.members.clone())
    }

    /// Keeps `summaries`, whose `paid` bytes the caller charged, under
    /// `binding` (dropping summaries of another binding), within the
    /// bounds; returns the bytes to release: replaced, dropped and refused
    /// summaries.
    pub(crate) fn keep(
        &mut self,
        binding: SummaryBinding,
        summaries: Vec<(std::path::PathBuf, std::sync::Arc<LexicalSummary>)>,
        paid: usize,
    ) -> usize {
        let mut released = 0;
        if !self
            .binding
            .as_ref()
            .is_some_and(|held| held.matches(&binding))
        {
            let held = self
                .members
                .values()
                .map(|summary| summary.bytes())
                .sum::<usize>();
            released += held;
            self.members.clear();
            self.bytes -= held;
            self.binding = Some(binding);
        }
        let mut kept = 0;
        for (path, summary) in summaries {
            let bytes = summary.bytes();
            if let Some(old) = self.members.remove(&path) {
                let old = old.bytes();
                self.bytes -= old;
                released += old;
            }
            if self.members.len() + self.symbols.len() >= MAX_LEXICAL_SUMMARIES
                || self.bytes + bytes > MAX_LEXICAL_SUMMARY_BYTES
            {
                continue;
            }
            self.bytes += bytes;
            kept += bytes;
            self.members.insert(path, summary);
        }
        released + paid.saturating_sub(kept)
    }

    /// Drops the summaries of paths `member` rejects; returns their bytes.
    pub(crate) fn retain(&mut self, member: impl Fn(&std::path::Path) -> bool) -> usize {
        let before = self.bytes;
        self.members.retain(|path, summary| {
            let keep = member(path);
            if !keep {
                self.bytes -= summary.bytes();
            }
            keep
        });
        self.symbols.retain(|path, list| {
            let keep = member(path);
            if !keep {
                self.bytes -= list.bytes();
            }
            keep
        });
        before - self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A needle missing from the filter is no identifier of the text; one
    /// present (case-insensitively) is always held.
    #[test]
    fn a_token_filter_never_misses_an_identifier() {
        let text = b"unit Guarded; {$I Defs.inc} const GetSharedValues = 1; x_9 := TDatabaseInfo;";
        let mut hashes = TokenHashes::default();
        hashes.insert_text(text);
        let filter = hashes.clone().into_filter();
        for run in identifier_runs(text) {
            let token = String::from_utf8(text[run].to_vec()).unwrap();
            assert!(filter.may_hold(&[token.to_ascii_uppercase()]), "{token}");
        }
        assert!(filter.may_hold(&["tdatabaseinfo".to_owned()]));
        assert!(!filter.may_hold(&["sharedvalue".to_owned()]));
        assert!(!filter.may_hold(&["databaseinfo".to_owned()]));
        let mut other = TokenHashes::default();
        other.insert_text(b"SharedValue");
        hashes.union(&other);
        assert!(hashes.into_filter().may_hold(&["SHAREDVALUE".to_owned()]));
    }

    /// The filter grows with the identifiers it holds, so a large member
    /// keeps a low false-positive rate; its negatives stay exact.
    #[test]
    fn a_token_filter_is_sized_by_its_identifiers() {
        let small = {
            let mut hashes = TokenHashes::default();
            hashes.insert_text(b"unit A;");
            hashes.into_filter()
        };
        assert_eq!(small.bytes(), TOKEN_FILTER_MIN_BITS / 8);
        let text = (0..25_000)
            .map(|index| format!("Ident{index} "))
            .collect::<String>();
        let mut hashes = TokenHashes::default();
        hashes.insert_text(text.as_bytes());
        let large = hashes.into_filter();
        assert_eq!(large.bytes(), (1 << 19) / 8);
        for index in (0..25_000).step_by(97) {
            assert!(large.may_hold(&[format!("ident{index}")]));
        }
        let false_positives = (0..10_000)
            .filter(|index| large.may_hold(&[format!("absent{index}")]))
            .count();
        assert!(false_positives < 100, "{false_positives} of 10000");
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
