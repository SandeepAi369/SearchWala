// ============================================================================
// SearchWala v5.2.0 - LLM Synthesis Engine (Zero-SDK Architecture)
//
// All LLM calls use raw reqwest HTTP — no provider SDKs.
// Supports: OpenAI, Gemini, Anthropic, Groq, Together, OpenRouter,
//           Cerebras, DeepSeek, xAI, Ollama, Cohere, and any
//           OpenAI-compatible endpoint.
//
// Iterative Deep Research:
//   Research mode processes sources in batches of 50, calling the LLM
//   iteratively to build a comprehensive long-form report.
//   Each batch expands the previous report with new evidence.
//
// Time Awareness:
//   Current date/time injected into every system prompt via chrono.
//
// Modes:
//   - Lite: Single call, 25 chunks, 16K context → concise bullets
//   - Research: Multi-batch iterative, 50/batch, 64K total → detailed report
// ============================================================================

use std::collections::HashSet;
use std::fmt::Write;
use std::time::Duration;

use axum::response::sse::Event;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use tokio::sync::mpsc;

use crate::models::{LlmConfig, SourceResult};
use crate::query_intel::{QueryIntelligence, QueryIntent};

/// Language-mirroring instruction appended to every system prompt.
///
/// v7.0's prompts were hardcoded English, so a Hindi query about Hindi sources
/// produced an English answer. For a tool whose best-performing extraction
/// case is Hindi news, that is a product defect, not a nicety.
const LANGUAGE_RULE: &str = " LANGUAGE: Answer in the SAME language the user      asked in. If the question is in Hindi, answer in Hindi; if in Spanish,      answer in Spanish. When the sources are in a different language from the      question, translate the facts into the question's language — never switch      the answer's language to match the sources.";

/// How much context the model may receive.
///
/// v7.0 hardcoded `MAX_CONTEXT_CHARS_LITE = 16_000`. The pipeline scraped up
/// to 420 pages and then handed the LLM 16 KB. After Phase 3 raised extraction
/// to 374k-680k chars per search, that cap was discarding >97% of the evidence
/// it had just gathered. 2026 models have 200K-2M token windows.
///
/// Defaults are deliberately conservative — overshooting costs the user money —
/// and `CONTEXT_MAX_TOKENS` overrides everything.
pub struct ContextBudget {
    pub max_tokens: usize,
}

impl ContextBudget {
    pub fn for_model(model: &str) -> Self {
        if let Some(v) = std::env::var("CONTEXT_MAX_TOKENS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        {
            return Self { max_tokens: v.clamp(2_000, 900_000) };
        }
        let m = model.to_lowercase();
        let max_tokens = if m.contains("gemini") && m.contains("pro") {
            400_000
        } else if m.contains("gemini") {
            200_000
        } else if m.contains("opus") || m.contains("sonnet") || m.contains("fable") {
            150_000
        } else if m.contains("gpt-5") || m.contains("o4") || m.contains("gpt-4.1") {
            120_000
        } else if m.contains("haiku") || m.contains("mini") || m.contains("flash") {
            60_000
        } else {
            // Unknown model: stay close to the old behaviour rather than risk
            // a context-length error on someone's self-hosted 8k model.
            24_000
        };
        Self { max_tokens }
    }

    /// Characters of source text we may pack, leaving headroom for the prompt
    /// scaffolding and the answer itself.
    pub fn max_context_chars(&self) -> usize {
        // ~3.3 chars/token is a safe average across Latin and Indic scripts
        // (Devanagari is far denser per token than English).
        let usable = (self.max_tokens as f64 * 0.70) as usize;
        usable.saturating_mul(33) / 10
    }

    pub fn max_output_tokens(&self) -> usize {
        (self.max_tokens / 8).clamp(2_048, 32_000)
    }
}

/// Rough token estimate that accounts for script density.
pub fn est_tokens(s: &str) -> usize {
    let mut units = 0usize;
    for c in s.chars() {
        units += if c.is_ascii() { 1 } else { 3 };
    }
    units / 4 + 1
}

const MAX_CONTEXT_CHUNKS_LITE: usize = 30;    // v5.2.0: increased from 25 for richer context
const BATCH_SIZE_RESEARCH: usize = 50;
// MAX_CONTEXT_CHARS_LITE (16_000) REMOVED in v8.0 — superseded by
// ContextBudget::for_model, which scales with the model instead of discarding
// >97% of the evidence the pipeline gathered.
const MAX_CONTEXT_CHARS_RESEARCH_BATCH: usize = 12_000;
const DEFAULT_LLM_TIMEOUT_MS: u64 = 45_000;
const RESEARCH_BATCH_TIMEOUT_MS: u64 = 60_000;
const FIRST_BATCH_WAIT_MS: u64 = 60_000;
const PIPELINE_ACCUMULATION_MS: u64 = 5_000;

#[derive(Debug, Default)]
pub struct LlmExecutionResult {
    pub llm_answer: Option<String>,
    pub llm_error: Option<String>,
    pub batches_processed: usize,
}

// =============================================================================
// Internal ChatMessage — replaces genai::chat::ChatMessage
// =============================================================================

#[derive(Debug, Clone)]
pub(crate) struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), content: content.into() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: content.into() }
    }
}

// =============================================================================
// Provider Detection — determines API format, endpoint, and auth strategy
// =============================================================================

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
enum ApiFormat {
    /// OpenAI-compatible: POST /chat/completions (used by all providers including Gemini)
    OpenAiCompatible,
    /// Google Gemini native: POST /v1beta/models/{model}:generateContent (reserved)
    Gemini,
    /// Anthropic Claude: POST /v1/messages
    Anthropic,
}

struct ProviderInfo {
    format: ApiFormat,
    base_url: String,
}

fn resolve_provider(config: &LlmConfig) -> ProviderInfo {
    let provider = config.provider.trim().to_lowercase();

    // If user provided a custom base_url, use it directly (it already includes version path)
    if let Some(ref base_url) = config.base_url {
        let url = base_url.trim();
        if !url.is_empty() {
            let format = if provider == "anthropic" {
                ApiFormat::Anthropic
            } else {
                // All other providers (including Gemini) use OpenAI-compatible format
                ApiFormat::OpenAiCompatible
            };
            return ProviderInfo {
                format,
                base_url: ensure_trailing_slash(url),
            };
        }
    }

    // Default endpoints per provider — base_url includes the full version path
    // The frontend presets also follow this pattern (e.g. "https://api.groq.com/openai/v1")
    match provider.as_str() {
        "gemini" | "google" => ProviderInfo {
            // Use Google's OpenAI-compatible endpoint (same approach as Perplexica)
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai/".to_string(),
        },
        "anthropic" => ProviderInfo {
            format: ApiFormat::Anthropic,
            base_url: "https://api.anthropic.com/".to_string(),
        },
        "openai" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://api.openai.com/v1/".to_string(),
        },
        "groq" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://api.groq.com/openai/v1/".to_string(),
        },
        "together" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://api.together.xyz/v1/".to_string(),
        },
        "openrouter" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://openrouter.ai/api/v1/".to_string(),
        },
        "cerebras" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://api.cerebras.ai/v1/".to_string(),
        },
        "deepseek" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://api.deepseek.com/v1/".to_string(),
        },
        "xai" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://api.x.ai/v1/".to_string(),
        },
        "ollama" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "http://localhost:11434/v1/".to_string(),
        },
        "cohere" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://api.cohere.ai/compatibility/v1/".to_string(),
        },
        "fireworks" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://api.fireworks.ai/inference/v1/".to_string(),
        },
        "perplexity" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://api.perplexity.ai/v1/".to_string(),
        },
        "mistral_api" | "mistral" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://api.mistral.ai/v1/".to_string(),
        },
        "sambanova" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://api.sambanova.ai/v1/".to_string(),
        },
        "nvidia_nim" => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://integrate.api.nvidia.com/v1/".to_string(),
        },
        // Default: treat as OpenAI-compatible
        _ => ProviderInfo {
            format: ApiFormat::OpenAiCompatible,
            base_url: "https://api.openai.com/v1/".to_string(),
        },
    }
}

// =============================================================================
// Request/Response Payload Builders
// =============================================================================

/// Build the full URL for an LLM call.
/// base_url already includes the version path (e.g. "https://api.groq.com/openai/v1/")
/// so we just append the endpoint path.
fn build_chat_url(info: &ProviderInfo, model: &str, stream: bool) -> String {
    match info.format {
        ApiFormat::OpenAiCompatible => {
            format!("{}chat/completions", info.base_url)
        }
        ApiFormat::Gemini => {
            // Native Gemini API (only used when no base_url override)
            let action = if stream { "streamGenerateContent?alt=sse" } else { "generateContent" };
            format!("{}v1beta/models/{}:{}", info.base_url.trim_end_matches('/'), model, action)
        }
        ApiFormat::Anthropic => {
            format!("{}v1/messages", info.base_url.trim_end_matches('/'))
        }
    }
}

/// Build auth headers per provider
fn build_auth_headers(config: &LlmConfig, info: &ProviderInfo) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    match info.format {
        ApiFormat::Gemini => {
            // Gemini uses x-goog-api-key header
            if let Ok(val) = HeaderValue::from_str(&config.api_key) {
                headers.insert("x-goog-api-key", val);
            }
        }
        ApiFormat::Anthropic => {
            // Anthropic uses x-api-key + anthropic-version
            if let Ok(val) = HeaderValue::from_str(&config.api_key) {
                headers.insert("x-api-key", val);
            }
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        ApiFormat::OpenAiCompatible => {
            // Bearer token auth
            let bearer = format!("Bearer {}", config.api_key);
            if let Ok(val) = HeaderValue::from_str(&bearer) {
                headers.insert(AUTHORIZATION, val);
            }
        }
    }

    headers
}

/// Build request JSON body per provider format
fn build_request_body(
    messages: &[ChatMessage],
    model: &str,
    info: &ProviderInfo,
    stream: bool,
    max_output_tokens: usize,
) -> serde_json::Value {
    match info.format {
        ApiFormat::OpenAiCompatible => {
            let msgs: Vec<serde_json::Value> = messages.iter().map(|m| {
                serde_json::json!({
                    "role": m.role,
                    "content": m.content,
                })
            }).collect();

            let mut body = serde_json::json!({
                "model": model,
                "messages": msgs,
                // v8.0: both were ABSENT in v7.0. Without max_tokens the
                // provider applies its own default, which truncates long
                // research reports; without temperature, synthesis was at the
                // provider default (often 1.0) for a task that wants
                // determinism.
                "max_tokens": max_output_tokens,
                "temperature": 0.2,
            });

            if stream {
                body["stream"] = serde_json::json!(true);
            }

            body
        }
        ApiFormat::Gemini => {
            // Gemini uses `system_instruction` (separate from contents)
            // and `contents` array with role "user" / "model"
            let mut system_text = String::new();
            let mut contents: Vec<serde_json::Value> = Vec::new();

            for msg in messages {
                match msg.role.as_str() {
                    "system" => {
                        system_text = msg.content.clone();
                    }
                    "user" => {
                        contents.push(serde_json::json!({
                            "role": "user",
                            "parts": [{"text": msg.content}]
                        }));
                    }
                    "assistant" | "model" => {
                        contents.push(serde_json::json!({
                            "role": "model",
                            "parts": [{"text": msg.content}]
                        }));
                    }
                    _ => {
                        contents.push(serde_json::json!({
                            "role": "user",
                            "parts": [{"text": msg.content}]
                        }));
                    }
                }
            }

            let mut body = serde_json::json!({
                "contents": contents,
            });

            if !system_text.is_empty() {
                body["system_instruction"] = serde_json::json!({
                    "parts": [{"text": system_text}]
                });
            }

            body
        }
        ApiFormat::Anthropic => {
            // Anthropic: system is top-level, messages only contain user/assistant
            let mut system_text = String::new();
            let mut msgs: Vec<serde_json::Value> = Vec::new();

            for msg in messages {
                match msg.role.as_str() {
                    "system" => {
                        system_text = msg.content.clone();
                    }
                    "user" | "assistant" => {
                        msgs.push(serde_json::json!({
                            "role": msg.role,
                            "content": msg.content,
                        }));
                    }
                    _ => {
                        msgs.push(serde_json::json!({
                            "role": "user",
                            "content": msg.content,
                        }));
                    }
                }
            }

            let mut body = serde_json::json!({
                "model": model,
                "messages": msgs,
                // v7.0 hardcoded 4096, which truncated every research report.
                "max_tokens": max_output_tokens.max(8192),
                "temperature": 0.2,
            });

            if !system_text.is_empty() {
                body["system"] = serde_json::json!(system_text);
            }

            if stream {
                body["stream"] = serde_json::json!(true);
            }

            body
        }
    }
}

/// Extract text content from provider-specific JSON response
fn extract_response_text(json: &serde_json::Value, format: &ApiFormat) -> Option<String> {
    match format {
        ApiFormat::OpenAiCompatible => {
            // choices[0].message.content
            json.get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        }
        ApiFormat::Gemini => {
            // candidates[0].content.parts[0].text
            json.get("candidates")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("content"))
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.get(0))
                .and_then(|p| p.get("text"))
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        }
        ApiFormat::Anthropic => {
            // content[0].text
            json.get("content")
                .and_then(|c| c.get(0))
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        }
    }
}

/// Extract error message from API error JSON — universal for all providers.
/// Tries multiple common error structures since each provider uses different nesting.
fn extract_error_message(json: &serde_json::Value, _format: &ApiFormat) -> String {
    // Try: { "error": { "message": "..." } } — OpenAI, Groq, Gemini
    if let Some(msg) = json
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return msg.to_string();
    }

    // Try: { "error": "string" } — some providers use a flat error string
    if let Some(msg) = json.get("error").and_then(|e| e.as_str()) {
        return msg.to_string();
    }

    // Try: { "error": { "error": { "message": "..." } } } — deeply nested
    if let Some(msg) = json
        .get("error")
        .and_then(|e| e.get("error"))
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return msg.to_string();
    }

    // Try: { "message": "..." } — top-level message (Anthropic, some others)
    if let Some(msg) = json.get("message").and_then(|m| m.as_str()) {
        return msg.to_string();
    }

    // Try: { "detail": "..." } — FastAPI-style errors
    if let Some(msg) = json.get("detail").and_then(|m| m.as_str()) {
        return msg.to_string();
    }

    // Fallback: stringify the first 300 chars of the JSON
    let raw = json.to_string();
    if raw.len() > 300 {
        format!("{}...", &raw[..300])
    } else {
        raw
    }
}

// =============================================================================
// Core LLM Call — Non-Streaming (replaces genai client.exec_chat)
// =============================================================================

pub(crate) fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .pool_max_idle_per_host(5)
        .tcp_nodelay(true)
        .build()
        .expect("LLM HTTP client error")
}

/// Make a single non-streaming LLM call. Returns extracted text or error string.
pub(crate) async fn call_llm(
    config: &LlmConfig,
    messages: &[ChatMessage],
) -> Result<String, String> {
    let info = resolve_provider(config);
    let url = build_chat_url(&info, &config.model, false);
    let headers = build_auth_headers(config, &info);
    let body = build_request_body(messages, &config.model, &info, false, ContextBudget::for_model(&config.model).max_output_tokens());

    tracing::info!(
        "LLM call: provider={} model={} format={:?} url={}",
        config.provider, config.model, info.format,
        url.split('?').next().unwrap_or(&url) // Don't log API key in Gemini URL params
    );

    let client = build_http_client();

    let resp = client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("http_request_failed: {e}"))?;

    let status = resp.status();
    let resp_text = resp.text().await.map_err(|e| format!("response_read_error: {e}"))?;

    if !status.is_success() {
        let error_msg = match serde_json::from_str::<serde_json::Value>(&resp_text) {
            Ok(json) => extract_error_message(&json, &info.format),
            Err(_) => resp_text.chars().take(500).collect(),
        };
        return Err(format!("http_{}: {}", status.as_u16(), error_msg));
    }

    let json: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| format!("json_parse_error: {e}"))?;

    extract_response_text(&json, &info.format)
        .ok_or_else(|| {
            // Log raw response structure for debugging (without API keys)
            let keys: Vec<&str> = json.as_object()
                .map(|o| o.keys().map(|k| k.as_str()).collect())
                .unwrap_or_default();
            format!("empty_response: could not extract text from response (keys: {:?})", keys)
        })
}

// =============================================================================
// Core LLM Call — SSE Streaming (replaces genai client.exec_chat_stream)
// =============================================================================

/// Extract a text chunk from a streaming SSE data line per provider format
fn extract_stream_chunk(data: &str, format: &ApiFormat) -> Option<String> {
    if data.trim() == "[DONE]" {
        return None;
    }

    let json: serde_json::Value = serde_json::from_str(data).ok()?;

    match format {
        ApiFormat::OpenAiCompatible => {
            // choices[0].delta.content
            json.get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("delta"))
                .and_then(|d| d.get("content"))
                .and_then(|t| t.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        }
        ApiFormat::Gemini => {
            // candidates[0].content.parts[0].text
            json.get("candidates")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("content"))
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.get(0))
                .and_then(|p| p.get("text"))
                .and_then(|t| t.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        }
        ApiFormat::Anthropic => {
            // Anthropic SSE: event type = content_block_delta
            // delta.text for content_block_delta events
            let event_type = json.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if event_type == "content_block_delta" {
                json.get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            } else {
                None
            }
        }
    }
}

/// Stream LLM response chunks via SSE, sending each chunk through tx_sse.
pub(crate) async fn call_llm_stream(
    config: &LlmConfig,
    messages: &[ChatMessage],
    tx_sse: &mpsc::Sender<Result<Event, std::convert::Infallible>>,
) -> Result<(), String> {
    let info = resolve_provider(config);
    let url = build_chat_url(&info, &config.model, true);
    let headers = build_auth_headers(config, &info);
    let body = build_request_body(messages, &config.model, &info, true, ContextBudget::for_model(&config.model).max_output_tokens());

    tracing::info!(
        "LLM stream: provider={} model={} format={:?}",
        config.provider, config.model, info.format,
    );

    // Use a longer timeout for streaming — no overall timeout, just connect timeout
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(5)
        .tcp_nodelay(true)
        .build()
        .map_err(|e| format!("client_build_error: {e}"))?;

    let resp = client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("http_request_failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let err_text = resp.text().await.unwrap_or_default();
        let error_msg = match serde_json::from_str::<serde_json::Value>(&err_text) {
            Ok(json) => extract_error_message(&json, &info.format),
            Err(_) => err_text.chars().take(500).collect(),
        };
        return Err(format!("http_{}: {}", status.as_u16(), error_msg));
    }

    // Read the byte stream and parse SSE data lines
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk_result) = stream.next().await {
        let chunk = match chunk_result {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(e) => {
                tracing::warn!("Stream read error: {e}");
                break;
            }
        };

        buffer.push_str(&chunk);

        // Process complete SSE lines from buffer
        while let Some(newline_pos) = buffer.find('\n') {
            let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
            buffer = buffer[newline_pos + 1..].to_string();

            if line.is_empty() {
                continue;
            }

            // SSE format: "data: {...}"
            let data = if let Some(d) = line.strip_prefix("data: ") {
                d
            } else if let Some(d) = line.strip_prefix("data:") {
                d
            } else {
                // Skip event: lines, id: lines, etc.
                continue;
            };

            let data = data.trim();
            if data == "[DONE]" {
                break;
            }

            if let Some(text) = extract_stream_chunk(data, &info.format) {
                let json = serde_json::json!({
                    "type": "llm_chunk",
                    "text": text,
                }).to_string();
                let _ = tx_sse.send(Ok(Event::default().data(json))).await;
            }
        }
    }

    Ok(())
}

// =============================================================================
// DIRECT SYNTHESIS — Single-call for lite mode
// =============================================================================

// =============================================================================
// v5.2.0: INTENT-AWARE SYNTHESIS — Selects optimal prompt per query intent
// =============================================================================

pub async fn summarize_direct_with_intent(
    query: &str,
    llm_config: LlmConfig,
    sources: &[SourceResult],
    intel: &QueryIntelligence,
) -> LlmExecutionResult {
    if sources.is_empty() {
        return LlmExecutionResult {
            llm_answer: None,
            llm_error: Some("llm_skipped: no scraped content available".to_string()),
            batches_processed: 0,
        };
    }

    // v8.0 P1-10: the budget follows the model, not a fixed 16 KB.
    let budget = ContextBudget::for_model(&llm_config.model);
    let context = build_ranked_context(sources, budget.max_context_chars());
    if context.is_empty() {
        return LlmExecutionResult {
            llm_answer: None,
            llm_error: Some("llm_skipped: no usable source text".to_string()),
            batches_processed: 0,
        };
    }

    tracing::info!(
        "LLM intent-aware synthesis: intent={} sources={} cited={} context_chars={} budget={}",
        intel.intent,
        sources.len(),
        context.citation_map.len(),
        crate::util::char_len(&context.text),
        budget.max_context_chars(),
    );

    let timeout_ms = llm_config.timeout_ms.unwrap_or(DEFAULT_LLM_TIMEOUT_MS);
    let (system_prompt, user_prompt) = build_intent_prompts(query, &context.text, intel);

    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(user_prompt),
    ];

    tracing::info!("LLM calling model={} provider={} intent={}", llm_config.model, llm_config.provider, intel.intent);

    let call_result = if timeout_ms == 0 {
        call_llm(&llm_config, &messages).await
    } else {
        match tokio::time::timeout(Duration::from_millis(timeout_ms), call_llm(&llm_config, &messages)).await {
            Ok(result) => result,
            Err(_) => {
                return LlmExecutionResult {
                    llm_answer: None,
                    llm_error: Some(format!("llm_timeout: exceeded {}ms", timeout_ms)),
                    batches_processed: 0,
                };
            }
        }
    };

    match call_result {
        Ok(answer) => {
            let answer = post_process_answer(&answer, query);
            // v6.1.0: Citation faithfulness check — flag unsupported citations
            let checks = verify_citations(&answer, sources, &context.citation_map);
            let unsupported = checks.iter().filter(|c| !c.supported).count();
            if unsupported > 0 {
                tracing::info!("{} unsupported citation(s) in answer", unsupported);
            }
            if answer.is_empty() {
                LlmExecutionResult {
                    llm_answer: None,
                    llm_error: Some("llm_empty_response".to_string()),
                    batches_processed: 1,
                }
            } else if answer_quality_insufficient(&answer) {
                // v6.1.0: Quality-gated retry — one retry max
                tracing::info!(
                    "LLM quality gate failed (len={}, has_citations={}), retrying once",
                    answer.len(),
                    answer.contains('[') && answer.contains(']')
                );

                let retry_result = if timeout_ms == 0 {
                    call_llm(&llm_config, &messages).await
                } else {
                    match tokio::time::timeout(
                        Duration::from_millis(timeout_ms),
                        call_llm(&llm_config, &messages),
                    ).await {
                        Ok(r) => r,
                        Err(_) => {
                            // Retry timed out — return original answer
                            return LlmExecutionResult {
                                llm_answer: Some(answer),
                                llm_error: None,
                                batches_processed: 1,
                            };
                        }
                    }
                };

                match retry_result {
                    Ok(retry_answer) => {
                        let retry_answer = post_process_answer(&retry_answer, query);
                        let _ = verify_citations(&retry_answer, sources, &context.citation_map);
                        // Pick the better answer (longer + has citations)
                        let final_answer = if retry_answer.len() > answer.len()
                            || (retry_answer.contains('[') && !answer.contains('['))
                        {
                            tracing::info!("LLM retry produced better answer ({} vs {} chars)", retry_answer.len(), answer.len());
                            retry_answer
                        } else {
                            answer
                        };
                        LlmExecutionResult {
                            llm_answer: Some(final_answer),
                            llm_error: None,
                            batches_processed: 2,
                        }
                    }
                    Err(_) => {
                        // Retry failed — return original answer
                        LlmExecutionResult {
                            llm_answer: Some(answer),
                            llm_error: None,
                            batches_processed: 1,
                        }
                    }
                }
            } else {
                tracing::info!("LLM intent-aware synthesis complete: {} chars (intent={})", answer.len(), intel.intent);
                LlmExecutionResult {
                    llm_answer: Some(answer),
                    llm_error: None,
                    batches_processed: 1,
                }
            }
        }
        Err(err) => {
            tracing::error!("LLM call failed: {}", err);
            LlmExecutionResult {
                llm_answer: None,
                llm_error: Some(format!("llm_error: {err}")),
                batches_processed: 0,
            }
        }
    }
}

// =============================================================================
// ITERATIVE DEEP RESEARCH — Multi-batch synthesis for long-form reports
//
// Flow:
//   1. Split all sources into batches of 50
//   2. Batch 1: Generate initial comprehensive report
//   3. Batch 2+: Send previous_report + new sources → LLM expands
//   4. Return final accumulated long-form report
// =============================================================================

pub async fn summarize_iterative(
    query: &str,
    llm_config: LlmConfig,
    sources: &[SourceResult],
    temp_db: Option<&crate::cache::TempDb>,
    session_id: Option<&str>,
) -> LlmExecutionResult {
    if sources.is_empty() {
        return LlmExecutionResult {
            llm_answer: None,
            llm_error: Some("llm_skipped: no scraped content available".to_string()),
            batches_processed: 0,
        };
    }

    // Split sources into batches of 50
    let batches: Vec<&[SourceResult]> = sources.chunks(BATCH_SIZE_RESEARCH).collect();
    let total_batches = batches.len();

    tracing::info!(
        "LLM iterative research: {} total sources → {} batches of ≤{} each",
        sources.len(),
        total_batches,
        BATCH_SIZE_RESEARCH
    );

    let timeout_ms = llm_config.timeout_ms.unwrap_or(RESEARCH_BATCH_TIMEOUT_MS);

    let mut accumulated_report = String::new();
    let mut global_source_offset = 0usize;

    for (batch_idx, batch) in batches.iter().enumerate() {
        let batch_num = batch_idx + 1;

        tracing::info!(
            "LLM research batch {}/{}: {} sources (offset {})",
            batch_num,
            total_batches,
            batch.len(),
            global_source_offset
        );

        // Update temp DB progress
        if let (Some(db), Some(sid)) = (temp_db, session_id) {
            let progress = format!("Batch {}/{}", batch_num, total_batches);
            db.update_status(sid, &format!("llm_batch_{}", batch_num), &progress).await;
        }

        // Build context for this batch with global source IDs
        let context = build_research_batch_context(query, batch, global_source_offset);
        if context.is_empty() {
            global_source_offset += batch.len();
            continue;
        }

        let (system_prompt, user_prompt) = if batch_idx == 0 {
            build_research_initial_prompts(query, &context, total_batches)
        } else {
            build_research_continuation_prompts(
                query,
                &context,
                &accumulated_report,
                batch_num,
                total_batches,
            )
        };

        let messages = vec![
            ChatMessage::system(system_prompt),
            ChatMessage::user(user_prompt),
        ];

        tracing::info!(
            "LLM batch {}/{} calling model={} context_len={} prev_report_len={}",
            batch_num,
            total_batches,
            llm_config.model,
            context.len(),
            accumulated_report.len()
        );

        let call_result = match tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            call_llm(&llm_config, &messages),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!("LLM batch {}/{} timed out after {}ms", batch_num, total_batches, timeout_ms);
                // If we already have a partial report, return it instead of failing
                if !accumulated_report.is_empty() {
                    tracing::info!("Returning partial report ({} chars) after timeout", accumulated_report.len());
                    return LlmExecutionResult {
                        llm_answer: Some(accumulated_report),
                        llm_error: Some(format!(
                            "partial_report: batch {}/{} timed out, showing results from {} batches",
                            batch_num, total_batches, batch_idx
                        )),
                        batches_processed: batch_idx,
                    };
                }
                return LlmExecutionResult {
                    llm_answer: None,
                    llm_error: Some(format!("llm_timeout: batch {}/{} exceeded {}ms", batch_num, total_batches, timeout_ms)),
                    batches_processed: batch_idx,
                };
            }
        };

        match call_result {
            Ok(answer) => {
                let batch_answer = answer.trim().to_string();
                if !batch_answer.is_empty() {
                    accumulated_report = batch_answer;
                    tracing::info!(
                        "LLM batch {}/{} complete: report now {} chars",
                        batch_num,
                        total_batches,
                        accumulated_report.len()
                    );

                    // Update partial answer in temp DB
                    if let (Some(db), Some(sid)) = (temp_db, session_id) {
                        db.update_partial_answer(sid, &accumulated_report).await;
                    }
                }
            }
            Err(err) => {
                tracing::error!("LLM batch {}/{} error: {}", batch_num, total_batches, err);
                // Return partial if we have it
                if !accumulated_report.is_empty() {
                    return LlmExecutionResult {
                        llm_answer: Some(accumulated_report),
                        llm_error: Some(format!(
                            "partial_report: batch {}/{} failed ({}), showing partial results",
                            batch_num, total_batches, err
                        )),
                        batches_processed: batch_idx,
                    };
                }
                return LlmExecutionResult {
                    llm_answer: None,
                    llm_error: Some(format!("llm_error: {err}")),
                    batches_processed: batch_idx,
                };
            }
        }

        global_source_offset += batch.len();
    }

    if accumulated_report.is_empty() {
        LlmExecutionResult {
            llm_answer: None,
            llm_error: Some("llm_empty_response: all batches returned empty".to_string()),
            batches_processed: total_batches,
        }
    } else {
        tracing::info!(
            "LLM iterative research complete: {} chars across {} batches",
            accumulated_report.len(),
            total_batches
        );
        LlmExecutionResult {
            llm_answer: Some(accumulated_report),
            llm_error: None,
            batches_processed: total_batches,
        }
    }
}

// =============================================================================
// CHANNEL-BASED SYNTHESIS — Used by stream.rs (SSE streaming path)
// =============================================================================

pub async fn summarize_from_stream(
    query: &str,
    llm_config: LlmConfig,
    mut rx: mpsc::Receiver<SourceResult>,
    research_mode: bool,
) -> LlmExecutionResult {
    let max_chunks = if research_mode { BATCH_SIZE_RESEARCH } else { MAX_CONTEXT_CHUNKS_LITE };

    let first = match tokio::time::timeout(Duration::from_millis(FIRST_BATCH_WAIT_MS), rx.recv()).await {
        Ok(Some(source)) => source,
        Ok(None) => {
            return LlmExecutionResult {
                llm_answer: None,
                llm_error: Some("llm_skipped: no scraped content available".to_string()),
                batches_processed: 0,
            };
        }
        Err(_) => {
            return LlmExecutionResult {
                llm_answer: None,
                llm_error: Some("llm_timeout: waiting for first scraped batch".to_string()),
                batches_processed: 0,
            };
        }
    };

    let mut batch = vec![first];
    let collect_deadline = tokio::time::Instant::now() + Duration::from_millis(PIPELINE_ACCUMULATION_MS);
    while batch.len() < max_chunks {
        match tokio::time::timeout_at(collect_deadline, rx.recv()).await {
            Ok(Some(source)) => batch.push(source),
            Ok(None) | Err(_) => break,
        }
    }

    let context = build_ranked_context(&batch, 12_000).text;
    if context.is_empty() {
        return LlmExecutionResult {
            llm_answer: None,
            llm_error: Some("llm_skipped: relevance filter produced empty context".to_string()),
            batches_processed: 0,
        };
    }

    let timeout_ms = llm_config.timeout_ms.unwrap_or(DEFAULT_LLM_TIMEOUT_MS);

    let (system_prompt, user_prompt) = build_lite_prompts(query, &context);

    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(user_prompt),
    ];

    let call_result = if timeout_ms == 0 {
        call_llm(&llm_config, &messages).await
    } else {
        match tokio::time::timeout(Duration::from_millis(timeout_ms), call_llm(&llm_config, &messages)).await {
            Ok(result) => result,
            Err(_) => {
                return LlmExecutionResult {
                    llm_answer: None,
                    llm_error: Some(format!("llm_timeout: exceeded {}ms", timeout_ms)),
                    batches_processed: 0,
                };
            }
        }
    };

    match call_result {
        Ok(answer) => {
            let answer = answer.trim().to_string();
            if answer.is_empty() {
                LlmExecutionResult {
                    llm_answer: None,
                    llm_error: Some("llm_empty_response".to_string()),
                    batches_processed: 1,
                }
            } else {
                LlmExecutionResult {
                    llm_answer: Some(answer),
                    llm_error: None,
                    batches_processed: 1,
                }
            }
        }
        Err(err) => LlmExecutionResult {
            llm_answer: None,
            llm_error: Some(format!("llm_error: {err}")),
            batches_processed: 0,
        },
    }
}

// =============================================================================
// Context Building
// =============================================================================

/// Context block plus the mapping needed to resolve `[n]` back to a source.
pub struct BuiltContext {
    pub text: String,
    /// citation number (1-based) -> index into the `sources` slice passed in.
    pub citation_map: Vec<usize>,
}

impl BuiltContext {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
    /// Resolve a `[n]` citation to its source.
    pub fn resolve<'a>(&self, n: usize, sources: &'a [SourceResult]) -> Option<&'a SourceResult> {
        let idx = *self.citation_map.get(n.checked_sub(1)?)?;
        sources.get(idx)
    }
}

/// Assemble the LLM context from an ALREADY-RANKED source list.
///
/// ═══════════════════════════════════════════════════════════════════════════
/// v8.0 P1-6: this function no longer re-ranks.
///
/// v7.0 ran `ranking::rank_top_chunks_with_rrf` (BM25+ -> RRF boost -> MMR
/// diversity -> lost-in-middle) and then threw that ordering away here,
/// re-sorting by:
///
///     if src.title.to_lowercase().contains(kw) { score += 3.0 }
///     if text_lower.contains(kw)               { score += 1.0 }
///
/// Substring containment. No IDF, no term frequency, no length normalization —
/// a strictly worse ranker, applied last, so it won. The entire BM25+/MMR/
/// lost-in-middle stack was decorative.
///
/// v8.0 P0-11: it also returns a citation map.
///
/// v7.0 numbered the sources `[1]`, `[2]`, ... by their position in the
/// REORDERED list, then `verify_citations` resolved `[n]` against
/// `sources[n-1]` — the original, unreordered slice. Index 0 coincides under
/// the sandwich reorder, but every index from 1 onward pointed at the wrong
/// source. Citations were systematically misattributed.
/// ═══════════════════════════════════════════════════════════════════════════
fn build_ranked_context(sources: &[SourceResult], budget_chars: usize) -> BuiltContext {
    if sources.is_empty() {
        return BuiltContext { text: String::new(), citation_map: Vec::new() };
    }

    // Respect the incoming order; only decide WHERE each source sits.
    let order = lost_in_middle_order(sources.len());

    let mut text = String::with_capacity(budget_chars.min(64 * 1024));
    let mut citation_map: Vec<usize> = Vec::new();
    let mut used = 0usize;

    for idx in order {
        let src = &sources[idx];
        let body_raw = src.extracted_text.trim();
        if body_raw.is_empty() {
            continue;
        }

        let remaining = budget_chars.saturating_sub(used);
        // Reserve room for the header line.
        if remaining < 400 {
            break;
        }
        let header_budget = 220;
        let body_budget = remaining.saturating_sub(header_budget);

        // v7.0 BUG: it computed an estimated block size and `break`-ed on the
        // first source that did not fit. A single long source at position 3
        // therefore TRUNCATED the whole context to two sources. Truncate the
        // body and continue instead.
        let body = crate::util::truncate_chars(body_raw, body_budget);
        if crate::util::char_len(body) < 80 {
            continue;
        }

        let n = citation_map.len() + 1;
        let cred = credibility_tag(&src.url);
        let date = src
            .published
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "undated".to_string());

        // The date is now IN the context. v7.0 told the model to "prefer the
        // most recent sources" while giving it no dates to compare.
        let _ = write!(
            text,
            "[{}] {} {} — published {} ({})\n{}\n\n",
            n, cred, src.title, date, src.url, body
        );
        used = crate::util::char_len(&text);
        citation_map.push(idx);
    }

    BuiltContext { text, citation_map }
}

/// Lost-in-Middle ordering (Liu et al., TACL 2024, arXiv 2307.03172).
///
/// Models show a U-shaped accuracy curve over context position, so the most
/// relevant items go first and last and the least relevant in the middle.
/// Returns indices into a relevance-descending list.
fn lost_in_middle_order(len: usize) -> Vec<usize> {
    if len <= 2 {
        return (0..len).collect();
    }
    let mut front = Vec::with_capacity(len);
    let mut back = Vec::with_capacity(len);
    for i in 0..len {
        if i % 2 == 0 {
            front.push(i);
        } else {
            back.push(i);
        }
    }
    back.reverse();
    front.extend(back);
    front
}

/// Lost-in-Middle reordering: place most relevant items at start and end,
/// least relevant in the middle. This maximizes LLM attention on the best sources.
///

/// Build context for a research batch using global source IDs.
fn build_research_batch_context(
    _query: &str,
    sources: &[SourceResult],
    global_offset: usize,
) -> String {
    if sources.is_empty() {
        return String::new();
    }

    // v5.0: Pre-allocate + write!() — zero intermediate format!() allocations
    let mut context = String::with_capacity(MAX_CONTEXT_CHARS_RESEARCH_BATCH.min(16_384));
    for (idx, source) in sources.iter().enumerate() {
        let global_id = global_offset + idx + 1;
        let trimmed = source.extracted_text.trim();
        let cred = credibility_tag(&source.url);

        let est_len = 10 + cred.len() + source.title.len() + source.url.len() + trimmed.len();
        if context.len() + est_len > MAX_CONTEXT_CHARS_RESEARCH_BATCH {
            break;
        }

        let _ = write!(
            context,
            "[{}] {} {} ({})\n{}\n\n",
            global_id, cred, source.title, source.url, trimmed
        );
    }

    context
}

fn credibility_tag(url: &str) -> String {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
        .unwrap_or_else(|| "unknown".to_string());

    let high_trust = [
        ("wikipedia.org", "Wikipedia"),
        ("nih.gov", "NIH"),
        ("who.int", "WHO"),
        ("nature.com", "Nature"),
        ("sciencedirect.com", "ScienceDirect"),
        ("arxiv.org", "arXiv"),
        ("pubmed.ncbi", "PubMed"),
    ];

    if host.ends_with(".gov") || host.ends_with(".edu") {
        return "[High Trust - Institutional]".to_string();
    }

    for (needle, label) in high_trust {
        if host.contains(needle) {
            return format!("[High Trust - {}]", label);
        }
    }

    let forum = [
        ("reddit.com", "Reddit"),
        ("stackexchange.com", "StackExchange"),
        ("stackoverflow.com", "StackOverflow"),
        ("quora.com", "Quora"),
        ("news.ycombinator.com", "Hacker News"),
    ];

    for (needle, label) in forum {
        if host.contains(needle) {
            return format!("[Forum Discussion - {}]", label);
        }
    }

    // v5.2.0: Major news outlets
    let news = [
        ("bbc.com", "BBC"), ("bbc.co.uk", "BBC"),
        ("reuters.com", "Reuters"), ("apnews.com", "AP News"),
        ("nytimes.com", "NYT"), ("theguardian.com", "Guardian"),
        ("cnn.com", "CNN"), ("bloomberg.com", "Bloomberg"),
        ("washingtonpost.com", "WashPost"), ("wsj.com", "WSJ"),
        ("aljazeera.com", "Al Jazeera"), ("economist.com", "Economist"),
    ];

    for (needle, label) in news {
        if host.contains(needle) {
            return format!("[High Trust - News - {}]", label);
        }
    }

    // v5.2.0: Tech documentation
    let tech_docs = [
        ("docs.python.org", "Python Docs"),
        ("developer.mozilla.org", "MDN"),
        ("docs.rust-lang.org", "Rust Docs"),
        ("docs.microsoft.com", "MS Docs"),
        ("learn.microsoft.com", "MS Learn"),
        ("docs.github.com", "GitHub Docs"),
        ("docs.aws.amazon.com", "AWS Docs"),
        ("cloud.google.com", "Google Cloud"),
    ];

    for (needle, label) in tech_docs {
        if host.contains(needle) {
            return format!("[High Trust - Docs - {}]", label);
        }
    }

    // v5.2.0: Tech news/blogs
    let tech_news = [
        ("techcrunch.com", "TechCrunch"), ("arstechnica.com", "ArsTechnica"),
        ("theverge.com", "The Verge"), ("wired.com", "Wired"),
        ("zdnet.com", "ZDNet"), ("engadget.com", "Engadget"),
    ];

    for (needle, label) in tech_news {
        if host.contains(needle) {
            return format!("[Tech News - {}]", label);
        }
    }

    format!("[General Web - {}]", host.trim_start_matches("www."))
}

/// v6.1.0: Numeric trust score for domain authority.
/// Used as a modifier within ranking — not an override.
/// Scores are config-driven and tunable against the eval set.
///
/// Returns a multiplier:
///   .edu/.gov, Wikipedia, Nature, arXiv → 1.5
///   StackOverflow, GitHub, MDN → 1.3
///   Major news (BBC, Reuters, NYT) → 1.2
///   Tech news/blogs → 1.1
///   Forums (Reddit, Quora) → 0.8
///   Unknown → 1.0
pub fn trust_score(url: &str) -> f32 {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
        .unwrap_or_default();

    // Tier 1: Institutional / academic (1.5)
    if host.ends_with(".gov") || host.ends_with(".edu") {
        return 1.5;
    }

    // Tier 1: High-trust scholarly sources (1.5)
    const SCHOLARLY: &[&str] = &[
        "wikipedia.org", "nih.gov", "who.int", "nature.com",
        "sciencedirect.com", "arxiv.org", "pubmed.ncbi",
        "britannica.com", "plato.stanford.edu",
    ];
    for domain in SCHOLARLY {
        if host.contains(domain) {
            return 1.5;
        }
    }

    // Tier 2: Technical docs & Q&A (1.3)
    const TECH_AUTHORITY: &[&str] = &[
        "stackoverflow.com", "stackexchange.com", "github.com",
        "developer.mozilla.org", "docs.python.org", "docs.rust-lang.org",
        "docs.microsoft.com", "learn.microsoft.com", "docs.aws.amazon.com",
        "cloud.google.com", "docs.github.com",
    ];
    for domain in TECH_AUTHORITY {
        if host.contains(domain) {
            return 1.3;
        }
    }

    // Tier 3: Major news outlets (1.2)
    const NEWS: &[&str] = &[
        "bbc.com", "bbc.co.uk", "reuters.com", "apnews.com",
        "nytimes.com", "theguardian.com", "bloomberg.com",
        "washingtonpost.com", "wsj.com", "economist.com",
    ];
    for domain in NEWS {
        if host.contains(domain) {
            return 1.2;
        }
    }

    // Tier 3.5: Tech news (1.1)
    const TECH_NEWS: &[&str] = &[
        "techcrunch.com", "arstechnica.com", "theverge.com",
        "wired.com", "zdnet.com",
    ];
    for domain in TECH_NEWS {
        if host.contains(domain) {
            return 1.1;
        }
    }

    // Tier 4: Forums (0.8 — valuable but need cross-referencing)
    const FORUMS: &[&str] = &[
        "reddit.com", "quora.com", "news.ycombinator.com",
    ];
    for domain in FORUMS {
        if host.contains(domain) {
            return 0.8;
        }
    }

    // Unknown → neutral
    1.0
}

fn ensure_trailing_slash(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{}/", url)
    }
}

// =============================================================================
// Prompt Builders — with time awareness via chrono
// =============================================================================

fn current_datetime_str() -> String {
    let now = chrono::Utc::now();
    format!(
        "{} (Year: {})",
        now.format("%A, %B %d, %Y at %H:%M UTC"),
        now.format("%Y")
    )
}

// =============================================================================
// Answer Post-Processing — strip preamble, question echoing, thinking blocks
// =============================================================================

fn post_process_answer(raw: &str, _query: &str) -> String {
    let mut answer = raw.trim().to_string();

    // 1. Strip <think>/<thought>/<thinking> blocks (various models emit internal reasoning)
    let think_patterns = [
        (r"<think>", r"</think>"),
        (r"<thought>", r"</thought>"),
        (r"<thinking>", r"</thinking>"),
    ];
    for (open, close) in &think_patterns {
        while let Some(start) = answer.to_lowercase().find(&open.to_lowercase()) {
            if let Some(end) = answer.to_lowercase().find(&close.to_lowercase()) {
                let end_pos = end + close.len();
                answer = format!("{}{}", &answer[..start], &answer[end_pos..]);
            } else {
                // Unclosed tag — remove from start to end
                answer = answer[..start].to_string();
                break;
            }
        }
    }

    // 2. Strip common LLM preamble lines (case-insensitive, first line only)
    let preamble_patterns = [
        "based on the search results",
        "based on the provided sources",
        "based on the sources provided",
        "according to the search results",
        "according to the sources",
        "here is what i found",
        "here's what i found",
        "let me summarize",
        "i found the following",
        "from the search results",
        "based on my research",
        "here is a summary",
        "here's a summary",
    ];

    // Check and strip preamble from the first line
    let first_newline = answer.find('\n').unwrap_or(answer.len());
    let first_line_lower = answer[..first_newline].to_lowercase();
    for pattern in &preamble_patterns {
        if first_line_lower.starts_with(pattern) || first_line_lower.contains(pattern) {
            // If the first line is ONLY preamble (no substantive content), remove it
            if first_newline < answer.len() {
                let rest = answer[first_newline..].trim_start();
                if !rest.is_empty() {
                    answer = rest.to_string();
                }
            }
            break;
        }
    }

    answer.trim().to_string()
}

/// v6.1.0: Citation faithfulness verification.
/// Check that each cited [n] in the LLM answer has token overlap with the
/// actual source content. Flag unsupported citations.
///
/// This is the main defense against confident-but-hallucinated attributions.
/// Returns the answer with unsupported citations flagged.
/// Outcome of checking one `[n]` citation against the source it points at.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CitationCheck {
    /// The number as it appears in the answer.
    pub number: usize,
    pub url: String,
    pub title: String,
    /// Fraction of the claim's content words that appear in the source.
    pub overlap: f64,
    pub supported: bool,
}

/// Citation faithfulness check.
///
/// ═══════════════════════════════════════════════════════════════════════════
/// v8.0 fixes two things.
///
/// 1. It RESOLVES CORRECTLY. v7.0 mapped `[n]` to `sources[n-1]`, but the
///    numbers in the answer refer to positions in the lost-in-middle REORDERED
///    context. Index 0 coincides; everything from `[2]` onward pointed at the
///    wrong source. Now it goes through the `citation_map` that built the
///    context.
///
/// 2. It RETURNS SOMETHING. v7.0's body ended with `result` — the unmodified
///    input — so despite its doc comment ("Returns the answer with unsupported
///    citations flagged") it only wrote log lines. It was a no-op that cost a
///    full substring scan per citation, called twice per answer.
/// ═══════════════════════════════════════════════════════════════════════════
pub fn verify_citations(
    answer: &str,
    sources: &[SourceResult],
    citation_map: &[usize],
) -> Vec<CitationCheck> {
    if sources.is_empty() || answer.is_empty() || citation_map.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<CitationCheck> = Vec::new();
    let mut seen_numbers: HashSet<usize> = HashSet::new();

    for (n, sentence) in extract_citation_contexts(answer) {
        if !seen_numbers.insert(n) {
            continue; // report each citation number once
        }
        // n is 1-based into the CONTEXT, not into `sources`.
        let Some(&src_idx) = citation_map.get(n.wrapping_sub(1)) else { continue };
        let Some(src) = sources.get(src_idx) else { continue };

        let source_text = format!("{} {}", src.title, src.extracted_text).to_lowercase();
        let claim = sentence.to_lowercase();
        let claim_tokens: HashSet<&str> = claim
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| crate::util::char_len(t) > 3)
            .collect();
        if claim_tokens.is_empty() {
            continue;
        }
        let hits = claim_tokens.iter().filter(|t| source_text.contains(**t)).count();
        let overlap = hits as f64 / claim_tokens.len() as f64;

        out.push(CitationCheck {
            number: n,
            url: src.url.clone(),
            title: src.title.clone(),
            overlap,
            supported: overlap >= 0.25,
        });
    }

    let unsupported = out.iter().filter(|c| !c.supported).count();
    if unsupported > 0 {
        tracing::warn!(
            "Citation check: {} of {} citations have <25% token overlap with their source",
            unsupported,
            out.len()
        );
    }
    out
}

/// Find every `[n]` in the answer and the sentence containing it.
/// Byte-boundary-safe throughout.
fn extract_citation_contexts(answer: &str) -> Vec<(usize, String)> {
    let chars: Vec<(usize, char)> = answer.char_indices().collect();
    let mut found = Vec::new();
    let mut i = 0usize;

    while i < chars.len() {
        if chars[i].1 != '[' {
            i += 1;
            continue;
        }
        let bracket_byte = chars[i].0;
        let mut j = i + 1;
        let mut num = String::new();
        while j < chars.len() && chars[j].1.is_ascii_digit() {
            num.push(chars[j].1);
            j += 1;
        }
        if j >= chars.len() || chars[j].1 != ']' || num.is_empty() {
            i += 1;
            continue;
        }
        if let Ok(n) = num.parse::<usize>() {
            let after = chars[j].0 + chars[j].1.len_utf8();
            // Sentence bounds, snapped to char boundaries.
            let start = answer[..bracket_byte].rfind(". ").map(|p| p + 2).unwrap_or(0);
            let end = answer
                .get(after..)
                .and_then(|rest| rest.find(". ").map(|p| after + p + 1))
                .unwrap_or(answer.len());
            let (s, e) = (snap(answer, start), snap(answer, end.min(answer.len())));
            if s < e {
                found.push((n, answer[s..e].to_string()));
            }
        }
        i = j + 1;
    }
    found
}

/// Snap a byte offset down to the nearest char boundary.
fn snap(s: &str, mut idx: usize) -> usize {
    if idx > s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// v6.1.0: Quality gate — check if an LLM answer is worth retrying.
/// Returns true if the answer is too short or lacks citations.
fn answer_quality_insufficient(answer: &str) -> bool {
    let trimmed = answer.trim();

    // Too short — likely a refusal or garbage
    if trimmed.len() < 80 {
        return true;
    }

    // Has content but no citations at all — likely hallucinated
    // (Only flag if answer is substantial enough to warrant citations)
    if trimmed.len() > 200 {
        let has_citation = trimmed.contains("[1]")
            || trimmed.contains("[2]")
            || trimmed.contains("[3]");
        if !has_citation {
            return true;
        }
    }

    false
}

fn build_lite_prompts(query: &str, context: &str) -> (String, String) {
    let now = current_datetime_str();
    let year = chrono::Utc::now().format("%Y").to_string();
    let system = format!(
        "You are a precise search synthesis engine. \
         TODAY'S DATE: {}. \
         CRITICAL RULES: \
         1. STRICTLY answer the SPECIFIC question asked. Do NOT discuss unrelated topics. \
         2. If the user asks about 'OpenAI', answer ONLY about OpenAI — NOT Google, NOT Anthropic. \
         3. If the user asks about a specific product/company, focus ONLY on that entity. \
         4. Use ONLY information from the provided sources — never hallucinate. \
         5. Every factual statement MUST cite the source like [1], [2]. \
         6. Prioritize [High Trust] sources. Flag outdated information. \
         7. If no source directly answers the query, say so honestly. \
         8. RECENCY IS CRITICAL: Today is {}. Always prefer the MOST RECENT information from sources. \
            If a source mentions dates from {} or later, prioritize that over older data. \
            If the user asks about 'latest', 'current', 'now', or 'today', always provide the MOST UP-TO-DATE answer possible. \
            NEVER present old/outdated data as current unless explicitly noting it is historical.{}",
        now, now, year, LANGUAGE_RULE
    );

    let user = format!(
        "USER'S EXACT QUESTION: {}\n\n\
         SOURCES (use ONLY relevant ones):\n{}\n\n\
         Answer the user's EXACT question in 5-8 focused bullet points. \
         Each bullet: 2-3 sentences with [n] citations. \
         IGNORE sources that are not directly relevant to the question. \
         If the question is about recent/current events, prioritize the NEWEST sources. \
         End with: Sources Used: [n] <url>",
        query, context
    );

    (system, user)
}

fn build_research_initial_prompts(query: &str, context: &str, total_batches: usize) -> (String, String) {
    let system = format!(
        "You are an expert research analyst producing a comprehensive, detailed research report. \
         Current date: {}. \
         You are processing batch 1 of {} total batches of source material. \
         STRICT RULES: \
         1. Use ONLY the provided source material — NEVER hallucinate or add unsourced claims. \
         2. Every factual statement MUST have a citation like [1], [2], etc. \
         3. Prioritize [High Trust] sources over forums and general web. \
         4. Flag any information that appears outdated given today's date. \
         5. Write in a structured, detailed, analytical style — NOT bullet points. \
         6. Use clear section headings (## format). \
         7. Be thorough — this is a deep research report, not a summary.{}",
        current_datetime_str(),
        total_batches,
        LANGUAGE_RULE
    );

    let user = format!(
        "RESEARCH QUERY:\n{}\n\n\
         SOURCE MATERIAL (Batch 1/{}):\n{}\n\n\
         Write a comprehensive, well-structured research report based ONLY on these sources. \
         Include:\n\
         - An overview/introduction section\n\
         - Key findings organized by theme or topic\n\
         - Important details, data points, and context from the sources\n\
         - Analysis of source agreement/disagreement where relevant\n\
         - A 'Sources Used' section at the end: [n] <url>\n\n\
         Write at length. Be detailed and thorough. Do NOT summarize briefly — expand fully.",
        query, total_batches, context
    );

    (system, user)
}

fn build_research_continuation_prompts(
    query: &str,
    new_context: &str,
    previous_report: &str,
    batch_num: usize,
    total_batches: usize,
) -> (String, String) {
    let system = format!(
        "You are an expert research analyst expanding a research report with new sources. \
         Date: {}. Batch {} of {}. \
         RULES: 1. Integrate new info into the report — do NOT restart. \
         2. ADD new details and evidence. 3. KEEP existing content. \
         4. Cite source numbers like [51], [52]. 5. NEVER hallucinate. \
         6. Maintain structure. Add sections if warranted.{}",
        current_datetime_str(),
        batch_num,
        total_batches,
        LANGUAGE_RULE
    );

    // Truncate previous report to fit within model context limits
    // ~3000 chars ≈ 750 tokens, leaves room for new sources + system
    let max_prev = 3000;
    let prev_truncated = if previous_report.len() > max_prev {
        let start = previous_report.len().saturating_sub(max_prev);
        // Find safe char boundary
        let safe_start = (start..start+4).find(|&i| previous_report.is_char_boundary(i)).unwrap_or(start);
        format!("[...truncated...] {}", &previous_report[safe_start..])
    } else {
        previous_report.to_string()
    };

    let user = format!(
        "QUERY: {}\n\nEXISTING REPORT (summary):\n{}\n\n\
         NEW SOURCES (Batch {}/{}):\n{}\n\n\
         Expand the report with these new sources. Add new details, cite sources, keep existing content.",
        query, prev_truncated, batch_num, total_batches, new_context
    );

    (system, user)
}

// =============================================================================
// v5.2.0: INTENT-AWARE PROMPT BUILDER
// Selects optimal system/user prompt template based on QueryIntelligence
// =============================================================================

fn build_intent_prompts(query: &str, context: &str, intel: &QueryIntelligence) -> (String, String) {
    let now = current_datetime_str();
    let year = chrono::Utc::now().format("%Y").to_string();

    let (intent_instruction, format_instruction) = match &intel.intent {
        QueryIntent::Factual => (
            "Give a DIRECT, CONCISE answer to the specific question. \
            Lead with the KEY FACT the user is looking for. \
            Then expand with supporting context and details.".to_string(),
            "Answer with 3-5 focused bullet points. Each bullet: 1-2 sentences with [n] citations. \
             Start with the direct answer, then add context.".to_string(),
        ),

        QueryIntent::Temporal => (
            format!(
                "TODAY IS {}. The user wants CURRENT, UP-TO-DATE information. \
                CRITICAL: Prioritize the NEWEST sources. \
                If a source is from {} or later, it takes priority. \
                Flag ANY information that appears outdated. \
                If sources conflict on dates/facts, go with the most recent one.",
                now, year
            ),
            "Answer with 5-8 bullet points focused on the MOST CURRENT information. \
             Each bullet: 2-3 sentences with [n] citations. \
             If the user asks about 'latest', 'current', or 'now', ensure your answer \
             reflects the absolute most recent data. Explicitly mention dates when available.".to_string(),
        ),

        QueryIntent::Person => (
            format!(
                "The user is asking about a SPECIFIC PERSON. \
                Provide a comprehensive but focused profile. Date: {}.", now
            ),
            "Structure your answer as:\n\
             - **Current Role/Position**: What they do NOW\n\
             - **Key Background**: Brief career/life highlights\n\
             - **Notable Achievements**: What they're known for\n\
             - **Recent Activity**: What they've done recently\n\
             Each point with [n] citations.".to_string(),
        ),

        QueryIntent::Comparison => (
            "The user wants a COMPARISON. Provide a balanced, structured analysis \
             comparing the subjects. Be fair to both sides.".to_string(),
            "Structure your answer as a comparison:\n\
             - **Overview**: Brief intro to both subjects\n\
             - **Key Differences**: The most important distinctions\n\
             - **Strengths**: What each excels at\n\
             - **Weaknesses**: Where each falls short\n\
             - **Bottom Line**: Summary recommendation if applicable\n\
             Each point with [n] citations.".to_string(),
        ),

        QueryIntent::HowTo => (
            "The user wants PRACTICAL INSTRUCTIONS. Provide clear, actionable \
             step-by-step guidance. Be specific and concrete.".to_string(),
            "Provide step-by-step instructions:\n\
             1. Number each step clearly\n\
             2. Each step: 1-2 sentences, practical and actionable\n\
             3. Include prerequisites or requirements first\n\
             4. Add tips or warnings where relevant\n\
             5. Cite sources with [n] for verification".to_string(),
        ),

        QueryIntent::Research | QueryIntent::Navigational => (
            format!(
                "Provide a comprehensive, well-organized analysis. Date: {}.", now
            ),
            "Answer in 6-10 detailed bullet points covering all aspects. \
             Each bullet: 2-3 sentences with [n] citations. \
             Cover multiple perspectives where relevant.".to_string(),
        ),
    };

    let system = format!(
        "You are a precise search synthesis engine. \
         TODAY'S DATE: {}. \
         INTENT: {}. \
         {} \
         CRITICAL RULES: \
         1. STRICTLY answer the SPECIFIC question asked. Do NOT discuss unrelated topics. \
         2. If the user asks about 'X', answer ONLY about X. \
         3. Use ONLY information from the provided sources — NEVER hallucinate. \
         4. Every factual statement MUST cite the source like [1], [2]. \
         5. Prioritize [High Trust] sources over [General Web]. \
         6. If no source directly answers the query, say so HONESTLY. \
         7. NEVER present outdated data as current unless explicitly noting it is historical.{}",
        now, intel.intent, intent_instruction, LANGUAGE_RULE
    );

    let user = format!(
        "USER'S EXACT QUESTION: {}\n\n\
         SOURCES (use ONLY relevant ones):\n{}\n\n\
         {} \
         IGNORE sources that are not directly relevant to the question. \
         End with: Sources Used: [n] <url>",
        query, context, format_instruction
    );

    (system, user)
}

// =============================================================================
// SSE Streaming Synthesis — Used by stream.rs
// =============================================================================

pub async fn summarize_from_stream_sse(
    query: String,
    llm_config: LlmConfig,
    mut rx: mpsc::Receiver<SourceResult>,
    tx_sse: mpsc::Sender<Result<Event, std::convert::Infallible>>,
    research_mode: bool,
) {
    let max_chunks = if research_mode { BATCH_SIZE_RESEARCH } else { MAX_CONTEXT_CHUNKS_LITE };

    let first = match tokio::time::timeout(Duration::from_millis(FIRST_BATCH_WAIT_MS), rx.recv()).await {
        Ok(Some(source)) => source,
        Ok(None) | Err(_) => {
            let json = serde_json::json!({"type": "llm_error", "text": "llm_skipped: no scraped content available"}).to_string();
            let _ = tx_sse.send(Ok(Event::default().data(json))).await;
            return;
        }
    };

    let mut batch = vec![first];
    let collect_deadline = tokio::time::Instant::now() + Duration::from_millis(PIPELINE_ACCUMULATION_MS);
    while batch.len() < max_chunks {
        match tokio::time::timeout_at(collect_deadline, rx.recv()).await {
            Ok(Some(source)) => batch.push(source),
            Ok(None) | Err(_) => break,
        }
    }

    let context = build_ranked_context(&batch, if research_mode { 12_000 } else { 48_000 }).text;
    if context.is_empty() {
        let json = serde_json::json!({"type": "llm_error", "text": "llm_skipped: relevance filter produced empty context"}).to_string();
        let _ = tx_sse.send(Ok(Event::default().data(json))).await;
        return;
    }

    let (system_prompt, user_prompt) = build_lite_prompts(&query, &context);

    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(user_prompt),
    ];

    match call_llm_stream(&llm_config, &messages, &tx_sse).await {
        Ok(()) => {
            let _ = tx_sse
                .send(Ok(Event::default().data(
                    serde_json::json!({"type": "llm_done"}).to_string(),
                )))
                .await;
        }
        Err(err) => {
            let json = serde_json::json!({"type": "llm_error", "text": format!("llm_error: {err}")}).to_string();
            let _ = tx_sse.send(Ok(Event::default().data(json))).await;
        }
    }
}

// =============================================================================
// Dynamic Model Fetcher — GET /api/models
//
// Called by the UI "Fetch Models" button. base_url comes from the frontend
// presets (e.g. "https://api.groq.com/openai/v1") and already includes the
// version path. We just append "/models".
// =============================================================================

pub async fn fetch_provider_models(
    api_key: &str,
    base_url: &str,
    provider: &str,
) -> Result<Vec<String>, String> {
    if api_key.is_empty() || base_url.is_empty() {
        return Err("api_key and base_url are required".to_string());
    }

    // base_url already includes the version path (e.g. /v1 or /v1beta/openai)
    // Just append /models
    let models_url = format!("{}models", ensure_trailing_slash(base_url.trim()));
    tracing::info!("Fetching models from: {}", models_url);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("client_error: {e}"))?;

    // Use provider-specific auth headers
    let is_anthropic = provider.eq_ignore_ascii_case("anthropic");
    let mut req = client
        .get(&models_url)
        .header("Accept", "application/json");

    if is_anthropic {
        req = req
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01");
    } else {
        req = req.header("Authorization", format!("Bearer {}", api_key));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("request_failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let err_text = resp.text().await.unwrap_or_default();
        let detail = match serde_json::from_str::<serde_json::Value>(&err_text) {
            Ok(json) => extract_error_message(&json, &ApiFormat::OpenAiCompatible),
            Err(_) => err_text.chars().take(200).collect(),
        };
        return Err(format!("http_{}: {}", status, detail));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("json_parse_error: {e}"))?;

    let mut models: Vec<String> = Vec::new();

    // Format 1: OpenAI-compatible — { "data": [ { "id": "model-name" }, ... ] }
    if let Some(data) = json.get("data").and_then(|v| v.as_array()) {
        for item in data {
            if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                let id = id.trim();
                if !id.is_empty() {
                    // Strip "models/" prefix if present (Gemini OpenAI-compat returns this)
                    let clean_id = id.strip_prefix("models/").unwrap_or(id);
                    models.push(clean_id.to_string());
                }
            }
        }
    }

    // Format 2: Gemini native — { "models": [ { "name": "models/gemini-2.0-flash", ... } ] }
    if models.is_empty() {
        if let Some(data) = json.get("models").and_then(|v| v.as_array()) {
            for item in data {
                // Only include models that support generateContent
                let methods = item
                    .get("supportedGenerationMethods")
                    .and_then(|v| v.as_array());
                let supports_chat = methods
                    .map(|ms| ms.iter().any(|m| m.as_str() == Some("generateContent")))
                    .unwrap_or(true);

                if supports_chat {
                    if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                        let name = name.trim();
                        if !name.is_empty() {
                            // Strip "models/" prefix
                            let clean = name.strip_prefix("models/").unwrap_or(name);
                            models.push(clean.to_string());
                        }
                    }
                }
            }
        }
    }

    models.sort();
    tracing::info!("Fetched {} models from {}", models.len(), base_url);
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SourceResult;

    fn src(url: &str, title: &str, text: &str) -> SourceResult {
        SourceResult {
            url: url.into(),
            title: title.into(),
            extracted_text: text.into(),
            char_count: crate::util::char_len(text),
            engine: "test".into(),
            confidence: 1.0,
            ..Default::default()
        }
    }

    // ── P0-11: the citation index bug ──

    #[test]
    fn citation_map_round_trips_through_lost_in_middle_reorder() {
        // Five distinct sources; the context reorders them into a sandwich.
        // Every [n] in the answer must resolve to the source the MODEL saw as
        // [n] — v7.0 resolved against the unreordered slice, so [2] onward
        // pointed at the wrong source.
        let sources: Vec<SourceResult> = (0..5)
            .map(|i| {
                src(
                    &format!("https://s{i}.test/a"),
                    &format!("Title {i}"),
                    &format!("Body of source number {i}. {}", "filler words here. ".repeat(20)),
                )
            })
            .collect();

        let ctx = build_ranked_context(&sources, 100_000);
        assert_eq!(ctx.citation_map.len(), 5, "all five should fit the budget");

        for n in 1..=5 {
            let resolved = ctx.resolve(n, &sources).expect("must resolve");
            // The context block labelled [n] must contain THIS source's url.
            let marker = format!("[{n}] ");
            let pos = ctx.text.find(&marker).expect("label present");
            let block_end = ctx.text[pos + marker.len()..]
                .find("\n\n")
                .map(|e| pos + marker.len() + e)
                .unwrap_or(ctx.text.len());
            let block = &ctx.text[pos..block_end];
            assert!(
                block.contains(&resolved.url),
                "citation [{n}] resolves to {} but its context block is {:?}",
                resolved.url,
                block.chars().take(90).collect::<String>()
            );
        }
    }

    #[test]
    fn lost_in_middle_puts_best_first_and_second_best_last() {
        let order = lost_in_middle_order(5);
        assert_eq!(order.first(), Some(&0), "most relevant must lead");
        assert_eq!(order.last(), Some(&1), "second most relevant must close");
        // Every index present exactly once.
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn lost_in_middle_handles_tiny_inputs() {
        assert_eq!(lost_in_middle_order(0), Vec::<usize>::new());
        assert_eq!(lost_in_middle_order(1), vec![0]);
        assert_eq!(lost_in_middle_order(2), vec![0, 1]);
    }

    // ── P1-6: one long source must not truncate the whole context ──

    #[test]
    fn a_long_source_is_truncated_not_allowed_to_end_the_context() {
        // v7.0 estimated the block size and `break`-ed on the first source
        // that did not fit, so one long source at position 3 cut the context
        // to two sources.
        let mut sources = vec![
            src("https://a.test/1", "Short A", &"alpha words. ".repeat(40)),
            src("https://b.test/2", "HUGE", &"beta ".repeat(50_000)), // ~250k chars
        ];
        sources.push(src("https://c.test/3", "Short C", &"gamma words. ".repeat(40)));
        sources.push(src("https://d.test/4", "Short D", &"delta words. ".repeat(40)));

        let ctx = build_ranked_context(&sources, 20_000);
        assert!(
            ctx.citation_map.len() >= 3,
            "oversized source truncated the context to {} sources",
            ctx.citation_map.len()
        );
        assert!(crate::util::char_len(&ctx.text) <= 20_500, "budget overrun");
    }

    #[test]
    fn context_includes_publication_dates() {
        // v7.0 told the model to "prefer the most recent sources" while
        // giving it no dates to compare.
        let mut s = src("https://a.test/1", "Dated", &"content words. ".repeat(40));
        s.published = chrono::DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z")
            .ok()
            .map(|d| d.with_timezone(&chrono::Utc));
        let ctx = build_ranked_context(&[s], 50_000);
        assert!(ctx.text.contains("2026-10-01"), "date missing from context");
        let undated = src("https://b.test/2", "Undated", &"content words. ".repeat(40));
        let ctx2 = build_ranked_context(&[undated], 50_000);
        assert!(ctx2.text.contains("undated"));
    }

    // ── verify_citations now actually verifies ──

    #[test]
    fn verify_citations_flags_unsupported_claims() {
        // Sources must clear build_ranked_context's 80-char minimum — a
        // sub-80-char "source" is not evidence and is correctly skipped.
        let sources = vec![
            src(
                "https://a.test/1",
                "Cricket Schedule",
                "The tournament begins in October 2027 across nine venues in India. \
                 Matches will be played in Mumbai, Delhi, Chennai and Kolkata, with \
                 the final scheduled at Ahmedabad in November.",
            ),
            src(
                "https://b.test/2",
                "Unrelated Page",
                "This page discusses marine biology and coral reef conservation \
                 efforts across the Pacific. Researchers monitor bleaching events \
                 and document species recovery after warming periods.",
            ),
        ];
        let ctx = build_ranked_context(&sources, 50_000);
        // [1] makes a claim the first source supports; [2] does not.
        let answer = "The tournament begins in October 2027 across nine venues [1]. \
                      The tournament also features robotic umpires and orbital stadiums [2].";
        let checks = verify_citations(answer, &sources, &ctx.citation_map);
        assert_eq!(checks.len(), 2, "both citations should be checked: {checks:?}");
        let c2 = checks.iter().find(|c| c.number == 2).unwrap();
        assert!(!c2.supported, "fabricated claim should be unsupported (overlap {})", c2.overlap);
    }

    #[test]
    fn verify_citations_returns_data_not_the_input_string() {
        // v7.0's body ended with `result` — the unmodified input — so despite
        // its doc comment it only wrote log lines.
        let sources = vec![src(
            "https://a.test/1",
            "Topic",
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu \
             nu xi omicron pi rho sigma tau upsilon phi chi psi omega",
        )];
        let ctx = build_ranked_context(&sources, 50_000);
        assert_eq!(ctx.citation_map.len(), 1, "source should have been included");
        let checks = verify_citations("Some claim about alpha beta gamma [1].", &sources, &ctx.citation_map);
        assert!(!checks.is_empty());
        assert_eq!(checks[0].url, "https://a.test/1");
    }

    #[test]
    fn verify_citations_is_utf8_safe_on_hindi_answers() {
        let sources = vec![src(
            "https://amarujala.test/1",
            "विश्व कप",
            "भारत और पाकिस्तान के बीच मुकाबला दस अक्टूबर को खेला जाएगा। टूर्नामेंट भारत में होगा।",
        )];
        let ctx = build_ranked_context(&sources, 50_000);
        let answer = "भारत और पाकिस्तान के बीच मुकाबला दस अक्टूबर को होगा [1]। यह टूर्नामेंट भारत में है [1]।";
        let checks = verify_citations(answer, &sources, &ctx.citation_map);
        assert!(!checks.is_empty(), "should find the Hindi citation");
    }

    #[test]
    fn extract_citation_contexts_ignores_non_citations() {
        let found = extract_citation_contexts("See [abc] and [12x] but [3] counts.");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, 3);
    }

    #[test]
    fn out_of_range_citations_are_dropped_not_panics() {
        let sources = vec![src(
            "https://a.test/1",
            "Topic",
            "some body text here for the source that is comfortably longer than \
             the eighty character minimum the context builder enforces",
        )];
        let ctx = build_ranked_context(&sources, 50_000);
        // [9] has no mapping.
        let checks = verify_citations("Claim [9].", &sources, &ctx.citation_map);
        assert!(checks.is_empty());
    }

    // ── P1-10: context budget ──

    #[test]
    fn context_budget_scales_with_the_model() {
        std::env::remove_var("CONTEXT_MAX_TOKENS");
        let gemini = ContextBudget::for_model("gemini-3-pro");
        let haiku = ContextBudget::for_model("claude-haiku-4-5");
        let unknown = ContextBudget::for_model("some-local-llama");
        assert!(gemini.max_context_chars() > haiku.max_context_chars());
        assert!(haiku.max_context_chars() > unknown.max_context_chars());
        // All of them must beat the v7.0 fixed 16,000.
        assert!(unknown.max_context_chars() > 16_000, "even the conservative default should beat v7.0");
        assert!(gemini.max_context_chars() > 500_000);
    }

    #[test]
    fn context_budget_env_override_is_clamped() {
        std::env::set_var("CONTEXT_MAX_TOKENS", "999999999");
        assert!(ContextBudget::for_model("x").max_tokens <= 900_000);
        std::env::set_var("CONTEXT_MAX_TOKENS", "1");
        assert!(ContextBudget::for_model("x").max_tokens >= 2_000);
        std::env::remove_var("CONTEXT_MAX_TOKENS");
    }

    #[test]
    fn max_output_tokens_is_never_the_v7_truncating_4096_for_big_models() {
        std::env::remove_var("CONTEXT_MAX_TOKENS");
        let b = ContextBudget::for_model("claude-opus-5");
        assert!(b.max_output_tokens() > 4096, "research reports were truncated at 4096");
    }

    #[test]
    fn est_tokens_accounts_for_script_density() {
        // Devanagari costs more tokens per character than ASCII.
        let ascii = "a".repeat(100);
        let hindi = "क".repeat(100);
        assert!(est_tokens(&hindi) > est_tokens(&ascii));
    }

    #[test]
    fn trust_score_is_actually_wired_and_ranks_authorities_higher() {
        // It was pub-but-never-called in v7.0.
        assert!(trust_score("https://www.nih.gov/x") > trust_score("https://random-blog.test/x"));
        assert!(trust_score("https://en.wikipedia.org/wiki/X") > trust_score("https://random-blog.test/x"));
        assert!(trust_score("https://docs.rust-lang.org/std/") > trust_score("https://random-blog.test/x"));
    }
}
