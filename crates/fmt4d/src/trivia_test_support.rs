use std::cell::Cell;

thread_local! {
    static LEAF_INDEX_BUILDS: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn reset_leaf_index_build_count() {
    LEAF_INDEX_BUILDS.with(|count| count.set(0));
}

pub(crate) fn leaf_index_build_count() -> usize {
    LEAF_INDEX_BUILDS.with(Cell::get)
}

pub(crate) fn record_leaf_index_build() {
    LEAF_INDEX_BUILDS.with(|count| count.set(count.get() + 1));
}
