// ══════════════════════════════════════════════════════════════════════════════
// Startpage — Google proxy
//
// v8.0 changes:
//   * Requires TLS impersonation (Cloudflare-fronted).
//   * Status checked before parsing; `rank_position` global across pages.
//   * Snippets are now actually extracted. v7.0 had
//       `let snippet = String::new(); // Startpage snippets are in separate elements`
//     so every Startpage result carried an empty snippet.
//   * The `a[href]` fallback scraper is DELETED (harvested nav links).
//   * NOTE for ranking: Startpage proxies GOOGLE. `engines::engine_family`
//     maps it to the "google" family so RRF does not treat it as independent
//     evidence for Google's own ordering.
// ══════════════════════════════════════════════════════════════════════════════

use std::collections::HashSet;
use std::sync::LazyLock;

use scraper::{Html, Selector};

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

pub struct Startpage;

const PAGES: usize = 3;

static SEL_RESULT: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.result, div.w-gl__result, [class*='result']").unwrap());
static SEL_LINK: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("a.result-link, a[class*='result-link'], a.w-gl__result-title").unwrap()
});
static SEL_DESC: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("p.description, p[class*='description'], .w-gl__description").unwrap()
});

#[async_trait::async_trait]
impl super::SearchEngine for Startpage {
    fn name(&self) -> &str {
        "startpage"
    }

    fn needs_stealth(&self) -> bool {
        true
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        let mut all_results = Vec::new();
        let mut seen_urls: HashSet<String> = HashSet::new();
        let mut rank_base = 0usize;

        for page in 1..=super::page_budget(PAGES) {
            let form: Vec<(String, String)> = vec![
                ("query".to_string(), query.to_string()),
                ("cat".to_string(), "web".to_string()),
                ("language".to_string(), "english".to_string()),
                ("page".to_string(), page.to_string()),
            ];

            super::health::record_attempt("startpage");
            let resp = match f
                .post_form(
                    "https://www.startpage.com/sp/search",
                    &form,
                    &[("Referer", "https://www.startpage.com/")],
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("Startpage page {} transport error: {}", page, e);
                    super::health::record_transport_err("startpage");
                    break;
                }
            };

            if resp.status != 200 {
                tracing::warn!("Startpage: HTTP {} — not parsing", resp.status);
                super::health::record_block("startpage", resp.status);
                break;
            }
            if super::health::looks_like_challenge(&resp.body) {
                super::health::record_block("startpage", 0);
                break;
            }

            let page_results = parse_startpage_html(&resp.body, rank_base);
            super::health::record_parsed("startpage", page_results.len());
            if page_results.is_empty() {
                if page == 1 {
                    super::health::dump_zero_result_body(
                        "startpage",
                        "https://www.startpage.com/sp/search",
                        &resp.body,
                    );
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

        tracing::info!("Startpage [{}]: {} results", f.kind(), all_results.len());
        all_results
    }
}

pub fn parse_startpage_html(html: &str, rank_base: usize) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    // Startpage uses hashed CSS suffixes (`result-link css-xyz123`), so we
    // match on the stable substring rather than an exact class.
    for el in document.select(&SEL_RESULT) {
        let Some(link) = el.select(&SEL_LINK).next() else { continue };
        let href = match link.value().attr("href") {
            Some(h) if h.starts_with("http") && !h.contains("startpage.com") => h.to_string(),
            _ => continue,
        };
        let title = link.text().collect::<String>().trim().to_string();
        if crate::util::char_len(&title) < 5 {
            continue;
        }
        if !seen.insert(href.clone()) {
            continue;
        }

        // v7.0 hardcoded this to String::new().
        let snippet = el
            .select(&SEL_DESC)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        results.push(RawSearchResult {
            url: href,
            title,
            snippet,
            engine: "startpage".to_string(),
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
    fn extracts_snippets_not_empty_strings() {
        let html = r#"<html><body>
          <div class="result css-abc">
            <a class="result-link css-xyz" href="https://a.test/1">A Real Result Title</a>
            <p class="description css-q">This is the snippet text for result one.</p>
          </div>
        </body></html>"#;
        let r = parse_startpage_html(html, 0);
        assert_eq!(r.len(), 1);
        assert!(!r[0].snippet.is_empty(), "v7.0 always returned an empty snippet");
        assert!(r[0].snippet.contains("snippet text"));
        assert_eq!(r[0].rank_position, 1);
    }

    #[test]
    fn global_rank_across_pages() {
        let html = r#"<html><body><div class="result">
            <a class="result-link" href="https://a.test/1">Result Title Here</a>
            <p class="description">Snip</p></div></body></html>"#;
        assert_eq!(parse_startpage_html(html, 10)[0].rank_position, 11);
    }

    #[test]
    fn skips_self_links_and_short_titles() {
        let html = r#"<html><body>
          <div class="result"><a class="result-link" href="https://www.startpage.com/settings">Settings Page</a></div>
          <div class="result"><a class="result-link" href="https://ok.test/x">Hi</a></div>
        </body></html>"#;
        assert_eq!(parse_startpage_html(html, 0).len(), 0);
    }
}
