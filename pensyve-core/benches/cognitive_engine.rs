// Bench fixtures use `as f64` from small loop indices and the criterion-bundled
// `black_box` re-export; replacing them with `f64::from` / `std::hint::black_box`
// would touch every benchmark site without changing what the benches measure.
#![allow(deprecated, clippy::cast_lossless)]

use criterion::{Criterion, black_box, criterion_group, criterion_main};
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
        .map(|_| {
            (0..100)
                .map(|i| (Uuid::new_v4(), 1.0 - i as f32 / 100.0))
                .collect()
        })
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

/// Phase 2B — per-passage extraction throughput (target: <5ms p95).
fn bench_dep_parse_extract(c: &mut Criterion) {
    // Representative chat-style memory passage: 3 sentences, mix of
    // nsubj→root→dobj + nsubj→root→pobj shapes. Length is ~30 tokens,
    // well under the 200-token skip cap.
    let passage = "Alice works at Acme Corp. \
        Bob lives in Brooklyn with his family. \
        Carol bought a Tesla last weekend.";
    c.bench_function("dep_parse_extract_passage", |bencher| {
        bencher.iter(|| {
            let _ = pensyve_core::extraction::dep_parse::extract_triples(
                black_box(Uuid::nil()),
                black_box(passage),
            );
        });
    });
}

/// Phase 2C — `PprIndex::build_from_storage` on a 10k-passage
/// namespace. The brief's acceptance criterion is p95 < 500ms.
///
/// Builds an in-memory `SQLite` with the migration-v3 schema, seeds
/// 10k passages × 5 entities (50k passage-entity edges) with a
/// realistic skew (some entities appear in many passages, most in
/// 1-2) so the degree dampener has work to do, then benchmarks
/// `PprIndex::build_from_storage`. The seeding step is OUTSIDE the
/// iter loop so the timer only measures the build itself.
fn bench_ppr_build_from_storage_10k(c: &mut Criterion) {
    use rusqlite::Connection;

    let namespace_id = Uuid::new_v4().to_string();
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE kg_entities (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            namespace_id TEXT NOT NULL,
            lemma TEXT NOT NULL,
            embedding BLOB,
            created_at INTEGER NOT NULL,
            UNIQUE(namespace_id, lemma)
        );
        CREATE TABLE kg_passage_entities (
            passage_id TEXT NOT NULL,
            entity_id INTEGER NOT NULL,
            weight REAL NOT NULL,
            PRIMARY KEY(passage_id, entity_id)
        );",
    )
    .unwrap();

    // 500 distinct entity lemmas; 10k passages each connected to 5
    // entities, with skew: entity_idx = (passage_idx * 7 + offset) %
    // 500 — the multiplier creates a deterministic but unevenly
    // distributed entity-degree profile (some entities show up
    // ~100x, others ~20x).
    let n_entities = 500_usize;
    let n_passages = 10_000_usize;
    for ent_idx in 0..n_entities {
        conn.execute(
            "INSERT INTO kg_entities (namespace_id, lemma, created_at) VALUES (?1, ?2, 0)",
            rusqlite::params![namespace_id, format!("entity_{ent_idx}")],
        )
        .unwrap();
    }
    // Wrap the bulk insert in a transaction for sane seeding speed.
    let tx = conn.unchecked_transaction().unwrap();
    for p_idx in 0..n_passages {
        let pid = Uuid::new_v4().to_string();
        for k in 0..5 {
            let ent_idx = ((p_idx * 7 + k) % n_entities) + 1; // SQLite ids are 1-based
            tx.execute(
                "INSERT OR IGNORE INTO kg_passage_entities (passage_id, entity_id, weight) VALUES (?1, ?2, 1.0)",
                rusqlite::params![pid, i64::try_from(ent_idx).unwrap()],
            )
            .unwrap();
        }
    }
    tx.commit().unwrap();

    c.bench_function("ppr_build_from_storage_10k_passages", |bencher| {
        bencher.iter(|| {
            let _ = pensyve_core::retrieval::ppr::PprIndex::build_from_storage(
                black_box(&conn),
                black_box(&namespace_id),
            )
            .unwrap();
        });
    });
}

/// Phase 2C — `PprIndex::query` on a 10k-passage index. Companion to
/// the build bench; verifies the query-side path also stays cheap.
fn bench_ppr_query_10k(c: &mut Criterion) {
    use rusqlite::Connection;

    let namespace_id = Uuid::new_v4().to_string();
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE kg_entities (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            namespace_id TEXT NOT NULL,
            lemma TEXT NOT NULL,
            embedding BLOB,
            created_at INTEGER NOT NULL,
            UNIQUE(namespace_id, lemma)
        );
        CREATE TABLE kg_passage_entities (
            passage_id TEXT NOT NULL,
            entity_id INTEGER NOT NULL,
            weight REAL NOT NULL,
            PRIMARY KEY(passage_id, entity_id)
        );",
    )
    .unwrap();

    let n_entities = 500_usize;
    let n_passages = 10_000_usize;
    for ent_idx in 0..n_entities {
        conn.execute(
            "INSERT INTO kg_entities (namespace_id, lemma, created_at) VALUES (?1, ?2, 0)",
            rusqlite::params![namespace_id, format!("entity_{ent_idx}")],
        )
        .unwrap();
    }
    let tx = conn.unchecked_transaction().unwrap();
    for p_idx in 0..n_passages {
        let pid = Uuid::new_v4().to_string();
        for k in 0..5 {
            let ent_idx = ((p_idx * 7 + k) % n_entities) + 1;
            tx.execute(
                "INSERT OR IGNORE INTO kg_passage_entities (passage_id, entity_id, weight) VALUES (?1, ?2, 1.0)",
                rusqlite::params![pid, i64::try_from(ent_idx).unwrap()],
            )
            .unwrap();
        }
    }
    tx.commit().unwrap();

    let idx =
        pensyve_core::retrieval::ppr::PprIndex::build_from_storage(&conn, &namespace_id).unwrap();
    // Seed with 3 query entities (a realistic count after dep-parse).
    let seeds: Vec<Uuid> = (0..3)
        .map(|i| pensyve_core::retrieval::ppr::lemma_uuid(&format!("entity_{i}")))
        .collect();

    c.bench_function("ppr_query_10k_passages_alpha_0_15", |bencher| {
        bencher.iter(|| {
            let _ = idx.query(black_box(&seeds), black_box(&[]), 0.15, 20, 50);
        });
    });
}

criterion_group!(
    benches,
    bench_cosine_768,
    bench_base_level_activation,
    bench_rrf_fusion,
    bench_ring_buffer,
    bench_dep_parse_extract,
    bench_ppr_build_from_storage_10k,
    bench_ppr_query_10k,
);
criterion_main!(benches);
