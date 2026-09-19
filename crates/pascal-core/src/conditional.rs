//! Conservative, offset-preserving analysis of Pascal compiler directives.
//!
//! This module intentionally remains a small abstract interpreter rather than
//! a Delphi preprocessor.  It knows project-provided defines and facts proved
//! unconditionally while walking a source buffer; anything else remains
//! [`Truth::Unknown`].

use crate::resolver::{CancellationToken, NoCancellation};
use pascal_project::canonical_option_name;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
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

pub use pascal_project::ConditionalFact as Truth;
pub use pascal_project::{CompilerVersion, ConditionalContext, ConstantValue};

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
    /// The condition evaluated by an IF/ELSEIF directive, when its expression
    /// was syntactically supported.  This is used by downstream configured
    /// projections; an unknown condition remains `None`.
    pub condition: Option<Truth>,
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
    options: HashMap<String, Truth>,
    constants: BTreeMap<String, ConstantValue>,
    source_constants: BTreeSet<String>,
    compiler_version: Option<CompilerVersion>,
    bytes: usize,
    rejected_context: bool,
}

impl ConditionalEnvironment {
    fn new() -> Self {
        Self::default()
    }

    pub fn from_defines(defines: &[String]) -> Self {
        Self::from_context(&ConditionalContext::from_defines(defines))
    }

    pub fn from_context(context: &ConditionalContext) -> Self {
        Self::try_from_context(context).unwrap_or_else(|| Self {
            rejected_context: true,
            ..Self::default()
        })
    }

    /// Build an environment without cloning any retained payload until all
    /// entry and byte limits have been checked.
    pub fn try_from_context(context: &ConditionalContext) -> Option<Self> {
        let mut budget = AnalysisBudget {
            cancel: None,
            work: 0,
            byte_work: 0,
            exhausted: false,
        };
        Self::try_from_context_with_budget(context, &mut budget)
    }

    fn try_from_context_with_budget(
        context: &ConditionalContext,
        budget: &mut AnalysisBudget<'_>,
    ) -> Option<Self> {
        let mut environment = Self::new();
        if let Some(version) = context.compiler_version {
            if !budget.charge(1) || !budget.charge_bytes(size_of::<CompilerVersion>()) {
                return None;
            }
            if !environment.set_compiler_version(version) {
                return None;
            }
        }
        for (symbol, value) in &context.defines {
            let raw_symbol = canonical_symbol_ref(symbol)?;
            if !admit_context_entry(budget, raw_symbol.len(), size_of::<Truth>()) {
                return None;
            }
            let symbol = canonical_symbol(raw_symbol)?;
            let value = environment
                .get(&symbol)
                .copied()
                .map_or(*value, |previous| previous.merge(*value));
            if !environment.try_insert_value(&symbol, value) {
                return None;
            }
        }
        for (option, value) in &context.options {
            let raw_option = canonical_symbol_ref(option)?;
            if !admit_context_entry(budget, raw_option.len(), size_of::<Truth>()) {
                return None;
            }
            let option = canonical_option_name(raw_option)?;
            let value = environment
                .options
                .get(&option)
                .copied()
                .map_or(*value, |previous| previous.merge(*value));
            if !environment.try_insert_option(&option, value) {
                return None;
            }
        }
        for (name, value) in &context.constants {
            let raw_name = canonical_symbol_ref(name)?;
            if !admit_context_entry(
                budget,
                raw_name.len(),
                size_of::<ConstantValue>().saturating_add(constant_size(value)),
            ) {
                return None;
            }
            let name = canonical_symbol(raw_name)?;
            if let Some(previous) = environment.constants.get(&name) {
                if previous != value {
                    return None;
                }
                continue;
            }
            if !environment.try_insert_constant(&name, value) {
                return None;
            }
        }
        Some(environment)
    }

    pub fn to_context(&self) -> ConditionalContext {
        ConditionalContext {
            compiler_version: self.compiler_version,
            defines: self
                .values
                .iter()
                .map(|(key, value)| (key.clone(), *value))
                .collect(),
            options: self
                .options
                .iter()
                .map(|(key, value)| (key.clone(), *value))
                .collect(),
            constants: self.constants.clone(),
        }
    }

    fn insert_value(&mut self, key: String, value: Truth) {
        self.replace_fact_bytes(&key, self.value_entry_size(&key));
        self.values.insert(key, value);
    }

    fn insert_option(&mut self, key: String, value: Truth) {
        self.replace_option_bytes(&key, self.value_entry_size(&key));
        self.options.insert(key, value);
    }

    fn insert_constant(&mut self, key: String, value: ConstantValue) {
        self.replace_constant_bytes(&key, self.constant_entry_size(&key, &value));
        self.constants.insert(key, value);
    }

    fn value_entry_size(&self, key: &str) -> usize {
        key.len().saturating_add(size_of::<Truth>())
    }

    fn constant_entry_size(&self, key: &str, value: &ConstantValue) -> usize {
        key.len()
            .saturating_add(size_of::<ConstantValue>())
            .saturating_add(constant_size(value))
    }

    fn source_constant_entry_size(&self, key: &str) -> usize {
        key.len()
    }

    fn replace_fact_bytes(&mut self, key: &str, new_bytes: usize) {
        let old_bytes = self
            .values
            .contains_key(key)
            .then(|| self.value_entry_size(key))
            .unwrap_or(0);
        self.bytes = self
            .bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
    }

    fn replace_option_bytes(&mut self, key: &str, new_bytes: usize) {
        let old_bytes = self
            .options
            .contains_key(key)
            .then(|| self.value_entry_size(key))
            .unwrap_or(0);
        self.bytes = self
            .bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
    }

    fn replace_constant_bytes(&mut self, key: &str, new_bytes: usize) {
        let old_bytes = self
            .constants
            .get(key)
            .map(|value| self.constant_entry_size(key, value))
            .unwrap_or(0);
        self.bytes = self
            .bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
    }

    fn set_compiler_version(&mut self, version: CompilerVersion) -> bool {
        let old_bytes = if self.compiler_version.is_some() {
            size_of::<CompilerVersion>()
        } else {
            0
        };
        let new_bytes = size_of::<CompilerVersion>();
        let Some(bytes) = self.bytes.saturating_sub(old_bytes).checked_add(new_bytes) else {
            return false;
        };
        if bytes > MAX_ENVIRONMENT_BYTES {
            return false;
        }
        self.bytes = bytes;
        self.compiler_version = Some(version);
        true
    }

    fn try_insert_value(&mut self, key: &str, value: Truth) -> bool {
        let additional = if self.values.contains_key(key) {
            0
        } else {
            self.value_entry_size(key)
        };
        if self.len() >= MAX_ENVIRONMENT_ENTRIES && additional > 0 {
            return false;
        }
        if self.bytes.saturating_add(additional) > MAX_ENVIRONMENT_BYTES {
            return false;
        }
        self.insert_value(key.to_owned(), value);
        true
    }

    fn try_insert_option(&mut self, key: &str, value: Truth) -> bool {
        let additional = if self.options.contains_key(key) {
            0
        } else {
            self.value_entry_size(key)
        };
        if self.len() >= MAX_ENVIRONMENT_ENTRIES && additional > 0 {
            return false;
        }
        if self.bytes.saturating_add(additional) > MAX_ENVIRONMENT_BYTES {
            return false;
        }
        self.insert_option(key.to_owned(), value);
        true
    }

    fn try_insert_constant(&mut self, key: &str, value: &ConstantValue) -> bool {
        let additional = if let Some(previous) = self.constants.get(key) {
            self.constant_entry_size(key, value)
                .saturating_sub(self.constant_entry_size(key, previous))
        } else {
            self.constant_entry_size(key, value)
        };
        if self.len() >= MAX_ENVIRONMENT_ENTRIES && !self.constants.contains_key(key) {
            return false;
        }
        if self.bytes.saturating_add(additional) > MAX_ENVIRONMENT_BYTES {
            return false;
        }
        self.insert_constant(key.to_owned(), value.clone());
        true
    }

    fn try_insert_source_constant(&mut self, key: &str, value: &ConstantValue) -> bool {
        let constant_additional = if let Some(previous) = self.constants.get(key) {
            self.constant_entry_size(key, value)
                .saturating_sub(self.constant_entry_size(key, previous))
        } else {
            self.constant_entry_size(key, value)
        };
        let provenance_additional = if !self.source_constants.contains(key) {
            self.source_constant_entry_size(key)
        } else {
            0
        };
        if self.len() >= MAX_ENVIRONMENT_ENTRIES && !self.constants.contains_key(key) {
            return false;
        }
        let Some(additional) = constant_additional.checked_add(provenance_additional) else {
            return false;
        };
        if self
            .bytes
            .checked_add(additional)
            .is_none_or(|bytes| bytes > MAX_ENVIRONMENT_BYTES)
        {
            return false;
        }
        self.insert_constant(key.to_owned(), value.clone());
        if provenance_additional > 0 {
            self.bytes += provenance_additional;
        }
        self.source_constants.insert(key.to_owned());
        true
    }

    /// Whether this environment carries any inherited DEFINE/UNDEF facts.
    pub fn has_facts(&self) -> bool {
        !self.values.is_empty()
            || !self.options.is_empty()
            || !self.constants.is_empty()
            || self.compiler_version.is_some()
    }

    /// Return a stable identity for the currently established facts.
    ///
    /// Consumers that cache stateful include analysis must include this value
    /// in their cache key: the same physical include can be reached with
    /// different inherited DEFINE/UNDEF environments.
    pub fn fingerprint(&self) -> u64 {
        let mut facts = self.values.iter().collect::<Vec<_>>();
        facts.sort_by(|left, right| left.0.cmp(right.0));
        let mut hasher = DefaultHasher::new();
        1_u8.hash(&mut hasher);
        facts.len().hash(&mut hasher);
        for (symbol, value) in facts {
            symbol.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        let mut options = self.options.iter().collect::<Vec<_>>();
        options.sort_by(|left, right| left.0.cmp(right.0));
        2_u8.hash(&mut hasher);
        options.len().hash(&mut hasher);
        for (option, value) in options {
            option.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        3_u8.hash(&mut hasher);
        self.constants.len().hash(&mut hasher);
        for (name, value) in &self.constants {
            name.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        4_u8.hash(&mut hasher);
        self.compiler_version.hash(&mut hasher);
        5_u8.hash(&mut hasher);
        self.source_constants.len().hash(&mut hasher);
        for name in &self.source_constants {
            name.hash(&mut hasher);
        }
        6_u8.hash(&mut hasher);
        self.rejected_context.hash(&mut hasher);
        hasher.finish()
    }

    fn len(&self) -> usize {
        self.values
            .len()
            .saturating_add(self.options.len())
            .saturating_add(self.constants.len())
            .saturating_add(usize::from(self.compiler_version.is_some()))
    }
    fn contains_key(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }
    fn get(&self, key: &str) -> Option<&Truth> {
        self.values.get(key)
    }

    fn option(&self, key: &str) -> Truth {
        self.options.get(key).copied().unwrap_or(Truth::Unknown)
    }

    fn constant(&self, key: &str) -> Option<&ConstantValue> {
        self.constants.get(key)
    }
    fn keys(&self) -> impl Iterator<Item = &String> {
        self.values.keys()
    }
    fn bytes(&self) -> usize {
        self.bytes
    }

    fn clear(&mut self) {
        self.values.clear();
        self.options.clear();
        self.clear_constants();
    }

    fn clear_constants(&mut self) {
        self.constants.clear();
        self.source_constants.clear();
        self.recompute_bytes();
    }

    fn insert(&mut self, key: String, value: Truth) {
        self.insert_value(key, value);
    }

    fn insert_option_value(&mut self, key: String, value: Truth) {
        self.insert_option(key, value);
    }

    fn insert_constant_value(&mut self, key: String, value: ConstantValue) {
        self.insert_constant(key, value);
    }

    fn remove_constant(&mut self, key: &str) {
        if let Some(value) = self.constants.remove(key) {
            self.bytes = self
                .bytes
                .saturating_sub(self.constant_entry_size(key, &value));
        }
        if self.source_constants.remove(key) {
            self.bytes = self
                .bytes
                .saturating_sub(self.source_constant_entry_size(key));
        }
    }

    fn remove_source_constants(&mut self) {
        let names = self.source_constants.iter().cloned().collect::<Vec<_>>();
        for name in names {
            self.remove_constant(&name);
        }
    }

    fn source_constants_fingerprint(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.source_constants.hash(&mut hasher);
        for name in &self.source_constants {
            if let Some(value) = self.constants.get(name) {
                value.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    fn recompute_bytes(&mut self) {
        let mut bytes = if self.compiler_version.is_some() {
            size_of::<CompilerVersion>()
        } else {
            0
        };
        for key in self.values.keys() {
            bytes = bytes.saturating_add(self.value_entry_size(key));
        }
        for key in self.options.keys() {
            bytes = bytes.saturating_add(self.value_entry_size(key));
        }
        for (key, value) in &self.constants {
            bytes = bytes.saturating_add(self.constant_entry_size(key, value));
        }
        for key in &self.source_constants {
            bytes = bytes.saturating_add(self.source_constant_entry_size(key));
        }
        self.bytes = bytes;
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

/// Analyze a source buffer with an explicit compiler/option/constant context.
///
/// The context is copied into the request-local environment.  DEFINE/UNDEF,
/// option switches, include transitions, and proven constants therefore flow
/// in source order without mutating the caller's project metadata.
pub fn analyze_with_context(source: &str, context: &ConditionalContext) -> ConditionalAnalysis {
    analyze_with_context_and_cancel(source, context, &NoCancellation)
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
    let context = ConditionalContext::from_defines(project_defines);
    analyze_with_context_and_cancel(source, &context, cancel)
}

/// Context-aware variant of [`analyze_with_context`] that polls cancellation.
pub fn analyze_with_context_and_cancel(
    source: &str,
    context: &ConditionalContext,
    cancel: &dyn CancellationToken,
) -> ConditionalAnalysis {
    analyze_inner(source, context, Some(cancel))
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

fn admit_context_entry(
    budget: &mut AnalysisBudget<'_>,
    key_len: usize,
    payload_bytes: usize,
) -> bool {
    if !budget.poll() {
        return false;
    }
    let Some(entry_bytes) = key_len.checked_add(payload_bytes) else {
        budget.exhausted = true;
        return false;
    };
    if entry_bytes > MAX_ENVIRONMENT_BYTES {
        budget.exhausted = true;
        return false;
    }
    budget.charge(key_len.max(1)) && budget.charge_bytes(entry_bytes)
}

fn analyze_inner(
    source: &str,
    context: &ConditionalContext,
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
    let mut environment = match initial_environment(context, &mut budget) {
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
    let initial_complete = !environment.rejected_context
        && environment.len() <= MAX_ENVIRONMENT_ENTRIES
        && environment.bytes() <= MAX_ENVIRONMENT_BYTES;
    analyze_lexed(
        source,
        lexed,
        environment,
        Some(cancel),
        Some(include),
        initial_complete,
    )
}

/// Context-aware include callback variant used by source expansion and audit
/// workers.  The mutable environment still carries inherited state between
/// adjacent include occurrences.
pub fn analyze_with_include_callback_context(
    source: &str,
    environment: &mut ConditionalEnvironment,
    cancel: &dyn CancellationToken,
    include: &mut dyn FnMut(
        &ConditionalDirective,
        &mut ConditionalEnvironment,
    ) -> IncludeTransition,
) -> ConditionalAnalysis {
    analyze_with_include_callback(source, environment, cancel, include)
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
    let mut active = if initial_complete {
        Truth::True
    } else {
        Truth::Unknown
    };
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
        if !observe_source_constants(
            source,
            cursor..raw.start,
            active,
            environment,
            &mut complete,
            &mut budget,
        ) {
            complete = false;
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
            condition: None,
        });

        match kind {
            DirectiveKind::ConditionalStart => {
                if frames.len() >= MAX_CONDITIONAL_DEPTH {
                    complete = false;
                    active = Truth::Unknown;
                } else {
                    let condition = evaluate_condition(&raw.body, environment, &mut complete);
                    if let Some(directive) = directives.last_mut() {
                        directive.condition = Some(condition);
                    }
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
                    if let Some(directive) = directives.last_mut() {
                        directive.condition = Some(condition);
                    }
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
                        let source_constants_before = environment.source_constants_fingerprint();
                        let transition = include(
                            directives.last().expect("include directive was recorded"),
                            environment,
                        );
                        if !transition.complete {
                            complete = false;
                        }
                        if !transition.environment_known {
                            environment.clear();
                        } else if environment.source_constants_fingerprint()
                            != source_constants_before
                        {
                            // A source-derived constant may belong to a local
                            // include/routine scope.  It is not safe to carry
                            // it back to the including frame without a full
                            // binding identity proof.
                            environment.remove_source_constants();
                        }
                    } else {
                        environment.clear();
                    }
                } else if active == Truth::Unknown {
                    environment.clear();
                }
            }
            DirectiveKind::MethodInfo | DirectiveKind::Harmless | DirectiveKind::Other => {
                if active != Truth::False
                    && is_option_directive(&raw.body)
                    && !apply_option_directive(environment, &raw.body, active, &mut budget)
                {
                    complete = false;
                }
            }
        }
        cursor = raw.end;
    }

    if cursor < source.len()
        && !observe_source_constants(
            source,
            cursor..source.len(),
            active,
            environment,
            &mut complete,
            &mut budget,
        )
    {
        complete = false;
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
    context: &ConditionalContext,
    budget: &mut AnalysisBudget<'_>,
) -> Option<ConditionalEnvironment> {
    let environment = ConditionalEnvironment::try_from_context_with_budget(context, budget)?;
    if !budget.check_environment_bytes(environment.bytes())
        || environment.len() > MAX_ENVIRONMENT_ENTRIES
        || !budget.charge_bytes(environment.bytes())
    {
        budget.exhausted = true;
        return None;
    }
    Some(environment)
}

fn canonical_symbol(symbol: &str) -> Option<String> {
    let symbol = canonical_symbol_ref(symbol)?;
    Some(symbol.to_ascii_uppercase())
}

fn canonical_symbol_ref(symbol: &str) -> Option<&str> {
    let symbol = symbol.trim().trim_start_matches('&');
    let mut bytes = symbol.bytes();
    let first = bytes.next()?;
    if (!first.is_ascii_alphabetic() && first != b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.')
    {
        return None;
    }
    Some(symbol)
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
        // The current environment is the environment under the branch
        // assumption.  Reachability uncertainty is handled by the enclosing
        // frame's exit merge; merging here would lose facts equal on every
        // unknown branch.
        Truth::Unknown => value,
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

fn apply_option_fact(
    environment: &mut ConditionalEnvironment,
    option: &str,
    value: Truth,
    activity: Truth,
    budget: &mut AnalysisBudget<'_>,
) -> bool {
    if !budget.charge(1) {
        return false;
    }
    let key = match canonical_option_name(option) {
        Some(key) => key,
        None => return false,
    };
    let previous = environment.option(&key);
    let next = match activity {
        Truth::True => value,
        Truth::False => previous,
        Truth::Unknown => value,
    };
    if !budget.charge_bytes(key.len()) {
        return false;
    }
    if !environment.options.contains_key(&key) && environment.len() >= MAX_ENVIRONMENT_ENTRIES {
        budget.exhausted = true;
        return false;
    }
    let bytes = environment
        .bytes()
        .saturating_add(key.len().saturating_add(size_of::<Truth>()));
    if !environment.options.contains_key(&key) && !budget.check_environment_bytes(bytes) {
        return false;
    }
    environment.insert_option_value(key, next);
    true
}

fn constant_size(value: &ConstantValue) -> usize {
    match value {
        ConstantValue::Boolean(_) => 1,
        ConstantValue::Integer(_) => size_of::<i64>(),
        ConstantValue::String(value) => value.len(),
        ConstantValue::Version(_) => size_of::<CompilerVersion>(),
    }
}

/// Whether a directive is a state-changing compiler option that the bounded
/// evaluator can track and downstream projections must replace or mask.
pub fn is_option_directive(body: &str) -> bool {
    let Some(keyword) = directive_keyword(body) else {
        return false;
    };
    let keyword = keyword.to_ascii_lowercase();
    if matches!(
        keyword.as_str(),
        "if" | "ifdef" | "ifndef" | "ifopt" | "else" | "elseif" | "elif" | "endif"
    ) {
        return false;
    }
    let base = keyword.trim_end_matches(['+', '-']);
    is_harmless_keyword(base)
        || supported_option_name(base).is_some()
        || keyword.ends_with(['+', '-'])
        || state_value_word(directive_arguments(body))
}

fn state_value_word(argument: &str) -> bool {
    argument
        .split_ascii_whitespace()
        .next()
        .is_some_and(|value| {
            value.eq_ignore_ascii_case("on")
                || value.eq_ignore_ascii_case("off")
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("yes")
                || value.eq_ignore_ascii_case("no")
                || value == "+"
                || value == "-"
        })
}

fn supported_option_name(name: &str) -> Option<String> {
    let name = canonical_option_name(name)?;
    matches!(
        name.as_str(),
        "R" | "O" | "Q" | "C" | "B" | "I" | "X" | "T" | "D" | "Y" | "RUNTIME_CHECKS"
    )
    .then_some(name)
}

fn apply_option_directive(
    environment: &mut ConditionalEnvironment,
    body: &str,
    activity: Truth,
    budget: &mut AnalysisBudget<'_>,
) -> bool {
    let Some(keyword) = directive_keyword(body) else {
        return false;
    };
    let argument = directive_arguments(body).trim();
    let (name, suffix_value) = keyword
        .strip_suffix('+')
        .map(|name| (name, Some(Truth::True)))
        .or_else(|| {
            keyword
                .strip_suffix('-')
                .map(|name| (name, Some(Truth::False)))
        })
        .unwrap_or((keyword, None));
    let malformed_suffix = name.ends_with(['+', '-']);
    let normalized_name = name.trim_end_matches(['+', '-']);
    let Some(canonical_name) = supported_option_name(normalized_name) else {
        if suffix_value.is_some() || state_value_word(argument) {
            if let Some(canonical_name) = canonical_option_name(normalized_name) {
                return apply_option_fact(
                    environment,
                    &canonical_name,
                    Truth::Unknown,
                    activity,
                    budget,
                );
            }
        }
        // Many harmless compiler directives carry a directive-specific value
        // rather than an on/off switch (for example `WARN SYMBOL_DEPRECATED
        // OFF`, `MESSAGE ERROR 'text'`, or `APPTYPE CONSOLE`).  They do not
        // establish a fact consumed by the conditional evaluator.
        return is_harmless_keyword(&normalized_name.to_ascii_lowercase());
    };
    let value = if malformed_suffix {
        Truth::Unknown
    } else {
        match suffix_value {
            Some(value) if argument.is_empty() => value,
            Some(_) => Truth::Unknown,
            None => {
                let mut words = argument.split_ascii_whitespace();
                match (words.next(), words.next()) {
                    (Some(value), None)
                        if value.eq_ignore_ascii_case("on")
                            || value.eq_ignore_ascii_case("true")
                            || value.eq_ignore_ascii_case("+")
                            || value.eq_ignore_ascii_case("yes") =>
                    {
                        Truth::True
                    }
                    (Some(value), None)
                        if value.eq_ignore_ascii_case("off")
                            || value.eq_ignore_ascii_case("false")
                            || value.eq_ignore_ascii_case("-")
                            || value.eq_ignore_ascii_case("no") =>
                    {
                        Truth::False
                    }
                    _ => Truth::Unknown,
                }
            }
        }
    };
    apply_option_fact(environment, &canonical_name, value, activity, budget)
}

fn observe_source_constants(
    source: &str,
    range: Range<usize>,
    activity: Truth,
    environment: &mut ConditionalEnvironment,
    complete: &mut bool,
    budget: &mut AnalysisBudget<'_>,
) -> bool {
    if activity == Truth::False || range.start >= range.end {
        return true;
    }
    let Some(text) = source.get(range.clone()) else {
        *complete = false;
        return false;
    };
    let bytes = text.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if !budget.charge(1) {
            return false;
        }
        let Some((keyword_start, keyword_end)) = next_source_identifier(bytes, cursor) else {
            break;
        };
        cursor = keyword_end;
        let identifier = &text[keyword_start..keyword_end];
        if is_unsupported_scope_boundary(identifier) {
            // This bounded scanner cannot prove bindings inside a routine,
            // class, record, or block.  Drop every inherited constant rather
            // than allowing a same-named local declaration to reuse a global
            // fact (including constants supplied explicitly by the client).
            environment.clear_constants();
            continue;
        }
        if !identifier.eq_ignore_ascii_case("const") {
            continue;
        }
        let Some(is_global) = source_constant_is_provably_global(
            source,
            range.start.saturating_add(keyword_start),
            budget,
        ) else {
            return false;
        };
        if !is_global {
            // A local/class/record declaration can shadow an already-known
            // unit fact.  Without binding identity, invalidate all constants,
            // including explicit context values, instead of retaining the
            // outer binding through an unsupported scope.
            environment.clear_constants();
            continue;
        }
        let mut declaration = keyword_end;
        loop {
            skip_source_space_and_comments(bytes, &mut declaration);
            let Some((name_start, name_end)) = next_source_identifier(bytes, declaration) else {
                break;
            };
            declaration = name_end;
            skip_source_space_and_comments(bytes, &mut declaration);
            if bytes.get(declaration) == Some(&b':') {
                // Typed declarations without an initializer do not prove a
                // value; stop this const block rather than guessing its extent.
                break;
            }
            if bytes.get(declaration) != Some(&b'=') {
                break;
            }
            declaration += 1;
            let Some(end) = find_source_semicolon(bytes, declaration) else {
                *complete = false;
                return false;
            };
            let expression = &text[declaration..end];
            let mut expression_complete = true;
            let value =
                evaluate_typed_expression(expression, environment, &mut expression_complete);
            // An unsupported source-constant initializer is simply not an
            // admitted fact.  It is not a malformed conditional directive,
            // so it must not invalidate otherwise usable parser projection
            // for the entire source buffer.  Stop this conservative const
            // block as well: after an unsupported initializer we no longer
            // have enough grammar context to distinguish another constant
            // declarator from a recovered Pascal declaration.
            if !expression_complete {
                break;
            }
            if let Some(value) = value.to_constant() {
                let Some(name) = canonical_symbol(&text[name_start..name_end]) else {
                    *complete = false;
                    return false;
                };
                let provenance_bytes = if !environment.source_constants.contains(&name) {
                    environment.source_constant_entry_size(&name)
                } else {
                    0
                };
                if !budget.charge_bytes(
                    name.len()
                        .saturating_add(size_of::<ConstantValue>())
                        .saturating_add(constant_size(&value)),
                ) {
                    return false;
                }
                if !budget.charge_bytes(provenance_bytes)
                    || !environment.try_insert_source_constant(&name, &value)
                {
                    budget.exhausted = true;
                    return false;
                }
            }
            declaration = end.saturating_add(1);
        }
    }
    true
}

/// Source constants are only admitted when the declaration is in the
/// conservative unit/program-level subset. A full Pascal scope resolver does
/// not belong in this bounded directive evaluator: once a routine or block
/// introducer has appeared, a later `const` may be local and must not be
/// treated as a compiler-wide fact.
fn source_constant_is_provably_global(
    source: &str,
    keyword_start: usize,
    budget: &mut AnalysisBudget<'_>,
) -> Option<bool> {
    let Some(prefix) = source.get(..keyword_start) else {
        return Some(false);
    };
    if !budget.charge(prefix.len()) || !budget.charge_bytes(prefix.len()) {
        return None;
    }
    let bytes = prefix.as_bytes();
    let mut cursor = 0;
    while let Some((start, end)) = next_source_identifier(bytes, cursor) {
        if !budget.poll() {
            return None;
        }
        cursor = end;
        let identifier = &prefix[start..end];
        if identifier.eq_ignore_ascii_case("procedure")
            || identifier.eq_ignore_ascii_case("function")
            || identifier.eq_ignore_ascii_case("constructor")
            || identifier.eq_ignore_ascii_case("destructor")
            || identifier.eq_ignore_ascii_case("operator")
            || identifier.eq_ignore_ascii_case("begin")
            || identifier.eq_ignore_ascii_case("class")
            || identifier.eq_ignore_ascii_case("record")
        {
            return Some(false);
        }
    }
    Some(true)
}

fn is_unsupported_scope_boundary(identifier: &str) -> bool {
    identifier.eq_ignore_ascii_case("procedure")
        || identifier.eq_ignore_ascii_case("function")
        || identifier.eq_ignore_ascii_case("constructor")
        || identifier.eq_ignore_ascii_case("destructor")
        || identifier.eq_ignore_ascii_case("operator")
        || identifier.eq_ignore_ascii_case("begin")
        || identifier.eq_ignore_ascii_case("class")
        || identifier.eq_ignore_ascii_case("record")
}

fn next_source_identifier(bytes: &[u8], mut cursor: usize) -> Option<(usize, usize)> {
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' => cursor = skip_string(bytes, cursor)?,
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor)
            }
            b'{' => cursor = find_byte(bytes, cursor + 1, b'}')?.saturating_add(1),
            b'(' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = find_sequence(bytes, cursor + 2, b"*)")?.saturating_add(2)
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len()
                    && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
                {
                    cursor += 1;
                }
                return Some((start, cursor));
            }
            _ => cursor += 1,
        }
    }
    None
}

fn skip_source_space_and_comments(bytes: &[u8], cursor: &mut usize) {
    while *cursor < bytes.len() {
        if bytes[*cursor].is_ascii_whitespace() {
            *cursor += 1;
        } else if bytes[*cursor] == b'/' && bytes.get(*cursor + 1) == Some(&b'/') {
            *cursor = skip_line_comment(bytes, *cursor);
        } else if bytes[*cursor] == b'{' {
            let Some(close) = find_byte(bytes, *cursor + 1, b'}') else {
                *cursor = bytes.len();
                break;
            };
            *cursor = close + 1;
        } else if bytes[*cursor] == b'(' && bytes.get(*cursor + 1) == Some(&b'*') {
            let Some(close) = find_sequence(bytes, *cursor + 2, b"*)") else {
                *cursor = bytes.len();
                break;
            };
            *cursor = close + 2;
        } else {
            break;
        }
    }
}

fn find_source_semicolon(bytes: &[u8], mut cursor: usize) -> Option<usize> {
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' => cursor = skip_string(bytes, cursor)?,
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor)
            }
            b'{' => cursor = find_byte(bytes, cursor + 1, b'}')?.saturating_add(1),
            b'(' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = find_sequence(bytes, cursor + 2, b"*)")?.saturating_add(2)
            }
            b';' => return Some(cursor),
            _ => cursor += 1,
        }
    }
    None
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
    let mut keys = current.keys().cloned().collect::<Vec<_>>();
    keys.extend(incoming.keys().cloned());
    keys.sort();
    keys.dedup();
    let mut option_keys = current.options.keys().cloned().collect::<Vec<_>>();
    option_keys.extend(incoming.options.keys().cloned());
    option_keys.sort();
    option_keys.dedup();
    let mut constant_keys = current.constants.keys().cloned().collect::<Vec<_>>();
    constant_keys.extend(incoming.constants.keys().cloned());
    constant_keys.sort();
    constant_keys.dedup();

    let mut merged_bytes = current
        .compiler_version
        .zip(incoming.compiler_version)
        .filter(|(left, right)| left == right)
        .map(|_| size_of::<CompilerVersion>())
        .unwrap_or(0);
    for key in &keys {
        let Some(bytes) = merged_bytes.checked_add(current.value_entry_size(key)) else {
            budget.exhausted = true;
            return false;
        };
        merged_bytes = bytes;
    }
    for key in &option_keys {
        let Some(bytes) = merged_bytes.checked_add(current.value_entry_size(key)) else {
            budget.exhausted = true;
            return false;
        };
        merged_bytes = bytes;
    }
    for key in &constant_keys {
        if let (Some(left), Some(right)) = (current.constants.get(key), incoming.constants.get(key))
        {
            if left == right {
                let Some(bytes) = merged_bytes.checked_add(current.constant_entry_size(key, left))
                else {
                    budget.exhausted = true;
                    return false;
                };
                merged_bytes = bytes;
            }
        }
    }
    for name in &current.source_constants {
        if current.constants.get(name) == incoming.constants.get(name)
            && current.constants.contains_key(name)
        {
            let Some(bytes) = merged_bytes.checked_add(current.source_constant_entry_size(name))
            else {
                budget.exhausted = true;
                return false;
            };
            merged_bytes = bytes;
        }
    }
    for name in &incoming.source_constants {
        if !current.source_constants.contains(name)
            && current.constants.get(name) == incoming.constants.get(name)
            && current.constants.contains_key(name)
        {
            let Some(bytes) = merged_bytes.checked_add(current.source_constant_entry_size(name))
            else {
                budget.exhausted = true;
                return false;
            };
            merged_bytes = bytes;
        }
    }
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

    for key in keys {
        let left = current.get(&key).copied().unwrap_or(Truth::Unknown);
        let right = incoming.get(&key).copied().unwrap_or(Truth::Unknown);
        current.insert(key, left.merge(right));
    }
    for key in option_keys {
        let left = current.options.get(&key).copied().unwrap_or(Truth::Unknown);
        let right = incoming
            .options
            .get(&key)
            .copied()
            .unwrap_or(Truth::Unknown);
        current.insert_option_value(key, left.merge(right));
    }
    for key in constant_keys {
        let value = match (current.constants.get(&key), incoming.constants.get(&key)) {
            (Some(left), Some(right)) if left == right => Some(left.clone()),
            _ => None,
        };
        if let Some(value) = value {
            current.insert_constant_value(key, value);
        } else {
            current.remove_constant(&key);
        }
    }
    current.compiler_version = if current.compiler_version == incoming.compiler_version {
        current.compiler_version
    } else {
        None
    };
    current
        .source_constants
        .extend(incoming.source_constants.iter().cloned());
    current
        .source_constants
        .retain(|name| current.constants.contains_key(name));
    current.recompute_bytes();
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
        "ifopt" => {
            let argument = arguments.trim();
            let mut characters = argument.chars();
            let last = match characters.next_back() {
                Some(last) => last,
                None => {
                    *complete = false;
                    return Truth::Unknown;
                }
            };
            if !matches!(last, '+' | '-') {
                *complete = false;
                return Truth::Unknown;
            }
            let option = characters.as_str().trim();
            let Some(option) = supported_option_name(option) else {
                *complete = false;
                return Truth::Unknown;
            };
            let value = environment.option(&option);
            match last {
                '+' => value,
                '-' => value.not(),
                _ => unreachable!("IFOPT suffix was checked above"),
            }
        }
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
    evaluate_typed_expression(expression, environment, complete).as_truth()
}

fn evaluate_typed_expression(
    expression: &str,
    environment: &ConditionalEnvironment,
    complete: &mut bool,
) -> Value {
    if expression.len() > MAX_EXPRESSION_BYTES {
        *complete = false;
        return Value::Unknown;
    }
    let tokens = tokenize_expression(expression);
    if tokens.len() > MAX_EXPRESSION_TOKENS {
        *complete = false;
        return Value::Unknown;
    }
    let mut parser = ExpressionParser {
        tokens,
        index: 0,
        environment,
        malformed: false,
        depth: 0,
        work: 0,
    };
    let result = parser.parse_comparison();
    if !matches!(parser.peek(), ExprToken::End) {
        parser.malformed = true;
    }
    if parser.malformed || parser.work > MAX_EXPRESSION_TOKENS {
        *complete = false;
        return Value::Error;
    }
    if matches!(&result, Value::Error) {
        *complete = false;
    }
    result
}

#[derive(Debug, Clone, PartialEq)]
enum ExprToken {
    Identifier(String),
    Number(i64),
    Version(CompilerVersion),
    String(String),
    LParen,
    RParen,
    Comma,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
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
            b',' => {
                tokens.push(ExprToken::Comma);
                index += 1;
            }
            b'+' => {
                tokens.push(ExprToken::Plus);
                index += 1;
            }
            b'-' => {
                tokens.push(ExprToken::Minus);
                index += 1;
            }
            b'*' => {
                tokens.push(ExprToken::Star);
                index += 1;
            }
            b'/' => {
                tokens.push(ExprToken::Slash);
                index += 1;
            }
            b'%' => {
                tokens.push(ExprToken::Percent);
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
                if bytes.get(index) == Some(&b'.')
                    && bytes
                        .get(index + 1)
                        .is_some_and(|byte| byte.is_ascii_digit())
                {
                    index += 1;
                    while index < bytes.len() && bytes[index].is_ascii_digit() {
                        index += 1;
                    }
                    let value = CompilerVersion::parse(&expression[start..index]);
                    tokens.push(value.map_or(ExprToken::Invalid, ExprToken::Version));
                } else {
                    let value = expression[start..index].parse::<i64>().ok();
                    tokens.push(value.map_or(ExprToken::Invalid, ExprToken::Number));
                }
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
    depth: usize,
    work: usize,
}

impl ExpressionParser<'_> {
    fn peek(&self) -> &ExprToken {
        self.tokens.get(self.index).unwrap_or(&ExprToken::End)
    }

    fn take(&mut self) -> ExprToken {
        let token = self.peek().clone();
        self.index = self.index.saturating_add(1);
        self.work = self.work.saturating_add(1);
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

    fn parse_additive(&mut self) -> Value {
        let mut value = self.parse_multiplicative();
        loop {
            let operator = match self.peek() {
                ExprToken::Plus => Some(BinaryAdditive::Add),
                ExprToken::Minus => Some(BinaryAdditive::Subtract),
                ExprToken::Identifier(name) if name.eq_ignore_ascii_case("or") => {
                    Some(BinaryAdditive::Or)
                }
                ExprToken::Identifier(name) if name.eq_ignore_ascii_case("xor") => {
                    Some(BinaryAdditive::Xor)
                }
                _ => None,
            };
            let Some(operator) = operator else {
                break;
            };
            self.take();
            let right = self.parse_multiplicative();
            value = match operator {
                BinaryAdditive::Add => value.add(right),
                BinaryAdditive::Subtract => value.subtract(right),
                BinaryAdditive::Or => value.or(right),
                BinaryAdditive::Xor => value.xor(right),
            };
        }
        value
    }

    fn parse_multiplicative(&mut self) -> Value {
        let mut value = self.parse_unary();
        loop {
            let operator = match self.peek() {
                ExprToken::Star => Some(BinaryArithmetic::Multiply),
                ExprToken::Slash => Some(BinaryArithmetic::RealDivide),
                ExprToken::Percent => Some(BinaryArithmetic::Remainder),
                ExprToken::Identifier(name) if name.eq_ignore_ascii_case("div") => {
                    Some(BinaryArithmetic::Divide)
                }
                ExprToken::Identifier(name) if name.eq_ignore_ascii_case("mod") => {
                    Some(BinaryArithmetic::Remainder)
                }
                ExprToken::Identifier(name) if name.eq_ignore_ascii_case("and") => {
                    Some(BinaryArithmetic::And)
                }
                ExprToken::Identifier(name) if name.eq_ignore_ascii_case("shl") => {
                    Some(BinaryArithmetic::ShiftLeft)
                }
                ExprToken::Identifier(name) if name.eq_ignore_ascii_case("shr") => {
                    Some(BinaryArithmetic::ShiftRight)
                }
                _ => None,
            };
            let Some(operator) = operator else {
                break;
            };
            self.take();
            let right = self.parse_unary();
            value = value.arithmetic(right, operator);
        }
        value
    }

    fn parse_unary(&mut self) -> Value {
        if self.consume_identifier("not") {
            self.parse_unary().not()
        } else if matches!(self.peek(), ExprToken::Plus) {
            self.take();
            match self.parse_unary() {
                Value::Number(value) => Value::Number(value),
                Value::Unknown => Value::Unknown,
                Value::Error => Value::Error,
                _ => Value::Error,
            }
        } else if matches!(self.peek(), ExprToken::Minus) {
            self.take();
            self.parse_unary().negate()
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> Value {
        if matches!(self.peek(), ExprToken::LParen) {
            self.depth = self.depth.saturating_add(1);
            if self.depth > MAX_CONDITIONAL_DEPTH {
                self.malformed = true;
                return Value::Unknown;
            }
            self.take();
            let value = self.parse_comparison();
            if !matches!(self.peek(), ExprToken::RParen) {
                self.malformed = true;
            } else {
                self.take();
            }
            self.depth = self.depth.saturating_sub(1);
            value
        } else {
            self.parse_value()
        }
    }

    fn parse_comparison(&mut self) -> Value {
        let mut left = self.parse_additive();
        if self.consume_identifier("is") || self.consume_identifier("as") {
            let _ = self.parse_additive();
            return Value::Error;
        }
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
        let right = self.parse_additive();
        left = compare_values(left, right, operator);
        left
    }

    fn parse_value(&mut self) -> Value {
        match self.take() {
            ExprToken::Identifier(identifier) if identifier.eq_ignore_ascii_case("defined") => {
                if !matches!(self.peek(), ExprToken::LParen) {
                    self.malformed = true;
                    return Value::Error;
                }
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
                    .map(|symbol| Value::Truth(environment_value(self.environment, &symbol)))
                    .unwrap_or_else(|| {
                        self.malformed = true;
                        Value::Unknown
                    })
            }
            ExprToken::Identifier(identifier) if identifier.eq_ignore_ascii_case("declared") => {
                let symbol = self.parse_function_identifier();
                symbol
                    .map(|symbol| {
                        Value::Truth(if self.environment.constant(&symbol).is_some() {
                            Truth::True
                        } else {
                            Truth::Unknown
                        })
                    })
                    .unwrap_or_else(|| {
                        self.malformed = true;
                        Value::Unknown
                    })
            }
            ExprToken::Identifier(identifier)
                if identifier.eq_ignore_ascii_case("compilerVersion") =>
            {
                self.environment
                    .compiler_version
                    .map(Value::Version)
                    .unwrap_or(Value::Unknown)
            }
            ExprToken::Identifier(identifier) if identifier.eq_ignore_ascii_case("true") => {
                Value::Truth(Truth::True)
            }
            ExprToken::Identifier(identifier) if identifier.eq_ignore_ascii_case("false") => {
                Value::Truth(Truth::False)
            }
            ExprToken::Identifier(identifier) => {
                if matches!(self.peek(), ExprToken::LParen) {
                    self.take();
                    let mut argument = None;
                    let mut arity = 0usize;
                    if !matches!(self.peek(), ExprToken::RParen) {
                        loop {
                            let value = self.parse_comparison();
                            if argument.is_none() {
                                argument = Some(value);
                            }
                            arity = arity.saturating_add(1);
                            if !matches!(self.peek(), ExprToken::Comma) {
                                break;
                            }
                            self.take();
                        }
                    }
                    if !matches!(self.peek(), ExprToken::RParen) {
                        self.malformed = true;
                    } else {
                        self.take();
                    }
                    if arity != 1 {
                        self.malformed = true;
                        return Value::Error;
                    }
                    match identifier.to_ascii_lowercase().as_str() {
                        "length" => match argument {
                            Some(Value::String(value)) if value.is_ascii() => {
                                i64::try_from(value.len())
                                    .map(Value::Number)
                                    .unwrap_or(Value::Error)
                            }
                            Some(Value::Unknown) => Value::Unknown,
                            _ => Value::Error,
                        },
                        "ord" => match argument {
                            Some(Value::String(value)) if value.is_ascii() => {
                                let mut chars = value.chars();
                                match (chars.next(), chars.next()) {
                                    (Some(character), None) => {
                                        Value::Number(i64::from(u32::from(character)))
                                    }
                                    _ => Value::Error,
                                }
                            }
                            Some(Value::Truth(Truth::True)) => Value::Number(1),
                            Some(Value::Truth(Truth::False)) => Value::Number(0),
                            Some(Value::Truth(Truth::Unknown)) | Some(Value::Unknown) => {
                                Value::Unknown
                            }
                            _ => Value::Error,
                        },
                        // No type/width information is carried by the
                        // bounded context, so SizeOf is never a proven fact.
                        "sizeof" => Value::Error,
                        _ => Value::Error,
                    }
                } else {
                    let key = canonical_symbol(&identifier);
                    key.and_then(|key| {
                        self.environment
                            .constant(&key)
                            .cloned()
                            .map(Value::from_constant)
                            .or_else(|| {
                                self.environment.values.get(&key).copied().map(Value::Truth)
                            })
                    })
                    .unwrap_or(Value::Unknown)
                }
            }
            ExprToken::Number(value) => Value::Number(value),
            ExprToken::Version(value) => Value::Version(value),
            ExprToken::String(value) => Value::String(value),
            ExprToken::Invalid | ExprToken::End => {
                self.malformed = true;
                Value::Error
            }
            ExprToken::LParen
            | ExprToken::RParen
            | ExprToken::Comma
            | ExprToken::Plus
            | ExprToken::Minus
            | ExprToken::Star
            | ExprToken::Slash
            | ExprToken::Percent
            | ExprToken::Equal
            | ExprToken::NotEqual
            | ExprToken::Less
            | ExprToken::LessEqual
            | ExprToken::Greater
            | ExprToken::GreaterEqual => {
                self.malformed = true;
                Value::Error
            }
        }
    }

    fn parse_function_identifier(&mut self) -> Option<String> {
        if !matches!(self.peek(), ExprToken::LParen) {
            self.malformed = true;
            return None;
        }
        self.take();
        let value = match self.take() {
            ExprToken::Identifier(value) => Some(canonical_symbol(&value)?),
            _ => None,
        };
        if !matches!(self.peek(), ExprToken::RParen) {
            self.malformed = true;
        } else {
            self.take();
        }
        value
    }
}

#[derive(Debug, Clone)]
enum Value {
    Truth(Truth),
    Number(i64),
    String(String),
    Version(CompilerVersion),
    Unknown,
    Error,
}

impl Value {
    fn from_constant(value: ConstantValue) -> Self {
        match value {
            ConstantValue::Boolean(value) => {
                Self::Truth(if value { Truth::True } else { Truth::False })
            }
            ConstantValue::Integer(value) => Self::Number(value),
            ConstantValue::String(value) => Self::String(value),
            ConstantValue::Version(value) => Self::Version(value),
        }
    }

    fn to_constant(&self) -> Option<ConstantValue> {
        match self {
            Self::Truth(Truth::True) => Some(ConstantValue::Boolean(true)),
            Self::Truth(Truth::False) => Some(ConstantValue::Boolean(false)),
            Self::Truth(Truth::Unknown) | Self::Unknown | Self::Error => None,
            Self::Number(value) => Some(ConstantValue::Integer(*value)),
            Self::String(value) => Some(ConstantValue::String(value.clone())),
            Self::Version(value) => Some(ConstantValue::Version(*value)),
        }
    }

    fn as_truth(&self) -> Truth {
        match self {
            Self::Truth(value) => *value,
            Self::Number(_) | Self::String(_) | Self::Version(_) | Self::Unknown | Self::Error => {
                Truth::Unknown
            }
        }
    }

    fn logical_truth(&self) -> Option<Truth> {
        match self {
            Self::Truth(value) => Some(*value),
            Self::Unknown => Some(Truth::Unknown),
            Self::Number(_) | Self::String(_) | Self::Version(_) => None,
            Self::Error => None,
        }
    }

    fn and(self, other: Self) -> Self {
        if matches!(self, Self::Error) || matches!(other, Self::Error) {
            return Self::Error;
        }
        match (self, other) {
            (Self::Truth(left), Self::Truth(right)) => Self::Truth(left.and(right)),
            (Self::Number(left), Self::Number(right)) => Self::Number(left & right),
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (left, right) => match (left.logical_truth(), right.logical_truth()) {
                (Some(left), Some(right)) => Self::Truth(left.and(right)),
                _ => Self::Error,
            },
        }
    }

    fn or(self, other: Self) -> Self {
        if matches!(self, Self::Error) || matches!(other, Self::Error) {
            return Self::Error;
        }
        match (self, other) {
            (Self::Truth(left), Self::Truth(right)) => Self::Truth(left.or(right)),
            (Self::Number(left), Self::Number(right)) => Self::Number(left | right),
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (left, right) => match (left.logical_truth(), right.logical_truth()) {
                (Some(left), Some(right)) => Self::Truth(left.or(right)),
                _ => Self::Error,
            },
        }
    }

    fn xor(self, other: Self) -> Self {
        if matches!(self, Self::Error) || matches!(other, Self::Error) {
            return Self::Error;
        }
        match (self, other) {
            (Self::Number(left), Self::Number(right)) => Self::Number(left ^ right),
            (Self::Truth(left), Self::Truth(right)) => Self::Truth(match (left, right) {
                (Truth::True, Truth::False) | (Truth::False, Truth::True) => Truth::True,
                (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
                _ => Truth::False,
            }),
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            _ => Self::Error,
        }
    }

    fn add(self, other: Self) -> Self {
        if matches!(self, Self::Error) || matches!(other, Self::Error) {
            return Self::Error;
        }
        match (self, other) {
            (Self::Number(left), Self::Number(right)) => left
                .checked_add(right)
                .map(Self::Number)
                .unwrap_or(Self::Error),
            (Self::String(left), Self::String(right)) => {
                if left.len().saturating_add(right.len()) > MAX_EXPRESSION_BYTES {
                    Self::Error
                } else {
                    Self::String(format!("{left}{right}"))
                }
            }
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            _ => Self::Error,
        }
    }

    fn subtract(self, other: Self) -> Self {
        if matches!(self, Self::Error) || matches!(other, Self::Error) {
            return Self::Error;
        }
        match (self, other) {
            (Self::Number(left), Self::Number(right)) => left
                .checked_sub(right)
                .map(Self::Number)
                .unwrap_or(Self::Error),
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            _ => Self::Error,
        }
    }

    fn negate(self) -> Self {
        match self {
            Self::Number(value) => value.checked_neg().map(Self::Number).unwrap_or(Self::Error),
            Self::Error => Self::Error,
            Self::Unknown => Self::Unknown,
            _ => Self::Error,
        }
    }

    fn arithmetic(self, other: Self, operator: BinaryArithmetic) -> Self {
        if matches!(self, Self::Error) || matches!(other, Self::Error) {
            return Self::Error;
        }
        if matches!(operator, BinaryArithmetic::And) {
            return self.and(other);
        }
        let (Self::Number(left), Self::Number(right)) = (self, other) else {
            return Self::Error;
        };
        match operator {
            BinaryArithmetic::And => unreachable!("logical and is handled before arithmetic"),
            BinaryArithmetic::Multiply => left
                .checked_mul(right)
                .map(Self::Number)
                .unwrap_or(Self::Error),
            BinaryArithmetic::Divide => {
                if right == 0 {
                    Self::Error
                } else {
                    left.checked_div(right)
                        .map(Self::Number)
                        .unwrap_or(Self::Error)
                }
            }
            // Delphi's `/` operator is real division. Real values are outside
            // this evaluator's admitted constant subset, so never coerce it
            // to integer division.
            BinaryArithmetic::RealDivide => Self::Unknown,
            BinaryArithmetic::Remainder => {
                if right == 0 {
                    Self::Error
                } else {
                    left.checked_rem(right)
                        .map(Self::Number)
                        .unwrap_or(Self::Error)
                }
            }
            BinaryArithmetic::ShiftLeft => u32::try_from(right)
                .ok()
                .and_then(|shift| left.checked_shl(shift))
                .map(Self::Number)
                .unwrap_or(Self::Error),
            BinaryArithmetic::ShiftRight => u32::try_from(right)
                .ok()
                .filter(|shift| *shift < 64)
                .map(|shift| Self::Number(left >> shift))
                .unwrap_or(Self::Error),
        }
    }

    fn not(self) -> Self {
        match self {
            Self::Truth(value) => Self::Truth(value.not()),
            Self::Number(value) => Self::Number(!value),
            Self::String(_) | Self::Version(_) => Self::Error,
            Self::Unknown => Self::Unknown,
            Self::Error => Self::Error,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum BinaryAdditive {
    Add,
    Subtract,
    Or,
    Xor,
}

#[derive(Debug, Clone, Copy)]
enum BinaryArithmetic {
    Multiply,
    Divide,
    RealDivide,
    Remainder,
    And,
    ShiftLeft,
    ShiftRight,
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

fn compare_values(left: Value, right: Value, comparison: Comparison) -> Value {
    if matches!(left, Value::Error) || matches!(right, Value::Error) {
        return Value::Error;
    }
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            Value::Truth(compare_order(left, right, comparison))
        }
        (Value::String(left), Value::String(right)) => {
            Value::Truth(compare_order(left, right, comparison))
        }
        (Value::Version(left), Value::Version(right)) => left
            .cmp_numeric(right)
            .map(|ordering| Value::Truth(compare_ordering(ordering, comparison)))
            .unwrap_or(Value::Error),
        (Value::Version(left), Value::Number(right)) => left
            .cmp_integer(right)
            .map(|ordering| Value::Truth(compare_ordering(ordering, comparison)))
            .unwrap_or(Value::Error),
        (Value::Number(left), Value::Version(right)) => right
            .cmp_integer(left)
            .map(|ordering| Value::Truth(compare_ordering(ordering.reverse(), comparison)))
            .unwrap_or(Value::Error),
        (Value::Truth(left), Value::Truth(right)) => {
            Value::Truth(compare_truths(left, right, comparison))
        }
        (Value::Unknown, _) | (_, Value::Unknown) => Value::Unknown,
        _ => Value::Error,
    }
}

fn compare_ordering(ordering: std::cmp::Ordering, comparison: Comparison) -> Truth {
    let value = match comparison {
        Comparison::Equal => ordering == std::cmp::Ordering::Equal,
        Comparison::NotEqual => ordering != std::cmp::Ordering::Equal,
        Comparison::Less => ordering == std::cmp::Ordering::Less,
        Comparison::LessEqual => ordering != std::cmp::Ordering::Greater,
        Comparison::Greater => ordering == std::cmp::Ordering::Greater,
        Comparison::GreaterEqual => ordering != std::cmp::Ordering::Less,
    };
    if value { Truth::True } else { Truth::False }
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
