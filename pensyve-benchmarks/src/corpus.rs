use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand_distr::Normal;
use uuid::Uuid;

/// Configuration for synthetic corpus generation.
#[derive(Debug, Clone)]
pub struct CorpusConfig {
    /// Number of memories to generate.
    pub n_memories: usize,
    /// Number of distinct entities.
    pub n_entities: usize,
    /// Number of queries to generate.
    pub n_queries: usize,
    /// Embedding dimensionality.
    pub dimensions: usize,
    /// Number of Gaussian clusters.
    pub n_clusters: usize,
    /// Fraction of memories to mark as superseded.
    pub supersession_rate: f64,
}

impl Default for CorpusConfig {
    fn default() -> Self {
        Self {
            n_memories: 1000,
            n_entities: 50,
            n_queries: 200,
            dimensions: 768,
            n_clusters: 20,
            supersession_rate: 0.05,
        }
    }
}

/// A single synthetic memory entry.
#[derive(Debug, Clone)]
pub struct SyntheticMemory {
    pub id: Uuid,
    pub entity_id: Uuid,
    pub content: String,
    pub embedding: Vec<f32>,
    pub access_count: u32,
    pub timestamp_secs: f64,
    pub superseded: bool,
}

/// A single synthetic query with ground-truth answers.
#[derive(Debug, Clone)]
pub struct SyntheticQuery {
    pub text: String,
    pub embedding: Vec<f32>,
    pub intent: String,
    pub gold_memory_ids: Vec<Uuid>,
    pub difficulty: String,
}

/// The full generated corpus.
#[derive(Debug, Clone)]
pub struct SyntheticCorpus {
    pub memories: Vec<SyntheticMemory>,
    pub entities: Vec<Uuid>,
    pub queries: Vec<SyntheticQuery>,
}

/// L2-normalize a vector. Returns a unit vector (or zero vector if input is zero).
pub fn normalize(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm < f32::EPSILON {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

/// Cosine similarity between two vectors of the same length.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Generate a synthetic corpus from `config` using a seeded RNG for reproducibility.
pub fn generate_corpus(config: &CorpusConfig, seed: u64) -> SyntheticCorpus {
    let mut rng = StdRng::seed_from_u64(seed);

    // --- Entities ---
    let entities: Vec<Uuid> = (0..config.n_entities)
        .map(|_| random_uuid(&mut rng))
        .collect();

    // --- Cluster centroids (normalized) ---
    let noise_centroid = Normal::new(0.0_f32, 1.0_f32).unwrap();
    let centroids: Vec<Vec<f32>> = (0..config.n_clusters)
        .map(|_| {
            let raw: Vec<f32> = (0..config.dimensions)
                .map(|_| rng.sample(noise_centroid))
                .collect();
            normalize(&raw)
        })
        .collect();

    // --- Memories ---
    let noise_memory = Normal::new(0.0_f32, 0.2_f32).unwrap();
    let n_superseded = ((config.n_memories as f64 * config.supersession_rate).round()) as usize;

    let memories: Vec<SyntheticMemory> = (0..config.n_memories)
        .map(|i| {
            let cluster_idx = i % config.n_clusters;
            let entity_idx = i % config.n_entities;

            let raw: Vec<f32> = centroids[cluster_idx]
                .iter()
                .map(|&c| c + rng.sample(noise_memory))
                .collect();
            let embedding = normalize(&raw);

            SyntheticMemory {
                id: random_uuid(&mut rng),
                entity_id: entities[entity_idx],
                content: format!("memory_{i}_cluster_{cluster_idx}_entity_{entity_idx}"),
                embedding,
                access_count: rng.random_range(0_u32..10_u32),
                timestamp_secs: rng.random_range(0.0_f64..1_000_000.0_f64),
                superseded: i < n_superseded,
            }
        })
        .collect();

    // --- Queries ---
    let noise_query = Normal::new(0.0_f32, 0.1_f32).unwrap();
    let difficulty_levels = ["easy", "medium", "hard"];

    let queries: Vec<SyntheticQuery> = (0..config.n_queries)
        .map(|i| {
            let cluster_idx = i % config.n_clusters;

            let raw: Vec<f32> = centroids[cluster_idx]
                .iter()
                .map(|&c| c + rng.sample(noise_query))
                .collect();
            let embedding = normalize(&raw);

            // Gold memories: same cluster, cosine > 0.7, not superseded.
            let mut gold_memory_ids: Vec<Uuid> = memories
                .iter()
                .filter(|m| {
                    !m.superseded
                        && (m.content.contains(&format!("cluster_{cluster_idx}_"))
                            || cosine_similarity(&m.embedding, &embedding) > 0.7)
                })
                .filter(|m| cosine_similarity(&m.embedding, &embedding) > 0.7)
                .map(|m| m.id)
                .collect();

            // Guarantee at least one gold memory: pick the closest non-superseded memory
            // in this cluster if the list is still empty.
            if gold_memory_ids.is_empty()
                && let Some(best) = memories.iter().filter(|m| !m.superseded).max_by(|a, b| {
                    cosine_similarity(&a.embedding, &embedding)
                        .partial_cmp(&cosine_similarity(&b.embedding, &embedding))
                        .unwrap()
                }) {
                    gold_memory_ids.push(best.id);
                }

            let difficulty = difficulty_levels[i % difficulty_levels.len()].to_string();

            SyntheticQuery {
                text: format!("query_{i}_cluster_{cluster_idx}"),
                embedding,
                intent: format!("intent_cluster_{cluster_idx}"),
                gold_memory_ids,
                difficulty,
            }
        })
        .collect();

    SyntheticCorpus {
        memories,
        entities,
        queries,
    }
}

/// Generate a UUID from an existing seeded RNG (avoids relying on getrandom).
fn random_uuid(rng: &mut StdRng) -> Uuid {
    let bytes: [u8; 16] = rng.random();
    uuid::Builder::from_random_bytes(bytes).into_uuid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_corpus_size() {
        let config = CorpusConfig {
            n_memories: 100,
            n_entities: 10,
            ..CorpusConfig::default()
        };
        let corpus = generate_corpus(&config, 42);
        assert_eq!(corpus.memories.len(), 100);
        assert_eq!(corpus.entities.len(), 10);
    }

    #[test]
    fn test_corpus_has_ground_truth() {
        let config = CorpusConfig {
            n_queries: 20,
            ..CorpusConfig::default()
        };
        let corpus = generate_corpus(&config, 42);
        assert_eq!(corpus.queries.len(), 20);
        for query in &corpus.queries {
            assert!(
                !query.gold_memory_ids.is_empty(),
                "query '{}' has no gold memories",
                query.text
            );
        }
    }

    #[test]
    fn test_corpus_embeddings_normalized() {
        let config = CorpusConfig {
            dimensions: 32,
            n_memories: 50,
            n_queries: 10,
            n_clusters: 5,
            n_entities: 5,
            ..CorpusConfig::default()
        };
        let corpus = generate_corpus(&config, 99);
        for memory in &corpus.memories {
            let norm: f32 = memory.embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-5,
                "memory '{}' embedding norm is {norm}, expected ≈1.0",
                memory.content
            );
        }
        for query in &corpus.queries {
            let norm: f32 = query.embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-5,
                "query '{}' embedding norm is {norm}, expected ≈1.0",
                query.text
            );
        }
    }
}
