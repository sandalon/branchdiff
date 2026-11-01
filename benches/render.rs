use branchdiff::diff;
use criterion::{criterion_group, criterion_main, Criterion};

fn parse_large_patch(c: &mut Criterion) {
    const LINES: usize = 300;
    let mut patch = String::new();
    for f in 0..5_000 {
        patch.push_str(&format!("diff --git a/f{f} b/f{f}\n--- a/f{f}\n+++ b/f{f}\n@@ -1,{LINES} +1,{LINES} @@\n"));
        for i in 0..LINES {
            patch.push_str(&format!(" let value_{i} = compute_the_thing({i}, \"some typical line of code\");\n"));
        }
    }
    c.bench_function("parse about 100 MB", |b| b.iter(|| diff::parse(patch.as_bytes())));
}

criterion_group!(benches, parse_large_patch);
criterion_main!(benches);