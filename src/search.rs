use chrono::Datelike;
// ============================================================================
// SearchWala v7.0.0 - Search Orchestrator (Intelligence-Driven)
// Features:
//   - QueryIntelligence: intent detection, temporal classification, entity extraction
//   - RRF k=60 (Cormack 2009): cross-engine consensus ranking
//   - BM25+ (Lv & Zhai 2011) + MMR (Carbonell & Goldstein 1998) chunk ranking
//   - Lost-in-Middle context ordering (Liu et al. 2024)
//   - JoinSet engine dispatch with deadline-based early cancellation
//   - Per-stage timing instrumentation (engine, scrape, rank, llm)
//   - Citation faithfulness verification + quality-gated LLM retry
//   - Intent-aware LLM prompts and source count optimization
//   - Iterative deep research: multi-batch LLM synthesis
//   - TempDb session tracking; conversations are saved by the caller (chats.rs)
// ============================================================================

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use scraper::Html;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config;
use crate::engines;
use crate::extractor;
use crate::fetcher::{self, Fetcher};
use crate::llm;
use crate::models::*;
use crate::modes;
use crate::proxy_pool::ProxyPoolManager;
use crate::query_intel;
use crate::ranking;
use crate::synthesis;
use crate::url_utils;

/// Optional domain-level inclusion/exclusion rules for surgical source control.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomainFilter {
    /// Normalized lowercase domains to query and prioritize (or restrict to when `strict == true`).
    pub include: Vec<String>,
    /// Normalized lowercase domains to drop before triage, summarization, and scraping.
    pub exclude: Vec<String>,
    /// When true and `include` is non-empty, strictly restrict results and scraping to `include`.
    /// When false (default), SearchWala keeps its own organic consensus answers intact while
    /// prioritizing `include` at the top.
    pub strict: bool,
}

impl DomainFilter {
    pub fn is_excluded(&self, url: &str) -> bool {
        self.exclude
            .iter()
            .any(|d| url_utils::host_matches_domain(url, d))
    }

    pub fn is_included(&self, url: &str) -> bool {
        self.include
            .iter()
            .any(|d| url_utils::host_matches_domain(url, d))
    }

    pub fn is_strict_active(&self) -> bool {
        self.strict && !self.include.is_empty()
    }

    pub fn allows_url(&self, url: &str) -> bool {
        if self.is_excluded(url) {
            return false;
        }
        if self.is_strict_active() {
            return self.is_included(url);
        }
        true
    }
}

/// Parameters for one search request.
#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub query: String,
    /// Explicit URL budget. Clamped into the mode's bounds; `None` means the
    /// mode decides.
    pub max_results: Option<usize>,
    pub focus_mode: Option<String>,
    /// Subject-area focus, composable with any mode.
    pub domain: Option<String>,
    pub llm: Option<LlmConfig>,
    pub enable_copilot: bool,
    /// Earlier turns of the conversation for the LLM, oldest first. Empty
    /// when memory is off.
    pub memory: Vec<crate::chats::PriorTurn>,
    /// Optional domain include/exclude rules and strict-mode toggle.
    pub domain_filter: DomainFilter,
}

/// Pipeline progress, emitted to streaming clients.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Progress {
    Plan {
        mode: String,
        tier: String,
        domain: String,
        engines: usize,
        sub_queries: Vec<String>,
        max_urls: usize,
    },
    Engines {
        raw_results: usize,
        families: usize,
        ms: u128,
    },
    Sources {
        results: Vec<SearchHit>,
    },
    Extracted {
        sources: usize,
        chars: usize,
        ms: u128,
    },
    Synthesizing {
        synthesis: String,
    },
    /// Best verbatim answer sentence(s), before synthesis starts.
    Highlight {
        highlight: crate::highlight::Highlight,
    },
}

async fn emit(tx: &Option<tokio::sync::mpsc::Sender<Progress>>, p: Progress) {
    if let Some(t) = tx {
        let _ = t.send(p).await;
    }
}

/// One consensus-ranked search hit returned by [`search_serp_only`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct SerpHit {
    pub rank: usize,
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub engines: Vec<String>,
    pub found_by_families: usize,
    pub rrf_score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<chrono::DateTime<chrono::Utc>>,
    /// Engine-supplied abstract/text when available without scraping (e.g. OpenAlex, arXiv).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abstract_text: Option<String>,
}

/// Output of [`search_serp_only`] (Stages 1–4: engine dispatch, RRF consensus,
/// and pre-scrape triage — without page scraping or LLM synthesis).
#[derive(Debug, serde::Serialize)]
pub struct SerpSearchResponse {
    pub query: String,
    pub mode: String,
    pub tier: String,
    pub domain: String,
    pub intent: String,
    pub is_time_sensitive: bool,
    pub sub_queries: Vec<String>,
    pub elapsed_seconds: f64,
    pub engine_stats: EngineStats,
    pub results: Vec<SerpHit>,
    pub discovered_domains: Vec<DiscoveredDomain>,
    pub total_discovered_domains: usize,
}

/// Options for [`fetch_and_extract_url`].
#[derive(Debug, Clone)]
pub struct FetchExtractOptions {
    pub timeout: Duration,
    pub max_bytes: usize,
    pub allow_amp_fallback: bool,
    pub use_cache: bool,
    /// Accept `text/plain` and `text/markdown` responses directly (useful for
    /// docs, RFCs, and `llms.txt` when called from `extract_url`).
    pub allow_plain_text: bool,
}

impl Default for FetchExtractOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(12),
            max_bytes: config::max_html_bytes(),
            allow_amp_fallback: true,
            use_cache: true,
            allow_plain_text: true,
        }
    }
}

/// Result of [`fetch_and_extract_url`].
#[derive(Debug, Clone)]
pub struct ExtractedPage {
    pub url: String,
    pub final_url: String,
    pub extracted: crate::extractor::Extracted,
    pub char_count: usize,
    pub cached: bool,
    pub elapsed_ms: u64,
}

/// Typed failure modes for [`fetch_and_extract_url`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractError {
    InvalidUrl(String),
    BlockedBySsrf(String),
    UnsupportedContentType(String),
    AntiBotBlocked { status: u16 },
    HttpStatus { status: u16 },
    Timeout,
    NetworkError(String),
    UnusableContent { chars: usize },
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtractError::InvalidUrl(msg) => write!(f, "invalid URL: {msg}"),
            ExtractError::BlockedBySsrf(msg) => write!(f, "URL blocked by SSRF guard: {msg}"),
            ExtractError::UnsupportedContentType(ct) => {
                write!(f, "unsupported content type: {ct}")
            }
            ExtractError::AntiBotBlocked { status } => {
                write!(f, "blocked by anti-bot protection (HTTP {status})")
            }
            ExtractError::HttpStatus { status } => {
                write!(f, "upstream returned HTTP {status}")
            }
            ExtractError::Timeout => write!(f, "request timed out"),
            ExtractError::NetworkError(msg) => write!(f, "network error: {msg}"),
            ExtractError::UnusableContent { chars } => {
                write!(
                    f,
                    "extracted content too short or unreadable ({chars} chars)"
                )
            }
        }
    }
}

impl std::error::Error for ExtractError {}

/// Back-compatible entry point (no domain, no progress).
pub async fn execute_search(
    query: &str,
    max_results: Option<usize>,
    focus_mode: Option<String>,
    llm_config: Option<LlmConfig>,
    enable_copilot: Option<bool>,
    temp_db: Option<&crate::cache::TempDb>,
) -> SearchResponse {
    execute_search_full(
        SearchParams {
            query: query.to_string(),
            max_results,
            focus_mode,
            domain: None,
            llm: llm_config,
            enable_copilot: enable_copilot.unwrap_or(false),
            memory: Vec::new(),
            domain_filter: DomainFilter::default(),
        },
        temp_db,
        None,
    )
    .await
}

/// Fast SERP-only consensus search (Stages 1–4): runs query intelligence,
/// mode-appropriate parallel engine dispatch, cross-family RRF consensus, and
/// pre-scrape triage without fetching target pages.
pub async fn search_serp_only(
    query: &str,
    focus_mode: Option<&str>,
    domain: Option<&str>,
    max_results: Option<usize>,
    domain_filter: &DomainFilter,
    progress: Option<tokio::sync::mpsc::Sender<Progress>>,
) -> SerpSearchResponse {
    let start = std::time::Instant::now();
    let trimmed = query.trim().to_string();
    let (mode, domain_val) = modes::parse(focus_mode, domain);
    let intel = query_intel::analyze_query(&trimmed);
    let profile = modes::resolve(mode, domain_val, &intel, None);
    let result_limit = max_results.unwrap_or(15).clamp(1, 50);

    if trimmed.is_empty() {
        return SerpSearchResponse {
            query: trimmed,
            mode: mode.as_str().to_string(),
            tier: profile.tier.to_string(),
            domain: domain_val.as_str().to_string(),
            intent: format!("{:?}", intel.intent),
            is_time_sensitive: intel.is_time_sensitive,
            sub_queries: Vec::new(),
            elapsed_seconds: start.elapsed().as_secs_f64(),
            engine_stats: EngineStats::default(),
            results: Vec::new(),
            discovered_domains: Vec::new(),
            total_discovered_domains: 0,
        };
    }

    let effective_query = apply_recency_hint(&trimmed, &intel, mode);
    let mut query_variations = if profile.max_sub_queries > 1 && query_intel::multi_query_enabled()
    {
        modes::facet_queries(
            &effective_query,
            &intel,
            profile.facets,
            profile.max_sub_queries,
        )
    } else {
        vec![effective_query.clone()]
    };
    if query_variations.is_empty() {
        query_variations.push(effective_query.clone());
    }
    if profile.site_filter.is_some() {
        query_variations = query_variations
            .iter()
            .map(|q| modes::apply_site_filter(q, profile.site_filter))
            .collect();
    }

    let engine_list = engines::filter_healthy(profile.engines.clone());
    let engine_instances = engines::get_engines(&engine_list);

    emit(
        &progress,
        Progress::Plan {
            mode: mode.as_str().into(),
            tier: profile.tier.into(),
            domain: domain_val.as_str().into(),
            engines: engine_instances.len(),
            sub_queries: query_variations.clone(),
            max_urls: result_limit,
        },
    )
    .await;

    let (plain_fetcher, stealth_fetcher) = fetcher::build_pair(None);
    let stealth_engine_set: std::collections::HashSet<String> =
        config::stealth_engines().into_iter().collect();
    let stealth_mode = config::stealth_mode();
    let proxy_pool = ProxyPoolManager::from_env();
    let engine_concurrency = config::engine_concurrency().clamp(1, 40);
    let engine_semaphore = Arc::new(Semaphore::new(engine_concurrency));

    let stage_start = std::time::Instant::now();
    let (tagged, all_queries) = dispatch_with_domain_targets(
        &engine_instances,
        &query_variations,
        &effective_query,
        domain_filter,
        profile.site_filter.is_some(),
        &plain_fetcher,
        &stealth_fetcher,
        &stealth_engine_set,
        &stealth_mode,
        &proxy_pool,
        &engine_semaphore,
        config::jitter_min_ms(),
        config::jitter_max_ms(),
        engine_concurrency,
        profile.engine_deadline,
        profile.page_budget,
        profile.engine_early_exit,
    )
    .await;

    let fused_scores = compute_fused_scores(&tagged, &all_queries);
    let mut all_results: Vec<RawSearchResult> = tagged.into_iter().flat_map(|(_, v)| v).collect();

    if profile.site_filter.is_some() {
        all_results.retain(|r| modes::matches_site_filter(&r.url, profile.site_filter));
    }

    let engine_dispatch_ms = stage_start.elapsed().as_millis();
    let families: std::collections::HashSet<&str> = all_results
        .iter()
        .map(|r| engines::engine_family(&r.engine))
        .collect();
    emit(
        &progress,
        Progress::Engines {
            raw_results: all_results.len(),
            families: families.len(),
            ms: engine_dispatch_ms,
        },
    )
    .await;

    let total_raw = all_results.len();
    let sel = fuse_and_select_candidates(
        &all_results,
        fused_scores,
        &trimmed,
        &effective_query,
        &intel,
        &profile,
        result_limit,
        domain_filter,
    );
    let deduped_count = sel.deduplicated_urls;
    let hits = candidates_to_serp_hits(&sel.candidates, result_limit);
    let discovered_domains = sel.discovered_domains;
    let total_discovered_domains = sel.total_discovered_domains;

    let progress_hits: Vec<SearchHit> = hits
        .iter()
        .map(|h| SearchHit {
            url: h.url.clone(),
            title: h.title.clone(),
            snippet: h.snippet.clone(),
            engine: h.engines.join(","),
            rrf_score: h.rrf_score,
            found_by_families: h.found_by_families,
        })
        .collect();
    emit(
        &progress,
        Progress::Sources {
            results: progress_hits,
        },
    )
    .await;

    SerpSearchResponse {
        query: trimmed,
        mode: mode.as_str().to_string(),
        tier: profile.tier.to_string(),
        domain: domain_val.as_str().to_string(),
        intent: format!("{:?}", intel.intent),
        is_time_sensitive: intel.is_time_sensitive,
        sub_queries: query_variations,
        elapsed_seconds: start.elapsed().as_secs_f64(),
        engine_stats: EngineStats {
            engines_queried: engine_list,
            total_raw_results: total_raw,
            deduplicated_urls: deduped_count,
        },
        results: hits,
        discovered_domains,
        total_discovered_domains,
    }
}

/// Everything known about one candidate URL before it is scraped.
struct Candidate {
    url: String,
    title: String,
    snippet: String,
    engines: Vec<String>,
    families: Vec<&'static str>,
    rrf: f64,
    published: Option<chrono::DateTime<chrono::Utc>>,
    /// Text supplied by the engine itself (paper abstracts).
    full_text: Option<String>,
}

/// Append the dynamic current year for time-sensitive non-evergreen searches
/// when the query does not already name a year.
fn apply_recency_hint(
    effective_query: &str,
    intel: &query_intel::QueryIntelligence,
    mode: modes::Mode,
) -> String {
    if intel.is_time_sensitive && !matches!(mode, modes::Mode::Study | modes::Mode::Academic) {
        let q_lower = effective_query.to_lowercase();
        let current_year = chrono::Utc::now().year();
        let has_year =
            ((current_year - 15)..=(current_year + 10)).any(|y| q_lower.contains(&y.to_string()));
        if !has_year {
            return format!("{} {}", effective_query, current_year);
        }
    }
    effective_query.to_string()
}

/// Compute weighted cross-query fusion scores when multiple sub-queries ran.
fn compute_fused_scores(
    tagged: &[(usize, Vec<RawSearchResult>)],
    query_variations: &[String],
) -> Option<HashMap<String, f64>> {
    if query_variations.len() <= 1 {
        return None;
    }
    // Regroup per sub-query and fuse with the ORIGINAL weighted 2x. The
    // original is `query_variations[0]` — it already carries any site:
    // filter, so comparing against the bare query would never match and
    // the 2x weight would silently never apply.
    let mut grouped: Vec<Vec<RawSearchResult>> = vec![Vec::new(); query_variations.len()];
    for (idx, batch) in tagged {
        if let Some(slot) = grouped.get_mut(*idx) {
            slot.extend(batch.iter().cloned());
        }
    }
    let per_query: Vec<(String, Vec<RawSearchResult>)> =
        query_variations.iter().cloned().zip(grouped).collect();
    Some(ranking::cross_query_scores(
        &per_query,
        &query_variations[0],
    ))
}

/// Maximum targeted `site:` queries emitted for `include_domains`.
const MAX_TARGETED_DOMAIN_QUERIES: usize = 5;
/// Maximum distinct domains packaged in `discovered_domains`.
const MAX_DISCOVERED_DOMAINS: usize = 15;

/// Build clean `site:{domain}` queries for `include_domains` (only dispatched to
/// site-capable engines so JSON API engines never search literal `site:` strings).
fn build_targeted_domain_queries(
    effective_query: &str,
    domain_filter: &DomainFilter,
    has_mode_site_filter: bool,
) -> Vec<String> {
    if has_mode_site_filter || domain_filter.include.is_empty() {
        return Vec::new();
    }
    let q = effective_query.trim();
    if q.is_empty() || q.to_lowercase().contains("site:") {
        return Vec::new();
    }
    domain_filter
        .include
        .iter()
        .take(MAX_TARGETED_DOMAIN_QUERIES)
        .map(|d| format!("{q} site:{d}"))
        .collect()
}

/// Dispatch the primary mode roster concurrently with targeted `site:` queries
/// for `include_domains` (on site-capable engines only).
#[allow(clippy::too_many_arguments)]
async fn dispatch_with_domain_targets(
    engine_instances: &[std::sync::Arc<dyn engines::SearchEngine>],
    query_variations: &[String],
    effective_query: &str,
    domain_filter: &DomainFilter,
    has_mode_site_filter: bool,
    plain_fetcher: &Fetcher,
    stealth_fetcher: &Fetcher,
    stealth_engine_set: &std::collections::HashSet<String>,
    stealth_mode: &str,
    proxy_pool: &ProxyPoolManager,
    engine_semaphore: &Arc<Semaphore>,
    jitter_min: u64,
    jitter_max: u64,
    engine_concurrency: usize,
    deadline: Duration,
    page_budget: usize,
    early_exit: Option<(usize, usize)>,
) -> (Vec<(usize, Vec<RawSearchResult>)>, Vec<String>) {
    let targeted_queries =
        build_targeted_domain_queries(effective_query, domain_filter, has_mode_site_filter);

    if targeted_queries.is_empty() {
        let tagged = dispatch_engines(
            engine_instances,
            query_variations,
            plain_fetcher,
            stealth_fetcher,
            stealth_engine_set,
            stealth_mode,
            proxy_pool,
            engine_semaphore,
            jitter_min,
            jitter_max,
            engine_concurrency,
            deadline,
            page_budget,
            early_exit,
        )
        .await;
        return (tagged, query_variations.to_vec());
    }

    // Build site-capable engine instances for Track B (ensuring targeted `site:`
    // queries work even in study/academic modes whose rosters are API-heavy).
    // If primary site-capable engines have been quarantined mid-session (e.g.
    // Bing soft-block + DDG 202), automatically top up from fallback site-capable
    // engines so strict/targeted searches stay resilient.
    let mut site_engine_names = engines::filter_healthy(modes::site_capable_engines());
    if site_engine_names.len() < 3 {
        for backup in engines::filter_healthy(modes::site_capable_fallback_engines()) {
            if !site_engine_names.contains(&backup) {
                site_engine_names.push(backup);
            }
        }
    }
    let site_engine_instances = engines::get_engines(&site_engine_names);

    // When `strict_domains` is ON, skip throwaway unscoped web queries so we do
    // not double requests to rate-limited engines (Brave/DDG/Bing) for hits we
    // would immediately discard. Keep Wikipedia/arXiv API engines on the bare
    // query only if those domains are explicitly included.
    if domain_filter.is_strict_active() {
        let mut specialized_names: Vec<String> = Vec::new();
        if domain_filter
            .include
            .iter()
            .any(|d| d == "wikipedia.org" || d.ends_with(".wikipedia.org"))
        {
            specialized_names.push("wikipedia".to_string());
        }
        if domain_filter
            .include
            .iter()
            .any(|d| d == "arxiv.org" || d.ends_with(".arxiv.org"))
        {
            specialized_names.push("arxiv".to_string());
        }

        let strict_pages = page_budget.clamp(1, 2);
        let (mut tagged, mut all_queries) = if specialized_names.is_empty() {
            let targeted_tagged = dispatch_engines(
                &site_engine_instances,
                &targeted_queries,
                plain_fetcher,
                stealth_fetcher,
                stealth_engine_set,
                stealth_mode,
                proxy_pool,
                engine_semaphore,
                jitter_min,
                jitter_max,
                engine_concurrency,
                deadline,
                strict_pages,
                None,
            )
            .await;
            (targeted_tagged, targeted_queries)
        } else {
            let spec_instances = engines::get_engines(&engines::filter_healthy(specialized_names));
            let bare_queries = vec![effective_query.to_string()];
            let (mut targeted_tagged, spec_tagged) = tokio::join!(
                dispatch_engines(
                    &site_engine_instances,
                    &targeted_queries,
                    plain_fetcher,
                    stealth_fetcher,
                    stealth_engine_set,
                    stealth_mode,
                    proxy_pool,
                    engine_semaphore,
                    jitter_min,
                    jitter_max,
                    engine_concurrency,
                    deadline,
                    strict_pages,
                    None,
                ),
                dispatch_engines(
                    &spec_instances,
                    &bare_queries,
                    plain_fetcher,
                    stealth_fetcher,
                    stealth_engine_set,
                    stealth_mode,
                    proxy_pool,
                    engine_semaphore,
                    jitter_min,
                    jitter_max,
                    engine_concurrency,
                    deadline,
                    1,
                    None,
                )
            );
            let offset = targeted_queries.len();
            for (idx, batch) in spec_tagged {
                targeted_tagged.push((offset + idx, batch));
            }
            let mut combined = targeted_queries;
            combined.extend(bare_queries);
            (targeted_tagged, combined)
        };

        // Targeted `site:` batches must only retain results that actually match
        // `include_domains` (dropping any engine soft-block spam that ignored `site:`).
        for (_, batch) in &mut tagged {
            batch.retain(|r| {
                !domain_filter.is_excluded(&r.url) && domain_filter.is_included(&r.url)
            });
        }

        // Resilience fallback: if all targeted `site:` queries produced zero
        // URLs matching `include_domains` (e.g., transient Brave HTTP 429 burst
        // while Bing/DDG are quarantined), retry once with plain-text domain
        // queries (`"{query} {domain}"`) across the mode's healthy engines
        // (including API engines like Marginalia). Strict filtering still
        // guarantees zero domain leakage.
        let has_included_hit = tagged.iter().any(|(_, batch)| !batch.is_empty());
        if !has_included_hit && !engine_instances.is_empty() {
            let fallback_queries: Vec<String> = domain_filter
                .include
                .iter()
                .take(3)
                .map(|d| format!("{} {d}", effective_query.trim()))
                .collect();
            if !fallback_queries.is_empty() {
                let fallback_tagged = dispatch_engines(
                    engine_instances,
                    &fallback_queries,
                    plain_fetcher,
                    stealth_fetcher,
                    stealth_engine_set,
                    stealth_mode,
                    proxy_pool,
                    engine_semaphore,
                    jitter_min,
                    jitter_max,
                    engine_concurrency,
                    deadline.min(Duration::from_millis(2_500)),
                    1,
                    None,
                )
                .await;
                let offset = all_queries.len();
                for (idx, mut batch) in fallback_tagged {
                    batch.retain(|r| {
                        !domain_filter.is_excluded(&r.url) && domain_filter.is_included(&r.url)
                    });
                    if !batch.is_empty() {
                        tagged.push((offset + idx, batch));
                    }
                }
                all_queries.extend(fallback_queries);
            }
        }

        return (tagged, all_queries);
    }

    let (mut base_tagged, targeted_tagged) = tokio::join!(
        dispatch_engines(
            engine_instances,
            query_variations,
            plain_fetcher,
            stealth_fetcher,
            stealth_engine_set,
            stealth_mode,
            proxy_pool,
            engine_semaphore,
            jitter_min,
            jitter_max,
            engine_concurrency,
            deadline,
            page_budget,
            // Do not early-exit generic dispatch while targeted domain tasks run.
            None,
        ),
        dispatch_engines(
            &site_engine_instances,
            &targeted_queries,
            plain_fetcher,
            stealth_fetcher,
            stealth_engine_set,
            stealth_mode,
            proxy_pool,
            engine_semaphore,
            jitter_min,
            jitter_max,
            engine_concurrency,
            deadline,
            1,
            None,
        )
    );

    let offset = query_variations.len();
    for (idx, mut batch) in targeted_tagged {
        // Track B ran explicit `site:` queries; drop any engine soft-block junk
        // that does not actually belong to `include_domains`.
        batch.retain(|r| !domain_filter.is_excluded(&r.url) && domain_filter.is_included(&r.url));
        if !batch.is_empty() {
            base_tagged.push((offset + idx, batch));
        }
    }

    let mut all_queries = query_variations.to_vec();
    all_queries.extend(targeted_queries);
    (base_tagged, all_queries)
}

/// Summarize distinct domains discovered across the full post-exclusion sorted
/// candidate pool (before `max_urls` / `max_results` truncation).
fn summarize_discovered_domains(
    sorted_candidates: &[Candidate],
    max_domains: usize,
) -> (Vec<DiscoveredDomain>, usize) {
    let mut index_by_domain: HashMap<String, usize> = HashMap::new();
    let mut domains: Vec<DiscoveredDomain> = Vec::new();

    for c in sorted_candidates {
        let dom = url_utils::display_domain(&c.url);
        if dom.is_empty() {
            continue;
        }
        if let Some(&pos) = index_by_domain.get(&dom) {
            let entry = &mut domains[pos];
            entry.result_count += 1;
            if c.families.len() > entry.max_families {
                entry.max_families = c.families.len();
            }
            if c.rrf > entry.top_rrf_score {
                entry.top_rrf_score = c.rrf;
            }
        } else {
            index_by_domain.insert(dom.clone(), domains.len());
            domains.push(DiscoveredDomain {
                domain: dom,
                result_count: 1,
                max_families: c.families.len(),
                top_rrf_score: c.rrf,
                top_url: c.url.clone(),
                top_title: crate::util::truncate_chars(c.title.trim(), 120).to_string(),
            });
        }
    }

    let total = domains.len();
    domains.truncate(max_domains);
    (domains, total)
}

/// Output of [`fuse_and_select_candidates`], preserving true pre-truncation
/// deduplicated URL and discovered-domain counts alongside the truncated pool.
struct CandidateSelection {
    rrf_scores: HashMap<String, f64>,
    candidates: Vec<Candidate>,
    deduplicated_urls: usize,
    discovered_domains: Vec<DiscoveredDomain>,
    total_discovered_domains: usize,
}

/// Collapse raw engine results by canonical URL, apply domain exclusion/inclusion
/// rules, compute cross-family RRF consensus and pre-scrape triage, build the
/// compact `discovered_domains` rollup, and truncate to `max_urls`.
#[allow(clippy::too_many_arguments)]
fn fuse_and_select_candidates(
    all_results: &[RawSearchResult],
    fused_scores: Option<HashMap<String, f64>>,
    raw_query: &str,
    effective_query: &str,
    intel: &query_intel::QueryIntelligence,
    profile: &modes::ModeProfile,
    max_urls: usize,
    domain_filter: &DomainFilter,
) -> CandidateSelection {
    let rrf_scores = fused_scores.unwrap_or_else(|| ranking::compute_rrf_scores(all_results));

    let mut by_url: HashMap<String, Candidate> = HashMap::new();
    for item in all_results {
        if domain_filter.is_excluded(&item.url) {
            continue;
        }
        let key = url_utils::canonical_key(&item.url);
        let c = by_url.entry(key).or_insert_with(|| Candidate {
            url: item.url.clone(),
            title: item.title.clone(),
            snippet: item.snippet.clone(),
            engines: Vec::new(),
            families: Vec::new(),
            rrf: 0.0,
            published: item.published,
            full_text: None,
        });
        if c.snippet.len() < item.snippet.len() {
            c.snippet = item.snippet.clone();
        }
        if c.title.is_empty() {
            c.title = item.title.clone();
        }
        if c.published.is_none() {
            c.published = item.published;
        }
        if c.full_text.is_none() {
            c.full_text = item.full_text.clone();
        }
        if !c.engines.contains(&item.engine) {
            c.engines.push(item.engine.clone());
        }
        let fam = engines::engine_family(&item.engine);
        if !c.families.contains(&fam) {
            c.families.push(fam);
        }
    }

    let site_mode = profile.site_filter.is_some();
    let mut ranked: Vec<Candidate> = by_url
        .into_iter()
        .filter_map(|(key, mut c)| {
            // Site-filtered modes and explicitly included domains bypass the
            // general skip list.
            let bypass_skip = site_mode || domain_filter.is_included(&c.url);
            if !bypass_skip && url_utils::should_skip(&c.url) {
                return None;
            }
            c.rrf = rrf_scores.get(&key).copied().unwrap_or(0.0);
            Some(c)
        })
        .collect();

    // Selection score = consensus × mode/domain source preference + triage.
    let triage_terms = ranking::query_terms(&format!("{} {}", raw_query, effective_query));
    let selection_score = |c: &Candidate| -> f64 {
        pre_scrape_triage_score(
            c,
            raw_query,
            &triage_terms,
            intel,
            profile.mode,
            profile.domain,
        )
    };
    ranked.sort_by(|a, b| {
        selection_score(b)
            .partial_cmp(&selection_score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.families.len().cmp(&a.families.len()))
    });

    // When strict domain restriction is active, enforce it BEFORE summarizing
    // `discovered_domains` and counting `deduplicated_urls` so external/spam
    // domains can never leak into `discovered_domains` or `content[0].text`.
    if domain_filter.is_strict_active() {
        ranked.retain(|c| domain_filter.is_included(&c.url));
    }

    let deduplicated_urls = ranked.len();
    let (discovered_domains, total_discovered_domains) =
        summarize_discovered_domains(&ranked, MAX_DISCOVERED_DOMAINS);

    if domain_filter.is_strict_active() {
        ranked.truncate(max_urls);
    } else if !domain_filter.include.is_empty() && max_urls > 0 {
        // Balanced soft priority (`strict_domains: false`): prioritize included
        // domains at the top (round-robin across included domains, capped at
        // half the budget so SearchWala's organic consensus answers stay intact),
        // then fill remaining slots with organic consensus results and any
        // leftover included hits if organic runs short.
        let mut by_inc_domain: Vec<std::collections::VecDeque<Candidate>> =
            (0..domain_filter.include.len())
                .map(|_| std::collections::VecDeque::new())
                .collect();
        let mut organic: Vec<Candidate> = Vec::new();
        for c in ranked {
            if let Some(pos) = domain_filter
                .include
                .iter()
                .position(|d| url_utils::host_matches_domain(&c.url, d))
            {
                by_inc_domain[pos].push_back(c);
            } else {
                organic.push(c);
            }
        }
        // Order included domain buckets by their best candidate's score so the
        // strongest matching included domain always takes rank #1.
        by_inc_domain.sort_by(|a, b| {
            let sa = a.front().map(&selection_score).unwrap_or(f64::NEG_INFINITY);
            let sb = b.front().map(&selection_score).unwrap_or(f64::NEG_INFINITY);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });

        let priority_cap = (max_urls / 2).max(1);
        let mut prioritized: Vec<Candidate> = Vec::with_capacity(max_urls);
        let mut overflow_included: Vec<Candidate> = Vec::new();
        loop {
            let mut progressed = false;
            for bucket in &mut by_inc_domain {
                if let Some(cand) = bucket.pop_front() {
                    progressed = true;
                    if prioritized.len() < priority_cap {
                        prioritized.push(cand);
                    } else {
                        overflow_included.push(cand);
                    }
                }
            }
            if !progressed {
                break;
            }
        }

        for cand in organic {
            if prioritized.len() >= max_urls {
                break;
            }
            prioritized.push(cand);
        }
        for cand in overflow_included {
            if prioritized.len() >= max_urls {
                break;
            }
            prioritized.push(cand);
        }
        ranked = prioritized;
    } else {
        ranked.truncate(max_urls);
    }

    CandidateSelection {
        rrf_scores,
        candidates: ranked,
        deduplicated_urls,
        discovered_domains,
        total_discovered_domains,
    }
}

/// Project ranked candidates into [`SerpHit`] items for SERP-only callers.
fn candidates_to_serp_hits(candidates: &[Candidate], limit: usize) -> Vec<SerpHit> {
    candidates
        .iter()
        .take(limit)
        .enumerate()
        .map(|(i, c)| {
            let abstract_text = c
                .full_text
                .as_deref()
                .filter(|t| crate::util::char_len(t) >= 80)
                .map(|t| crate::util::truncate_chars(t.trim(), 2_000).to_string());
            SerpHit {
                rank: i + 1,
                url: c.url.clone(),
                title: c.title.clone(),
                snippet: crate::util::truncate_chars(&c.snippet, 400).to_string(),
                engines: c.engines.clone(),
                found_by_families: c.families.len(),
                rrf_score: c.rrf,
                published: c.published,
                abstract_text,
            }
        })
        .collect()
}

/// The page title to show and cite.
///
/// Trafilatura's title for Wikipedia is the article's short description, so
/// "Bain-marie" was cited as "type of heated bath" and "Boiling" as "type of
/// vaporization; bulk phenomenon". When the extracted title shares no word
/// with the search engine's title, the engine's is kept.
fn pick_title(url: &str, engine_title: &str, extracted: String) -> String {
    // Wikipedia: the article title is in the URL, exactly.
    if let Some(t) = wikipedia_title(url) {
        return t;
    }
    if extracted.trim().is_empty() {
        return engine_title.to_string();
    }
    if engine_title.trim().is_empty() {
        return extracted;
    }
    // Content words only: "the" shared by "The Legend of Tennessee
    // Moltisanti" and "episode of The Sopranos" is not agreement.
    let words = |s: &str| -> std::collections::HashSet<String> {
        crate::ranking::query_terms(s)
            .into_iter()
            .map(|w| crate::highlight::stem(&w))
            .collect()
    };
    if words(engine_title).is_disjoint(&words(&extracted)) {
        engine_title.to_string()
    } else {
        extracted
    }
}

/// "https://en.wikipedia.org/wiki/Invention_of_the_telephone" ->
/// "Invention of the telephone".
fn wikipedia_title(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    if !u.host_str()?.ends_with("wikipedia.org") {
        return None;
    }
    let slug = u.path().strip_prefix("/wiki/")?;
    if slug.is_empty() || slug.contains(':') {
        return None; // Special:, Talk:, File: pages
    }
    let title = urlencoding::decode(slug).ok()?.replace('_', " ");
    (!title.trim().is_empty()).then_some(title)
}

/// The highest-ranked pages a scrape early exit must wait for.
const MUST_FINISH_TOP: usize = 3;

/// The whole pipeline. Every mode-dependent decision is read from the
/// `ModeProfile` — see `modes.rs` for why that matters.
pub async fn execute_search_full(
    params: SearchParams,
    temp_db: Option<&crate::cache::TempDb>,
    progress: Option<tokio::sync::mpsc::Sender<Progress>>,
) -> SearchResponse {
    let start = std::time::Instant::now();
    let query = params.query.trim().to_string();
    let (mode, domain) = modes::parse(params.focus_mode.as_deref(), params.domain.as_deref());

    // Validate the caller's LLM config once. A bad config is reported in
    // `llm_error`; the search itself still runs.
    let mut params = params;
    let mut llm_config_error: Option<String> = None;
    if let Some(cfg) = params.llm.take() {
        match llm::prepare_config(cfg) {
            Ok(cfg) => params.llm = Some(cfg),
            Err(e) => {
                tracing::warn!("LLM config rejected: {}", e);
                llm_config_error = Some(e);
            }
        }
    }

    // ── Conversation memory formatting ──
    let (memory_text, memory_used) = match &params.llm {
        Some(cfg) if !params.memory.is_empty() => {
            // About a fifth of the model's window, at ~4 characters a token.
            let window = llm::ContextBudget::for_config(cfg).max_tokens;
            crate::chats::memory_block(&params.memory, (window * 4 / 5).clamp(1_500, 60_000))
        }
        _ => (String::new(), 0),
    };

    // ── Unified Semantic Query Understanding (LLM-First) ──
    // Runs a single fast semantic understanding pass that classifies category
    // (Product/Org, Live News, History/Static, General Everyday), resolves
    // follow-up context while locking entities, fixes phonetic/voice typos,
    // neutralizes stale years dynamically, and plans semantic sub-queries.
    let mut effective_query = query.clone();
    let mut search_query = None;
    let mut copilot_out = None;
    let mut semantic_plan: Option<crate::copilot::SemanticQueryPlan> = None;

    let should_run_semantic_plan = params.llm.is_some()
        && (!matches!(mode, modes::Mode::Fast)
            || params.enable_copilot
            || !params.memory.is_empty()
            || query.split_whitespace().count() > 5
            || !query.is_ascii());

    if should_run_semantic_plan {
        if let Some(cfg) = &params.llm {
            semantic_plan = crate::copilot::understand_query(&query, &params.memory, cfg, 3).await;
        }
    }

    if let Some(ref plan) = semantic_plan {
        tracing::info!(
            "Semantic Query Plan: category={} intent={:?} time_sensitive={} entities={:?} primary=[{}] sub_queries={:?}",
            plan.category,
            plan.intent,
            plan.is_time_sensitive,
            plan.entities,
            plan.primary_query,
            plan.sub_queries
        );
        effective_query = plan.primary_query.clone();
        if !effective_query.eq_ignore_ascii_case(&query) {
            search_query = Some(effective_query.clone());
        }
        if params.enable_copilot {
            copilot_out = Some(effective_query.clone());
        }
    } else {
        // Fallback if semantic understanding timed out or was skipped
        if let (Some(cfg), false) = (&params.llm, params.memory.is_empty()) {
            if let Some(q) = synthesis::standalone_query(&query, &params.memory, cfg).await {
                tracing::info!("Follow-up searched as: [{}]", q);
                effective_query = q.clone();
                search_query = Some(q);
            }
        }
        if params.enable_copilot {
            if let Some(cfg) = &params.llm {
                let rewritten = crate::copilot::rewrite_query(&effective_query, cfg).await;
                tracing::info!(
                    "Copilot rewrote query: [{}] -> [{}]",
                    effective_query,
                    rewritten
                );
                copilot_out = Some(rewritten.clone());
                effective_query = rewritten;
            }
        }
    }

    // ── Query intelligence + mode profile ──
    let mut intel = query_intel::analyze_query(&effective_query);
    // Merge entities from raw query as well so nothing from the raw prompt is lost
    for raw_ent in query_intel::analyze_query(&query).key_entities {
        if !intel
            .key_entities
            .iter()
            .any(|e| e.eq_ignore_ascii_case(&raw_ent))
        {
            intel.key_entities.push(raw_ent);
        }
    }
    if let Some(ref plan) = semantic_plan {
        intel.semantic_category = plan.category;
        if let Some(ref sem_intent) = plan.intent {
            intel.intent = sem_intent.clone();
        }
        if plan.is_time_sensitive {
            intel.is_time_sensitive = true;
        }
        if intel.semantic_category == query_intel::SemanticCategory::HistoryStatic
            && intel.intent == query_intel::QueryIntent::HowTo
        {
            intel.intent = query_intel::QueryIntent::Research;
        }
        if intel.is_time_sensitive && intel.intent == query_intel::QueryIntent::Factual {
            intel.intent = query_intel::QueryIntent::Temporal;
        }
        intel.boost_news_engines = intel.is_time_sensitive
            || intel.intent == query_intel::QueryIntent::Temporal
            || intel.semantic_category == query_intel::SemanticCategory::LiveNews;
        intel.corrective_query = plan.corrective_query.clone();

        // When the LLM Semantic Query Plan extracted canonical entities, use them
        // directly (avoiding naive lowercase prefix tokens from raw query fallback).
        if !plan.entities.is_empty() {
            let mut clean_entities = Vec::new();
            for ent in &plan.entities {
                let trimmed = ent.trim();
                if !trimmed.is_empty()
                    && !clean_entities
                        .iter()
                        .any(|e: &String| e.eq_ignore_ascii_case(trimmed))
                {
                    clean_entities.push(trimmed.to_string());
                }
            }
            intel.key_entities = clean_entities;
        }
    }

    let intel_query = effective_query.clone();
    let profile = modes::resolve(mode, domain, &intel, params.max_results);
    tracing::info!(
        "Mode {} [{}] domain={} category={} intent={} complexity={} | engines={} pages={} deadline={:?} \
         urls={} subq={} top_k={} synthesis={:?}",
        mode.as_str(), profile.tier, domain.as_str(), intel.semantic_category, intel.intent, intel.complexity,
        profile.engines.len(), profile.page_budget, profile.engine_deadline,
        profile.max_urls, profile.max_sub_queries, profile.llm_top_k, profile.synthesis
    );

    // ── Recency hint for engines (resolved dynamically via chrono::Utc::now().year()) ──
    // Skipped for Study and Academic: those topics are evergreen.
    effective_query = apply_recency_hint(&effective_query, &intel, mode);

    // ── Sub-queries ──
    let sub_query_cap = if semantic_plan.is_some() && !matches!(mode, modes::Mode::Fast) {
        profile.max_sub_queries.max(2)
    } else {
        profile.max_sub_queries
    };

    let mut query_variations: Vec<String> = if sub_query_cap > 1
        && query_intel::multi_query_enabled()
    {
        if let Some(ref plan) = semantic_plan {
            let mut v = vec![effective_query.clone()];
            for sq in &plan.sub_queries {
                if !v.iter().any(|existing| existing.eq_ignore_ascii_case(sq)) {
                    v.push(sq.clone());
                }
            }
            v.truncate(sub_query_cap);
            v
        } else {
            let mut planned = None;
            if profile.llm_query_planning {
                if let Some(cfg) = &params.llm {
                    planned = synthesis::plan_subqueries(
                        &effective_query,
                        cfg,
                        sub_query_cap.saturating_sub(1),
                    )
                    .await;
                }
            }
            match planned {
                Some(p) => {
                    tracing::info!("LLM planned {} sub-queries", p.len());
                    let mut v = vec![effective_query.clone()];
                    v.extend(p);
                    v.truncate(sub_query_cap);
                    v
                }
                None => {
                    modes::facet_queries(&effective_query, &intel, profile.facets, sub_query_cap)
                }
            }
        }
    } else {
        vec![effective_query.clone()]
    };
    if query_variations.is_empty() {
        query_variations.push(effective_query.clone());
    }
    // `site:` only goes to engines that honour it — the profile's roster for
    // site-filtered modes contains nothing else.
    if profile.site_filter.is_some() {
        query_variations = query_variations
            .iter()
            .map(|q| modes::apply_site_filter(q, profile.site_filter))
            .collect();
    }
    tracing::info!(
        "Sub-queries: {:?}",
        query_variations
            .iter()
            .map(|q| crate::util::truncate_chars(q, 60))
            .collect::<Vec<_>>()
    );

    let engine_list = engines::filter_healthy(profile.engines.clone());
    let engine_instances = engines::get_engines(&engine_list);

    emit(
        &progress,
        Progress::Plan {
            mode: mode.as_str().into(),
            tier: profile.tier.into(),
            domain: domain.as_str().into(),
            engines: engine_instances.len(),
            sub_queries: query_variations.clone(),
            max_urls: profile.max_urls,
        },
    )
    .await;

    // ── Dispatch ──
    let (plain_fetcher, stealth_fetcher) = fetcher::build_pair(None);
    let stealth_engine_set: std::collections::HashSet<String> =
        config::stealth_engines().into_iter().collect();
    let stealth_mode = config::stealth_mode();
    let proxy_pool = ProxyPoolManager::from_env();
    let engine_concurrency = config::engine_concurrency().clamp(1, 40);
    let engine_semaphore = Arc::new(Semaphore::new(engine_concurrency));

    let stage_start = std::time::Instant::now();
    let (tagged, all_queries) = dispatch_with_domain_targets(
        &engine_instances,
        &query_variations,
        &effective_query,
        &params.domain_filter,
        profile.site_filter.is_some(),
        &plain_fetcher,
        &stealth_fetcher,
        &stealth_engine_set,
        &stealth_mode,
        &proxy_pool,
        &engine_semaphore,
        config::jitter_min_ms(),
        config::jitter_max_ms(),
        engine_concurrency,
        profile.engine_deadline,
        profile.page_budget,
        profile.engine_early_exit,
    )
    .await;

    // Multi-query modes score each page by weighted fusion across sub-queries;
    // the raw results are kept either way so engine/family lists stay exact.
    let fused_scores = compute_fused_scores(&tagged, &all_queries);
    let mut all_results: Vec<RawSearchResult> = tagged.into_iter().flat_map(|(_, v)| v).collect();

    if profile.site_filter.is_some() {
        let before = all_results.len();
        all_results.retain(|r| modes::matches_site_filter(&r.url, profile.site_filter));
        tracing::info!("Site filter kept {}/{} results", all_results.len(), before);
    }

    let engine_dispatch_ms = stage_start.elapsed().as_millis();
    let families: std::collections::HashSet<&str> = all_results
        .iter()
        .map(|r| engines::engine_family(&r.engine))
        .collect();
    tracing::info!(
        "⏱ Stage [engine_dispatch]: {}ms — {} results, {} families",
        engine_dispatch_ms,
        all_results.len(),
        families.len()
    );
    emit(
        &progress,
        Progress::Engines {
            raw_results: all_results.len(),
            families: families.len(),
            ms: engine_dispatch_ms,
        },
    )
    .await;

    // ── Consensus ranking + source selection ──
    let stage_start = std::time::Instant::now();
    let total_raw = all_results.len();
    let sel = fuse_and_select_candidates(
        &all_results,
        fused_scores,
        &query,
        &effective_query,
        &intel,
        &profile,
        profile.max_urls,
        &params.domain_filter,
    );
    let rrf_ms = stage_start.elapsed().as_millis();
    let rrf_scores = sel.rrf_scores;
    let ranked = sel.candidates;
    let discovered_domains = sel.discovered_domains;
    let total_discovered_domains = sel.total_discovered_domains;
    let deduped_count = sel.deduplicated_urls;

    let search_results: Vec<SearchHit> = ranked
        .iter()
        .map(|c| SearchHit {
            url: c.url.clone(),
            title: c.title.clone(),
            snippet: crate::util::truncate_chars(&c.snippet, 400).to_string(),
            engine: c.engines.join(","),
            rrf_score: c.rrf,
            found_by_families: c.families.len(),
        })
        .collect();
    emit(
        &progress,
        Progress::Sources {
            results: search_results.clone(),
        },
    )
    .await;

    // ── Scrape ──
    let concurrency = config::concurrency().min(40);
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let scrape_client = shared_scrape_client().clone();
    let scrape_max_bytes = config::max_html_bytes();
    let scrape_timeout = profile.scrape_timeout;
    let snippet_fallback = profile.snippet_fallback;

    let stage_start = std::time::Instant::now();
    let mut join_set = JoinSet::new();
    // Early exit used to take whichever pages finished FIRST. In Fast mode
    // (exit at 6 sources) that dropped the #1 and #3 ranked pages for "boiling
    // point of water" — Wikipedia's "Boiling point", found by two engines —
    // and kept "Bain-marie", which then supplied the top passage. The
    // top-ranked pages must finish (or fail) before an early exit; the stage
    // deadline still caps the wait.
    let must_finish = MUST_FINISH_TOP.min(ranked.len());
    let mut pending_top: std::collections::HashSet<usize> = (0..must_finish).collect();
    let selection_rank: HashMap<String, usize> = ranked
        .iter()
        .enumerate()
        .map(|(i, c)| (c.url.clone(), i))
        .collect();
    for (rank_idx, c) in ranked.into_iter().enumerate() {
        let sem = semaphore.clone();
        let client = scrape_client.clone();
        join_set.spawn(async move {
            let result = async move {
                let engine_name = c.engines.join(",");

                // Engine-supplied text (OpenAlex / arXiv abstracts): no fetch.
                // Publisher landing pages are slow, often paywalled, often blocked.
                if let Some(ft) = c
                    .full_text
                    .as_ref()
                    .filter(|t| crate::util::char_len(t) >= 150)
                {
                    return Some(SourceResult {
                        url: c.url.clone(),
                        title: c.title.clone(),
                        extracted_text: ft.clone(),
                        char_count: crate::util::char_len(ft),
                        engine: engine_name,
                        published: c.published,
                        language: None,
                        byline: None,
                        site_name: None,
                        confidence: 0.9,
                        extractor: "abstract",
                    });
                }

                if let Some(cached) = crate::cache_layer::get_page(&c.url).await {
                    return Some(SourceResult {
                        url: c.url.clone(),
                        title: pick_title(&c.url, &c.title, cached.title.clone()),
                        extracted_text: cached.text.clone(),
                        char_count: crate::util::char_len(&cached.text),
                        engine: engine_name,
                        published: cached.published.or(c.published),
                        language: cached.language.clone(),
                        byline: cached.byline.clone(),
                        site_name: cached.site_name.clone(),
                        confidence: cached.confidence,
                        extractor: cached.source,
                    });
                }

                let scraped = {
                    let _permit = sem.acquire().await.ok()?;
                    scrape_url(&client, &c.url, scrape_timeout, scrape_max_bytes, mode).await
                };
                match scraped {
                    Some(e) => {
                        crate::cache_layer::put_page(&c.url, e.clone()).await;
                        let title = pick_title(&c.url, &c.title, e.title);
                        Some(SourceResult {
                            char_count: crate::util::char_len(&e.text),
                            url: c.url,
                            title,
                            extracted_text: e.text,
                            engine: engine_name,
                            published: e.published.or(c.published),
                            language: e.language,
                            byline: e.byline,
                            site_name: e.site_name,
                            confidence: e.confidence,
                            extractor: e.source,
                        })
                    }
                    // Blocked or empty page: in modes where that is routine
                    // (Reddit CAPTCHA walls, YouTube description-only pages) the
                    // engine snippet is the best text available. Low confidence so
                    // it ranks below any real extraction.
                    None if snippet_fallback && crate::util::char_len(&c.snippet) >= 80 => {
                        let text = format!("{}\n\n{}", c.title, c.snippet);
                        Some(SourceResult {
                            char_count: crate::util::char_len(&text),
                            url: c.url,
                            title: c.title,
                            extracted_text: text,
                            engine: engine_name,
                            published: c.published,
                            language: None,
                            byline: None,
                            site_name: None,
                            confidence: 0.35,
                            extractor: "snippet",
                        })
                    }
                    None => None,
                }
            }
            .await;
            (rank_idx, result)
        });
    }

    // Collect, stopping early once BOTH the source and text targets are met.
    // The thresholds come from the profile — v8.0 applied Auto's 25-source /
    // 80k-char exit to every mode, so Deep stopped as early as Auto. An
    // explicitly-set env var still overrides, for operators.
    let want_sources = config::env_usize("SCRAPE_MIN_SOURCES")
        .unwrap_or(profile.scrape_min_sources)
        .min(deduped_count.max(1));
    let want_chars = config::env_usize("SCRAPE_MIN_CHARS").unwrap_or(profile.scrape_min_chars);
    let stage_deadline = config::env_usize("SCRAPE_STAGE_DEADLINE")
        .map(|s| Duration::from_secs(s as u64))
        .unwrap_or(profile.scrape_stage_deadline);
    let scrape_deadline = tokio::time::Instant::now() + stage_deadline;

    let mut raw_scraped_results: Vec<SourceResult> = Vec::new();
    let mut scraped_chars = 0usize;
    let mut done = 0usize;
    let total_tasks = join_set.len();
    loop {
        if done >= total_tasks {
            break;
        }
        if raw_scraped_results.len() >= want_sources
            && scraped_chars >= want_chars
            && pending_top.is_empty()
        {
            tracing::info!(
                "Scrape early-exit: {} sources / {} chars ({}/{} tasks)",
                raw_scraped_results.len(),
                scraped_chars,
                done,
                total_tasks
            );
            join_set.abort_all();
            break;
        }
        let next = tokio::select! {
            res = join_set.join_next() => res,
            _ = tokio::time::sleep_until(scrape_deadline) => {
                if !join_set.is_empty() {
                    tracing::info!(
                        "Scrape deadline ({:?}): {} sources from {}/{} tasks",
                        stage_deadline, raw_scraped_results.len(), done, total_tasks
                    );
                    join_set.abort_all();
                }
                break;
            }
        };
        match next {
            Some(Ok((idx, Some(r)))) => {
                pending_top.remove(&idx);
                scraped_chars += r.char_count;
                raw_scraped_results.push(r);
                done += 1;
            }
            Some(Ok((idx, None))) => {
                pending_top.remove(&idx);
                done += 1;
            }
            Some(Err(e)) => {
                if !e.is_cancelled() {
                    tracing::debug!("Scrape task error: {}", e);
                }
                done += 1;
            }
            None => break,
        }
    }
    while let Some(j) = join_set.join_next().await {
        if let Ok((_, Some(r))) = j {
            scraped_chars += r.char_count;
            raw_scraped_results.push(r);
        }
    }

    let scrape_ms = stage_start.elapsed().as_millis();
    let scraped_count = raw_scraped_results.len();
    let by_extractor = {
        let mut m: std::collections::BTreeMap<&str, usize> = Default::default();
        for r in &raw_scraped_results {
            *m.entry(r.extractor).or_insert(0) += 1;
        }
        m
    };
    tracing::info!(
        "⏱ Stage [scrape]: {}ms — {} sources from {} URLs | extracted {} chars (avg {}/source) | {:?}",
        scrape_ms,
        scraped_count,
        deduped_count,
        scraped_chars,
        scraped_chars / scraped_count.max(1),
        by_extractor
    );
    emit(
        &progress,
        Progress::Extracted {
            sources: scraped_count,
            chars: scraped_chars,
            ms: scrape_ms,
        },
    )
    .await;

    // ── Rank chunks ──
    // Build the lexical ranking query: preserve the user's raw question while
    // appending any resolved/typo-corrected entities from semantic understanding
    // so both native-language and English/resolved entity passages score fairly.
    let ranking_query = build_ranking_query(&query, &intel_query, &intel);
    let mut rrf_scores = rrf_scores;
    let rank_opts = ranking::RankOptions::from_profile(&profile);

    let stage_start = std::time::Instant::now();
    let mut results = ranking::rank_with_options(
        &ranking_query,
        &raw_scraped_results,
        profile.llm_top_k,
        Some(&rrf_scores),
        Some(&intel),
        &rank_opts,
    );

    // ── Self-Correcting Second-Pass Retrieval ──
    // If the top ranked chunks fail entity or current-year coverage on a non-Fast
    // query, dispatch a targeted corrective query and merge the new sources.
    if !matches!(mode, modes::Mode::Fast)
        && profile.site_filter.is_none()
        && !params.domain_filter.is_strict_active()
        && !evidence_is_sufficient(&results, &intel)
    {
        if let Some(corrective_q) = build_corrective_query(&intel, &query_variations) {
            tracing::info!(
                "🔄 Self-correcting retrieval triggered (insufficient pass-1 coverage): [{}]",
                corrective_q
            );
            let corr_variations = vec![corrective_q.clone()];
            let corr_deadline = Duration::from_millis(2_500);
            let corr_tagged = dispatch_engines(
                &engine_instances,
                &corr_variations,
                &plain_fetcher,
                &stealth_fetcher,
                &stealth_engine_set,
                &stealth_mode,
                &proxy_pool,
                &engine_semaphore,
                config::jitter_min_ms(),
                config::jitter_max_ms(),
                engine_concurrency,
                corr_deadline,
                1,
                Some((18, 2)),
            )
            .await;

            let corr_raw: Vec<RawSearchResult> =
                corr_tagged.into_iter().flat_map(|(_, v)| v).collect();
            if !corr_raw.is_empty() {
                let corr_rrf = ranking::compute_rrf_scores(&corr_raw);
                for (k, v) in &corr_rrf {
                    let entry = rrf_scores.entry(k.clone()).or_insert(0.0);
                    if *v > *entry {
                        *entry = *v;
                    }
                }

                let mut seen_urls: std::collections::HashSet<String> = selection_rank
                    .keys()
                    .map(|u| url_utils::canonical_key(u))
                    .collect();
                for r in &raw_scraped_results {
                    seen_urls.insert(url_utils::canonical_key(&r.url));
                }

                let mut corr_candidates: Vec<Candidate> = Vec::new();
                for item in corr_raw {
                    let key = url_utils::canonical_key(&item.url);
                    if !params.domain_filter.allows_url(&item.url)
                        || !seen_urls.insert(key.clone())
                        || (!params.domain_filter.is_included(&item.url)
                            && url_utils::should_skip(&item.url))
                    {
                        continue;
                    }
                    corr_candidates.push(Candidate {
                        url: item.url,
                        title: item.title,
                        snippet: item.snippet,
                        engines: vec![item.engine.clone()],
                        families: vec![engines::engine_family(&item.engine)],
                        rrf: corr_rrf.get(&key).copied().unwrap_or(0.01),
                        published: item.published,
                        full_text: item.full_text,
                    });
                }

                corr_candidates.sort_by(|a, b| {
                    b.rrf
                        .partial_cmp(&a.rrf)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                corr_candidates.truncate(6);

                if !corr_candidates.is_empty() {
                    let mut corr_join = JoinSet::new();
                    for c in corr_candidates {
                        let sem = semaphore.clone();
                        let client = scrape_client.clone();
                        corr_join.spawn(async move {
                            let engine_name = c.engines.join(",");
                            if let Some(cached) = crate::cache_layer::get_page(&c.url).await {
                                return Some(SourceResult {
                                    url: c.url.clone(),
                                    title: pick_title(&c.url, &c.title, cached.title.clone()),
                                    extracted_text: cached.text.clone(),
                                    char_count: crate::util::char_len(&cached.text),
                                    engine: engine_name,
                                    published: cached.published.or(c.published),
                                    language: cached.language.clone(),
                                    byline: cached.byline.clone(),
                                    site_name: cached.site_name.clone(),
                                    confidence: cached.confidence,
                                    extractor: cached.source,
                                });
                            }
                            let _permit = sem.acquire().await.ok()?;
                            let scraped =
                                scrape_url(&client, &c.url, scrape_timeout, scrape_max_bytes, mode)
                                    .await?;
                            crate::cache_layer::put_page(&c.url, scraped.clone()).await;
                            let title = pick_title(&c.url, &c.title, scraped.title);
                            Some(SourceResult {
                                char_count: crate::util::char_len(&scraped.text),
                                url: c.url,
                                title,
                                extracted_text: scraped.text,
                                engine: engine_name,
                                published: scraped.published.or(c.published),
                                language: scraped.language,
                                byline: scraped.byline,
                                site_name: scraped.site_name,
                                confidence: scraped.confidence,
                                extractor: scraped.source,
                            })
                        });
                    }

                    let corr_scrape_deadline =
                        tokio::time::Instant::now() + Duration::from_millis(3_000);
                    let mut added_sources = 0usize;
                    loop {
                        let next = tokio::select! {
                            res = corr_join.join_next() => res,
                            _ = tokio::time::sleep_until(corr_scrape_deadline) => {
                                corr_join.abort_all();
                                break;
                            }
                        };
                        match next {
                            Some(Ok(Some(src))) => {
                                raw_scraped_results.push(src);
                                added_sources += 1;
                            }
                            Some(_) => {}
                            None => break,
                        }
                    }

                    if added_sources > 0 {
                        tracing::info!(
                            "🔄 Self-correcting pass added {} new sources; re-ranking pool ({} total sources)",
                            added_sources,
                            raw_scraped_results.len()
                        );
                        results = ranking::rank_with_options(
                            &ranking_query,
                            &raw_scraped_results,
                            profile.llm_top_k,
                            Some(&rrf_scores),
                            Some(&intel),
                            &rank_opts,
                        );
                    }
                }
            }
        }
    }

    let rank_ms = stage_start.elapsed().as_millis();
    let scraped_count = raw_scraped_results.len();
    tracing::info!(
        "⏱ Stage [rank]: {}ms — {} chunks from {} sources",
        rank_ms,
        results.len(),
        scraped_count
    );

    // The verbatim answer sentence is known as soon as passages are ranked;
    // streaming clients show it while the LLM is still writing. The opening
    // of the best-ranked pages is considered too: ranked passages favour
    // sentences dense in query words, and the lead is where an article
    // states its answer.
    let leads =
        crate::highlight::leads(&raw_scraped_results, |url| selection_rank.get(url).copied());
    let highlight = crate::highlight::extract(&effective_query, &intel, &results, &leads);
    crate::highlight::dump_case(
        &effective_query,
        &intel_query,
        &results,
        &leads,
        highlight.as_ref(),
    );
    if let Some(h) = &highlight {
        emit(
            &progress,
            Progress::Highlight {
                highlight: h.clone(),
            },
        )
        .await;
    }

    // ── Synthesis ──
    let session_id = match temp_db {
        Some(db) => Some(db.create_session(&query).await),
        None => None,
    };
    if let (Some(db), Some(sid)) = (temp_db, session_id.as_deref()) {
        db.update_status(sid, "scraping_done", "").await;
        db.update_sources(sid, scraped_count).await;
    }

    let stage_start = std::time::Instant::now();
    let llm_result = match params.llm {
        Some(cfg) if !results.is_empty() => {
            emit(
                &progress,
                Progress::Synthesizing {
                    synthesis: format!("{:?}", profile.synthesis),
                },
            )
            .await;
            synthesis::synthesize(
                &query,
                cfg,
                &results,
                &intel,
                &profile,
                &memory_text,
                temp_db,
                session_id.as_deref(),
            )
            .await
        }
        Some(_) => llm::LlmExecutionResult {
            llm_error: Some("llm_skipped: no scraped content available".to_string()),
            ..Default::default()
        },
        None => llm::LlmExecutionResult {
            llm_error: llm_config_error,
            ..Default::default()
        },
    };
    let llm_ms = stage_start.elapsed().as_millis();

    if let (Some(db), Some(sid)) = (temp_db, session_id.as_deref()) {
        db.wipe_session(sid).await;
    }

    let elapsed = start.elapsed().as_secs_f64();
    tracing::info!(
        "⏱ Pipeline TOTAL: {:.0}ms — engines={}ms scrape={}ms rank={}ms rrf={}ms llm={}ms | \
         raw={} selected={} scraped={} chunks={} mode={}",
        elapsed * 1000.0,
        engine_dispatch_ms,
        scrape_ms,
        rank_ms,
        rrf_ms,
        llm_ms,
        total_raw,
        deduped_count,
        scraped_count,
        results.len(),
        profile.tier
    );

    let sources_processed = results.len();

    SearchResponse {
        query,
        search_query,
        memory_used,
        conversation_id: None,
        turn_id: None,
        mode: mode.as_str().to_string(),
        tier: profile.tier.to_string(),
        domain: domain.as_str().to_string(),
        sub_queries: query_variations,
        sources_found: total_raw,
        sources_processed,
        sources_extracted: scraped_count,
        chars_extracted: scraped_chars,
        results,
        search_results,
        discovered_domains,
        total_discovered_domains,
        copilot_query: copilot_out,
        llm_answer: llm_result.llm_answer,
        llm_error: llm_result.llm_error,
        citation_map: llm_result.citation_map,
        citation_checks: llm_result.citation_checks,
        highlight,
        elapsed_seconds: elapsed,
        engine_stats: EngineStats {
            engines_queried: engine_list,
            total_raw_results: total_raw,
            deduplicated_urls: deduped_count,
        },
    }
}

// =============================================================================
// Engine Dispatch Helper — v6.1.0: JoinSet with early-cancel
//
// Uses JoinSet instead of join_all so results stream as they complete.
// Cancels remaining engines once we have enough good sources OR the
// deadline expires. This controls p99 latency, which fixed concurrency
// can't.
//
// Deadline: lite 2.5s, research 8s, specialized 5s, default 6s
// =============================================================================

#[allow(clippy::too_many_arguments)]
async fn dispatch_engines(
    engine_instances: &[std::sync::Arc<dyn engines::SearchEngine>],
    query_variations: &[String],
    plain_fetcher: &Fetcher,
    stealth_fetcher: &Fetcher,
    stealth_engine_set: &std::collections::HashSet<String>,
    stealth_mode: &str,
    proxy_pool: &ProxyPoolManager,
    engine_semaphore: &Arc<Semaphore>,
    jitter_min: u64,
    jitter_max: u64,
    engine_concurrency: usize,
    deadline: Duration,
    page_budget: usize,
    early_exit: Option<(usize, usize)>,
) -> Vec<(usize, Vec<RawSearchResult>)> {
    let mut join_set = JoinSet::new();
    let mut dispatch_index: u64 = 0;

    for (variation_idx, variation) in query_variations.iter().enumerate() {
        for engine in engine_instances {
            // v8.0: clone the Arc instead of re-running get_engines() inside the
            // task. That call re-entered generic::spec_for(), which used
            // Box::leak to fabricate &'static str templates for Google
            // variants — an unbounded leak, once per alias per variation per
            // search, for the process lifetime.
            let engine = std::sync::Arc::clone(engine);
            let ename = engine.name().to_string();

            // ── Pick the fetcher. THIS is the line v7.0 was missing. ──
            // The stealth client existed, was constructed per search, and was
            // passed in as `_stealth_client` — unreachable behind an
            // underscore, because the trait only took &reqwest::Client.
            let use_stealth = match stealth_mode {
                "off" => false,
                "on" => true,
                // "auto": the engine declares its own need, with the
                // STEALTH_ENGINES env list as an override.
                _ => engine.needs_stealth() || stealth_engine_set.contains(&ename),
            };
            let fetcher = if use_stealth {
                stealth_fetcher.clone()
            } else {
                plain_fetcher.clone()
            };

            let query_variant = variation.clone();
            let jitter = config::random_jitter_ms(jitter_min, jitter_max);
            let proxy_hint = proxy_pool.next_proxy();
            let pool = proxy_pool.clone();
            let sem = engine_semaphore.clone();
            let extra_spread = (dispatch_index % engine_concurrency.max(1) as u64) * 15;
            dispatch_index += 1;

            join_set.spawn(async move {
                // ── SERP cache (P2-3) ──
                // Checked BEFORE the jitter sleep and the semaphore, so a warm
                // query skips engine HTTP entirely. Without this the dispatch
                // stage always consumed its full deadline waiting on slow
                // engines, which made a repeated query exactly as slow as a
                // cold one even with a warm page cache.
                // Keyed by page budget too: a 1-page Fast result must not be
                // served to a 3-page Deep search for the same query.
                if let Some(cached) =
                    crate::cache_layer::get_serp(&ename, &query_variant, page_budget).await
                {
                    tracing::debug!("SERP cache hit: {} ({} results)", ename, cached.len());
                    return (variation_idx, cached);
                }

                // ── Sleep BEFORE acquiring the permit (v8.0 P0-9) ──
                // v7.0 acquired the semaphore permit and *then* slept for
                // jitter + spread (up to ~785ms). Each task therefore occupied
                // a concurrency slot doing nothing. At ENGINE_CONCURRENCY=20
                // with 175 tasks that is ~7s of queued sleeping against a 6s
                // deadline, so most engines were aborted before they ever
                // issued a request. The measured run completed "33/35 engines"
                // while only 4 produced data.
                let delay = jitter.saturating_add(extra_spread);
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }

                let _permit = match sem.acquire().await {
                    Ok(p) => p,
                    Err(_) => return (variation_idx, Vec::new()),
                };

                // Per-engine proxy still needs its own client; keep it rare.
                let fetcher = match proxy_hint.as_deref() {
                    Some(proxy) => {
                        tracing::debug!("Engine {} via proxy {}", ename, proxy);
                        if fetcher.is_stealth() {
                            Fetcher::Stealth(config::build_stealth_client(Some(proxy)))
                        } else {
                            Fetcher::Plain(fetcher::build_plain_client(Some(proxy)))
                        }
                    }
                    None => fetcher,
                };

                let mut results =
                    engines::with_page_budget(page_budget, engine.search(&fetcher, &query_variant))
                        .await;

                // A 200 page of filler that ignores the query is a block, not
                // data: drop it (and never cache it). Mixed pages lose only
                // their filler.
                match engines::health::screen_serp(&ename, &query_variant, &mut results) {
                    engines::health::SerpVerdict::Clean => {}
                    engines::health::SerpVerdict::Trimmed(n) => {
                        tracing::info!("{}: dropped {} off-topic results", ename, n);
                    }
                    engines::health::SerpVerdict::Blocked => {
                        tracing::warn!(
                            "{}: results unrelated to the query — soft block, discarded",
                            ename
                        );
                        engines::health::record_block(&ename, 0);
                    }
                }

                if let Some(proxy) = proxy_hint {
                    if results.is_empty() {
                        pool.mark_proxy_failure(&proxy);
                    } else {
                        pool.mark_proxy_success(&proxy);
                    }
                }

                // Empty results are deliberately NOT cached — an empty result
                // is usually a block or selector rot, and caching it would
                // persist the failure for the whole TTL.
                crate::cache_layer::put_serp(&ename, &query_variant, page_budget, results.clone())
                    .await;

                (variation_idx, results)
            });
        }
    }

    // ── Collect with a deadline-based early cancel ──
    let deadline_instant = tokio::time::Instant::now() + deadline;
    let mut all: Vec<(usize, Vec<RawSearchResult>)> = Vec::new();
    let mut tasks_completed = 0usize;
    let total_tasks = join_set.len();

    // Early-exit thresholds. The deadline should be a CEILING, not a floor:
    // in the measured run it was consumed in full on every query because four
    // engines always fail and the collector waited for them, so a fully warm
    // cache still cost 6,002 ms here.
    // Per-mode (see ModeProfile::engine_early_exit); an explicitly-set env
    // var still overrides for operators.
    let (min_results_to_exit, min_families_to_exit) = match (
        config::env_usize("EARLY_EXIT_MIN_RESULTS"),
        config::env_usize("EARLY_EXIT_MIN_FAMILIES"),
    ) {
        (Some(r), f) => (r, f.unwrap_or(3)),
        (None, _) => early_exit.unwrap_or((usize::MAX, usize::MAX)),
    };

    // Soft deadline: past 60% of the window, accept one family fewer. The
    // hard requirement is right while engines are still arriving, but when the
    // rest are hung or rate-limited (measured: Brave 429 + a stalled Marginalia
    // left Fast waiting the full 2.5 s for a third family that never came),
    // holding out only adds latency.
    let soft_instant = tokio::time::Instant::now() + deadline.mul_f64(0.6);
    let mut soft_passed = false;

    let sufficient =
        |all: &Vec<(usize, Vec<RawSearchResult>)>, relaxed: bool| -> Option<(usize, usize)> {
            let flat_len: usize = all.iter().map(|(_, v)| v.len()).sum();
            // Past the soft deadline, two thirds of the target count is enough.
            let need_results = if relaxed {
                min_results_to_exit.saturating_mul(2) / 3
            } else {
                min_results_to_exit
            };
            if flat_len < need_results {
                return None;
            }
            let fams: std::collections::HashSet<&str> = all
                .iter()
                .flat_map(|(_, v)| v.iter())
                .map(|r| engines::engine_family(&r.engine))
                .collect();
            let need = if relaxed {
                min_families_to_exit.saturating_sub(1).max(2)
            } else {
                min_families_to_exit
            };
            (fams.len() >= need).then_some((flat_len, fams.len()))
        };

    loop {
        if tasks_completed >= total_tasks {
            break;
        }
        // Stop as soon as coverage is sufficient for this mode.
        if let Some((n, f)) = sufficient(&all, soft_passed) {
            tracing::info!(
                "Engine early-exit{}: {} results from {} independent families \
                 ({}/{} tasks done) — aborting {} slower task(s)",
                if soft_passed { " (soft deadline)" } else { "" },
                n,
                f,
                tasks_completed,
                total_tasks,
                join_set.len()
            );
            join_set.abort_all();
            break;
        }

        let early_exit_enabled = min_results_to_exit != usize::MAX;
        let result = tokio::select! {
            res = join_set.join_next() => res,
            _ = tokio::time::sleep_until(soft_instant), if early_exit_enabled && !soft_passed => {
                soft_passed = true;
                continue;
            }
            _ = tokio::time::sleep_until(deadline_instant) => {
                let remaining = join_set.len();
                if remaining > 0 {
                    tracing::info!(
                        "Engine deadline ({:?}) reached: {}/{} tasks done, aborting {} (have {} batches)",
                        deadline, tasks_completed, total_tasks, remaining, all.len()
                    );
                    join_set.abort_all();
                }
                break;
            }
        };

        match result {
            Some(Ok(tagged)) => {
                all.push(tagged);
                tasks_completed += 1;
            }
            Some(Err(err)) => {
                if !err.is_cancelled() {
                    tracing::debug!("Engine task join error: {}", err);
                }
                tasks_completed += 1;
            }
            None => {
                tracing::debug!(
                    "Engine collect: set drained at {}/{} tasks",
                    tasks_completed,
                    total_tasks
                );
                break;
            }
        }
    }

    // Drain anything that finished between the abort and now.
    while let Some(result) = join_set.join_next().await {
        if let Ok(tagged) = result {
            all.push(tagged);
        }
    }

    // v8.0: count DISTINCT engines that produced results, not tasks. v7.0
    // logged "engines completed" for engines x variations, which made a
    // single-engine run look like 33 engines succeeding.
    let flat: Vec<&RawSearchResult> = all.iter().flat_map(|(_, v)| v.iter()).collect();
    let producing: std::collections::HashSet<&str> =
        flat.iter().map(|r| r.engine.as_str()).collect();
    let families: std::collections::HashSet<&str> = flat
        .iter()
        .map(|r| engines::engine_family(&r.engine))
        .collect();
    tracing::info!(
        "Engine dispatch: {}/{} tasks done | {} engines produced results | {} independent families | {} raw results",
        tasks_completed, total_tasks, producing.len(), families.len(), flat.len()
    );
    all
}

// =============================================================================
// Scrape & Single-URL Extraction Pipeline — with SSRF guard & retry on 403/429/503
// =============================================================================

/// Shared lazily-initialized scrape client (SSRF-guarded, pooled across calls).
static SCRAPE_CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(build_scrape_client);

/// Access the shared SSRF-guarded scrape client.
pub fn shared_scrape_client() -> &'static reqwest::Client {
    &SCRAPE_CLIENT
}

/// Fetch and extract readable content + metadata from a single public HTTP(S) URL.
///
/// Enforces SearchWala's full SSRF policy (`scrape_url_allowed`,
/// `PublicOnlyResolver`, per-hop redirect checks), `old.reddit.com` rewrite,
/// YouTube fast path, charset-aware decoding, bot-wall screening, Readability +
/// conditional Trafilatura + JSON-LD + Deep Reader + optional AMP fallback, and
/// the 6-hour in-memory page cache.
pub async fn fetch_and_extract_url(
    url: &str,
    opts: &FetchExtractOptions,
) -> Result<ExtractedPage, ExtractError> {
    fetch_and_extract_with_client(shared_scrape_client(), url, opts, true, false).await
}

async fn scrape_url(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
    max_bytes: usize,
    mode: modes::Mode,
) -> Option<crate::extractor::Extracted> {
    let opts = FetchExtractOptions {
        timeout,
        max_bytes,
        allow_amp_fallback: mode != modes::Mode::Fast,
        // `execute_search_full` handles page cache lookup/insert before acquiring
        // the scrape semaphore so cache hits never consume a scrape slot.
        use_cache: false,
        allow_plain_text: false,
    };
    fetch_and_extract_with_client(client, url, &opts, true, mode == modes::Mode::YouTube)
        .await
        .ok()
        .map(|p| p.extracted)
}

async fn fetch_and_extract_with_client(
    client: &reqwest::Client,
    raw_url: &str,
    opts: &FetchExtractOptions,
    enforce_ssrf: bool,
    youtube_mode: bool,
) -> Result<ExtractedPage, ExtractError> {
    let start = std::time::Instant::now();
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return Err(ExtractError::InvalidUrl("URL cannot be empty".into()));
    }
    let parsed_raw = url::Url::parse(trimmed)
        .map_err(|e| ExtractError::InvalidUrl(format!("failed to parse URL: {e}")))?;

    // Check scheme & SSRF before rewriting or checking cache.
    if !matches!(parsed_raw.scheme(), "http" | "https") {
        return Err(ExtractError::BlockedBySsrf(format!(
            "scheme {:?} is not allowed",
            parsed_raw.scheme()
        )));
    }

    // New Reddit is a JavaScript shell; old.reddit serves real HTML. Applied in
    // every mode, since Reddit threads surface in ordinary searches too.
    let target_url = reddit_to_old(trimmed);
    let parsed_target = url::Url::parse(&target_url)
        .map_err(|e| ExtractError::InvalidUrl(format!("invalid rewritten URL: {e}")))?;
    if enforce_ssrf && !crate::ssrf::scrape_url_allowed(&parsed_target) {
        tracing::warn!("Refusing to scrape non-public URL: {}", target_url);
        return Err(ExtractError::BlockedBySsrf(format!(
            "non-public host rejected: {}",
            parsed_target.host_str().unwrap_or("<unknown>")
        )));
    }

    if opts.use_cache {
        if let Some(cached) = crate::cache_layer::get_page(&target_url).await {
            let extracted = (*cached).clone();
            let char_count = crate::util::char_len(&extracted.text);
            return Ok(ExtractedPage {
                url: trimmed.to_string(),
                final_url: target_url,
                extracted,
                char_count,
                cached: true,
                elapsed_ms: start.elapsed().as_millis() as u64,
            });
        }
    }

    let attempt =
        fetch_and_extract_attempt(client, &target_url, opts, enforce_ssrf, youtube_mode).await;
    let (final_url, extracted) = match attempt {
        Ok(ok) => ok,
        Err(ExtractError::AntiBotBlocked { .. }) => {
            // ── Retry once with fresh browser profile after jitter ──
            let delay = config::random_jitter_ms(200, 500);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            tracing::debug!("Retry after block: {}", target_url);
            fetch_and_extract_attempt(client, &target_url, opts, enforce_ssrf, youtube_mode).await?
        }
        Err(err) => return Err(err),
    };

    if opts.use_cache {
        crate::cache_layer::put_page(&target_url, extracted.clone()).await;
    }

    let char_count = crate::util::char_len(&extracted.text);
    Ok(ExtractedPage {
        url: trimmed.to_string(),
        final_url,
        extracted,
        char_count,
        cached: false,
        elapsed_ms: start.elapsed().as_millis() as u64,
    })
}

async fn fetch_and_extract_attempt(
    client: &reqwest::Client,
    target_url: &str,
    opts: &FetchExtractOptions,
    enforce_ssrf: bool,
    youtube_mode: bool,
) -> Result<(String, crate::extractor::Extracted), ExtractError> {
    // Accept-Encoding is NOT set by hand: reqwest advertises exactly what it
    // can decode (the Phase-1 zstd fix), and a manual header would override it.
    let mut req = client.get(target_url).header(
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
    );

    req = config::apply_browser_headers(req, target_url);

    if !opts.timeout.is_zero() {
        req = req.timeout(opts.timeout);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("Fetch failed {}: {}", target_url, e);
            if e.is_timeout() {
                return Err(ExtractError::Timeout);
            }
            let msg = e.to_string();
            if msg.contains("no public address") {
                return Err(ExtractError::BlockedBySsrf(
                    "host resolved to a non-public IP address".into(),
                ));
            }
            return Err(ExtractError::NetworkError(msg));
        }
    };

    let status = resp.status().as_u16();
    // ── Detect anti-bot blocks and retry ──
    if status == 403 || status == 429 || status == 503 {
        tracing::debug!("Blocked (HTTP {}) at {}", status, target_url);
        return Err(ExtractError::AntiBotBlocked { status });
    }

    // Redirect stopped by `scrape_redirect_policy` when targeting a non-public host.
    if resp.status().is_redirection() {
        if let Some(loc) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        {
            if let Ok(resolved) = resp.url().join(loc) {
                if !crate::ssrf::scrape_url_allowed(&resolved) {
                    return Err(ExtractError::BlockedBySsrf(format!(
                        "redirect target rejected: {}",
                        resolved.host_str().unwrap_or("<unknown>")
                    )));
                }
            }
        }
        return Err(ExtractError::HttpStatus { status });
    }

    if !resp.status().is_success() {
        return Err(ExtractError::HttpStatus { status });
    }

    let final_url = resp.url().to_string();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|ct| ct.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let is_plain_text = content_type.contains("text/plain")
        || content_type.contains("text/markdown")
        || content_type.contains("text/x-markdown");
    let is_html = content_type.is_empty()
        || content_type.contains("text/html")
        || content_type.contains("application/xhtml");

    if !is_html && !(opts.allow_plain_text && is_plain_text) {
        return Err(ExtractError::UnsupportedContentType(content_type));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ExtractError::NetworkError(e.to_string()))?;

    // Charset-aware decode (v8.0). `from_utf8_lossy` mangled every
    // windows-1252 / ISO-8859-1 / GBK / Shift_JIS page into replacement chars.
    let capped_bytes = &bytes[..bytes.len().min(opts.max_bytes)];
    let body = crate::fetcher::decode_body(capped_bytes);

    if !is_html && is_plain_text {
        let trimmed = body.trim().to_string();
        if trimmed.is_empty() {
            return Err(ExtractError::UnusableContent { chars: 0 });
        }
        let title = plain_text_title(&trimmed, &final_url);
        return Ok((
            final_url,
            crate::extractor::Extracted {
                title,
                text: trimmed,
                confidence: 0.95,
                source: "plain-text",
                ..Default::default()
            },
        ));
    }

    // Bot walls served with HTTP 200 (Reddit's "Welcome to Reddit" CAPTCHA,
    // Cloudflare interstitials). v8.0 only screened SERPs, so these pages were
    // extracted and handed to the LLM as sources.
    if crate::engines::health::looks_like_blocked_page(&body) {
        tracing::debug!("Blocked/interstitial page at {}", target_url);
        return Err(ExtractError::AntiBotBlocked { status: 200 });
    }

    // ── Single-pass DOM extraction: title + text in one parse ──
    // v5.0 fix: eliminates the double Html::parse_document() bottleneck
    if youtube_mode || target_url.contains("youtube.com/watch") {
        // YouTube fast path: try JSON-LD first (no DOM parse needed)
        if let Some(desc) = extract_youtube_json_ld(&body) {
            return Ok((
                final_url,
                crate::extractor::Extracted {
                    title: extractor::extract_title_from_html(&body),
                    text: desc,
                    confidence: 0.6,
                    source: "youtube",
                    ..Default::default()
                },
            ));
        }
        // Fallback: single DOM parse for both meta desc + title
        let doc = Html::parse_document(&body);
        let title = extractor::extract_title_from_doc_pub(&doc);
        if let Some(desc) = extractor::extract_youtube_meta(&doc) {
            return Ok((
                final_url,
                crate::extractor::Extracted {
                    title,
                    text: desc,
                    confidence: 0.6,
                    source: "youtube",
                    ..Default::default()
                },
            ));
        }
    }

    // Normal pages: Readability + conditional Trafilatura + JSON-LD + Deep Reader.
    // Extraction is CPU-bound; run on Tokio's blocking pool so it cannot stall
    // network I/O. AMP fallback runs when canonical extraction is thin.
    let amp = if opts.allow_amp_fallback {
        crate::deep_reader::amp_url(&body, &final_url)
    } else {
        None
    };
    let page_url = final_url.clone();
    let extracted = tokio::task::spawn_blocking(move || extractor::extract(&body, &page_url))
        .await
        .map_err(|e| ExtractError::NetworkError(format!("extraction task failed: {e}")))?;

    if !extractor::is_usable(&extracted) {
        if let Some(amp_url) = amp {
            if let Some(mut e) = fetch_amp_fallback(client, &amp_url, opts, enforce_ssrf).await {
                tracing::debug!(
                    "AMP fallback read {} chars for {}",
                    e.text.len(),
                    target_url
                );
                e.source = "amp";
                return Ok((amp_url, e));
            }
        }
        let chars = crate::util::char_len(&extracted.text);
        return Err(ExtractError::UnusableContent { chars });
    }
    Ok((final_url, extracted))
}

/// Infer a readable title for a plain-text or Markdown document.
fn plain_text_title(text: &str, url: &str) -> String {
    if let Some(first) = text.lines().map(str::trim).find(|l| !l.is_empty()) {
        if let Some(heading) = first.strip_prefix("# ") {
            let h = heading.trim();
            if !h.is_empty() {
                return crate::util::truncate_chars(h, 160).to_string();
            }
        }
    }
    if let Ok(u) = url::Url::parse(url) {
        if let Some(seg) = u
            .path_segments()
            .and_then(|mut s| s.next_back())
            .filter(|s| !s.is_empty())
        {
            return seg.to_string();
        }
        if let Some(host) = u.host_str() {
            return host.to_string();
        }
    }
    String::new()
}

/// Plain fetch + extract for a secondary URL (the AMP variant). No retries.
async fn fetch_amp_fallback(
    client: &reqwest::Client,
    url: &str,
    opts: &FetchExtractOptions,
    enforce_ssrf: bool,
) -> Option<crate::extractor::Extracted> {
    if enforce_ssrf && !url::Url::parse(url).is_ok_and(|u| crate::ssrf::scrape_url_allowed(&u)) {
        return None;
    }
    let mut req = client
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8");
    req = config::apply_browser_headers(req, url);
    if !opts.timeout.is_zero() {
        req = req.timeout(opts.timeout);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    let html = crate::fetcher::decode_body(&bytes[..bytes.len().min(opts.max_bytes)]);
    if crate::engines::health::looks_like_blocked_page(&html) {
        return None;
    }
    let page_url = url.to_string();
    let e = tokio::task::spawn_blocking(move || extractor::extract(&html, &page_url))
        .await
        .ok()?;
    extractor::is_usable(&e).then_some(e)
}

/// Rewrite any reddit.com URL to old.reddit.com.
///
/// v8.0 did `.replace("www.reddit.com", "old.reddit.com").replace("reddit.com",
/// "old.reddit.com")` — the second replace also matched inside the first's
/// output, turning www.reddit.com into `old.old.reddit.com`. Reddit mode never
/// fetched a valid thread URL.
pub fn reddit_to_old(url: &str) -> String {
    let Ok(mut u) = url::Url::parse(url) else {
        return url.to_string();
    };
    let host = u.host_str().unwrap_or_default().to_lowercase();
    let is_reddit = host == "reddit.com" || host.ends_with(".reddit.com");
    if !is_reddit || host == "old.reddit.com" {
        return url.to_string();
    }
    // Media/CDN subdomains are not thread pages.
    if host.starts_with("i.") || host.starts_with("v.") || host.starts_with("preview.") {
        return url.to_string();
    }
    if u.set_host(Some("old.reddit.com")).is_err() {
        return url.to_string();
    }
    u.to_string()
}

/// Extract YouTube shortDescription from JSON-LD without any DOM parsing.
fn extract_youtube_json_ld(html: &str) -> Option<String> {
    // Try JSON-LD shortDescription (zero DOM parse — pure string scan)
    let marker = "\"shortDescription\":\"";
    let start = html.find(marker)? + marker.len();
    let rest = &html[start..];

    let mut end = 0;
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == '"' {
            end = i;
            break;
        }
    }

    if end == 0 {
        return None;
    }

    let raw = &rest[..end];
    let cleaned = raw
        .replace("\\n", " ")
        .replace("\\\"", "\"")
        .replace("\\u0026", "&")
        .replace("\\/", "/")
        .trim()
        .to_string();

    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

// =============================================================================
// HTTP Client Builders
// =============================================================================

/// Build a properly configured scrape client.
fn build_scrape_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(crate::ssrf::scrape_redirect_policy(5))
        .dns_resolver(std::sync::Arc::new(crate::ssrf::PublicOnlyResolver))
        .pool_max_idle_per_host(12)
        .user_agent(config::random_user_agent())
        .tcp_nodelay(true)
        .cookie_store(true)
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .zstd(true)
        .build()
        .expect("Failed to build scrape HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reddit_rewrite_never_doubles_the_prefix() {
        // v8.0 produced https://old.old.reddit.com/... for this input.
        assert_eq!(
            reddit_to_old("https://www.reddit.com/r/rust/comments/abc/title/"),
            "https://old.reddit.com/r/rust/comments/abc/title/"
        );
        assert_eq!(
            reddit_to_old("https://reddit.com/r/x"),
            "https://old.reddit.com/r/x"
        );
        assert_eq!(
            reddit_to_old("https://m.reddit.com/r/x"),
            "https://old.reddit.com/r/x"
        );
        assert_eq!(
            reddit_to_old("https://old.reddit.com/r/x"),
            "https://old.reddit.com/r/x"
        );
    }

    #[test]
    fn reddit_rewrite_leaves_other_hosts_and_media_alone() {
        assert_eq!(
            reddit_to_old("https://notreddit.com/r/x"),
            "https://notreddit.com/r/x"
        );
        assert_eq!(
            reddit_to_old("https://i.reddit.com/x.png"),
            "https://i.reddit.com/x.png"
        );
        assert_eq!(
            reddit_to_old("https://example.com/reddit.com/x"),
            "https://example.com/reddit.com/x"
        );
        assert_eq!(reddit_to_old("not a url"), "not a url");
    }
}

#[cfg(test)]
mod title_tests {
    use super::{pick_title, wikipedia_title};

    #[test]
    fn keeps_engine_title_when_extracted_title_is_unrelated() {
        assert_eq!(
            pick_title(
                "https://x.test/a",
                "Bain-marie",
                "type of heated bath".into()
            ),
            "Bain-marie"
        );
        assert_eq!(
            pick_title("https://x.test/p", "Paris - Travel guide", "Paris".into()),
            "Paris",
            "a related extracted title (site suffix stripped) is preferred"
        );
        assert_eq!(
            pick_title("https://x.test/e", "", "Extracted".into()),
            "Extracted"
        );
        assert_eq!(
            pick_title("https://x.test/e", "Engine", "".into()),
            "Engine"
        );
        // Shared stopwords are not agreement.
        assert_eq!(
            pick_title(
                "https://x.test/s",
                "The Legend of Tennessee Moltisanti",
                "episode of The Sopranos (S1 E8)".into()
            ),
            "The Legend of Tennessee Moltisanti"
        );
    }

    #[test]
    fn wikipedia_titles_come_from_the_url() {
        assert_eq!(
            pick_title(
                "https://en.wikipedia.org/wiki/Invention_of_the_telephone",
                "",
                "aspect of history relating to telephones".into()
            ),
            "Invention of the telephone"
        );
        assert_eq!(
            wikipedia_title("https://hi.wikipedia.org/wiki/%E0%A4%AD%E0%A4%BE%E0%A4%B0%E0%A4%A4")
                .as_deref(),
            Some("भारत")
        );
        assert_eq!(
            wikipedia_title("https://en.wikipedia.org/wiki/Special:Search"),
            None
        );
        assert_eq!(wikipedia_title("https://example.org/wiki/X"), None);
    }
}

/// Combines the user's raw query with any resolved or typo-corrected entities
/// from Semantic Query Understanding so BM25 scores both raw terms and resolved
/// canonical entities (including cross-lingual technical entities).
fn build_ranking_query(
    raw_query: &str,
    effective_query: &str,
    intel: &query_intel::QueryIntelligence,
) -> String {
    let mut combined = raw_query.trim().to_string();
    let lower_combined = combined.to_lowercase();

    // Append resolved entity tokens that aren't already verbatim in raw_query
    for ent in &intel.key_entities {
        for tok in ent.split_whitespace() {
            let clean = tok.trim_matches(|c: char| !crate::util::is_word_char(c));
            if clean.chars().count() >= 3 && !lower_combined.contains(&clean.to_lowercase()) {
                combined.push(' ');
                combined.push_str(clean);
            }
        }
    }

    // If follow-up or typo resolution produced additional content words in effective_query, include them
    if !effective_query.eq_ignore_ascii_case(raw_query) {
        let current_lower = combined.to_lowercase();
        for tok in effective_query.split_whitespace() {
            let clean = tok.trim_matches(|c: char| !crate::util::is_word_char(c));
            if clean.chars().count() >= 3
                && !clean.chars().all(|c| c.is_ascii_digit())
                && !current_lower.contains(&clean.to_lowercase())
            {
                combined.push(' ');
                combined.push_str(clean);
            }
        }
    }

    combined
}

/// Evaluates whether the top-5 ranked chunks have sufficient entity coverage
/// and (when `intel.is_time_sensitive` is true) current-year evidence.
pub(crate) fn evidence_is_sufficient(
    results: &[SourceResult],
    intel: &query_intel::QueryIntelligence,
) -> bool {
    if results.is_empty() {
        return false;
    }

    let top_chunks: Vec<&SourceResult> = results.iter().take(5).collect();

    // 1. Check entity coverage across the top-5 chunks
    if !intel.key_entities.is_empty() {
        let mut matched_entities = 0usize;
        for ent in &intel.key_entities {
            let tokens: Vec<String> = ent
                .split_whitespace()
                .map(|t| {
                    t.trim_matches(|c: char| !crate::util::is_word_char(c))
                        .to_lowercase()
                })
                .filter(|t| t.chars().count() >= 3)
                .collect();
            if tokens.is_empty() {
                matched_entities += 1;
                continue;
            }
            let found = top_chunks.iter().any(|chunk| {
                let hay = format!(
                    "{} {} {} {}",
                    chunk.title.to_lowercase(),
                    chunk.url.to_lowercase(),
                    chunk.site_name.as_deref().unwrap_or("").to_lowercase(),
                    chunk.extracted_text.to_lowercase()
                );
                tokens.iter().any(|tok| hay.contains(tok.as_str()))
            });
            if found {
                matched_entities += 1;
            }
        }

        if matched_entities * 2 < intel.key_entities.len() {
            return false;
        }
    }

    // 2. For time-sensitive queries, verify at least one top-5 chunk carries
    // current-year evidence (resolved dynamically via chrono::Utc::now().year()).
    if intel.is_time_sensitive {
        let current_year = chrono::Utc::now().year();
        let current_year_str = current_year.to_string();
        let has_current_year_evidence = top_chunks.iter().any(|chunk| {
            let date_is_current = chunk
                .published
                .map(|d| d.year() >= current_year)
                .unwrap_or(false);
            date_is_current
                || chunk.extracted_text.contains(&current_year_str)
                || chunk.title.contains(&current_year_str)
        });
        if !has_current_year_evidence {
            return false;
        }
    }

    true
}

/// Builds the second-pass corrective query from the semantic plan (or extracted
/// entities + dynamic current year) if it hasn't already been dispatched.
fn build_corrective_query(
    intel: &query_intel::QueryIntelligence,
    already_searched: &[String],
) -> Option<String> {
    let candidate = if let Some(ref cq) = intel.corrective_query {
        cq.trim().to_string()
    } else if !intel.key_entities.is_empty() {
        let base = intel.key_entities.join(" ");
        if intel.is_time_sensitive {
            format!("{} {}", base, chrono::Utc::now().year())
        } else {
            base
        }
    } else {
        return None;
    };

    if candidate.chars().count() < 3 {
        return None;
    }
    let already_ran = already_searched
        .iter()
        .any(|q| q.eq_ignore_ascii_case(&candidate));
    (!already_ran).then_some(candidate)
}

/// Adaptive Pre-Scrape Title, Subdomain & Snippet Triage (zero hardcoded domain or year lists).
/// Evaluates a SERP candidate's title, URL/subdomain structure, and snippet against
/// target entities, query terms, and dynamic temporal context BEFORE scraping full HTML.
fn pre_scrape_triage_score(
    c: &Candidate,
    raw_query: &str,
    query_terms: &[String],
    intel: &query_intel::QueryIntelligence,
    mode: modes::Mode,
    domain: modes::Domain,
) -> f64 {
    let mut s = c.rrf * modes::source_boost(&c.url, mode, domain);
    let (host, path) = modes::parse_host_and_path(&c.url);

    // 1. Subdomain & Host Entity Alignment
    let (matched_ent, _) =
        crate::llm::url_matches_query_entities(&c.url, None, &intel.key_entities);
    if matched_ent > 0 {
        s *= 1.50 + (0.15 * (matched_ent.min(2) as f64));
        if modes::is_structural_docs_url(&host, &path)
            || path.contains("/changelog")
            || path.contains("/releases")
            || path.contains("/release-notes")
            || path.contains("/announcements")
            || path.starts_with("/blog")
            || path.starts_with("/news")
        {
            s *= 1.20;
        }
    }

    // 2. Title & Snippet Entity + Specific Sub-Topic Qualifier Overlap
    let title_lower = c.title.to_lowercase();
    let snippet_lower = c.snippet.to_lowercase();
    let url_lower = c.url.to_lowercase();

    let mut entity_token_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut entity_hits_in_meta = 0usize;
    for ent in &intel.key_entities {
        let mut hit = false;
        for t in ent
            .split_whitespace()
            .map(|t| {
                t.trim_matches(|ch: char| !crate::util::is_word_char(ch))
                    .to_lowercase()
            })
            .filter(|t| t.chars().count() >= 3)
        {
            if title_lower.contains(&t) || snippet_lower.contains(&t) || url_lower.contains(&t) {
                hit = true;
            }
            entity_token_set.insert(t);
        }
        if hit {
            entity_hits_in_meta += 1;
        }
    }

    let mut term_hits_title = 0usize;
    let mut term_hits_snippet = 0usize;
    let mut subtopic_term_count = 0usize;
    let mut subtopic_hits = 0usize;

    for term in query_terms {
        if term.chars().count() < 3 {
            continue;
        }
        let in_title_or_url =
            title_lower.contains(term.as_str()) || url_lower.contains(term.as_str());
        let in_snippet = snippet_lower.contains(term.as_str());
        if in_title_or_url {
            term_hits_title += 1;
        } else if in_snippet {
            term_hits_snippet += 1;
        }
        if !entity_token_set.contains(term.as_str()) {
            subtopic_term_count += 1;
            if in_title_or_url || in_snippet {
                subtopic_hits += 1;
            }
        }
    }

    if entity_hits_in_meta > 0 && term_hits_title > 0 {
        s *= 1.25;
    } else if !query_terms.is_empty()
        && matched_ent == 0
        && entity_hits_in_meta == 0
        && term_hits_title == 0
        && term_hits_snippet == 0
    {
        // Off-topic SERP noise with zero entity or query term overlap in title/URL/snippet
        s *= 0.30;
    }

    // Sub-topic specificity boost: when a query asks about specific aspects/clauses
    // of an entity (e.g., "administration" and "land grants" in "Aihole inscription"),
    // strongly boost candidates whose title/snippet explicitly covers those sub-topics,
    // and demote generic encyclopedia summaries that only name the entity without sub-topics.
    let is_encyc = crate::llm::is_encyclopedia_url(&c.url, None);
    if subtopic_term_count > 0 && (entity_hits_in_meta > 0 || matched_ent > 0) {
        if subtopic_hits > 0 {
            s *= 1.0 + 0.25 * (subtopic_hits.min(3) as f64);
        } else if is_encyc && mode != modes::Mode::Study {
            s *= 0.68;
        }
    }

    // Boost scholarly, institutional, or primary documentation sources on research/historical queries
    if (entity_hits_in_meta > 0 || term_hits_title > 0)
        && (modes::is_structural_scholarly_url(&host, &path) || modes::is_institutional_host(&host))
    {
        s *= 1.28;
    }

    // 3. Dynamic Temporal Alignment in Title, URL & Snippet (resolved via chrono::Utc::now().year())
    if intel.is_time_sensitive && !matches!(mode, modes::Mode::Study | modes::Mode::Academic) {
        let current_year = chrono::Utc::now().year();
        let cur_year_str = current_year.to_string();
        let meta_text = format!("{} {} {}", title_lower, snippet_lower, path);

        let mentions_current_year = meta_text.contains(&cur_year_str)
            || c.published
                .map(|d| d.year() >= current_year)
                .unwrap_or(false);

        if mentions_current_year {
            s *= 1.25;
        } else {
            // Penalize candidates whose title/URL/date explicitly anchors to an unrequested past year
            let raw_lower = raw_query.to_lowercase();
            let has_stale_year = ((current_year - 10)..current_year).any(|y| {
                let ys = y.to_string();
                !raw_lower.contains(&ys)
                    && (title_lower.contains(&ys)
                        || path.contains(&ys)
                        || c.published.map(|d| d.year() == y).unwrap_or(false))
            });
            if has_stale_year {
                s *= 0.55;
            }
        }
    }

    // 4. Encyclopedia Pre-Scrape Demotion on Live / Product Queries
    if crate::llm::is_encyclopedia_url(&c.url, None)
        && mode != modes::Mode::Study
        && (intel.is_time_sensitive
            || matches!(
                intel.semantic_category,
                query_intel::SemanticCategory::LiveNews | query_intel::SemanticCategory::ProductOrg
            ))
    {
        s *= 0.65;
    }

    if c.full_text.is_some() && matches!(mode, modes::Mode::Academic | modes::Mode::Study) {
        s *= 1.5;
    }

    s
}

#[cfg(test)]
mod stage1_sufficiency_tests {
    use super::*;
    use chrono::{Datelike, TimeZone, Utc};

    #[test]
    fn evidence_sufficiency_checks_entity_and_dynamic_current_year_coverage() {
        let current_year = chrono::Utc::now().year();
        let past_year = current_year - 2;

        let mut intel = crate::query_intel::analyze_query("latest Google Antigravity release");
        intel.is_time_sensitive = true;
        intel.key_entities = vec!["Google Antigravity".to_string()];

        let stale_unrelated = vec![SourceResult {
            url: "https://en.wikipedia.org/wiki/Google_Play".to_string(),
            title: "Google Play".to_string(),
            extracted_text: format!(
                "Google Play is a digital distribution service updated in {past_year}."
            ),
            char_count: 72,
            engine: "wikipedia".to_string(),
            published: Utc.with_ymd_and_hms(past_year, 5, 10, 0, 0, 0).single(),
            language: Some("en".to_string()),
            byline: None,
            site_name: Some("Wikipedia".to_string()),
            confidence: 0.9,
            extractor: "dom_smoothie",
        }];
        assert!(
            !evidence_is_sufficient(&stale_unrelated, &intel),
            "stale source should fail sufficiency on time-sensitive query"
        );

        let fresh_matching = vec![SourceResult {
            url: "https://antigravity.google/changelog".to_string(),
            title: "Google Antigravity Changelog".to_string(),
            extracted_text: format!(
                "Google Antigravity release notes and platform updates ({current_year})."
            ),
            char_count: 75,
            engine: "brave".to_string(),
            published: Utc.with_ymd_and_hms(current_year, 3, 15, 0, 0, 0).single(),
            language: Some("en".to_string()),
            byline: None,
            site_name: Some("Google Antigravity".to_string()),
            confidence: 0.95,
            extractor: "dom_smoothie",
        }];
        assert!(
            evidence_is_sufficient(&fresh_matching, &intel),
            "fresh entity-matching source should pass sufficiency"
        );
    }

    #[test]
    fn pre_scrape_triage_prioritizes_official_current_candidates_over_stale_or_offtopic() {
        let current_year = chrono::Utc::now().year();
        let stale_year = current_year - 2;

        let raw_q = "Google Antigravity latest release features";
        let mut intel = crate::query_intel::analyze_query(raw_q);
        intel.is_time_sensitive = true;
        intel.semantic_category = crate::query_intel::SemanticCategory::ProductOrg;
        intel.key_entities = vec!["Google Antigravity".to_string()];
        let terms = crate::ranking::query_terms(raw_q);

        let official_cand = Candidate {
            url: "https://antigravity.google/changelog".to_string(),
            title: format!("Google Antigravity Changelog ({current_year})"),
            snippet: "Latest release features, agentic IDE tools, and workspace updates."
                .to_string(),
            engines: vec!["brave".to_string()],
            families: vec!["brave"],
            rrf: 0.02,
            published: None,
            full_text: None,
        };
        let stale_blog_cand = Candidate {
            url: format!("https://some-random-techblog.test/review-{stale_year}"),
            title: format!("Early review from {stale_year}"),
            snippet: "An old post about Google Antigravity.".to_string(),
            engines: vec!["brave".to_string()],
            families: vec!["brave"],
            rrf: 0.02,
            published: None,
            full_text: None,
        };
        let offtopic_wiki_cand = Candidate {
            url: "https://en.wikipedia.org/wiki/Google_Doodle".to_string(),
            title: "Google Doodle - Wikipedia".to_string(),
            snippet: "A special temporary alteration of the logo on homepages.".to_string(),
            engines: vec!["wikipedia".to_string()],
            families: vec!["wikipedia"],
            rrf: 0.02,
            published: None,
            full_text: None,
        };

        let s_official = pre_scrape_triage_score(
            &official_cand,
            raw_q,
            &terms,
            &intel,
            modes::Mode::Auto,
            modes::Domain::General,
        );
        let s_stale = pre_scrape_triage_score(
            &stale_blog_cand,
            raw_q,
            &terms,
            &intel,
            modes::Mode::Auto,
            modes::Domain::General,
        );
        let s_wiki = pre_scrape_triage_score(
            &offtopic_wiki_cand,
            raw_q,
            &terms,
            &intel,
            modes::Mode::Auto,
            modes::Domain::General,
        );

        assert!(
            s_official > s_stale * 3.0,
            "official ({s_official}) must beat stale ({s_stale})"
        );
        assert!(
            s_official > s_wiki * 3.0,
            "official ({s_official}) must beat off-topic wiki ({s_wiki})"
        );
    }
}

#[cfg(test)]
mod mcp_foundation_tests {
    use super::*;
    use axum::{
        http::{header, StatusCode},
        response::IntoResponse,
        routing::get,
        Router,
    };

    fn long_article_html(title: &str, tag: &str) -> String {
        let body: String = (0..6)
            .map(|i| {
                format!(
                    "<p>{tag} paragraph {i}: SearchWala extracts structured prose cleanly from \
                     real-world HTML documents without relying on third-party SDKs or cloud APIs.</p>"
                )
            })
            .collect();
        format!(
            "<!DOCTYPE html><html lang=\"en\"><head><title>{title}</title></head>\
             <body><article><h1>{title}</h1>{body}</article></body></html>"
        )
    }

    async fn spawn_mock_site() -> String {
        let app = Router::new()
            .route(
                "/article",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                        long_article_html("Rust MCP Architecture", "ARTICLE"),
                    )
                }),
            )
            .route(
                "/llms.txt",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                        "# SearchWala Docs\n\nSearchWala exposes consensus search and deep extraction over MCP.",
                    )
                }),
            )
            .route(
                "/file.pdf",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "application/pdf")],
                        b"%PDF-1.7 binary payload".as_slice(),
                    )
                }),
            )
            .route(
                "/blocked-403",
                get(|| async { (StatusCode::FORBIDDEN, "Forbidden") }),
            )
            .route(
                "/captcha-200",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/html")],
                        "<html><head><title>Just a moment...</title></head><body>Checking your browser before accessing</body></html>",
                    )
                }),
            )
            .route(
                "/missing",
                get(|| async { (StatusCode::NOT_FOUND, "Not found") }),
            )
            .route(
                "/thin-with-amp",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/html")],
                        "<html><head><title>Thin</title><link rel=\"amphtml\" href=\"/amp/story\"></head><body><p>Short teaser.</p></body></html>",
                    )
                }),
            )
            .route(
                "/amp/story",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/html")],
                        long_article_html("AMP Full Story", "AMPBODY"),
                    )
                }),
            )
            .route(
                "/thin-only",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/html")],
                        "<html><head><title>Tiny</title></head><body><p>Hi</p></body></html>",
                    )
                }),
            )
            .route(
                "/redirect-imds",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, "http://169.254.169.254/latest/meta-data/")],
                    )
                        .into_response()
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local mock listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{}", addr)
    }

    fn local_test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(crate::ssrf::scrape_redirect_policy(5))
            .build()
            .expect("test reqwest client")
    }

    #[tokio::test]
    async fn search_serp_only_handles_empty_query_offline() {
        let resp = search_serp_only(
            "   ",
            Some("fast"),
            Some("tech"),
            Some(10),
            &DomainFilter::default(),
            None,
        )
        .await;
        assert!(resp.query.is_empty());
        assert_eq!(resp.mode, "fast");
        assert_eq!(resp.domain, "tech");
        assert!(resp.results.is_empty());
        assert!(resp.discovered_domains.is_empty());
        assert_eq!(resp.total_discovered_domains, 0);
        assert_eq!(resp.engine_stats.total_raw_results, 0);
    }

    #[test]
    fn fuse_and_select_candidates_dedups_and_preserves_abstracts() {
        let raw = vec![
            RawSearchResult {
                url: "https://arxiv.org/abs/2601.12345".into(),
                title: "Consensus Meta-Search".into(),
                snippet: "Short snippet".into(),
                engine: "arxiv".into(),
                rank_position: 1,
                published: None,
                full_text: Some(
                    "This paper evaluates cross-family Reciprocal Rank Fusion across independent \
                     web and scholarly indexes with measurable nDCG improvements."
                        .into(),
                ),
            },
            RawSearchResult {
                url: "https://www.arxiv.org/abs/2601.12345?utm_source=test".into(),
                title: "Consensus Meta-Search".into(),
                snippet: "Longer consensus snippet from Brave".into(),
                engine: "brave".into(),
                rank_position: 2,
                published: None,
                full_text: None,
            },
            RawSearchResult {
                url: "https://facebook.com/skipped-post".into(),
                title: "Should be skipped".into(),
                snippet: "Skipped domain".into(),
                engine: "bing".into(),
                rank_position: 1,
                published: None,
                full_text: None,
            },
        ];

        let query = "consensus meta-search";
        let intel = query_intel::analyze_query(query);
        let profile = modes::resolve(modes::Mode::Academic, modes::Domain::Science, &intel, None);
        let sel = fuse_and_select_candidates(
            &raw,
            None,
            query,
            query,
            &intel,
            &profile,
            10,
            &DomainFilter::default(),
        );
        let ranked = sel.candidates;
        let discovered = sel.discovered_domains;
        let total_domains = sel.total_discovered_domains;
        assert_eq!(
            ranked.len(),
            1,
            "www/utm variant must collapse and facebook skipped"
        );
        assert_eq!(sel.deduplicated_urls, 1);
        assert_eq!(ranked[0].families.len(), 2, "arxiv + brave are 2 families");
        assert_eq!(total_domains, 1);
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].domain, "arxiv.org");
        assert_eq!(discovered[0].max_families, 2);

        let hits = candidates_to_serp_hits(&ranked, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rank, 1);
        assert_eq!(hits[0].found_by_families, 2);
        assert_eq!(hits[0].snippet, "Longer consensus snippet from Brave");
        assert!(
            hits[0]
                .abstract_text
                .as_deref()
                .is_some_and(|a| a.contains("Reciprocal Rank Fusion")),
            "engine-supplied abstract must be preserved"
        );
    }

    #[test]
    fn domain_filter_supports_soft_priority_strict_restriction_exclusion_and_discovered_rollup() {
        let raw = vec![
            RawSearchResult {
                url: "https://en.wikipedia.org/wiki/Rust_(programming_language)".into(),
                title: "Rust programming language".into(),
                snippet: "Rust async and memory safety overview.".into(),
                engine: "wikipedia".into(),
                rank_position: 1,
                published: None,
                full_text: None,
            },
            RawSearchResult {
                url: "https://en.wikipedia.org/wiki/Rust_(programming_language)".into(),
                title: "Rust programming language".into(),
                snippet: "Rust async overview from Bing.".into(),
                engine: "bing".into(),
                rank_position: 1,
                published: None,
                full_text: None,
            },
            RawSearchResult {
                url: "https://docs.rs/tokio/latest/tokio/macro.select.html".into(),
                title: "tokio::select cancellation safety".into(),
                snippet: "Detailed docs on tokio select cancellation safety.".into(),
                engine: "brave".into(),
                rank_position: 4,
                published: None,
                full_text: None,
            },
            RawSearchResult {
                url: "https://docs.rs/tokio/latest/tokio/task/index.html".into(),
                title: "tokio::task".into(),
                snippet: "Tokio async tasks documentation.".into(),
                engine: "bing".into(),
                rank_position: 6,
                published: None,
                full_text: None,
            },
            RawSearchResult {
                url: "https://noisy-blog.example/rust-select".into(),
                title: "Noisy blog post".into(),
                snippet: "Low quality scraped tutorial.".into(),
                engine: "bing".into(),
                rank_position: 2,
                published: None,
                full_text: None,
            },
            RawSearchResult {
                url: "https://www.amazon.com/dp/B0RUSTBOOK".into(),
                title: "Programming Rust Book".into(),
                snippet: "Rust async programming book.".into(),
                engine: "bing".into(),
                rank_position: 5,
                published: None,
                full_text: None,
            },
        ];

        let query = "rust async tokio select cancellation safety";
        let intel = query_intel::analyze_query(query);
        let profile = modes::resolve(modes::Mode::Auto, modes::Domain::Tech, &intel, None);

        // 1. Soft mode (strict=false, default): docs.rs + amazon.com prioritized & bypass SKIP_DOMAINS;
        // noisy-blog.example excluded; organic wikipedia result preserved.
        let soft_filter = DomainFilter {
            include: vec!["docs.rs".into(), "amazon.com".into()],
            exclude: vec!["noisy-blog.example".into()],
            strict: false,
        };
        let soft_sel = fuse_and_select_candidates(
            &raw,
            None,
            query,
            query,
            &intel,
            &profile,
            10,
            &soft_filter,
        );
        let soft_ranked = soft_sel.candidates;
        let soft_discovered = soft_sel.discovered_domains;
        let soft_total = soft_sel.total_discovered_domains;
        assert!(
            soft_ranked
                .iter()
                .all(|c| !c.url.contains("noisy-blog.example")),
            "excluded domain must never survive triage"
        );
        assert!(
            soft_discovered
                .iter()
                .all(|d| d.domain != "noisy-blog.example"),
            "excluded domain must not appear in discovered_domains"
        );
        assert!(
            soft_ranked[0].url.contains("docs.rs"),
            "included domain must be prioritized at top: {:?}",
            soft_ranked.iter().map(|c| &c.url).collect::<Vec<_>>()
        );
        assert!(
            soft_ranked.iter().any(|c| c.url.contains("wikipedia.org")),
            "soft mode (strict=false) must keep SearchWala's organic consensus results intact"
        );
        assert!(
            soft_ranked.iter().any(|c| c.url.contains("amazon.com")),
            "explicit include_domains must bypass SKIP_DOMAINS"
        );
        assert_eq!(soft_total, 3, "docs.rs + amazon.com + en.wikipedia.org");
        let docs_entry = soft_discovered
            .iter()
            .find(|d| d.domain == "docs.rs")
            .unwrap();
        assert_eq!(docs_entry.result_count, 2);

        // 2. Strict mode (strict=true): ONLY docs.rs returned in BOTH candidates
        // AND discovered_domains (external/spam domains must never leak).
        let strict_filter = DomainFilter {
            include: vec!["docs.rs".into()],
            exclude: vec!["noisy-blog.example".into()],
            strict: true,
        };
        let strict_sel = fuse_and_select_candidates(
            &raw,
            None,
            query,
            query,
            &intel,
            &profile,
            10,
            &strict_filter,
        );
        let strict_ranked = strict_sel.candidates;
        let strict_discovered = strict_sel.discovered_domains;
        assert_eq!(strict_ranked.len(), 2);
        assert_eq!(strict_sel.deduplicated_urls, 2);
        assert!(strict_ranked.iter().all(|c| c.url.contains("docs.rs")));
        assert!(
            strict_discovered.iter().all(|d| d.domain == "docs.rs"),
            "strict mode must not leak non-included domains into discovered_domains: {strict_discovered:?}"
        );

        // 3. Balanced soft priority: when many included hits compete with organic
        // consensus results under strict=false, included domains take top priority
        // slots (round-robin across included domains, capped at half the budget)
        // while keeping SearchWala's organic consensus answers intact.
        let mut crowded_raw = Vec::new();
        for i in 1..=6 {
            crowded_raw.push(RawSearchResult {
                url: format!("https://docs.rs/tokio/item-{i}"),
                title: format!("Tokio docs item {i}"),
                snippet: "Rust tokio select cancellation safety".into(),
                engine: "brave".into(),
                rank_position: i,
                published: None,
                full_text: None,
            });
            crowded_raw.push(RawSearchResult {
                url: format!("https://tokio.rs/tutorial/item-{i}"),
                title: format!("Tokio tutorial item {i}"),
                snippet: "Rust tokio select cancellation safety".into(),
                engine: "bing".into(),
                rank_position: i,
                published: None,
                full_text: None,
            });
            crowded_raw.push(RawSearchResult {
                url: format!("https://organic{i}.example/rust-select"),
                title: format!("Organic rust guide {i}"),
                snippet: "Rust tokio select cancellation safety".into(),
                engine: "duckduckgo".into(),
                rank_position: i,
                published: None,
                full_text: None,
            });
        }
        let balanced_filter = DomainFilter {
            include: vec!["docs.rs".into(), "tokio.rs".into()],
            exclude: vec![],
            strict: false,
        };
        let balanced_sel = fuse_and_select_candidates(
            &crowded_raw,
            None,
            query,
            query,
            &intel,
            &profile,
            10,
            &balanced_filter,
        );
        let balanced = balanced_sel.candidates;
        assert_eq!(
            balanced_sel.deduplicated_urls, 18,
            "deduplicated_urls must reflect pre-truncation pool (18), not truncated cap (10)"
        );
        assert_eq!(balanced.len(), 10);
        let inc_cnt = balanced
            .iter()
            .filter(|c| balanced_filter.is_included(&c.url))
            .count();
        let org_cnt = balanced.len() - inc_cnt;
        assert_eq!(
            inc_cnt, 5,
            "priority slots capped at half of max_urls (5/10)"
        );
        assert_eq!(org_cnt, 5, "organic consensus answers keep 5/10 slots");
        assert!(balanced[..5]
            .iter()
            .all(|c| balanced_filter.is_included(&c.url)));
        assert!(balanced[..5].iter().any(|c| c.url.contains("docs.rs")));
        assert!(balanced[..5].iter().any(|c| c.url.contains("tokio.rs")));

        // 4. Targeted site: query generation caps cleanly and avoids double site:.
        let tq = build_targeted_domain_queries(query, &soft_filter, false);
        assert_eq!(
            tq,
            vec![
                format!("{query} site:docs.rs"),
                format!("{query} site:amazon.com")
            ]
        );
        assert!(build_targeted_domain_queries(query, &soft_filter, true).is_empty());
        assert!(build_targeted_domain_queries("foo site:bar.com", &soft_filter, false).is_empty());
    }

    #[test]
    fn apply_recency_hint_skips_evergreen_modes_and_existing_years() {
        let mut intel = query_intel::analyze_query("latest rust release");
        intel.is_time_sensitive = true;
        let cur_year = chrono::Utc::now().year().to_string();

        let auto_q = apply_recency_hint("latest rust release", &intel, modes::Mode::Auto);
        assert!(auto_q.contains(&cur_year));

        let study_q = apply_recency_hint("latest rust release", &intel, modes::Mode::Study);
        assert_eq!(study_q, "latest rust release");

        let explicit = format!("rust release {cur_year}");
        assert_eq!(
            apply_recency_hint(&explicit, &intel, modes::Mode::Auto),
            explicit
        );
    }

    #[tokio::test]
    async fn fetch_and_extract_url_blocks_invalid_and_ssrf_targets() {
        let opts = FetchExtractOptions::default();

        assert!(matches!(
            fetch_and_extract_url("", &opts).await,
            Err(ExtractError::InvalidUrl(_))
        ));
        assert!(matches!(
            fetch_and_extract_url("not a url", &opts).await,
            Err(ExtractError::InvalidUrl(_))
        ));

        for blocked in [
            "http://127.0.0.1:8000/api/chats",
            "http://localhost:8000/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:8000/",
            "http://10.0.0.1/",
            "file:///etc/passwd",
            "ftp://example.com/file",
        ] {
            let res = fetch_and_extract_url(blocked, &opts).await;
            assert!(
                matches!(res, Err(ExtractError::BlockedBySsrf(_))),
                "expected BlockedBySsrf for {blocked}, got {res:?}"
            );
        }
    }

    #[tokio::test]
    async fn fetch_and_extract_url_serves_from_page_cache_without_network() {
        let url = "https://cached-offline-check.example/docs/guide";
        crate::cache_layer::put_page(
            url,
            crate::extractor::Extracted {
                title: "Cached Guide".into(),
                text: "Cached article body served purely from Moka without network I/O.".into(),
                confidence: 0.92,
                source: "readability",
                ..Default::default()
            },
        )
        .await;

        let page = fetch_and_extract_url(url, &FetchExtractOptions::default())
            .await
            .expect("should hit cache");
        assert!(page.cached);
        assert_eq!(page.extracted.title, "Cached Guide");
        assert!(page.extracted.text.contains("purely from Moka"));
    }

    #[tokio::test]
    async fn fetch_and_extract_local_mock_server_end_to_end() {
        let base = spawn_mock_site().await;

        // 1. Production entrypoint MUST block the local 127.0.0.1 mock server via SSRF guard.
        let prod_res =
            fetch_and_extract_url(&format!("{base}/article"), &FetchExtractOptions::default())
                .await;
        assert!(
            matches!(prod_res, Err(ExtractError::BlockedBySsrf(_))),
            "production fetch_and_extract_url must reject loopback: {prod_res:?}"
        );

        let client = local_test_client();
        let opts = FetchExtractOptions {
            use_cache: true,
            ..Default::default()
        };

        // 2. HTML article extraction + cache miss then cache hit.
        let art_url = format!("{base}/article");
        let page = fetch_and_extract_with_client(&client, &art_url, &opts, false, false)
            .await
            .expect("article extraction");
        assert!(!page.cached);
        assert_eq!(page.extracted.title, "Rust MCP Architecture");
        assert!(page.extracted.text.contains("ARTICLE paragraph 5"));
        assert!(page.char_count > 300);

        let cached = fetch_and_extract_with_client(&client, &art_url, &opts, false, false)
            .await
            .expect("cached article");
        assert!(cached.cached);

        let bypassed = fetch_and_extract_with_client(
            &client,
            &art_url,
            &FetchExtractOptions {
                use_cache: false,
                ..opts.clone()
            },
            false,
            false,
        )
        .await
        .expect("bypassed cache");
        assert!(!bypassed.cached);

        // 3. Markdown/plain-text extraction with H1 title inference.
        let md = fetch_and_extract_with_client(
            &client,
            &format!("{base}/llms.txt"),
            &opts,
            false,
            false,
        )
        .await
        .expect("markdown extraction");
        assert_eq!(md.extracted.source, "plain-text");
        assert_eq!(md.extracted.title, "SearchWala Docs");
        assert!(md.extracted.text.contains("consensus search"));

        // 4. Binary content-type rejected cleanly.
        let pdf = fetch_and_extract_with_client(
            &client,
            &format!("{base}/file.pdf"),
            &opts,
            false,
            false,
        )
        .await;
        assert!(
            matches!(pdf, Err(ExtractError::UnsupportedContentType(ref ct)) if ct.contains("application/pdf")),
            "expected UnsupportedContentType, got {pdf:?}"
        );

        // 5. Anti-bot 403 and HTTP 200 Cloudflare interstitial.
        let f403 = fetch_and_extract_with_client(
            &client,
            &format!("{base}/blocked-403"),
            &opts,
            false,
            false,
        )
        .await;
        assert_eq!(
            f403.unwrap_err(),
            ExtractError::AntiBotBlocked { status: 403 }
        );

        let cap = fetch_and_extract_with_client(
            &client,
            &format!("{base}/captcha-200"),
            &opts,
            false,
            false,
        )
        .await;
        assert_eq!(
            cap.unwrap_err(),
            ExtractError::AntiBotBlocked { status: 200 }
        );

        // 6. HTTP 404 status error.
        let miss =
            fetch_and_extract_with_client(&client, &format!("{base}/missing"), &opts, false, false)
                .await;
        assert_eq!(miss.unwrap_err(), ExtractError::HttpStatus { status: 404 });

        // 7. Thin canonical HTML + AMP fallback recovers full story.
        let amp = fetch_and_extract_with_client(
            &client,
            &format!("{base}/thin-with-amp"),
            &opts,
            false,
            false,
        )
        .await
        .expect("AMP fallback should recover full body");
        assert_eq!(amp.extracted.source, "amp");
        assert!(amp.extracted.text.contains("AMPBODY paragraph 5"));

        // 8. Thin page with no AMP returns UnusableContent.
        let thin = fetch_and_extract_with_client(
            &client,
            &format!("{base}/thin-only"),
            &opts,
            false,
            false,
        )
        .await;
        assert!(
            matches!(thin, Err(ExtractError::UnusableContent { .. })),
            "expected UnusableContent, got {thin:?}"
        );

        // 9. Redirect to cloud metadata IP is caught and reported as BlockedBySsrf.
        let redir = fetch_and_extract_with_client(
            &client,
            &format!("{base}/redirect-imds"),
            &opts,
            false,
            false,
        )
        .await;
        assert!(
            matches!(redir, Err(ExtractError::BlockedBySsrf(_))),
            "redirect to IMDS must return BlockedBySsrf, got {redir:?}"
        );
    }
}
