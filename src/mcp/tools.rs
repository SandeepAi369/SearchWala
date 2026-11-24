// ============================================================================
// SearchWala MCP — Tool registry, JSON Schemas, formatters & handlers
//
// Exposes 4 sharply-scoped, read-only tools:
//   1. search_web       — fast multi-engine consensus SERP discovery (~1–3 s)
//   2. search_passages  — full pipeline: search + extract + BM25+/MMR + highlight
//   3. extract_url      — SSRF-guarded deep page extraction (+ optional BM25+ focus)
//   4. search_status    — version, modes/domains, engine health, cache stats
// ============================================================================

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cache::TempDb;
use crate::mcp::protocol::{CallToolResult, JsonRpcNotification, ToolAnnotations, ToolDescriptor};
use crate::models::{DiscoveredDomain, SearchHit, SourceResult};
use crate::search::{
    self, DomainFilter, ExtractedPage, FetchExtractOptions, Progress, SearchParams,
    SerpSearchResponse,
};

/// Channel used by transports (stdio / Streamable HTTP SSE) to forward live
/// `notifications/progress` messages while a tool is running.
pub type ProgressSender = tokio::sync::mpsc::Sender<JsonRpcNotification>;

pub const VALID_MODES: &[&str] = &[
    "fast", "auto", "deep", "study", "academic", "reddit", "youtube",
];

pub const VALID_DOMAINS: &[&str] = &["general", "tech", "science", "finance", "health", "news"];

const DEFAULT_WEB_RESULTS: usize = 15;
const MIN_WEB_RESULTS: usize = 1;
const MAX_WEB_RESULTS: usize = 50;

const DEFAULT_PASSAGES_TOP_K: usize = 8;
const MIN_PASSAGES_TOP_K: usize = 1;
const MAX_PASSAGES_TOP_K: usize = 50;

const MIN_MAX_URLS: usize = 3;
const MAX_MAX_URLS: usize = 150;

const DEFAULT_PASSAGES_MAX_CHARS: usize = 12_000;
const MIN_PASSAGES_MAX_CHARS: usize = 2_000;
const MAX_PASSAGES_MAX_CHARS: usize = 120_000;

const DEFAULT_EXTRACT_MAX_CHARS: usize = 12_000;
const MIN_EXTRACT_MAX_CHARS: usize = 500;
const MAX_EXTRACT_MAX_CHARS: usize = 100_000;

const DEFAULT_EXTRACT_TOP_K: usize = 8;
const MIN_EXTRACT_TOP_K: usize = 1;
const MAX_EXTRACT_TOP_K: usize = 20;

const MAX_INCLUDE_DOMAINS: usize = 10;
const MAX_EXCLUDE_DOMAINS: usize = 20;

const SEARCH_WEB_TEXT_SNIPPET_CHARS: usize = 200;
const SEARCH_WEB_TEXT_ABSTRACT_CHARS: usize = 320;

/// Full descriptor list returned by `tools/list`.
pub fn list_tools() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: "search_web",
            title: "Multi-Engine Consensus Web Search",
            description: "Fast multi-engine consensus web search (~1-3s) returning ranked URLs, snippets, family agreement, and a compact list of additional discovered domains without scraping. Supports include_domains, exclude_domains, and strict_domains (false prioritizes included domains alongside organic results; true restricts strictly to them).",
            input_schema: search_web_input_schema(),
            output_schema: search_web_output_schema(),
            annotations: ToolAnnotations {
                title: "Multi-Engine Consensus Web Search",
                read_only_hint: true,
                destructive_hint: false,
                idempotent_hint: false,
                open_world_hint: true,
            },
        },
        ToolDescriptor {
            name: "search_passages",
            title: "Web Search & BM25+/MMR Passage Retrieval",
            description: "Full web grounding pipeline: multi-engine consensus search, deep page extraction, BM25+/MMR passage ranking, verbatim quick-answer highlight, and a compact list of additional discovered domains. Supports include_domains, exclude_domains, and strict_domains (false keeps organic answers while prioritizing included domains; true scrapes strictly from them).",
            input_schema: search_passages_input_schema(),
            output_schema: search_passages_output_schema(),
            annotations: ToolAnnotations {
                title: "Web Search & BM25+/MMR Passage Retrieval",
                read_only_hint: true,
                destructive_hint: false,
                idempotent_hint: false,
                open_world_hint: true,
            },
        },
        ToolDescriptor {
            name: "extract_url",
            title: "Deep Article & Web Page Extraction",
            description: "Fetch a public http(s) URL (SSRF-guarded, 6h cache) and extract clean readable text and metadata. Pass optional `query` to return only the top BM25+/MMR-ranked passages relevant to that question.",
            input_schema: extract_url_input_schema(),
            output_schema: extract_url_output_schema(),
            annotations: ToolAnnotations {
                title: "Deep Article & Web Page Extraction",
                read_only_hint: true,
                destructive_hint: false,
                idempotent_hint: false,
                open_world_hint: true,
            },
        },
        ToolDescriptor {
            name: "search_status",
            title: "SearchWala Engine Health & Cache Status",
            description: "Return SearchWala version, available modes and domains, live per-engine health verdicts, and SERP/page cache stats.",
            input_schema: search_status_input_schema(),
            output_schema: search_status_output_schema(),
            annotations: ToolAnnotations {
                title: "SearchWala Engine Health & Cache Status",
                read_only_hint: true,
                destructive_hint: false,
                idempotent_hint: true,
                open_world_hint: false,
            },
        },
    ]
}

/// Dispatch one `tools/call` invocation. Returns `Err(msg)` only for unknown
/// tool names or malformed JSON argument envelopes (mapped to JSON-RPC `-32602`);
/// operational/input failures return `Ok(CallToolResult::error(...))`.
pub async fn call_tool(
    name: &str,
    arguments: Option<Value>,
    temp_db: &TempDb,
    progress_token: Option<Value>,
    progress_tx: Option<ProgressSender>,
) -> Result<CallToolResult, String> {
    let args_val = arguments.unwrap_or_else(|| json!({}));
    if !args_val.is_object() {
        return Err("tool arguments must be a JSON object".into());
    }

    match name {
        "search_web" => {
            let args: SearchWebArgs = serde_json::from_value(args_val)
                .map_err(|e| format!("invalid arguments for search_web: {e}"))?;
            Ok(exec_search_web(args, progress_token, progress_tx).await)
        }
        "search_passages" => {
            let args: SearchPassagesArgs = serde_json::from_value(args_val)
                .map_err(|e| format!("invalid arguments for search_passages: {e}"))?;
            Ok(exec_search_passages(args, temp_db, progress_token, progress_tx).await)
        }
        "extract_url" => {
            let args: ExtractUrlArgs = serde_json::from_value(args_val)
                .map_err(|e| format!("invalid arguments for extract_url: {e}"))?;
            Ok(exec_extract_url(args).await)
        }
        "search_status" => {
            let _args: SearchStatusArgs = serde_json::from_value(args_val)
                .map_err(|e| format!("invalid arguments for search_status: {e}"))?;
            Ok(exec_search_status())
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

// ----------------------------------------------------------------------------
// Input Argument Structs & Validation Helpers
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchWebArgs {
    pub query: String,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub max_results: Option<usize>,
    #[serde(default)]
    pub include_domains: Option<Vec<String>>,
    #[serde(default)]
    pub exclude_domains: Option<Vec<String>>,
    #[serde(default)]
    pub strict_domains: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchPassagesArgs {
    pub query: String,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub max_urls: Option<usize>,
    #[serde(default)]
    pub max_total_chars: Option<usize>,
    #[serde(default)]
    pub include_domains: Option<Vec<String>>,
    #[serde(default)]
    pub exclude_domains: Option<Vec<String>>,
    #[serde(default)]
    pub strict_domains: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractUrlArgs {
    pub url: String,
    #[serde(default)]
    pub max_chars: Option<usize>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub allow_amp_fallback: Option<bool>,
    #[serde(default)]
    pub bypass_cache: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SearchStatusArgs {}

fn validate_mode(mode: Option<&str>) -> Result<Option<String>, String> {
    match mode.map(|m| m.trim().to_ascii_lowercase()) {
        None => Ok(None),
        Some(ref m) if m.is_empty() => Ok(None),
        Some(m) if VALID_MODES.contains(&m.as_str()) => Ok(Some(m)),
        Some(other) => Err(format!(
            "invalid mode {other:?}; expected one of: {}",
            VALID_MODES.join(", ")
        )),
    }
}

fn validate_domain(domain: Option<&str>) -> Result<Option<String>, String> {
    match domain.map(|d| d.trim().to_ascii_lowercase()) {
        None => Ok(None),
        Some(ref d) if d.is_empty() => Ok(None),
        Some(d) if VALID_DOMAINS.contains(&d.as_str()) => Ok(Some(d)),
        Some(other) => Err(format!(
            "invalid domain {other:?}; expected one of: {}",
            VALID_DOMAINS.join(", ")
        )),
    }
}

fn parse_domain_list(
    raw: Option<Vec<String>>,
    field: &str,
    max_items: usize,
) -> Result<Vec<String>, String> {
    let Some(items) = raw else {
        return Ok(Vec::new());
    };
    if items.len() > max_items {
        return Err(format!(
            "{field} accepts at most {max_items} domains (got {})",
            items.len()
        ));
    }
    let mut out = Vec::with_capacity(items.len());
    for entry in items {
        let norm = crate::url_utils::normalize_domain_filter(&entry).ok_or_else(|| {
            format!(
                "invalid domain {entry:?} in {field} (expected a valid domain such as \"docs.rs\")"
            )
        })?;
        if !out.contains(&norm) {
            out.push(norm);
        }
    }
    Ok(out)
}

fn parse_domain_filter(
    include_domains: Option<Vec<String>>,
    exclude_domains: Option<Vec<String>>,
    strict_domains: Option<bool>,
) -> Result<DomainFilter, String> {
    let include = parse_domain_list(include_domains, "include_domains", MAX_INCLUDE_DOMAINS)?;
    let exclude = parse_domain_list(exclude_domains, "exclude_domains", MAX_EXCLUDE_DOMAINS)?;
    for inc in &include {
        if exclude.contains(inc) {
            return Err(format!(
                "domain {inc:?} cannot appear in both include_domains and exclude_domains"
            ));
        }
    }
    let strict = strict_domains.unwrap_or_else(crate::config::mcp_strict_domains_default);
    Ok(DomainFilter {
        include,
        exclude,
        strict,
    })
}

/// Append a clean, zero-noise domain-only list after SearchWala's main results
/// or passages (no repetitive descriptions, snippets, or per-domain boilerplate).
fn append_discovered_domains_text(out: &mut String, domains: &[DiscoveredDomain], total: usize) {
    if domains.is_empty() {
        return;
    }
    let names: Vec<&str> = domains.iter().map(|d| d.domain.as_str()).collect();
    if total > domains.len() {
        let _ = writeln!(
            out,
            "Additional sources with useful information ({} of {} domains found — pass any via `include_domains` or `extract_url` to drill down):\n{}",
            domains.len(),
            total,
            names.join(", ")
        );
    } else {
        let _ = writeln!(
            out,
            "Additional sources with useful information (pass any via `include_domains` or `extract_url` to drill down):\n{}",
            names.join(", ")
        );
    }
}

// ----------------------------------------------------------------------------
// Progress Bridge (search::Progress -> MCP notifications/progress)
// ----------------------------------------------------------------------------

fn spawn_progress_bridge(
    progress_token: Option<Value>,
    progress_tx: Option<ProgressSender>,
    total_steps: u64,
) -> (
    Option<tokio::sync::mpsc::Sender<Progress>>,
    Option<tokio::task::JoinHandle<()>>,
) {
    let (Some(token), Some(out_tx)) = (progress_token, progress_tx) else {
        return (None, None);
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Progress>(32);
    let handle = tokio::spawn(async move {
        let mut step = 0u64;
        while let Some(p) = rx.recv().await {
            step = (step + 1).min(total_steps);
            let msg = match p {
                Progress::Plan {
                    mode,
                    tier,
                    engines,
                    sub_queries,
                    ..
                } => format!(
                    "Planned {mode} ({tier}) across {engines} engines ({} sub-queries)",
                    sub_queries.len()
                ),
                Progress::Engines {
                    raw_results,
                    families,
                    ms,
                } => format!(
                    "Engines returned {raw_results} raw results across {families} independent families in {ms}ms"
                ),
                Progress::Sources { results } => {
                    format!("Selected {} consensus-ranked sources", results.len())
                }
                Progress::Extracted {
                    sources,
                    chars,
                    ms,
                } => format!("Extracted {sources} pages ({chars} chars) in {ms}ms"),
                Progress::Highlight { .. } => "Computed verbatim answer highlight".to_string(),
                Progress::Synthesizing { synthesis } => format!("Synthesizing ({synthesis})"),
            };
            let notif = JsonRpcNotification::progress(token.clone(), step, Some(total_steps), msg);
            if out_tx.send(notif).await.is_err() {
                break;
            }
        }
    });
    (Some(tx), Some(handle))
}

// ----------------------------------------------------------------------------
// Tool 1: search_web
// ----------------------------------------------------------------------------

async fn exec_search_web(
    args: SearchWebArgs,
    progress_token: Option<Value>,
    progress_tx: Option<ProgressSender>,
) -> CallToolResult {
    let query = args.query.trim().to_string();
    if query.is_empty() {
        return CallToolResult::error("query cannot be empty");
    }
    let mode = match validate_mode(args.mode.as_deref()) {
        Ok(m) => m,
        Err(e) => return CallToolResult::error(e),
    };
    let domain = match validate_domain(args.domain.as_deref()) {
        Ok(d) => d,
        Err(e) => return CallToolResult::error(e),
    };
    let domain_filter = match parse_domain_filter(
        args.include_domains,
        args.exclude_domains,
        args.strict_domains,
    ) {
        Ok(f) => f,
        Err(e) => return CallToolResult::error(e),
    };
    let max_results = args
        .max_results
        .unwrap_or(DEFAULT_WEB_RESULTS)
        .clamp(MIN_WEB_RESULTS, MAX_WEB_RESULTS);

    let (ptx, bridge) = spawn_progress_bridge(progress_token, progress_tx, 3);
    let resp = search::search_serp_only(
        &query,
        mode.as_deref(),
        domain.as_deref(),
        Some(max_results),
        &domain_filter,
        ptx,
    )
    .await;
    if let Some(h) = bridge {
        let _ = h.await;
    }

    let text = format_search_web_text(&resp);
    let structured = serde_json::to_value(&resp).unwrap_or_else(|_| json!({}));
    CallToolResult::success(text, structured)
}

fn format_search_web_text(resp: &SerpSearchResponse) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "## Web Search: {}\nMode: `{}` ({}) | Domain: `{}` | {} results in {:.2}s ({} raw hits / {} engines)\n",
        resp.query,
        resp.mode,
        resp.tier,
        resp.domain,
        resp.results.len(),
        resp.elapsed_seconds,
        resp.engine_stats.total_raw_results,
        resp.engine_stats.engines_queried.len(),
    );
    if resp.results.is_empty() {
        out.push_str("No results returned. Engines may be temporarily unreachable or rate-limited (or `strict_domains: true` matched no candidates); inspect `discovered_domains` or call `search_status`.\n\n");
    } else {
        for hit in &resp.results {
            let title = if hit.title.trim().is_empty() {
                hit.url.as_str()
            } else {
                hit.title.trim()
            };
            let mut meta = format!("{} fam", hit.found_by_families);
            if let Some(pub_dt) = hit.published {
                let _ = write!(meta, " · {}", pub_dt.format("%Y-%m-%d"));
            }
            let _ = writeln!(out, "{}. **{}** — {} ({})", hit.rank, title, hit.url, meta);
            let snip = hit.snippet.trim();
            if !snip.is_empty() {
                let is_long = crate::util::char_len(snip) > SEARCH_WEB_TEXT_SNIPPET_CHARS;
                let preview = crate::util::truncate_chars(snip, SEARCH_WEB_TEXT_SNIPPET_CHARS);
                if is_long {
                    let _ = writeln!(out, "   {preview}…");
                } else {
                    let _ = writeln!(out, "   {preview}");
                }
            }
            if let Some(abs) = hit
                .abstract_text
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let is_long = crate::util::char_len(abs) > SEARCH_WEB_TEXT_ABSTRACT_CHARS;
                let preview = crate::util::truncate_chars(abs, SEARCH_WEB_TEXT_ABSTRACT_CHARS);
                if is_long {
                    let _ = writeln!(out, "   Abstract: {preview}…");
                } else {
                    let _ = writeln!(out, "   Abstract: {preview}");
                }
            }
        }
        out.push('\n');
    }
    append_discovered_domains_text(
        &mut out,
        &resp.discovered_domains,
        resp.total_discovered_domains,
    );
    out
}

// ----------------------------------------------------------------------------
// Tool 2: search_passages
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct HighlightOut {
    text: String,
    url: String,
    title: String,
    terms: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PassageOut {
    citation_id: usize,
    url: String,
    title: String,
    text: String,
    char_count: usize,
    engine: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    published: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    byline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    site_name: Option<String>,
    confidence: f64,
    extractor: String,
}

#[derive(Debug, Clone, Serialize)]
struct TopSourceOut {
    url: String,
    title: String,
    found_by_families: usize,
    rrf_score: f64,
}

#[derive(Debug, Clone, Serialize)]
struct SearchPassagesOutput {
    query: String,
    mode: String,
    tier: String,
    domain: String,
    sub_queries: Vec<String>,
    sources_found: usize,
    sources_extracted: usize,
    chars_extracted: usize,
    elapsed_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    highlight: Option<HighlightOut>,
    passages: Vec<PassageOut>,
    top_sources: Vec<TopSourceOut>,
    discovered_domains: Vec<DiscoveredDomain>,
    total_discovered_domains: usize,
}

/// Pack ranked [`SourceResult`] chunks into bounded [`PassageOut`] items without
/// exceeding `top_k` or `max_total_chars` (UTF-8 safe).
fn pack_passages(
    results: &[SourceResult],
    top_k: usize,
    max_total_chars: usize,
) -> Vec<PassageOut> {
    let mut out = Vec::new();
    let mut used_chars = 0usize;

    for src in results.iter().take(top_k) {
        if used_chars >= max_total_chars {
            break;
        }
        let remaining = max_total_chars - used_chars;
        let text_len = crate::util::char_len(&src.extracted_text);
        let text = if text_len <= remaining {
            src.extracted_text.clone()
        } else if out.is_empty() || remaining >= 120 {
            crate::util::truncate_chars(&src.extracted_text, remaining).to_string()
        } else {
            break;
        };
        let char_count = crate::util::char_len(&text);
        used_chars += char_count;

        out.push(PassageOut {
            citation_id: out.len() + 1,
            url: src.url.clone(),
            title: src.title.clone(),
            text,
            char_count,
            engine: src.engine.clone(),
            published: src.published,
            language: src.language.clone(),
            byline: src.byline.clone(),
            site_name: src.site_name.clone(),
            confidence: src.confidence,
            extractor: src.extractor.to_string(),
        });
    }

    out
}

fn build_search_passages_output(
    resp: crate::models::SearchResponse,
    top_k: usize,
    max_total_chars: usize,
) -> SearchPassagesOutput {
    let highlight = resp.highlight.map(|h| HighlightOut {
        text: h.text,
        url: h.url,
        title: h.title,
        terms: h.terms,
    });
    let passages = pack_passages(&resp.results, top_k, max_total_chars);
    let top_sources = resp
        .search_results
        .into_iter()
        .take(8)
        .map(|s: SearchHit| TopSourceOut {
            url: s.url,
            title: s.title,
            found_by_families: s.found_by_families,
            rrf_score: s.rrf_score,
        })
        .collect();

    SearchPassagesOutput {
        query: resp.query,
        mode: resp.mode,
        tier: resp.tier,
        domain: resp.domain,
        sub_queries: resp.sub_queries,
        sources_found: resp.sources_found,
        sources_extracted: resp.sources_extracted,
        chars_extracted: resp.chars_extracted,
        elapsed_seconds: resp.elapsed_seconds,
        highlight,
        passages,
        top_sources,
        discovered_domains: resp.discovered_domains,
        total_discovered_domains: resp.total_discovered_domains,
    }
}

fn format_search_passages_text(out_data: &SearchPassagesOutput) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "## Passage Retrieval: {}\nMode: `{}` ({}) | Domain: `{}` | {} passages from {} extracted pages ({} raw hits) in {:.2}s\n",
        out_data.query,
        out_data.mode,
        out_data.tier,
        out_data.domain,
        out_data.passages.len(),
        out_data.sources_extracted,
        out_data.sources_found,
        out_data.elapsed_seconds,
    );

    if let Some(ref h) = out_data.highlight {
        let _ = writeln!(
            out,
            "> **Quick Answer (verbatim):** {}\n> Source: [{}]({})\n",
            h.text, h.title, h.url
        );
    }

    if out_data.passages.is_empty() {
        out.push_str("No readable passages were extracted. Inspect `discovered_domains` / `top_sources` or call `search_status`.\n\n");
    } else {
        for p in &out_data.passages {
            let mut meta = Vec::new();
            if let Some(dt) = p.published {
                meta.push(format!("published {}", dt.format("%Y-%m-%d")));
            }
            if let Some(ref site) = p.site_name {
                meta.push(site.clone());
            }
            if !p.extractor.is_empty() {
                meta.push(p.extractor.clone());
            }
            meta.push(format!("conf {:.2}", p.confidence));
            let _ = writeln!(
                out,
                "[{}] **{}** ({})\n{}\n{}\n",
                p.citation_id,
                if p.title.trim().is_empty() {
                    p.url.as_str()
                } else {
                    p.title.trim()
                },
                meta.join(" · "),
                p.url,
                p.text
            );
        }
    }
    append_discovered_domains_text(
        &mut out,
        &out_data.discovered_domains,
        out_data.total_discovered_domains,
    );
    out
}

async fn exec_search_passages(
    args: SearchPassagesArgs,
    temp_db: &TempDb,
    progress_token: Option<Value>,
    progress_tx: Option<ProgressSender>,
) -> CallToolResult {
    let query = args.query.trim().to_string();
    if query.is_empty() {
        return CallToolResult::error("query cannot be empty");
    }
    let mode = match validate_mode(args.mode.as_deref()) {
        Ok(m) => m,
        Err(e) => return CallToolResult::error(e),
    };
    let domain = match validate_domain(args.domain.as_deref()) {
        Ok(d) => d,
        Err(e) => return CallToolResult::error(e),
    };
    let domain_filter = match parse_domain_filter(
        args.include_domains,
        args.exclude_domains,
        args.strict_domains,
    ) {
        Ok(f) => f,
        Err(e) => return CallToolResult::error(e),
    };

    let top_k = args
        .top_k
        .unwrap_or(DEFAULT_PASSAGES_TOP_K)
        .clamp(MIN_PASSAGES_TOP_K, MAX_PASSAGES_TOP_K);
    let max_urls = args.max_urls.map(|u| u.clamp(MIN_MAX_URLS, MAX_MAX_URLS));
    let max_total_chars = args
        .max_total_chars
        .unwrap_or(DEFAULT_PASSAGES_MAX_CHARS)
        .clamp(MIN_PASSAGES_MAX_CHARS, MAX_PASSAGES_MAX_CHARS);

    let params = SearchParams {
        query,
        max_results: max_urls,
        focus_mode: mode,
        domain,
        llm: None,
        enable_copilot: false,
        memory: Vec::new(),
        domain_filter,
    };

    let (ptx, bridge) = spawn_progress_bridge(progress_token, progress_tx, 5);
    let resp = search::execute_search_full(params, Some(temp_db), ptx).await;
    if let Some(h) = bridge {
        let _ = h.await;
    }

    let output = build_search_passages_output(resp, top_k, max_total_chars);
    let text = format_search_passages_text(&output);
    let structured = serde_json::to_value(&output).unwrap_or_else(|_| json!({}));
    CallToolResult::success(text, structured)
}

// ----------------------------------------------------------------------------
// Tool 3: extract_url
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct ExtractedPassageOut {
    rank: usize,
    text: String,
    char_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    published: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize)]
struct ExtractUrlOutput {
    url: String,
    canonical_url: String,
    title: String,
    /// Extracted page text when `query` is omitted; omitted when `passages` is populated
    /// to avoid duplicating focused passages inside `structuredContent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// BM25+/MMR-ranked passages when `query` is provided; omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    passages: Vec<ExtractedPassageOut>,
    char_count: usize,
    total_chars: usize,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    published: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    byline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    site_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_type: Option<String>,
    extractor: String,
    confidence: f64,
    cached: bool,
    elapsed_ms: u64,
}

fn build_extract_url_output(
    page: ExtractedPage,
    max_chars: usize,
    focus_query: Option<&str>,
    top_k: usize,
) -> ExtractUrlOutput {
    let total_chars = page.char_count;
    let trimmed_q = focus_query.map(str::trim).filter(|q| !q.is_empty());

    let (text, passages, char_count, truncated) = if let Some(q) = trimmed_q {
        let single_source = SourceResult {
            url: page.final_url.clone(),
            title: page.extracted.title.clone(),
            extracted_text: page.extracted.text.clone(),
            char_count: total_chars,
            engine: "direct".to_string(),
            published: page.extracted.published,
            language: page.extracted.language.clone(),
            byline: page.extracted.byline.clone(),
            site_name: page.extracted.site_name.clone(),
            confidence: page.extracted.confidence,
            extractor: page.extracted.source,
        };
        let intel = crate::query_intel::analyze_query(q);
        let rank_opts = crate::ranking::RankOptions {
            max_chunks_per_url: top_k.max(1),
            freshness: false,
            ..Default::default()
        };
        let ranked = crate::ranking::rank_with_options(
            q,
            &[single_source],
            top_k,
            None,
            Some(&intel),
            &rank_opts,
        );
        let mut used = 0usize;
        let mut p_out = Vec::new();
        for chunk in ranked {
            if used >= max_chars {
                break;
            }
            let remaining = max_chars - used;
            let c_len = crate::util::char_len(&chunk.extracted_text);
            let body = if c_len <= remaining {
                chunk.extracted_text
            } else if p_out.is_empty() || remaining >= 120 {
                crate::util::truncate_chars(&chunk.extracted_text, remaining).to_string()
            } else {
                break;
            };
            let len = crate::util::char_len(&body);
            used += len;
            p_out.push(ExtractedPassageOut {
                rank: p_out.len() + 1,
                text: body,
                char_count: len,
                published: chunk.published,
            });
        }
        let clipped = used < total_chars;
        (None, p_out, used, clipped)
    } else if total_chars > max_chars {
        let clipped_text = crate::util::truncate_chars(&page.extracted.text, max_chars).to_string();
        let count = crate::util::char_len(&clipped_text);
        (Some(clipped_text), Vec::new(), count, true)
    } else {
        (
            Some(page.extracted.text.clone()),
            Vec::new(),
            total_chars,
            false,
        )
    };

    ExtractUrlOutput {
        url: page.url,
        canonical_url: page.final_url,
        title: page.extracted.title,
        text,
        passages,
        char_count,
        total_chars,
        truncated,
        published: page.extracted.published,
        language: page.extracted.language,
        byline: page.extracted.byline,
        site_name: page.extracted.site_name,
        page_type: page.extracted.page_type,
        extractor: page.extracted.source.to_string(),
        confidence: page.extracted.confidence,
        cached: page.cached,
        elapsed_ms: page.elapsed_ms,
    }
}

fn format_extract_url_text(out_data: &ExtractUrlOutput) -> String {
    let mut out = String::new();
    let heading = if out_data.title.trim().is_empty() {
        out_data.canonical_url.as_str()
    } else {
        out_data.title.trim()
    };
    let _ = writeln!(out, "# {heading}\nURL: {}", out_data.canonical_url);
    let mut meta = Vec::new();
    if let Some(ref byline) = out_data.byline {
        meta.push(format!("Author: {byline}"));
    }
    if let Some(ref site) = out_data.site_name {
        meta.push(format!("Site: {site}"));
    }
    if let Some(dt) = out_data.published {
        meta.push(format!("Published: {}", dt.format("%Y-%m-%d")));
    }
    if let Some(ref lang) = out_data.language {
        meta.push(format!("Lang: {lang}"));
    }
    meta.push(format!("Extractor: {}", out_data.extractor));
    meta.push(format!(
        "Chars: {}/{}",
        out_data.char_count, out_data.total_chars
    ));
    if out_data.cached {
        meta.push("cached".to_string());
    }
    let _ = writeln!(out, "{}\n", meta.join(" | "));
    if !out_data.passages.is_empty() {
        for (idx, p) in out_data.passages.iter().enumerate() {
            if idx > 0 {
                out.push_str("\n\n");
            }
            let _ = write!(out, "[{}] {}", p.rank, p.text);
        }
    } else if let Some(ref text) = out_data.text {
        out.push_str(text);
        if out_data.truncated {
            let _ = write!(
                out,
                "\n\n[Truncated at {} of {} chars — pass a larger `max_chars` or provide `query` for focused BM25+ passages.]",
                out_data.char_count, out_data.total_chars
            );
        }
    }
    out
}

async fn exec_extract_url(args: ExtractUrlArgs) -> CallToolResult {
    let url = args.url.trim().to_string();
    if url.is_empty() {
        return CallToolResult::error("url cannot be empty");
    }
    let max_chars = args
        .max_chars
        .unwrap_or(DEFAULT_EXTRACT_MAX_CHARS)
        .clamp(MIN_EXTRACT_MAX_CHARS, MAX_EXTRACT_MAX_CHARS);
    let top_k = args
        .top_k
        .unwrap_or(DEFAULT_EXTRACT_TOP_K)
        .clamp(MIN_EXTRACT_TOP_K, MAX_EXTRACT_TOP_K);

    let opts = FetchExtractOptions {
        allow_amp_fallback: args.allow_amp_fallback.unwrap_or(true),
        use_cache: !args.bypass_cache.unwrap_or(false),
        ..Default::default()
    };

    match search::fetch_and_extract_url(&url, &opts).await {
        Ok(page) => {
            let output = build_extract_url_output(page, max_chars, args.query.as_deref(), top_k);
            let text = format_extract_url_text(&output);
            let structured = serde_json::to_value(&output).unwrap_or_else(|_| json!({}));
            CallToolResult::success(text, structured)
        }
        Err(err) => CallToolResult::error(err.to_string()),
    }
}

// ----------------------------------------------------------------------------
// Tool 4: search_status
// ----------------------------------------------------------------------------

pub fn build_status_snapshot() -> Value {
    crate::engines::health::register_known(&crate::modes::all_roster_engines());
    let engines = crate::engines::health::snapshot();
    let mut summary: BTreeMap<&'static str, usize> = BTreeMap::new();
    for e in &engines {
        *summary.entry(e.verdict).or_insert(0) += 1;
    }
    let cache = crate::cache_layer::stats();
    json!({
        "version": crate::config::VERSION,
        "modes": VALID_MODES,
        "domains": VALID_DOMAINS,
        "engine_summary": summary,
        "engines": engines,
        "cache": cache,
    })
}

fn exec_search_status() -> CallToolResult {
    let data = build_status_snapshot();
    let engine_count = data["engines"].as_array().map_or(0, Vec::len);
    let cache = &data["cache"];
    let mut text = String::new();
    let _ = writeln!(
        text,
        "## SearchWala v{} Status\nModes: {} (budgets: `searchwala://config/modes`)\nDomains: {}\nEngines tracked: {}\nVerdicts: {}\nCache: enabled={} | SERP {} entries (hit ratio {:.1}%) | Page {} entries (hit ratio {:.1}%)",
        crate::config::VERSION,
        VALID_MODES.join(", "),
        VALID_DOMAINS.join(", "),
        engine_count,
        data["engine_summary"],
        cache["enabled"],
        cache["serp_entries"],
        cache["serp_hit_ratio"].as_f64().unwrap_or(0.0) * 100.0,
        cache["page_entries"],
        cache["page_hit_ratio"].as_f64().unwrap_or(0.0) * 100.0,
    );
    CallToolResult::success(text, data)
}

// ----------------------------------------------------------------------------
// JSON Schemas (Draft 2020-12)
// ----------------------------------------------------------------------------

fn discovered_domains_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "object",
            "required": ["domain", "result_count", "max_families", "top_rrf_score", "top_url", "top_title"],
            "properties": {
                "domain": { "type": "string" },
                "result_count": { "type": "integer" },
                "max_families": { "type": "integer" },
                "top_rrf_score": { "type": "number" },
                "top_url": { "type": "string" },
                "top_title": { "type": "string" }
            }
        }
    })
}

fn search_web_input_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["query"],
        "additionalProperties": false,
        "properties": {
            "query": {
                "type": "string",
                "minLength": 1,
                "description": "The search query in any language."
            },
            "mode": {
                "type": "string",
                "enum": VALID_MODES,
                "default": "auto",
                "description": "Search profile controlling engine roster, pagination, and sub-queries."
            },
            "domain": {
                "type": "string",
                "enum": VALID_DOMAINS,
                "default": "general",
                "description": "Optional subject-area focus composable with any mode."
            },
            "max_results": {
                "type": "integer",
                "minimum": MIN_WEB_RESULTS,
                "maximum": MAX_WEB_RESULTS,
                "default": DEFAULT_WEB_RESULTS,
                "description": "Maximum number of consensus-ranked search hits to return."
            },
            "include_domains": {
                "type": "array",
                "maxItems": MAX_INCLUDE_DOMAINS,
                "items": { "type": "string", "minLength": 3 },
                "description": "Optional domains to target and prioritize (e.g. [\"docs.rs\", \"github.com\"]). Prioritized at the top by default, or strictly restricted when `strict_domains` is true."
            },
            "exclude_domains": {
                "type": "array",
                "maxItems": MAX_EXCLUDE_DOMAINS,
                "items": { "type": "string", "minLength": 3 },
                "description": "Optional domains to drop before triage and ranking (e.g. [\"pinterest.com\"])."
            },
            "strict_domains": {
                "type": "boolean",
                "default": false,
                "description": "When true with `include_domains`, strictly restrict results to those domains. Defaults to false (keeps SearchWala's organic consensus answers while prioritizing `include_domains`)."
            }
        }
    })
}

fn search_web_output_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": [
            "query", "mode", "tier", "domain", "intent",
            "is_time_sensitive", "sub_queries", "elapsed_seconds",
            "engine_stats", "results", "discovered_domains", "total_discovered_domains"
        ],
        "properties": {
            "query": { "type": "string" },
            "mode": { "type": "string" },
            "tier": { "type": "string" },
            "domain": { "type": "string" },
            "intent": { "type": "string" },
            "is_time_sensitive": { "type": "boolean" },
            "sub_queries": { "type": "array", "items": { "type": "string" } },
            "elapsed_seconds": { "type": "number" },
            "engine_stats": {
                "type": "object",
                "required": ["engines_queried", "total_raw_results", "deduplicated_urls"],
                "properties": {
                    "engines_queried": { "type": "array", "items": { "type": "string" } },
                    "total_raw_results": { "type": "integer" },
                    "deduplicated_urls": { "type": "integer" }
                }
            },
            "results": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["rank", "url", "title", "snippet", "engines", "found_by_families", "rrf_score"],
                    "properties": {
                        "rank": { "type": "integer" },
                        "url": { "type": "string" },
                        "title": { "type": "string" },
                        "snippet": { "type": "string" },
                        "engines": { "type": "array", "items": { "type": "string" } },
                        "found_by_families": { "type": "integer" },
                        "rrf_score": { "type": "number" },
                        "published": { "type": "string" },
                        "abstract_text": { "type": "string" }
                    }
                }
            },
            "discovered_domains": discovered_domains_schema(),
            "total_discovered_domains": { "type": "integer" }
        }
    })
}

fn search_passages_input_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["query"],
        "additionalProperties": false,
        "properties": {
            "query": {
                "type": "string",
                "minLength": 1,
                "description": "Question or research query to gather verified, ranked passages for."
            },
            "mode": {
                "type": "string",
                "enum": VALID_MODES,
                "default": "auto",
                "description": "fast (~2-4s), auto (~3-6s), deep (~15-25s), study, academic (OpenAlex+arXiv), reddit, or youtube."
            },
            "domain": {
                "type": "string",
                "enum": VALID_DOMAINS,
                "default": "general",
                "description": "Optional subject-area authority and engine overlay."
            },
            "top_k": {
                "type": "integer",
                "minimum": MIN_PASSAGES_TOP_K,
                "maximum": MAX_PASSAGES_TOP_K,
                "default": DEFAULT_PASSAGES_TOP_K,
                "description": "Maximum number of BM25+/MMR-ranked passages to return (bounded by the mode's passage pool)."
            },
            "max_urls": {
                "type": "integer",
                "minimum": MIN_MAX_URLS,
                "maximum": MAX_MAX_URLS,
                "description": "Optional override for number of pages to scrape (clamped to the mode's safe bounds)."
            },
            "max_total_chars": {
                "type": "integer",
                "minimum": MIN_PASSAGES_MAX_CHARS,
                "maximum": MAX_PASSAGES_MAX_CHARS,
                "default": DEFAULT_PASSAGES_MAX_CHARS,
                "description": "Total character budget across returned passages (UTF-8 safe)."
            },
            "include_domains": {
                "type": "array",
                "maxItems": MAX_INCLUDE_DOMAINS,
                "items": { "type": "string", "minLength": 3 },
                "description": "Optional domains to target and prioritize (e.g. [\"docs.rs\", \"github.com\"]). Prioritized at the top by default, or strictly restricted when `strict_domains` is true."
            },
            "exclude_domains": {
                "type": "array",
                "maxItems": MAX_EXCLUDE_DOMAINS,
                "items": { "type": "string", "minLength": 3 },
                "description": "Optional domains to drop before triage and scraping."
            },
            "strict_domains": {
                "type": "boolean",
                "default": false,
                "description": "When true with `include_domains`, strictly restrict scraping and passages to those domains. Defaults to false (keeps SearchWala's organic consensus answers while prioritizing `include_domains`)."
            }
        }
    })
}

fn search_passages_output_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": [
            "query", "mode", "tier", "domain", "sub_queries",
            "sources_found", "sources_extracted", "chars_extracted",
            "elapsed_seconds", "passages", "top_sources",
            "discovered_domains", "total_discovered_domains"
        ],
        "properties": {
            "query": { "type": "string" },
            "mode": { "type": "string" },
            "tier": { "type": "string" },
            "domain": { "type": "string" },
            "sub_queries": { "type": "array", "items": { "type": "string" } },
            "sources_found": { "type": "integer" },
            "sources_extracted": { "type": "integer" },
            "chars_extracted": { "type": "integer" },
            "elapsed_seconds": { "type": "number" },
            "highlight": {
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "url": { "type": "string" },
                    "title": { "type": "string" },
                    "terms": { "type": "array", "items": { "type": "string" } }
                }
            },
            "passages": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": [
                        "citation_id", "url", "title", "text", "char_count",
                        "engine", "confidence", "extractor"
                    ],
                    "properties": {
                        "citation_id": { "type": "integer" },
                        "url": { "type": "string" },
                        "title": { "type": "string" },
                        "text": { "type": "string" },
                        "char_count": { "type": "integer" },
                        "engine": { "type": "string" },
                        "published": { "type": "string" },
                        "language": { "type": "string" },
                        "byline": { "type": "string" },
                        "site_name": { "type": "string" },
                        "confidence": { "type": "number" },
                        "extractor": { "type": "string" }
                    }
                }
            },
            "top_sources": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["url", "title", "found_by_families", "rrf_score"],
                    "properties": {
                        "url": { "type": "string" },
                        "title": { "type": "string" },
                        "found_by_families": { "type": "integer" },
                        "rrf_score": { "type": "number" }
                    }
                }
            },
            "discovered_domains": discovered_domains_schema(),
            "total_discovered_domains": { "type": "integer" }
        }
    })
}

fn extract_url_input_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["url"],
        "additionalProperties": false,
        "properties": {
            "url": {
                "type": "string",
                "format": "uri",
                "minLength": 1,
                "description": "Public http:// or https:// URL to fetch and extract."
            },
            "max_chars": {
                "type": "integer",
                "minimum": MIN_EXTRACT_MAX_CHARS,
                "maximum": MAX_EXTRACT_MAX_CHARS,
                "default": DEFAULT_EXTRACT_MAX_CHARS,
                "description": "Maximum characters of text to return (UTF-8 safe truncation)."
            },
            "query": {
                "type": "string",
                "description": "Optional focus query. When provided, runs BM25+/MMR passage ranking on this page and returns the most relevant passages."
            },
            "top_k": {
                "type": "integer",
                "minimum": MIN_EXTRACT_TOP_K,
                "maximum": MAX_EXTRACT_TOP_K,
                "default": DEFAULT_EXTRACT_TOP_K,
                "description": "Maximum passages to return when `query` is provided."
            },
            "allow_amp_fallback": {
                "type": "boolean",
                "default": true,
                "description": "Allow fetching the same-site AMP variant if canonical extraction is thin."
            },
            "bypass_cache": {
                "type": "boolean",
                "default": false,
                "description": "Force a fresh fetch instead of reading from the 6-hour in-memory page cache."
            }
        }
    })
}

fn extract_url_output_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": [
            "url", "canonical_url", "title",
            "char_count", "total_chars", "truncated",
            "extractor", "confidence", "cached", "elapsed_ms"
        ],
        "properties": {
            "url": { "type": "string" },
            "canonical_url": { "type": "string" },
            "title": { "type": "string" },
            "text": { "type": "string" },
            "passages": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["rank", "text", "char_count"],
                    "properties": {
                        "rank": { "type": "integer" },
                        "text": { "type": "string" },
                        "char_count": { "type": "integer" },
                        "published": { "type": "string" }
                    }
                }
            },
            "char_count": { "type": "integer" },
            "total_chars": { "type": "integer" },
            "truncated": { "type": "boolean" },
            "published": { "type": "string" },
            "language": { "type": "string" },
            "byline": { "type": "string" },
            "site_name": { "type": "string" },
            "page_type": { "type": "string" },
            "extractor": { "type": "string" },
            "confidence": { "type": "number" },
            "cached": { "type": "boolean" },
            "elapsed_ms": { "type": "integer" }
        }
    })
}

fn search_status_input_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "properties": {}
    })
}

fn search_status_output_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": [
            "version", "modes", "domains",
            "engine_summary", "engines", "cache"
        ],
        "properties": {
            "version": { "type": "string" },
            "modes": { "type": "array", "items": { "type": "string" } },
            "domains": { "type": "array", "items": { "type": "string" } },
            "engine_summary": { "type": "object" },
            "engines": { "type": "array", "items": { "type": "object" } },
            "cache": { "type": "object" }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_four_tools_have_valid_schemas_and_readonly_annotations() {
        let tools = list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "search_web",
                "search_passages",
                "extract_url",
                "search_status"
            ]
        );
        for t in &tools {
            assert!(!t.description.is_empty());
            assert_eq!(t.input_schema["type"], "object");
            assert_eq!(t.output_schema["type"], "object");
            assert!(t.annotations.read_only_hint);
            assert!(!t.annotations.destructive_hint);
        }
        assert!(!tools[3].annotations.open_world_hint);
        assert!(tools[3].annotations.idempotent_hint);
    }

    #[tokio::test]
    async fn validates_empty_queries_invalid_modes_and_unknown_tools_offline() {
        let db = TempDb::new();

        // Unknown tool -> Err (protocol -32602)
        let unknown = call_tool("nonexistent", None, &db, None, None).await;
        assert!(unknown.is_err());

        // Unknown parameter rejected by deny_unknown_fields -> Err (-32602)
        let extra = call_tool(
            "search_web",
            Some(json!({"query": "rust", "bogus": true})),
            &db,
            None,
            None,
        )
        .await;
        assert!(extra.is_err());

        // Empty query -> CallToolResult::error (isError: true)
        let empty_web = call_tool("search_web", Some(json!({"query": "   "})), &db, None, None)
            .await
            .unwrap();
        assert!(empty_web.is_error);

        let empty_pass = call_tool(
            "search_passages",
            Some(json!({"query": ""})),
            &db,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(empty_pass.is_error);

        // Invalid mode / domain -> CallToolResult::error
        let bad_mode = call_tool(
            "search_web",
            Some(json!({"query": "rust", "mode": "turbo"})),
            &db,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(bad_mode.is_error);
        assert!(bad_mode.content[0].text.contains("invalid mode"));

        let bad_domain = call_tool(
            "search_passages",
            Some(json!({"query": "rust", "domain": "astrology"})),
            &db,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(bad_domain.is_error);
        assert!(bad_domain.content[0].text.contains("invalid domain"));
    }

    #[tokio::test]
    async fn extract_url_enforces_ssrf_and_reads_cached_pages_with_bm25_focus() {
        let db = TempDb::new();

        // SSRF targets must fail with isError: true before any network I/O
        for bad in [
            "http://127.0.0.1:8000/api/chats",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:8000/",
            "file:///etc/passwd",
        ] {
            let res = call_tool("extract_url", Some(json!({"url": bad})), &db, None, None)
                .await
                .unwrap();
            assert!(res.is_error, "expected isError for {bad}");
            assert!(
                res.content[0].text.contains("SSRF"),
                "expected SSRF reason for {bad}, got: {}",
                res.content[0].text
            );
        }

        // Seed cache with a multi-paragraph Hindi + English document (zero network)
        let url = "https://mcp-tools-offline-test.example/guide";
        let body = format!(
            "First paragraph about gardening and tomato watering schedules in summer climates.\n\n\
             Second paragraph about Rust async cancellation safety in tokio::select loops and drop semantics.\n\n\
             Third paragraph explaining Hindi Devanagari truncation safety: {} — ensuring no UTF-8 panic.",
            "भारत ".repeat(60)
        );
        crate::cache_layer::put_page(
            url,
            crate::extractor::Extracted {
                title: "Mixed Guide".into(),
                text: body,
                language: Some("en".into()),
                byline: Some("Test Author".into()),
                site_name: Some("Offline Test".into()),
                confidence: 0.91,
                source: "readability",
                ..Default::default()
            },
        )
        .await;

        // 1. Full extraction with UTF-8 safe truncation at 500 chars
        let trunc = call_tool(
            "extract_url",
            Some(json!({"url": url, "max_chars": 500})),
            &db,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(!trunc.is_error);
        let sc = trunc.structured_content.as_ref().unwrap();
        assert_eq!(sc["cached"], true);
        assert_eq!(sc["truncated"], true);
        assert!(sc["text"].is_string());
        assert!(sc.get("passages").is_none());
        assert!(sc.get("published").is_none());
        assert!(sc.get("page_type").is_none());
        assert!(sc["char_count"].as_u64().unwrap() <= 500);
        assert!(sc["total_chars"].as_u64().unwrap() > 500);
        assert_eq!(sc["byline"], "Test Author");

        // 2. Focused BM25+ extraction with `query` (must NOT duplicate passages in `text`)
        let focused = call_tool(
            "extract_url",
            Some(json!({
                "url": url,
                "query": "Rust async cancellation safety tokio select",
                "top_k": 5
            })),
            &db,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(!focused.is_error);
        let fsc = focused.structured_content.as_ref().unwrap();
        assert!(
            fsc.get("text").is_none(),
            "focused extract_url must omit `text` to avoid duplicating `passages`"
        );
        let passages = fsc["passages"].as_array().unwrap();
        assert!(!passages.is_empty());
        assert!(
            passages[0]["text"]
                .as_str()
                .unwrap()
                .contains("cancellation safety"),
            "BM25+ must rank the Rust cancellation paragraph first: {passages:?}"
        );
        assert!(focused.content[0].text.contains("[1] "));
        assert!(focused.content[0].text.contains("cancellation safety"));
    }

    #[test]
    fn pack_passages_respects_top_k_and_utf8_char_budget() {
        let hindi = "भारत".repeat(250); // 1000 chars, 3000 bytes
        let sources = vec![
            SourceResult {
                url: "https://a.example/1".into(),
                title: "One".into(),
                extracted_text: hindi.clone(),
                char_count: 1000,
                engine: "wikipedia".into(),
                confidence: 0.9,
                extractor: "readability",
                ..Default::default()
            },
            SourceResult {
                url: "https://b.example/2".into(),
                title: "Two".into(),
                extracted_text: hindi,
                char_count: 1000,
                engine: "brave".into(),
                confidence: 0.85,
                extractor: "trafilatura",
                ..Default::default()
            },
        ];

        let packed = pack_passages(&sources, 10, 1200);
        assert_eq!(packed.len(), 2);
        assert_eq!(packed[0].citation_id, 1);
        assert_eq!(packed[0].char_count, 1000);
        assert_eq!(packed[1].citation_id, 2);
        assert_eq!(packed[1].char_count, 200);
        assert_eq!(packed[0].char_count + packed[1].char_count, 1200);
    }

    #[tokio::test]
    async fn search_status_works_with_omitted_or_empty_arguments() {
        let db = TempDb::new();
        let res = call_tool("search_status", None, &db, None, None)
            .await
            .unwrap();
        assert!(!res.is_error);
        let sc = res.structured_content.as_ref().unwrap();
        assert_eq!(sc["version"], crate::config::VERSION);
        assert!(sc["engines"].as_array().unwrap().len() >= 10);
        assert!(
            sc.get("mode_profiles").is_none(),
            "static mode_profiles should be omitted from search_status (served by searchwala://config/modes)"
        );
        assert!(sc["cache"].is_object());
    }

    #[test]
    fn token_efficiency_compact_web_format_and_skip_none_fields() {
        let long_snippet = "x".repeat(300);
        let resp = SerpSearchResponse {
            query: "rust tokio".into(),
            mode: "fast".into(),
            tier: "fast".into(),
            domain: "tech".into(),
            intent: "informational".into(),
            is_time_sensitive: false,
            sub_queries: vec!["rust tokio".into()],
            elapsed_seconds: 1.2,
            engine_stats: crate::models::EngineStats {
                engines_queried: vec!["brave".into()],
                total_raw_results: 10,
                deduplicated_urls: 5,
            },
            results: vec![crate::search::SerpHit {
                rank: 1,
                url: "https://docs.rs/tokio".into(),
                title: "Tokio Docs".into(),
                snippet: long_snippet,
                engines: vec!["brave".into(), "bing".into()],
                found_by_families: 2,
                rrf_score: 0.032,
                published: None,
                abstract_text: None,
            }],
            discovered_domains: vec![],
            total_discovered_domains: 0,
        };
        let text = format_search_web_text(&resp);
        assert!(text.contains("1. **Tokio Docs** — https://docs.rs/tokio (2 fam)"));
        assert!(!text.contains("RRF:"));
        assert!(!text.contains("Engines:"));
        let snip_line = text
            .lines()
            .find(|l| l.starts_with("   x"))
            .expect("compact snippet line");
        assert!(
            crate::util::char_len(snip_line.trim()) <= SEARCH_WEB_TEXT_SNIPPET_CHARS + 1,
            "snippet line must be capped at ~200 chars: {}",
            snip_line.len()
        );

        // Passages output must omit `None` optional metadata and `top_sources[].snippet`.
        let p_out = build_search_passages_output(
            crate::models::SearchResponse {
                query: "rust".into(),
                search_query: None,
                memory_used: 0,
                conversation_id: None,
                turn_id: None,
                mode: "fast".into(),
                tier: "fast".into(),
                domain: "general".into(),
                sub_queries: vec!["rust".into()],
                results: vec![SourceResult {
                    url: "https://docs.rs/tokio".into(),
                    title: "Tokio".into(),
                    extracted_text: "Tokio runtime overview.".into(),
                    char_count: 23,
                    engine: "brave".into(),
                    confidence: 0.9,
                    extractor: "readability",
                    ..Default::default()
                }],
                search_results: vec![SearchHit {
                    url: "https://docs.rs/tokio".into(),
                    title: "Tokio".into(),
                    snippet: "verbose snippet that should not bloat top_sources".into(),
                    engine: "brave".into(),
                    found_by_families: 2,
                    rrf_score: 0.03,
                }],
                discovered_domains: vec![],
                total_discovered_domains: 0,
                sources_found: 10,
                sources_processed: 1,
                sources_extracted: 1,
                chars_extracted: 23,
                copilot_query: None,
                llm_answer: None,
                llm_error: None,
                citation_map: vec![],
                citation_checks: vec![],
                highlight: None,
                elapsed_seconds: 1.0,
                engine_stats: crate::models::EngineStats {
                    engines_queried: vec!["brave".into()],
                    total_raw_results: 10,
                    deduplicated_urls: 5,
                },
            },
            DEFAULT_PASSAGES_TOP_K,
            DEFAULT_PASSAGES_MAX_CHARS,
        );
        let val = serde_json::to_value(&p_out).unwrap();
        assert!(val.get("highlight").is_none());
        let p0 = &val["passages"][0];
        assert!(p0.get("published").is_none());
        assert!(p0.get("language").is_none());
        assert!(p0.get("byline").is_none());
        assert!(p0.get("site_name").is_none());
        let ts0 = &val["top_sources"][0];
        assert!(ts0.get("snippet").is_none());
    }

    #[tokio::test]
    async fn progress_bridge_emits_mcp_progress_notifications() {
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (ptx, handle) = spawn_progress_bridge(Some(json!("tok-1")), Some(out_tx), 3);
        let ptx = ptx.unwrap();
        ptx.send(Progress::Plan {
            mode: "fast".into(),
            tier: "fast".into(),
            domain: "general".into(),
            engines: 5,
            sub_queries: vec!["q".into()],
            max_urls: 10,
        })
        .await
        .unwrap();
        ptx.send(Progress::Engines {
            raw_results: 42,
            families: 3,
            ms: 810,
        })
        .await
        .unwrap();
        drop(ptx);
        handle.unwrap().await.unwrap();

        let n1 = out_rx.recv().await.unwrap();
        let n2 = out_rx.recv().await.unwrap();
        assert_eq!(n1.method, "notifications/progress");
        assert_eq!(n1.params["progressToken"], "tok-1");
        assert_eq!(n1.params["progress"], 1);
        assert_eq!(n1.params["total"], 3);
        assert_eq!(n2.params["progress"], 2);
        assert!(n2.params["message"]
            .as_str()
            .unwrap()
            .contains("42 raw results"));
    }

    #[tokio::test]
    async fn domain_filter_validation_rejects_invalid_overlaps_and_caps() {
        let db = TempDb::new();

        // Invalid domain string -> isError: true
        let bad = call_tool(
            "search_web",
            Some(json!({"query": "rust", "include_domains": ["not a domain"]})),
            &db,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(bad.is_error);
        assert!(bad.content[0].text.contains("invalid domain"));

        // Overlap between include_domains and exclude_domains -> isError: true
        let overlap = call_tool(
            "search_passages",
            Some(json!({
                "query": "rust",
                "include_domains": ["https://www.docs.rs/tokio"],
                "exclude_domains": ["docs.rs"]
            })),
            &db,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(overlap.is_error);
        assert!(overlap.content[0]
            .text
            .contains("both include_domains and exclude_domains"));

        // Exceeding MAX_INCLUDE_DOMAINS -> isError: true
        let too_many: Vec<String> = (0..12).map(|i| format!("site{i}.example")).collect();
        let cap_err = call_tool(
            "search_web",
            Some(json!({"query": "rust", "include_domains": too_many})),
            &db,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(cap_err.is_error);
        assert!(cap_err.content[0].text.contains("at most"));

        // Valid normalization & default strict_domains = false
        let parsed = parse_domain_filter(
            Some(vec!["https://www.Docs.rs/tokio".into(), "docs.rs".into()]),
            Some(vec!["Pinterest.com".into()]),
            None,
        )
        .unwrap();
        assert_eq!(parsed.include, vec!["docs.rs"]);
        assert_eq!(parsed.exclude, vec!["pinterest.com"]);
        assert!(!parsed.strict);

        let strict = parse_domain_filter(Some(vec!["docs.rs".into()]), None, Some(true)).unwrap();
        assert!(strict.strict);
    }

    #[test]
    fn discovered_domains_text_is_clean_and_zero_noise() {
        let domains = vec![
            DiscoveredDomain {
                domain: "docs.rs".into(),
                result_count: 5,
                max_families: 3,
                top_rrf_score: 0.05,
                top_url: "https://docs.rs/tokio".into(),
                top_title: "Should not appear in text list".into(),
            },
            DiscoveredDomain {
                domain: "tokio.rs".into(),
                result_count: 2,
                max_families: 2,
                top_rrf_score: 0.03,
                top_url: "https://tokio.rs/tutorial".into(),
                top_title: "Also omitted from text list".into(),
            },
        ];
        let mut buf = String::new();
        append_discovered_domains_text(&mut buf, &domains, 18);
        let lines: Vec<&str> = buf.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "must be exactly header + bare domain list: {buf:?}"
        );
        assert!(lines[0].contains("2 of 18"));
        assert_eq!(lines[1], "docs.rs, tokio.rs");
        assert!(!buf.contains("Should not appear"));
        assert!(!buf.contains("https://"));
    }

    #[test]
    fn search_schemas_declare_domain_controls_and_discovered_domains() {
        let tools = list_tools();
        for tool in &tools[..2] {
            let props = &tool.input_schema["properties"];
            assert!(props.get("include_domains").is_some(), "{}", tool.name);
            assert!(props.get("exclude_domains").is_some(), "{}", tool.name);
            assert_eq!(props["strict_domains"]["default"], false, "{}", tool.name);

            let out_props = &tool.output_schema["properties"];
            assert!(
                out_props.get("discovered_domains").is_some(),
                "{}",
                tool.name
            );
            assert!(
                out_props.get("total_discovered_domains").is_some(),
                "{}",
                tool.name
            );
        }
    }
}
