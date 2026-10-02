// ============================================================================
// SearchWala v5.0.1 - Data Models
// ============================================================================

use serde::{Deserialize, Serialize};

// --- Request ----------------------------------------------------------------

#[derive(Deserialize, Clone)]
pub struct LlmConfig {
    pub provider: String,
    pub api_key: String,
    pub model: String,
    pub base_url: Option<String>,
    pub timeout_ms: Option<u64>,
    /// Per-call output budget. Set by the server from the search mode; clients
    /// may also pass it.
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
}

/// Hand-written so a stray `{:?}` cannot write the user's API key to the log.
///
/// v7.0 derived `Debug` on a struct holding a plaintext BYOK key. One
/// `tracing::debug!("{:?}", cfg)` anywhere — now or in future — would have
/// leaked it.
impl std::fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("timeout_ms", &self.timeout_ms)
            .field("max_output_tokens", &self.max_output_tokens)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    /// Subject-area focus — tech | science | finance | health | news.
    /// Composes with `focus_mode` ("deep" + "health" is valid).
    #[serde(default)]
    pub domain: Option<String>,
    /// Max number of URLs to scrape (default: from config)
    pub max_results: Option<usize>,
    /// Mode: auto | fast | deep | study | academic | reddit | youtube.
    /// Legacy spellings still work: lite→auto, research→deep,
    /// specialized_<domain>→auto+domain. Absent → auto.
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
    /// Mode actually used ("fast", "auto", "deep", "study", ...).
    pub mode: String,
    /// Resolved tier, e.g. "auto:light" — shows how Auto sized this query.
    pub tier: String,
    pub domain: String,
    /// The queries actually sent to engines (original first).
    pub sub_queries: Vec<String>,
    pub sources_found: usize,
    pub sources_processed: usize,
    /// Pages whose text was extracted (before chunking).
    pub sources_extracted: usize,
    /// Characters of text extracted across those pages.
    pub chars_extracted: usize,
    pub results: Vec<SourceResult>,
    pub search_results: Vec<SearchHit>,
    pub copilot_query: Option<String>,
    pub llm_answer: Option<String>,
    pub llm_error: Option<String>,
    /// Maps citation number n (1-based) to an index into `results`.
    ///
    /// Without this the UI cannot turn `[n]` into a link: the numbers refer to
    /// positions in the lost-in-middle REORDERED context, not to `results`
    /// order. v7.0 had neither the map nor clickable citations, and its own
    /// internal resolution was wrong for every index from [2] onward.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub citation_map: Vec<usize>,
    /// Per-citation support check (token overlap with the cited source).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub citation_checks: Vec<crate::llm::CitationCheck>,
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
    /// Comma-separated list of every engine that surfaced this URL.
    pub engine: String,
    /// Cross-family RRF consensus score. The UI can now show WHY a result
    /// ranks where it does; v7.0 rendered this list in arrival order with no
    /// score at all.
    #[serde(default)]
    pub rrf_score: f64,
    /// How many INDEPENDENT engine families found it (see engines::engine_family).
    #[serde(default)]
    pub found_by_families: usize,
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
    /// Text the engine already supplied (e.g. a paper abstract from OpenAlex).
    /// When substantial, the scrape stage uses it instead of fetching the URL —
    /// publisher landing pages are slow, often paywalled, and often blocked.
    pub full_text: Option<String>,
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
    /// What each mode does by default: engines, budgets, synthesis.
    pub modes: serde_json::Value,
    pub concurrent_scrapes: usize,
    pub concurrent_engines: usize,
    pub jitter_min_ms: u64,
    pub jitter_max_ms: u64,
    pub proxy_cooldown_secs: u64,
    pub user_agents_count: usize,
}
