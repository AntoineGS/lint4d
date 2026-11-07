//! The identifiers a provider's interface text could declare (l4d-onnm,
//! D46). Pascal has no macros: a unit can declare a name only by spelling it
//! as an identifier token in its interface or in an include expanded there.
//! A provider whose interface is unknown therefore still cannot declare a
//! name that never occurs as such a token, so it does not block the proof
//! that a name used in a conditional expression is not shadowed.
//!
//! The scan tokenizes as the directive lexer does (`conditional.rs`
//! `lex_directives_inner`): `{ }`, `(* *)` and `//` comments, `'` strings
//! and same-line `"` assembler strings. Comment and string contents are not
//! identifiers. It over-approximates everywhere else: every conditional
//! branch is read, every include directive must resolve, and an identifier
//! run is split at any byte that is not ASCII `[A-Za-z0-9_]`.
use super::{CompileTimeBudget, IncompleteReason};
use crate::conditional::{DirectiveKind, directive_kind};
use std::ops::Range;

/// Bytes scanned between budget charges and cancellation checks.
const CHUNK: usize = 4096;

#[cfg(test)]
thread_local! {
    static EXAMINED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Count bytes a delimiter search examines, for the linearity tests.
fn examined(_bytes: usize) {
    #[cfg(test)]
    EXAMINED.with(|count| count.set(count.get() + _bytes));
}

/// Identifiers occurring in a provider's interface text and every include
/// it could expand there, ASCII-lowercased and sorted.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InterfaceNames {
    names: Vec<Box<str>>,
}

impl InterfaceNames {
    pub(super) fn new(mut names: Vec<Box<str>>) -> Self {
        names.sort_unstable();
        names.dedup();
        Self { names }
    }

    /// Whether the interface could declare the unqualified `name`. A name
    /// that is not a plain ASCII identifier is never proven absent.
    pub fn may_declare(&self, name: &str) -> bool {
        let name = name.strip_prefix('&').unwrap_or(name);
        if name.is_empty() || !name.bytes().all(identifier_byte) {
            return true;
        }
        let lower = name.to_ascii_lowercase();
        self.names
            .binary_search_by(|probe| (**probe).cmp(lower.as_str()))
            .is_ok()
    }

    /// Bytes the set retains.
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            + self
                .names
                .iter()
                .map(|n| size_of::<Box<str>>() + n.len())
                .sum::<usize>()
    }

    pub(super) fn len(&self) -> usize {
        self.names.len()
    }
}

/// What scanning one source found.
pub(super) struct Scan {
    pub names: Vec<Box<str>>,
    /// Include directives (directive range, requested name), in order.
    pub includes: Vec<(Range<usize>, String)>,
    /// Whether the scan proves anything: false after an unterminated
    /// comment or string, an include without a static name, or a non-ASCII
    /// letter that case-folds to an ASCII one.
    pub proof: bool,
    /// Bytes read.
    pub end: usize,
}

fn identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// A non-ASCII character an identifier could hold that Delphi's
/// case-insensitive comparison might equate with an ASCII letter (U+017F
/// uppercases to `S`, U+212A lowercases to `k`).
pub(super) fn folds_to_ascii(ch: char) -> bool {
    ch.is_alphanumeric()
        && (ch.to_uppercase().any(|c| c.is_ascii_alphanumeric())
            || ch.to_lowercase().any(|c| c.is_ascii_alphanumeric()))
}

struct Scanner<'t, 'b, 'c> {
    text: &'t str,
    budget: &'b mut CompileTimeBudget<'c>,
    /// Bytes before this offset were already credited to the request.
    floor: usize,
    charged: usize,
    names: Vec<Box<str>>,
    includes: Vec<(Range<usize>, String)>,
    proof: bool,
}

impl Scanner<'_, '_, '_> {
    /// Charge every byte up to `end` before it is read: scanning is real
    /// work, and the bytes no earlier scan of the request credited grant
    /// their per-byte allowance (D10).
    fn reach(&mut self, end: usize) -> Result<(), IncompleteReason> {
        let end = end.min(self.text.len());
        while self.charged < end {
            let next = (self.charged + CHUNK).min(self.text.len());
            let read = next - self.charged;
            let granted = next.saturating_sub(self.floor.max(self.charged));
            self.budget.charge_scanned(granted, 1, read)?;
            self.charged = next;
        }
        Ok(())
    }

    fn name(&mut self, run: &str) -> Result<(), IncompleteReason> {
        self.budget
            .charge(1, run.len(), size_of::<Box<str>>() + run.len())?;
        self.names.push(run.to_ascii_lowercase().into_boxed_str());
        Ok(())
    }

    /// An identifier run at `start..end`. A run that follows a number or a
    /// `$`/`#` literal prefix could hide an identifier after the literal's
    /// digits, so each of its suffixes starting at a letter counts too.
    fn run(&mut self, start: usize, end: usize) -> Result<(), IncompleteReason> {
        let bytes = self.text.as_bytes();
        let literal = bytes[start].is_ascii_digit()
            || start
                .checked_sub(1)
                .is_some_and(|p| matches!(bytes[p], b'$' | b'#'));
        if !bytes[start].is_ascii_digit() {
            self.name(&self.text[start..end])?;
        }
        if literal {
            let starts: Vec<usize> = (start + 1..end)
                .filter(|&at| bytes[at].is_ascii_alphabetic() || bytes[at] == b'_')
                .collect();
            for at in starts {
                self.name(&self.text[at..end])?;
            }
        }
        Ok(())
    }

    fn directive(&mut self, range: Range<usize>, body: &str, depth: &mut Option<usize>) {
        match directive_kind(body) {
            DirectiveKind::Include => match crate::resolver::include_name(body) {
                Some(name) => self.includes.push((range, name)),
                None => self.proof = false,
            },
            DirectiveKind::ConditionalStart => *depth = depth.and_then(|d| d.checked_add(1)),
            DirectiveKind::ConditionalEnd => *depth = depth.and_then(|d| d.checked_sub(1)),
            _ => {}
        }
    }
}

/// Scan `text` for identifiers. A root source stops at an `implementation`
/// keyword outside every conditional, when no include came before it (an
/// include may open a conditional the unit closes): whichever
/// `implementation` the compiler takes is not later than that one, and
/// everything before it is read. An include is read whole.
pub(super) fn scan(
    text: &str,
    root: bool,
    floor: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<Scan, IncompleteReason> {
    let mut scanner = Scanner {
        text,
        budget,
        floor,
        charged: 0,
        names: Vec::new(),
        includes: Vec::new(),
        proof: true,
    };
    let bytes = text.as_bytes();
    // `None`: unbalanced nesting, so no `implementation` is unconditional.
    let mut depth = Some(0usize);
    // The last byte of the previous token, outside comments: `.` and `&`
    // make the next word an identifier, not a keyword.
    let mut previous = 0u8;
    let mut index = 0;
    let end = loop {
        if index >= bytes.len() {
            break bytes.len();
        }
        scanner.reach(index + 1)?;
        match bytes[index] {
            b'\'' => {
                let mut at = index + 1;
                let close = loop {
                    scanner.reach(at + 2)?;
                    match bytes.get(at) {
                        None | Some(b'\n') => break None,
                        Some(b'\'') if bytes.get(at + 1) == Some(&b'\'') => at += 2,
                        Some(b'\'') => break Some(at + 1),
                        Some(_) => at += 1,
                    }
                };
                previous = b'\'';
                match close {
                    Some(next) => index = next,
                    None => {
                        scanner.proof = false;
                        break at;
                    }
                }
            }
            b'"' => {
                // As `skip_assembler_string`: to the closing quote on the
                // same line, else past the quote alone. A later quote on the
                // line would have closed it, so each byte is read once.
                let mut at = index + 1;
                let close = loop {
                    scanner.reach(at + 1)?;
                    match bytes.get(at) {
                        None | Some(b'\n') => break None,
                        Some(b'"') => break Some(at),
                        Some(_) => at += 1,
                    }
                };
                examined(at - index);
                previous = b'"';
                index = close.map_or(index + 1, |close| close + 1);
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                let line_end = text[index..].find('\n').map_or(bytes.len(), |n| index + n);
                scanner.reach(line_end)?;
                index = line_end;
            }
            b'{' => {
                let close = text[index + 1..].find('}').map(|n| index + 1 + n);
                let Some(close) = close else {
                    scanner.reach(bytes.len())?;
                    scanner.proof = false;
                    break bytes.len();
                };
                scanner.reach(close + 1)?;
                if bytes.get(index + 1) == Some(&b'$') {
                    scanner.directive(index..close + 1, &text[index + 2..close], &mut depth);
                }
                index = close + 1;
            }
            b'(' if bytes.get(index + 1) == Some(&b'*') => {
                let close = text[index + 2..].find("*)").map(|n| index + 2 + n);
                let Some(close) = close else {
                    scanner.reach(bytes.len())?;
                    scanner.proof = false;
                    break bytes.len();
                };
                scanner.reach(close + 2)?;
                if bytes.get(index + 2) == Some(&b'$') {
                    scanner.directive(index..close + 2, &text[index + 3..close], &mut depth);
                }
                index = close + 2;
            }
            byte if identifier_byte(byte) => {
                let mut stop = index;
                while stop < bytes.len() && identifier_byte(bytes[stop]) {
                    stop += 1;
                    scanner.reach(stop + 1)?;
                }
                if root
                    && depth == Some(0)
                    && scanner.includes.is_empty()
                    && !matches!(previous, b'&' | b'.')
                    && text[index..stop].eq_ignore_ascii_case("implementation")
                {
                    break stop;
                }
                scanner.run(index, stop)?;
                previous = bytes[stop - 1];
                index = stop;
            }
            byte if byte >= 0x80 => {
                let ch = text[index..].chars().next().expect("char boundary");
                if folds_to_ascii(ch) {
                    scanner.proof = false;
                }
                previous = byte;
                index += ch.len_utf8();
            }
            byte => {
                if !byte.is_ascii_whitespace() {
                    previous = byte;
                }
                index += 1;
            }
        }
    };
    Ok(Scan {
        names: scanner.names,
        includes: scanner.includes,
        proof: scanner.proof,
        end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoCancellation;
    use crate::compile_time::CompileTimeLimits;

    fn names(text: &str, root: bool) -> (InterfaceNames, Scan) {
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let scan = scan(text, root, 0, &mut budget).unwrap();
        (InterfaceNames::new(scan.names.clone()), scan)
    }

    #[test]
    fn comments_strings_and_directives_hold_no_identifiers() {
        let (n, s) = names(
            "const A = 'Hidden'; { Hidden } (* Hidden *) // Hidden\n {$IF Hidden > 1} B",
            true,
        );
        assert!(s.proof);
        assert!(n.may_declare("a") && n.may_declare("B") && n.may_declare("&b"));
        assert!(!n.may_declare("Hidden"));
    }

    #[test]
    fn literal_prefixes_count_every_suffix() {
        let (n, _) = names("x = $1Fname; y = 12Other; #13Third", true);
        assert!(n.may_declare("name") && n.may_declare("Fname"));
        assert!(n.may_declare("Other") && n.may_declare("Third"));
    }

    #[test]
    fn a_root_stops_at_an_unconditional_implementation_only() {
        let (n, s) = names("A implementation B", true);
        assert!(n.may_declare("A") && !n.may_declare("B"));
        assert_eq!(s.end, "A implementation".len());
        let (n, _) = names(
            "{$IFDEF X} implementation {$ENDIF} B implementation C",
            true,
        );
        assert!(n.may_declare("B") && !n.may_declare("C"));
        let (n, _) = names("&implementation B", true);
        assert!(n.may_declare("B"));
        let (n, _) = names("A implementation B", false);
        assert!(n.may_declare("B"));
    }

    /// Fix round 1 (I3): a line of double quotes is scanned once, charged
    /// as it is read.
    #[test]
    fn a_long_double_quote_line_is_scanned_linearly() {
        let text = format!("A {} B", "\"".repeat(20_000));
        let before = EXAMINED.with(std::cell::Cell::get);
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let scan = scan(&text, true, 0, &mut budget).unwrap();
        let examined = EXAMINED.with(std::cell::Cell::get) - before;
        assert!(examined <= 2 * text.len(), "{examined} bytes examined");
        assert!(budget.snapshot().byte_work >= text.len());
        let names = InterfaceNames::new(scan.names);
        assert!(names.may_declare("A") && names.may_declare("B"));
    }

    #[test]
    fn includes_are_listed_and_switches_are_not() {
        let (_, s) = names("{$I+} {$I a.inc} (*$INCLUDE 'b c.inc'*) {$I-}", true);
        let includes: Vec<_> = s.includes.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(includes, ["a.inc", "b c.inc"]);
        assert!(s.proof);
    }
}
