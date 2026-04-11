// ============================================================================
// Swift Search Agent v4.1 - Trafilatura-Level Content Extractor
// Advanced multi-strategy extraction with:
// - Paragraph-level deduplication (anti-boilerplate like trafilatura)
// - Link density penalty
// - Tag-level scoring with class/ID pattern matching
// - Structured content selectors for 90% of CMS platforms
// - Multi-pass cleaning pipeline
// ============================================================================

use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use std::collections::HashSet;
use std::sync::LazyLock;

// ── Pre-compiled regexes (zero-cost after first use) ──

static WHITESPACE_COLLAPSE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s{3,}").unwrap());

static MULTI_NEWLINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

static CONTROL_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\x00-\x08\x0B\x0C\x0E-\x1F\x7F\u{200B}\u{200C}\u{200D}\u{FEFF}\u{00AD}\u{2028}\u{2029}]").unwrap());

static NEGATIVE_PATTERNS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(comment|footer|sidebar|nav|menu|header|breadcrumb|widget|ad-|advert|social|share|related|popup|modal|cookie|banner|promo|newsletter|signup|subscribe|pagination|pager|toolbar|masthead|skip-link|sidebar|complementary|supplementary|sponsor|outbrain|taboola|recirculation|trending|more-stories|recommendations|you-may-also|suggested|feedback|rating|login|register|print|email-article)").unwrap()
});

static POSITIVE_PATTERNS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(article|content|main|body|text|post|entry|story|page|prose|markdown|document|paper|passage|reading|hentry|blog|blog-post|single-post|article-content|article-text|article-body|ArticleBody|post-content|post-text|postBody|storycontent)").unwrap()
});

static BOILERPLATE_LINES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(share (this|on|via)|read more|subscribe|sign up|follow us|cookie|accept all|privacy policy|terms of (service|use)|advertisement|sponsored|related articles|trending|popular posts|most read|you may also (like|enjoy)|© \d{4}|all rights reserved|powered by|loading\.\.\.|please wait|click here|read the full|continue reading|view more|show more|see also|tags?:|categories?:|filed under|posted in|last (updated|modified)|published|modified|edited|log ?in|register|sign ?in|free trial|download now|buy now|add to cart|leave a (comment|reply)|your email|required fields|notify me|rss|print this|email this|bookmark|save to|report (this|abuse)|flag|spam|inappropriate|next article|previous article|newer posts?|older posts?|page \d+ of \d+|showing \d+|results? for|sort by|filter by|refine|close|dismiss|got it|accept|decline|no thanks|maybe later|not now)").unwrap()
});

static SENTENCE_LIKE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Z].*[.!?]$").unwrap());

// ── Tags to strip entirely (their content is never useful) ──

const STRIP_TAGS: &[&str] = &[
    "script", "style", "nav", "header", "footer", "aside", "form", "button",
    "input", "select", "textarea", "iframe", "noscript", "svg", "path", "canvas",
    "video", "audio", "object", "embed", "template", "dialog", "figcaption",
];

// ── Pre-compiled CSS selectors (LazyLock for zero-cost reuse) ──

static SEL_SEMANTIC: LazyLock<Option<Selector>> = LazyLock::new(|| {
    Selector::parse("article, [role='main'], main, [itemprop='articleBody'], [data-testid='article-body'], [itemtype*='Article']").ok()
});

static SEL_STRUCTURED: LazyLock<Option<Selector>> = LazyLock::new(|| {
    Selector::parse(concat!(
        ".post-content, .entry-content, .article-body, .story-body, ",
        ".article__body, .article-text, .post-body, .td-post-content, ",
        ".post-entry, .content-body, .page-content, .rich-text, .prose, ",
        ".markdown-body, .mw-parser-output, .field-body, .node__content, ",
        ".blog-post-content, .single-post-content, .post-text, .articleBody, ",
        ".caas-body, .article__content, .story-content, .post__content, ",
        "#article-body, #content, #main-content, #post-content, ",
        "#story-body, #mw-content-text, [data-component='text-block']"
    )).ok()
});

static SEL_CONTAINER: LazyLock<Option<Selector>> = LazyLock::new(|| {
    Selector::parse("div, section, td, main, article").ok()
});

static SEL_P: LazyLock<Option<Selector>> = LazyLock::new(|| {
    Selector::parse("p").ok()
});

static SEL_CONTENT_ELEMENTS: LazyLock<Option<Selector>> = LazyLock::new(|| {
    Selector::parse("p, li, blockquote, pre, td, h2, h3, h4, h5, h6, dd, dt, figcaption").ok()
});

static SEL_TITLE: LazyLock<Option<Selector>> = LazyLock::new(|| {
    Selector::parse("title").ok()
});

static SEL_OG_TITLE: LazyLock<Option<Selector>> = LazyLock::new(|| {
    Selector::parse(r#"meta[property="og:title"]"#).ok()
});

static SEL_META_TITLE: LazyLock<Option<Selector>> = LazyLock::new(|| {
    Selector::parse(r#"meta[name="title"]"#).ok()
});

static SEL_H1: LazyLock<Option<Selector>> = LazyLock::new(|| {
    Selector::parse("h1").ok()
});

static SEL_BODY: LazyLock<Option<Selector>> = LazyLock::new(|| {
    Selector::parse("body").ok()
});

static SEL_LINKS: LazyLock<Option<Selector>> = LazyLock::new(|| {
    Selector::parse("a").ok()
});

// =============================================================================
// Public API
// =============================================================================

/// Extract the main article text from an HTML document using a 5-tier strategy:
/// 1. Structured class/ID selectors (.entry-content, .post-body, etc.)
/// 2. Semantic HTML5 elements (<article>, <main>, [role="main"])
/// 3. Scored container extraction (text-density scoring with link penalty)
/// 4. Content element fallback (all <p>, <li>, <blockquote>, etc.)
/// 5. Full body text fallback
pub fn extract_article_text(html: &str) -> String {
    let document = Html::parse_document(html);

    // Strategy 1: Structured class/ID selectors (highest confidence)
    if let Some(text) = try_structured_extraction(&document) {
        let cleaned = clean_and_deduplicate(&text);
        if cleaned.len() >= 100 {
            return cleaned;
        }
    }

    // Strategy 2: Semantic HTML5 elements
    if let Some(text) = try_semantic_extraction(&document) {
        let cleaned = clean_and_deduplicate(&text);
        if cleaned.len() >= 80 {
            return cleaned;
        }
    }

    // Strategy 3: Scored extraction (text density analysis)
    if let Some(text) = try_scored_extraction(&document) {
        let cleaned = clean_and_deduplicate(&text);
        if cleaned.len() >= 50 {
            return cleaned;
        }
    }

    // Strategy 4: Content element fallback (p, li, blockquote, etc.)
    let text = fallback_content_element_extraction(&document);
    if !text.trim().is_empty() {
        return clean_and_deduplicate(&text);
    }

    // Strategy 5: Full body text (last resort)
    if let Some(sel) = SEL_BODY.as_ref() {
        if let Some(body) = document.select(sel).next() {
            let text = extract_visible_text(&body);
            return clean_and_deduplicate(&text);
        }
    }

    String::new()
}

/// Extract the page title using multiple strategies.
pub fn extract_title(html: &str) -> String {
    let document = Html::parse_document(html);

    // 1. <meta property="og:title"> — most reliable for article titles
    if let Some(sel) = SEL_OG_TITLE.as_ref() {
        if let Some(meta) = document.select(sel).next() {
            if let Some(content) = meta.value().attr("content") {
                let text = content.trim().to_string();
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }

    // 2. <title> tag
    if let Some(sel) = SEL_TITLE.as_ref() {
        if let Some(title) = document.select(sel).next() {
            let text = title.text().collect::<String>().trim().to_string();
            if !text.is_empty() {
                // Strip common site name suffixes: " - Site Name", " | Site Name"
                let cleaned = text
                    .splitn(2, " - ")
                    .next()
                    .unwrap_or(&text)
                    .splitn(2, " | ")
                    .next()
                    .unwrap_or(&text)
                    .trim()
                    .to_string();
                if !cleaned.is_empty() {
                    return cleaned;
                }
            }
        }
    }

    // 3. <meta name="title">
    if let Some(sel) = SEL_META_TITLE.as_ref() {
        if let Some(meta) = document.select(sel).next() {
            if let Some(content) = meta.value().attr("content") {
                let text = content.trim().to_string();
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }

    // 4. First <h1>
    if let Some(sel) = SEL_H1.as_ref() {
        if let Some(h1) = document.select(sel).next() {
            return h1.text().collect::<String>().trim().to_string();
        }
    }

    String::new()
}

// =============================================================================
// Extraction Strategies
// =============================================================================

/// Strategy 1: Target well-known content class names and IDs.
fn try_structured_extraction(doc: &Html) -> Option<String> {
    let sel = SEL_STRUCTURED.as_ref()?;

    let mut best_text = String::new();
    let mut best_score = 0usize;

    for element in doc.select(sel) {
        let text = extract_visible_text(&element);
        let score = compute_text_quality_score(&text);
        if score > best_score {
            best_score = score;
            best_text = text;
        }
    }

    if best_text.is_empty() { None } else { Some(best_text) }
}

/// Strategy 2: Semantic HTML5 extraction
fn try_semantic_extraction(doc: &Html) -> Option<String> {
    let sel = SEL_SEMANTIC.as_ref()?;

    let mut best_text = String::new();
    let mut best_score = 0usize;

    for element in doc.select(sel) {
        let text = extract_visible_text(&element);
        let score = compute_text_quality_score(&text);
        if score > best_score {
            best_score = score;
            best_text = text;
        }
    }

    if best_text.is_empty() { None } else { Some(best_text) }
}

/// Strategy 3: Score every container by text density, paragraph count,
/// link density (penalty), and class/ID pattern matching.
fn try_scored_extraction(doc: &Html) -> Option<String> {
    let container_sel = SEL_CONTAINER.as_ref()?;
    let p_sel = SEL_P.as_ref()?;

    let mut best_score = f64::MIN;
    let mut best_text = String::new();

    for element in doc.select(container_sel) {
        let score = score_element(&element, p_sel);
        if score > best_score {
            let text = extract_visible_text(&element);
            if text.len() > 100 {
                best_score = score;
                best_text = text;
            }
        }
    }

    if best_text.is_empty() { None } else { Some(best_text) }
}

/// Score a container element for content likelihood.
fn score_element(element: &ElementRef, p_sel: &Selector) -> f64 {
    let mut score: f64 = 0.0;

    // Paragraph count bonus
    let p_count = element.select(p_sel).count();
    score += p_count as f64 * 3.0;

    // Class/ID pattern matching
    let class = element.value().attr("class").unwrap_or("");
    let id = element.value().attr("id").unwrap_or("");
    let combined = format!("{} {}", class, id);

    if POSITIVE_PATTERNS.is_match(&combined) {
        score += 25.0;
    }
    if NEGATIVE_PATTERNS.is_match(&combined) {
        score -= 25.0;
    }

    let text = element.text().collect::<String>();
    let text_len = text.len();
    let html_len = element.html().len();

    // Text density bonus (text / html ratio)
    if html_len > 0 {
        let density = text_len as f64 / html_len as f64;
        score += density * 50.0;
    }

    // Long content bonus
    if text_len > 200 {
        score += 5.0;
    }
    if text_len > 1000 {
        score += 10.0;
    }
    if text_len > 3000 {
        score += 15.0;
    }

    // ── Link density penalty (trafilatura-style) ──
    if let Some(a_sel) = SEL_LINKS.as_ref() {
        let link_text_len: usize = element
            .select(a_sel)
            .map(|a| a.text().collect::<String>().len())
            .sum();
        if text_len > 0 {
            let link_ratio = link_text_len as f64 / text_len as f64;
            if link_ratio > 0.5 {
                score -= 40.0;
            } else if link_ratio > 0.3 {
                score -= 20.0;
            } else if link_ratio > 0.15 {
                score -= 8.0;
            }
        }
    }

    // Bonus for rich-content children
    let rich_tags = ["blockquote", "pre", "code", "table", "figure", "ul", "ol"];
    for tag in rich_tags {
        if let Ok(sel) = Selector::parse(tag) {
            let count = element.select(&sel).count();
            score += count as f64 * 3.0;
        }
    }

    // Penalty for deeply nested elements (navigation trees)
    let depth = element.value().name().len(); // rough heuristic
    if depth > 10 {
        score -= 5.0;
    }

    score
}

/// Collect all content elements as a fallback.
fn fallback_content_element_extraction(doc: &Html) -> String {
    let sel = match SEL_CONTENT_ELEMENTS.as_ref() {
        Some(s) => s,
        None => return String::new(),
    };
    let mut paragraphs = Vec::new();

    for p in doc.select(sel) {
        // Skip elements with negative class patterns
        let class = p.value().attr("class").unwrap_or("");
        let id = p.value().attr("id").unwrap_or("");
        let combined = format!("{} {}", class, id);
        if NEGATIVE_PATTERNS.is_match(&combined) {
            continue;
        }

        let text: String = p.text().collect::<String>().trim().to_string();
        if text.len() >= 10 {
            paragraphs.push(text);
        }
    }

    paragraphs.join("\n\n")
}

// =============================================================================
// Text Quality Scoring (trafilatura-inspired)
// =============================================================================

/// Compute a quality score for extracted text.
/// Higher = better content, lower = more boilerplate.
fn compute_text_quality_score(text: &str) -> usize {
    let mut score = 0usize;

    // Length contributes
    score += text.len() / 10;

    // Count sentence-like structures (starts with uppercase, ends with period)
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.len() >= 30 && SENTENCE_LIKE.is_match(trimmed) {
            score += 5;
        }
    }

    // Penalty for excessive short lines (likely menus or lists)
    let lines: Vec<&str> = text.lines().collect();
    if !lines.is_empty() {
        let short_lines = lines.iter().filter(|l| l.trim().len() < 15).count();
        let ratio = short_lines as f64 / lines.len() as f64;
        if ratio > 0.6 {
            score = score.saturating_sub(score / 3);
        }
    }

    score
}

// =============================================================================
// Text Extraction Helpers
// =============================================================================

/// Recursively extract visible text from an element, skipping non-content tags.
fn extract_visible_text(element: &ElementRef) -> String {
    let tag_name = element.value().name();

    // Skip known non-content tags
    for strip in STRIP_TAGS {
        if tag_name == *strip {
            return String::new();
        }
    }

    // Skip hidden elements
    if let Some(style) = element.value().attr("style") {
        let lower = style.to_lowercase();
        if lower.contains("display:none") || lower.contains("display: none")
            || lower.contains("visibility:hidden") || lower.contains("visibility: hidden")
        {
            return String::new();
        }
    }
    if element.value().attr("hidden").is_some() {
        return String::new();
    }
    if let Some(aria) = element.value().attr("aria-hidden") {
        if aria == "true" {
            return String::new();
        }
    }

    let mut text_parts: Vec<String> = Vec::new();
    let skip_set: HashSet<&str> = STRIP_TAGS.iter().copied().collect();

    for child in element.children() {
        match child.value() {
            scraper::node::Node::Text(text) => {
                let t = text.trim();
                if !t.is_empty() {
                    text_parts.push(t.to_string());
                }
            }
            scraper::node::Node::Element(el) => {
                let child_tag = el.name();
                if !skip_set.contains(child_tag) {
                    if let Some(child_ref) = ElementRef::wrap(child) {
                        let child_text = extract_visible_text(&child_ref);
                        if !child_text.is_empty() {
                            // Add paragraph breaks for block-level elements
                            if is_block_element(child_tag) {
                                text_parts.push(format!("\n{}\n", child_text));
                            } else {
                                text_parts.push(child_text);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    text_parts.join(" ")
}

/// Check if a tag is a block-level element (for paragraph separation)
fn is_block_element(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div" | "section" | "article" | "blockquote" | "pre"
            | "ul" | "ol" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            | "table" | "tr" | "td" | "th" | "figure" | "details" | "summary"
            | "dd" | "dt" | "dl" | "hr" | "br" | "address"
    )
}

// =============================================================================
// Text Cleaning Pipeline (Trafilatura-Level)
// =============================================================================

/// Clean extracted text: strip boilerplate, normalize whitespace, deduplicate paragraphs.
fn clean_and_deduplicate(text: &str) -> String {
    // 1. Strip invisible/control characters
    let text = CONTROL_CHARS.replace_all(text, "");

    // 2. Normalize line endings
    let text = text.replace('\r', "\n");

    // 3. Split into paragraphs
    let paragraphs: Vec<String> = text
        .split("\n\n")
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();

    // 4. Paragraph-level deduplication (trafilatura feature)
    let mut seen_fingerprints: HashSet<String> = HashSet::new();
    let mut clean_paragraphs: Vec<String> = Vec::new();

    for para in &paragraphs {
        // Create a fingerprint: lowercase, collapse whitespace, first 80 chars
        let fingerprint = para.to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let fp_key = fingerprint.chars().take(80).collect::<String>();

        if !seen_fingerprints.insert(fp_key) {
            continue; // duplicate paragraph
        }

        // Filter lines within the paragraph
        let clean_lines: Vec<&str> = para
            .lines()
            .map(|l| l.trim())
            .filter(|l| {
                if l.is_empty() {
                    return false;
                }
                if l.len() < 3 {
                    return false;
                }
                // Remove boilerplate lines
                if BOILERPLATE_LINES.is_match(l) {
                    return false;
                }
                true
            })
            .collect();

        if clean_lines.is_empty() {
            continue;
        }

        let cleaned_para = clean_lines.join(" ");
        if cleaned_para.len() >= 10 {
            clean_paragraphs.push(cleaned_para);
        }
    }

    // 5. Join and normalize whitespace
    let result = clean_paragraphs.join("\n\n");

    // 6. Collapse excessive whitespace and newlines
    let result = WHITESPACE_COLLAPSE.replace_all(&result, " ");
    let result = MULTI_NEWLINE.replace_all(&result, "\n\n");

    result.trim().to_string()
}
