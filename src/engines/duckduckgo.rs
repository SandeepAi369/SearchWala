// ══════════════════════════════════════════════════════════════════════════════
// DuckDuckGo — html endpoint scraping
//
// v8.0 changes:
//   * Requires TLS impersonation. Measured 2026-10-01:
//       plain reqwest -> HTTP 202 + CAPTCHA ("Select all squares containing a
//                        duck"), which v7.0 happily parsed as a result page
//       wreq/BoringSSL -> HTTP 200, 10 `.result__a` links + 40 `uddg=` targets
//   * Switched from `lite.duckduckgo.com` POST to `html.duckduckgo.com`. The
//     lite parser was broken regardless of blocking: it selected titles with
//     `a.result-snippet`, a class that does not exist (the lite page uses
//     `a.result-link` for titles and `td.result-snippet` for snippets), so it
//     returned 0 results even on a 200.
//   * Status checked before parsing; `rank_position` global across pages.
//   * `/l/?uddg=` redirect targets are unwrapped to the real URL.
// ══════════════════════════════════════════════════════════════════════════════

use std::collections::HashSet;
use std::sync::LazyLock;

use scraper::{Html, Selector};

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

pub struct DuckDuckGo;

const PAGES: usize = 2;

static SEL_RESULT: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".result, .web-result").unwrap());
static SEL_LINK: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.result__a, .result__title a").unwrap());
static SEL_SNIPPET: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".result__snippet, a.result__snippet").unwrap());

#[async_trait::async_trait]
impl super::SearchEngine for DuckDuckGo {
    fn name(&self) -> &str {
        "duckduckgo"
    }

    fn needs_stealth(&self) -> bool {
        true
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        let mut all_results = Vec::new();
        let mut seen_urls: HashSet<String> = HashSet::new();
        let mut rank_base = 0usize;

        for page in 0..super::page_budget(PAGES) {
            let url = format!(
                "https://html.duckduckgo.com/html/?q={}&s={}",
                urlencoding::encode(query),
                page * 10
            );

            super::health::record_attempt("duckduckgo");
            let resp = match f.get(&url, &[]).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("DuckDuckGo page {} transport error: {}", page, e);
                    super::health::record_transport_err("duckduckgo");
                    break;
                }
            };

            // v7.0 returned HTTP 202 with a CAPTCHA and parsed it anyway.
            if resp.status != 200 {
                tracing::warn!("DuckDuckGo: HTTP {} — not parsing", resp.status);
                super::health::record_block("duckduckgo", resp.status);
                break;
            }
            if super::health::looks_like_challenge(&resp.body) {
                tracing::warn!("DuckDuckGo: CAPTCHA challenge — not parsing");
                super::health::record_block("duckduckgo", 0);
                break;
            }

            let page_results = parse_ddg_html(&resp.body, rank_base);
            super::health::record_parsed("duckduckgo", page_results.len());
            if page_results.is_empty() {
                if page == 0 {
                    super::health::dump_zero_result_body("duckduckgo", &url, &resp.body);
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

        tracing::info!("DuckDuckGo [{}]: {} results", f.kind(), all_results.len());
        all_results
    }
}

/// Unwrap DuckDuckGo's `/l/?uddg=<encoded>` redirect to the real target.
fn unwrap_ddg_href(href: &str) -> Option<String> {
    if href.starts_with("http") && !href.contains("duckduckgo.com/l/") {
        return Some(href.to_string());
    }
    let abs = if href.starts_with("http") {
        href.to_string()
    } else if href.starts_with("//") {
        format!("https:{href}")
    } else {
        format!("https://duckduckgo.com{href}")
    };
    let parsed = url::Url::parse(&abs).ok()?;
    for (k, v) in parsed.query_pairs() {
        if k == "uddg" {
            let decoded = urlencoding::decode(&v).map(|d| d.to_string()).unwrap_or_else(|_| v.to_string());
            if decoded.starts_with("http") {
                return Some(decoded);
            }
        }
    }
    None
}

pub fn parse_ddg_html(html: &str, rank_base: usize) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    for el in document.select(&SEL_RESULT) {
        let Some(link) = el.select(&SEL_LINK).next() else { continue };
        let Some(href) = link.value().attr("href") else { continue };
        let Some(target) = unwrap_ddg_href(href) else { continue };

        let title = link.text().collect::<String>().trim().to_string();
        if title.is_empty() {
            continue;
        }
        let snippet = el
            .select(&SEL_SNIPPET)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        results.push(RawSearchResult {
            url: target,
            title,
            snippet,
            engine: "duckduckgo".to_string(),
            rank_position: rank_base + results.len() + 1,
            published: None,
            full_text: None,
        });
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_uddg_redirects() {
        assert_eq!(
            unwrap_ddg_href("/l/?uddg=https%3A%2F%2Fexample.com%2Farticle&rut=abc"),
            Some("https://example.com/article".to_string())
        );
        assert_eq!(
            unwrap_ddg_href("https://example.com/direct"),
            Some("https://example.com/direct".to_string())
        );
        assert_eq!(unwrap_ddg_href("/settings"), None);
    }

    #[test]
    fn parses_real_ddg_html_structure() {
        let html = r#"<html><body>
          <div class="result">
            <h2 class="result__title"><a class="result__a" href="/l/?uddg=https%3A%2F%2Fa.test%2F1">First Hit</a></h2>
            <a class="result__snippet">Snippet one here</a>
          </div>
          <div class="result">
            <h2 class="result__title"><a class="result__a" href="/l/?uddg=https%3A%2F%2Fb.test%2F2">Second Hit</a></h2>
            <a class="result__snippet">Snippet two here</a>
          </div>
        </body></html>"#;

        let r = parse_ddg_html(html, 0);
        assert_eq!(r.len(), 2, "got {r:?}");
        assert_eq!(r[0].url, "https://a.test/1");
        assert_eq!(r[0].title, "First Hit");
        assert_eq!(r[0].snippet, "Snippet one here");
        assert_eq!(r[1].rank_position, 2);

        let p2 = parse_ddg_html(html, 10);
        assert_eq!(p2[0].rank_position, 11, "global rank across pages");
    }

    #[test]
    fn captcha_page_yields_nothing() {
        let captcha = "<html><body>Unfortunately, bots use DuckDuckGo too. \
                       Select all squares containing a duck:</body></html>";
        assert!(super::super::health::looks_like_challenge(captcha));
        assert_eq!(parse_ddg_html(captcha, 0).len(), 0);
    }
}
