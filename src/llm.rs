// ============================================================================
// SearchWala v5.1.0 - LLM Synthesis Engine (Zero-SDK Architecture)
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

use std::fmt::Write;
use std::time::Duration;

use axum::response::sse::Event;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use tokio::sync::mpsc;

use crate::models::{LlmConfig, SourceResult};

const MAX_CONTEXT_CHUNKS_LITE: usize = 25;
const BATCH_SIZE_RESEARCH: usize = 50;
const MAX_CONTEXT_CHARS_LITE: usize = 10_000;
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
                "max_tokens": 4096,
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
    let body = build_request_body(messages, &config.model, &info, false);

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
    let body = build_request_body(messages, &config.model, &info, true);

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

pub async fn summarize_direct(
    query: &str,
    llm_config: LlmConfig,
    sources: &[SourceResult],
    _research_mode: bool,
) -> LlmExecutionResult {
    if sources.is_empty() {
        return LlmExecutionResult {
            llm_answer: None,
            llm_error: Some("llm_skipped: no scraped content available".to_string()),
            batches_processed: 0,
        };
    }

    let context = build_ranked_context(query, sources, false);
    if context.is_empty() {
        return LlmExecutionResult {
            llm_answer: None,
            llm_error: Some("llm_skipped: relevance filter produced empty context".to_string()),
            batches_processed: 0,
        };
    }

    tracing::info!(
        "LLM direct synthesis: {} sources, context_len={}, mode=lite",
        sources.len(),
        context.len(),
    );

    let timeout_ms = llm_config.timeout_ms.unwrap_or(DEFAULT_LLM_TIMEOUT_MS);

    let (system_prompt, user_prompt) = build_lite_prompts(query, &context);

    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(user_prompt),
    ];

    tracing::info!("LLM calling model={} provider={} timeout={}ms", llm_config.model, llm_config.provider, timeout_ms);

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
            if answer.is_empty() {
                LlmExecutionResult {
                    llm_answer: None,
                    llm_error: Some("llm_empty_response".to_string()),
                    batches_processed: 1,
                }
            } else {
                tracing::info!("LLM lite synthesis complete: {} chars", answer.len());
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

    let context = build_ranked_context(query, &batch, research_mode);
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

fn build_ranked_context(query: &str, sources: &[SourceResult], research_mode: bool) -> String {
    if sources.is_empty() {
        return String::new();
    }

    let max_chunks = if research_mode { BATCH_SIZE_RESEARCH } else { MAX_CONTEXT_CHUNKS_LITE };
    let max_chars = if research_mode { MAX_CONTEXT_CHARS_RESEARCH_BATCH } else { MAX_CONTEXT_CHARS_LITE };

    // Extract query keywords for relevance scoring
    let query_lower = query.to_lowercase();
    let stop_words: std::collections::HashSet<&str> = [
        "what","is","the","a","an","of","in","to","for","and","or","how",
        "does","do","can","about","with","from","that","this","are","was",
        "were","be","been","being","have","has","had","will","would","it",
        "its","on","at","by","but","not","so","if","than","then","my","me",
        "we","you","your","who","which","when","where","why","all","each",
    ].iter().cloned().collect();

    let keywords: Vec<String> = query_lower
        .split_whitespace()
        .filter(|w| w.len() > 2 && !stop_words.contains(w))
        .map(|w| w.to_string())
        .collect();

    // Score each source by keyword relevance
    let mut scored: Vec<(usize, f32)> = sources.iter().enumerate().map(|(idx, src)| {
        let text_lower = format!("{} {}", src.title, src.extracted_text).to_lowercase();
        if keywords.is_empty() {
            return (idx, 1.0);
        }
        let mut score: f32 = 0.0;
        for kw in &keywords {
            // Title match = 3x weight
            if src.title.to_lowercase().contains(kw.as_str()) {
                score += 3.0;
            }
            // Body match = 1x weight
            if text_lower.contains(kw.as_str()) {
                score += 1.0;
            }
        }
        // Normalize by keyword count
        let relevance = score / (keywords.len() as f32 * 4.0);
        (idx, relevance)
    }).collect();

    // Sort by relevance descending
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Filter out sources with zero relevance (completely off-topic)
    let min_relevance = if research_mode { 0.0 } else { 0.05 };

    // v5.0: Pre-allocate buffer + use write!() to eliminate intermediate String allocs
    let mut context = String::with_capacity(max_chars.min(16_384));
    for (used, (idx, relevance)) in scored.into_iter().enumerate() {
        if used >= max_chunks || relevance < min_relevance {
            break;
        }
        let source = &sources[idx];
        let trimmed_text = source.extracted_text.trim();
        let cred = credibility_tag(&source.url);

        // Estimate block size to check capacity before writing
        let est_len = 10 + cred.len() + source.title.len() + source.url.len() + trimmed_text.len();
        if context.len() + est_len > max_chars {
            break;
        }

        let _ = write!(
            context,
            "[{}] {} {} ({})\n{}\n\n",
            used + 1, cred, source.title, source.url, trimmed_text
        );
    }

    context
}

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

    format!("[General Web - {}]", host.trim_start_matches("www."))
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
            NEVER present old/outdated data as current unless explicitly noting it is historical.",
        now, now, year
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
         7. Be thorough — this is a deep research report, not a summary.",
        current_datetime_str(),
        total_batches
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
         6. Maintain structure. Add sections if warranted.",
        current_datetime_str(),
        batch_num,
        total_batches
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

    let context = build_ranked_context(&query, &batch, research_mode);
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
