// ============================================================================
// Swift-Search-RS v4.0.0
// ============================================================================
//
// Native Rust meta-search + extraction + optional BYOK LLM synthesis.
//
// Pipeline:
// 1) Query multiple search engines concurrently
// 2) Deduplicate URLs
// 3) Concurrently scrape + extract readable text
// 4) Optionally run token-saver LLM synthesis with strict timeout fallback
//
// ============================================================================

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
    extract::Json,
    http::StatusCode,
    response::{Html, IntoResponse, sse::{Event, Sse}},
    routing::{get, post},
    Router,
};
use tower_http::cors::CorsLayer;

use models::*;

struct AppState {
    start_time: Instant,
}

const BENCHMARK_UI: &str = include_str!("../benchmark_ui.html");

/// POST /search - Main search endpoint
async fn search_handler(
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
        body.enable_copilot
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

/// POST /search/stream - Streaming search endpoint explicitly for LLM synthesis
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

/// GET /health - Health check
async fn health_handler(state: axum::extract::State<Arc<AppState>>) -> impl IntoResponse {
    let uptime = state.start_time.elapsed().as_secs();
    Json(HealthResponse {
        status: "ok".to_string(),
        version: "4.0.0".to_string(),
        engines: config::enabled_engines(),
        uptime_seconds: uptime,
    })
}

/// GET /config - Configuration info
async fn config_handler() -> impl IntoResponse {
    Json(ConfigResponse {
        version: "4.0.0".to_string(),
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

/// GET / - Root endpoint (for uptime pings)
async fn root_handler() -> impl IntoResponse {
    Html(BENCHMARK_UI)
}

/// GET /about - Service metadata JSON endpoint
async fn about_handler() -> impl IntoResponse {
    Json(serde_json::json!({
        "name": "Swift-Search-RS",
        "version": "4.0.0",
        "language": "Rust",
        "description": "Ultra-fast native meta-search & scrape API with optional BYOK LLM synthesis",
        "endpoints": {
            "POST /search": "Search and scrape (body: {\"query\":\"...\",\"llm\":{...optional...}})",
            "POST /search/lite-llm": "LLM synthesis with forced lite mode",
            "POST /search/research-llm": "LLM synthesis with forced research mode",
            "GET /health": "Health check",
            "GET /config": "Current configuration"
        }
    }))
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
    tracing::info!("  Swift-Search-RS v4.0.0");
    tracing::info!("  Language: Rust");
    tracing::info!("  Engines: {:?}", engines);
    tracing::info!("  Max URLs: {}", config::max_urls());
    tracing::info!("  Concurrency: {}", config::concurrency());
    tracing::info!("  Scrape timeout: {}s", config::scrape_timeout_secs());
    tracing::info!("  Max HTML: {} bytes", config::max_html_bytes());
    tracing::info!("  CORS: permissive");
    tracing::info!("  Port: {}", port);
    tracing::info!("============================================");

    let state = Arc::new(AppState {
        start_time: Instant::now(),
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
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("Listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");

    axum::serve(listener, app).await.expect("Server error");
}
