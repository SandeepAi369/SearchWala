// ══════════════════════════════════════════════════════════════════════════════
// Yahoo Search — HTML scraping (Bing-derived index)
//
// v8.0: requires TLS impersonation (plain reqwest got connection failures,
// measured 2026-10-01). Status checked before parsing; rank_position global
// across pages; pages reduced 5 -> 3.
//
// NOTE for ranking: Yahoo is Bing-derived, so `engines::engine_family` maps it
// to the "bing" family. v7.0 counted `yahoo` and `yahoo_news` as two
// independent engines (the live run logged "yahoo: 28" twice) AND treated them
// as independent of Bing, triple-counting one index in RRF "consensus".
// ══════════════════════════════════════════════════════════════════════════════

use std::collections::HashSet;
use std::sync::LazyLock;

use scraper::{Html, Selector};

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

pub struct Yahoo;

const PAGES: usize = 3;

static SEL_RESULT: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".algo-sr, .dd.algo, li.ov-a").unwrap());
static SEL_H3: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h3").unwrap());
static SEL_TITLE: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("h3.title a, h3 a, .compTitle a, .title a, a.d-ib").unwrap()
});
static SEL_DESC: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(".compText p, .compText, .fc-falcon, .lh-l, div.compText p").unwrap()
});

#[async_trait::async_trait]
impl super::SearchEngine for Yahoo {
    fn name(&self) -> &str {
        "yahoo"
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
                "https://search.yahoo.com/search?p={}&n=20&b={}",
                urlencoding::encode(query),
                page * 10 + 1
            );

            super::health::record_attempt("yahoo");
            let resp = match f.get(&url, &[]).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("Yahoo page {} transport error: {}", page, e);
                    super::health::record_transport_err("yahoo");
                    break;
                }
            };
            if resp.status != 200 {
                tracing::warn!("Yahoo: HTTP {} — not parsing", resp.status);
                super::health::record_block("yahoo", resp.status);
                break;
            }
            if super::health::looks_like_challenge(&resp.body) {
                super::health::record_block("yahoo", 0);
                break;
            }

            let page_results = parse_yahoo_html(&resp.body, rank_base);
            super::health::record_parsed("yahoo", page_results.len());
            if page_results.is_empty() {
                if page == 0 {
                    super::health::dump_zero_result_body("yahoo", &url, &resp.body);
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

        tracing::info!("Yahoo [{}]: {} results", f.kind(), all_results.len());
        all_results
    }
}

pub fn parse_yahoo_html(html: &str, rank_base: usize) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    for element in document.select(&SEL_RESULT) {
        let Some(link) = element.select(&SEL_TITLE).next() else { continue };
        let Some(href) = link.value().attr("href") else { continue };

        let actual_url = extract_yahoo_url(href);
        if actual_url.is_empty() {
            continue;
        }
        let title = yahoo_title(&link);
        if title.is_empty() {
            continue;
        }
        let snippet = element
            .select(&SEL_DESC)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        results.push(RawSearchResult {
            url: actual_url,
            title,
            snippet,
            engine: "yahoo".to_string(),
            rank_position: rank_base + results.len() + 1,
            published: None,
            full_text: None,
        });
    }

    results
}

/// The visible title of a Yahoo result link.
///
/// Yahoo's title anchor starts with a breadcrumb span (site name + display
/// URL) before the title text, so `link.text()` produced
/// "YouTubehttps://www.youtube.com › watchHow do solar panels work? - …" for
/// every result (seen in the Reddit and YouTube modes, where it is the main
/// engine). Yahoo serves two layouts (measured: alternating between requests):
/// A puts the anchor inside `<h3>` with a clean `aria-label`; B wraps the
/// breadcrumb and the `<h3>` inside the anchor. Use the label, then the inner
/// `<h3>`, then the anchor's own text nodes.
fn yahoo_title(link: &scraper::ElementRef) -> String {
    if let Some(label) = link.value().attr("aria-label") {
        let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
        if !label.is_empty() {
            return label;
        }
    }
    // Layout B: the anchor wraps breadcrumb + <h3><span>title</span></h3>.
    if let Some(h3) = link.select(&SEL_H3).next() {
        let t = h3.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
        if !t.is_empty() {
            return t;
        }
    }
    let own: String = link
        .children()
        .filter_map(|n| n.value().as_text().map(|t| t.to_string()))
        .collect();
    let own = own.split_whitespace().collect::<Vec<_>>().join(" ");
    if !own.is_empty() {
        return own;
    }
    link.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Yahoo wraps targets as `.../RU=<percent-encoded>/RK=...`.
pub fn extract_yahoo_url(href: &str) -> String {
    if let Some(pos) = href.find("RU=") {
        let start = pos + 3;
        let end = href[start..].find('/').map(|e| start + e).unwrap_or(href.len());
        if let Ok(decoded) = urlencoding::decode(&href[start..end]) {
            if decoded.starts_with("http") {
                return decoded.to_string();
            }
        }
    }
    if let Some(pos) = href.rfind("/*http") {
        // Byte-safe: "/*http" is pure ASCII so pos+2 is a valid boundary.
        return href[pos + 2..].to_string();
    }
    if (href.starts_with("http://") || href.starts_with("https://")) && !href.contains("yahoo.com") {
        return href.to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_ru_redirect() {
        let href = "https://r.search.yahoo.com/_ylt=abc/RU=https%3a%2f%2fexample.com%2fpage/RK=2/RS=xyz";
        assert_eq!(extract_yahoo_url(href), "https://example.com/page");
    }

    #[test]
    fn passes_through_direct_external_links() {
        assert_eq!(extract_yahoo_url("https://example.com/x"), "https://example.com/x");
        assert_eq!(extract_yahoo_url("https://www.yahoo.com/news"), "");
        assert_eq!(extract_yahoo_url("/relative"), "");
    }

    #[test]
    fn parses_results_with_global_rank() {
        let html = r#"<html><body>
          <div class="algo-sr"><h3 class="title"><a href="https://a.test/1">Yahoo Result One</a></h3>
            <div class="compText"><p>Snippet one</p></div></div>
          <div class="algo-sr"><h3 class="title"><a href="https://b.test/2">Yahoo Result Two</a></h3>
            <div class="compText"><p>Snippet two</p></div></div>
        </body></html>"#;
        let r = parse_yahoo_html(html, 0);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].rank_position, 1);
        assert_eq!(parse_yahoo_html(html, 20)[0].rank_position, 21);
    }

    #[test]
    fn title_skips_the_breadcrumb_span() {
        // Yahoo's 2026 markup: breadcrumb span inside the title anchor.
        let with_label = r#"<div class="algo-sr"><h3 class="title"><a href="https://www.youtube.com/watch?v=x"
            aria-label="How do solar panels work? - YouTube"><span class="d-ib p-abs">YouTube<span
            class="fc-pewter">https://www.youtube.com › watch</span></span>How do solar panels work? - YouTube</a></h3></div>"#;
        assert_eq!(parse_yahoo_html(with_label, 0)[0].title, "How do solar panels work? - YouTube");

        let no_label = with_label.replace(r#"aria-label="How do solar panels work? - YouTube""#, "");
        assert_eq!(parse_yahoo_html(&no_label, 0)[0].title, "How do solar panels work? - YouTube");
    }

    #[test]
    fn title_from_layout_where_the_anchor_wraps_the_heading() {
        // Yahoo layout B (live, 2026-10-02): no aria-label, h3 inside the anchor.
        let html = r#"<li><div class="dd algo algo-sr"><div class="compTitle options-toggle"><a class="d-ib"
            href="https://www.reddit.com/r/laptops/comments/1/best/"><div><span>Reddit</span>https://www.reddit.com
            <span> › r › laptops</span></div><h3 class="title"><span class="d-b">Best Laptop for college student? : r/laptops</span></h3></a></div>
            <div class="compText"><p>snippet</p></div></div></li>"#;
        let r = parse_yahoo_html(html, 0);
        assert_eq!(r[0].title, "Best Laptop for college student? : r/laptops");
    }
}
