// ============================================================================
// Qrux v5.0.1 - Search Engines Module
// Tiered engines: primary (fast) + backup (reliable) + domain-specialized
// Smart fallback: if primary engines fail, instantly switch to backups
// ============================================================================

pub mod brave;
pub mod duckduckgo;
pub mod generic;
pub mod mojeek;
pub mod qwant;
pub mod startpage;
pub mod wiby;
pub mod wikipedia;
pub mod yahoo;

use crate::models::RawSearchResult;
use reqwest::Client;

#[async_trait::async_trait]
pub trait SearchEngine: Send + Sync {
    fn name(&self) -> &str;
    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult>;
}

pub fn get_engines(enabled: &[String]) -> Vec<Box<dyn SearchEngine>> {
    let mut engines: Vec<Box<dyn SearchEngine>> = Vec::new();

    for name in enabled {
        match name.as_str() {
            "duckduckgo" | "duckduckgo_html" | "duckduckgo_news" | "duckduckgo_images" | "duckduckgo_videos" => {
                engines.push(Box::new(duckduckgo::DuckDuckGo))
            }
            "brave" | "brave_news" => engines.push(Box::new(brave::Brave)),
            "yahoo" | "yahoo_news" => engines.push(Box::new(yahoo::Yahoo)),
            "qwant" => engines.push(Box::new(qwant::Qwant)),
            "mojeek" => engines.push(Box::new(mojeek::Mojeek)),
            "startpage" => engines.push(Box::new(startpage::Startpage)),
            "wikipedia" => engines.push(Box::new(wikipedia::Wikipedia)),
            "wiby" => engines.push(Box::new(wiby::Wiby)),
            _ => {
                if let Some(spec) = generic::spec_for(name) {
                    engines.push(Box::new(generic::GenericEngine::new(name, spec)));
                } else {
                    tracing::warn!("Unknown engine: {}", name);
                }
            }
        }
    }

    engines
}

// =============================================================================
// Engine Tiers — Primary (fast, reliable) vs Backup (slower, but always works)
// =============================================================================

/// Primary fast engines — these respond in 1-3s and rarely fail
pub fn primary_engines() -> Vec<String> {
    vec![
        "wikipedia", "duckduckgo", "brave", "yahoo", "bing",
        "google", "qwant", "startpage", "mojeek", "ecosia", "wiby",
    ].into_iter().map(|s| s.to_string()).collect()
}

/// Backup engines — activated when primary engines return too few results
pub fn backup_engines() -> Vec<String> {
    vec![
        "bing_news", "google_news", "brave_news", "yahoo_news",
        "yandex", "ask", "dogpile", "excite", "webcrawler",
        "presearch", "yep", "mwmbl", "marginalia", "stract",
        "bing_us", "bing_uk", "google_us", "google_uk",
    ].into_iter().map(|s| s.to_string()).collect()
}

/// Domain-specialized engine sets for Specialized Mode
pub fn specialized_engines(domain: &str) -> Vec<String> {
    let base: Vec<&str> = match domain {
        "tech" => vec![
            "duckduckgo", "brave", "google", "bing", "startpage", "qwant",
            "google_news", "bing_news", "marginalia", "stract", "wiby",
        ],
        "science" => vec![
            "wikipedia", "google", "google_scholar", "brave", "duckduckgo",
            "startpage", "mojeek", "bing", "ecosia", "qwant",
        ],
        "finance" => vec![
            "google", "google_news", "bing", "bing_news", "yahoo", "yahoo_news",
            "brave", "brave_news", "duckduckgo", "startpage",
        ],
        "health" => vec![
            "google", "wikipedia", "bing", "duckduckgo", "brave",
            "google_scholar", "mojeek", "startpage", "ecosia", "qwant",
        ],
        "news" => vec![
            "google_news", "bing_news", "brave_news", "yahoo_news",
            "duckduckgo_news", "google", "bing", "brave", "yahoo", "duckduckgo",
        ],
        _ => vec![
            "wikipedia", "duckduckgo", "brave", "google", "bing",
            "yahoo", "qwant", "startpage", "mojeek", "ecosia",
        ],
    };
    base.into_iter().map(|s| s.to_string()).collect()
}

/// Generate query variations — now domain-aware for Specialized Mode
pub fn generate_query_variations(query: &str) -> Vec<String> {
    let base = query.trim();
    if base.is_empty() {
        return Vec::new();
    }

    vec![
        base.to_string(),
        format!("{} news", base),
        format!("{} forum", base),
    ]
}

/// Generate domain-aware query variations for Specialized Mode
pub fn generate_specialized_variations(query: &str, domain: &str) -> Vec<String> {
    let base = query.trim();
    if base.is_empty() {
        return Vec::new();
    }

    match domain {
        "tech" => vec![
            base.to_string(),
            format!("{} programming tutorial", base),
        ],
        "science" => vec![
            base.to_string(),
            format!("{} research study", base),
        ],
        "finance" => vec![
            base.to_string(),
            format!("{} market analysis", base),
        ],
        "health" => vec![
            base.to_string(),
            format!("{} medical research", base),
        ],
        "news" => vec![
            base.to_string(),
            format!("{} latest 2026", base),
        ],
        _ => vec![
            base.to_string(),
            format!("{} guide", base),
        ],
    }
}

/// Minimum result threshold — if primary engines return fewer than this,
/// we trigger backup engines immediately.
pub const FALLBACK_THRESHOLD: usize = 8;
