// ══════════════════════════════════════════════════════════════════════════════
// Marginalia Search — public JSON API
//
// Verified live 2026-10-01: HTTP 200, clean JSON, 11 pages of results, no key
// required. This replaces the v7.0 `marginalia` HTML scraper, which pointed at
// `search.marginalia.nu/search?query=` and relied on the generic link-scraper.
//
//   GET https://api.marginalia-search.com/public/search/{url-encoded-query}
//   -> { "license": "...", "query": "...", "page": 1, "pages": 11,
//        "results": [ { "url", "title", "description", ... } ] }
//
// Results are licensed CC-BY-NC-SA 4.0 — attribution is surfaced in the UI.
// The "public" key is rate-limited; a dedicated key can be set via
// MARGINALIA_API_KEY.
// ══════════════════════════════════════════════════════════════════════════════

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

pub struct MarginaliaApi;

#[derive(serde::Deserialize)]
struct MgResponse {
    #[serde(default)]
    results: Vec<MgResult>,
}

#[derive(serde::Deserialize)]
struct MgResult {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
}

fn api_key() -> String {
    std::env::var("MARGINALIA_API_KEY").unwrap_or_else(|_| "public".to_string())
}

#[async_trait::async_trait]
impl super::SearchEngine for MarginaliaApi {
    fn name(&self) -> &str {
        "marginalia_api"
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        super::health::record_attempt(self.name());

        let url = format!(
            "https://api.marginalia-search.com/{}/search/{}",
            api_key(),
            urlencoding::encode(query)
        );

        let page = match f.get(&url, &[("Accept", "application/json")]).await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("marginalia_api transport error: {}", e);
                super::health::record_transport_err(self.name());
                return Vec::new();
            }
        };

        if page.status != 200 {
            tracing::warn!("marginalia_api: HTTP {} — treating as blocked", page.status);
            super::health::record_block(self.name(), page.status);
            return Vec::new();
        }

        let parsed: MgResponse = match serde_json::from_str(&page.body) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("marginalia_api: JSON parse failed: {}", e);
                super::health::record_parsed(self.name(), 0);
                return Vec::new();
            }
        };

        let out: Vec<RawSearchResult> = parsed
            .results
            .into_iter()
            .filter(|r| !r.url.is_empty() && !r.title.is_empty())
            .enumerate()
            .map(|(i, r)| RawSearchResult {
                url: r.url,
                title: r.title,
                snippet: r.description,
                engine: "marginalia_api".to_string(),
                rank_position: i + 1,
                published: None,
                full_text: None,
            })
            .collect();

        super::health::record_parsed(self.name(), out.len());
        tracing::info!("marginalia_api: {} results", out.len());
        out
    }
}
