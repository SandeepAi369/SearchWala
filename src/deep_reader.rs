// ============================================================================
// Deep reader — recover text that DOM extractors cannot see.
//
// Measured 2026-10-02 with synthetic pages (examples/xprobe at the time):
// trafilatura + Readability already handle <noscript>, `hidden`, inline
// `display:none` and <details> content. They return NOTHING for three page
// shapes that are now common:
//
//   shape                                   before   source of the text
//   Declarative Shadow DOM (<template          0     template content is an
//     shadowrootmode>)                                inert fragment to parsers
//   Next.js pages router (__NEXT_DATA__)      0     JSON in a <script>
//   Next.js app router (self.__next_f.push)   0     RSC "flight" payload
//
// The same applies to Nuxt (__NUXT_DATA__), Remix (__remixContext), Redux /
// Apollo state blobs (window.__INITIAL_STATE__ / __APOLLO_STATE__) and Astro
// islands: the article is in a script as data, the DOM is an empty shell.
//
// Strategies here, each a CANDIDATE that `extractor::extract` weighs against
// the DOM result:
//   * unwrap_shadow_templates  — make shadow-root content ordinary DOM
//   * hydration_text           — harvest prose strings from hydration data
//   * amp_url                  — the page's AMP variant (server-rendered,
//                                minimal markup); fetched by the scraper
// ============================================================================

use regex::Regex;
use scraper::{Html, Selector};
use std::collections::HashSet;
use std::sync::LazyLock;

/// Cap on harvested text, so a giant state blob cannot dominate a source.
const MAX_HARVEST_CHARS: usize = 200_000;
/// A harvested string must be at least this long to count as prose.
const MIN_PROSE_CHARS: usize = 80;
const MIN_PROSE_WORDS: usize = 12;

// ----------------------------------------------------------------------------
// Declarative Shadow DOM
// ----------------------------------------------------------------------------

static TEMPLATE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<template\b[^>]*>|</template\s*>").unwrap());

/// Remove the `<template shadowrootmode=…>` wrappers (and their matching
/// closing tags) so shadow-root content parses as ordinary DOM. Ordinary
/// `<template>` elements are left alone. Returns None when the page has no
/// declarative shadow roots.
pub fn unwrap_shadow_templates(html: &str) -> Option<String> {
    let lower_has = html.contains("shadowrootmode") || html.contains("shadowroot=");
    if !lower_has {
        return None;
    }
    let mut out = String::with_capacity(html.len());
    let mut stack: Vec<bool> = Vec::new();
    let mut last = 0;
    let mut changed = false;
    for m in TEMPLATE_TAG.find_iter(html) {
        let tag = m.as_str();
        let drop = if tag.starts_with("</") {
            stack.pop().unwrap_or(false)
        } else {
            let is_shadow = tag.to_ascii_lowercase().contains("shadowroot");
            stack.push(is_shadow);
            is_shadow
        };
        if drop {
            out.push_str(&html[last..m.start()]);
            last = m.end();
            changed = true;
        }
    }
    out.push_str(&html[last..]);
    changed.then_some(out)
}

// ----------------------------------------------------------------------------
// Hydration data
// ----------------------------------------------------------------------------

static SCRIPT: LazyLock<Selector> = LazyLock::new(|| Selector::parse("script").unwrap());
/// A JSON string literal of meaningful length (escape-aware).
static STRING_LITERAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""((?:[^"\\]|\\.){60,})""#).unwrap());
static HTML_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]{0,400}>").unwrap());
static BLOCK_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</(p|div|h[1-6]|li|blockquote|section)>|<br\s*/?>").unwrap());

/// Markers of scripts that carry page data rather than code.
const HYDRATION_MARKERS: &[&str] = &[
    "__NEXT_DATA__",
    "__next_f",
    "__NUXT_DATA__",
    "__NUXT__",
    "__remixContext",
    "__INITIAL_STATE__",
    "__PRELOADED_STATE__",
    "__APOLLO_STATE__",
    "__INITIAL_DATA__",
    "__SERVER_DATA__",
    "astro-island",
];

fn is_data_script(attrs_type: Option<&str>, id: Option<&str>, body: &str) -> bool {
    if matches!(attrs_type, Some(t) if t.eq_ignore_ascii_case("application/ld+json")) {
        return false; // handled by extractor::json_ld_article_body
    }
    if matches!(attrs_type, Some(t) if t.eq_ignore_ascii_case("application/json")) {
        return true;
    }
    if id.is_some_and(|i| HYDRATION_MARKERS.iter().any(|m| i.contains(m))) {
        return true;
    }
    let head = crate::util::truncate_bytes(body, 400);
    HYDRATION_MARKERS.iter().any(|m| head.contains(m))
}

/// Decode a JSON string-literal body (without quotes).
fn decode_literal(inner: &str) -> Option<String> {
    serde_json::from_str::<String>(&format!("\"{inner}\"")).ok()
}

/// RSC flight data and nested JSON carry their own string literals.
fn looks_nested(s: &str) -> bool {
    s.contains("\"children\"") || s.contains("[\"$\"") || s.contains("\":{\"") || s.contains("\":\"")
}

/// Plain text from a string that may contain HTML (CMS bodies stored as HTML).
fn to_plain(s: &str) -> String {
    let with_breaks = BLOCK_END.replace_all(s, "\n\n");
    let no_tags = HTML_TAG.replace_all(&with_breaks, " ");
    let decoded = decode_entities(&no_tags);
    decoded
        .split("\n\n")
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&rsquo;", "\u{2019}")
        .replace("&lsquo;", "\u{2018}")
        .replace("&ldquo;", "\u{201C}")
        .replace("&rdquo;", "\u{201D}")
        .replace("&mdash;", "\u{2014}")
        .replace("&ndash;", "\u{2013}")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// True for natural-language text, false for URLs, CSS, code, ids, base64.
pub fn is_prose(s: &str) -> bool {
    let chars = s.chars().count();
    if chars < MIN_PROSE_CHARS {
        return false;
    }
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() < MIN_PROSE_WORDS {
        return false;
    }
    let t = s.trim_start();
    if t.starts_with("http") || t.starts_with('/') || t.starts_with("data:") || t.starts_with('{') {
        return false;
    }
    if ["function(", "=>", "var(--", "{{", "}}", "px;", ";base64,"].iter().any(|m| s.contains(m)) {
        return false;
    }
    let non_space = s.chars().filter(|c| !c.is_whitespace()).count().max(1);
    let letters = s.chars().filter(|c| c.is_alphabetic() || crate::util::is_word_char(*c) && !c.is_ascii_digit()).count();
    if (letters as f64) / (non_space as f64) < 0.65 {
        return false;
    }
    let avg_word = non_space as f64 / words.len() as f64;
    if !(2.0..=14.0).contains(&avg_word) {
        return false;
    }
    // Sentences, not a list of tags or a title.
    s.contains(". ") || s.contains("। ") || s.ends_with('.') || s.ends_with('।') || words.len() >= 25
}

fn harvest(text: &str, depth: u8, out: &mut Vec<String>, seen: &mut HashSet<String>, total: &mut usize) {
    for cap in STRING_LITERAL.captures_iter(text) {
        if *total >= MAX_HARVEST_CHARS {
            return;
        }
        let Some(decoded) = decode_literal(&cap[1]) else { continue };
        if depth < 3 && looks_nested(&decoded) {
            harvest(&decoded, depth + 1, out, seen, total);
            continue;
        }
        let plain = to_plain(&decoded);
        for para in plain.split("\n\n") {
            if !is_prose(para) {
                continue;
            }
            let key: String = para.chars().take(80).collect::<String>().to_lowercase();
            if seen.insert(key) {
                *total += para.chars().count();
                out.push(para.to_string());
            }
        }
    }
}

/// Prose harvested from hydration data, in document order, or None.
pub fn hydration_text(html: &str) -> Option<String> {
    if !HYDRATION_MARKERS.iter().any(|m| html.contains(m)) && !html.contains("application/json") {
        return None;
    }
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut total = 0usize;
    for s in doc.select(&SCRIPT) {
        if s.value().attr("src").is_some() {
            continue;
        }
        let body: String = s.text().collect();
        if body.len() < 200 || !is_data_script(s.value().attr("type"), s.value().attr("id"), &body) {
            continue;
        }
        harvest(&body, 0, &mut out, &mut seen, &mut total);
    }
    // Astro islands keep props in an attribute, not a script.
    if html.contains("astro-island") {
        if let Ok(sel) = Selector::parse("astro-island[props]") {
            for el in doc.select(&sel) {
                if let Some(p) = el.value().attr("props") {
                    harvest(p, 0, &mut out, &mut seen, &mut total);
                }
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join("\n\n"))
    }
}

// ----------------------------------------------------------------------------
// AMP
// ----------------------------------------------------------------------------

static AMP_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<link\b[^>]*\brel=["']?amphtml["']?[^>]*>"#).unwrap()
});
static HREF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\bhref=["']([^"']+)["']"#).unwrap());

/// The page's AMP variant, resolved against `base`, when it is a different
/// http(s) URL on the same registrable host.
pub fn amp_url(html: &str, base: &str) -> Option<String> {
    let head = crate::util::truncate_bytes(html, 200_000);
    let tag = AMP_LINK.find(head)?.as_str();
    let href = HREF.captures(tag)?.get(1)?.as_str();
    let base_url = url::Url::parse(base).ok()?;
    let amp = base_url.join(&decode_entities(href)).ok()?;
    if !matches!(amp.scheme(), "http" | "https") || amp.as_str() == base_url.as_str() {
        return None;
    }
    let root = |h: &str| h.trim_start_matches("www.").trim_start_matches("amp.").to_string();
    let same = match (amp.host_str(), base_url.host_str()) {
        (Some(a), Some(b)) => root(a).ends_with(&root(b)) || root(b).ends_with(&root(a)),
        _ => false,
    };
    same.then(|| amp.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paras(tag: &str, n: usize) -> String {
        (0..n)
            .map(|i| format!("{tag} paragraph {i}: The reactor cooling loop transfers heat from the core to the steam generators, and engineers monitor its pressure continuously."))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn unwraps_declarative_shadow_roots_only() {
        let html = r#"<div><template shadowrootmode="open"><p>Shadow text</p></template><template id="t"><p>inert</p></template></div>"#;
        let out = unwrap_shadow_templates(html).unwrap();
        assert!(out.contains("<div><p>Shadow text</p><template id=\"t\">"), "{out}");
        assert!(unwrap_shadow_templates("<p>no shadow</p>").is_none());
    }

    #[test]
    fn harvests_next_data() {
        let body = paras("NEXT", 4);
        let html = format!(
            r#"<html><body><div id="__next"></div><script id="__NEXT_DATA__" type="application/json">{{"props":{{"pageProps":{{"title":"Short","article":{{"body":"{body}"}},"url":"https://example.org/a/very/long/path/that/is/not/prose/at/all/but/is/long/enough"}}}}}}</script></body></html>"#
        );
        let t = hydration_text(&html).expect("harvested");
        assert!(t.contains("NEXT paragraph 3"), "{t}");
        assert!(!t.contains("https://"), "URLs are not prose: {t}");
    }

    #[test]
    fn harvests_rsc_flight_payload() {
        let body = paras("RSC", 3);
        let flight = format!(r#"5:["$","p",null,{{"children":"{body}"}}]"#);
        let literal = serde_json::to_string(&flight).unwrap();
        let html = format!(r#"<html><body><script>self.__next_f.push([1,{literal}])</script></body></html>"#);
        let t = hydration_text(&html).expect("harvested");
        assert!(t.contains("RSC paragraph 2"), "{t}");
    }

    #[test]
    fn hydration_html_bodies_become_paragraphs() {
        let p1 = paras("ONE", 1);
        let p2 = paras("TWO", 1);
        let html = format!(
            r#"<script>window.__INITIAL_STATE__ = {{"post":{{"content":"<p>{p1}</p><p>{p2}</p>"}}}};</script>"#
        );
        let t = hydration_text(&html).unwrap();
        assert_eq!(t.split("\n\n").count(), 2, "{t}");
    }

    #[test]
    fn prose_filter_rejects_code_and_urls() {
        assert!(!is_prose("https://example.org/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t/u/v/w/x/y/z/aa/bb/cc"));
        assert!(!is_prose(".btn{color:var(--accent);padding:4px;} .card{margin:0 auto;} .x{display:flex;} .y{gap:2px;}"));
        assert!(is_prose(&paras("OK", 1)));
        assert!(is_prose("भारत दक्षिण एशिया में स्थित एक विशाल देश है जिसकी राजधानी नई दिल्ली है। यह विश्व का सबसे अधिक जनसंख्या वाला देश है और यहाँ अनेक भाषाएँ बोली जाती हैं।"));
    }

    #[test]
    fn amp_url_same_site_only() {
        let page = r#"<head><link rel="amphtml" href="/amp/story-1"></head>"#;
        assert_eq!(amp_url(page, "https://www.news.test/story-1").as_deref(), Some("https://www.news.test/amp/story-1"));
        let sub = r#"<link rel='amphtml' href='https://amp.news.test/story-1'>"#;
        assert!(amp_url(sub, "https://www.news.test/story-1").is_some());
        let other = r#"<link rel="amphtml" href="https://evil.test/x">"#;
        assert!(amp_url(other, "https://www.news.test/story-1").is_none());
        assert!(amp_url("<p>none</p>", "https://www.news.test/").is_none());
    }
}
