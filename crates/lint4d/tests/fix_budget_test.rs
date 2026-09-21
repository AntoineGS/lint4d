use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lint4d::config::Config;
use lint4d::engine::{FileInfo, parse_file};
use lint4d::fix::{FixWorkBudget, fix_file_edits_bounded};

static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: This forwards the allocator contract unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: This forwards the allocator contract unchanged.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATED_BYTES.fetch_add(size, Ordering::Relaxed);
        // SAFETY: This forwards the allocator contract unchanged.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

struct CountingBudget {
    work: usize,
    bytes: usize,
    byte_limit: usize,
}

impl FixWorkBudget for CountingBudget {
    fn charge_work(&mut self, amount: usize) -> Result<(), String> {
        self.work = self.work.saturating_add(amount);
        Ok(())
    }

    fn charge_bytes(&mut self, amount: usize) -> Result<(), String> {
        self.bytes = self.bytes.saturating_add(amount);
        if self.bytes > self.byte_limit {
            return Err("test request byte budget exhausted".to_string());
        }
        Ok(())
    }
}

struct CancellingBudget {
    work: usize,
    bytes: usize,
    cancel_after_work: usize,
    cancel: Arc<AtomicBool>,
}

impl FixWorkBudget for CancellingBudget {
    fn charge_work(&mut self, amount: usize) -> Result<(), String> {
        self.work = self.work.saturating_add(amount);
        if self.work >= self.cancel_after_work {
            self.cancel.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn charge_bytes(&mut self, amount: usize) -> Result<(), String> {
        self.bytes = self.bytes.saturating_add(amount);
        Ok(())
    }
}

fn large_class_fixture() -> String {
    let mut source = String::from("unit Main;\ninterface\ntype TDemo = class\npublic\n");
    for index in 0..2_000 {
        source.push_str(&format!("Field{index}: Integer;\n"));
    }
    for index in 0..200 {
        source.push_str(&format!("procedure Method{index};\n"));
    }
    source.push_str("end;\nimplementation\n");
    for index in 0..200 {
        source.push_str(&format!(
            "procedure TDemo.Method{index};\nvar badLocal: Integer;\nbegin badLocal := 1; end;\n"
        ));
    }
    source.push_str("end.\n");
    source
}

#[test]
fn bounded_fix_materialization_stays_inside_the_advertised_byte_budget() {
    let source = large_class_fixture();
    let file = FileInfo::new(PathBuf::from("Main.pas"));
    let (_, parse_errors) = parse_file(&file, source.as_bytes()).expect("fixture parses");
    assert!(
        parse_errors.is_empty(),
        "fixture has parser errors: {parse_errors:?}"
    );
    let config = "version = 1".parse::<Config>().expect("configuration");
    let cancel = AtomicBool::new(false);
    let mut budget = CountingBudget {
        work: 0,
        bytes: 0,
        byte_limit: 32 * 1024 * 1024,
    };
    let before = ALLOCATED_BYTES.load(Ordering::Relaxed);

    let result = fix_file_edits_bounded(
        &file,
        source.as_bytes(),
        &config,
        &["local-variable-naming"],
        &mut budget,
        &cancel,
    )
    .expect("bounded fix should complete within the request budget");

    let allocated = ALLOCATED_BYTES.load(Ordering::Relaxed) - before;
    assert_eq!(result.len(), 400);
    assert!(
        budget.bytes <= 32 * 1024 * 1024,
        "logical owned bytes exceeded the request budget: {}",
        budget.bytes
    );
    assert!(
        allocated <= 32 * 1024 * 1024,
        "bounded fix allocated {allocated} cumulative Rust bytes despite the 32 MiB limit"
    );
}

#[test]
fn bounded_fix_fails_closed_after_scope_materialization_exhausts_the_budget() {
    let source = large_class_fixture();
    let file = FileInfo::new(PathBuf::from("Main.pas"));
    let config = "version = 1".parse::<Config>().expect("configuration");
    let cancel = AtomicBool::new(false);
    let mut budget = CountingBudget {
        work: 0,
        bytes: 0,
        byte_limit: 1_000_000,
    };

    let error = fix_file_edits_bounded(
        &file,
        source.as_bytes(),
        &config,
        &["local-variable-naming"],
        &mut budget,
        &cancel,
    )
    .expect_err("scope/materialization work must consume the shared byte budget");

    assert_eq!(error, "test request byte budget exhausted");
    assert!(budget.bytes > 1_000_000);
}

#[test]
fn bounded_fix_observes_cancellation_after_entering_materialization_loops() {
    let source = large_class_fixture();
    let file = FileInfo::new(PathBuf::from("Main.pas"));
    let config = "version = 1".parse::<Config>().expect("configuration");
    let cancel = Arc::new(AtomicBool::new(false));
    let mut budget = CancellingBudget {
        work: 0,
        bytes: 0,
        cancel_after_work: 80_000,
        cancel: Arc::clone(&cancel),
    };

    let error = fix_file_edits_bounded(
        &file,
        source.as_bytes(),
        &config,
        &["local-variable-naming"],
        &mut budget,
        cancel.as_ref(),
    )
    .expect_err("cancellation raised inside a materialization loop must withhold edits");

    assert_eq!(error, "request cancelled");
    assert!(budget.work >= 80_000);
}
