// ============================================================================
// Swift Search Agent v4.0 - Search Orchestrator
// Full pipeline: Engines -> URLs -> Scrape -> Extract -> Optional BYOK LLM
// ============================================================================

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;

use crate::config;
use crate::engines;
use crate::extractor;
use crate::llm;
use crate::models::*;
use crate::url_utils;

/// Execute the full search pipeline.
pub async fn execute_search(
    query: &str,
    max_results: Option<usize>,
    llm_config: Option<LlmConfig>,
) -> SearchResponse {
    let start = std::time::Instant::now();
    let max_urls = max_results.unwrap_or_else(config::max_urls);

    tracing::info!("NEW SEARCH query={}", &query[..query.len().min(80)]);

    // --- Phase 1: Meta-search (all engines concurrently) ---------------------
    let enabled = config::enabled_engines();
    let engine_instances = engines::get_engines(&enabled);
    let client = build_search_client();

    let search_futures: Vec<_> = engine_instances
        .iter()
        .map(|engine| {
            let client = client.clone();
            let query = query.to_string();
            let name = engine.name().to_string();
            async move {
                let results = engine.search(&client, &query).await;
                tracing::info!("Engine [{}]: {} results", name, results.len());
                results
            }
        })
        .collect();

    let engine_results = futures::future::join_all(search_futures).await;

    let mut all_results: Vec<RawSearchResult> = Vec::new();
    for batch in engine_results {
        all_results.extend(batch);
    }

    let total_raw = all_results.len();
    tracing::info!(
        "Meta-search: {} total raw results from {} engines",
        total_raw,
        enabled.len()
    );

    // --- Phase 2: URL deduplication ------------------------------------------
    let raw_urls: Vec<String> = all_results.iter().map(|r| r.url.clone()).collect();
    let unique_urls = url_utils::deduplicate(raw_urls, max_urls);
    let deduped_count = unique_urls.len();

    tracing::info!("Deduplicated: {} -> {} URLs", total_raw, deduped_count);

    // Build URL -> engine map and expose classic raw search list for fallback UI.
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

    // --- Phase 3: Concurrent scrape + extraction with ghost-stream to LLM ----
    let concurrency = config::concurrency();
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

    for url in unique_urls {
        let sem = semaphore.clone();
        let client = scrape_client.clone();
        let timeout_secs = config::scrape_timeout_secs();
        let max_bytes = config::max_html_bytes();

        let engine_name = engine_by_url
            .get(&url)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        join_set.spawn(async move {
            let _permit = sem.acquire().await.ok()?;
            match scrape_url(&client, &url, timeout_secs, max_bytes).await {
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

    let mut results: Vec<SourceResult> = Vec::new();
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok(Some(result)) if result.char_count >= config::min_text_length() => {
                if let Some(tx) = maybe_sender.as_ref() {
                    let _ = tx.send(result.clone()).await;
                }
                results.push(result);
            }
            Ok(_) => {}
            Err(err) => {
                tracing::debug!("Scrape task join error: {}", err);
            }
        }
    }

    // Close sender so LLM task can finish cleanly once channel is drained.
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

    let sources_processed = results.len();
    let elapsed = start.elapsed().as_secs_f64();

    results.sort_by(|a, b| b.char_count.cmp(&a.char_count));

    tracing::info!(
        "Scraped {}/{} URLs in {:.2}s",
        sources_processed,
        deduped_count,
        elapsed
    );

    SearchResponse {
        query: query.to_string(),
        sources_found: deduped_count,
        sources_processed,
        results,
        search_results,
        llm_answer: llm_result.llm_answer,
        llm_error: llm_result.llm_error,
        elapsed_seconds: (elapsed * 100.0).round() / 100.0,
        engine_stats: EngineStats {
            engines_queried: enabled,
            total_raw_results: total_raw,
            deduplicated_urls: deduped_count,
        },
    }
}

/// Scrape a single URL and extract article text.
async fn scrape_url(
    client: &reqwest::Client,
    url: &str,
    timeout_secs: u64,
    max_bytes: usize,
) -> Option<(String, String)> {
    let resp = match client
        .get(url)
        .header("User-Agent", config::random_user_agent())
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Accept-Encoding", "gzip, deflate, br")
        .timeout(Duration::from_secs(timeout_secs))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("Fetch failed {}: {}", url, e);
            return None;
        }
    };

    if let Some(ct) = resp.headers().get("content-type") {
        let ct_str = ct.to_str().unwrap_or("");
        if !ct_str.contains("text/html") && !ct_str.contains("application/xhtml") {
            tracing::debug!("Skipping non-HTML content type: {}", ct_str);
            return None;
        }
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => return None,
    };

    if bytes.len() > max_bytes {
        tracing::debug!("Page too large ({}B > {}B): {}", bytes.len(), max_bytes, url);
    }

    let html_bytes = &bytes[..bytes.len().min(max_bytes)];
    let html = String::from_utf8_lossy(html_bytes).to_string();

    let title = extractor::extract_title(&html);
    let text = extractor::extract_article_text(&html);

    if text.len() < config::min_text_length() {
        return None;
    }

    Some((title, text))
}

/// Build HTTP client for search engine queries.
fn build_search_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(5))
        .pool_max_idle_per_host(5)
        .gzip(true)
        .brotli(true)
        .build()
        .expect("Failed to build search HTTP client")
}

/// Build HTTP client for web scraping (more generous timeouts).
fn build_scrape_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(config::scrape_timeout_secs()))
        .redirect(reqwest::redirect::Policy::limited(5))
        .pool_max_idle_per_host(10)
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .build()
        .expect("Failed to build scrape HTTP client")
}
