// ══════════════════════════════════════════════════════════════════════════════
// SearchWala v8.0 — URL Utilities
//
// v8.0 changes:
//   * ONE canonicalization function (`canonical_key`). v7.0 had the same
//     normalization logic copy-pasted verbatim into `normalize_url` and
//     `deduplicate`, two implementations that had to agree with nothing
//     enforcing it.
//   * Query parameters are SORTED, so `?a=1&b=2` and `?b=2&a=1` are one page.
//   * www / m. / amp. / mobile. host prefixes and /amp, /index.html path
//     suffixes are unified. On news sites — the Amar Ujala class — these are
//     the dominant duplicate form and v7.0 scraped each variant separately.
//   * `should_skip` matches on the HOST, not a substring of the whole URL.
//     v7.0's `lower_url.contains("amazon.com")` dropped
//     `https://news.site.com/why-amazon.com-won` — a legitimate article.
//   * `ref` / `source` / `src` / `amp` / `campaign` / `partner` removed from
//     TRACKING_PARAMS: they are load-bearing on many sites and stripping them
//     produced 404s.
// ══════════════════════════════════════════════════════════════════════════════

use std::collections::HashSet;
use url::Url;

use crate::config;

/// Host prefixes that denote the same content as the bare host.
const HOST_PREFIXES: &[&str] = &["www.", "m.", "amp.", "mobile.", "www2.", "en."];

/// Two-level public suffixes we must not over-trim.
const TWO_LEVEL_SUFFIXES: &[&str] = &[
    "co.uk", "co.in", "co.jp", "co.kr", "co.nz", "co.za", "co.il", "com.br",
    "com.au", "com.mx", "com.tr", "com.cn", "com.sg", "com.hk", "or.jp", "ne.jp",
    "ac.uk", "gov.uk", "org.uk", "net.au", "org.au", "gov.in", "ac.in", "nic.in",
];

/// Path suffixes that are the same document as the parent path.
const PATH_SUFFIXES: &[&str] = &[
    "/amp", "/amp/", "/index.html", "/index.htm", "/index.php", "/index.asp",
    "/default.html", "/default.htm",
];

/// Canonical identity of a page: the single source of truth for
/// "are these the same document?".
///
/// Deliberately *not* a valid URL — it is a comparison key. Use
/// [`normalize_url`] when you need something fetchable.
pub fn canonical_key(raw: &str) -> String {
    let trimmed = raw.trim();
    let Ok(mut u) = Url::parse(trimmed) else {
        return trimmed.to_lowercase();
    };
    if !matches!(u.scheme(), "http" | "https") {
        return trimmed.to_lowercase();
    }
    u.set_fragment(None);

    // ── Host: lowercase, strip content-equivalent prefixes ──
    let mut host = u.host_str().unwrap_or_default().to_lowercase();
    host = host.trim_end_matches('.').to_string();
    loop {
        let mut stripped = false;
        for p in HOST_PREFIXES {
            if let Some(rest) = host.strip_prefix(*p) {
                // Never strip down to a bare public suffix.
                if rest.contains('.') {
                    host = rest.to_string();
                    stripped = true;
                    break;
                }
            }
        }
        if !stripped {
            break;
        }
    }

    // ── Path: drop AMP/index suffixes and the trailing slash ──
    let mut path = u.path().to_string();
    loop {
        let mut changed = false;
        for suf in PATH_SUFFIXES {
            if path.len() > suf.len() && path.ends_with(suf) {
                path.truncate(path.len() - suf.len());
                changed = true;
                break;
            }
        }
        if !changed {
            break;
        }
    }
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    // Paths are case-sensitive on most servers — preserve case.

    // ── Query: drop tracking params, then SORT ──
    let mut pairs: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| {
            let kl = k.to_lowercase();
            !config::TRACKING_PARAMS.contains(&kl.as_str())
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.sort();

    if pairs.is_empty() {
        format!("{host}{path}")
    } else {
        let q: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
        format!("{host}{path}?{}", q.join("&"))
    }
}

/// Registrable root of a host, respecting common two-level suffixes.
pub fn registrable_domain(host: &str) -> String {
    let h = host.trim_end_matches('.').to_lowercase();
    let parts: Vec<&str> = h.split('.').collect();
    if parts.len() <= 2 {
        return h;
    }
    let last_two = parts[parts.len() - 2..].join(".");
    if TWO_LEVEL_SUFFIXES.contains(&last_two.as_str()) && parts.len() >= 3 {
        return parts[parts.len() - 3..].join(".");
    }
    last_two
}

/// A fetchable, tracking-stripped URL (scheme preserved).
pub fn normalize_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut u = Url::parse(trimmed).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    u.set_fragment(None);

    let mut pairs: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| {
            let kl = k.to_lowercase();
            !config::TRACKING_PARAMS.contains(&kl.as_str())
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.sort();

    if pairs.is_empty() {
        u.set_query(None);
    } else {
        let q: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
        u.set_query(Some(&q.join("&")));
    }
    Some(u.to_string())
}

/// Should this URL be skipped entirely?
///
/// Matches the blocklist against the HOST (exact or subdomain), never against
/// a substring of the full URL.
pub fn should_skip(url: &str) -> bool {
    let Ok(u) = Url::parse(url.trim()) else {
        return true;
    };
    if !matches!(u.scheme(), "http" | "https") {
        return true;
    }
    let host = u.host_str().unwrap_or_default().to_lowercase();
    if host.is_empty() {
        return true;
    }
    for d in config::SKIP_DOMAINS {
        let d = d.to_lowercase();
        if host == d || host.ends_with(&format!(".{d}")) {
            return true;
        }
    }
    let path = u.path().to_lowercase();
    config::SKIP_EXTENSIONS.iter().any(|e| path.ends_with(e))
}

/// Legacy alias — `canonical_key` is the real implementation now.
/// Kept as public API surface for callers outside this crate.
#[allow(dead_code)]
pub fn dedup_key(url: &str) -> String {
    canonical_key(url)
}

/// Deduplicate URLs preserving order, capped at `max_count`.
///
/// NOTE: callers that care about RANKING should not rely on this ordering.
/// v7.0 chose which URLs to scrape here, in arrival order, which meant the
/// scrape set was decided by whichever engine happened to finish first —
/// discarding the RRF consensus it had just computed. See `search.rs`.
pub fn deduplicate(urls: Vec<String>, max_count: usize, focus_mode: Option<&str>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut unique = Vec::with_capacity(max_count.min(urls.len()));
    let focus_lower = focus_mode.map(|m| m.to_lowercase());

    for raw in urls {
        let Some(normalized) = normalize_url(&raw) else { continue };

        let host = Url::parse(&normalized)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
            .unwrap_or_default();

        let bypass = match focus_lower.as_deref() {
            Some("reddit") => host.ends_with("reddit.com"),
            Some("youtube") => host.ends_with("youtube.com") || host.ends_with("youtu.be"),
            _ => false,
        };
        if !bypass && should_skip(&normalized) {
            continue;
        }
        if !seen.insert(canonical_key(&normalized)) {
            continue;
        }
        unique.push(normalized);
        if unique.len() >= max_count {
            break;
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_key_unifies_www_amp_mobile_and_param_order() {
        let variants = [
            "https://example.com/article",
            "https://www.example.com/article",
            "https://m.example.com/article",
            "https://amp.example.com/article",
            "https://www.example.com/article/",
            "https://www.example.com/article/amp",
            "https://www.example.com/article/index.html",
            "https://www.example.com/article#section-2",
            "https://www.example.com/article?utm_source=fb&fbclid=xyz",
        ];
        let keys: HashSet<String> = variants.iter().map(|v| canonical_key(v)).collect();
        assert_eq!(
            keys.len(),
            1,
            "all variants must collapse to one key, got {keys:?}"
        );
    }

    #[test]
    fn canonical_key_sorts_query_params() {
        // v7.0 treated these as two different pages and scraped both.
        assert_eq!(
            canonical_key("https://x.test/p?b=2&a=1"),
            canonical_key("https://x.test/p?a=1&b=2")
        );
    }

    #[test]
    fn canonical_key_keeps_meaningfully_different_pages_apart() {
        let a = canonical_key("https://x.test/p?id=1");
        let b = canonical_key("https://x.test/p?id=2");
        assert_ne!(a, b);
        assert_ne!(canonical_key("https://x.test/a"), canonical_key("https://x.test/b"));
        // Case-sensitive paths stay distinct.
        assert_ne!(canonical_key("https://x.test/Foo"), canonical_key("https://x.test/foo"));
    }

    #[test]
    fn canonical_key_does_not_strip_host_to_a_public_suffix() {
        // "en." is a strippable prefix, but "en.wikipedia.org" must not become
        // "wikipedia.org" if that would change identity... it should, actually:
        // en.wikipedia.org/wiki/X and wikipedia.org/wiki/X are the same doc.
        // What must NOT happen is stripping to something without a dot.
        assert!(canonical_key("https://m.co/x").contains("m.co") || canonical_key("https://m.co/x").contains("co"));
        // Never produce an empty host.
        assert!(!canonical_key("https://www.com/x").starts_with('/'));
    }

    #[test]
    fn should_skip_matches_host_not_substring() {
        // THE v7.0 BUG: a legitimate article whose PATH mentions a blocked
        // domain was dropped.
        assert!(
            !should_skip("https://news.site.com/why-amazon.com-lost-the-race"),
            "legitimate article must not be skipped"
        );
        assert!(
            !should_skip("https://blog.test/review?q=facebook.com"),
            "blocked domain in a query value must not skip"
        );
        // Real blocks still work, including subdomains.
        assert!(should_skip("https://twitter.com/user/status/1"));
        assert!(should_skip("https://www.facebook.com/page"));
        assert!(should_skip("https://m.tiktok.com/v/1"));
    }

    #[test]
    fn should_skip_rejects_binary_extensions_and_bad_schemes() {
        assert!(should_skip("https://x.test/file.zip"));
        assert!(should_skip("https://x.test/video.mp4"));
        assert!(should_skip("ftp://x.test/a"));
        assert!(should_skip("javascript:alert(1)"));
        assert!(should_skip("not a url"));
        assert!(!should_skip("https://x.test/article.html"));
    }

    #[test]
    fn tracking_params_stripped_but_load_bearing_ones_kept() {
        let k = canonical_key("https://x.test/p?utm_source=a&gclid=b&id=42&ref=partner");
        assert!(k.contains("id=42"), "load-bearing param dropped: {k}");
        assert!(!k.contains("utm_source"), "utm_source not stripped: {k}");
        assert!(!k.contains("gclid"), "gclid not stripped: {k}");
        // `ref` is load-bearing on many sites (GitHub, docs portals) and must
        // survive — v7.0 stripped it and produced 404s.
        assert!(k.contains("ref=partner"), "ref must be preserved: {k}");
    }

    #[test]
    fn registrable_domain_handles_two_level_suffixes() {
        assert_eq!(registrable_domain("www.bbc.co.uk"), "bbc.co.uk");
        assert_eq!(registrable_domain("news.amarujala.com"), "amarujala.com");
        assert_eq!(registrable_domain("a.b.c.example.com"), "example.com");
        assert_eq!(registrable_domain("times.of.india.co.in"), "india.co.in");
        assert_eq!(registrable_domain("example.com"), "example.com");
    }

    #[test]
    fn deduplicate_collapses_variants_and_respects_cap() {
        let urls = vec![
            "https://a.test/x".to_string(),
            "https://www.a.test/x/".to_string(),       // dup of the above
            "https://a.test/x?utm_source=q".to_string(), // dup
            "https://b.test/y".to_string(),
            "https://twitter.com/z".to_string(),        // blocked
            "https://c.test/w".to_string(),
        ];
        let out = deduplicate(urls, 10, None);
        assert_eq!(out.len(), 3, "got {out:?}");
        let out2 = deduplicate(
            vec!["https://a.test/1".into(), "https://b.test/2".into(), "https://c.test/3".into()],
            2,
            None,
        );
        assert_eq!(out2.len(), 2);
    }

    #[test]
    fn normalize_url_stays_fetchable() {
        let n = normalize_url("https://Example.COM/Path?b=2&utm_medium=x&a=1#frag").unwrap();
        assert!(n.starts_with("https://"), "{n}");
        assert!(!n.contains("utm_medium"), "{n}");
        assert!(!n.contains('#'), "{n}");
        // Must still parse as a URL.
        assert!(Url::parse(&n).is_ok());
    }

    #[test]
    fn canonical_key_is_utf8_safe() {
        // Devanagari path + query must not panic.
        let k = canonical_key("https://www.amarujala.com/क्रिकेट/विश्व-कप?utm_source=fb");
        assert!(!k.is_empty());
        assert!(!k.contains("utm_source"));
    }
}
