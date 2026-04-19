use crate::models::LlmConfig;
use crate::llm::{call_llm, ChatMessage};
use std::time::Duration;

pub async fn rewrite_query(query: &str, llm_config: &LlmConfig) -> String {
    let timeout_ms = llm_config.timeout_ms.unwrap_or(5000).clamp(2000, 10000);

    let system_prompt = "You are SearchWala Copilot. Rewrite the user's query into an optimized, highly dense search engine query string. Return ONLY the rewritten query string without any quotes, brackets, or explanations. Target exact keywords.";
    
    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(query.to_string()),
    ];

    match tokio::time::timeout(Duration::from_millis(timeout_ms), call_llm(llm_config, &messages)).await {
        Ok(Ok(answer)) => {
            let answer = answer.trim().to_string();
            // Clean up any quotes the LLM might have added
            let clean_answer = answer.trim_matches('"').trim_matches('\'').to_string();
            
            if clean_answer.is_empty() {
                query.to_string()
            } else {
                clean_answer
            }
        }
        Ok(Err(e)) => {
            tracing::debug!("SearchWala-Copilot LLM Error: {}", e);
            query.to_string()
        }
        Err(_) => {
            tracing::debug!("SearchWala-Copilot LLM Timeout");
            query.to_string()
        }
    }
}
