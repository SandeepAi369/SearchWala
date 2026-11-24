// ============================================================================
// SearchWala MCP — Stdio Transport (JSON-RPC 2.0 over stdin/stdout)
//
// Design invariants:
//   * Single-line newline-delimited JSON-RPC 2.0 frames on stdout.
//   * All logs go strictly to stderr (configured by the CLI entry point).
//   * Cancel-safe `Lines::next_line()` inside `tokio::select!` reaps finished
//     tasks continuously so long-lived agent sessions never accumulate handles.
//   * Requests execute concurrently; `notifications/cancelled` aborts the
//     matching in-flight task immediately and suppresses its reply.
//   * Progress notifications (`notifications/progress`) are guaranteed to flush
//     before the final JSON-RPC response for the same request.
//   * Clean EOF on stdin drains non-cancelled in-flight tasks before exiting so
//     piped / batch invocations never lose responses.
// ============================================================================

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::task::{AbortHandle, JoinSet};

use crate::mcp::protocol::{
    extract_cancelled_id, extract_progress_token, IncomingMessage, JsonRpcNotification,
    JsonRpcResponse, RequestId, ERR_PARSE,
};
use crate::mcp::McpServer;

/// Run the MCP stdio server on process `stdin` and `stdout`.
pub async fn run_stdio(server: McpServer) -> std::io::Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_io(server, stdin, stdout).await
}

async fn write_json_line<W, T>(writer: &Arc<Mutex<W>>, msg: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_string(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    let mut guard = writer.lock().await;
    guard.write_all(line.as_bytes()).await?;
    guard.flush().await
}

/// Core async stdio loop over arbitrary `AsyncBufRead` / `AsyncWrite` streams.
pub async fn serve_io<R, W>(server: McpServer, reader: R, writer: W) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let writer = Arc::new(Mutex::new(writer));
    let in_flight: Arc<Mutex<HashMap<RequestId, AbortHandle>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut tasks: JoinSet<()> = JoinSet::new();
    let mut lines = reader.lines();

    loop {
        let next_line = tokio::select! {
            res = lines.next_line() => res,
            Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(e) = joined {
                    if e.is_panic() {
                        tracing::error!("MCP stdio worker panicked: {e}");
                    }
                }
                continue;
            }
        };

        let raw_line = match next_line {
            Ok(Some(l)) => l,
            Ok(None) => break, // clean EOF on stdin
            Err(e) => {
                tracing::error!("MCP stdio read error: {e}");
                return Err(e);
            }
        };

        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let msg: IncomingMessage = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(e) => {
                let err_resp =
                    JsonRpcResponse::err(None, ERR_PARSE, format!("JSON parse error: {e}"));
                let _ = write_json_line(&writer, &err_resp).await;
                continue;
            }
        };

        // Handle cancellation notification directly so running tasks abort immediately.
        if msg.id.is_none() && msg.method.as_deref() == Some("notifications/cancelled") {
            if let Some(cancel_id) = extract_cancelled_id(msg.params.as_ref()) {
                if let Some(handle) = in_flight.lock().await.remove(&cancel_id) {
                    tracing::debug!("MCP cancelling in-flight request {:?}", cancel_id);
                    handle.abort();
                }
            }
            continue;
        }

        // Other notifications (e.g. `notifications/initialized`) produce no response.
        let Some(req_id) = msg.id.clone() else {
            let _ = server.handle_message(msg, None).await;
            continue;
        };

        let server_clone = server.clone();
        let writer_clone = Arc::clone(&writer);
        let in_flight_clone = Arc::clone(&in_flight);
        let id_for_task = req_id.clone();

        let abort_handle = tasks.spawn(async move {
            let wants_progress = extract_progress_token(msg.params.as_ref()).is_some();
            let (progress_tx, forwarder) = if wants_progress {
                let (tx, mut rx) = tokio::sync::mpsc::channel::<JsonRpcNotification>(32);
                let w = Arc::clone(&writer_clone);
                let fwd = tokio::spawn(async move {
                    while let Some(notif) = rx.recv().await {
                        if write_json_line(&w, &notif).await.is_err() {
                            break;
                        }
                    }
                });
                (Some(tx), Some(fwd))
            } else {
                (None, None)
            };

            let resp = server_clone.handle_message(msg, progress_tx).await;

            // Ensure all progress notifications flush before the final response.
            if let Some(fwd) = forwarder {
                let _ = fwd.await;
            }

            in_flight_clone.lock().await.remove(&id_for_task);

            if let Some(r) = resp {
                if let Err(e) = write_json_line(&writer_clone, &r).await {
                    tracing::debug!("MCP stdout write failed: {e}");
                }
            }
        });

        in_flight.lock().await.insert(req_id, abort_handle);
    }

    // Drain any still-running non-cancelled requests before closing stdout.
    while let Some(joined) = tasks.join_next().await {
        if let Err(e) = joined {
            if e.is_panic() {
                tracing::error!("MCP stdio worker panicked during drain: {e}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[tokio::test]
    async fn duplex_session_handles_handshake_tools_resources_and_errors() {
        let (mut client_write, server_read) = tokio::io::duplex(16 * 1024);
        let (server_write, client_read) = tokio::io::duplex(64 * 1024);

        let server_task = tokio::spawn(async move {
            serve_io(
                McpServer::default(),
                BufReader::new(server_read),
                server_write,
            )
            .await
            .expect("serve_io clean shutdown");
        });

        let requests = [
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "duplex-test", "version": "1.0" } }
            })
            .to_string(),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }).to_string(),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }).to_string(),
            json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "search_status" } }).to_string(),
            json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": { "name": "extract_url", "arguments": { "url": "http://127.0.0.1:8000/api/chats" } }
            })
            .to_string(),
            json!({ "jsonrpc": "2.0", "id": 6, "method": "resources/read", "params": { "uri": "searchwala://config/modes" } }).to_string(),
            "{not valid json".to_string(),
        ];

        for line in &requests {
            client_write
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
        }
        client_write.shutdown().await.unwrap();
        drop(client_write);

        let mut reader = BufReader::new(client_read).lines();
        let mut responses: Vec<Value> = Vec::new();
        while let Some(line) = reader.next_line().await.unwrap() {
            let v: Value =
                serde_json::from_str(&line).expect("every stdout line must be valid JSON");
            responses.push(v);
        }
        server_task.await.unwrap();

        // 7 replies expected (notifications/initialized produces none).
        assert_eq!(responses.len(), 7, "got responses: {responses:?}");

        let by_id = |id: Value| -> &Value {
            responses
                .iter()
                .find(|r| r["id"] == id)
                .unwrap_or_else(|| panic!("missing response for id={id}: {responses:?}"))
        };

        assert_eq!(
            by_id(json!(1))["result"]["serverInfo"]["name"],
            "searchwala"
        );
        assert_eq!(by_id(json!(2))["result"], json!({}));
        assert_eq!(
            by_id(json!(3))["result"]["tools"].as_array().unwrap().len(),
            4
        );
        assert_eq!(
            by_id(json!(4))["result"]["structuredContent"]["version"],
            crate::config::VERSION
        );
        assert_eq!(by_id(json!(5))["result"]["isError"], true);
        assert!(by_id(json!(5))["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("SSRF"));
        assert!(by_id(json!(6))["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("fast"));
        assert_eq!(by_id(Value::Null)["error"]["code"], ERR_PARSE);
    }

    #[tokio::test]
    async fn duplex_concurrency_progress_and_cancellation() {
        let (mut client_write, server_read) = tokio::io::duplex(16 * 1024);
        let (server_write, client_read) = tokio::io::duplex(16 * 1024);

        let server_task = tokio::spawn(async move {
            serve_io(
                McpServer::default(),
                BufReader::new(server_read),
                server_write,
            )
            .await
            .unwrap();
        });

        let mut reader = BufReader::new(client_read).lines();

        // Start a 5s sleep request (id=100) with a progressToken.
        let slow_req = json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "_test/sleep",
            "params": { "ms": 5000, "_meta": { "progressToken": "p-100" } }
        });
        client_write
            .write_all(format!("{slow_req}\n").as_bytes())
            .await
            .unwrap();
        client_write.flush().await.unwrap();

        // First line back should be the progress notification from id=100.
        let prog_line = tokio::time::timeout(std::time::Duration::from_secs(2), reader.next_line())
            .await
            .expect("progress notification timely")
            .unwrap()
            .unwrap();
        let prog_val: Value = serde_json::from_str(&prog_line).unwrap();
        assert_eq!(prog_val["method"], "notifications/progress");
        assert_eq!(prog_val["params"]["progressToken"], "p-100");

        // Send a concurrent ping (id=101) while id=100 is still sleeping — must reply immediately.
        let ping_req = json!({ "jsonrpc": "2.0", "id": 101, "method": "ping" });
        client_write
            .write_all(format!("{ping_req}\n").as_bytes())
            .await
            .unwrap();
        client_write.flush().await.unwrap();

        let ping_line = tokio::time::timeout(std::time::Duration::from_secs(1), reader.next_line())
            .await
            .expect("concurrent ping must not block on slow request")
            .unwrap()
            .unwrap();
        let ping_val: Value = serde_json::from_str(&ping_line).unwrap();
        assert_eq!(ping_val["id"], 101);
        assert_eq!(ping_val["result"], json!({}));

        // Cancel id=100 and close stdin — server must abort id=100 and exit promptly without replying to id=100.
        let cancel = json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": 100 }
        });
        client_write
            .write_all(format!("{cancel}\n").as_bytes())
            .await
            .unwrap();
        client_write.shutdown().await.unwrap();
        drop(client_write);

        let extra = tokio::time::timeout(std::time::Duration::from_secs(2), reader.next_line())
            .await
            .expect("cancelled task must finish quickly, not sleep 5s")
            .unwrap();
        assert!(
            extra.is_none(),
            "cancelled request must not send a response, got: {extra:?}"
        );
        server_task.await.unwrap();
    }
}
