// ══════════════════════════════════════════════════════════════════════════════
// Mojeek Search Engine — HTML scraping (privacy-focused, no tracking)
// ══════════════════════════════════════════════════════════════════════════════

use crate::config;
use crate::models::RawSearchResult;
use reqwest::Client;
use scraper::{Html, Selector};

pub struct Mojeek;

#[async_trait::async_trait]
impl super::SearchEngine for Mojeek {
    fn name(&self) -> &str {
        "mojeek"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        let url = format!(
            "https://www.mojeek.com/search?q={}",
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
                tracing::debug!("Mojeek request failed: {}", e);
                return vec![];
            }
        };

        let html_text = match resp.text().await {
            Ok(t) => t,
            Err(_) => return vec![],
        };

        parse_mojeek_html(&html_text)
    }
}

fn parse_mojeek_html(html: &str) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    // Mojeek uses <ul class="results-standard"> <li> for each result
    // Each li has a.title for the link and p.s for the description
    let result_selector = Selector::parse("ul.results-standard li, .result-col").unwrap();
    let title_selector = Selector::parse("a.ob, a.title, h2 a").unwrap();
    let desc_selector = Selector::parse("p.s, .result-desc").unwrap();

    for element in document.select(&result_selector) {
        let link = match element.select(&title_selector).next() {
            Some(a) => a,
            None => continue,
        };

        let href = match link.value().attr("href") {
            Some(h) if h.starts_with("http") => h.to_string(),
            _ => continue,
        };

        // Skip Mojeek's own links
        if href.contains("mojeek.com") {
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
            url: href,
            title,
            snippet,
            engine: "mojeek".to_string(),
        });
    }

    tracing::debug!("Mojeek: {} results parsed", results.len());
    results
}
