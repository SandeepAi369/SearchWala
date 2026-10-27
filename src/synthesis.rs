// ============================================================================
// SearchWala v8.1 — Mode-specific answer synthesis
//
// Every mode gets the prompt that matches its job:
//
//   Fast       -> Concise      direct answer + up to 4 cited bullets
//   Auto       -> Standard     intent-aware prompt (lookups get Concise)
//   Deep       -> Report       MAP-REDUCE over all sources (see below)
//   Study      -> StudyNotes   structured notes, misconceptions, self-test
//   Academic   -> Literature   literature review, preprints labelled
//   Reddit     -> Community    consensus / disagreement / anecdotal caveat
//   YouTube    -> Video        what each video covers
//
// plus a domain addendum (tech / science / finance / health / news).
//
// WHY MAP-REDUCE FOR DEEP
// v8.0's iterative research rewrote the report batch by batch, but each
// continuation call was shown only the LAST 3,000 characters of the report so
// far — while being told "keep existing content" — and its output then
// REPLACED the report. With four batches, the final report was essentially
// batch 4 plus a 3,000-character tail; findings from earlier batches were
// lost. Map-reduce extracts cited notes from every batch (in parallel) and
// writes the report once, from all of them.
// ============================================================================

use std::time::Duration;

use crate::llm::{
    self, BuiltContext, ChatMessage, ContextBudget, LlmExecutionResult, DEFAULT_LLM_TIMEOUT_MS,
    LANGUAGE_RULE,
};
use crate::models::{LlmConfig, SourceResult};
use crate::modes::{Domain, ModeProfile, Synthesis};
use crate::query_intel::{QueryIntelligence, QueryIntent};

// ─────────────────────────────────────────────────────────────────────────────
// Domain addenda
// ─────────────────────────────────────────────────────────────────────────────

pub fn domain_rules(domain: Domain) -> &'static str {
    match domain {
        Domain::General => "",
        Domain::Tech => {
            " DOMAIN (technology): Prefer official documentation and source repositories. \
             Give exact version numbers, API names and short code snippets when the sources \
             contain them. Flag deprecated or version-specific behaviour."
        }
        Domain::Science => {
            " DOMAIN (science): Distinguish established consensus from preliminary or \
             single-study findings. Report figures with units. Name the study type when the \
             source gives it."
        }
        Domain::Finance => {
            " DOMAIN (finance): Give exact figures with their date and currency. Separate \
             reported facts from forecasts and opinion. End with: 'This is not financial advice.'"
        }
        Domain::Health => {
            " DOMAIN (health): Prefer health authorities (WHO, NIH, CDC, NHS) and peer-reviewed \
             evidence, and state how strong the evidence is. Do not diagnose. End with: \
             'This is general information, not medical advice — consult a qualified professional.'"
        }
        Domain::News => {
            " DOMAIN (news): Lead with what happened, when (exact dates) and who reported it. \
             Give a short dated timeline if events unfolded over time. Attribute claims to \
             outlets and flag unverified or conflicting reports."
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Prompt builders (pure functions — unit tested)
// ─────────────────────────────────────────────────────────────────────────────

fn base_rules() -> &'static str {
    " Use ONLY the numbered sources — never add outside facts. Cite every factual \
     statement with its source number like [3] or [3][7]. If the sources do not \
     answer something, say so plainly rather than guessing."
}

pub fn concise_prompts(q: &str, ctx: &str, domain: Domain) -> (String, String) {
    let system = format!(
        "You are a fast, precise search assistant. TODAY: {}.{} Be brief: no preamble, \
         no headings.{}{}",
        llm::current_datetime_str(),
        base_rules(),
        domain_rules(domain),
        LANGUAGE_RULE
    );
    let user = format!(
        "QUESTION: {q}\n\nSOURCES:\n{ctx}\n\n\
         Give the direct answer in 1-2 sentences, then at most 4 short bullets of key \
         supporting facts, each ending with its citation [n]. Maximum about 150 words."
    );
    (system, user)
}

pub fn standard_prompts(q: &str, ctx: &str, intel: &QueryIntelligence, domain: Domain) -> (String, String) {
    let (mut system, user) = llm::build_intent_prompts(q, ctx, intel);
    system.push_str(domain_rules(domain));
    (system, user)
}

pub fn study_prompts(q: &str, ctx: &str, domain: Domain) -> (String, String) {
    let system = format!(
        "You are an expert tutor writing study notes for a student. TODAY: {}.{} \
         Explain from first principles, define every technical term the first time it \
         appears, and keep formulas, units and dates exact. Where sources disagree, say so.{}{}",
        llm::current_datetime_str(),
        base_rules(),
        domain_rules(domain),
        LANGUAGE_RULE
    );
    let user = format!(
        "TOPIC / QUESTION: {q}\n\nSOURCES:\n{ctx}\n\n\
         Write study notes in Markdown with exactly these sections:\n\
         ## Overview\n2-3 plain-language sentences: what this is and why it matters.\n\
         ## Key Concepts\nBullets in the form **Term** — definition [n].\n\
         ## Detailed Explanation\n### sub-sections that build from fundamentals to advanced \
         ideas; every claim cited.\n\
         ## Worked Example\nA concrete example or calculation taken from the sources. If the \
         sources contain none, write one sentence saying so — do not invent one.\n\
         ## Common Misconceptions\nWhat learners often get wrong, and the correction [n].\n\
         ## Quick Revision\n5-8 one-line bullets worth memorising.\n\
         ## Self-Test\n5 questions of increasing difficulty, each followed by its answer in \
         parentheses."
    );
    (system, user)
}

pub fn literature_prompts(q: &str, ctx: &str, domain: Domain) -> (String, String) {
    let system = format!(
        "You are a research librarian writing a literature review. TODAY: {}.{} \
         Source headers give each paper's year, venue and citation count — use them. \
         Treat arXiv entries as preprints that are NOT peer-reviewed and label them as such. \
         Never invent papers, authors, years or numbers.{}{}",
        llm::current_datetime_str(),
        base_rules(),
        domain_rules(domain),
        LANGUAGE_RULE
    );
    let user = format!(
        "RESEARCH QUESTION: {q}\n\nSOURCES:\n{ctx}\n\n\
         Write a literature review in Markdown:\n\
         ## Scope\nOne paragraph restating the question and what the sources cover.\n\
         ## Key Findings\nGrouped by theme (### sub-headings). For each finding give the \
         citation [n], the year, and the study type or sample when stated.\n\
         ## Areas of Consensus\n## Disagreements and Open Questions\n\
         ## Methodological Notes and Limitations\n\
         ## Most Influential Works\nUp to 5 sources [n] with year and why each matters \
         (e.g. citation count, review status).\n\
         ## Summary"
    );
    (system, user)
}

pub fn community_prompts(q: &str, ctx: &str) -> (String, String) {
    let system = format!(
        "You summarise community discussions from Reddit. TODAY: {}.{} These are personal \
         opinions and experiences, not verified facts — present them that way.{}",
        llm::current_datetime_str(),
        base_rules(),
        LANGUAGE_RULE
    );
    let user = format!(
        "QUESTION: {q}\n\nDISCUSSIONS:\n{ctx}\n\n\
         Summarise in Markdown:\n## Community Consensus\n\
         ## Notable Opinions and Experiences\n## Disagreements\n## Practical Tips\n\
         ## Caveats\nNote that these are anecdotal, may be outdated, and how many threads \
         the summary rests on. Cite threads as [n]."
    );
    (system, user)
}

pub fn video_prompts(q: &str, ctx: &str) -> (String, String) {
    let system = format!(
        "You help a user choose YouTube videos. TODAY: {}.{} You only have each video's \
         title and description, not its transcript — never claim to know what is said \
         inside a video beyond that.{}",
        llm::current_datetime_str(),
        base_rules(),
        LANGUAGE_RULE
    );
    let user = format!(
        "QUESTION: {q}\n\nVIDEOS:\n{ctx}\n\n\
         In Markdown:\n## Most Relevant Videos\nFor each (best first): title and [n], what it \
         covers according to its description, and who it suits (beginner / advanced).\n\
         ## Suggested Watch Order\n## What These Videos Probably Won't Cover"
    );
    (system, user)
}

/// Evidence-extraction prompt for one Deep batch.
///
/// There is deliberately NO "if nothing is relevant, reply exactly: X"
/// escape hatch. Measured with a 7B model: with that line, every batch of a
/// "rust vs go for backend web services" search — sources plainly about Rust
/// vs Go — came back "NO RELEVANT FACTS", and Deep returned nothing. The
/// exact-phrase exit was the most salient instruction in the prompt. Without
/// it, the same batch produced precise cited bullets. An irrelevant batch now
/// simply yields no cited lines, which `clean_notes` discards.
pub fn map_prompts(q: &str, ctx: &str, first: usize, last: usize) -> (String, String) {
    let system = format!(
        "You are a meticulous research assistant extracting evidence. TODAY: {}. Use ONLY \
         the numbered sources provided.",
        llm::current_datetime_str()
    );
    let user = format!(
        "RESEARCH QUESTION: {q}\n\nSOURCES [{first}]-[{last}]:\n{ctx}\n\n\
         Extract every fact from these sources that helps answer the research question, as \
         terse bullet notes.\nRules:\n\
         - One fact per bullet. Keep exact numbers, dates, names and units.\n\
         - End EVERY bullet with its source number(s), e.g. [{first}] or [{first}][{last}].\n\
         - State disagreements between sources explicitly.\n\
         - Skip irrelevant sources. No introduction, no conclusion."
    );
    (system, user)
}

pub fn reduce_prompts(
    q: &str,
    notes: &str,
    n_sources: usize,
    intel: &QueryIntelligence,
    domain: Domain,
) -> (String, String) {
    let system = format!(
        "You are an expert research analyst writing a comprehensive report. TODAY: {}. You \
         are given evidence notes extracted from {} sources; every note carries its source \
         number. Use ONLY these notes — never add outside facts. Cite with exactly the [n] \
         numbers the notes use. For time-sensitive points prefer the most recent \
         information and flag outdated or conflicting claims.{}{}",
        llm::current_datetime_str(),
        n_sources,
        domain_rules(domain),
        LANGUAGE_RULE
    );
    let shape = match intel.intent {
        QueryIntent::Comparison => "\nInclude a Markdown comparison table of the main differences.",
        QueryIntent::HowTo => "\nInclude a numbered step-by-step section.",
        QueryIntent::Temporal => "\nInclude a dated timeline of key events.",
        QueryIntent::Person => "\nInclude a short dated career timeline.",
        _ => "",
    };
    let user = format!(
        "RESEARCH QUESTION: {q}\n\nEVIDENCE NOTES:\n{notes}\n\n\
         Write the report in Markdown:\n\
         ## Executive Summary\n3-5 sentences.\n\
         ## Key Findings\nOrganised by theme with ### sub-headings; every claim cited [n].\n\
         ## Points of Disagreement or Uncertainty\n\
         ## Practical Implications\n\
         ## Conclusion\n\
         Be thorough and specific — this is a deep research report, not a summary. Merge \
         duplicate facts and cite all of their sources.{shape}"
    );
    (system, user)
}

// ─────────────────────────────────────────────────────────────────────────────
// Quality gate
// ─────────────────────────────────────────────────────────────────────────────

/// Does the answer contain at least one `[n]` citation?
///
/// v8.0's gate only recognised the literal strings "[1]", "[2]" and "[3]", so
/// an answer citing [5] and [7] was judged "uncited" and triggered a wasted
/// retry call.
pub fn has_any_citation(answer: &str) -> bool {
    let b = answer.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'[' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && j < b.len() && b[j] == b']' {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Is this answer worth one retry?
pub fn insufficient(answer: &str, synthesis: Synthesis) -> bool {
    let n = crate::util::char_len(answer.trim());
    // A short answer is the GOAL of Concise ("Paris is the capital of France
    // [1]." is 35 characters), so only a near-empty reply earns a retry there.
    let min = match synthesis {
        Synthesis::Concise => 12,
        _ => 120,
    };
    n < min || (n > 200 && !has_any_citation(answer))
}

// ─────────────────────────────────────────────────────────────────────────────
// Single-call engine (every mode except Deep)
// ─────────────────────────────────────────────────────────────────────────────

fn timeout_for(cfg: &LlmConfig) -> Duration {
    Duration::from_millis(cfg.timeout_ms.unwrap_or(DEFAULT_LLM_TIMEOUT_MS).max(1_000))
}

async fn call_with_timeout(cfg: &LlmConfig, msgs: &[ChatMessage]) -> Result<String, String> {
    match tokio::time::timeout(timeout_for(cfg), llm::call_llm_retrying(cfg, msgs)).await {
        Ok(r) => r,
        Err(_) => Err(format!("llm_timeout: exceeded {:?}", timeout_for(cfg))),
    }
}

fn result(answer: Option<String>, error: Option<String>, batches: usize) -> LlmExecutionResult {
    LlmExecutionResult {
        llm_answer: answer,
        llm_error: error,
        batches_processed: batches,
        ..Default::default()
    }
}

async fn single_call(
    query: &str,
    cfg: &LlmConfig,
    sources: &[SourceResult],
    ctx: &BuiltContext,
    prompts: (String, String),
    synthesis: Synthesis,
) -> LlmExecutionResult {
    let msgs = vec![ChatMessage::system(prompts.0), ChatMessage::user(prompts.1)];

    let first = match call_with_timeout(cfg, &msgs).await {
        Ok(a) => llm::post_process_answer(&a, query),
        Err(e) => return result(None, Some(format!("llm_error: {e}")), 0),
    };
    if first.trim().is_empty() {
        return result(None, Some("llm_empty_response".into()), 1);
    }

    let mut answer = first;
    let mut calls = 1;
    if insufficient(&answer, synthesis) {
        tracing::info!("synthesis quality gate failed ({} chars), retrying once", answer.len());
        calls = 2;
        if let Ok(retry) = call_with_timeout(cfg, &msgs).await {
            let retry = llm::post_process_answer(&retry, query);
            let better = (has_any_citation(&retry) && !has_any_citation(&answer))
                || (has_any_citation(&retry) == has_any_citation(&answer) && retry.len() > answer.len());
            if better {
                answer = retry;
            }
        }
    }

    // Checks describe the answer actually RETURNED. v8.0 computed them for the
    // first answer and returned them alongside the retry's text.
    let checks = llm::verify_citations(&answer, sources, &ctx.citation_map);
    LlmExecutionResult {
        llm_answer: Some(answer),
        llm_error: None,
        batches_processed: calls,
        citation_map: ctx.citation_map.clone(),
        citation_checks: checks,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Deep: map-reduce
// ─────────────────────────────────────────────────────────────────────────────

/// Upper bound on map calls per Deep report.
const MAX_MAP_BATCHES: usize = 6;

/// Number sources by their position in `sources` (1-based), so every map batch
/// and the final report share one numbering and the citation map is the
/// identity over the included sources.
fn numbered_block(sources: &[SourceResult], range: std::ops::Range<usize>) -> String {
    let mut s = String::new();
    for i in range {
        let src = &sources[i];
        let date = src
            .published
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "undated".into());
        s.push_str(&format!(
            "[{}] {} {} — published {} ({})\n{}\n\n",
            i + 1,
            llm::credibility_tag(&src.url),
            src.title,
            date,
            src.url,
            src.extracted_text.trim()
        ));
    }
    s
}

/// Split sources into contiguous batches of at most `max_chars` of text.
pub fn plan_batches(sources: &[SourceResult], max_chars: usize) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (i, s) in sources.iter().enumerate() {
        let n = crate::util::char_len(&s.extracted_text) + 200;
        if i > start && used + n > max_chars {
            out.push(start..i);
            start = i;
            used = 0;
        }
        used += n;
    }
    if start < sources.len() {
        out.push(start..sources.len());
    }
    out
}

/// Keep only note lines that carry a citation in this batch's range — strips
/// any preamble the model adds despite instructions, and any citation it
/// hallucinated outside the batch.
pub fn clean_notes(raw: &str, range: &std::ops::Range<usize>) -> String {
    if raw.trim().eq_ignore_ascii_case("NO RELEVANT FACTS") {
        return String::new();
    }
    let mut kept = Vec::new();
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let nums = citation_numbers(t);
        if nums.is_empty() {
            continue;
        }
        if nums.iter().all(|n| *n > range.start && *n <= range.end) {
            let t = t.trim_start_matches(['-', '*', '•', ' ']);
            kept.push(format!("- {t}"));
        }
    }
    kept.join("\n")
}

fn citation_numbers(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'[' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && j < b.len() && b[j] == b']' {
                if let Ok(n) = s[i + 1..j].parse() {
                    out.push(n);
                }
                i = j;
            }
        }
        i += 1;
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn map_reduce(
    query: &str,
    cfg: &LlmConfig,
    sources: &[SourceResult],
    intel: &QueryIntelligence,
    profile: &ModeProfile,
    memory: &str,
    temp_db: Option<&crate::cache::TempDb>,
    session_id: Option<&str>,
) -> LlmExecutionResult {
    let budget = ContextBudget::for_config(cfg);
    // Map calls write terse notes; reserve a modest output for them.
    let map_out = budget.fit_output(1_500);
    let map_ctx_chars = llm::chars_for_tokens(budget.context_tokens(map_out), sources);
    let batch_chars = map_ctx_chars.min(profile.context_char_cap).clamp(3_000, 60_000);
    // The reduce call must hold all the notes plus the report it writes.
    let reduce_out = budget.fit_output(cfg.max_output_tokens.unwrap_or(profile.max_output_tokens));
    let max_ctx = llm::chars_for_tokens(budget.context_tokens(reduce_out), sources).min(profile.context_char_cap);
    let mut batches = plan_batches(sources, batch_chars);
    // Cap the number of map calls. On a small local window (4k tokens ≈ 7k
    // chars per batch) 120 chunks meant ~12 sequential calls — minutes of
    // waiting. Chunks arrive ranked by relevance, so dropping the TAIL loses
    // the least useful evidence. Hosted large-context models fit everything in
    // two or three batches and are unaffected.
    if batches.len() > MAX_MAP_BATCHES {
        let kept_until = batches[MAX_MAP_BATCHES - 1].end;
        tracing::info!(
            "Deep map-reduce: {} batches needed at {} chars each; keeping the top {} sources in {} batches",
            batches.len(), batch_chars, kept_until, MAX_MAP_BATCHES
        );
        batches.truncate(MAX_MAP_BATCHES);
    }
    let identity: Vec<usize> = (0..sources.len()).collect();

    // A single batch fits in one call: skip the map step entirely.
    if batches.len() <= 1 {
        let ctx = BuiltContext {
            text: numbered_block(sources, 0..sources.len()),
            citation_map: identity.clone(),
        };
        let notes_free = reduce_prompts(query, &ctx.text, sources.len(), intel, profile.domain);
        return single_call(query, cfg, sources, &ctx, with_memory(notes_free, memory), Synthesis::Report).await;
    }

    tracing::info!(
        "Deep map-reduce: {} sources in {} batches (≤{} chars each)",
        sources.len(),
        batches.len(),
        batch_chars
    );

    // ── MAP: bounded concurrency ──
    let mut map_cfg = cfg.clone();
    map_cfg.max_output_tokens = Some(map_out);
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(3));
    let mut set = tokio::task::JoinSet::new();
    for (bi, range) in batches.iter().cloned().enumerate() {
        let (sys, user) = map_prompts(query, &numbered_block(sources, range.clone()), range.start + 1, range.end);
        let cfg = map_cfg.clone();
        let sem = sem.clone();
        set.spawn(async move {
            let _p = sem.acquire().await.ok();
            let msgs = vec![ChatMessage::system(sys), ChatMessage::user(user)];
            (bi, range, call_with_timeout(&cfg, &msgs).await)
        });
    }

    let mut notes: Vec<(usize, String)> = Vec::new();
    let mut failed = 0;
    let mut done = 0;
    let total = batches.len();
    while let Some(joined) = set.join_next().await {
        done += 1;
        if let (Some(db), Some(sid)) = (temp_db, session_id) {
            db.update_status(sid, "deep_map", &format!("Reading sources: batch {done}/{total}")).await;
        }
        match joined {
            Ok((bi, range, Ok(raw))) => {
                let cleaned = clean_notes(&raw, &range);
                if cleaned.is_empty() {
                    tracing::info!(
                        "Deep map batch {} [{}-{}] yielded no usable notes; raw output begins: {:?}",
                        bi, range.start + 1, range.end, crate::util::truncate_chars(&raw, 400)
                    );
                }
                if !cleaned.is_empty() {
                    notes.push((bi, format!("Notes from sources [{}]-[{}]:\n{}", range.start + 1, range.end, cleaned)));
                }
            }
            Ok((_, _, Err(e))) => {
                failed += 1;
                tracing::warn!("Deep map batch failed: {}", e);
            }
            Err(e) => {
                failed += 1;
                tracing::warn!("Deep map task panicked: {}", e);
            }
        }
    }
    notes.sort_by_key(|(bi, _)| *bi);

    if notes.is_empty() {
        return result(
            None,
            Some(format!("llm_error: all {failed} evidence-extraction batches failed or found nothing")),
            total,
        );
    }

    let mut joined_notes = notes.into_iter().map(|(_, n)| n).collect::<Vec<_>>().join("\n\n");
    joined_notes = crate::util::truncate_chars(&joined_notes, max_ctx).to_string();

    // ── REDUCE ──
    if let (Some(db), Some(sid)) = (temp_db, session_id) {
        db.update_status(sid, "deep_reduce", "Writing the report").await;
    }
    // Map calls only extract evidence; the conversation matters when the
    // report is written.
    let (sys, user) = with_memory(reduce_prompts(query, &joined_notes, sources.len(), intel, profile.domain), memory);
    let msgs = vec![ChatMessage::system(sys), ChatMessage::user(user)];
    let mut reduce_cfg = cfg.clone();
    reduce_cfg.max_output_tokens = Some(reduce_out);
    let report = match call_with_timeout(&reduce_cfg, &msgs).await {
        Ok(r) => llm::post_process_answer(&r, query),
        Err(e) => {
            // The notes are cited evidence in their own right — return them
            // rather than nothing.
            tracing::warn!("Deep reduce failed ({}); returning the evidence notes", e);
            let checks = llm::verify_citations(&joined_notes, sources, &identity);
            return LlmExecutionResult {
                llm_answer: Some(format!("## Evidence Notes\n\n{joined_notes}")),
                llm_error: Some(format!("partial: report step failed ({e}); showing extracted notes")),
                batches_processed: total,
                citation_map: identity,
                citation_checks: checks,
            };
        }
    };

    let checks = llm::verify_citations(&report, sources, &identity);
    LlmExecutionResult {
        llm_answer: Some(report),
        llm_error: if failed > 0 {
            Some(format!("partial: {failed} of {total} evidence batches failed"))
        } else {
            None
        },
        batches_processed: total + 1,
        citation_map: identity,
        citation_checks: checks,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// LLM query planning (Deep)
// ─────────────────────────────────────────────────────────────────────────────

/// Parse a model's list of queries: one per line, numbering/bullets/quotes
/// stripped, de-duplicated, bounded in length.
pub fn parse_planned_queries(raw: &str, original: &str, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        let t = strip_list_marker(line.trim())
            .trim()
            .trim_matches(['"', '\'', '`'])
            .trim();
        let n = crate::util::char_len(t);
        if !(3..=120).contains(&n) || t.ends_with(':') {
            continue;
        }
        if t.eq_ignore_ascii_case(original.trim()) || out.iter().any(|o| o.eq_ignore_ascii_case(t)) {
            continue;
        }
        out.push(t.to_string());
        if out.len() >= max {
            break;
        }
    }
    out
}

/// Remove a leading list marker: "1." / "12)" / "-" / "*" / "•". Only digits
/// FOLLOWED by '.' or ')' count as numbering, so "5G network latency" keeps
/// its leading digit.
fn strip_list_marker(s: &str) -> &str {
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &s[digits..]; // ASCII digits: byte offset == char offset
        if let Some(r) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')')) {
            return r.trim_start();
        }
        return s;
    }
    s.trim_start_matches(['-', '*', '•']).trim_start()
}

/// Ask the LLM for sub-queries. Returns None on any failure so the caller can
/// fall back to rule-based facets.
pub async fn plan_subqueries(query: &str, cfg: &LlmConfig, n: usize) -> Option<Vec<String>> {
    if n == 0 {
        return None;
    }
    let mut c = cfg.clone();
    c.max_output_tokens = Some(300);
    c.timeout_ms = Some(c.timeout_ms.unwrap_or(10_000).min(12_000));
    let msgs = vec![
        ChatMessage::system("You plan web searches for a research assistant.".to_string()),
        ChatMessage::user(format!(
            "Question: {query}\n\nWrite {n} distinct web search queries that together cover this \
             question from different angles (background, latest developments, evidence and data, \
             criticisms, comparisons — whichever apply). Each query 3-10 words, in the same \
             language as the question. Output ONLY the queries, one per line, no numbering, \
             no quotes."
        )),
    ];
    match call_with_timeout(&c, &msgs).await {
        Ok(raw) => {
            let v = parse_planned_queries(&raw, query, n);
            if v.is_empty() { None } else { Some(v) }
        }
        Err(e) => {
            tracing::info!("LLM query planning unavailable ({}), using rule-based facets", e);
            None
        }
    }
}

/// Rewrites a follow-up ("and its battery?") into a query the engines can
/// answer on its own ("iPhone 17 battery life"), using the last few turns.
/// Engines never see the conversation itself. Returns None when the question
/// already stands alone, or on any failure (the question is then searched as
/// asked).
pub async fn standalone_query(query: &str, prior: &[crate::chats::PriorTurn], cfg: &LlmConfig) -> Option<String> {
    let recent: Vec<String> = prior
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(|t| format!("User: {}\nAssistant: {}", t.query.trim(), crate::util::truncate_chars(t.answer.trim(), 300)))
        .collect();
    if recent.is_empty() {
        return None;
    }
    let mut c = cfg.clone();
    c.max_output_tokens = Some(80);
    c.timeout_ms = Some(c.timeout_ms.unwrap_or(8_000).min(8_000));
    let msgs = vec![
        ChatMessage::system(
            "You turn a follow-up question into a standalone web search query. Replace pronouns \
             and references (it, they, that, the second one) with what they refer to in the \
             conversation. Keep the user's language. If the question already makes sense on its \
             own, return it unchanged. Output ONLY the query, on one line."
                .to_string(),
        ),
        ChatMessage::user(format!("Conversation:\n{}\n\nFollow-up question: {query}", recent.join("\n\n"))),
    ];
    let raw = match call_with_timeout(&c, &msgs).await {
        Ok(r) => r,
        Err(e) => {
            tracing::info!("follow-up rewrite unavailable ({e}); searching the question as asked");
            return None;
        }
    };
    let q = raw.lines().map(str::trim).find(|l| !l.is_empty())?.trim_matches(['"', '\'', '`']).trim().to_string();
    let plausible = !q.is_empty() && q.chars().count() <= 200 && !q.to_lowercase().starts_with("here");
    (plausible && !q.eq_ignore_ascii_case(query.trim())).then_some(q)
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Adds the conversation so far to a prompt pair. It is there to resolve
/// "it", "that one", "compare them": the rules forbid treating it as
/// evidence, so an earlier wrong answer cannot be repeated as fact.
pub fn with_memory((system, user): (String, String), memory: &str) -> (String, String) {
    if memory.trim().is_empty() {
        return (system, user);
    }
    (
        format!(
            "{system}\n\nCONVERSATION SO FAR: the user is continuing a conversation. Use it only to \
             understand what the current question refers to and to keep the answer consistent \
             with what the user already knows. It is NOT a source: every factual claim must come \
             from the numbered sources and be cited from them. Where an earlier answer conflicts \
             with the sources, follow the sources and say the earlier answer was wrong. Never \
             cite the conversation."
        ),
        format!("CONVERSATION SO FAR (context only, not a source):\n{memory}\n\n---\n\n{user}"),
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn synthesize(
    query: &str,
    cfg: LlmConfig,
    sources: &[SourceResult],
    intel: &QueryIntelligence,
    profile: &ModeProfile,
    memory: &str,
    temp_db: Option<&crate::cache::TempDb>,
    session_id: Option<&str>,
) -> LlmExecutionResult {
    let window = ContextBudget::for_config(&cfg).max_tokens;
    let (mut out, truncated) = llm::TRUNCATED
        .scope(std::cell::Cell::new(false), async {
            let r = synthesize_inner(query, cfg, sources, intel, profile, memory, temp_db, session_id).await;
            (r, llm::TRUNCATED.with(|t| t.get()))
        })
        .await;
    if truncated && out.llm_answer.is_some() {
        // Measured: Hindi study notes on a 4k-token local model stopped after
        // "Worked Example" with no indication. Say so, and say how to fix it.
        let note = format!(
            "partial: the answer reached the model's output limit and may be cut off \
             (window ≈{window} tokens). For local models raise the server's context \
             (e.g. OLLAMA_CONTEXT_LENGTH) and set CONTEXT_MAX_TOKENS to match."
        );
        out.llm_error = Some(match out.llm_error.take() {
            Some(e) => format!("{e}; {note}"),
            None => note,
        });
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn synthesize_inner(
    query: &str,
    cfg: LlmConfig,
    sources: &[SourceResult],
    intel: &QueryIntelligence,
    profile: &ModeProfile,
    memory: &str,
    temp_db: Option<&crate::cache::TempDb>,
    session_id: Option<&str>,
) -> LlmExecutionResult {
    if sources.is_empty() {
        return result(None, Some("llm_skipped: no scraped content available".into()), 0);
    }
    let budget = ContextBudget::for_config(&cfg);
    let mut cfg = cfg;
    // The mode's output length, capped so prompt + output fit one window.
    let out = budget.fit_output(cfg.max_output_tokens.unwrap_or(profile.max_output_tokens));
    cfg.max_output_tokens = Some(out);

    if profile.synthesis == Synthesis::Report {
        return map_reduce(query, &cfg, sources, intel, profile, memory, temp_db, session_id).await;
    }

    // Memory shares the window with the sources.
    let max_ctx = llm::chars_for_tokens(budget.context_tokens(out), sources)
        .min(profile.context_char_cap)
        .saturating_sub(crate::util::char_len(memory));
    let ctx = llm::build_ranked_context(sources, max_ctx);
    if ctx.is_empty() {
        return result(None, Some("llm_skipped: no usable source text".into()), 0);
    }
    tracing::info!(
        "Synthesis {:?}: {} sources, {} cited, {} context chars (cap {})",
        profile.synthesis,
        sources.len(),
        ctx.citation_map.len(),
        crate::util::char_len(&ctx.text),
        max_ctx
    );

    let prompts = match profile.synthesis {
        Synthesis::Concise => concise_prompts(query, &ctx.text, profile.domain),
        Synthesis::Standard => standard_prompts(query, &ctx.text, intel, profile.domain),
        Synthesis::StudyNotes => study_prompts(query, &ctx.text, profile.domain),
        Synthesis::Literature => literature_prompts(query, &ctx.text, profile.domain),
        Synthesis::Community => community_prompts(query, &ctx.text),
        Synthesis::Video => video_prompts(query, &ctx.text),
        Synthesis::Report => unreachable!("handled above"),
    };
    single_call(query, &cfg, sources, &ctx, with_memory(prompts, memory), profile.synthesis).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_intel::analyze_query;

    fn src(i: usize, chars: usize) -> SourceResult {
        SourceResult {
            url: format!("https://s{i}.test/a"),
            title: format!("Source {i}"),
            extracted_text: "x".repeat(chars),
            char_count: chars,
            engine: "test".into(),
            confidence: 1.0,
            ..Default::default()
        }
    }

    #[test]
    fn citation_detection_accepts_any_number() {
        // v8.0 only recognised "[1]", "[2]", "[3]".
        assert!(has_any_citation("as shown [5] and [17]"));
        assert!(has_any_citation("x [1]"));
        assert!(!has_any_citation("no citations [here] or [] or [a1]"));
        assert!(!has_any_citation("trailing [12"));
    }

    #[test]
    fn quality_gate_is_mode_aware() {
        assert!(!insufficient("Paris is the capital of France [1].", Synthesis::Concise));
        assert!(insufficient("Paris.", Synthesis::Concise));
        let long_uncited = "word ".repeat(80);
        assert!(insufficient(&long_uncited, Synthesis::StudyNotes));
        let long_cited = format!("{} [9]", "word ".repeat(80));
        assert!(!insufficient(&long_cited, Synthesis::StudyNotes));
    }

    #[test]
    fn every_prompt_carries_the_shared_rules() {
        let intel = analyze_query("how does photosynthesis work");
        let all = [
            concise_prompts("q", "CTX", Domain::General),
            standard_prompts("q", "CTX", &intel, Domain::General),
            study_prompts("q", "CTX", Domain::General),
            literature_prompts("q", "CTX", Domain::General),
            community_prompts("q", "CTX"),
            video_prompts("q", "CTX"),
        ];
        for (sys, user) in &all {
            assert!(sys.contains("LANGUAGE"), "missing language rule: {sys}");
            assert!(user.contains("CTX"), "context not included");
        }
    }

    #[test]
    fn study_prompt_has_the_learning_structure() {
        let (_, user) = study_prompts("photosynthesis", "CTX", Domain::General);
        for section in ["## Overview", "## Key Concepts", "## Detailed Explanation",
                        "## Worked Example", "## Common Misconceptions", "## Quick Revision",
                        "## Self-Test"] {
            assert!(user.contains(section), "study notes missing {section}");
        }
        // Must not invent examples.
        assert!(user.contains("do not invent"));
    }

    #[test]
    fn literature_prompt_labels_preprints() {
        let (sys, user) = literature_prompts("q", "CTX", Domain::General);
        assert!(sys.contains("preprints") && sys.contains("NOT peer-reviewed"));
        assert!(user.contains("## Areas of Consensus") && user.contains("## Most Influential Works"));
    }

    #[test]
    fn domain_rules_are_applied() {
        let (sys, _) = concise_prompts("q", "CTX", Domain::Health);
        assert!(sys.contains("not medical advice"));
        let (sys, _) = study_prompts("q", "CTX", Domain::Finance);
        assert!(sys.contains("not financial advice"));
        assert_eq!(domain_rules(Domain::General), "");
    }

    #[test]
    fn map_prompt_has_no_exact_phrase_escape_hatch() {
        let (_, user) = map_prompts("q", "CTX", 1, 10);
        assert!(!user.contains("reply exactly"), "escape hatch makes small models skip every batch");
        assert!(user.contains("[1]"), "must show the citation format with real numbers");
    }

    #[test]
    fn reduce_prompt_shapes_by_intent() {
        let cmp = analyze_query("rust vs go for backend services");
        let (_, user) = reduce_prompts("q", "NOTES", 10, &cmp, Domain::General);
        assert!(user.contains("comparison table"));
        let how = analyze_query("how to deploy a docker container to aws");
        let (_, user) = reduce_prompts("q", "NOTES", 10, &how, Domain::General);
        assert!(user.contains("step-by-step"));
    }

    #[test]
    fn batches_cover_every_source_exactly_once() {
        let sources: Vec<SourceResult> = (0..25).map(|i| src(i, 3_000)).collect();
        let b = plan_batches(&sources, 10_000);
        assert!(b.len() >= 7, "expected several batches, got {b:?}");
        let mut covered = vec![false; sources.len()];
        for r in &b {
            for i in r.clone() {
                assert!(!covered[i], "source {i} in two batches");
                covered[i] = true;
            }
        }
        assert!(covered.iter().all(|c| *c), "a source was dropped");
    }

    #[test]
    fn an_oversized_source_still_gets_its_own_batch() {
        let sources = vec![src(0, 100), src(1, 50_000), src(2, 100)];
        let b = plan_batches(&sources, 10_000);
        assert_eq!(b.iter().map(|r| r.len()).sum::<usize>(), 3);
        assert!(b.iter().all(|r| !r.is_empty()));
    }

    #[test]
    fn batches_handle_empty_input() {
        assert!(plan_batches(&[], 10_000).is_empty());
    }

    #[test]
    fn numbering_is_global_across_batches() {
        let sources: Vec<SourceResult> = (0..6).map(|i| src(i, 100)).collect();
        let block = numbered_block(&sources, 3..6);
        assert!(block.starts_with("[4] "), "batch 2 must continue numbering: {block}");
        assert!(block.contains("[6] ") && !block.contains("[1] "));
    }

    #[test]
    fn notes_cleaning_keeps_only_in_range_cited_bullets() {
        let raw = "Here are the notes:\n\
                   - mRNA is translated by ribosomes [4]\n\
                   * LNPs protect the mRNA [5][6]\n\
                   - hallucinated cross-batch claim [1]\n\
                   - no citation here\n";
        let cleaned = clean_notes(raw, &(3..6));
        assert!(cleaned.contains("ribosomes [4]"));
        assert!(cleaned.contains("LNPs protect the mRNA [5][6]"));
        assert!(!cleaned.contains("hallucinated"), "out-of-range citation kept");
        assert!(!cleaned.contains("Here are"), "preamble kept");
        assert!(!cleaned.contains("no citation"));
        assert_eq!(clean_notes("NO RELEVANT FACTS", &(0..3)), "");
    }

    #[test]
    fn planned_queries_are_parsed_defensively() {
        let raw = "Here are some queries:\n1. mRNA vaccine mechanism\n- \"lipid nanoparticle delivery\"\n\
                   * mRNA vaccine side effects data\n2) how does mRNA vaccine technology work\n\
                   mRNA VACCINE MECHANISM\nx\n";
        let v = parse_planned_queries(raw, "how does mRNA vaccine technology work", 5);
        assert_eq!(
            v,
            vec![
                "mRNA vaccine mechanism".to_string(),
                "lipid nanoparticle delivery".to_string(),
                "mRNA vaccine side effects data".to_string(),
            ],
            "preamble, duplicates, the original and too-short lines must be dropped"
        );
        assert_eq!(parse_planned_queries(raw, "q", 1).len(), 1);
        // Leading digits that are part of the query are not numbering.
        let g = parse_planned_queries("1. 5G network latency\n2) 3D printing costs\n", "q", 5);
        assert_eq!(g, vec!["5G network latency".to_string(), "3D printing costs".to_string()]);
        // Non-ASCII queries survive intact.
        let hi = parse_planned_queries("भारत पाकिस्तान मैच तारीख\n", "q", 3);
        assert_eq!(hi, vec!["भारत पाकिस्तान मैच तारीख".to_string()]);
    }
}
