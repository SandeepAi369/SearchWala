// ============================================================================
// SearchWala v7.0.0 - Search Engines Module
// Tiered engines: primary (fast) + backup (reliable) + domain-specialized
// Smart fallback: if primary engines fail, instantly switch to backups
// ============================================================================

pub mod bing;
pub mod brave;
pub mod duckduckgo;
pub mod generic;
pub mod health;
pub mod marginalia_api;
pub mod mojeek;
pub mod qwant;
pub mod startpage;
pub mod wiby;
pub mod wikipedia;
pub mod yahoo;

use std::sync::Arc;

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

#[async_trait::async_trait]
pub trait SearchEngine: Send + Sync {
    fn name(&self) -> &str;

    /// True if this engine sits behind a WAF/anti-bot and must be fetched with
    /// TLS impersonation. Measured on 2026-10-01: with plain reqwest, Brave
    /// returned 429, DuckDuckGo a CAPTCHA, and Bing its homepage; with wreq all
    /// three returned real result pages.
    fn needs_stealth(&self) -> bool {
        false
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult>;
}

/// Build engine instances as `Arc` so `dispatch_engines` can clone them into
/// spawned tasks.
///
/// v7.0 called `get_engines(&[name])` *inside every spawned task*, which
/// re-ran `generic::spec_for()` — and that function used `Box::leak` to
/// manufacture `&'static str` templates for Google variants. Every Google
/// alias, on every query variation, on every search, leaked a String that was
/// never freed: an unbounded leak proportional to search volume.
pub fn get_engines(enabled: &[String]) -> Vec<Arc<dyn SearchEngine>> {
    let mut engines: Vec<Arc<dyn SearchEngine>> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for name in enabled {
        // Collapse duplicate aliases that resolve to the same implementation.
        // v7.0 mapped `brave` and `brave_news` to the SAME Brave struct, and
        // `yahoo`/`yahoo_news` to the same Yahoo struct — the live run logged
        // "yahoo: 28" twice, double-counting one engine in RRF consensus.
        match name.as_str() {
            "duckduckgo" | "duckduckgo_html" => {
                if seen.insert("duckduckgo".to_string()) {
                    engines.push(Arc::new(duckduckgo::DuckDuckGo));
                }
            }
            "brave" => {
                if seen.insert("brave".to_string()) {
                    engines.push(Arc::new(brave::Brave));
                }
            }
            // Dedicated Bing parser — the generic link-scraper cannot handle
            // Bing's `h2`-without-anchor title markup (see engines/bing.rs).
            n if n.starts_with("bing") && !n.contains("_images") && !n.contains("_videos") => {
                // `seen` owns its keys — do NOT Box::leak here (that was the
                // P0-3 unbounded-leak bug).
                if seen.insert(n.to_string()) {
                    engines.push(Arc::new(bing::Bing::new(n)));
                }
            }
            "yahoo" => {
                if seen.insert("yahoo".to_string()) {
                    engines.push(Arc::new(yahoo::Yahoo));
                }
            }
            "qwant" => {
                if seen.insert("qwant".to_string()) {
                    engines.push(Arc::new(qwant::Qwant));
                }
            }
            "mojeek" => {
                if seen.insert("mojeek".to_string()) {
                    engines.push(Arc::new(mojeek::Mojeek));
                }
            }
            "startpage" => {
                if seen.insert("startpage".to_string()) {
                    engines.push(Arc::new(startpage::Startpage));
                }
            }
            "wikipedia" => {
                if seen.insert("wikipedia".to_string()) {
                    engines.push(Arc::new(wikipedia::Wikipedia));
                }
            }
            "wiby" => {
                if seen.insert("wiby".to_string()) {
                    engines.push(Arc::new(wiby::Wiby));
                }
            }
            "marginalia" | "marginalia_api" => {
                if seen.insert("marginalia_api".to_string()) {
                    engines.push(Arc::new(marginalia_api::MarginaliaApi));
                }
            }
            other => {
                if let Some(spec) = generic::spec_for(other) {
                    engines.push(Arc::new(generic::GenericEngine::new(other, spec)));
                } else {
                    tracing::warn!("Unknown engine (no spec): {}", other);
                }
            }
        }
    }

    engines
}

// =============================================================================
// Engine Tiers — Primary (fast, reliable) vs Backup (slower, but always works)
// =============================================================================

/// Primary fast engines — these respond in 1-3s and rarely fail.
///
/// v8.0: pruned against measured liveness (2026-10-01). Engines whose hosts do
/// not resolve or whose endpoints 404 are no longer listed — they only burned
/// dispatch deadline.
pub fn primary_engines() -> Vec<String> {
    vec![
        "wikipedia",      // JSON API, always works
        "marginalia_api", // JSON API, verified live, no key
        "duckduckgo",     // needs stealth
        "brave",          // needs stealth
        "bing",           // needs stealth
        "yahoo",          // needs stealth
        "google",         // needs stealth (still JS-walled; kept for when it works)
        "mojeek",
        "startpage",
        "qwant",
        "wiby",
    ]
    .into_iter()
    .map(|s| s.to_string())
    .collect()
}

/// Secondary engines — broader coverage, less reliable.
pub fn backup_engines() -> Vec<String> {
    vec![
        // Verticals with genuinely distinct endpoints.
        "bing_news", "google_news", "brave_news", "yahoo_news",
        // Regional variants — now carry real market/locale params (P0-8a).
        "bing_uk", "bing_in", "bing_de", "google_uk", "google_in", "google_de",
        // Independents that returned results in the audit.
        "yandex", "mwmbl", "ecosia",
        // 200-but-selector-dependent; health registry will quarantine if dead.
        "4get", "yacy",
        // REMOVED (measured dead 2026-10-01): alexandria, excite, lycos,
        // mullvad_leta, pipilika, searchencrypt (no DNS/refused);
        // whoogle 502, librex 404, right_dao 503, ask 404, stract 404;
        // dogpile/webcrawler/info/searchalot/aol/gibiru/kiddle (JS+token walls).
    ]
    .into_iter()
    .map(|s| s.to_string())
    .collect()
}

/// ALL engines dispatched simultaneously — maximum coverage, no waiting.
/// Quarantined engines are filtered out by the caller via `health::should_skip`.
pub fn all_engines() -> Vec<String> {
    let mut all = primary_engines();
    all.extend(backup_engines());
    all
}

/// Drop engines the health registry has benched (P1-1 auto-quarantine).
pub fn filter_healthy(names: Vec<String>) -> Vec<String> {
    let before = names.len();
    let kept: Vec<String> = names
        .into_iter()
        .filter(|n| {
            if health::should_skip(n) {
                tracing::debug!("Skipping quarantined engine: {}", n);
                false
            } else {
                true
            }
        })
        .collect();
    if kept.len() < before {
        tracing::info!(
            "Engine quarantine: {} of {} engines skipped (0 results over their sample)",
            before - kept.len(),
            before
        );
    }
    kept
}

/// Domain-specialized engine sets for Specialized Mode
pub fn specialized_engines(domain: &str) -> Vec<String> {
    let base: Vec<&str> = match domain {
        "tech" => vec![
            "duckduckgo", "brave", "google", "bing", "startpage", "qwant",
            "google_news", "bing_news", "marginalia", "stract", "wiby",
        ],
        "science" => vec![
            "wikipedia", "google", "google_scholar", "brave", "duckduckgo",
            "startpage", "mojeek", "bing", "ecosia", "qwant",
        ],
        "finance" => vec![
            "google", "google_news", "bing", "bing_news", "yahoo", "yahoo_news",
            "brave", "brave_news", "duckduckgo", "startpage",
        ],
        "health" => vec![
            "google", "wikipedia", "bing", "duckduckgo", "brave",
            "google_scholar", "mojeek", "startpage", "ecosia", "qwant",
        ],
        "news" => vec![
            "google_news", "bing_news", "brave_news", "yahoo_news",
            "duckduckgo_news", "google", "bing", "brave", "yahoo", "duckduckgo",
        ],
        _ => vec![
            "wikipedia", "duckduckgo", "brave", "google", "bing",
            "yahoo", "qwant", "startpage", "mojeek", "ecosia",
        ],
    };
    base.into_iter().map(|s| s.to_string()).collect()
}

// =============================================================================
// Engine Families — independent evidence groups for RRF
//
// WHY THIS MATTERS
// RRF's entire premise is that agreement between INDEPENDENT rankers is a
// quality signal. v7.0 violated that badly:
//
//   * 15 Bing aliases (bing, bing_us, bing_uk, bing_in, ...) all built the
//     SAME URL — spec_for ignored the suffix and never added a market param.
//     RRF therefore counted Bing's ordering 15 times and called it "consensus".
//   * Startpage proxies Google; Yahoo and DuckDuckGo are Bing-derived. Treating
//     a Google proxy as independent evidence *for* Google's ranking is the same
//     error, just less obvious.
//
// Collapsing to families makes "found by N engines" mean what it claims.
// =============================================================================

/// The independent-evidence group an engine belongs to.
pub fn engine_family(name: &str) -> &'static str {
    // Order matters: check the most specific prefixes first.
    if name.starts_with("google") {
        return "google";
    }
    if name.starts_with("startpage") || name.starts_with("ecosia") {
        return "google"; // both proxy Google's index
    }
    if name.starts_with("bing") {
        return "bing";
    }
    if name.starts_with("duckduckgo") || name.starts_with("yahoo") {
        return "bing"; // both Bing-derived
    }
    if name.starts_with("brave") {
        return "brave";
    }
    if name.starts_with("yandex") {
        return "yandex";
    }
    if name.starts_with("mojeek") {
        return "mojeek";
    }
    if name.starts_with("marginalia") {
        return "marginalia";
    }
    if name.starts_with("wikipedia") {
        return "wikipedia";
    }
    if name.starts_with("qwant") {
        return "qwant";
    }
    if name.starts_with("mwmbl") {
        return "mwmbl";
    }
    if name.starts_with("wiby") {
        return "wiby";
    }
    "indie"
}

/// Reliability weight per FAMILY (multiplies the RRF contribution).
///
/// Weights are per-family rather than per-alias so a regional variant cannot
/// inflate its parent's score.
pub fn family_weight(family: &str) -> f32 {
    match family {
        // Large, high-precision indices.
        "google" | "bing" => 1.5,
        // Curated / authoritative.
        "wikipedia" => 1.5,
        // Independent indices with real coverage.
        "brave" => 1.4,
        "yandex" | "mojeek" => 1.1,
        "qwant" => 1.1,
        // Independent but narrow — valuable for diversity, weaker on precision.
        "marginalia" | "mwmbl" | "wiby" => 0.9,
        _ => 1.0,
    }
}

/// Backwards-compatible per-engine weight, resolved through the family table.
pub fn engine_weight(name: &str) -> f32 {
    family_weight(engine_family(name))
}
