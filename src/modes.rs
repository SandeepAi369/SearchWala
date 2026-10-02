// ============================================================================
// SearchWala v8.1 — Search modes
//
// ONE place that decides how much work a search does.
//
// WHY THIS EXISTS
// Before v8.1, mode behaviour was spread across a dozen `if is_fast_mode /
// is_research_mode / is_specialized` branches in search.rs, a second copy in
// stream.rs, and hard-coded numbers in the UI. Measured on 2026-10-02 with the
// same query in every mode, the result was close to inverted:
//
//   mode       time    chars extracted   engine tasks
//   fast       5.6 s   1,101,005          9
//   auto       4.6 s     454,170         17
//   research   9.3 s     181,249         62  (27 "Unknown engine" warnings)
//
// Fast was slower than Auto and extracted the MOST text; Deep extracted the
// LEAST. The causes were all mode-plumbing bugs:
//   * the scrape-stage early-exit (25 sources / 80k chars) applied to every
//     mode, so Deep stopped as early as Auto;
//   * Fast had no source or text budget at all, only a URL cap;
//   * Deep/Academic/Reddit/YouTube fell through to `config::enabled_engines()`
//     — the legacy 90-name list, including walled Google aliases and dead
//     hosts that have no spec;
//   * Deep took the non-ranked path: no RRF, no freshness, and BM25 scored
//     against the engine-syntax query.
//
// Every mode now resolves to a `ModeProfile`, and search.rs reads only that.
// ============================================================================

use std::time::Duration;

use crate::engines;
use crate::query_intel::{QueryComplexity, QueryIntelligence, QueryIntent};

// ─────────────────────────────────────────────────────────────────────────────
// Mode and domain
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Speed first: few engines, one SERP page, a handful of sources.
    Fast,
    /// Sizes itself to the query (see `auto_tier`).
    Auto,
    /// Maximum coverage: every engine, deep decomposition, map-reduce report.
    Deep,
    /// Structured study notes from educational sources.
    Study,
    /// Scholarly sources (OpenAlex, arXiv) and a literature-review answer.
    Academic,
    /// Reddit discussions.
    Reddit,
    /// YouTube videos.
    YouTube,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Fast => "fast",
            Mode::Auto => "auto",
            Mode::Deep => "deep",
            Mode::Study => "study",
            Mode::Academic => "academic",
            Mode::Reddit => "reddit",
            Mode::YouTube => "youtube",
        }
    }
}

/// Subject-area focus. ORTHOGONAL to the mode: "Deep + Health" is a valid
/// combination.
///
/// v7.0/v8.0 encoded a domain as its own mode string (`specialized_health`),
/// and the UI built that string by DISCARDING the selected mode — choosing
/// "Deep Research" and then "Health" silently produced a non-deep search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    General,
    Tech,
    Science,
    Finance,
    Health,
    News,
}

impl Domain {
    pub fn parse(s: &str) -> Domain {
        match s.trim().to_lowercase().as_str() {
            "tech" | "technology" | "code" | "programming" => Domain::Tech,
            "science" | "sci" => Domain::Science,
            "finance" | "business" | "markets" => Domain::Finance,
            "health" | "medical" | "medicine" => Domain::Health,
            "news" => Domain::News,
            _ => Domain::General,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Domain::General => "general",
            Domain::Tech => "tech",
            Domain::Science => "science",
            Domain::Finance => "finance",
            Domain::Health => "health",
            Domain::News => "news",
        }
    }
}

/// Parse the request's `focus_mode` and optional `domain`.
///
/// Accepts every legacy spelling so existing API clients keep working:
/// `lite` → Auto, `research` → Deep, `specialized_<domain>` → Auto + domain.
pub fn parse(focus_mode: Option<&str>, domain: Option<&str>) -> (Mode, Domain) {
    let raw = focus_mode.map(|m| m.trim().to_lowercase()).unwrap_or_default();

    let (mode, legacy_domain) = match raw.as_str() {
        "fast" | "quick" => (Mode::Fast, None),
        "deep" | "research" | "deep_research" => (Mode::Deep, None),
        "study" | "learn" | "notes" => (Mode::Study, None),
        "academic" | "scholar" | "papers" => (Mode::Academic, None),
        "reddit" => (Mode::Reddit, None),
        "youtube" | "video" => (Mode::YouTube, None),
        s if s.starts_with("specialized") => {
            let d = s.strip_prefix("specialized_").unwrap_or("");
            (Mode::Auto, Some(Domain::parse(d)))
        }
        // "auto", "lite", "", and anything unrecognised.
        _ => (Mode::Auto, None),
    };

    let domain = domain
        .map(Domain::parse)
        .filter(|d| *d != Domain::General)
        .or(legacy_domain)
        .unwrap_or(Domain::General);

    (mode, domain)
}

// ─────────────────────────────────────────────────────────────────────────────
// Profile
// ─────────────────────────────────────────────────────────────────────────────

/// How the answer is synthesized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Synthesis {
    /// Direct answer + a few cited bullets.
    Concise,
    /// Intent-aware single call (the pre-v8.1 default).
    Standard,
    /// Map-reduce: per-batch cited notes, then one report over all notes.
    Report,
    /// Structured study notes.
    StudyNotes,
    /// Literature review.
    Literature,
    /// Summary of community discussion.
    Community,
    /// Overview of videos found.
    Video,
}

/// How sub-queries are generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Facets {
    /// The pre-v8.1 conservative decomposition (comparison sides, year,
    /// multi-word entities).
    Basic,
    /// Aspect expansion by intent — overview, evidence, limitations, latest.
    Deep,
    /// Learning-oriented: explanation and worked examples.
    Study,
    /// Review / meta-analysis phrasing for scholarly indices.
    Academic,
}

#[derive(Debug, Clone)]
pub struct ModeProfile {
    pub mode: Mode,
    pub domain: Domain,
    /// Human-readable tier, e.g. "auto:light", for logs and the API response.
    pub tier: &'static str,

    // ── Retrieval ──
    pub engines: Vec<String>,
    /// SERP pages per engine. Fast uses 1: a 3-page engine aborted at a 2.5 s
    /// deadline LOSES the pages it already fetched (measured: 2 of 9 Fast
    /// tasks aborted), so asking for fewer pages is strictly better.
    pub page_budget: usize,
    pub engine_deadline: Duration,
    /// Stop waiting for engines once (results, independent families) are in.
    ///
    /// Only Fast and Auto:lookup use it. Phase 5 measured that exiting early
    /// cost Auto ~60% of its raw results and most cross-engine agreement — the
    /// wrong trade when consensus is the point. For Fast, speed IS the point:
    /// one hung engine was holding the stage to the full 2.5 s deadline when
    /// three families had answered by ~1.3 s.
    pub engine_early_exit: Option<(usize, usize)>,
    /// Restrict every engine query to one site (reddit.com, youtube.com).
    pub site_filter: Option<&'static str>,

    // ── Sub-queries ──
    pub max_sub_queries: usize,
    pub facets: Facets,
    /// Let the LLM plan sub-queries when one is configured (Deep only).
    pub llm_query_planning: bool,

    // ── Scraping ──
    pub max_urls: usize,
    pub scrape_timeout: Duration,
    pub scrape_stage_deadline: Duration,
    /// Early-exit once BOTH thresholds are met.
    pub scrape_min_sources: usize,
    pub scrape_min_chars: usize,
    /// Use the search-engine snippet when a page cannot be fetched. Only worth
    /// it where pages are routinely blocked (Reddit, YouTube).
    pub snippet_fallback: bool,

    // ── Ranking ──
    pub llm_top_k: usize,
    pub max_chunks_per_url: usize,
    /// Apply recency decay (still gated on the query being time-sensitive).
    pub freshness: bool,

    // ── Synthesis ──
    pub synthesis: Synthesis,
    /// Hard ceiling on context characters, below the model's own budget.
    pub context_char_cap: usize,
    pub max_output_tokens: usize,
}

/// URL budget bounds per mode: (floor, ceiling). An explicit `max_results`
/// from the client is clamped into this range; otherwise the profile default
/// is used.
fn url_bounds(mode: Mode) -> (usize, usize) {
    match mode {
        Mode::Fast => (3, 20),
        Mode::Auto => (5, 100),
        Mode::Deep => (40, 300),
        Mode::Study => (10, 80),
        Mode::Academic => (10, 80),
        Mode::Reddit | Mode::YouTube => (5, 50),
    }
}

/// Engines reliable enough for a sub-3-second answer: three JSON APIs plus the
/// three HTML engines that worked consistently with TLS impersonation.
fn fast_roster() -> Vec<String> {
    ["wikipedia", "bing", "duckduckgo", "brave", "marginalia_api"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Engines that honour a `site:` operator. v8.0 appended `site:reddit.com` to
/// the query and sent it to EVERY engine — including the Wikipedia and
/// Marginalia JSON APIs, which searched for the literal string.
fn site_capable_roster() -> Vec<String> {
    ["bing", "duckduckgo", "brave", "yahoo"].iter().map(|s| s.to_string()).collect()
}

fn add(list: &mut Vec<String>, names: &[&str]) {
    for n in names {
        if !list.iter().any(|e| e == n) {
            list.push((*n).to_string());
        }
    }
}

/// Add an operator-configured SearXNG instance to any roster. It federates
/// many engines through the operator's own IP, so it helps every mode.
fn with_searxng(mut list: Vec<String>) -> Vec<String> {
    if engines::searxng::is_enabled() {
        add(&mut list, &["searxng"]);
    }
    list
}

/// Domain-specific engine additions.
fn domain_engines(domain: Domain, list: &mut Vec<String>) {
    match domain {
        Domain::News | Domain::Finance => add(list, &["bing_news", "brave_news"]),
        Domain::Science => add(list, &["openalex", "arxiv", "wikipedia"]),
        Domain::Health => add(list, &["openalex", "wikipedia"]),
        Domain::Tech => add(list, &["marginalia_api"]),
        Domain::General => {}
    }
}

/// The three Auto tiers. Auto used to scale only `max_urls`, and the Complex
/// branch did `max_urls.max(100)` — RAISING a caller's explicit cap of 20 to
/// 100.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoTier {
    /// Navigational lookups: the user wants a site, not an essay.
    Lookup,
    /// Short factual questions.
    Light,
    Standard,
    /// Long, comparative or explanatory questions.
    Heavy,
}

pub fn auto_tier(intel: &QueryIntelligence) -> AutoTier {
    match intel.intent {
        QueryIntent::Navigational => return AutoTier::Lookup,
        QueryIntent::Research | QueryIntent::Comparison => return AutoTier::Heavy,
        _ => {}
    }
    match intel.complexity {
        QueryComplexity::Simple => AutoTier::Light,
        QueryComplexity::Medium => AutoTier::Standard,
        QueryComplexity::Complex => AutoTier::Heavy,
    }
}

/// Resolve the full profile for one request.
pub fn resolve(
    mode: Mode,
    domain: Domain,
    intel: &QueryIntelligence,
    requested_urls: Option<usize>,
) -> ModeProfile {
    let mut p = match mode {
        // ── FAST ────────────────────────────────────────────────────────────
        Mode::Fast => ModeProfile {
            mode,
            domain,
            tier: "fast",
            engines: fast_roster(),
            page_budget: 1,
            engine_deadline: Duration::from_millis(2_500),
            engine_early_exit: Some((24, 3)),
            site_filter: None,
            max_sub_queries: 1,
            facets: Facets::Basic,
            llm_query_planning: false,
            max_urls: 10,
            scrape_timeout: Duration::from_secs(3),
            scrape_stage_deadline: Duration::from_millis(3_500),
            scrape_min_sources: 6,
            scrape_min_chars: 25_000,
            snippet_fallback: false,
            llm_top_k: 8,
            max_chunks_per_url: 2,
            freshness: true,
            synthesis: Synthesis::Concise,
            context_char_cap: 12_000,
            max_output_tokens: 700,
        },

        // ── AUTO ────────────────────────────────────────────────────────────
        Mode::Auto => {
            let tier = auto_tier(intel);
            let (name, engines_list, pages, deadline_ms, urls, min_src, min_chars, subs, top_k, out) =
                match tier {
                    AutoTier::Lookup => {
                        ("auto:lookup", fast_roster(), 1, 2_500, 8, 5, 15_000, 1, 8, 600)
                    }
                    AutoTier::Light => (
                        "auto:light",
                        engines::primary_engines(),
                        1,
                        4_000,
                        20,
                        10,
                        50_000,
                        1,
                        15,
                        1_200,
                    ),
                    AutoTier::Standard => (
                        "auto:standard",
                        engines::all_engines(),
                        2,
                        6_000,
                        35,
                        20,
                        120_000,
                        2,
                        25,
                        2_000,
                    ),
                    AutoTier::Heavy => (
                        "auto:heavy",
                        engines::all_engines(),
                        3,
                        8_000,
                        55,
                        30,
                        250_000,
                        3,
                        35,
                        3_000,
                    ),
                };
            let mut engines_list = engines_list;
            // Time-sensitive questions get the news verticals.
            if intel.is_time_sensitive || intel.intent == QueryIntent::Temporal {
                add(&mut engines_list, &["bing_news", "brave_news"]);
            }
            ModeProfile {
                mode,
                domain,
                tier: name,
                engines: engines_list,
                page_budget: pages,
                engine_deadline: Duration::from_millis(deadline_ms),
                engine_early_exit: if tier == AutoTier::Lookup { Some((20, 3)) } else { None },
                site_filter: None,
                max_sub_queries: subs,
                facets: Facets::Basic,
                llm_query_planning: false,
                max_urls: urls,
                scrape_timeout: Duration::from_secs(if tier == AutoTier::Lookup { 3 } else { 8 }),
                scrape_stage_deadline: Duration::from_secs(match tier {
                    AutoTier::Lookup => 4,
                    AutoTier::Light => 6,
                    AutoTier::Standard => 9,
                    AutoTier::Heavy => 12,
                }),
                scrape_min_sources: min_src,
                scrape_min_chars: min_chars,
                snippet_fallback: false,
                llm_top_k: top_k,
                max_chunks_per_url: 3,
                freshness: true,
                synthesis: if tier == AutoTier::Lookup {
                    Synthesis::Concise
                } else {
                    Synthesis::Standard
                },
                context_char_cap: match tier {
                    AutoTier::Lookup => 10_000,
                    AutoTier::Light => 30_000,
                    AutoTier::Standard => 60_000,
                    AutoTier::Heavy => 90_000,
                },
                max_output_tokens: out,
            }
        }

        // ── DEEP ────────────────────────────────────────────────────────────
        Mode::Deep => {
            let mut list = engines::all_engines();
            add(&mut list, &["bing_news", "brave_news"]);
            // Scholarly indices only where they are likely to be relevant.
            // Measured: with Factual included, Deep on "capital of France"
            // ranked three economics papers about financial CAPITAL (Journal of
            // Finance, QJE) above Wikipedia's "Paris".
            if intel.intent == QueryIntent::Research
                || matches!(domain, Domain::Science | Domain::Health | Domain::Tech)
            {
                add(&mut list, &["openalex"]);
            }
            ModeProfile {
                mode,
                domain,
                tier: "deep",
                engines: list,
                page_budget: 3,
                engine_deadline: Duration::from_secs(15),
                engine_early_exit: None,
                site_filter: None,
                max_sub_queries: 5,
                facets: Facets::Deep,
                llm_query_planning: true,
                max_urls: 150,
                scrape_timeout: Duration::from_secs(10),
                scrape_stage_deadline: Duration::from_secs(40),
                // Effectively "scrape everything selected". The 25-source
                // early-exit used to apply here too, which is why Deep
                // extracted less than Fast.
                scrape_min_sources: 120,
                scrape_min_chars: 2_500_000,
                snippet_fallback: false,
                llm_top_k: 120,
                max_chunks_per_url: 4,
                freshness: true,
                synthesis: Synthesis::Report,
                context_char_cap: 400_000,
                max_output_tokens: 8_000,
            }
        }

        // ── STUDY ───────────────────────────────────────────────────────────
        Mode::Study => ModeProfile {
            mode,
            domain,
            tier: "study",
            engines: vec![
                "wikipedia".into(),
                "marginalia_api".into(),
                "bing".into(),
                "duckduckgo".into(),
                "brave".into(),
                "yahoo".into(),
            ],
            page_budget: 2,
            engine_deadline: Duration::from_secs(6),
                engine_early_exit: None,
            site_filter: None,
            max_sub_queries: 3,
            facets: Facets::Study,
            llm_query_planning: false,
            max_urls: 35,
            scrape_timeout: Duration::from_secs(8),
            scrape_stage_deadline: Duration::from_secs(12),
            scrape_min_sources: 20,
            scrape_min_chars: 150_000,
            snippet_fallback: false,
            llm_top_k: 30,
            max_chunks_per_url: 4,
            // Study topics are evergreen; recency bias would demote the
            // canonical explanation in favour of this week's blog post.
            freshness: false,
            synthesis: Synthesis::StudyNotes,
            context_char_cap: 90_000,
            max_output_tokens: 4_000,
        },

        // ── ACADEMIC ────────────────────────────────────────────────────────
        // v8.0 appended `site:edu OR site:gov OR site:nature.com` to the query
        // and sent it to every engine. The JSON APIs searched for that literal
        // text; HTML engines returned university homepages. Academic now goes
        // to scholarly indices directly.
        Mode::Academic => ModeProfile {
            mode,
            domain,
            tier: "academic",
            engines: vec![
                "openalex".into(),
                "arxiv".into(),
                "wikipedia".into(),
                "bing".into(),
                "duckduckgo".into(),
                "brave".into(),
            ],
            page_budget: 2,
            engine_deadline: Duration::from_secs(8),
                engine_early_exit: None,
            site_filter: None,
            max_sub_queries: 2,
            facets: Facets::Academic,
            llm_query_planning: false,
            max_urls: 40,
            scrape_timeout: Duration::from_secs(10),
            scrape_stage_deadline: Duration::from_secs(14),
            scrape_min_sources: 25,
            scrape_min_chars: 120_000,
            snippet_fallback: false,
            llm_top_k: 35,
            max_chunks_per_url: 3,
            freshness: false,
            synthesis: Synthesis::Literature,
            context_char_cap: 90_000,
            max_output_tokens: 4_000,
        },

        // ── REDDIT / YOUTUBE ───────────────────────────────────────────────
        Mode::Reddit | Mode::YouTube => ModeProfile {
            mode,
            domain,
            tier: if mode == Mode::Reddit { "reddit" } else { "youtube" },
            engines: site_capable_roster(),
            page_budget: 2,
            engine_deadline: Duration::from_secs(6),
                engine_early_exit: None,
            site_filter: Some(if mode == Mode::Reddit { "reddit.com" } else { "youtube.com" }),
            max_sub_queries: 1,
            facets: Facets::Basic,
            llm_query_planning: false,
            max_urls: 25,
            scrape_timeout: Duration::from_secs(8),
            scrape_stage_deadline: Duration::from_secs(10),
            scrape_min_sources: 15,
            scrape_min_chars: 40_000,
            // Reddit thread pages come back as a "Welcome to Reddit" CAPTCHA
            // from many IPs, and YouTube pages carry only a description. The
            // engine snippet is often the best text available.
            snippet_fallback: true,
            llm_top_k: 25,
            max_chunks_per_url: 2,
            freshness: true,
            synthesis: if mode == Mode::Reddit { Synthesis::Community } else { Synthesis::Video },
            context_char_cap: 40_000,
            max_output_tokens: 2_000,
        },
    };

    // ── Domain overlay ──
    if mode != Mode::Reddit && mode != Mode::YouTube {
        domain_engines(domain, &mut p.engines);
    }
    if domain == Domain::News {
        p.freshness = true;
    }

    // Fast stays small even with a domain overlay.
    if mode == Mode::Fast {
        p.engines.truncate(7);
    }

    if mode != Mode::Reddit && mode != Mode::YouTube {
        p.engines = with_searxng(std::mem::take(&mut p.engines));
    }

    // ── Explicit URL budget from the client ──
    if let Some(req) = requested_urls {
        let (lo, hi) = url_bounds(mode);
        p.max_urls = req.clamp(lo, hi);
    }
    // The early-exit source threshold can never exceed what we scrape.
    p.scrape_min_sources = p.scrape_min_sources.min(p.max_urls.max(1));

    p
}

/// Every engine any mode can dispatch — what `/health` reports.
pub fn all_roster_engines() -> Vec<String> {
    let intel = crate::query_intel::analyze_query("example research question about a topic");
    let mut all: Vec<String> = Vec::new();
    for mode in [Mode::Fast, Mode::Auto, Mode::Deep, Mode::Study, Mode::Academic, Mode::Reddit, Mode::YouTube] {
        for domain in [Domain::General, Domain::Tech, Domain::Science, Domain::Finance, Domain::Health, Domain::News] {
            for e in resolve(mode, domain, &intel, None).engines {
                if !all.contains(&e) {
                    all.push(e);
                }
            }
        }
    }
    all.sort();
    all
}

/// One-line summary of a mode's default profile, for `/config`.
pub fn describe_modes() -> serde_json::Value {
    let intel = crate::query_intel::analyze_query("how does a typical example topic work in practice");
    let mut out = serde_json::Map::new();
    for mode in [Mode::Fast, Mode::Auto, Mode::Deep, Mode::Study, Mode::Academic, Mode::Reddit, Mode::YouTube] {
        let p = resolve(mode, Domain::General, &intel, None);
        out.insert(
            mode.as_str().to_string(),
            serde_json::json!({
                "tier_for_sample_query": p.tier,
                "engines": p.engines,
                "serp_pages_per_engine": p.page_budget,
                "engine_deadline_ms": p.engine_deadline.as_millis() as u64,
                "max_urls": p.max_urls,
                "sub_queries": p.max_sub_queries,
                "scrape_deadline_ms": p.scrape_stage_deadline.as_millis() as u64,
                "chunks_to_llm": p.llm_top_k,
                "synthesis": format!("{:?}", p.synthesis),
                "max_output_tokens": p.max_output_tokens,
            }),
        );
    }
    serde_json::Value::Object(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// Source preferences per mode and domain
// ─────────────────────────────────────────────────────────────────────────────

const STUDY_HOSTS: &[&str] = &[
    "wikipedia.org", "britannica.com", "khanacademy.org", "openstax.org",
    "libretexts.org", "ncert.nic.in", "byjus.com", "geeksforgeeks.org",
    "mathsisfun.com", "physicsclassroom.com", "ocw.mit.edu", "developer.mozilla.org",
    "chem.libretexts.org", "bio.libretexts.org", "math.libretexts.org",
    "cuemath.com", "vedantu.com", "toppr.com", "sparknotes.com", "studysmarter.co.uk",
];

const SCHOLARLY_HOSTS: &[&str] = &[
    "nature.com", "science.org", "cell.com", "pnas.org", "arxiv.org", "nih.gov",
    "sciencedirect.com", "springer.com", "link.springer.com", "wiley.com",
    "plos.org", "acm.org", "ieee.org", "jstor.org", "openalex.org", "doi.org",
    "semanticscholar.org", "biorxiv.org", "medrxiv.org", "thelancet.com",
    "nejm.org", "bmj.com", "frontiersin.org", "mdpi.com", "aps.org", "iop.org",
];

const TECH_HOSTS: &[&str] = &[
    "docs.rs", "doc.rust-lang.org", "developer.mozilla.org", "docs.python.org",
    "learn.microsoft.com", "github.com", "stackoverflow.com", "kubernetes.io",
    "docs.docker.com", "go.dev", "nodejs.org", "react.dev", "postgresql.org",
    "man7.org", "rfc-editor.org", "w3.org", "developer.apple.com",
    "developer.android.com", "cloud.google.com", "docs.aws.amazon.com",
];

const FINANCE_HOSTS: &[&str] = &[
    "reuters.com", "bloomberg.com", "ft.com", "wsj.com", "sec.gov",
    "federalreserve.gov", "rbi.org.in", "sebi.gov.in", "imf.org", "worldbank.org",
    "moneycontrol.com", "economictimes.indiatimes.com", "livemint.com",
    "investopedia.com", "nseindia.com", "bseindia.com", "cnbc.com",
];

const HEALTH_HOSTS: &[&str] = &[
    "who.int", "nih.gov", "cdc.gov", "nhs.uk", "mayoclinic.org", "medlineplus.gov",
    "cochrane.org", "clevelandclinic.org", "hopkinsmedicine.org", "fda.gov",
    "mohfw.gov.in", "icmr.gov.in", "ema.europa.eu", "nice.org.uk",
];

const NEWS_HOSTS: &[&str] = &[
    "reuters.com", "apnews.com", "bbc.com", "bbc.co.uk", "theguardian.com",
    "nytimes.com", "aljazeera.com", "thehindu.com", "indianexpress.com",
    "hindustantimes.com", "ndtv.com", "amarujala.com", "bhaskar.com", "jagran.com",
    "timesofindia.indiatimes.com", "livemint.com", "npr.org", "washingtonpost.com",
];

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
        .unwrap_or_default()
}

fn host_matches(host: &str, list: &[&str]) -> bool {
    list.iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Ranking multiplier for preferred sources in this mode/domain.
///
/// Stacks with `llm::trust_score`; capped so a preferred blog cannot outrank a
/// genuinely more relevant page purely on reputation.
pub fn source_boost(url: &str, mode: Mode, domain: Domain) -> f64 {
    let host = host_of(url);
    if host.is_empty() {
        return 1.0;
    }
    let mut b: f64 = 1.0;
    match mode {
        Mode::Study => {
            if host_matches(&host, STUDY_HOSTS) || host.ends_with(".edu") || host.ends_with(".ac.in") {
                b *= 1.35;
            }
        }
        Mode::Academic => {
            if host_matches(&host, SCHOLARLY_HOSTS) || host.ends_with(".edu") {
                b *= 1.4;
            }
        }
        _ => {}
    }
    let list: &[&str] = match domain {
        Domain::Tech => TECH_HOSTS,
        Domain::Science => SCHOLARLY_HOSTS,
        Domain::Finance => FINANCE_HOSTS,
        Domain::Health => HEALTH_HOSTS,
        Domain::News => NEWS_HOSTS,
        Domain::General => &[],
    };
    if host_matches(&host, list) {
        b *= 1.3;
    }
    b.min(1.8)
}

// ─────────────────────────────────────────────────────────────────────────────
// Sub-query facets
// ─────────────────────────────────────────────────────────────────────────────

/// Strip question scaffolding so facets attach to the topic, not the phrasing:
/// "explain photosynthesis for class 10" -> "photosynthesis for class 10".
pub fn core_topic(query: &str) -> String {
    let q = query.trim().trim_end_matches(['?', '.', '!']);
    let lower = q.to_lowercase();
    const PREFIXES: &[&str] = &[
        "please explain ", "can you explain ", "explain to me ", "explain ",
        "what is the ", "what is a ", "what is an ", "what is ", "what are the ",
        "what are ", "who is ", "who was ", "how does ", "how do ", "how is ",
        "tell me about ", "notes on ", "study notes on ", "study notes for ",
        "teach me ", "define ", "definition of ", "describe ", "overview of ",
    ];
    for p in PREFIXES {
        if lower.starts_with(p) {
            // Byte offset is safe: every prefix is ASCII, so it ends on a
            // character boundary in the original string too.
            let rest = q[p.len()..].trim();
            let rest = rest
                .strip_suffix(" work")
                .or_else(|| rest.strip_suffix(" works"))
                .unwrap_or(rest)
                .trim();
            if crate::util::char_len(rest) >= 3 {
                return rest.to_string();
            }
        }
    }
    // Hindi question scaffolding usually comes AFTER the topic:
    // "प्रकाश संश्लेषण क्या है" = "photosynthesis what-is".
    const HI_SUFFIXES: &[&str] = &[
        " क्या होता है", " क्या होती है", " क्या हैं", " क्या है", " कैसे काम करता है",
        " कैसे काम करती है", " के बारे में बताइए", " के बारे में", " समझाइए", " समझाओ",
    ];
    for s in HI_SUFFIXES {
        if let Some(rest) = q.strip_suffix(s) {
            let rest = rest.trim();
            if crate::util::char_len(rest) >= 2 {
                return rest.to_string();
            }
        }
    }
    q.to_string()
}

/// Mode-specific sub-queries. The original query always comes first.
pub fn facet_queries(query: &str, intel: &QueryIntelligence, facets: Facets, max: usize) -> Vec<String> {
    let q = query.trim();
    if q.is_empty() || max == 0 {
        return Vec::new();
    }
    let max = max.max(1);
    let topic = core_topic(q);
    let year = chrono::Utc::now().format("%Y").to_string();

    let mut out: Vec<String> = vec![q.to_string()];

    let extra: Vec<String> = match facets {
        Facets::Basic => {
            // Reuse the existing conservative decomposer.
            let base = crate::query_intel::decompose_query(q, intel);
            base.into_iter().skip(1).collect()
        }
        Facets::Deep => {
            let mut v = Vec::new();
            // Comparison sides are the most valuable split there is.
            if intel.intent == QueryIntent::Comparison {
                for sep in [" vs ", " vs. ", " versus ", " compared to "] {
                    if let Some((a, b)) = q.split_once(sep) {
                        v.push(a.trim().to_string());
                        v.push(b.trim().to_string());
                        break;
                    }
                }
            }
            match intel.intent {
                QueryIntent::Temporal => {
                    v.push(format!("{topic} latest news {year}"));
                    v.push(format!("{topic} timeline"));
                    v.push(format!("{topic} analysis"));
                }
                QueryIntent::Person => {
                    v.push(format!("{topic} biography"));
                    v.push(format!("{topic} latest news"));
                    v.push(format!("{topic} interview"));
                }
                QueryIntent::HowTo => {
                    v.push(format!("{topic} step by step guide"));
                    v.push(format!("{topic} common mistakes"));
                    v.push(format!("{topic} best practices"));
                }
                QueryIntent::Navigational => {}
                // A factual question needs corroboration, not "limitations" or
                // "research evidence" — those facets changed the meaning of
                // "capital of France" into economics papers about capital.
                QueryIntent::Factual => {
                    v.push(format!("{topic} overview"));
                    v.push(format!("{topic} history"));
                    v.push(format!("{topic} key facts"));
                }
                _ => {
                    v.push(format!("{topic} explained"));
                    v.push(format!("{topic} research evidence"));
                    v.push(format!("{topic} limitations challenges"));
                    v.push(format!("{topic} latest developments {year}"));
                }
            }
            v
        }
        Facets::Study => vec![
            format!("{topic} explained simply"),
            format!("{topic} examples"),
            format!("{topic} definition key concepts"),
        ],
        Facets::Academic => vec![
            format!("{topic} review"),
            format!("{topic} meta-analysis"),
        ],
    };

    for e in extra {
        if out.len() >= max {
            break;
        }
        let e = e.split_whitespace().collect::<Vec<_>>().join(" ");
        if crate::util::char_len(&e) < 3 {
            continue;
        }
        if !out.iter().any(|x| x.eq_ignore_ascii_case(&e)) {
            out.push(e);
        }
    }
    out.truncate(max);
    out
}

/// Apply a `site:` filter for Reddit/YouTube modes.
pub fn apply_site_filter(query: &str, site: Option<&str>) -> String {
    match site {
        Some(s) if !query.to_lowercase().contains("site:") => format!("{} site:{}", query.trim(), s),
        _ => query.trim().to_string(),
    }
}

/// Should this result be kept under the profile's site filter?
pub fn matches_site_filter(url: &str, site: Option<&str>) -> bool {
    let Some(site) = site else { return true };
    let host = host_of(url);
    match site {
        "youtube.com" => host.ends_with("youtube.com") || host == "youtu.be",
        s => host == s || host.ends_with(&format!(".{s}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_intel::analyze_query;

    fn prof(mode: &str, domain: Option<&str>, q: &str) -> ModeProfile {
        let (m, d) = parse(Some(mode), domain);
        resolve(m, d, &analyze_query(q), None)
    }

    #[test]
    fn parses_every_legacy_spelling() {
        assert_eq!(parse(None, None), (Mode::Auto, Domain::General));
        assert_eq!(parse(Some("lite"), None).0, Mode::Auto);
        assert_eq!(parse(Some("research"), None).0, Mode::Deep);
        assert_eq!(parse(Some("DEEP"), None).0, Mode::Deep);
        assert_eq!(parse(Some("study"), None).0, Mode::Study);
        assert_eq!(parse(Some("specialized_health"), None), (Mode::Auto, Domain::Health));
        assert_eq!(parse(Some("nonsense"), None).0, Mode::Auto);
    }

    #[test]
    fn domain_composes_with_mode_instead_of_replacing_it() {
        // v8.0's UI turned "Deep + Health" into "specialized_health" and lost
        // the Deep. Both now survive.
        assert_eq!(parse(Some("deep"), Some("health")), (Mode::Deep, Domain::Health));
        // An explicit domain wins over a legacy specialized_ string.
        assert_eq!(parse(Some("specialized_tech"), Some("news")).1, Domain::News);
    }

    #[test]
    fn modes_are_ordered_by_depth() {
        let q = "how does mRNA vaccine technology work";
        let fast = prof("fast", None, q);
        let auto = prof("auto", None, q);
        let deep = prof("deep", None, q);

        // The measured v8.0 inversion: Fast extracted more than Deep.
        assert!(fast.max_urls < auto.max_urls && auto.max_urls < deep.max_urls);
        assert!(fast.engine_deadline < auto.engine_deadline);
        assert!(auto.engine_deadline < deep.engine_deadline);
        assert!(fast.scrape_stage_deadline < auto.scrape_stage_deadline);
        assert!(fast.scrape_min_chars < auto.scrape_min_chars);
        assert!(auto.scrape_min_chars < deep.scrape_min_chars);
        assert!(fast.llm_top_k < auto.llm_top_k && auto.llm_top_k < deep.llm_top_k);
        assert!(fast.engines.len() < deep.engines.len());
        assert!(fast.max_sub_queries < deep.max_sub_queries);
        assert!(fast.page_budget < deep.page_budget);
    }

    #[test]
    fn fast_is_genuinely_small() {
        let p = prof("fast", None, "capital of france");
        assert_eq!(p.page_budget, 1);
        assert_eq!(p.max_sub_queries, 1);
        assert!(p.max_urls <= 12);
        assert!(p.engine_deadline <= Duration::from_millis(2_500));
        assert!(p.scrape_stage_deadline <= Duration::from_secs(4));
        assert_eq!(p.synthesis, Synthesis::Concise);
        assert!(p.max_output_tokens <= 800);
    }

    #[test]
    fn only_fast_paths_exit_engines_early() {
        // Early exit trades consensus for speed — right for Fast, wrong for
        // everything that ranks on cross-engine agreement.
        assert!(prof("fast", None, "capital of france").engine_early_exit.is_some());
        assert!(prof("auto", None, "github login").engine_early_exit.is_some());
        for (mode, q) in [("auto", "capital of france"), ("auto", "rust vs go for servers"),
                          ("deep", "x y z"), ("study", "photosynthesis"), ("academic", "crispr")] {
            assert!(prof(mode, None, q).engine_early_exit.is_none(), "{mode}/{q}");
        }
    }

    #[test]
    fn deep_does_not_stop_early() {
        let p = prof("deep", None, "impact of large language models on software engineering");
        assert!(p.scrape_min_sources >= 100, "Deep must not early-exit at Auto's thresholds");
        assert!(p.scrape_min_chars >= 1_000_000);
        assert_eq!(p.synthesis, Synthesis::Report);
        assert!(p.max_sub_queries >= 4);
        assert!(p.llm_query_planning);
    }

    #[test]
    fn auto_scales_with_the_query() {
        let nav = prof("auto", None, "github login");
        let light = prof("auto", None, "capital of france");
        let heavy = prof("auto", None, "postgres vs mysql for analytics workloads at scale");
        assert_eq!(nav.tier, "auto:lookup");
        assert_eq!(light.tier, "auto:light");
        assert_eq!(heavy.tier, "auto:heavy");
        assert!(nav.max_urls < light.max_urls && light.max_urls < heavy.max_urls);
        assert!(nav.engines.len() < heavy.engines.len());
        assert!(light.max_sub_queries < heavy.max_sub_queries);
    }

    #[test]
    fn auto_adds_news_for_time_sensitive_queries() {
        let p = prof("auto", None, "latest iphone release news");
        assert!(p.engines.iter().any(|e| e == "bing_news"));
        let q = prof("auto", None, "capital of france");
        assert!(!q.engines.iter().any(|e| e == "bing_news"));
    }

    #[test]
    fn explicit_url_budget_is_clamped_never_inverted() {
        let intel = analyze_query("postgres vs mysql for analytics");
        // v8.0 Auto did max_urls.max(100), raising a caller's 20 to 100.
        let p = resolve(Mode::Auto, Domain::General, &intel, Some(20));
        assert_eq!(p.max_urls, 20);
        let f = resolve(Mode::Fast, Domain::General, &intel, Some(500));
        assert!(f.max_urls <= 20, "Fast must stay fast even if asked for 500");
        let d = resolve(Mode::Deep, Domain::General, &intel, Some(5));
        assert!(d.max_urls >= 40, "Deep has a floor");
        // The early-exit threshold never exceeds what is scraped.
        assert!(p.scrape_min_sources <= p.max_urls);
    }

    #[test]
    fn rosters_only_name_engines_that_exist() {
        // v8.0's specialized lists still named google, google_scholar,
        // startpage, qwant, stract and duckduckgo_news. Every name in every
        // profile must now build an engine.
        let q = "how does mRNA vaccine technology work";
        for mode in ["fast", "auto", "deep", "study", "academic", "reddit", "youtube"] {
            for dom in ["", "tech", "science", "finance", "health", "news"] {
                let p = prof(mode, Some(dom), q);
                for name in &p.engines {
                    let built = crate::engines::get_engines(std::slice::from_ref(name));
                    assert_eq!(built.len(), 1, "mode={mode} domain={dom}: {name:?} builds nothing");
                }
                for walled in ["google", "google_scholar", "startpage", "qwant", "stract", "duckduckgo_news"] {
                    assert!(
                        !p.engines.iter().any(|e| e == walled),
                        "mode={mode} domain={dom} dispatches walled engine {walled}"
                    );
                }
            }
        }
    }

    #[test]
    fn site_filtered_modes_only_use_site_capable_engines() {
        for mode in ["reddit", "youtube"] {
            let p = prof(mode, None, "best mechanical keyboard");
            assert!(p.site_filter.is_some());
            for e in &p.engines {
                assert!(
                    !["wikipedia", "marginalia_api", "wiby", "openalex", "arxiv"].contains(&e.as_str()),
                    "{mode} sends a site: operator to {e}, which searches for it literally"
                );
            }
            assert!(p.snippet_fallback);
        }
    }

    #[test]
    fn academic_uses_scholarly_indices() {
        let p = prof("academic", None, "CRISPR off-target effects");
        assert!(p.engines.iter().any(|e| e == "openalex"));
        assert!(p.engines.iter().any(|e| e == "arxiv"));
        assert_eq!(p.synthesis, Synthesis::Literature);
        assert!(p.site_filter.is_none(), "no more site:edu OR site:gov string");
    }

    #[test]
    fn study_is_evergreen_and_structured() {
        let p = prof("study", None, "explain photosynthesis");
        assert_eq!(p.synthesis, Synthesis::StudyNotes);
        assert!(!p.freshness);
        assert_eq!(p.facets, Facets::Study);
    }

    #[test]
    fn domains_add_relevant_engines() {
        assert!(prof("auto", Some("news"), "election results").engines.iter().any(|e| e == "bing_news"));
        assert!(prof("auto", Some("science"), "dark matter").engines.iter().any(|e| e == "openalex"));
        assert!(prof("fast", Some("finance"), "nifty today").engines.len() <= 7);
    }

    #[test]
    fn source_boost_prefers_mode_and_domain_authorities() {
        assert!(source_boost("https://en.wikipedia.org/wiki/X", Mode::Study, Domain::General) > 1.0);
        assert!(source_boost("https://ncert.nic.in/textbook", Mode::Study, Domain::General) > 1.0);
        assert!(source_boost("https://www.nature.com/articles/x", Mode::Academic, Domain::General) > 1.0);
        assert!(source_boost("https://www.who.int/news", Mode::Auto, Domain::Health) > 1.0);
        assert!(source_boost("https://docs.rs/tokio", Mode::Auto, Domain::Tech) > 1.0);
        assert_eq!(source_boost("https://random-blog.test/x", Mode::Study, Domain::Health), 1.0);
        // Capped.
        assert!(source_boost("https://www.nih.gov/x", Mode::Academic, Domain::Health) <= 1.8);
        // Lookalike hosts do not match.
        assert_eq!(source_boost("https://notwikipedia.org.evil.test/", Mode::Study, Domain::General), 1.0);
    }

    #[test]
    fn core_topic_strips_question_scaffolding() {
        assert_eq!(core_topic("explain photosynthesis for class 10"), "photosynthesis for class 10");
        assert_eq!(core_topic("What is quantum entanglement?"), "quantum entanglement");
        assert_eq!(core_topic("how does TCP congestion control work"), "TCP congestion control");
        assert_eq!(core_topic("photosynthesis"), "photosynthesis");
        // Non-ASCII queries pass through untouched and never panic.
        assert_eq!(core_topic("प्रकाश संश्लेषण"), "प्रकाश संश्लेषण");
        // Hindi question words trail the topic.
        assert_eq!(core_topic("प्रकाश संश्लेषण क्या है"), "प्रकाश संश्लेषण");
        assert_eq!(core_topic("प्रकाश संश्लेषण क्या है?"), "प्रकाश संश्लेषण");
        assert_eq!(core_topic("इंटरनेट कैसे काम करता है"), "इंटरनेट");
    }

    #[test]
    fn facets_respect_the_cap_and_keep_the_original_first() {
        let q = "how does mRNA vaccine technology work";
        let intel = analyze_query(q);
        for (facets, max) in [(Facets::Deep, 5), (Facets::Study, 3), (Facets::Academic, 2), (Facets::Basic, 3)] {
            let v = facet_queries(q, &intel, facets, max);
            assert!(!v.is_empty() && v.len() <= max, "{facets:?}: {v:?}");
            assert_eq!(v[0], q);
            let mut lower: Vec<String> = v.iter().map(|s| s.to_lowercase()).collect();
            lower.sort();
            lower.dedup();
            assert_eq!(lower.len(), v.len(), "duplicates in {v:?}");
        }
        assert_eq!(facet_queries(q, &intel, Facets::Deep, 1), vec![q.to_string()]);
    }

    #[test]
    fn deep_facets_do_not_change_the_meaning_of_factual_questions() {
        let q = "capital of France";
        let v = facet_queries(q, &analyze_query(q), Facets::Deep, 5);
        for bad in ["limitations", "research evidence", "challenges"] {
            assert!(!v.iter().any(|s| s.contains(bad)), "{bad} facet on a factual query: {v:?}");
        }
        // Explanatory questions still get the research facets.
        let r = "how does mRNA vaccine technology work";
        let v = facet_queries(r, &analyze_query(r), Facets::Deep, 5);
        assert!(v.iter().any(|s| s.contains("research evidence")), "{v:?}");
    }

    #[test]
    fn deep_only_adds_scholarly_indices_for_research_questions() {
        assert!(!prof("deep", None, "capital of France").engines.iter().any(|e| e == "openalex"));
        assert!(prof("deep", None, "how does mRNA vaccine technology work").engines.iter().any(|e| e == "openalex"));
        assert!(prof("deep", Some("health"), "capital of France").engines.iter().any(|e| e == "openalex"));
    }

    #[test]
    fn deep_facets_split_comparisons() {
        let q = "rust vs go for backend services";
        let v = facet_queries(q, &analyze_query(q), Facets::Deep, 5);
        assert!(v.iter().any(|s| s == "rust"), "{v:?}");
        assert!(v.iter().any(|s| s.starts_with("go")), "{v:?}");
    }

    #[test]
    fn site_filter_helpers() {
        assert_eq!(apply_site_filter("best keyboard", Some("reddit.com")), "best keyboard site:reddit.com");
        assert_eq!(apply_site_filter("x site:foo.com", Some("reddit.com")), "x site:foo.com");
        assert_eq!(apply_site_filter("x", None), "x");
        assert!(matches_site_filter("https://old.reddit.com/r/x", Some("reddit.com")));
        assert!(!matches_site_filter("https://notreddit.com/r/x", Some("reddit.com")));
        assert!(matches_site_filter("https://youtu.be/abc", Some("youtube.com")));
        assert!(matches_site_filter("https://anything.test/", None));
    }
}
