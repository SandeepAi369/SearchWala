// ══════════════════════════════════════════════════════════════════════════════
// DuckDuckGo Search Engine — HTML scraping approach
// ══════════════════════════════════════════════════════════════════════════════

use crate::config;
use crate::models::RawSearchResult;
use reqwest::Client;
use scraper::{Html, Selector};

pub struct DuckDuckGo;

#[async_trait::async_trait]
impl super::SearchEngine for DuckDuckGo {
    fn name(&self) -> &str {
        "duckduckgo"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        let url = "https://html.duckduckgo.com/html/";
        let ua = config::random_user_agent();

        let resp = match client
            .post(url)
            .header("User-Agent", ua)
            .header("Referer", "https://duckduckgo.com/")
            .header("Accept", "text/html,application/xhtml+xml")
            .header("Accept-Language", "en-US,en;q=0.9")
            .form(&[("q", query), ("kl", "us-en")])
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("DuckDuckGo request failed: {}", e);
                return vec![];
            }
        };

        let html_text = match resp.text().await {
            Ok(t) => t,
            Err(_) => return vec![],
        };

        parse_ddg_html(&html_text)
    }
}

fn parse_ddg_html(html: &str) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    // DuckDuckGo HTML results use .result class with .result__a for links
    let result_selector = Selector::parse(".result").unwrap();
    let link_selector = Selector::parse(".result__a").unwrap();
    let snippet_selector = Selector::parse(".result__snippet").unwrap();

    for element in document.select(&result_selector) {
        let link = match element.select(&link_selector).next() {
            Some(a) => a,
            None => continue,
        };

        // Extract href — DDG uses redirect URLs, need to extract actual URL
        let href = match link.value().attr("href") {
            Some(h) => h,
            None => continue,
        };

        // DDG wraps URLs in a redirect: //duckduckgo.com/l/?uddg=ENCODED_URL&...
        let actual_url = extract_ddg_url(href);
        if actual_url.is_empty() {
            continue;
        }

        let title = link.text().collect::<String>().trim().to_string();

        let snippet = element
            .select(&snippet_selector)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        results.push(RawSearchResult {
            url: actual_url,
            title,
            snippet,
            engine: "duckduckgo".to_string(),
        });
    }

    tracing::debug!("DuckDuckGo: {} results parsed", results.len());
    results
}

/// Extract actual URL from DDG redirect format
fn extract_ddg_url(href: &str) -> String {
    // DDG format: //duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com&rut=...
    if let Some(pos) = href.find("uddg=") {
        let encoded = &href[pos + 5..];
        let end = encoded.find('&').unwrap_or(encoded.len());
        let encoded_url = &encoded[..end];
        match urlencoding::decode(encoded_url) {
            Ok(decoded) => return decoded.to_string(),
            Err(_) => {}
        }
    }

    // Direct URL (no redirect)
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }

    String::new()
}
