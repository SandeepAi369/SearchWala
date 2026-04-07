// ══════════════════════════════════════════════════════════════════════════════
// Swift Search Agent v3.0 — Search Engines Module
// Native meta-search — NO SearxNG dependency
// ══════════════════════════════════════════════════════════════════════════════

pub mod duckduckgo;
pub mod brave;
pub mod yahoo;
pub mod qwant;
pub mod mojeek;

use crate::models::RawSearchResult;
use reqwest::Client;

/// Trait that all search engines must implement
#[async_trait::async_trait]
pub trait SearchEngine: Send + Sync {
    /// Engine name (e.g., "duckduckgo")
    fn name(&self) -> &str;

    /// Perform search and return raw results
    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult>;
}

/// Get all enabled engine instances based on config
pub fn get_engines(enabled: &[String]) -> Vec<Box<dyn SearchEngine>> {
    let mut engines: Vec<Box<dyn SearchEngine>> = Vec::new();

    for name in enabled {
        match name.as_str() {
            "duckduckgo" => engines.push(Box::new(duckduckgo::DuckDuckGo)),
            "brave" => engines.push(Box::new(brave::Brave)),
            "yahoo" => engines.push(Box::new(yahoo::Yahoo)),
            "qwant" => engines.push(Box::new(qwant::Qwant)),
            "mojeek" => engines.push(Box::new(mojeek::Mojeek)),
            _ => tracing::warn!("Unknown engine: {}", name),
        }
    }

    engines
}
