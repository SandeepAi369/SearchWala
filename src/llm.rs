use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::response::sse::Event;
use futures::StreamExt;
use genai::chat::{ChatMessage, ChatRequest};
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ServiceTarget};
use tokio::sync::mpsc;

use crate::models::{LlmConfig, SourceResult};

const CONTEXT_TOP_FRACTION: f64 = 0.30;
const MAX_CONTEXT_CHUNKS: usize = 40;
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
    while batch.len() < 10 {
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
    match provider.as_str() {
        "openai" | "anthropic" | "gemini" | "groq" | "ollama" | "xai" | "deepseek" | "cohere" | "zai" => {
            format!("{}::{}", provider, model)
        }
        _ => model.to_string(),
    }
}

fn build_ranked_context(query: &str, sources: &[SourceResult]) -> String {
    struct Candidate {
        score: f64,
        source_header: String,
        paragraph: String,
    }

    let query_terms = tokenize(query);
    let query_phrase = query.trim().to_lowercase();

    let mut candidates = Vec::new();
    for (idx, source) in sources.iter().enumerate() {
        let source_id = idx + 1;
        let source_header = format!(
            "[{}] {} {} ({})",
            source_id,
            credibility_tag(&source.url),
            source.title,
            source.url
        );

        for paragraph in split_into_chunks(&source.extracted_text) {
            let score = score_chunk(&query_terms, &query_phrase, &paragraph);
            if score > 0.0 {
                candidates.push(Candidate {
                    score,
                    source_header: source_header.clone(),
                    paragraph,
                });
            }
        }
    }

    if candidates.is_empty() {
        for (idx, source) in sources.iter().enumerate() {
            if let Some(paragraph) = split_into_chunks(&source.extracted_text).into_iter().next() {
                candidates.push(Candidate {
                    score: 0.1,
                    source_header: format!(
                        "[{}] {} {} ({})",
                        idx + 1,
                        credibility_tag(&source.url),
                        source.title,
                        source.url
                    ),
                    paragraph,
                });
            }
            if candidates.len() >= 8 {
                break;
            }
        }
    }

    if candidates.is_empty() {
        return String::new();
    }

    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut keep = ((candidates.len() as f64) * CONTEXT_TOP_FRACTION).ceil() as usize;
    keep = keep.clamp(1, MAX_CONTEXT_CHUNKS);

    let mut context = String::new();
    for candidate in candidates.into_iter().take(keep) {
        let block = format!("{}\n{}\n\n", candidate.source_header, candidate.paragraph);
        if context.len() + block.len() > MAX_CONTEXT_CHARS {
            break;
        }
        context.push_str(&block);
    }

    context
}

fn split_into_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let normalized = text.replace('\r', "");

    for paragraph in normalized.split("\n\n") {
        let paragraph = paragraph.trim();
        if paragraph.len() < 20 {
            continue;
        }

        if paragraph.len() <= 700 {
            chunks.push(paragraph.to_string());
            continue;
        }

        let mut current = String::new();
        for sentence in paragraph.split_terminator('.') {
            let sentence = sentence.trim();
            if sentence.len() < 4 {
                continue;
            }

            if current.len() + sentence.len() + 2 > 550 && !current.is_empty() {
                chunks.push(current.trim().to_string());
                current.clear();
            }

            current.push_str(sentence);
            current.push_str(". ");
        }

        if current.trim().len() >= 25 {
            chunks.push(current.trim().to_string());
        }
    }

    if chunks.is_empty() {
        let compact = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
        if compact.len() >= 40 {
            chunks.push(compact.chars().take(550).collect());
        }
    }

    chunks
}

fn score_chunk(query_terms: &[String], query_phrase: &str, paragraph: &str) -> f64 {
    let para_lc = paragraph.to_lowercase();
    let tokens = tokenize(&para_lc);
    if tokens.is_empty() {
        return 0.0;
    }

    let mut freq: HashMap<&str, usize> = HashMap::new();
    for token in &tokens {
        *freq.entry(token.as_str()).or_insert(0) += 1;
    }

    let mut score = 0.0;

    if query_terms.is_empty() {
        for token in query_phrase.split_whitespace() {
            let token = token.trim().to_lowercase();
            if token.len() >= 2 && para_lc.contains(&token) {
                score += 1.0;
            }
        }
        if score == 0.0 {
            score = 0.2;
        }
    } else {
        for term in query_terms {
            let count = freq.get(term.as_str()).copied().unwrap_or(0) as f64;
            if count > 0.0 {
                let weight = 1.0 + ((term.len().min(12) as f64 - 2.0) / 10.0);
                score += count * weight;
            }
        }
    }

    if !query_phrase.is_empty() && para_lc.contains(query_phrase) {
        score += 4.0;
    }

    if para_lc.chars().any(|c| c.is_ascii_digit()) {
        score += 0.35;
    }

    score / (1.0 + (paragraph.len() as f64 / 500.0))
}

fn tokenize(text: &str) -> Vec<String> {
    let stop_words: HashSet<&str> = [
        "the", "and", "for", "with", "that", "this", "from", "into", "about", "what", "when", "where", "which", "were", "have", "your", "you", "are", "how", "why", "can", "will", "not", "but", "has", "had", "its", "their", "than", "then",
    ]
    .into_iter()
    .collect();

    text.split(|c: char| !c.is_alphanumeric())
        .filter_map(|token| {
            let token = token.trim().to_lowercase();
            if token.len() < 2 || stop_words.contains(token.as_str()) {
                None
            } else {
                Some(token)
            }
        })
        .collect()
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
    while batch.len() < 10 {
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
