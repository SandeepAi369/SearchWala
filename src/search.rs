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
use crate::modes;
use crate::synthesis;
use crate::proxy_pool::ProxyPoolManager;
use crate::query_intel;
use crate::ranking;
use crate::url_utils;

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
}

async fn emit(tx: &Option<tokio::sync::mpsc::Sender<Progress>>, p: Progress) {
    if let Some(t) = tx {
        let _ = t.send(p).await;
    }
}

/// Back-compatible entry point (no domain, no progress).
pub async fn execute_search(
    query: &str,
    max_results: Option<usize>,
    focus_mode: Option<String>,
    llm_config: Option<LlmConfig>,
    enable_copilot: Option<bool>,
    temp_db: Option<&crate::cache::TempDb>,
    history_db: Option<&crate::cache::HistoryDb>,
) -> SearchResponse {
    execute_search_full(
        SearchParams {
            query: query.to_string(),
            max_results,
            focus_mode,
            domain: None,
            llm: llm_config,
            enable_copilot: enable_copilot.unwrap_or(false),
        },
        temp_db,
        history_db,
        None,
    )
    .await
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

/// The whole pipeline. Every mode-dependent decision is read from the
/// `ModeProfile` — see `modes.rs` for why that matters.
pub async fn execute_search_full(
    params: SearchParams,
    temp_db: Option<&crate::cache::TempDb>,
    history_db: Option<&crate::cache::HistoryDb>,
    progress: Option<tokio::sync::mpsc::Sender<Progress>>,
) -> SearchResponse {
    let start = std::time::Instant::now();
    let query = params.query.trim().to_string();
    let (mode, domain) = modes::parse(params.focus_mode.as_deref(), params.domain.as_deref());

    // ── Optional LLM query rewrite ──
    let mut effective_query = query.clone();
    let mut copilot_out = None;
    if params.enable_copilot {
        if let Some(cfg) = &params.llm {
            let rewritten = crate::copilot::rewrite_query(&effective_query, cfg).await;
            tracing::info!("Copilot rewrote query: [{}] -> [{}]", effective_query, rewritten);
            copilot_out = Some(rewritten.clone());
            effective_query = rewritten;
        }
    }

    // ── Query intelligence + mode profile ──
    let intel = query_intel::analyze_query(&effective_query);
    let profile = modes::resolve(mode, domain, &intel, params.max_results);
    tracing::info!(
        "Mode {} [{}] domain={} intent={} complexity={} | engines={} pages={} deadline={:?} \
         urls={} subq={} top_k={} synthesis={:?}",
        mode.as_str(), profile.tier, domain.as_str(), intel.intent, intel.complexity,
        profile.engines.len(), profile.page_budget, profile.engine_deadline,
        profile.max_urls, profile.max_sub_queries, profile.llm_top_k, profile.synthesis
    );

    // ── Recency hint for engines ──
    // Skipped for Study and Academic: those topics are evergreen, and a year in
    // an OpenAlex/arXiv search restricts results to papers containing "2026".
    if intel.is_time_sensitive && !matches!(mode, modes::Mode::Study | modes::Mode::Academic) {
        let q_lower = effective_query.to_lowercase();
        let has_year = (2015..=2035).any(|y| q_lower.contains(&y.to_string()));
        if !has_year {
            effective_query = format!("{} {}", effective_query, chrono::Utc::now().format("%Y"));
        }
    }

    // ── Sub-queries ──
    let mut query_variations: Vec<String> =
        if profile.max_sub_queries > 1 && query_intel::multi_query_enabled() {
            let mut planned = None;
            if profile.llm_query_planning {
                if let Some(cfg) = &params.llm {
                    planned = synthesis::plan_subqueries(
                        &effective_query,
                        cfg,
                        profile.max_sub_queries.saturating_sub(1),
                    )
                    .await;
                }
            }
            match planned {
                Some(p) => {
                    tracing::info!("LLM planned {} sub-queries", p.len());
                    let mut v = vec![effective_query.clone()];
                    v.extend(p);
                    v.truncate(profile.max_sub_queries);
                    v
                }
                None => modes::facet_queries(&effective_query, &intel, profile.facets, profile.max_sub_queries),
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
    tracing::info!("Sub-queries: {:?}", query_variations.iter().map(|q| crate::util::truncate_chars(q, 60)).collect::<Vec<_>>());

    let engine_list = engines::filter_healthy(profile.engines.clone());
    let engine_instances = engines::get_engines(&engine_list);

    emit(&progress, Progress::Plan {
        mode: mode.as_str().into(),
        tier: profile.tier.into(),
        domain: domain.as_str().into(),
        engines: engine_instances.len(),
        sub_queries: query_variations.clone(),
        max_urls: profile.max_urls,
    })
    .await;

    // ── Dispatch ──
    let (plain_fetcher, stealth_fetcher) = fetcher::build_pair(None);
    let stealth_engine_set: std::collections::HashSet<String> =
        config::stealth_engines().into_iter().collect();
    let stealth_mode = config::stealth_mode();
    let proxy_pool = ProxyPoolManager::from_env();
    let engine_concurrency = config::engine_concurrency().max(1).min(40);
    let engine_semaphore = Arc::new(Semaphore::new(engine_concurrency));

    let stage_start = std::time::Instant::now();
    let tagged = dispatch_engines(
        &engine_instances,
        &query_variations,
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
    let fused_scores: Option<HashMap<String, f64>> = if query_variations.len() <= 1 {
        None
    } else {
        // Regroup per sub-query and fuse with the ORIGINAL weighted 2x. The
        // original is `query_variations[0]` — it already carries any site:
        // filter, so comparing against the bare query would never match and
        // the 2x weight would silently never apply.
        let mut grouped: Vec<Vec<RawSearchResult>> = vec![Vec::new(); query_variations.len()];
        for (idx, batch) in &tagged {
            if let Some(slot) = grouped.get_mut(*idx) {
                slot.extend(batch.iter().cloned());
            }
        }
        let per_query: Vec<(String, Vec<RawSearchResult>)> =
            query_variations.iter().cloned().zip(grouped).collect();
        Some(ranking::cross_query_scores(&per_query, &query_variations[0]))
    };
    let mut all_results: Vec<RawSearchResult> = tagged.into_iter().flat_map(|(_, v)| v).collect();

    if profile.site_filter.is_some() {
        let before = all_results.len();
        all_results.retain(|r| modes::matches_site_filter(&r.url, profile.site_filter));
        tracing::info!("Site filter kept {}/{} results", all_results.len(), before);
    }

    let engine_dispatch_ms = stage_start.elapsed().as_millis();
    let families: std::collections::HashSet<&str> =
        all_results.iter().map(|r| engines::engine_family(&r.engine)).collect();
    tracing::info!(
        "⏱ Stage [engine_dispatch]: {}ms — {} results, {} families",
        engine_dispatch_ms, all_results.len(), families.len()
    );
    emit(&progress, Progress::Engines {
        raw_results: all_results.len(),
        families: families.len(),
        ms: engine_dispatch_ms,
    })
    .await;

    // ── Consensus ranking + source selection ──
    let stage_start = std::time::Instant::now();
    let rrf_scores = fused_scores.unwrap_or_else(|| ranking::compute_rrf_scores(&all_results));
    let rrf_ms = stage_start.elapsed().as_millis();
    let total_raw = all_results.len();

    let mut by_url: HashMap<String, Candidate> = HashMap::new();
    for item in &all_results {
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
            // Site-filtered modes must keep their own domain even if it is on
            // the general skip list.
            if !site_mode && url_utils::should_skip(&c.url) {
                return None;
            }
            c.rrf = rrf_scores.get(&key).copied().unwrap_or(0.0);
            Some(c)
        })
        .collect();
    // Selection score = consensus × mode/domain source preference.
    //
    // Pure RRF rewards being found by many indices, which a scholarly paper —
    // present in OpenAlex alone — can never be. Measured in Academic mode:
    // OpenAlex returned 40 papers per sub-query (18-21 with abstracts), yet
    // Wikipedia filled the top 11 slots and only 3 abstracts were used. The
    // same preferences already applied to chunk ranking; they now decide which
    // pages are read, too.
    let selection_score = |c: &Candidate| -> f64 {
        let mut s = c.rrf * modes::source_boost(&c.url, mode, domain);
        // Engine-supplied abstracts are free to "scrape" and dense with
        // evidence; prefer them where evidence quality matters most.
        // Not Deep: there it floated off-topic papers above the canonical
        // answer for factual questions (measured on "capital of France").
        if c.full_text.is_some() && matches!(mode, modes::Mode::Academic | modes::Mode::Study) {
            s *= 1.5;
        }
        s
    };
    ranked.sort_by(|a, b| {
        selection_score(b)
            .partial_cmp(&selection_score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.families.len().cmp(&a.families.len()))
    });
    ranked.truncate(profile.max_urls);
    let deduped_count = ranked.len();

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
    emit(&progress, Progress::Sources { results: search_results.clone() }).await;

    // ── Scrape ──
    let concurrency = config::concurrency().min(40);
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let scrape_client = build_scrape_client();
    let scrape_max_bytes = config::max_html_bytes();
    let scrape_timeout = profile.scrape_timeout;
    let snippet_fallback = profile.snippet_fallback;

    let stage_start = std::time::Instant::now();
    let mut join_set = JoinSet::new();
    for c in ranked {
        let sem = semaphore.clone();
        let client = scrape_client.clone();
        join_set.spawn(async move {
            let engine_name = c.engines.join(",");

            // Engine-supplied text (OpenAlex / arXiv abstracts): no fetch.
            // Publisher landing pages are slow, often paywalled, often blocked.
            if let Some(ft) = c.full_text.as_ref().filter(|t| crate::util::char_len(t) >= 150) {
                return Some(SourceResult {
                    url: c.url.clone(),
                    title: c.title.clone(),
                    extracted_text: ft.clone(),
                    char_count: crate::util::char_len(ft),
                    engine: engine_name,
                    published: c.published,
                    language: None,
                    confidence: 0.9,
                    extractor: "abstract",
                });
            }

            if let Some(cached) = crate::cache_layer::get_page(&c.url).await {
                return Some(SourceResult {
                    url: c.url.clone(),
                    title: cached.title.clone(),
                    extracted_text: cached.text.clone(),
                    char_count: crate::util::char_len(&cached.text),
                    engine: engine_name,
                    published: cached.published.or(c.published),
                    language: cached.language.clone(),
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
                    Some(SourceResult {
                        char_count: crate::util::char_len(&e.text),
                        url: c.url,
                        title: if e.title.is_empty() { c.title } else { e.title },
                        extracted_text: e.text,
                        engine: engine_name,
                        published: e.published.or(c.published),
                        language: e.language,
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
                        confidence: 0.35,
                        extractor: "snippet",
                    })
                }
                None => None,
            }
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
        if raw_scraped_results.len() >= want_sources && scraped_chars >= want_chars {
            tracing::info!(
                "Scrape early-exit: {} sources / {} chars ({}/{} tasks)",
                raw_scraped_results.len(), scraped_chars, done, total_tasks
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
            Some(Ok(Some(r))) => {
                scraped_chars += r.char_count;
                raw_scraped_results.push(r);
                done += 1;
            }
            Some(Ok(None)) => done += 1,
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
        if let Ok(Some(r)) = j {
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
    emit(&progress, Progress::Extracted { sources: scraped_count, chars: scraped_chars, ms: scrape_ms }).await;

    // ── Rank chunks ──
    // Every mode takes the ranked path now. v8.0 Deep/Academic/Reddit/YouTube
    // skipped it: no RRF boost, no freshness, and BM25 scored against the
    // engine-syntax query (with `site:` and an injected year as terms).
    let stage_start = std::time::Instant::now();
    let results = ranking::rank_with_options(
        &query,
        &raw_scraped_results,
        profile.llm_top_k,
        Some(&rrf_scores),
        Some(&intel),
        &ranking::RankOptions::from_profile(&profile),
    );
    let rank_ms = stage_start.elapsed().as_millis();
    tracing::info!("⏱ Stage [rank]: {}ms — {} chunks from {} sources", rank_ms, results.len(), scraped_count);

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
            emit(&progress, Progress::Synthesizing { synthesis: format!("{:?}", profile.synthesis) }).await;
            synthesis::synthesize(&query, cfg, &results, &intel, &profile, temp_db, session_id.as_deref()).await
        }
        Some(_) => llm::LlmExecutionResult {
            llm_error: Some("llm_skipped: no scraped content available".to_string()),
            ..Default::default()
        },
        None => llm::LlmExecutionResult::default(),
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
        engine_dispatch_ms, scrape_ms, rank_ms, rrf_ms, llm_ms,
        total_raw, deduped_count, scraped_count, results.len(), profile.tier
    );

    let sources_processed = results.len();
    if let Some(hdb) = history_db {
        if hdb.is_enabled() {
            let entry = crate::cache::build_history_entry(
                &query,
                profile.tier,
                llm_result.llm_answer.as_deref(),
                sources_processed,
                total_raw,
                elapsed,
            );
            hdb.add_entry(entry).await;
        }
    }

    SearchResponse {
        query,
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
        copilot_query: copilot_out,
        llm_answer: llm_result.llm_answer,
        llm_error: llm_result.llm_error,
        citation_map: llm_result.citation_map,
        citation_checks: llm_result.citation_checks,
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
                    engines::with_page_budget(page_budget, engine.search(&fetcher, &query_variant)).await;

                // A 200 page of filler that ignores the query is a block, not
                // data: drop it (and never cache it). Mixed pages lose only
                // their filler.
                match engines::health::screen_serp(&ename, &query_variant, &mut results) {
                    engines::health::SerpVerdict::Clean => {}
                    engines::health::SerpVerdict::Trimmed(n) => {
                        tracing::info!("{}: dropped {} off-topic results", ename, n);
                    }
                    engines::health::SerpVerdict::Blocked => {
                        tracing::warn!("{}: results unrelated to the query — soft block, discarded", ename);
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
                crate::cache_layer::put_serp(&ename, &query_variant, page_budget, results.clone()).await;

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

    let sufficient = |all: &Vec<(usize, Vec<RawSearchResult>)>, relaxed: bool| -> Option<(usize, usize)> {
        let flat_len: usize = all.iter().map(|(_, v)| v.len()).sum();
        // Past the soft deadline, two thirds of the target count is enough.
        let need_results = if relaxed { min_results_to_exit.saturating_mul(2) / 3 } else { min_results_to_exit };
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
                n, f, tasks_completed, total_tasks, join_set.len()
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
    timeout: Duration,
    max_bytes: usize,
    mode: modes::Mode,
) -> Option<crate::extractor::Extracted> {
    // First attempt
    match scrape_url_inner(client, url, timeout, max_bytes, mode).await {
        ScrapeResult::Ok(e) => Some(e),
        ScrapeResult::Blocked => {
            // ── Retry with fresh browser profile after delay ──
            let delay = config::random_jitter_ms(200, 500);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            tracing::debug!("Retry after block: {}", url);
            match scrape_url_inner(client, url, timeout, max_bytes, mode).await {
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
    timeout: Duration,
    max_bytes: usize,
    mode: modes::Mode,
) -> ScrapeResult {
    // New Reddit is a JavaScript shell; old.reddit serves real HTML. Applied in
    // every mode, since Reddit threads surface in ordinary searches too.
    let target_url = reddit_to_old(url);

    // Accept-Encoding is NOT set by hand: reqwest advertises exactly what it
    // can decode (the Phase-1 zstd fix), and a manual header would override it.
    let mut req = client
        .get(&target_url)
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8");

    req = config::apply_browser_headers(req, &target_url);

    if !timeout.is_zero() {
        req = req.timeout(timeout);
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

    // Bot walls served with HTTP 200 (Reddit's "Welcome to Reddit" CAPTCHA,
    // Cloudflare interstitials). v8.0 only screened SERPs, so these pages were
    // extracted and handed to the LLM as sources.
    if crate::engines::health::looks_like_blocked_page(&html) {
        tracing::debug!("Blocked/interstitial page at {}", target_url);
        return ScrapeResult::Blocked;
    }

    // ── Single-pass DOM extraction: title + text in one parse ──
    // v5.0 fix: eliminates the double Html::parse_document() bottleneck
    if mode == modes::Mode::YouTube || target_url.contains("youtube.com/watch") {
        // YouTube fast path: try JSON-LD first (no DOM parse needed)
        if let Some(desc) = extract_youtube_json_ld(&html) {
            return ScrapeResult::Ok(crate::extractor::Extracted {
                title: extractor::extract_title_from_html(&html),
                text: desc,
                confidence: 0.6,
                source: "youtube",
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
                source: "youtube",
                ..Default::default()
            });
        }
    }

    // Normal pages: trafilatura + conditional Readability (v8.0 P1-4).
    // Extraction is CPU-bound (trafilatura ~40 ms/page, 140 ms on a large
    // Wikipedia article). Running it inline blocked tokio worker threads and
    // stalled every other in-flight fetch; the blocking pool is the right home.
    let page_url = target_url.clone();
    let extracted = match tokio::task::spawn_blocking(move || extractor::extract(&html, &page_url)).await {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("extraction task failed for {}: {}", target_url, e);
            return ScrapeResult::Skip;
        }
    };
    if !extractor::is_usable(&extracted) {
        return ScrapeResult::Skip;
    }
    ScrapeResult::Ok(extracted)
}

/// Rewrite any reddit.com URL to old.reddit.com.
///
/// v8.0 did `.replace("www.reddit.com", "old.reddit.com").replace("reddit.com",
/// "old.reddit.com")` — the second replace also matched inside the first's
/// output, turning www.reddit.com into `old.old.reddit.com`. Reddit mode never
/// fetched a valid thread URL.
pub fn reddit_to_old(url: &str) -> String {
    let Ok(mut u) = url::Url::parse(url) else { return url.to_string() };
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
        assert_eq!(reddit_to_old("https://reddit.com/r/x"), "https://old.reddit.com/r/x");
        assert_eq!(reddit_to_old("https://m.reddit.com/r/x"), "https://old.reddit.com/r/x");
        assert_eq!(reddit_to_old("https://old.reddit.com/r/x"), "https://old.reddit.com/r/x");
    }

    #[test]
    fn reddit_rewrite_leaves_other_hosts_and_media_alone() {
        assert_eq!(reddit_to_old("https://notreddit.com/r/x"), "https://notreddit.com/r/x");
        assert_eq!(reddit_to_old("https://i.reddit.com/x.png"), "https://i.reddit.com/x.png");
        assert_eq!(reddit_to_old("https://example.com/reddit.com/x"), "https://example.com/reddit.com/x");
        assert_eq!(reddit_to_old("not a url"), "not a url");
    }
}
