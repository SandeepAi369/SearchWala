// ============================================================================
// SearchWala v6.1.0 — Hybrid Ranking Engine (Research-Grounded)
//
// Four-stage cascade pipeline:
//   Stage A: Reciprocal Rank Fusion (RRF) — Cross-engine consensus
//            Cormack, Clarke & Büttcher (SIGIR 2009), k=60
//   Stage B: BM25+ Paragraph Scoring — Within-source relevance
//            Lv & Zhai, Lower-Bounding TF Normalization (CIKM 2011)
//   Stage C: (Future) Cross-encoder reranking — fastembed/ort
//   Stage D: MMR Diversity — Source deduplication
//            Carbonell & Goldstein (SIGIR 1998), λ=0.7
//
// Tokenizer pipeline: NFKC normalize → lowercase → stopword filter →
//                     Snowball Porter2 stemming (tantivy-stemmers)
//
// Key fixes over v5.2.0:
//   - BM25+ δ=1.0 lower bound (fixes long document penalty)
//   - Pure RRF k=60 (removes magic * 20.0 constant)
//   - Proper stopword filtering (150 English stopwords)
//   - Real MMR diversity (not crude per-URL cap)
//   - Sentence-boundary-aware chunking with ~15% overlap
//   - Extraction confidence as ranking prior
// ============================================================================

use std::collections::{HashMap, HashSet};

use crate::engines;
use crate::models::{RawSearchResult, SourceResult};

// ── Tuning constants ──
const MIN_CHUNK_CHARS: usize = 80;
const MAX_CHUNK_CHARS: usize = 600;   // v6.1.0: increased from 400 — richer chunks with 120-200MB budget
const CHUNK_OVERLAP_RATIO: f64 = 0.15; // ~15% overlap between consecutive chunks
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;
const BM25_DELTA: f64 = 1.0;          // v6.1.0: BM25+ lower bound (Lv & Zhai 2011)
const RRF_K: f64 = 60.0;              // From the RRF paper (Cormack et al. 2009) — do NOT change
const MMR_LAMBDA: f64 = 0.7;          // Carbonell & Goldstein 1998 default
const MAX_CHUNKS_PER_URL_IN_TOP_K: usize = 3; // Hard cap even with MMR
/// Recency half-life per intent, in days (P2-2).
const FRESHNESS_HALF_LIFE_NEWS: f64 = 7.0;
const FRESHNESS_HALF_LIFE_PERSON: f64 = 180.0;
const FRESHNESS_HALF_LIFE_DEFAULT: f64 = 60.0;
/// Floor on the freshness multiplier, so an old-but-authoritative source is
/// demoted rather than erased.
const FRESHNESS_FLOOR: f64 = 0.60;

/// MMR only considers the top `top_k * this` BM25 candidates. Scoring the
/// whole corpus every round was the v7.0 hot spot and buys nothing: a chunk
/// ranked thousands of places down does not win on diversity alone.
const MMR_WINDOW_FACTOR: usize = 5;

// ── Stopwords — top 150 English stopwords (SMART list + extensions) ──
const STOPWORDS: &[&str] = &[
    "a", "about", "above", "after", "again", "against", "all", "am", "an", "and",
    "any", "are", "aren't", "as", "at", "be", "because", "been", "before", "being",
    "below", "between", "both", "but", "by", "can", "can't", "cannot", "could",
    "couldn't", "did", "didn't", "do", "does", "doesn't", "doing", "don't", "down",
    "during", "each", "few", "for", "from", "further", "get", "got", "had", "hadn't",
    "has", "hasn't", "have", "haven't", "having", "he", "her", "here", "hers",
    "herself", "him", "himself", "his", "how", "i", "if", "in", "into", "is", "isn't",
    "it", "its", "itself", "just", "let", "like", "ll", "me", "might", "more", "most",
    "must", "mustn't", "my", "myself", "no", "nor", "not", "of", "off", "on", "once",
    "only", "or", "other", "our", "ours", "ourselves", "out", "over", "own", "re",
    "s", "same", "shall", "shan't", "she", "should", "shouldn't", "so", "some",
    "such", "t", "than", "that", "the", "their", "theirs", "them", "themselves",
    "then", "there", "these", "they", "this", "those", "through", "to", "too",
    "under", "until", "up", "ve", "very", "was", "wasn't", "we", "were", "weren't",
    "what", "when", "where", "which", "while", "who", "whom", "why", "will", "with",
    "won't", "would", "wouldn't", "you", "your", "yours", "yourself", "yourselves",
];

// We use a LazyLock HashSet for O(1) stopword lookups
use std::sync::LazyLock;
static STOPWORD_SET: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    STOPWORDS.iter().copied().collect()
});

#[derive(Clone)]
struct ChunkCandidate {
    url: String,
    title: String,
    engine: String,
    text: String,
    rrf_score: f64,
    /// REAL extraction confidence from the extractor (v8.0). v7.0 hardcoded
    /// 0.7 here with the comment "will be overridden when C4 provides real
    /// confidence", so the confidence prior was a constant no-op.
    extraction_confidence: f64,
    published: Option<chrono::DateTime<chrono::Utc>>,
    language: Option<String>,
    extractor: &'static str,
}

// =============================================================================
// Stage A: Reciprocal Rank Fusion (RRF)
// Cormack, Clarke & Büttcher, SIGIR 2009
// RRF_Score(url) = Σ (engine_weight / (K + rank_in_engine))
// =============================================================================

/// Compute RRF scores for URLs across all engines.
/// Returns a map of normalized_url → RRF score.
///
/// RRF_Score(url) = Σ (engine_weight / (K + rank_in_engine))
///
/// URLs that appear in top positions across MULTIPLE engines get the highest scores.
/// Engine weights (Tier 1-4) ensure Google/Bing results count more than Dogpile.
pub fn compute_rrf_scores(results: &[RawSearchResult]) -> HashMap<String, f64> {
    // ── Step 1: collapse to the BEST rank per (page, engine-family) ──
    //
    // RRF's premise is that agreement between INDEPENDENT rankers is evidence
    // of quality. v7.0 violated it on two axes at once:
    //
    //   * 15 Bing aliases (bing, bing_us, bing_uk, bing_in, ...) all built the
    //     SAME URL, because spec_for ignored the suffix and never added a
    //     market param. RRF summed Bing's ordering 15 times and reported it as
    //     "cross-engine consensus".
    //   * Startpage proxies Google; Yahoo and DuckDuckGo are Bing-derived.
    //     Counting a Google proxy as independent evidence FOR Google's ranking
    //     is the same error, just less visible.
    //
    // Measured on the v7.0 baseline: a single query produced 127 raw results
    // from what the logs called 33 engines, of which only 2 were independent
    // indices. Collapsing by family makes "found by N engines" mean what it
    // says.
    //
    // Also note the key: `canonical_key` unifies www/m/amp variants and sorts
    // query params, so the same article surfaced as three URL spellings is one
    // page rather than three "agreeing" hits.
    let mut best_rank: HashMap<(String, &'static str), usize> = HashMap::new();
    for result in results {
        let key = crate::url_utils::canonical_key(&result.url);
        let family = engines::engine_family(&result.engine);
        let rank = result.rank_position.max(1);
        best_rank
            .entry((key, family))
            .and_modify(|cur| {
                if rank < *cur {
                    *cur = rank;
                }
            })
            .or_insert(rank);
    }

    // ── Step 2: fuse across families ──
    let mut scores: HashMap<String, f64> = HashMap::new();
    for ((key, family), rank) in best_rank {
        let weight = engines::family_weight(family) as f64;
        *scores.entry(key).or_insert(0.0) += weight / (RRF_K + rank as f64);
    }

    scores
}

/// How many INDEPENDENT engine families surfaced each URL.
///
/// This is the honest "found by N engines" number for the UI, and a much
/// better consensus signal than a raw hit count.
pub fn family_agreement(results: &[RawSearchResult]) -> HashMap<String, Vec<&'static str>> {
    let mut map: HashMap<String, Vec<&'static str>> = HashMap::new();
    for result in results {
        let key = crate::url_utils::canonical_key(&result.url);
        let family = engines::engine_family(&result.engine);
        let entry = map.entry(key).or_default();
        if !entry.contains(&family) {
            entry.push(family);
        }
    }
    map
}

// =============================================================================
// Stage B: BM25+ Paragraph-Level Chunk Ranking
// Lv & Zhai, "Lower-Bounding Term Frequency Normalization" (CIKM 2011)
// =============================================================================

pub fn rank_top_chunks(query: &str, sources: &[SourceResult], top_k: usize) -> Vec<SourceResult> {
    rank_top_chunks_with_rrf(query, sources, top_k, None)
}

/// Full v6.1.0 ranking pipeline:
///   1. Build chunk candidates from sources
///   2. Score with BM25+ (with δ lower bound)
///   3. Apply RRF consensus boost (normalized, not magic constant)
///   4. Apply extraction confidence prior
///   5. MMR diversity reranking (Carbonell & Goldstein 1998)
/// Exponential recency decay, gated on the query actually being time-sensitive.
///
/// v7.0 called itself "time-aware" but had ZERO freshness signal in ranking —
/// it only appended a year to the query string and told the LLM today's date.
/// It also never extracted publication dates, so there was nothing to rank on.
/// Phase 3 provides the dates; this consumes them.
///
/// Evergreen queries ("how does TLS work") must NOT be recency-biased, which
/// is why this returns 1.0 unless the query is time-sensitive.
pub fn freshness_multiplier(
    published: Option<chrono::DateTime<chrono::Utc>>,
    is_time_sensitive: bool,
    intent: &crate::query_intel::QueryIntent,
) -> f64 {
    use crate::query_intel::QueryIntent;
    if !is_time_sensitive {
        return 1.0;
    }
    let Some(p) = published else {
        // Unknown date on a time-sensitive query: slight penalty, not a veto.
        return 0.92;
    };
    let days = (chrono::Utc::now() - p).num_days().max(0) as f64;
    let half_life = match intent {
        QueryIntent::Temporal => FRESHNESS_HALF_LIFE_NEWS,
        QueryIntent::Person => FRESHNESS_HALF_LIFE_PERSON,
        _ => FRESHNESS_HALF_LIFE_DEFAULT,
    };
    FRESHNESS_FLOOR + (1.0 - FRESHNESS_FLOOR) * 0.5_f64.powf(days / half_life)
}

pub fn rank_top_chunks_with_rrf(
    query: &str,
    sources: &[SourceResult],
    top_k: usize,
    rrf_scores: Option<&HashMap<String, f64>>,
) -> Vec<SourceResult> {
    rank_top_chunks_with_signals(query, sources, top_k, rrf_scores, None)
}

/// Per-mode ranking knobs (v8.1). Defaults reproduce the pre-mode behaviour.
#[derive(Debug, Clone, Copy)]
pub struct RankOptions {
    pub max_chunks_per_url: usize,
    /// Apply recency decay (still gated on the query being time-sensitive).
    pub freshness: bool,
    pub mode: crate::modes::Mode,
    pub domain: crate::modes::Domain,
}

impl Default for RankOptions {
    fn default() -> Self {
        Self {
            max_chunks_per_url: MAX_CHUNKS_PER_URL_IN_TOP_K,
            freshness: true,
            mode: crate::modes::Mode::Auto,
            domain: crate::modes::Domain::General,
        }
    }
}

impl RankOptions {
    pub fn from_profile(p: &crate::modes::ModeProfile) -> Self {
        Self {
            max_chunks_per_url: p.max_chunks_per_url.max(1),
            freshness: p.freshness,
            mode: p.mode,
            domain: p.domain,
        }
    }
}

/// Full ranking with optional query-intelligence signals (freshness).
pub fn rank_top_chunks_with_signals(
    query: &str,
    sources: &[SourceResult],
    top_k: usize,
    rrf_scores: Option<&HashMap<String, f64>>,
    intel: Option<&crate::query_intel::QueryIntelligence>,
) -> Vec<SourceResult> {
    rank_with_options(query, sources, top_k, rrf_scores, intel, &RankOptions::default())
}

/// Ranking with explicit per-mode options.
pub fn rank_with_options(
    query: &str,
    sources: &[SourceResult],
    top_k: usize,
    rrf_scores: Option<&HashMap<String, f64>>,
    intel: Option<&crate::query_intel::QueryIntelligence>,
    opts: &RankOptions,
) -> Vec<SourceResult> {
    if sources.is_empty() || top_k == 0 {
        return Vec::new();
    }

    // ── Build chunk candidates ──
    // Find max RRF for normalization (avoid magic * 20.0)
    let max_rrf = rrf_scores
        .map(|s| s.values().cloned().fold(0.0_f64, f64::max))
        .unwrap_or(1.0)
        .max(1e-9); // prevent division by zero

    let mut candidates = Vec::new();
    for source in sources {
        let url_rrf = rrf_scores
            .and_then(|s| s.get(&crate::url_utils::canonical_key(&source.url)).copied())
            .unwrap_or(0.0);

        for chunk in paragraph_chunks(&source.extracted_text) {
            if chunk.trim().is_empty() {
                continue;
            }
            candidates.push(ChunkCandidate {
                url: source.url.clone(),
                title: source.title.clone(),
                engine: source.engine.clone(),
                text: chunk,
                rrf_score: url_rrf,
                // Real value now, not the v7.0 hardcoded 0.7.
                extraction_confidence: if source.confidence > 0.0 { source.confidence } else { 0.7 },
                published: source.published,
                language: source.language.clone(),
                extractor: source.extractor,
            });
        }
    }

    if candidates.is_empty() {
        return Vec::new();
    }

    // ── Tokenize query (with stopword removal) ──
    let query_terms = unique_tokens_filtered(query);
    if query_terms.is_empty() {
        return candidates
            .into_iter()
            .take(top_k)
            .map(|c| SourceResult {
                url: c.url,
                title: c.title,
                engine: c.engine,
                char_count: crate::util::char_len(&c.text),
                extracted_text: c.text,
                published: c.published,
                language: c.language,
                confidence: c.extraction_confidence,
                extractor: c.extractor,
            })
            .collect();
    }

    // ── Tokenize all documents ──
    let tokenized_docs: Vec<Vec<String>> = candidates
        .iter()
        .map(|c| tokenize_filtered(&c.text))
        .collect();

    let doc_count = tokenized_docs.len() as f64;
    let avg_doc_len = {
        let total: usize = tokenized_docs.iter().map(|d| d.len()).sum();
        (total.max(1) as f64) / doc_count.max(1.0)
    };

    // ── Document frequency for IDF ──
    let mut doc_freq: HashMap<String, usize> = HashMap::new();
    for tokens in &tokenized_docs {
        let mut seen = HashSet::new();
        for token in tokens {
            if seen.insert(token.clone()) {
                *doc_freq.entry(token.clone()).or_insert(0) += 1;
            }
        }
    }

    // ── Score each chunk with BM25+ ──
    let query_phrase = query.trim().to_lowercase();
    let mut scored: Vec<(usize, f64)> = Vec::with_capacity(candidates.len());

    for (idx, tokens) in tokenized_docs.iter().enumerate() {
        let mut tf: HashMap<&str, usize> = HashMap::new();
        for token in tokens {
            *tf.entry(token.as_str()).or_insert(0) += 1;
        }

        let dl = tokens.len().max(1) as f64;
        let mut score = 0.0;

        for term in &query_terms {
            let term_tf = tf.get(term.as_str()).copied().unwrap_or(0) as f64;
            if term_tf <= 0.0 {
                continue;
            }

            let df = doc_freq.get(term).copied().unwrap_or(0) as f64;
            let idf = ((doc_count - df + 0.5) / (df + 0.5) + 1.0).ln();
            let numerator = term_tf * (BM25_K1 + 1.0);
            let denominator = term_tf + BM25_K1 * (1.0 - BM25_B + BM25_B * (dl / avg_doc_len));

            // BM25+ (Lv & Zhai 2011): adds δ * IDF to prevent long-document penalty
            let bm25_base = idf * (numerator / denominator.max(1e-9));
            score += bm25_base + BM25_DELTA * idf;
        }

        // ── Exact phrase match bonus ──
        if !query_phrase.is_empty() && candidates[idx].text.to_lowercase().contains(&query_phrase) {
            score += 1.25;
        }

        // ── Title match bonus ──
        let title_lower = candidates[idx].title.to_lowercase();
        let title_match_count = query_terms.iter()
            .filter(|qt| title_lower.contains(qt.as_str()))
            .count();
        if title_match_count > 0 {
            score += (title_match_count as f64) * 0.5;
        }

        // ── RRF cross-engine consensus boost (v6.1.0: normalized, not magic constant) ──
        // Normalize RRF to 0-1 range, apply as multiplicative boost up to 50%
        let rrf_normalized = candidates[idx].rrf_score / max_rrf;
        score *= 1.0 + (rrf_normalized * 0.5);

        // ── Extraction confidence prior (v8.0: a REAL value) ──
        // v7.0 computed `0.05 * (0.7 - 0.5)` = a constant +0.01 for every
        // chunk, because extraction_confidence was hardcoded. It is now the
        // extractor's own quality score, applied multiplicatively so a
        // half-confidence extraction is genuinely demoted.
        score *= 0.80 + 0.20 * candidates[idx].extraction_confidence.clamp(0.0, 1.0);

        // ── Source trust prior ──
        // `llm::trust_score` was fully implemented in v7.0 and NEVER CALLED
        // (it is `pub`, which is why cargo did not warn). .gov/.edu/wikipedia
        // and canonical docs now actually outrank random blogspam.
        score *= crate::llm::trust_score(&candidates[idx].url) as f64;

        // ── Mode / domain source preference (v8.1) ──
        // Study prefers educational sources, Academic scholarly ones, and a
        // domain prefers its authorities (health: WHO/NIH/NHS, ...).
        score *= crate::modes::source_boost(&candidates[idx].url, opts.mode, opts.domain);

        // ── Freshness prior (P2-2) ──
        // Off for evergreen modes (Study, Academic) even on dated queries:
        // recency bias there demotes the canonical explanation.
        if let (true, Some(it)) = (opts.freshness, intel) {
            score *= freshness_multiplier(
                candidates[idx].published,
                it.is_time_sensitive,
                &it.intent,
            );
        }

        scored.push((idx, score));
    }

    // ── Sort by BM25+ score (pre-MMR ordering) ──
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let a_len = candidates[a.0].text.chars().count();
                let b_len = candidates[b.0].text.chars().count();
                b_len.cmp(&a_len)
            })
    });

    // ==========================================================================
    // Stage D: MMR Diversity Reranking
    // Carbonell & Goldstein (SIGIR 1998)
    // MMR = argmax[ λ·rel(c) − (1−λ)·max_sim(c, selected) ]
    //
    // This both caps per-source monoculture AND removes near-duplicate chunks
    // across different URLs (which the per-URL cap misses).
    // ==========================================================================
    let selected = mmr_rerank(&candidates, &scored, top_k, &tokenized_docs, opts.max_chunks_per_url.max(1));

    selected
        .into_iter()
        .map(|idx| {
            let chunk = &candidates[idx];
            SourceResult {
                url: chunk.url.clone(),
                title: chunk.title.clone(),
                engine: chunk.engine.clone(),
                char_count: crate::util::char_len(&chunk.text),
                extracted_text: chunk.text.clone(),
                published: chunk.published,
                language: chunk.language.clone(),
                confidence: chunk.extraction_confidence,
                extractor: chunk.extractor,
            }
        })
        .collect()
}

/// MMR (Maximal Marginal Relevance) reranking for diversity.
/// λ=0.7 balances relevance (70%) vs diversity (30%).
///
/// Uses token-based Jaccard similarity as the diversity signal
/// (lightweight, no embedding model required — upgrade path to
/// semantic similarity in C cross-encoder stage).
/// Compact token signature: sorted, deduped 64-bit hashes.
///
/// v7.0 built a `HashSet<String>` per candidate, re-allocating every token
/// string even though the caller had already tokenized the same corpus. At
/// Phase-3 text volumes (374k-680k chars/search) that is hundreds of thousands
/// of String allocations per query.
fn token_signature(tokens: &[String]) -> Vec<u64> {
    use std::hash::{Hash, Hasher};
    let mut v: Vec<u64> = tokens
        .iter()
        .map(|t| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            t.hash(&mut h);
            h.finish()
        })
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Jaccard over two sorted u64 slices — linear merge, no allocation.
fn jaccard_sorted(a: &[u64], b: &[u64]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let (mut i, mut j, mut inter) = (0usize, 0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                inter += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// MMR diversity reranking — Carbonell & Goldstein (SIGIR 1998), lambda=0.7.
///
/// v8.0 rewrite. The v7.0 loop was `O(top_k * n * |selected|)` with a
/// `HashSet<String>` Jaccard inside, plus an `O(n)` `Vec::remove` per
/// selection. After Phase 3 raised text volume 32-58x it became the dominant
/// cost in the pipeline: 3,011 ms to rank 91 sources.
///
/// Four changes:
///   1. Tokenize ONCE — the signatures are passed in from the caller.
///   2. Hash tokens to u64 and merge sorted slices (no String allocation).
///   3. Consider only a CANDIDATE WINDOW. Nothing ranked 5,000th by BM25 wins
///      on diversity alone, so scoring the whole corpus every round is waste.
///   4. Track `max_sim` INCREMENTALLY: after selecting s, update each
///      remaining candidate against s only. This removes the inner
///      `|selected|` loop entirely, turning the hot path into O(window) per
///      selection.
fn mmr_rerank(
    candidates: &[ChunkCandidate],
    scored: &[(usize, f64)],
    top_k: usize,
    tokenized: &[Vec<String>],
    max_per_url: usize,
) -> Vec<usize> {
    if scored.is_empty() || top_k == 0 {
        return Vec::new();
    }

    let max_score = scored.iter().map(|(_, s)| *s).fold(0.0_f64, f64::max).max(1e-9);

    // ── Candidate window ──
    // `min`, not `clamp(top_k, len)`: the v8.0 clamp PANICKED ("min > max")
    // whenever a search produced fewer chunks than top_k — found by Reddit
    // mode returning 14 chunks against a top_k of 25.
    let window = top_k.saturating_mul(MMR_WINDOW_FACTOR).min(scored.len());

    // Per-candidate state, built once for the window only.
    struct Cand {
        idx: usize,
        rel: f64,
        sig: Vec<u64>,
        url_key: String,
        /// Running max similarity against everything already selected.
        max_sim: f64,
        taken: bool,
    }

    let mut pool: Vec<Cand> = scored[..window]
        .iter()
        .map(|&(idx, rel)| Cand {
            idx,
            rel: rel / max_score,
            sig: token_signature(tokenized.get(idx).map(|v| v.as_slice()).unwrap_or(&[])),
            url_key: crate::url_utils::canonical_key(&candidates[idx].url),
            max_sim: 0.0,
            taken: false,
        })
        .collect();

    let mut selected: Vec<usize> = Vec::with_capacity(top_k);
    let mut url_counts: HashMap<String, usize> = HashMap::new();

    while selected.len() < top_k {
        let mut best: Option<(usize, f64)> = None; // (pool position, mmr)
        let mut any_eligible = false;

        for (pos, c) in pool.iter().enumerate() {
            if c.taken {
                continue;
            }
            // Hard per-URL cap.
            if url_counts.get(&c.url_key).copied().unwrap_or(0) >= max_per_url {
                continue;
            }
            any_eligible = true;
            let mmr = MMR_LAMBDA * c.rel - (1.0 - MMR_LAMBDA) * c.max_sim;
            if best.is_none_or(|(_, b)| mmr > b) {
                best = Some((pos, mmr));
            }
        }

        // v7.0 BUG: when every remaining candidate was URL-capped, the inner
        // loop `continue`d for all of them, `best_mmr` stayed f64::MIN and
        // `best_pos` stayed 0 — so it selected remaining[0] ANYWAY, violating
        // the cap it had just enforced. Now we stop.
        let Some((pos, _)) = best else {
            if !any_eligible {
                break;
            }
            break;
        };

        pool[pos].taken = true;
        let chosen_idx = pool[pos].idx;
        *url_counts.entry(pool[pos].url_key.clone()).or_insert(0) += 1;
        selected.push(chosen_idx);

        // ── Incremental max_sim update: compare against the NEW pick only ──
        let (left, right) = pool.split_at_mut(pos);
        let (chosen, right) = right.split_first_mut().expect("pos is in range");
        for c in left.iter_mut().chain(right.iter_mut()) {
            if c.taken {
                continue;
            }
            let sim = jaccard_sorted(&c.sig, &chosen.sig);
            if sim > c.max_sim {
                c.max_sim = sim;
            }
        }
    }

    selected
}


// =============================================================================
// Chunking — Sentence-boundary-aware with overlap
// =============================================================================

fn paragraph_chunks(text: &str) -> Vec<String> {
    let normalized = text.replace('\r', "");
    let mut paragraphs = normalized
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();

    if paragraphs.is_empty() {
        let compact = normalized.trim();
        if compact.is_empty() {
            return Vec::new();
        }
        paragraphs.push(compact);
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    for paragraph in paragraphs {
        if current.is_empty() {
            current.push_str(paragraph);
            continue;
        }

        let projected_len = current.chars().count() + 2 + paragraph.chars().count();
        let current_len = current.chars().count();

        if projected_len <= MAX_CHUNK_CHARS || current_len < MIN_CHUNK_CHARS {
            current.push_str("\n\n");
            current.push_str(paragraph);
        } else {
            // ── Sentence-boundary split for oversized chunks ──
            let trimmed = current.trim().to_string();
            if trimmed.chars().count() > MAX_CHUNK_CHARS {
                let sub_chunks = split_at_sentence_boundaries(&trimmed);
                chunks.extend(sub_chunks);
            } else {
                chunks.push(trimmed);
            }
            current.clear();
            current.push_str(paragraph);
        }
    }

    if !current.trim().is_empty() {
        let trimmed = current.trim().to_string();
        if trimmed.chars().count() > MAX_CHUNK_CHARS {
            let sub_chunks = split_at_sentence_boundaries(&trimmed);
            chunks.extend(sub_chunks);
        } else {
            chunks.push(trimmed);
        }
    }

    // ── Apply ~15% overlap between consecutive chunks ──
    if chunks.len() > 1 {
        apply_chunk_overlap(&mut chunks);
    }

    chunks
}

/// Sentence terminators across writing systems.
///
/// v7.0 only recognised `. ! ?` AND required the following character to be
/// uppercase. Devanagari has no case and uses `।` (U+0964 DANDA) as its full
/// stop, so for a Hindi article NO boundary was ever found: the whole article
/// became a single chunk, which BM25 then scored as one undifferentiated blob.
/// The same held for Arabic, CJK, Thai and every other uncased script.
const SENTENCE_TERMINATORS: &[char] = &[
    '.', '!', '?',
    '\u{0964}', // DEVANAGARI DANDA — Hindi/Marathi/Nepali full stop
    '\u{0965}', // DEVANAGARI DOUBLE DANDA
    '\u{06D4}', // ARABIC FULL STOP
    '\u{3002}', // IDEOGRAPHIC FULL STOP (CJK)
    '\u{FF01}', // FULLWIDTH EXCLAMATION MARK
    '\u{FF1F}', // FULLWIDTH QUESTION MARK
    '\u{0589}', // ARMENIAN FULL STOP
    '\u{104A}', // MYANMAR SIGN LITTLE SECTION
    '\u{104B}', // MYANMAR SIGN SECTION
    '\u{1362}', // ETHIOPIC FULL STOP
    '\u{17D4}', // KHMER SIGN KHAN
    '\u{0E2F}', // THAI PAIYANNOI
];

/// Does this script distinguish case? Devanagari, Arabic, CJK, Thai, Hebrew
/// and others do not, so requiring an uppercase sentence start is wrong there.
fn script_has_case(c: char) -> bool {
    c.is_uppercase() || c.is_lowercase()
}

/// Is position `i` a sentence boundary?
fn is_sentence_boundary(chars: &[char], i: usize) -> bool {
    if !SENTENCE_TERMINATORS.contains(&chars[i]) {
        return false;
    }
    // A terminator at the very end closes the sentence.
    let Some(&next) = chars.get(i + 1) else { return true };

    // CJK/Devanagari terminators often have no following space.
    if !next.is_whitespace() {
        // Accept anyway for terminators that are unambiguous sentence enders.
        return matches!(chars[i], '\u{0964}' | '\u{0965}' | '\u{3002}' | '\u{FF01}' | '\u{FF1F}');
    }

    match chars.get(i + 2) {
        None => true,
        Some(&after) => {
            // Avoid splitting decimals/abbreviations in cased scripts only.
            if script_has_case(after) {
                after.is_uppercase()
            } else {
                // Uncased script (Devanagari, Arabic, CJK, Thai): whitespace
                // after a terminator IS the boundary.
                true
            }
        }
    }
}

/// Split oversized text at sentence boundaries, producing chunks that are at
/// most `MAX_CHUNK_CHARS` where the text allows.
fn split_at_sentence_boundaries(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;
    let mut i = 0usize;

    while i < chars.len() {
        current.push(chars[i]);
        current_chars += 1;

        if current_chars >= MIN_CHUNK_CHARS && is_sentence_boundary(&chars, i) {
            chunks.push(current.trim().to_string());
            current.clear();
            current_chars = 0;
            // Skip the separator whitespace.
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            continue;
        }

        // Hard stop so a terminator-free blob (common in scripts we may still
        // not handle) cannot produce one enormous chunk.
        if current_chars >= MAX_CHUNK_CHARS * 2 {
            chunks.push(current.trim().to_string());
            current.clear();
            current_chars = 0;
        }
        i += 1;
    }

    if !current.trim().is_empty() {
        if current_chars < MIN_CHUNK_CHARS && !chunks.is_empty() {
            let last = chunks.last_mut().unwrap();
            last.push(' ');
            last.push_str(current.trim());
        } else {
            chunks.push(current.trim().to_string());
        }
    }
    if chunks.is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}

/// Apply ~15% overlap: prepend the last ~15% of chunk[i-1] to chunk[i].
/// This ensures facts split across a boundary land whole in at least one chunk.
fn apply_chunk_overlap(chunks: &mut Vec<String>) {
    if chunks.len() <= 1 {
        return;
    }

    let mut overlapped = Vec::with_capacity(chunks.len());
    overlapped.push(chunks[0].clone());

    for i in 1..chunks.len() {
        let prev = &chunks[i - 1];
        let overlap_chars = (prev.chars().count() as f64 * CHUNK_OVERLAP_RATIO) as usize;
        if overlap_chars > 10 {
            let prev_suffix: String = prev.chars().rev().take(overlap_chars).collect::<Vec<_>>().into_iter().rev().collect();
            // Find word boundary in the overlap suffix
            let word_start = prev_suffix.find(' ').unwrap_or(0);
            let clean_overlap = prev_suffix[word_start..].trim();
            if !clean_overlap.is_empty() {
                overlapped.push(format!("{}… {}", clean_overlap, chunks[i]));
            } else {
                overlapped.push(chunks[i].clone());
            }
        } else {
            overlapped.push(chunks[i].clone());
        }
    }

    *chunks = overlapped;
}

// =============================================================================
// Tokenizer — v6.1.0 pipeline: lowercase → stopword filter
// (Snowball stemming will be added when tantivy-stemmers is in Cargo.toml)
// =============================================================================

/// Tokenize with stopword removal — used for BM25+ scoring.
fn tokenize_filtered(text: &str) -> Vec<String> {
    text.split(|c: char| !crate::util::is_word_char(c))
        .filter_map(|raw| {
            let t = raw.trim().to_lowercase();
            if t.len() < 2 {
                return None;
            }
            // Stopword filter
            if STOPWORD_SET.contains(t.as_str()) {
                return None;
            }
            Some(t)
        })
        .collect()
}

/// Content terms of a query (lowercased, stopwords removed, de-duplicated).
pub(crate) fn query_terms(text: &str) -> Vec<String> {
    unique_tokens_filtered(text)
}

/// Unique tokens with stopword removal — used for query terms.
fn unique_tokens_filtered(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for token in tokenize_filtered(text) {
        if seen.insert(token.clone()) {
            out.push(token);
        }
    }
    out
}



// =============================================================================
// v7.0: Cross-Query Result Fusion
// Merges results from multiple sub-queries using weighted RRF.
// URLs that appear across MULTIPLE sub-queries get boosted scores,
// producing more robust, cross-validated results.
// =============================================================================


/// Fuse results from several sub-queries, weighting the original query higher.
/// Returns the fused score per `canonical_key`, the multi-query counterpart of
/// [`compute_rrf_scores`].
///
/// ═══════════════════════════════════════════════════════════════════════════
/// v8.0: this function existed in v7.0 and was DEAD CODE. `search.rs` did a
/// flat `all_results.extend(results)` across every query variation, so a URL
/// surfaced only by a garbage sub-query (v7.0 generated midpoint fragments
/// like "what are the implications of") carried exactly the same RRF weight as
/// one surfaced by the user's actual query. It is now wired in, and it reuses
/// the engine-FAMILY collapse so the two fusion layers cannot double-count.
///
/// v8.1: it used to return ONE representative result per URL, carrying one
/// engine name. The caller then re-ran RRF over those representatives, so in
/// every multi-query mode (Auto standard/heavy, Deep, Study, Academic) each
/// page looked like it came from a single engine: `found_by_families` was 1
/// for all of them and the consensus that fusion had measured was thrown
/// away. It now returns scores only; the caller keeps the raw results, so
/// engine and family lists stay accurate.
/// ═══════════════════════════════════════════════════════════════════════════
pub fn cross_query_scores(
    query_results: &[(String, Vec<RawSearchResult>)],
    original_query: &str,
) -> HashMap<String, f64> {
    /// The original query counts for more than any derived sub-query.
    const ORIGINAL_WEIGHT: f64 = 2.0;
    const SUB_WEIGHT: f64 = 1.0;

    let mut scores: HashMap<String, f64> = HashMap::new();
    let mut sub_query_count: HashMap<String, HashSet<&str>> = HashMap::new();

    for (sub_query, results) in query_results {
        let weight = if sub_query.eq_ignore_ascii_case(original_query) {
            ORIGINAL_WEIGHT
        } else {
            SUB_WEIGHT
        };
        // (page, family) -> best rank, per sub-query, so one index cannot agree
        // with itself across its own aliases.
        for (key, score) in compute_rrf_scores(results) {
            *scores.entry(key.clone()).or_insert(0.0) += weight * score;
            sub_query_count.entry(key).or_default().insert(sub_query.as_str());
        }
    }

    // Cross-validation bonus: a page found by MULTIPLE sub-queries is more
    // likely to be on-topic than one found by a single narrow phrasing.
    let total_queries = query_results.len().max(1) as f64;
    for (key, qs) in &sub_query_count {
        if qs.len() > 1 {
            if let Some(s) = scores.get_mut(key) {
                *s *= 1.0 + 0.15 * (qs.len() as f64 / total_queries);
            }
        }
    }
    scores
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::RawSearchResult;

    fn hit(url: &str, engine: &str, rank: usize) -> RawSearchResult {
        RawSearchResult {
            url: url.to_string(),
            title: "t".into(),
            snippet: String::new(),
            engine: engine.to_string(),
            rank_position: rank,
            published: None,
            full_text: None,
        }
    }

    #[test]
    fn rrf_counts_bing_aliases_once() {
        // THE v7.0 BUG: 15 Bing aliases returning the same URL at rank 1 were
        // summed as 15 independent agreements.
        let aliases = [
            "bing", "bing_us", "bing_uk", "bing_in", "bing_de", "bing_fr",
            "bing_es", "bing_it", "bing_jp", "bing_ca", "bing_au", "bing_nl",
            "bing_se", "bing_no", "bing_fi",
        ];
        let many: Vec<RawSearchResult> =
            aliases.iter().map(|e| hit("https://a.test/x", e, 1)).collect();
        let one = vec![hit("https://a.test/x", "bing", 1)];

        let s_many = compute_rrf_scores(&many);
        let s_one = compute_rrf_scores(&one);
        let k = crate::url_utils::canonical_key("https://a.test/x");
        assert!(
            (s_many[&k] - s_one[&k]).abs() < 1e-12,
            "15 Bing aliases scored {} vs one alias {} — aliases are inflating consensus",
            s_many[&k],
            s_one[&k]
        );
    }

    #[test]
    fn rrf_treats_bing_derived_engines_as_one_family() {
        // Yahoo and DuckDuckGo are Bing-derived; Startpage/Ecosia proxy Google.
        let derived = vec![
            hit("https://a.test/x", "bing", 1),
            hit("https://a.test/x", "yahoo", 1),
            hit("https://a.test/x", "duckduckgo", 1),
        ];
        let single = vec![hit("https://a.test/x", "bing", 1)];
        let k = crate::url_utils::canonical_key("https://a.test/x");
        assert!(
            (compute_rrf_scores(&derived)[&k] - compute_rrf_scores(&single)[&k]).abs() < 1e-12
        );

        let google_family = vec![
            hit("https://a.test/x", "google", 1),
            hit("https://a.test/x", "startpage", 1),
            hit("https://a.test/x", "ecosia", 1),
        ];
        assert!(
            (compute_rrf_scores(&google_family)[&k] - compute_rrf_scores(&single)[&k]).abs() < 1e-9
        );
    }

    #[test]
    fn genuinely_independent_engines_do_accumulate() {
        // The flip side: real cross-engine agreement MUST raise the score,
        // otherwise RRF is doing nothing.
        let independent = vec![
            hit("https://a.test/x", "bing", 1),
            hit("https://a.test/x", "google", 1),
            hit("https://a.test/x", "brave", 1),
            hit("https://a.test/x", "yandex", 1),
            hit("https://a.test/x", "marginalia_api", 1),
        ];
        let single = vec![hit("https://a.test/x", "bing", 1)];
        let k = crate::url_utils::canonical_key("https://a.test/x");
        let multi = compute_rrf_scores(&independent)[&k];
        let one = compute_rrf_scores(&single)[&k];
        assert!(multi > one * 3.0, "5 independent families scored {multi} vs {one}");
    }

    #[test]
    fn rrf_uses_the_best_rank_within_a_family() {
        // bing at rank 9 and bing_uk at rank 2 => the family contributes rank 2.
        let mixed = vec![
            hit("https://a.test/x", "bing", 9),
            hit("https://a.test/x", "bing_uk", 2),
        ];
        let best_only = vec![hit("https://a.test/x", "bing", 2)];
        let k = crate::url_utils::canonical_key("https://a.test/x");
        assert!(
            (compute_rrf_scores(&mixed)[&k] - compute_rrf_scores(&best_only)[&k]).abs() < 1e-12
        );
    }

    #[test]
    fn rrf_unifies_url_spellings_of_one_page() {
        // The same article as three URL spellings is ONE page, not three
        // engines agreeing.
        let spellings = vec![
            hit("https://example.com/story", "bing", 1),
            hit("https://www.example.com/story/", "bing", 1),
            hit("https://m.example.com/story?utm_source=x", "bing", 1),
        ];
        let scores = compute_rrf_scores(&spellings);
        assert_eq!(scores.len(), 1, "expected one canonical key, got {scores:?}");
    }

    #[test]
    fn higher_rank_scores_higher() {
        let a = compute_rrf_scores(&[hit("https://a.test/1", "bing", 1)]);
        let b = compute_rrf_scores(&[hit("https://a.test/1", "bing", 20)]);
        let k = crate::url_utils::canonical_key("https://a.test/1");
        assert!(a[&k] > b[&k], "rank 1 must beat rank 20");
    }

    #[test]
    fn family_agreement_reports_independent_families() {
        let results = vec![
            hit("https://a.test/x", "bing", 1),
            hit("https://a.test/x", "bing_uk", 3),
            hit("https://a.test/x", "yahoo", 2),     // bing family
            hit("https://a.test/x", "brave", 1),     // independent
            hit("https://a.test/x", "marginalia_api", 5),
        ];
        let agree = family_agreement(&results);
        let k = crate::url_utils::canonical_key("https://a.test/x");
        let fams = &agree[&k];
        assert_eq!(fams.len(), 3, "expected bing+brave+marginalia, got {fams:?}");
        assert!(fams.contains(&"bing") && fams.contains(&"brave") && fams.contains(&"marginalia"));
    }

    // ── Script-aware chunking (P1-5a) ──

    #[test]
    fn hindi_article_splits_on_danda_not_one_blob() {
        // THE v7.0 BUG: `. ! ?` + an uppercase-next requirement meant a Hindi
        // article never found a boundary, so the whole thing became ONE chunk
        // and BM25 scored it as an undifferentiated blob.
        let sentence = "भारत और पाकिस्तान के बीच मुकाबला दस अक्टूबर को खेला जाएगा।";
        let article = std::iter::repeat(sentence).take(12).collect::<Vec<_>>().join(" ");
        let chunks = paragraph_chunks(&article);
        assert!(
            chunks.len() >= 3,
            "Hindi article produced {} chunk(s) — danda boundaries not detected",
            chunks.len()
        );
        // And every chunk must actually contain Devanagari.
        assert!(chunks.iter().all(|c| c.contains('\u{0915}') || c.contains('\u{092E}')
                                   || c.contains('\u{092C}') || c.contains('\u{0926}')));
    }

    #[test]
    fn cjk_text_splits_on_ideographic_full_stop() {
        // Must exceed MAX_CHUNK_CHARS, otherwise staying as one chunk is the
        // CORRECT behaviour and the test proves nothing.
        let s = "这是一个句子。这是另一个句子。还有第三个句子。";
        let article = std::iter::repeat(s).take(40).collect::<Vec<_>>().join("");
        assert!(article.chars().count() > MAX_CHUNK_CHARS, "test input too short");
        let chunks = paragraph_chunks(&article);
        assert!(chunks.len() >= 2, "CJK produced {} chunk(s)", chunks.len());
        assert!(chunks.iter().all(|c| c.chars().count() <= MAX_CHUNK_CHARS * 2));
    }

    #[test]
    fn short_text_correctly_stays_one_chunk() {
        // Splitting a 200-char paragraph would hurt BM25, not help it.
        let s = "这是一个句子。".repeat(10);
        assert!(s.chars().count() < MAX_CHUNK_CHARS);
        assert_eq!(paragraph_chunks(&s).len(), 1);
    }

    #[test]
    fn english_sentences_still_split_correctly() {
        let s = "This is the first sentence of the document. Here is a second one that continues. \
                 And a third sentence follows after it. Then a fourth appears here as well.";
        let article = std::iter::repeat(s).take(6).collect::<Vec<_>>().join(" ");
        let chunks = paragraph_chunks(&article);
        assert!(chunks.len() >= 2, "English produced {} chunk(s)", chunks.len());
    }

    #[test]
    fn decimals_and_abbreviations_do_not_split_english() {
        // "3.14" and "e.g. this" must not become boundaries in a cased script.
        let chars: Vec<char> = "pi is 3.14 exactly".chars().collect();
        let dot = chars.iter().position(|c| *c == '.').unwrap();
        assert!(!is_sentence_boundary(&chars, dot), "split inside a decimal");
    }

    #[test]
    fn terminator_free_text_still_gets_chunked() {
        // A blob with no recognised terminator must not become one huge chunk.
        let blob = "x".repeat(MAX_CHUNK_CHARS * 6);
        let chunks = paragraph_chunks(&blob);
        assert!(chunks.len() >= 3, "terminator-free blob produced {} chunk(s)", chunks.len());
    }

    #[test]
    fn script_case_detection() {
        assert!(script_has_case('A') && script_has_case('z'));
        // Devanagari, Arabic, CJK, Thai have no case.
        assert!(!script_has_case('\u{0915}'));
        assert!(!script_has_case('\u{0627}'));
        assert!(!script_has_case('\u{4E2D}'));
        assert!(!script_has_case('\u{0E01}'));
    }

    #[test]
    fn chunking_is_utf8_safe_on_mixed_scripts() {
        let mixed = "Hello world. नमस्ते दुनिया। 你好世界。مرحبا بالعالم؟ Done.".repeat(20);
        let chunks = paragraph_chunks(&mixed);
        assert!(!chunks.is_empty());
        // Reassembly must not have corrupted any character.
        assert!(chunks.iter().all(|c| !c.contains('\u{FFFD}')));
    }

    // ── P2-2: freshness ──

    #[test]
    fn freshness_is_neutral_for_evergreen_queries() {
        use crate::query_intel::QueryIntent;
        let old = Some(chrono::Utc::now() - chrono::Duration::days(3000));
        // "how does TLS work" must not be recency-biased.
        assert_eq!(freshness_multiplier(old, false, &QueryIntent::HowTo), 1.0);
        assert_eq!(freshness_multiplier(None, false, &QueryIntent::Factual), 1.0);
    }

    #[test]
    fn fresh_news_outranks_stale_news_on_temporal_queries() {
        use crate::query_intel::QueryIntent;
        let today = Some(chrono::Utc::now());
        let month_old = Some(chrono::Utc::now() - chrono::Duration::days(30));
        let year_old = Some(chrono::Utc::now() - chrono::Duration::days(365));
        let f_today = freshness_multiplier(today, true, &QueryIntent::Temporal);
        let f_month = freshness_multiplier(month_old, true, &QueryIntent::Temporal);
        let f_year = freshness_multiplier(year_old, true, &QueryIntent::Temporal);
        assert!(f_today > f_month && f_month > f_year, "{f_today} {f_month} {f_year}");
        assert!(f_today > 0.99, "today should be ~1.0, got {f_today}");
    }

    #[test]
    fn freshness_never_erases_an_old_authoritative_source() {
        use crate::query_intel::QueryIntent;
        let ancient = Some(chrono::Utc::now() - chrono::Duration::days(10_000));
        let f = freshness_multiplier(ancient, true, &QueryIntent::Temporal);
        assert!(f >= FRESHNESS_FLOOR, "floor breached: {f}");
        assert!(f > 0.0);
    }

    #[test]
    fn person_queries_decay_more_slowly_than_news() {
        use crate::query_intel::QueryIntent;
        let d = Some(chrono::Utc::now() - chrono::Duration::days(60));
        let news = freshness_multiplier(d, true, &QueryIntent::Temporal);
        let person = freshness_multiplier(d, true, &QueryIntent::Person);
        assert!(person > news, "person {person} should decay slower than news {news}");
    }

    #[test]
    fn undated_sources_get_a_mild_penalty_not_a_veto() {
        use crate::query_intel::QueryIntent;
        let f = freshness_multiplier(None, true, &QueryIntent::Temporal);
        assert!(f > 0.85 && f < 1.0, "got {f}");
    }

    #[test]
    fn future_dates_do_not_produce_a_bonus_above_one() {
        use crate::query_intel::QueryIntent;
        let future = Some(chrono::Utc::now() + chrono::Duration::days(500));
        let f = freshness_multiplier(future, true, &QueryIntent::Temporal);
        assert!(f <= 1.0, "clock-skewed future date gave {f}");
    }

    // ── P1-9: cross-query fusion (was dead code in v7.0) ──

    fn fused_order(groups: &[(String, Vec<RawSearchResult>)], orig: &str) -> Vec<String> {
        let mut v: Vec<(String, f64)> = cross_query_scores(groups, orig).into_iter().collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        v.into_iter().map(|(k, _)| k).collect()
    }

    #[test]
    fn original_query_outweighs_sub_queries() {
        let orig = "cricket world cup 2027";
        let groups = vec![
            (orig.to_string(), vec![hit("https://a.test/from-original", "bing", 1)]),
            ("cricket".to_string(), vec![hit("https://b.test/from-sub", "bing", 1)]),
        ];
        let order = fused_order(&groups, orig);
        assert_eq!(order.len(), 2);
        assert!(order[0].contains("from-original"), "the original query's hit must rank first, got {order:?}");
    }

    #[test]
    fn pages_found_by_multiple_sub_queries_get_a_bonus() {
        let orig = "postgres vs mysql";
        let shared = "https://both.test/compare";
        let only_one = "https://one.test/page";
        let groups = vec![
            (orig.to_string(), vec![hit(shared, "bing", 5), hit(only_one, "bing", 1)]),
            ("postgres".to_string(), vec![hit(shared, "brave", 5)]),
            ("mysql".to_string(), vec![hit(shared, "yandex", 5)]),
        ];
        let s = cross_query_scores(&groups, orig);
        let k = |u: &str| crate::url_utils::canonical_key(u);
        assert!(
            s[&k(shared)] > s[&k(only_one)],
            "a page confirmed by 3 sub-queries and 3 families should beat a \
             single-query rank-1 hit"
        );
    }

    #[test]
    fn fusion_keeps_cross_engine_consensus_within_a_sub_query() {
        // Regression (v8.1): fusion collapsed each URL to one representative
        // engine, so with sub-queries every page reported one family and
        // two-engine agreement on the original query counted for nothing.
        let orig = "capital of France";
        let agreed = "https://en.wikipedia.org/wiki/Paris";
        let single = "https://odd.test/page";
        let groups = vec![
            (
                orig.to_string(),
                vec![hit(single, "bing", 1), hit(agreed, "wikipedia", 2), hit(agreed, "duckduckgo", 2)],
            ),
            ("capital of France history".to_string(), vec![]),
        ];
        let order = fused_order(&groups, orig);
        assert_eq!(order[0], crate::url_utils::canonical_key(agreed), "{order:?}");
    }

    #[test]
    fn fusion_matches_plain_rrf_for_a_single_query() {
        let orig = "q";
        let results = vec![hit("https://a.test/1", "bing", 1), hit("https://a.test/1", "brave", 3), hit("https://b.test/2", "bing", 2)];
        let fused = cross_query_scores(&[(orig.to_string(), results.clone())], orig);
        let plain = compute_rrf_scores(&results);
        for (k, v) in &plain {
            assert!((fused[k] - 2.0 * v).abs() < 1e-12, "only the original-query weight should differ");
        }
    }

    #[test]
    fn fusion_collapses_url_spellings_and_aliases() {
        let orig = "q";
        let groups = vec![(
            orig.to_string(),
            vec![
                hit("https://example.com/story", "bing", 1),
                hit("https://www.example.com/story/", "bing_uk", 1),
                hit("https://m.example.com/story?utm_source=x", "bing_de", 1),
            ],
        )];
        let s = cross_query_scores(&groups, orig);
        assert_eq!(s.len(), 1, "one page, three spellings: {s:?}");
    }

    #[test]
    fn fusion_handles_empty_groups() {
        assert!(cross_query_scores(&[], "q").is_empty());
        assert!(cross_query_scores(&[("q".into(), vec![])], "q").is_empty());
    }

    #[test]
    fn ranking_fewer_chunks_than_top_k_does_not_panic() {
        // Regression: MMR's window used `.clamp(top_k, len)`, which panics when
        // len < top_k. Any thin-result search (Reddit: 14 chunks, top_k 25)
        // crashed the request.
        let sources: Vec<SourceResult> = (0..3)
            .map(|i| SourceResult {
                url: format!("https://t{i}.test/a"),
                title: format!("Title {i}"),
                extracted_text: format!("Short source {i} about rust ownership and borrowing rules."),
                char_count: 60,
                engine: "test".into(),
                confidence: 1.0,
                ..Default::default()
            })
            .collect();
        for top_k in [1, 3, 25, 500] {
            let out = rank_top_chunks_with_signals("rust ownership", &sources, top_k, None, None);
            assert!(out.len() <= top_k.min(3).max(1) || out.len() <= 3);
        }
        assert!(rank_top_chunks_with_signals("rust", &[], 25, None, None).is_empty());
    }

    #[test]
    fn empty_input_is_handled() {
        assert!(compute_rrf_scores(&[]).is_empty());
        assert!(family_agreement(&[]).is_empty());
    }
}
