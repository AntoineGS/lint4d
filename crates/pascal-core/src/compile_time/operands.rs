//! Bounded access syntax; selectors bind static descriptors, never memory.
use super::bindings::{self, Binding, TypeExpr};
use super::declarations::limit;
use super::*;
use pascal_project::LayoutSettings;
use std::{mem::size_of, ops::Range};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SizeOfOperand {
    text: String,
    root: Range<usize>,
    selectors: Vec<Selector>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Selector {
    Field(Range<usize>),
    Index(Vec<Range<usize>>),
    Dereference,
}

pub fn parse_sizeof_operand(text: &str, budget: &mut CompileTimeBudget<'_>) -> Fact<SizeOfOperand> {
    let before = budget.snapshot().retained_bytes;
    let result = (|| {
        limit(text.len(), 4096, "max_expression_bytes")?;
        // Reserve all span/vector/string payload before constructing syntax.
        let reserve = text.len() * (size_of::<Selector>() + size_of::<Range<usize>>() + 1);
        budget.charge(text.len() + 1, text.len(), reserve)?;
        let mut p = AccessParser {
            text,
            pos: 0,
            tokens: 0,
        };
        let Some(root) = p.name() else {
            return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
        };
        let mut selectors = Vec::new();
        loop {
            p.space();
            match p.byte() {
                None => break,
                Some(b'.') => {
                    p.pos += 1;
                    p.tokens += 1;
                    let Some(name) = p.name() else {
                        return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
                    };
                    selectors.push(Selector::Field(name));
                }
                Some(b'^') => {
                    p.pos += 1;
                    p.tokens += 1;
                    selectors.push(Selector::Dereference);
                }
                Some(b'[') => {
                    p.pos += 1;
                    p.tokens += 1;
                    let mut start = p.pos;
                    let mut depth = 0;
                    let mut quoted = false;
                    let mut indices = Vec::new();
                    loop {
                        let Some(ch) = p.byte() else {
                            return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
                        };
                        if ch == b'\'' {
                            quoted = !quoted;
                        }
                        if !quoted {
                            if ch == b'(' {
                                depth += 1;
                                limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
                            }
                            if ch == b')' {
                                if depth == 0 {
                                    return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
                                }
                                depth -= 1;
                            }
                            if (ch == b',' || ch == b']') && depth == 0 {
                                if p.text[start..p.pos].trim().is_empty() {
                                    return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
                                }
                                p.tokens += crate::conditional::expression_token_count(
                                    &p.text[start..p.pos],
                                    budget,
                                )?;
                                indices.push(start..p.pos);
                                p.pos += 1;
                                if ch == b']' {
                                    break;
                                }
                                start = p.pos;
                                continue;
                            }
                            if ch == b'[' || ch == b']' {
                                return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
                            }
                        }
                        p.pos += 1;
                    }
                    selectors.push(Selector::Index(indices));
                }
                _ => return Ok(Fact::Unknown(UnknownReason::InvalidOperand)),
            }
            limit(
                p.tokens,
                crate::conditional::MAX_EXPRESSION_TOKENS,
                "max_expression_tokens",
            )?;
        }
        let retained = size_of::<SizeOfOperand>()
            + text.len()
            + selectors.len() * size_of::<Selector>()
            + selectors
                .iter()
                .map(|s| match s {
                    Selector::Index(v) => v.len() * size_of::<Range<usize>>(),
                    _ => 0,
                })
                .sum::<usize>();
        budget.charge(1, text.len(), retained)?;
        budget.release(reserve);
        Ok(Fact::Known(SizeOfOperand {
            text: text.to_owned(),
            root,
            selectors,
        }))
    })();
    if !matches!(result, Ok(Fact::Known(_))) {
        budget.release(budget.snapshot().retained_bytes.saturating_sub(before));
    }
    result.unwrap_or_else(Fact::Incomplete)
}

struct AccessParser<'a> {
    text: &'a str,
    pos: usize,
    tokens: usize,
}
impl AccessParser<'_> {
    fn byte(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }
    fn space(&mut self) {
        while self.byte().is_some_and(|b| b.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }
    fn name(&mut self) -> Option<Range<usize>> {
        self.space();
        let start = self.pos;
        if self.byte() == Some(b'&') {
            self.pos += 1;
        }
        let mut chars = self.text[self.pos..].chars();
        let first = chars.next()?;
        if first != '_' && !first.is_alphabetic() {
            return None;
        }
        self.pos += first.len_utf8();
        for ch in chars {
            if ch != '_' && !ch.is_alphanumeric() {
                break;
            }
            self.pos += ch.len_utf8();
        }
        self.tokens += 1;
        Some(start..self.pos)
    }
}

struct BoundType<'a> {
    carrier: Carrier<'a>,
    site: &'a BindingSite,
    settings: &'a LayoutSettings,
    owner: &'a DefinitionKey,
}

#[derive(Clone, Copy)]
enum Carrier<'a> {
    Captured(&'a TypeExpr),
    Intrinsic(BuiltinType, &'a str),
}

impl CompileTimeState {
    pub fn layout_operand(
        &self,
        site: BindingSite,
        operand: &SizeOfOperand,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Fact<KnownLayout> {
        let before = budget.snapshot().retained_bytes;
        let result = (|| {
            budget.charge(1, 0, 0)?;
            if let Some(reason) = &self.failure {
                return Ok(Fact::Incomplete(reason.clone()));
            }
            if self.unknown {
                return Ok(Fact::Unknown(UnknownReason::UnknownActivity));
            }
            if site.scope.0 as usize >= self.scopes.len() || site.sequence.0 > self.sequence.0 {
                return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
            }
            let root = &operand.text[operand.root.clone()];
            budget.charge(1, root.len(), root.len())?;
            let root = root.to_ascii_lowercase();
            let mut first_selector = 0;
            let binding = if let Some(binding) = bindings::lookup(self, &site, &root, budget)? {
                binding
            } else {
                if operand.selectors.is_empty() {
                    return Ok(bindings::layout_name(self, site, &root, budget));
                }
                let Some(unit) = &self.unit_name else {
                    return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
                };
                budget.charge(1, unit.len(), 0)?;
                let mut components = unit.split('.');
                let Some(first) = components.next() else {
                    return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
                };
                budget.charge(1, first.len().min(root.len()), 0)?;
                if !bindings::same_name(first, &root) {
                    return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
                }
                for component in components {
                    let Some(Selector::Field(span)) = operand.selectors.get(first_selector) else {
                        return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
                    };
                    let text = &operand.text[span.clone()];
                    budget.charge(1, component.len().min(text.len()), 0)?;
                    if !bindings::same_name(component, text) {
                        return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
                    }
                    first_selector += 1;
                }
                let Some(Selector::Field(span)) = operand.selectors.get(first_selector) else {
                    return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
                };
                first_selector += 1;
                let global_site = BindingSite {
                    scope: ScopeId(0),
                    sequence: site.sequence,
                };
                let Some(binding) =
                    bindings::lookup(self, &global_site, &operand.text[span.clone()], budget)?
                else {
                    return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
                };
                binding
            };
            let Binding::Declaration(decl) = binding else {
                return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
            };
            if decl.is_routine {
                return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
            }
            let mut bound = BoundType {
                carrier: Carrier::Captured(&decl.ty),
                site: &decl.type_site,
                settings: &decl.settings,
                owner: &decl.key,
            };
            for selector in operand.selectors.iter().skip(first_selector) {
                bound = match bound.resolve(self, budget)? {
                    Fact::Known(bound) => bound,
                    Fact::Unknown(reason) => return Ok(Fact::Unknown(reason)),
                    Fact::Incomplete(reason) => return Ok(Fact::Incomplete(reason)),
                };
                let selected = match selector {
                    Selector::Field(span) => {
                        bound.record_field(self, &operand.text[span.clone()], budget)?
                    }
                    Selector::Dereference => bound.pointee(self, &site, budget)?,
                    Selector::Index(indices) => bound.array_element(
                        self,
                        &site,
                        indices.iter().map(|r| &operand.text[r.clone()]),
                        budget,
                    )?,
                };
                bound = match selected {
                    Fact::Known(bound) => bound,
                    Fact::Unknown(reason) => return Ok(Fact::Unknown(reason)),
                    Fact::Incomplete(reason) => return Ok(Fact::Incomplete(reason)),
                };
            }
            let storage = match bound.carrier {
                Carrier::Captured(ty) => bindings::storage_type(
                    self,
                    bound.site,
                    ty,
                    bound.settings,
                    &mut Vec::new(),
                    0,
                    budget,
                )?,
                Carrier::Intrinsic(ty, _) => builtin_layout(ty, &self.context, bound.settings),
            };
            Ok(match storage {
                Fact::Known(storage) => {
                    let bytes = size_of::<TypeIdentity>()
                        + match bound.carrier {
                            Carrier::Captured(_) => bound.owner.anchor.source.as_str().len(),
                            Carrier::Intrinsic(_, name) => name.len(),
                        };
                    budget.charge(1, bytes, bytes)?;
                    Fact::Known(KnownLayout {
                        storage,
                        identity: match bound.carrier {
                            Carrier::Captured(_) => TypeIdentity::Declared(bound.owner.clone()),
                            Carrier::Intrinsic(_, name) => TypeIdentity::Intrinsic(name.to_owned()),
                        },
                        dependencies: Vec::new(),
                    })
                }
                Fact::Unknown(reason) => Fact::Unknown(reason),
                Fact::Incomplete(reason) => Fact::Incomplete(reason),
            })
        })();
        // Only the returned identity survives the query. All traversal scratch is released.
        let keep = match &result {
            Ok(Fact::Known(v)) => match &v.identity {
                TypeIdentity::Declared(k) => {
                    size_of::<TypeIdentity>() + k.anchor.source.as_str().len()
                }
                TypeIdentity::Intrinsic(n) => size_of::<TypeIdentity>() + n.len(),
            },
            _ => 0,
        };
        budget.release(
            budget
                .snapshot()
                .retained_bytes
                .saturating_sub(before)
                .saturating_sub(keep),
        );
        result.unwrap_or_else(Fact::Incomplete)
    }
}

impl<'a> BoundType<'a> {
    fn record_field(
        self,
        state: &'a CompileTimeState,
        name: &str,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Fact<Self>, IncompleteReason> {
        let Carrier::Captured(TypeExpr::Record(record)) = self.carrier else {
            return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
        };
        let Some((ty, site, settings)) =
            super::records::operand_field(state, record, name, budget)?
        else {
            return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
        };
        Ok(Fact::Known(Self {
            carrier: Carrier::Captured(ty),
            site,
            settings,
            ..self
        }))
    }

    fn array_element<'b>(
        self,
        state: &CompileTimeState,
        query_site: &BindingSite,
        indices: impl Iterator<Item = &'b str>,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Fact<Self>, IncompleteReason> {
        let Carrier::Captured(TypeExpr::Array {
            dimensions,
            element,
            open,
            packed,
        }) = self.carrier
        else {
            return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
        };
        if *open
            || *packed
            || !super::arrays::operand_indices(
                state, self.site, query_site, dimensions, indices, budget,
            )?
        {
            return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
        }
        Ok(Fact::Known(Self {
            carrier: Carrier::Captured(element),
            ..self
        }))
    }

    fn pointee(
        self,
        state: &'a CompileTimeState,
        query_site: &BindingSite,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Fact<Self>, IncompleteReason> {
        let Carrier::Captured(TypeExpr::Pointer(name)) = self.carrier else {
            return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
        };
        let binding = bindings::lookup(state, self.site, name, budget)?;
        let binding = if binding.is_none() && bindings::intrinsic(name).is_none() {
            // Legal forward completion stays in the captured scope, not newer
            // routine-local shadows. Storage itself never requires completion.
            bindings::lookup(
                state,
                &BindingSite {
                    scope: self.site.scope,
                    sequence: query_site.sequence,
                },
                name,
                budget,
            )?
        } else {
            binding
        };
        let Some(binding) = binding else {
            return Ok(match bindings::intrinsic(name) {
                Some(ty) => Fact::Known(Self {
                    carrier: Carrier::Intrinsic(ty, name),
                    ..self
                }),
                None => Fact::Unknown(UnknownReason::UnresolvedName),
            });
        };
        let Some(decl) = binding.type_declaration() else {
            return Ok(Fact::Unknown(UnknownReason::InvalidOperand));
        };
        Ok(Fact::Known(Self {
            carrier: Carrier::Captured(&decl.ty),
            site: &decl.type_site,
            settings: &decl.settings,
            owner: &decl.key,
        }))
    }

    fn resolve(
        mut self,
        state: &'a CompileTimeState,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Fact<Self>, IncompleteReason> {
        let before = budget.snapshot().retained_bytes;
        let result = (|| {
            let mut visited: Vec<&DefinitionKey> = Vec::new();
            let mut depth = 0usize;
            loop {
                limit(depth, budget.limits().max_type_depth, "max_type_depth")?;
                budget.charge(1, 0, 0)?;
                let Carrier::Captured(TypeExpr::Named(name) | TypeExpr::StrongAlias(name)) =
                    self.carrier
                else {
                    return Ok(Fact::Known(self));
                };
                let Some(binding) = bindings::lookup(state, self.site, name, budget)? else {
                    return Ok(Fact::Known(self));
                };
                let Some(decl) = binding.type_declaration() else {
                    return Ok(Fact::Unknown(UnknownReason::UnresolvedName));
                };
                for key in &visited {
                    budget.charge(
                        1,
                        key.anchor
                            .source
                            .as_str()
                            .len()
                            .min(decl.key.anchor.source.as_str().len()),
                        0,
                    )?;
                    if *key == &decl.key {
                        return Ok(Fact::Unknown(UnknownReason::CyclicType));
                    }
                }
                budget.charge(1, 0, size_of::<&DefinitionKey>())?;
                visited.push(&decl.key);
                self = Self {
                    carrier: Carrier::Captured(&decl.ty),
                    site: &decl.type_site,
                    settings: &decl.settings,
                    owner: &decl.key,
                };
                depth = depth
                    .checked_add(1)
                    .ok_or(IncompleteReason::MalformedSource)?;
            }
        })();
        budget.release(budget.snapshot().retained_bytes.saturating_sub(before));
        result
    }
}
