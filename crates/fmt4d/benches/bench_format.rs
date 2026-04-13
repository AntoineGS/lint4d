//! Minimal format_source benchmark to sanity-check Phase C performance
//! fixes. This is NOT a full benchmark suite — the full suite is Phase
//! E/F work. Just enough to see numbers move when PERF-C1/C2/C3 land.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use fmt4d::config::FmtConfig;
use fmt4d::formatter::format_source;
use pascal_core::FileInfo;
use std::collections::HashSet;
use std::path::PathBuf;

fn sample_unit(lines: usize) -> String {
    // Produce a realistic-ish Pascal unit with `lines` const declarations.
    let mut src = String::with_capacity(lines * 40 + 128);
    src.push_str("unit BenchUnit;\n\ninterface\n\nconst\n");
    for i in 0..lines {
        src.push_str(&format!("  k{:04} = 'value {}';\n", i, i));
    }
    src.push_str("\nimplementation\n\nend.\n");
    src
}

fn bench_format_1k_lines(c: &mut Criterion) {
    let src = sample_unit(1000);
    let info = FileInfo::new(PathBuf::from("BenchUnit.pas"));
    let config = FmtConfig::default();
    let external = HashSet::new();
    c.bench_function("format_source_1000_const_lines", |b| {
        b.iter(|| {
            let result = format_source(
                black_box(src.as_bytes()),
                black_box(&info),
                black_box(&config),
                black_box(&external),
            )
            .unwrap();
            black_box(result);
        });
    });
}

criterion_group!(benches, bench_format_1k_lines);
criterion_main!(benches);
