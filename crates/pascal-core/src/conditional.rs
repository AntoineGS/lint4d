//! Conservative, offset-preserving analysis of Pascal compiler directives.
//!
//! This module intentionally remains a small abstract interpreter rather than
//! a Delphi preprocessor.  It knows project-provided defines and facts proved
//! unconditionally while walking a source buffer; anything else remains
//! [`Truth::Unknown`].

use crate::resolver::{CancellationToken, NoCancellation};
use std::collections::HashMap;
use std::mem::size_of;
use std::ops::Range;

const MAX_DIRECTIVES: usize = 16_384;
const MAX_CONDITIONAL_DEPTH: usize = 256;
const MAX_EXPRESSION_TOKENS: usize = 256;
const MAX_EXPRESSION_BYTES: usize = 4_096;
const MAX_ENVIRONMENT_ENTRIES: usize = 32_768;
const MAX_ENVIRONMENT_BYTES: usize = 1024 * 1024;
const MAX_ENVIRONMENT_WORK: usize = 1_000_000;
const MAX_ENVIRONMENT_BYTE_WORK: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, value) | (value, Self::True) => value,
            (Self::Unknown, Self::Unknown) => Self::Unknown,
        }
    }

    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, value) | (value, Self::False) => value,
            (Self::Unknown, Self::Unknown) => Self::Unknown,
        }
    }

    fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }

    fn merge(self, other: Self) -> Self {
        if self == other { self } else { Self::Unknown }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveKind {
    Include,
    ConditionalStart,
    ConditionalMiddle,
    ConditionalEnd,
    Define,
    Undef,
    MethodInfo,
    Harmless,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionalDirective {
    pub kind: DirectiveKind,
    pub body: String,
    pub start: usize,
    pub end: usize,
    /// Whether the directive itself is in a branch that may execute.
    pub activity: Truth,
}

impl ConditionalDirective {
    pub fn potentially_active(&self) -> bool {
        self.activity != Truth::False
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionalAnalysis {
    /// Source used for parsing/indexing. It has the same byte length and line
    /// breaks as the original source.
    pub projected_source: String,
    pub inactive_spans: Vec<Range<usize>>,
    pub unknown_spans: Vec<Range<usize>>,
    pub directives: Vec<ConditionalDirective>,
    /// False means that a malformed structure, cancellation, or safety budget
    /// prevented a complete walk. A true result may still contain unknown
    /// activity spans.
    pub complete: bool,
}

impl ConditionalAnalysis {
    pub fn is_unknown_at(&self, offset: usize) -> bool {
        self.unknown_spans
            .iter()
            .any(|span| span.start <= offset && offset < span.end)
    }

    pub fn unknown_contains_identifier(&self, source: &str, name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        let wanted = name.trim_start_matches('&');
        identifier_spans(source).into_iter().any(|(start, end)| {
            end > start
                && source
                    .get(start..end)
                    .is_some_and(|identifier| identifier.eq_ignore_ascii_case(wanted))
                && self
                    .unknown_spans
                    .iter()
                    .any(|span| start >= span.start && end <= span.end)
        })
    }

    pub fn potentially_active_contains_identifier(&self, source: &str, names: &[String]) -> bool {
        identifier_spans(source).into_iter().any(|(start, end)| {
            let in_inactive = self
                .inactive_spans
                .iter()
                .any(|span| start >= span.start && end <= span.end);
            let in_directive = self
                .directives
                .iter()
                .any(|directive| start >= directive.start && end <= directive.end);
            !in_inactive
                && !in_directive
                && source.get(start..end).is_some_and(|identifier| {
                    names
                        .iter()
                        .any(|name| identifier.eq_ignore_ascii_case(name.trim_start_matches('&')))
                })
        })
    }

    pub fn pascal_condition_contains_identifier(&self, names: &[String]) -> bool {
        if names
            .iter()
            .any(|name| !name.trim_start_matches('&').is_ascii())
        {
            return true;
        }
        self.directives.iter().any(|directive| {
            let is_expression = matches!(
                directive.kind,
                DirectiveKind::ConditionalStart | DirectiveKind::ConditionalMiddle
            ) && directive_keyword(&directive.body).is_some_and(|keyword| {
                keyword.eq_ignore_ascii_case("if")
                    || keyword.eq_ignore_ascii_case("elseif")
                    || keyword.eq_ignore_ascii_case("elif")
            });
            if !is_expression {
                return false;
            }
            let expression = directive_arguments(&directive.body);
            identifier_spans(expression)
                .into_iter()
                .any(|(start, end)| {
                    let identifier = &expression[start..end];
                    names.iter().any(|name| {
                        identifier.eq_ignore_ascii_case(name.trim_start_matches('&'))
                            && !is_defined_call(expression, start)
                    })
                })
        })
    }

    /// Unknown branch activity is only unsafe for source analysis when the
    /// branch can contribute Pascal tokens or another include. Compiler-only
    /// directive blocks do not affect the parsed binding graph and may be
    /// carried through safely.
    pub fn unknown_activity_requires_fail_closed(&self) -> bool {
        if self.directives.iter().any(|directive| {
            directive.activity == Truth::Unknown && directive.kind == DirectiveKind::Include
        }) {
            return true;
        }
        if self.unknown_spans.is_empty() {
            return false;
        }
        self.unknown_spans.iter().any(|span| {
            self.projected_source
                .get(span.clone())
                .is_some_and(contains_pascal_tokens)
        })
    }
}

fn contains_pascal_tokens(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' && bytes[index] != b'\r' {
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'{' {
            let Some(close) = bytes[index + 1..].iter().position(|byte| *byte == b'}') else {
                return true;
            };
            index = index.saturating_add(close).saturating_add(2);
            continue;
        }
        if bytes[index] == b'(' && bytes.get(index + 1) == Some(&b'*') {
            let Some(close) = bytes[index + 2..]
                .windows(2)
                .position(|window| window == b"*)")
            else {
                return true;
            };
            index = index.saturating_add(close).saturating_add(4);
            continue;
        }
        return true;
    }
    false
}

#[derive(Debug, Clone)]
struct RawDirective {
    start: usize,
    end: usize,
    body: String,
}

#[derive(Debug, Default)]
struct LexResult {
    directives: Vec<RawDirective>,
    complete: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ConditionalEnvironment {
    values: HashMap<String, Truth>,
    bytes: usize,
}

impl ConditionalEnvironment {
    fn new() -> Self {
        Self::default()
    }

    pub fn from_defines(defines: &[String]) -> Self {
        let mut environment = Self::new();
        for define in defines {
            if let Some(symbol) = canonical_symbol(define) {
                environment.insert(symbol, Truth::True);
            }
        }
        environment
    }

    fn len(&self) -> usize {
        self.values.len()
    }
    fn contains_key(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }
    fn get(&self, key: &str) -> Option<&Truth> {
        self.values.get(key)
    }
    fn keys(&self) -> impl Iterator<Item = &String> {
        self.values.keys()
    }
    fn bytes(&self) -> usize {
        self.bytes
    }

    fn clear(&mut self) {
        self.values.clear();
        self.bytes = 0;
    }

    fn insert(&mut self, key: String, value: Truth) {
        if !self.values.contains_key(&key) {
            self.bytes = self
                .bytes
                .saturating_add(key.len().saturating_add(size_of::<Truth>()));
        }
        self.values.insert(key, value);
    }
}

#[derive(Debug)]
struct ConditionalFrame {
    parent_active: Truth,
    before_environment: ConditionalEnvironment,
    remaining: Truth,
    current_active: Truth,
    has_else: bool,
    merged_environment: Option<ConditionalEnvironment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncludeTransition {
    pub complete: bool,
    pub environment_known: bool,
}

type IncludeCallback<'a> =
    &'a mut dyn FnMut(&ConditionalDirective, &mut ConditionalEnvironment) -> IncludeTransition;

/// Analyze a source buffer using the selected project's positive define facts.
pub fn analyze(source: &str, project_defines: &[String]) -> ConditionalAnalysis {
    analyze_with_cancel(source, project_defines, &NoCancellation)
}

/// Analyze a source buffer while polling a caller-owned cancellation token.
///
/// Cancellation is represented as an incomplete analysis because callers must
/// fail closed whenever abstract interpretation did not finish.
pub fn analyze_with_cancel(
    source: &str,
    project_defines: &[String],
    cancel: &dyn CancellationToken,
) -> ConditionalAnalysis {
    analyze_inner(source, project_defines, Some(cancel))
}

struct AnalysisBudget<'a> {
    cancel: Option<&'a dyn CancellationToken>,
    work: usize,
    byte_work: usize,
    exhausted: bool,
}

impl AnalysisBudget<'_> {
    fn poll(&mut self) -> bool {
        if self.exhausted {
            return false;
        }
        if self.cancel.is_some_and(|cancel| cancel.is_cancelled()) {
            self.exhausted = true;
            return false;
        }
        true
    }

    fn charge(&mut self, amount: usize) -> bool {
        if !self.poll() {
            return false;
        }
        let Some(work) = self.work.checked_add(amount) else {
            self.exhausted = true;
            return false;
        };
        if work > MAX_ENVIRONMENT_WORK {
            self.exhausted = true;
            return false;
        }
        self.work = work;
        true
    }

    fn charge_bytes(&mut self, amount: usize) -> bool {
        if !self.poll() {
            return false;
        }
        let Some(byte_work) = self.byte_work.checked_add(amount) else {
            self.exhausted = true;
            return false;
        };
        if byte_work > MAX_ENVIRONMENT_BYTE_WORK {
            self.exhausted = true;
            return false;
        }
        self.byte_work = byte_work;
        true
    }

    fn check_environment_bytes(&mut self, bytes: usize) -> bool {
        if bytes > MAX_ENVIRONMENT_BYTES {
            self.exhausted = true;
            false
        } else {
            true
        }
    }
}

fn analyze_inner(
    source: &str,
    project_defines: &[String],
    cancel: Option<&dyn CancellationToken>,
) -> ConditionalAnalysis {
    let lexed = lex_directives(source, cancel);
    let mut budget = AnalysisBudget {
        cancel,
        work: 0,
        byte_work: 0,
        exhausted: false,
    };
    let mut complete = lexed.complete;
    let mut environment = match initial_environment(project_defines, &mut budget) {
        Some(environment) => environment,
        None => {
            complete = false;
            ConditionalEnvironment::new()
        }
    };
    analyze_lexed(source, lexed, &mut environment, cancel, None, complete)
}

pub fn analyze_with_include_callback(
    source: &str,
    environment: &mut ConditionalEnvironment,
    cancel: &dyn CancellationToken,
    include: &mut dyn FnMut(
        &ConditionalDirective,
        &mut ConditionalEnvironment,
    ) -> IncludeTransition,
) -> ConditionalAnalysis {
    let lexed = lex_directives(source, Some(cancel));
    analyze_lexed(
        source,
        lexed,
        environment,
        Some(cancel),
        Some(include),
        true,
    )
}

fn analyze_lexed(
    source: &str,
    lexed: LexResult,
    environment: &mut ConditionalEnvironment,
    cancel: Option<&dyn CancellationToken>,
    mut include: Option<IncludeCallback<'_>>,
    initial_complete: bool,
) -> ConditionalAnalysis {
    let mut budget = AnalysisBudget {
        cancel,
        work: 0,
        byte_work: 0,
        exhausted: false,
    };
    let mut complete = initial_complete && lexed.complete;
    let mut active = Truth::True;
    let mut frames = Vec::new();
    let mut inactive_spans = Vec::new();
    let mut unknown_spans = Vec::new();
    let mut directives = Vec::with_capacity(lexed.directives.len());
    let mut masked_spans = Vec::with_capacity(lexed.directives.len());
    let mut cursor = 0;

    for raw in lexed.directives {
        if !budget.poll() {
            complete = false;
            break;
        }
        add_activity_span(
            &mut inactive_spans,
            &mut unknown_spans,
            cursor,
            raw.start,
            active,
        );
        masked_spans.push(raw.start..raw.end);
        let kind = directive_kind(&raw.body);
        directives.push(ConditionalDirective {
            kind,
            body: raw.body.clone(),
            start: raw.start,
            end: raw.end,
            activity: active,
        });

        match kind {
            DirectiveKind::ConditionalStart => {
                if frames.len() >= MAX_CONDITIONAL_DEPTH {
                    complete = false;
                    active = Truth::Unknown;
                } else {
                    let condition = evaluate_condition(&raw.body, environment, &mut complete);
                    let Some(before_environment) = clone_environment(environment, &mut budget)
                    else {
                        complete = false;
                        cursor = raw.end;
                        break;
                    };
                    let current_active = active.and(condition);
                    frames.push(ConditionalFrame {
                        parent_active: active,
                        before_environment,
                        remaining: condition.not(),
                        current_active,
                        has_else: false,
                        merged_environment: None,
                    });
                    active = current_active;
                }
            }
            DirectiveKind::ConditionalMiddle => {
                let Some(frame) = frames.last_mut() else {
                    complete = false;
                    active = Truth::Unknown;
                    cursor = raw.end;
                    continue;
                };
                let is_else = is_else_directive(&raw.body);
                if is_else && !directive_arguments(&raw.body).trim().is_empty() {
                    complete = false;
                }
                if is_else && frame.has_else {
                    complete = false;
                }
                if !is_else && frame.has_else {
                    complete = false;
                }
                if frame.current_active != Truth::False
                    && !merge_environment(&mut frame.merged_environment, environment, &mut budget)
                {
                    complete = false;
                    cursor = raw.end;
                    break;
                }
                let Some(restored_environment) =
                    clone_environment(&frame.before_environment, &mut budget)
                else {
                    complete = false;
                    cursor = raw.end;
                    break;
                };
                *environment = restored_environment;

                if is_else {
                    frame.has_else = true;
                    frame.current_active = frame.parent_active.and(frame.remaining);
                    frame.remaining = Truth::False;
                } else {
                    let condition = evaluate_condition(&raw.body, environment, &mut complete);
                    frame.current_active = frame.parent_active.and(frame.remaining).and(condition);
                    frame.remaining = frame.remaining.and(condition.not());
                }
                active = frame.current_active;
            }
            DirectiveKind::ConditionalEnd => {
                if !directive_arguments(&raw.body).trim().is_empty() {
                    complete = false;
                }
                let Some(mut frame) = frames.pop() else {
                    complete = false;
                    active = Truth::Unknown;
                    cursor = raw.end;
                    continue;
                };
                let Some(merged_environment) =
                    merge_conditional_environment(&mut frame, environment, &mut budget)
                else {
                    complete = false;
                    cursor = raw.end;
                    break;
                };
                *environment = merged_environment;
                active = frame.parent_active;
            }
            DirectiveKind::Define | DirectiveKind::Undef => {
                if let Some(symbol) = directive_symbol(&raw.body) {
                    let value = if kind == DirectiveKind::Define {
                        Truth::True
                    } else {
                        Truth::False
                    };
                    if !apply_fact(environment, &symbol, value, active, &mut budget) {
                        complete = false;
                    }
                } else {
                    complete = false;
                }
            }
            DirectiveKind::Include => {
                // Include files can DEFINE/UNDEF symbols or inject nested
                // directives. Unless their contents have been soundly
                // processed, no previously known fact survives an active
                // include boundary.
                if active == Truth::True {
                    if let Some(include) = include.as_deref_mut() {
                        let transition = include(
                            directives.last().expect("include directive was recorded"),
                            environment,
                        );
                        if !transition.complete {
                            complete = false;
                        }
                        if !transition.environment_known {
                            environment.clear();
                        }
                    } else {
                        environment.clear();
                    }
                } else if active == Truth::Unknown {
                    environment.clear();
                }
            }
            DirectiveKind::MethodInfo | DirectiveKind::Harmless | DirectiveKind::Other => {}
        }
        cursor = raw.end;
    }

    add_activity_span(
        &mut inactive_spans,
        &mut unknown_spans,
        cursor,
        source.len(),
        active,
    );
    if !frames.is_empty() {
        complete = false;
    }
    if !budget.poll() {
        complete = false;
    }
    if !complete {
        inactive_spans.clear();
        unknown_spans.clear();
        if !source.is_empty() {
            unknown_spans.push(0..source.len());
        }
    }

    ConditionalAnalysis {
        projected_source: project_source(source, &inactive_spans, &masked_spans),
        inactive_spans,
        unknown_spans,
        directives,
        complete,
    }
}

fn initial_environment(
    project_defines: &[String],
    budget: &mut AnalysisBudget<'_>,
) -> Option<ConditionalEnvironment> {
    let mut environment = ConditionalEnvironment::new();
    for define in project_defines {
        if !budget.charge(1) {
            return None;
        }
        if let Some(symbol) = canonical_symbol(define) {
            if !budget.charge_bytes(symbol.len()) {
                return None;
            }
            if !environment.contains_key(&symbol) && environment.len() >= MAX_ENVIRONMENT_ENTRIES {
                budget.exhausted = true;
                return None;
            }
            if !environment.contains_key(&symbol)
                && !budget.check_environment_bytes(
                    environment
                        .bytes()
                        .saturating_add(symbol.len().saturating_add(size_of::<Truth>())),
                )
            {
                return None;
            }
            environment.insert(symbol, Truth::True);
        }
    }
    Some(environment)
}

fn canonical_symbol(symbol: &str) -> Option<String> {
    let symbol = symbol.trim().trim_start_matches('&');
    if symbol.is_empty()
        || !symbol
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.')
    {
        return None;
    }
    Some(symbol.to_ascii_uppercase())
}

fn environment_value(environment: &ConditionalEnvironment, symbol: &str) -> Truth {
    canonical_symbol(symbol)
        .and_then(|symbol| environment.get(&symbol).copied())
        .unwrap_or(Truth::Unknown)
}

fn clone_environment(
    environment: &ConditionalEnvironment,
    budget: &mut AnalysisBudget<'_>,
) -> Option<ConditionalEnvironment> {
    if environment.len() > MAX_ENVIRONMENT_ENTRIES
        || !budget.charge(environment.len())
        || !budget.check_environment_bytes(environment.bytes())
        || !budget.charge_bytes(environment.bytes())
    {
        budget.exhausted = true;
        return None;
    }
    Some(environment.clone())
}

fn apply_fact(
    environment: &mut ConditionalEnvironment,
    symbol: &str,
    value: Truth,
    activity: Truth,
    budget: &mut AnalysisBudget<'_>,
) -> bool {
    if !budget.charge(1) {
        return false;
    }
    let previous = environment_value(environment, symbol);
    let next = match activity {
        Truth::True => value,
        Truth::False => previous,
        Truth::Unknown => previous.merge(value),
    };
    if let Some(symbol) = canonical_symbol(symbol) {
        if !budget.charge_bytes(symbol.len()) {
            return false;
        }
        if !environment.contains_key(&symbol) && environment.len() >= MAX_ENVIRONMENT_ENTRIES {
            budget.exhausted = true;
            return false;
        }
        if !environment.contains_key(&symbol)
            && !budget.check_environment_bytes(
                environment
                    .bytes()
                    .saturating_add(symbol.len().saturating_add(size_of::<Truth>())),
            )
        {
            return false;
        }
        environment.insert(symbol, next);
    }
    true
}

fn merge_conditional_environment(
    frame: &mut ConditionalFrame,
    current_environment: &ConditionalEnvironment,
    budget: &mut AnalysisBudget<'_>,
) -> Option<ConditionalEnvironment> {
    let mut merged = frame.merged_environment.take();
    if frame.current_active != Truth::False
        && !merge_environment(&mut merged, current_environment, budget)
    {
        return None;
    }
    if frame.parent_active != Truth::True
        && !merge_environment(&mut merged, &frame.before_environment, budget)
    {
        return None;
    }
    if !frame.has_else
        && frame.remaining != Truth::False
        && !merge_environment(&mut merged, &frame.before_environment, budget)
    {
        return None;
    }
    Some(merged.unwrap_or_else(|| std::mem::take(&mut frame.before_environment)))
}

fn merge_environment(
    target: &mut Option<ConditionalEnvironment>,
    incoming: &ConditionalEnvironment,
    budget: &mut AnalysisBudget<'_>,
) -> bool {
    let Some(current) = target.as_mut() else {
        *target = clone_environment(incoming, budget);
        return target.is_some();
    };
    if !budget.charge(current.len().saturating_add(incoming.len())) {
        return false;
    }
    let additional_bytes = incoming
        .keys()
        .filter(|key| !current.contains_key(key.as_str()))
        .map(|key| key.len().saturating_add(size_of::<Truth>()))
        .try_fold(0usize, |total, bytes| total.checked_add(bytes));
    let Some(additional_bytes) = additional_bytes else {
        budget.exhausted = true;
        return false;
    };
    let Some(merged_bytes) = current.bytes().checked_add(additional_bytes) else {
        budget.exhausted = true;
        return false;
    };
    if !budget.check_environment_bytes(merged_bytes)
        || !budget.charge_bytes(
            current
                .bytes()
                .saturating_add(incoming.bytes())
                .saturating_add(merged_bytes),
        )
    {
        return false;
    }
    let mut keys = current.keys().cloned().collect::<Vec<_>>();
    keys.extend(incoming.keys().cloned());
    keys.sort();
    keys.dedup();
    for key in keys {
        let left = current.get(&key).copied().unwrap_or(Truth::Unknown);
        let right = incoming.get(&key).copied().unwrap_or(Truth::Unknown);
        current.insert(key, left.merge(right));
    }
    if current.len() > MAX_ENVIRONMENT_ENTRIES || current.bytes() > MAX_ENVIRONMENT_BYTES {
        budget.exhausted = true;
        return false;
    }
    true
}

fn add_activity_span(
    inactive: &mut Vec<Range<usize>>,
    unknown: &mut Vec<Range<usize>>,
    start: usize,
    end: usize,
    activity: Truth,
) {
    if start >= end {
        return;
    }
    match activity {
        Truth::False => inactive.push(start..end),
        Truth::Unknown => unknown.push(start..end),
        Truth::True => {}
    }
}

fn project_source(source: &str, inactive: &[Range<usize>], directives: &[Range<usize>]) -> String {
    let mut bytes = source.as_bytes().to_vec();
    for span in inactive.iter().chain(directives.iter()) {
        let start = span.start.min(bytes.len());
        let end = span.end.min(bytes.len());
        for byte in &mut bytes[start..end] {
            if *byte != b'\n' && *byte != b'\r' {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(bytes).expect("offset-preserving projection remains UTF-8")
}

fn lex_directives(source: &str, cancel: Option<&dyn CancellationToken>) -> LexResult {
    let bytes = source.as_bytes();
    let mut result = LexResult {
        directives: Vec::new(),
        complete: true,
    };
    let mut index = 0;
    while index < bytes.len() {
        if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
            result.complete = false;
            break;
        }
        match bytes[index] {
            b'\'' => match skip_string(bytes, index) {
                Some(next) => index = next,
                None => {
                    result.complete = false;
                    break;
                }
            },
            b'/' if bytes.get(index + 1) == Some(&b'/') => index = skip_line_comment(bytes, index),
            b'{' if bytes.get(index + 1) == Some(&b'$') => {
                let Some(close) = find_byte(bytes, index + 2, b'}') else {
                    result.complete = false;
                    break;
                };
                if result.directives.len() >= MAX_DIRECTIVES {
                    result.complete = false;
                    break;
                }
                result.directives.push(RawDirective {
                    start: index,
                    end: close + 1,
                    body: source[index + 2..close].to_owned(),
                });
                index = close + 1;
            }
            b'{' => match find_byte(bytes, index + 1, b'}') {
                Some(close) => index = close + 1,
                None => {
                    result.complete = false;
                    break;
                }
            },
            b'(' if bytes.get(index + 1) == Some(&b'*') => {
                if bytes.get(index + 2) == Some(&b'$') {
                    let Some(close) = find_sequence(bytes, index + 3, b"*)") else {
                        result.complete = false;
                        break;
                    };
                    if result.directives.len() >= MAX_DIRECTIVES {
                        result.complete = false;
                        break;
                    }
                    result.directives.push(RawDirective {
                        start: index,
                        end: close + 2,
                        body: source[index + 3..close].to_owned(),
                    });
                    index = close + 2;
                } else {
                    let Some(close) = find_sequence(bytes, index + 2, b"*)") else {
                        result.complete = false;
                        break;
                    };
                    index = close + 2;
                }
            }
            _ => index += 1,
        }
    }
    result
}

fn find_byte(bytes: &[u8], start: usize, wanted: u8) -> Option<usize> {
    bytes[start..]
        .iter()
        .position(|byte| *byte == wanted)
        .map(|offset| start + offset)
}

fn find_sequence(bytes: &[u8], start: usize, wanted: &[u8]) -> Option<usize> {
    bytes[start..]
        .windows(wanted.len())
        .position(|window| window == wanted)
        .map(|offset| start + offset)
}

fn skip_string(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] != b'\'' {
            if bytes[index] == b'\n' {
                return None;
            }
            index += 1;
        } else if bytes.get(index + 1) == Some(&b'\'') {
            index += 2;
        } else {
            return Some(index + 1);
        }
    }
    None
}

fn skip_line_comment(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 2;
    while index < bytes.len() && bytes[index] != b'\n' {
        index += 1;
    }
    index
}

fn directive_kind(body: &str) -> DirectiveKind {
    let keyword = directive_keyword(body)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match keyword.as_str() {
        "i" | "include" => DirectiveKind::Include,
        "if" | "ifdef" | "ifndef" | "ifopt" => DirectiveKind::ConditionalStart,
        "else" | "elseif" | "elif" => DirectiveKind::ConditionalMiddle,
        "endif" | "ifend" => DirectiveKind::ConditionalEnd,
        "define" => DirectiveKind::Define,
        "undef" => DirectiveKind::Undef,
        "methodinfo" => DirectiveKind::MethodInfo,
        _ if is_harmless_keyword(&keyword) => DirectiveKind::Harmless,
        _ => DirectiveKind::Other,
    }
}

fn directive_keyword(body: &str) -> Option<&str> {
    body.trim_start()
        .split(|character: char| character.is_ascii_whitespace() || character == ':')
        .next()
        .filter(|keyword| !keyword.is_empty())
}

fn directive_arguments(body: &str) -> &str {
    let body = body.trim_start();
    let Some(keyword) = directive_keyword(body) else {
        return "";
    };
    body.get(keyword.len()..)
        .map(str::trim_start)
        .unwrap_or_default()
}

fn is_defined_call(expression: &str, identifier_start: usize) -> bool {
    let prefix = expression[..identifier_start].trim_end();
    let Some(prefix) = prefix.strip_suffix('(') else {
        return false;
    };
    let prefix = prefix.trim_end();
    let start = prefix
        .as_bytes()
        .iter()
        .rposition(|byte| !is_identifier_byte(*byte))
        .map_or(0, |index| index + 1);
    prefix[start..].eq_ignore_ascii_case("defined")
}

fn is_else_directive(body: &str) -> bool {
    directive_keyword(body).is_some_and(|keyword| keyword.eq_ignore_ascii_case("else"))
}

fn directive_symbol(body: &str) -> Option<String> {
    let argument = directive_arguments(body).trim();
    let symbol = argument.strip_prefix('&').unwrap_or(argument);
    let mut characters = symbol.chars();
    let first = characters.next()?;
    if !first.is_ascii_alphabetic() && first != '_' {
        return None;
    }
    if !characters
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '.')
    {
        return None;
    }
    canonical_symbol(argument)
}

/// Return the canonical symbol from a DEFINE/UNDEF directive for bounded
/// consumers that carry conditional facts across source boundaries.
pub fn defined_symbol(body: &str) -> Option<String> {
    directive_symbol(body)
}

fn evaluate_condition(
    body: &str,
    environment: &ConditionalEnvironment,
    complete: &mut bool,
) -> Truth {
    let keyword = directive_keyword(body)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let arguments = directive_arguments(body);
    match keyword.as_str() {
        "ifdef" => directive_symbol(body)
            .map(|symbol| environment_value(environment, &symbol))
            .unwrap_or_else(|| {
                *complete = false;
                Truth::Unknown
            }),
        "ifndef" => directive_symbol(body)
            .map(|symbol| environment_value(environment, &symbol).not())
            .unwrap_or_else(|| {
                *complete = false;
                Truth::Unknown
            }),
        "if" | "elseif" | "elif" => evaluate_expression(arguments, environment, complete),
        "ifopt" => Truth::Unknown,
        _ => {
            *complete = false;
            Truth::Unknown
        }
    }
}

fn evaluate_expression(
    expression: &str,
    environment: &ConditionalEnvironment,
    complete: &mut bool,
) -> Truth {
    if expression.len() > MAX_EXPRESSION_BYTES {
        *complete = false;
        return Truth::Unknown;
    }
    let tokens = tokenize_expression(expression);
    if tokens.len() > MAX_EXPRESSION_TOKENS {
        *complete = false;
        return Truth::Unknown;
    }
    let mut parser = ExpressionParser {
        tokens,
        index: 0,
        environment,
        malformed: false,
    };
    let result = parser.parse_comparison().as_truth();
    if !matches!(parser.peek(), ExprToken::End) {
        parser.malformed = true;
    }
    if parser.malformed {
        *complete = false;
    }
    result
}

#[derive(Debug, Clone, PartialEq)]
enum ExprToken {
    Identifier(String),
    Number(i64),
    String(String),
    LParen,
    RParen,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    End,
    Invalid,
}

fn tokenize_expression(expression: &str) -> Vec<ExprToken> {
    let bytes = expression.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        match bytes[index] {
            b'(' => {
                tokens.push(ExprToken::LParen);
                index += 1;
            }
            b')' => {
                tokens.push(ExprToken::RParen);
                index += 1;
            }
            b'=' => {
                tokens.push(ExprToken::Equal);
                index += 1;
            }
            b'<' => {
                if bytes.get(index + 1) == Some(&b'=') {
                    tokens.push(ExprToken::LessEqual);
                    index += 2;
                } else if bytes.get(index + 1) == Some(&b'>') {
                    tokens.push(ExprToken::NotEqual);
                    index += 2;
                } else {
                    tokens.push(ExprToken::Less);
                    index += 1;
                }
            }
            b'>' => {
                if bytes.get(index + 1) == Some(&b'=') {
                    tokens.push(ExprToken::GreaterEqual);
                    index += 2;
                } else {
                    tokens.push(ExprToken::Greater);
                    index += 1;
                }
            }
            b'!' if bytes.get(index + 1) == Some(&b'=') => {
                tokens.push(ExprToken::NotEqual);
                index += 2;
            }
            b'\'' => {
                index += 1;
                let mut value = String::new();
                let mut closed = false;
                while index < bytes.len() {
                    if bytes[index] == b'\'' {
                        if bytes.get(index + 1) == Some(&b'\'') {
                            value.push('\'');
                            index += 2;
                        } else {
                            index += 1;
                            closed = true;
                            break;
                        }
                    } else {
                        let next = expression[index..].chars().next().unwrap_or_default();
                        value.push(next);
                        index += next.len_utf8();
                    }
                }
                if closed {
                    tokens.push(ExprToken::String(value));
                } else {
                    tokens.push(ExprToken::Invalid);
                    break;
                }
            }
            b'$' if bytes
                .get(index + 1)
                .is_some_and(|byte| byte.is_ascii_hexdigit()) =>
            {
                let start = index + 1;
                index += 1;
                while index < bytes.len() && bytes[index].is_ascii_hexdigit() {
                    index += 1;
                }
                let value = i64::from_str_radix(&expression[start..index], 16).ok();
                tokens.push(value.map_or(ExprToken::Invalid, ExprToken::Number));
            }
            byte if byte.is_ascii_digit() => {
                let start = index;
                index += 1;
                while index < bytes.len() && bytes[index].is_ascii_digit() {
                    index += 1;
                }
                let value = expression[start..index].parse::<i64>().ok();
                tokens.push(value.map_or(ExprToken::Invalid, ExprToken::Number));
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' || byte == b'&' => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric()
                        || bytes[index] == b'_'
                        || bytes[index] == b'.')
                {
                    index += 1;
                }
                tokens.push(ExprToken::Identifier(
                    expression[start..index].trim_start_matches('&').to_owned(),
                ));
            }
            _ => {
                tokens.push(ExprToken::Invalid);
                index += 1;
            }
        }
    }
    tokens.push(ExprToken::End);
    tokens
}

struct ExpressionParser<'a> {
    tokens: Vec<ExprToken>,
    index: usize,
    environment: &'a ConditionalEnvironment,
    malformed: bool,
}

impl ExpressionParser<'_> {
    fn peek(&self) -> &ExprToken {
        self.tokens.get(self.index).unwrap_or(&ExprToken::End)
    }

    fn take(&mut self) -> ExprToken {
        let token = self.peek().clone();
        self.index = self.index.saturating_add(1);
        token
    }

    fn consume_identifier(&mut self, wanted: &str) -> bool {
        if matches!(self.peek(), ExprToken::Identifier(value) if value.eq_ignore_ascii_case(wanted))
        {
            self.index = self.index.saturating_add(1);
            true
        } else {
            false
        }
    }

    fn parse_or(&mut self) -> Value {
        let mut value = self.parse_and();
        while self.consume_identifier("or") {
            value = value.or(self.parse_and());
        }
        value
    }

    fn parse_and(&mut self) -> Value {
        let mut value = self.parse_not();
        while self.consume_identifier("and") {
            value = value.and(self.parse_not());
        }
        value
    }

    fn parse_not(&mut self) -> Value {
        if self.consume_identifier("not") {
            self.parse_not().not()
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> Value {
        if matches!(self.peek(), ExprToken::LParen) {
            self.take();
            let value = self.parse_comparison();
            if !matches!(self.peek(), ExprToken::RParen) {
                self.malformed = true;
            } else {
                self.take();
            }
            value
        } else {
            self.parse_value()
        }
    }

    fn parse_comparison(&mut self) -> Value {
        let left = self.parse_or();
        let operator = match self.peek() {
            ExprToken::Equal => Some(Comparison::Equal),
            ExprToken::NotEqual => Some(Comparison::NotEqual),
            ExprToken::Less => Some(Comparison::Less),
            ExprToken::LessEqual => Some(Comparison::LessEqual),
            ExprToken::Greater => Some(Comparison::Greater),
            ExprToken::GreaterEqual => Some(Comparison::GreaterEqual),
            _ => None,
        };
        let Some(operator) = operator else {
            return left;
        };
        self.take();
        let right = self.parse_or();
        Value::Truth(compare_values(left, right, operator))
    }

    fn parse_value(&mut self) -> Value {
        match self.take() {
            ExprToken::Identifier(identifier) if identifier.eq_ignore_ascii_case("defined") => {
                let symbol = if matches!(self.peek(), ExprToken::LParen) {
                    self.take();
                    let symbol = match self.take() {
                        ExprToken::Identifier(value) => Some(value),
                        _ => None,
                    };
                    if !matches!(self.peek(), ExprToken::RParen) {
                        self.malformed = true;
                    } else {
                        self.take();
                    }
                    symbol
                } else {
                    match self.take() {
                        ExprToken::Identifier(value) => Some(value),
                        _ => None,
                    }
                };
                symbol
                    .map(|symbol| Value::Truth(environment_value(self.environment, &symbol)))
                    .unwrap_or_else(|| {
                        self.malformed = true;
                        Value::Unknown
                    })
            }
            ExprToken::Identifier(identifier) if identifier.eq_ignore_ascii_case("true") => {
                Value::Truth(Truth::True)
            }
            ExprToken::Identifier(identifier) if identifier.eq_ignore_ascii_case("false") => {
                Value::Truth(Truth::False)
            }
            ExprToken::Identifier(_) => Value::Unknown,
            ExprToken::Number(value) => Value::Number(value),
            ExprToken::String(value) => Value::String(value),
            ExprToken::Invalid | ExprToken::End => {
                self.malformed = true;
                Value::Unknown
            }
            ExprToken::LParen
            | ExprToken::RParen
            | ExprToken::Equal
            | ExprToken::NotEqual
            | ExprToken::Less
            | ExprToken::LessEqual
            | ExprToken::Greater
            | ExprToken::GreaterEqual => {
                self.malformed = true;
                Value::Unknown
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Value {
    Truth(Truth),
    Number(i64),
    String(String),
    Unknown,
}

impl Value {
    fn as_truth(&self) -> Truth {
        match self {
            Self::Truth(value) => *value,
            Self::Number(_) | Self::String(_) | Self::Unknown => Truth::Unknown,
        }
    }

    fn logical_truth(&self) -> Option<Truth> {
        match self {
            Self::Truth(value) => Some(*value),
            Self::Unknown => Some(Truth::Unknown),
            Self::Number(_) | Self::String(_) => None,
        }
    }

    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Truth(left), Self::Truth(right)) => Self::Truth(left.and(right)),
            (Self::Number(left), Self::Number(right)) => Self::Number(left & right),
            (left, right) => match (left.logical_truth(), right.logical_truth()) {
                (Some(left), Some(right)) => Self::Truth(left.and(right)),
                _ => Self::Unknown,
            },
        }
    }

    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::Truth(left), Self::Truth(right)) => Self::Truth(left.or(right)),
            (Self::Number(left), Self::Number(right)) => Self::Number(left | right),
            (left, right) => match (left.logical_truth(), right.logical_truth()) {
                (Some(left), Some(right)) => Self::Truth(left.or(right)),
                _ => Self::Unknown,
            },
        }
    }

    fn not(self) -> Self {
        match self {
            Self::Truth(value) => Self::Truth(value.not()),
            Self::Number(value) => Self::Number(!value),
            Self::String(_) | Self::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

fn compare_values(left: Value, right: Value, comparison: Comparison) -> Truth {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => compare_order(left, right, comparison),
        (Value::String(left), Value::String(right)) => compare_order(left, right, comparison),
        (Value::Truth(left), Value::Truth(right)) => compare_truths(left, right, comparison),
        _ => Truth::Unknown,
    }
}

fn compare_truths(left: Truth, right: Truth, comparison: Comparison) -> Truth {
    let left = match left {
        Truth::False => 0_u8,
        Truth::True => 1_u8,
        Truth::Unknown => return Truth::Unknown,
    };
    let right = match right {
        Truth::False => 0_u8,
        Truth::True => 1_u8,
        Truth::Unknown => return Truth::Unknown,
    };
    compare_order(left, right, comparison)
}

fn compare_order<T: PartialOrd + PartialEq>(left: T, right: T, comparison: Comparison) -> Truth {
    let value = match comparison {
        Comparison::Equal => left == right,
        Comparison::NotEqual => left != right,
        Comparison::Less => left < right,
        Comparison::LessEqual => left <= right,
        Comparison::Greater => left > right,
        Comparison::GreaterEqual => left >= right,
    };
    if value { Truth::True } else { Truth::False }
}

fn identifier_spans(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut result = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' => index = skip_string(bytes, index).unwrap_or(bytes.len()),
            b'/' if bytes.get(index + 1) == Some(&b'/') => index = skip_line_comment(bytes, index),
            b'{' => {
                index = find_byte(bytes, index + 1, b'}').map_or(bytes.len(), |close| close + 1)
            }
            b'(' if bytes.get(index + 1) == Some(&b'*') => {
                index =
                    find_sequence(bytes, index + 2, b"*)").map_or(bytes.len(), |close| close + 2)
            }
            byte if is_identifier_byte(byte) => {
                let start = index;
                index += 1;
                while index < bytes.len() && is_identifier_byte(bytes[index]) {
                    index += 1;
                }
                result.push((start, index));
            }
            _ => index += 1,
        }
    }
    result
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_harmless_keyword(keyword: &str) -> bool {
    let keyword = keyword.trim_end_matches(['+', '-']);
    matches!(
        keyword,
        "apptype"
            | "asmmode"
            | "assertions"
            | "booleval"
            | "debug"
            | "debugsymbols"
            | "endregion"
            | "excessprecision"
            | "extendedsyntax"
            | "h"
            | "hints"
            | "longstrings"
            | "m"
            | "message"
            | "mode"
            | "objexportall"
            | "optimization"
            | "overflowchecks"
            | "q"
            | "r"
            | "rangechecks"
            | "region"
            | "rtti"
            | "stronglinktypes"
            | "t"
            | "typedaddress"
            | "warn"
            | "warnings"
            | "writeableconst"
            | "x"
    )
}
