// ============================================================================
// SearchWala v6.1.0 - Evaluation Harness (C9)
//
// Metrics-driven evaluation framework for measuring ranking quality
// and pipeline latency. Every ranking change must be validated against
// the golden query set before merging.
//
// Metrics implemented:
//   - nDCG@10 (Normalized Discounted Cumulative Gain at rank 10)
//   - MRR (Mean Reciprocal Rank)
//   - Recall@k
//   - Per-stage latency percentiles (p50/p95/p99)
//   - Per-engine success rate
//   - Cache hit ratio
//
// Reference: Järvelin & Kekäläinen (2002), Cumulated Gain-Based
// Evaluation of IR Techniques — the nDCG standard.
// ============================================================================

use std::collections::HashMap;
use std::time::Duration;

/// Relevance grade for a (query, url) pair in the golden set.
/// Follows the standard 4-point scale from TREC/MS MARCO.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum RelevanceGrade {
    /// Not relevant (grade 0)
    NotRelevant = 0,
    /// Marginally relevant (grade 1)
    Marginal = 1,
    /// Relevant (grade 2)
    Relevant = 2,
    /// Highly relevant / perfect answer (grade 3)
    Perfect = 3,
}

impl RelevanceGrade {
    pub fn score(self) -> f64 {
        self as u8 as f64
    }
}

/// A single golden query with hand-labeled relevance judgments.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct GoldenQuery {
    /// The search query string
    pub query: String,
    /// Expected intent classification
    pub expected_intent: String,
    /// Expected focus mode (lite/research/specialized)
    pub expected_mode: String,
    /// Map of URL → relevance grade (hand-labeled)
    pub relevance_judgments: HashMap<String, RelevanceGrade>,
    /// Optional: expected answer keywords for LLM quality check
    pub expected_keywords: Vec<String>,
}

/// The complete golden query set loaded from JSON.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct GoldenQuerySet {
    pub version: String,
    pub queries: Vec<GoldenQuery>,
}

// =============================================================================
// nDCG@k — Normalized Discounted Cumulative Gain
// =============================================================================

/// Compute DCG@k for a ranked list of relevance grades.
/// DCG@k = Σ_{i=1}^{k} (2^rel_i - 1) / log2(i + 1)
fn dcg_at_k(relevance_scores: &[f64], k: usize) -> f64 {
    relevance_scores
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, &rel)| {
            let gain = (2.0_f64).powf(rel) - 1.0;
            let discount = (i as f64 + 2.0).log2(); // log2(i+2) since i is 0-indexed
            gain / discount
        })
        .sum()
}

/// Compute nDCG@k for a ranked list against ideal ordering.
/// nDCG@k = DCG@k / IDCG@k where IDCG is DCG of the ideal ranking.
///
/// Returns 0.0 if there are no relevant documents (IDCG = 0).
pub fn ndcg_at_k(ranked_urls: &[String], judgments: &HashMap<String, RelevanceGrade>, k: usize) -> f64 {
    if ranked_urls.is_empty() || judgments.is_empty() {
        return 0.0;
    }

    // Get relevance scores in ranked order
    let relevance_scores: Vec<f64> = ranked_urls
        .iter()
        .map(|url| {
            // Normalize URL for matching
            let normalized = crate::url_utils::normalize_url(url)
                .unwrap_or_else(|| url.clone());
            judgments
                .get(&normalized)
                .or_else(|| judgments.get(url))
                .map(|g| g.score())
                .unwrap_or(0.0)
        })
        .collect();

    let dcg = dcg_at_k(&relevance_scores, k);

    // Compute ideal DCG: sort all judgment scores descending
    let mut ideal_scores: Vec<f64> = judgments.values().map(|g| g.score()).collect();
    ideal_scores.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let idcg = dcg_at_k(&ideal_scores, k);

    if idcg <= 0.0 {
        0.0
    } else {
        dcg / idcg
    }
}

// =============================================================================
// MRR — Mean Reciprocal Rank
// =============================================================================

/// Compute Reciprocal Rank for a single query.
/// RR = 1 / rank_of_first_relevant_result (0 if none found).
pub fn reciprocal_rank(ranked_urls: &[String], judgments: &HashMap<String, RelevanceGrade>) -> f64 {
    for (i, url) in ranked_urls.iter().enumerate() {
        let normalized = crate::url_utils::normalize_url(url)
            .unwrap_or_else(|| url.clone());
        let is_relevant = judgments
            .get(&normalized)
            .or_else(|| judgments.get(url))
            .map(|g| *g != RelevanceGrade::NotRelevant)
            .unwrap_or(false);

        if is_relevant {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

/// Compute MRR across multiple queries.
pub fn mean_reciprocal_rank(results: &[(Vec<String>, HashMap<String, RelevanceGrade>)]) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
    let sum: f64 = results
        .iter()
        .map(|(urls, judgments)| reciprocal_rank(urls, judgments))
        .sum();
    sum / results.len() as f64
}

// =============================================================================
// Recall@k
// =============================================================================

/// Compute Recall@k: fraction of relevant documents found in top-k results.
pub fn recall_at_k(ranked_urls: &[String], judgments: &HashMap<String, RelevanceGrade>, k: usize) -> f64 {
    let total_relevant = judgments
        .values()
        .filter(|g| **g != RelevanceGrade::NotRelevant)
        .count();

    if total_relevant == 0 {
        return 0.0;
    }

    let found_relevant = ranked_urls
        .iter()
        .take(k)
        .filter(|url| {
            let normalized = crate::url_utils::normalize_url(url)
                .unwrap_or_else(|| (*url).clone());
            judgments
                .get(&normalized)
                .or_else(|| judgments.get(*url))
                .map(|g| *g != RelevanceGrade::NotRelevant)
                .unwrap_or(false)
        })
        .count();

    found_relevant as f64 / total_relevant as f64
}

// =============================================================================
// Latency Tracking — Per-Stage Percentiles
// =============================================================================

/// Tracks latency samples for a single pipeline stage.
#[derive(Debug, Clone, Default)]
pub struct LatencyTracker {
    samples: Vec<Duration>,
}

impl LatencyTracker {
    pub fn new() -> Self {
        Self { samples: Vec::new() }
    }

    pub fn record(&mut self, duration: Duration) {
        self.samples.push(duration);
    }

    pub fn count(&self) -> usize {
        self.samples.len()
    }

    /// Compute the p-th percentile (0-100).
    pub fn percentile(&self, p: f64) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }

        let mut sorted: Vec<Duration> = self.samples.clone();
        sorted.sort();

        let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
        let idx = idx.min(sorted.len() - 1);
        sorted[idx]
    }

    pub fn p50(&self) -> Duration {
        self.percentile(50.0)
    }

    pub fn p95(&self) -> Duration {
        self.percentile(95.0)
    }

    pub fn p99(&self) -> Duration {
        self.percentile(99.0)
    }

    pub fn mean(&self) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let total: Duration = self.samples.iter().sum();
        total / self.samples.len() as u32
    }
}

/// All pipeline stage latency trackers.
#[derive(Debug, Clone, Default)]
pub struct PipelineLatency {
    pub engine_dispatch: LatencyTracker,
    pub scrape: LatencyTracker,
    pub extract: LatencyTracker,
    pub rank: LatencyTracker,
    pub rerank: LatencyTracker,
    pub llm_synthesis: LatencyTracker,
    pub total: LatencyTracker,
}

// =============================================================================
// Engine Health Tracking
// =============================================================================

/// Per-engine success/failure tracking for pruning dead-weight engines.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EngineHealth {
    pub total_queries: u64,
    pub successful_queries: u64,
    pub failed_queries: u64,
    pub total_results_returned: u64,
    /// Average response time in milliseconds
    pub avg_response_ms: f64,
    /// Results that appeared in the final top-10 answer
    pub results_in_final_answer: u64,
}

impl EngineHealth {
    pub fn success_rate(&self) -> f64 {
        if self.total_queries == 0 {
            return 0.0;
        }
        self.successful_queries as f64 / self.total_queries as f64
    }

    pub fn contribution_rate(&self) -> f64 {
        if self.total_results_returned == 0 {
            return 0.0;
        }
        self.results_in_final_answer as f64 / self.total_results_returned as f64
    }
}

// =============================================================================
// Cache Metrics
// =============================================================================

/// Cache hit/miss tracking for the 3-tier cache system.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CacheMetrics {
    pub engine_cache_hits: u64,
    pub engine_cache_misses: u64,
    pub scrape_cache_hits: u64,
    pub scrape_cache_misses: u64,
    pub answer_cache_hits: u64,
    pub answer_cache_misses: u64,
}

impl CacheMetrics {
    pub fn engine_hit_ratio(&self) -> f64 {
        let total = self.engine_cache_hits + self.engine_cache_misses;
        if total == 0 { 0.0 } else { self.engine_cache_hits as f64 / total as f64 }
    }

    pub fn scrape_hit_ratio(&self) -> f64 {
        let total = self.scrape_cache_hits + self.scrape_cache_misses;
        if total == 0 { 0.0 } else { self.scrape_cache_hits as f64 / total as f64 }
    }

    pub fn answer_hit_ratio(&self) -> f64 {
        let total = self.answer_cache_hits + self.answer_cache_misses;
        if total == 0 { 0.0 } else { self.answer_cache_hits as f64 / total as f64 }
    }
}

// =============================================================================
// Full Evaluation Report
// =============================================================================

/// Complete evaluation report capturing all quality and performance metrics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EvalReport {
    /// Timestamp of evaluation run
    pub timestamp: String,
    /// Version being evaluated
    pub version: String,
    /// Number of golden queries evaluated
    pub queries_evaluated: usize,

    // ── Ranking quality ──
    /// Mean nDCG@10 across all golden queries
    pub mean_ndcg_at_10: f64,
    /// Mean Reciprocal Rank
    pub mrr: f64,
    /// Mean Recall@10
    pub mean_recall_at_10: f64,
    /// Mean Recall@20
    pub mean_recall_at_20: f64,

    // ── Latency percentiles (milliseconds) ──
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    pub latency_p99_ms: f64,

    // ── Engine health ──
    pub engine_health: HashMap<String, EngineHealth>,

    // ── Cache performance ──
    pub cache_metrics: CacheMetrics,

    // ── Extraction quality ──
    /// Average extraction confidence across all scraped pages
    pub avg_extraction_confidence: f64,
}

// =============================================================================
// Golden Query Set Loader
// =============================================================================

/// Load golden queries from a JSON file.
/// Expected path: benchmarks/golden_queries.json
pub fn load_golden_queries(path: &str) -> Result<GoldenQuerySet, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read golden queries from {}: {}", path, e))?;
    serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse golden queries: {}", e))
}

// =============================================================================
// Evaluation Runner
// =============================================================================

/// Run nDCG@10 evaluation for a set of query results against golden judgments.
///
/// `results` is a Vec of (query_string, ranked_urls) pairs.
/// Returns per-query nDCG@10 scores and the mean.
pub fn evaluate_ranking(
    results: &[(String, Vec<String>)],
    golden: &GoldenQuerySet,
) -> (Vec<(String, f64)>, f64) {
    let golden_map: HashMap<String, &GoldenQuery> = golden
        .queries
        .iter()
        .map(|gq| (gq.query.to_lowercase(), gq))
        .collect();

    let mut scores: Vec<(String, f64)> = Vec::new();

    for (query, ranked_urls) in results {
        let q_lower = query.to_lowercase();
        if let Some(gq) = golden_map.get(&q_lower) {
            let ndcg = ndcg_at_k(ranked_urls, &gq.relevance_judgments, 10);
            scores.push((query.clone(), ndcg));
        }
    }

    let mean = if scores.is_empty() {
        0.0
    } else {
        scores.iter().map(|(_, s)| s).sum::<f64>() / scores.len() as f64
    };

    (scores, mean)
}

/// Generate a full evaluation report from collected metrics.
pub fn build_eval_report(
    pipeline_latency: &PipelineLatency,
    engine_health: HashMap<String, EngineHealth>,
    cache_metrics: CacheMetrics,
    ranking_results: &[(String, Vec<String>)],
    golden: &GoldenQuerySet,
    avg_extraction_confidence: f64,
) -> EvalReport {
    let (_, mean_ndcg) = evaluate_ranking(ranking_results, golden);

    // Compute MRR
    let golden_map: HashMap<String, &GoldenQuery> = golden
        .queries
        .iter()
        .map(|gq| (gq.query.to_lowercase(), gq))
        .collect();

    let mrr_pairs: Vec<(Vec<String>, HashMap<String, RelevanceGrade>)> = ranking_results
        .iter()
        .filter_map(|(query, urls)| {
            golden_map.get(&query.to_lowercase()).map(|gq| {
                (urls.clone(), gq.relevance_judgments.clone())
            })
        })
        .collect();

    let mrr = mean_reciprocal_rank(&mrr_pairs);

    // Compute mean Recall@10 and Recall@20
    let mut recall_10_sum = 0.0;
    let mut recall_20_sum = 0.0;
    let mut recall_count = 0usize;

    for (query, urls) in ranking_results {
        if let Some(gq) = golden_map.get(&query.to_lowercase()) {
            recall_10_sum += recall_at_k(urls, &gq.relevance_judgments, 10);
            recall_20_sum += recall_at_k(urls, &gq.relevance_judgments, 20);
            recall_count += 1;
        }
    }

    let mean_recall_10 = if recall_count > 0 { recall_10_sum / recall_count as f64 } else { 0.0 };
    let mean_recall_20 = if recall_count > 0 { recall_20_sum / recall_count as f64 } else { 0.0 };

    EvalReport {
        timestamp: chrono::Utc::now().to_rfc3339(),
        version: "6.1.0".to_string(),
        queries_evaluated: ranking_results.len(),
        mean_ndcg_at_10: mean_ndcg,
        mrr,
        mean_recall_at_10: mean_recall_10,
        mean_recall_at_20: mean_recall_20,
        latency_p50_ms: pipeline_latency.total.p50().as_secs_f64() * 1000.0,
        latency_p95_ms: pipeline_latency.total.p95().as_secs_f64() * 1000.0,
        latency_p99_ms: pipeline_latency.total.p99().as_secs_f64() * 1000.0,
        engine_health,
        cache_metrics,
        avg_extraction_confidence,
    }
}
