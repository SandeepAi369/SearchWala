// ══════════════════════════════════════════════════════════════════════════════
// Qwant — HTML scraping
//
// v8.0: Qwant's main site is a JS SPA, so we target the `lite` interface which
// serves server-rendered HTML. Status is checked before parsing.
//
// v7.0 used a bare `a[href]` scan over the SPA shell, which produced
// navigation links rather than results — and did so silently.
// ══════════════════════════════════════════════════════════════════════════════

use std::collections::HashSet;
use std::sync::LazyLock;

use scraper::{Html, Selector};

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

pub struct Qwant;

static SEL_RESULT: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("article, .result, .web-result").unwrap());
static SEL_LINK: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a[href^='http']").unwrap());
static SEL_DESC: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("p, .description").unwrap());

#[async_trait::async_trait]
impl super::SearchEngine for Qwant {
    fn name(&self) -> &str {
        "qwant"
    }

    fn needs_stealth(&self) -> bool {
        true
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        let url = format!("https://lite.qwant.com/?q={}&t=web", urlencoding::encode(query));

        super::health::record_attempt("qwant");
        let resp = match f.get(&url, &[]).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("Qwant transport error: {}", e);
                super::health::record_transport_err("qwant");
                return Vec::new();
            }
        };
        if resp.status != 200 {
            tracing::warn!("Qwant: HTTP {} — not parsing", resp.status);
            super::health::record_block("qwant", resp.status);
            return Vec::new();
        }
        if super::health::looks_like_challenge(&resp.body) {
            super::health::record_block("qwant", 0);
            return Vec::new();
        }

        let results = parse_qwant_html(&resp.body, 0);
        super::health::record_parsed("qwant", results.len());
        if results.is_empty() {
            super::health::dump_zero_result_body("qwant", &url, &resp.body);
        }
        tracing::info!("Qwant [{}]: {} results", f.kind(), results.len());
        results
    }
}

pub fn parse_qwant_html(html: &str, rank_base: usize) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for el in document.select(&SEL_RESULT) {
        let Some(link) = el.select(&SEL_LINK).next() else { continue };
        let href = match link.value().attr("href") {
            Some(h) if h.starts_with("http") && !h.contains("qwant.com") => h.to_string(),
            _ => continue,
        };
        let title = link.text().collect::<String>().trim().to_string();
        if crate::util::char_len(&title) < 5 {
            continue;
        }
        if !seen.insert(href.clone()) {
            continue;
        }
        let snippet = el
            .select(&SEL_DESC)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        results.push(RawSearchResult {
            url: href,
            title,
            snippet,
            engine: "qwant".to_string(),
            rank_position: rank_base + results.len() + 1,
            published: None,
        });
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_qwant_lite_articles() {
        let html = r#"<html><body>
          <article><a href="https://a.test/1">A Qwant Result Title</a><p>Snippet A</p></article>
          <article><a href="https://b.test/2">B Qwant Result Title</a><p>Snippet B</p></article>
        </body></html>"#;
        let r = parse_qwant_html(html, 0);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].rank_position, 1);
        assert_eq!(r[0].snippet, "Snippet A");
    }

    #[test]
    fn ignores_self_links() {
        let html = r#"<html><body><article>
            <a href="https://www.qwant.com/settings">Qwant Settings Page</a></article></body></html>"#;
        assert_eq!(parse_qwant_html(html, 0).len(), 0);
    }
}
