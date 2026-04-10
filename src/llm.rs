use std::time::Duration;

use axum::response::sse::Event;
use futures::StreamExt;
use genai::chat::{ChatMessage, ChatRequest};
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ServiceTarget};
use tokio::sync::mpsc;

use crate::models::{LlmConfig, SourceResult};

const MAX_CONTEXT_CHUNKS: usize = 25;
const MAX_CONTEXT_CHARS: usize = 16_000;
const DEFAULT_LLM_TIMEOUT_MS: u64 = 9_000;
const FIRST_BATCH_WAIT_MS: u64 = 2_500;
const PIPELINE_ACCUMULATION_MS: u64 = 1_200;

#[derive(Debug, Default)]
pub struct LlmExecutionResult {
    pub llm_answer: Option<String>,
    pub llm_error: Option<String>,
}

pub async fn summarize_from_stream(
    query: &str,
    llm_config: LlmConfig,
    mut rx: mpsc::Receiver<SourceResult>,
) -> LlmExecutionResult {
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
    while batch.len() < MAX_CONTEXT_CHUNKS {
        match tokio::time::timeout_at(collect_deadline, rx.recv()).await {
            Ok(Some(source)) => batch.push(source),
            Ok(None) | Err(_) => break,
        }
    }

    let context = build_ranked_context(query, &batch);
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
    if provider == "cerebras" {
        return format!("openai::{}", model);
    }

    match provider.as_str() {
        "openai" | "anthropic" | "gemini" | "groq" | "ollama" | "xai" | "deepseek" | "cohere" | "zai" => {
            format!("{}::{}", provider, model)
        }
        _ => model.to_string(),
    }
}

fn build_ranked_context(_query: &str, sources: &[SourceResult]) -> String {
    if sources.is_empty() {
        return String::new();
    }

    let mut context = String::new();
    for (idx, source) in sources.iter().take(MAX_CONTEXT_CHUNKS).enumerate() {
        let block = format!(
            "[{}] {} {} ({})\n{}\n\n",
            idx + 1,
            credibility_tag(&source.url),
            source.title,
            source.url,
            source.extracted_text.trim()
        );

        if context.len() + block.len() > MAX_CONTEXT_CHARS {
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

pub async fn summarize_from_stream_sse(
    query: String,
    llm_config: LlmConfig,
    mut rx: mpsc::Receiver<SourceResult>,
    tx_sse: mpsc::Sender<Result<Event, std::convert::Infallible>>,
) {
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
    while batch.len() < MAX_CONTEXT_CHUNKS {
        match tokio::time::timeout_at(collect_deadline, rx.recv()).await {
            Ok(Some(source)) => batch.push(source),
            Ok(None) | Err(_) => break,
        }
    }

    let context = build_ranked_context(&query, &batch);
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
