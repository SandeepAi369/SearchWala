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
    response::{
        sse::{Event, Sse},
        Html, IntoResponse,
    },
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

fn params_from(
    body: SearchRequest,
    focus_override: Option<&str>,
    default_llm_urls: Option<usize>,
) -> search::SearchParams {
    search::SearchParams {
        query: body.query.trim().to_string(),
        max_results: body.max_results.or(default_llm_urls),
        focus_mode: focus_override.map(str::to_string).or(body.focus_mode),
        domain: body.domain,
        llm: body.llm,
        enable_copilot: body.enable_copilot.unwrap_or(false),
        memory: Vec::new(),
        domain_filter: search::DomainFilter::default(),
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
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let loopback_bind = matches!(bind.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]");
    let host_name = host.rsplit_once(':').map_or(host, |(h, p)| {
        if p.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            host
        }
    });
    if loopback_bind && !matches!(host_name, "127.0.0.1" | "localhost" | "[::1]") {
        return false;
    }
    match headers
        .get(axum::http::header::ORIGIN)
        .and_then(|o| o.to_str().ok())
    {
        None => true,
        Some(origin) => origin
            .split_once("://")
            .is_some_and(|(_, rest)| rest == host),
    }
}

fn forbidden() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(
            serde_json::json!({ "error": "conversations are only available to the SearchWala page on this machine" }),
        ),
    )
}

/// Saves the question (when the request opts in with `save: true` and saving
/// is on) and loads the conversation's earlier turns as memory (when the
/// request asks for them and has an LLM). Scripts and tools calling the API
/// without `save` leave nothing on disk.
async fn begin_turn(
    state: &AppState,
    body: &SearchRequest,
    trusted: bool,
    params: &mut search::SearchParams,
) -> Option<(String, i64)> {
    let store = state.chats.as_ref().filter(|_| trusted)?;
    let mode = body.focus_mode.clone().unwrap_or_else(|| "auto".into());
    let turn = if body.save == Some(true) && store.saving_enabled().await {
        match store
            .begin_turn(body.conversation_id.clone(), &params.query, &mode)
            .await
        {
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
    let conversation = turn
        .as_ref()
        .map(|(c, _)| c.clone())
        .or_else(|| body.conversation_id.clone());
    if let (true, true, Some(conv)) = (wanted > 0, params.llm.is_some(), conversation) {
        let before = turn.as_ref().map_or(i64::MAX, |(_, t)| *t);
        params.memory = store
            .prior_turns(&conv, before, wanted)
            .await
            .unwrap_or_default();
    }
    turn
}

/// Completes the saved turn with the answer and what the UI rendered.
async fn finish_turn(
    state: &AppState,
    turn: &Option<(String, i64)>,
    response: &mut SearchResponse,
) {
    let (Some(store), Some((conv, id))) = (state.chats.as_ref(), turn) else {
        return;
    };
    response.conversation_id = Some(conv.clone());
    response.turn_id = Some(*id);
    let status = if response.llm_answer.is_some() || response.sources_processed > 0 {
        "done"
    } else {
        "failed"
    };
    let json = serde_json::to_value(&*response).ok();
    if let Err(e) = store
        .finish_turn(
            *id,
            status,
            response.search_query.clone(),
            response.llm_answer.clone(),
            json,
        )
        .await
    {
        tracing::warn!("chats: could not save the answer: {e}");
    }
}

/// The non-streaming endpoints: save around one pipeline run.
async fn run_saved(
    state: &AppState,
    body: SearchRequest,
    headers: &axum::http::HeaderMap,
    focus_override: Option<&str>,
) -> SearchResponse {
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
            tokio::spawn(async move {
                search::execute_search_full(params, Some(&state.temp_db), Some(ptx)).await
            })
        };
        if let Some((conv, id)) = &turn {
            state
                .running
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(*id, pipeline.abort_handle());
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
            state
                .running
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(id);
        }
        let final_event = match outcome {
            Ok(mut resp) => {
                finish_turn(&state, &turn, &mut resp).await;
                serde_json::json!({ "type": "result", "response": resp })
            }
            Err(e) => {
                if let (Some(store), Some((_, id))) = (state.chats.as_ref(), &turn) {
                    let status = if e.is_cancelled() {
                        "stopped"
                    } else {
                        "failed"
                    };
                    let _ = store.finish_turn(*id, status, None, None, None).await;
                }
                serde_json::json!({ "type": "error", "error": e.to_string() })
            }
        };
        if client_gone {
            return;
        }
        let _ = tx
            .send(Ok(Event::default().data(final_event.to_string())))
            .await;
        let _ = tx
            .send(Ok(
                Event::default().data(serde_json::json!({ "type": "done" }).to_string())
            ))
            .await;
    });

    Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
        .keep_alive(axum::response::sse::KeepAlive::new())
}

// =============================================================================
// Conversations API (only for the SearchWala page; see `from_own_page`)
// =============================================================================

type ApiResult = Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)>;

fn store_for<'a>(
    state: &'a AppState,
    headers: &axum::http::HeaderMap,
) -> Result<&'a chats::ChatStore, (StatusCode, Json<serde_json::Value>)> {
    if !from_own_page(headers) {
        return Err(forbidden());
    }
    state.chats.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": "the conversation database could not be opened; see the server log" })))
    })
}

fn db_error(e: String) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": e })),
    )
}

fn not_found() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "no such conversation" })),
    )
}

#[derive(serde::Deserialize)]
struct ListQuery {
    q: Option<String>,
    limit: Option<usize>,
}

/// GET /api/chats?q=&limit= — conversations, most recent first.
async fn chats_list_handler(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<ListQuery>,
) -> ApiResult {
    let store = store_for(&state, &headers)?;
    let list = store
        .list(q.q, q.limit.unwrap_or(500))
        .await
        .map_err(db_error)?;
    Ok(Json(
        serde_json::json!({ "saving": store.saving_enabled().await, "conversations": list }),
    ))
}

/// GET /api/chats/{id} — every turn, as it was rendered.
async fn chat_get_handler(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult {
    let store = store_for(&state, &headers)?;
    let conv = store
        .get(&id)
        .await
        .map_err(db_error)?
        .ok_or_else(not_found)?;
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
async fn chat_delete_handler(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult {
    let store = store_for(&state, &headers)?;
    if !store.delete(&id).await.map_err(db_error)? {
        return Err(not_found());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// DELETE /api/chats — every conversation.
async fn chats_delete_all_handler(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> ApiResult {
    let store = store_for(&state, &headers)?;
    let n = store.delete_all().await.map_err(db_error)?;
    Ok(Json(serde_json::json!({ "deleted": n })))
}

#[derive(serde::Deserialize)]
struct SavingBody {
    saving: bool,
}

/// GET /api/chats/settings
async fn chats_settings_get_handler(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> ApiResult {
    let store = store_for(&state, &headers)?;
    Ok(Json(
        serde_json::json!({ "saving": store.saving_enabled().await }),
    ))
}

/// PUT /api/chats/settings {"saving": bool}
async fn chats_settings_put_handler(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SavingBody>,
) -> ApiResult {
    let store = store_for(&state, &headers)?;
    store.set_saving(body.saving).await.map_err(db_error)?;
    Ok(Json(serde_json::json!({ "saving": body.saving })))
}

/// POST /api/chats/turns/{id}/stop — cancels a running saved turn.
async fn turn_stop_handler(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> ApiResult {
    let store = store_for(&state, &headers)?;
    let stopped = store.stop_turn(id).await.map_err(db_error)?;
    if let Some(h) = state
        .running
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&id)
    {
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
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Html(SEARCH_UI),
    )
}

/// POST /api/models - Dynamic model fetcher
async fn models_handler(
    Json(body): Json<serde_json::Value>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let api_key = body
        .get("api_key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let base_url = body
        .get("base_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let provider = body
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

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
    if let Err(e) = Some(base_url.trim())
        .filter(|b| !b.is_empty())
        .map_or(Ok(()), |b| ssrf::validate_llm_base_url(b).map(|_| ()))
    {
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
            "POST /mcp": "Model Context Protocol (Streamable HTTP)",
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
    let _permit = TTS_GATE.try_acquire().map_err(|_| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            "TTS busy, retry shortly".to_string(),
        )
    })?;

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
                if next == ']' {
                    chars.next();
                    break;
                }
                if !next.is_ascii_digit() && next != ',' && next != ' ' {
                    is_ref = false;
                }
                buf.push(next);
                chars.next();
            }
            if !is_ref {
                result.push_str(&buf);
            } // Keep non-numeric bracket content
        } else {
            result.push(ch);
        }
    }

    // Final cleanup
    let final_text: String = result.split_whitespace().collect::<Vec<&str>>().join(" ");

    // Use the OS temp dir (not a hardcoded /tmp) and guarantee cleanup even on
    // the error paths — v7.0 returned early on a non-zero exit status BEFORE
    // remove_file, leaking an mp3 per failed request.
    let tmp_path = std::env::temp_dir().join(format!(
        "searchwala_tts_{}_{}.mp3",
        std::process::id(),
        rand::random::<u32>()
    ));
    let _tmp_guard = TempFileGuard(tmp_path.clone());

    // Resolve edge-tts dynamically from EDGE_TTS_PATH, $HOME/.local/bin/edge-tts,
    // /home/$USER/.local/bin/edge-tts (when HOME is overridden in clean-room runs),
    // system bin directories, or $PATH (zero hardcoded username).
    let edge_tts_path = std::env::var("EDGE_TTS_PATH").unwrap_or_else(|_| {
        let mut candidates: Vec<String> = Vec::new();
        if let Ok(home) = std::env::var("HOME") {
            candidates.push(format!(
                "{}/.local/bin/edge-tts",
                home.trim_end_matches('/')
            ));
        }
        if let Ok(user) = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")) {
            if !user.trim().is_empty() {
                candidates.push(format!("/home/{}/.local/bin/edge-tts", user.trim()));
            }
        }
        candidates.push("/usr/local/bin/edge-tts".to_string());
        candidates.push("/usr/bin/edge-tts".to_string());
        for cand in candidates {
            if std::path::Path::new(&cand).exists() {
                return cand;
            }
        }
        "edge-tts".to_string()
    });

    // An English voice reads Devanagari as silence or gibberish.
    let voice = if final_text
        .chars()
        .any(|c| ('\u{0900}'..='\u{097F}').contains(&c))
    {
        "hi-IN-SwaraNeural"
    } else {
        "en-US-AvaNeural"
    };
    let mut cmd = tokio::process::Command::new(&edge_tts_path);
    // If edge-tts lives in <prefix>/.local/bin/edge-tts, set PYTHONUSERBASE=<prefix>/.local
    // so Python locates user site-packages even when HOME is overridden (e.g. HOME=/tmp/sw_clean).
    if let Some(local_base) = edge_tts_path.strip_suffix("/bin/edge-tts") {
        if local_base.ends_with("/.local") {
            cmd.env("PYTHONUSERBASE", local_base);
        }
    }
    let output = cmd
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
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("edge-tts not found: {e}"),
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("edge-tts error: {stderr}"),
        ));
    }

    let audio_bytes = tokio::fs::read(&tmp_path).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read error: {e}"),
        )
    })?;
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliCommand {
    Serve {
        port: Option<u16>,
        bind: Option<String>,
        enable_mcp: bool,
    },
    McpStdio {
        log_level: Option<String>,
    },
    McpHttp {
        port: Option<u16>,
        bind: Option<String>,
        log_level: Option<String>,
    },
    Version,
    Help,
}

fn print_usage() {
    println!(
        "SearchWala v{ver} — Private meta-search, deep extraction, and MCP server\n\n\
         USAGE:\n\
         \x20 searchwala                              Start web UI + REST API + /mcp HTTP server (default)\n\
         \x20 searchwala serve [OPTIONS]              Start HTTP server\n\
         \x20 searchwala mcp [OPTIONS]                Start Model Context Protocol (MCP) server (stdio by default)\n\
         \x20 searchwala --version                    Print version and exit\n\
         \x20 searchwala --help                       Print this help and exit\n\n\
         SERVE OPTIONS:\n\
         \x20 --port <PORT>                           Override PORT env var (default: 8000)\n\
         \x20 --bind <HOST>                           Override BIND env var (default: 127.0.0.1)\n\
         \x20 --no-mcp                                Disable Streamable HTTP /mcp endpoint\n\n\
         MCP OPTIONS:\n\
         \x20 --transport <stdio|http>                MCP transport (default: stdio)\n\
         \x20 --http                                  Shorthand for --transport http (headless /mcp + /health)\n\
         \x20 --port <PORT>                           Port for --http mode (default: PORT env or 8000)\n\
         \x20 --bind <HOST>                           Bind host for --http mode (default: BIND env or 127.0.0.1)\n\
         \x20 --log-level <LEVEL>                     Log level: error|warn|info|debug|trace|off (default: warn in stdio, info in http)\n",
        ver = crate::config::VERSION
    );
}

fn parse_cli_args(args: &[String]) -> Result<CliCommand, String> {
    if args.is_empty() {
        return Ok(CliCommand::Serve {
            port: None,
            bind: None,
            enable_mcp: true,
        });
    }

    match args[0].as_str() {
        "-h" | "--help" | "help" => Ok(CliCommand::Help),
        "-V" | "--version" | "version" => Ok(CliCommand::Version),
        "serve" => parse_serve_flags(&args[1..]),
        "mcp" => parse_mcp_flags(&args[1..]),
        other if other.starts_with('-') => parse_serve_flags(args),
        other => Err(format!(
            "unknown subcommand {other:?} (expected `serve` or `mcp`; run `searchwala --help`)"
        )),
    }
}

fn take_flag_value(args: &[String], idx: &mut usize, flag: &str) -> Result<String, String> {
    if let Some((_, val)) = args[*idx].split_once('=') {
        if val.is_empty() {
            return Err(format!("missing value for {flag}"));
        }
        *idx += 1;
        return Ok(val.to_string());
    }
    let val = args
        .get(*idx + 1)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("missing value for {flag}"))?;
    *idx += 2;
    Ok(val.clone())
}

fn parse_serve_flags(args: &[String]) -> Result<CliCommand, String> {
    let mut port = None;
    let mut bind = None;
    let mut enable_mcp = true;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if matches!(arg, "-h" | "--help") {
            return Ok(CliCommand::Help);
        } else if arg == "--no-mcp" {
            enable_mcp = false;
            i += 1;
        } else if arg == "--port" || arg.starts_with("--port=") {
            let v = take_flag_value(args, &mut i, "--port")?;
            let p: u16 = v
                .parse()
                .map_err(|_| format!("invalid --port value: {v:?}"))?;
            port = Some(p);
        } else if arg == "--bind" || arg.starts_with("--bind=") {
            let v = take_flag_value(args, &mut i, "--bind")?;
            bind = Some(v);
        } else {
            return Err(format!("unknown option for `serve`: {arg:?}"));
        }
    }
    Ok(CliCommand::Serve {
        port,
        bind,
        enable_mcp,
    })
}

fn parse_mcp_flags(args: &[String]) -> Result<CliCommand, String> {
    let mut http_mode = false;
    let mut port = None;
    let mut bind = None;
    let mut log_level = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if matches!(arg, "-h" | "--help") {
            return Ok(CliCommand::Help);
        } else if arg == "--http" {
            http_mode = true;
            i += 1;
        } else if arg == "--transport" || arg.starts_with("--transport=") {
            let v = take_flag_value(args, &mut i, "--transport")?.to_ascii_lowercase();
            match v.as_str() {
                "stdio" => http_mode = false,
                "http" | "streamable-http" => http_mode = true,
                _ => {
                    return Err(format!(
                        "unsupported --transport {v:?} (expected `stdio` or `http`)"
                    ));
                }
            }
        } else if arg == "--port" || arg.starts_with("--port=") {
            let v = take_flag_value(args, &mut i, "--port")?;
            let p: u16 = v
                .parse()
                .map_err(|_| format!("invalid --port value: {v:?}"))?;
            port = Some(p);
        } else if arg == "--bind" || arg.starts_with("--bind=") {
            let v = take_flag_value(args, &mut i, "--bind")?;
            bind = Some(v);
        } else if arg == "--log-level" || arg.starts_with("--log-level=") {
            let v = take_flag_value(args, &mut i, "--log-level")?.to_ascii_lowercase();
            if !matches!(
                v.as_str(),
                "error" | "warn" | "info" | "debug" | "trace" | "off"
            ) {
                return Err(format!(
                    "invalid --log-level {v:?} (expected error|warn|info|debug|trace|off)"
                ));
            }
            log_level = Some(v);
        } else {
            return Err(format!("unknown option for `mcp`: {arg:?}"));
        }
    }
    if http_mode {
        Ok(CliCommand::McpHttp {
            port,
            bind,
            log_level,
        })
    } else {
        if port.is_some() || bind.is_some() {
            return Err(
                "`--port` and `--bind` require `--http` (or `--transport http`) with `mcp`".into(),
            );
        }
        Ok(CliCommand::McpStdio { log_level })
    }
}

async fn run_mcp_stdio(log_level: Option<String>) {
    // MCP stdio framing owns stdout exclusively; route all tracing to stderr
    // and default to `error` so live upstream engine 429/202 warnings never
    // spam stderr in MCP hosts (per-engine health is exposed via `search_status`
    // and `searchwala://health/engines`). Users can still opt in via `--log-level`
    // or `RUST_LOG`.
    let filter = match log_level {
        Some(lvl) => format!("searchwala={lvl}"),
        None => std::env::var("RUST_LOG").unwrap_or_else(|_| "searchwala=error".to_string()),
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_target(false)
        .compact()
        .init();

    let temp_db = cache::TempDb::new();
    cache::spawn_temp_db_cleaner(temp_db.clone());

    let server = mcp::McpServer::new(temp_db);
    if let Err(e) = mcp::stdio::run_stdio(server).await {
        tracing::error!("MCP stdio transport error: {e}");
        std::process::exit(1);
    }
}

async fn run_mcp_http(
    port_override: Option<u16>,
    bind_override: Option<String>,
    log_level: Option<String>,
) {
    let filter = match log_level {
        Some(lvl) => format!("searchwala={lvl}"),
        None => std::env::var("RUST_LOG").unwrap_or_else(|_| "searchwala=info".to_string()),
    };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_target(false)
        .compact()
        .init();

    let port = port_override.unwrap_or_else(config::port);
    let bind_host = bind_override
        .unwrap_or_else(|| std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1".to_string()));
    let temp_db = cache::TempDb::new();
    cache::spawn_temp_db_cleaner(temp_db.clone());

    let server = mcp::McpServer::new(temp_db);
    if let Err(e) = mcp::http::run_http(server, bind_host, port).await {
        tracing::error!("MCP HTTP transport error: {e}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();

    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match parse_cli_args(&raw_args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(2);
        }
    };

    match cmd {
        CliCommand::Help => {
            print_usage();
            return;
        }
        CliCommand::Version => {
            println!("searchwala {}", crate::config::VERSION);
            return;
        }
        CliCommand::McpStdio { log_level } => {
            run_mcp_stdio(log_level).await;
            return;
        }
        CliCommand::McpHttp {
            port,
            bind,
            log_level,
        } => {
            run_mcp_http(port, bind, log_level).await;
            return;
        }
        CliCommand::Serve {
            port,
            bind,
            enable_mcp,
        } => {
            run_http_server(port, bind, enable_mcp).await;
        }
    }
}

async fn run_http_server(
    port_override: Option<u16>,
    bind_override: Option<String>,
    enable_mcp: bool,
) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "searchwala=info,tower_http=info".into()),
        )
        .with_target(false)
        .compact()
        .init();

    let port = port_override.unwrap_or_else(config::port);
    let bind_host = bind_override
        .unwrap_or_else(|| std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1".to_string()));
    let engines = modes::all_roster_engines();

    tracing::info!("============================================");
    tracing::info!("  SearchWala v{}", crate::config::VERSION);
    tracing::info!("  Language: Rust");
    tracing::info!("  Engines: {} total", engines.len());
    tracing::info!("  Modes: fast, auto, deep, study, academic, reddit, youtube (+ domains)");
    tracing::info!("  Concurrency: {}", config::concurrency());

    tracing::info!("  Features: Iterative Research, TempDb, saved conversations, MCP");
    tracing::info!(
        "  MCP HTTP: {}",
        if enable_mcp { "/mcp" } else { "disabled" }
    );
    tracing::info!("  CORS: permissive");
    tracing::info!("  Port: {}", port);
    tracing::info!("============================================");

    // Initialize dual database system
    let temp_db = cache::TempDb::new();
    let chats = match chats::ChatStore::open_default() {
        Ok(s) => {
            tracing::info!(
                "  Conversations: {:?}",
                chats::ChatStore::data_dir().join("searchwala.db")
            );
            Some(s)
        }
        Err(e) => {
            tracing::error!("Conversations disabled, the database could not be opened: {e}");
            None
        }
    };

    // Spawn background cleanup for expired temp sessions
    cache::spawn_temp_db_cleaner(temp_db.clone());

    let mcp_server = mcp::McpServer::new(temp_db.clone());
    let state = Arc::new(AppState {
        start_time: Instant::now(),
        temp_db,
        chats,
        running: Default::default(),
    });

    let mut app = Router::new()
        .route("/", get(root_handler))
        .route("/index.html", get(root_handler))
        .route("/ui", get(root_handler))
        .route("/about", get(about_handler))
        .route("/health", get(health_handler))
        .route("/config", get(config_handler))
        .route("/search", get(root_handler).post(search_handler))
        .route("/search/auto-llm", post(search_auto_llm_handler))
        .route("/search/lite-llm", post(search_auto_llm_handler)) // backward compat
        .route("/search/research-llm", post(search_research_llm_handler))
        .route("/search/stream", post(stream_handler))
        .route("/api/models", post(models_handler))
        .route(
            "/api/chats",
            get(chats_list_handler).delete(chats_delete_all_handler),
        )
        .route(
            "/api/chats/settings",
            get(chats_settings_get_handler).put(chats_settings_put_handler),
        )
        .route("/api/chats/turns/{id}/stop", post(turn_stop_handler))
        .route(
            "/api/chats/{id}",
            get(chat_get_handler)
                .patch(chat_rename_handler)
                .delete(chat_delete_handler),
        )
        .route("/api/tts", get(tts_handler))
        .route("/health/engines", get(engine_health_handler))
        .route("/health/cache", get(cache_stats_handler))
        .with_state(state);

    if enable_mcp {
        app = app.merge(mcp::http::mcp_routes(mcp_server, bind_host.clone()));
    }

    let cors = CorsLayer::permissive().expose_headers([
        axum::http::header::HeaderName::from_static(mcp::http::HEADER_SESSION_ID),
        axum::http::header::HeaderName::from_static(mcp::http::HEADER_PROTOCOL_VERSION),
    ]);

    let app = app
        // v8.0: a handler panic used to abort the task with no response, so the
        // client saw a bare connection reset. The UTF-8 byte-slice bugs in
        // search.rs/llm.rs did exactly that on Hindi queries.
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        .layer(tower_http::limit::RequestBodyLimitLayer::new(256 * 1024))
        .layer(cors);

    let addr = format!("{}:{}", bind_host, port);
    tracing::info!("Listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");

    axum::serve(listener, app).await.expect("Server error");
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|v| (*v).to_string()).collect()
    }

    #[test]
    fn cli_defaults_to_serve_with_no_args() {
        assert_eq!(
            parse_cli_args(&[]).unwrap(),
            CliCommand::Serve {
                port: None,
                bind: None,
                enable_mcp: true
            }
        );
    }

    #[test]
    fn cli_parses_serve_mcp_http_help_and_version() {
        assert_eq!(
            parse_cli_args(&s(&[
                "serve",
                "--port",
                "9000",
                "--bind=0.0.0.0",
                "--no-mcp"
            ]))
            .unwrap(),
            CliCommand::Serve {
                port: Some(9000),
                bind: Some("0.0.0.0".into()),
                enable_mcp: false
            }
        );
        assert_eq!(
            parse_cli_args(&s(&["mcp"])).unwrap(),
            CliCommand::McpStdio { log_level: None }
        );
        assert_eq!(
            parse_cli_args(&s(&["mcp", "--transport", "stdio", "--log-level=debug"])).unwrap(),
            CliCommand::McpStdio {
                log_level: Some("debug".into())
            }
        );
        assert_eq!(
            parse_cli_args(&s(&["mcp", "--http", "--port=8123", "--bind", "127.0.0.1"])).unwrap(),
            CliCommand::McpHttp {
                port: Some(8123),
                bind: Some("127.0.0.1".into()),
                log_level: None
            }
        );
        assert_eq!(
            parse_cli_args(&s(&["mcp", "--transport=http", "--log-level", "warn"])).unwrap(),
            CliCommand::McpHttp {
                port: None,
                bind: None,
                log_level: Some("warn".into())
            }
        );
        assert_eq!(
            parse_cli_args(&s(&["--version"])).unwrap(),
            CliCommand::Version
        );
        assert_eq!(parse_cli_args(&s(&["-h"])).unwrap(), CliCommand::Help);
        assert_eq!(
            parse_cli_args(&s(&["mcp", "--help"])).unwrap(),
            CliCommand::Help
        );
    }

    #[test]
    fn cli_rejects_unknown_subcommands_and_invalid_flags() {
        assert!(parse_cli_args(&s(&["bogus"])).is_err());
        assert!(parse_cli_args(&s(&["serve", "--port", "notanumber"])).is_err());
        assert!(parse_cli_args(&s(&["mcp", "--log-level", "verbose"])).is_err());
        assert!(parse_cli_args(&s(&["mcp", "--transport", "ws"])).is_err());
        assert!(parse_cli_args(&s(&["mcp", "--port", "8080"])).is_err());
    }
}
