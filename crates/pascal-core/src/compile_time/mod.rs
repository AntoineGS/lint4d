//! Bounded, reference-derived Delphi compile-time storage facts.
//!
//! Declaration ingestion and lexical queries share a request-wide budget.
//! Intrinsic layout remains separate from parsing and name binding.
mod arrays;
mod bindings;
mod budget;
mod coordinates;
mod declarations;
mod header;
mod model;
mod operands;
mod records;
mod scalars;
mod session;

pub use budget::{
    AdmittedOwned, AdmittedShared, CompileTimeBudget, CompileTimeLimits, HEADER_MAX_BRANCH_STATES,
    HEADER_MAX_BYTE_WORK,
};
pub(crate) use coordinates::text as coordinate_text;
pub use declarations::CompileTimeState;
pub(crate) use header::plain_header_in_session;
pub use header::verify_unit_header;
pub use model::*;
pub use operands::{SizeOfOperand, parse_sizeof_operand};
pub use scalars::{BuiltinType, builtin_layout};
pub use session::*;
