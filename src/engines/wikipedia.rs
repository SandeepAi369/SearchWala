// ══════════════════════════════════════════════════════════════════════════════
// Wikipedia — full-text search API
//
// v8.0 FIX: v7.0 used `action=opensearch`, which is a TITLE-PREFIX AUTOCOMPLETE
// endpoint, not a search endpoint. Measured 2026-10-01:
//
//   query                              opensearch   query&list=search
//   "cricket world cup 2027 schedule"       0              20
//   "who is the ceo of google"              0              20
//
// Wikipedia was therefore returning zero results for essentially every
// natural-language query — silently, since zero results looks identical to
// "nothing matched". It only worked for queries that happened to be an exact
// article-title prefix.
//
// `action=query&list=search` is the real full-text index. It also gives us a
// snippet and the page timestamp, which opensearch does not.
//
// WIKIPEDIA_LANGS (default "en") can fan out to other language editions, e.g.
// "en,hi" to cover Hindi queries.
// ══════════════════════════════════════════════════════════════════════════════

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

pub struct Wikipedia;

fn languages() -> Vec<String> {
    parse_languages(&std::env::var("WIKIPEDIA_LANGS").unwrap_or_else(|_| "en".to_string()))
}

/// Testable core — avoids process-global env mutation in tests.
fn parse_languages(raw: &str) -> Vec<String> {
    let out: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_lowercase())
        // Guard against injection into the hostname.
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .take(4)
        .collect();
    if out.is_empty() { vec!["en".to_string()] } else { out }
}

/// Strip the `<span class="searchmatch">` highlighting Wikipedia wraps around
/// matched terms in snippets.
fn strip_search_highlight(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[async_trait::async_trait]
impl super::SearchEngine for Wikipedia {
    fn name(&self) -> &str {
        "wikipedia"
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        let encoded = urlencoding::encode(query);
        let mut out = Vec::new();

        for lang in languages() {
            let url = format!(
                "https://{lang}.wikipedia.org/w/api.php?action=query&list=search\
                 &srsearch={encoded}&srlimit=20&srprop=snippet|timestamp&format=json&formatversion=2"
            );
            super::health::record_attempt("wikipedia");

            let (status, json) = match f.get_json(&url).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!("Wikipedia({}) error: {}", lang, e);
                    super::health::record_transport_err("wikipedia");
                    continue;
                }
            };
            if status != 200 {
                super::health::record_block("wikipedia", status);
                continue;
            }

            let before = out.len();
            let hits = json
                .get("query")
                .and_then(|q| q.get("search"))
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default();

            for hit in hits {
                let title = hit.get("title").and_then(|v| v.as_str()).unwrap_or("").trim();
                if title.is_empty() {
                    continue;
                }
                let snippet = hit
                    .get("snippet")
                    .and_then(|v| v.as_str())
                    .map(strip_search_highlight)
                    .unwrap_or_default();
                let published = hit
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&chrono::Utc));

                // Build the canonical article URL from the title.
                let url = format!(
                    "https://{lang}.wikipedia.org/wiki/{}",
                    urlencoding::encode(&title.replace(' ', "_"))
                );

                out.push(RawSearchResult {
                    url,
                    title: title.to_string(),
                    snippet,
                    engine: "wikipedia".to_string(),
                    rank_position: out.len() + 1,
                    published,
                });
            }
            super::health::record_parsed("wikipedia", out.len() - before);
        }

        tracing::info!("Wikipedia: {} results", out.len());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_searchmatch_highlighting() {
        let raw = r#"The 2027 <span class="searchmatch">Cricket</span> World Cup will be held"#;
        assert_eq!(
            strip_search_highlight(raw),
            "The 2027 Cricket World Cup will be held"
        );
    }

    #[test]
    fn language_list_rejects_injection() {
        // Only alphanumeric + '-' survive, so a crafted value cannot escape
        // into the hostname we format into the request URL.
        let langs = parse_languages("en,hi,../../evil,bad host,xx");
        assert!(langs.contains(&"en".to_string()));
        assert!(langs.contains(&"hi".to_string()));
        assert!(
            !langs.iter().any(|l| l.contains('/') || l.contains(' ') || l.contains('.')),
            "unsafe language code survived: {langs:?}"
        );
    }

    #[test]
    fn default_language_is_en() {
        assert_eq!(parse_languages("en"), vec!["en".to_string()]);
        assert_eq!(parse_languages(""), vec!["en".to_string()]);
        assert_eq!(parse_languages(",,,"), vec!["en".to_string()]);
    }

    #[test]
    fn language_fanout_is_capped() {
        let langs = parse_languages("en,hi,fr,de,es,it,ja");
        assert!(langs.len() <= 4, "fan-out must be capped: {langs:?}");
    }
}
