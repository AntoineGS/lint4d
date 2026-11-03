//! Shared directory catalogues (l4d-20).

#[cfg(test)]
use std::path::{Path, PathBuf};

#[cfg(test)]
thread_local! {
    static TEST_LISTINGS: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Records a directory a catalogue walk is about to list.
#[cfg(test)]
pub(crate) fn test_record_listing(directory: &Path) {
    TEST_LISTINGS.with(|listings| listings.borrow_mut().push(directory.to_path_buf()));
}

/// The directories listed on this thread since the last reset.
#[cfg(test)]
pub(crate) fn test_listings() -> Vec<PathBuf> {
    TEST_LISTINGS.with(|listings| listings.borrow().clone())
}

#[cfg(test)]
pub(crate) fn test_reset_listings() {
    TEST_LISTINGS.with(|listings| listings.borrow_mut().clear());
}
