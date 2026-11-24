// ============================================================================
// SearchWala MCP — JSON-RPC 2.0 & Model Context Protocol wire types
//
// Zero-dependency implementation aligned with SearchWala's lean architecture.
// Supports MCP protocol revisions 2025-11-25, 2025-06-18, 2025-03-26, and
// 2024-11-05 (including structuredContent, outputSchema, annotations,
// notifications/progress, notifications/cancelled, and read-only resources).
// ============================================================================

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Latest MCP protocol revision advertised when the client's requested version
/// is unknown or omitted.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// Protocol revisions accepted verbatim during `initialize` negotiation.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

// Standard JSON-RPC 2.0 & MCP error codes
pub const ERR_PARSE: i64 = -32700;
pub const ERR_INVALID_REQUEST: i64 = -32600;
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERR_INVALID_PARAMS: i64 = -32602;
pub const ERR_INTERNAL: i64 = -32603;
pub const ERR_RESOURCE_NOT_FOUND: i64 = -32002;

/// Negotiate the MCP protocol version per spec: echo the client's version when
/// supported, otherwise respond with our default latest supported version.
pub fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    if let Some(req) = requested {
        let trimmed = req.trim();
        for &ver in SUPPORTED_PROTOCOL_VERSIONS {
            if ver == trimmed {
                return ver;
            }
        }
    }
    DEFAULT_PROTOCOL_VERSION
}

/// JSON-RPC 2.0 request identifier (string or integer).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    Int(i64),
    Uint(u64),
}

/// Incoming JSON-RPC 2.0 message (request or notification).
#[derive(Debug, Clone, Deserialize)]
pub struct IncomingMessage {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<RequestId>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
}

/// Outgoing JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Outgoing JSON-RPC 2.0 response envelope.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Option<RequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn ok(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id: Some(id),
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Option<RequestId>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Outgoing JSON-RPC 2.0 server notification (no `id`).
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub params: Value,
}

impl JsonRpcNotification {
    pub fn progress(
        progress_token: Value,
        progress: u64,
        total: Option<u64>,
        message: impl Into<String>,
    ) -> Self {
        let mut params = serde_json::Map::new();
        params.insert("progressToken".into(), progress_token);
        params.insert("progress".into(), json!(progress));
        if let Some(t) = total {
            params.insert("total".into(), json!(t));
        }
        params.insert("message".into(), json!(message.into()));
        Self {
            jsonrpc: "2.0",
            method: "notifications/progress",
            params: Value::Object(params),
        }
    }
}

/// Extract `_meta.progressToken` from request `params` if present and valid
/// (string or number per MCP spec).
pub fn extract_progress_token(params: Option<&Value>) -> Option<Value> {
    let token = params?.get("_meta")?.get("progressToken")?;
    if token.is_string() || token.is_number() {
        Some(token.clone())
    } else {
        None
    }
}

/// Extract `requestId` from `notifications/cancelled` params.
pub fn extract_cancelled_id(params: Option<&Value>) -> Option<RequestId> {
    let raw = params?.get("requestId")?.clone();
    serde_json::from_value::<RequestId>(raw).ok()
}

// ----------------------------------------------------------------------------
// MCP Initialize, Tool, and Resource payload types
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    #[serde(default)]
    pub protocol_version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerImplementation {
    pub name: &'static str,
    pub title: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    pub list_changed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesCapability {
    pub subscribe: bool,
    pub list_changed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    pub tools: ToolsCapability,
    pub resources: ResourcesCapability,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: &'static str,
    pub capabilities: ServerCapabilities,
    pub server_info: ServerImplementation,
    pub instructions: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    pub title: &'static str,
    pub read_only_hint: bool,
    pub destructive_hint: bool,
    pub idempotent_hint: bool,
    pub open_world_hint: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub output_schema: Value,
    pub annotations: ToolAnnotations,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextContent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
}

impl TextContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            kind: "text",
            text: text.into(),
        }
    }
}

/// MCP `tools/call` result payload carrying both human/LLM-readable `content`
/// and machine-typed `structuredContent`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    pub content: Vec<TextContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

impl CallToolResult {
    pub fn success(text: impl Into<String>, structured: Value) -> Self {
        Self {
            content: vec![TextContent::new(text)],
            structured_content: Some(structured),
            is_error: false,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![TextContent::new(message)],
            structured_content: None,
            is_error: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDescriptor {
    pub uri: &'static str,
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub mime_type: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceContents {
    pub uri: String,
    pub mime_type: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadResourceParams {
    pub uri: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_supported_and_fallback_protocol_versions() {
        assert_eq!(negotiate_protocol_version(Some("2025-11-25")), "2025-11-25");
        assert_eq!(negotiate_protocol_version(Some("2025-06-18")), "2025-06-18");
        assert_eq!(negotiate_protocol_version(Some("2025-03-26")), "2025-03-26");
        assert_eq!(negotiate_protocol_version(Some("2024-11-05")), "2024-11-05");
        assert_eq!(
            negotiate_protocol_version(Some("2099-01-01")),
            DEFAULT_PROTOCOL_VERSION
        );
        assert_eq!(negotiate_protocol_version(None), DEFAULT_PROTOCOL_VERSION);
    }

    #[test]
    fn parses_string_and_numeric_request_ids() {
        let s: IncomingMessage =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":"abc-1","method":"ping"}"#).unwrap();
        assert_eq!(s.id, Some(RequestId::String("abc-1".into())));

        let n: IncomingMessage =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":42,"method":"ping"}"#).unwrap();
        assert_eq!(n.id, Some(RequestId::Int(42)));

        let notif: IncomingMessage =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert_eq!(notif.id, None);
    }

    #[test]
    fn extracts_progress_token_and_cancelled_id() {
        let params = json!({
            "name": "search_passages",
            "_meta": { "progressToken": "tok-7" }
        });
        assert_eq!(extract_progress_token(Some(&params)), Some(json!("tok-7")));

        let num_params = json!({ "_meta": { "progressToken": 9 } });
        assert_eq!(extract_progress_token(Some(&num_params)), Some(json!(9)));

        let bad_params = json!({ "_meta": { "progressToken": { "bad": true } } });
        assert_eq!(extract_progress_token(Some(&bad_params)), None);

        let cancel = json!({ "requestId": 12, "reason": "user cancelled" });
        assert_eq!(
            extract_cancelled_id(Some(&cancel)),
            Some(RequestId::Int(12))
        );
    }

    #[test]
    fn serializes_call_tool_result_omitting_false_is_error() {
        let ok = CallToolResult::success("ok", json!({"a": 1}));
        let v = serde_json::to_value(&ok).unwrap();
        assert!(v.get("isError").is_none());
        assert_eq!(v["structuredContent"]["a"], 1);
        assert_eq!(v["content"][0]["type"], "text");

        let err = CallToolResult::error("boom");
        let ev = serde_json::to_value(&err).unwrap();
        assert_eq!(ev["isError"], true);
        assert!(ev.get("structuredContent").is_none());
    }
}
