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

#[cfg(test)]
mod tests {
    use super::*;

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
