//! Prefix proofs use the session's directive interpreter and include authority.
use super::*;
use crate::{CancellationToken, LoadedSource, ResolverError};
use pascal_project::ConditionalContext;

pub fn verify_unit_header(
    root: &LoadedSource,
    context: &ConditionalContext,
    resolver: &mut dyn CompileTimeResolver,
    limits: CompileTimeLimits,
    cancel: &dyn CancellationToken,
) -> Result<Fact<VerifiedUnitHeader>, ResolverError> {
    let mut budget = CompileTimeBudget::new(limits, cancel);
    verify_unit_header_in_session(root, context, resolver, &mut budget, cancel)
}

pub(crate) fn verify_unit_header_in_session(
    root: &LoadedSource,
    context: &ConditionalContext,
    resolver: &mut dyn CompileTimeResolver,
    budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
) -> Result<Fact<VerifiedUnitHeader>, ResolverError> {
    super::session::header_in_session(root, context, resolver, budget, cancel)
}

#[derive(Default)]
pub(super) struct HeaderProof {
    stage: u8, // unit, component, dot-or-semicolon, terminal
    mode: u8,  // normal, brace, line, star, star-end, slash, paren
    header: Option<VerifiedUnitHeader>,
    rejected: bool,
    uncertain: bool,
    pending_active: bool,
    pending_unknown: bool,
    name_invoice: usize,
    parts_invoice: usize,
}

impl HeaderProof {
    pub(super) fn done(&self) -> bool {
        self.stage == 3 || self.rejected
    }

    pub(super) fn feed(
        &mut self,
        fragment: SourceFragment<'_>,
        activity: crate::conditional::Truth,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), IncompleteReason> {
        use crate::conditional::Truth;
        let bytes = fragment.text.as_bytes();
        let mut i =
            if self.mode == 0 && fragment.physical_start == 0 && bytes.starts_with(b"\xef\xbb\xbf")
            {
                budget.charge(1, 3, 0)?;
                3
            } else {
                0
            };
        while i < bytes.len() && !self.done() {
            budget.charge(1, 1, 0)?;
            let b = bytes[i];
            match self.mode {
                1 => {
                    if b == b'}' {
                        self.mode = 0;
                    }
                    i += 1;
                    continue;
                }
                2 => {
                    if matches!(b, b'\r' | b'\n') {
                        self.mode = 0;
                    }
                    i += 1;
                    continue;
                }
                3 => {
                    if b == b'*' {
                        self.mode = 4;
                    }
                    i += 1;
                    continue;
                }
                4 => {
                    self.mode = if b == b')' {
                        0
                    } else if b == b'*' {
                        4
                    } else {
                        3
                    };
                    i += 1;
                    continue;
                }
                5 => {
                    self.mode = 0;
                    if b == b'/' {
                        self.mode = 2;
                        i += 1;
                        continue;
                    }
                    self.rejected = self.pending_active;
                    self.uncertain |= self.pending_unknown;
                }
                6 => {
                    self.mode = 0;
                    if b == b'*' {
                        self.mode = 3;
                        i += 1;
                        continue;
                    }
                    self.rejected = self.pending_active;
                    self.uncertain |= self.pending_unknown;
                }
                _ => {}
            }
            if self.done() {
                break;
            }
            if b.is_ascii_whitespace() {
                i += 1;
                continue;
            }
            match b {
                b'{' => {
                    self.mode = 1;
                    i += 1;
                    continue;
                }
                b'/' => {
                    self.mode = 5;
                    self.pending_active = activity != Truth::False;
                    self.pending_unknown = activity == Truth::Unknown;
                    i += 1;
                    continue;
                }
                b'(' => {
                    self.mode = 6;
                    self.pending_active = activity != Truth::False;
                    self.pending_unknown = activity == Truth::Unknown;
                    i += 1;
                    continue;
                }
                _ => {}
            }
            if activity == Truth::False {
                i += 1;
                continue;
            }
            if activity == Truth::Unknown {
                self.rejected = true;
                self.uncertain = true;
                break;
            }
            let start = i;
            if b.is_ascii_alphabetic() || b == b'_' {
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    budget.charge(1, 1, 0)?;
                    i += 1;
                }
                let word = &fragment.text[start..i];
                if self.stage == 0 && word.eq_ignore_ascii_case("unit") {
                    let payload = fragment.source.as_str().len();
                    budget.charge(1, payload, payload)?;
                    self.header = Some(VerifiedUnitHeader {
                        name: String::new(),
                        declaration: SourceAnchor {
                            source: fragment.source.clone(),
                            occurrence: fragment.occurrence,
                            range: fragment.physical_start + start..fragment.physical_start + i,
                        },
                        name_parts: Vec::new(),
                    });
                    self.stage = 1;
                } else if self.stage == 1 {
                    let h = self.header.as_mut().expect("unit token");
                    let length = h
                        .name
                        .len()
                        .checked_add(word.len())
                        .and_then(|n| n.checked_add(usize::from(!h.name.is_empty())))
                        .ok_or(IncompleteReason::MalformedSource)?;
                    let slots = h
                        .name_parts
                        .len()
                        .checked_add(1)
                        .and_then(|n| n.checked_mul(std::mem::size_of::<SourceAnchor>()))
                        .ok_or(IncompleteReason::MalformedSource)?;
                    let payload = length
                        .checked_add(slots)
                        .and_then(|n| n.checked_add(fragment.source.as_str().len()))
                        .ok_or(IncompleteReason::MalformedSource)?;
                    let copies = self
                        .name_invoice
                        .checked_add(self.parts_invoice)
                        .and_then(|n| n.checked_add(payload))
                        .ok_or(IncompleteReason::MalformedSource)?;
                    budget.charge(1, copies, payload)?;
                    h.name.reserve_exact(length - h.name.len());
                    h.name_parts.reserve_exact(1);
                    budget.release(self.name_invoice + self.parts_invoice);
                    self.name_invoice = length;
                    self.parts_invoice = slots;
                    if !h.name.is_empty() {
                        h.name.push('.');
                    }
                    h.name.push_str(word);
                    h.name_parts.push(SourceAnchor {
                        source: fragment.source.clone(),
                        occurrence: fragment.occurrence,
                        range: fragment.physical_start + start..fragment.physical_start + i,
                    });
                    self.stage = 2;
                } else {
                    self.rejected = true;
                }
            } else if b == b'.' && self.stage == 2 {
                budget.charge(1, 1, 0)?;
                self.stage = 1;
                i += 1;
            } else if b == b';' && self.stage == 2 {
                let h = self.header.as_mut().expect("unit token");
                if h.declaration.source != *fragment.source
                    || h.declaration.occurrence != fragment.occurrence
                {
                    self.rejected = true;
                } else {
                    h.declaration.range.end = fragment.physical_start + i + 1;
                    self.stage = 3;
                }
            } else {
                self.rejected = true;
            }
        }
        Ok(())
    }

    pub(super) fn finish(self) -> Fact<VerifiedUnitHeader> {
        if self.stage == 3 && !self.rejected {
            Fact::Known(self.header.expect("complete header"))
        } else {
            Fact::Unknown(if self.uncertain {
                UnknownReason::UnknownActivity
            } else {
                UnknownReason::UnresolvedName
            })
        }
    }
}

/// No conditional evaluator is needed if the shared directive scanner reaches
/// the first Pascal semicolon without finding a directive. Directive-bearing
/// prefixes fall back to the source-order session, using the same root budget.
pub(crate) fn plain_header_in_session(
    root: &LoadedSource,
    text: &str,
    budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
) -> Result<Option<AdmittedOwned<Fact<VerifiedUnitHeader>>>, ResolverError> {
    let mut output = budget.admit_compilation(0, |budget| {
        let mut scanner = crate::conditional::DirectiveScanner::prefix(budget)
            .map_err(|r| super::session::resolver_error(r, budget))?;
        let next = scanner
            .next(text, 0, &root.id, OccurrenceId(0), budget, cancel)
            .map_err(|r| super::session::resolver_error(r, budget))?;
        if next.directive.is_some() {
            return Ok(None);
        }
        let mut proof = budget
            .admit_owned(0, 0, 0, HeaderProof::default)
            .map_err(|r| super::session::resolver_error(r, budget))?;
        proof
            .with_budget_mut(budget, |proof, budget| {
                proof.feed(
                    SourceFragment {
                        source: &root.id,
                        occurrence: OccurrenceId(0),
                        physical_start: 0,
                        text: &text[..next.text_end],
                    },
                    crate::conditional::Truth::True,
                    budget,
                )
            })
            .map_err(|r| super::session::resolver_error(r, budget))?
            .map_err(|r| super::session::resolver_error(r, budget))?;
        let header = proof
            .map_value(budget, HeaderProof::finish)
            .map_err(|r| super::session::resolver_error(r, budget))?;
        Ok(Some(header))
    })?;
    Ok(output.value_mut().take())
}
