//! Shared, explicit conditional-compilation context metadata.
//!
//! The project crate owns these small value types so project discovery and the
//! core conditional evaluator can exchange compiler facts without depending on
//! one another.  A missing fact is deliberately represented as `Unknown`; no
//! host platform, current date, or Rust toolchain value is ever inserted.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

/// Three-valued fact used for defines and compiler options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConditionalFact {
    True,
    False,
    Unknown,
}

impl ConditionalFact {
    pub const fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, value) | (value, Self::True) => value,
            (Self::Unknown, Self::Unknown) => Self::Unknown,
        }
    }

    pub const fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, value) | (value, Self::False) => value,
            (Self::Unknown, Self::Unknown) => Self::Unknown,
        }
    }

    pub const fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }

    pub const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            (Self::Unknown, Self::Unknown) => Self::Unknown,
            _ => Self::Unknown,
        }
    }
}

const MAX_COMPILER_VERSION_SCALE: u8 = 6;

/// Exact decimal compiler version used by Delphi's `CompilerVersion`.
///
/// `CompilerVersion` is a numeric constant, not a semantic-version tuple:
/// `18.5`, `18.50`, and `18.500000` are the same value.  The public legacy
/// component fields remain available for source compatibility, while the
/// private normalized decimal is the only value used for equality and
/// comparison.  A value created through [`Self::with_patch`] is deliberately
/// unsupported because `24.0.1` has no Delphi numeric meaning.
#[derive(Debug, Clone, Copy)]
pub struct CompilerVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    mantissa: Option<u64>,
    scale: u8,
}

impl CompilerVersion {
    pub fn new(major: u32, minor: u32) -> Self {
        let scale = decimal_digits(minor);
        let mantissa =
            decimal_mantissa(major, minor, scale).map(|value| normalize_decimal(value, scale));
        let (mantissa, scale) = mantissa.unwrap_or((0, 0));
        Self {
            major,
            minor,
            patch: 0,
            mantissa: (decimal_digits(minor) <= MAX_COMPILER_VERSION_SCALE).then_some(mantissa),
            scale,
        }
    }

    /// Construct an explicitly unsupported dotted value.
    ///
    /// Delphi does not define a semantic-version interpretation for a patch
    /// component.  Keeping this constructor as an invalid value lets older
    /// callers fail closed instead of silently changing `CompilerVersion`
    /// semantics.
    pub const fn with_patch(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
            mantissa: None,
            scale: 0,
        }
    }

    /// Parse a bounded decimal version such as `24`, `24.0`, or `18.50`.
    /// Dotted semantic-version input (for example `24.0.1`) is rejected.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let (major_text, fractional_text) = match value.split_once('.') {
            Some((major, fractional)) => (major, Some(fractional)),
            None => (value, None),
        };
        if major_text.is_empty() || !major_text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let major = major_text.parse::<u32>().ok()?;
        let fractional_text = fractional_text.unwrap_or_default();
        if fractional_text.is_empty() {
            return if value.ends_with('.') {
                None
            } else {
                Some(Self::new(major, 0))
            };
        }
        if fractional_text.len() > usize::from(MAX_COMPILER_VERSION_SCALE)
            || !fractional_text.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let scale = u8::try_from(fractional_text.len()).ok()?;
        let minor = fractional_text.parse::<u32>().ok()?;
        let mantissa = decimal_mantissa(major, minor, scale)?;
        let (mantissa, scale) = normalize_decimal(mantissa, scale);
        Some(Self {
            major,
            minor,
            patch: 0,
            mantissa: Some(mantissa),
            scale,
        })
    }

    /// Return the normalized exact decimal representation when supported.
    pub fn numeric_parts(self) -> Option<(u64, u8)> {
        self.mantissa.map(|mantissa| (mantissa, self.scale))
    }

    /// Compare against an integer without narrowing it to a `u32`.
    pub fn cmp_integer(self, integer: i64) -> Option<Ordering> {
        let mantissa = self.mantissa?;
        if integer < 0 {
            return Some(Ordering::Greater);
        }
        compare_decimal(mantissa, self.scale, integer as u64, 0)
    }

    /// Compare two exact decimal compiler versions.
    pub fn cmp_numeric(self, other: Self) -> Option<Ordering> {
        let (left, left_scale) = self.numeric_parts()?;
        let (right, right_scale) = other.numeric_parts()?;
        compare_decimal(left, left_scale, right, right_scale)
    }
}

impl PartialEq for CompilerVersion {
    fn eq(&self, other: &Self) -> bool {
        match (self.numeric_parts(), other.numeric_parts()) {
            (Some(left), Some(right)) => left == right,
            (None, None) => {
                (self.major, self.minor, self.patch) == (other.major, other.minor, other.patch)
            }
            _ => false,
        }
    }
}

impl Eq for CompilerVersion {}

impl Hash for CompilerVersion {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self.numeric_parts() {
            Some((mantissa, scale)) => {
                true.hash(state);
                mantissa.hash(state);
                scale.hash(state);
            }
            None => {
                false.hash(state);
                self.major.hash(state);
                self.minor.hash(state);
                self.patch.hash(state);
            }
        }
    }
}

impl PartialOrd for CompilerVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CompilerVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.numeric_parts(), other.numeric_parts()) {
            (Some((left, left_scale)), Some((right, right_scale))) => {
                compare_decimal(left, left_scale, right, right_scale).unwrap_or(Ordering::Equal)
            }
            (None, None) => {
                (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
            }
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
        }
    }
}

fn decimal_digits(value: u32) -> u8 {
    if value == 0 {
        0
    } else {
        let mut value = value;
        let mut digits = 0;
        while value > 0 {
            value /= 10;
            digits += 1;
        }
        digits
    }
}

fn decimal_mantissa(major: u32, minor: u32, scale: u8) -> Option<u64> {
    let factor = 10_u64.checked_pow(u32::from(scale))?;
    u64::from(major)
        .checked_mul(factor)?
        .checked_add(u64::from(minor))
}

fn normalize_decimal(mut mantissa: u64, mut scale: u8) -> (u64, u8) {
    while scale > 0 && mantissa % 10 == 0 {
        mantissa /= 10;
        scale -= 1;
    }
    (mantissa, scale)
}

fn compare_decimal(left: u64, left_scale: u8, right: u64, right_scale: u8) -> Option<Ordering> {
    let scale = left_scale.max(right_scale);
    let left = u128::from(left).checked_mul(10_u128.checked_pow(u32::from(scale - left_scale))?)?;
    let right =
        u128::from(right).checked_mul(10_u128.checked_pow(u32::from(scale - right_scale))?)?;
    Some(left.cmp(&right))
}

/// Typed constants admitted by the bounded evaluator.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConstantValue {
    Boolean(bool),
    Integer(i64),
    String(String),
    Version(CompilerVersion),
}

/// Explicit facts supplied by a project/configuration or inherited at an
/// include boundary.  Facts absent from these maps are unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ConditionalContext {
    pub compiler_version: Option<CompilerVersion>,
    pub defines: BTreeMap<String, ConditionalFact>,
    pub options: BTreeMap<String, ConditionalFact>,
    pub constants: BTreeMap<String, ConstantValue>,
}

impl ConditionalContext {
    pub fn from_defines(defines: &[String]) -> Self {
        let mut context = Self::default();
        for define in defines {
            context.set_define(define, ConditionalFact::True);
        }
        context
    }

    pub fn with_compiler_version(mut self, version: CompilerVersion) -> Self {
        self.compiler_version = Some(version);
        self
    }

    pub fn with_option(mut self, name: impl AsRef<str>, value: ConditionalFact) -> Self {
        self.set_option(name, value);
        self
    }

    pub fn with_constant(mut self, name: impl AsRef<str>, value: ConstantValue) -> Self {
        self.set_constant(name, value);
        self
    }

    pub fn set_define(&mut self, name: impl AsRef<str>, value: ConditionalFact) {
        if let Some(name) = canonical_name(name.as_ref()) {
            self.defines.insert(name, value);
        }
    }

    pub fn set_option(&mut self, name: impl AsRef<str>, value: ConditionalFact) {
        if let Some(name) = canonical_option_name(name.as_ref()) {
            let value = self
                .options
                .get(&name)
                .copied()
                .map_or(value, |previous| previous.merge(value));
            self.options.insert(name, value);
        }
    }

    pub fn set_constant(&mut self, name: impl AsRef<str>, value: ConstantValue) {
        if let Some(name) = canonical_name(name.as_ref()) {
            self.constants.insert(name, value);
        }
    }

    pub fn define(&self, name: &str) -> ConditionalFact {
        canonical_name(name)
            .and_then(|name| self.defines.get(&name).copied())
            .unwrap_or(ConditionalFact::Unknown)
    }

    pub fn option(&self, name: &str) -> ConditionalFact {
        canonical_option_name(name)
            .and_then(|name| self.options.get(&name).copied())
            .unwrap_or(ConditionalFact::Unknown)
    }

    pub fn constant(&self, name: &str) -> Option<&ConstantValue> {
        canonical_name(name).and_then(|name| self.constants.get(&name))
    }

    pub fn fingerprint(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        0_u8.hash(&mut hasher);
        self.compiler_version.hash(&mut hasher);
        1_u8.hash(&mut hasher);
        self.defines.len().hash(&mut hasher);
        for (name, value) in &self.defines {
            name.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        2_u8.hash(&mut hasher);
        self.options.len().hash(&mut hasher);
        for (name, value) in &self.options {
            name.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        3_u8.hash(&mut hasher);
        self.constants.len().hash(&mut hasher);
        for (name, value) in &self.constants {
            name.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        hasher.finish()
    }
}

fn canonical_name(name: &str) -> Option<String> {
    let name = name.trim().trim_start_matches('&');
    let mut bytes = name.bytes();
    let first = bytes.next()?;
    if (!first.is_ascii_alphabetic() && first != b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.')
    {
        return None;
    }
    Some(name.to_ascii_uppercase())
}

/// Canonicalize the finite set of option aliases understood by the evaluator.
/// Unknown names remain distinct, but case and the supported Delphi short/long
/// spellings never create contradictory facts.
pub fn canonical_option_name(name: &str) -> Option<String> {
    let name = canonical_name(name)?;
    let canonical = match name.as_str() {
        "R" | "RANGECHECKS" | "RANGE_CHECKS" => "R",
        "O" | "OPTIMIZATION" => "O",
        "Q" | "OVERFLOWCHECKS" | "OVERFLOW_CHECKS" => "Q",
        "C" | "ASSERTIONS" => "C",
        "B" | "BOOLEVAL" => "B",
        "I" | "IOCHECKS" | "IOERRORS" => "I",
        "X" | "EXTENDEDSYNTAX" => "X",
        "T" | "TYPEDADDRESS" => "T",
        "D" | "DEBUGINFO" | "DEBUG_INFORMATION" => "D",
        "RUNTIMECHECKS" | "RUNTIME_CHECKS" => "RUNTIME_CHECKS",
        "DEBUGINFORMATION" => "D",
        "Y" | "REFERENCEINFO" | "REFERENCE_INFO" | "DEFINITIONINFO" => "Y",
        other => other,
    };
    Some(canonical.to_string())
}
