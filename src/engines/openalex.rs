// ══════════════════════════════════════════════════════════════════════════════
// OpenAlex — open scholarly index (works search)
//
// Verified live 2026-10-02: HTTP 200 in ~1.7 s, no key, 174k results for
// "mRNA vaccine mechanism", with citation counts, years, open-access links and
// (for many papers) abstracts.
//
// Why this exists: v8.0's Academic mode appended
// `site:edu OR site:gov OR site:nature.com` to the query and sent it to every
// engine. The JSON APIs searched for that literal text and the HTML engines
// returned university homepages. This queries a scholarly index directly.
//
// The abstract is passed through as `full_text`, so the scrape stage can use
// it without fetching the publisher landing page (slow, often paywalled).
//
//   GET https://api.openalex.org/works?search=<q>&per-page=N&select=...
//
// Set OPENALEX_MAILTO to join OpenAlex's "polite pool" (better rate limits).
// ══════════════════════════════════════════════════════════════════════════════

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

pub struct OpenAlex;

const PER_PAGE: usize = 20;

/// Rebuild an abstract from OpenAlex's inverted index
/// (`{"word": [positions...]}`). Positions can have gaps; those are skipped.
pub fn rebuild_abstract(inv: &serde_json::Value) -> String {
    let Some(map) = inv.as_object() else { return String::new() };
    let mut slots: Vec<(u64, &str)> = Vec::new();
    for (word, positions) in map {
        if let Some(arr) = positions.as_array() {
            for p in arr {
                if let Some(i) = p.as_u64() {
                    // Guard against a malicious/huge index blowing up memory.
                    if i < 20_000 {
                        slots.push((i, word.as_str()));
                    }
                }
            }
        }
    }
    slots.sort_by_key(|(i, _)| *i);
    slots.into_iter().map(|(_, w)| w).collect::<Vec<_>>().join(" ")
}

fn s<'a>(v: &'a serde_json::Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for k in path {
        cur = cur.get(*k)?;
    }
    cur.as_str()
}

/// Turn one OpenAlex work into a search result. Returns None for works with no
/// title or no usable URL.
pub fn work_to_result(w: &serde_json::Value, rank: usize) -> Option<RawSearchResult> {
    let title = s(w, &["title"]).or_else(|| s(w, &["display_name"]))?.trim().to_string();
    if title.is_empty() {
        return None;
    }

    // Prefer the human-readable landing page; fall back to the DOI, then the
    // OpenAlex record itself. (oa_url is often a PDF, which we cannot extract.)
    let url = s(w, &["primary_location", "landing_page_url"])
        .filter(|u| u.starts_with("http"))
        .or_else(|| s(w, &["doi"]).filter(|u| u.starts_with("http")))
        .or_else(|| s(w, &["id"]).filter(|u| u.starts_with("http")))?
        .to_string();

    let year = w.get("publication_year").and_then(|v| v.as_u64());
    let cites = w.get("cited_by_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let venue = s(w, &["primary_location", "source", "display_name"]).unwrap_or("");
    let kind = s(w, &["type"]).unwrap_or("");

    let authors: Vec<String> = w
        .get("authorships")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| s(a, &["author", "display_name"]).map(str::to_string))
                .take(4)
                .collect()
        })
        .unwrap_or_default();

    let abstract_text = w
        .get("abstract_inverted_index")
        .map(rebuild_abstract)
        .unwrap_or_default();

    let published = s(w, &["publication_date"])
        .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(dt, chrono::Utc));

    // A header the LLM can cite from: year, venue, citation count, type.
    let mut header = format!("Paper: {title}");
    if let Some(y) = year {
        header.push_str(&format!(" ({y})"));
    }
    if !authors.is_empty() {
        header.push_str(&format!(". Authors: {}", authors.join(", ")));
    }
    if !venue.is_empty() {
        header.push_str(&format!(". Venue: {venue}"));
    }
    if !kind.is_empty() {
        header.push_str(&format!(". Type: {kind}"));
    }
    header.push_str(&format!(". Cited by: {cites}."));

    let full_text = if crate::util::char_len(&abstract_text) >= 150 {
        Some(format!("{header}\n\nAbstract: {abstract_text}"))
    } else {
        None
    };

    let snippet = if abstract_text.is_empty() {
        header.clone()
    } else {
        crate::util::truncate_chars(&abstract_text, 280).to_string()
    };

    let display_title = match year {
        Some(y) => format!("{title} ({y}, cited {cites}×)"),
        None => format!("{title} (cited {cites}×)"),
    };

    Some(RawSearchResult {
        url,
        title: display_title,
        snippet,
        engine: "openalex".to_string(),
        rank_position: rank,
        published,
        full_text,
    })
}

#[async_trait::async_trait]
impl super::SearchEngine for OpenAlex {
    fn name(&self) -> &str {
        "openalex"
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        super::health::record_attempt("openalex");
        let per_page = PER_PAGE * super::page_budget(3);
        let mut url = format!(
            "https://api.openalex.org/works?search={}&per-page={}&select=id,doi,title,display_name,\
             publication_year,publication_date,cited_by_count,abstract_inverted_index,\
             primary_location,authorships,type",
            urlencoding::encode(query),
            per_page.min(50)
        );
        if let Ok(m) = std::env::var("OPENALEX_MAILTO") {
            if m.contains('@') && !m.contains('&') {
                url.push_str(&format!("&mailto={}", urlencoding::encode(&m)));
            }
        }

        let (status, json) = match f.get_json(&url).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("openalex error: {}", e);
                super::health::record_transport_err("openalex");
                return Vec::new();
            }
        };
        if status != 200 {
            tracing::warn!("openalex: HTTP {}", status);
            super::health::record_block("openalex", status);
            return Vec::new();
        }

        let out: Vec<RawSearchResult> = json
            .get("results")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .enumerate()
                    .filter_map(|(i, w)| work_to_result(w, i + 1))
                    .collect()
            })
            .unwrap_or_default();

        super::health::record_parsed("openalex", out.len());
        tracing::info!(
            "openalex: {} results ({} with abstracts)",
            out.len(),
            out.iter().filter(|r| r.full_text.is_some()).count()
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuilds_abstract_in_position_order() {
        let inv = serde_json::json!({"vaccines": [2], "mRNA": [0], "teach": [1, 4], "cells": [3]});
        assert_eq!(rebuild_abstract(&inv), "mRNA teach vaccines cells teach");
    }

    #[test]
    fn rebuild_ignores_garbage() {
        assert_eq!(rebuild_abstract(&serde_json::json!(null)), "");
        assert_eq!(rebuild_abstract(&serde_json::json!({"x": "notarray"})), "");
        // Absurd positions are dropped instead of allocating.
        assert_eq!(rebuild_abstract(&serde_json::json!({"x": [999999999]})), "");
    }

    fn sample_work() -> serde_json::Value {
        let words: Vec<String> = (0..60).map(|i| format!("word{i}")).collect();
        let mut inv = serde_json::Map::new();
        for (i, w) in words.iter().enumerate() {
            inv.insert(w.clone(), serde_json::json!([i]));
        }
        serde_json::json!({
            "id": "https://openalex.org/W1",
            "doi": "https://doi.org/10.1000/xyz",
            "title": "mRNA vaccines — a new era",
            "publication_year": 2018,
            "publication_date": "2018-01-12",
            "cited_by_count": 4727,
            "type": "review",
            "abstract_inverted_index": inv,
            "primary_location": {
                "landing_page_url": "https://www.nature.com/articles/nrd.2017.243",
                "source": {"display_name": "Nature Reviews Drug Discovery"}
            },
            "authorships": [{"author": {"display_name": "N. Pardi"}}]
        })
    }

    #[test]
    fn converts_a_work_with_citable_metadata() {
        let r = work_to_result(&sample_work(), 1).expect("should convert");
        assert_eq!(r.url, "https://www.nature.com/articles/nrd.2017.243");
        assert!(r.title.contains("2018") && r.title.contains("4727"), "{}", r.title);
        assert!(r.published.is_some());
        let ft = r.full_text.expect("abstract should become full_text");
        assert!(ft.starts_with("Paper: mRNA vaccines"));
        assert!(ft.contains("Cited by: 4727") && ft.contains("Venue: Nature Reviews"));
        assert!(ft.contains("Abstract: word0 word1"));
    }

    #[test]
    fn falls_back_to_doi_when_no_landing_page() {
        let mut w = sample_work();
        w["primary_location"] = serde_json::json!(null);
        assert_eq!(work_to_result(&w, 1).unwrap().url, "https://doi.org/10.1000/xyz");
    }

    #[test]
    fn short_or_missing_abstract_is_not_full_text() {
        let mut w = sample_work();
        w["abstract_inverted_index"] = serde_json::json!({"short": [0]});
        let r = work_to_result(&w, 1).unwrap();
        assert!(r.full_text.is_none(), "a one-word abstract must not replace scraping");
    }

    #[test]
    fn rejects_untitled_works() {
        let mut w = sample_work();
        w["title"] = serde_json::json!("");
        w["display_name"] = serde_json::json!(null);
        assert!(work_to_result(&w, 1).is_none());
    }
}
