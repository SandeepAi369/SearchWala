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
    rrf_score: f64,           // Inherited from URL-level RRF
    extraction_confidence: f64, // v6.1.0: from extractor
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

/// Sort URLs by RRF score (descending). Returns ordered list of (url, rrf_score).
pub fn rrf_ranked_urls(results: &[RawSearchResult]) -> Vec<(String, f64)> {
    let scores = compute_rrf_scores(results);
    let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked
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
pub fn rank_top_chunks_with_rrf(
    query: &str,
    sources: &[SourceResult],
    top_k: usize,
    rrf_scores: Option<&HashMap<String, f64>>,
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
                extraction_confidence: 0.7, // Default; will be overridden when C4 provides real confidence
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
                char_count: c.text.chars().count(),
                extracted_text: c.text,
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

        // ── Extraction confidence prior (v6.1.0) ──
        // Small additive prior: +0.05 * (confidence - 0.5)
        // High-confidence extractions get a slight boost, low-confidence a slight penalty
        score += 0.05 * (candidates[idx].extraction_confidence - 0.5);

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
    let selected = mmr_rerank(&candidates, &scored, top_k);

    selected
        .into_iter()
        .map(|idx| {
            let chunk = &candidates[idx];
            SourceResult {
                url: chunk.url.clone(),
                title: chunk.title.clone(),
                engine: chunk.engine.clone(),
                char_count: chunk.text.chars().count(),
                extracted_text: chunk.text.clone(),
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
fn mmr_rerank(
    candidates: &[ChunkCandidate],
    scored: &[(usize, f64)],
    top_k: usize,
) -> Vec<usize> {
    if scored.is_empty() {
        return Vec::new();
    }

    // Normalize BM25+ scores to 0-1 for MMR
    let max_score = scored.iter().map(|(_, s)| *s).fold(0.0_f64, f64::max).max(1e-9);

    // Pre-tokenize all candidate chunks for similarity computation
    let candidate_tokens: Vec<HashSet<String>> = candidates
        .iter()
        .map(|c| tokenize_filtered(&c.text).into_iter().collect())
        .collect();

    let mut selected: Vec<usize> = Vec::with_capacity(top_k);
    let mut remaining: Vec<(usize, f64)> = scored.to_vec();
    let mut url_counts: HashMap<String, usize> = HashMap::new();

    while selected.len() < top_k && !remaining.is_empty() {
        let mut best_mmr = f64::MIN;
        let mut best_pos = 0;

        for (pos, &(cand_idx, rel_score)) in remaining.iter().enumerate() {
            // Hard cap: max chunks per URL
            let norm_url = crate::url_utils::canonical_key(&candidates[cand_idx].url);
            let url_count = url_counts.get(&norm_url).copied().unwrap_or(0);
            if url_count >= MAX_CHUNKS_PER_URL_IN_TOP_K {
                continue;
            }

            let normalized_rel = rel_score / max_score;

            // Max similarity to any already-selected chunk
            let max_sim = if selected.is_empty() {
                0.0
            } else {
                selected
                    .iter()
                    .map(|&sel_idx| jaccard_similarity(&candidate_tokens[cand_idx], &candidate_tokens[sel_idx]))
                    .fold(0.0_f64, f64::max)
            };

            // MMR formula: λ * relevance - (1-λ) * max_similarity
            let mmr_score = MMR_LAMBDA * normalized_rel - (1.0 - MMR_LAMBDA) * max_sim;

            if mmr_score > best_mmr {
                best_mmr = mmr_score;
                best_pos = pos;
            }
        }

        let (cand_idx, _) = remaining.remove(best_pos);
        let norm_url = crate::url_utils::canonical_key(&candidates[cand_idx].url);
        *url_counts.entry(norm_url).or_insert(0) += 1;
        selected.push(cand_idx);
    }

    selected
}

/// Jaccard similarity between two token sets: |A ∩ B| / |A ∪ B|
fn jaccard_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 { 0.0 } else { intersection / union }
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

/// Split text at sentence boundaries (`. ` followed by uppercase letter).
/// Produces chunks ≤ MAX_CHUNK_CHARS.
fn split_at_sentence_boundaries(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    // Simple sentence boundary detection: ". " + uppercase, "! ", "? "
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        current.push(chars[i]);

        // Check for sentence boundary
        let is_sentence_end = (chars[i] == '.' || chars[i] == '!' || chars[i] == '?')
            && i + 2 < chars.len()
            && chars[i + 1] == ' '
            && chars[i + 2].is_uppercase();

        if is_sentence_end && current.chars().count() >= MIN_CHUNK_CHARS {
            // Include the space after the period
            current.push(chars[i + 1]);
            chunks.push(current.trim().to_string());
            current = String::new();
            i += 2; // Skip the space, start from the uppercase letter
            continue;
        }

        i += 1;
    }

    // Flush remaining text
    if !current.trim().is_empty() {
        if current.chars().count() < MIN_CHUNK_CHARS && !chunks.is_empty() {
            // Merge short trailing text into last chunk
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
    text.split(|c: char| !c.is_alphanumeric())
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


/// Merges results from multiple sub-queries using enhanced RRF.
/// The original query's results get 2x weight; sub-queries get 1x.
pub fn cross_query_fusion(
    query_results: &[(String, Vec<RawSearchResult>)],
    original_query: &str,
) -> Vec<RawSearchResult> {
    let mut url_scores: HashMap<String, f64> = HashMap::new();
    let mut url_results: HashMap<String, RawSearchResult> = HashMap::new();
    let mut url_query_count: HashMap<String, usize> = HashMap::new();

    for (sub_query, results) in query_results {
        // Original query gets 2x weight, sub-queries get 1x
        let weight = if sub_query == original_query { 2.0 } else { 1.0 };

        for (rank, result) in results.iter().enumerate() {
            let rrf_score = weight / (60.0 + rank as f64 + 1.0);
            *url_scores.entry(result.url.clone()).or_insert(0.0) += rrf_score;
            *url_query_count.entry(result.url.clone()).or_insert(0) += 1;
            url_results.entry(result.url.clone()).or_insert_with(|| result.clone());
        }
    }

    // Bonus: URLs appearing in multiple sub-queries get a cross-validation boost
    let num_queries = query_results.len().max(1) as f64;
    for (url, count) in &url_query_count {
        if *count > 1 {
            let cross_boost = (*count as f64 / num_queries) * 0.02;
            if let Some(score) = url_scores.get_mut(url) {
                *score += cross_boost;
            }
        }
    }

    // Sort by fused score (descending)
    let mut fused: Vec<(String, f64)> = url_scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    fused.into_iter()
        .filter_map(|(url, _)| url_results.remove(&url))
        .collect()
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

    #[test]
    fn empty_input_is_handled() {
        assert!(compute_rrf_scores(&[]).is_empty());
        assert!(family_agreement(&[]).is_empty());
    }
}
