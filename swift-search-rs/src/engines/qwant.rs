// ══════════════════════════════════════════════════════════════════════════════
// Qwant Search Engine — API JSON approach (Qwant has a clean API)
// ══════════════════════════════════════════════════════════════════════════════

use crate::config;
use crate::models::RawSearchResult;
use reqwest::Client;

pub struct Qwant;

#[async_trait::async_trait]
impl super::SearchEngine for Qwant {
    fn name(&self) -> &str {
        "qwant"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        // Qwant API endpoint (lite/web)
        let url = format!(
            "https://api.qwant.com/v3/search/web?q={}&count=20&locale=en_US&offset=0&device=desktop",
            urlencoding::encode(query)
        );
        let ua = config::random_user_agent();

        let resp = match client
            .get(&url)
            .header("User-Agent", ua)
            .header("Accept", "application/json")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Origin", "https://www.qwant.com")
            .header("Referer", "https://www.qwant.com/")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("Qwant request failed: {}", e);
                return vec![];
            }
        };

        let json: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("Qwant JSON parse failed: {}", e);
                return vec![];
            }
        };

        parse_qwant_json(&json)
    }
}

fn parse_qwant_json(json: &serde_json::Value) -> Vec<RawSearchResult> {
    let mut results = Vec::new();

    // Qwant response: { data: { result: { items: { mainline: [ { items: [...] } ] } } } }
    let items = json
        .pointer("/data/result/items/mainline")
        .and_then(|v| v.as_array());

    if let Some(mainline) = items {
        for group in mainline {
            if let Some(group_items) = group.get("items").and_then(|v| v.as_array()) {
                for item in group_items {
                    let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
                    let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    let desc = item.get("desc").and_then(|v| v.as_str()).unwrap_or("");

                    if !url.is_empty() && !title.is_empty() {
                        results.push(RawSearchResult {
                            url: url.to_string(),
                            title: title.to_string(),
                            snippet: desc.to_string(),
                            engine: "qwant".to_string(),
                        });
                    }
                }
            }
        }
    }

    tracing::debug!("Qwant: {} results parsed", results.len());
    results
}
