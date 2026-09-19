//! Shared, explicit conditional-compilation context metadata.
//!
//! The project crate owns these small value types so project discovery and the
//! core conditional evaluator can exchange compiler facts without depending on
//! one another.  A missing fact is deliberately represented as `Unknown`; no
//! host platform, current date, or Rust toolchain value is ever inserted.

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

/// Exact, non-floating-point compiler version used by `CompilerVersion`.
///
/// Delphi exposes versions such as `24.0` and `18.5`.  Keeping each component
/// as an integer avoids binary floating-point comparisons and preserves the
/// precision explicitly supplied by a project/client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CompilerVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl CompilerVersion {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self {
            major,
            minor,
            patch: 0,
        }
    }

    pub const fn with_patch(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parse a decimal version such as `24`, `24.0`, or `24.0.1`.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim().strip_prefix('v').unwrap_or(value.trim());
        let mut parts = value.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().map_or(Some(0), |part| part.parse().ok())?;
        let patch = parts.next().map_or(Some(0), |part| part.parse().ok())?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
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
        if let Some(name) = canonical_name(name.as_ref()) {
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
        canonical_name(name)
            .and_then(|name| self.options.get(&name).copied())
            .unwrap_or(ConditionalFact::Unknown)
    }

    pub fn constant(&self, name: &str) -> Option<&ConstantValue> {
        canonical_name(name).and_then(|name| self.constants.get(&name))
    }

    pub fn fingerprint(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

fn canonical_name(name: &str) -> Option<String> {
    let name = name.trim().trim_start_matches('&');
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.')
    {
        return None;
    }
    Some(name.to_ascii_uppercase())
}
