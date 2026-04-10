// ============================================================================
// Swift Search Agent v4.1 - LLM Synthesis Engine
// FIXED: Added summarize_direct() — no more race condition with channels
// Research mode: 50 chunks / 32K context | Lite mode: 25 chunks / 16K context
// ============================================================================

use std::time::Duration;

use axum::response::sse::Event;
use futures::StreamExt;
use genai::chat::{ChatMessage, ChatRequest};
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ServiceTarget};
use tokio::sync::mpsc;

use crate::models::{LlmConfig, SourceResult};

const MAX_CONTEXT_CHUNKS: usize = 25;
const MAX_CONTEXT_CHUNKS_RESEARCH: usize = 50;
const MAX_CONTEXT_CHARS: usize = 16_000;
const MAX_CONTEXT_CHARS_RESEARCH: usize = 32_000;
const DEFAULT_LLM_TIMEOUT_MS: u64 = 45_000;
const FIRST_BATCH_WAIT_MS: u64 = 60_000;
const PIPELINE_ACCUMULATION_MS: u64 = 5_000;

#[derive(Debug, Default)]
pub struct LlmExecutionResult {
    pub llm_answer: Option<String>,
    pub llm_error: Option<String>,
}

// =============================================================================
// DIRECT SYNTHESIS — Takes scraped data directly, no channel race condition
// This is the PRIMARY path used by search.rs
// =============================================================================

pub async fn summarize_direct(
    query: &str,
    llm_config: LlmConfig,
    sources: &[SourceResult],
    research_mode: bool,
) -> LlmExecutionResult {
    if sources.is_empty() {
        return LlmExecutionResult {
            llm_answer: None,
            llm_error: Some("llm_skipped: no scraped content available".to_string()),
        };
    }

    let context = build_ranked_context(query, sources, research_mode);
    if context.is_empty() {
        return LlmExecutionResult {
            llm_answer: None,
            llm_error: Some("llm_skipped: relevance filter produced empty context".to_string()),
        };
    }

    tracing::info!(
        "LLM direct synthesis: {} sources, context_len={}, research={}",
        sources.len(),
        context.len(),
        research_mode
    );

    let client = build_client(&llm_config);
    let model = namespaced_model(&llm_config.provider, &llm_config.model);
    let timeout_ms = llm_config.timeout_ms.unwrap_or(DEFAULT_LLM_TIMEOUT_MS);

    let (system_prompt, user_prompt) = build_prompts(query, &context, false);

    let chat_req = ChatRequest::new(vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(user_prompt),
    ]);

    tracing::info!("LLM calling model={} provider={} timeout={}ms", model, llm_config.provider, timeout_ms);

    let call_result = if timeout_ms == 0 {
        client.exec_chat(&model, chat_req, None).await
    } else {
        match tokio::time::timeout(Duration::from_millis(timeout_ms), client.exec_chat(&model, chat_req, None)).await {
            Ok(result) => result,
            Err(_) => {
                return LlmExecutionResult {
                    llm_answer: None,
                    llm_error: Some(format!("llm_timeout: exceeded {}ms", timeout_ms)),
                };
            }
        }
    };

    match call_result {
        Ok(chat_res) => {
            let answer = chat_res.first_text().unwrap_or("").trim().to_string();
            if answer.is_empty() {
                LlmExecutionResult {
                    llm_answer: None,
                    llm_error: Some("llm_empty_response".to_string()),
                }
            } else {
                tracing::info!("LLM synthesis complete: {} chars", answer.len());
                LlmExecutionResult {
                    llm_answer: Some(answer),
                    llm_error: None,
                }
            }
        }
        Err(err) => {
            tracing::error!("LLM call failed: {}", err);
            LlmExecutionResult {
                llm_answer: None,
                llm_error: Some(format!("llm_error: {err}")),
            }
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
    let max_chunks = if research_mode { MAX_CONTEXT_CHUNKS_RESEARCH } else { MAX_CONTEXT_CHUNKS };

    // Wait much longer for first batch — scraping can take 30-60s
    let first = match tokio::time::timeout(Duration::from_millis(FIRST_BATCH_WAIT_MS), rx.recv()).await {
        Ok(Some(source)) => source,
        Ok(None) => {
            return LlmExecutionResult {
                llm_answer: None,
                llm_error: Some("llm_skipped: no scraped content available".to_string()),
            };
        }
        Err(_) => {
            return LlmExecutionResult {
                llm_answer: None,
                llm_error: Some("llm_timeout: waiting for first scraped batch".to_string()),
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
        };
    }

    let client = build_client(&llm_config);
    let model = namespaced_model(&llm_config.provider, &llm_config.model);
    let timeout_ms = llm_config.timeout_ms.unwrap_or(DEFAULT_LLM_TIMEOUT_MS);

    let (system_prompt, user_prompt) = build_prompts(query, &context, false);

    let chat_req = ChatRequest::new(vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(user_prompt),
    ]);

    let call_result = if timeout_ms == 0 {
        client.exec_chat(&model, chat_req, None).await
    } else {
        match tokio::time::timeout(Duration::from_millis(timeout_ms), client.exec_chat(&model, chat_req, None)).await {
            Ok(result) => result,
            Err(_) => {
                return LlmExecutionResult {
                    llm_answer: None,
                    llm_error: Some(format!("llm_timeout: exceeded {}ms", timeout_ms)),
                };
            }
        }
    };

    match call_result {
        Ok(chat_res) => {
            let answer = chat_res.first_text().unwrap_or("").trim().to_string();
            if answer.is_empty() {
                LlmExecutionResult {
                    llm_answer: None,
                    llm_error: Some("llm_empty_response".to_string()),
                }
            } else {
                LlmExecutionResult {
                    llm_answer: Some(answer),
                    llm_error: None,
                }
            }
        }
        Err(err) => LlmExecutionResult {
            llm_answer: None,
            llm_error: Some(format!("llm_error: {err}")),
        },
    }
}

// =============================================================================
// Client & Model Helpers
// =============================================================================

pub(crate) fn build_client(config: &LlmConfig) -> Client {
    let mut builder = Client::builder();

    let api_key = config.api_key.clone();
    builder = builder.with_auth_resolver_fn(move |_model_iden| {
        Ok(Some(AuthData::from_single(api_key.clone())))
    });

    if let Some(base_url) = config.base_url.as_ref().filter(|u| !u.trim().is_empty()) {
        let endpoint_url = ensure_trailing_slash(base_url.trim());
        let api_key = config.api_key.clone();

        builder = builder.with_service_target_resolver_fn(move |service_target: ServiceTarget| {
            let ServiceTarget { model, .. } = service_target;
            Ok(ServiceTarget {
                endpoint: Endpoint::from_owned(endpoint_url.clone()),
                auth: AuthData::from_single(api_key.clone()),
                model,
            })
        });
    }

    builder.build()
}

pub(crate) fn namespaced_model(provider: &str, model: &str) -> String {
    if model.contains("::") {
        return model.to_string();
    }

    let provider = provider.trim().to_lowercase();

    // All OpenAI-compatible providers (custom endpoints) use the openai:: prefix
    if provider == "openai_compatible"
        || provider == "cerebras"
        || provider == "openrouter"
        || provider == "together"
        || provider == "fireworks"
        || provider == "perplexity"
        || provider == "mistral_api"
        || provider == "sambanova"
        || provider == "nvidia_nim"
        || provider == "azure_openai"
    {
        return format!("openai::{}", model);
    }

    match provider.as_str() {
        "openai" | "anthropic" | "gemini" | "groq" | "ollama" | "xai" | "deepseek" | "cohere" | "zai" => {
            format!("{}::{}", provider, model)
        }
        _ => {
            // Unknown provider — assume OpenAI-compatible
            format!("openai::{}", model)
        }
    }
}

fn build_ranked_context(_query: &str, sources: &[SourceResult], research_mode: bool) -> String {
    if sources.is_empty() {
        return String::new();
    }

    let max_chunks = if research_mode { MAX_CONTEXT_CHUNKS_RESEARCH } else { MAX_CONTEXT_CHUNKS };
    let max_chars = if research_mode { MAX_CONTEXT_CHARS_RESEARCH } else { MAX_CONTEXT_CHARS };

    let mut context = String::new();
    for (idx, source) in sources.iter().take(max_chunks).enumerate() {
        let block = format!(
            "[{}] {} {} ({})\n{}\n\n",
            idx + 1,
            credibility_tag(&source.url),
            source.title,
            source.url,
            source.extracted_text.trim()
        );

        if context.len() + block.len() > max_chars {
            break;
        }
        context.push_str(&block);
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

fn build_prompts(query: &str, context: &str, streaming: bool) -> (String, String) {
    let system = "You are a synthesis engine for web search results. Always prioritize evidence from [High Trust] sources over [Forum Discussion] and [General Web]. If sources conflict, mention that briefly and use the highest-trust evidence. Use only provided context. Every factual sentence must include at least one source citation like [1] or [2]. End with a 'Sources Used:' section mapping source IDs to URLs.".to_string();

    let user = if streaming {
        format!(
            "Query:\n{}\n\nUse only this curated context:\n{}\n\nStream a concise factual answer with [n] citations for each factual sentence, then end with:\nSources Used:\n[n] <url>",
            query, context
        )
    } else {
        format!(
            "Query:\n{}\n\nUse only this curated context:\n{}\n\nReturn the best answer in 3-7 short bullet points. Cite each factual point with [n] where n maps to source IDs from context. Finish with:\nSources Used:\n[n] <url>",
            query, context
        )
    };

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
    let max_chunks = if research_mode { MAX_CONTEXT_CHUNKS_RESEARCH } else { MAX_CONTEXT_CHUNKS };

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

    let client = build_client(&llm_config);
    let model = namespaced_model(&llm_config.provider, &llm_config.model);

    let (system_prompt, user_prompt) = build_prompts(&query, &context, true);

    let chat_req = ChatRequest::new(vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(user_prompt),
    ]);

    match client.exec_chat_stream(&model, chat_req, None).await {
        Ok(mut res) => {
            while let Some(Ok(event)) = res.stream.next().await {
                if let genai::chat::ChatStreamEvent::Chunk(chunk) = event {
                    let json = serde_json::json!({
                        "type": "llm_chunk",
                        "text": chunk.content
                    })
                    .to_string();
                    let _ = tx_sse.send(Ok(Event::default().data(json))).await;
                }
            }
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
// Pings provider's /v1/models endpoint and returns available models
// =============================================================================

pub async fn fetch_provider_models(
    api_key: &str,
    base_url: &str,
) -> Result<Vec<String>, String> {
    if api_key.is_empty() || base_url.is_empty() {
        return Err("api_key and base_url are required".to_string());
    }

    let models_url = format!("{}v1/models", ensure_trailing_slash(base_url.trim()));
    tracing::info!("Fetching models from: {}", models_url);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("client_error: {e}"))?;

    let resp = client
        .get(&models_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("request_failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("http_{}: models endpoint returned error", resp.status().as_u16()));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("json_parse_error: {e}"))?;

    let mut models: Vec<String> = Vec::new();

    if let Some(data) = json.get("data").and_then(|v| v.as_array()) {
        for item in data {
            if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                let id = id.trim().to_string();
                if !id.is_empty() {
                    models.push(id);
                }
            }
        }
    }

    models.sort();
    Ok(models)
}
