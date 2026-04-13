use crate::models::LlmConfig;
use crate::llm::{build_client, namespaced_model};
use genai::chat::{ChatMessage, ChatRequest};
use std::time::Duration;

pub async fn rewrite_query(query: &str, llm_config: &LlmConfig) -> String {
    let client = build_client(llm_config);
    let model = namespaced_model(&llm_config.provider, &llm_config.model);
    let timeout_ms = llm_config.timeout_ms.unwrap_or(5000).clamp(2000, 10000);

    let system_prompt = "You are Qrux Copilot. Rewrite the user's query into an optimized, highly dense search engine query string. Return ONLY the rewritten query string without any quotes, brackets, or explanations. Target exact keywords.";
    
    let chat_req = ChatRequest::new(vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(query.to_string()),
    ]);

    match tokio::time::timeout(Duration::from_millis(timeout_ms), client.exec_chat(&model, chat_req, None)).await {
        Ok(Ok(chat_res)) => {
            let answer = chat_res.first_text().unwrap_or("").trim().to_string();
            // Clean up any quotes the LLM might have added
            let clean_answer = answer.trim_matches('"').trim_matches('\'').to_string();
            
            if clean_answer.is_empty() {
                query.to_string()
            } else {
                clean_answer
            }
        }
        Ok(Err(e)) => {
            tracing::debug!("Qrux-Copilot LLM Error: {}", e);
            query.to_string()
        }
        Err(_) => {
            tracing::debug!("Qrux-Copilot LLM Timeout");
            query.to_string()
        }
    }
}
