// ============================================================================
// SearchWala v8.0 — Evaluation harness runner
//
// WHY THIS EXISTS
// `src/eval.rs` is 474 lines of complete, correct IR evaluation code — nDCG@k,
// MRR, Recall@k, latency percentiles, per-engine success rates — and
// `benchmarks/golden_queries.json` holds 50 hand-labelled queries. In v7.0
// NEITHER was referenced anywhere in the codebase, and the project had zero
// tests. "Better results" was unmeasurable.
//
// Usage:
//   cargo run --release --bin eval                    # full golden set
//   cargo run --release --bin eval -- --limit 10      # quick pass
//   cargo run --release --bin eval -- --baseline b.json   # fail on regression
//   cargo run --release --bin eval -- --save b.json       # record a baseline
//
// Exit code 1 on a statistically meaningful nDCG@10 regression, so this can
// gate CI.
// ============================================================================

use std::collections::HashMap;
use std::time::Instant;

use searchwala::eval::{
    self, CacheMetrics, EngineHealth as EvalEngineHealth, PipelineLatency,
};

#[derive(Default)]
struct Args {
    limit: Option<usize>,
    baseline: Option<String>,
    save: Option<String>,
    focus: String,
    /// None = let the mode decide (the realistic comparison).
    max_results: Option<usize>,
    json: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        focus: "auto".to_string(),
        max_results: None,
        ..Default::default()
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--limit" => {
                a.limit = argv.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "--baseline" => {
                a.baseline = argv.get(i + 1).cloned();
                i += 2;
            }
            "--save" => {
                a.save = argv.get(i + 1).cloned();
                i += 2;
            }
            "--focus" => {
                if let Some(v) = argv.get(i + 1) {
                    a.focus = v.clone();
                }
                i += 2;
            }
            "--max-results" => {
                a.max_results = argv.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "--json" => {
                a.json = true;
                i += 1;
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    a
}

/// Tolerance for nDCG regression. Live search against real engines is not
/// deterministic — engines reorder, rate-limit and go down — so a hard equality
/// gate would fail constantly and teach everyone to ignore it.
const NDCG_REGRESSION_TOLERANCE: f64 = 0.03;

#[tokio::main]
async fn main() {
    let args = parse_args();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "searchwala=warn".into()),
        )
        .with_target(false)
        .compact()
        .init();

    let path = "benchmarks/golden_queries.json";
    let golden = match eval::load_golden_queries(path) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("Failed to load {path}: {e}");
            std::process::exit(2);
        }
    };

    let total = args.limit.unwrap_or(golden.queries.len()).min(golden.queries.len());
    eprintln!(
        "Evaluating {total} of {} golden queries (focus={}, max_results={:?})",
        golden.queries.len(),
        args.focus,
        args.max_results
    );

    let mut latency = PipelineLatency::default();
    let mut ranking_results: Vec<(String, Vec<String>)> = Vec::new();
    let mut confidences: Vec<f64> = Vec::new();
    let mut intent_correct = 0usize;
    let mut intent_total = 0usize;

    for (i, gq) in golden.queries.iter().take(total).enumerate() {
        let t0 = Instant::now();
        let resp = searchwala::search::execute_search(
            &gq.query,
            args.max_results,
            Some(args.focus.clone()),
            None, // no LLM — we are measuring RETRIEVAL and RANKING
            None,
            None,
        )
        .await;
        let elapsed = t0.elapsed();
            latency.total.record(elapsed);

        // Ranked URL list, in the order the pipeline chose to show them.
        let urls: Vec<String> = resp.search_results.iter().map(|h| h.url.clone()).collect();

        let ndcg = eval::ndcg_at_k(&urls, &gq.relevance_judgments, 10);
        let rr = eval::reciprocal_rank(&urls, &gq.relevance_judgments);

        // Intent classification accuracy — cheap and worth tracking.
        if !gq.expected_intent.is_empty() {
            intent_total += 1;
            let got = searchwala::query_intel::analyze_query(&gq.query).intent.to_string();
            if got == gq.expected_intent {
                intent_correct += 1;
            }
        }

        for r in &resp.results {
            if r.confidence > 0.0 {
                confidences.push(r.confidence);
            }
        }

        eprintln!(
            "  [{:>3}/{}] nDCG@10={:.3} RR={:.3} urls={:<3} raw={:<4} {:.2}s  {}",
            i + 1,
            total,
            ndcg,
            rr,
            urls.len(),
            resp.sources_found,
            elapsed.as_secs_f64(),
            searchwala::util::truncate_chars(&gq.query, 44)
        );

        ranking_results.push((gq.query.clone(), urls));
    }

    let avg_conf = if confidences.is_empty() {
        0.0
    } else {
        confidences.iter().sum::<f64>() / confidences.len() as f64
    };

    // Real engine health from the live registry, mapped into eval's shape.
    let mut engine_health: HashMap<String, EvalEngineHealth> = HashMap::new();
    for r in searchwala::engines::health::snapshot() {
        engine_health.insert(
            r.engine.clone(),
            EvalEngineHealth {
                total_queries: r.attempts,
                successful_queries: r.http_ok,
                failed_queries: r.blocked + r.transport_err,
                total_results_returned: r.results_total,
                avg_response_ms: 0.0,
                results_in_final_answer: 0,
            },
        );
    }

    // Real cache metrics — eval.rs has defined these ratios since v6.1 and
    // nothing populated them.
    let cs = searchwala::cache_layer::stats();
    let cache_metrics = CacheMetrics {
        engine_cache_hits: cs.serp_hits,
        engine_cache_misses: cs.serp_misses,
        scrape_cache_hits: cs.page_hits,
        scrape_cache_misses: cs.page_misses,
        answer_cache_hits: 0,
        answer_cache_misses: 0,
    };

    let report = eval::build_eval_report(
        &latency,
        engine_health,
        cache_metrics,
        &ranking_results,
        &golden,
        avg_conf,
    );

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    } else {
        println!("\n══════════ EVALUATION REPORT ══════════");
        println!("  version            {}", report.version);
        println!("  queries            {}", report.queries_evaluated);
        println!("  ── ranking quality ──");
        println!("  nDCG@10            {:.4}", report.mean_ndcg_at_10);
        println!("  MRR                {:.4}", report.mrr);
        println!("  Recall@10          {:.4}", report.mean_recall_at_10);
        println!("  Recall@20          {:.4}", report.mean_recall_at_20);
        println!("  ── latency ──");
        println!("  p50 / p95 / p99    {:.0} / {:.0} / {:.0} ms",
                 report.latency_p50_ms, report.latency_p95_ms, report.latency_p99_ms);
        println!("  ── extraction ──");
        println!("  avg confidence     {:.3}", avg_conf);
        if intent_total > 0 {
            println!("  ── query intelligence ──");
            println!("  intent accuracy    {:.1}% ({}/{})",
                     intent_correct as f64 / intent_total as f64 * 100.0,
                     intent_correct, intent_total);
        }
        println!("  ── cache ──");
        println!("  serp / page hit     {:.0}% / {:.0}%",
                 cs.serp_hit_ratio * 100.0, cs.page_hit_ratio * 100.0);
    }

    if let Some(p) = &args.save {
        match serde_json::to_string_pretty(&report) {
            Ok(j) => match std::fs::write(p, j) {
                Ok(()) => eprintln!("\nBaseline written to {p}"),
                Err(e) => eprintln!("\nFailed to write {p}: {e}"),
            },
            Err(e) => eprintln!("\nFailed to serialize report: {e}"),
        }
    }

    if let Some(p) = &args.baseline {
        let prev: Result<eval::EvalReport, String> = std::fs::read_to_string(p)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()));
        match prev {
            Ok(base) => {
                let delta = report.mean_ndcg_at_10 - base.mean_ndcg_at_10;
                println!(
                    "\n  baseline nDCG@10   {:.4}  ->  {:.4}  ({:+.4})",
                    base.mean_ndcg_at_10, report.mean_ndcg_at_10, delta
                );
                if delta < -NDCG_REGRESSION_TOLERANCE {
                    eprintln!(
                        "\nREGRESSION: nDCG@10 dropped {:.4}, beyond the {:.2} tolerance.",
                        -delta, NDCG_REGRESSION_TOLERANCE
                    );
                    std::process::exit(1);
                }
                println!("  no regression beyond the {NDCG_REGRESSION_TOLERANCE:.2} tolerance.");
            }
            Err(e) => eprintln!("\nCould not read baseline {p}: {e} (skipping gate)"),
        }
    }
}
