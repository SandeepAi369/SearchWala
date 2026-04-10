// ============================================================================
// Swift Search Agent v4.0 - Configuration
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

/// Scrape timeout per URL (seconds). 0 means no timeout.
pub fn scrape_timeout_secs() -> u64 {
    std::env::var("SCRAPE_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
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
pub fn min_text_length() -> usize {
    0
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

// --- Browser Header Rotation -------------------------------------------------

struct BrowserProfile {
    user_agent: &'static str,
    sec_ch_ua: &'static str,
    sec_ch_ua_mobile: &'static str,
    sec_ch_ua_platform: &'static str,
    accept_language: &'static str,
    referer: &'static str,
}

const BROWSER_PROFILES: &[BrowserProfile] = &[
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"126\", \"Not.A/Brand\";v=\"24\", \"Google Chrome\";v=\"126\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.google.com/",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0",
        sec_ch_ua: "\"Chromium\";v=\"126\", \"Not.A/Brand\";v=\"24\", \"Microsoft Edge\";v=\"126\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.bing.com/",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-US,en;q=0.8",
        referer: "https://duckduckgo.com/",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"126\", \"Not.A/Brand\";v=\"24\", \"Google Chrome\";v=\"126\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Linux\"",
        accept_language: "en-US,en;q=0.7",
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

pub fn apply_browser_headers(
    builder: reqwest::RequestBuilder,
    target_url: &str,
) -> reqwest::RequestBuilder {
    let headers = random_browser_headers();

    let dynamic_referer = url::Url::parse(target_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| format!("https://{}/", h)))
        .unwrap_or_else(|| headers.referer.to_string());

    let mut req = builder
        .header("User-Agent", headers.user_agent)
        .header("Accept-Language", headers.accept_language)
        .header("Referer", dynamic_referer);

    if !headers.sec_ch_ua.is_empty() {
        req = req.header("Sec-CH-UA", headers.sec_ch_ua);
    }
    if !headers.sec_ch_ua_mobile.is_empty() {
        req = req.header("Sec-CH-UA-Mobile", headers.sec_ch_ua_mobile);
    }
    if !headers.sec_ch_ua_platform.is_empty() {
        req = req.header("Sec-CH-UA-Platform", headers.sec_ch_ua_platform);
    }

    req
}

pub fn user_agents_count() -> usize {
    BROWSER_PROFILES.len()
}

// --- Engines -----------------------------------------------------------------

pub fn enabled_engines() -> Vec<String> {
    let default = "wikipedia,duckduckgo,duckduckgo_html,duckduckgo_news,duckduckgo_images,duckduckgo_videos,brave,brave_news,yahoo,yahoo_news,bing,bing_news,bing_images,bing_videos,bing_us,bing_uk,bing_in,bing_de,bing_fr,bing_es,bing_it,bing_jp,bing_ca,bing_au,bing_nl,bing_se,bing_no,bing_fi,google,google_news,google_scholar,google_images,google_videos,google_us,google_uk,google_in,google_de,google_fr,google_es,google_it,google_br,google_jp,google_ca,google_au,google_nl,google_se,google_no,google_fi,qwant,startpage,mojeek,yandex,yandex_ru,yandex_global,baidu,baidu_cn,ecosia,ecosia_de,ecosia_fr,metager,metager_de,swisscows,swisscows_ch,ask,ask_us,aol,aol_search,lycos,dogpile,gibiru,searchencrypt,presearch,yep,mwmbl,sogou,sogou_cn,naver,daum,seznam,rambler,searchalot,excite,webcrawler,info,pipilika,kiddle";

    let raw = std::env::var("ENGINES").unwrap_or_else(|_| default.to_string());
    raw.split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

// --- Domains to Skip ---------------------------------------------------------

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

// --- Tracking Parameters to Remove ------------------------------------------

pub const TRACKING_PARAMS: &[&str] = &[
    "utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content",
    "utm_id", "utm_cid", "fbclid", "gclid", "gclsrc", "dclid", "msclkid",
    "twclid", "igshid", "ref", "source", "src", "campaign", "affiliate", "partner",
    "_ga", "_gl", "_gid", "mc_cid", "mc_eid", "mkt_tok", "amp", "amp_js_v", "usqp",
    "spm", "share_from", "scm",
];
