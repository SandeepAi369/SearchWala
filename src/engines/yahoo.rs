// ══════════════════════════════════════════════════════════════════════════════
// Yahoo Search Engine — Multi-page HTML scraping
// ══════════════════════════════════════════════════════════════════════════════

use crate::models::RawSearchResult;
use reqwest::Client;
use scraper::{Html, Selector};
use std::collections::HashSet;

pub struct Yahoo;

const PAGES: usize = 5;

#[async_trait::async_trait]
impl super::SearchEngine for Yahoo {
    fn name(&self) -> &str {
        "yahoo"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        let mut all_results = Vec::new();
        let mut seen_urls: HashSet<String> = HashSet::new();

        for page in 0..PAGES {
            let start = page * 10 + 1; // Yahoo uses 1-based b= param
            let url = format!(
                "https://search.yahoo.com/search?p={}&n=20&b={}",
                urlencoding::encode(query),
                start
            );

            let req = crate::config::apply_browser_headers(client
                .get(&url), &url);
            let resp = match req
                .header("Accept", "text/html,application/xhtml+xml")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("Yahoo page {} request failed: {}", page, e);
                    break;
                }
            };

            let html_text = match resp.text().await {
                Ok(t) => t,
                Err(_) => break,
            };

            let page_results = parse_yahoo_html(&html_text);
            if page_results.is_empty() {
                break;
            }

            for r in page_results {
                if seen_urls.insert(r.url.clone()) {
                    all_results.push(r);
                }
            }
        }

        tracing::info!("Yahoo total: {} results", all_results.len());
        all_results
    }
}

fn parse_yahoo_html(html: &str) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

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

    results
}

fn extract_yahoo_url(href: &str) -> String {
    if let Some(pos) = href.find("RU=") {
        let start = pos + 3;
        let end = href[start..].find('/').map(|e| start + e).unwrap_or(href.len());
        let encoded = &href[start..end];
        if let Ok(decoded) = urlencoding::decode(encoded) {
            return decoded.to_string();
        }
    }

    if let Some(pos) = href.rfind("/*http") {
        let url = &href[pos + 2..];
        return url.to_string();
    }

    if href.starts_with("http://") || href.starts_with("https://") {
        if !href.contains("yahoo.com") {
            return href.to_string();
        }
    }

    String::new()
}
