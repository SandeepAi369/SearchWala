// ============================================================================
// Swift Search Agent v4.0 - Search Orchestrator
// ============================================================================

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use scraper::{Html, Selector};
use tokio::sync::{mpsc, Semaphore};
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
) -> SearchResponse {
    let start = std::time::Instant::now();

    let normalized_focus = focus_mode
        .as_deref()
        .map(|m| m.trim().to_lowercase())
        .filter(|m| !m.is_empty());

    let is_lite_mode = matches!(normalized_focus.as_deref(), Some("lite"));
    let use_ranked_chunk_path = is_lite_mode;

    let max_urls = if is_lite_mode {
        max_results.unwrap_or_else(config::max_urls).min(35)
    } else {
        max_results.unwrap_or_else(config::max_urls).min(900)
    };

    let mut effective_query = apply_focus_mode(query, normalized_focus.as_deref());
    let mut copilot_out = None;

    if enable_copilot.unwrap_or(false) {
        if let Some(cfg) = &llm_config {
            let rewritten = crate::copilot::rewrite_query(&effective_query, cfg).await;
            tracing::info!("Swift-Copilot rewrote query: [{}] -> [{}]", effective_query, rewritten);
            copilot_out = Some(rewritten.clone());
            effective_query = rewritten;
        }
    }

    let query_variations = if is_lite_mode {
        vec![effective_query.clone()]
    } else {
        engines::generate_query_variations(&effective_query)
    };

    let jitter_min = config::jitter_min_ms();
    let jitter_max = config::jitter_max_ms();

    tracing::info!(
        "NEW SEARCH query={} focus_mode={:?} lite_mode={} max_urls={} snowball_variations={}",
        &effective_query[..effective_query.len().min(120)],
        normalized_focus,
        is_lite_mode,
        max_urls,
        query_variations.len()
    );

    let enabled = config::enabled_engines();
    let engine_instances = engines::get_engines(&enabled);
    let base_search_client = build_search_client(None);
    let proxy_pool = ProxyPoolManager::from_env();
    if proxy_pool.has_proxies() {
        tracing::info!("Proxy pool loaded with {} entries", proxy_pool.len());
    }
    let engine_concurrency = config::engine_concurrency().max(1).min(24);
    let engine_semaphore = Arc::new(Semaphore::new(engine_concurrency));

    let mut search_futures = Vec::new();
    let mut dispatch_index: u64 = 0;

    for variation in &query_variations {
        for engine in &engine_instances {
            let base_client = base_search_client.clone();
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

    let mut all_results: Vec<RawSearchResult> = Vec::new();
    for batch in engine_results {
        all_results.extend(batch);
    }

    let total_raw = all_results.len();
    tracing::info!("Meta-search total raw results={} engines={}", total_raw, enabled.len());

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

    let (maybe_sender, llm_handle) = if let Some(cfg) = llm_config {
        let (tx, rx) = mpsc::channel::<SourceResult>((concurrency.max(2)) * 2);
        let query_for_llm = query.to_string();
        let handle = tokio::spawn(async move { llm::summarize_from_stream(&query_for_llm, cfg, rx).await });
        (Some(tx), Some(handle))
    } else {
        (None, None)
    };

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

    let mut results = if use_ranked_chunk_path {
        ranking::rank_top_chunks(&effective_query, &raw_scraped_results, 25)
    } else {
        raw_scraped_results
    };

    if let Some(tx) = maybe_sender.as_ref() {
        let llm_inputs = if use_ranked_chunk_path {
            results.clone()
        } else {
            ranking::rank_top_chunks(&effective_query, &results, 25)
        };

        for item in llm_inputs {
            let _ = tx.send(item).await;
        }
    }

    drop(maybe_sender);

    let llm_result = if let Some(handle) = llm_handle {
        match handle.await {
            Ok(result) => result,
            Err(err) => llm::LlmExecutionResult {
                llm_answer: None,
                llm_error: Some(format!("llm_task_join_error: {}", err)),
            },
        }
    } else {
        llm::LlmExecutionResult::default()
    };

    if !use_ranked_chunk_path {
        results.sort_by(|a, b| b.char_count.cmp(&a.char_count));
    }

    let sources_processed = if use_ranked_chunk_path {
        results.len()
    } else {
        deduped_count
    };

    SearchResponse {
        query: query.to_string(),
        sources_found: total_raw,
        sources_processed,
        results,
        search_results,
        copilot_query: copilot_out,
        llm_answer: llm_result.llm_answer,
        llm_error: llm_result.llm_error,
        elapsed_seconds: start.elapsed().as_secs_f64(),
        engine_stats: EngineStats {
            engines_queried: enabled,
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
        Some("research") | Some("lite") | _ => base.to_string(),
    }
}

async fn scrape_url(
    client: &reqwest::Client,
    url: &str,
    timeout_secs: u64,
    max_bytes: usize,
    _lite_mode: bool,
    focus_mode: Option<&str>,
) -> Option<(String, String)> {
    let mut target_url = url.to_string();

    if matches!(focus_mode, Some("reddit")) && target_url.contains("reddit.com/") && !target_url.contains("old.reddit.com") {
        target_url = target_url
            .replace("www.reddit.com", "old.reddit.com")
            .replace("reddit.com", "old.reddit.com");
    }

    let mut req = client
        .get(&target_url)
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9")
        .header("Accept-Encoding", "gzip, deflate, br");

    req = config::apply_browser_headers(req, &target_url);

    if timeout_secs > 0 {
        req = req.timeout(Duration::from_secs(timeout_secs));
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("Fetch failed {}: {}", target_url, e);
            return None;
        }
    };

    if let Some(ct) = resp.headers().get("content-type") {
        let ct_str = ct.to_str().unwrap_or("");
        if !ct_str.contains("text/html") && !ct_str.contains("application/xhtml") {
            return None;
        }
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => return None,
    };

    let html_bytes = &bytes[..bytes.len().min(max_bytes)];
    let html = String::from_utf8_lossy(html_bytes).to_string();
    let title = extractor::extract_title(&html);

    if matches!(focus_mode, Some("youtube")) || target_url.contains("youtube.com/watch") {
        if let Some(desc) = extract_youtube_short_description(&html) {
            return Some((title, desc));
        }
    }

    let text = extractor::extract_article_text(&html);
    Some((title, text))
}

fn extract_youtube_short_description(html: &str) -> Option<String> {
    if let Ok(selector) = Selector::parse("meta[name='shortDescription']") {
        let doc = Html::parse_document(html);
        if let Some(meta) = doc.select(&selector).next() {
            if let Some(content) = meta.value().attr("content") {
                let cleaned = content.trim();
                if !cleaned.is_empty() {
                    return Some(cleaned.to_string());
                }
            }
        }
    }

    let marker = "\"shortDescription\":\"";
    let start = html.find(marker)? + marker.len();
    let rest = &html[start..];
    let end = rest.find("\"")?;
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

fn build_scrape_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .pool_max_idle_per_host(12)
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .build()
        .expect("Failed to build scrape HTTP client")
}
