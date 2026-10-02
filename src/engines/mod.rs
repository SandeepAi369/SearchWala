// ============================================================================
// SearchWala v7.0.0 - Search Engines Module
// Tiered engines: primary (fast) + backup (reliable) + domain-specialized
// Smart fallback: if primary engines fail, instantly switch to backups
// ============================================================================

pub mod arxiv;
pub mod bing;
pub mod brave;
pub mod duckduckgo;
pub mod generic;
pub mod health;
pub mod marginalia_api;
pub mod mojeek;
pub mod openalex;
pub mod qwant;
pub mod searxng;
pub mod startpage;
pub mod wiby;
pub mod wikipedia;
pub mod yahoo;

use std::sync::Arc;

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

// ─────────────────────────────────────────────────────────────────────────────
// Per-search SERP page budget
//
// Engines paginate with fixed constants (Bing 3, Brave 3, Yahoo 3, ...). When
// a mode's deadline is shorter than an engine's full pagination, the dispatch
// aborts the task and every page it had ALREADY fetched is dropped with it —
// measured in v8.0 Fast mode: 2 of 9 tasks aborted at 2.5 s, contributing
// nothing. Each mode now sets a page budget that engines honour, so Fast asks
// for one page and gets it rather than asking for three and getting zero.
// ─────────────────────────────────────────────────────────────────────────────

tokio::task_local! {
    static PAGE_BUDGET: usize;
}

/// Pages this engine may fetch: its own default, capped by the current
/// search's budget. Outside a budgeted scope the default applies unchanged.
pub fn page_budget(default: usize) -> usize {
    PAGE_BUDGET
        .try_with(|b| (*b).max(1).min(default.max(1)))
        .unwrap_or(default)
}

/// Run `fut` with a page budget in scope.
pub async fn with_page_budget<F: std::future::Future>(budget: usize, fut: F) -> F::Output {
    PAGE_BUDGET.scope(budget.max(1), fut).await
}

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
            "searxng" => {
                if seen.insert("searxng".to_string()) {
                    engines.push(Arc::new(searxng::SearxNG));
                }
            }
            "openalex" => {
                if seen.insert("openalex".to_string()) {
                    engines.push(Arc::new(openalex::OpenAlex));
                }
            }
            "arxiv" => {
                if seen.insert("arxiv".to_string()) {
                    engines.push(Arc::new(arxiv::Arxiv));
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
        // ── JSON APIs: reliable, no scraping, no WAF ──
        "wikipedia",      // full-text search API (v8.0 fix)
        "marginalia_api", // public JSON API, no key
        "wiby",           // indie JSON API
        // ── HTML, stealth-fetched, verified working 2026-10-01 ──
        "bing",           // 86 results/query
        "brave",          // via brave_news on the same family
        "duckduckgo",     // 80 results/query
        "yahoo",          // 63 results/query
        // ── IP-sensitive: 403/303 here, may work elsewhere. The health
        //    registry quarantines them automatically if they stay dead. ──
        "mojeek",
        "yandex",
    ]
    .into_iter()
    .map(|s| s.to_string())
    .collect()
}

/// Secondary engines — broader coverage, less reliable.
pub fn backup_engines() -> Vec<String> {
    vec![
        // Verticals with genuinely distinct endpoints and verified parsers.
        "bing_news", "brave_news",
        // Regional Bing variants — real market params, one page each.
        "bing_uk", "bing_in", "bing_de",
        // Independents worth trying; quarantine handles them if dead.
        "mwmbl", "ecosia", "yacy",
    ]
    .into_iter()
    .map(|s| s.to_string())
    .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// REMOVED FROM THE DEFAULT ROSTER (v8.0)
//
// These are STRUCTURALLY broken, not merely IP-blocked, so auto-quarantine
// would only discover that after wasting dispatch deadline on every search.
// Their specs are retained so `ENGINES=...` can still force them for anyone
// with a working path (a proxy, a self-hosted instance, a residential IP).
//
// Evidence, all measured 2026-10-01:
//
//   google, google_news, google_uk, google_in, google_de
//     HTTP 200 but an `enablejs` JS interstitial with
//     `table,div,span,p{display:none}` and ZERO result links. Retested with
//     `CONSENT=YES+...`, `SOCS=...`, `gbv=1` and `hl/gl` — every variant still
//     returned enablejs, 0 `/url?q=`, 0 `<h3>`. Google now requires full JS
//     execution; there is no HTML path.
//
//   startpage
//     Serves a consent/submit page, not a SERP: 19 anchors, all pointing at
//     startpage.com, plus a "Submit" button. The POST flow needs a token the
//     page itself mints.
//
//   qwant (incl. lite.qwant.com)
//     118 KB of JavaScript with ZERO server-rendered anchors — not even
//     `a[href^='http']` matched. A pure SPA shell.
//
//   4get
//     The 4get.ca instance is DOWN; the page links to
//     bsky.app/profile/4getisdown.bsky.social.
//
//   yahoo_news
//     JS-rendered; only a single `.NewsArticle` wrapper and one self-link are
//     present server-side. Plain `yahoo` covers the family at 63 results.
//
// Previously removed in the Phase 1 pass (no DNS / 404 / dead instance):
//   alexandria, excite, lycos, mullvad_leta, pipilika, searchencrypt,
//   whoogle, librex, right_dao, ask, stract, dogpile, webcrawler, info,
//   searchalot, aol, gibiru, kiddle, presearch, yep.
// ─────────────────────────────────────────────────────────────────────────────

/// ALL engines dispatched simultaneously — maximum coverage, no waiting.
/// Quarantined engines are filtered out by the caller via `health::should_skip`.
pub fn all_engines() -> Vec<String> {
    // `ENGINES` replaces the general web pool (used by Auto's standard/heavy
    // tiers and by Deep). Mode-specific rosters (Fast, Study, Academic,
    // Reddit, YouTube) are deliberate and unaffected.
    if let Some(v) = crate::config::engines_override() {
        return v;
    }
    let mut all = primary_engines();
    all.extend(backup_engines());
    // Optional upstream: only when the operator has configured an instance.
    if searxng::is_enabled() {
        all.insert(0, "searxng".to_string());
    }
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
    // SearXNG federates upstream engines. A hit it sourced from Bing must land
    // in the "bing" family, or federating would silently double-count the same
    // index alongside our own Bing results.
    if let Some(upstream) = name.strip_prefix("searxng:") {
        let fam = engine_family(upstream);
        // Unknown upstream => its own family, not "indie" lumped with others.
        return if fam == "indie" { "searxng" } else { fam };
    }
    if name == "searxng" {
        return "searxng";
    }
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
    if name.starts_with("openalex") {
        return "openalex";
    }
    if name.starts_with("arxiv") {
        return "arxiv";
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
        // Scholarly indices: authoritative but topically narrow.
        "openalex" => 1.2,
        "arxiv" => 1.0,
        _ => 1.0,
    }
}

/// Backwards-compatible per-engine weight, resolved through the family table.
pub fn engine_weight(name: &str) -> f32 {
    family_weight(engine_family(name))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_engine_is_constructible() {
        // Guards against a roster entry with no matching spec, which v7.0
        // handled by logging "Unknown engine" and silently dispatching nothing.
        for name in all_engines() {
            let built = get_engines(&[name.clone()]);
            assert_eq!(built.len(), 1, "roster entry {name:?} produced no engine");
            assert!(!built[0].name().is_empty());
        }
    }

    #[test]
    fn duplicate_aliases_collapse_to_one_instance() {
        // v7.0 mapped brave+brave_news and yahoo+yahoo_news to the SAME struct,
        // then counted each twice in RRF (the live log showed "yahoo: 28" twice).
        let built = get_engines(&[
            "duckduckgo".into(),
            "duckduckgo_html".into(), // same endpoint
            "wikipedia".into(),
            "wikipedia".into(),       // literal duplicate
        ]);
        let names: Vec<&str> = built.iter().map(|e| e.name()).collect();
        assert_eq!(built.len(), 2, "aliases did not collapse: {names:?}");
    }

    #[test]
    fn structurally_broken_engines_are_not_in_the_default_roster() {
        let roster = all_engines();
        for dead in [
            "google", "google_news", "google_uk", "google_in", "google_de",
            "startpage", "qwant", "4get", "yahoo_news",
        ] {
            assert!(
                !roster.contains(&dead.to_string()),
                "{dead} is JS-walled/dead and must not be dispatched by default"
            );
        }
    }

    #[test]
    fn removed_engines_can_still_be_forced_explicitly() {
        // Specs are retained so ENGINES=google works for anyone with a path.
        for forced in ["google", "startpage", "qwant"] {
            let built = get_engines(&[forced.to_string()]);
            assert_eq!(built.len(), 1, "{forced} should still be constructible on request");
        }
    }

    #[test]
    fn waf_engines_declare_needs_stealth() {
        for name in ["bing", "brave", "duckduckgo", "yahoo"] {
            let built = get_engines(&[name.to_string()]);
            assert!(built[0].needs_stealth(), "{name} must use TLS impersonation");
        }
        // JSON APIs do not need it.
        for name in ["wikipedia", "marginalia_api", "wiby"] {
            let built = get_engines(&[name.to_string()]);
            assert!(!built[0].needs_stealth(), "{name} should not need stealth");
        }
    }

    #[test]
    fn engine_family_groups_derived_indices_correctly() {
        // Bing-derived.
        for e in ["bing", "bing_uk", "bing_news", "duckduckgo", "yahoo", "yahoo_news"] {
            assert_eq!(engine_family(e), "bing", "{e}");
        }
        // Google-derived (incl. proxies).
        for e in ["google", "google_in", "startpage", "ecosia"] {
            assert_eq!(engine_family(e), "google", "{e}");
        }
        // Genuinely independent.
        assert_eq!(engine_family("brave"), "brave");
        assert_eq!(engine_family("brave_news"), "brave");
        assert_eq!(engine_family("marginalia_api"), "marginalia");
        assert_eq!(engine_family("yandex"), "yandex");
        assert_eq!(engine_family("mojeek"), "mojeek");
    }

    #[test]
    fn default_roster_has_enough_independent_families() {
        let fams: std::collections::HashSet<&str> =
            all_engines().iter().map(|e| engine_family(e)).collect();
        assert!(
            fams.len() >= 6,
            "only {} independent families in the roster: {fams:?}",
            fams.len()
        );
    }
}
