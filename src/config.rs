// ============================================================================
// SearchWala v7.0.0 - Configuration
// Advanced browser fingerprinting, WAF bypass, and stealth scraping config
// ============================================================================

use rand::seq::IndexedRandom;
use rand::RngExt;

/// Canonical version string sourced from Cargo.toml at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");


/// Concurrent scrape limit.
pub fn concurrency() -> usize {
    std::env::var("CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32)
}

/// Concurrent engine request limit.
pub fn engine_concurrency() -> usize {
    std::env::var("ENGINE_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20)
}




/// An explicitly-set numeric env var, or None. Used where a mode profile
/// supplies the default and the environment should only override it when the
/// operator actually set something.
pub fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
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

/// Random jitter with optional Poisson-like distribution for human realism.
/// Uniform random between lo..=hi, then applies a bias toward shorter delays
/// (most human actions are fast with occasional pauses).
pub fn random_jitter_ms(min_ms: u64, max_ms: u64) -> u64 {
    let lo = min_ms.min(max_ms);
    let hi = min_ms.max(max_ms);
    if lo == hi {
        return lo;
    }
    // Generate two random values and take the minimum for a right-skewed distribution
    // This mimics human behavior: mostly quick, occasionally slow
    let mut rng = rand::rng();
    let a: u64 = rng.random_range(lo..=hi);
    let b: u64 = rng.random_range(lo..=hi);
    a.min(b) // right-skewed: bias toward faster actions
}

// =============================================================================
// Browser Profile System — 20 Realistic 2026 Profiles for WAF Bypass
// =============================================================================
// Each profile is a coherent browser "identity" — the UA string, client hints,
// language, and referer all match what that real browser would send.
// Anti-bot systems verify this consistency; mismatched headers are flagged.
// ALL PROFILES UPDATED TO OCTOBER 2026 CURRENT VERSIONS.
// Chrome 155 / Edge 154 / Firefox 157 / Safari 27 / Opera 119

struct BrowserProfile {
    user_agent: &'static str,
    sec_ch_ua: &'static str,
    sec_ch_ua_mobile: &'static str,
    sec_ch_ua_platform: &'static str,
    accept_language: &'static str,
    referer: &'static str,
}

const BROWSER_PROFILES: &[BrowserProfile] = &[
    // ══════════════════════════════════════════════════════════════════
    // OCTOBER 2026 — Chrome 155, Edge 154, Firefox 157, Safari 27
    // ══════════════════════════════════════════════════════════════════

    // ── Chrome 155 — Windows 11 ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"155\", \"Not_A Brand\";v=\"24\", \"Google Chrome\";v=\"155\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.google.com/",
    },
    // ── Chrome 155 — macOS ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"155\", \"Not_A Brand\";v=\"24\", \"Google Chrome\";v=\"155\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"macOS\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.google.com/",
    },
    // ── Chrome 155 — Linux ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"155\", \"Not_A Brand\";v=\"24\", \"Google Chrome\";v=\"155\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Linux\"",
        accept_language: "en-US,en;q=0.7",
        referer: "https://www.google.com/",
    },
    // ── Chrome 154 — Windows 11 ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"154\", \"Not_A Brand\";v=\"24\", \"Google Chrome\";v=\"154\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-US,en;q=0.9,es;q=0.8",
        referer: "https://www.google.com/",
    },
    // ── Chrome 154 — macOS ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"154\", \"Not_A Brand\";v=\"24\", \"Google Chrome\";v=\"154\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"macOS\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.google.com/",
    },
    // ── Edge 154 — Windows 11 ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36 Edg/154.0.4258.48",
        sec_ch_ua: "\"Chromium\";v=\"154\", \"Not_A Brand\";v=\"24\", \"Microsoft Edge\";v=\"154\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.bing.com/",
    },
    // ── Edge 154 — macOS ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36 Edg/154.0.4258.48",
        sec_ch_ua: "\"Chromium\";v=\"154\", \"Not_A Brand\";v=\"24\", \"Microsoft Edge\";v=\"154\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"macOS\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.bing.com/",
    },
    // ── Edge 154 — Linux ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36 Edg/154.0.4258.48",
        sec_ch_ua: "\"Chromium\";v=\"154\", \"Not_A Brand\";v=\"24\", \"Microsoft Edge\";v=\"154\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Linux\"",
        accept_language: "en-GB,en;q=0.9",
        referer: "https://www.bing.com/",
    },
    // ── Firefox 157 — Windows 11 ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:157.0) Gecko/20100101 Firefox/157.0",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-US,en;q=0.5",
        referer: "https://www.google.com/",
    },
    // ── Firefox 157 — macOS ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15.8; rv:157.0) Gecko/20100101 Firefox/157.0",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-US,en;q=0.5",
        referer: "https://duckduckgo.com/",
    },
    // ── Firefox 157 — Linux ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (X11; Linux x86_64; rv:157.0) Gecko/20100101 Firefox/157.0",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-US,en;q=0.5",
        referer: "https://search.brave.com/",
    },
    // ── Firefox 156 — Windows ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:156.0) Gecko/20100101 Firefox/156.0",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-US,en;q=0.5",
        referer: "https://www.google.com/",
    },
    // ── Safari 27.0 — macOS Sequoia ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_8_1) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.0 Safari/605.1.15",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.google.com/",
    },
    // ── Safari 26.0 — macOS ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
        sec_ch_ua: "",
        sec_ch_ua_mobile: "",
        sec_ch_ua_platform: "",
        accept_language: "en-AU,en;q=0.9",
        referer: "https://duckduckgo.com/",
    },
    // ── Chrome 155 — ChromeOS ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (X11; CrOS x86_64 14816.0.0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"155\", \"Not_A Brand\";v=\"24\", \"Google Chrome\";v=\"155\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Chrome OS\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.google.com/",
    },
    // ── Chrome 153 — Windows (slightly older, still common) ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/153.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"153\", \"Not)A;Brand\";v=\"99\", \"Google Chrome\";v=\"153\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.google.com/",
    },
    // ── Opera 119 (Chrome 154 base) — Windows ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36 OPR/119.0.0.0",
        sec_ch_ua: "\"Chromium\";v=\"154\", \"Not_A Brand\";v=\"24\", \"Opera\";v=\"119\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://www.google.com/",
    },
    // ── Brave Browser (Chrome 155 base) — Windows ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"155\", \"Not_A Brand\";v=\"24\", \"Brave\";v=\"155\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-US,en;q=0.9",
        referer: "https://search.brave.com/",
    },
    // ── Vivaldi (Chrome 155 base) — Linux ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"155\", \"Not_A Brand\";v=\"24\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Linux\"",
        accept_language: "en-US,en;q=0.7",
        referer: "https://duckduckgo.com/",
    },
    // ── Chrome 155 — Windows (UK locale) ──
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36",
        sec_ch_ua: "\"Chromium\";v=\"155\", \"Not_A Brand\";v=\"24\", \"Google Chrome\";v=\"155\"",
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: "\"Windows\"",
        accept_language: "en-GB,en;q=0.9,en-US;q=0.8",
        referer: "https://www.google.co.uk/",
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
    let mut rng = rand::rng();
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


/// Context for Sec-Fetch headers — WAFs validate consistency between these
#[derive(Clone, Copy, Debug)]
pub enum RequestContext {
    /// Direct navigation (typing URL / clicking search result)
    /// Sec-Fetch-Site: none, Sec-Fetch-Mode: navigate, Sec-Fetch-User: ?1
    Navigation,
    /// Cross-site navigation (following a link from another site)
    /// Sec-Fetch-Site: cross-site, Sec-Fetch-Mode: navigate, Sec-Fetch-User: ?1
    CrossSite,
    /// Same-site follow-up (pagination, AJAX within same domain)
    /// Sec-Fetch-Site: same-origin, Sec-Fetch-Mode: navigate
    SameOrigin,
}

/// Apply a full suite of browser-realistic headers to any request.
/// Includes Sec-Fetch-* headers that modern WAFs check for consistency.
pub fn apply_browser_headers(
    builder: reqwest::RequestBuilder,
    target_url: &str,
) -> reqwest::RequestBuilder {
    apply_browser_headers_ctx(builder, target_url, RequestContext::Navigation)
}

/// Apply headers with explicit Sec-Fetch context
pub fn apply_browser_headers_ctx(
    builder: reqwest::RequestBuilder,
    target_url: &str,
    ctx: RequestContext,
) -> reqwest::RequestBuilder {
    let headers = random_browser_headers();

    // Use the target site's own domain as referer (looks like a click-through)
    let dynamic_referer = url::Url::parse(target_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| format!("https://{}/", h)))
        .unwrap_or_else(|| headers.referer.to_string());

    // Context-aware Sec-Fetch headers (WAFs validate consistency)
    let (fetch_site, fetch_mode, fetch_user) = match ctx {
        RequestContext::Navigation => ("none", "navigate", true),
        RequestContext::CrossSite => ("cross-site", "navigate", true),
        RequestContext::SameOrigin => ("same-origin", "navigate", false),
    };

    let mut req = builder
        .header("User-Agent", headers.user_agent)
        .header("Accept-Language", headers.accept_language)
        .header("Referer", dynamic_referer)
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Site", fetch_site)
        .header("Sec-Fetch-Mode", fetch_mode)
        // ── Additional browser signals ──
        .header("DNT", "1")
        .header("Upgrade-Insecure-Requests", "1")
        .header("Cache-Control", "max-age=0")
        .header("Priority", "u=0, i");

    if fetch_user {
        req = req.header("Sec-Fetch-User", "?1");
    }

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
            "\"15.8.0\""
        } else if headers.sec_ch_ua_platform.contains("Chrome OS") {
            "\"14816.0.0\""
        } else {
            "\"6.12.0\""
        };
        req = req.header("Sec-CH-UA-Platform-Version", platform_ver);
    }

    // v5.0: Randomize Accept header variants (evade fingerprint-based blocking)
    let mut rng = rand::rng();
    let accept_variants = [
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    ];
    req = req.header("Accept", *accept_variants.choose(&mut rng).unwrap());

    // ── Accept-Encoding is deliberately NOT set here (v8.0 P0-4) ──
    //
    // v7.0 hand-wrote this header and advertised `zstd` in two of three
    // randomized variants, while the reqwest client had no zstd feature
    // enabled. Origins that honoured the offer (increasingly common behind
    // Cloudflare) returned bodies reqwest could not decompress; the raw zstd
    // bytes then went through from_utf8_lossy and produced garbage, so the
    // parser found zero results — silently, and on roughly two thirds of
    // requests to zstd-capable hosts.
    //
    // reqwest/wreq now emit Accept-Encoding themselves, derived from what they
    // can actually decode (gzip + brotli + deflate + zstd, all enabled in
    // Cargo.toml). A hand-written encoding list that disagrees with the
    // client's real capability is a correctness bug dressed as evasion.

    req
}

pub fn user_agents_count() -> usize {
    BROWSER_PROFILES.len()
}

// =============================================================================
// Engines — Full list of enabled search engines
// =============================================================================

/// Engines named in the `ENGINES` env var, if the operator set it.
///
/// v8.0 kept a hard-coded 90-name default here that nothing dispatched any
/// more, yet `/health`, `/config` and the startup banner all reported it
/// ("Engines: 90 total"). The real rosters live in `modes.rs`; this is only an
/// override for the general web pool (see `engines::all_engines`).
pub fn engines_override() -> Option<Vec<String>> {
    let raw = std::env::var("ENGINES").ok()?;
    let v: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if v.is_empty() { None } else { Some(v) }
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

// v8.0: `ref`, `source`, `src`, `campaign`, `partner`, `affiliate` and `amp`
// were REMOVED from this list. They are load-bearing query parameters on many
// sites (documentation portals use `?src=`, repository hosts use `?ref=`), and
// stripping them turned working URLs into 404s. Only unambiguous ad/analytics
// parameters remain.
pub const TRACKING_PARAMS: &[&str] = &[
    "utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content",
    "utm_id", "utm_cid", "utm_reader", "utm_name", "utm_social",
    "fbclid", "gclid", "gclsrc", "dclid", "msclkid", "twclid", "igshid",
    "ttclid", "li_fat_id", "wbraid", "gbraid",
    "_ga", "_gl", "_gid", "mc_cid", "mc_eid", "mkt_tok",
    "amp_js_v", "usqp", "spm", "share_from", "scm",
    "yclid", "_openstat", "vero_id", "pk_campaign", "pk_kwd", "piwik_campaign",
    "hsa_acc", "hsa_cam", "hsa_grp", "hsa_ad", "hsa_src", "hsa_tgt", "hsa_kw",
    "hsa_mt", "hsa_net", "hsa_ver", "_hsenc", "_hsmi",
];



// =============================================================================
// Multi-Query Intelligence Configuration
// =============================================================================


// =============================================================================
// Stealth HTTP Client — wreq with BoringSSL TLS impersonation
// =============================================================================
// Modern WAFs (Cloudflare, Akamai, DataDome) inspect:
//   1. TLS ClientHello (JA4 fingerprint — cipher order, extensions, ML-KEM)
//   2. HTTP/2 SETTINGS frame (window size, stream priority, pseudo-headers)
//   3. User-Agent + Client Hints consistency
// Standard reqwest gets blocked because its TLS fingerprint is non-browser.
// wreq clones real Chrome/Firefox/Safari TLS+HTTP/2 fingerprints via BoringSSL.
// =============================================================================

/// Engines that require TLS impersonation (behind Cloudflare/WAFs)
pub fn stealth_engines() -> Vec<String> {
    let default = "brave,startpage,yahoo,google,google_scholar,google_news,google_images,google_videos,google_us,google_uk,google_in,google_de,google_fr,google_es,google_it,google_br,google_jp,google_ca,google_au,google_nl,google_se,google_no,google_fi,bing,bing_news,bing_images,bing_videos,bing_us,bing_uk,bing_in,bing_de,bing_fr,bing_es,bing_it,bing_jp,bing_ca,bing_au,bing_nl,bing_se,bing_no,bing_fi,ecosia,yandex";
    let raw = std::env::var("STEALTH_ENGINES").unwrap_or_else(|_| default.to_string());
    raw.split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether stealth mode is enabled (default: auto)
pub fn stealth_mode() -> String {
    std::env::var("STEALTH_MODE").unwrap_or_else(|_| "auto".to_string()).to_lowercase()
}

/// Build a wreq stealth client with browser TLS + HTTP/2 impersonation.
///
/// v8.0: uses `Emulation::weighted_random()` instead of a pinned
/// `Emulation::Chrome147`.
///
/// `weighted_random()` installs a real browser profile and keeps the
/// User-Agent, client hints, JA4 TLS fingerprint and HTTP/2 SETTINGS mutually
/// consistent. v7.0 pinned a Chrome-147 TLS fingerprint while
/// `apply_browser_headers` overrode the UA with hand-written strings claiming
/// Chrome 155 / Firefox 157 / Safari 27 — versions that `wreq-util 0.2` cannot
/// fingerprint at all (its ceiling is Chrome 149). A UA/TLS mismatch is a
/// STRONGER bot signal than no spoofing, because it is exactly the
/// inconsistency WAFs test for. `Fetcher::Stealth` therefore no longer applies
/// our own UA headers.
pub fn build_stealth_client(proxy: Option<&str>) -> wreq::Client {
    use wreq_util::Emulation;

    let mut builder = wreq::Client::builder()
        .emulation(Emulation::weighted_random())
        .cookie_store(true)
        .connect_timeout(std::time::Duration::from_secs(8))
        .timeout(std::time::Duration::from_secs(16))
        .redirect(wreq::redirect::Policy::limited(10))
        .gzip(true)
        .brotli(true)
        .zstd(true)
        .deflate(true);

    if let Some(proxy_url) = proxy {
        match wreq::Proxy::all(proxy_url) {
            Ok(proxy_cfg) => {
                builder = builder.proxy(proxy_cfg);
            }
            Err(err) => {
                tracing::warn!("wreq: Invalid proxy {}: {}", proxy_url, err);
            }
        }
    }

    builder.build().expect("Failed to build wreq stealth client")
}

// NOTE: `apply_stealth_headers` was REMOVED in v8.0.
//
// It overrode the User-Agent and Sec-CH-UA headers that `wreq-util`'s
// Emulation had already set consistently with the TLS/HTTP2 fingerprint,
// recreating the exact UA-vs-TLS mismatch that WAFs look for. The stealth
// path in `crate::fetcher::Fetcher` now sends only Accept-Language and
// per-request extras, letting wreq own the browser identity.
