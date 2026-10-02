// ============================================================================
// SearchWala v8.0 — Result caches (P2-3)
//
// WHY THIS EXISTS
// v7.0 had no result cache of any kind. `cache.rs` held session and history
// stores; nothing memoised engine responses or extracted pages, so every
// repeated query re-dispatched every engine and re-scraped every URL. The
// measured scrape stage was 3.4-12.3 seconds.
//
// Two tiers, chosen for different volatility:
//
//   SERP  short TTL (10 min). News results genuinely change, and engines
//         punish repetition — this tier is also what makes it cheap to poll
//         /health/engines without hammering upstreams.
//   PAGE  long TTL (6 h). Article bodies are stable, and this is where the
//         real latency lives.
//
// `eval.rs` already defines engine_hit_ratio / scrape_hit_ratio /
// answer_hit_ratio for exactly this and nothing populated them.
// ============================================================================

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::Duration;

use crate::extractor::Extracted;
use crate::models::RawSearchResult;

fn serp_ttl() -> Duration {
    Duration::from_secs(
        std::env::var("SERP_CACHE_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600),
    )
}

fn page_ttl() -> Duration {
    Duration::from_secs(
        std::env::var("PAGE_CACHE_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(21_600),
    )
}

fn page_capacity() -> u64 {
    std::env::var("PAGE_CACHE_ENTRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_000)
}

fn cache_enabled() -> bool {
    !matches!(std::env::var("CACHE").as_deref(), Ok("off") | Ok("0") | Ok("false"))
}

/// (engine, normalized query, page index) -> that engine's results.
type SerpKey = (String, String, usize);

static SERP: LazyLock<moka::future::Cache<SerpKey, Vec<RawSearchResult>>> = LazyLock::new(|| {
    moka::future::Cache::builder()
        .max_capacity(20_000)
        .time_to_live(serp_ttl())
        .build()
});

/// canonical_key(url) -> extracted page.
static PAGE: LazyLock<moka::future::Cache<String, std::sync::Arc<Extracted>>> =
    LazyLock::new(|| {
        moka::future::Cache::builder()
            .max_capacity(page_capacity())
            // Weigh by extracted length so a few huge pages cannot evict
            // everything else.
            .weigher(|_k, v: &std::sync::Arc<Extracted>| {
                (v.text.len() / 1024).clamp(1, u32::MAX as usize) as u32
            })
            .time_to_live(page_ttl())
            .build()
    });

static SERP_HITS: AtomicU64 = AtomicU64::new(0);
static SERP_MISS: AtomicU64 = AtomicU64::new(0);
static PAGE_HITS: AtomicU64 = AtomicU64::new(0);
static PAGE_MISS: AtomicU64 = AtomicU64::new(0);

/// Normalize a query for cache keying: case- and whitespace-insensitive.
pub fn query_key(q: &str) -> String {
    q.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

// ── SERP tier ──

pub async fn get_serp(engine: &str, query: &str, page: usize) -> Option<Vec<RawSearchResult>> {
    if !cache_enabled() {
        return None;
    }
    let key = (engine.to_string(), query_key(query), page);
    match SERP.get(&key).await {
        Some(v) => {
            SERP_HITS.fetch_add(1, Ordering::Relaxed);
            Some(v)
        }
        None => {
            SERP_MISS.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

pub async fn put_serp(engine: &str, query: &str, page: usize, results: Vec<RawSearchResult>) {
    if !cache_enabled() || results.is_empty() {
        // Never cache an empty result: it is usually a block or selector rot,
        // and caching it would persist the failure for the whole TTL.
        return;
    }
    SERP.insert((engine.to_string(), query_key(query), page), results).await;
}

// ── PAGE tier ──

pub async fn get_page(url: &str) -> Option<std::sync::Arc<Extracted>> {
    if !cache_enabled() {
        return None;
    }
    match PAGE.get(&crate::url_utils::canonical_key(url)).await {
        Some(v) => {
            PAGE_HITS.fetch_add(1, Ordering::Relaxed);
            Some(v)
        }
        None => {
            PAGE_MISS.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

pub async fn put_page(url: &str, extracted: Extracted) {
    if !cache_enabled() || extracted.text.is_empty() {
        return;
    }
    PAGE.insert(
        crate::url_utils::canonical_key(url),
        std::sync::Arc::new(extracted),
    )
    .await;
}

// ── Stats ──

#[derive(serde::Serialize)]
pub struct CacheStats {
    pub enabled: bool,
    pub serp_entries: u64,
    pub serp_hits: u64,
    pub serp_misses: u64,
    pub serp_hit_ratio: f64,
    pub page_entries: u64,
    pub page_hits: u64,
    pub page_misses: u64,
    pub page_hit_ratio: f64,
    pub serp_ttl_secs: u64,
    pub page_ttl_secs: u64,
}

fn ratio(h: u64, m: u64) -> f64 {
    let t = h + m;
    if t == 0 {
        0.0
    } else {
        h as f64 / t as f64
    }
}

pub fn stats() -> CacheStats {
    let (sh, sm) = (SERP_HITS.load(Ordering::Relaxed), SERP_MISS.load(Ordering::Relaxed));
    let (ph, pm) = (PAGE_HITS.load(Ordering::Relaxed), PAGE_MISS.load(Ordering::Relaxed));
    CacheStats {
        enabled: cache_enabled(),
        serp_entries: SERP.entry_count(),
        serp_hits: sh,
        serp_misses: sm,
        serp_hit_ratio: ratio(sh, sm),
        page_entries: PAGE.entry_count(),
        page_hits: ph,
        page_misses: pm,
        page_hit_ratio: ratio(ph, pm),
        serp_ttl_secs: serp_ttl().as_secs(),
        page_ttl_secs: page_ttl().as_secs(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(url: &str) -> RawSearchResult {
        RawSearchResult {
            url: url.into(),
            title: "t".into(),
            snippet: String::new(),
            engine: "test".into(),
            rank_position: 1,
            published: None,
            full_text: None,
        }
    }

    #[test]
    fn query_key_is_case_and_whitespace_insensitive() {
        assert_eq!(query_key("  Cricket   World  CUP "), query_key("cricket world cup"));
        assert_eq!(query_key("A\tB\nC"), "a b c");
        // Distinct queries stay distinct.
        assert_ne!(query_key("cricket world cup"), query_key("cricket world cup 2027"));
    }

    #[test]
    fn query_key_is_utf8_safe() {
        let k = query_key("  भारत और  पाकिस्तान  ");
        assert_eq!(k, "भारत और पाकिस्तान");
    }

    #[tokio::test]
    async fn serp_round_trips() {
        let q = "serp-roundtrip-unique-query";
        assert!(get_serp("eng", q, 0).await.is_none(), "should start empty");
        put_serp("eng", q, 0, vec![hit("https://a.test/1")]).await;
        let got = get_serp("eng", q, 0).await.expect("should be cached");
        assert_eq!(got.len(), 1);
        // Case/whitespace variants hit the same entry.
        assert!(get_serp("eng", "  SERP-ROUNDTRIP-UNIQUE-QUERY ", 0).await.is_some());
        // A different page is a different key.
        assert!(get_serp("eng", q, 1).await.is_none());
        // A different engine is a different key.
        assert!(get_serp("other", q, 0).await.is_none());
    }

    #[tokio::test]
    async fn empty_serp_results_are_never_cached() {
        // Caching an empty result would persist a block or selector rot for
        // the whole TTL.
        let q = "empty-not-cached-unique-query";
        put_serp("eng2", q, 0, vec![]).await;
        assert!(get_serp("eng2", q, 0).await.is_none());
    }

    #[tokio::test]
    async fn page_cache_keys_on_canonical_url() {
        let e = Extracted {
            text: "some extracted body text".into(),
            title: "T".into(),
            confidence: 1.0,
            ..Default::default()
        };
        put_page("https://pagecache-test.example/article", e).await;
        // www/trailing-slash/tracking variants must hit the same entry.
        assert!(get_page("https://www.pagecache-test.example/article/").await.is_some());
        assert!(get_page("https://pagecache-test.example/article?utm_source=x").await.is_some());
        assert!(get_page("https://pagecache-test.example/different").await.is_none());
    }

    #[tokio::test]
    async fn empty_pages_are_never_cached() {
        put_page("https://empty-page-test.example/x", Extracted::default()).await;
        assert!(get_page("https://empty-page-test.example/x").await.is_none());
    }

    #[test]
    fn stats_report_ratios_without_dividing_by_zero() {
        let s = stats();
        assert!(s.serp_hit_ratio >= 0.0 && s.serp_hit_ratio <= 1.0);
        assert!(s.page_hit_ratio >= 0.0 && s.page_hit_ratio <= 1.0);
        assert!(s.serp_ttl_secs > 0 && s.page_ttl_secs > 0);
    }
}
