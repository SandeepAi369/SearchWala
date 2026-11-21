use crate::chats::PriorTurn;
use crate::llm::{call_llm, ChatMessage};
use crate::models::LlmConfig;
use crate::query_intel::{QueryIntent, SemanticCategory};
use chrono::Datelike;
use serde::Deserialize;
use std::collections::HashSet;
use std::time::Duration;

/// Structured output from the single-pass LLM Query Understanding step.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticQueryPlan {
    pub category: SemanticCategory,
    pub intent: Option<QueryIntent>,
    pub is_time_sensitive: bool,
    pub primary_query: String,
    pub sub_queries: Vec<String>,
    pub corrective_query: Option<String>,
    pub entities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawSemanticPlan {
    #[serde(default)]
    category: String,
    #[serde(default)]
    intent: Option<String>,
    #[serde(default)]
    is_time_sensitive: Option<bool>,
    #[serde(default)]
    primary_query: String,
    #[serde(default)]
    sub_queries: Vec<String>,
    #[serde(default)]
    corrective_query: Option<String>,
    #[serde(default)]
    entities: Vec<String>,
}

/// Dynamically replaces any past 4-digit year injected by an LLM (due to an
/// older training cutoff) that was NOT present in the user's raw query with
/// the live current year (`chrono::Utc::now().year()`).
pub fn neutralize_stale_years(raw_user_query: &str, candidate_query: &str) -> String {
    let current_year = chrono::Utc::now().year();
    let oldest_stale = current_year - 10;

    let raw_years: HashSet<i32> = raw_user_query
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|tok| tok.len() == 4 && tok.chars().all(|c| c.is_ascii_digit()))
        .filter_map(|tok| tok.parse::<i32>().ok())
        .collect();

    let mut out = String::with_capacity(candidate_query.len() + 4);
    let chars: Vec<char> = candidate_query.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let prev_is_alnum = start > 0 && chars[start - 1].is_alphanumeric();
            let next_is_alnum = i < chars.len() && chars[i].is_alphanumeric();
            let num_str: String = chars[start..i].iter().collect();
            if num_str.len() == 4 && !prev_is_alnum && !next_is_alnum {
                if let Ok(y) = num_str.parse::<i32>() {
                    if (oldest_stale..current_year).contains(&y) && !raw_years.contains(&y) {
                        out.push_str(&current_year.to_string());
                        continue;
                    }
                }
            }
            out.push_str(&num_str);
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Ensures distinctive subject entities present in `raw_user_query` are never
/// dropped when an LLM rewrites a follow-up or distills a conversational prompt.
pub fn enforce_entity_retention(
    raw_user_query: &str,
    candidate_query: &str,
    extracted_entities: &[String],
) -> String {
    let cand_trimmed = candidate_query.trim();
    if cand_trimmed.is_empty() {
        return raw_user_query.trim().to_string();
    }

    // If the raw query is non-ASCII and the candidate query is an ASCII
    // cross-lingual translation (or vice versa), do not force raw script tokens
    // onto the translated sub-query.
    let raw_has_non_ascii = raw_user_query
        .chars()
        .any(|c| !c.is_ascii() && c.is_alphabetic());
    let cand_has_non_ascii = cand_trimmed
        .chars()
        .any(|c| !c.is_ascii() && c.is_alphabetic());
    if raw_has_non_ascii != cand_has_non_ascii {
        return cand_trimmed.to_string();
    }

    let raw_lower = raw_user_query.to_lowercase();
    let cand_lower = cand_trimmed.to_lowercase();

    let mut candidate_entities: Vec<String> = extracted_entities.to_vec();
    for structural in crate::query_intel::analyze_query(raw_user_query).key_entities {
        if !candidate_entities
            .iter()
            .any(|e| e.eq_ignore_ascii_case(&structural))
        {
            candidate_entities.push(structural);
        }
    }

    let mut missing_entities: Vec<String> = Vec::new();
    for ent in &candidate_entities {
        let ent_trimmed = ent.trim();
        if ent_trimmed.chars().count() < 2 {
            continue;
        }
        let ent_tokens: Vec<String> = ent_trimmed
            .split_whitespace()
            .map(|t| {
                t.trim_matches(|c: char| !crate::util::is_word_char(c))
                    .to_lowercase()
            })
            .filter(|t| t.chars().count() >= 3)
            .collect();
        if ent_tokens.is_empty() {
            continue;
        }

        // Only lock entities that were actually mentioned in the user's raw prompt
        let present_in_raw = ent_tokens.iter().any(|t| raw_lower.contains(t.as_str()));
        if !present_in_raw {
            continue;
        }

        // Check whether at least one token (or 4-char phonetic prefix) survived in candidate_query
        let present_in_cand = ent_tokens.iter().any(|t| {
            if cand_lower.contains(t.as_str()) {
                return true;
            }
            let prefix_len = t.chars().count().min(4);
            if prefix_len >= 4 {
                let prefix: String = t.chars().take(prefix_len).collect();
                cand_lower
                    .split(|c: char| !crate::util::is_word_char(c))
                    .any(|ct| ct.starts_with(&prefix))
            } else {
                false
            }
        });

        if !present_in_cand {
            missing_entities.push(ent_trimmed.to_string());
        }
    }

    if missing_entities.is_empty() {
        cand_trimmed.to_string()
    } else {
        format!("{} {}", missing_entities.join(" "), cand_trimmed)
    }
}

fn parse_intent_label(label: &str) -> Option<QueryIntent> {
    let norm: String = label
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    match norm.as_str() {
        "factual" | "fact" => Some(QueryIntent::Factual),
        "temporal" | "news" | "livenews" | "recency" => Some(QueryIntent::Temporal),
        "person" | "biography" | "people" => Some(QueryIntent::Person),
        "comparison" | "compare" | "versus" => Some(QueryIntent::Comparison),
        "howto" | "tutorial" | "procedural" => Some(QueryIntent::HowTo),
        "navigational" | "navigation" | "site" => Some(QueryIntent::Navigational),
        "research" | "deepresearch" | "explanatory" => Some(QueryIntent::Research),
        _ => None,
    }
}

/// Extract and validate a `SemanticQueryPlan` from the LLM's raw response.
pub fn parse_semantic_plan(
    raw_output: &str,
    raw_user_query: &str,
    max_sub_queries: usize,
) -> Option<SemanticQueryPlan> {
    let start = raw_output.find('{')?;
    let end = raw_output.rfind('}')?;
    if end <= start {
        return None;
    }
    let json_slice = &raw_output[start..=end];
    let parsed: RawSemanticPlan = serde_json::from_str(json_slice).ok()?;

    let entities: Vec<String> = parsed
        .entities
        .into_iter()
        .map(|e| e.trim().trim_matches(['"', '\'', '`']).trim().to_string())
        .filter(|e| e.chars().count() >= 2 && e.chars().count() <= 80)
        .collect();

    let raw_primary = parsed
        .primary_query
        .trim()
        .trim_matches(['"', '\'', '`'])
        .trim();
    if raw_primary.is_empty() {
        return None;
    }

    let primary_year_clean = neutralize_stale_years(raw_user_query, raw_primary);
    let primary_query = enforce_entity_retention(raw_user_query, &primary_year_clean, &entities);

    let intent = parsed.intent.as_deref().and_then(parse_intent_label);
    let category = SemanticCategory::from_label(&parsed.category).unwrap_or_else(|| {
        let fallback_intent = intent.clone().unwrap_or(QueryIntent::Factual);
        SemanticCategory::default_for_intent(
            &fallback_intent,
            parsed.is_time_sensitive.unwrap_or(false),
        )
    });
    let is_time_sensitive =
        parsed.is_time_sensitive.unwrap_or(false) || category == SemanticCategory::LiveNews;

    let mut sub_queries = Vec::new();
    let mut seen = HashSet::new();
    seen.insert(primary_query.to_lowercase());

    for sq in parsed.sub_queries {
        let clean = sq.trim().trim_matches(['"', '\'', '`']).trim();
        if clean.chars().count() < 3 || clean.chars().count() > 140 {
            continue;
        }
        let year_clean = neutralize_stale_years(raw_user_query, clean);
        let retained = enforce_entity_retention(raw_user_query, &year_clean, &entities);
        if seen.insert(retained.to_lowercase()) {
            sub_queries.push(retained);
            if sub_queries.len() >= max_sub_queries.max(1) {
                break;
            }
        }
    }

    let corrective_query = parsed
        .corrective_query
        .map(|cq| {
            let clean = cq.trim().trim_matches(['"', '\'', '`']).trim();
            let year_clean = neutralize_stale_years(raw_user_query, clean);
            enforce_entity_retention(raw_user_query, &year_clean, &entities)
        })
        .filter(|cq| cq.chars().count() >= 3 && !cq.eq_ignore_ascii_case(&primary_query));

    Some(SemanticQueryPlan {
        category,
        intent,
        is_time_sensitive,
        primary_query,
        sub_queries,
        corrective_query,
        entities,
    })
}

/// Runs a single fast LLM Query Understanding pass that dynamically:
/// 1. Classifies semantic category (`Product/Org`, `Live News`, `History/Static`, `General Everyday`)
/// 2. Extracts canonical entities across any language or script
/// 3. Resolves follow-up conversation context while locking subject entities
/// 4. Corrects phonetic/voice typos and builds targeted search queries
/// 5. Generates a semantic corrective query for self-correcting retrieval
pub async fn understand_query(
    raw_query: &str,
    prior_turns: &[PriorTurn],
    llm_config: &LlmConfig,
    max_sub_queries: usize,
) -> Option<SemanticQueryPlan> {
    let q = raw_query.trim();
    if q.is_empty() {
        return None;
    }

    let now = chrono::Utc::now();
    let today_iso = now.format("%Y-%m-%d").to_string();
    let current_year = now.year();

    let mut cfg = llm_config.clone();
    cfg.max_output_tokens = Some(320);
    let timeout_ms = cfg.timeout_ms.unwrap_or(4500).clamp(2000, 6000);
    cfg.timeout_ms = Some(timeout_ms);

    let system_prompt = format!(
        "You are SearchWala's Semantic Query Understanding Engine. Today's live date is {today_iso} (current year {current_year}).\n\
         Analyze the user's question (and optional prior conversation turns) and output ONLY a single valid JSON object with these keys:\n\
         - \"category\": exactly one of \"Product/Org\", \"Live News\", \"History/Static\", or \"General Everyday\"\n\
         - \"intent\": one of \"factual\", \"temporal\", \"person\", \"comparison\", \"howto\", \"navigational\", or \"research\"\n\
         - \"is_time_sensitive\": boolean (true if the user asks for latest, recent, current, or time-dependent facts as of {current_year})\n\
         - \"entities\": array of canonical subject/brand/product/person/organization names mentioned in the question\n\
         - \"primary_query\": concise, high-signal search query string. Fix obvious voice-dictation or phonetic spelling errors in context, strip conversational filler, resolve pronouns from prior turns ONLY if the question refers to them, and NEVER drop any entity named in the user's question. When a year is needed for recency, always use {current_year}\n\
         - \"sub_queries\": array of 1 to {max_sub} distinct search queries covering complementary angles. If the question is in a non-Latin script and asks about global science, software, or international topics, include both a native-script query and an English technical query\n\
         - \"corrective_query\": a targeted search query aimed at official primary sources or documentation for the extracted entities if initial search results are insufficient",
        max_sub = max_sub_queries.clamp(1, 3)
    );

    let recent_context: Vec<String> = prior_turns
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(|t| {
            format!(
                "User: {}\nAssistant: {}",
                t.query.trim(),
                crate::util::truncate_chars(t.answer.trim(), 240)
            )
        })
        .collect();

    let user_prompt = if recent_context.is_empty() {
        format!("User question: {q}")
    } else {
        format!(
            "Prior conversation turns:\n{}\n\nCurrent user question: {q}",
            recent_context.join("\n\n")
        )
    };

    let messages = vec![
        ChatMessage::system(&system_prompt),
        ChatMessage::user(user_prompt),
    ];

    match tokio::time::timeout(Duration::from_millis(timeout_ms), call_llm(&cfg, &messages)).await {
        Ok(Ok(raw)) => parse_semantic_plan(&raw, q, max_sub_queries.clamp(1, 3)),
        Ok(Err(e)) => {
            tracing::debug!("Semantic Query Understanding LLM Error: {}", e);
            None
        }
        Err(_) => {
            tracing::debug!("Semantic Query Understanding LLM Timeout");
            None
        }
    }
}

pub async fn rewrite_query(query: &str, llm_config: &LlmConfig) -> String {
    if let Some(plan) = understand_query(query, &[], llm_config, 1).await {
        return plan.primary_query;
    }

    let timeout_ms = llm_config.timeout_ms.unwrap_or(5000).clamp(2000, 10000);
    let current_year = chrono::Utc::now().year();
    let system_prompt = format!(
        "You are SearchWala Copilot. Today's year is {current_year}. Rewrite the user's query into an optimized, highly dense search engine query string. Preserve every named entity and subject. Return ONLY the rewritten query string without quotes, brackets, or explanations."
    );

    let messages = vec![
        ChatMessage::system(&system_prompt),
        ChatMessage::user(query.to_string()),
    ];

    match tokio::time::timeout(Duration::from_millis(timeout_ms), call_llm(llm_config, &messages))
        .await
    {
        Ok(Ok(answer)) => {
            let clean_answer = answer
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_string();
            if clean_answer.is_empty() {
                query.to_string()
            } else {
                let year_clean = neutralize_stale_years(query, &clean_answer);
                enforce_entity_retention(query, &year_clean, &[])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_years_injected_by_llm_are_neutralized_to_current_year() {
        let current_year = chrono::Utc::now().year();
        let stale_one = current_year - 1;
        let stale_two = current_year - 2;

        let raw_q = "What is the latest update on Google Antigravity?";
        let llm_q = format!("Google Antigravity latest update release notes {stale_two} {stale_one}");
        let fixed = neutralize_stale_years(raw_q, &llm_q);

        assert!(
            !fixed.contains(&stale_two.to_string()) && !fixed.contains(&stale_one.to_string()),
            "Stale years were not neutralized: {fixed}"
        );
        assert!(
            fixed.contains(&current_year.to_string()),
            "Current year {current_year} missing from neutralized query: {fixed}"
        );
    }

    #[test]
    fn explicit_past_year_in_user_query_is_preserved() {
        let current_year = chrono::Utc::now().year();
        let user_requested_year = current_year - 2;

        let raw_q = format!("who won the {user_requested_year} championship");
        let llm_q = format!("{user_requested_year} championship winner results");
        let fixed = neutralize_stale_years(&raw_q, &llm_q);

        assert_eq!(fixed, llm_q);
    }

    #[test]
    fn entity_retention_restores_dropped_subject_entities() {
        let raw_q = "What is the dhruv rathi a Indian YouTube a latest video he released?";
        let broken_rewrite = "latest video released by Indian YouTuber";
        let entities = vec!["Dhruv Rathee".to_string()];

        let guarded = enforce_entity_retention(raw_q, broken_rewrite, &entities);
        assert!(
            guarded.to_lowercase().contains("dhruv"),
            "Dropped entity should be restored: {guarded}"
        );
    }

    #[test]
    fn parse_semantic_plan_handles_categories_and_neutralizes_stale_years() {
        let current_year = chrono::Utc::now().year();
        let stale_year = current_year - 2;
        let raw_q = "Wait, what is the latest update on Claude Cowork? Specifically I'm asking about features.";
        let llm_json = format!(
            r#"```json
            {{
                "category": "Product/Org",
                "intent": "temporal",
                "is_time_sensitive": true,
                "entities": ["Claude Cowork", "Anthropic"],
                "primary_query": "Claude Cowork Anthropic latest update features {stale_year}",
                "sub_queries": [
                    "Claude Cowork release notes changelog {stale_year}",
                    "Anthropic Claude Cowork announcements"
                ],
                "corrective_query": "Claude Cowork official documentation release notes {stale_year}"
            }}
            ```"#
        );

        let plan = parse_semantic_plan(&llm_json, raw_q, 3).expect("should parse valid JSON plan");
        assert_eq!(plan.category, SemanticCategory::ProductOrg);
        assert_eq!(plan.intent, Some(QueryIntent::Temporal));
        assert!(plan.is_time_sensitive);
        assert!(plan.primary_query.contains(&current_year.to_string()));
        assert!(!plan.primary_query.contains(&stale_year.to_string()));
        assert_eq!(plan.sub_queries.len(), 2);
        assert!(plan.sub_queries[0].contains(&current_year.to_string()));
        assert!(
            plan.corrective_query
                .as_deref()
                .unwrap_or("")
                .contains(&current_year.to_string())
        );
    }
}
