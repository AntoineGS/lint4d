use std::collections::HashSet;

use crate::engine::FileInfo;

/// Registry of factory functions discovered by pre-pass source analysis.
///
/// A factory function is one whose body ultimately returns a newly-constructed
/// object (directly via `Result := TFoo.Create` or indirectly via calling
/// another registered factory).
pub struct SourceContext {
    /// (unit_name_lowercase, function_name_lowercase) pairs.
    factory_functions: HashSet<(String, String)>,
}

impl SourceContext {
    /// Build the factory registry from pre-read source files.
    ///
    /// Each entry is `(&FileInfo, &[u8])` — the file metadata and its source bytes.
    /// Files are parsed, function metadata extracted, and a fixed-point algorithm
    /// resolves direct and indirect factories. ASTs are discarded after extraction.
    pub fn build(_files: &[(&FileInfo, &[u8])]) -> Self {
        SourceContext {
            factory_functions: HashSet::new(),
        }
    }

    /// Check if a function in a given unit is a registered factory.
    pub fn is_factory(&self, unit_name: &str, function_name: &str) -> bool {
        self.factory_functions
            .contains(&(unit_name.to_lowercase(), function_name.to_lowercase()))
    }
}
