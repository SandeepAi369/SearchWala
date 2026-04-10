use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config;

#[derive(Clone, Debug)]
pub struct ProxyPoolManager {
    proxies: Arc<Vec<String>>,
    cursor: Arc<AtomicUsize>,
    unhealthy_until: Arc<Mutex<HashMap<String, Instant>>>,
    cooldown: Duration,
}

impl ProxyPoolManager {
    pub fn from_env() -> Self {
        let csv = std::env::var("PROXY_POOL").unwrap_or_default();
        let file_path = std::env::var("PROXY_POOL_FILE").unwrap_or_default();
        let tor_ports = std::env::var("TOR_PROXY_PORTS").unwrap_or_default();

        let mut proxies = csv
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
            .collect::<Vec<_>>();

        if !file_path.trim().is_empty() {
            if let Ok(raw) = std::fs::read_to_string(&file_path) {
                for line in raw.lines() {
                    let p = line.trim();
                    if !p.is_empty() && !p.starts_with('#') {
                        proxies.push(p.to_string());
                    }
                }
            }
        }

        for port in tor_ports
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
        {
            if port.chars().all(|c| c.is_ascii_digit()) {
                proxies.push(format!("socks5h://127.0.0.1:{}", port));
            }
        }

        // Preserve order while removing duplicates.
        let mut dedup = Vec::with_capacity(proxies.len());
        let mut seen = std::collections::HashSet::new();
        for p in proxies {
            if seen.insert(p.clone()) {
                dedup.push(p);
            }
        }

        Self {
            proxies: Arc::new(dedup),
            cursor: Arc::new(AtomicUsize::new(0)),
            unhealthy_until: Arc::new(Mutex::new(HashMap::new())),
            cooldown: Duration::from_secs(config::proxy_cooldown_secs()),
        }
    }

    pub fn has_proxies(&self) -> bool {
        !self.proxies.is_empty()
    }

    pub fn len(&self) -> usize {
        self.proxies.len()
    }

    pub fn mark_proxy_failure(&self, proxy: &str) {
        if self.proxies.is_empty() || proxy.is_empty() {
            return;
        }
        if let Ok(mut lock) = self.unhealthy_until.lock() {
            lock.insert(proxy.to_string(), Instant::now() + self.cooldown);
        }
    }

    pub fn mark_proxy_success(&self, proxy: &str) {
        if proxy.is_empty() {
            return;
        }
        if let Ok(mut lock) = self.unhealthy_until.lock() {
            lock.remove(proxy);
        }
    }

    fn proxy_is_available(&self, proxy: &str) -> bool {
        if let Ok(mut lock) = self.unhealthy_until.lock() {
            if let Some(until) = lock.get(proxy).copied() {
                if Instant::now() >= until {
                    lock.remove(proxy);
                    return true;
                }
                return false;
            }
        }
        true
    }

    pub fn next_proxy(&self) -> Option<String> {
        if self.proxies.is_empty() {
            return None;
        }

        for _ in 0..self.proxies.len() {
            let idx = self.cursor.fetch_add(1, Ordering::Relaxed);
            let candidate = self.proxies[idx % self.proxies.len()].clone();
            if self.proxy_is_available(&candidate) {
                return Some(candidate);
            }
        }

        None
    }
}
