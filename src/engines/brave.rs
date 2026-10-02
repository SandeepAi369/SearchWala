// ══════════════════════════════════════════════════════════════════════════════
// Brave Search — multi-page HTML scraping
//
// v8.0: requires TLS impersonation. Measured 2026-10-01 on the same query:
//   plain reqwest -> HTTP 429 on the FIRST request, 0 results
//   wreq/BoringSSL -> HTTP 200, 200 KB, 42 `.snippet` elements
//
// Also fixed here:
//   * `offset` is a PAGE index (0,1,2), not a row offset. v7.0 sent 0,10,20.
//   * HTTP status is checked before parsing (v7.0 parsed the 429 body).
//   * `rank_position` is global across pages.
//   * The `a[href]` fallback scraper is DELETED — on a 429/challenge page it
//     harvested navigation and error-page links and emitted them as results.
// ══════════════════════════════════════════════════════════════════════════════

use std::collections::HashSet;
use std::sync::LazyLock;

use scraper::{Html, Selector};

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

pub struct Brave;

const PAGES: usize = 3;

// ── Selectors verified against tests/fixtures/serp_brave.html (2026-10-01) ──
//
// Brave is a Svelte app with hashed class suffixes (`svelte-14r20fy`), so we
// match on the stable part. The v7.0 selectors are all GONE from the live
// markup, which is why Brave returned 0 results even after the 429 was fixed:
//   `#results`            -> 0 matches (container id removed)
//   `a.snippet-title`     -> 0 matches
//   `.snippet-description`-> 0 matches
// Fixture census: div.snippet[data-type="web"] = 20, a.l1 = 20,
//                 .title = 20, .generic-snippet = 20.
static SEL_RESULT: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.snippet[data-type=\"web\"]").unwrap());
static SEL_TITLE_LINK: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.l1, a[href^=\"http\"]").unwrap());
static SEL_TITLE_TEXT: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".title, [class*=\"snippet-title\"]").unwrap());
static SEL_DESC: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".generic-snippet .content, .generic-snippet, [class*=\"snippet-content\"]")
        .unwrap()
});

#[async_trait::async_trait]
impl super::SearchEngine for Brave {
    fn name(&self) -> &str {
        "brave"
    }

    fn needs_stealth(&self) -> bool {
        true
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        let mut all_results = Vec::new();
        let mut seen_urls: HashSet<String> = HashSet::new();
        let mut rank_base = 0usize;

        for page in 0..super::page_budget(PAGES) {
            // Brave's `offset` is the page number, not a row offset.
            let url = format!(
                "https://search.brave.com/search?q={}&source=web&offset={}",
                urlencoding::encode(query),
                page
            );

            super::health::record_attempt("brave");
            let resp = match f.get(&url, &[]).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("Brave page {} transport error: {}", page, e);
                    super::health::record_transport_err("brave");
                    break;
                }
            };

            if resp.status != 200 {
                tracing::warn!("Brave: HTTP {} — not parsing", resp.status);
                super::health::record_block("brave", resp.status);
                break;
            }
            if super::health::looks_like_challenge(&resp.body) {
                tracing::warn!("Brave: bot challenge in 200 body — not parsing");
                super::health::record_block("brave", 0);
                break;
            }

            let page_results = parse_brave_html(&resp.body, rank_base);
            super::health::record_parsed("brave", page_results.len());
            if page_results.is_empty() {
                if page == 0 {
                    super::health::dump_zero_result_body("brave", &url, &resp.body);
                }
                break;
            }
            rank_base += page_results.len();

            for r in page_results {
                if seen_urls.insert(r.url.clone()) {
                    all_results.push(r);
                }
            }
        }

        tracing::info!("Brave [{}]: {} results", f.kind(), all_results.len());
        all_results
    }
}

pub fn parse_brave_html(html: &str, rank_base: usize) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    for element in document.select(&SEL_RESULT) {
        let Some(link) = element.select(&SEL_TITLE_LINK).next() else { continue };
        let href = match link.value().attr("href") {
            Some(h) if h.starts_with("http") => h.to_string(),
            _ => continue,
        };

        // The anchor's own text is the site name + breadcrumb URL
        // ("ICC Cricket icc-cricket.com > tournaments > ..."), not the title.
        // The real title lives in a sibling `.title` div.
        let title = element
            .select(&SEL_TITLE_TEXT)
            .next()
            .map(|t| t.text().collect::<String>().trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| link.text().collect::<String>().trim().to_string());
        if title.is_empty() {
            continue;
        }
        let snippet = element
            .select(&SEL_DESC)
            .next()
            .map(|s| s.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();

        results.push(RawSearchResult {
            url: href,
            title,
            snippet,
            engine: "brave".to_string(),
            rank_position: rank_base + results.len() + 1,
            published: None,
            full_text: None,
        });
    }

    // NOTE: no `a[href]` fallback. An engine returning 0 results is honest and
    // actionable (the health registry flags it as selector_rot); an engine
    // returning 40 navigation links is neither.
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real Brave SERP captured 2026-10-01. This test is the guard against
    /// silent selector rot — the failure mode that left Brave returning 0
    /// results through all of v7.0.
    const FIXTURE: &str = include_str!("../../tests/fixtures/serp_brave.html");

    #[test]
    fn parses_live_brave_fixture() {
        let r = parse_brave_html(FIXTURE, 0);
        assert!(r.len() >= 15, "expected >=15 results from the real SERP, got {}", r.len());

        // Titles must be real titles, not the site-name/breadcrumb anchor text.
        let first = &r[0];
        assert!(
            !first.title.contains("\u{203a}") && !first.title.contains(" > "),
            "title looks like a breadcrumb: {:?}",
            first.title
        );
        assert!(first.url.starts_with("https://"), "bad url {:?}", first.url);
        assert_eq!(first.rank_position, 1);

        // Snippets must be populated for most results.
        let with_snippet = r.iter().filter(|x| x.snippet.len() > 20).count();
        assert!(
            with_snippet * 2 >= r.len(),
            "only {}/{} results had snippets",
            with_snippet,
            r.len()
        );

        // No self-links.
        assert!(
            !r.iter().any(|x| x.url.contains("search.brave.com")),
            "self-link leaked into results"
        );
    }

    #[test]
    fn fixture_rank_continues_across_pages() {
        let p2 = parse_brave_html(FIXTURE, 20);
        assert_eq!(p2[0].rank_position, 21);
    }

    #[test]
    fn no_junk_fallback_on_an_error_page() {
        // A 429/challenge page full of nav links must yield ZERO results, not
        // 40 harvested hrefs (the v7.0 behaviour).
        let error_page = r#"<html><body>
            <a href="https://brave.com/download">Download Brave Browser</a>
            <a href="https://community.brave.com/support">Support Community</a>
            <a href="https://example.com/unrelated">Some Unrelated Page</a>
        </body></html>"#;
        assert_eq!(parse_brave_html(error_page, 0).len(), 0);
    }
}
