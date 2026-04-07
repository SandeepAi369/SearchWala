// ══════════════════════════════════════════════════════════════════════════════
// Swift Search Agent v3.0 — Data Models
// ══════════════════════════════════════════════════════════════════════════════

use serde::{Deserialize, Serialize};

// ─── Request ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    /// Max number of URLs to scrape (default: from config)
    pub max_results: Option<usize>,
}

// ─── Response ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub sources_found: usize,
    pub sources_processed: usize,
    pub results: Vec<SourceResult>,
    pub elapsed_seconds: f64,
    pub engine_stats: EngineStats,
}

#[derive(Debug, Serialize)]
pub struct SourceResult {
    pub url: String,
    pub title: String,
    pub extracted_text: String,
    pub char_count: usize,
    pub engine: String,
}

#[derive(Debug, Serialize, Default)]
pub struct EngineStats {
    pub engines_queried: Vec<String>,
    pub total_raw_results: usize,
    pub deduplicated_urls: usize,
}

// ─── Search Result (internal, from engines) ──────────────────────────────────

#[derive(Debug, Clone)]
pub struct RawSearchResult {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub engine: String,
}

// ─── Health ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub engines: Vec<String>,
    pub uptime_seconds: u64,
}

// ─── Config Info ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    pub version: String,
    pub engines: Vec<String>,
    pub max_urls: usize,
    pub scrape_timeout_secs: u64,
    pub concurrent_scrapes: usize,
    pub user_agents_count: usize,
}
