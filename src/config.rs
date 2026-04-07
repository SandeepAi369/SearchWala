// ══════════════════════════════════════════════════════════════════════════════
// Swift Search Agent v3.0 — Configuration
// ══════════════════════════════════════════════════════════════════════════════

use rand::seq::SliceRandom;

/// Maximum URLs to scrape per query
pub fn max_urls() -> usize {
    std::env::var("MAX_URLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15)
}

/// Concurrent scrape limit
pub fn concurrency() -> usize {
    std::env::var("CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
}

/// Scrape timeout per URL (seconds)
pub fn scrape_timeout_secs() -> u64 {
    std::env::var("SCRAPE_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10)
}

/// Server port
pub fn port() -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8000)
}

/// Maximum HTML bytes to download per page (prevents OOM on huge pages)
pub fn max_html_bytes() -> usize {
    std::env::var("MAX_HTML_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500_000) // 500KB
}

/// Minimum extracted text length to consider a scrape successful
pub fn min_text_length() -> usize {
    50
}

// ─── User-Agent Rotation ─────────────────────────────────────────────────────

const USER_AGENTS: &[&str] = &[
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15",
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36 Edg/125.0.0.0",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0",
];

pub fn random_user_agent() -> &'static str {
    let mut rng = rand::thread_rng();
    USER_AGENTS.choose(&mut rng).unwrap_or(&USER_AGENTS[0])
}

pub fn user_agents_count() -> usize {
    USER_AGENTS.len()
}

// ─── Engines ─────────────────────────────────────────────────────────────────

pub fn enabled_engines() -> Vec<String> {
    let default = "duckduckgo,brave,yahoo,qwant,mojeek";
    let raw = std::env::var("ENGINES").unwrap_or_else(|_| default.to_string());
    raw.split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

// ─── Domains to Skip ─────────────────────────────────────────────────────────

pub const SKIP_DOMAINS: &[&str] = &[
    "youtube.com", "youtu.be", "vimeo.com",
    "twitter.com", "x.com", "facebook.com", "instagram.com",
    "linkedin.com", "pinterest.com", "tiktok.com",
    "reddit.com",
    "play.google.com", "apps.apple.com",
    "drive.google.com", "docs.google.com",
    "amazon.com", "ebay.com", "aliexpress.com",
];

pub const SKIP_EXTENSIONS: &[&str] = &[
    ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx",
    ".zip", ".rar", ".7z", ".tar", ".gz",
    ".mp3", ".mp4", ".avi", ".mkv", ".mov",
    ".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg",
    ".exe", ".msi", ".dmg", ".apk",
];

// ─── Tracking Parameters to Remove ──────────────────────────────────────────

pub const TRACKING_PARAMS: &[&str] = &[
    "utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content",
    "utm_id", "utm_cid",
    "fbclid", "gclid", "gclsrc", "dclid", "msclkid",
    "twclid", "igshid",
    "ref", "source", "src", "campaign", "affiliate", "partner",
    "_ga", "_gl", "_gid", "mc_cid", "mc_eid", "mkt_tok",
    "amp", "amp_js_v", "usqp",
    "spm", "share_from", "scm",
];
