//! Bounded, reference-derived Delphi compile-time storage facts.
//!
//! Declaration ingestion and lexical queries share a request-wide budget.
//! Intrinsic layout remains separate from parsing and name binding.
mod arrays;
mod bindings;
mod budget;
mod declarations;
mod model;
mod operands;
mod records;
mod scalars;

pub use budget::{CompileTimeBudget, CompileTimeLimits};
pub use declarations::CompileTimeState;
pub use model::*;
pub use operands::{SizeOfOperand, parse_sizeof_operand};
pub use scalars::{BuiltinType, builtin_layout};
