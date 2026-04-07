// ══════════════════════════════════════════════════════════════════════════════
// ⚡ Swift-Search-Rs v3.0.0
// ══════════════════════════════════════════════════════════════════════════════
//
//  A single compiled Rust binary that:
//  1. Queries 5 search engines natively (DuckDuckGo, Brave, Yahoo, Qwant, Mojeek)
//  2. Deduplicates and normalizes URLs
//  3. Concurrently scrapes pages with streaming HTTP
//  4. Extracts article text using Readability heuristics
//  5. Returns raw JSON — zero LLM, bring your own AI
//
//  Deploy anywhere: 512MB VPS, HF Spaces, Docker, bare metal.
//  Peak RAM: ~22MB under full load.
//
// ══════════════════════════════════════════════════════════════════════════════

mod config;
mod engines;
mod extractor;
mod models;
mod search;
mod url_utils;

use axum::{
    extract::Json,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use tower_http::cors::CorsLayer;
use std::sync::Arc;
use std::time::Instant;

use models::*;

// ─── Application State ──────────────────────────────────────────────────────

struct AppState {
    start_time: Instant,
}

// ─── Endpoints ───────────────────────────────────────────────────────────────

/// POST /search — Main search endpoint
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

    let response = search::execute_search(&query, body.max_results).await;

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

/// GET /health — Health check
async fn health_handler(
    state: axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    let uptime = state.start_time.elapsed().as_secs();
    Json(HealthResponse {
        status: "ok".to_string(),
        version: "3.0.0".to_string(),
        engines: config::enabled_engines(),
        uptime_seconds: uptime,
    })
}

/// GET /config — Configuration info
async fn config_handler() -> impl IntoResponse {
    Json(ConfigResponse {
        version: "3.0.0".to_string(),
        engines: config::enabled_engines(),
        max_urls: config::max_urls(),
        scrape_timeout_secs: config::scrape_timeout_secs(),
        concurrent_scrapes: config::concurrency(),
        user_agents_count: config::user_agents_count(),
    })
}

/// GET / — Root endpoint (for uptime pings)
async fn root_handler() -> impl IntoResponse {
    Json(serde_json::json!({
        "name": "Swift-Search-Rs",
        "version": "3.0.0",
        "language": "Rust",
        "description": "Ultra-fast native meta-search & scrape API",
        "endpoints": {
            "POST /search": "Search and scrape (body: {\"query\": \"...\"})",
            "GET /health": "Health check",
            "GET /config": "Current configuration"
        }
    }))
}

// ─── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // Load .env file if present
    let _ = dotenvy::dotenv();

    // Initialize logging
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

    // Print startup banner
    tracing::info!("═══════════════════════════════════════════════════");
    tracing::info!("  ⚡ Swift-Search-Rs v3.0.0");
    tracing::info!("  Language: Rust");
    tracing::info!("  Engines: {:?}", engines);
    tracing::info!("  Max URLs: {}", config::max_urls());
    tracing::info!("  Concurrency: {}", config::concurrency());
    tracing::info!("  Scrape timeout: {}s", config::scrape_timeout_secs());
    tracing::info!("  Max HTML: {} bytes", config::max_html_bytes());
    tracing::info!("  Port: {}", port);
    tracing::info!("═══════════════════════════════════════════════════");

    let state = Arc::new(AppState {
        start_time: Instant::now(),
    });

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/health", get(health_handler))
        .route("/config", get(config_handler))
        .route("/search", post(search_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("Listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");

    axum::serve(listener, app)
        .await
        .expect("Server error");
}
