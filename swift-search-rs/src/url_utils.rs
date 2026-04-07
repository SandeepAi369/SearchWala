// ══════════════════════════════════════════════════════════════════════════════
// Swift Search Agent v3.0 — URL Utilities
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
            format!("{}{}", host.to_lowercase(), path.to_lowercase())
        }
        Err(_) => url.to_lowercase(),
    }
}

/// Deduplicate a list of URLs, preserving order, limited to max_count
pub fn deduplicate(urls: Vec<String>, max_count: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();

    for raw_url in urls {
        // Normalize first
        let url = match normalize_url(&raw_url) {
            Some(u) => u,
            None => continue,
        };

        // Skip blocked URLs
        if should_skip(&url) {
            continue;
        }

        // Deduplicate
        let key = dedup_key(&url);
        if seen.contains(&key) {
            continue;
        }
        seen.insert(key);
        unique.push(url);

        if unique.len() >= max_count {
            break;
        }
    }

    unique
}
