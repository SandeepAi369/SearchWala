// ============================================================================
// SearchWala v5.0.1 - Data Models
// ============================================================================

use serde::{Deserialize, Serialize};

// --- Request ----------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
pub struct LlmConfig {
    pub provider: String,
    pub api_key: String,
    pub model: String,
    pub base_url: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    /// Max number of URLs to scrape (default: from config)
    pub max_results: Option<usize>,
    /// Optional focus mode: auto | fast | research | academic | reddit | youtube.
    /// When absent, defaults to "auto" (QueryIntelligence-driven).
    pub focus_mode: Option<String>,
    /// Optional BYOK LLM config for synthesized answer generation.
    pub llm: Option<LlmConfig>,
    /// Enable the SearchWala Copilot pre-computation rewriter
    pub enable_copilot: Option<bool>,
}

// --- Response ---------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub sources_found: usize,
    pub sources_processed: usize,
    pub results: Vec<SourceResult>,
    pub search_results: Vec<SearchHit>,
    pub copilot_query: Option<String>,
    pub llm_answer: Option<String>,
    pub llm_error: Option<String>,
    pub elapsed_seconds: f64,
    pub engine_stats: EngineStats,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct SourceResult {
    pub url: String,
    pub title: String,
    pub extracted_text: String,
    /// CHARACTER count. v7.0 set this from `text.len()` (bytes) in search.rs
    /// but `text.chars().count()` in ranking.rs — the same field in two units,
    /// which inflated Devanagari/CJK by 3x on one path.
    pub char_count: usize,
    pub engine: String,
    /// Publication date when the page declares one (P2-2 freshness ranking).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<chrono::DateTime<chrono::Utc>>,
    /// Detected content language, e.g. "hi" (drives language-mirrored answers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Real extraction confidence 0.0-1.0. v7.0 hardcoded 0.7 in ranking.rs
    /// with a comment promising this value.
    pub confidence: f64,
    /// Which extractor produced the text ("trafilatura" | "readability" | "json-ld").
    #[serde(skip_serializing_if = "str::is_empty")]
    pub extractor: &'static str,
}

#[derive(Debug, Serialize, Clone)]
pub struct SearchHit {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub engine: String,
}

#[derive(Debug, Serialize, Default)]
pub struct EngineStats {
    pub engines_queried: Vec<String>,
    pub total_raw_results: usize,
    pub deduplicated_urls: usize,
}

// --- Search Result (internal, from engines) --------------------------------

#[derive(Debug, Clone)]
pub struct RawSearchResult {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub engine: String,
    /// Position in this engine's result list (1-based), **global across pages**.
    /// v7.0 reset this to 1 on every page, which destroyed the rank signal RRF
    /// depends on: a result at true position 21 scored like position 1.
    pub rank_position: usize,
    /// Publication date when the engine reports one (news verticals do).
    pub published: Option<chrono::DateTime<chrono::Utc>>,
}

// --- Health -----------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub engines: Vec<String>,
    pub uptime_seconds: u64,
}

// --- Config Info ------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    pub version: String,
    pub engines: Vec<String>,
    pub max_urls: usize,
    pub scrape_timeout_secs: u64,
    pub concurrent_scrapes: usize,
    pub concurrent_engines: usize,
    pub jitter_min_ms: u64,
    pub jitter_max_ms: u64,
    pub proxy_cooldown_secs: u64,
    pub user_agents_count: usize,
}
