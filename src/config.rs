// ============================================================================
// Qrux v5.0.1 - Configuration
// Advanced browser fingerprinting, WAF bypass, and stealth scraping config
// ============================================================================

use rand::seq::SliceRandom;
use rand::Rng;

/// Maximum URLs to scrape per query.
pub fn max_urls() -> usize {
    std::env::var("MAX_URLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(420)
}

/// Concurrent scrape limit.
pub fn concurrency() -> usize {
    std::env::var("CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24)
}

/// Concurrent engine request limit.
pub fn engine_concurrency() -> usize {
    std::env::var("ENGINE_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10)
}

/// Scrape timeout per URL (seconds). 0 means use client default.
/// Changed from 0 (infinite) to 12s — this was the root cause of
/// extraction failing after ~10 sources (hung connections starving semaphore).
pub fn scrape_timeout_secs() -> u64 {
    std::env::var("SCRAPE_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12)
}

/// Server port.
pub fn port() -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8000)
}

/// Maximum HTML bytes to download per page.
pub fn max_html_bytes() -> usize {
    std::env::var("MAX_HTML_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_500_000)
}

/// Minimum extracted text length to consider a scrape successful.
/// Changed from hardcoded 0 to configurable with default 50.
pub fn min_text_length() -> usize {
    std::env::var("MIN_TEXT_LENGTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
}

pub fn jitter_min_ms() -> u64 {
    std::env::var("JITTER_MIN_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
}

pub fn jitter_max_ms() -> u64 {
    std::env::var("JITTER_MAX_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
}

/// Cooldown window for failing proxies.
pub fn proxy_cooldown_secs() -> u64 {
    std::env::var("PROXY_COOLDOWN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120)
}

pub fn random_jitter_ms(min_ms: u64, max_ms: u64) -> u64 {
    let lo = min_ms.min(max_ms);
    let hi = min_ms.max(max_ms);
    if lo == hi {
        return lo;
    }
    rand::thread_rng().gen_range(lo..=hi)
}

// =============================================================================
// Browser Profile System — 12 Realistic Profiles for WAF Bypass
// =============================================================================
// Each profile is a coherent browser "identity" — the UA string, client hints,
// language, and referer all match what that real browser would send.
// Anti-bot systems verify this consistency; mismatched headers are flagged.

struct BrowserProfile {
    user_agent: &'static str,
    sec_ch_ua: &'static str,
    sec_ch_ua_mobile: &'static str,
    sec_ch_ua_platform: &'static str,
    accept_language: &'static str,
    referer: &'static str,
}

const BROWSER_PROFILES: &[BrowserProfile] = &[
    // ── Chrome 128 — Windows ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"128\", \"Not;A=Brand\";v=\"24\", \"Google Chrome\";v=\"128\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.google.com/",
    },
    // ── Chrome 127 — Windows ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"127\", \"Not)A;Brand\";v=\"99\", \"Google Chrome\";v=\"127\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-US,en;q=0.9,es;q=0.8",
        referer: "https://www.google.com/",
    },
    // ── Chrome 128 — macOS ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"128\", \"Not;A=Brand\";v=\"24\", \"Google Chrome\";v=\"128\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"macOS\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.google.com/",
    },
    // ── Chrome 126 — Linux ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"126\", \"Not-A.Brand\";v=\"24\", \"Google Chrome\";v=\"126\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Linux\"",
        accept_language: "en-US,en;q=0.7",
        referer: "https://search.brave.com/",
    },
    // ── Edge 128 — Windows ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36 Edg/128.0.0.0",
        sec_ch_ua: "\"Chromium\";v=\"128\", \"Not;A=Brand\";v=\"24\", \"Microsoft Edge\";v=\"128\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.bing.com/",
    },
    // ── Edge 127 — Windows ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36 Edg/127.0.0.0",
        sec_ch_ua: "\"Chromium\";v=\"127\", \"Not)A;Brand\";v=\"99\", \"Microsoft Edge\";v=\"127\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-GB,en;q=0.9",
        referer: "https://www.bing.com/",
    },
    // ── Safari 17.5 — macOS ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-US,en;q=0.8",
        referer: "https://duckduckgo.com/",
    },
    // ── Safari 18.0 — macOS ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6_1) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-AU,en;q=0.9",
        referer: "https://www.google.com.au/",
    },
    // ── Firefox 129 — Windows ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:129.0) Gecko/20100101 Firefox/129.0",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-US,en;q=0.5",
        referer: "https://www.google.com/",
    },
    // ── Firefox 128 — macOS ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.5; rv:128.0) Gecko/20100101 Firefox/128.0",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-US,en;q=0.5",
        referer: "https://duckduckgo.com/",
    },
    // ── Firefox 129 — Linux ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (X11; Linux x86_64; rv:129.0) Gecko/20100101 Firefox/129.0",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-US,en;q=0.5",
        referer: "https://search.brave.com/",
    },
    // ── Chrome 127 — ChromeOS ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (X11; CrOS x86_64 14541.0.0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"127\", \"Not)A;Brand\";v=\"99\", \"Google Chrome\";v=\"127\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Chrome OS\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.google.com/",
    },
    // ── Chrome 131 — Windows 11 (2026 latest) ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"131\", \"Not_A Brand\";v=\"24\", \"Google Chrome\";v=\"131\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.google.com/",
    },
    // ── Chrome 131 — macOS Sequoia ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_2) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"131\", \"Not_A Brand\";v=\"24\", \"Google Chrome\";v=\"131\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"macOS\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.google.com/",
    },
    // ── Edge 131 — Windows 11 ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0",
        sec_ch_ua: "\"Chromium\";v=\"131\", \"Not_A Brand\";v=\"24\", \"Microsoft Edge\";v=\"131\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.bing.com/",
    },
    // ── Firefox 133 — Windows 11 ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:133.0) Gecko/20100101 Firefox/133.0",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-US,en;q=0.5",
        referer: "https://www.google.com/",
    },
    // ── Safari 18.2 — macOS Sequoia ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_2) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.2 Safari/605.1.15",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-US,en;q=0.9",
        referer: "https://duckduckgo.com/",
    },
    // ── Firefox 133 — Linux ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-US,en;q=0.5",
        referer: "https://search.brave.com/",
    },
];

pub struct BrowserHeaders {
    pub user_agent: &'static str,
    pub sec_ch_ua: &'static str,
    pub sec_ch_ua_mobile: &'static str,
    pub sec_ch_ua_platform: &'static str,
    pub accept_language: &'static str,
    pub referer: &'static str,
}

pub fn random_browser_headers() -> BrowserHeaders {
    let mut rng = rand::thread_rng();
    let profile = BROWSER_PROFILES
        .choose(&mut rng)
        .unwrap_or(&BROWSER_PROFILES[0]);

    BrowserHeaders {
        user_agent: profile.user_agent,
        sec_ch_ua: profile.sec_ch_ua,
        sec_ch_ua_mobile: profile.sec_ch_ua_mobile,
        sec_ch_ua_platform: profile.sec_ch_ua_platform,
        accept_language: profile.accept_language,
        referer: profile.referer,
    }
}

pub fn random_user_agent() -> &'static str {
    random_browser_headers().user_agent
}

/// Apply a full suite of browser-realistic headers to any request.
/// Includes Sec-Fetch-* headers that modern WAFs check for consistency.
pub fn apply_browser_headers(
    builder: reqwest::RequestBuilder,
    target_url: &str,
) -> reqwest::RequestBuilder {
    let headers = random_browser_headers();

    // Use the target site's own domain as referer (looks like a click-through)
    let dynamic_referer = url::Url::parse(target_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| format!("https://{}/", h)))
        .unwrap_or_else(|| headers.referer.to_string());

    let mut req = builder
        .header("User-Agent", headers.user_agent)
        .header("Accept-Language", headers.accept_language)
        .header("Referer", dynamic_referer)
        // ── Modern Sec-Fetch headers (required by WAFs) ──
        .header("Sec-Fetch-Site", "none")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-User", "?1")
        // ── Additional browser signals ──
        .header("DNT", "1")
        .header("Upgrade-Insecure-Requests", "1")
        .header("Cache-Control", "max-age=0")
        .header("Priority", "u=0, i");

    // Chrome/Edge send client hints; Firefox/Safari do not
    if !headers.sec_ch_ua.is_empty() {
        req = req.header("Sec-CH-UA", headers.sec_ch_ua);
        // v5.0: Additional client hints that Cloudflare/Akamai WAFs now check
        req = req.header("Sec-CH-UA-Arch", "\"x86\"");
        req = req.header("Sec-CH-UA-Bitness", "\"64\"");
        req = req.header("Sec-CH-UA-Full-Version-List", headers.sec_ch_ua);
    }
    if !headers.sec_ch_ua_mobile.is_empty() {
        req = req.header("Sec-CH-UA-Mobile", headers.sec_ch_ua_mobile);
    }
    if !headers.sec_ch_ua_platform.is_empty() {
        req = req.header("Sec-CH-UA-Platform", headers.sec_ch_ua_platform);
        // v5.0: Platform version hint (modern WAFs validate this)
        let platform_ver = if headers.sec_ch_ua_platform.contains("Windows") {
            "\"15.0.0\""
        } else if headers.sec_ch_ua_platform.contains("macOS") {
            "\"15.2.0\""
        } else if headers.sec_ch_ua_platform.contains("Chrome OS") {
            "\"14541.0.0\""
        } else {
            "\"6.8.0\""
        };
        req = req.header("Sec-CH-UA-Platform-Version", platform_ver);
    }

    // v5.0: Randomize Accept header variants (evade fingerprint-based blocking)
    let mut rng = rand::thread_rng();
    let accept_variants = [
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    ];
    req = req.header("Accept", accept_variants[rng.gen_range(0..accept_variants.len())]);

    req
}

pub fn user_agents_count() -> usize {
    BROWSER_PROFILES.len()
}

// =============================================================================
// Engines — Full list of enabled search engines
// =============================================================================

pub fn enabled_engines() -> Vec<String> {
    let default = "wikipedia,duckduckgo,duckduckgo_html,duckduckgo_news,duckduckgo_images,duckduckgo_videos,brave,brave_news,yahoo,yahoo_news,bing,bing_news,bing_images,bing_videos,bing_us,bing_uk,bing_in,bing_de,bing_fr,bing_es,bing_it,bing_jp,bing_ca,bing_au,bing_nl,bing_se,bing_no,bing_fi,google,google_news,google_scholar,google_images,google_videos,google_us,google_uk,google_in,google_de,google_fr,google_es,google_it,google_br,google_jp,google_ca,google_au,google_nl,google_se,google_no,google_fi,qwant,startpage,mojeek,yandex,yandex_ru,yandex_global,baidu,baidu_cn,ecosia,ecosia_de,ecosia_fr,metager,metager_de,swisscows,swisscows_ch,ask,ask_us,aol,aol_search,lycos,dogpile,gibiru,searchencrypt,presearch,yep,mwmbl,sogou,sogou_cn,naver,daum,seznam,rambler,searchalot,excite,webcrawler,info,pipilika,kiddle,marginalia,wiby,right_dao,stract";

    let raw = std::env::var("ENGINES").unwrap_or_else(|_| default.to_string());
    raw.split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

// =============================================================================
// Domains to Skip
// =============================================================================

pub const SKIP_DOMAINS: &[&str] = &[
    "vimeo.com",
    "twitter.com",
    "x.com",
    "facebook.com",
    "instagram.com",
    "linkedin.com",
    "pinterest.com",
    "tiktok.com",
    "play.google.com",
    "apps.apple.com",
    "drive.google.com",
    "docs.google.com",
    "amazon.com",
    "ebay.com",
    "aliexpress.com",
];

pub const SKIP_EXTENSIONS: &[&str] = &[
    ".zip", ".rar", ".7z", ".tar", ".gz", ".mp3", ".mp4", ".avi", ".mkv", ".mov", ".exe", ".msi", ".dmg", ".apk",
];

// =============================================================================
// Tracking Parameters to Remove
// =============================================================================

pub const TRACKING_PARAMS: &[&str] = &[
    "utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content",
    "utm_id", "utm_cid", "fbclid", "gclid", "gclsrc", "dclid", "msclkid",
    "twclid", "igshid", "ref", "source", "src", "campaign", "affiliate", "partner",
    "_ga", "_gl", "_gid", "mc_cid", "mc_eid", "mkt_tok", "amp", "amp_js_v", "usqp",
    "spm", "share_from", "scm",
];
