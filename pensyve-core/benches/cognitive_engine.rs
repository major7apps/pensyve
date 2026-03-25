use criterion::{black_box, criterion_group, criterion_main, Criterion};
use uuid::Uuid;

fn bench_cosine_768(c: &mut Criterion) {
    let a: Vec<f32> = (0..768).map(|i| (i as f32 * 0.01).sin()).collect();
    let b: Vec<f32> = (0..768).map(|i| (i as f32 * 0.02).cos()).collect();
    c.bench_function("cosine_similarity_768d", |bencher| {
        bencher.iter(|| pensyve_core::embedding::cosine_similarity(black_box(&a), black_box(&b)));
    });
}

fn bench_base_level_activation(c: &mut Criterion) {
    let times: Vec<f64> = (0..100).map(|i| i as f64 * 3600.0).collect();
    c.bench_function("actr_activation_100", |bencher| {
        bencher.iter(|| {
            pensyve_core::activation::base_level_activation(black_box(&times), 360_000.0, 0.5)
        });
    });
}

fn bench_rrf_fusion(c: &mut Criterion) {
    let rankings: Vec<Vec<(Uuid, f32)>> = (0..6)
        .map(|_| (0..100).map(|i| (Uuid::new_v4(), 1.0 - i as f32 / 100.0)).collect())
        .collect();
    let weights = vec![1.0_f32, 0.8, 1.0, 0.8, 0.5, 0.5];
    c.bench_function("rrf_6x100", |bencher| {
        bencher.iter(|| {
            pensyve_core::rrf::reciprocal_rank_fusion(black_box(&rankings), black_box(&weights), 60)
        });
    });
}

fn bench_ring_buffer(c: &mut Criterion) {
    c.bench_function("ring_buffer_push_100", |bencher| {
        bencher.iter(|| {
            let mut buf = pensyve_core::activation::AccessRingBuffer::new(100);
            for i in 0..100 {
                buf.push(black_box(i as f64 * 1000.0));
            }
            buf.activation(100_000.0, 0.5)
        });
    });
}

criterion_group!(benches, bench_cosine_768, bench_base_level_activation, bench_rrf_fusion, bench_ring_buffer);
criterion_main!(benches);
