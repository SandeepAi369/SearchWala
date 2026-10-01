// ══════════════════════════════════════════════════════════════════════════════
// Bing Search — dedicated parser
//
// WHY A DEDICATED ENGINE
// v7.0 routed Bing through the generic link-scraper, which looked for
// `h2 a[href]`. Bing's 2026 markup puts the title text in a bare `<h2>` with
// NO anchor inside it, and the result URL in a separate sibling `<a>`. So
// `li.b_algo h2 a` matches ZERO elements and the generic path fell through to
// `#search a[href]` / `main a[href]`, harvesting navigation chrome.
//
// Selectors verified against tests/fixtures/serp_bing.html (2026-10-01):
//   li.b_algo              -> 5
//   li.b_algo h2           -> 5   ("2027 Cricket World Cup - Wikipedia")
//   li.b_algo h2 a         -> 0   <-- the v7.0 assumption
//   li.b_algo a[href^=http]-> 10  (first is the result URL)
//   li.b_algo .b_caption p -> 5   (snippet)
//   li.b_algo .tptt        -> 5   (display domain)
//
// Bing also requires TLS impersonation: with plain reqwest it served its
// HOMEPAGE instead of a SERP (observed `/cricket/manifest.json`,
// `/?FORM=Z9FD1`), which parsed to zero results with no error.
// ══════════════════════════════════════════════════════════════════════════════

use std::collections::HashSet;
use std::sync::LazyLock;

use scraper::{Html, Selector};

use crate::fetcher::Fetcher;
use crate::models::RawSearchResult;

pub struct Bing {
    name: String,
    /// Market code, e.g. "en-GB". None = Bing's own geo default.
    market: Option<&'static str>,
    /// `news` vertical instead of web.
    news: bool,
    /// Pages to fetch. Regional aliases fetch ONE page.
    ///
    /// Regional variants exist for result DIVERSITY, not depth, and they share
    /// bing.com with the primary engine under the per-host gate. Paginating
    /// all of them would serialize ~15 requests against one host and blow the
    /// dispatch deadline. One page each gives the diversity at a fraction of
    /// the cost.
    pages: usize,
}

static SEL_RESULT: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("li.b_algo, .b_algo").unwrap());
static SEL_TITLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h2").unwrap());
static SEL_LINK: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a[href^=\"http\"]").unwrap());
static SEL_SNIPPET: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".b_caption p, .b_algoSlug, p").unwrap());
// News vertical markup, verified against tests/fixtures/serp_bing_news.html
// (2026-10-01). The v7.0/early-v8.0 guesses all matched ZERO:
//   .news-card -> 0, .newsitem -> 0, .na_cnt -> 0
// What actually exists: div[data-url] -> 5 containers, a.title -> 5 links.
static SEL_NEWS_CARD: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("div[data-url], .news-card, .newsitem, .na_cnt").unwrap()
});
static SEL_NEWS_LINK: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.title, a[href^=\"http\"]").unwrap());
static SEL_NEWS_SNIPPET: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".snippet, .news_snippet, p").unwrap());

impl Bing {
    pub fn new(name: &str) -> Self {
        let n = name.to_lowercase();
        let market = market_for(&n);
        let news = n.contains("_news");
        // Primary `bing` paginates; aliases do not.
        let pages = if news {
            2
        } else if market.is_some() || n != "bing" {
            1
        } else {
            3
        };
        Self { market, news, pages, name: n }
    }
}

/// Regional aliases must send a real market, or every "variant" is the same
/// request. v7.0 had 15 Bing aliases that all built one identical URL and then
/// fed RRF 15 copies of Bing's ranking as "cross-engine consensus".
fn market_for(name: &str) -> Option<&'static str> {
    match name.rsplit('_').next().unwrap_or("") {
        "us" => Some("en-US"),
        "uk" => Some("en-GB"),
        "in" => Some("en-IN"),
        "de" => Some("de-DE"),
        "fr" => Some("fr-FR"),
        "es" => Some("es-ES"),
        "it" => Some("it-IT"),
        "jp" => Some("ja-JP"),
        "ca" => Some("en-CA"),
        "au" => Some("en-AU"),
        "nl" => Some("nl-NL"),
        "se" => Some("sv-SE"),
        "no" => Some("nb-NO"),
        "fi" => Some("fi-FI"),
        _ => None,
    }
}

#[async_trait::async_trait]
impl super::SearchEngine for Bing {
    fn name(&self) -> &str {
        &self.name
    }

    fn needs_stealth(&self) -> bool {
        true
    }

    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        let mut all_results = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut rank_base = 0usize;
        let encoded = urlencoding::encode(query);

        for page in 0..self.pages {
            // Bing's `first` is a 1-based row offset.
            let first = page * 10 + 1;
            let base = if self.news {
                format!("https://www.bing.com/news/search?q={encoded}&first={first}")
            } else {
                format!("https://www.bing.com/search?q={encoded}&first={first}")
            };
            let url = match self.market {
                Some(m) => format!("{base}&mkt={m}&setlang={m}"),
                None => base,
            };

            super::health::record_attempt(&self.name);
            let resp = match f.get(&url, &[]).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("{} transport error: {}", self.name, e);
                    super::health::record_transport_err(&self.name);
                    break;
                }
            };
            if resp.status != 200 {
                tracing::warn!("{}: HTTP {} — not parsing", self.name, resp.status);
                super::health::record_block(&self.name, resp.status);
                break;
            }
            if super::health::looks_like_challenge(&resp.body) {
                super::health::record_block(&self.name, 0);
                break;
            }

            let page_results = if self.news {
                parse_bing_news(&resp.body, &self.name, rank_base)
            } else {
                parse_bing_html(&resp.body, &self.name, rank_base)
            };
            super::health::record_parsed(&self.name, page_results.len());
            if page_results.is_empty() {
                if page == 0 {
                    super::health::dump_zero_result_body(&self.name, &url, &resp.body);
                }
                break;
            }
            rank_base += page_results.len();

            for r in page_results {
                if seen.insert(r.url.clone()) {
                    all_results.push(r);
                }
            }
        }

        tracing::info!("{} [{}]: {} results", self.name, f.kind(), all_results.len());
        all_results
    }
}

/// Minimal base64url decoder (no padding required). Avoids pulling in a
/// base64 crate for this one use.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    const INVALID: u8 = 0xFF;
    let mut table = [INVALID; 256];
    for (i, c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
        .iter()
        .enumerate()
    {
        table[*c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = table[c as usize];
        if v == INVALID {
            return None; // not base64url
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Unwrap Bing's click-tracking redirect.
///
/// Bing A/B tests between direct result hrefs and
/// `https://www.bing.com/ck/a?!&&p=<hash>&u=a1<base64url>&ntb=1` wrappers.
/// This cost us every Bing result on the variant that uses wrappers: the
/// self-link filter saw `bing.com` and rejected all 10 results, so Bing
/// reported 0 with HTTP 200 — indistinguishable from "no matches" without the
/// SERP dump that caught it.
///
/// Returns `None` for Bing-internal destinations (images/videos/maps nav),
/// which decode to relative paths.
pub fn unwrap_bing_redirect(href: &str) -> Option<String> {
    if !href.contains("/ck/a") {
        return None;
    }
    let normalized = href.replace("&amp;", "&");
    let parsed = url::Url::parse(&normalized).ok()?;
    let u = parsed.query_pairs().find(|(k, _)| k == "u").map(|(_, v)| v.to_string())?;
    // Bing prefixes the payload with a format marker, currently "a1".
    let payload = u.strip_prefix("a1").unwrap_or(&u);
    let bytes = b64url_decode(payload)?;
    let decoded = String::from_utf8(bytes).ok()?;
    if decoded.starts_with("http://") || decoded.starts_with("https://") {
        Some(decoded)
    } else {
        None // relative => Bing-internal vertical link
    }
}

/// Resolve a result href to an external destination, or None.
fn resolve_result_href(href: &str) -> Option<String> {
    if let Some(target) = unwrap_bing_redirect(href) {
        return Some(target);
    }
    if !href.starts_with("http") {
        return None;
    }
    let host = url::Url::parse(href).ok()?.host_str()?.to_lowercase();
    if host.ends_with("bing.com") || host.ends_with("microsoft.com") || host.ends_with("msn.com") {
        return None;
    }
    Some(href.to_string())
}

pub fn parse_bing_html(html: &str, engine: &str, rank_base: usize) -> Vec<RawSearchResult> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();

    for el in doc.select(&SEL_RESULT) {
        // Title: the h2's TEXT (there is no anchor inside it).
        let title = el
            .select(&SEL_TITLE)
            .next()
            .map(|h| h.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        if crate::util::char_len(&title) < 3 {
            continue;
        }

        // URL: first anchor that resolves to an EXTERNAL destination, after
        // unwrapping Bing's ck/a click-tracking redirect.
        let Some(url) = el
            .select(&SEL_LINK)
            .filter_map(|a| a.value().attr("href"))
            .find_map(resolve_result_href)
        else {
            continue;
        };

        let snippet = el
            .select(&SEL_SNIPPET)
            .next()
            .map(|p| p.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();

        out.push(RawSearchResult {
            url,
            title,
            snippet,
            engine: engine.to_string(),
            rank_position: rank_base + out.len() + 1,
            published: None,
        });
    }

    out
}

pub fn parse_bing_news(html: &str, engine: &str, rank_base: usize) -> Vec<RawSearchResult> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for el in doc.select(&SEL_NEWS_CARD) {
        let Some(link) = el.select(&SEL_NEWS_LINK).next() else { continue };
        let Some(raw_href) = link.value().attr("href") else { continue };
        let Some(href) = resolve_result_href(raw_href) else { continue };
        let href = href.as_str();
        let title = link
            .value()
            .attr("title")
            .map(|t| t.to_string())
            .unwrap_or_else(|| link.text().collect::<String>().trim().to_string());
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        if crate::util::char_len(&title) < 5 || !seen.insert(href.to_string()) {
            continue;
        }
        let snippet = el
            .select(&SEL_NEWS_SNIPPET)
            .next()
            .map(|s| s.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        out.push(RawSearchResult {
            url: href.to_string(),
            title,
            snippet,
            engine: engine.to_string(),
            rank_position: rank_base + out.len() + 1,
            published: None,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real Bing SERP captured 2026-10-01.
    const FIXTURE: &str = include_str!("../../tests/fixtures/serp_bing.html");

    #[test]
    fn parses_live_bing_fixture() {
        let r = parse_bing_html(FIXTURE, "bing", 0);
        assert!(r.len() >= 4, "expected >=4 results, got {}: {r:?}", r.len());

        let first = &r[0];
        assert!(
            first.title.contains("Cricket World Cup"),
            "unexpected title {:?}",
            first.title
        );
        assert!(
            first.url.starts_with("https://") && !first.url.contains("bing.com"),
            "bad url {:?}",
            first.url
        );
        assert!(!first.snippet.is_empty(), "snippet should be populated");
        assert_eq!(first.rank_position, 1);
    }

    #[test]
    fn h2_has_no_anchor_so_title_comes_from_text() {
        // Documents the exact v7.0 wrong assumption.
        let doc = Html::parse_document(FIXTURE);
        let with_anchor = Selector::parse("li.b_algo h2 a").unwrap();
        let bare_h2 = Selector::parse("li.b_algo h2").unwrap();
        assert_eq!(doc.select(&with_anchor).count(), 0, "v7.0 selected this and got nothing");
        assert!(doc.select(&bare_h2).count() >= 4);
    }

    #[test]
    fn rejects_bing_self_links() {
        let html = r#"<html><body><li class="b_algo">
            <h2>A Result Title Here</h2>
            <a href="https://www.bing.com/ck/a?tracking">tracker</a>
            <a href="https://real.test/article">real</a>
            <div class="b_caption"><p>The snippet.</p></div>
        </li></body></html>"#;
        let r = parse_bing_html(html, "bing", 0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://real.test/article");
    }

    #[test]
    fn only_the_primary_bing_paginates() {
        // Keeps the per-host gate from serializing 15 requests to bing.com.
        assert_eq!(Bing::new("bing").pages, 3);
        assert_eq!(Bing::new("bing_uk").pages, 1);
        assert_eq!(Bing::new("bing_in").pages, 1);
        assert_eq!(Bing::new("bing_news").pages, 2);
    }

    #[test]
    fn regional_markets_are_distinct() {
        assert_eq!(market_for("bing"), None);
        assert_eq!(market_for("bing_uk"), Some("en-GB"));
        assert_eq!(market_for("bing_in"), Some("en-IN"));
        assert_eq!(market_for("bing_de"), Some("de-DE"));
    }

    #[test]
    fn global_rank_across_pages() {
        let p2 = parse_bing_html(FIXTURE, "bing", 10);
        assert_eq!(p2[0].rank_position, 11);
    }

    // ── Bing's OTHER A/B variant: every href is a ck/a tracking wrapper ──
    //
    // Captured live 2026-10-01 from a run where Bing reported 0 results with
    // HTTP 200. Both variants must parse, or Bing silently drops to zero
    // depending on which bucket the request lands in.
    const FIXTURE_CKA: &str = include_str!("../../tests/fixtures/serp_bing_cka.html");

    #[test]
    fn decodes_bing_ck_a_redirect() {
        let href = "https://www.bing.com/ck/a?!&&p=abc&u=a1aHR0cHM6Ly9leGFtcGxlLmNvbS9hcnRpY2xl&ntb=1";
        assert_eq!(
            unwrap_bing_redirect(href).as_deref(),
            Some("https://example.com/article")
        );
    }

    #[test]
    fn rejects_bing_internal_vertical_redirects() {
        // u decodes to "/images/search?q=..." — a relative Bing path.
        let href = "https://www.bing.com/ck/a?!&&p=x&u=a1L2ltYWdlcy9zZWFyY2g_cT1x";
        assert_eq!(unwrap_bing_redirect(href), None);
    }

    #[test]
    fn parses_the_ck_a_variant_fixture() {
        let r = parse_bing_html(FIXTURE_CKA, "bing", 0);
        assert!(
            r.len() >= 5,
            "ck/a variant must parse; got {} results (this returned 0 before the \
             redirect decoder)",
            r.len()
        );
        for x in &r {
            assert!(
                !x.url.contains("bing.com"),
                "unwrapped url still points at bing: {:?}",
                x.url
            );
            assert!(x.url.starts_with("http"), "bad url {:?}", x.url);
        }
        assert_eq!(r[0].rank_position, 1);
    }

    /// Real Bing News SERP captured 2026-10-01 from a run that returned 0.
    const FIXTURE_NEWS: &str = include_str!("../../tests/fixtures/serp_bing_news.html");

    #[test]
    fn parses_live_bing_news_fixture() {
        let r = parse_bing_news(FIXTURE_NEWS, "bing_news", 0);
        assert!(
            r.len() >= 4,
            "bing_news must parse; got {} (the .news-card/.newsitem/.na_cnt \
             selectors all matched zero)",
            r.len()
        );
        for x in &r {
            assert!(x.url.starts_with("http"), "bad url {:?}", x.url);
            assert!(!x.url.contains("bing.com"), "self-link {:?}", x.url);
            assert!(crate::util::char_len(&x.title) >= 5, "short title {:?}", x.title);
        }
        assert_eq!(r[0].rank_position, 1);
    }

    #[test]
    fn b64url_decoder_handles_missing_padding() {
        assert_eq!(b64url_decode("aHR0cHM6Ly9hLnRlc3Q").unwrap(), b"https://a.test");
        assert_eq!(b64url_decode("YQ").unwrap(), b"a");
        assert!(b64url_decode("not base64!!").is_none());
    }
}
