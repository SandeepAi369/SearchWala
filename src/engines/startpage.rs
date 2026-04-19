// ══════════════════════════════════════════════════════════════════════════════
// Startpage Search Engine — Multi-page HTML scraping (privacy-focused proxy)
// Uses correct 2025+ class selectors
// ══════════════════════════════════════════════════════════════════════════════

use crate::models::RawSearchResult;
use reqwest::Client;
use scraper::{Html, Selector};
use std::collections::HashSet;

pub struct Startpage;

const PAGES: usize = 5;

#[async_trait::async_trait]
impl super::SearchEngine for Startpage {
    fn name(&self) -> &str {
        "startpage"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        let mut all_results = Vec::new();
        let mut seen_urls: HashSet<String> = HashSet::new();

        for page in 1..=PAGES {

            let req = crate::config::apply_browser_headers(client
                .post("https://www.startpage.com/sp/search"), "https://www.startpage.com/sp/search");
            let resp = match req
                .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
                .header("Accept-Language", "en-US,en;q=0.9")
                .header("Referer", "https://www.startpage.com/")
                .form(&[
                    ("query", query),
                    ("cat", "web"),
                    ("language", "english"),
                    ("page", &page.to_string()),
                ])
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("Startpage page {} request failed: {}", page, e);
                    break;
                }
            };

            let html_text = match resp.text().await {
                Ok(t) => t,
                Err(_) => break,
            };

            let page_results = parse_startpage_html(&html_text);
            if page_results.is_empty() && page > 1 {
                break;
            }

            for r in page_results {
                if seen_urls.insert(r.url.clone()) {
                    all_results.push(r);
                }
            }
        }

        tracing::info!("Startpage total: {} results", all_results.len());
        all_results
    }
}

fn parse_startpage_html(html: &str) -> Vec<RawSearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();

    // 2025 Startpage uses dynamic CSS class names but the result structure is:
    //   <div class="result css-...">
    //     <a class="result-title result-link css-..." href="...">Title</a>
    //     <p class="description css-...">Snippet</p>
    //   </div>
    //
    // We match on partial class names that are stable:
    //   - "result-link" for the title link
    //   - "description" for the snippet

    // Strategy 1: Find all result-link anchors
    let link_selector = Selector::parse("a.result-link, a[class*='result-link']").unwrap();
    let all_links: Vec<_> = document.select(&link_selector).collect();

    if !all_links.is_empty() {
        for link in all_links {
            let href = match link.value().attr("href") {
                Some(h) if h.starts_with("http") && !h.contains("startpage.com") => h.to_string(),
                _ => continue,
            };

            let title = link.text().collect::<String>().trim().to_string();
            if title.is_empty() || title.len() < 5 {
                continue;
            }

            // Try to find the snippet from the parent or sibling
            let snippet = String::new(); // Startpage snippets are in separate elements

            results.push(RawSearchResult {
                url: href,
                title,
                snippet,
                engine: "startpage".to_string(),
                rank_position: results.len() + 1,
            });
        }
    }

    // Strategy 2: Fallback — extract all external HTTPS links with meaningful text
    if results.is_empty() {
        let any_link = Selector::parse("a[href]").unwrap();
        let mut seen = HashSet::new();
        
        for link in document.select(&any_link) {
            if let Some(href) = link.value().attr("href") {
                if href.starts_with("https://")
                    && !href.contains("startpage.com")
                    && !href.contains("google.com")
                    && !href.contains("bing.com")
                {
                    let title = link.text().collect::<String>().trim().to_string();
                    if title.len() > 10 && seen.insert(href.to_string()) {
                        results.push(RawSearchResult {
                            url: href.to_string(),
                            title,
                            snippet: String::new(),
                            engine: "startpage".to_string(),
                            rank_position: results.len() + 1,
                        });
                    }
                }
            }
        }
    }

    results
}
