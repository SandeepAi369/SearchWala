// ============================================================================
// SearchWala MCP — Streamable HTTP Transport (Axum 0.8)
//
// Implements the MCP Streamable HTTP specification on a single `/mcp` endpoint:
//   * POST /mcp   — JSON-RPC 2.0 request/notification dispatch:
//                   - `202 Accepted` for notifications (`notifications/initialized`,
//                     `notifications/cancelled`)
//                   - `text/event-stream` SSE carrying `notifications/progress` +
//                     final response when `Accept: text/event-stream` and
//                     `_meta.progressToken` are provided
//                   - `application/json` direct response otherwise
//                   - Manages `Mcp-Session-Id` (supporting both stateful and
//                     stateless clients) and echoes `MCP-Protocol-Version`
//   * DELETE /mcp — terminates an active `Mcp-Session-Id` (`204 No Content`)
//   * GET /mcp    — `405 Method Not Allowed` (`Allow: POST, DELETE`)
//
// Security:
//   When bound to loopback (`127.0.0.1`, `localhost`, `::1`), enforces Host
//   and Origin checks against DNS rebinding and cross-origin browser pages
//   (`403 Forbidden`), while allowing native MCP clients that omit `Origin`.
// ============================================================================

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use tokio::task::AbortHandle;
use tower_http::cors::{Any, CorsLayer};

use crate::mcp::protocol::{
    extract_cancelled_id, extract_progress_token, IncomingMessage, JsonRpcNotification,
    JsonRpcResponse, RequestId, DEFAULT_PROTOCOL_VERSION, ERR_PARSE, SUPPORTED_PROTOCOL_VERSIONS,
};
use crate::mcp::McpServer;

pub const HEADER_SESSION_ID: &str = "mcp-session-id";
pub const HEADER_PROTOCOL_VERSION: &str = "mcp-protocol-version";

const SESSION_TTL: Duration = Duration::from_secs(2 * 60 * 60);
const MAX_SESSIONS: usize = 512;

#[derive(Clone)]
pub struct McpHttpState {
    server: McpServer,
    bind_host: String,
    sessions: Arc<Mutex<HashMap<String, Instant>>>,
    in_flight: Arc<tokio::sync::Mutex<HashMap<RequestId, AbortHandle>>>,
}

impl McpHttpState {
    pub fn new(server: McpServer, bind_host: impl Into<String>) -> Self {
        Self {
            server,
            bind_host: bind_host.into(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    fn create_session(&self) -> String {
        let id = format!("{:032x}", rand::random::<u128>());
        let now = Instant::now();
        let mut map = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, seen| now.duration_since(*seen) < SESSION_TTL);
        if map.len() >= MAX_SESSIONS {
            if let Some(oldest_key) = map
                .iter()
                .min_by_key(|(_, ts)| **ts)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest_key);
            }
        }
        map.insert(id.clone(), now);
        id
    }

    fn touch_session(&self, id: &str) -> bool {
        let now = Instant::now();
        let mut map = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, seen| now.duration_since(*seen) < SESSION_TTL);
        if let Some(ts) = map.get_mut(id) {
            *ts = now;
            true
        } else {
            false
        }
    }

    fn remove_session(&self, id: &str) {
        let mut map = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        map.remove(id);
    }
}

/// Build an Axum router serving `/mcp` (mountable via `.merge(...)` on the main
/// server or standalone via [`run_http`]).
pub fn mcp_routes(server: McpServer, bind_host: impl Into<String>) -> Router {
    let state = McpHttpState::new(server, bind_host);
    Router::new()
        .route(
            "/mcp",
            get(mcp_get_handler)
                .post(mcp_post_handler)
                .delete(mcp_delete_handler),
        )
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(state)
}

/// Run a standalone headless Streamable HTTP MCP server (`/mcp` + `/health` only,
/// zero Web UI, zero SQLite database, zero LLM API keys required).
pub async fn run_http(server: McpServer, bind_host: String, port: u16) -> std::io::Result<()> {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers([
            HeaderName::from_static(HEADER_SESSION_ID),
            HeaderName::from_static(HEADER_PROTOCOL_VERSION),
        ]);

    let app = Router::new()
        .route("/health", get(headless_health_handler))
        .merge(mcp_routes(server, bind_host.clone()))
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        .layer(tower_http::limit::RequestBodyLimitLayer::new(256 * 1024))
        .layer(cors);

    let addr = format!("{bind_host}:{port}");
    tracing::info!("SearchWala MCP Streamable HTTP listening on http://{addr}/mcp");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await
}

async fn headless_health_handler() -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "transport": "mcp-streamable-http",
        "endpoint": "/mcp",
        "version": crate::config::VERSION,
    }))
}

// ----------------------------------------------------------------------------
// Loopback Host & Origin Guard (DNS-rebinding & cross-origin browser defense)
// ----------------------------------------------------------------------------

fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().to_ascii_lowercase();
    matches!(h.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

fn strip_port(host_header: &str) -> &str {
    let trimmed = host_header.trim();
    if let Some(rest) = trimmed.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return &trimmed[..=end + 1];
        }
        return trimmed;
    }
    match trimmed.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => trimmed,
    }
}

fn check_loopback_security(
    headers: &HeaderMap,
    bind_host: &str,
) -> Result<(), (StatusCode, Json<Value>)> {
    if !is_loopback_host(bind_host) {
        return Ok(());
    }

    if let Some(host_val) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        if !is_loopback_host(strip_port(host_val)) {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "rejected non-loopback Host header on localhost-bound MCP server"
                })),
            ));
        }
    }

    // Native MCP clients omit `Origin`; browser pages always send it. When
    // present, allow only loopback origins.
    if let Some(origin_val) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let allowed = url::Url::parse(origin_val)
            .ok()
            .and_then(|u| u.host_str().map(is_loopback_host))
            .unwrap_or(false);
        if !allowed {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "rejected cross-origin browser request to localhost MCP endpoint"
                })),
            ));
        }
    }

    Ok(())
}

fn negotiated_header_version(headers: &HeaderMap) -> &'static str {
    if let Some(raw) = headers
        .get(HEADER_PROTOCOL_VERSION)
        .and_then(|v| v.to_str().ok())
    {
        for &supported in SUPPORTED_PROTOCOL_VERSIONS {
            if supported == raw.trim() {
                return supported;
            }
        }
    }
    DEFAULT_PROTOCOL_VERSION
}

fn apply_mcp_headers(
    headers: &mut HeaderMap,
    protocol_version: &'static str,
    session_id: Option<&str>,
) {
    headers.insert(
        HeaderName::from_static(HEADER_PROTOCOL_VERSION),
        HeaderValue::from_static(protocol_version),
    );
    if let Some(sid) = session_id {
        if let Ok(val) = HeaderValue::from_str(sid) {
            headers.insert(HeaderName::from_static(HEADER_SESSION_ID), val);
        }
    }
}

// ----------------------------------------------------------------------------
// Route Handlers
// ----------------------------------------------------------------------------

async fn mcp_get_handler(State(state): State<McpHttpState>, headers: HeaderMap) -> Response {
    if let Err(e) = check_loopback_security(&headers, &state.bind_host) {
        return e.into_response();
    }
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST, DELETE")],
        Json(json!({
            "error": "GET SSE stream is not offered; use POST /mcp (with Accept: text/event-stream for progress streaming) or DELETE /mcp."
        })),
    )
        .into_response()
}

async fn mcp_delete_handler(State(state): State<McpHttpState>, headers: HeaderMap) -> Response {
    if let Err(e) = check_loopback_security(&headers, &state.bind_host) {
        return e.into_response();
    }
    if let Some(sid) = headers.get(HEADER_SESSION_ID).and_then(|v| v.to_str().ok()) {
        state.remove_session(sid.trim());
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn mcp_post_handler(
    State(state): State<McpHttpState>,
    headers: HeaderMap,
    payload: Result<Json<IncomingMessage>, JsonRejection>,
) -> Response {
    if let Err(e) = check_loopback_security(&headers, &state.bind_host) {
        return e.into_response();
    }

    let Json(msg) = match payload {
        Ok(m) => m,
        Err(e) => {
            let err_resp = JsonRpcResponse::err(None, ERR_PARSE, format!("JSON parse error: {e}"));
            let mut resp = (StatusCode::BAD_REQUEST, Json(err_resp)).into_response();
            apply_mcp_headers(resp.headers_mut(), DEFAULT_PROTOCOL_VERSION, None);
            return resp;
        }
    };

    let is_initialize = msg.method.as_deref() == Some("initialize");
    let incoming_sid = headers
        .get(HEADER_SESSION_ID)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Validate session ID if caller provided one on a non-initialize request.
    // Callers that omit `Mcp-Session-Id` work seamlessly in stateless mode.
    if !is_initialize {
        if let Some(ref sid) = incoming_sid {
            if !state.touch_session(sid) {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "error": "unknown or expired Mcp-Session-Id; re-initialize session"
                    })),
                )
                    .into_response();
            }
        }
    }

    // Notifications (no `id`) -> 202 Accepted.
    let Some(req_id) = msg.id.clone() else {
        if msg.method.as_deref() == Some("notifications/cancelled") {
            if let Some(cancel_id) = extract_cancelled_id(msg.params.as_ref()) {
                if let Some(handle) = state.in_flight.lock().await.remove(&cancel_id) {
                    tracing::debug!("MCP HTTP cancelling in-flight request {:?}", cancel_id);
                    handle.abort();
                }
            }
        } else {
            let _ = state.server.handle_message(msg, None).await;
        }
        let mut resp = StatusCode::ACCEPTED.into_response();
        apply_mcp_headers(
            resp.headers_mut(),
            negotiated_header_version(&headers),
            incoming_sid.as_deref(),
        );
        return resp;
    };

    let accepts_sse = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.to_ascii_lowercase().contains("text/event-stream"));
    let wants_progress = extract_progress_token(msg.params.as_ref()).is_some();

    if accepts_sse && wants_progress {
        let protocol_version = negotiated_header_version(&headers);
        let session_for_header = incoming_sid.clone();
        let (sse_tx, sse_rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);
        let state_clone = state.clone();
        let id_key = req_id.clone();

        let worker = tokio::spawn(async move {
            let (ptx, mut prx) = tokio::sync::mpsc::channel::<JsonRpcNotification>(32);
            let fwd_tx = sse_tx.clone();
            let fwd = tokio::spawn(async move {
                while let Some(notif) = prx.recv().await {
                    if let Ok(data) = serde_json::to_string(&notif) {
                        if fwd_tx
                            .send(Ok(Event::default().event("message").data(data)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            });

            let rpc_resp = state_clone.server.handle_message(msg, Some(ptx)).await;
            let _ = fwd.await;
            state_clone.in_flight.lock().await.remove(&id_key);

            if let Some(r) = rpc_resp {
                if let Ok(data) = serde_json::to_string(&r) {
                    let _ = sse_tx
                        .send(Ok(Event::default().event("message").data(data)))
                        .await;
                }
            }
        });

        state
            .in_flight
            .lock()
            .await
            .insert(req_id, worker.abort_handle());

        let stream = tokio_stream::wrappers::ReceiverStream::new(sse_rx);
        let mut resp = Sse::new(stream)
            .keep_alive(KeepAlive::new())
            .into_response();
        apply_mcp_headers(
            resp.headers_mut(),
            protocol_version,
            session_for_header.as_deref(),
        );
        return resp;
    }

    // Standard JSON request/response path.
    let rpc_resp = state.server.handle_message(msg, None).await;
    let session_for_header = if is_initialize {
        Some(state.create_session())
    } else {
        incoming_sid
    };
    let protocol_version = if is_initialize {
        rpc_resp
            .as_ref()
            .and_then(|r| r.result.as_ref())
            .and_then(|res| res.get("protocolVersion"))
            .and_then(|v| v.as_str())
            .and_then(|s| {
                SUPPORTED_PROTOCOL_VERSIONS
                    .iter()
                    .copied()
                    .find(|&v| v == s)
            })
            .unwrap_or(DEFAULT_PROTOCOL_VERSION)
    } else {
        negotiated_header_version(&headers)
    };

    match rpc_resp {
        Some(r) => {
            let mut resp = (StatusCode::OK, Json(r)).into_response();
            apply_mcp_headers(
                resp.headers_mut(),
                protocol_version,
                session_for_header.as_deref(),
            );
            resp
        }
        None => {
            let mut resp = StatusCode::ACCEPTED.into_response();
            apply_mcp_headers(
                resp.headers_mut(),
                protocol_version,
                session_for_header.as_deref(),
            );
            resp
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn_test_mcp_http() -> String {
        let app = mcp_routes(McpServer::default(), "127.0.0.1");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local mcp http listener");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}/mcp")
    }

    #[tokio::test]
    async fn streamable_http_full_lifecycle_sessions_sse_and_security_guards() {
        let url = spawn_test_mcp_http().await;
        let client = reqwest::Client::new();

        // 1. GET /mcp -> 405 Method Not Allowed with Allow: POST, DELETE
        let get_resp = client.get(&url).send().await.unwrap();
        assert_eq!(get_resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            get_resp
                .headers()
                .get(header::ALLOW)
                .and_then(|v| v.to_str().ok()),
            Some("POST, DELETE")
        );

        // 2. Cross-origin browser Origin header -> 403 Forbidden
        let evil_origin = client
            .post(&url)
            .header("Origin", "https://evil.example")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(evil_origin.status(), StatusCode::FORBIDDEN);

        // 3. DNS-rebinding Host header -> 403 Forbidden
        let evil_host = client
            .post(&url)
            .header("Host", "rebind.evil.example:8000")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(evil_host.status(), StatusCode::FORBIDDEN);

        // 4. Malformed JSON -> 400 Bad Request with JSON-RPC -32700
        let bad_json = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body("{not valid")
            .send()
            .await
            .unwrap();
        assert_eq!(bad_json.status(), StatusCode::BAD_REQUEST);
        let bad_body: Value = bad_json.json().await.unwrap();
        assert_eq!(bad_body["error"]["code"], ERR_PARSE);

        // 5. Initialize -> 200 OK + Mcp-Session-Id + MCP-Protocol-Version
        let init_resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "http-test", "version": "1.0" } }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(init_resp.status(), StatusCode::OK);
        let session_id = init_resp
            .headers()
            .get(HEADER_SESSION_ID)
            .expect("initialize must emit Mcp-Session-Id")
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            init_resp
                .headers()
                .get(HEADER_PROTOCOL_VERSION)
                .and_then(|v| v.to_str().ok()),
            Some("2025-06-18")
        );
        let init_json: Value = init_resp.json().await.unwrap();
        assert_eq!(init_json["result"]["serverInfo"]["name"], "searchwala");

        // 6. Notification (`notifications/initialized`) -> 202 Accepted
        let notif_resp = client
            .post(&url)
            .header(HEADER_SESSION_ID, &session_id)
            .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .send()
            .await
            .unwrap();
        assert_eq!(notif_resp.status(), StatusCode::ACCEPTED);

        // 7. tools/list + tools/call(search_status) using active session
        let list_resp: Value = client
            .post(&url)
            .header(HEADER_SESSION_ID, &session_id)
            .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list_resp["result"]["tools"].as_array().unwrap().len(), 4);

        let status_resp: Value = client
            .post(&url)
            .header(HEADER_SESSION_ID, &session_id)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": { "name": "search_status" }
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            status_resp["result"]["structuredContent"]["version"],
            crate::config::VERSION
        );

        // 8. Stateless request (no Mcp-Session-Id header) works seamlessly
        let stateless_ping: Value = client
            .post(&url)
            .json(&json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(stateless_ping["result"], json!({}));

        // 9. SSE progress streaming when Accept: text/event-stream + progressToken
        let sse_resp = client
            .post(&url)
            .header(header::ACCEPT, "text/event-stream")
            .header(HEADER_SESSION_ID, &session_id)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "_test/sleep",
                "params": { "ms": 20, "_meta": { "progressToken": "sse-tok" } }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(sse_resp.status(), StatusCode::OK);
        let ct = sse_resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.contains("text/event-stream"), "got content-type: {ct}");
        let sse_body = sse_resp.text().await.unwrap();
        assert!(
            sse_body.contains("notifications/progress") && sse_body.contains("sse-tok"),
            "SSE stream must emit progress notification: {sse_body}"
        );
        assert!(
            sse_body.contains("\"slept_ms\":20"),
            "SSE stream must emit final JSON-RPC response: {sse_body}"
        );

        // 10. DELETE /mcp terminates session; reusing terminated session returns 404
        let del = client
            .delete(&url)
            .header(HEADER_SESSION_ID, &session_id)
            .send()
            .await
            .unwrap();
        assert_eq!(del.status(), StatusCode::NO_CONTENT);

        let expired = client
            .post(&url)
            .header(HEADER_SESSION_ID, &session_id)
            .json(&json!({ "jsonrpc": "2.0", "id": 6, "method": "ping" }))
            .send()
            .await
            .unwrap();
        assert_eq!(expired.status(), StatusCode::NOT_FOUND);
    }
}
