// ============================================================================
// SearchWala v8.0 — Engine Health Registry
//
// WHY THIS EXISTS
// v7.0 could not distinguish "this engine found nothing for this query" from
// "this engine has been dead for six months". That is why 28 of 35 engines
// shipped broken: google returned an `enablejs` interstitial, bing served its
// homepage, duckduckgo served a CAPTCHA, and six hosts did not resolve at all
// — all silently parsed as empty result sets.
//
// The counter that matters is `zero_results`: HTTP 200 with zero PARSED
// results means the selectors have rotted. That is invisible without this
// metric and is distinguishable from a block only by it.
// ============================================================================

use crate::models::RawSearchResult;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, RwLock};

#[derive(Default, Debug)]
pub struct EngineHealth {
    /// Requests we attempted.
    pub attempts: AtomicU64,
    /// HTTP 200 responses that we actually parsed.
    pub http_ok: AtomicU64,
    /// Non-200, or 200 with a bot-challenge marker in the body.
    pub blocked: AtomicU64,
    /// DNS / TCP / TLS failure — the host is unreachable.
    pub transport_err: AtomicU64,
    /// HTTP 200 but the parser found nothing => SELECTOR ROT.
    pub zero_results: AtomicU64,
    /// Total results parsed across all attempts.
    pub results_total: AtomicU64,
}

static REGISTRY: LazyLock<RwLock<HashMap<String, EngineHealth>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Minimum attempts before we are willing to judge an engine.
const MIN_SAMPLE: u64 = 8;

fn with_engine<F: FnOnce(&EngineHealth)>(engine: &str, f: F) {
    // Fast path: already registered, shared read lock only.
    if let Ok(map) = REGISTRY.read() {
        if let Some(h) = map.get(engine) {
            f(h);
            return;
        }
    }
    // Slow path: insert once.
    if let Ok(mut map) = REGISTRY.write() {
        let h = map.entry(engine.to_string()).or_default();
        f(h);
    }
}

/// Register engines so `/health/engines` lists them before their first use.
pub fn register_known(engines: &[String]) {
    for e in engines {
        with_engine(e, |_| {});
    }
}

pub fn record_attempt(engine: &str) {
    with_engine(engine, |h| {
        h.attempts.fetch_add(1, Ordering::Relaxed);
    });
}

/// Non-200 status, or 200 with a challenge marker (pass `status == 0` for the
/// soft-block case).
pub fn record_block(engine: &str, status: u16) {
    with_engine(engine, |h| {
        h.blocked.fetch_add(1, Ordering::Relaxed);
    });
    tracing::debug!("health[{}]: blocked (status={})", engine, status);
}

pub fn record_transport_err(engine: &str) {
    with_engine(engine, |h| {
        h.transport_err.fetch_add(1, Ordering::Relaxed);
    });
}

/// Save a response body that parsed to zero results, so selector rot can be
/// diagnosed instead of guessed at. Opt-in via `DUMP_SERP_DIR`.
///
/// This exists because "HTTP 200 but 0 results" is the failure mode that hid
/// ~28 broken engines through all of v7.0, and reproducing it by hand requires
/// matching the exact request the server made.
pub fn dump_zero_result_body(engine: &str, url: &str, body: &str) {
    let Ok(dir) = std::env::var("DUMP_SERP_DIR") else { return };
    if dir.trim().is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    let safe: String = engine
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let path = std::path::Path::new(&dir).join(format!(
        "zero_{}_{}.html",
        safe,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let header = format!("<!-- engine={engine} url={url} -->\n");
    if std::fs::write(&path, format!("{header}{body}")).is_ok() {
        tracing::warn!("health[{}]: 0 results — body dumped to {:?}", engine, path);
    }
}

/// Record a successfully fetched + parsed page. `n == 0` flags selector rot.
pub fn record_parsed(engine: &str, n: usize) {
    with_engine(engine, |h| {
        h.http_ok.fetch_add(1, Ordering::Relaxed);
        h.results_total.fetch_add(n as u64, Ordering::Relaxed);
        if n == 0 {
            h.zero_results.fetch_add(1, Ordering::Relaxed);
        }
    });
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EngineHealthReport {
    pub engine: String,
    pub attempts: u64,
    pub http_ok: u64,
    pub blocked: u64,
    pub transport_err: u64,
    pub zero_results: u64,
    pub results_total: u64,
    pub avg_results: f64,
    /// One of: healthy | degraded | selector_rot | blocked | dead_host | unproven
    pub verdict: &'static str,
}

fn verdict(r: &EngineHealthReport) -> &'static str {
    if r.attempts < MIN_SAMPLE {
        return "unproven";
    }
    // Host does not resolve / refuses connections.
    if r.transport_err * 2 >= r.attempts {
        return "dead_host";
    }
    // Consistently challenged or rate-limited.
    if r.blocked * 2 >= r.attempts {
        return "blocked";
    }
    // Fetching fine, parsing nothing — our selectors are stale.
    if r.http_ok >= MIN_SAMPLE && r.results_total == 0 {
        return "selector_rot";
    }
    if r.avg_results >= 5.0 {
        "healthy"
    } else {
        "degraded"
    }
}

pub fn snapshot() -> Vec<EngineHealthReport> {
    let Ok(map) = REGISTRY.read() else {
        return Vec::new();
    };
    let mut out: Vec<EngineHealthReport> = map
        .iter()
        .map(|(engine, h)| {
            let attempts = h.attempts.load(Ordering::Relaxed);
            let results_total = h.results_total.load(Ordering::Relaxed);
            let http_ok = h.http_ok.load(Ordering::Relaxed);
            let mut r = EngineHealthReport {
                engine: engine.clone(),
                attempts,
                http_ok,
                blocked: h.blocked.load(Ordering::Relaxed),
                transport_err: h.transport_err.load(Ordering::Relaxed),
                zero_results: h.zero_results.load(Ordering::Relaxed),
                results_total,
                avg_results: if attempts == 0 {
                    0.0
                } else {
                    results_total as f64 / attempts as f64
                },
                verdict: "unproven",
            };
            r.verdict = verdict(&r);
            r
        })
        .collect();
    // Worst first — the actionable ones surface at the top.
    out.sort_by(|a, b| {
        a.avg_results
            .partial_cmp(&b.avg_results)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.engine.cmp(&b.engine))
    });
    out
}

/// Auto-quarantine: skip engines that have had a fair sample and produced
/// nothing. Prevents dead hosts from consuming the dispatch deadline.
///
/// Disabled by `ENGINE_QUARANTINE=off`.
pub fn should_skip(engine: &str) -> bool {
    if std::env::var("ENGINE_QUARANTINE").as_deref() == Ok("off") {
        return false;
    }
    let Ok(map) = REGISTRY.read() else {
        return false;
    };
    let Some(h) = map.get(engine) else {
        return false;
    };
    let attempts = h.attempts.load(Ordering::Relaxed);
    if attempts < MIN_SAMPLE {
        return false; // give every engine a fair trial
    }
    h.results_total.load(Ordering::Relaxed) == 0
}

/// Test-only reset.
#[cfg(test)]
pub fn reset() {
    if let Ok(mut map) = REGISTRY.write() {
        map.clear();
    }
}

// ============================================================================
// Soft-block detection — HTTP 200 bodies that are not search results
// ============================================================================

/// Markers for pages that return 200 but contain a bot challenge rather than
/// results. Every one of these was observed live on 2026-10-01.
const CHALLENGE_MARKERS: &[&str] = &[
    "enablejs",                      // Google JS interstitial
    "Select all squares containing", // DuckDuckGo CAPTCHA
    "unusual traffic",               // Google rate limit
    "/sorry/index",                  // Google soft ban
    "cf-browser-verification",       // Cloudflare
    "Just a moment...",              // Cloudflare interstitial
    "Checking your browser before",  // Cloudflare legacy
    "DDoS protection by",
    "Attention Required! | Cloudflare",
    "Please verify you are a human",
    "captcha-delivery.com",          // DataDome
    "Access to this page has been denied", // PerimeterX
    // Yandex redirects bots to /showcaptcha with HTTP 200; the generic
    // fallback selectors then returned its "SmartCaptcha by Yandex Cloud"
    // footer link as a search result (seen at rank 6 in Deep).
    "/checkcaptcha",
    "smartcaptcha",
];

/// True if the body looks like a bot challenge rather than a SERP.
///
/// Only the head of the document is scanned: challenge pages are small and
/// front-load their message, while real SERPs can embed these words deep
/// inside inline JS (observed on Brave and Bing, which is why a whole-body
/// scan produced false positives during the audit).
pub fn looks_like_challenge(body: &str) -> bool {
    let head = crate::util::truncate_bytes(body, 16_384);
    CHALLENGE_MARKERS.iter().any(|m| head.contains(m))
}

/// Title-level markers of block / interstitial pages that are served with
/// HTTP 200 on CONTENT pages (as opposed to SERPs).
const BLOCKED_PAGE_TITLES: &[&str] = &[
    "<title>Welcome to Reddit",       // Reddit's CAPTCHA interstitial
    "<title>Just a moment",           // Cloudflare
    "<title>Attention Required",      // Cloudflare
    "<title>Access Denied",           // Akamai / generic
    "<title>403 Forbidden",
    "<title>Robot or human?",         // PerimeterX / Walmart-style
    "<title>Are you a robot",
    "<title>Pardon Our Interruption", // Distil / Imperva
];

/// True if a SCRAPED page is a bot wall rather than content.
///
/// v8.0 only checked search-engine responses for challenges. Scraped pages
/// went straight to the extractor, so Reddit's "Welcome to Reddit" CAPTCHA
/// page — served with HTTP 200 — was extracted (3,186 chars, measured) and
/// passed to the LLM as if it were a source.
pub fn looks_like_blocked_page(html: &str) -> bool {
    if looks_like_challenge(html) {
        return true;
    }
    let head = crate::util::truncate_bytes(html, 16_384);
    let lower_head = head.to_ascii_lowercase();
    BLOCKED_PAGE_TITLES
        .iter()
        .any(|m| lower_head.contains(&m.to_ascii_lowercase()))
        || (lower_head.contains("<title>reddit") && lower_head.contains("captcha"))
        || lower_head.contains("enable javascript and cookies to continue")
}

/// Fewer results than this are not enough to judge a page.
const SOFT_BLOCK_MIN_RESULTS: usize = 5;
/// Below this share of on-topic results the whole page is filler.
const SOFT_BLOCK_ON_TOPIC_RATIO: f64 = 0.25;
/// Below this share the page is mixed: real results padded with filler.
const MIXED_PAGE_ON_TOPIC_RATIO: f64 = 0.6;

/// Index APIs that require every query term and never serve filler. Their
/// snippets show only some of the matched terms, so screening them by snippet
/// overlap produced false blocks ("definition of photosynthesis": every
/// Wikipedia hit was about photosynthesis, none said "definition").
const UNSCREENED_ENGINES: &[&str] = &["wikipedia", "openalex", "arxiv"];

/// What [`screen_serp`] did to a page of results.
#[derive(Debug, PartialEq)]
pub enum SerpVerdict {
    Clean,
    /// Filler results were removed; the count is how many.
    Trimmed(usize),
    /// The whole page ignored the query and was cleared.
    Blocked,
}

/// Screen a 200 OK result page for an engine's "soft block": filler that
/// ignores the query.
///
/// Measured 2026-10-02 (Bing, with both curl and the impersonating client):
/// "capital of France" returned Bhubaneswar and Odisha, "best budget phone
/// 2026" returned Mumbai's BEST bus company, and "mRNA vaccine mechanism"
/// returned pornography and Chinese Q&A pages about browser games. Page 2 was
/// byte-identical to page 1. None of it trips a challenge marker, so it went
/// straight into ranking, and two explicit sites reached Auto's top 6.
///
/// A real SERP mentions the query in most titles/snippets (engines bold the
/// matched words). Filler matches at most the query's FIRST word ("capital",
/// "best", "rust"), so a result is on-topic when it contains two or more
/// terms, or any term other than the first. ("definition of photosynthesis"
/// results that only say "photosynthesis" are on-topic; "capital of France"
/// results that only say "capital" are not.) Mostly-filler pages are cleared, mixed pages keep only on-topic
/// results, and healthy pages are left untouched so that a relevant result
/// with a terse title is not lost. Cross-script cases (a Hindi query answered
/// by English pages) cannot be judged by overlap and are left alone.
pub fn screen_serp(engine: &str, query: &str, results: &mut Vec<RawSearchResult>) -> SerpVerdict {
    if UNSCREENED_ENGINES.contains(&engine) {
        return SerpVerdict::Clean;
    }
    let Some(flags) = on_topic_flags(query, results) else { return SerpVerdict::Clean };
    let on_topic = flags.iter().filter(|f| **f).count();
    let ratio = on_topic as f64 / results.len() as f64;
    if ratio < SOFT_BLOCK_ON_TOPIC_RATIO {
        results.clear();
        SerpVerdict::Blocked
    } else if ratio < MIXED_PAGE_ON_TOPIC_RATIO {
        let dropped = results.len() - on_topic;
        let mut keep = flags.into_iter();
        results.retain(|_| keep.next().unwrap_or(true));
        SerpVerdict::Trimmed(dropped)
    } else {
        SerpVerdict::Clean
    }
}

/// Per-result on-topic flags, or None when the page cannot be judged.
fn on_topic_flags(query: &str, results: &[RawSearchResult]) -> Option<Vec<bool>> {
    if results.len() < SOFT_BLOCK_MIN_RESULTS {
        return None;
    }
    // Operators such as site: are not content terms.
    let content: Vec<&str> = query.split_whitespace().filter(|w| !w.contains(':')).collect();
    let terms: Vec<String> = crate::ranking::query_terms(&content.join(" "))
        .into_iter()
        .map(|t| stem(&t))
        .collect();
    if terms.is_empty() || !terms.iter().all(|t| t.is_ascii()) {
        return None;
    }
    Some(
        results
            .iter()
            .map(|r| {
                let url = urlencoding::decode(&r.url).map(|u| u.into_owned()).unwrap_or_default();
                let text = format!("{} {} {}", r.title, r.snippet, url).to_lowercase();
                let hits: Vec<usize> =
                    (0..terms.len()).filter(|&i| text.contains(terms[i].as_str())).collect();
                hits.len() >= 2 || hits.iter().any(|&i| i > 0) || (terms.len() == 1 && hits.len() == 1)
            })
            .collect(),
    )
}

/// Crude suffix trim so "vaccines" matches "vaccine" and "phones" "phone".
fn stem(term: &str) -> String {
    let n = term.chars().count();
    if n >= 6 {
        term.chars().take(n - 2).collect()
    } else {
        term.trim_end_matches('s').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(title: &str, snippet: &str, url: &str) -> RawSearchResult {
        RawSearchResult {
            url: url.into(),
            title: title.into(),
            snippet: snippet.into(),
            engine: "bing".into(),
            rank_position: 1,
            published: None,
            full_text: None,
        }
    }

    #[test]
    fn yandex_captcha_page_is_a_challenge() {
        let page = r#"<html><head><title>Are you not a robot?</title></head><body>
            <form method="POST" action="/checkcaptcha?key=abc"><div class="CheckboxCaptcha"></div></form>
            <a href="https://yandex.cloud/en/services/smartcaptcha?utm_source=captcha">SmartCaptcha</a></body></html>"#;
        assert!(looks_like_challenge(page));
    }

    #[test]
    fn soft_block_detects_first_word_filler() {
        // Real Bing output for "capital of France" (2026-10-02).
        let filler = vec![
            res("Bhubaneswar - Wikipedia", "Bhubaneswar is the capital and largest city of Odisha", "https://en.wikipedia.org/wiki/Bhubaneswar"),
            res("Odisha - Wikipedia", "Odisha is a state in eastern India", "https://en.wikipedia.org/wiki/Odisha"),
            res("Bhubaneswar - Odisha Tourism", "Temple city and capital", "https://odishatourism.gov.in/x"),
            res("Online Trading | Capital.com", "Trade CFDs", "https://capital.com/"),
            res("Capital of Odisha - Vedantu", "Capital of Odisha facts and map", "https://www.vedantu.com/x"),
            res("Capital of Odisha, What is the Capital? - adda247", "", "https://www.adda247.com/x"),
        ];
        let mut page = filler;
        assert_eq!(screen_serp("bing", "capital of France", &mut page), SerpVerdict::Blocked);
        assert!(page.is_empty());
    }

    #[test]
    fn soft_block_detects_unrelated_junk() {
        let junk: Vec<RawSearchResult> = (0..8)
            .map(|i| res(&format!("Download and install Google Chrome {i}"), "Chrome help", "https://support.google.com/chrome"))
            .collect();
        assert_eq!(screen_serp("bing", "mRNA vaccine mechanism", &mut junk.clone()), SerpVerdict::Blocked);
        assert_eq!(screen_serp("bing", "mRNA", &mut junk.clone()), SerpVerdict::Blocked);
    }

    #[test]
    fn soft_block_keeps_pages_matching_only_the_topic_word() {
        // Regression: a "≥2 terms" rule blocked this real page.
        let mut page: Vec<RawSearchResult> = (0..8)
            .map(|i| res(&format!("Photosynthesis {i}"), "Plants convert light into chemical energy", "https://example.org/p"))
            .collect();
        assert_eq!(screen_serp("bing", "definition of photosynthesis", &mut page), SerpVerdict::Clean);
    }

    #[test]
    fn curated_index_apis_are_not_screened() {
        let mut junk: Vec<RawSearchResult> = (0..8).map(|i| res(&format!("Unrelated {i}"), "", "https://a.test/")).collect();
        assert_eq!(screen_serp("wikipedia", "capital of France", &mut junk), SerpVerdict::Clean);
        assert_eq!(junk.len(), 8);
    }

    #[test]
    fn soft_block_trims_mixed_pages() {
        // Real results padded with first-word filler: keep the real ones.
        let mut page = vec![
            res("10 Best Budget Phones in 2026", "", "https://a.test/1"),
            res("Best cheap phones 2026", "", "https://a.test/2"),
            res("Budget phone buying guide", "", "https://a.test/3"),
            res("BEST Undertaking - Mumbai buses", "", "https://best.test/1"),
            res("BEST Online Payment", "", "https://best.test/2"),
            res("BEST Online Payment", "", "https://best.test/3"),
        ];
        assert_eq!(screen_serp("bing", "best budget phone 2026", &mut page), SerpVerdict::Trimmed(3));
        assert_eq!(page.len(), 3);
        assert!(page.iter().all(|r| r.url.starts_with("https://a.test")));
    }

    #[test]
    fn soft_block_accepts_real_serps() {
        let real = vec![
            res("Paris - Wikipedia", "Paris is the capital and largest city of France", "https://en.wikipedia.org/wiki/Paris"),
            res("List of capitals of France - Wikipedia", "", "https://en.wikipedia.org/wiki/List_of_capitals_of_France"),
            res("What is the Capital of France? - WorldAtlas", "", "https://www.worldatlas.com/x"),
            res("France | History, Map, Flag, Capital | Britannica", "", "https://www.britannica.com/place/France"),
            res("Paris travel guide", "Everything to see in Paris", "https://example.com/paris"),
            res("Capital punishment in France", "", "https://en.wikipedia.org/wiki/Capital_punishment_in_France"),
        ];
        assert_eq!(screen_serp("bing", "capital of France", &mut real.clone()), SerpVerdict::Clean);

        // Plurals and inflections still count.
        let phones: Vec<RawSearchResult> = (0..6)
            .map(|i| res(&format!("10 Best Budget Phones {i}"), "cheap smartphones reviewed", "https://example.com/p"))
            .collect();
        assert_eq!(screen_serp("bing", "best budget phone 2026", &mut phones.clone()), SerpVerdict::Clean);
    }

    #[test]
    fn soft_block_leaves_operators_small_pages_and_other_scripts_alone() {
        let reddit: Vec<RawSearchResult> = (0..6)
            .map(|i| res(&format!("Best mechanical keyboard for coding? {i}"), "", "https://www.reddit.com/r/x"))
            .collect();
        assert_eq!(screen_serp("bing", "mechanical keyboard site:reddit.com", &mut reddit.clone()), SerpVerdict::Clean);

        let few = vec![res("Unrelated", "", "https://a.test/"), res("Also unrelated", "", "https://b.test/")];
        assert_eq!(screen_serp("bing", "capital of France", &mut few.clone()), SerpVerdict::Clean);

        let english: Vec<RawSearchResult> = (0..6)
            .map(|i| res(&format!("India - Wikipedia {i}"), "New Delhi is the capital", "https://en.wikipedia.org/wiki/India"))
            .collect();
        assert_eq!(screen_serp("bing", "भारत की राजधानी", &mut english.clone()), SerpVerdict::Clean);
    }

    #[test]
    fn detects_reddit_interstitial_but_not_real_threads() {
        let wall = "<html><head><title>Welcome to Reddit</title></head><body>\
                    <div class='captcha'>verify</div></body></html>";
        assert!(looks_like_blocked_page(wall));
        let thread = "<html><head><title>Rust 1.80 released : r/rust</title></head>\
                      <body><div class='commentarea'>Great release!</div></body></html>";
        assert!(!looks_like_blocked_page(thread));
        // An article that merely MENTIONS a CAPTCHA is not a wall.
        let article = "<html><head><title>How CAPTCHAs work - Wired</title></head>\
                       <body>A captcha is a test...</body></html>";
        assert!(!looks_like_blocked_page(article));
        assert!(looks_like_blocked_page("<html><head><TITLE>Access Denied</TITLE></head></html>"));
    }

    #[test]
    fn detects_google_enablejs_interstitial() {
        let body = r#"<!DOCTYPE html><html><head><title>Google Search</title>
            <style>table,div,span,p{display:none}</style>
            <script>enablejs</script>Please click here if you are not redirected"#;
        assert!(looks_like_challenge(body));
    }

    #[test]
    fn detects_duckduckgo_captcha() {
        let body = "<html><body>Unfortunately, bots use DuckDuckGo too. \
                    Select all squares containing a duck:</body></html>";
        assert!(looks_like_challenge(body));
    }

    #[test]
    fn does_not_flag_a_real_serp() {
        let body = format!(
            "<html><body>{}</body></html>",
            "<div class=\"snippet\"><a href=\"https://example.com\">Result</a></div>".repeat(40)
        );
        assert!(!looks_like_challenge(&body));
    }

    #[test]
    fn only_scans_head_so_deep_inline_js_does_not_false_positive() {
        // A real SERP with the word "captcha" in a script 100KB down.
        let mut body = String::from("<html><body>");
        body.push_str(&"<div class=\"result\">ok</div>".repeat(5000));
        body.push_str("<script>var x='Just a moment...';</script></body></html>");
        assert!(body.len() > 16_384);
        assert!(!looks_like_challenge(&body));
    }

    #[test]
    fn challenge_scan_is_utf8_safe() {
        // Devanagari body longer than the 16KB scan window — must not panic.
        let body = "क".repeat(20_000);
        assert!(!looks_like_challenge(&body));
    }

    #[test]
    fn quarantine_needs_a_sample_before_benching() {
        reset();
        for _ in 0..3 {
            record_attempt("trial");
            record_parsed("trial", 0);
        }
        assert!(!should_skip("trial"), "3 attempts is too few to judge");
        for _ in 0..10 {
            record_attempt("trial");
            record_parsed("trial", 0);
        }
        assert!(should_skip("trial"), "13 attempts with 0 results => quarantine");
    }

    #[test]
    fn productive_engine_is_never_quarantined() {
        reset();
        for _ in 0..20 {
            record_attempt("good");
            record_parsed("good", 12);
        }
        assert!(!should_skip("good"));
        let snap = snapshot();
        let r = snap.iter().find(|r| r.engine == "good").unwrap();
        assert_eq!(r.verdict, "healthy");
    }

    #[test]
    fn classifies_selector_rot_vs_dead_host() {
        reset();
        for _ in 0..12 {
            record_attempt("rotted");
            record_parsed("rotted", 0); // 200 OK, nothing parsed
        }
        for _ in 0..12 {
            record_attempt("gone");
            record_transport_err("gone"); // never connected
        }
        let snap = snapshot();
        let rot = snap.iter().find(|r| r.engine == "rotted").unwrap();
        let gone = snap.iter().find(|r| r.engine == "gone").unwrap();
        assert_eq!(rot.verdict, "selector_rot");
        assert_eq!(gone.verdict, "dead_host");
    }
}
