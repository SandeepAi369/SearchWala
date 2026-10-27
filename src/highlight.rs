// ============================================================================
// Answer highlight — the best sentence(s) for the query, without an LLM.
//
// Fast mode exists to answer quickly, but with no model configured the UI
// showed "No AI answer returned" and a list of links: the answer was in the
// passages (for "boiling point of water at sea level", "At sea level water
// boils at 100 °C (212 °F)") and nothing surfaced it. With a model it still
// helps: the highlight appears as soon as retrieval finishes, before the
// answer streams, and it is verbatim from one source, so it cannot be a
// hallucination.
//
// Scoring is deliberately simple and deterministic:
//   coverage of the query's content terms (dominant),
//   intent cues (a number for factual/temporal questions, definitional
//   phrasing for "what is" questions),
//   the passage's rank (passages are already ranked by BM25 + consensus),
//   penalties for fragments, wiki reference markers and navigation text.
// ============================================================================

use crate::models::SourceResult;
use crate::query_intel::{QueryIntelligence, QueryIntent};
use serde::Serialize;

/// How many top-ranked passages to read.
const PASSAGES: usize = 8;
/// Sentences outside this character range are fragments or walls of text.
const MIN_SENTENCE_CHARS: usize = 25;
const MAX_SENTENCE_CHARS: usize = 360;
/// A sentence must contain at least this share of the query's terms.
const MIN_COVERAGE: f64 = 0.5;
/// Below this the best sentence is not worth showing (filler, fragments).
const MIN_SCORE: f64 = 2.0;

/// Hindi function words: not content terms.
const HI_STOPWORDS: &[&str] = &[
    "की", "का", "के", "है", "हैं", "क्या", "में", "और", "को", "से", "पर", "एक", "यह", "वह", "था", "थे", "हो", "कौन", "कैसे", "कब", "कहाँ", "क्यों", "ने",
    "किसने", "कहां", "कितना", "कितने", "कितनी",
];

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Highlight {
    /// One or two verbatim sentences.
    pub text: String,
    /// The query terms found in `text` (for the UI to mark).
    pub terms: Vec<String>,
    pub url: String,
    pub title: String,
    /// 0-based index into `SearchResponse::results`.
    pub result_index: usize,
}

/// Light suffix stripping so "boiling", "boils" and "boiled" all match
/// "boil", and "vaccines" matches "vaccine". (A fixed "drop two characters"
/// rule turned "boiling" into "boili", which never matched "boils".)
pub fn stem(term: &str) -> String {
    for suffix in ["ing", "edly", "ed", "es", "s", "ly"] {
        if let Some(base) = term.strip_suffix(suffix) {
            if base.chars().count() >= 3 && base.is_ascii() {
                return base.to_string();
            }
        }
    }
    term.to_string()
}

/// Split passage text into sentences. Handles Latin and Devanagari stops and
/// the "…" separators the chunker puts between non-adjacent windows.
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '\n' || c == '…' {
            push_sentence(&mut out, &mut cur);
            continue;
        }
        cur.push(c);
        let next = chars.get(i + 1).copied();
        let ends = match c {
            '।' | '?' | '!' => true,
            // "3.5" and "e.g." are not sentence ends; "patent.In 1887" (blocks
            // joined without a space by the extractor) is.
            '.' => {
                let glued = i > 0
                    && chars[i - 1].is_lowercase()
                    && next.is_some_and(|n| n.is_uppercase())
                    && chars.get(i + 2).is_some_and(|n| n.is_lowercase());
                (next.is_none_or(|n| n.is_whitespace()) || glued) && !ends_with_abbreviation(&cur)
            }
            _ => false,
        };
        if ends {
            push_sentence(&mut out, &mut cur);
        }
    }
    push_sentence(&mut out, &mut cur);
    out
}

fn ends_with_abbreviation(s: &str) -> bool {
    let tail = s.trim_end_matches('.').rsplit(' ').next().unwrap_or("").to_ascii_lowercase();
    matches!(tail.as_str(), "e.g" | "i.e" | "vs" | "mr" | "mrs" | "dr" | "st" | "approx" | "no" | "fig" | "etc" | "jr" | "sr" | "inc" | "ltd" | "co")
        || (tail.len() == 1 && tail.chars().all(|c| c.is_ascii_alphabetic()))
        // Dotted initialisms: "U.S.", "U.K.", "Ph.D."
        || (tail.contains('.') && tail.split('.').all(|p| p.len() <= 2))
}

fn push_sentence(out: &mut Vec<String>, cur: &mut String) {
    let s = clean(cur);
    if !s.is_empty() {
        out.push(s);
    }
    cur.clear();
}

/// Drop wiki reference markers ("[ 14 ]", "[citation needed]") and squash
/// whitespace.
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    let mut bracket = String::new();
    for c in s.chars() {
        match c {
            '[' => {
                depth += 1;
                bracket.clear();
            }
            ']' if depth > 0 => {
                depth -= 1;
                let b = bracket.trim().to_ascii_lowercase();
                let is_ref = b.chars().all(|c| c.is_ascii_digit() || c.is_whitespace() || c == ',')
                    || b.starts_with("citation")
                    || b.starts_with("note")
                    || b == "edit";
                if !is_ref {
                    out.push('[');
                    out.push_str(&bracket);
                    out.push(']');
                }
            }
            _ if depth > 0 => bracket.push(c),
            _ => out.push(c),
        }
    }
    // Social-post decoration: "#TajMahal #ShahJahan", emoji.
    out.split_whitespace()
        .filter(|w| !(w.starts_with('#') && w.chars().count() > 1))
        .map(|w| w.chars().filter(|c| !is_emoji(*c)).collect::<String>())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_emoji(c: char) -> bool {
    matches!(c as u32, 0x1F300..=0x1FAFF | 0x2600..=0x27BF | 0xFE0F | 0x200D)
}

static GLUED_HEADING: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"[a-z](The|In|A|An|This|It|He|She|They|On|At|As|For|When|After|Before)\b").unwrap()
});

static NAME: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"\b[A-Z][a-z]+(?:\s+[A-Z][a-z]+){1,3}\b").unwrap()
});
static NUMBER: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"\d[\d,]*(?:\.\d+)?").unwrap());

/// Words that start capitalised names but are not names.
const NOT_NAMES: &[&str] = &["The", "In", "A", "An", "This", "It", "On", "At", "For", "When", "After", "Before", "He", "She", "They", "His", "Her"];

/// The person/thing most sources agree on, by surname (last word of a
/// capitalised multi-word name), counted once per source. Redundancy across
/// independent pages is the classic signal for factoid answers (AskMSR).
fn consensus_name(passages: &[&SourceResult], query_lower: &str) -> Option<String> {
    let mut by_source: std::collections::HashMap<String, std::collections::HashSet<&str>> = Default::default();
    for p in passages {
        for m in NAME.find_iter(&p.extracted_text) {
            let words: Vec<&str> = m.as_str().split_whitespace().collect();
            if NOT_NAMES.contains(&words[0]) {
                continue;
            }
            let surname = words[words.len() - 1];
            if query_lower.contains(&surname.to_lowercase()) {
                continue; // a query word ("Telephone"), not an answer
            }
            by_source.entry(surname.to_string()).or_default().insert(p.url.as_str());
        }
    }
    by_source
        .into_iter()
        .map(|(n, s)| (n, s.len()))
        .filter(|(_, n)| *n >= 2)
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .map(|(n, _)| n)
}

/// Distinct sources containing each number.
fn number_support(passages: &[&SourceResult]) -> std::collections::HashMap<String, usize> {
    let mut by_number: std::collections::HashMap<String, std::collections::HashSet<&str>> = Default::default();
    for p in passages {
        for m in NUMBER.find_iter(&p.extracted_text) {
            let n = m.as_str().replace(',', "");
            if n.len() >= 2 {
                by_number.entry(n).or_default().insert(p.url.as_str());
            }
        }
    }
    by_number.into_iter().map(|(n, s)| (n, s.len())).collect()
}

/// Hindi question words. "क्या" also means "whether", but in a quick-answer
/// candidate it is almost always the question being repeated.
const HI_QUESTION_WORDS: &[&str] = &["क्या", "किसने", "कौन", "कब", "कहाँ", "कहां", "कैसे", "क्यों", "कितना", "कितने", "कितनी"];

/// Article filler that announces an answer instead of giving it.
const FILLER: &[&str] = &[
    "in this article", "in today's article", "this article", "we will tell", "we will discuss", "let us know", "let's know",
    "read on", "find out", "आर्टिकल", "क्या आप जानते", "आइये जानते", "आइए जानते", "जानते है", "इस लेख", "आज के इस", "जानते हैं", "बताएंगे", "बताने वाले", "जानकारी देंगे", "जानकारी देंगें",
];

/// Navigation boxes, year lists, tables flattened into text: "१८९६• १९००•
/// १९०४• …" (live, a Hindi Wikipedia navbox).
fn looks_like_debris(s: &str) -> bool {
    let bullets = s.chars().filter(|c| matches!(c, '•' | '|' | '·' | '►' | '»')).count();
    if bullets >= 3 {
        return true;
    }
    let non_space = s.chars().filter(|c| !c.is_whitespace()).count().max(1);
    let digits = s.chars().filter(|c| c.is_numeric()).count();
    let letters = s.chars().filter(|c| c.is_alphabetic() || ('\u{0900}'..='\u{0DFF}').contains(c)).count();
    digits * 3 > non_space || letters * 2 < non_space
}

/// Two or more capitalised words at the start: "Alexander Graham Bell was…".
fn starts_with_name(s: &str) -> bool {
    let words: Vec<&str> = s.split_whitespace().take(3).collect();
    words.len() >= 2
        && words[..2].iter().all(|w| {
            let mut c = w.chars();
            c.next().is_some_and(|f| f.is_uppercase()) && c.next().is_some_and(|f| f.is_lowercase())
        })
        && !matches!(words[0], "The" | "In" | "A" | "An" | "This" | "It" | "On" | "At" | "For")
}

fn is_definition_query(query: &str) -> bool {
    let q = query.trim().to_lowercase();
    ["what is ", "what are ", "what's ", "define ", "definition of ", "meaning of ", "who is ", "who was "]
        .iter()
        .any(|p| q.starts_with(p))
        || q.contains(" क्या है")
        || q.contains(" कौन है")
}

/// Fact-checking outlets. Their pages repeat the false claim word for word
/// before rebutting it, so the claim matches the query as well as any answer:
/// live 2026-10-27, "how do mRNA vaccines teach the immune system…" got
/// fullfact.org's quote of "SARS-CoV-2 doesn't exist, and the vaccines
/// actually destroy your immune system" as its quick answer.
const FACT_CHECK_HOSTS: &[&str] = &[
    "fullfact.org", "snopes.com", "politifact.com", "factcheck.org", "leadstories.com", "healthfeedback.org",
    "sciencefeedback.co", "checkyourfact.com", "factcheck.afp.com", "altnews.in", "boomlive.in", "factly.in",
    "vishvasnews.com", "newschecker.in", "thequint.com/news/webqoof", "misbar.com", "truthorfiction.com",
];

fn is_fact_check_page(url: &str, title: &str) -> bool {
    let u = url.to_ascii_lowercase();
    let host_and_path = u.split("://").nth(1).unwrap_or(&u).trim_start_matches("www.");
    let t = title.to_lowercase();
    FACT_CHECK_HOSTS.iter().any(|h| host_and_path.starts_with(h))
        || ["fact-check", "factcheck", "fact_check"].iter().any(|m| u.contains(m))
        || t.contains("fact check")
        || t.contains("fact-check")
}

/// A sentence that reports someone else's claim rather than stating a fact:
/// "Claim: …", a sentence that is one quotation, or one introduced by
/// "…posts claim that:" just before it.
fn is_reported_claim(s: &str, previous: Option<&str>) -> bool {
    let lower = s.to_lowercase();
    if ["claim:", "myth:", "fake:", "false:", "rumour:", "rumor:", "misleading:"].iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    let first = s.chars().next();
    let last = s.trim_end_matches(['.', '!']).chars().last();
    if first.is_some_and(|c| matches!(c, '"' | '“' | '‘' | '\'')) && last.is_some_and(|c| matches!(c, '"' | '”' | '’' | '\'')) {
        return true;
    }
    // "Many people assume the seasons happen because Earth moves closer to
    // the Sun…" states the misconception; it answers only when the same
    // sentence rebuts it ("…, but there's no evidence").
    let belief = [
        "many people assume", "many people think", "many people believe", "some people think", "some people believe",
        "it is commonly believed", "it's commonly believed", "common misconception", "popular belief", "you may have heard",
        "you might think", "people often think",
    ];
    let rebutted = [", but ", " however", "no evidence", "not true", "isn't true", "is false", "is a myth", "actually"];
    if belief.iter().any(|b| lower.contains(b)) && !rebutted.iter().any(|r| lower.contains(r)) {
        return true;
    }
    previous.is_some_and(|p| {
        let p = p.to_lowercase();
        p.ends_with(':')
            && ["claim", "post", "viral", "rumour", "rumor", "myth", "message", "video", "said", "says", "wrote"]
                .iter()
                .any(|w| p.contains(w))
    })
}

/// Common English words that carry no answer content.
const EN_STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "was", "were", "with", "that", "this", "from", "they", "them", "their", "have", "has",
    "had", "not", "but", "can", "will", "would", "could", "should", "its", "it's", "into", "than", "then", "there",
    "which", "who", "what", "when", "where", "why", "how", "also", "been", "being", "more", "most", "such", "some",
    "any", "all", "each", "other", "these", "those", "our", "your", "you", "his", "her", "she", "him", "one", "may",
    "use", "used", "very", "just", "only", "about", "does", "did", "doe",
];

/// Content words of `lower` that are not query terms: what the sentence adds.
fn new_words<'a>(lower: &'a str, stems: &[String]) -> Vec<&'a str> {
    lower
        .split(|c: char| !crate::util::is_word_char(c))
        .filter(|w| w.chars().count() >= 3 && !EN_STOPWORDS.contains(w) && !HI_STOPWORDS.contains(w))
        .filter(|w| !stems.iter().any(|st| w.starts_with(st.as_str())))
        .collect()
}

/// How many top-ranked pages contribute their opening.
const LEAD_PAGES: usize = 5;
const LEAD_CHARS: usize = 700;

/// The opening of the best-ranked extracted pages (by selection rank), as
/// passages. Snippet fallbacks are skipped: they are not the page.
pub fn leads(sources: &[SourceResult], rank_of: impl Fn(&str) -> Option<usize>) -> Vec<SourceResult> {
    let mut v: Vec<&SourceResult> = sources.iter().filter(|s| s.extractor != "snippet").collect();
    v.sort_by_key(|s| rank_of(&s.url).unwrap_or(usize::MAX));
    v.into_iter()
        .take(LEAD_PAGES)
        .map(|s| {
            // Skip a leading title line or very short fragments.
            let body: String = s
                .extracted_text
                .split('\n')
                .map(str::trim)
                .filter(|l| l.chars().count() >= 60)
                .collect::<Vec<_>>()
                .join(" ");
            SourceResult { extracted_text: crate::util::truncate_chars(&body, LEAD_CHARS).to_string(), ..s.clone() }
        })
        .filter(|s| !s.extracted_text.is_empty())
        .collect()
}

/// Do two consecutive query terms appear within a few words, in order?
/// ("invented the telephone" in "who invented the telephone".)
fn has_phrase(sentence_lower: &str, stems: &[String]) -> bool {
    let words: Vec<&str> = sentence_lower.split(|c: char| !crate::util::is_word_char(c)).filter(|w| !w.is_empty()).collect();
    stems.windows(2).any(|pair| {
        words.iter().enumerate().any(|(i, w)| {
            w.starts_with(pair[0].as_str())
                && words[i + 1..words.len().min(i + 4)].iter().any(|n| n.starts_with(pair[1].as_str()))
        })
    })
}

/// Pick the best answer sentence(s), or None when no sentence covers enough
/// of the query to be worth showing. `leads` are page openings (see
/// [`leads`]); their first sentences get a bonus.
pub fn extract(query: &str, intel: &QueryIntelligence, results: &[SourceResult], leads: &[SourceResult]) -> Option<Highlight> {
    let terms: Vec<String> = crate::ranking::query_terms(query)
        .into_iter()
        .filter(|t| !matches!(t.as_str(), "today" | "now" | "latest" | "current" | "explain"))
        .filter(|t| !HI_STOPWORDS.contains(&t.as_str()))
        .collect();
    if terms.is_empty() {
        return None;
    }
    let stems: Vec<String> = terms.iter().map(|t| stem(t)).collect();
    let wants_number = matches!(intel.intent, QueryIntent::Factual | QueryIntent::Temporal)
        || ["how many", "how much", "how far", "how long", "how old", "when ", "population", "price", "temperature"]
            .iter()
            .any(|p| query.to_lowercase().contains(p));
    let definitional = is_definition_query(query);
    let who_question = query.trim().to_lowercase().starts_with("who ");

    let query_lower = query.trim().to_lowercase();
    let all_passages: Vec<&SourceResult> = leads.iter().chain(results.iter().take(PASSAGES * 2)).collect();
    let answer_name = if who_question { consensus_name(&all_passages, &query_lower) } else { None };
    let numbers = if wants_number { number_support(&all_passages) } else { Default::default() };
    // Words per distinct page, for corroboration: an answer's new words show
    // up on other independent pages; a quoted claim or an off-topic aside
    // usually doesn't.
    let mut page_words: Vec<(&str, std::collections::HashSet<String>)> = Vec::new();
    for p in &all_passages {
        let words = p.extracted_text.to_lowercase().split(|c: char| !crate::util::is_word_char(c)).map(stem).collect::<Vec<_>>();
        match page_words.iter_mut().find(|(u, _)| *u == p.url.as_str()) {
            Some((_, set)) => set.extend(words),
            None => page_words.push((p.url.as_str(), words.into_iter().collect())),
        }
    }
    // The query restated (minus a trailing "?") — a sentence that repeats it
    // is usually announcing an answer, not giving one.
    let query_phrase: String = query_lower.trim_end_matches(['?', '।']).trim().to_string();
    let question_form = ["what ", "who ", "how ", "why ", "when ", "where ", "which ", "is ", "are ", "can ", "does ", "do "]
        .iter()
        .any(|w| query_lower.starts_with(w))
        || ["क्या", "कौन", "कैसे", "क्यों", "कब", "कहाँ"].iter().any(|w| query_lower.contains(w));

    // Candidates: (passage, rank position, is_lead).
    let candidates: Vec<(&SourceResult, usize, bool)> = leads
        .iter()
        .enumerate()
        .map(|(i, r)| (r, i, true))
        .chain(results.iter().take(PASSAGES).enumerate().map(|(i, r)| (r, i, false)))
        .collect();
    let mut best: Option<(f64, usize, usize, Vec<String>)> = None;
    for (ci, &(r, ri, is_lead)) in candidates.iter().enumerate() {
        let sents = sentences(&r.extracted_text);
        let fact_check = is_fact_check_page(&r.url, &r.title);
        for (si, s) in sents.iter().enumerate() {
            let len = s.chars().count();
            if !(MIN_SENTENCE_CHARS..=MAX_SENTENCE_CHARS).contains(&len) {
                continue;
            }
            if looks_like_debris(s) {
                continue;
            }
            // A question is never the answer, however many query words it
            // repeats. As a -1.0 penalty it still won: live 2026-10-27, a
            // heading "Why do noise-cancelling headphones work well on
            // airplane hum but not on people talking?" matched every query
            // word and beat the sentences that explain it.
            if s.trim_end_matches(['"', '”', '’', '\'', ')']).ends_with('?') {
                continue;
            }
            if is_reported_claim(s, si.checked_sub(1).map(|i| sents[i].as_str())) {
                continue;
            }
            let lower = s.to_lowercase();
            let added = new_words(&lower, &stems);
            // Nothing beyond the query's own words: a restatement.
            if added.is_empty() {
                continue;
            }
            let matched: Vec<usize> = (0..stems.len()).filter(|&i| lower.contains(stems[i].as_str())).collect();
            let coverage = matched.len() as f64 / stems.len() as f64;
            if coverage < MIN_COVERAGE {
                continue;
            }
            let mut score = coverage * 3.0;
            if wants_number && s.chars().any(|c| c.is_ascii_digit()) {
                score += 0.8;
            }
            if definitional
                && [" is a ", " is an ", " is the ", " are ", " refers to ", " means ", " was a ", " was an "]
                    .iter()
                    .any(|p| lower.contains(p))
            {
                score += 0.6;
            }
            // Answer redundancy across sources.
            if let Some(name) = &answer_name {
                if s.contains(name.as_str()) {
                    score += 1.5;
                }
            }
            if !numbers.is_empty() {
                let support = NUMBER
                    .find_iter(s)
                    .filter_map(|m| numbers.get(&m.as_str().replace(',', "")))
                    .max()
                    .copied()
                    .unwrap_or(0);
                score += 0.25 * support.saturating_sub(1).min(4) as f64;
            }
            // Questions and restatements of the question are not answers.
            if s.contains('?') || (question_form && query_phrase.chars().count() >= 8 && lower.contains(&query_phrase)) {
                score -= 1.0;
            }
            // A Hindi sentence holding the query's question word asks it
            // ("ताजमहल किसने बनवाया था / … (Taj Mahal Kisne Banaya Tha)");
            // the answer ("शाहजहाँ ने…") doesn't contain it.
            if HI_QUESTION_WORDS.iter().any(|w| query_lower.contains(w) && s.contains(w)) {
                continue;
            }
            if FILLER.iter().any(|f| lower.contains(f)) {
                score -= 1.5;
            }
            // Hindi definitional answer: "… नई दिल्ली है।"
            if definitional && (s.ends_with("है।") || s.ends_with("हैं।") || s.ends_with("था।")) && !s.contains("क्या") {
                score += 0.4;
            }
            // A heading glued onto the sentence ("Legal HeadachesThe inventor").
            let glued = GLUED_HEADING.is_match(s);
            if glued {
                score -= 1.0;
            }
            // "Who invented…" asks for a person: a sentence that opens with a
            // name answers it directly.
            if who_question && !glued && starts_with_name(s) {
                score += 1.2;
            }
            // Opens by pointing back at a sentence the reader cannot see.
            if ["because of this", "this ", "these ", "that ", "it ", "they ", "thus", "therefore", "however", "as a result"]
                .iter()
                .any(|p| lower.starts_with(p))
            {
                score -= 0.5;
            }
            // Sentence starts mid-thought ("and then…", "which…").
            if s.chars().next().is_some_and(|c| c.is_lowercase()) {
                score -= 0.7;
            }
            // The article states its answer up front.
            if is_lead && si < 2 {
                score += if si == 0 { 1.0 } else { 0.6 };
            }
            if stems.len() >= 2 && has_phrase(&lower, &stems) {
                score += 0.8;
            }
            // Navigation, calculators, cookie text.
            if ["click", "cookie", "subscribe", "sign up", "calculator", "enter the"].iter().any(|p| lower.contains(p)) {
                score -= 1.0;
            }
            // A page about the query beats a page that mentions it: the
            // sentence "...100 °C, the boiling point of water at sea level"
            // came from "Bain-marie" before this, over "Boiling point".
            let title = r.title.to_lowercase();
            let title_cov = stems.iter().filter(|s| title.contains(s.as_str())).count() as f64 / stems.len() as f64;
            score += 1.5 * title_cov;
            // Independent engines agreeing on the page.
            let engines = r.engine.split(',').filter(|e| !e.is_empty()).count();
            score += 0.15 * (engines.saturating_sub(1).min(3)) as f64;
            // No closing punctuation: a heading or menu run the extractor
            // joined into one line ("बना थाताजमहल किसने बनवाया था…").
            if !s.trim_end().ends_with(['.', '!', '।', ')', '"', '”', '’', '\'']) {
                score -= 0.8;
            }
            // Fact-check pages quote the claims they rebut; their rebuttals
            // rarely read as a standalone answer either.
            if fact_check {
                score -= 2.0;
            }
            // Corroboration: share of the sentence's new words found on at
            // least one other page. Centred on one half so it reorders
            // candidates without moving the MIN_SCORE bar.
            if added.len() >= 3 {
                let corroborated = added
                    .iter()
                    .filter(|w| {
                        let w = stem(w);
                        page_words.iter().any(|(u, set)| *u != r.url.as_str() && set.contains(&w))
                    })
                    .count();
                score += 0.8 * (corroborated as f64 / added.len() as f64 - 0.5);
            }
            // Earlier passages are better ranked; earlier sentences in a
            // passage are usually the lead.
            score *= 1.0 / (1.0 + 0.08 * ri as f64);
            score *= 1.0 / (1.0 + 0.03 * si as f64);
            if best.as_ref().is_none_or(|b| score > b.0) {
                let found = matched.iter().map(|&i| terms[i].clone()).collect();
                best = Some((score, ci, si, found));
            }
        }
    }

    let (score, ci, si, found) = best?;
    if score < MIN_SCORE {
        return None;
    }
    let r = candidates[ci].0;
    let sents = sentences(&r.extracted_text);
    let mut text = sents[si].clone();
    // A short lead sentence reads better with its follow-up.
    if text.chars().count() < 110 {
        if let Some(next) = sents.get(si + 1) {
            let n = next.chars().count();
            let next_lower = next.to_lowercase();
            if (MIN_SENTENCE_CHARS..=MAX_SENTENCE_CHARS).contains(&n)
                && next.chars().next().is_some_and(|c| !c.is_lowercase())
                && !next.contains('?')
                && !FILLER.iter().any(|f| next_lower.contains(f))
                && !HI_QUESTION_WORDS.iter().any(|w| query_lower.contains(w) && next.contains(w))
                && !is_reported_claim(next, Some(text.as_str()))
            {
                text = format!("{text} {next}");
            }
        }
    }
    // Index into `results` for the UI; a lead's page may not have a ranked
    // passage, in which case the first passage from the same URL (or 0).
    let result_index = results.iter().position(|x| x.url == r.url).unwrap_or(0);
    Some(Highlight { text, terms: found, url: r.url.clone(), title: r.title.clone(), result_index })
}

/// Save the highlight's exact inputs and output, so a scoring change can be
/// replayed offline against real pages (`cargo run --bin highlight_probe`)
/// instead of judged on whatever the engines return that day. Opt-in via
/// `DUMP_HIGHLIGHT_DIR`.
pub fn dump_case(query: &str, intel_query: &str, results: &[SourceResult], leads: &[SourceResult], got: Option<&Highlight>) {
    let Ok(dir) = std::env::var("DUMP_HIGHLIGHT_DIR") else { return };
    if dir.trim().is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    let slug: String = query.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).take(60).collect();
    let case = serde_json::json!({
        "query": query,
        "intel_query": intel_query,
        "results": &results[..results.len().min(PASSAGES * 2)],
        "leads": leads,
        "highlight": got.map(|h| &h.text),
    });
    let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{slug}.json")), case.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_intel::analyze_query;

    fn src(url: &str, title: &str, text: &str) -> SourceResult {
        SourceResult {
            url: url.into(),
            title: title.into(),
            extracted_text: text.into(),
            char_count: text.chars().count(),
            engine: "test".into(),
            ..Default::default()
        }
    }

    #[test]
    fn picks_the_sentence_with_the_number_for_a_factual_question() {
        let q = "boiling point of water at sea level";
        let results = vec![
            src("https://a.test/bain", "Bain-marie", "A bain-marie is a container of hot water used to cook food gently. The water temperature stays below the boiling point."),
            src("https://b.test/bp", "Boiling point", "The boiling point of a liquid depends on pressure. At sea level, the boiling point of water is 100 °C (212 °F). At higher altitudes water boils at lower temperatures."),
        ];
        let h = extract(q, &analyze_query(q), &results, &[]).expect("highlight");
        assert!(h.text.contains("100 °C"), "{h:?}");
        assert_eq!(h.result_index, 1);
        assert!(h.terms.iter().any(|t| t == "water"));
    }

    #[test]
    fn prefers_the_page_about_the_query_over_one_that_mentions_it() {
        let q = "boiling point of water at sea level";
        let results = vec![
            src("https://a.test/bain", "Bain-marie", "When the bain-marie is used at sea level, the water will not exceed 100 °C, the boiling point of water at sea level."),
            src("https://b.test/bp", "Boiling point - Wikipedia", "At sea level, water boils at 100 °C (212 °F), which is the boiling point of water."),
        ];
        let h = extract(q, &analyze_query(q), &results, &[]).unwrap();
        assert_eq!(h.result_index, 1, "{h:?}");
    }

    #[test]
    fn the_lead_of_the_top_page_answers_who_questions() {
        // Live 2026-10-02: passages alone gave "Thomas Edison invented the
        // carbon microphone which produced a strong telephone signal."
        let q = "who invented the telephone";
        let page = "https://en.wikipedia.org/wiki/Invention_of_the_telephone";
        let results = vec![src(page, "Invention of the telephone", "Thomas Edison invented the carbon microphone which produced a strong telephone signal.")];
        let leads = vec![src(page, "Invention of the telephone", "Alexander Graham Bell was the first to be awarded a patent for the electric telephone by the United States Patent and Trademark Office (USPTO) in March 1876. Before Bell, several inventors worked on transmitting sound.")];
        let h = extract(q, &analyze_query(q), &results, &leads).unwrap();
        assert!(h.text.starts_with("Alexander Graham Bell"), "{}", h.text);
    }

    #[test]
    fn leads_follow_selection_rank_and_skip_snippets() {
        let mut snippet = src("https://c.test/", "C", &"snippet text that is long enough to count as a line of content. ".repeat(2));
        snippet.extractor = "snippet";
        let pages = vec![
            src("https://b.test/", "B", &format!("B title\n{}", "Second ranked page opening sentence with enough characters to count. ".repeat(2))),
            src("https://a.test/", "A", &"First ranked page opening sentence with enough characters to count. ".repeat(2)),
            snippet,
        ];
        let rank = |u: &str| match u { "https://a.test/" => Some(0), "https://b.test/" => Some(1), _ => Some(2) };
        let l = leads(&pages, rank);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].url, "https://a.test/");
        assert!(!l[1].extracted_text.starts_with("B title"), "short title line skipped");
    }

    #[test]
    fn splitter_handles_glued_sentences_and_initialisms() {
        let s = sentences("Bell won the patent.In 1887, the U.S. government sued to annul it. Done here.");
        assert_eq!(s[0], "Bell won the patent.");
        assert_eq!(s[1], "In 1887, the U.S. government sued to annul it.");
    }

    #[test]
    fn glued_headings_lose_to_clean_sentences() {
        let q = "who invented the telephone";
        let results = vec![
            src("https://h.test/", "Telephone & Inventions", "Legal HeadachesThe inventor faced a legal battle over the telephone patent with rivals."),
            src("https://w.test/", "Alexander Graham Bell", "Alexander Graham Bell was an inventor who is credited with patenting the first practical telephone."),
        ];
        let h = extract(q, &analyze_query(q), &results, &[]).unwrap();
        assert!(h.text.starts_with("Alexander Graham Bell"), "{}", h.text);
    }

    #[test]
    fn who_questions_prefer_the_name_most_sources_agree_on() {
        // Live 2026-10-02: the Edison sentence matched every query word.
        let q = "who invented the telephone";
        let results = vec![
            src("https://a.test/", "History of the telephone", "Thomas Edison invented the carbon microphone which produced a strong telephone signal."),
            src("https://b.test/", "Telephone", "The telephone was patented by Alexander Graham Bell in 1876, who is widely credited as its inventor."),
            src("https://c.test/", "Bell", "Alexander Graham Bell made the first telephone call to Thomas Watson."),
            src("https://d.test/", "Inventors", "Elisha Gray and Alexander Graham Bell both filed telephone designs."),
        ];
        let h = extract(q, &analyze_query(q), &results, &[]).unwrap();
        assert!(h.text.contains("Bell"), "{}", h.text);
    }

    #[test]
    fn filler_that_restates_the_question_loses() {
        let q = "भारत की राजधानी क्या है";
        let results = vec![
            src("https://a.test/", "भारत की राजधानी क्या है", "यदि आपको नहीं पता कि भारत की राजधानी क्या है तो आज के इस आर्टिकल में हम आपको सम्पूर्ण जानकारी देंगें।"),
            src("https://b.test/", "नई दिल्ली", "नई दिल्ली भारत की राजधानी है। यह दिल्ली राष्ट्रीय राजधानी क्षेत्र का हिस्सा है।"),
        ];
        let h = extract(q, &analyze_query(q), &results, &[]).unwrap();
        assert!(h.text.starts_with("नई दिल्ली भारत की राजधानी है"), "{}", h.text);
    }

    #[test]
    fn numbers_confirmed_by_several_sources_win() {
        let q = "boiling point of water at sea level";
        let results = vec![
            src("https://a.test/", "Water facts", "At sea level the boiling point of water is 102 °C according to one blog."),
            src("https://b.test/", "Boiling point", "The boiling point of water at sea level is 100 °C (212 °F)."),
            src("https://c.test/", "Physics", "Water boils at 100 °C under standard pressure."),
            src("https://d.test/", "Cooking", "Pasta water reaches 100 °C when boiling."),
        ];
        let h = extract(q, &analyze_query(q), &results, &[]).unwrap();
        assert!(h.text.contains("100 °C"), "{}", h.text);
    }

    #[test]
    fn navigation_debris_is_never_a_highlight() {
        assert!(looks_like_debris("१८९६• १९००• १९०४• १९०८• १९१२• १९२०• भारत ने फ़्रांस की राजधानी पैरिस में भाग लिया था।"));
        assert!(looks_like_debris("2019 2020 2021 2022 2023 2024 2025 capital"));
        assert!(!looks_like_debris("The boiling point of water at sea level is 100 °C (212 °F)."));
        assert!(!looks_like_debris("नई दिल्ली भारत की राजधानी है।"));
    }

    #[test]
    fn stems_match_inflections() {
        assert_eq!(stem("boiling"), "boil");
        assert_eq!(stem("boils"), "boil");
        assert_eq!(stem("vaccines"), "vaccin");
        assert_eq!(stem("gas"), "gas");
        assert_eq!(stem("sea"), "sea");
    }

    #[test]
    fn prefers_definitions_for_what_is_questions() {
        let q = "what is photosynthesis";
        let results = vec![src(
            "https://a.test/p",
            "Photosynthesis",
            "Photosynthesis research has a long history [ 3 ]. Photosynthesis is the process by which plants use light energy to make sugars from carbon dioxide and water [ 4 ].",
        )];
        let h = extract(q, &analyze_query(q), &results, &[]).unwrap();
        assert!(h.text.starts_with("Photosynthesis is the process"), "{}", h.text);
        assert!(!h.text.contains("[ 4 ]"), "reference markers must be removed: {}", h.text);
    }

    #[test]
    fn none_when_nothing_covers_the_query() {
        let q = "capital of France";
        let results = vec![src("https://a.test/x", "x", "Odisha is a state in eastern India with a long coastline.")];
        assert!(extract(q, &analyze_query(q), &results, &[]).is_none());
    }

    #[test]
    fn sentence_splitter_respects_decimals_and_abbreviations() {
        let s = sentences("Water boils at 99.97 °C at 1 atm, e.g. at sea level. Next sentence here.");
        assert_eq!(s.len(), 2, "{s:?}");
        assert!(s[0].contains("99.97"));
    }

    #[test]
    fn devanagari_sentences_split_on_danda() {
        let s = sentences("भारत की राजधानी नई दिल्ली है। यह एक बड़ा शहर है।");
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn a_question_heading_is_never_the_answer() {
        // Live 2026-10-27: this heading matched every query word and won.
        let q = "How do noise-cancelling headphones work, and why do they block engine hum better than people talking?";
        let results = vec![
            src("https://p.test/", "Noise-Cancelling Headphones: The Physics of Anti-Noise", "Why do noise-cancelling headphones work well on airplane hum but not on people talking?"),
            src("https://f.test/", "How noise-cancelling headphones work", "Noise-cancelling headphones use microphones to measure incoming sound and play an inverted wave, which cancels steady low-frequency engine hum far better than changing speech."),
        ];
        let h = extract(q, &analyze_query(q), &results, &[]).unwrap();
        assert!(h.text.starts_with("Noise-cancelling headphones use"), "{}", h.text);
    }

    #[test]
    fn fact_check_pages_do_not_supply_the_claim_they_rebut() {
        // Live 2026-10-27: fullfact.org's quote of the false claim became the
        // quick answer.
        let q = "How do mRNA vaccines teach the immune system to fight a virus, and how are they different from traditional vaccines?";
        let results = vec![
            src("https://fullfact.org/health/mrna-vaccines/", "The mRNA vaccines don't show Covid-19 'doesn't exist'", "The mRNA vaccines are disguised as regular vaccines but they don't hold a portion of a virus because SARS-CoV-2 doesn't exist, and the vaccines actually destroy your immune system."),
            src("https://m.test/", "mRNA vaccines", "mRNA vaccines give cells instructions to make a harmless piece of the virus spike protein, which teaches the immune system to recognise the virus, while traditional vaccines put a weakened or inactivated virus into the body."),
            src("https://n.test/", "How vaccines work", "Traditional vaccines use a weakened or inactivated virus, and the immune system learns to recognise its spike protein."),
        ];
        let h = extract(q, &analyze_query(q), &results, &[]).unwrap();
        assert!(!h.url.contains("fullfact"), "{}", h.text);
        assert!(!h.text.contains("doesn't exist"), "{}", h.text);
    }

    #[test]
    fn a_stated_misconception_is_not_the_answer() {
        let q = "what causes the seasons on earth";
        let results = vec![
            src("https://a.test/", "Seasons", "Many people assume that the seasons on Earth happen because Earth moves closer to the Sun in summer."),
            src("https://b.test/", "Seasons", "The seasons on Earth are caused by the tilt of its axis, which changes how directly sunlight strikes each hemisphere."),
        ];
        let h = extract(q, &analyze_query(q), &results, &[]).unwrap();
        assert!(h.text.contains("tilt"), "{}", h.text);
        // Rebutted in the same sentence, it is an answer.
        assert!(!is_reported_claim("You may have heard that knuckle cracking causes arthritis, but there's no evidence to support this claim.", None));
    }

    #[test]
    fn reported_claims_are_recognised() {
        assert!(is_reported_claim("Claim: 5G towers spread the coronavirus.", None));
        assert!(is_reported_claim("\u{201c}Vaccines destroy your immune system.\u{201d}", None));
        assert!(is_reported_claim("Vaccines destroy your immune system.", Some("A viral post claims:")));
        assert!(!is_reported_claim("Vaccines train your immune system.", Some("Here is how they work.")));
    }

    #[test]
    fn hindi_sentences_repeating_the_question_word_lose() {
        // Live 2026-10-27: the answer was the query restated with a
        // transliteration.
        let q = "ताजमहल किसने बनवाया";
        let results = vec![
            src("https://a.test/", "ताजमहल किसने बनवाया", "ताजमहल किसने बनवाया था / ताजमहल किसने बनाया था (Taj Mahal Kisne Banaya Tha)।"),
            src("https://b.test/", "ताजमहल", "मुगल बादशाह शाहजहाँ ने अपनी पत्नी मुमताज़ महल की याद में ताजमहल बनवाया था। #TajMahal 🔥"),
        ];
        let h = extract(q, &analyze_query(q), &results, &[]).unwrap();
        assert!(h.text.starts_with("मुगल बादशाह शाहजहाँ"), "{}", h.text);
        assert!(!h.text.contains('#') && !h.text.contains('🔥'), "{}", h.text);
    }

    #[test]
    fn fact_check_pages_are_recognised() {
        assert!(is_fact_check_page("https://www.snopes.com/fact-check/x/", "x"));
        assert!(is_fact_check_page("https://fullfact.org/health/y/", "y"));
        assert!(is_fact_check_page("https://news.example/2026/z", "Fact Check: z"));
        assert!(!is_fact_check_page("https://en.wikipedia.org/wiki/Fact", "Fact"));
    }
}
