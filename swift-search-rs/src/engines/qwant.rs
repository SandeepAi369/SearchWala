// ══════════════════════════════════════════════════════════════════════════════
// Qwant Search Engine — HTML scraping fallback
// Qwant's internal API is unreliable and undocumented.
// This implementation scrapes the HTML search results page instead.
// ══════════════════════════════════════════════════════════════════════════════

use crate::config;
use crate::models::RawSearchResult;
use reqwest::Client;
use scraper::{Html, Selector};

pub struct Qwant;

#[async_trait::async_trait]
impl super::SearchEngine for Qwant {
    fn name(&self) -> &str {
        "qwant"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        // Use Qwant's web interface and scrape the HTML
        let url = format!(
            "https://www.qwant.com/?q={}&t=web",
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
                tracing::debug!("Qwant request failed: {}", e);
                return vec![];
            }
        };

        let html_text = match resp.text().await {
            Ok(t) => t,
            Err(_) => return vec![],
        };

        parse_qwant_html(&html_text)
    }
}

fn parse_qwant_html(html: &str) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    // Qwant renders results in various structures — try multiple selectors
    let selectors_to_try = [
        // Modern Qwant layout
        ("a[data-testid='serp-result-title']", "", ""),
        // Fallback generic
        (".result a[href]", ".result__desc", ""),
    ];

    // Generic approach: find all links that look like search results
    let a_sel = Selector::parse("a[href]").unwrap();
    
    for link in document.select(&a_sel) {
        let href = match link.value().attr("href") {
            Some(h) => h,
            None => continue,
        };

        // Must be external HTTPS link, not Qwant internal
        if !href.starts_with("https://") || href.contains("qwant.com") {
            continue;
        }

        // Skip common non-result links
        if href.contains("google.com") || href.contains("bing.com") {
            continue;
        }

        let title = link.text().collect::<String>().trim().to_string();
        
        // Only keep links with meaningful titles (>5 chars)
        if title.len() < 5 {
            continue;
        }

        // Avoid duplicate URLs
        let already_exists = results.iter().any(|r: &RawSearchResult| r.url == href);
        if already_exists {
            continue;
        }

        results.push(RawSearchResult {
            url: href.to_string(),
            title,
            snippet: String::new(),
            engine: "qwant".to_string(),
        });
    }

    tracing::debug!("Qwant: {} results parsed", results.len());
    results
}
