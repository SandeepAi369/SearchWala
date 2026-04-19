// ══════════════════════════════════════════════════════════════════════════════
// Wiby Search Engine — JSON API (indie/smolweb search)
// ══════════════════════════════════════════════════════════════════════════════

use crate::models::RawSearchResult;
use reqwest::Client;

pub struct Wiby;

#[async_trait::async_trait]
impl super::SearchEngine for Wiby {
    fn name(&self) -> &str {
        "wiby"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        let encoded = urlencoding::encode(query);
        let url = format!("https://wiby.me/json/?q={}", encoded);

        let req = crate::config::apply_browser_headers(client.get(&url), &url)
            .header("Accept", "application/json,text/plain,*/*");

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("Wiby request failed: {}", e);
                return Vec::new();
            }
        };

        let json: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };

        let mut out = Vec::new();

        // Wiby returns {"Results": [{...}, ...]}
        if let Some(results) = json.get("Results").and_then(|v| v.as_array()) {
            for item in results {
                let url = item.get("URL").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let title = item.get("Title").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let snippet = item.get("Snippet").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();

                if url.is_empty() || title.is_empty() {
                    continue;
                }

                out.push(RawSearchResult {
                    url,
                    title,
                    snippet,
                    engine: "wiby".to_string(),
                    rank_position: out.len() + 1,
                });
            }
        }

        out
    }
}
