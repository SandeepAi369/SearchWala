// ══════════════════════════════════════════════════════════════════════════════
// Brave Search Engine — Multi-page HTML scraping
// ══════════════════════════════════════════════════════════════════════════════

use crate::models::RawSearchResult;
use reqwest::Client;
use scraper::{Html, Selector};
use std::collections::HashSet;

pub struct Brave;

const PAGES: usize = 3;

#[async_trait::async_trait]
impl super::SearchEngine for Brave {
    fn name(&self) -> &str {
        "brave"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        let mut all_results = Vec::new();
        let mut seen_urls: HashSet<String> = HashSet::new();

        for page in 0..PAGES {
            let offset = page * 10;
            let url = format!(
                "https://search.brave.com/search?q={}&source=web&offset={}",
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
                    tracing::debug!("Brave page {} request failed: {}", page, e);
                    break;
                }
            };

            let html_text = match resp.text().await {
                Ok(t) => t,
                Err(_) => break,
            };

            let page_results = parse_brave_html(&html_text);
            if page_results.is_empty() {
                break;
            }

            for r in page_results {
                if seen_urls.insert(r.url.clone()) {
                    all_results.push(r);
                }
            }
        }

        tracing::info!("Brave total: {} results", all_results.len());
        all_results
    }
}

fn parse_brave_html(html: &str) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    // Primary selector
    let result_selector = Selector::parse("#results .snippet").unwrap();
    let title_link_selector = Selector::parse(".snippet-title a, .result-header a, .heading-serpresult a").unwrap();
    let desc_selector = Selector::parse(".snippet-description, .snippet-content, .result-snippet").unwrap();

    for element in document.select(&result_selector) {
        let link = match element.select(&title_link_selector).next() {
            Some(a) => a,
            None => continue,
        };

        let href = match link.value().attr("href") {
            Some(h) if h.starts_with("http") => h.to_string(),
            _ => continue,
        };

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
            url: href,
            title,
            snippet,
            engine: "brave".to_string(),
        });
    }

    // Fallback: try generic link extraction if structured selectors miss
    if results.is_empty() {
        let any_link = Selector::parse("a[href]").unwrap();
        for link in document.select(&any_link) {
            if let Some(href) = link.value().attr("href") {
                if href.starts_with("https://") && !href.contains("brave.com") {
                    let title = link.text().collect::<String>().trim().to_string();
                    if !title.is_empty() && title.len() > 5 {
                        results.push(RawSearchResult {
                            url: href.to_string(),
                            title,
                            snippet: String::new(),
                            engine: "brave".to_string(),
                        });
                    }
                }
            }
        }
    }

    results
}
