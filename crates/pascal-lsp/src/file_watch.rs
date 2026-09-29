use std::path::Path;

/// Non-recursive directory watch registration. Returns `false` when the
/// directory cannot be watched; callers then rely on stamp probes alone.
pub(crate) trait DirectoryWatch: Send {
    fn watch(&mut self, directory: &Path) -> bool;
    fn unwatch(&mut self, directory: &Path);
}
