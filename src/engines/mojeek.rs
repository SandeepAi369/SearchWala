// ══════════════════════════════════════════════════════════════════════════════
// Mojeek — independent index, HTML scraping
//
// v8.0: status checked before parsing; rank_position global across pages;
// pages reduced from 5 to 3 (Mojeek throttles aggressively — measured 5.5 KB
// responses with 2 links on 2026-10-01). Mojeek's `s` param is 1-based.
// ══════════════════════════════════════════════════════════════════════════════

use std::collections::HashSet;
use std::sync::LazyLock;

use scraper::{Html, Selector};

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

pub struct Mojeek;

const PAGES: usize = 3;

static SEL_LI: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("ul.results-standard li, ul.results li").unwrap());
static SEL_TITLE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.title, h2 a").unwrap());
static SEL_SNIPPET: LazyLock<Selector> = LazyLock::new(|| Selector::parse("p.s").unwrap());

#[async_trait::async_trait]
impl super::SearchEngine for Mojeek {
    fn name(&self) -> &str {
        "mojeek"
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        let mut all_results = Vec::new();
        let mut seen_urls: HashSet<String> = HashSet::new();
        let mut rank_base = 0usize;

        for page in 0..super::page_budget(PAGES) {
            let url = format!(
                "https://www.mojeek.com/search?q={}&s={}",
                urlencoding::encode(query),
                page * 10 + 1
            );

            super::health::record_attempt("mojeek");
            let resp = match f.get(&url, &[]).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("Mojeek page {} transport error: {}", page, e);
                    super::health::record_transport_err("mojeek");
                    break;
                }
            };
            if resp.status != 200 {
                tracing::warn!("Mojeek: HTTP {} — not parsing", resp.status);
                super::health::record_block("mojeek", resp.status);
                break;
            }
            if super::health::looks_like_challenge(&resp.body) {
                super::health::record_block("mojeek", 0);
                break;
            }

            let page_results = parse_mojeek_html(&resp.body, rank_base);
            super::health::record_parsed("mojeek", page_results.len());
            if page_results.is_empty() {
                if page == 0 {
                    super::health::dump_zero_result_body("mojeek", &url, &resp.body);
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

        tracing::info!("Mojeek [{}]: {} results", f.kind(), all_results.len());
        all_results
    }
}

pub fn parse_mojeek_html(html: &str, rank_base: usize) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    for element in document.select(&SEL_LI) {
        let Some(link) = element.select(&SEL_TITLE).next() else { continue };
        let href = match link.value().attr("href") {
            Some(h) if h.starts_with("http") && !h.contains("mojeek.com") => h.to_string(),
            _ => continue,
        };
        let title = link.text().collect::<String>().trim().to_string();
        if title.is_empty() {
            continue;
        }
        let snippet = element
            .select(&SEL_SNIPPET)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        results.push(RawSearchResult {
            url: href,
            title,
            snippet,
            engine: "mojeek".to_string(),
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
    fn parses_mojeek_list_structure() {
        let html = r#"<html><body><ul class="results-standard">
          <li><h2><a class="title" href="https://a.test/1">First Mojeek Result</a></h2>
              <p class="s">Snippet for the first result.</p></li>
          <li><h2><a class="title" href="https://b.test/2">Second Mojeek Result</a></h2>
              <p class="s">Snippet for the second.</p></li>
        </ul></body></html>"#;
        let r = parse_mojeek_html(html, 0);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].rank_position, 1);
        assert!(r[0].snippet.contains("first result"));
        assert_eq!(parse_mojeek_html(html, 10)[0].rank_position, 11);
    }
}
