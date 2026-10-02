// ============================================================================
// SearchWala v8.0 — Content Extractor
//
// POLICY: rs-trafilatura primary, dom_smoothie (Readability) as a conditional
// second opinion, take the longer result, always keep trafilatura's metadata.
//
// WHY, measured on a 10-page corpus (Hindi news, English news, docs, wiki,
// blog) on 2026-10-01:
//
//   policy                                    chars     ms   boilerplate
//   v7.0 hand-rolled 7-strategy              76,462     55        0
//   rs-trafilatura only                     117,032    374        0
//   dom_smoothie (Readability) only          102,248     78        1
//   both always, take longer                 121,490    457        1
//   trafilatura + CONDITIONAL readability    120,898    416        1   <-- this
//
// Trafilatura alone is 96.3% of best-of-all, but the average hides tail risk:
// on nih_gov it extracted 1,733 chars where Readability got 3,887 — a 55% loss
// on that page shape. The conditional second opinion recovers that for +11%
// CPU, and gets 99.5% of the always-both result at 91% of its cost.
//
// Readability alone is not viable: −13% content and, decisively, it provides
// NO metadata. Trafilatura gives `date`, `language` and `extraction_quality`,
// which the freshness ranking (P2-2) and the ranking confidence prior (P1-6)
// both need, and which the v7.0 extractor never produced at all.
//
// The v7.0 extractor is retired. Its specific defects, for the record:
//   1. JSON-LD short-circuited on `@type: "WebPage"` and returned that node's
//      `description`. On the Amar Ujala article that meant 180 chars instead
//      of the 4,596-char `articleBody` sitting in block 3 of the same page —
//      a 26x content loss, measured.
//   2. No `@graph` support, so Yoast/RankMath sites (a large share of
//      WordPress news) got no structured extraction at all.
//   3. `@type` as an array (`["Article","NewsArticle"]`) silently failed
//      `.as_str()`.
//   4. `try_scored_extraction` called `element.html()` per container,
//      re-serializing the subtree — accidentally quadratic on large pages.
//   5. `extract_visible_text` joined block children with `" "` after wrapping
//      them in `"\n{}\n"`, producing `"\nA\n \nB\n"`. `clean_and_deduplicate`
//      split on `"\n\n"`, which therefore never matched: paragraph dedup never
//      ran and chunking saw one giant blob.
//   6. ASCII-only sentence detection (`[A-Z].*[.!?]$`) and byte-based length
//      gates, both of which mis-handle Devanagari and CJK.
// ============================================================================

use scraper::{Html, Selector};
use std::sync::LazyLock;

/// Minimum characters for an extraction to count as success.
/// CHARACTERS, not bytes — Devanagari is 3 bytes/char, so a byte gate let
/// Hindi pages through with a third of the content (v7.0 bug).
fn min_text_chars() -> usize {
    std::env::var("MIN_TEXT_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120)
}

/// Result of extracting one page.
#[derive(Debug, Clone, Default)]
pub struct Extracted {
    pub title: String,
    pub text: String,
    /// Publication date, when the page declares one. The "time-aware" v7.0
    /// never extracted this, so ranking had no freshness signal at all.
    pub published: Option<chrono::DateTime<chrono::Utc>>,
    /// BCP-47-ish language code from the extractor ("hi", "en", ...).
    pub language: Option<String>,
    /// Page-type classification ("article", "forum", "documentation", ...).
    pub page_type: Option<String>,
    /// 0.0–1.0 extraction confidence. v7.0 hardcoded 0.7 with the comment
    /// "will be overridden when C4 provides real confidence" — this is it.
    pub confidence: f64,
    /// Which extractor produced the text that was kept.
    pub source: &'static str,
}

// ============================================================================
// Primary: rs-trafilatura
// ============================================================================

fn trafilatura_extract(html: &str, url: &str) -> Option<Extracted> {
    let opts = rs_trafilatura::Options {
        url: Some(url.to_string()),
        include_tables: true,
        // Links and comments add boilerplate without adding evidence.
        include_links: false,
        include_comments: false,
        ..Default::default()
    };
    let r = rs_trafilatura::extract_with_options(html, &opts).ok()?;
    Some(Extracted {
        title: r.metadata.title.clone().unwrap_or_default(),
        text: r.content_text,
        published: r.metadata.date,
        language: r.metadata.language.clone(),
        page_type: r.metadata.page_type.clone(),
        confidence: r.extraction_quality.clamp(0.0, 1.0),
        source: "trafilatura",
    })
}

// ============================================================================
// Second opinion: dom_smoothie (Mozilla Readability port)
// ============================================================================

fn readability_extract(html: &str, url: &str) -> Option<String> {
    let cfg = dom_smoothie::Config { ..Default::default() };
    let mut r = dom_smoothie::Readability::new(html, Some(url), Some(cfg)).ok()?;
    let article = r.parse().ok()?;
    let text = article.text_content.to_string();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Should we spend the extra ~8ms/page on a second opinion?
///
/// An absolute "< 600 chars" threshold never fired on the benchmark corpus —
/// trafilatura always produced *something*. What it misses is content that is
/// thin RELATIVE to the page, which is the nih_gov failure mode.
fn wants_second_opinion(primary: &Extracted, html_len: usize) -> bool {
    let chars = crate::util::char_len(&primary.text);
    if chars < 600 {
        return true;
    }
    if primary.confidence < 0.80 {
        return true;
    }
    // Text-to-markup ratio: a content-rich page that yielded little text is
    // the signal that the primary extractor picked the wrong container.
    let ratio = chars as f64 / html_len.max(1) as f64;
    ratio < 0.035
}

// ============================================================================
// Public API
// ============================================================================

/// Extract readable content from a page.
///
/// DOM extraction (trafilatura, then Readability as a second opinion) runs on
/// the page with any declarative shadow roots unwrapped. Structured sources
/// then compete with the DOM result (see `deep_reader`): JSON-LD
/// `articleBody` and microdata are authoritative and win when clearly longer
/// (paywalled pages show a teaser in the DOM and the full text there);
/// hydration data wins on JS shells; text density is a last resort.
pub fn extract(html: &str, url: &str) -> Extracted {
    let unwrapped = crate::deep_reader::unwrap_shadow_templates(html);
    let html = unwrapped.as_deref().unwrap_or(html);

    let mut best = trafilatura_extract(html, url).unwrap_or_default();

    if wants_second_opinion(&best, html.len()) {
        if let Some(alt) = readability_extract(html, url) {
            if crate::util::char_len(&alt) > crate::util::char_len(&best.text) {
                // Keep trafilatura's METADATA (date/language/quality) — only
                // the text body is replaced.
                best.text = alt;
                best.source = "readability";
            }
        }
    }

    // Title fallback chain, for the pages where trafilatura reports none.
    if best.title.trim().is_empty() {
        best.title = extract_title_from_html(html);
    }

    let dom_chars = crate::util::char_len(&best.text);
    let min = min_text_chars();
    let adopt = |best: &mut Extracted, text: String, source: &'static str, confidence: f64, factor: f64| {
        let n = crate::util::char_len(&text);
        let current = crate::util::char_len(&best.text);
        if n >= min && n as f64 >= current as f64 * factor {
            best.text = text;
            best.source = source;
            best.confidence = best.confidence.max(confidence);
        }
    };

    // Ground truth when present. 1.3x: a full body beats a teaser, but a
    // slightly longer JSON copy is not worth losing the DOM's structure.
    if html.contains("articleBody") {
        if let Some(body) = json_ld_article_body(html) {
            adopt(&mut best, body, "json-ld", 0.85, 1.3);
        }
        if let Some(body) = crate::deep_reader::microdata_body(&Html::parse_document(html)) {
            adopt(&mut best, body, "microdata", 0.8, 1.3);
        }
    }

    // JS shells (Next.js, Nuxt, Remix, Redux/Apollo state): the DOM is nearly
    // empty and the article is data in a script. Only consulted when the DOM
    // result is thin, and only adopted when it is at least twice as long.
    if dom_chars < 2_000 {
        if let Some(text) = crate::deep_reader::hydration_text(html) {
            adopt(&mut best, text, "hydration", 0.7, 2.0);
        }
    }

    // Last resort: densest text block.
    if crate::util::char_len(&best.text) < min {
        if let Some(text) = crate::deep_reader::density_text(html) {
            adopt(&mut best, text, "density", 0.55, 1.0);
        }
    }

    best.text = normalize_whitespace(&best.text);
    best
}

/// Back-compat shim for call sites that only need (title, text).
#[allow(dead_code)]
pub fn extract_title_and_text(html: &str) -> (String, String) {
    let e = extract(html, "https://example.invalid/");
    (e.title, e.text)
}

/// Back-compat shim.
#[allow(dead_code)]
pub fn extract_article_text(html: &str) -> String {
    extract(html, "https://example.invalid/").text
}

/// True when the extraction is substantial enough to keep.
pub fn is_usable(e: &Extracted) -> bool {
    crate::util::char_len(&e.text) >= min_text_chars()
}

// ============================================================================
// Helpers
// ============================================================================

static SEL_OG_TITLE: LazyLock<Option<Selector>> =
    LazyLock::new(|| Selector::parse(r#"meta[property="og:title"]"#).ok());
static SEL_TITLE: LazyLock<Option<Selector>> = LazyLock::new(|| Selector::parse("title").ok());
static SEL_H1: LazyLock<Option<Selector>> = LazyLock::new(|| Selector::parse("h1").ok());
static SEL_META_DESC: LazyLock<Option<Selector>> =
    LazyLock::new(|| Selector::parse("meta[name='description']").ok());
static SEL_JSON_LD: LazyLock<Option<Selector>> =
    LazyLock::new(|| Selector::parse("script[type='application/ld+json']").ok());

/// Title with site-name suffix stripped.
///
/// v7.0 had two title functions that disagreed: `extract_title` stripped
/// " - Site Name" suffixes while `extract_title_from_doc` (the one actually
/// called) did not.
pub fn extract_title_from_html(html: &str) -> String {
    let doc = Html::parse_document(html);
    extract_title_from_doc(&doc)
}

pub fn extract_title_from_doc(doc: &Html) -> String {
    if let Some(sel) = SEL_OG_TITLE.as_ref() {
        if let Some(m) = doc.select(sel).next() {
            if let Some(c) = m.value().attr("content") {
                let t = c.trim();
                if !t.is_empty() {
                    return t.to_string();
                }
            }
        }
    }
    if let Some(sel) = SEL_TITLE.as_ref() {
        if let Some(t) = doc.select(sel).next() {
            let raw = t.text().collect::<String>().trim().to_string();
            if !raw.is_empty() {
                return strip_site_suffix(&raw);
            }
        }
    }
    if let Some(sel) = SEL_H1.as_ref() {
        if let Some(h) = doc.select(sel).next() {
            return h.text().collect::<String>().trim().to_string();
        }
    }
    String::new()
}

/// Strip a trailing " - Site" / " | Site" / " — Site" segment.
fn strip_site_suffix(title: &str) -> String {
    for sep in [" | ", " - ", " — ", " – ", " :: ", " » "] {
        if let Some(idx) = title.rfind(sep) {
            let head = title[..idx].trim();
            // Only strip when a substantial title remains.
            if crate::util::char_len(head) >= 15 {
                return head.to_string();
            }
        }
    }
    title.to_string()
}

pub fn extract_title_from_doc_pub(doc: &Html) -> String {
    extract_title_from_doc(doc)
}

/// YouTube meta description (kept from v7.0 — the YouTube fast path uses it).
pub fn extract_youtube_meta(doc: &Html) -> Option<String> {
    let sel = SEL_META_DESC.as_ref()?;
    let m = doc.select(sel).next()?;
    let c = m.value().attr("content")?.trim();
    if crate::util::char_len(c) > 30 {
        Some(c.to_string())
    } else {
        None
    }
}

/// Collect every JSON-LD node, including `@graph` and nested arrays, and
/// return the LONGEST `articleBody`.
///
/// This is deliberately articleBody-only. v7.0 accepted a `description` from
/// the first article-ish node — and it counted `"WebPage"` as article-ish — so
/// on Amar Ujala it returned block 0's 180-char page blurb and never reached
/// block 3's 4,596-char `articleBody`.
pub fn json_ld_article_body(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let sel = SEL_JSON_LD.as_ref()?;
    let mut nodes = Vec::new();
    for script in doc.select(sel) {
        let raw = script.text().collect::<String>();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim()) {
            flatten_ld(&v, &mut nodes);
        }
    }

    let mut best: Option<String> = None;
    for n in &nodes {
        let Some(body) = n.get("articleBody").and_then(|v| v.as_str()) else { continue };
        let body = strip_tags(body.trim());
        if crate::util::char_len(&body) < 100 {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|b| crate::util::char_len(&body) > crate::util::char_len(b))
        {
            best = Some(body);
        }
    }
    best
}

/// Walk arrays and `@graph` so nested nodes are reachable. Yoast and RankMath
/// — which power a large share of WordPress news sites — put everything under
/// `@graph`, and v7.0 only looked at top-level objects or a top-level array.
fn flatten_ld(v: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match v {
        serde_json::Value::Array(a) => {
            for x in a {
                flatten_ld(x, out);
            }
        }
        serde_json::Value::Object(o) => {
            if let Some(g) = o.get("@graph") {
                flatten_ld(g, out);
            }
            out.push(v.clone());
            for key in ["mainEntityOfPage", "mainContentOfPage", "hasPart"] {
                if let Some(x) = o.get(key) {
                    if x.is_object() || x.is_array() {
                        flatten_ld(x, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// `@type` may be a string OR an array of strings.
/// v7.0's `.as_str()` returned None for `["Article","NewsArticle"]`.
pub fn ld_types(n: &serde_json::Value) -> Vec<String> {
    match n.get("@type") {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(a)) => {
            a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()
        }
        _ => Vec::new(),
    }
}

/// Strip HTML tags from a text fragment (JSON-LD bodies sometimes contain them).
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Collapse runs of whitespace while preserving paragraph breaks.
fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0usize;
    for line in s.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            // Any run of blank lines becomes exactly one paragraph break, so
            // downstream `split("\n\n")` actually works. v7.0 produced
            // "\nA\n \nB\n", which that split never matched.
            out.push_str(if blank_run > 0 { "\n\n" } else { "\n" });
        }
        blank_run = 0;
        let collapsed: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
        out.push_str(&collapsed);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMARUJALA: &str = include_str!("../tests/fixtures/articles/amarujala_hi.html");

    /// THE headline regression test.
    ///
    /// v7.0 returned 180 characters here — the `@type: "WebPage"` node's
    /// `description` — while a 4,596-character `articleBody` sat in block 3 of
    /// the same document. A 26x content loss on every Yoast-style page.
    #[test]
    fn amarujala_hindi_article_extracts_full_body_not_the_meta_description() {
        let got = extract(
            AMARUJALA,
            "https://www.amarujala.com/cricket/cricket-news/odi-world-cup-2026-india-vs-pakistan-match-venue-date-time-all-you-need-to-know-2026-10-01",
        );
        let chars = crate::util::char_len(&got.text);
        assert!(
            chars > 4_000,
            "expected >4000 chars, got {chars} (v7.0 returned 180). source={} text={:?}",
            got.source,
            got.text.chars().take(120).collect::<String>()
        );
        // Must be the Hindi article body, not an English chrome string.
        assert!(
            got.text.contains('\u{0915}') || got.text.contains('\u{092E}'),
            "extracted text does not look like Devanagari"
        );
    }

    #[test]
    fn amarujala_yields_publication_date_and_language() {
        let got = extract(AMARUJALA, "https://www.amarujala.com/cricket/cricket-news/x");
        // v7.0 extracted neither, despite calling itself "time-aware".
        assert!(got.published.is_some(), "datePublished must be extracted");
        assert_eq!(got.language.as_deref(), Some("hi"), "language should be Hindi");
        assert!(got.confidence > 0.5, "confidence {} too low", got.confidence);
    }

    #[test]
    fn json_ld_prefers_longest_articlebody_over_any_description() {
        // Block order mirrors the real Amar Ujala document: WebPage first.
        let html = r#"<html><head>
          <script type="application/ld+json">
            {"@context":"https://schema.org","@type":"WebPage",
             "description":"A short page blurb that v7.0 returned instead of the article, padded out past the hundred character gate so it would win."}
          </script>
          <script type="application/ld+json">
            {"@context":"https://schema.org","@type":["Article","NewsArticle"],
             "articleBody":"THE REAL BODY. Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua enim ad minim veniam quis nostrud exercitation ullamco."}
          </script>
        </head><body></body></html>"#;
        let body = json_ld_article_body(html).expect("should find articleBody");
        assert!(body.starts_with("THE REAL BODY"), "got {body:?}");
    }

    #[test]
    fn json_ld_walks_graph_nodes() {
        // Yoast/RankMath shape — v7.0 never looked inside @graph.
        let html = r#"<html><head><script type="application/ld+json">
          {"@context":"https://schema.org","@graph":[
            {"@type":"WebSite","name":"Site"},
            {"@type":"NewsArticle","articleBody":"Graph nested body text that is comfortably longer than the one hundred character minimum so the extractor accepts it as real content."}
          ]}
        </script></head><body></body></html>"#;
        let body = json_ld_article_body(html).expect("must find body inside @graph");
        assert!(body.starts_with("Graph nested body"), "got {body:?}");
    }

    #[test]
    fn ld_types_handles_string_and_array() {
        let s: serde_json::Value = serde_json::json!({"@type":"NewsArticle"});
        let a: serde_json::Value = serde_json::json!({"@type":["Article","NewsArticle"]});
        assert_eq!(ld_types(&s), vec!["NewsArticle"]);
        assert_eq!(ld_types(&a), vec!["Article", "NewsArticle"]);
        assert!(ld_types(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn normalize_whitespace_produces_real_paragraph_breaks() {
        // v7.0 emitted "\nA\n \nB\n" and then split on "\n\n", which never
        // matched — so paragraph dedup and chunking both saw one blob.
        let messy = "\nFirst para\n \n\nSecond   para\n\n\n\nThird\n";
        let out = normalize_whitespace(messy);
        assert_eq!(out.split("\n\n").count(), 3, "got {out:?}");
        assert!(!out.contains("   "), "whitespace not collapsed: {out:?}");
    }

    #[test]
    fn strip_site_suffix_keeps_substantial_titles() {
        assert_eq!(
            strip_site_suffix("India vs Pakistan World Cup Match Date | Amar Ujala"),
            "India vs Pakistan World Cup Match Date"
        );
        // Must not reduce a title to nothing.
        assert_eq!(strip_site_suffix("Short | X"), "Short | X");
    }

    #[test]
    fn second_opinion_triggers_on_thin_relative_output() {
        let thin = Extracted {
            text: "x".repeat(700),
            confidence: 0.95,
            ..Default::default()
        };
        // 700 chars from a 100KB page => ratio 0.007, well under 0.035.
        assert!(wants_second_opinion(&thin, 100_000));
        // Same text from a small page is fine.
        assert!(!wants_second_opinion(&thin, 10_000));
        // Low confidence always triggers.
        let unsure = Extracted { text: "x".repeat(5000), confidence: 0.4, ..Default::default() };
        assert!(wants_second_opinion(&unsure, 10_000));
        // Very short always triggers.
        let tiny = Extracted { text: "x".repeat(100), confidence: 1.0, ..Default::default() };
        assert!(wants_second_opinion(&tiny, 1_000));
    }

    #[test]
    fn strip_tags_removes_markup() {
        assert_eq!(strip_tags("a <b>bold</b> word"), "a bold word");
        assert_eq!(strip_tags("<p>only</p>"), "only");
    }

    #[test]
    fn handles_empty_and_garbage_input_without_panicking() {
        for input in ["", "<html></html>", "not html at all", "<<<>>>"] {
            let e = extract(input, "https://x.test/");
            assert!(!is_usable(&e) || crate::util::char_len(&e.text) >= min_text_chars());
        }
    }

    #[test]
    fn min_text_gate_is_character_based() {
        // 50 Devanagari chars = 150 bytes. A byte gate would wrongly pass this
        // as "150 characters" of content (the v7.0 behaviour).
        let hindi = "क".repeat(50);
        assert_eq!(crate::util::char_len(&hindi), 50);
        assert_eq!(hindi.len(), 150);
        let e = Extracted { text: hindi, ..Default::default() };
        assert!(!is_usable(&e), "50 chars must not pass a 120-char gate");
    }

    // ── Deep reader: page shapes the DOM extractors returned nothing for ──

    fn prose(tag: &str, n: usize) -> String {
        (0..n)
            .map(|i| format!("{tag} paragraph {i}: The reactor cooling loop transfers heat from the core to the steam generators, and engineers monitor its pressure continuously."))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn reads_declarative_shadow_dom() {
        let html = format!(
            "<html><head><title>S</title></head><body><main><div><template shadowrootmode=\"open\"><p>{}</p><p>{}</p></template></div></main></body></html>",
            prose("SHADOW", 3),
            prose("ROOT", 3)
        );
        let e = extract(&html, "https://example.org/s");
        assert!(e.text.contains("SHADOW paragraph 2") && e.text.contains("ROOT paragraph 0"), "{}", e.text);
    }

    #[test]
    fn reads_next_js_shells() {
        let pages = format!(
            r#"<html><head><title>N</title></head><body><div id="__next"></div><script id="__NEXT_DATA__" type="application/json">{{"props":{{"pageProps":{{"post":{{"body":"{}"}}}}}}}}</script></body></html>"#,
            prose("NEXT", 5)
        );
        let e = extract(&pages, "https://example.org/n");
        assert_eq!(e.source, "hydration");
        assert!(e.text.contains("NEXT paragraph 4"));

        let flight = serde_json::to_string(&format!(r#"5:["$","p",null,{{"children":"{}"}}]"#, prose("RSC", 4))).unwrap();
        let app = format!(r#"<html><head><title>R</title></head><body><div></div><script>self.__next_f.push([1,{flight}])</script></body></html>"#);
        let e = extract(&app, "https://example.org/r");
        assert!(e.text.contains("RSC paragraph 3"), "{}", e.text);
    }

    #[test]
    fn json_ld_full_body_beats_a_dom_teaser() {
        let body = prose("FULL", 8);
        let html = format!(
            r#"<html><head><title>P</title><script type="application/ld+json">{{"@type":"NewsArticle","headline":"P","articleBody":"{body}"}}</script></head>
            <body><article><p>{}</p><div class="paywall">Subscribe to read more</div></article></body></html>"#,
            prose("TEASER", 1)
        );
        let e = extract(&html, "https://example.org/p");
        assert!(e.text.contains("FULL paragraph 7"), "via {}: {}", e.source, e.text);
    }

    #[test]
    fn ordinary_articles_are_not_replaced_by_hydration_noise() {
        // A normal article page that also ships a small state blob.
        let html = format!(
            r#"<html><head><title>A</title></head><body><article><p>{}</p><p>{}</p></article>
            <script>window.__INITIAL_STATE__ = {{"related":"{}"}};</script></body></html>"#,
            prose("REAL", 6),
            prose("ALSO", 6),
            prose("RELATED", 1)
        );
        let e = extract(&html, "https://example.org/a");
        assert_ne!(e.source, "hydration");
        assert!(e.text.contains("REAL paragraph 5"));
    }
}
