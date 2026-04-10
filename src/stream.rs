use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use axum::response::sse::Event;
use scraper::{Html, Selector};
use tokio_stream::wrappers::ReceiverStream;
use std::convert::Infallible;
use std::time::Duration;

use crate::config;
use crate::engines;
use crate::extractor;
use crate::llm;
use crate::models::*;
use crate::proxy_pool::ProxyPoolManager;
use crate::ranking;
use crate::url_utils;

pub fn execute_stream_search(
    query: String,
    max_results: Option<usize>,
    focus_mode: Option<String>,
    llm_config: Option<LlmConfig>,
    enable_copilot: Option<bool>,
) -> ReceiverStream<Result<Event, Infallible>> {
    let (tx, rx) = mpsc::channel(200);
    
    // Spawn background task taking ownership of everything
    tokio::spawn(async move {
        let start = std::time::Instant::now();
        let _ = tx.send(Ok(Event::default().data(serde_json::json!({"type": "info", "text": "Starting Swift Meta-Search Pipeline..."}).to_string()))).await;

        let normalized_focus = focus_mode
            .as_deref()
            .map(|m| m.trim().to_lowercase())
            .filter(|m| !m.is_empty());
        let is_lite_mode = matches!(normalized_focus.as_deref(), Some("lite"));
        let llm_enabled = llm_config.is_some();
        let use_ranked_chunk_path = is_lite_mode;
        let max_urls = if is_lite_mode {
            max_results.unwrap_or_else(config::max_urls).min(35)
        } else {
            max_results.unwrap_or_else(config::max_urls).min(900)
        };

        // Copilot execution inline
        let mut effective_query = apply_focus_mode(&query, normalized_focus.as_deref());
        if enable_copilot.unwrap_or(false) {
            if let Some(cfg) = &llm_config {
                let _ = tx.send(Ok(Event::default().data(serde_json::json!({"type": "info", "text": "Swift Copilot rewriting query..."}).to_string()))).await;
                let rewritten = crate::copilot::rewrite_query(&effective_query, cfg).await;
                let _ = tx.send(Ok(Event::default().data(serde_json::json!({"type": "copilot_query", "text": &rewritten}).to_string()))).await;
                effective_query = rewritten;
            }
        }

        let _ = tx.send(Ok(Event::default().data(serde_json::json!({"type": "info", "text": format!("Searching engines for: {}", effective_query)}).to_string()))).await;

        let query_variations = if is_lite_mode {
            vec![effective_query.clone()]
        } else {
            engines::generate_query_variations(&effective_query)
        };

        let enabled = config::enabled_engines();
        let engine_instances = engines::get_engines(&enabled);
        let base_search_client = build_search_client(None);
        let proxy_pool = ProxyPoolManager::from_env();
        let engine_concurrency = config::engine_concurrency().max(1).min(24);
        let semaphore = Arc::new(Semaphore::new(engine_concurrency));
        let jitter_min = config::jitter_min_ms();
        let jitter_max = config::jitter_max_ms();
        let mut search_futures = Vec::new();
        let mut dispatch_idx: u64 = 0;

        for variation in &query_variations {
            for engine in &engine_instances {
                let sem = semaphore.clone();
                let engine = engine.as_ref();
                let query_clone = variation.clone();
                let base_client = base_search_client.clone();
                let proxy_hint = proxy_pool.next_proxy();
                let pool = proxy_pool.clone();
                let jitter = config::random_jitter_ms(jitter_min, jitter_max);
                let spread = (dispatch_idx % engine_concurrency as u64) * 15;
                dispatch_idx += 1;

                search_futures.push(async move {
                    let _permit = match sem.acquire().await {
                        Ok(p) => p,
                        Err(_) => return Vec::new(),
                    };

                    let delay = jitter.saturating_add(spread);
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }

                    let client = if let Some(proxy) = proxy_hint.as_deref() {
                        build_search_client(Some(proxy))
                    } else {
                        base_client
                    };

                    let results = engine.search(&client, &query_clone).await;

                    if let Some(proxy) = proxy_hint {
                        if results.is_empty() {
                            pool.mark_proxy_failure(&proxy);
                        } else {
                            pool.mark_proxy_success(&proxy);
                        }
                    }

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
        let _ = tx.send(Ok(Event::default().data(serde_json::json!({"type": "info", "text": format!("Meta-Search found {} raw links. Deduplicating...", total_raw)}).to_string()))).await;

        let raw_urls: Vec<String> = all_results.iter().map(|r| r.url.clone()).collect();
        let unique_urls = url_utils::deduplicate(raw_urls, max_urls, normalized_focus.as_deref());
        let deduped_count = unique_urls.len();
        let _ = tx.send(Ok(Event::default().data(serde_json::json!({
            "type": "info",
            "text": format!("Deduplicated to {} unique URLs.", deduped_count)
        }).to_string()))).await;

        let mut engine_by_url: HashMap<String, String> = HashMap::new();
        let mut seen = HashSet::new();
        let mut search_hits: Vec<SearchHit> = Vec::new();

        for item in &all_results {
            let normalized = url_utils::normalize_url(&item.url).unwrap_or_else(|| item.url.clone());
            engine_by_url.entry(normalized.clone()).or_insert_with(|| item.engine.clone());
            if seen.insert(normalized) && search_hits.len() < max_urls {
                search_hits.push(SearchHit {
                    url: item.url.clone(),
                    title: item.title.clone(),
                    snippet: item.snippet.clone(),
                    engine: item.engine.clone(),
                });
            }
        }

        let _ = tx.send(Ok(Event::default().data(serde_json::json!({
            "type": "search_hits",
            "data": search_hits
        }).to_string()))).await;

        // Scraping concurrent layer
        let concurrency = config::concurrency().min(40);
        let semaphore = Arc::new(Semaphore::new(concurrency));
        let scrape_client = build_scrape_client();

        // Optional llm channel
        let (mut llm_tx, llm_rx) = if llm_enabled {
            let (sc_tx, sc_rx) = mpsc::channel::<SourceResult>((concurrency.max(2)) * 2);
            (Some(sc_tx), Some(sc_rx))
        } else {
            (None, None)
        };

        // Spawn LLM in sibling background so it streams immediately
        if let Some(cfg) = llm_config {
            if let Some(sc_rx) = llm_rx {
                let tx_sse = tx.clone();
                let q_for_llm = query.clone();
                tokio::spawn(async move {
                    llm::summarize_from_stream_sse(q_for_llm, cfg, sc_rx, tx_sse).await;
                });
            }
        }

        let mut join_set = tokio::task::JoinSet::new();
        let scrape_timeout_secs = if is_lite_mode { config::scrape_timeout_secs().min(4) } else { config::scrape_timeout_secs() };
        let scrape_max_bytes = if is_lite_mode { config::max_html_bytes().min(180_000) } else { config::max_html_bytes() };
        let mut min_char_threshold = if is_lite_mode { config::min_text_length().min(400) } else { config::min_text_length() };
        if matches!(normalized_focus.as_deref(), Some("youtube") | Some("reddit")) { min_char_threshold = 10; }

        for url in unique_urls {
            let sem = semaphore.clone();
            let client_cl = scrape_client.clone();
            let engine_name = engine_by_url.get(&url).cloned().unwrap_or_else(|| "unknown".to_string());
            let focus = normalized_focus.clone();

            join_set.spawn(async move {
                let _permit = sem.acquire().await.ok()?;
                match scrape_url(
                    &client_cl,
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
                        Some(SourceResult { url, title, extracted_text: text, char_count, engine: engine_name })
                    }
                    None => None,
                }
            });
        }

        let mut scraped_results: Vec<SourceResult> = Vec::new();
        while let Some(join_res) = join_set.join_next().await {
            if let Ok(Some(result)) = join_res {
                if result.char_count >= min_char_threshold {
                    scraped_results.push(result);
                }
            }
        }

        let ranked_results = if use_ranked_chunk_path {
            ranking::rank_top_chunks(&effective_query, &scraped_results, 25)
        } else {
            Vec::new()
        };

        if let Some(ref mut ltx) = llm_tx {
            let llm_inputs = if use_ranked_chunk_path {
                ranked_results.clone()
            } else {
                ranking::rank_top_chunks(&effective_query, &scraped_results, 25)
            };

            for item in llm_inputs {
                let _ = ltx.send(item).await;
            }
        }

        drop(llm_tx);

        let reported_sources = if use_ranked_chunk_path {
            ranked_results.len()
        } else {
            scraped_results.len()
        };

        if use_ranked_chunk_path {
            let _ = tx.send(Ok(Event::default().data(serde_json::json!({
                "type": "ranked_chunks",
                "count": ranked_results.len()
            }).to_string()))).await;
        }

        let _ = tx.send(Ok(Event::default().data(serde_json::json!({
            "type": "scrape_complete",
            "sources_processed": reported_sources,
            "elapsed_seconds": start.elapsed().as_secs_f64()
        }).to_string()))).await;

    });

    ReceiverStream::new(rx)
}

fn apply_focus_mode(query: &str, focus_mode: Option<&str>) -> String {
    let base = query.trim();
    match focus_mode {
        Some("reddit") => format!("{} site:reddit.com", base),
        Some("youtube") => format!("{} site:youtube.com", base),
        Some("academic") => format!("{} site:edu OR site:gov OR site:nature.com", base),
        Some("lite") | _ => base.to_string(),
    }
}

async fn scrape_url(
    client: &reqwest::Client,
    url: &str,
    timeout_secs: u64,
    max_bytes: usize,
    lite_mode: bool,
    focus_mode: Option<&str>,
) -> Option<(String, String)> {
    let mut target_url = url.to_string();

    if matches!(focus_mode, Some("reddit"))
        && target_url.contains("reddit.com/")
        && !target_url.contains("old.reddit.com")
    {
        target_url = target_url
            .replace("www.reddit.com", "old.reddit.com")
            .replace("reddit.com", "old.reddit.com");
    }

    let mut req = config::apply_browser_headers(client.get(&target_url), &target_url)
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9")
        .header("Accept-Encoding", "gzip, deflate, br");

    if timeout_secs > 0 {
        req = req.timeout(Duration::from_secs(timeout_secs));
    }

    let resp = match req.send().await
    {
        Ok(r) => r,
        Err(_) => return None,
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

    let mut text = extractor::extract_article_text(&html);

    if lite_mode {
        if text.len() < 120 { return None; }
        if text.len() > 4_000 { text.truncate(4_000); }
        return Some((title, text));
    }
    if text.len() < config::min_text_length() { return None; }
    
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
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(5))
        .pool_max_idle_per_host(5)
        .cookie_store(true)
        .tcp_nodelay(true)
        .gzip(true).brotli(true)
        .deflate(true)
        .https_only(false);

    if let Some(proxy_url) = proxy {
        if let Ok(proxy_cfg) = reqwest::Proxy::all(proxy_url) {
            builder = builder.proxy(proxy_cfg);
        }
    }

    builder.build().expect("search HTTP client error")
}

fn build_scrape_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(config::scrape_timeout_secs()))
        .redirect(reqwest::redirect::Policy::limited(5))
        .pool_max_idle_per_host(10)
        .gzip(true).brotli(true).deflate(true)
        .build().expect("scrape HTTP client error")
}
