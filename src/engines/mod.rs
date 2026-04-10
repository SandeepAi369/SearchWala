// ============================================================================
// Swift Search RS v4.0 - Search Engines Module
// ============================================================================

pub mod brave;
pub mod duckduckgo;
pub mod generic;
pub mod mojeek;
pub mod qwant;
pub mod startpage;
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
