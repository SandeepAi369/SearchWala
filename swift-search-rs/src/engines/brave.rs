// ══════════════════════════════════════════════════════════════════════════════
// Brave Search Engine — HTML scraping
// ══════════════════════════════════════════════════════════════════════════════

use crate::config;
use crate::models::RawSearchResult;
use reqwest::Client;
use scraper::{Html, Selector};

pub struct Brave;

#[async_trait::async_trait]
impl super::SearchEngine for Brave {
    fn name(&self) -> &str {
        "brave"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        let url = format!(
            "https://search.brave.com/search?q={}&source=web",
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
                tracing::debug!("Brave request failed: {}", e);
                return vec![];
            }
        };

        let html_text = match resp.text().await {
            Ok(t) => t,
            Err(_) => return vec![],
        };

        parse_brave_html(&html_text)
    }
}

fn parse_brave_html(html: &str) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    // Brave search results are in #results .snippet
    // Each result has .snippet-title a for the link and .snippet-description for text
    let result_selector = Selector::parse("#results .snippet").unwrap();
    let title_link_selector = Selector::parse(".snippet-title a, .result-header a").unwrap();
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

    tracing::debug!("Brave: {} results parsed", results.len());
    results
}
