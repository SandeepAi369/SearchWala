// ============================================================================
// Qrux v5.0.1 - LLM Synthesis Engine
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
use genai::chat::{ChatMessage, ChatRequest};
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ServiceTarget};
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

    let client = build_client(&llm_config);
    let model = namespaced_model(&llm_config.provider, &llm_config.model);
    let timeout_ms = llm_config.timeout_ms.unwrap_or(DEFAULT_LLM_TIMEOUT_MS);

    let (system_prompt, user_prompt) = build_lite_prompts(query, &context);

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
                    batches_processed: 0,
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

    let client = build_client(&llm_config);
    let model = namespaced_model(&llm_config.provider, &llm_config.model);
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

        let chat_req = ChatRequest::new(vec![
            ChatMessage::system(system_prompt),
            ChatMessage::user(user_prompt),
        ]);

        tracing::info!(
            "LLM batch {}/{} calling model={} context_len={} prev_report_len={}",
            batch_num,
            total_batches,
            model,
            context.len(),
            accumulated_report.len()
        );

        let call_result = match tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            client.exec_chat(&model, chat_req, None),
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
            Ok(chat_res) => {
                let batch_answer = chat_res.first_text().unwrap_or("").trim().to_string();
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

    let client = build_client(&llm_config);
    let model = namespaced_model(&llm_config.provider, &llm_config.model);
    let timeout_ms = llm_config.timeout_ms.unwrap_or(DEFAULT_LLM_TIMEOUT_MS);

    let (system_prompt, user_prompt) = build_lite_prompts(query, &context);

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
                    batches_processed: 0,
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
            format!("openai::{}", model)
        }
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
    let mut used = 0;
    for (idx, relevance) in scored {
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
        used += 1;
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
    chrono::Utc::now().format("%B %d, %Y at %H:%M UTC").to_string()
}

fn build_lite_prompts(query: &str, context: &str) -> (String, String) {
    let system = format!(
        "You are a precise search synthesis engine. Current date: {}. \
         CRITICAL RULES: \
         1. STRICTLY answer the SPECIFIC question asked. Do NOT discuss unrelated topics. \
         2. If the user asks about 'OpenAI', answer ONLY about OpenAI — NOT Google, NOT Anthropic. \
         3. If the user asks about a specific product/company, focus ONLY on that entity. \
         4. Use ONLY information from the provided sources — never hallucinate. \
         5. Every factual statement MUST cite the source like [1], [2]. \
         6. Prioritize [High Trust] sources. Flag outdated information. \
         7. If no source directly answers the query, say so honestly.",
        current_datetime_str()
    );

    let user = format!(
        "USER'S EXACT QUESTION: {}\n\n\
         SOURCES (use ONLY relevant ones):\n{}\n\n\
         Answer the user's EXACT question in 5-8 focused bullet points. \
         Each bullet: 2-3 sentences with [n] citations. \
         IGNORE sources that are not directly relevant to the question. \
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

    let client = build_client(&llm_config);
    let model = namespaced_model(&llm_config.provider, &llm_config.model);

    let (system_prompt, user_prompt) = build_lite_prompts(&query, &context);

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
