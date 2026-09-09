use criterion::{criterion_group, criterion_main, Criterion};

fn smoke_benchmark(_c: &mut Criterion) {}

criterion_group!(orca, smoke_benchmark);
criterion_main!(orca);
