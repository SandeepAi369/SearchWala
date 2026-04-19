// ══════════════════════════════════════════════════════════════════════════════
// Mojeek Search Engine — Multi-page HTML scraping (privacy-focused)
// Fixed selectors to match actual Mojeek 2025+ HTML structure
// ══════════════════════════════════════════════════════════════════════════════

use crate::models::RawSearchResult;
use reqwest::Client;
use scraper::{Html, Selector};
use std::collections::HashSet;

pub struct Mojeek;

const PAGES: usize = 5;

#[async_trait::async_trait]
impl super::SearchEngine for Mojeek {
    fn name(&self) -> &str {
        "mojeek"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        let mut all_results = Vec::new();
        let mut seen_urls: HashSet<String> = HashSet::new();

        for page in 1..=PAGES {
            let url = format!(
                "https://www.mojeek.com/search?q={}&s={}",
                urlencoding::encode(query),
                (page - 1) * 10 + 1
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
                    tracing::debug!("Mojeek page {} request failed: {}", page, e);
                    break;
                }
            };

            let html_text = match resp.text().await {
                Ok(t) => t,
                Err(_) => break,
            };

            let page_results = parse_mojeek_html(&html_text);
            if page_results.is_empty() {
                break;
            }

            for r in page_results {
                if seen_urls.insert(r.url.clone()) {
                    all_results.push(r);
                }
            }
        }

        tracing::info!("Mojeek total: {} results", all_results.len());
        all_results
    }
}

fn parse_mojeek_html(html: &str) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    // Mojeek 2025 structure:
    // <ul class="results-standard">
    //   <li class="r1">
    //     <a class="ob" href="...">...</a>      ← URL source  
    //     <h2><a class="title" href="...">Title</a></h2>  ← title
    //     <p class="s">snippet text</p>         ← snippet
    //   </li>
    // </ul>
    let li_selector = Selector::parse("ul.results-standard li").unwrap();
    let title_selector = Selector::parse("a.title, h2 a").unwrap();
    let snippet_selector = Selector::parse("p.s").unwrap();

    for element in document.select(&li_selector) {
        let link = match element.select(&title_selector).next() {
            Some(a) => a,
            None => continue,
        };

        let href = match link.value().attr("href") {
            Some(h) if h.starts_with("http") => h.to_string(),
            _ => continue,
        };

        if href.contains("mojeek.com") {
            continue;
        }

        let title = link.text().collect::<String>().trim().to_string();
        if title.is_empty() {
            continue;
        }

        let snippet = element
            .select(&snippet_selector)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        results.push(RawSearchResult {
            url: href,
            title,
            snippet,
            engine: "mojeek".to_string(),
            rank_position: results.len() + 1,
        });
    }

    results
}
