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

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};

use crate::models::{LlmConfig, SourceResult};
use crate::query_intel::{QueryIntelligence, QueryIntent};

/// Language-mirroring instruction appended to every system prompt.
///
/// v7.0's prompts were hardcoded English, so a Hindi query about Hindi sources
/// produced an English answer. For a tool whose best-performing extraction
/// case is Hindi news, that is a product defect, not a nicety.
pub(crate) const LANGUAGE_RULE: &str = " LANGUAGE: Answer in the SAME language the user      asked in. If the question is in Hindi, answer in Hindi; if in Spanish,      answer in Spanish. When the sources are in a different language from the      question, translate the facts into the question's language — never switch      the answer's language to match the sources.";

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
    /// The model's whole window: prompt + generated output.
    pub max_tokens: usize,
}

/// Tokens reserved for the system prompt, instructions and per-source headers.
const SCAFFOLD_TOKENS: usize = 900;

/// Default window assumed for a LOCAL model server.
///
/// Measured 2026-10-02 against Ollama 0.14.2 (qwen2.5-coder 7B, whose own
/// limit is 32,768): a 15k-token prompt sent to the OpenAI-compatible endpoint
/// came back with `prompt_tokens: 4096`. Ollama silently truncates to its
/// runtime `num_ctx`, dropping the START of the prompt — which is where the
/// system prompt and every instruction live. LM Studio and llama.cpp default
/// to similar windows. SearchWala previously assumed 24k tokens for any
/// unknown model, so local users lost ~80% of each prompt without any error.
const LOCAL_SERVER_DEFAULT_TOKENS: usize = 4_096;

fn is_local_llm(cfg: &LlmConfig) -> bool {
    let provider = cfg.provider.to_lowercase();
    if matches!(provider.as_str(), "ollama" | "lmstudio" | "lm_studio" | "llamacpp" | "llama.cpp") {
        return true;
    }
    let Some(base) = cfg.base_url.as_deref() else { return false };
    let Ok(u) = url::Url::parse(base.trim()) else { return false };
    match u.host() {
        Some(url::Host::Domain(d)) => d == "localhost" || d.ends_with(".local") || d.ends_with(".localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

impl ContextBudget {
    /// Budget for a configured provider. Prefer this over `for_model`.
    pub fn for_config(cfg: &LlmConfig) -> Self {
        // The window the user set in Settings, then the server's env.
        let override_tokens = cfg.context_window.filter(|&t| t > 0).or_else(|| std::env::var("CONTEXT_MAX_TOKENS").ok().and_then(|v| v.parse().ok()));
        Self::for_config_with_override(cfg, override_tokens, std::env::var("OLLAMA_CONTEXT_LENGTH").ok().and_then(|v| v.parse().ok()))
    }

    /// Testable core.
    pub fn for_config_with_override(
        cfg: &LlmConfig,
        override_tokens: Option<usize>,
        ollama_ctx: Option<usize>,
    ) -> Self {
        if let Some(v) = override_tokens {
            return Self { max_tokens: v.clamp(2_000, 2_000_000) };
        }
        if is_local_llm(cfg) {
            // Users who raise Ollama's window usually export
            // OLLAMA_CONTEXT_LENGTH in the same shell — honour it.
            let t = ollama_ctx.unwrap_or(LOCAL_SERVER_DEFAULT_TOKENS);
            return Self { max_tokens: t.clamp(2_000, 900_000) };
        }
        Self::for_model_with_override(&cfg.model, None)
    }

    pub fn for_model(model: &str) -> Self {
        Self::for_model_with_override(
            model,
            std::env::var("CONTEXT_MAX_TOKENS").ok().and_then(|v| v.parse::<usize>().ok()),
        )
    }

    /// Testable core — the override is a parameter, not an env read.
    pub fn for_model_with_override(model: &str, override_tokens: Option<usize>) -> Self {
        if let Some(v) = override_tokens {
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
            // Unknown hosted model: conservative.
            24_000
        };
        Self { max_tokens }
    }

    /// Largest output worth requesting: an eighth of the window, never more
    /// than a third of it. The old fixed 2,048-token floor, on a 4,096-token
    /// local window, left almost nothing for the sources.
    pub fn max_output_tokens(&self) -> usize {
        let floor = if self.max_tokens >= 16_000 { 2_048 } else { 256 };
        (self.max_tokens / 8).clamp(floor, 32_000).min(self.max_tokens / 3)
    }

    /// Cap a mode's requested output so it fits the window.
    pub fn fit_output(&self, requested: usize) -> usize {
        requested.min(self.max_tokens / 3).max(128)
    }

    /// Tokens left for source text once `output_tokens` and the prompt
    /// scaffolding are reserved. Prompt and output share ONE window.
    pub fn context_tokens(&self, output_tokens: usize) -> usize {
        self.max_tokens
            .saturating_sub(output_tokens + SCAFFOLD_TOKENS)
            .max(400)
    }

    /// Characters of source text at an English-like density, with the default
    /// output reserved. Prefer `chars_for_tokens` with the real sources.
    pub fn max_context_chars(&self) -> usize {
        self.context_tokens(self.max_output_tokens()) * 4
    }
}

/// How many characters of THESE sources fit in `tokens`.
///
/// A fixed chars-per-token ratio is wrong across scripts: English runs ~4
/// characters per token, Devanagari closer to 1-1.5. The previous code used
/// 3.3 for everything (its comment claimed this was "safe" for Indic scripts,
/// which has the direction backwards), so a Hindi context overshot its token
/// budget by roughly 2-3x. This measures the density of the actual text.
pub fn chars_for_tokens(tokens: usize, sources: &[SourceResult]) -> usize {
    let mut chars = 0usize;
    let mut est = 0usize;
    for s in sources.iter().take(40) {
        let sample = crate::util::truncate_chars(&s.extracted_text, 2_000);
        chars += crate::util::char_len(sample);
        est += est_tokens(sample);
    }
    let per_token = if est == 0 { 4.0 } else { (chars as f64 / est as f64).clamp(0.8, 4.5) };
    (tokens as f64 * per_token) as usize
}

/// Rough token estimate that accounts for script density.
pub fn est_tokens(s: &str) -> usize {
    let mut units = 0usize;
    for c in s.chars() {
        units += if c.is_ascii() { 1 } else { 3 };
    }
    units / 4 + 1
}

// MAX_CONTEXT_CHARS_LITE (16_000) REMOVED in v8.0 — superseded by
// ContextBudget::for_model, which scales with the model instead of discarding
// >97% of the evidence the pipeline gathered.
pub(crate) const DEFAULT_LLM_TIMEOUT_MS: u64 = 45_000;

#[derive(Debug, Default)]
pub struct LlmExecutionResult {
    pub llm_answer: Option<String>,
    pub llm_error: Option<String>,
    pub batches_processed: usize,
    /// citation number (1-based) -> index into the sources slice.
    pub citation_map: Vec<usize>,
    /// Support check per citation.
    pub citation_checks: Vec<CitationCheck>,
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

fn resolve_provider(config: &LlmConfig) -> Result<ProviderInfo, String> {
    let provider = config.provider.trim().to_lowercase();

    // If the user provided a custom base_url, VALIDATE it first (v8.0 P1-12a).
    // This value arrives in the request body, and with permissive CORS v7.0
    // would relay the user's API key to any address an attacker chose.
    if let Some(ref base_url) = config.base_url {
        let url = base_url.trim();
        if !url.is_empty() {
            if let Err(e) = crate::ssrf::validate_llm_base_url(url) {
                // v8.1: this used to fall back to the provider's DEFAULT
                // endpoint. For "openai_compatible" that is api.openai.com, so
                // a key meant for a custom/LAN endpoint was sent to OpenAI.
                tracing::warn!("Rejected caller-supplied base_url: {}", e);
                return Err(format!("base_url rejected: {e}"));
            }
            let format = if provider == "anthropic" {
                ApiFormat::Anthropic
            } else {
                // All other providers (including Gemini) use OpenAI-compatible format
                ApiFormat::OpenAiCompatible
            };
            return Ok(ProviderInfo {
                format,
                base_url: ensure_trailing_slash(&normalize_base_url(url, &provider)),
            });
        }
    }

    Ok(default_provider_for(&provider))
}

/// Trim endpoint paths users paste along with the base URL. The chat path is
/// appended later, so ".../v1/chat/completions" became
/// ".../v1/chat/completions/chat/completions", and an Anthropic base of
/// "https://api.anthropic.com/v1" became ".../v1/v1/messages".
pub(crate) fn normalize_base_url(raw: &str, provider: &str) -> String {
    let mut u = raw.trim().trim_end_matches('/').to_string();
    for suffix in ["/chat/completions", "/completions", "/v1/messages", "/messages", "/models"] {
        if let Some(stripped) = u.strip_suffix(suffix) {
            u = stripped.trim_end_matches('/').to_string();
            break;
        }
    }
    if provider == "anthropic" {
        if let Some(stripped) = u.strip_suffix("/v1") {
            u = stripped.to_string();
        }
    }
    u
}

/// Normalise and validate a caller-supplied LLM config once, where the
/// request enters the pipeline. Every later stage can then trust it.
///
/// Fixes, each a real failure mode:
///   * keys pasted with a trailing newline/space failed `HeaderValue`
///     parsing, the auth header was silently dropped, and the provider
///     answered 401 with no hint why;
///   * a pasted "Bearer sk-…" became "Bearer Bearer sk-…";
///   * " gpt-4o-mini" (leading space) is a 404 "model not found";
///   * an empty key was sent as "Bearer " instead of being reported;
///   * a rejected base_url fell back to another provider's endpoint;
///   * `timeout_ms` had no upper bound.
pub fn prepare_config(mut c: LlmConfig) -> Result<LlmConfig, String> {
    c.provider = c.provider.trim().to_lowercase();
    c.model = c.model.trim().to_string();
    let mut key = c.api_key.trim();
    for prefix in ["Bearer ", "bearer ", "BEARER "] {
        if let Some(rest) = key.strip_prefix(prefix) {
            key = rest.trim();
        }
    }
    c.api_key = key.to_string();
    c.base_url = c.base_url.map(|b| b.trim().to_string()).filter(|b| !b.is_empty());

    if c.model.is_empty() {
        return Err("llm_config: no model name was given".into());
    }
    if c.api_key.chars().any(|ch| ch.is_whitespace() || ch.is_control() || !ch.is_ascii()) {
        return Err("llm_config: the API key contains spaces, line breaks or non-ASCII characters; copy it again from your provider".into());
    }
    if c.api_key.is_empty() && !is_local_llm(&c) {
        return Err("llm_config: the API key is empty".into());
    }
    if let Some(b) = &c.base_url {
        crate::ssrf::validate_llm_base_url(b).map_err(|e| format!("llm_config: base_url rejected: {e}"))?;
    } else if !KNOWN_PROVIDERS.contains(&c.provider.as_str()) {
        // Unknown names fell through to api.openai.com — the key went to a
        // provider the user never chose.
        return Err(format!(
            "llm_config: provider {:?} needs a base_url (the endpoint of your OpenAI-compatible server)",
            c.provider
        ));
    }
    c.timeout_ms = c.timeout_ms.map(|t| t.clamp(1_000, 600_000));
    Ok(c)
}

/// Provider names with a built-in endpoint (see `default_provider_for`).
const KNOWN_PROVIDERS: &[&str] = &[
    "gemini", "google", "anthropic", "openai", "groq", "together", "openrouter", "cerebras", "deepseek", "xai",
    "ollama", "cohere", "fireworks", "perplexity", "mistral_api", "mistral", "sambanova", "nvidia_nim",
];

/// Remove the caller's key from any text that may be shown or logged.
/// Some providers echo the submitted key in their 401 message.
pub(crate) fn redact_key(text: &str, key: &str) -> String {
    if key.len() < 8 {
        return text.to_string();
    }
    text.replace(key, "<redacted>")
}

/// Built-in endpoint for a known provider name.
fn default_provider_for(provider: &str) -> ProviderInfo {
    match provider {
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
            format!("{}v1beta/models/{}:{}", ensure_trailing_slash(&info.base_url), model, action)
        }
        ApiFormat::Anthropic => {
            // v8.1: this was `base.trim_end_matches('/')` + "v1/messages",
            // i.e. "https://api.anthropic.comv1/messages" — Anthropic calls
            // could never have succeeded.
            format!("{}v1/messages", ensure_trailing_slash(&info.base_url))
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
        ApiFormat::OpenAiCompatible if config.api_key.is_empty() => {
            // Local servers (Ollama, llama.cpp) need no auth.
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
                        // Join, don't overwrite: a second system message
                        // replaced the first.
                        if !system_text.is_empty() {
                            system_text.push_str("\n\n");
                        }
                        system_text.push_str(&msg.content);
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
                        // Join, don't overwrite: a second system message
                        // replaced the first.
                        if !system_text.is_empty() {
                            system_text.push_str("\n\n");
                        }
                        system_text.push_str(&msg.content);
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
                // v8.1: was `.max(8192)`, which forced even Fast's two-sentence
                // answers onto an 8K budget. Modes now set the length deliberately.
                "max_tokens": max_output_tokens.max(256),
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
            // v8.0: `content` may be a string, null, or an array of typed
            // parts; reasoning models sometimes put the answer only in
            // `reasoning_content`. v7.0 handled the string case alone and
            // reported "empty_response" for everything else.
            let msg = json.get("choices")?.get(0)?.get("message")?;
            match msg.get("content") {
                Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
                Some(serde_json::Value::Array(parts)) => {
                    let s: String = parts
                        .iter()
                        .filter(|p| {
                            p.get("type").and_then(|t| t.as_str()).is_none_or(|t| t == "text")
                        })
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect();
                    (!s.trim().is_empty()).then_some(s)
                }
                _ => msg
                    .get("reasoning_content")
                    .and_then(|t| t.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string),
            }
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
            // v8.0: scan for the first TEXT block instead of assuming index 0.
            //
            // With extended thinking enabled, content[0] is a
            // {"type":"thinking", ...} block with no "text" field, so v7.0's
            // `content[0].text` returned None and the whole call failed with
            // "empty_response" — the answer was silently discarded.
            let blocks = json.get("content")?.as_array()?;
            let mut out = String::new();
            for b in blocks {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        out.push_str(t);
                    }
                }
            }
            (!out.trim().is_empty()).then_some(out)
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
    if crate::util::char_len(&raw) > 300 {
        format!("{}...", crate::util::truncate_chars(&raw, 300))
    } else {
        raw
    }
}

// =============================================================================
// Core LLM Call — Non-Streaming (replaces genai client.exec_chat)
// =============================================================================

/// ONE shared client. v7.0 called `build_http_client()` inside every
/// `call_llm`, constructing a fresh TLS stack and connection pool per call —
/// in research mode that is one client per batch.
static LLM_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

pub(crate) fn http_client() -> &'static reqwest::Client {
    LLM_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(300))
            // A validated base_url must not be able to redirect us to an
            // address validation would have refused (169.254.169.254, LAN).
            // LLM APIs do not redirect POSTs.
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(8)
            .tcp_nodelay(true)
            .build()
            .expect("LLM HTTP client error")
    })
}

/// Retryable provider failures: rate limits and transient server errors.
fn is_retryable(err: &str) -> bool {
    err.starts_with("http_429")
        || err.starts_with("http_500")
        || err.starts_with("http_502")
        || err.starts_with("http_503")
        || err.starts_with("http_504")
        || err.starts_with("http_529")
        || err.contains("http_request_failed")
        || err.contains("response_read_error")
}

/// `call_llm` with exponential backoff. v7.0 had NO retry at all: a single 429
/// from a rate-limited provider failed the whole search.
pub(crate) async fn call_llm_retrying(
    config: &LlmConfig,
    messages: &[ChatMessage],
) -> Result<String, String> {
    const ATTEMPTS: usize = 4;
    let mut delay = Duration::from_millis(700);
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match call_llm(config, messages).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = e;
                if attempt == ATTEMPTS || !is_retryable(&last) {
                    break;
                }
                tracing::warn!(
                    "LLM attempt {}/{} failed ({}), retrying in {:?}",
                    attempt, ATTEMPTS, last, delay
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(8));
            }
        }
    }
    Err(last)
}

/// Output-token budget for one call: the mode's explicit value when set
/// (Fast answers are deliberately short), else the model-derived default.
fn output_tokens_for(config: &LlmConfig) -> usize {
    config
        .max_output_tokens
        .map(|n| n.clamp(128, 64_000))
        .unwrap_or_else(|| ContextBudget::for_config(config).max_output_tokens())
}

/// Make a single non-streaming LLM call. Returns extracted text or error string.
pub(crate) async fn call_llm(
    config: &LlmConfig,
    messages: &[ChatMessage],
) -> Result<String, String> {
    let info = resolve_provider(config)?;
    let url = build_chat_url(&info, &config.model, false);
    let headers = build_auth_headers(config, &info);
    let body = build_request_body(messages, &config.model, &info, false, output_tokens_for(config));

    tracing::info!(
        "LLM call: provider={} model={} format={:?} url={}",
        config.provider, config.model, info.format,
        url.split('?').next().unwrap_or(&url) // Don't log API key in Gemini URL params
    );

    let resp = http_client()
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| redact_key(&format!("http_request_failed: {e}"), &config.api_key))?;

    let status = resp.status();
    let resp_text = resp.text().await.map_err(|e| format!("response_read_error: {e}"))?;

    if !status.is_success() {
        let error_msg = match serde_json::from_str::<serde_json::Value>(&resp_text) {
            Ok(json) => extract_error_message(&json, &info.format),
            Err(_) => resp_text.chars().take(500).collect(),
        };
        return Err(redact_key(&format!("http_{}: {}", status.as_u16(), error_msg), &config.api_key));
    }

    let json: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| format!("json_parse_error: {e}"))?;

    if was_truncated(&json) {
        // Recorded per task so synthesis can tell the user, instead of
        // returning an answer that silently stops mid-section.
        let _ = TRUNCATED.try_with(|t| t.set(true));
        tracing::info!("LLM output hit the max_tokens limit (model={})", config.model);
    }

    extract_response_text(&json, &info.format)
        .ok_or_else(|| {
            // Log raw response structure for debugging (without API keys)
            let keys: Vec<&str> = json.as_object()
                .map(|o| o.keys().map(|k| k.as_str()).collect())
                .unwrap_or_default();
            format!("empty_response: could not extract text from response (keys: {:?})", keys)
        })
}

tokio::task_local! {
    /// Set when any LLM call in the current scope stopped at its token limit.
    pub(crate) static TRUNCATED: std::cell::Cell<bool>;
}

/// Did the provider stop because it ran out of output tokens?
/// OpenAI-compatible: `finish_reason: "length"`; Anthropic: `stop_reason:
/// "max_tokens"`; Gemini (OpenAI-compatible endpoint) also reports "length".
pub fn was_truncated(json: &serde_json::Value) -> bool {
    let oai = json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str());
    let anthropic = json.get("stop_reason").and_then(|f| f.as_str());
    matches!(oai, Some("length") | Some("max_tokens")) || anthropic == Some("max_tokens")
}

// =============================================================================
// Core LLM Call — SSE Streaming (replaces genai client.exec_chat_stream)
// =============================================================================



// =============================================================================
// DIRECT SYNTHESIS — Single-call for lite mode
// =============================================================================

// =============================================================================
// v5.2.0: INTENT-AWARE SYNTHESIS — Selects optimal prompt per query intent
// =============================================================================


// =============================================================================
// ITERATIVE DEEP RESEARCH — Multi-batch synthesis for long-form reports
//
// Flow:
//   1. Split all sources into batches of 50
//   2. Batch 1: Generate initial comprehensive report
//   3. Batch 2+: Send previous_report + new sources → LLM expands
//   4. Return final accumulated long-form report
// =============================================================================


// =============================================================================
// CHANNEL-BASED SYNTHESIS — Used by stream.rs (SSE streaming path)
// =============================================================================

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
/// ```text
/// if src.title.to_lowercase().contains(kw) { score += 3.0 }
/// if text_lower.contains(kw)               { score += 1.0 }
/// ```
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
pub(crate) fn build_ranked_context(sources: &[SourceResult], budget_chars: usize) -> BuiltContext {
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

pub(crate) fn credibility_tag(url: &str) -> String {
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

pub(crate) fn current_datetime_str() -> String {
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

pub(crate) fn post_process_answer(raw: &str, _query: &str) -> String {
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
///
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
            .split(|c: char| !crate::util::is_word_char(c))
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





// =============================================================================
// v5.2.0: INTENT-AWARE PROMPT BUILDER
// Selects optimal system/user prompt template based on QueryIntelligence
// =============================================================================

pub(crate) fn build_intent_prompts(query: &str, context: &str, intel: &QueryIntelligence) -> (String, String) {
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
    let provider = provider.trim().to_lowercase();
    let api_key = api_key.trim().trim_start_matches("Bearer ").trim();
    // No base_url: use the provider's own endpoint. The UI presets for
    // OpenAI, Anthropic and Gemini have none, so "Fetch list" used to refuse
    // to run for exactly the most common providers.
    let base = if base_url.trim().is_empty() {
        default_provider_for(&provider).base_url
    } else {
        normalize_base_url(base_url, &provider)
    };
    let probe = LlmConfig {
        provider: provider.clone(),
        api_key: api_key.to_string(),
        model: String::new(),
        base_url: Some(base.clone()),
        timeout_ms: None,
        max_output_tokens: None,
        context_window: None,
    };
    if api_key.is_empty() && !is_local_llm(&probe) {
        return Err("an API key is required".to_string());
    }
    if api_key.chars().any(|c| c.is_whitespace() || c.is_control() || !c.is_ascii()) {
        return Err("the API key contains spaces, line breaks or non-ASCII characters".to_string());
    }

    let is_anthropic = provider == "anthropic";
    let models_url = if is_anthropic {
        format!("{}v1/models", ensure_trailing_slash(base.trim_end_matches("/v1")))
    } else {
        format!("{}models", ensure_trailing_slash(&base))
    };
    tracing::info!("Fetching models from: {}", models_url);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("client_error: {e}"))?;

    let mut req = client
        .get(&models_url)
        .header("Accept", "application/json");

    if is_anthropic {
        req = req
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01");
    } else if !api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", api_key));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| redact_key(&format!("request_failed: {e}"), api_key))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let err_text = resp.text().await.unwrap_or_default();
        let detail = match serde_json::from_str::<serde_json::Value>(&err_text) {
            Ok(json) => extract_error_message(&json, &ApiFormat::OpenAiCompatible),
            Err(_) => err_text.chars().take(200).collect(),
        };
        return Err(redact_key(&format!("http_{}: {}", status, detail), api_key));
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
        let gemini = ContextBudget::for_model_with_override("gemini-3-pro", None);
        let haiku = ContextBudget::for_model_with_override("claude-haiku-4-5", None);
        let unknown = ContextBudget::for_model_with_override("some-local-llama", None);
        assert!(gemini.max_context_chars() > haiku.max_context_chars());
        assert!(haiku.max_context_chars() > unknown.max_context_chars());
        // All of them must beat the v7.0 fixed 16,000.
        assert!(unknown.max_context_chars() > 16_000, "even the conservative default should beat v7.0");
        assert!(gemini.max_context_chars() > 500_000);
    }

    #[test]
    fn context_budget_env_override_is_clamped() {
        assert!(ContextBudget::for_model_with_override("x", Some(999_999_999)).max_tokens <= 900_000);
        assert!(ContextBudget::for_model_with_override("x", Some(1)).max_tokens >= 2_000);
        assert_eq!(ContextBudget::for_model_with_override("x", Some(50_000)).max_tokens, 50_000);
    }

    #[test]
    fn max_output_tokens_is_never_the_v7_truncating_4096_for_big_models() {
        let b = ContextBudget::for_model_with_override("claude-opus-5", None);
        assert!(b.max_output_tokens() > 4096, "research reports were truncated at 4096");
    }

    fn cfg(provider: &str, model: &str, base: Option<&str>) -> crate::models::LlmConfig {
        crate::models::LlmConfig {
            provider: provider.into(),
            api_key: "k".into(),
            model: model.into(),
            base_url: base.map(str::to_string),
            timeout_ms: None,
            max_output_tokens: None,
            context_window: None,
        }
    }

    #[test]
    fn detects_truncated_responses_across_providers() {
        assert!(was_truncated(&serde_json::json!({"choices":[{"finish_reason":"length"}]})));
        assert!(was_truncated(&serde_json::json!({"stop_reason":"max_tokens"})));
        assert!(!was_truncated(&serde_json::json!({"choices":[{"finish_reason":"stop"}]})));
        assert!(!was_truncated(&serde_json::json!({"stop_reason":"end_turn"})));
        assert!(!was_truncated(&serde_json::json!({})));
    }

    #[test]
    fn local_servers_get_a_window_they_actually_have() {
        // Measured: Ollama truncated a 15k-token prompt to 4,096 tokens.
        let o = ContextBudget::for_config_with_override(&cfg("ollama", "qwen2.5-coder:7b", Some("http://localhost:11434/v1")), None, None);
        assert_eq!(o.max_tokens, 4_096);
        // Any loopback/private base URL is treated as a local server.
        let lm = ContextBudget::for_config_with_override(&cfg("openai_compatible", "local-model", Some("http://127.0.0.1:1234/v1")), None, None);
        assert_eq!(lm.max_tokens, 4_096);
        // OLLAMA_CONTEXT_LENGTH is honoured, and CONTEXT_MAX_TOKENS wins over all.
        let big = ContextBudget::for_config_with_override(&cfg("ollama", "m", None), None, Some(32_768));
        assert_eq!(big.max_tokens, 32_768);
        let forced = ContextBudget::for_config_with_override(&cfg("ollama", "m", None), Some(8_000), Some(32_768));
        assert_eq!(forced.max_tokens, 8_000);
        // Hosted providers keep their model-based budgets.
        let hosted = ContextBudget::for_config_with_override(&cfg("anthropic", "claude-opus-5", None), None, None);
        assert!(hosted.max_tokens >= 100_000);
    }

    #[test]
    fn prompt_and_output_fit_inside_one_window() {
        for w in [4_096usize, 8_000, 32_768, 150_000] {
            let b = ContextBudget { max_tokens: w };
            for requested in [600, 2_000, 4_000, 8_000] {
                let out = b.fit_output(requested);
                let ctx = b.context_tokens(out);
                assert!(out + ctx + 900 <= w.max(out + 400 + 900), "window {w}, requested {requested}: {out}+{ctx} overflows");
                assert!(out <= w / 3 || out == 128);
            }
            assert!(b.max_output_tokens() <= w / 3);
        }
        // The old fixed 2,048 floor would have taken half a 4k window.
        assert!(ContextBudget { max_tokens: 4_096 }.max_output_tokens() <= 1_365);
    }

    #[test]
    fn character_budget_follows_the_script_of_the_sources() {
        let en = vec![src("https://a.test/1", "T", &"The quick brown fox jumps over the lazy dog. ".repeat(40))];
        let hi = vec![src("https://b.test/1", "T", &"भारत और पाकिस्तान के बीच मुकाबला खेला जाएगा। ".repeat(40))];
        let en_chars = chars_for_tokens(1_000, &en);
        let hi_chars = chars_for_tokens(1_000, &hi);
        assert!(en_chars >= 3_000, "English ~4 chars/token, got {en_chars}");
        assert!(hi_chars < en_chars / 2, "Devanagari must get far fewer chars per token: {hi_chars} vs {en_chars}");
        // No sources: falls back to English density instead of dividing by zero.
        assert_eq!(chars_for_tokens(100, &[]), 400);
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

    // ── v8.1: API key / config handling ──

    fn user_cfg(provider: &str, key: &str, model: &str, base: Option<&str>) -> crate::models::LlmConfig {
        crate::models::LlmConfig {
            provider: provider.into(),
            api_key: key.into(),
            model: model.into(),
            base_url: base.map(str::to_string),
            timeout_ms: None,
            max_output_tokens: None,
            context_window: None,
        }
    }

    #[test]
    fn pasted_keys_are_cleaned_not_silently_dropped() {
        let c = prepare_config(user_cfg(" Groq ", "  gsk_abc123XYZ\n", " llama-3.3-70b ", Some(" https://api.groq.com/openai/v1 "))).unwrap();
        assert_eq!(c.api_key, "gsk_abc123XYZ");
        assert_eq!(c.model, "llama-3.3-70b");
        assert_eq!(c.provider, "groq");
        assert_eq!(c.base_url.as_deref(), Some("https://api.groq.com/openai/v1"));
        let headers = build_auth_headers(&c, &resolve_provider(&c).unwrap());
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer gsk_abc123XYZ");

        let b = prepare_config(user_cfg("openai", "Bearer sk-live-123", "gpt-4o-mini", None)).unwrap();
        assert_eq!(b.api_key, "sk-live-123", "a pasted 'Bearer ' prefix must not be doubled");
    }

    #[test]
    fn bad_configs_are_reported_with_a_reason() {
        let e = prepare_config(user_cfg("openai", "sk-abc def", "gpt-4o-mini", None)).unwrap_err();
        assert!(e.contains("API key"), "{e}");
        let e = prepare_config(user_cfg("openai", "sk-ab\u{200b}c", "gpt-4o-mini", None)).unwrap_err();
        assert!(e.contains("non-ASCII"), "zero-width space: {e}");
        let e = prepare_config(user_cfg("openai", "", "gpt-4o-mini", None)).unwrap_err();
        assert!(e.contains("empty"), "{e}");
        let e = prepare_config(user_cfg("openai", "sk-1", "  ", None)).unwrap_err();
        assert!(e.contains("model"), "{e}");
        let c = prepare_config(user_cfg("openai", "sk-1", "m", None)).map(|mut c| {
            c.timeout_ms = Some(u64::MAX);
            c
        }).unwrap();
        assert_eq!(prepare_config(c).unwrap().timeout_ms, Some(600_000));
    }

    #[test]
    fn local_models_need_no_key_and_send_no_auth_header() {
        let c = prepare_config(user_cfg("ollama", "", "qwen2.5", None)).unwrap();
        let headers = build_auth_headers(&c, &resolve_provider(&c).unwrap());
        assert!(headers.get(AUTHORIZATION).is_none());
    }

    #[test]
    fn a_rejected_base_url_is_an_error_not_a_fallback() {
        // Previously: fell back to api.openai.com and sent the custom key there.
        let c = user_cfg("openai_compatible", "sk-custom", "m", Some("http://169.254.169.254/v1"));
        assert!(resolve_provider(&c).is_err());
        assert!(prepare_config(c).unwrap_err().contains("base_url rejected"));
    }

    #[test]
    fn pasted_endpoint_paths_are_trimmed_from_base_urls() {
        assert_eq!(normalize_base_url("https://api.x.ai/v1/chat/completions", "xai"), "https://api.x.ai/v1");
        assert_eq!(normalize_base_url("https://api.anthropic.com/v1/", "anthropic"), "https://api.anthropic.com");
        assert_eq!(normalize_base_url("https://api.anthropic.com/v1/messages", "anthropic"), "https://api.anthropic.com");
        assert_eq!(normalize_base_url("https://openrouter.ai/api/v1/", "openrouter"), "https://openrouter.ai/api/v1");
        let c = user_cfg("anthropic", "k", "claude", Some("https://api.anthropic.com/v1"));
        assert_eq!(build_chat_url(&resolve_provider(&c).unwrap(), "claude", false), "https://api.anthropic.com/v1/messages");
        let o = user_cfg("openai_compatible", "k", "m", Some("https://llm.example.com/v1/chat/completions"));
        assert_eq!(build_chat_url(&resolve_provider(&o).unwrap(), "m", false), "https://llm.example.com/v1/chat/completions");
    }

    #[test]
    fn unknown_provider_without_base_url_is_refused() {
        let e = prepare_config(user_cfg("openai_compatible", "sk-x", "m", None)).unwrap_err();
        assert!(e.contains("needs a base_url"), "{e}");
        assert!(prepare_config(user_cfg("openai_compatible", "sk-x", "m", Some("https://llm.example.com/v1"))).is_ok());
    }

    #[test]
    fn anthropic_joins_multiple_system_messages() {
        let msgs = vec![
            ChatMessage { role: "system".into(), content: "A".into() },
            ChatMessage { role: "system".into(), content: "B".into() },
            ChatMessage { role: "user".into(), content: "q".into() },
        ];
        let info = ProviderInfo { format: ApiFormat::Anthropic, base_url: "https://api.anthropic.com/".into() };
        let body = build_request_body(&msgs, "claude", &info, false, 512);
        assert_eq!(body["system"], "A\n\nB");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn default_anthropic_endpoint_is_well_formed() {
        let c = user_cfg("anthropic", "k", "claude-haiku", None);
        assert_eq!(build_chat_url(&resolve_provider(&c).unwrap(), "claude-haiku", false), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn keys_are_redacted_from_provider_errors() {
        let msg = "Incorrect API key provided: sk-proj-abcdef123456. Check your key.";
        assert_eq!(redact_key(msg, "sk-proj-abcdef123456"), "Incorrect API key provided: <redacted>. Check your key.");
        assert_eq!(redact_key("x", ""), "x");
    }
}
