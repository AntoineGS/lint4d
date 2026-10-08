//! Bounded, reference-derived Delphi compile-time storage facts.
//!
//! This module does not bind names or analyze declarations. Callers must prove
//! intrinsic identity before using the closed scalar registry and must charge
//! the shared request budget before processing or retaining source payloads.
mod budget;
mod model;
mod scalars;

pub use budget::{CompileTimeBudget, CompileTimeLimits};
pub use model::*;
pub use scalars::{BuiltinType, builtin_layout};
