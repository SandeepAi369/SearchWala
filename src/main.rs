// ============================================================================
// SearchWala v7.0.0
// ============================================================================
//
// Native Rust meta-search + extraction + optional BYOK LLM synthesis.
// v7.0.0: Research-grounded architecture upgrade
// Iterative Deep Research | Dual Database | Time-Aware LLM
//
// Pipeline:
// 1) Analyze query intent (factual/temporal/person/comparison/howto)
// 2) Query 90+ search engines concurrently
// 3) Reciprocal Rank Fusion (cross-engine consensus ranking)
// 4) Concurrently scrape + extract readable text (7-tier extractor)
// 5) Hybrid BM25+RRF chunk ranking
// 6) Intent-aware LLM synthesis (multi-batch for research)
//
// ============================================================================

use searchwala::*;

use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::{Json, Query},
    http::StatusCode,
    response::{Html, IntoResponse, sse::{Event, Sse}},
    routing::{get, post},
    Router,
};
use tower_http::cors::CorsLayer;

use models::*;

struct AppState {
    start_time: Instant,
    temp_db: cache::TempDb,
    history_db: cache::HistoryDb,
}

const SEARCH_UI: &str = include_str!("../ui.html");


fn params_from(body: SearchRequest, focus_override: Option<&str>, default_llm_urls: Option<usize>) -> search::SearchParams {
    search::SearchParams {
        query: body.query.trim().to_string(),
        max_results: body.max_results.or(default_llm_urls),
        focus_mode: focus_override.map(str::to_string).or(body.focus_mode),
        domain: body.domain,
        llm: body.llm,
        enable_copilot: body.enable_copilot.unwrap_or(false),
    }
}

fn empty_query_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": "Query cannot be empty" })),
    )
}

fn no_results_error(resp: &SearchResponse) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": "No results found. Search engines may be unreachable.",
            "query": resp.query,
            "mode": resp.mode,
            "engines_queried": resp.engine_stats.engines_queried,
        })),
    )
}

/// POST /search — the main endpoint. Mode via `focus_mode`, subject area via
/// `domain`; synthesis runs when `llm` is supplied.
async fn search_handler(
    state: axum::extract::State<Arc<AppState>>,
    Json(body): Json<SearchRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if body.query.trim().is_empty() {
        return Err(empty_query_error());
    }
    let params = params_from(body, None, None);
    let response = search::execute_search_full(
        params,
        Some(&state.temp_db),
        Some(&state.history_db),
        None,
    )
    .await;
    if response.sources_processed == 0 {
        return Err(no_results_error(&response));
    }
    Ok(Json(response))
}

/// POST /search/auto-llm — kept for API compatibility. Requires `llm`; the
/// mode defaults to auto but `focus_mode` is honoured.
async fn search_auto_llm_handler(
    state: axum::extract::State<Arc<AppState>>,
    Json(body): Json<SearchRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if body.query.trim().is_empty() {
        return Err(empty_query_error());
    }
    if body.llm.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "LLM config is required for /search/auto-llm" })),
        ));
    }
    let params = params_from(body, None, None);
    let response =
        search::execute_search_full(params, Some(&state.temp_db), Some(&state.history_db), None).await;
    if response.sources_processed == 0 {
        return Err(no_results_error(&response));
    }
    Ok(Json(response))
}

/// POST /search/research-llm — kept for API compatibility; forces Deep mode.
/// v8.0 also dropped the request's domain here.
async fn search_research_llm_handler(
    state: axum::extract::State<Arc<AppState>>,
    Json(body): Json<SearchRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if body.query.trim().is_empty() {
        return Err(empty_query_error());
    }
    if body.llm.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "LLM config is required for /search/research-llm" })),
        ));
    }
    let params = params_from(body, Some("deep"), None);
    let response =
        search::execute_search_full(params, Some(&state.temp_db), Some(&state.history_db), None).await;
    if response.sources_processed == 0 {
        return Err(no_results_error(&response));
    }
    Ok(Json(response))
}

/// POST /search/stream — Server-Sent Events.
///
/// v8.0 ran a SEPARATE, stale copy of the pipeline here (`stream.rs`): no
/// query intelligence, no RRF, `lite` as its only recognised mode, the full
/// 90-name legacy engine list, and no deadline. Every mode therefore behaved
/// differently on this endpoint. It now runs the same pipeline and forwards
/// its progress events, then the final response:
///
///   data: {"type":"plan", ...}
///   data: {"type":"engines", ...}
///   data: {"type":"sources", ...}
///   data: {"type":"extracted", ...}
///   data: {"type":"synthesizing", ...}
///   data: {"type":"result", "response": { ...same shape as POST /search... }}
///   data: {"type":"done"}
///
/// If the client disconnects, the pipeline task is aborted rather than left
/// scraping for a response nobody will read.
async fn stream_handler(
    state: axum::extract::State<Arc<AppState>>,
    Json(body): Json<SearchRequest>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
    let params = params_from(body, None, None);
    let state = state.0.clone();

    tokio::spawn(async move {
        let (ptx, mut prx) = tokio::sync::mpsc::channel::<search::Progress>(32);
        let pipeline = {
            let state = state.clone();
            tokio::spawn(async move {
                search::execute_search_full(params, Some(&state.temp_db), Some(&state.history_db), Some(ptx)).await
            })
        };

        while let Some(p) = prx.recv().await {
            let data = serde_json::to_string(&p).unwrap_or_default();
            if tx.send(Ok(Event::default().data(data))).await.is_err() {
                tracing::info!("stream client disconnected; aborting pipeline");
                pipeline.abort();
                return;
            }
        }

        let final_event = match pipeline.await {
            Ok(resp) => serde_json::json!({ "type": "result", "response": resp }),
            Err(e) => serde_json::json!({ "type": "error", "error": e.to_string() }),
        };
        let _ = tx.send(Ok(Event::default().data(final_event.to_string()))).await;
        let _ = tx
            .send(Ok(Event::default().data(serde_json::json!({ "type": "done" }).to_string())))
            .await;
    });

    Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
        .keep_alive(axum::response::sse::KeepAlive::new())
}

// =============================================================================
// History API
// =============================================================================

/// POST /api/history/enable - Enable history DB and load from disk
async fn history_enable_handler(
    state: axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    match state.history_db.enable().await {
        Ok(count) => Json(serde_json::json!({
            "status": "enabled",
            "entries_loaded": count
        })),
        Err(err) => Json(serde_json::json!({
            "status": "error",
            "error": err
        })),
    }
}

/// POST /api/history/disable - Disable history DB
async fn history_disable_handler(
    state: axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    state.history_db.disable();
    Json(serde_json::json!({ "status": "disabled" }))
}

/// GET /api/history - Get all history entries
async fn history_get_handler(
    state: axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    let entries = state.history_db.get_all().await;
    Json(serde_json::json!({
        "enabled": state.history_db.is_enabled(),
        "count": entries.len(),
        "entries": entries
    }))
}

/// DELETE /api/history - Clear all history
async fn history_clear_handler(
    state: axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    match state.history_db.clear().await {
        Ok(()) => Json(serde_json::json!({ "status": "cleared" })),
        Err(err) => Json(serde_json::json!({ "status": "error", "error": err })),
    }
}

/// GET /api/history/status - Check history DB status
async fn history_status_handler(
    state: axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    Json(serde_json::json!({
        "enabled": state.history_db.is_enabled(),
        "count": state.history_db.count().await,
        "temp_sessions": state.temp_db.active_count().await
    }))
}

// =============================================================================
// Existing Endpoints
// =============================================================================

/// GET /health - Health check
async fn health_handler(state: axum::extract::State<Arc<AppState>>) -> impl IntoResponse {
    let uptime = state.start_time.elapsed().as_secs();
    Json(HealthResponse {
        status: "ok".to_string(),
        version: crate::config::VERSION.to_string(),
        engines: modes::all_roster_engines(),
        uptime_seconds: uptime,
    })
}

/// GET /config - Configuration info
async fn config_handler() -> impl IntoResponse {
    Json(ConfigResponse {
        version: crate::config::VERSION.to_string(),
        engines: modes::all_roster_engines(),
        modes: modes::describe_modes(),
        concurrent_scrapes: config::concurrency(),
        concurrent_engines: config::engine_concurrency(),
        jitter_min_ms: config::jitter_min_ms(),
        jitter_max_ms: config::jitter_max_ms(),
        proxy_cooldown_secs: config::proxy_cooldown_secs(),
        user_agents_count: config::user_agents_count(),
    })
}

/// GET / - Root endpoint
///
/// `no-store`: the page is compiled into the binary, so after an upgrade a
/// cached copy would be a different UI talking to a newer API.
async fn root_handler() -> impl IntoResponse {
    ([(axum::http::header::CACHE_CONTROL, "no-store")], Html(SEARCH_UI))
}

/// POST /api/models - Dynamic model fetcher
async fn models_handler(
    Json(body): Json<serde_json::Value>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let api_key = body.get("api_key").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let base_url = body.get("base_url").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let provider = body.get("provider").and_then(|v| v.as_str()).unwrap_or("").to_string();

    if base_url.trim().is_empty() && provider.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "base_url or provider is required" })),
        ));
    }

    // v8.0 P1-12a: this endpoint takes a caller-supplied base_url and fetches
    // it with the user's credentials. Without this check, any page in the
    // user's browser could use it to read loopback services and cloud metadata.
    // (An empty base_url means the provider's built-in endpoint.)
    if let Err(e) = Some(base_url.trim()).filter(|b| !b.is_empty()).map_or(Ok(()), |b| ssrf::validate_llm_base_url(b).map(|_| ())) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("rejected base_url: {e}") })),
        ));
    }

    match llm::fetch_provider_models(&api_key, &base_url, &provider).await {
        Ok(models) => Ok(Json(serde_json::json!({ "models": models }))),
        Err(err) => Err((
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": err })),
        )),
    }
}

/// GET /health/engines — per-engine liveness, the metric v7.0 lacked.
///
/// `zero_results` is the important one: HTTP 200 with nothing parsed means our
/// selectors have rotted, which is otherwise indistinguishable from "no
/// results for this query". That blind spot is why 28 of 35 engines shipped
/// broken in v7.0.
///
/// Every engine a mode can use is listed, even before its first search: an
/// empty `{"engines":[],"summary":{}}` after a restart looked like a broken
/// server rather than "no searches yet".
async fn engine_health_handler() -> impl IntoResponse {
    engines::health::register_known(&modes::all_roster_engines());
    let snap = engines::health::snapshot();
    let mut by_verdict: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for r in &snap {
        *by_verdict.entry(r.verdict).or_insert(0) += 1;
    }
    let searched = snap.iter().any(|r| r.attempts > 0);
    Json(serde_json::json!({
        "note": if searched {
            "Counters since server start. 'unproven' = fewer than 8 attempts so far."
        } else {
            "No searches since server start; counters fill in as searches run."
        },
        "summary": by_verdict,
        "engines": snap,
    }))
}

/// GET /health/cache — SERP and page cache hit ratios.
///
/// `eval.rs` has defined engine_hit_ratio / scrape_hit_ratio / answer_hit_ratio
/// since v6.1 and nothing populated them; this is the first real data.
async fn cache_stats_handler() -> impl IntoResponse {
    Json(cache_layer::stats())
}

/// GET /about - Service metadata
async fn about_handler() -> impl IntoResponse {
    Json(serde_json::json!({
        "name": "SearchWala",
        "version": crate::config::VERSION,
        "language": "Rust",
        "description": "Ultra-fast native meta-search & scrape API with iterative deep research LLM synthesis",
        "features": [
            "90+ search engines with smart fallback",
            "Iterative multi-batch deep research",
            "Specialized domain modes (Tech/Science/Finance/Health/News)",
            "Time-aware LLM with chrono injection",
            "Dual database (TempDb + HistoryDb)",
            "Smart engine tiering — primary + backup resilience",
            "Dynamic model fetching",
            "OpenAI-compatible provider support"
        ],
        "endpoints": {
            "POST /search": "Search and scrape",
            "POST /search/lite-llm": "Lite mode LLM synthesis",
            "POST /search/research-llm": "Iterative deep research",
            "POST /search/stream": "SSE streaming",
            "POST /api/models": "Dynamic model fetcher",
            "GET /api/history": "Get search history",
            "POST /api/history/enable": "Enable history DB",
            "DELETE /api/history": "Clear history",
            "GET /health": "Health check"
        }
    }))
}

// =============================================================================
// TTS — Microsoft Edge Neural Voice (en-US-AvaNeural, same as XeL Studio)
// =============================================================================

#[derive(serde::Deserialize)]
struct TtsQuery {
    text: String,
}

/// GET /api/tts?text=Hello+world → audio/mpeg
/// Removes a temp file when dropped, on every return path.
struct TempFileGuard(std::path::PathBuf);
impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Cap concurrent `edge-tts` subprocesses. v7.0 spawned one process per
/// unauthenticated GET with no limit — a trivial fork bomb given that the
/// server binds 0.0.0.0 with permissive CORS.
static TTS_GATE: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(2));

async fn tts_handler(
    Query(params): Query<TtsQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _permit = TTS_GATE
        .try_acquire()
        .map_err(|_| (StatusCode::TOO_MANY_REQUESTS, "TTS busy, retry shortly".to_string()))?;

    let text = params.text.trim().to_string();
    if text.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Missing 'text' parameter".into()));
    }

    // Cap at 5000 CHARACTERS. v7.0 gated on text.len() (bytes) then took 5000
    // chars, so Devanagari/CJK input was measured in one unit and cut in
    // another.
    let capped = crate::util::truncate_chars(&text, 5000).to_string();

    // Aggressive TTS cleaning — strip ALL non-speech artifacts
    let cleaned: String = capped
        // Strip source references: [1], [2,3], etc.
        .replace("**", "")
        .replace("##", "")
        .replace("###", "")
        .replace('`', "")
        .replace("\n\n", ". ")
        .replace('\n', " ");

    // Use regex-like manual stripping for brackets/refs
    let mut result = String::with_capacity(cleaned.len());
    let mut chars = cleaned.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '[' {
            // Skip [N] or [N,M] references
            let mut buf = String::new();
            let mut is_ref = true;
            while let Some(&next) = chars.peek() {
                if next == ']' { chars.next(); break; }
                if !next.is_ascii_digit() && next != ',' && next != ' ' { is_ref = false; }
                buf.push(next);
                chars.next();
            }
            if !is_ref { result.push_str(&buf); } // Keep non-numeric bracket content
        } else {
            result.push(ch);
        }
    }

    // Final cleanup
    let final_text: String = result
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ");

    // Use the OS temp dir (not a hardcoded /tmp) and guarantee cleanup even on
    // the error paths — v7.0 returned early on a non-zero exit status BEFORE
    // remove_file, leaking an mp3 per failed request.
    let tmp_path = std::env::temp_dir().join(format!(
        "searchwala_tts_{}_{}.mp3",
        std::process::id(),
        rand::random::<u32>()
    ));
    let _tmp_guard = TempFileGuard(tmp_path.clone());

    // v7.0 hardcoded /home/sandeep/.local/bin/edge-tts, which exists for
    // exactly one user. Resolve from env or $PATH only.
    let edge_tts_path = std::env::var("EDGE_TTS_PATH").unwrap_or_else(|_| {
        for path in &["/usr/local/bin/edge-tts", "/usr/bin/edge-tts"] {
            if std::path::Path::new(path).exists() {
                return path.to_string();
            }
        }
        "edge-tts".to_string()
    });

    let output = tokio::process::Command::new(&edge_tts_path)
        .arg("--text")
        .arg(&final_text)
        .arg("--voice")
        .arg("en-US-AvaNeural")
        .arg("--rate")
        .arg("+15%")
        .arg("--write-media")
        .arg(&tmp_path)
        .output()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("edge-tts not found: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("edge-tts error: {stderr}")));
    }

    let audio_bytes = tokio::fs::read(&tmp_path).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("read error: {e}")))?;
    // cleanup handled by _tmp_guard on drop

    Ok((
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "audio/mpeg"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        audio_bytes,
    ))
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "searchwala=info,tower_http=info".into()),
        )
        .with_target(false)
        .compact()
        .init();

    let port = config::port();
    let engines = modes::all_roster_engines();

    tracing::info!("============================================");
    tracing::info!("  SearchWala v{}", crate::config::VERSION);
    tracing::info!("  Language: Rust");
    tracing::info!("  Engines: {} total", engines.len());
    tracing::info!("  Modes: fast, auto, deep, study, academic, reddit, youtube (+ domains)");
    tracing::info!("  Concurrency: {}", config::concurrency());

    tracing::info!("  Features: Iterative Research, TempDb, HistoryDb");
    tracing::info!("  CORS: permissive");
    tracing::info!("  Port: {}", port);
    tracing::info!("============================================");

    // Initialize dual database system
    let temp_db = cache::TempDb::new();
    let history_db = cache::HistoryDb::new();

    // Spawn background cleanup for expired temp sessions
    cache::spawn_temp_db_cleaner(temp_db.clone());

    let state = Arc::new(AppState {
        start_time: Instant::now(),
        temp_db,
        history_db,
    });

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/index.html", get(root_handler))
        .route("/ui", get(root_handler))
        .route("/about", get(about_handler))
        .route("/health", get(health_handler))
        .route("/config", get(config_handler))
        .route("/search", get(root_handler).post(search_handler))
        .route("/search/auto-llm", post(search_auto_llm_handler))
        .route("/search/lite-llm", post(search_auto_llm_handler))  // backward compat
        .route("/search/research-llm", post(search_research_llm_handler))
        .route("/search/stream", post(stream_handler))
        .route("/api/models", post(models_handler))
        .route("/api/history", get(history_get_handler).delete(history_clear_handler))
        .route("/api/history/enable", post(history_enable_handler))
        .route("/api/history/disable", post(history_disable_handler))
        .route("/api/history/status", get(history_status_handler))
        .route("/api/tts", get(tts_handler))
        .route("/health/engines", get(engine_health_handler))
        .route("/health/cache", get(cache_stats_handler))
        // v8.0: a handler panic used to abort the task with no response, so the
        // client saw a bare connection reset. The UTF-8 byte-slice bugs in
        // search.rs/llm.rs did exactly that on Hindi queries.
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        .layer(tower_http::limit::RequestBodyLimitLayer::new(256 * 1024))
        .layer(CorsLayer::permissive())
        .with_state(state);

    // v8.0: default to loopback. This process relays the user's BYOK LLM API
    // key to a caller-supplied base_url and runs an unauthenticated
    // subprocess endpoint; binding every interface by default made both
    // reachable from the local network. Set BIND=0.0.0.0 to expose it.
    let bind_host = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1".to_string());
    let addr = format!("{}:{}", bind_host, port);
    tracing::info!("Listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");

    axum::serve(listener, app).await.expect("Server error");
}
