// ══════════════════════════════════════════════════════════════════════════════
// Swift Search Agent v3.0 — Search Orchestrator
// Full pipeline: Engines → URLs → Scrape → Extract → JSON
// ══════════════════════════════════════════════════════════════════════════════

use std::time::Duration;
use tokio::sync::Semaphore;
use std::sync::Arc;

use crate::config;
use crate::engines;
use crate::extractor;
use crate::models::*;
use crate::url_utils;

/// Execute the full search pipeline
pub async fn execute_search(query: &str, max_results: Option<usize>) -> SearchResponse {
    let start = std::time::Instant::now();
    let max_urls = max_results.unwrap_or_else(config::max_urls);

    tracing::info!("━━━ NEW SEARCH ━━━ query={}", &query[..query.len().min(80)]);

    // ── Phase 1: Meta-Search (query all enabled engines concurrently) ────────
    let enabled = config::enabled_engines();
    let engine_instances = engines::get_engines(&enabled);

    let client = build_search_client();

    // Query all engines — sequential is fine since each is I/O-bound and fast
    let mut all_results: Vec<RawSearchResult> = Vec::new();
    for engine in &engine_instances {
        let results = engine.search(&client, query).await;
        tracing::info!("  Engine [{}]: {} results", engine.name(), results.len());
        all_results.extend(results);
    }

    let total_raw = all_results.len();
    tracing::info!("Meta-search: {} total raw results from {} engines",
                   total_raw, enabled.len());

    // ── Phase 2: URL Deduplication ───────────────────────────────────────────
    let raw_urls: Vec<String> = all_results.iter().map(|r| r.url.clone()).collect();
    let unique_urls = url_utils::deduplicate(raw_urls, max_urls);
    let deduped_count = unique_urls.len();

    tracing::info!("Deduplicated: {} → {} URLs", total_raw, deduped_count);

    // Build URL → metadata map for enriching results later
    let url_meta: std::collections::HashMap<String, &RawSearchResult> = all_results
        .iter()
        .map(|r| {
            let norm = url_utils::normalize_url(&r.url).unwrap_or_else(|| r.url.clone());
            (norm, r)
        })
        .collect();

    // ── Phase 3: Concurrent Scraping + Extraction ────────────────────────────
    let concurrency = config::concurrency();
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let scrape_client = build_scrape_client();

    let mut scrape_handles = Vec::new();

    for url in &unique_urls {
        let sem = semaphore.clone();
        let client = scrape_client.clone();
        let url = url.clone();
        let timeout_secs = config::scrape_timeout_secs();
        let max_bytes = config::max_html_bytes();

        // Find engine name for this URL
        let engine_name = url_meta
            .get(&url)
            .map(|r| r.engine.clone())
            .unwrap_or_else(|| "unknown".to_string());

        scrape_handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();

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
        }));
    }

    // Collect scrape results
    let mut results: Vec<SourceResult> = Vec::new();
    for handle in scrape_handles {
        match handle.await {
            Ok(Some(result)) if result.char_count >= config::min_text_length() => {
                results.push(result);
            }
            _ => {}
        }
    }

    let sources_processed = results.len();
    let elapsed = start.elapsed().as_secs_f64();

    tracing::info!("Scraped {}/{} URLs in {:.2}s",
                   sources_processed, deduped_count, elapsed);

    SearchResponse {
        query: query.to_string(),
        sources_found: deduped_count,
        sources_processed,
        results,
        elapsed_seconds: (elapsed * 100.0).round() / 100.0,
        engine_stats: EngineStats {
            engines_queried: enabled,
            total_raw_results: total_raw,
            deduplicated_urls: deduped_count,
        },
    }
}

/// Scrape a single URL and extract article text
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

    // Check content type
    if let Some(ct) = resp.headers().get("content-type") {
        let ct_str = ct.to_str().unwrap_or("");
        if !ct_str.contains("text/html") && !ct_str.contains("application/xhtml") {
            tracing::debug!("Skipping non-HTML content type: {}", ct_str);
            return None;
        }
    }

    // Read body with size limit
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => return None,
    };

    if bytes.len() > max_bytes {
        tracing::debug!("Page too large ({}B > {}B): {}", bytes.len(), max_bytes, url);
    }

    // Only process up to max_bytes
    let html_bytes = &bytes[..bytes.len().min(max_bytes)];
    let html = String::from_utf8_lossy(html_bytes).to_string();

    // Extract title
    let title = extractor::extract_title(&html);

    // Extract article text
    let text = extractor::extract_article_text(&html);

    if text.len() < config::min_text_length() {
        return None;
    }

    Some((title, text))
}

/// Build HTTP client for search engine queries
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

/// Build HTTP client for web scraping (more generous timeouts)
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
