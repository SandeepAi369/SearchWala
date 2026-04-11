// ============================================================================
// Swift-Search-RS v4.2.0
// ============================================================================
//
// Native Rust meta-search + extraction + optional BYOK LLM synthesis.
// Iterative Deep Research | Dual Database | Time-Aware LLM
//
// Pipeline:
// 1) Query 90+ search engines concurrently
// 2) Deduplicate URLs
// 3) Concurrently scrape + extract readable text
// 4) Optionally run iterative LLM synthesis (multi-batch for research)
//
// ============================================================================

pub mod cache;
pub mod config;
pub mod engines;
pub mod extractor;
pub mod llm;
pub mod models;
pub mod ranking;
pub mod search;
pub mod url_utils;
pub mod copilot;
pub mod stream;
pub mod proxy_pool;

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

const BENCHMARK_UI: &str = include_str!("../benchmark_ui.html");

/// POST /search - Main search endpoint
async fn search_handler(
    state: axum::extract::State<Arc<AppState>>,
    Json(body): Json<SearchRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let query = body.query.trim().to_string();

    if query.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "Query cannot be empty"
            })),
        ));
    }

    let response = search::execute_search(
        &query,
        body.max_results,
        body.focus_mode,
        body.llm,
        body.enable_copilot,
        Some(&state.temp_db),
        Some(&state.history_db),
    ).await;

    if response.sources_processed == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "No results found. Search engines may be unreachable.",
                "query": query,
                "engines_queried": response.engine_stats.engines_queried,
            })),
        ));
    }

    Ok(Json(response))
}

/// POST /search/lite-llm - LLM flow forced to lite focus mode
async fn search_lite_llm_handler(
    state: axum::extract::State<Arc<AppState>>,
    Json(body): Json<SearchRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let query = body.query.trim().to_string();

    if query.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "Query cannot be empty"
            })),
        ));
    }

    let Some(llm_cfg) = body.llm else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "LLM config is required for /search/lite-llm"
            })),
        ));
    };

    let response = search::execute_search(
        &query,
        body.max_results.or(Some(30)),
        Some("lite".to_string()),
        Some(llm_cfg),
        body.enable_copilot,
        Some(&state.temp_db),
        Some(&state.history_db),
    )
    .await;

    if response.sources_processed == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "No results found. Search engines may be unreachable.",
                "query": query,
                "engines_queried": response.engine_stats.engines_queried,
            })),
        ));
    }

    Ok(Json(response))
}

/// POST /search/research-llm - LLM flow forced to research focus mode
async fn search_research_llm_handler(
    state: axum::extract::State<Arc<AppState>>,
    Json(body): Json<SearchRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let query = body.query.trim().to_string();

    if query.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "Query cannot be empty"
            })),
        ));
    }

    let Some(llm_cfg) = body.llm else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "LLM config is required for /search/research-llm"
            })),
        ));
    };

    let response = search::execute_search(
        &query,
        body.max_results,
        Some("research".to_string()),
        Some(llm_cfg),
        body.enable_copilot,
        Some(&state.temp_db),
        Some(&state.history_db),
    )
    .await;

    if response.sources_processed == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "No results found. Search engines may be unreachable.",
                "query": query,
                "engines_queried": response.engine_stats.engines_queried,
            })),
        ));
    }

    Ok(Json(response))
}

/// POST /search/stream - Streaming search endpoint
async fn stream_handler(
    Json(body): Json<SearchRequest>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let query = body.query.trim().to_string();
    let stream = stream::execute_stream_search(
        query,
        body.max_results,
        body.focus_mode,
        body.llm,
        body.enable_copilot
    );
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::new())
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
        version: "4.3.0".to_string(),
        engines: config::enabled_engines(),
        uptime_seconds: uptime,
    })
}

/// GET /config - Configuration info
async fn config_handler() -> impl IntoResponse {
    Json(ConfigResponse {
        version: "4.3.0".to_string(),
        engines: config::enabled_engines(),
        max_urls: config::max_urls(),
        scrape_timeout_secs: config::scrape_timeout_secs(),
        concurrent_scrapes: config::concurrency(),
        concurrent_engines: config::engine_concurrency(),
        jitter_min_ms: config::jitter_min_ms(),
        jitter_max_ms: config::jitter_max_ms(),
        proxy_cooldown_secs: config::proxy_cooldown_secs(),
        user_agents_count: config::user_agents_count(),
    })
}

/// GET / - Root endpoint
async fn root_handler() -> impl IntoResponse {
    Html(BENCHMARK_UI)
}

/// POST /api/models - Dynamic model fetcher
async fn models_handler(
    Json(body): Json<serde_json::Value>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let api_key = body.get("api_key").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let base_url = body.get("base_url").and_then(|v| v.as_str()).unwrap_or("").to_string();

    if api_key.is_empty() || base_url.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "api_key and base_url are required" })),
        ));
    }

    match llm::fetch_provider_models(&api_key, &base_url).await {
        Ok(models) => Ok(Json(serde_json::json!({ "models": models }))),
        Err(err) => Err((
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": err })),
        )),
    }
}

/// GET /about - Service metadata
async fn about_handler() -> impl IntoResponse {
    Json(serde_json::json!({
        "name": "Swift-Search-RS",
        "version": "4.3.0",
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
async fn tts_handler(
    Query(params): Query<TtsQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let text = params.text.trim().to_string();
    if text.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Missing 'text' parameter".into()));
    }

    // Cap at 5000 chars (same as XeL Studio)
    let capped = if text.len() > 5000 {
        text.chars().take(5000).collect::<String>()
    } else {
        text.clone()
    };

    // Clean for TTS (remove markdown)
    let cleaned: String = capped
        .replace("**", "")
        .replace("##", "")
        .replace("###", "")
        .replace('`', "")
        .replace('[', "")
        .replace(']', "")
        .replace("\n\n", ". ")
        .replace('\n', " ");

    let edge_tts_path = std::env::var("EDGE_TTS_PATH")
        .unwrap_or_else(|_| {
            // Try common locations
            for path in &[
                "/home/sandeep/.local/bin/edge-tts",
                "/usr/local/bin/edge-tts",
                "/usr/bin/edge-tts",
            ] {
                if std::path::Path::new(path).exists() {
                    return path.to_string();
                }
            }
            "edge-tts".to_string() // fallback to PATH
        });

    let tmp_path = format!("/tmp/swift_tts_{}.mp3", std::process::id());

    let output = tokio::process::Command::new(&edge_tts_path)
        .arg("--text")
        .arg(&cleaned)
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

    let _ = tokio::fs::remove_file(&tmp_path).await;

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
                .unwrap_or_else(|_| "swift_search_rs=info,tower_http=info".into()),
        )
        .with_target(false)
        .compact()
        .init();

    let port = config::port();
    let engines = config::enabled_engines();

    tracing::info!("============================================");
    tracing::info!("  Swift-Search-RS v4.2.0");
    tracing::info!("  Language: Rust");
    tracing::info!("  Engines: {} total", engines.len());
    tracing::info!("  Max URLs: {}", config::max_urls());
    tracing::info!("  Concurrency: {}", config::concurrency());
    tracing::info!("  Scrape timeout: {}s", config::scrape_timeout_secs());
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
        .route("/benchmark_ui.html", get(root_handler))
        .route("/ui", get(root_handler))
        .route("/about", get(about_handler))
        .route("/health", get(health_handler))
        .route("/config", get(config_handler))
        .route("/search", get(root_handler).post(search_handler))
        .route("/search/lite-llm", post(search_lite_llm_handler))
        .route("/search/research-llm", post(search_research_llm_handler))
        .route("/search/stream", post(stream_handler))
        .route("/api/models", post(models_handler))
        .route("/api/history", get(history_get_handler).delete(history_clear_handler))
        .route("/api/history/enable", post(history_enable_handler))
        .route("/api/history/disable", post(history_disable_handler))
        .route("/api/history/status", get(history_status_handler))
        .route("/api/tts", get(tts_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("Listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");

    axum::serve(listener, app).await.expect("Server error");
}
