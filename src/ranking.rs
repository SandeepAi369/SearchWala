// ============================================================================
// SearchWala v5.2.0 - Hybrid Ranking Engine
//
// Two-stage ranking pipeline:
//   Stage 1: Reciprocal Rank Fusion (RRF) — Cross-engine consensus ranking
//   Stage 2: BM25 Paragraph Chunking — Within-source content relevance
//
// RRF is the industry-standard algorithm used by Google, Elasticsearch,
// Azure AI Search for merging ranked lists from multiple retrieval systems.
// Formula: RRF_Score(url) = Σ engine_weight / (k + rank_in_engine)
//
// Engine weights (from engines/mod.rs) give Tier 1 engines (Google, Bing,
// Wikipedia) 1.5x influence over Tier 4 aggregators (Dogpile, Excite).
// ============================================================================

use std::collections::{HashMap, HashSet};

use crate::engines;
use crate::models::{RawSearchResult, SourceResult};

const MIN_CHUNK_CHARS: usize = 80;   // Lowered from 200 — keep short valuable paragraphs
const MAX_CHUNK_CHARS: usize = 400;
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;
const RRF_K: f64 = 60.0;  // Industry-standard smoothing constant

#[derive(Clone)]
struct ChunkCandidate {
    url: String,
    title: String,
    engine: String,
    text: String,
    rrf_score: f64,  // Inherited from URL-level RRF
}

// =============================================================================
// Stage 1: Reciprocal Rank Fusion (RRF)
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
// Stage 2: BM25 Paragraph-Level Chunk Ranking (with RRF boost)
// =============================================================================

pub fn rank_top_chunks(query: &str, sources: &[SourceResult], top_k: usize) -> Vec<SourceResult> {
    rank_top_chunks_with_rrf(query, sources, top_k, None)
}

/// Rank chunks using BM25 + optional RRF boost from cross-engine consensus.
/// When rrf_scores is provided, chunks from high-RRF URLs get an additive boost.
pub fn rank_top_chunks_with_rrf(
    query: &str,
    sources: &[SourceResult],
    top_k: usize,
    rrf_scores: Option<&HashMap<String, f64>>,
) -> Vec<SourceResult> {
    if sources.is_empty() || top_k == 0 {
        return Vec::new();
    }

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
            });
        }
    }

    if candidates.is_empty() {
        return Vec::new();
    }

    let query_terms = unique_tokens(query);
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

    let tokenized_docs: Vec<Vec<String>> = candidates
        .iter()
        .map(|c| tokenize(&c.text))
        .collect();

    let doc_count = tokenized_docs.len() as f64;
    let avg_doc_len = {
        let total: usize = tokenized_docs.iter().map(|d| d.len()).sum();
        (total.max(1) as f64) / doc_count.max(1.0)
    };

    let mut doc_freq: HashMap<String, usize> = HashMap::new();
    for tokens in &tokenized_docs {
        let mut seen = HashSet::new();
        for token in tokens {
            if seen.insert(token.clone()) {
                *doc_freq.entry(token.clone()).or_insert(0) += 1;
            }
        }
    }

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
            score += idf * (numerator / denominator.max(1e-9));
        }

        // Exact phrase match bonus
        if !query_phrase.is_empty() && candidates[idx].text.to_lowercase().contains(&query_phrase) {
            score += 1.25;
        }

        // Title match bonus — chunks from pages whose title matches the query get boosted
        let title_lower = candidates[idx].title.to_lowercase();
        let title_match_count = query_terms.iter()
            .filter(|qt| title_lower.contains(qt.as_str()))
            .count();
        if title_match_count > 0 {
            score += (title_match_count as f64) * 0.5;
        }

        // RRF cross-engine consensus boost (additive, scaled)
        // RRF scores are typically 0.01-0.1 range, so multiply by 20 to make impactful
        score += candidates[idx].rrf_score * 20.0;

        scored.push((idx, score));
    }

    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let a_len = candidates[a.0].text.chars().count();
                let b_len = candidates[b.0].text.chars().count();
                b_len.cmp(&a_len)
            })
    });

    scored
        .into_iter()
        .take(top_k)
        .map(|(idx, _)| {
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
            chunks.push(current.trim().to_string());
            current.clear();
            current.push_str(paragraph);
        }
    }

    if !current.trim().is_empty() {
        chunks.push(current.trim().to_string());
    }

    chunks
}

fn unique_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for token in tokenize(text) {
        if seen.insert(token.clone()) {
            out.push(token);
        }
    }
    out
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
