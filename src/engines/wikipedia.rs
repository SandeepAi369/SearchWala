use crate::models::RawSearchResult;
use reqwest::Client;

pub struct Wikipedia;

#[async_trait::async_trait]
impl super::SearchEngine for Wikipedia {
    fn name(&self) -> &str {
        "wikipedia"
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        let encoded = urlencoding::encode(query);
        let url = format!(
            "https://en.wikipedia.org/w/api.php?action=opensearch&search={}&limit=20&namespace=0&format=json",
            encoded
        );

        let req = crate::config::apply_browser_headers(client.get(&url), &url)
            .header("Accept", "application/json,text/plain,*/*");

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("Wikipedia request failed: {}", e);
                return Vec::new();
            }
        };

        let json: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };

        let titles = json.get(1).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let snippets = json.get(2).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let links = json.get(3).and_then(|v| v.as_array()).cloned().unwrap_or_default();

        let mut out = Vec::new();
        let max_len = titles.len().min(snippets.len()).min(links.len());

        for i in 0..max_len {
            let title = titles[i].as_str().unwrap_or("").trim().to_string();
            let snippet = snippets[i].as_str().unwrap_or("").trim().to_string();
            let link = links[i].as_str().unwrap_or("").trim().to_string();

            if title.is_empty() || link.is_empty() {
                continue;
            }

            out.push(RawSearchResult {
                url: link,
                title,
                snippet,
                engine: "wikipedia".to_string(),
            });
        }

        out
    }
}
