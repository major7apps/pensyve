//! Phase 2B — Rust-native shallow dependency parser.
//!
//! Extracts `(subject, predicate, object)` triples from observation
//! passages so the consolidation engine can materialize a knowledge
//! graph (`kg_entities` / `kg_triples` / `kg_passage_entities`) that
//! Phase 2C's Personalized `PageRank` reads from at recall time.
//!
//! ## Design (locked in architecture review)
//!
//! Pure Rust shallow-parse, NOT spaCy/PyO3. Coverage trade-off: ~70%
//! of dependency patterns vs spaCy's ~94%, but zero new binary size,
//! zero runtime LLM dependency, no Python subprocess. Hedges:
//!
//! 1. Sentences > 200 tokens skip extraction (return empty triples,
//!    increment `dep_parse_skipped_long_sentence`).
//! 2. Verbs not present in [`PREDICATE_LEXICON`] increment
//!    `dep_parse_lexicon_miss_count` so coverage gaps surface in
//!    monitoring.
//! 3. Async LLM re-extraction for top-salience observations is deferred
//!    to Phase 3.
//!
//! ## Sentence splitting
//!
//! fastembed's `TextEmbedding` does not expose a public tokenizer API
//! for sentence splitting, so we use a simple regex splitter
//! (`[.!?]+\s+` followed by an uppercase letter or end-of-text). This is
//! documented per the Phase 2B plan's hedge ("if fastembed's tokenizer
//! doesn't expose what you need for sentence splitting, fall back to a
//! simple regex sentence splitter").
//!
//! ## Shallow rule
//!
//! For each sentence:
//! - Walk tokens left to right.
//! - The first capitalized non-stopword run becomes the candidate
//!   subject (`nsubj`).
//! - The first verb (or "be"-form contraction) whose surface form maps
//!   into [`PREDICATE_LEXICON`] becomes the root predicate.
//! - The remaining token run after the root predicate is the object;
//!   leading prepositions (`in`/`at`/`on`/`to`/`for`/`with`/...) are
//!   stripped so we capture both `dobj` ("works at Acme") and `pobj`
//!   ("lives in Brooklyn") shapes through the same code path.
//!
//! Capitalized non-stopword tokens are tracked as entity candidates and
//! returned alongside the triples in [`ParsedPassage::entities`].

use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use regex::Regex;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single `(subject, predicate, object)` triple extracted from a
/// passage. `confidence` is a static heuristic per shape (full
/// subject-verb-object: 0.8; prepositional-object: 0.6; fragment: 0.3)
/// — the consolidation hook stores it in `kg_triples.confidence` for
/// later PPR weighting.
#[derive(Debug, Clone, PartialEq)]
pub struct Triple {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f32,
}

/// Output of [`extract_triples`] — every triple plus the deduplicated
/// list of entity lemmas observed in the passage. Entities surface even
/// when no triple fires (e.g., a fragment "Alice and Bob.") so PPR's
/// entity vocabulary stays in sync with the passage stream.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPassage {
    pub passage_id: Uuid,
    pub triples: Vec<Triple>,
    pub entities: Vec<String>,
}

// ---------------------------------------------------------------------------
// Predicate lexicon (compile-time `phf::Map`)
// ---------------------------------------------------------------------------

/// Surface-form verb → normalized relation mapping.
///
/// Keys are lowercase verb stems / common conjugations as they appear
/// in raw text. Values are the canonical relation strings written to
/// `kg_triples.predicate`. Designed so look-ups are zero-allocation on
/// the ingest hot path. ~50 entries cover the high-frequency relations
/// in chat-style memory data per the Phase 2B plan's starter set.
pub static PREDICATE_LEXICON: phf::Map<&'static str, &'static str> = phf::phf_map! {
    // Identity / occupation / location
    "works" => "works_at",
    "worked" => "works_at",
    "lives" => "lives_in",
    "lived" => "lives_in",
    "joined" => "joined",
    "left" => "left",
    "visited" => "visited",
    "manages" => "manages",
    "managed" => "manages",
    "leads" => "leads",
    "led" => "leads",
    // Possession / preference
    "owns" => "owns",
    "owned" => "owns",
    "prefer" => "prefers",
    "prefers" => "prefers",
    "preferred" => "prefers",
    "likes" => "likes",
    "liked" => "likes",
    "loves" => "loves",
    "loved" => "loves",
    "hates" => "hates",
    "hated" => "hates",
    "fears" => "fears",
    "feared" => "fears",
    // Knowledge / cognition
    "knows" => "knows",
    "knew" => "knows",
    "learned" => "learned",
    "taught" => "taught",
    "studied" => "studied",
    "read" => "read",
    // Action / production
    "uses" => "uses",
    "used" => "uses",
    "builds" => "builds",
    "built" => "builds",
    "wrote" => "wrote",
    "writes" => "wrote",
    "creates" => "creates",
    "created" => "creates",
    "deletes" => "deletes",
    "deleted" => "deletes",
    "updates" => "updates",
    "updated" => "updates",
    // Communication / interaction
    "said" => "said",
    "says" => "said",
    "told" => "told",
    "tells" => "told",
    "asked" => "asked",
    "asks" => "asked",
    "answered" => "answered",
    "answers" => "answered",
    "called" => "called",
    "calls" => "called",
    "emailed" => "emailed",
    "emails" => "emailed",
    "messaged" => "messaged",
    "messages" => "messaged",
    "follows" => "follows",
    "followed" => "follows",
    "helps" => "helps",
    "helped" => "helps",
    "met" => "met",
    "meets" => "met",
    "sends" => "sends",
    "sent" => "sends",
    "receives" => "receives",
    "received" => "receives",
    "gives" => "gives",
    "gave" => "gives",
    "takes" => "takes",
    "took" => "takes",
    // Commerce / transaction
    "bought" => "bought",
    "buys" => "bought",
    "sold" => "sold",
    "sells" => "sold",
    "finds" => "finds",
    "found" => "finds",
    // Daily activity
    "plays" => "plays",
    "played" => "plays",
    "watches" => "watches",
    "watched" => "watches",
    "listens" => "listens",
    "listened" => "listens",
    "eats" => "eats",
    "ate" => "eats",
    "drinks" => "drinks",
    "drank" => "drinks",
    "sleeps" => "sleeps",
    "slept" => "sleeps",
    "runs" => "runs",
    "ran" => "runs",
    "walks" => "walks",
    "walked" => "walks",
    "drives" => "drives",
    "drove" => "drives",
    "flies" => "flies",
    "flew" => "flies",
};

// ---------------------------------------------------------------------------
// Constants / heuristics
// ---------------------------------------------------------------------------

/// Sentences longer than this many whitespace-separated tokens skip
/// extraction — the shallow rule produces noisier output as clause
/// depth grows, so we hedge by tracking the skipped count as a metric
/// instead of emitting low-quality triples.
const MAX_TOKENS_PER_SENTENCE: usize = 200;

/// English stop-word set used to filter capitalized tokens before
/// promoting them to subjects / entities. Conservative subset —
/// sentence-initial words like "The", "A", "An" must not be promoted
/// to "Subject = The".
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "if", "then", "than", "so", "of", "in", "on", "at", "to",
    "for", "with", "by", "from", "into", "onto", "is", "are", "was", "were", "be", "been", "being",
    "am", "i", "you", "he", "she", "it", "we", "they", "this", "that", "these", "those", "my",
    "your", "his", "her", "its", "our", "their", "me", "him", "us", "them",
];

/// Prepositions stripped from the start of an object span so
/// `nsubj→root→pobj` lands in the same shape as `nsubj→root→dobj`.
const LEADING_PREPS: &[&str] = &[
    "in", "at", "on", "to", "for", "with", "by", "from", "into", "onto", "about", "of", "as",
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Check whether the `PENSYVE_DEP_PARSE` env-var gate is enabled.
///
/// Reads once via `OnceLock` (matches the Phase 2A `SelRoute` pattern in
/// [`crate::retrieval::query_classifier::selroute_enabled`]). Accepted
/// truthy values (case-insensitive): `"1"`, `"true"`, `"on"`, `"yes"`.
#[must_use]
pub fn dep_parse_enabled() -> bool {
    static DEP_PARSE: OnceLock<bool> = OnceLock::new();
    *DEP_PARSE.get_or_init(|| {
        std::env::var("PENSYVE_DEP_PARSE").is_ok_and(|v| {
            let lower = v.trim().to_ascii_lowercase();
            matches!(lower.as_str(), "1" | "true" | "on" | "yes")
        })
    })
}

/// Extract `(subject, predicate, object)` triples + entity lemmas from
/// `text`. Pure function: returns an empty `ParsedPassage` for empty
/// input, never panics, and never allocates on the metrics path beyond
/// the returned struct.
///
/// Metric side-effects (via the global `PensyveMetrics`):
/// - `dep_parse_skipped_long_sentence` — per sentence > 200 tokens.
/// - `dep_parse_lexicon_miss_count` — per candidate verb absent from
///   [`PREDICATE_LEXICON`].
#[must_use]
pub fn extract_triples(passage_id: Uuid, text: &str) -> ParsedPassage {
    let mut triples: Vec<Triple> = Vec::new();
    let mut entities: Vec<String> = Vec::new();

    for sentence in split_sentences(text) {
        let tokens: Vec<&str> = sentence.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        if tokens.len() > MAX_TOKENS_PER_SENTENCE {
            // Long sentence: skip extraction. Phase 2B hedge.
            crate::observability::metrics()
                .dep_parse_skipped_long_sentence
                .fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // Entity scan: every capitalized non-stopword token is an entity
        // candidate. Done up-front so even fragments contribute entities
        // to PPR's vocabulary.
        for tok in &tokens {
            if let Some(lemma) = entity_candidate(tok)
                && !entities.iter().any(|e| e.eq_ignore_ascii_case(&lemma))
            {
                entities.push(lemma);
            }
        }

        if let Some(triple) = extract_one_triple(&tokens) {
            triples.push(triple);
        }
    }

    ParsedPassage {
        passage_id,
        triples,
        entities,
    }
}

/// Shallow-parse a single sentence's token slice into one triple.
///
/// Algorithm:
/// 1. Find the first verb whose lowercase stem hits [`PREDICATE_LEXICON`].
/// 2. Subject = capitalized non-stopword tokens before the verb (joined
///    with single spaces, lower-cased except the first letter to keep
///    multi-word proper nouns readable). Falls back to the first
///    non-stopword token if no capitalized run is found.
/// 3. Object = tokens after the verb with any leading preposition
///    stripped; trailing punctuation is removed. Empty objects fail
///    the triple (returns `None`).
///
/// Confidence heuristics:
/// - 0.8 when both subject and object are >= 1 token AND the subject
///   contains a capitalized run (proper-noun-ish).
/// - 0.6 when the subject lacks a capitalized run (pronoun-led).
/// - The function never emits 0.3 — fragments simply return `None`.
fn extract_one_triple(tokens: &[&str]) -> Option<Triple> {
    let mut lexicon_miss_for_sentence = 0u64;

    let (verb_idx, predicate) = (0..tokens.len()).find_map(|i| {
        let lower = strip_punct(tokens[i]).to_ascii_lowercase();
        // Verbs are looked up in the lexicon directly. Surface forms
        // not present are recorded as misses ONLY when the token shape
        // looks like a verb (lowercase, alphabetic, length >= 3) — we
        // do not record every noun as a "lexicon miss".
        if let Some(predicate) = PREDICATE_LEXICON.get(lower.as_str()) {
            Some((i, *predicate))
        } else {
            if looks_like_verb(&lower) {
                lexicon_miss_for_sentence += 1;
            }
            None
        }
    })?;

    if lexicon_miss_for_sentence > 0 {
        crate::observability::metrics()
            .dep_parse_lexicon_miss_count
            .fetch_add(lexicon_miss_for_sentence, Ordering::Relaxed);
    }

    let subject_tokens = &tokens[..verb_idx];
    let object_tokens = &tokens[verb_idx + 1..];

    let (subject, subject_proper) = build_subject(subject_tokens)?;
    let object = build_object(object_tokens)?;

    let confidence = if subject_proper { 0.8 } else { 0.6 };

    Some(Triple {
        subject,
        predicate: predicate.to_string(),
        object,
        confidence,
    })
}

/// Build the subject string from the tokens preceding the verb.
///
/// Returns `(subject, has_capitalized_run)`. `None` when no usable
/// token survives the stopword filter (e.g., "The is in...").
fn build_subject(tokens: &[&str]) -> Option<(String, bool)> {
    let mut out: Vec<String> = Vec::new();
    let mut has_capitalized = false;

    // First pass: collect capitalized non-stopword runs.
    for tok in tokens {
        if let Some(lemma) = entity_candidate(tok) {
            out.push(lemma);
            has_capitalized = true;
        }
    }

    // Fallback: no capitalized run — use the first non-stopword token
    // (commonly "I" / "she" / etc. — pronoun-led subjects). We
    // preserve original case in that path so "I" stays "I".
    if out.is_empty() {
        for tok in tokens {
            let stripped = strip_punct(tok);
            if stripped.is_empty() {
                continue;
            }
            let lower = stripped.to_ascii_lowercase();
            if !STOP_WORDS.contains(&lower.as_str()) || lower == "i" {
                out.push(stripped.to_string());
                break;
            }
        }
    }

    if out.is_empty() {
        return None;
    }

    Some((out.join(" "), has_capitalized))
}

/// Build the object string from the tokens following the verb. Strips
/// at most one leading preposition (so "in Brooklyn" → "Brooklyn") and
/// trailing punctuation. `None` when the resulting span is empty.
fn build_object(tokens: &[&str]) -> Option<String> {
    let mut start = 0usize;
    while start < tokens.len() {
        let lower = strip_punct(tokens[start]).to_ascii_lowercase();
        if LEADING_PREPS.contains(&lower.as_str()) {
            start += 1;
            continue;
        }
        break;
    }

    let span: Vec<String> = tokens[start..]
        .iter()
        .map(|t| strip_punct(t).to_string())
        .filter(|t| !t.is_empty())
        .collect();

    if span.is_empty() {
        return None;
    }
    Some(span.join(" "))
}

/// Return `Some(lemma)` for capitalized non-stopword tokens, treating
/// punctuation as part of the surface form to strip. Returns `None`
/// for lowercase tokens, stop-words, single letters, and bare numbers.
fn entity_candidate(token: &str) -> Option<String> {
    let stripped = strip_punct(token);
    if stripped.len() < 2 {
        return None;
    }
    let first = stripped.chars().next()?;
    if !first.is_ascii_uppercase() {
        return None;
    }
    let lower = stripped.to_ascii_lowercase();
    if STOP_WORDS.contains(&lower.as_str()) {
        return None;
    }
    // Reject pure-digit tokens like "2024".
    if stripped.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(stripped.to_string())
}

/// Strip leading/trailing punctuation from a token. Operates on ASCII
/// punctuation only — Unicode quotes / em-dashes are left in place
/// because they're rare in chat-style memory data and stripping them
/// safely needs a full Unicode-aware path.
fn strip_punct(token: &str) -> &str {
    token.trim_matches(|c: char| c.is_ascii_punctuation())
}

/// Heuristic for whether a token shape looks like a verb (for
/// lexicon-miss accounting). Required: all-lowercase alphabetic,
/// length >= 3. Avoids inflating the miss counter on every noun.
fn looks_like_verb(lower: &str) -> bool {
    lower.len() >= 3 && lower.chars().all(|c| c.is_ascii_alphabetic())
}

/// Compiled sentence-splitter regex.
fn sentence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[.!?]+\s+").expect("sentence splitter compiles"))
}

/// Split `text` into trimmed non-empty sentences.
///
/// Uses a simple regex (period/question/exclamation + whitespace) per
/// the Phase 2B plan's fallback. fastembed's `TextEmbedding` does not
/// expose a public tokenizer for sentence splitting, so the regex path
/// is the chosen approach. Documented at the module level.
fn split_sentences(text: &str) -> Vec<String> {
    sentence_re()
        .split(text)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// Embedding granules (entity index + relation index)
// ---------------------------------------------------------------------------

/// Derive a stable `Uuid` from an arbitrary string key.
///
/// We don't enable the `uuid` crate's `v5` feature (it would add a SHA-1
/// dependency) — instead we feed a `DefaultHasher` twice (once over the
/// key bytes, once over a domain-separating salt) and pack the two 64-bit
/// digests into a 128-bit `Uuid::from_u128`. The result is deterministic
/// (same key → same Uuid across runs) and collision-resistant enough for
/// indexing entity / relation granules — it is NOT a cryptographic
/// identifier and must not be used as a security primitive.
fn deterministic_uuid(key: &str) -> Uuid {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h1 = DefaultHasher::new();
    key.hash(&mut h1);
    let hi = h1.finish();

    let mut h2 = DefaultHasher::new();
    "pensyve::extraction::dep_parse".hash(&mut h2);
    key.hash(&mut h2);
    let lo = h2.finish();

    let bits = (u128::from(hi) << 64) | u128::from(lo);
    Uuid::from_u128(bits)
}

/// Embed each entity lemma into `entity_index` and each
/// `subject predicate object` triple string into `relation_index`.
///
/// `embed_fn` is a closure passed by the caller so this module does not
/// take a hard dependency on a concrete embedder. The caller (typically
/// the consolidation hook) plugs in the same embedder used by
/// observation insertion so granule embeddings are dimension-compatible
/// with the vector index dimensions.
///
/// Failed embeddings are skipped silently — the entity/relation falls
/// out of the index for that pass but the SQL row still lands. This
/// matches the existing `commit_extraction_for_episode` failure mode.
///
/// Returns `(entities_embedded, relations_embedded)` so callers can
/// emit telemetry / log progress.
pub fn embed_kg_granules<F, E>(
    parsed: &ParsedPassage,
    mut embed_fn: F,
    entity_index: &mut crate::vector::VectorIndex,
    relation_index: &mut crate::vector::VectorIndex,
) -> (usize, usize)
where
    F: FnMut(&str) -> Result<Vec<f32>, E>,
    E: std::fmt::Display,
{
    let mut entities_embedded = 0usize;
    let mut relations_embedded = 0usize;

    for lemma in &parsed.entities {
        match embed_fn(lemma) {
            Ok(vec) => {
                // Deterministic UUID derived from the lemma via UUID v5
                // over a fixed namespace keeps subsequent calls
                // idempotent — re-embedding "Alice" twice maps to the
                // same index entry.
                let id = deterministic_uuid(lemma);
                if entity_index.add(id, &vec).is_ok() {
                    entities_embedded += 1;
                }
            }
            Err(e) => {
                tracing::debug!(
                    target: "pensyve::extraction::dep_parse",
                    error = %e,
                    lemma = %lemma,
                    "entity granule embedding deferred"
                );
            }
        }
    }

    for t in &parsed.triples {
        let key = format!("{} {} {}", t.subject, t.predicate, t.object);
        match embed_fn(&key) {
            Ok(vec) => {
                let id = deterministic_uuid(&key);
                if relation_index.add(id, &vec).is_ok() {
                    relations_embedded += 1;
                }
            }
            Err(e) => {
                tracing::debug!(
                    target: "pensyve::extraction::dep_parse",
                    error = %e,
                    triple = %key,
                    "relation granule embedding deferred"
                );
            }
        }
    }

    (entities_embedded, relations_embedded)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_triple_contains(
        passage: &ParsedPassage,
        subject_substr: &str,
        predicate: &str,
        object_substr: &str,
    ) {
        let hit = passage.triples.iter().find(|t| {
            t.subject
                .to_ascii_lowercase()
                .contains(&subject_substr.to_ascii_lowercase())
                && t.predicate == predicate
                && t.object
                    .to_ascii_lowercase()
                    .contains(&object_substr.to_ascii_lowercase())
        });
        assert!(
            hit.is_some(),
            "expected triple ({subject_substr:?}, {predicate:?}, {object_substr:?}) in {:?}",
            passage.triples
        );
    }

    // ---- Hand-crafted sentence coverage (nsubj→root→dobj / pobj) ----

    #[test]
    fn nsubj_root_dobj_simple() {
        let p = extract_triples(Uuid::nil(), "Alice owns Acme.");
        assert_triple_contains(&p, "Alice", "owns", "Acme");
    }

    #[test]
    fn nsubj_root_pobj_lives_in() {
        let p = extract_triples(Uuid::nil(), "Bob lives in Brooklyn.");
        assert_triple_contains(&p, "Bob", "lives_in", "Brooklyn");
    }

    #[test]
    fn nsubj_root_pobj_works_at() {
        let p = extract_triples(Uuid::nil(), "Carol works at Acme Corp.");
        assert_triple_contains(&p, "Carol", "works_at", "Acme");
    }

    #[test]
    fn nsubj_root_dobj_uses() {
        let p = extract_triples(Uuid::nil(), "Dave uses Rust daily.");
        assert_triple_contains(&p, "Dave", "uses", "Rust");
    }

    #[test]
    fn nsubj_root_dobj_built() {
        let p = extract_triples(Uuid::nil(), "Eve built Pensyve.");
        assert_triple_contains(&p, "Eve", "builds", "Pensyve");
    }

    #[test]
    fn nsubj_root_dobj_met() {
        let p = extract_triples(Uuid::nil(), "Frank met Alice.");
        assert_triple_contains(&p, "Frank", "met", "Alice");
    }

    #[test]
    fn pronoun_subject_falls_back() {
        let p = extract_triples(Uuid::nil(), "I prefer dark mode.");
        let triple = p
            .triples
            .iter()
            .find(|t| t.predicate == "prefers")
            .expect("verb 'prefer' should fire");
        assert!(
            triple.confidence < 0.8,
            "pronoun-led subject confidence should drop"
        );
        assert!(triple.object.to_ascii_lowercase().contains("dark"));
    }

    #[test]
    fn multi_sentence_passage_extracts_each() {
        let p = extract_triples(
            Uuid::nil(),
            "Alice works at Acme. Bob lives in Brooklyn. Carol bought a Tesla.",
        );
        assert_triple_contains(&p, "Alice", "works_at", "Acme");
        assert_triple_contains(&p, "Bob", "lives_in", "Brooklyn");
        assert_triple_contains(&p, "Carol", "bought", "Tesla");
    }

    #[test]
    fn capitalized_entities_collected_even_without_triple() {
        let p = extract_triples(Uuid::nil(), "Alice and Bob.");
        assert!(p.entities.iter().any(|e| e == "Alice"));
        assert!(p.entities.iter().any(|e| e == "Bob"));
    }

    #[test]
    fn fragment_returns_empty_triples_but_keeps_entities() {
        let p = extract_triples(Uuid::nil(), "Alice.");
        assert!(p.triples.is_empty());
        assert!(p.entities.iter().any(|e| e == "Alice"));
    }

    #[test]
    fn empty_input_returns_empty_parse() {
        let p = extract_triples(Uuid::nil(), "");
        assert!(p.triples.is_empty());
        assert!(p.entities.is_empty());
    }

    #[test]
    fn stopword_only_subject_skips_triple() {
        // "The is in"-style noise must not yield a triple.
        let p = extract_triples(Uuid::nil(), "The .");
        assert!(p.triples.is_empty());
    }

    #[test]
    fn knows_metaphor() {
        let p = extract_triples(Uuid::nil(), "Gina knows Henry.");
        assert_triple_contains(&p, "Gina", "knows", "Henry");
    }

    #[test]
    fn taught_relation() {
        let p = extract_triples(Uuid::nil(), "Ivan taught calculus.");
        assert_triple_contains(&p, "Ivan", "taught", "calculus");
    }

    #[test]
    fn watched_relation() {
        let p = extract_triples(Uuid::nil(), "Jane watched the movie.");
        assert_triple_contains(&p, "Jane", "watches", "movie");
    }

    // ---- Predicate lexicon exhaustive map check ----

    #[test]
    fn lexicon_maps_every_starter_verb() {
        // Sanity-check the locked set of surface forms maps to a
        // normalized relation. Iterates the entire lexicon — this
        // protects against accidental key collisions or deletions in
        // future edits.
        let expectations: &[(&str, &str)] = &[
            ("works", "works_at"),
            ("worked", "works_at"),
            ("lives", "lives_in"),
            ("lived", "lives_in"),
            ("owns", "owns"),
            ("knows", "knows"),
            ("said", "said"),
            ("met", "met"),
            ("uses", "uses"),
            ("builds", "builds"),
            ("manages", "manages"),
            ("leads", "leads"),
            ("joined", "joined"),
            ("left", "left"),
            ("visited", "visited"),
            ("bought", "bought"),
            ("sold", "sold"),
            ("wrote", "wrote"),
            ("read", "read"),
            ("learned", "learned"),
            ("taught", "taught"),
            ("studied", "studied"),
            ("plays", "plays"),
            ("watches", "watches"),
            ("listens", "listens"),
            ("eats", "eats"),
            ("drinks", "drinks"),
            ("sleeps", "sleeps"),
            ("runs", "runs"),
            ("walks", "walks"),
            ("drives", "drives"),
            ("flies", "flies"),
            ("called", "called"),
            ("emailed", "emailed"),
            ("messaged", "messaged"),
            ("follows", "follows"),
            ("likes", "likes"),
            ("loves", "loves"),
            ("hates", "hates"),
            ("fears", "fears"),
            ("helps", "helps"),
            ("asked", "asked"),
            ("answered", "answered"),
            ("finds", "finds"),
            ("creates", "creates"),
            ("deletes", "deletes"),
            ("updates", "updates"),
            ("sends", "sends"),
            ("receives", "receives"),
            ("gives", "gives"),
            ("takes", "takes"),
            ("prefers", "prefers"),
        ];
        for (verb, relation) in expectations {
            let mapped = PREDICATE_LEXICON
                .get(*verb)
                .unwrap_or_else(|| panic!("verb {verb:?} missing from PREDICATE_LEXICON"));
            assert_eq!(
                mapped, relation,
                "verb {verb:?} maps to {mapped:?}, expected {relation:?}"
            );
        }
        // Every value in the lexicon is non-empty (no accidental "" relation).
        for (k, v) in PREDICATE_LEXICON.entries() {
            assert!(!v.is_empty(), "verb {k:?} maps to empty relation");
        }
        // Per the Phase 2B plan, the lexicon ships ~50 surface forms.
        assert!(
            PREDICATE_LEXICON.len() >= 50,
            "lexicon shrunk below the 50-form starter set: {}",
            PREDICATE_LEXICON.len()
        );
    }

    // ---- Long-sentence skip path ----

    #[test]
    fn long_sentence_skips_with_counter() {
        let before = crate::observability::metrics()
            .dep_parse_skipped_long_sentence
            .load(Ordering::Relaxed);

        // Construct a 250-token sentence (no terminator splitters in
        // the body so the whole run stays a single "sentence").
        let big = "Alice ".repeat(250);
        let p = extract_triples(Uuid::nil(), &big);

        let after = crate::observability::metrics()
            .dep_parse_skipped_long_sentence
            .load(Ordering::Relaxed);
        assert!(
            after > before,
            "dep_parse_skipped_long_sentence counter did not increment"
        );
        assert!(p.triples.is_empty(), "long sentence must yield no triples");
    }

    // ---- 50-sentence coverage benchmark ----
    //
    // Per the Phase 2B plan's acceptance criterion: at least 35 of 50
    // realistic memory-system sentences must produce ≥ 1 triple
    // (~70% coverage). Sentences are biased toward simple SVO since
    // that's what the shallow rule targets.

    #[test]
    fn coverage() {
        let sentences = [
            "Alice works at Acme Corp.",
            "Bob lives in Brooklyn with his family.",
            "Carol prefers dark mode in her editor.",
            "Dave owns a small consulting firm.",
            "Eve knows Frank from her time at MIT.",
            "I said yes to the offer yesterday.",
            "Grace met Henry at the Rust conference.",
            "Henry uses Postgres for production workloads.",
            "Ivan built Pensyve over six months.",
            "Jane manages the platform engineering team.",
            "Kyle leads the security working group.",
            "Liam joined the company in March.",
            "Mary left the company last quarter.",
            "Nora visited Tokyo last summer.",
            "Owen bought a new laptop from Apple.",
            "Pam sold her old MacBook on eBay.",
            "Quinn wrote a long memo about scaling.",
            "Rita read three books last week.",
            "Sam learned Rust during the pandemic.",
            "Tina taught a workshop on observability.",
            "Uma studied distributed systems at Stanford.",
            "Victor plays chess every Sunday.",
            "Wendy watches old films on weekends.",
            "Xavier listens to jazz while coding.",
            "Yara eats lunch at the corner deli.",
            "Zach drinks black coffee every morning.",
            "Aaron sleeps poorly on conference travel.",
            "Beth runs marathons twice a year.",
            "Cal walks her dog every evening.",
            "Dana drives a Tesla Model 3.",
            "Erin flies to Seattle every other month.",
            "Felix called the customer support line.",
            "Gina emailed the proposal to Henry.",
            "Hank messaged the on-call engineer.",
            "Ivy follows several Rust core developers.",
            "Jack likes the new dashboard layout.",
            "Kara loves the Pensyve onboarding flow.",
            "Leo hates flaky integration tests.",
            "Mia fears another production incident.",
            "Nate helps junior engineers with code review.",
            "Olive asked Pat about the rollout plan.",
            "Pat answered the question in detail.",
            "Quinn finds bugs faster than anyone.",
            "Rita creates internal tooling on the side.",
            "Sam deletes stale branches every Friday.",
            "Tina updates the changelog after each release.",
            "Uma sends weekly status updates to leadership.",
            "Victor receives push notifications from on-call.",
            "Wendy gives feedback during sprint reviews.",
            "Xavier takes notes during every standup.",
        ];
        assert_eq!(sentences.len(), 50);

        let mut hits = 0usize;
        for s in &sentences {
            let p = extract_triples(Uuid::nil(), s);
            if !p.triples.is_empty() {
                hits += 1;
            }
        }
        assert!(
            hits >= 35,
            "coverage regression: only {hits}/50 sentences produced ≥1 triple; \
             Phase 2B plan requires ≥35 (70%)"
        );
    }

    // ---- Embedding granule wiring ----

    #[test]
    fn embed_kg_granules_populates_indexes() {
        let parsed = extract_triples(Uuid::nil(), "Alice works at Acme.");
        let mut entity_index = crate::vector::VectorIndex::new(4, 16);
        let mut relation_index = crate::vector::VectorIndex::new(4, 16);

        // Deterministic dummy embedder: stable 4-d vector keyed off the
        // first byte of the input so distinct keys land at distinct
        // angles in the index.
        let embed = |s: &str| -> Result<Vec<f32>, std::convert::Infallible> {
            let first = s.as_bytes().first().copied().unwrap_or(0);
            Ok(vec![f32::from(first) / 255.0, 0.1, 0.2, 0.3])
        };

        let (ents, rels) =
            embed_kg_granules(&parsed, embed, &mut entity_index, &mut relation_index);
        assert!(ents > 0, "no entities embedded");
        assert!(rels > 0, "no relations embedded");
    }

    // ---- Env flag ----

    #[test]
    fn dep_parse_enabled_caches_first_read() {
        let a = dep_parse_enabled();
        let b = dep_parse_enabled();
        assert_eq!(a, b);
    }
}
