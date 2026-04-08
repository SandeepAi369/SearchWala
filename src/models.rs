// ============================================================================
// Swift Search Agent v4.0 - Data Models
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
    /// Optional BYOK LLM config for synthesized answer generation.
    pub llm: Option<LlmConfig>,
}

// --- Response ---------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub sources_found: usize,
    pub sources_processed: usize,
    pub results: Vec<SourceResult>,
    pub search_results: Vec<SearchHit>,
    pub llm_answer: Option<String>,
    pub llm_error: Option<String>,
    pub elapsed_seconds: f64,
    pub engine_stats: EngineStats,
}

#[derive(Debug, Serialize, Clone)]
pub struct SourceResult {
    pub url: String,
    pub title: String,
    pub extracted_text: String,
    pub char_count: usize,
    pub engine: String,
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
    pub user_agents_count: usize,
}
