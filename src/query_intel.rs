// ============================================================================
// SearchWala v5.2.0 - Query Intelligence Engine
//
// Analyzes user queries BEFORE engine dispatch to determine:
// - Intent type (factual, temporal, person, comparison, howto, etc.)
// - Time sensitivity (needs current info vs evergreen)
// - Key entities (proper nouns, subjects)
// - Query complexity (simple fact vs deep research)
// - Optimal source count and response strategy
//
// All detection is rule-based (zero ML, zero latency, zero dependencies).
// ============================================================================

use std::collections::HashSet;

/// The detected intent category driving prompt selection and ranking behavior.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryIntent {
    /// Direct factual question: "capital of India", "boiling point of water"
    Factual,
    /// Time-sensitive: "latest OpenAI model", "current CEO of Google"
    Temporal,
    /// Person-focused: "Who is Elon Musk", "Tell me about Einstein"
    Person,
    /// Comparison: "X vs Y", "difference between A and B"
    Comparison,
    /// How-to / tutorial: "how to cook pasta", "how does quantum computing work"
    HowTo,
    /// Navigational: user wants a specific site ("reddit", "youtube login")
    Navigational,
    /// Deep research / complex multi-part query
    Research,
}

/// How complex the user's query is — drives source count and response depth.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryComplexity {
    /// 1-3 words, simple factual lookup
    Simple,
    /// 4-8 words, moderate detail needed
    Medium,
    /// 9+ words, multi-part or nuanced
    Complex,
}

/// Full intelligence report for a user query.
#[derive(Debug, Clone)]
pub struct QueryIntelligence {
    /// Primary detected intent
    pub intent: QueryIntent,
    /// Whether the query needs current/fresh information
    pub is_time_sensitive: bool,
    /// Extracted temporal hint if any ("2026", "today", "this week")
    pub temporal_hint: Option<String>,
    /// Key entities extracted from the query (proper nouns, subjects)
    pub key_entities: Vec<String>,
    /// Query complexity level
    pub complexity: QueryComplexity,
    /// Recommended number of sources to pass to LLM
    pub optimal_source_count: usize,
    /// Whether to boost news engines in dispatch
    pub boost_news_engines: bool,
}

// =============================================================================
// Public API
// =============================================================================

/// Analyze a query and produce a full intelligence report.
/// This runs in < 1ms — pure string matching, zero allocations beyond output.
pub fn analyze_query(query: &str) -> QueryIntelligence {
    let q = query.trim();
    let q_lower = q.to_lowercase();
    let words: Vec<&str> = q_lower.split_whitespace().collect();
    let word_count = words.len();

    // ── Step 1: Detect intent ──
    let intent = detect_intent(&q_lower, &words);

    // ── Step 2: Time sensitivity ──
    let (is_time_sensitive, temporal_hint) = detect_temporal(&q_lower, &intent);

    // ── Step 3: Entity extraction ──
    let key_entities = extract_entities(q);

    // ── Step 4: Complexity ──
    let complexity = if word_count <= 3 {
        QueryComplexity::Simple
    } else if word_count <= 8 {
        QueryComplexity::Medium
    } else {
        QueryComplexity::Complex
    };

    // ── Step 5: Optimal source count ──
    let optimal_source_count = match (&intent, &complexity) {
        (QueryIntent::Factual, QueryComplexity::Simple) => 15,
        (QueryIntent::Factual, _) => 20,
        (QueryIntent::Temporal, _) => 25,
        (QueryIntent::Person, _) => 25,
        (QueryIntent::Comparison, _) => 30,
        (QueryIntent::HowTo, _) => 25,
        (QueryIntent::Navigational, _) => 10,
        (QueryIntent::Research, _) => 40,
    };

    // ── Step 6: News engine boost ──
    let boost_news_engines = is_time_sensitive
        || intent == QueryIntent::Temporal
        || q_lower.contains("news")
        || q_lower.contains("breaking")
        || q_lower.contains("announced");

    QueryIntelligence {
        intent,
        is_time_sensitive,
        temporal_hint,
        key_entities,
        complexity,
        optimal_source_count,
        boost_news_engines,
    }
}

// =============================================================================
// Intent Detection — Pattern-based classification
// =============================================================================

fn detect_intent(q_lower: &str, words: &[&str]) -> QueryIntent {
    // ── Navigational (shortest circuit) ──
    let nav_sites = [
        "facebook", "youtube", "twitter", "reddit", "instagram", "tiktok",
        "amazon", "google", "github", "linkedin", "netflix", "spotify",
        "wikipedia", "stackoverflow", "gmail", "outlook", "whatsapp",
    ];
    if words.len() <= 3 {
        for site in &nav_sites {
            if q_lower.contains(site) && (q_lower.contains("login") || q_lower.contains("website") || q_lower == *site) {
                return QueryIntent::Navigational;
            }
        }
    }

    // ── Comparison ──
    if q_lower.contains(" vs ") || q_lower.contains(" versus ")
        || q_lower.contains("difference between")
        || q_lower.contains("compared to")
        || q_lower.contains("comparison of")
        || q_lower.contains(" or ") && (q_lower.contains("which") || q_lower.contains("better"))
    {
        return QueryIntent::Comparison;
    }

    // ── Person ──
    let person_patterns = [
        "who is", "who was", "who are", "tell me about",
        "biography of", "bio of", "life of", "about the person",
    ];
    for pattern in &person_patterns {
        if q_lower.starts_with(pattern) || q_lower.contains(pattern) {
            return QueryIntent::Person;
        }
    }

    // ── HowTo ──
    let howto_patterns = [
        "how to", "how do", "how can", "how does", "how is",
        "step by step", "steps to", "guide to", "tutorial",
        "instructions for", "ways to", "tips for", "best way to",
        "learn to", "teach me",
    ];
    for pattern in &howto_patterns {
        if q_lower.starts_with(pattern) || q_lower.contains(pattern) {
            return QueryIntent::HowTo;
        }
    }

    // ── Temporal (time-sensitive queries) ──
    let temporal_patterns = [
        "latest", "current", "newest", "recent", "today",
        "right now", "as of", "breaking", "just announced",
        "this year", "this week", "this month",
        "what is the price", "stock price", "market cap",
        "score of", "results of",
    ];
    for pattern in &temporal_patterns {
        if q_lower.contains(pattern) {
            return QueryIntent::Temporal;
        }
    }

    // Implicit temporal: role/status queries always need current info
    let role_patterns = [
        "ceo of", "president of", "prime minister of", "leader of",
        "founder of", "chairman of", "director of", "head of",
        "worth of", "net worth", "salary of", "age of",
    ];
    for pattern in &role_patterns {
        if q_lower.contains(pattern) {
            return QueryIntent::Temporal;
        }
    }

    // ── Research (complex queries) ──
    let research_patterns = [
        "explain", "analyze", "research", "comprehensive",
        "in depth", "detailed", "overview of", "deep dive",
        "implications of", "impact of", "future of",
    ];
    if words.len() >= 8 {
        return QueryIntent::Research;
    }
    for pattern in &research_patterns {
        if q_lower.contains(pattern) {
            return QueryIntent::Research;
        }
    }

    // ── Default: Factual ──
    QueryIntent::Factual
}

// =============================================================================
// Temporal / Time-Sensitivity Detection
// =============================================================================

fn detect_temporal(q_lower: &str, intent: &QueryIntent) -> (bool, Option<String>) {
    // Direct recency keywords
    let direct_recency = [
        "latest", "current", "newest", "recent", "today", "now",
        "updated", "breaking", "just", "new",
    ];

    // Time-relative keywords
    let time_relative = [
        ("this year", "this year"),
        ("this week", "this week"),
        ("this month", "this month"),
        ("this quarter", "this quarter"),
        ("yesterday", "yesterday"),
        ("last week", "last week"),
        ("last month", "last month"),
        ("right now", "right now"),
        ("as of", "as of"),
    ];

    // Implicit recency patterns (dynamic facts that change)
    let implicit_recency = [
        "who is the", "how much is", "what is the price",
        "what is the score", "what is the status",
        "is it", "are they", "will they",
        "ceo of", "president of", "prime minister",
        "leader of", "worth of", "net worth",
        "stock price", "market cap", "population of",
    ];

    // Check direct keywords
    for kw in &direct_recency {
        if q_lower.contains(kw) {
            return (true, Some(kw.to_string()));
        }
    }

    // Check time-relative phrases
    for (pattern, hint) in &time_relative {
        if q_lower.contains(pattern) {
            return (true, Some(hint.to_string()));
        }
    }

    // Check implicit recency
    for pattern in &implicit_recency {
        if q_lower.contains(pattern) {
            return (true, Some("current".to_string()));
        }
    }

    // Temporal intent is always time-sensitive
    if *intent == QueryIntent::Temporal {
        return (true, Some("current".to_string()));
    }

    // Check for year references (2020-2030)
    for year in 2020..=2030 {
        let year_str = year.to_string();
        if q_lower.contains(&year_str) {
            return (true, Some(year_str));
        }
    }

    (false, None)
}

// =============================================================================
// Entity Extraction — Lightweight proper noun detection
// =============================================================================

fn extract_entities(query: &str) -> Vec<String> {
    let stop_words: HashSet<&str> = [
        "what", "is", "the", "a", "an", "of", "in", "to", "for", "and", "or",
        "how", "does", "do", "can", "about", "with", "from", "that", "this",
        "are", "was", "were", "be", "been", "being", "have", "has", "had",
        "will", "would", "it", "its", "on", "at", "by", "but", "not", "so",
        "if", "than", "then", "my", "me", "we", "you", "your", "who", "which",
        "when", "where", "why", "all", "each", "tell", "give", "show", "find",
        "between", "vs", "versus", "compared", "latest", "current", "recent",
        "new", "best", "top", "most", "many", "much", "very", "just", "also",
        "now", "today", "explain", "describe", "analyze",
    ].iter().cloned().collect();

    let mut entities = Vec::new();
    let mut current_entity = Vec::new();

    for word in query.split_whitespace() {
        let clean = word.trim_matches(|c: char| !c.is_alphanumeric());
        if clean.is_empty() {
            continue;
        }

        let first_char = clean.chars().next().unwrap_or(' ');
        let lower = clean.to_lowercase();

        // Detect capitalized words that aren't stop words and aren't at sentence start
        if first_char.is_uppercase() && !stop_words.contains(lower.as_str()) && clean.len() > 1 {
            current_entity.push(clean.to_string());
        } else {
            if !current_entity.is_empty() {
                entities.push(current_entity.join(" "));
                current_entity.clear();
            }
        }
    }

    // Flush last entity
    if !current_entity.is_empty() {
        entities.push(current_entity.join(" "));
    }

    // Deduplicate
    let mut seen = HashSet::new();
    entities.retain(|e| seen.insert(e.clone()));

    entities
}

// =============================================================================
// Display implementations for logging
// =============================================================================

impl std::fmt::Display for QueryIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryIntent::Factual => write!(f, "factual"),
            QueryIntent::Temporal => write!(f, "temporal"),
            QueryIntent::Person => write!(f, "person"),
            QueryIntent::Comparison => write!(f, "comparison"),
            QueryIntent::HowTo => write!(f, "howto"),
            QueryIntent::Navigational => write!(f, "navigational"),
            QueryIntent::Research => write!(f, "research"),
        }
    }
}

impl std::fmt::Display for QueryComplexity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryComplexity::Simple => write!(f, "simple"),
            QueryComplexity::Medium => write!(f, "medium"),
            QueryComplexity::Complex => write!(f, "complex"),
        }
    }
}
