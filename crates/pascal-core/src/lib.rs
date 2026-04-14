pub mod config_discovery;
pub mod directive_fragment_rewrite;
pub mod directives;
pub mod discovery;
pub mod discovery_bds;
pub mod discovery_dproj;
pub mod discovery_msbuild;
pub mod node_kind;
pub mod parser;
pub mod text;
pub mod types;

pub use directives::FormatOffRegion;
pub use text::{SourceEncoding, decode_bytes, detect_encoding, encode_as};
pub use types::{Diagnostic, FileInfo, FileType, Severity};
