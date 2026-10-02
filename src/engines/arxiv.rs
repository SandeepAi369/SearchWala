// ══════════════════════════════════════════════════════════════════════════════
// arXiv — preprint search via the public Atom API
//
// Verified live 2026-10-02: HTTP 200 in ~0.8 s.
//
//   GET https://export.arxiv.org/api/query?search_query=...&max_results=N
//
// Each entry's abstract becomes `full_text`, so no PDF needs to be fetched.
// Results are labelled as preprints so the Academic synthesis can distinguish
// them from peer-reviewed work.
// ══════════════════════════════════════════════════════════════════════════════

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

pub struct Arxiv;

const MAX_RESULTS: usize = 15;

/// Words that carry no search value in an arXiv `all:` query.
const STOP: &[&str] = &[
    "the", "and", "for", "with", "what", "how", "does", "are", "is", "of", "in",
    "on", "to", "a", "an", "about", "explain", "review", "latest", "work", "works",
];

/// Build an arXiv `search_query` from free text: significant words joined
/// with AND. arXiv treats a bare multi-word string as a phrase, which almost
/// never matches.
pub fn build_query(q: &str) -> Option<String> {
    let words: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .map(|w| w.trim().to_lowercase())
        .filter(|w| w.chars().count() >= 3 && !STOP.contains(&w.as_str()))
        .take(4)
        .collect();
    if words.is_empty() {
        return None;
    }
    Some(words.iter().map(|w| format!("all:{w}")).collect::<Vec<_>>().join(" AND "))
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Text of the first `<tag>…</tag>` inside `block`.
fn tag_text<'a>(block: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let start = block.find(&open)?;
    let gt = block[start..].find('>')? + start + 1;
    let close = format!("</{tag}>");
    let end = block[gt..].find(&close)? + gt;
    Some(&block[gt..end])
}

fn collapse(s: &str) -> String {
    decode_entities(s).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse an arXiv Atom feed.
pub fn parse_feed(xml: &str) -> Vec<RawSearchResult> {
    let mut out = Vec::new();
    for (i, entry) in xml.split("<entry>").skip(1).enumerate() {
        let entry = entry.split("</entry>").next().unwrap_or(entry);
        let Some(id) = tag_text(entry, "id").map(|s| s.trim().to_string()) else { continue };
        if !id.starts_with("http") {
            continue;
        }
        let title = tag_text(entry, "title").map(collapse).unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let summary = tag_text(entry, "summary").map(collapse).unwrap_or_default();
        let published = tag_text(entry, "published")
            .and_then(|d| chrono::DateTime::parse_from_rfc3339(d.trim()).ok())
            .map(|d| d.with_timezone(&chrono::Utc));
        let authors: Vec<String> = entry
            .split("<author>")
            .skip(1)
            .filter_map(|a| tag_text(a, "name").map(collapse))
            .take(4)
            .collect();
        let year = published.map(|d| d.format("%Y").to_string()).unwrap_or_default();

        // https://arxiv.org/abs/2101.00001v2 -> canonical without the version.
        let url = id.replace("http://", "https://");
        let url = match url.rfind('v') {
            Some(p) if url[p + 1..].chars().all(|c| c.is_ascii_digit()) && p > url.len().saturating_sub(4) => {
                url[..p].to_string()
            }
            _ => url,
        };

        let mut header = format!("Preprint (arXiv, not peer-reviewed): {title}");
        if !year.is_empty() {
            header.push_str(&format!(" ({year})"));
        }
        if !authors.is_empty() {
            header.push_str(&format!(". Authors: {}", authors.join(", ")));
        }
        header.push('.');

        let full_text = if crate::util::char_len(&summary) >= 150 {
            Some(format!("{header}\n\nAbstract: {summary}"))
        } else {
            None
        };

        out.push(RawSearchResult {
            url,
            title: if year.is_empty() { format!("{title} (arXiv)") } else { format!("{title} (arXiv {year})") },
            snippet: crate::util::truncate_chars(&summary, 280).to_string(),
            engine: "arxiv".to_string(),
            rank_position: i + 1,
            published,
            full_text,
        });
    }
    out
}

#[async_trait::async_trait]
impl super::SearchEngine for Arxiv {
    fn name(&self) -> &str {
        "arxiv"
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        let Some(sq) = build_query(query) else { return Vec::new() };
        super::health::record_attempt("arxiv");
        let url = format!(
            "https://export.arxiv.org/api/query?search_query={}&start=0&max_results={}&sortBy=relevance",
            urlencoding::encode(&sq),
            MAX_RESULTS
        );
        let page = match f.get(&url, &[("Accept", "application/atom+xml")]).await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("arxiv error: {}", e);
                super::health::record_transport_err("arxiv");
                return Vec::new();
            }
        };
        if page.status != 200 {
            super::health::record_block("arxiv", page.status);
            return Vec::new();
        }
        let out = parse_feed(&page.body);
        super::health::record_parsed("arxiv", out.len());
        tracing::info!("arxiv: {} results", out.len());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>http://arxiv.org/abs/2101.00001v2</id>
    <published>2021-01-01T00:00:00Z</published>
    <title>Opportunities and challenges of
      mRNA technologies</title>
    <summary>  We review mRNA platforms &amp; delivery vehicles, covering lipid nanoparticles,
      stability, immunogenicity, manufacturing scale-up and the regulatory path for
      next-generation vaccines and therapeutics across infectious disease and oncology. </summary>
    <author><name>A. Author</name></author>
    <author><name>B. Author</name></author>
  </entry>
  <entry>
    <id>not-a-url</id><title>skip me</title>
  </entry>
</feed>"#;

    #[test]
    fn parses_entries_and_skips_bad_ones() {
        let r = parse_feed(FEED);
        assert_eq!(r.len(), 1);
        let e = &r[0];
        assert_eq!(e.url, "https://arxiv.org/abs/2101.00001", "version suffix stripped, https");
        assert_eq!(e.title, "Opportunities and challenges of mRNA technologies (arXiv 2021)");
        assert!(e.snippet.contains("platforms & delivery"), "entities decoded: {}", e.snippet);
        let ft = e.full_text.as_ref().unwrap();
        assert!(ft.starts_with("Preprint (arXiv, not peer-reviewed)"));
        assert!(ft.contains("A. Author, B. Author"));
        assert!(e.published.is_some());
    }

    #[test]
    fn builds_and_queries_from_significant_words() {
        assert_eq!(
            build_query("how does mRNA vaccine technology work").as_deref(),
            Some("all:mrna AND all:vaccine AND all:technology")
        );
        assert_eq!(build_query("what is a"), None);
    }

    #[test]
    fn empty_feed_is_empty() {
        assert!(parse_feed("").is_empty());
        assert!(parse_feed("<feed></feed>").is_empty());
    }
}
