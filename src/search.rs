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
//   - TempDb session tracking + HistoryDb persistence
// ============================================================================

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use scraper::Html;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config;
use crate::engines;
use crate::fetcher::{self, Fetcher};
use crate::extractor;
use crate::llm;
use crate::models::*;
use crate::proxy_pool::ProxyPoolManager;
use crate::query_intel;
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

    // v6.1.0: Mode detection — auto (default), fast, research, specialized
    let is_auto_mode = matches!(normalized_focus.as_deref(), Some("auto") | Some("lite") | None);
    let is_fast_mode = matches!(normalized_focus.as_deref(), Some("fast"));
    let is_research_mode = matches!(normalized_focus.as_deref(), Some("research"));

    // Specialized mode detection: "specialized_tech", "specialized_science", etc.
    let is_specialized = normalized_focus.as_ref().map_or(false, |m| m.starts_with("specialized"));
    let specialized_domain = if is_specialized {
        normalized_focus.as_ref().and_then(|m| m.strip_prefix("specialized_")).map(|s| s.to_string())
    } else {
        None
    };

    let use_ranked_chunk_path = is_auto_mode || is_fast_mode || is_specialized;

    let mut max_urls = if is_fast_mode {
        max_results.unwrap_or(30).min(30)
    } else if is_auto_mode {
        // Auto mode: initial cap, QueryIntelligence will refine after analysis
        max_results.unwrap_or_else(config::max_urls).min(100)
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

    // ═══════════════════════════════════════════════════════════════════════
    // v6.1.0: QUERY INTELLIGENCE — Analyze intent before anything else
    // This drives time injection, engine boost, source count, and LLM prompts
    // In Auto mode, it also adaptively tunes max_urls and engine selection
    // ═══════════════════════════════════════════════════════════════════════
    let intel = query_intel::analyze_query(&effective_query);
    tracing::info!(
        "QueryIntel: intent={} time_sensitive={} complexity={} entities={:?} optimal_sources={}",
        intel.intent, intel.is_time_sensitive, intel.complexity,
        intel.key_entities, intel.optimal_source_count
    );

    // v6.1.0: Auto-mode adaptive tuning — QueryIntelligence refines max_urls
    if is_auto_mode {
        max_urls = match intel.complexity {
            query_intel::QueryComplexity::Simple => max_urls.min(50),
            query_intel::QueryComplexity::Complex => max_urls.max(100).min(150),
            query_intel::QueryComplexity::Medium => max_urls, // moderate — keep default
        };
        // Navigational queries just need a URL, go fast
        if intel.intent == query_intel::QueryIntent::Navigational {
            max_urls = max_urls.min(30);
        }
        tracing::info!("Auto-mode tuned: complexity={:?} → max_urls={}", intel.complexity, max_urls);
    }

    // ─── Time-enrichment: now driven by QueryIntelligence ───
    // If query is time-sensitive, inject current date for search engine recency
    {
        let q_lower = effective_query.to_lowercase();
        // Check if query already contains a year (2020-2030)
        let has_year = (2020..=2030).any(|y| q_lower.contains(&y.to_string()));

        if intel.is_time_sensitive && !has_year {
            let year = chrono::Utc::now().format("%Y").to_string();
            effective_query = format!("{} {}", effective_query, year);
            tracing::info!("Time-enrichment (intent={}): appended year → [{}]", intel.intent, effective_query);
        }
    }

    // v7.0: Multi-query decomposition — expand query into targeted sub-queries
    let query_variations = if query_intel::multi_query_enabled() && !is_fast_mode {
        let intel = query_intel::analyze_query(&effective_query);
        let sub_queries = query_intel::decompose_query(&effective_query, &intel);
        tracing::info!(
            "Multi-query: decomposed into {} sub-queries: {:?}",
            sub_queries.len(),
            sub_queries.iter().map(|q| crate::util::truncate_chars(q, 50)).collect::<Vec<_>>()
        );
        sub_queries
    } else {
        vec![effective_query.clone()]
    };

    let jitter_min = config::jitter_min_ms();
    let jitter_max = config::jitter_max_ms();

    tracing::info!(
        "NEW SEARCH query={} focus_mode={:?} auto={} fast={} research={} max_urls={} variations={}",
        crate::util::truncate_chars(&effective_query, 120),
        normalized_focus,
        is_auto_mode,
        is_fast_mode,
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
    } else if is_fast_mode {
        // Fast mode: primary engines only — speed over coverage
        engines::primary_engines()
    } else if is_auto_mode {
        // Auto mode: all engines for maximum coverage
        engines::all_engines()
    } else {
        config::enabled_engines()
    };

    // v8.0: drop engines the health registry has benched (P1-1).
    let engine_list = engines::filter_healthy(engine_list);
    let engine_instances = engines::get_engines(&engine_list);

    // Build BOTH fetchers ONCE. Cloning a Fetcher is an Arc clone of the inner
    // client, so every task shares connection pools.
    //
    // v7.0 constructed a fresh reqwest::Client *inside each spawned task* when
    // proxies were enabled — up to 175 clients per search, each with its own
    // pool, defeating connection reuse entirely. And the wreq client it built
    // was passed in as `_stealth_client` and never used at all.
    let (plain_fetcher, stealth_fetcher) = fetcher::build_pair(None);
    let stealth_engine_set: std::collections::HashSet<String> =
        config::stealth_engines().into_iter().collect();
    let stealth_mode = config::stealth_mode();
    tracing::info!(
        "Stealth mode: {} ({} engines in override set)",
        stealth_mode,
        stealth_engine_set.len()
    );
    let proxy_pool = ProxyPoolManager::from_env();
    if proxy_pool.has_proxies() {
        tracing::info!("Proxy pool loaded with {} entries", proxy_pool.len());
    }
    let engine_concurrency = config::engine_concurrency().max(1).min(40);
    let engine_semaphore = Arc::new(Semaphore::new(engine_concurrency));

    let mut all_results: Vec<RawSearchResult> = Vec::new();

    // ── v6.1.0: Stage timing — engine dispatch ──
    let stage_start = std::time::Instant::now();

    // ── Single-phase: ALL engines fire simultaneously ──
    // v8.0: mode-aware deadline. v7.0 hardcoded 6s for every mode despite a
    // comment promising "lite 2.5s, research 8s, specialized 5s" — research
    // mode, which is meant to gather 200+ sources, got the same 6s as fast.
    let engine_deadline = Duration::from_millis(if is_fast_mode {
        2_500
    } else if is_research_mode {
        20_000
    } else if is_specialized {
        8_000
    } else {
        6_000
    });

    let results = dispatch_engines(
        &engine_instances, &query_variations,
        &plain_fetcher, &stealth_fetcher,
        &stealth_engine_set, &stealth_mode,
        &proxy_pool, &engine_semaphore, jitter_min, jitter_max,
        engine_concurrency, engine_deadline,
    ).await;
    all_results.extend(results);

    let engine_dispatch_ms = stage_start.elapsed().as_millis();
    tracing::info!(
        "⏱ Stage [engine_dispatch]: {}ms — {} results from {} engines",
        engine_dispatch_ms, all_results.len(), engine_list.len()
    );

    // ═══════════════════════════════════════════════════════════════════════
    // v6.1.0: RRF PRE-RANKING — Compute cross-engine consensus scores
    // URLs appearing in top positions across MULTIPLE engines get highest scores
    // ═══════════════════════════════════════════════════════════════════════
    let stage_start = std::time::Instant::now();
    let rrf_scores = ranking::compute_rrf_scores(&all_results);
    let rrf_ms = stage_start.elapsed().as_millis();
    tracing::info!("⏱ Stage [rrf_compute]: {}ms — {} unique URLs", rrf_ms, rrf_scores.len());

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
    // v5.2.0: Use intent-aware optimal source count from QueryIntelligence
    let llm_top_k: usize = if is_research_mode {
        200
    } else if is_specialized {
        50
    } else {
        intel.optimal_source_count.max(25) // At least 25, but QueryIntel may suggest more
    };

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

    // ── v6.1.0: Stage timing — scrape ──
    let stage_start = std::time::Instant::now();

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
                is_fast_mode,
                focus.as_deref(),
            )
            .await
            {
                Some(e) => {
                    let char_count = crate::util::char_len(&e.text);
                    Some(SourceResult {
                        url,
                        title: e.title,
                        extracted_text: e.text,
                        char_count,
                        engine: engine_name,
                        published: e.published,
                        language: e.language,
                        confidence: e.confidence,
                        extractor: e.source,
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

    let scrape_ms = stage_start.elapsed().as_millis();
    // Extraction VOLUME is the metric that matters for answer quality — v7.0
    // averaged 470 chars/source, i.e. meta-description-level content, and had
    // no instrumentation that would have revealed it.
    let total_chars: usize = raw_scraped_results.iter().map(|r| r.char_count).sum();
    let avg_chars = total_chars / raw_scraped_results.len().max(1);
    let with_date = raw_scraped_results.iter().filter(|r| r.published.is_some()).count();
    tracing::info!(
        "⏱ Stage [scrape]: {}ms — {} sources from {} URLs | extracted {} chars (avg {}/source) | {} with publish date",
        scrape_ms, raw_scraped_results.len(), deduped_count, total_chars, avg_chars, with_date
    );

    // ── v6.1.0: Stage timing — ranking ──
    let stage_start = std::time::Instant::now();

    // ── Rank and prepare LLM input ──
    // v6.1.0: Pass RRF scores into ranking for hybrid BM25+RRF scoring
    let scraped_count = raw_scraped_results.len();
    let mut results = if use_ranked_chunk_path {
        ranking::rank_top_chunks_with_rrf(&effective_query, &raw_scraped_results, llm_top_k, Some(&rrf_scores))
    } else {
        raw_scraped_results
    };

    let rank_ms = stage_start.elapsed().as_millis();
    tracing::info!(
        "⏱ Stage [rank]: {}ms — {} chunks ranked from {} sources",
        rank_ms, results.len(), scraped_count
    );

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

    // ── v6.1.0: Stage timing — LLM synthesis ──
    let stage_start = std::time::Instant::now();

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
            // ── LITE MODE: single fast call with intent-aware prompts ──
            tracing::info!("Feeding {} chunks to LLM (lite mode, intent={})", llm_inputs.len(), intel.intent);
            llm::summarize_direct_with_intent(query, cfg, &llm_inputs, &intel).await
        }
    } else {
        llm::LlmExecutionResult::default()
    };

    let llm_ms = stage_start.elapsed().as_millis();
    tracing::info!(
        "⏱ Stage [llm]: {}ms — batches={} answer_len={}",
        llm_ms,
        llm_result.batches_processed,
        llm_result.llm_answer.as_ref().map(|a| a.len()).unwrap_or(0)
    );

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

    // v6.1.0: Pipeline timing summary
    tracing::info!(
        "⏱ Pipeline TOTAL: {:.0}ms — engines={}ms scrape={}ms rank={}ms rrf={}ms llm={}ms | raw={} dedup={} scraped={} mode={}",
        elapsed * 1000.0,
        engine_dispatch_ms, scrape_ms, rank_ms, rrf_ms, llm_ms,
        total_raw, deduped_count, scraped_count, focus_str
    );

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
        Some(m) if m.starts_with("specialized") => base.to_string(),
        // auto, fast, research, lite (backward compat), or unrecognized → pass through
        Some("auto") | Some("fast") | Some("research") | Some("lite") | _ => base.to_string(),
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
) -> Vec<RawSearchResult> {
    let mut join_set = JoinSet::new();
    let mut dispatch_index: u64 = 0;

    for variation in query_variations {
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
                    Err(_) => return Vec::new(),
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

                let results = engine.search(&fetcher, &query_variant).await;

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

    // ── Collect with a deadline-based early cancel ──
    let deadline_instant = tokio::time::Instant::now() + deadline;
    let mut all: Vec<RawSearchResult> = Vec::new();
    let mut tasks_completed = 0usize;
    let total_tasks = join_set.len();

    loop {
        let result = tokio::select! {
            res = join_set.join_next() => res,
            _ = tokio::time::sleep_until(deadline_instant) => {
                let remaining = join_set.len();
                if remaining > 0 {
                    tracing::info!(
                        "Engine deadline ({:?}) reached: {}/{} tasks done, aborting {} (have {} results)",
                        deadline, tasks_completed, total_tasks, remaining, all.len()
                    );
                    join_set.abort_all();
                }
                break;
            }
        };

        match result {
            Some(Ok(batch)) => {
                all.extend(batch);
                tasks_completed += 1;
            }
            Some(Err(err)) => {
                if !err.is_cancelled() {
                    tracing::debug!("Engine task join error: {}", err);
                }
                tasks_completed += 1;
            }
            None => break,
        }
    }

    // Drain anything that finished between the abort and now.
    while let Some(result) = join_set.join_next().await {
        if let Ok(batch) = result {
            all.extend(batch);
        }
    }

    // v8.0: count DISTINCT engines that produced results, not tasks. v7.0
    // logged "engines completed" for engines x variations, which made a
    // single-engine run look like 33 engines succeeding.
    let producing: std::collections::HashSet<&str> =
        all.iter().map(|r| r.engine.as_str()).collect();
    let families: std::collections::HashSet<&str> =
        all.iter().map(|r| engines::engine_family(&r.engine)).collect();
    tracing::info!(
        "Engine dispatch: {}/{} tasks done | {} engines produced results | {} independent families | {} raw results",
        tasks_completed, total_tasks, producing.len(), families.len(), all.len()
    );
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
) -> Option<crate::extractor::Extracted> {
    // First attempt
    match scrape_url_inner(client, url, timeout_secs, max_bytes, focus_mode).await {
        ScrapeResult::Ok(e) => Some(e),
        ScrapeResult::Blocked => {
            // ── Retry with fresh browser profile after delay ──
            let delay = config::random_jitter_ms(200, 500);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            tracing::debug!("Retry after block: {}", url);
            match scrape_url_inner(client, url, timeout_secs, max_bytes, focus_mode).await {
                ScrapeResult::Ok(e) => Some(e),
                _ => None,
            }
        }
        ScrapeResult::Skip => None,
    }
}

enum ScrapeResult {
    Ok(crate::extractor::Extracted),
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

    // Charset-aware decode (v8.0). `from_utf8_lossy` mangled every
    // windows-1252 / ISO-8859-1 / GBK / Shift_JIS page into replacement chars.
    let html_bytes = &bytes[..bytes.len().min(max_bytes)];
    let html = crate::fetcher::decode_body(html_bytes);

    // ── Single-pass DOM extraction: title + text in one parse ──
    // v5.0 fix: eliminates the double Html::parse_document() bottleneck
    if matches!(focus_mode, Some("youtube")) || target_url.contains("youtube.com/watch") {
        // YouTube fast path: try JSON-LD first (no DOM parse needed)
        if let Some(desc) = extract_youtube_json_ld(&html) {
            return ScrapeResult::Ok(crate::extractor::Extracted {
                title: extractor::extract_title_from_html(&html),
                text: desc,
                confidence: 0.6,
                ..Default::default()
            });
        }
        // Fallback: single DOM parse for both meta desc + title
        let doc = Html::parse_document(&html);
        let title = extractor::extract_title_from_doc_pub(&doc);
        if let Some(desc) = extractor::extract_youtube_meta(&doc) {
            return ScrapeResult::Ok(crate::extractor::Extracted {
                title,
                text: desc,
                confidence: 0.6,
                ..Default::default()
            });
        }
    }

    // Normal pages: trafilatura + conditional Readability (v8.0 P1-4).
    let extracted = extractor::extract(&html, &target_url);
    if !extractor::is_usable(&extracted) {
        return ScrapeResult::Skip;
    }
    ScrapeResult::Ok(extracted)
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
