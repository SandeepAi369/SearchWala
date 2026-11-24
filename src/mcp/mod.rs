// ============================================================================
// SearchWala MCP — Server core, resources & JSON-RPC method dispatch
//
// Shared by both transports (stdio in Chunk 3, Streamable HTTP in Chunk 4).
// Stateless with respect to SQLite chat history: MCP calls never touch
// `~/.searchwala/searchwala.db`, while reusing the in-memory SERP/page caches
// and TempDb session tracker.
// ============================================================================

pub mod http;
pub mod protocol;
pub mod stdio;
pub mod tools;

use serde_json::json;

use crate::cache::TempDb;
use crate::mcp::protocol::{
    extract_progress_token, negotiate_protocol_version, CallToolParams, IncomingMessage,
    InitializeParams, InitializeResult, JsonRpcResponse, ReadResourceParams, ResourceContents,
    ResourceDescriptor, ResourcesCapability, ServerCapabilities, ServerImplementation,
    ToolsCapability, ERR_INVALID_PARAMS, ERR_INVALID_REQUEST, ERR_METHOD_NOT_FOUND,
    ERR_RESOURCE_NOT_FOUND,
};
use crate::mcp::tools::ProgressSender;

pub const RESOURCE_MODES_URI: &str = "searchwala://config/modes";
pub const RESOURCE_ENGINES_URI: &str = "searchwala://health/engines";
pub const RESOURCE_CACHE_URI: &str = "searchwala://health/cache";

const SERVER_INSTRUCTIONS: &str = "\
SearchWala is a private, local Rust meta-search and deep content-extraction engine (zero API keys required). \
Use `search_web` for fast multi-engine consensus discovery and `search_passages` for verified BM25+/MMR-ranked passages with a verbatim quick-answer highlight. \
After a search returns SearchWala's consensus results and its compact list of additional discovered domains, you have two paths: \
(1) answer directly using SearchWala's consensus results/passages when sufficient, or \
(2) optionally drill into specific domains from the discovered list via `search_passages` with `include_domains` (default `strict_domains: false` prioritizes them while keeping organic answers; set `strict_domains: true` to restrict strictly to those domains) or `extract_url` with `query`. \
Use `exclude_domains` to drop noisy sources before scraping, and `search_status` (or `searchwala://` resources) to inspect engine health and cache stats.";

/// Core MCP request dispatcher shared across stdio and Streamable HTTP transports.
#[derive(Debug, Clone)]
pub struct McpServer {
    temp_db: TempDb,
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new(TempDb::new())
    }
}

impl McpServer {
    pub fn new(temp_db: TempDb) -> Self {
        Self { temp_db }
    }

    pub fn temp_db(&self) -> &TempDb {
        &self.temp_db
    }

    /// List all read-only MCP resources exposed by SearchWala.
    pub fn list_resources() -> Vec<ResourceDescriptor> {
        vec![
            ResourceDescriptor {
                uri: RESOURCE_MODES_URI,
                name: "modes",
                title: "SearchWala Mode Profiles",
                description: "JSON summary of all 7 search modes (fast, auto, deep, study, academic, reddit, youtube) and their default engine, scraping, and ranking budgets.",
                mime_type: "application/json",
            },
            ResourceDescriptor {
                uri: RESOURCE_ENGINES_URI,
                name: "engines",
                title: "Search Engine Health",
                description: "Live per-engine attempt counts, HTTP successes, blocks, transport errors, zero-result counts, and health verdicts since server start.",
                mime_type: "application/json",
            },
            ResourceDescriptor {
                uri: RESOURCE_CACHE_URI,
                name: "cache",
                title: "SERP & Page Cache Stats",
                description: "Live in-memory Moka SERP (10m TTL) and extracted-page (6h TTL) cache entry counts and hit ratios.",
                mime_type: "application/json",
            },
        ]
    }

    /// Read one resource URI as pretty-printed JSON.
    pub fn read_resource(uri: &str) -> Result<ResourceContents, String> {
        let payload = match uri.trim() {
            RESOURCE_MODES_URI => json!({
                "version": crate::config::VERSION,
                "modes": crate::modes::describe_modes(),
            }),
            RESOURCE_ENGINES_URI => {
                crate::engines::health::register_known(&crate::modes::all_roster_engines());
                let snap = crate::engines::health::snapshot();
                let mut summary: std::collections::BTreeMap<&'static str, usize> =
                    std::collections::BTreeMap::new();
                for r in &snap {
                    *summary.entry(r.verdict).or_insert(0) += 1;
                }
                json!({
                    "summary": summary,
                    "engines": snap,
                })
            }
            RESOURCE_CACHE_URI => {
                serde_json::to_value(crate::cache_layer::stats()).unwrap_or_else(|_| json!({}))
            }
            other => return Err(format!("unknown resource URI: {other}")),
        };
        let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string());
        Ok(ResourceContents {
            uri: uri.trim().to_string(),
            mime_type: "application/json",
            text,
        })
    }

    /// Dispatch a single parsed JSON-RPC 2.0 message.
    ///
    /// Returns `None` for notifications (messages without an `id`), or
    /// `Some(JsonRpcResponse)` for requests.
    pub async fn handle_message(
        &self,
        msg: IncomingMessage,
        progress_tx: Option<ProgressSender>,
    ) -> Option<JsonRpcResponse> {
        if msg.jsonrpc != "2.0" {
            return Some(JsonRpcResponse::err(
                msg.id,
                ERR_INVALID_REQUEST,
                "jsonrpc must be \"2.0\"",
            ));
        }

        let Some(method) = msg.method else {
            // Client response / malformed frame with no method.
            return msg.id.map(|id| {
                JsonRpcResponse::err(Some(id), ERR_INVALID_REQUEST, "missing method field")
            });
        };

        // Notifications have no `id` and must never receive a JSON-RPC reply.
        let Some(id) = msg.id else {
            match method.as_str() {
                "notifications/initialized" | "notifications/cancelled" => {
                    tracing::debug!("MCP notification received: {method}");
                }
                other => {
                    tracing::debug!("Ignoring unknown MCP notification: {other}");
                }
            }
            return None;
        };

        let resp = match method.as_str() {
            "initialize" => {
                let init_params: InitializeParams = match msg.params {
                    Some(v) => match serde_json::from_value(v) {
                        Ok(p) => p,
                        Err(e) => {
                            return Some(JsonRpcResponse::err(
                                Some(id),
                                ERR_INVALID_PARAMS,
                                format!("invalid initialize params: {e}"),
                            ));
                        }
                    },
                    None => InitializeParams::default(),
                };
                let protocol_version =
                    negotiate_protocol_version(init_params.protocol_version.as_deref());
                let res = InitializeResult {
                    protocol_version,
                    capabilities: ServerCapabilities {
                        tools: ToolsCapability {
                            list_changed: false,
                        },
                        resources: ResourcesCapability {
                            subscribe: false,
                            list_changed: false,
                        },
                    },
                    server_info: ServerImplementation {
                        name: "searchwala",
                        title: "SearchWala",
                        version: crate::config::VERSION,
                    },
                    instructions: SERVER_INSTRUCTIONS,
                };
                JsonRpcResponse::ok(id, serde_json::to_value(res).unwrap_or_else(|_| json!({})))
            }
            "ping" => JsonRpcResponse::ok(id, json!({})),
            "tools/list" => JsonRpcResponse::ok(id, json!({ "tools": tools::list_tools() })),
            "tools/call" => {
                let progress_token = extract_progress_token(msg.params.as_ref());
                let call_params: CallToolParams = match msg.params {
                    Some(v) => match serde_json::from_value(v) {
                        Ok(p) => p,
                        Err(e) => {
                            return Some(JsonRpcResponse::err(
                                Some(id),
                                ERR_INVALID_PARAMS,
                                format!("invalid tools/call params: {e}"),
                            ));
                        }
                    },
                    None => {
                        return Some(JsonRpcResponse::err(
                            Some(id),
                            ERR_INVALID_PARAMS,
                            "missing params for tools/call",
                        ));
                    }
                };
                match tools::call_tool(
                    &call_params.name,
                    call_params.arguments,
                    &self.temp_db,
                    progress_token,
                    progress_tx,
                )
                .await
                {
                    Ok(result) => JsonRpcResponse::ok(
                        id,
                        serde_json::to_value(result).unwrap_or_else(|_| json!({})),
                    ),
                    Err(err_msg) => JsonRpcResponse::err(Some(id), ERR_INVALID_PARAMS, err_msg),
                }
            }
            "resources/list" => {
                JsonRpcResponse::ok(id, json!({ "resources": Self::list_resources() }))
            }
            "resources/templates/list" => {
                JsonRpcResponse::ok(id, json!({ "resourceTemplates": [] }))
            }
            "resources/read" => {
                let read_params: ReadResourceParams = match msg.params {
                    Some(v) => match serde_json::from_value(v) {
                        Ok(p) => p,
                        Err(e) => {
                            return Some(JsonRpcResponse::err(
                                Some(id),
                                ERR_INVALID_PARAMS,
                                format!("invalid resources/read params: {e}"),
                            ));
                        }
                    },
                    None => {
                        return Some(JsonRpcResponse::err(
                            Some(id),
                            ERR_INVALID_PARAMS,
                            "missing params for resources/read",
                        ));
                    }
                };
                match Self::read_resource(&read_params.uri) {
                    Ok(contents) => JsonRpcResponse::ok(id, json!({ "contents": [contents] })),
                    Err(e) => JsonRpcResponse::err(Some(id), ERR_RESOURCE_NOT_FOUND, e),
                }
            }
            "prompts/list" => JsonRpcResponse::ok(id, json!({ "prompts": [] })),
            #[cfg(test)]
            "_test/sleep" => {
                let ms = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("ms"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50);
                if let (Some(tok), Some(tx)) =
                    (extract_progress_token(msg.params.as_ref()), progress_tx)
                {
                    let _ = tx
                        .send(crate::mcp::protocol::JsonRpcNotification::progress(
                            tok,
                            1,
                            Some(2),
                            "sleeping",
                        ))
                        .await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                JsonRpcResponse::ok(id, json!({ "slept_ms": ms }))
            }
            other => JsonRpcResponse::err(
                Some(id),
                ERR_METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            ),
        };

        Some(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::protocol::RequestId;
    use serde_json::Value;

    fn msg(raw: Value) -> IncomingMessage {
        serde_json::from_value(raw).expect("valid IncomingMessage JSON")
    }

    #[tokio::test]
    async fn full_lifecycle_handshake_list_and_resources_offline() {
        let server = McpServer::default();

        // 1. initialize
        let init_resp = server
            .handle_message(
                msg(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": { "name": "test-client", "version": "1.0.0" }
                    }
                })),
                None,
            )
            .await
            .expect("initialize returns response");
        assert!(init_resp.error.is_none());
        let res = init_resp.result.unwrap();
        assert_eq!(res["protocolVersion"], "2025-06-18");
        assert_eq!(res["serverInfo"]["name"], "searchwala");
        assert_eq!(res["serverInfo"]["version"], crate::config::VERSION);
        assert!(res["capabilities"]["tools"].is_object());
        assert!(res["capabilities"]["resources"].is_object());

        // 2. notifications/initialized produces NO response
        let notif_resp = server
            .handle_message(
                msg(json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                })),
                None,
            )
            .await;
        assert!(notif_resp.is_none());

        // 3. ping
        let ping_resp = server
            .handle_message(
                msg(json!({ "jsonrpc": "2.0", "id": "p1", "method": "ping" })),
                None,
            )
            .await
            .unwrap();
        assert_eq!(ping_resp.id, Some(RequestId::String("p1".into())));
        assert_eq!(ping_resp.result, Some(json!({})));

        // 4. tools/list
        let tools_resp = server
            .handle_message(
                msg(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })),
                None,
            )
            .await
            .unwrap();
        let tools = tools_resp.result.unwrap()["tools"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(tools.len(), 4);

        // 5. tools/call search_status
        let status_resp = server
            .handle_message(
                msg(json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": { "name": "search_status" }
                })),
                None,
            )
            .await
            .unwrap();
        let sres = status_resp.result.unwrap();
        assert!(sres.get("isError").is_none());
        assert_eq!(sres["structuredContent"]["version"], crate::config::VERSION);

        // 6. resources/list & resources/read for all 3 URIs
        let rlist = server
            .handle_message(
                msg(json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/list" })),
                None,
            )
            .await
            .unwrap();
        let resources = rlist.result.unwrap()["resources"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(resources.len(), 3);

        for uri in [RESOURCE_MODES_URI, RESOURCE_ENGINES_URI, RESOURCE_CACHE_URI] {
            let rread = server
                .handle_message(
                    msg(json!({
                        "jsonrpc": "2.0",
                        "id": 5,
                        "method": "resources/read",
                        "params": { "uri": uri }
                    })),
                    None,
                )
                .await
                .unwrap();
            assert!(rread.error.is_none(), "reading {uri} failed");
            let text = rread.result.unwrap()["contents"][0]["text"]
                .as_str()
                .unwrap()
                .to_string();
            let parsed: Value = serde_json::from_str(&text).expect("resource must be valid JSON");
            assert!(parsed.is_object());
        }

        // 7. Unknown resource returns -32002
        let bad_res = server
            .handle_message(
                msg(json!({
                    "jsonrpc": "2.0",
                    "id": 6,
                    "method": "resources/read",
                    "params": { "uri": "searchwala://does/not/exist" }
                })),
                None,
            )
            .await
            .unwrap();
        assert_eq!(bad_res.error.unwrap().code, ERR_RESOURCE_NOT_FOUND);

        // 8. Optional probe methods return empty lists cleanly (no -32601 noise)
        let prompts = server
            .handle_message(
                msg(json!({ "jsonrpc": "2.0", "id": 7, "method": "prompts/list" })),
                None,
            )
            .await
            .unwrap();
        assert_eq!(prompts.result.unwrap()["prompts"], json!([]));

        let tmpls = server
            .handle_message(
                msg(json!({ "jsonrpc": "2.0", "id": 8, "method": "resources/templates/list" })),
                None,
            )
            .await
            .unwrap();
        assert_eq!(tmpls.result.unwrap()["resourceTemplates"], json!([]));

        // 9. Unknown method returns -32601; bad jsonrpc version returns -32600
        let bad_method = server
            .handle_message(
                msg(json!({ "jsonrpc": "2.0", "id": 9, "method": "does/not/exist" })),
                None,
            )
            .await
            .unwrap();
        assert_eq!(bad_method.error.unwrap().code, ERR_METHOD_NOT_FOUND);

        let bad_ver = server
            .handle_message(
                msg(json!({ "jsonrpc": "1.0", "id": 10, "method": "ping" })),
                None,
            )
            .await
            .unwrap();
        assert_eq!(bad_ver.error.unwrap().code, ERR_INVALID_REQUEST);
    }
}
