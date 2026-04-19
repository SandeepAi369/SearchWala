// ══════════════════════════════════════════════════════════════════════════════
// DuckDuckGo Search Engine — Multi-page HTML scraping
// ══════════════════════════════════════════════════════════════════════════════

use crate::models::RawSearchResult;
use reqwest::Client;
use scraper::{Html, Selector};
use std::collections::HashSet;

pub struct DuckDuckGo;

const PAGES: usize = 2;

#[async_trait::async_trait]
impl super::SearchEngine for DuckDuckGo {
    fn name(&self) -> &str {
        "duckduckgo"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        let mut all_results = Vec::new();
        let mut seen_urls: HashSet<String> = HashSet::new();

        for page in 0..PAGES {
            let offset = page * 20; // Lite offset approximation or just generic
            
            // lite.duckduckgo.com/lite/ takes POST with q, s, dc, etc.
            let mut params = vec![
                ("q".to_string(), query.to_string()),
            ];
            
            if page > 0 {
                params.push(("s".to_string(), offset.to_string()));
                params.push(("dc".to_string(), (offset + 1).to_string()));
            }

            let req = crate::config::apply_browser_headers(client
                .post("https://lite.duckduckgo.com/lite/"), "https://lite.duckduckgo.com/lite/");
            let resp = match req
                .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Origin", "https://lite.duckduckgo.com")
                .header("Referer", "https://lite.duckduckgo.com/")
                .form(&params)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("DuckDuckGo page {} request failed: {}", page, e);
                    break;
                }
            };

            let html_text = match resp.text().await {
                Ok(t) => t,
                Err(_) => break,
            };

            let page_results = parse_duckduckgo_lite_html(&html_text);
            if page_results.is_empty() {
                break;
            }

            for r in page_results {
                if seen_urls.insert(r.url.clone()) {
                    all_results.push(r);
                }
            }
        }

        tracing::info!("DuckDuckGo total: {} results across {} pages", all_results.len(), PAGES);
        all_results
    }
}

fn parse_duckduckgo_lite_html(html: &str) -> Vec<RawSearchResult> {
    let mut results = Vec::new();
    let document = Html::parse_document(html);
    let row_sel = Selector::parse("tr").unwrap();
    let title_sel = Selector::parse("a.result-snippet").unwrap();
    let snippet_sel = Selector::parse("td.result-snippet").unwrap();

    let mut current_title = String::new();
    let mut current_url = String::new();

    for row in document.select(&row_sel) {
        if let Some(a) = row.select(&title_sel).next() {
            if let Some(href) = a.value().attr("href") {
                if !href.starts_with('/') {
                    current_url = href.to_string();
                    current_title = a.text().collect::<String>().trim().to_string();
                }
            }
        } else if !current_url.is_empty() && !current_title.is_empty() {
            if let Some(td) = row.select(&snippet_sel).next() {
                let snippet = td.text().collect::<String>().trim().to_string();
                if !snippet.is_empty() {
                    results.push(RawSearchResult {
                        engine: "duckduckgo".to_string(),
                        title: current_title.clone(),
                        url: current_url.clone(),
                        snippet,
                        rank_position: results.len() + 1,
                    });
                }
                current_title = String::new();
                current_url = String::new();
            }
        }
    }
    
    results
}
