// ══════════════════════════════════════════════════════════════════════════════
// SearXNG upstream — optional, user-supplied instance
//
// WHY THIS EXISTS
// After Phase 2, five engine families produce results from a normal
// residential/datacenter IP. Reaching more is blocked by the engines
// themselves, not by our code: mojeek, ecosia, mwmbl and yacy all returned
// 403/303 on 2026-10-01, and google is JS-walled. No amount of parser work
// changes that.
//
// A SearXNG instance is the practical, legitimate way past it:
//   * it federates ~80 engines and the SearXNG project maintains those parsers,
//     so parser rot stops being our problem for those sources;
//   * a self-hosted instance uses the operator's own IP and rate budget;
//   * it is free and open-source.
//
// Public instances are NOT viable and we do not ship any. Measured 2026-10-01:
//   priv.au       429
//   opnxng.com    429
//   searxng.site  403
//   searx.be      200 but returns HTML (json format disabled)
// So this engine is OFF unless `SEARXNG_BASE_URL` is set, and the docs tell
// operators to self-host. Pointing it at someone else's instance without
// permission would be rude and unreliable.
//
// Setup:
//   docker run -d -p 8080:8080 -e SEARXNG_SECRET=$(openssl rand -hex 32) \
//     searxng/searxng
//   # enable the json format in settings.yml, then:
//   export SEARXNG_BASE_URL=http://localhost:8080
// ══════════════════════════════════════════════════════════════════════════════

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

pub struct SearxNG;

#[derive(serde::Deserialize)]
struct SxResponse {
    #[serde(default)]
    results: Vec<SxResult>,
}

#[derive(serde::Deserialize)]
struct SxResult {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
    /// Which upstream engines SearXNG got this from.
    #[serde(default)]
    engines: Vec<String>,
    /// SearXNG emits this key in camelCase.
    #[serde(default, rename = "publishedDate")]
    published_date: Option<String>,
}

/// Configured instance base URL, or None when the engine is disabled.
pub fn base_url() -> Option<String> {
    sanitize_base_url(&std::env::var("SEARXNG_BASE_URL").ok()?)
}

/// Testable core — takes the raw value instead of reading process-global env,
/// which races under the parallel test harness.
pub fn sanitize_base_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    // Must be a parseable http(s) URL — we are about to send user queries there.
    let u = url::Url::parse(trimmed).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        tracing::warn!("SEARXNG_BASE_URL must be http(s), ignoring: {trimmed}");
        return None;
    }
    Some(trimmed.to_string())
}

pub fn is_enabled() -> bool {
    base_url().is_some()
}

#[async_trait::async_trait]
impl super::SearchEngine for SearxNG {
    fn name(&self) -> &str {
        "searxng"
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        let Some(base) = base_url() else {
            return Vec::new();
        };
        let url = format!(
            "{base}/search?q={}&format=json&safesearch=0",
            urlencoding::encode(query)
        );
        super::health::record_attempt("searxng");

        let (status, json) = match f.get_json(&url).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("searxng transport error: {}", e);
                super::health::record_transport_err("searxng");
                return Vec::new();
            }
        };
        if status != 200 {
            tracing::warn!(
                "searxng: HTTP {} — is the `json` format enabled in settings.yml?",
                status
            );
            super::health::record_block("searxng", status);
            return Vec::new();
        }

        let parsed: SxResponse = match serde_json::from_str(&json.to_string()) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("searxng: JSON shape unexpected: {}", e);
                super::health::record_parsed("searxng", 0);
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
                snippet: r.content,
                // Preserve which upstream engine the hit came from, prefixed so
                // `engine_family` can still group it. SearXNG results that came
                // from Bing must NOT count as independent evidence alongside
                // our own Bing results.
                engine: match r.engines.first() {
                    Some(up) => format!("searxng:{up}"),
                    None => "searxng".to_string(),
                },
                rank_position: i + 1,
                published: r
                    .published_date
                    .as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&chrono::Utc)),
                full_text: None,
            })
            .collect();

        super::health::record_parsed("searxng", out.len());
        tracing::info!("searxng: {} results from {}", out.len(), base);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_blank_is_disabled() {
        assert_eq!(sanitize_base_url(""), None);
        assert_eq!(sanitize_base_url("   "), None);
    }

    #[test]
    fn rejects_non_http_schemes() {
        assert_eq!(sanitize_base_url("file:///etc/passwd"), None);
        assert_eq!(sanitize_base_url("not a url"), None);
        assert_eq!(sanitize_base_url("ftp://x/"), None);
    }

    #[test]
    fn strips_trailing_slash() {
        assert_eq!(sanitize_base_url("http://localhost:8080/").as_deref(), Some("http://localhost:8080"));
        assert_eq!(sanitize_base_url("https://searx.example/ ").as_deref(), Some("https://searx.example"));
    }

    #[test]
    fn upstream_engine_is_preserved_for_family_grouping() {
        // A SearXNG hit sourced from Bing must group into the bing family, so
        // federating cannot silently double-count the same index alongside our
        // own Bing results.
        assert_eq!(crate::engines::engine_family("searxng:bing"), "bing");
        assert_eq!(crate::engines::engine_family("searxng:google"), "google");
        assert_eq!(crate::engines::engine_family("searxng:mojeek"), "mojeek");
        // Unknown upstream gets its own family rather than being lumped in.
        assert_eq!(crate::engines::engine_family("searxng"), "searxng");
        assert_eq!(crate::engines::engine_family("searxng:somethingnew"), "searxng");
    }
}
