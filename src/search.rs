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

use std::collections::HashMap;
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

    // ONE dispatch covering every (engine x sub-query) pair, under ONE
    // deadline. An earlier v8.0 draft dispatched sub-queries sequentially,
    // which multiplied the deadline by the number of variations (measured:
    // 18,004 ms for 3 sub-queries against a 6 s deadline). Results are tagged
    // with their variation index so cross-query fusion can still group them.
    let tagged = dispatch_engines(
        &engine_instances, &query_variations,
        &plain_fetcher, &stealth_fetcher,
        &stealth_engine_set, &stealth_mode,
        &proxy_pool, &engine_semaphore, jitter_min, jitter_max,
        engine_concurrency, engine_deadline,
    ).await;

    if query_variations.len() <= 1 {
        all_results.extend(tagged.into_iter().flat_map(|(_, v)| v));
    } else {
        // Regroup per sub-query and fuse, weighting the original query 2x
        // (P1-9). v7.0 flat-extended every variation into one list, so a URL
        // found only by a derived sub-query carried the same RRF weight as one
        // found by the user's actual query — and `cross_query_fusion`, which
        // exists to prevent exactly that, was dead code.
        let mut grouped: Vec<Vec<RawSearchResult>> = vec![Vec::new(); query_variations.len()];
        for (idx, batch) in tagged {
            if let Some(slot) = grouped.get_mut(idx) {
                slot.extend(batch);
            }
        }
        let per_query: Vec<(String, Vec<RawSearchResult>)> = query_variations
            .iter()
            .cloned()
            .zip(grouped)
            .inspect(|(q, v)| {
                tracing::info!(
                    "Sub-query [{}]: {} results",
                    crate::util::truncate_chars(q, 60),
                    v.len()
                );
            })
            .collect();
        let fused = ranking::cross_query_fusion(&per_query, &effective_query);
        tracing::info!(
            "Cross-query fusion: {} sub-queries -> {} fused unique URLs",
            per_query.len(),
            fused.len()
        );
        all_results.extend(fused);
    }

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

    // ═══════════════════════════════════════════════════════════════════════
    // v8.0 P1-3: SELECT SOURCES BY RRF CONSENSUS, NOT ARRIVAL ORDER
    //
    // v7.0 computed RRF for every URL and then threw the ordering away:
    //
    //     let raw_urls = all_results.iter().map(|r| r.url.clone()).collect();
    //     let unique_urls = url_utils::deduplicate(raw_urls, max_urls, focus);
    //
    // `deduplicate` walks `all_results` in the order engines happened to
    // FINISH and takes the first `max_urls`. In the measured baseline that
    // meant 85 URLs were scored and the 20 that got scraped were all
    // `bing_news` — simply because bing_news returned first. Brave's 44
    // results and Yahoo's 56 were discovered, scored, and silently dropped.
    //
    // The same arrival order also built `search_results`, which is what the UI
    // renders. So both "what we read" and "what we show" were decided by
    // network race conditions rather than by the consensus ranking.
    // ═══════════════════════════════════════════════════════════════════════

    struct Candidate {
        url: String,
        title: String,
        snippet: String,
        engines: Vec<String>,
        families: Vec<&'static str>,
        rrf: f64,
    }

    let mut by_url: HashMap<String, Candidate> = HashMap::new();
    for item in &all_results {
        let key = url_utils::canonical_key(&item.url);
        let entry = by_url.entry(key).or_insert_with(|| Candidate {
            url: item.url.clone(),
            title: item.title.clone(),
            snippet: item.snippet.clone(),
            engines: Vec::new(),
            families: Vec::new(),
            rrf: 0.0,
        });
        // Keep the best metadata across engines: v7.0 took whichever arrived
        // first, which for link-scraper engines was an empty snippet.
        if entry.snippet.is_empty() && !item.snippet.is_empty() {
            entry.snippet = item.snippet.clone();
        }
        if entry.title.is_empty() && !item.title.is_empty() {
            entry.title = item.title.clone();
        }
        if !entry.engines.contains(&item.engine) {
            entry.engines.push(item.engine.clone());
        }
        let fam = engines::engine_family(&item.engine);
        if !entry.families.contains(&fam) {
            entry.families.push(fam);
        }
    }

    // Attach consensus scores and drop blocked/binary URLs.
    let mut ranked: Vec<Candidate> = by_url
        .into_iter()
        .filter_map(|(key, mut c)| {
            let bypass = match normalized_focus.as_deref() {
                Some("reddit") => c.url.contains("reddit.com"),
                Some("youtube") => c.url.contains("youtube.com") || c.url.contains("youtu.be"),
                _ => false,
            };
            if !bypass && url_utils::should_skip(&c.url) {
                return None;
            }
            c.rrf = rrf_scores.get(&key).copied().unwrap_or(0.0);
            Some(c)
        })
        .collect();

    // Highest cross-family consensus first.
    ranked.sort_by(|a, b| {
        b.rrf
            .partial_cmp(&a.rrf)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Tie-break on how many INDEPENDENT families found it.
            .then_with(|| b.families.len().cmp(&a.families.len()))
    });

    let total_candidates = ranked.len();
    ranked.truncate(max_urls);
    let deduped_count = ranked.len();

    tracing::info!(
        "Source selection: {} candidates -> top {} by RRF consensus (top score {:.4}, {} found by >1 family)",
        total_candidates,
        deduped_count,
        ranked.first().map(|c| c.rrf).unwrap_or(0.0),
        ranked.iter().filter(|c| c.families.len() > 1).count()
    );

    let unique_urls: Vec<String> = ranked.iter().map(|c| c.url.clone()).collect();

    let mut engine_by_url: HashMap<String, String> = HashMap::new();
    for c in &ranked {
        engine_by_url.insert(c.url.clone(), c.engines.join(","));
    }

    // The UI list is now the SAME ranked set, in the same order.
    let search_results: Vec<SearchHit> = ranked
        .iter()
        .map(|c| SearchHit {
            url: c.url.clone(),
            title: c.title.clone(),
            snippet: c.snippet.clone(),
            engine: c.engines.join(","),
            rrf_score: c.rrf,
            found_by_families: c.families.len(),
        })
        .collect();

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
            // ── Page cache (P2-3) ──
            // Article bodies are stable, so this is where the real latency
            // win is: the measured scrape stage was 3.4-12.3 seconds.
            if let Some(cached) = crate::cache_layer::get_page(&url).await {
                return Some(SourceResult {
                    url: url.clone(),
                    title: cached.title.clone(),
                    extracted_text: cached.text.clone(),
                    char_count: crate::util::char_len(&cached.text),
                    engine: engine_name,
                    published: cached.published,
                    language: cached.language.clone(),
                    confidence: cached.confidence,
                    extractor: cached.source,
                });
            }

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
                    crate::cache_layer::put_page(&url, e.clone()).await;
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

    // ── Collect scraped results, with the same ceiling-not-floor discipline
    //    applied to the engine stage ──
    //
    // The scrape stage sat at exactly SCRAPE_TIMEOUT (8,000 ms) on every
    // query, because one slow URL out of forty holds the stage open even when
    // the other thirty-nine finished in under a second. We now stop once we
    // have enough *usable* text, and hard-stop at a stage deadline.
    let scrape_deadline = tokio::time::Instant::now()
        + Duration::from_secs(config::scrape_stage_deadline_secs());
    let want_sources = config::scrape_min_sources().min(deduped_count.max(1));
    let want_chars = config::scrape_min_chars();

    let mut raw_scraped_results: Vec<SourceResult> = Vec::new();
    let mut scraped_chars = 0usize;
    let mut scrape_tasks_done = 0usize;
    let total_scrape_tasks = join_set.len();

    loop {
        if scrape_tasks_done >= total_scrape_tasks {
            break;
        }
        // Enough evidence? Stop waiting on the tail.
        if raw_scraped_results.len() >= want_sources && scraped_chars >= want_chars {
            let remaining = join_set.len();
            tracing::info!(
                "Scrape early-exit: {} sources / {} chars ({}/{} tasks) — aborting {} straggler(s)",
                raw_scraped_results.len(), scraped_chars,
                scrape_tasks_done, total_scrape_tasks, remaining
            );
            join_set.abort_all();
            break;
        }

        let next = tokio::select! {
            res = join_set.join_next() => res,
            _ = tokio::time::sleep_until(scrape_deadline) => {
                let remaining = join_set.len();
                if remaining > 0 {
                    tracing::info!(
                        "Scrape deadline reached: {} sources from {}/{} tasks, aborting {}",
                        raw_scraped_results.len(), scrape_tasks_done, total_scrape_tasks, remaining
                    );
                    join_set.abort_all();
                }
                break;
            }
        };

        match next {
            Some(Ok(Some(result))) => {
                scraped_chars += result.char_count;
                raw_scraped_results.push(result);
                scrape_tasks_done += 1;
            }
            Some(Ok(None)) => scrape_tasks_done += 1,
            Some(Err(err)) => {
                if !err.is_cancelled() {
                    tracing::debug!("Scrape task join error: {}", err);
                }
                scrape_tasks_done += 1;
            }
            None => break,
        }
    }

    // Drain anything that completed between the abort and now. The running
    // char total is no longer consulted here — the exit decision is already
    // made — so it is deliberately not updated.
    while let Some(join_result) = join_set.join_next().await {
        if let Ok(Some(result)) = join_result {
            raw_scraped_results.push(result);
        }
    }
    let _ = scraped_chars;

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
    // NOTE: ranking uses the ORIGINAL `query`, not `effective_query`.
    // `effective_query` carries engine syntax — an injected year and any
    // `site:reddit.com` operator — and v7.0 fed that to BM25, so "site" and
    // "reddit.com" became scoring terms and skewed relevance toward chunks
    // that merely mention Reddit.
    let mut results = if use_ranked_chunk_path {
        ranking::rank_top_chunks_with_signals(query, &raw_scraped_results, llm_top_k, Some(&rrf_scores), Some(&intel))
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
                if let Some(cached) =
                    crate::cache_layer::get_serp(&ename, &query_variant, 0).await
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

                let results = engine.search(&fetcher, &query_variant).await;

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
                crate::cache_layer::put_serp(&ename, &query_variant, 0, results.clone()).await;

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
    let min_results_to_exit = config::early_exit_min_results();
    let min_families_to_exit = config::early_exit_min_families();

    loop {
        // Stop as soon as we have genuinely sufficient coverage. Additional
        // results from a fifth-choice engine have sharply diminishing value,
        // and waiting for known-failing engines has none.
        if tasks_completed >= total_tasks {
            break;
        }
        {
            let flat_len: usize = all.iter().map(|(_, v)| v.len()).sum();
            if flat_len >= min_results_to_exit {
                let fams: std::collections::HashSet<&str> = all
                    .iter()
                    .flat_map(|(_, v)| v.iter())
                    .map(|r| engines::engine_family(&r.engine))
                    .collect();
                if fams.len() >= min_families_to_exit {
                    let remaining = join_set.len();
                    tracing::info!(
                        "Engine early-exit: {} results from {} independent families \
                         ({}/{} tasks done) — aborting {} slower task(s)",
                        flat_len, fams.len(), tasks_completed, total_tasks, remaining
                    );
                    join_set.abort_all();
                    break;
                }
            }
        }

        let result = tokio::select! {
            res = join_set.join_next() => res,
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
                    tasks_completed, total_tasks
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
    let families: std::collections::HashSet<&str> =
        flat.iter().map(|r| engines::engine_family(&r.engine)).collect();
    tracing::info!(
        "Engine dispatch: {}/{} tasks done | {} engines produced results | {} independent families | {} raw results",
        tasks_completed, total_tasks, producing.len(), families.len(), flat.len()
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
