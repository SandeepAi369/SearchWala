// ══════════════════════════════════════════════════════════════════════════════
// Yahoo Search Engine — HTML scraping
// ══════════════════════════════════════════════════════════════════════════════

use crate::config;
use crate::models::RawSearchResult;
use reqwest::Client;
use scraper::{Html, Selector};

pub struct Yahoo;

#[async_trait::async_trait]
impl super::SearchEngine for Yahoo {
    fn name(&self) -> &str {
        "yahoo"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        let url = format!(
            "https://search.yahoo.com/search?p={}&n=20",
            urlencoding::encode(query)
        );
        let ua = config::random_user_agent();

        let resp = match client
            .get(&url)
            .header("User-Agent", ua)
            .header("Accept", "text/html,application/xhtml+xml")
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("Yahoo request failed: {}", e);
                return vec![];
            }
        };

        let html_text = match resp.text().await {
            Ok(t) => t,
            Err(_) => return vec![],
        };

        parse_yahoo_html(&html_text)
    }
}

fn parse_yahoo_html(html: &str) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    // Yahoo uses .algo-sr for search results
    // Links are in h3.title a or .compTitle a
    let result_selector = Selector::parse(".algo-sr, .dd.algo, li.ov-a").unwrap();
    let title_selector = Selector::parse("h3 a, .compTitle a, .title a").unwrap();
    let desc_selector = Selector::parse(".compText p, .fc-falcon, .lh-l").unwrap();

    for element in document.select(&result_selector) {
        let link = match element.select(&title_selector).next() {
            Some(a) => a,
            None => continue,
        };

        let href = match link.value().attr("href") {
            Some(h) => h.to_string(),
            None => continue,
        };

        // Yahoo uses redirect URLs — extract the actual URL
        let actual_url = extract_yahoo_url(&href);
        if actual_url.is_empty() {
            continue;
        }

        let title = link.text().collect::<String>().trim().to_string();
        if title.is_empty() {
            continue;
        }

        let snippet = element
            .select(&desc_selector)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        results.push(RawSearchResult {
            url: actual_url,
            title,
            snippet,
            engine: "yahoo".to_string(),
        });
    }

    tracing::debug!("Yahoo: {} results parsed", results.len());
    results
}

/// Extract actual URL from Yahoo redirect
fn extract_yahoo_url(href: &str) -> String {
    // Yahoo redirect: https://r.search.yahoo.com/.../*https://example.com
    if let Some(pos) = href.find("RU=") {
        let start = pos + 3;
        let end = href[start..].find('/').map(|e| start + e).unwrap_or(href.len());
        let encoded = &href[start..end];
        if let Ok(decoded) = urlencoding::decode(encoded) {
            return decoded.to_string();
        }
    }

    // Try finding /**http pattern
    if let Some(pos) = href.rfind("/*http") {
        let url = &href[pos + 2..]; // skip /*
        return url.to_string();
    }

    // Direct URL
    if href.starts_with("http://") || href.starts_with("https://") {
        if !href.contains("yahoo.com") {
            return href.to_string();
        }
    }

    String::new()
}
