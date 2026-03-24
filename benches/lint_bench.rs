use criterion::{black_box, criterion_group, criterion_main, Criterion};
use lint4d::config::Config;
use lint4d::engine::{parse_file, run_lint, FileInfo};

fn bench_lint_single_file(c: &mut Criterion) {
    let file = FileInfo::new("tests/fixtures/resource_leak/bad_unprotected.pas".into());
    let source = std::fs::read(&file.path).expect("fixture not found");
    let config: Config = "version = 1".parse().unwrap();

    c.bench_function("lint_single_file", |b| {
        b.iter(|| run_lint(black_box(&file), black_box(&source), black_box(&config)))
    });
}

fn bench_parse_only(c: &mut Criterion) {
    let file = FileInfo::new("tests/fixtures/resource_leak/bad_unprotected.pas".into());
    let source = std::fs::read(&file.path).expect("fixture not found");

    c.bench_function("parse_only", |b| {
        b.iter(|| parse_file(black_box(&file), black_box(&source)).unwrap())
    });
}

criterion_group!(benches, bench_lint_single_file, bench_parse_only);
criterion_main!(benches);
