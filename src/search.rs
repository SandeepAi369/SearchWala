// ============================================================================
// SearchWala v5.0.1 - Search Orchestrator
// Iterative deep research: multi-batch LLM synthesis
// TempDb session tracking + HistoryDb persistence
// ============================================================================

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use scraper::Html;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config;
use crate::engines;
use crate::extractor;
use crate::llm;
use crate::models::*;
use crate::proxy_pool::ProxyPoolManager;
use crate::ranking;
use crate::url_utils;

pub async fn execute_search(
    query: &str,
    max_results: Option<usize>,
    focus_mode: Option<String>,
    llm_config: Option<LlmConfig>,
    enable_copilot: Option<bool>,
    temp_db: Option<&crate::cache::TempDb>,
    history_db: Option<&crate::cache::HistoryDb>,
) -> SearchResponse {
    let start = std::time::Instant::now();

    let normalized_focus = focus_mode
        .as_deref()
        .map(|m| m.trim().to_lowercase())
        .filter(|m| !m.is_empty());

    let is_lite_mode = matches!(normalized_focus.as_deref(), Some("lite"));
    let is_research_mode = matches!(normalized_focus.as_deref(), Some("research"));

    // Specialized mode detection: "specialized_tech", "specialized_science", etc.
    let is_specialized = normalized_focus.as_ref().map_or(false, |m| m.starts_with("specialized"));
    let specialized_domain = if is_specialized {
        normalized_focus.as_ref().and_then(|m| m.strip_prefix("specialized_")).map(|s| s.to_string())
    } else {
        None
    };

    let use_ranked_chunk_path = is_lite_mode || is_specialized;

    let max_urls = if is_lite_mode {
        max_results.unwrap_or_else(config::max_urls).min(50)
    } else if is_specialized {
        max_results.unwrap_or(100).min(150)
    } else {
        max_results.unwrap_or_else(config::max_urls).min(900)
    };

    let mut effective_query = apply_focus_mode(query, normalized_focus.as_deref());
    let mut copilot_out = None;

    if enable_copilot.unwrap_or(false) {
        if let Some(cfg) = &llm_config {
            let rewritten = crate::copilot::rewrite_query(&effective_query, cfg).await;
            tracing::info!("SearchWala-Copilot rewrote query: [{}] -> [{}]", effective_query, rewritten);
            copilot_out = Some(rewritten.clone());
            effective_query = rewritten;
        }
    }

    // ─── Time-enrichment: auto-inject current year for recency ───
    // If the user's query implies they want current/latest info, or doesn't
    // contain a specific year, append the current year to nudge search engines
    // toward fresh results. This is critical for "Who is the CEO of X?" type queries.
    {
        let q_lower = effective_query.to_lowercase();
        let recency_keywords = ["latest", "current", "recent", "today", "new", "now",
            "this year", "right now", "as of", "updated", "newest"];
        let has_recency_hint = recency_keywords.iter().any(|kw| q_lower.contains(kw));
        // Check if query already contains a year (2020-2030)
        let has_year = (2020..=2030).any(|y| q_lower.contains(&y.to_string()));

        if has_recency_hint && !has_year {
            let year = chrono::Utc::now().format("%Y").to_string();
            effective_query = format!("{} {}", effective_query, year);
            tracing::info!("Time-enrichment: appended year → [{}]", effective_query);
        }
    }

    // Single query — no snowballing. User's query is sacred.
    let query_variations = vec![effective_query.clone()];

    let jitter_min = config::jitter_min_ms();
    let jitter_max = config::jitter_max_ms();

    tracing::info!(
        "NEW SEARCH query={} focus_mode={:?} lite_mode={} research_mode={} max_urls={} snowball_variations={}",
        &effective_query[..effective_query.len().min(120)],
        normalized_focus,
        is_lite_mode,
        is_research_mode,
        max_urls,
        query_variations.len()
    );

    // ══════════════════════════════════════════════════════════════════════
    // 2026: SIMULTANEOUS ALL-ENGINE DISPATCH
    // Fire ALL engines (primary + backup) at once — no waiting, no delay.
    // The old 2-phase approach wasted 5+ seconds waiting for primary to fail
    // before dispatching backups. Now everything runs in parallel.
    // ══════════════════════════════════════════════════════════════════════

    let engine_list = if is_specialized {
        let domain = specialized_domain.as_deref().unwrap_or("general");
        engines::specialized_engines(domain)
    } else if is_lite_mode {
        // Lite mode: fire ALL engines simultaneously for maximum coverage
        engines::all_engines()
    } else {
        config::enabled_engines()
    };

    let engine_instances = engines::get_engines(&engine_list);
    let base_search_client = build_search_client(None);
    let proxy_pool = ProxyPoolManager::from_env();
    if proxy_pool.has_proxies() {
        tracing::info!("Proxy pool loaded with {} entries", proxy_pool.len());
    }
    let engine_concurrency = config::engine_concurrency().max(1).min(40);
    let engine_semaphore = Arc::new(Semaphore::new(engine_concurrency));

    let mut all_results: Vec<RawSearchResult> = Vec::new();

    // ── Single-phase: ALL engines fire simultaneously ──
    let results = dispatch_engines(
        &engine_instances, &query_variations, &base_search_client,
        &proxy_pool, &engine_semaphore, jitter_min, jitter_max,
        engine_concurrency,
    ).await;
    all_results.extend(results);

    tracing::info!(
        "All-engine dispatch: {} results from {} engines",
        all_results.len(), engine_list.len()
    );

    let total_raw = all_results.len();

    let raw_urls: Vec<String> = all_results.iter().map(|r| r.url.clone()).collect();
    let unique_urls = url_utils::deduplicate(raw_urls, max_urls, normalized_focus.as_deref());
    let deduped_count = unique_urls.len();

    let mut engine_by_url: HashMap<String, String> = HashMap::new();
    let mut seen = HashSet::new();
    let mut search_results: Vec<SearchHit> = Vec::new();

    for item in &all_results {
        let normalized = url_utils::normalize_url(&item.url).unwrap_or_else(|| item.url.clone());

        engine_by_url
            .entry(normalized.clone())
            .or_insert_with(|| item.engine.clone());

        if seen.insert(normalized) && search_results.len() < max_urls {
            search_results.push(SearchHit {
                url: item.url.clone(),
                title: item.title.clone(),
                snippet: item.snippet.clone(),
                engine: item.engine.clone(),
            });
        }
    }

    let concurrency = config::concurrency().min(40);
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let scrape_client = build_scrape_client();

    // ── Research mode sends ALL sources to iterative LLM ──
    let llm_top_k: usize = if is_research_mode { 200 } else if is_specialized { 50 } else { 25 };

    // ══════════════════════════════════════════════════════════════════════
    // FIX: Create LLM channel AFTER scraping so we can feed data INLINE
    // OLD BUG: LLM task was spawned BEFORE scraping, with a 2.5s timeout
    //          on the first message. Scraping 30 URLs takes 15-60s, so
    //          the LLM would ALWAYS timeout before receiving any data.
    // NEW: We collect all scraped results first, then feed them to the LLM
    //      synchronously — no race condition, no timeout.
    // ══════════════════════════════════════════════════════════════════════

    let mut join_set = JoinSet::new();
    let scrape_timeout_secs = config::scrape_timeout_secs();
    let scrape_max_bytes = config::max_html_bytes();

    for url in unique_urls {
        let sem = semaphore.clone();
        let client = scrape_client.clone();
        let focus = normalized_focus.clone();

        let engine_name = engine_by_url
            .get(&url)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        join_set.spawn(async move {
            let _permit = sem.acquire().await.ok()?;
            match scrape_url(
                &client,
                &url,
                scrape_timeout_secs,
                scrape_max_bytes,
                is_lite_mode,
                focus.as_deref(),
            )
            .await
            {
                Some((title, text)) => {
                    let char_count = text.len();
                    Some(SourceResult {
                        url,
                        title,
                        extracted_text: text,
                        char_count,
                        engine: engine_name,
                    })
                }
                None => None,
            }
        });
    }

    // Collect all scraped results
    let mut raw_scraped_results: Vec<SourceResult> = Vec::new();
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok(Some(result)) => {
                raw_scraped_results.push(result);
            }
            Ok(_) => {}
            Err(err) => tracing::debug!("Scrape task join error: {}", err),
        }
    }

    tracing::info!(
        "Scraping complete: {} sources extracted out of {} deduplicated URLs",
        raw_scraped_results.len(),
        deduped_count
    );

    // ── Rank and prepare LLM input ──
    let scraped_count = raw_scraped_results.len();
    let mut results = if use_ranked_chunk_path {
        ranking::rank_top_chunks(&effective_query, &raw_scraped_results, llm_top_k)
    } else {
        raw_scraped_results
    };

    // ── Create TempDb session for progress tracking ──
    let session_id = if let Some(db) = temp_db {
        Some(db.create_session(query).await)
    } else {
        None
    };

    if let (Some(db), Some(sid)) = (temp_db, session_id.as_deref()) {
        db.update_status(sid, "scraping_done", "").await;
        db.update_sources(sid, scraped_count).await;
    }

    // ── NOW run LLM synthesis ──
    let llm_result = if let Some(cfg) = llm_config {
        let llm_inputs = if use_ranked_chunk_path {
            results.clone()
        } else {
            ranking::rank_top_chunks(&effective_query, &results, llm_top_k)
        };

        if llm_inputs.is_empty() {
            tracing::warn!("LLM skipped: no scraped content to synthesize");
            llm::LlmExecutionResult {
                llm_answer: None,
                llm_error: Some("llm_skipped: no scraped content available".to_string()),
                batches_processed: 0,
            }
        } else if is_research_mode {
            // ── ITERATIVE DEEP RESEARCH: multi-batch 50-source processing ──
            tracing::info!("Starting iterative deep research with {} sources", llm_inputs.len());
            llm::summarize_iterative(
                query,
                cfg,
                &llm_inputs,
                temp_db,
                session_id.as_deref(),
            ).await
        } else {
            // ── LITE MODE: single fast call ──
            tracing::info!("Feeding {} chunks to LLM (lite mode)", llm_inputs.len());
            llm::summarize_direct(query, cfg, &llm_inputs, false).await
        }
    } else {
        llm::LlmExecutionResult::default()
    };

    // ── Wipe TempDb session (auto-clean) ──
    if let (Some(db), Some(sid)) = (temp_db, session_id.as_deref()) {
        db.wipe_session(sid).await;
    }

    if !use_ranked_chunk_path {
        results.sort_by(|a, b| b.char_count.cmp(&a.char_count));
    }

    let sources_processed = if use_ranked_chunk_path {
        results.len()
    } else {
        deduped_count
    };

    let elapsed = start.elapsed().as_secs_f64();
    let focus_str = normalized_focus.as_deref().unwrap_or("lite");

    // ── Save to HistoryDb if enabled ──
    if let Some(hdb) = history_db {
        if hdb.is_enabled() {
            let entry = crate::cache::build_history_entry(
                query,
                focus_str,
                llm_result.llm_answer.as_deref(),
                sources_processed,
                total_raw,
                elapsed,
            );
            hdb.add_entry(entry).await;
        }
    }

    SearchResponse {
        query: query.to_string(),
        sources_found: total_raw,
        sources_processed,
        results,
        search_results,
        copilot_query: copilot_out,
        llm_answer: llm_result.llm_answer,
        llm_error: llm_result.llm_error,
        elapsed_seconds: elapsed,
        engine_stats: EngineStats {
            engines_queried: engine_list,
            total_raw_results: total_raw,
            deduplicated_urls: deduped_count,
        },
    }
}

fn apply_focus_mode(query: &str, focus_mode: Option<&str>) -> String {
    let base = query.trim();
    match focus_mode {
        Some("reddit") => format!("{} site:reddit.com", base),
        Some("youtube") => format!("{} site:youtube.com", base),
        Some("academic") => format!("{} site:edu OR site:gov OR site:nature.com", base),
        Some(m) if m.starts_with("specialized") => base.to_string(), // domain handled by engine selection
        Some("research") | Some("lite") | _ => base.to_string(),
    }
}

// =============================================================================
// Engine Dispatch Helper — used for both primary and backup phases
// =============================================================================

async fn dispatch_engines(
    engine_instances: &[Box<dyn engines::SearchEngine>],
    query_variations: &[String],
    base_client: &reqwest::Client,
    proxy_pool: &ProxyPoolManager,
    engine_semaphore: &Arc<Semaphore>,
    jitter_min: u64,
    jitter_max: u64,
    engine_concurrency: usize,
) -> Vec<RawSearchResult> {
    let mut search_futures = Vec::new();
    let mut dispatch_index: u64 = 0;

    for variation in query_variations {
        for engine in engine_instances {
            let base_client = base_client.clone();
            let query_variant = variation.clone();
            let engine_name = engine.name().to_string();
            let jitter = config::random_jitter_ms(jitter_min, jitter_max);
            let proxy_hint = proxy_pool.next_proxy();
            let pool = proxy_pool.clone();
            let sem = engine_semaphore.clone();
            let extra_spread = (dispatch_index % engine_concurrency as u64) * 15;
            let engine = engine.as_ref();
            dispatch_index += 1;

            search_futures.push(async move {
                let _permit = match sem.acquire().await {
                    Ok(p) => p,
                    Err(_) => return Vec::new(),
                };

                let delay = jitter.saturating_add(extra_spread);
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }

                let client = if let Some(proxy) = proxy_hint.as_deref() {
                    tracing::debug!("Engine {} using proxy hint {}", engine_name, proxy);
                    build_search_client(Some(proxy))
                } else {
                    base_client
                };

                let results = engine.search(&client, &query_variant).await;

                if let Some(proxy) = proxy_hint {
                    if results.is_empty() {
                        pool.mark_proxy_failure(&proxy);
                    } else {
                        pool.mark_proxy_success(&proxy);
                    }
                }

                tracing::info!("Engine [{}] variant [{}]: {} results", engine_name, query_variant, results.len());
                results
            });
        }
    }

    let engine_results = futures::future::join_all(search_futures).await;
    let mut all: Vec<RawSearchResult> = Vec::new();
    for batch in engine_results {
        all.extend(batch);
    }
    all
}

// =============================================================================
// Scrape Pipeline — with retry on 403/429/503
// =============================================================================

async fn scrape_url(
    client: &reqwest::Client,
    url: &str,
    timeout_secs: u64,
    max_bytes: usize,
    _lite_mode: bool,
    focus_mode: Option<&str>,
) -> Option<(String, String)> {
    // First attempt
    match scrape_url_inner(client, url, timeout_secs, max_bytes, focus_mode).await {
        ScrapeResult::Ok(title, text) => Some((title, text)),
        ScrapeResult::Blocked => {
            // ── Retry with fresh browser profile after delay ──
            let delay = config::random_jitter_ms(200, 500);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            tracing::debug!("Retry after block: {}", url);
            match scrape_url_inner(client, url, timeout_secs, max_bytes, focus_mode).await {
                ScrapeResult::Ok(title, text) => Some((title, text)),
                _ => None,
            }
        }
        ScrapeResult::Skip => None,
    }
}

enum ScrapeResult {
    Ok(String, String),
    Blocked,
    Skip,
}

async fn scrape_url_inner(
    client: &reqwest::Client,
    url: &str,
    timeout_secs: u64,
    max_bytes: usize,
    focus_mode: Option<&str>,
) -> ScrapeResult {
    let mut target_url = url.to_string();

    if matches!(focus_mode, Some("reddit")) && target_url.contains("reddit.com/") && !target_url.contains("old.reddit.com") {
        target_url = target_url
            .replace("www.reddit.com", "old.reddit.com")
            .replace("reddit.com", "old.reddit.com");
    }

    let mut req = client
        .get(&target_url)
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8")
        .header("Accept-Encoding", "gzip, deflate, br");

    req = config::apply_browser_headers(req, &target_url);

    if timeout_secs > 0 {
        req = req.timeout(Duration::from_secs(timeout_secs));
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("Fetch failed {}: {}", target_url, e);
            return ScrapeResult::Skip;
        }
    };

    // ── Detect anti-bot blocks and retry ──
    let status = resp.status().as_u16();
    if status == 403 || status == 429 || status == 503 {
        tracing::debug!("Blocked (HTTP {}) at {}", status, target_url);
        return ScrapeResult::Blocked;
    }

    if let Some(ct) = resp.headers().get("content-type") {
        let ct_str = ct.to_str().unwrap_or("");
        if !ct_str.contains("text/html") && !ct_str.contains("application/xhtml") {
            return ScrapeResult::Skip;
        }
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => return ScrapeResult::Skip,
    };

    let html_bytes = &bytes[..bytes.len().min(max_bytes)];
    let html = String::from_utf8_lossy(html_bytes).to_string();

    // ── Single-pass DOM extraction: title + text in one parse ──
    // v5.0 fix: eliminates the double Html::parse_document() bottleneck
    if matches!(focus_mode, Some("youtube")) || target_url.contains("youtube.com/watch") {
        // YouTube fast path: try JSON-LD first (no DOM parse needed)
        if let Some(desc) = extract_youtube_json_ld(&html) {
            let title = extractor::extract_title(&html);
            return ScrapeResult::Ok(title, desc);
        }
        // Fallback: single DOM parse for both meta desc + title
        let doc = Html::parse_document(&html);
        let title = extractor::extract_title_from_doc_pub(&doc);
        if let Some(desc) = extractor::extract_youtube_meta(&doc) {
            return ScrapeResult::Ok(title, desc);
        }
    }

    // Normal pages: single DOM parse for both title + article text
    let (title, text) = extractor::extract_title_and_text(&html);
    if text.len() < config::min_text_length() {
        return ScrapeResult::Skip;
    }

    ScrapeResult::Ok(title, text)
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

fn build_search_client(proxy: Option<&str>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(16))
        .redirect(reqwest::redirect::Policy::limited(10))
        .pool_max_idle_per_host(10)
        .cookie_store(true)
        .user_agent(config::random_user_agent())
        .tcp_nodelay(true)
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .https_only(false);

    if let Some(proxy_url) = proxy {
        match reqwest::Proxy::all(proxy_url) {
            Ok(proxy_cfg) => {
                builder = builder.proxy(proxy_cfg);
            }
            Err(err) => {
                tracing::warn!("Invalid proxy {}: {}", proxy_url, err);
            }
        }
    }

    builder
        .build()
        .expect("Failed to build search HTTP client")
}

/// Build a properly configured scrape client.
fn build_scrape_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(5))
        .pool_max_idle_per_host(12)
        .user_agent(config::random_user_agent())
        .tcp_nodelay(true)
        .cookie_store(true)
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .build()
        .expect("Failed to build scrape HTTP client")
}
