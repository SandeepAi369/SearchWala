// ══════════════════════════════════════════════════════════════════════════════
// Generic HTML search engine driver
//
// v8.0 changes:
//   * `Box::leak` removed — specs own their template via Cow (P0-3). The old
//     code leaked a String per Google alias per variation per search, forever.
//   * Pagination is explicit via `PageParam` (P0-6). v7.0 had three bugs:
//       - ~25 engines declared pages:2 with no {offset}/{page} placeholder, so
//         they fetched the identical URL twice (~120 wasted requests/search).
//       - `page_idx + 1` skipped page 1 on 0-indexed engines (yandex, ecosia).
//       - Brave's `offset` is a PAGE index, not a row offset.
//   * HTTP status is checked before parsing (P0-2). v7.0 parsed 429/403/202
//     CAPTCHA pages as if they were result pages.
//   * `rank_position` is global across pages (P0-7) — it previously reset to 1
//     on every page, destroying the rank signal RRF depends on.
//   * Regional aliases carry real market/locale params (P0-8a) instead of all
//     resolving to one identical URL.
// ══════════════════════════════════════════════════════════════════════════════

use std::borrow::Cow;

use scraper::{Html, Selector};

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

/// How an engine expresses "page N" in its query string.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PageParam {
    /// No pagination support — exactly one request.
    None,
    /// Row offset in tens, 0-based: 0, 10, 20 (google, ddg html).
    Offset10,
    /// Row offset in tens, 1-based: 1, 11, 21 (bing, yahoo, mojeek).
    Offset1Based,
    /// Page index, 0-based: 0, 1, 2 (yandex, ecosia, brave).
    PageZero,
    /// Page index, 1-based: 1, 2, 3 (sogou, startpage).
    PageOne,
}

#[derive(Clone, Debug)]
pub struct GenericEngineSpec {
    pub endpoint_template: Cow<'static, str>,
    pub pages: usize,
    pub page_param: PageParam,
    /// Engine sits behind a WAF — fetch with TLS impersonation.
    pub stealth: bool,
}

impl GenericEngineSpec {
    fn stat(template: &'static str, pages: usize, page_param: PageParam) -> Self {
        Self { endpoint_template: Cow::Borrowed(template), pages, page_param, stealth: false }
    }

    fn stat_stealth(template: &'static str, pages: usize, page_param: PageParam) -> Self {
        Self { endpoint_template: Cow::Borrowed(template), pages, page_param, stealth: true }
    }

    fn owned_stealth(template: String, pages: usize, page_param: PageParam) -> Self {
        Self { endpoint_template: Cow::Owned(template), pages, page_param, stealth: true }
    }

    /// A template with no placeholder cannot paginate — forcing 1 here is what
    /// eliminates the duplicate-request class of bug.
    pub fn effective_pages(&self) -> usize {
        if self.page_param == PageParam::None {
            1
        } else {
            self.pages.max(1)
        }
    }

    pub fn url_for_page(&self, encoded_query: &str, page_idx: usize) -> String {
        let value = match self.page_param {
            PageParam::None => 0,
            PageParam::Offset10 => page_idx * 10,
            PageParam::Offset1Based => page_idx * 10 + 1,
            PageParam::PageZero => page_idx,
            PageParam::PageOne => page_idx + 1,
        };
        let v = value.to_string();
        self.endpoint_template
            .replace("{query}", encoded_query)
            .replace("{offset}", &v)
            .replace("{page}", &v)
    }
}

pub struct GenericEngine {
    name: String,
    spec: GenericEngineSpec,
}

impl GenericEngine {
    pub fn new(name: &str, spec: GenericEngineSpec) -> Self {
        Self { name: name.to_string(), spec }
    }
}

#[async_trait::async_trait]
impl super::SearchEngine for GenericEngine {
    fn name(&self) -> &str {
        &self.name
    }

    fn needs_stealth(&self) -> bool {
        self.spec.stealth
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        let mut results = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let encoded = urlencoding::encode(query).to_string();

        // Global, 1-based rank across all pages (P0-7).
        let mut rank_base = 0usize;

        for page_idx in 0..self.spec.effective_pages() {
            let url = self.spec.url_for_page(&encoded, page_idx);
            super::health::record_attempt(&self.name);

            let page = match f.get(&url, &[]).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!("{} transport error for {}: {}", self.name, url, e);
                    super::health::record_transport_err(&self.name);
                    break; // host unreachable — later pages will fail too
                }
            };

            if page.status != 200 {
                tracing::warn!("{}: HTTP {} — not parsing", self.name, page.status);
                super::health::record_block(&self.name, page.status);
                break;
            }
            if super::health::looks_like_challenge(&page.body) {
                tracing::warn!("{}: bot challenge in 200 response — not parsing", self.name);
                super::health::record_block(&self.name, 0);
                break;
            }

            let page_results = parse_generic_html(&page.body, &self.name, &page.final_url, rank_base);
            super::health::record_parsed(&self.name, page_results.len());
            if page_results.is_empty() && page_idx == 0 {
                super::health::dump_zero_result_body(&self.name, &url, &page.body);
            }

            if page_results.is_empty() {
                // 200 OK with nothing parsed => selector rot or end of results.
                break;
            }
            rank_base += page_results.len();

            for r in page_results {
                if seen.insert(r.url.clone()) {
                    results.push(r);
                }
            }
        }

        tracing::info!("{} [{}]: {} results", self.name, f.kind(), results.len());
        results
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Parsing
// ══════════════════════════════════════════════════════════════════════════════

/// Pre-compiled selectors. v7.0 called `Selector::parse(..).unwrap()` on every
/// page of every engine — thousands of compilations per search.
static SELECTORS: std::sync::LazyLock<Vec<Selector>> = std::sync::LazyLock::new(|| {
    [
        // Engine-specific result containers first — far higher precision than
        // the generic fallbacks, which pick up nav/footer/related links.
        "li.b_algo h2 a",           // Bing
        ".result__a",               // DuckDuckGo html
        ".result__title a",         // DuckDuckGo html (alt)
        "a.result-link",            // DuckDuckGo lite / Startpage
        "#results .snippet a[href]",// Brave
        ".serp-item a.OrganicTitle-Link", // Yandex
        "ul.results-standard li a.title", // Mojeek
        "h3 a[href]",
        "h2 a[href]",
        // Generic fallbacks, lowest precision.
        "#search a[href]",
        "article a[href]",
        "main a[href]",
    ]
    .iter()
    .filter_map(|s| Selector::parse(s).ok())
    .collect()
});

pub fn parse_generic_html(
    html: &str,
    engine_name: &str,
    engine_url: &str,
    rank_base: usize,
) -> Vec<RawSearchResult> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let host = url::Url::parse(engine_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();
    let host_root = registrable_root(&host);

    // Try selectors in precision order and stop at the first that produces a
    // plausible result set. v7.0 unioned ALL selectors, so a page that matched
    // a precise selector still had nav links from `li a[href]` appended.
    for sel in SELECTORS.iter() {
        let mut batch: Vec<RawSearchResult> = Vec::new();
        for link in doc.select(sel) {
            let Some(href) = link.value().attr("href") else { continue };
            let Some(resolved) = resolve_target_url(href, engine_url) else { continue };

            if !resolved.starts_with("http://") && !resolved.starts_with("https://") {
                continue;
            }
            // Drop self-links by registrable root, not substring-of-URL.
            if !host_root.is_empty() && url_root(&resolved) == host_root {
                continue;
            }
            if !seen.insert(resolved.clone()) {
                continue;
            }

            let title = link.text().collect::<String>().trim().to_string();
            if crate::util::char_len(&title) < 3 {
                continue;
            }

            let snippet = link
                .value()
                .attr("aria-label")
                .map(|s| s.to_string())
                .unwrap_or_default();

            batch.push(RawSearchResult {
                url: resolved,
                title,
                snippet,
                engine: engine_name.to_string(),
                rank_position: rank_base + batch.len() + 1,
                published: None,
            });

            if batch.len() >= 40 {
                break;
            }
        }

        if batch.len() >= 3 {
            // Good enough — a precise selector matched.
            out = batch;
            break;
        }
        // Keep the best partial result in case nothing better matches.
        if batch.len() > out.len() {
            out = batch;
        }
        seen.clear();
        for r in &out {
            seen.insert(r.url.clone());
        }
    }

    out
}

/// Registrable-ish root: last two labels of the host ("www.bing.com" ->
/// "bing.com"). Good enough to detect self-links without a PSL dependency.
fn registrable_root(host: &str) -> String {
    let h = host.trim_end_matches('.').to_lowercase();
    let parts: Vec<&str> = h.split('.').collect();
    if parts.len() <= 2 {
        return h;
    }
    parts[parts.len() - 2..].join(".")
}

fn url_root(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(registrable_root))
        .unwrap_or_default()
}

fn resolve_target_url(href: &str, base: &str) -> Option<String> {
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    if href.starts_with("//") {
        return Some(format!("https:{href}"));
    }

    // Google-style redirect: /url?q=<target>
    if href.starts_with("/url?") {
        if let Ok(parsed) = url::Url::parse(&format!("https://www.google.com{href}")) {
            for (k, v) in parsed.query_pairs() {
                if k == "q" || k == "url" {
                    return Some(v.to_string());
                }
            }
        }
    }

    // DuckDuckGo redirect: /l/?uddg=<percent-encoded target>
    if href.contains("uddg=") {
        let abs = if href.starts_with("http") {
            href.to_string()
        } else {
            format!("https://duckduckgo.com{href}")
        };
        if let Ok(parsed) = url::Url::parse(&abs) {
            for (k, v) in parsed.query_pairs() {
                if k == "uddg" {
                    return Some(
                        urlencoding::decode(&v).map(|d| d.to_string()).unwrap_or_else(|_| v.to_string()),
                    );
                }
            }
        }
    }

    // Yahoo redirect: .../RU=<encoded>/RK=...
    if let Some(pos) = href.find("RU=") {
        let start = pos + 3;
        let end = href[start..].find('/').map(|e| start + e).unwrap_or(href.len());
        if let Ok(decoded) = urlencoding::decode(&href[start..end]) {
            if decoded.starts_with("http") {
                return Some(decoded.to_string());
            }
        }
    }

    // Relative path — resolve against the engine URL.
    if href.starts_with('/') {
        if let Ok(b) = url::Url::parse(base) {
            if let Ok(joined) = b.join(href) {
                return Some(joined.to_string());
            }
        }
    }

    None
}

// ══════════════════════════════════════════════════════════════════════════════
// Engine specs
// ══════════════════════════════════════════════════════════════════════════════

pub fn spec_for(name: &str) -> Option<GenericEngineSpec> {
    let n = name.to_lowercase();

    // ── Google family ──────────────────────────────────────────────────────
    if n.starts_with("google") {
        let domain = google_domain_for_engine(&n);
        let hl = google_locale_for_engine(&n);

        if n.contains("_news") {
            return Some(GenericEngineSpec::owned_stealth(
                format!("https://{domain}/search?q={{query}}&tbm=nws&start={{offset}}&hl={hl}"),
                2,
                PageParam::Offset10,
            ));
        }
        if n.contains("_scholar") {
            return Some(GenericEngineSpec::stat_stealth(
                "https://scholar.google.com/scholar?q={query}&start={offset}",
                2,
                PageParam::Offset10,
            ));
        }
        // Image/video verticals return no usable text sources — dropped.
        if n.contains("_images") || n.contains("_videos") {
            return None;
        }
        // Regional aliases fetch ONE page: they share google.com-family hosts
        // under the per-host gate, and they exist for diversity, not depth.
        let pages = if n == "google" { 3 } else { 1 };
        return Some(GenericEngineSpec::owned_stealth(
            format!("https://{domain}/search?q={{query}}&start={{offset}}&hl={hl}"),
            pages,
            PageParam::Offset10,
        ));
    }

    // ── Bing family — regional aliases now carry a real market ─────────────
    if n.starts_with("bing") {
        if n.contains("_news") {
            return Some(GenericEngineSpec::stat_stealth(
                "https://www.bing.com/news/search?q={query}&first={offset}",
                2,
                PageParam::Offset1Based,
            ));
        }
        if n.contains("_images") || n.contains("_videos") {
            return None;
        }
        let market = bing_market_for_engine(&n);
        return Some(match market {
            Some(m) => GenericEngineSpec::owned_stealth(
                format!(
                    "https://www.bing.com/search?q={{query}}&first={{offset}}&mkt={m}&setlang={m}"
                ),
                3,
                PageParam::Offset1Based,
            ),
            None => GenericEngineSpec::stat_stealth(
                "https://www.bing.com/search?q={query}&first={offset}",
                3,
                PageParam::Offset1Based,
            ),
        });
    }

    // ── DuckDuckGo: only the html endpoint is scrapable. The _news/_images/
    //    _videos verticals need a vqd token, and in v7.0 all four aliases
    //    resolved to the SAME html URL — four identical requests.
    if n.starts_with("duckduckgo") {
        if n == "duckduckgo" || n == "duckduckgo_html" {
            return Some(GenericEngineSpec::stat_stealth(
                "https://html.duckduckgo.com/html/?q={query}&s={offset}",
                2,
                PageParam::Offset10,
            ));
        }
        return None;
    }

    // ── Brave — `offset` is a PAGE index (0,1,2), not a row offset ─────────
    if n.starts_with("brave") {
        let endpoint = if n.contains("_news") {
            "https://search.brave.com/news?q={query}&offset={offset}"
        } else {
            "https://search.brave.com/search?q={query}&source=web&offset={offset}"
        };
        return Some(GenericEngineSpec::stat_stealth(endpoint, 3, PageParam::PageZero));
    }

    if n.starts_with("yahoo") {
        let endpoint = if n.contains("_news") {
            "https://news.search.yahoo.com/search?p={query}&b={offset}"
        } else {
            "https://search.yahoo.com/search?p={query}&b={offset}"
        };
        return Some(GenericEngineSpec::stat_stealth(endpoint, 3, PageParam::Offset1Based));
    }

    // Yandex `p` is 0-based — v7.0 started at p=1 and never fetched page one.
    if n.starts_with("yandex") {
        return Some(GenericEngineSpec::stat_stealth(
            "https://yandex.com/search/?text={query}&p={page}",
            2,
            PageParam::PageZero,
        ));
    }

    // Ecosia `p` is 0-based — same off-by-one as Yandex in v7.0.
    if n.starts_with("ecosia") {
        return Some(GenericEngineSpec::stat_stealth(
            "https://www.ecosia.org/search?q={query}&p={page}",
            2,
            PageParam::PageZero,
        ));
    }

    if n.starts_with("mwmbl") {
        return Some(GenericEngineSpec::stat(
            "https://mwmbl.org/?q={query}",
            1,
            PageParam::None,
        ));
    }

    if n.starts_with("4get") {
        return Some(GenericEngineSpec::stat("https://4get.ca/web?s={query}", 1, PageParam::None));
    }

    if n.starts_with("yacy") {
        return Some(GenericEngineSpec::stat(
            "https://yacy.searchlab.eu/yacysearch.html?query={query}",
            1,
            PageParam::None,
        ));
    }

    if n.starts_with("sogou") {
        return Some(GenericEngineSpec::stat(
            "https://www.sogou.com/web?query={query}&page={page}",
            2,
            PageParam::PageOne,
        ));
    }

    if n.starts_with("baidu") {
        return Some(GenericEngineSpec::stat(
            "https://www.baidu.com/s?wd={query}&pn={offset}",
            2,
            PageParam::Offset10,
        ));
    }

    if n.starts_with("naver") {
        return Some(GenericEngineSpec::stat(
            "https://search.naver.com/search.naver?query={query}",
            1,
            PageParam::None,
        ));
    }

    if n.starts_with("seznam") {
        return Some(GenericEngineSpec::stat("https://search.seznam.cz/?q={query}", 1, PageParam::None));
    }

    if n.starts_with("swisscows") {
        return Some(GenericEngineSpec::stat("https://swisscows.com/web?query={query}", 1, PageParam::None));
    }

    if n.starts_with("metager") {
        return Some(GenericEngineSpec::stat(
            "https://metager.org/meta/meta.ger3?eingabe={query}",
            1,
            PageParam::None,
        ));
    }

    // ── REMOVED in v8.0 — measured dead or walled on 2026-10-01 ────────────
    // No DNS / connection refused: alexandria.org, results.excite.com,
    //   search.lycos.com, leta.mullvad.net, pipilika.com, searchencrypt.com
    // HTTP errors: whoogle.io 502, librex.devol.it 404, rightdao.com 503,
    //   ask.com/web 404, stract.com/search 404
    // JS/token walls with no HTML path: dogpile, webcrawler, info, searchalot,
    //   aol, gibiru, kiddle, presearch (403), yep (403)
    // `marginalia` is now served by engines::marginalia_api (JSON API).

    None
}

fn google_domain_for_engine(name: &str) -> &'static str {
    match name.rsplit('_').next().unwrap_or("") {
        "uk" => "www.google.co.uk",
        "in" => "www.google.co.in",
        "de" => "www.google.de",
        "fr" => "www.google.fr",
        "es" => "www.google.es",
        "it" => "www.google.it",
        "br" => "www.google.com.br",
        "jp" => "www.google.co.jp",
        "ca" => "www.google.ca",
        "au" => "www.google.com.au",
        "nl" => "www.google.nl",
        "se" => "www.google.se",
        "no" => "www.google.no",
        "fi" => "www.google.fi",
        _ => "www.google.com",
    }
}

/// `hl` makes a regional alias actually different. Without it every Google
/// alias produced an identical request (v7.0).
fn google_locale_for_engine(name: &str) -> &'static str {
    match name.rsplit('_').next().unwrap_or("") {
        "uk" => "en-GB",
        "in" => "en-IN",
        "de" => "de",
        "fr" => "fr",
        "es" => "es",
        "it" => "it",
        "br" => "pt-BR",
        "jp" => "ja",
        "ca" => "en-CA",
        "au" => "en-AU",
        "nl" => "nl",
        "se" => "sv",
        "no" => "no",
        "fi" => "fi",
        _ => "en",
    }
}

fn bing_market_for_engine(name: &str) -> Option<&'static str> {
    match name.rsplit('_').next().unwrap_or("") {
        "uk" => Some("en-GB"),
        "in" => Some("en-IN"),
        "de" => Some("de-DE"),
        "fr" => Some("fr-FR"),
        "es" => Some("es-ES"),
        "it" => Some("it-IT"),
        "jp" => Some("ja-JP"),
        "ca" => Some("en-CA"),
        "au" => Some("en-AU"),
        "nl" => Some("nl-NL"),
        "se" => Some("sv-SE"),
        "no" => Some("nb-NO"),
        "fi" => Some("fi-FI"),
        "us" => Some("en-US"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_placeholder_means_exactly_one_request() {
        // The v7.0 bug: pages:2 with no {offset}/{page} fetched the same URL
        // twice. ~25 engines did this, wasting ~120 requests per search.
        let spec = GenericEngineSpec::stat("https://example.test/?q={query}", 2, PageParam::None);
        assert_eq!(spec.effective_pages(), 1);
        assert_eq!(
            spec.url_for_page("cats", 0),
            "https://example.test/?q=cats",
            "no placeholder, no pagination"
        );
    }

    #[test]
    fn page_zero_engines_start_at_zero() {
        // v7.0 used page_idx+1, so yandex/ecosia never fetched page one.
        let spec = GenericEngineSpec::stat("https://y.test/?text={query}&p={page}", 2, PageParam::PageZero);
        assert_eq!(spec.url_for_page("x", 0), "https://y.test/?text=x&p=0");
        assert_eq!(spec.url_for_page("x", 1), "https://y.test/?text=x&p=1");
    }

    #[test]
    fn offset10_and_offset1based_differ_correctly() {
        let g = GenericEngineSpec::stat("g?start={offset}", 3, PageParam::Offset10);
        assert_eq!(g.url_for_page("q", 0), "g?start=0");
        assert_eq!(g.url_for_page("q", 2), "g?start=20");

        let b = GenericEngineSpec::stat("b?first={offset}", 3, PageParam::Offset1Based);
        assert_eq!(b.url_for_page("q", 0), "b?first=1");
        assert_eq!(b.url_for_page("q", 2), "b?first=21");
    }

    #[test]
    fn brave_offset_is_a_page_index() {
        // v7.0 computed offset = page*10 -> 0,10,20, which is far past the
        // result set. Brave's offset is the page number.
        let spec = spec_for("brave").unwrap();
        assert_eq!(spec.page_param, PageParam::PageZero);
        let urls: Vec<String> = (0..3).map(|i| spec.url_for_page("q", i)).collect();
        assert!(urls[1].ends_with("offset=1"), "got {}", urls[1]);
        assert!(urls[2].ends_with("offset=2"), "got {}", urls[2]);
    }

    #[test]
    fn bing_regional_aliases_build_distinct_urls() {
        // v7.0: 15 Bing aliases -> one identical URL.
        let base = spec_for("bing").unwrap().url_for_page("q", 0);
        let uk = spec_for("bing_uk").unwrap().url_for_page("q", 0);
        let de = spec_for("bing_de").unwrap().url_for_page("q", 0);
        assert_ne!(base, uk);
        assert_ne!(uk, de);
        assert!(uk.contains("mkt=en-GB"), "got {uk}");
        assert!(de.contains("mkt=de-DE"), "got {de}");
    }

    #[test]
    fn google_regional_aliases_build_distinct_urls() {
        let com = spec_for("google").unwrap().url_for_page("q", 0);
        let uk = spec_for("google_uk").unwrap().url_for_page("q", 0);
        let in_ = spec_for("google_in").unwrap().url_for_page("q", 0);
        assert!(com.contains("www.google.com") && com.contains("hl=en"));
        assert!(uk.contains("www.google.co.uk") && uk.contains("hl=en-GB"));
        assert!(in_.contains("www.google.co.in") && in_.contains("hl=en-IN"));
        assert_ne!(uk, in_);
    }

    #[test]
    fn duplicate_ddg_verticals_have_no_spec() {
        // All four resolved to the same html URL in v7.0.
        assert!(spec_for("duckduckgo").is_some());
        assert!(spec_for("duckduckgo_news").is_none());
        assert!(spec_for("duckduckgo_images").is_none());
        assert!(spec_for("duckduckgo_videos").is_none());
    }

    #[test]
    fn dead_engines_have_no_spec() {
        for dead in [
            "alexandria", "excite", "lycos", "mullvad_leta", "pipilika",
            "searchencrypt", "whoogle", "librex", "right_dao", "ask",
            "stract", "dogpile", "webcrawler", "info", "gibiru", "kiddle",
            "presearch", "yep", "aol", "searchalot",
        ] {
            assert!(spec_for(dead).is_none(), "{dead} should have been removed");
        }
    }

    #[test]
    fn waf_engines_are_marked_stealth() {
        for e in ["google", "bing", "duckduckgo", "brave", "yahoo", "yandex", "ecosia"] {
            assert!(spec_for(e).unwrap().stealth, "{e} must use TLS impersonation");
        }
        // Indie engines do not need it.
        assert!(!spec_for("mwmbl").unwrap().stealth);
        assert!(!spec_for("4get").unwrap().stealth);
    }

    #[test]
    fn rank_position_is_global_across_pages() {
        // v7.0 reset rank to 1 on every page, so position 21 scored like
        // position 1 in RRF.
        let html = r#"<html><body><div id="search">
            <h3><a href="https://a.test/1">Result One Title</a></h3>
            <h3><a href="https://b.test/2">Result Two Title</a></h3>
            <h3><a href="https://c.test/3">Result Three Title</a></h3>
        </div></body></html>"#;
        let p1 = parse_generic_html(html, "test", "https://engine.test/s", 0);
        assert_eq!(p1.len(), 3);
        assert_eq!(p1[0].rank_position, 1);
        assert_eq!(p1[2].rank_position, 3);

        let p2 = parse_generic_html(html, "test", "https://engine.test/s", 3);
        assert_eq!(p2[0].rank_position, 4, "page 2 must continue, not restart");
        assert_eq!(p2[2].rank_position, 6);
    }

    #[test]
    fn self_links_filtered_by_host_root_not_substring() {
        // "https://news.site.com/why-bing.com-lost" must NOT be dropped when
        // scraping bing.com — v7.0's `resolved.contains(&host)` dropped it.
        let html = r#"<html><body><div id="search">
            <h3><a href="https://news.site.com/why-bing.com-lost">Why Bing Lost The Race</a></h3>
            <h3><a href="https://www.bing.com/settings">Bing Settings Page</a></h3>
            <h3><a href="https://other.test/article">Another Real Article</a></h3>
            <h3><a href="https://third.test/page">Third Real Article</a></h3>
        </div></body></html>"#;
        let r = parse_generic_html(html, "bing", "https://www.bing.com/search?q=x", 0);
        let urls: Vec<&str> = r.iter().map(|x| x.url.as_str()).collect();
        assert!(urls.iter().any(|u| u.contains("news.site.com")), "legit article dropped: {urls:?}");
        assert!(!urls.iter().any(|u| u.contains("bing.com/settings")), "self-link kept: {urls:?}");
    }

    #[test]
    fn resolves_ddg_and_google_redirects() {
        assert_eq!(
            resolve_target_url("/l/?uddg=https%3A%2F%2Fexample.com%2Fa", "https://html.duckduckgo.com/html/"),
            Some("https://example.com/a".to_string())
        );
        assert_eq!(
            resolve_target_url("/url?q=https://example.com/b&sa=U", "https://www.google.com/search"),
            Some("https://example.com/b".to_string())
        );
        assert_eq!(
            resolve_target_url("//cdn.example.com/x", "https://e.test/"),
            Some("https://cdn.example.com/x".to_string())
        );
    }

    #[test]
    fn registrable_root_collapses_subdomains() {
        assert_eq!(registrable_root("www.bing.com"), "bing.com");
        assert_eq!(registrable_root("news.search.yahoo.com"), "yahoo.com");
        assert_eq!(registrable_root("bing.com"), "bing.com");
        assert_eq!(registrable_root("localhost"), "localhost");
    }

    /// Direct regression test for the P0-3 unbounded leak.
    ///
    /// v7.0's `spec_for` built Google templates with `format!` and then called
    /// `Box::leak` to coerce them to `&'static str`. Because `get_engines` was
    /// invoked inside every spawned dispatch task, each Google alias leaked a
    /// String on every query variation of every search, forever.
    ///
    /// 200k iterations would leak ~10 MB under the old code. With `Cow` the
    /// allocations are owned and dropped, so RSS must not move.
    #[test]
    fn spec_for_does_not_leak_across_many_calls() {
        fn rss_kb() -> usize {
            std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("VmRSS:"))
                        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
                })
                .unwrap_or(0)
        }

        // Warm up so first-touch page faults are not counted.
        for _ in 0..20_000 {
            std::hint::black_box(spec_for("google_uk"));
        }
        let before = rss_kb();

        for _ in 0..200_000 {
            std::hint::black_box(spec_for("google"));
            std::hint::black_box(spec_for("google_uk"));
            std::hint::black_box(spec_for("google_news"));
            std::hint::black_box(spec_for("bing_de"));
        }

        let after = rss_kb();
        let growth_kb = after.saturating_sub(before);
        // 800k spec_for calls. Old code: ~800k leaked Strings (tens of MB).
        assert!(
            growth_kb < 4_096,
            "RSS grew {growth_kb} KB over 800k spec_for calls — Box::leak may be back"
        );
    }

    #[test]
    fn spec_templates_are_owned_or_borrowed_never_leaked() {
        use std::borrow::Cow;
        // Static templates stay Borrowed (zero alloc); dynamic ones are Owned
        // (freed on drop). Neither form can leak.
        assert!(matches!(
            spec_for("duckduckgo").unwrap().endpoint_template,
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            spec_for("google_uk").unwrap().endpoint_template,
            Cow::Owned(_)
        ));
    }

    #[test]
    fn regional_aliases_fetch_one_page_only() {
        // Keeps the per-host gate from serializing deep pagination across
        // every alias of the same origin.
        assert_eq!(spec_for("google").unwrap().pages, 3);
        assert_eq!(spec_for("google_uk").unwrap().pages, 1);
        assert_eq!(spec_for("google_in").unwrap().pages, 1);
    }

    #[test]
    fn parsing_is_utf8_safe_on_devanagari_titles() {
        let html = format!(
            r#"<html><body><div id="search"><h3><a href="https://x.test/a">{}</a></h3></div></body></html>"#,
            "भारत और पाकिस्तान के बीच विश्व कप मुकाबला"
        );
        let r = parse_generic_html(&html, "test", "https://e.test/", 0);
        assert_eq!(r.len(), 1);
        assert!(r[0].title.contains("भारत"));
    }
}
