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
    /// None when the database could not be opened: search still works,
    /// nothing is saved.
    chats: Option<chats::ChatStore>,
    /// Pipelines of saved turns still running, so Stop can cancel them.
    running: std::sync::Mutex<std::collections::HashMap<i64, tokio::task::AbortHandle>>,
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
        memory: Vec::new(),
    }
}

// =============================================================================
// Conversations: every turn is saved around the pipeline
// =============================================================================

/// Requests from the SearchWala page itself: no Origin header (same-origin
/// GET) or an Origin naming this host, and — while bound to loopback — a
/// loopback Host, which also defeats DNS rebinding. CORS is permissive for
/// the search API, so without this any website could read saved
/// conversations from http://localhost:8000.
fn from_own_page(headers: &axum::http::HeaderMap) -> bool {
    let host = headers.get(axum::http::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let loopback_bind = matches!(bind.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]");
    let host_name = host.rsplit_once(':').map_or(host, |(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { host });
    if loopback_bind && !matches!(host_name, "127.0.0.1" | "localhost" | "[::1]") {
        return false;
    }
    match headers.get(axum::http::header::ORIGIN).and_then(|o| o.to_str().ok()) {
        None => true,
        Some(origin) => origin.split_once("://").is_some_and(|(_, rest)| rest == host),
    }
}

fn forbidden() -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": "conversations are only available to the SearchWala page on this machine" })))
}

/// Saves the question (when the request opts in with `save: true` and saving
/// is on) and loads the conversation's earlier turns as memory (when the
/// request asks for them and has an LLM). Scripts and tools calling the API
/// without `save` leave nothing on disk.
async fn begin_turn(state: &AppState, body: &SearchRequest, trusted: bool, params: &mut search::SearchParams) -> Option<(String, i64)> {
    let store = state.chats.as_ref().filter(|_| trusted)?;
    let mode = body.focus_mode.clone().unwrap_or_else(|| "auto".into());
    let turn = if body.save == Some(true) && store.saving_enabled().await {
        match store.begin_turn(body.conversation_id.clone(), &params.query, &mode).await {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!("chats: could not save the question: {e}");
                None
            }
        }
    } else {
        None
    };
    let wanted = body.memory_turns.unwrap_or(0);
    let conversation = turn.as_ref().map(|(c, _)| c.clone()).or_else(|| body.conversation_id.clone());
    if let (true, true, Some(conv)) = (wanted > 0, params.llm.is_some(), conversation) {
        let before = turn.as_ref().map_or(i64::MAX, |(_, t)| *t);
        params.memory = store.prior_turns(&conv, before, wanted).await.unwrap_or_default();
    }
    turn
}

/// Completes the saved turn with the answer and what the UI rendered.
async fn finish_turn(state: &AppState, turn: &Option<(String, i64)>, response: &mut SearchResponse) {
    let (Some(store), Some((conv, id))) = (state.chats.as_ref(), turn) else { return };
    response.conversation_id = Some(conv.clone());
    response.turn_id = Some(*id);
    let status = if response.llm_answer.is_some() || response.sources_processed > 0 { "done" } else { "failed" };
    let json = serde_json::to_value(&*response).ok();
    if let Err(e) = store.finish_turn(*id, status, response.search_query.clone(), response.llm_answer.clone(), json).await {
        tracing::warn!("chats: could not save the answer: {e}");
    }
}

/// The non-streaming endpoints: save around one pipeline run.
async fn run_saved(state: &AppState, body: SearchRequest, headers: &axum::http::HeaderMap, focus_override: Option<&str>) -> SearchResponse {
    let mut params = params_from(body.clone(), focus_override, None);
    let turn = begin_turn(state, &body, from_own_page(headers), &mut params).await;
    let mut response = search::execute_search_full(params, Some(&state.temp_db), None).await;
    finish_turn(state, &turn, &mut response).await;
    response
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
    headers: axum::http::HeaderMap,
    Json(body): Json<SearchRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if body.query.trim().is_empty() {
        return Err(empty_query_error());
    }
    let response = run_saved(&state, body, &headers, None).await;
    if response.sources_processed == 0 {
        return Err(no_results_error(&response));
    }
    Ok(Json(response))
}

/// POST /search/auto-llm — kept for API compatibility. Requires `llm`; the
/// mode defaults to auto but `focus_mode` is honoured.
async fn search_auto_llm_handler(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
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
    let response = run_saved(&state, body, &headers, None).await;
    if response.sources_processed == 0 {
        return Err(no_results_error(&response));
    }
    Ok(Json(response))
}

/// POST /search/research-llm — kept for API compatibility; forces Deep mode.
/// v8.0 also dropped the request's domain here.
async fn search_research_llm_handler(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
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
    let response = run_saved(&state, body, &headers, Some("deep")).await;
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
/// A saved turn first announces itself:
///
///   data: {"type":"turn", "conversation_id": "...", "turn_id": 42}
///
/// If the client disconnects, a saved turn keeps running and saves its answer
/// (closing the tab must not lose the research; reopening the conversation
/// shows it). Only Stop (POST /api/chats/turns/{id}/stop) cancels it. An
/// unsaved search is aborted rather than left scraping for nobody.
async fn stream_handler(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SearchRequest>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
    let mut params = params_from(body.clone(), None, None);
    let state = state.0.clone();
    let turn = begin_turn(&state, &body, from_own_page(&headers), &mut params).await;

    tokio::spawn(async move {
        let (ptx, mut prx) = tokio::sync::mpsc::channel::<search::Progress>(32);
        let pipeline = {
            let state = state.clone();
            tokio::spawn(async move { search::execute_search_full(params, Some(&state.temp_db), Some(ptx)).await })
        };
        if let Some((conv, id)) = &turn {
            state.running.lock().unwrap_or_else(|p| p.into_inner()).insert(*id, pipeline.abort_handle());
            let ev = serde_json::json!({ "type": "turn", "conversation_id": conv, "turn_id": id });
            let _ = tx.send(Ok(Event::default().data(ev.to_string()))).await;
        }

        let mut client_gone = false;
        while let Some(p) = prx.recv().await {
            if client_gone {
                continue; // keep draining so the pipeline never blocks
            }
            let data = serde_json::to_string(&p).unwrap_or_default();
            if tx.send(Ok(Event::default().data(data))).await.is_err() {
                client_gone = true;
                if turn.is_none() {
                    tracing::info!("stream client disconnected; aborting pipeline");
                    pipeline.abort();
                    return;
                }
                tracing::info!("stream client disconnected; finishing the saved turn anyway");
            }
        }

        let outcome = pipeline.await;
        if let Some((_, id)) = &turn {
            state.running.lock().unwrap_or_else(|p| p.into_inner()).remove(id);
        }
        let final_event = match outcome {
            Ok(mut resp) => {
                finish_turn(&state, &turn, &mut resp).await;
                serde_json::json!({ "type": "result", "response": resp })
            }
            Err(e) => {
                if let (Some(store), Some((_, id))) = (state.chats.as_ref(), &turn) {
                    let status = if e.is_cancelled() { "stopped" } else { "failed" };
                    let _ = store.finish_turn(*id, status, None, None, None).await;
                }
                serde_json::json!({ "type": "error", "error": e.to_string() })
            }
        };
        if client_gone {
            return;
        }
        let _ = tx.send(Ok(Event::default().data(final_event.to_string()))).await;
        let _ = tx
            .send(Ok(Event::default().data(serde_json::json!({ "type": "done" }).to_string())))
            .await;
    });

    Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
        .keep_alive(axum::response::sse::KeepAlive::new())
}

// =============================================================================
// Conversations API (only for the SearchWala page; see `from_own_page`)
// =============================================================================

type ApiResult = Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)>;

fn store_for<'a>(state: &'a AppState, headers: &axum::http::HeaderMap) -> Result<&'a chats::ChatStore, (StatusCode, Json<serde_json::Value>)> {
    if !from_own_page(headers) {
        return Err(forbidden());
    }
    state.chats.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": "the conversation database could not be opened; see the server log" })))
    })
}

fn db_error(e: String) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e })))
}

fn not_found() -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "no such conversation" })))
}

#[derive(serde::Deserialize)]
struct ListQuery {
    q: Option<String>,
    limit: Option<usize>,
}

/// GET /api/chats?q=&limit= — conversations, most recent first.
async fn chats_list_handler(state: axum::extract::State<Arc<AppState>>, headers: axum::http::HeaderMap, Query(q): Query<ListQuery>) -> ApiResult {
    let store = store_for(&state, &headers)?;
    let list = store.list(q.q, q.limit.unwrap_or(500)).await.map_err(db_error)?;
    Ok(Json(serde_json::json!({ "saving": store.saving_enabled().await, "conversations": list })))
}

/// GET /api/chats/{id} — every turn, as it was rendered.
async fn chat_get_handler(state: axum::extract::State<Arc<AppState>>, headers: axum::http::HeaderMap, axum::extract::Path(id): axum::extract::Path<String>) -> ApiResult {
    let store = store_for(&state, &headers)?;
    let conv = store.get(&id).await.map_err(db_error)?.ok_or_else(not_found)?;
    Ok(Json(serde_json::to_value(conv).unwrap_or_default()))
}

#[derive(serde::Deserialize)]
struct RenameBody {
    title: String,
}

/// PATCH /api/chats/{id} {"title": "..."}
async fn chat_rename_handler(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<RenameBody>,
) -> ApiResult {
    let store = store_for(&state, &headers)?;
    if !store.rename(&id, &body.title).await.map_err(db_error)? {
        return Err(not_found());
    }
    Ok(Json(serde_json::json!({ "renamed": true })))
}

/// DELETE /api/chats/{id}
async fn chat_delete_handler(state: axum::extract::State<Arc<AppState>>, headers: axum::http::HeaderMap, axum::extract::Path(id): axum::extract::Path<String>) -> ApiResult {
    let store = store_for(&state, &headers)?;
    if !store.delete(&id).await.map_err(db_error)? {
        return Err(not_found());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// DELETE /api/chats — every conversation.
async fn chats_delete_all_handler(state: axum::extract::State<Arc<AppState>>, headers: axum::http::HeaderMap) -> ApiResult {
    let store = store_for(&state, &headers)?;
    let n = store.delete_all().await.map_err(db_error)?;
    Ok(Json(serde_json::json!({ "deleted": n })))
}

#[derive(serde::Deserialize)]
struct SavingBody {
    saving: bool,
}

/// GET /api/chats/settings
async fn chats_settings_get_handler(state: axum::extract::State<Arc<AppState>>, headers: axum::http::HeaderMap) -> ApiResult {
    let store = store_for(&state, &headers)?;
    Ok(Json(serde_json::json!({ "saving": store.saving_enabled().await })))
}

/// PUT /api/chats/settings {"saving": bool}
async fn chats_settings_put_handler(state: axum::extract::State<Arc<AppState>>, headers: axum::http::HeaderMap, Json(body): Json<SavingBody>) -> ApiResult {
    let store = store_for(&state, &headers)?;
    store.set_saving(body.saving).await.map_err(db_error)?;
    Ok(Json(serde_json::json!({ "saving": body.saving })))
}

/// POST /api/chats/turns/{id}/stop — cancels a running saved turn.
async fn turn_stop_handler(state: axum::extract::State<Arc<AppState>>, headers: axum::http::HeaderMap, axum::extract::Path(id): axum::extract::Path<i64>) -> ApiResult {
    let store = store_for(&state, &headers)?;
    let stopped = store.stop_turn(id).await.map_err(db_error)?;
    if let Some(h) = state.running.lock().unwrap_or_else(|p| p.into_inner()).remove(&id) {
        h.abort();
    }
    Ok(Json(serde_json::json!({ "stopped": stopped })))
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
            "Saved conversations with optional LLM memory (SQLite)",
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
            "GET /api/chats": "Saved conversations (this machine's page only)",
            "GET /api/chats/{id}": "One conversation with every turn",
            "DELETE /api/chats/{id}": "Delete a conversation",
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

    // An English voice reads Devanagari as silence or gibberish.
    let voice = if final_text.chars().any(|c| ('\u{0900}'..='\u{097F}').contains(&c)) { "hi-IN-SwaraNeural" } else { "en-US-AvaNeural" };
    let output = tokio::process::Command::new(&edge_tts_path)
        .arg("--text")
        .arg(&final_text)
        .arg("--voice")
        .arg(voice)
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

    tracing::info!("  Features: Iterative Research, TempDb, saved conversations");
    tracing::info!("  CORS: permissive");
    tracing::info!("  Port: {}", port);
    tracing::info!("============================================");

    // Initialize dual database system
    let temp_db = cache::TempDb::new();
    let chats = match chats::ChatStore::open_default() {
        Ok(s) => {
            tracing::info!("  Conversations: {:?}", chats::ChatStore::data_dir().join("searchwala.db"));
            Some(s)
        }
        Err(e) => {
            tracing::error!("Conversations disabled, the database could not be opened: {e}");
            None
        }
    };

    // Spawn background cleanup for expired temp sessions
    cache::spawn_temp_db_cleaner(temp_db.clone());

    let state = Arc::new(AppState {
        start_time: Instant::now(),
        temp_db,
        chats,
        running: Default::default(),
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
        .route("/api/chats", get(chats_list_handler).delete(chats_delete_all_handler))
        .route("/api/chats/settings", get(chats_settings_get_handler).put(chats_settings_put_handler))
        .route("/api/chats/turns/{id}/stop", post(turn_stop_handler))
        .route("/api/chats/{id}", get(chat_get_handler).patch(chat_rename_handler).delete(chat_delete_handler))
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
