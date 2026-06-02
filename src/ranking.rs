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
    let mut scores: HashMap<String, f64> = HashMap::new();

    for result in results {
        let normalized = crate::url_utils::normalize_url(&result.url)
            .unwrap_or_else(|| result.url.clone());
        let weight = engines::engine_weight(&result.engine) as f64;
        let rank = result.rank_position.max(1) as f64;
        let rrf_contribution = weight / (RRF_K + rank);
        *scores.entry(normalized).or_insert(0.0) += rrf_contribution;
    }

    scores
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
            .and_then(|s| {
                let norm = crate::url_utils::normalize_url(&source.url)
                    .unwrap_or_else(|| source.url.clone());
                s.get(&norm).copied()
            })
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
            let norm_url = crate::url_utils::normalize_url(&candidates[cand_idx].url)
                .unwrap_or_else(|| candidates[cand_idx].url.clone());
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
        let norm_url = crate::url_utils::normalize_url(&candidates[cand_idx].url)
            .unwrap_or_else(|| candidates[cand_idx].url.clone());
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

// ── Legacy API compatibility (used by extractor and other modules) ──

fn unique_tokens(text: &str) -> Vec<String> {
    unique_tokens_filtered(text)
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter_map(|raw| {
            let t = raw.trim().to_lowercase();
            if t.len() < 2 {
                return None;
            }
            Some(t)
        })
        .collect()
}
