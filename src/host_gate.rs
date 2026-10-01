// ============================================================================
// SearchWala v8.0 — Per-host request spacing
//
// WHY THIS EXISTS
// Engine aliases share origins. `brave` and `brave_news` both hit
// search.brave.com; the 15 Bing aliases all hit www.bing.com. v7.0 fired them
// all simultaneously, which is the fastest possible way to earn a 429.
//
// Measured 2026-10-01 after the stealth fix: brave_news returned 123 results
// while the `brave` web engine got HTTP 429 on all 3 attempts — the two
// engines were rate-limiting each other. Brave's web results are the more
// valuable of the two, so this was a self-inflicted loss.
//
// This gate enforces a minimum interval between requests to the same
// registrable host, so concurrent aliases queue instead of colliding. It is a
// politeness mechanism as much as a reliability one.
// ============================================================================

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Minimum spacing between requests to the same host.
fn host_interval() -> Duration {
    Duration::from_millis(
        std::env::var("HOST_MIN_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(400),
    )
}

/// Next permitted request time per host.
static NEXT_ALLOWED: LazyLock<Arc<Mutex<HashMap<String, Instant>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

/// Registrable-ish root of a URL's host: "news.search.yahoo.com" ->
/// "yahoo.com", so vertical subdomains share one budget with their parent.
pub fn host_key(url: &str) -> String {
    let host = match url::Url::parse(url) {
        Ok(u) => u.host_str().unwrap_or_default().to_lowercase(),
        Err(_) => return String::new(),
    };
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() <= 2 {
        return host;
    }
    // Handle two-level public suffixes (.co.uk, .com.br, .co.in, ...).
    let last_two = parts[parts.len() - 2..].join(".");
    const TWO_LEVEL: &[&str] = &[
        "co.uk", "co.in", "co.jp", "com.br", "com.au", "co.nz", "co.za", "com.mx",
        "co.kr", "com.tr", "com.cn", "co.il",
    ];
    if TWO_LEVEL.contains(&last_two.as_str()) && parts.len() >= 3 {
        return parts[parts.len() - 3..].join(".");
    }
    last_two
}

/// Wait until this host may be contacted again, then reserve the next slot.
///
/// Returns the delay that was applied, for logging.
pub async fn acquire(url: &str) -> Duration {
    acquire_with(url, host_interval()).await
}

/// Testable core — the interval is a parameter so tests do not have to mutate
/// process-global env, which races under the parallel harness.
pub async fn acquire_with(url: &str, interval: Duration) -> Duration {
    let key = host_key(url);
    if key.is_empty() {
        return Duration::ZERO;
    }
    if interval.is_zero() {
        return Duration::ZERO;
    }

    // Reserve a slot under the lock, then sleep OUTSIDE it. Holding a
    // std::sync::Mutex across an await would be both a lint error and a
    // throughput disaster.
    let wait = {
        let Ok(mut map) = NEXT_ALLOWED.lock() else {
            return Duration::ZERO;
        };
        let now = Instant::now();
        let slot = map.entry(key.clone()).or_insert(now);
        let wait = slot.saturating_duration_since(now);
        // Reserve: the next caller waits one further interval.
        *slot = (*slot).max(now) + interval;
        wait
    };

    if !wait.is_zero() {
        tracing::debug!("host_gate: waiting {:?} for {}", wait, key);
        tokio::time::sleep(wait).await;
    }
    wait
}

/// Drop stale entries so the map cannot grow without bound.
pub fn prune() {
    if let Ok(mut map) = NEXT_ALLOWED.lock() {
        let now = Instant::now();
        map.retain(|_, t| *t > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_collapses_subdomains_to_one_budget() {
        // The whole point: these must share a rate budget.
        assert_eq!(host_key("https://search.brave.com/search?q=x"), "brave.com");
        assert_eq!(host_key("https://search.brave.com/news?q=x"), "brave.com");
        assert_eq!(host_key("https://www.bing.com/search?q=x"), "bing.com");
        assert_eq!(host_key("https://www.bing.com/news/search?q=x"), "bing.com");
        assert_eq!(host_key("https://search.yahoo.com/search?p=x"), "yahoo.com");
        assert_eq!(host_key("https://news.search.yahoo.com/search?p=x"), "yahoo.com");
    }

    #[test]
    fn host_key_handles_two_level_suffixes() {
        assert_eq!(host_key("https://www.google.co.uk/search?q=x"), "google.co.uk");
        assert_eq!(host_key("https://www.google.co.in/search?q=x"), "google.co.in");
        assert_eq!(host_key("https://www.google.com.br/search?q=x"), "google.com.br");
        // Distinct regional Googles must NOT share one budget with google.com.
        assert_ne!(host_key("https://www.google.co.uk/"), host_key("https://www.google.com/"));
    }

    #[test]
    fn host_key_is_empty_for_garbage() {
        assert_eq!(host_key("not a url"), "");
        assert_eq!(host_key(""), "");
    }

    #[tokio::test]
    async fn same_host_requests_are_spaced_apart() {
        let iv = Duration::from_millis(150);
        let t0 = Instant::now();
        acquire_with("https://spacing-a.test/1", iv).await;
        acquire_with("https://spacing-a.test/2", iv).await;
        acquire_with("https://spacing-a.test/3", iv).await;
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_millis(250),
            "3 same-host requests should take >=2 intervals, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn different_hosts_are_not_delayed() {
        let iv = Duration::from_millis(400);
        let t0 = Instant::now();
        acquire_with("https://par-a.test/x", iv).await;
        acquire_with("https://par-b.test/x", iv).await;
        acquire_with("https://par-c.test/x", iv).await;
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "distinct hosts must run in parallel, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn zero_interval_disables_the_gate() {
        let t0 = Instant::now();
        for _ in 0..5 {
            acquire_with("https://disabled.test/x", Duration::ZERO).await;
        }
        assert!(t0.elapsed() < Duration::from_millis(100), "took {:?}", t0.elapsed());
    }

    #[test]
    fn prune_drops_expired_entries() {
        {
            let mut map = NEXT_ALLOWED.lock().unwrap();
            map.insert("stale-prune.test".into(), Instant::now() - Duration::from_secs(60));
        }
        prune();
        assert!(!NEXT_ALLOWED.lock().unwrap().contains_key("stale-prune.test"));
    }
}
