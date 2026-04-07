// ══════════════════════════════════════════════════════════════════════════════
// Swift Search Agent v3.0 — Content Extractor
// Replaces Trafilatura: Readability-style heuristic text extraction in pure Rust
// ══════════════════════════════════════════════════════════════════════════════

use scraper::{Html, Selector, ElementRef};
use regex::Regex;
use std::sync::LazyLock;

// ─── Compiled Regex Patterns (zero-cost at runtime after first use) ──────────

static WHITESPACE_COLLAPSE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s{3,}").unwrap());

static NEGATIVE_PATTERNS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(comment|footer|sidebar|nav|menu|header|breadcrumb|widget|ad-|social|share|related|popup|modal|cookie|banner|promo)").unwrap()
});

static POSITIVE_PATTERNS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(article|content|main|body|text|post|entry|story|page)").unwrap()
});

// ─── Tags to strip entirely ──────────────────────────────────────────────────

const STRIP_TAGS: &[&str] = &[
    "script", "style", "nav", "header", "footer", "aside",
    "form", "button", "input", "select", "textarea",
    "iframe", "noscript", "svg", "path", "canvas",
    "figure figcaption", // keep figure images but strip captions
];

// ─── Main Extraction Function ────────────────────────────────────────────────

/// Extract the main article text from HTML using readability heuristics.
///
/// Algorithm:
/// 1. Parse HTML into DOM
/// 2. Score candidate containers (div, article, section, main) by:
///    - Paragraph count
///    - Text density (text length / total child nodes)
///    - Positive class/id patterns (article, content, main, etc.)
///    - Negative class/id patterns (sidebar, nav, comment, etc.)
/// 3. Select the highest-scoring container
/// 4. Extract and clean the text from that container
pub fn extract_article_text(html: &str) -> String {
    let document = Html::parse_document(html);

    // Strategy 1: Try <article> tag first (semantic HTML)
    if let Some(text) = try_semantic_extraction(&document) {
        if text.len() >= 200 {
            return clean_text(&text);
        }
    }

    // Strategy 2: Readability-style scoring of content containers
    if let Some(text) = try_scored_extraction(&document) {
        if text.len() >= 100 {
            return clean_text(&text);
        }
    }

    // Strategy 3: Fallback — extract all <p> tags
    let text = fallback_paragraph_extraction(&document);
    clean_text(&text)
}

// ─── Strategy 1: Semantic HTML ───────────────────────────────────────────────

fn try_semantic_extraction(doc: &Html) -> Option<String> {
    let article_sel = Selector::parse("article, [role='main'], main").ok()?;

    let mut best_text = String::new();
    let mut best_len = 0;

    for element in doc.select(&article_sel) {
        let text = extract_visible_text(&element);
        if text.len() > best_len {
            best_len = text.len();
            best_text = text;
        }
    }

    if best_text.is_empty() {
        None
    } else {
        Some(best_text)
    }
}

// ─── Strategy 2: Readability Scoring ─────────────────────────────────────────

fn try_scored_extraction(doc: &Html) -> Option<String> {
    let container_sel = Selector::parse("div, section, td, main, article").ok()?;
    let p_sel = Selector::parse("p").ok()?;

    let mut best_score: f64 = 0.0;
    let mut best_text = String::new();

    for element in doc.select(&container_sel) {
        let score = score_element(&element, &p_sel);
        if score > best_score {
            best_score = score;
            let text = extract_visible_text(&element);
            if text.len() >= 100 {
                best_text = text;
            }
        }
    }

    if best_text.is_empty() {
        None
    } else {
        Some(best_text)
    }
}

/// Score a container element for "article-ness"
fn score_element(element: &ElementRef, p_sel: &Selector) -> f64 {
    let mut score: f64 = 0.0;

    // Count paragraphs
    let p_count = element.select(p_sel).count();
    score += p_count as f64 * 3.0;

    // Check class/id for positive/negative patterns
    let class = element.value().attr("class").unwrap_or("");
    let id = element.value().attr("id").unwrap_or("");
    let combined = format!("{} {}", class, id);

    if POSITIVE_PATTERNS.is_match(&combined) {
        score += 25.0;
    }
    if NEGATIVE_PATTERNS.is_match(&combined) {
        score -= 25.0;
    }

    // Text density: ratio of text to total content
    let text = element.text().collect::<String>();
    let text_len = text.len();
    let html_len = element.html().len();

    if html_len > 0 {
        let density = text_len as f64 / html_len as f64;
        score += density * 50.0;
    }

    // Bonus for longer text
    if text_len > 500 {
        score += 10.0;
    }
    if text_len > 2000 {
        score += 20.0;
    }

    score
}

// ─── Strategy 3: Fallback paragraph extraction ──────────────────────────────

fn fallback_paragraph_extraction(doc: &Html) -> String {
    let p_sel = Selector::parse("p").unwrap();
    let mut paragraphs = Vec::new();

    for p in doc.select(&p_sel) {
        let text: String = p.text().collect::<String>().trim().to_string();
        // Skip very short paragraphs (likely UI elements)
        if text.len() >= 30 {
            paragraphs.push(text);
        }
    }

    paragraphs.join("\n\n")
}

// ─── Text Extraction Helpers ─────────────────────────────────────────────────

/// Extract visible text from an element, skipping script/style/nav tags
fn extract_visible_text(element: &ElementRef) -> String {
    let tag_name = element.value().name();

    // Skip non-content tags
    for strip in STRIP_TAGS {
        if tag_name == *strip {
            return String::new();
        }
    }

    let mut text_parts: Vec<String> = Vec::new();
    let skip_tags: std::collections::HashSet<&str> = [
        "script", "style", "nav", "header", "footer", "aside",
        "form", "button", "iframe", "noscript", "svg",
    ].iter().cloned().collect();

    // Walk children — only extract text from content-bearing elements
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
                if !skip_tags.contains(child_tag) {
                    if let Some(child_ref) = ElementRef::wrap(child) {
                        let child_text = extract_visible_text(&child_ref);
                        if !child_text.is_empty() {
                            text_parts.push(child_text);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Join with appropriate spacing
    let joined = text_parts.join(" ");
    joined
}

/// Clean extracted text: collapse whitespace, trim
fn clean_text(text: &str) -> String {
    let cleaned = WHITESPACE_COLLAPSE.replace_all(text, "\n\n");
    cleaned.trim().to_string()
}

// ─── Title Extraction ────────────────────────────────────────────────────────

/// Extract page title from HTML
pub fn extract_title(html: &str) -> String {
    let document = Html::parse_document(html);

    // Try <title> tag first
    if let Ok(sel) = Selector::parse("title") {
        if let Some(title) = document.select(&sel).next() {
            let text = title.text().collect::<String>().trim().to_string();
            if !text.is_empty() {
                return text;
            }
        }
    }

    // Try og:title meta
    if let Ok(sel) = Selector::parse(r#"meta[property="og:title"]"#) {
        if let Some(meta) = document.select(&sel).next() {
            if let Some(content) = meta.value().attr("content") {
                let text = content.trim().to_string();
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }

    // Try h1
    if let Ok(sel) = Selector::parse("h1") {
        if let Some(h1) = document.select(&sel).next() {
            return h1.text().collect::<String>().trim().to_string();
        }
    }

    String::new()
}
