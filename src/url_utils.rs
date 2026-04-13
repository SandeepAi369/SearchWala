// ══════════════════════════════════════════════════════════════════════════════
// Qrux v5.0.1 — URL Utilities
// Advanced URL normalization, deduplication, and filtering
// ══════════════════════════════════════════════════════════════════════════════

use std::collections::HashSet;
use url::Url;

use crate::config;

/// Normalize a URL: lowercase domain, remove tracking params, remove fragment
pub fn normalize_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let mut parsed = match Url::parse(raw) {
        Ok(u) => u,
        Err(_) => return None,
    };

    // Must be http(s)
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return None;
    }

    // Remove fragment
    parsed.set_fragment(None);

    // Remove tracking parameters
    let filtered_query: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(key, _)| {
            let k = key.to_lowercase();
            !config::TRACKING_PARAMS.iter().any(|&t| t == k.as_str())
        })
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    if filtered_query.is_empty() {
        parsed.set_query(None);
    } else {
        let qs: String = filtered_query
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");
        parsed.set_query(Some(&qs));
    }

    Some(parsed.to_string())
}

/// Check if a URL should be skipped (blocked domain or binary extension)
pub fn should_skip(url: &str) -> bool {
    let lower = url.to_lowercase();

    // Check domain blocklist
    for domain in config::SKIP_DOMAINS {
        if lower.contains(domain) {
            return true;
        }
    }

    // Check extension blocklist
    if let Ok(parsed) = Url::parse(url) {
        let path = parsed.path().to_lowercase();
        for ext in config::SKIP_EXTENSIONS {
            if path.ends_with(ext) {
                return true;
            }
        }
    }

    false
}

/// Generate a dedup key from a URL (domain + path, lowercased)
pub fn dedup_key(url: &str) -> String {
    match Url::parse(url) {
        Ok(parsed) => {
            let host = parsed.host_str().unwrap_or("");
            let path = parsed.path().trim_end_matches('/');
            let query = parsed.query().unwrap_or("");
            if query.is_empty() {
                format!("{}{}", host.to_lowercase(), path.to_lowercase())
            } else {
                format!("{}{}?{}", host.to_lowercase(), path.to_lowercase(), query)
            }
        }
        Err(_) => url.to_lowercase(),
    }
}

/// Deduplicate a list of URLs, preserving order, limited to max_count.
/// v5.0: Each URL is parsed ONCE via url::Url, then reused for normalize + skip + dedup_key.
pub fn deduplicate(urls: Vec<String>, max_count: usize, focus_mode: Option<&str>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut unique = Vec::with_capacity(max_count.min(urls.len()));

    let focus_lower = focus_mode.map(|m| m.to_lowercase());

    for raw_url in urls {
        // Parse URL ONCE (was parsed 3 times before: normalize + should_skip + dedup_key)
        let raw_trimmed = raw_url.trim();
        if raw_trimmed.is_empty() {
            continue;
        }

        let mut parsed = match Url::parse(raw_trimmed) {
            Ok(u) => u,
            Err(_) => continue,
        };

        // Must be http(s)
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            continue;
        }

        // Normalize in-place: remove fragment + tracking params
        parsed.set_fragment(None);
        let filtered_query: Vec<(String, String)> = parsed
            .query_pairs()
            .filter(|(key, _)| {
                let k = key.to_lowercase();
                !config::TRACKING_PARAMS.iter().any(|&t| t == k.as_str())
            })
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        if filtered_query.is_empty() {
            parsed.set_query(None);
        } else {
            let qs: String = filtered_query
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join("&");
            parsed.set_query(Some(&qs));
        }

        let url_str = parsed.to_string();

        // Focus mode bypass check (using already-parsed URL)
        let host = parsed.host_str().unwrap_or("").to_lowercase();
        let bypass_skip = match focus_lower.as_deref() {
            Some("reddit") => host.contains("reddit.com"),
            Some("youtube") => host.contains("youtube.com") || host.contains("youtu.be"),
            _ => false,
        };

        // Domain blocklist check (using already-parsed host)
        if !bypass_skip {
            let lower_url = url_str.to_lowercase();
            let blocked_domain = config::SKIP_DOMAINS.iter().any(|d| lower_url.contains(d));
            let blocked_ext = config::SKIP_EXTENSIONS.iter().any(|e| parsed.path().to_lowercase().ends_with(e));
            if blocked_domain || blocked_ext {
                continue;
            }
        }

        // Dedup key (using already-parsed components)
        let path = parsed.path().trim_end_matches('/');
        let query = parsed.query().unwrap_or("");
        let key = if query.is_empty() {
            format!("{}{}", host, path.to_lowercase())
        } else {
            format!("{}{}?{}", host, path.to_lowercase(), query)
        };

        if !seen.insert(key) {
            continue;
        }
        unique.push(url_str);

        if unique.len() >= max_count {
            break;
        }
    }

    unique
}
