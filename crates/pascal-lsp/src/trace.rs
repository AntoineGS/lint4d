//! Opt-in timeline for diagnosing latency and indexing churn. Set
//! `PASCAL_LSP_TRACE` to a file path to append one line per event; unset,
//! every trace point costs one initialized `OnceLock` read.

use std::cell::Cell;
use std::fmt;
use std::io::Write;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

struct Sink {
    start: Instant,
    file: Mutex<std::fs::File>,
}

static SINK: OnceLock<Option<Sink>> = OnceLock::new();

fn sink() -> Option<&'static Sink> {
    SINK.get_or_init(|| {
        let path = std::env::var_os("PASCAL_LSP_TRACE")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Sink {
            start: Instant::now(),
            file: Mutex::new(file),
        })
    })
    .as_ref()
}

pub(crate) fn enabled() -> bool {
    sink().is_some()
}

pub(crate) fn write(args: fmt::Arguments<'_>) {
    let Some(sink) = sink() else {
        return;
    };
    let elapsed = sink.start.elapsed().as_secs_f64();
    let thread = std::thread::current();
    let line = format!("{elapsed:10.3} [{}] {args}\n", thread.name().unwrap_or("?"));
    let _ = sink
        .file
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .write_all(line.as_bytes());
}

macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::trace::enabled() {
            $crate::trace::write(format_args!($($arg)*));
        }
    };
}
pub(crate) use trace;

/// Project-cache activity on the current thread, so a span can report what
/// its work hit, computed, or waited for.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CacheCounters {
    pub hits: u64,
    pub computes: u64,
    pub detached: u64,
    pub peek_hits: u64,
    pub peek_misses: u64,
    pub wait: Duration,
    pub probes: Duration,
}

thread_local! {
    static COUNTERS: Cell<CacheCounters> = Cell::new(CacheCounters::default());
}

pub(crate) fn count(update: impl FnOnce(&mut CacheCounters)) {
    if !enabled() {
        return;
    }
    COUNTERS.with(|cell| {
        let mut counters = cell.get();
        update(&mut counters);
        cell.set(counters);
    });
}

fn counters() -> CacheCounters {
    COUNTERS.with(Cell::get)
}

/// Logs its label with the elapsed time and this thread's cache activity
/// when dropped.
pub(crate) struct Span {
    label: String,
    start: Instant,
    before: CacheCounters,
}

impl Span {
    pub(crate) fn new(label: impl FnOnce() -> String) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let label = label();
        write(format_args!("begin {label}"));
        Some(Self {
            label,
            start: Instant::now(),
            before: counters(),
        })
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let now = counters();
        let before = self.before;
        write(format_args!(
            "end   {} after {:.1} ms; cache hits={} computes={} detached={} peek={}/{} wait={:.1} ms probes={:.1} ms",
            self.label,
            self.start.elapsed().as_secs_f64() * 1000.0,
            now.hits - before.hits,
            now.computes - before.computes,
            now.detached - before.detached,
            now.peek_hits - before.peek_hits,
            (now.peek_hits - before.peek_hits) + (now.peek_misses - before.peek_misses),
            (now.wait - before.wait).as_secs_f64() * 1000.0,
            (now.probes - before.probes).as_secs_f64() * 1000.0,
        ));
    }
}

/// Shortens a `Debug` rendering for a one-line label.
pub(crate) fn short(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
