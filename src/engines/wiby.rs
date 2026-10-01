// ══════════════════════════════════════════════════════════════════════════════
// Wiby — indie / "smolweb" JSON API
// v8.0: goes through Fetcher so the status code is checked.
// ══════════════════════════════════════════════════════════════════════════════

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

pub struct Wiby;

#[async_trait::async_trait]
impl super::SearchEngine for Wiby {
    fn name(&self) -> &str {
        "wiby"
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        let url = format!("https://wiby.me/json/?q={}", urlencoding::encode(query));
        super::health::record_attempt("wiby");

        let (status, json) = match f.get_json(&url).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("Wiby error: {}", e);
                super::health::record_transport_err("wiby");
                return Vec::new();
            }
        };
        if status != 200 {
            super::health::record_block("wiby", status);
            return Vec::new();
        }

        let mut out = Vec::new();
        // Wiby returns either {"Results":[..]} or a bare array.
        let items = json
            .get("Results")
            .and_then(|v| v.as_array())
            .cloned()
            .or_else(|| json.as_array().cloned())
            .unwrap_or_default();

        for item in items {
            let url_s = item.get("URL").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            let title = item.get("Title").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            let snippet =
                item.get("Snippet").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            if url_s.is_empty() || title.is_empty() {
                continue;
            }
            out.push(RawSearchResult {
                url: url_s,
                title,
                snippet,
                engine: "wiby".to_string(),
                rank_position: out.len() + 1,
                published: None,
            });
        }

        super::health::record_parsed("wiby", out.len());
        tracing::info!("Wiby: {} results", out.len());
        out
    }
}
