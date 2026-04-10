// ══════════════════════════════════════════════════════════════════════════════
// Qwant Search Engine — Multi-page HTML scraping
// ══════════════════════════════════════════════════════════════════════════════

use crate::models::RawSearchResult;
use reqwest::Client;
use scraper::{Html, Selector};
use std::collections::HashSet;

pub struct Qwant;

const PAGES: usize = 3; // Qwant's HTML is JS-heavy, fewer pages are more reliable

#[async_trait::async_trait]
impl super::SearchEngine for Qwant {
    fn name(&self) -> &str {
        "qwant"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        let mut all_results = Vec::new();
        let mut seen_urls: HashSet<String> = HashSet::new();

        for page in 0..PAGES {
            let offset = page * 10;
            let url = format!(
                "https://www.qwant.com/?q={}&t=web&offset={}",
                urlencoding::encode(query),
                offset
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
                    tracing::debug!("Qwant page {} request failed: {}", page, e);
                    break;
                }
            };

            let html_text = match resp.text().await {
                Ok(t) => t,
                Err(_) => break,
            };

            let page_results = parse_qwant_html(&html_text);
            // Don't break if empty on first page — Qwant is JS-heavy
            if page_results.is_empty() && page > 0 {
                break;
            }

            for r in page_results {
                if seen_urls.insert(r.url.clone()) {
                    all_results.push(r);
                }
            }
        }

        tracing::info!("Qwant total: {} results", all_results.len());
        all_results
    }
}

fn parse_qwant_html(html: &str) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    // Generic approach: find all links that look like search results
    let a_sel = Selector::parse("a[href]").unwrap();
    
    for link in document.select(&a_sel) {
        let href = match link.value().attr("href") {
            Some(h) => h,
            None => continue,
        };

        if !href.starts_with("https://") || href.contains("qwant.com") {
            continue;
        }

        if href.contains("google.com") || href.contains("bing.com") {
            continue;
        }

        let title = link.text().collect::<String>().trim().to_string();
        
        if title.len() < 5 {
            continue;
        }

        if !seen.insert(href.to_string()) {
            continue;
        }

        results.push(RawSearchResult {
            url: href.to_string(),
            title,
            snippet: String::new(),
            engine: "qwant".to_string(),
        });
    }

    results
}
