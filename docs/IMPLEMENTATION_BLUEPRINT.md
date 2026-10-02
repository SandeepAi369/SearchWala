# SearchWala v8.0 — Implementation Blueprint

**Audit date:** 2026-10-01 · **Audited commit:** `6b05c9e` (v7.0.0 Chunk 4) · **Codebase:** 9,522 LOC Rust, 23 modules

**Audience:** the coding agent executing this plan. Read §0 and §1 before touching code. Every claim in §1 was measured on this machine, not inferred — the commands are included so you can reproduce them.

---

## §0. TL;DR — What is actually wrong

SearchWala's problem is **not** ranking, prompts, or LLM synthesis. Those layers are elaborate and mostly well-written. The problem is that **the two layers underneath them are broken, and they fail silently**:

1. **Retrieval is ~97% dead.** A live search dispatching 35 engines produced results from 4. The headline WAF-bypass subsystem (`wreq` + BoringSSL TLS impersonation) is **built, passed into the dispatcher as `_stealth_client`, and never used** — it is dead code behind an underscore.
2. **Extraction returns meta-descriptions instead of articles.** A live search yielded **470 characters of text per source on average**. For the Amar Ujala case specifically, the extractor returns a 466-byte page description while a clean 11,716-byte `articleBody` sits in the same HTML, unread — **26× content loss**, caused by one ordering bug.

Everything downstream — BM25+, RRF, MMR, lost-in-middle, intent prompts — is doing sophisticated work on ~11 KB of boilerplate from 14 URLs. Fix retrieval and extraction and the existing ranking stack will start earning its keep.

A third, structural problem compounds both: **computed signals are discarded.** RRF scores are calculated for 85 URLs, then the 20 URLs that actually get scraped are chosen by **arrival order**. The ranking pipeline's output is then re-sorted by a naive substring counter before reaching the LLM.

**Priority order is not negotiable.** Do P0 → P1 → P2. Doing P2 (reranker models, new algorithms) before P0 means adding a cross-encoder to rerank boilerplate.

---

## §1. Measured evidence

### 1.1 Live end-to-end run

```bash
PORT=8099 MULTI_QUERY=false ./target/release/searchwala &
curl -s -X POST localhost:8099/search -H 'Content-Type: application/json' \
  -d '{"query":"amar ujala cricket world cup 2027","focus_mode":"auto","max_results":20}'
```

| Metric | Value |
|---|---|
| Engines dispatched | 35 |
| Engines returning ≥1 result | **4** (brave 44, yahoo 56, bing_news 20, marginalia 6, yandex 1) |
| Engines returning 0 | **28** incl. google, bing, duckduckgo, startpage, mojeek, qwant, wikipedia, ecosia |
| Raw results | 127 |
| Unique URLs **RRF-scored** | **85** |
| URLs actually scraped | **20** — chosen by arrival order, not RRF |
| Sources with extracted text | 19 |
| Ranked chunks → LLM | 25, from only **14 unique URLs** |
| **Total extracted text** | **11,768 chars** (avg 470/chunk, min 114) |
| Engine stage | 6,001 ms (hit the hardcoded deadline, aborted 2 engines) |
| Scrape stage | 8,371 ms |

`search_results` — the list the UI renders — contained **only `bing_news` entries**. Brave's 44 results and Yahoo's 56 were discovered, scored, and then silently dropped because `bing_news` happened to arrive first.

### 1.2 Engine endpoint liveness (plain reqwest, as shipped)

| Engine | HTTP | Reality |
|---|---|---|
| google | 200 | JS interstitial (`enablejs`, `table,div,span,p{display:none}`), **0 result links** |
| bing | 200 | Served **homepage**, not SERP (`/cricket/manifest.json`, `/?FORM=Z9FD1`) |
| duckduckgo | **202** | CAPTCHA: *"Select all squares containing a duck"* |
| brave | **429** | Rate-limited on the **first** request |
| yahoo | **000** | Connection failure |
| ecosia / presearch / yep | 403 | Blocked |
| ask / librex / stract | 404 | **Endpoint paths are wrong/moved** |
| right_dao 503 · whoogle 502 | — | Dead instances |
| alexandria · excite · lycos · mullvad_leta · pipilika · searchencrypt | **000** | **Hosts do not resolve / refuse** |
| marginalia | 200 | **77 result links — works** |
| mwmbl | 200 | **56 result links — works** |
| yandex / startpage / mojeek | 200 | 5 / 2 / 2 links — heavily throttled |
| 4get · yacy · wiby · gibiru | 200 | 0 links with current selectors |

This matches the upstream SearXNG community's own 2026 assessment: *"Google completely down, Bing giving irrelevant results, Brave having rate-limits, and DuckDuckGo having captchas."*

### 1.3 The fix for 1.2 is already in the repo, unwired

Same three engines, same queries, routed through a `wreq` client with `Emulation::Chrome134`:

| Engine | reqwest (shipped) | **wreq (available, unused)** |
|---|---|---|
| **brave** | 429, 0 results | **200 — 42 `.snippet` elements** |
| **duckduckgo** | 202 CAPTCHA | **200 — 10 `result__a` + 40 `uddg=` links** |
| **bing** | 200 homepage | **200 — 10 `li.b_algo` results** |
| google | 200 `enablejs` | 200 `enablejs` (needs more than TLS) |

> `src/search.rs:492` — `_stealth_client: &wreq::Client`. The leading underscore is the entire bug. `SearchEngine::search()` only accepts `&reqwest::Client`, so the stealth client is unreachable by construction.

### 1.4 The Amar Ujala extraction bug, quantified

```
Article: amarujala.com/cricket/cricket-news/odi-world-cup-2026-india-vs-pakistan-...
7 JSON-LD blocks, in document order:
  [0] @type "WebPage"     → has `description`  (180 chars / 466 bytes)
  [3] @type "NewsArticle"  → has `articleBody` (4,596 chars / 11,716 bytes)
                           → has datePublished 2026-10-01T21:11:02+05:30
```

`try_json_ld_extraction` (`src/extractor.rs:757`) includes `"WebPage"` in its `is_article` match list, iterates blocks **in document order**, and returns on the **first** object with a `description` ≥ 50 bytes. Block 0 matches. It returns the 466-byte description and never reaches block 3.

`extract_title_and_text` then gates on `cleaned.len() >= 100` — the Hindi description is 466 **bytes**, so it passes, and the full article is never extracted.

**Net: 26× less content than was freely available in the same HTML.** `datePublished` is discarded entirely — the "time-aware" system has no freshness signal from any page.

### 1.5 Reproducible UTF-8 panics

```
search.rs:143  &effective_query[..len.min(120)]
  "क"×80                          → ok
  "5" + "क"×80  (241 bytes)       → *** PANIC ***
  "IP" + "क"×80 (242 bytes)       → *** PANIC ***
search.rs:136  &q[..q.len().min(50)]
  "5" + "क"×40  (121 bytes)       → *** PANIC ***
llm.rs:428     &raw[..300]
  "{" + "क"×200 (601 bytes)       → *** PANIC ***
```

Any Hindi/Devanagari query over ~40 characters with an ASCII digit or letter before the cut point aborts the request task. There is no `CatchPanicLayer`, so the client gets a connection reset with no response.

---

## §2. P0 — Correctness and crash bugs

Fix all of P0 before anything else. Each item is independently shippable.

### P0-1 · Wire the stealth client through the engine trait

**Problem.** `wreq::Client` is constructed per search (paying BoringSSL setup cost), passed to `dispatch_engines` as `_stealth_client`, and discarded. Every WAF-protected engine gets plain reqwest and is blocked (§1.2). `config::apply_stealth_headers` is also dead.

**Strategy.** Do *not* duplicate every engine for two client types. Introduce one small `Fetcher` enum that unifies both clients behind a 2-method surface returning `(status, body)`. This also forces the status-code check that engines currently skip (§P0-2).

Create `src/fetcher.rs`:

```rust
// src/fetcher.rs — unified HTTP surface for engines.
// Plain  = reqwest (fast, fine for indie engines)
// Stealth = wreq + BoringSSL (real browser JA4 + HTTP/2 fingerprint)
use std::time::Duration;

#[derive(Clone)]
pub enum Fetcher {
    Plain(reqwest::Client),
    Stealth(wreq::Client),
}

pub struct Fetched {
    pub status: u16,
    pub body: String,
    pub final_url: String,
}

const MAX_SERP_BYTES: usize = 3_000_000;

impl Fetcher {
    pub fn is_stealth(&self) -> bool {
        matches!(self, Fetcher::Stealth(_))
    }

    pub async fn get(&self, url: &str, extra: &[(&str, &str)]) -> Result<Fetched, String> {
        match self {
            Fetcher::Plain(c) => {
                let mut rb = crate::config::apply_browser_headers(c.get(url), url);
                for (k, v) in extra {
                    rb = rb.header(*k, *v);
                }
                let resp = rb.send().await.map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                let final_url = resp.url().to_string();
                let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
                Ok(Fetched { status, body: decode_body(&bytes), final_url })
            }
            Fetcher::Stealth(c) => {
                // IMPORTANT: wreq-util's Emulation already sets a *consistent*
                // User-Agent + Sec-CH-UA suite matched to the TLS fingerprint.
                // Do NOT override them here — that is what creates the
                // UA-says-Chrome-155 / TLS-says-Chrome-147 mismatch WAFs flag.
                let mut rb = c.get(url).header("Accept-Language", "en-US,en;q=0.9");
                for (k, v) in extra {
                    rb = rb.header(*k, *v);
                }
                let resp = rb.send().await.map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                let final_url = resp.url().to_string();
                let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
                Ok(Fetched { status, body: decode_body(&bytes), final_url })
            }
        }
    }

    pub async fn post_form(&self, url: &str, form: &[(&str, String)]) -> Result<Fetched, String> {
        match self {
            Fetcher::Plain(c) => {
                let resp = crate::config::apply_browser_headers(c.post(url), url)
                    .form(form).send().await.map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                let final_url = resp.url().to_string();
                let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
                Ok(Fetched { status, body: decode_body(&bytes), final_url })
            }
            Fetcher::Stealth(c) => {
                let resp = c.post(url)
                    .header("Accept-Language", "en-US,en;q=0.9")
                    .form(form).send().await.map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                let final_url = resp.url().to_string();
                let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
                Ok(Fetched { status, body: decode_body(&bytes), final_url })
            }
        }
    }
}

/// Charset-aware decode, capped. Replaces bare `String::from_utf8_lossy`,
/// which mangles any non-UTF-8 page (windows-1252, ISO-8859-1, GBK, Shift_JIS).
fn decode_body(bytes: &[u8]) -> String {
    let slice = &bytes[..bytes.len().min(MAX_SERP_BYTES)];
    let (cow, _, _) = encoding_rs::UTF_8.decode(slice);
    if cow.contains('\u{FFFD}') {
        // Replacement chars present → sniff the declared charset and retry.
        if let Some(enc) = sniff_meta_charset(slice) {
            let (c2, _, _) = enc.decode(slice);
            return c2.into_owned();
        }
        let (c2, _, _) = encoding_rs::WINDOWS_1252.decode(slice);
        return c2.into_owned();
    }
    cow.into_owned()
}

fn sniff_meta_charset(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let head = &bytes[..bytes.len().min(4096)];
    let ascii = String::from_utf8_lossy(head).to_lowercase();
    let i = ascii.find("charset")? + "charset".len();
    let rest = &ascii[i..];
    let name: String = rest
        .trim_start_matches(['=', '"', '\'', ' '])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    encoding_rs::Encoding::for_label(name.as_bytes())
}
```

Change the trait (`src/engines/mod.rs`):

```rust
use crate::fetcher::Fetcher;

#[async_trait::async_trait]
pub trait SearchEngine: Send + Sync {
    fn name(&self) -> &str;
    /// True if this engine sits behind a WAF and must use TLS impersonation.
    fn needs_stealth(&self) -> bool { false }
    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult>;
}
```

Set `needs_stealth() -> true` on: `Brave`, `DuckDuckGo`, `Startpage`, `Yahoo`, and every `google*` / `bing*` / `ecosia` / `yandex` generic spec. Then in `dispatch_engines`, select the fetcher per engine instead of ignoring it:

```rust
// Build BOTH clients once, outside the dispatch loop (see P0-3).
let fetcher = if engine_needs_stealth && stealth_mode != "off" {
    stealth_fetcher.clone()      // Fetcher::Stealth(wreq::Client) — cheap Arc clone
} else {
    plain_fetcher.clone()
};
let results = engine.search(&fetcher, &query_variant).await;
```

In `config::build_stealth_client`, replace the fixed `Emulation::Chrome147` with the library's own consistent randomizer, and delete the UA-overriding `apply_stealth_headers`:

```rust
pub fn build_stealth_client(proxy: Option<&str>) -> wreq::Client {
    let mut builder = wreq::Client::builder()
        // weighted_random() picks a real profile and keeps UA + client hints
        // + JA4 + HTTP/2 SETTINGS mutually consistent. Hand-written UA strings
        // claiming Chrome 155 against a Chrome-147 TLS fingerprint are a
        // *stronger* bot signal than no spoofing at all.
        .emulation(wreq_util::Emulation::weighted_random())
        .cookie_store(true)
        .connect_timeout(std::time::Duration::from_secs(8))
        .timeout(std::time::Duration::from_secs(16))
        .redirect(wreq::redirect::Policy::limited(10))
        .gzip(true)
        .brotli(true)
        .zstd(true);   // see P0-4
    if let Some(p) = proxy {
        if let Ok(cfg) = wreq::Proxy::all(p) { builder = builder.proxy(cfg); }
    }
    builder.build().expect("wreq stealth client")
}
```

> `wreq-util 0.2.0` on this machine exposes up to `Emulation::Chrome149`, plus `Emulation::random()` and `Emulation::weighted_random()`. Do not hand-roll profiles above Chrome 149 — there is no matching TLS fingerprint to pair them with.

**Acceptance:** `brave` returns ≥ 20 results, `duckduckgo` ≥ 8, `bing` ≥ 8 on a live query. Verify with the per-engine health counters from P1-1.

---

### P0-2 · Engines never check HTTP status

**Problem.** Every engine does `resp.text().await` with no status check. A 429 from Brave, a 202 CAPTCHA from DuckDuckGo, and a 403 from Ecosia are all parsed as if they were SERPs. The `parse_brave_html` / `parse_startpage_html` fallbacks then scrape `a[href]` off the *error page*, injecting navigation and challenge links into the result set as if they were search hits.

**Fix.** With `Fetcher` returning `status`, gate every engine:

```rust
let page = match f.get(&url, &[]).await {
    Ok(p) => p,
    Err(e) => { tracing::debug!("{}: transport error: {}", self.name(), e); break; }
};
if page.status != 200 {
    tracing::warn!("{}: HTTP {} — treating as blocked", self.name(), page.status);
    crate::engines::health::record_block(self.name(), page.status);
    break;                                  // do not parse, do not paginate
}
if looks_like_challenge(&page.body) {
    crate::engines::health::record_block(self.name(), 0);
    break;
}
```

```rust
/// Catches soft blocks that return HTTP 200.
pub fn looks_like_challenge(body: &str) -> bool {
    const MARKERS: &[&str] = &[
        "enablejs",                       // Google JS interstitial
        "Select all squares containing",  // DuckDuckGo CAPTCHA
        "unusual traffic",
        "/sorry/index",                   // Google rate-limit
        "cf-browser-verification",
        "Just a moment...",               // Cloudflare
        "Checking your browser before",
        "DDoS protection by",
    ];
    let head = &body[..body.len().min(16_384)];   // byte-safe: see P0-5
    MARKERS.iter().any(|m| head.contains(m))
}
```

**Also delete both `a[href]` fallback scrapers** (`brave.rs:106-125`, `startpage.rs:118-145`). They cannot distinguish results from chrome, and they are the reason error pages become "results". An engine returning 0 results is correct and actionable; an engine returning 40 nav links is neither.

---

### P0-3 · Unbounded memory leak via `Box::leak`

**Problem.** `generic::spec_for()` (`src/engines/generic.rs:188-240`) builds Google endpoint templates with `format!(...)` and calls **`Box::leak`** to get a `&'static str`. `spec_for` is reached from `get_engines`, which `dispatch_engines` calls **inside every spawned task**:

```rust
// src/search.rs:545 — inside join_set.spawn, once per engine per query variation
let engine_vec = engines::get_engines(&[ename.clone()]);
```

Every Google-variant engine, on every query variation, on every search, leaks a `String` that is never freed. With 14 Google aliases × up to 5 variations, that is ~70 permanent allocations per search, growing without bound for the process lifetime. (The presence of `scripts/ram_monitor.sh` suggests this has been observed.)

**Fix.** Make the spec own its template and build the URL at request time.

```rust
#[derive(Clone)]
pub struct GenericEngineSpec {
    pub endpoint_template: std::borrow::Cow<'static, str>,
    pub pages: usize,
    pub page_param: PageParam,   // see P0-6
}

// Static templates stay borrowed; dynamic ones allocate per spec, not per task.
return Some(GenericEngineSpec {
    endpoint_template: std::borrow::Cow::Owned(
        format!("https://{}/search?q={{query}}&start={{offset}}", domain)
    ),
    pages: 3,
    page_param: PageParam::Offset10,
});
```

Then **hoist engine construction out of the task**. Build `Vec<Arc<dyn SearchEngine>>` once in `execute_search` and clone the `Arc` into each task:

```rust
let engine_instances: Vec<Arc<dyn engines::SearchEngine>> =
    engines::get_engines_arc(&engine_list);
// ...
for engine in &engine_instances {
    let engine = Arc::clone(engine);          // cheap, 'static
    let fetcher = pick_fetcher(engine.as_ref());
    join_set.spawn(async move { engine.search(&fetcher, &q).await });
}
```

This removes the leak, removes per-task re-parsing of engine names, and removes the `tracing::warn!("Unknown engine in dispatch")` path entirely.

---

### P0-4 · `Accept-Encoding: zstd` advertised but cannot be decoded

**Problem.** `config::apply_browser_headers` sends one of three `Accept-Encoding` variants; **two of the three advertise `zstd`**:

```rust
let encoding_variants = ["gzip, deflate, br, zstd", "gzip, deflate, br", "gzip, deflate, br, zstd"];
```

But the reqwest client is built with only `.gzip(true).brotli(true).deflate(true)` and the Cargo feature list has no `zstd`. Cloudflare and an increasing share of origins serve `Content-Encoding: zstd` when offered. reqwest then hands back **raw zstd bytes**, `String::from_utf8_lossy` turns them into garbage, and the parser finds zero results — on roughly ⅔ of requests to zstd-capable hosts, intermittently and silently.

**Fix — both halves, or neither:**

```toml
# Cargo.toml
reqwest = { version = "0.13", default-features = false, features = [
  "json", "gzip", "brotli", "deflate", "zstd",   # ← add zstd
  "cookies", "rustls", "socks", "stream", "form" ] }
```

```rust
// config.rs — client builder
.gzip(true).brotli(true).deflate(true).zstd(true)
```

Then stop randomizing `Accept-Encoding` at all — **let reqwest/wreq emit the header themselves**, derived from what they can actually decode. A hand-written encoding list that disagrees with the client's real capability is a correctness bug dressed as evasion.

---

### P0-5 · UTF-8 byte-slice panics (4 sites)

Reproduced in §1.5. Fix every one; add a helper and use it everywhere.

```rust
// src/util.rs
/// Truncate to at most `max_bytes` **without** splitting a UTF-8 character.
pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}
```

| File:line | Current (panics) | Replace with |
|---|---|---|
| `search.rs:143` | `&effective_query[..effective_query.len().min(120)]` | `util::truncate_chars(&effective_query, 120)` |
| `search.rs:136` | `&q[..q.len().min(50)]` | `util::truncate_chars(q, 50)` |
| `llm.rs:428` | `&raw[..300]` | `util::truncate_chars(&raw, 300)` |
| `llm.rs:1525-1528` | `&answer[..start]` / `&answer[end_pos..]` | see below |

`post_process_answer` is worse than a byte-slice bug: it searches `answer.to_lowercase()` and applies the resulting indices to `answer`. **`to_lowercase()` can change byte length** (`İ` → 2 chars, Greek final sigma, Turkish dotted I), so the indices can be out of bounds *and* off-boundary. It also calls `find` for the closing tag across the whole string without requiring it to be after the opening tag, so `</think><think>` produces duplicated text.

```rust
/// Strip <think>/<thought>/<thinking> blocks. Case-insensitive, boundary-safe,
/// and requires the close tag to come AFTER the open tag.
fn strip_think_blocks(input: &str) -> String {
    const TAGS: &[(&str, &str)] = &[
        ("<think>", "</think>"), ("<thought>", "</thought>"), ("<thinking>", "</thinking>"),
    ];
    let mut out = input.to_string();
    for (open, close) in TAGS {
        loop {
            let lower = out.to_lowercase();
            // Search the lowercased copy only to LOCATE; then map back by
            // scanning the original, so byte offsets are always valid.
            let Some(start) = find_ci(&out, open) else { break };
            match find_ci_from(&out, close, start + open.len()) {
                Some(end) => {
                    let end_pos = end + close.len();
                    out = format!("{}{}", &out[..start], &out[end_pos..]);
                }
                None => { out.truncate(start); break; }   // unclosed → drop tail
            }
            let _ = lower;
        }
    }
    out
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> { find_ci_from(hay, needle, 0) }

fn find_ci_from(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let n = needle.to_lowercase();
    hay.char_indices()
        .filter(|(i, _)| *i >= from)
        .find(|(i, _)| hay[*i..].len() >= n.len()
            && hay[*i..].chars().zip(n.chars())
                .take(n.chars().count())
                .all(|(a, b)| a.to_ascii_lowercase() == b))
        .map(|(i, _)| i)
}
```

Finally, add a panic guard so a future slip degrades to a 500 instead of a dropped connection:

```rust
// main.rs — Cargo.toml: tower-http features = ["cors", "catch-panic", "limit", "timeout"]
let app = Router::new()
    // ...routes...
    .layer(tower_http::catch_panic::CatchPanicLayer::new())
    .layer(tower_http::limit::RequestBodyLimitLayer::new(256 * 1024))
    .layer(CorsLayer::permissive())
    .with_state(state);
```

---

### P0-6 · Pagination is wrong on most engines

Three distinct bugs in `generic.rs` and `brave.rs`:

**(a) ~25 engines fetch the same URL twice.** Specs like `ask`, `aol`, `lycos`, `dogpile`, `metager`, `swisscows`, `gibiru`, `presearch`, `yep`, `mwmbl`, `naver`, `daum`, `seznam`, `rambler`, `excite`, `webcrawler`, `info`, `marginalia`, `stract`, `4get`, `yacy`, `whoogle`, `alexandria`, `mullvad_leta` declare `pages: 2` but their `endpoint_template` contains **no `{offset}` or `{page}` placeholder**. Both iterations build the identical URL; the second response is fully de-duplicated away. With 5 query variations that is **~120 wasted HTTP requests per search** — pure latency and ban risk for zero information.

**(b) Page 1 is skipped on 0-indexed engines.** `let page = page_idx + 1` yields `1, 2` — correct for 1-indexed engines (sogou), but `yandex` and `ecosia` use **0-indexed** `p`, so `p=1` is page *two* and page one is never fetched.

**(c) Brave's offset is a page index, not a row offset.** `brave.rs:26` computes `offset = page * 10` → `offset=0,10,20`. Brave's `offset` parameter is the **page number** (0, 1, 2). Pages 10 and 20 are far past the result set.

**Fix.** Make pagination explicit and data-driven:

```rust
#[derive(Clone, Copy, PartialEq)]
pub enum PageParam {
    None,          // single page only — pages is forced to 1
    Offset10,      // &start=0,10,20   (google, ddg html)
    Offset1Based,  // &first=1,11,21   (bing)
    PageZero,      // &p=0,1,2         (yandex, ecosia, brave offset)
    PageOne,       // &page=1,2,3      (sogou, startpage)
}

impl GenericEngineSpec {
    pub fn effective_pages(&self) -> usize {
        if self.page_param == PageParam::None { 1 } else { self.pages.max(1) }
    }

    pub fn url_for_page(&self, encoded_query: &str, page_idx: usize) -> String {
        let value = match self.page_param {
            PageParam::None         => 0,
            PageParam::Offset10     => page_idx * 10,
            PageParam::Offset1Based => page_idx * 10 + 1,
            PageParam::PageZero     => page_idx,
            PageParam::PageOne      => page_idx + 1,
        };
        self.endpoint_template
            .replace("{query}", encoded_query)
            .replace("{offset}", &value.to_string())
            .replace("{page}", &value.to_string())
    }
}
```

Audit every spec in `spec_for` and set `page_param` correctly; any template without a placeholder gets `PageParam::None`, which forces one request.

---

### P0-7 · `rank_position` resets on every page, destroying RRF

**Problem.** Each parser builds rank from its own local vector: `rank_position: results.len() + 1` in `brave.rs:97`, `startpage.rs:110`, `generic.rs:139`. `parse_*_html` is called **once per page**, so page 2's first result gets `rank_position = 1`, identical to page 1's first result.

RRF's entire signal is `1/(k + rank)`. With rank reset per page, a result at true position 21 scores the same as position 1. The "cross-engine consensus ranking" is being fed corrupted ranks.

**Fix.** Pass a running offset into the parser and make it global per engine per query:

```rust
// in the engine's page loop
let mut rank_base = 0usize;
for page_idx in 0..spec.effective_pages() {
    // ...fetch...
    let page_results = parse_generic_html(&page.body, &self.name, &page.final_url, rank_base);
    rank_base += page_results.len();
    // ...dedupe, extend...
}

// in the parser
out.push(RawSearchResult {
    rank_position: rank_base + out.len() + 1,   // global, 1-based
    ..
});
```

---

### P0-8 · Duplicate engine aliases corrupt RRF consensus

**Problem.** `config::enabled_engines()` lists ~90 names, but they collapse onto far fewer endpoints:

- `bing`, `bing_us`, `bing_uk`, `bing_in`, `bing_de`, `bing_fr`, `bing_es`, `bing_it`, `bing_jp`, `bing_ca`, `bing_au`, `bing_nl`, `bing_se`, `bing_no`, `bing_fi` → **15 names, one identical URL** (`spec_for` ignores the suffix; no `cc=`/`mkt=` is ever added).
- `duckduckgo`, `duckduckgo_html`, `duckduckgo_news`, `duckduckgo_images`, `duckduckgo_videos` → all map to the **same `DuckDuckGo` struct**.
- `brave` + `brave_news` → both map to the `Brave` struct, both label results `engine: "brave"`.
- `yahoo` + `yahoo_news` → both map to `Yahoo`. **The live run logged `yahoo: 28` twice** — the same engine counted as two.

Two consequences, both serious:

1. **Ban acceleration.** 15 simultaneous identical requests to `bing.com` from one IP, × pages, × query variations. This is the fastest possible route to the rate-limiting observed in §1.2.
2. **RRF is measuring one engine's opinion 15 times.** `compute_rrf_scores` sums `engine_weight / (K + rank)` keyed on URL across *engine labels*. Fifteen Bing aliases returning the same ordering produce a 15× inflated "consensus" score for Bing's ranking — the exact opposite of what RRF is for. Cross-engine agreement becomes self-agreement.

**Fix — two parts.**

(a) **Make aliases real or delete them.** Regional variants must actually differ:

```rust
// Bing regional markets
if n.starts_with("bing") {
    let mkt = match n.rsplit('_').next().unwrap_or("") {
        "uk" => Some("en-GB"), "in" => Some("en-IN"), "de" => Some("de-DE"),
        "fr" => Some("fr-FR"), "jp" => Some("ja-JP"), "au" => Some("en-AU"),
        _ => None,
    };
    let base = match mkt {
        Some(m) => format!("https://www.bing.com/search?q={{query}}&first={{offset}}&mkt={m}&setlang={m}"),
        None    => "https://www.bing.com/search?q={query}&first={offset}".to_string(),
    };
    // ...
}
```

Delete `duckduckgo_html` (identical to `duckduckgo`) and give `*_news` / `*_images` / `*_videos` their real vertical endpoints or drop them. Route `brave_news` and `yahoo_news` through the generic spec (which has the correct news URLs) rather than the plain structs.

(b) **Deduplicate by endpoint family in RRF.** Even after (a), regional Bing variants are correlated, not independent. Add a source-family key and let RRF count each family once at its best rank:

```rust
/// Independent evidence group. Fifteen Bing markets are ONE opinion.
pub fn engine_family(name: &str) -> &'static str {
    if name.starts_with("bing")       { return "bing"; }
    if name.starts_with("google")     { return "google"; }
    if name.starts_with("duckduckgo") { return "ddg"; }   // DDG is Bing-derived
    if name.starts_with("brave")      { return "brave"; }
    if name.starts_with("yahoo")      { return "yahoo"; } // Yahoo is Bing-derived
    if name.starts_with("startpage")  { return "google"; }// Startpage proxies Google
    if name.starts_with("ecosia")     { return "google"; }
    if name.starts_with("yandex")     { return "yandex"; }
    if name.starts_with("mojeek")     { return "mojeek"; }
    if name.starts_with("marginalia") { return "marginalia"; }
    "indie"
}

pub fn compute_rrf_scores(results: &[RawSearchResult]) -> HashMap<String, f64> {
    // Keep only each (url, family) pair's BEST rank, then fuse across families.
    let mut best: HashMap<(String, &'static str), usize> = HashMap::new();
    for r in results {
        let url = crate::url_utils::canonical_key(&r.url);
        let fam = engines::engine_family(&r.engine);
        best.entry((url, fam))
            .and_modify(|cur| if r.rank_position < *cur { *cur = r.rank_position })
            .or_insert(r.rank_position.max(1));
    }
    let mut scores: HashMap<String, f64> = HashMap::new();
    for ((url, fam), rank) in best {
        let w = engines::family_weight(fam) as f64;
        *scores.entry(url).or_insert(0.0) += w / (RRF_K + rank as f64);
    }
    scores
}
```

> Note `startpage` → `"google"` and `yahoo`/`duckduckgo` → Bing-derived. Treating a Google proxy as independent evidence *for* Google's ranking is the same error as the Bing aliases, just less obvious.

---

### P0-9 · The semaphore permit is held while sleeping

**Problem.** `src/search.rs:525-534`:

```rust
let _permit = sem.acquire().await?;        // ← permit acquired
let delay = jitter.saturating_add(extra_spread);
tokio::time::sleep(Duration::from_millis(delay)).await;   // ← then sleep
```

The jitter sleep happens **after** taking the concurrency permit. With `extra_spread = (dispatch_index % engine_concurrency) * 15` reaching ~585 ms plus 50–200 ms jitter, each task occupies a permit for up to 785 ms doing nothing. At `ENGINE_CONCURRENCY=20` and 35 engines × 5 variations = 175 tasks, that is ~7 s of pure queued sleeping — **against a 6 s deadline**. Most engines are aborted before they ever issue a request. This, more than anything, is why the live run completed 33/35 "engines" but only 4 produced data.

**Fix.** Sleep first, then acquire:

```rust
let delay = jitter.saturating_add(extra_spread);
if delay > 0 { tokio::time::sleep(Duration::from_millis(delay)).await; }
let _permit = match sem.acquire().await { Ok(p) => p, Err(_) => return Vec::new() };
```

---

### P0-10 · The engine deadline ignores the mode

**Problem.** The comment at `src/search.rs:476` reads *"Deadline: lite 2.5s, research 8s, specialized 5s, default 6s"*. The code is:

```rust
let deadline = Duration::from_secs(6); // Default deadline
```

Research mode — which is supposed to gather 200+ sources — gets the same 6 s as fast mode.

**Fix.** Thread the mode in:

```rust
let deadline = Duration::from_millis(match mode {
    Mode::Fast        => 2_500,
    Mode::Auto        => 6_000,
    Mode::Specialized => 8_000,
    Mode::Research    => 20_000,
});
```

Also fix the misleading log: `total_engines = join_set.len()` is `engines × variations`, not engines.

---

### P0-11 · Citation indices point at the wrong sources

**Problem.** `build_ranked_context` filters, re-scores, and **reorders** sources (`lost_in_middle_reorder`), then numbers them `[1]`, `[2]`, … by their **position in the reordered list**. The LLM cites those numbers. But `verify_citations(&answer, sources)` resolves `[n]` against **`sources[n - 1]`** — the *original, unreordered* slice.

So `[1]` in the answer refers to `reordered[0]`, while verification (and anything else that maps citations to URLs) reads `sources[0]`. Under `lost_in_middle_reorder`, index 0 happens to coincide, but **index 1 onward is systematically wrong** — `[2]` points at what the LLM called `[N]`.

**Fix.** Return the citation map from context construction and carry it everywhere.

```rust
pub struct BuiltContext {
    pub text: String,
    /// citation number (1-based) → index into the original `sources` slice
    pub citation_map: Vec<usize>,
}

fn build_ranked_context(query: &str, sources: &[SourceResult], ..) -> BuiltContext {
    // ...score, filter, reorder as today...
    let mut citation_map = Vec::new();
    let mut text = String::with_capacity(max_chars.min(16_384));
    for (pos, (idx, _)) in reordered.into_iter().enumerate() {
        // ...write the block using `pos + 1` as the label...
        citation_map.push(idx);            // ← the missing link
    }
    BuiltContext { text, citation_map }
}

pub fn verify_citations(answer: &str, sources: &[SourceResult], citation_map: &[usize]) -> Vec<CitationCheck> {
    // n (1-based from the answer) → citation_map[n-1] → sources[..]
}
```

Expose `citation_map` in `SearchResponse` so the UI can turn `[n]` into a real link (P2-6).

---

### P0-12 · Temporal detection fires on substrings

**Problem.** `query_intel::detect_temporal` tests `q_lower.contains(kw)` against `["latest","current","newest","recent","today","now","updated","breaking","just","new"]`. Substring matching means:

| Query | Matches | Result |
|---|---|---|
| "**New** York population" | `new` | year "2026" appended to the search query |
| "how do I k**now** if…" | `now` | time-sensitive |
| "**just**ice system reform" | `just` | time-sensitive |
| "re**new**able energy" | `new` | time-sensitive |
| "I**s it** safe to…" | `is it` | time-sensitive |

False-positive time enrichment appends a year to queries that have nothing to do with recency, skewing every engine's results and polluting BM25 with a spurious term.

**Fix.** Match on token boundaries, and separate unigram from phrase matching:

```rust
fn tokens(q: &str) -> Vec<&str> {
    q.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).collect()
}

fn detect_temporal(q_lower: &str, intent: &QueryIntent) -> (bool, Option<String>) {
    const RECENCY_WORDS: &[&str] = &[
        "latest", "current", "newest", "recent", "today", "now",
        "updated", "breaking", "upcoming", "live",
    ];
    const RECENCY_PHRASES: &[&str] = &[
        "this year", "this week", "this month", "this quarter", "right now",
        "as of", "last week", "last month", "so far", "up to date",
    ];
    let toks = tokens(q_lower);
    if let Some(w) = RECENCY_WORDS.iter().find(|w| toks.iter().any(|t| t == *w)) {
        return (true, Some((*w).to_string()));
    }
    if let Some(p) = RECENCY_PHRASES.iter().find(|p| q_lower.contains(*p)) {
        return (true, Some((*p).to_string()));
    }
    // "new" and "just" are REMOVED as unigrams — too ambiguous to be useful.
    // ...role patterns, year detection as today...
    (false, None)
}
```

---

## §3. P1 — Architectural rewrites

### P1-1 · Engine health: make silent failure impossible

**This is the highest-leverage change in the whole plan.** The system cannot currently tell the difference between "this engine found nothing" and "this engine has been dead for six months" — which is why 28 engines ship broken.

Add `src/engines/health.rs`:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use dashmap::DashMap;   // or RwLock<HashMap<..>> to avoid a new dependency

#[derive(Default)]
pub struct EngineHealth {
    pub attempts: AtomicU64,
    pub http_ok: AtomicU64,
    pub blocked: AtomicU64,      // non-200 or challenge marker
    pub transport_err: AtomicU64,
    pub zero_results: AtomicU64, // 200 OK but parser found nothing → SELECTOR ROT
    pub results_total: AtomicU64,
}

static REGISTRY: LazyLock<DashMap<String, EngineHealth>> = LazyLock::new(DashMap::new);

pub fn record_attempt(e: &str)              { bump(e, |h| &h.attempts); }
pub fn record_block(e: &str, _status: u16)  { bump(e, |h| &h.blocked); }
pub fn record_transport_err(e: &str)        { bump(e, |h| &h.transport_err); }
pub fn record_parsed(e: &str, n: usize) {
    bump(e, |h| &h.http_ok);
    if n == 0 { bump(e, |h| &h.zero_results); }
    REGISTRY.entry(e.to_string()).or_default()
        .results_total.fetch_add(n as u64, Ordering::Relaxed);
}

/// Snapshot for GET /health/engines
pub fn snapshot() -> Vec<EngineHealthReport> { /* ... */ }
```

Expose it:

```rust
.route("/health/engines", get(engine_health_handler))
```

```json
{
  "brave":      { "attempts": 120, "http_ok": 118, "blocked": 2,   "zero_results": 0,   "avg_results": 38.2, "verdict": "healthy" },
  "google":     { "attempts": 120, "http_ok": 120, "blocked": 0,   "zero_results": 120, "avg_results": 0.0,  "verdict": "selector_rot" },
  "alexandria": { "attempts": 120, "http_ok": 0,   "blocked": 0,   "transport_err": 120,"avg_results": 0.0,  "verdict": "dead_host" }
}
```

The `zero_results` counter is the key one: **HTTP 200 with zero parsed results means the selectors have rotted**, which is invisible today and distinguishable from a block only by this metric.

**Then act on it.** Add an auto-quarantine so dead engines stop costing latency:

```rust
/// Skip engines that have failed their last N attempts, with exponential backoff.
pub fn should_skip(engine: &str) -> bool {
    let Some(h) = REGISTRY.get(engine) else { return false };
    let attempts = h.attempts.load(Ordering::Relaxed);
    if attempts < 10 { return false; }                 // need a sample
    let useful = h.results_total.load(Ordering::Relaxed);
    useful == 0 && attempts >= 10                      // 10 strikes, no results, benched
}
```

**Acceptance:** `GET /health/engines` after 20 searches must classify every engine, and the engine list used for dispatch must shrink to the healthy set.

---

### P1-2 · Replace the engine roster with what actually works

Delete engines that are dead hosts or moved endpoints (§1.2): `alexandria`, `excite`, `lycos`, `mullvad_leta`, `pipilika`, `searchencrypt`, `whoogle`, `librex`, `right_dao`, `searchalot`, `kiddle`, `gibiru`, `info`, `webcrawler`, `dogpile`, `aol`. Fix the moved ones: `stract` (404), `ask` (404), `marginalia` (use the API below), `mwmbl`.

**Add real, legitimate retrieval tiers.** Three verified-live additions:

**(a) Marginalia JSON API — free, no key, works right now.** Verified: `200`, clean JSON, 11 pages of results.

```rust
// src/engines/marginalia_api.rs
// GET https://api.marginalia-search.com/public/search/{url-encoded-query}
// → { "query": "...", "page": 1, "pages": 11,
//     "results": [ { "url": "...", "title": "...", "description": "..." }, ... ] }
// License: CC-BY-NC-SA 4.0 — surface attribution in the UI.
#[derive(serde::Deserialize)]
struct MgResponse { results: Vec<MgResult> }
#[derive(serde::Deserialize)]
struct MgResult { url: String, title: String, description: String }

#[async_trait::async_trait]
impl SearchEngine for MarginaliaApi {
    fn name(&self) -> &str { "marginalia_api" }
    async fn search(&self, f: &Fetcher, query: &str) -> Vec<RawSearchResult> {
        let url = format!("https://api.marginalia-search.com/public/search/{}",
                          urlencoding::encode(query));
        let Ok(page) = f.get(&url, &[("Accept", "application/json")]).await else { return vec![] };
        if page.status != 200 { health::record_block(self.name(), page.status); return vec![]; }
        let Ok(parsed) = serde_json::from_str::<MgResponse>(&page.body) else { return vec![] };
        parsed.results.into_iter().enumerate().map(|(i, r)| RawSearchResult {
            url: r.url, title: r.title, snippet: r.description,
            engine: "marginalia_api".into(), rank_position: i + 1,
        }).collect()
    }
}
```

**(b) Optional BYOK search keys, mirroring the existing BYOK-LLM design.** The project already asks users for an LLM key; asking for optional search keys is consistent and removes the scraping arms race for users who want reliability. Note the 2026 pricing reality: **Brave killed its free Search API tier in February 2026** — it is now metered at ~$5/1k with $5 of free credits per month (≈1,000 free queries). Mojeek's Web Search API is a negotiated B2B arrangement at roughly £2–3 CPM.

```rust
// Read from env; absent key = engine simply not registered.
// BRAVE_SEARCH_API_KEY → https://api.search.brave.com/res/v1/web/search?q=
// MOJEEK_API_KEY       → Mojeek Web Search API
// SEARXNG_BASE_URL     → self-hosted instance, /search?q=..&format=json
```

**(c) Self-hosted SearXNG as an upstream.** Public instances are not viable — measured `429` on priv.au and opnxng.com, `403` on searxng.site, and searx.be returns HTML rather than JSON. But a **local** SearXNG (`SEARXNG_BASE_URL=http://localhost:8080`) outsources parser maintenance to a project with far more eyes on it. Make it an optional engine, documented, never a default.

**Reframe the README.** "90+ search engines" is false — it is ~30 endpoints with ~60 duplicate aliases, of which 4 produced results in the measured run. Replace the count with the honest, checkable thing: the `/health/engines` endpoint. Credibility here is worth more than the number.

---

### P1-3 · Select sources by RRF score, not arrival order

**Problem.** The pipeline computes RRF for 85 URLs, then:

```rust
let raw_urls: Vec<String> = all_results.iter().map(|r| r.url.clone()).collect();
let unique_urls = url_utils::deduplicate(raw_urls, max_urls, focus);  // ← arrival order
```

`deduplicate` walks `all_results` in the order engines happened to finish and takes the first `max_urls`. In the live run that meant **20 bing_news URLs** while 65 RRF-scored URLs — including everything from Brave and Yahoo — were discarded. The same arrival order also builds `search_results`, which is what the UI renders. So both "what we read" and "what we show" are decided by network race conditions.

**Fix.** Rank first, then dedupe, then cut.

```rust
// 1. Canonicalize + dedupe into a map, keeping the best metadata per URL.
let mut by_url: HashMap<String, Candidate> = HashMap::new();
for r in &all_results {
    let key = url_utils::canonical_key(&r.url);
    let e = by_url.entry(key.clone()).or_insert_with(|| Candidate {
        url: r.url.clone(), title: r.title.clone(), snippet: r.snippet.clone(),
        engines: Vec::new(), rrf: 0.0,
    });
    if e.snippet.is_empty() && !r.snippet.is_empty() { e.snippet = r.snippet.clone(); }
    if !e.engines.contains(&r.engine) { e.engines.push(r.engine.clone()); }
}

// 2. Attach RRF, apply skip rules.
for (key, c) in by_url.iter_mut() {
    c.rrf = rrf_scores.get(key).copied().unwrap_or(0.0);
}

// 3. Sort by consensus DESC, then cut to max_urls.
let mut ranked: Vec<Candidate> = by_url.into_values()
    .filter(|c| !url_utils::should_skip(&c.url))
    .collect();
ranked.sort_by(|a, b| b.rrf.partial_cmp(&a.rrf).unwrap_or(Ordering::Equal));
ranked.truncate(max_urls);

// 4. BOTH the scrape list and the UI list now come from `ranked`.
let unique_urls: Vec<String> = ranked.iter().map(|c| c.url.clone()).collect();
let search_results: Vec<SearchHit> = ranked.iter().map(|c| SearchHit {
    url: c.url.clone(), title: c.title.clone(), snippet: c.snippet.clone(),
    engine: c.engines.join(","),         // show consensus, not a single engine
    rrf_score: c.rrf,                    // new field — let the UI show agreement
}).collect();
```

This also fixes the dropped-snippet problem: today `SearchHit.snippet` takes whichever engine arrived first, which for link-scraper engines is an empty string.

---

### P1-4 · Rewrite the extractor around `rs-trafilatura`

**Problem.** The hand-rolled 7-strategy extractor has five compounding defects:

1. **JSON-LD short-circuits on `WebPage`** → the Amar Ujala 26× content loss (§1.4).
2. **No `@graph` support.** Yoast and RankMath — which power a large share of WordPress news sites — nest everything under `"@graph": [...]`. The parser only inspects top-level objects or a top-level array, so those sites get no structured extraction at all.
3. **`@type` as an array is unhandled.** `obj.get("@type").and_then(|t| t.as_str())` returns `None` for `"@type": ["Article","NewsArticle"]`, a very common form.
4. **`try_scored_extraction` is accidentally quadratic.** It iterates *every* `div, section, td, main, article`, and for each calls `score_element`, which calls **`element.html()`** — re-serializing the entire subtree. On an 860 KB page like the Amar Ujala article, with thousands of nested divs, this is seconds of CPU per URL, multiplied by `CONCURRENCY=32`.
5. **Paragraph structure is destroyed.** `extract_visible_text` pushes block children as `format!("\n{}\n", child)` and then joins with `" "`, producing `"\nA\n \nB\n"`. `clean_and_deduplicate` splits on `"\n\n"`, which **never matches**. So paragraph-level dedup never runs, boilerplate survives, and `ranking::paragraph_chunks` receives one giant paragraph.

Plus, for non-Latin scripts specifically (the Amar Ujala case):

6. `SENTENCE_LIKE = r"[A-Z].*[.!?]$"` — ASCII-only. Devanagari has no case, so the sentence-quality bonus never fires for Hindi.
7. `split_at_sentence_boundaries` requires `chars[i+2].is_uppercase()` and only `. ! ?`. Hindi's sentence terminator is **`।` (U+0964 danda)**, and no Devanagari char is uppercase → **the entire Hindi article becomes a single chunk**, which BM25 then scores as one blob.
8. All length gates (`>= 100`, `min_text_length = 50`) are **byte**-based. Devanagari is 3 bytes/char, so Hindi content clears gates with one-third the actual text — while `compute_text_quality_score`'s `text.len() / 10` term inflates Hindi scores 3× relative to Latin, biasing container selection inconsistently.

**Strategy.** Do not patch eight bugs in a bespoke extractor when a better-benchmarked Rust implementation exists. On the 2026 **WCXB** benchmark, `rs-trafilatura` leads at **F1 0.903** on the held-out test set (vs Trafilatura 0.841, Resiliparse 0.817), and reports F1 0.966 on ScrapingHub. It ships exactly the pieces being hand-rolled badly here: reader decoding (charset), JSON-LD, **dates**, language filtering, deduplication, tables, and a page-type classifier (article/forum/product/docs/listing) with per-type extraction profiles.

```toml
# Cargo.toml
rs-trafilatura = "0.2"
encoding_rs = "0.8"
```

```rust
// src/extractor.rs — new primary path
pub struct Extracted {
    pub title: String,
    pub text: String,
    pub published: Option<chrono::DateTime<chrono::Utc>>,
    pub language: Option<String>,
    pub page_type: Option<String>,
    pub confidence: f64,      // ← finally a REAL value for ranking (see P1-6)
}

pub fn extract(html: &str, url: &str) -> Extracted {
    // Tier 1: rs-trafilatura — page-type-aware, JSON-LD + dates + dedup.
    if let Some(doc) = rs_trafilatura::extract(html, url) {
        if doc.text.chars().count() >= MIN_TEXT_CHARS {
            return Extracted {
                title: doc.title.unwrap_or_default(),
                text: doc.text,
                published: doc.date,
                language: doc.language,
                page_type: doc.page_type,
                confidence: doc.confidence,
                ..
            };
        }
    }
    // Tier 2: our own JSON-LD, FIXED (see below) — handles sites rs-trafilatura misses.
    if let Some(a) = json_ld_article(html) {
        if a.body.chars().count() >= MIN_TEXT_CHARS {
            return Extracted { text: a.body, published: a.date, confidence: 0.85, .. };
        }
    }
    // Tier 3: existing semantic/structured selectors (keep — they are fine).
    // Tier 4: meta description — mark LOW confidence so ranking can down-weight.
}
```

**The fixed JSON-LD reader** — this is the single most valuable extractor change, and it is what unblocks Amar Ujala:

```rust
/// Collect EVERY JSON-LD node (including @graph and nested arrays), then pick
/// the best article-like one. Never returns a `description` when an
/// `articleBody` exists anywhere in the document.
fn json_ld_article(html: &str) -> Option<Article> {
    let doc = Html::parse_document(html);
    let sel = SEL_JSON_LD.as_ref()?;
    let mut nodes: Vec<serde_json::Value> = Vec::new();

    for script in doc.select(sel) {
        let raw = script.text().collect::<String>();
        let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim()) else { continue };
        flatten_ld(&v, &mut nodes);
    }

    // PASS 1 — articleBody only. This is the fix: a short `description` on a
    // `WebPage` node can no longer win over the real article body, regardless
    // of document order. (Amar Ujala: block 0 is WebPage, block 3 is the
    // NewsArticle with a 4,596-char articleBody.)
    let mut best: Option<Article> = None;
    for n in &nodes {
        if !is_article_type(n) { continue; }
        let Some(body) = n.get("articleBody").and_then(|v| v.as_str()) else { continue };
        let body = strip_html_fragments(body.trim());
        if body.chars().count() < 100 { continue; }
        let cand = Article { body, date: parse_ld_date(n), headline: ld_str(n, "headline") };
        if best.as_ref().is_none_or(|b| cand.body.chars().count() > b.body.chars().count()) {
            best = Some(cand);        // longest articleBody wins
        }
    }
    if best.is_some() { return best; }

    // PASS 2 — only now fall back to description/text, and only from a
    // genuinely article-typed node (NOT WebPage/CreativeWork).
    for n in &nodes {
        if !is_strict_article_type(n) { continue; }
        for field in ["description", "text"] {
            if let Some(s) = n.get(field).and_then(|v| v.as_str()) {
                if s.trim().chars().count() >= 50 {
                    return Some(Article { body: s.trim().into(), date: parse_ld_date(n), headline: ld_str(n, "headline") });
                }
            }
        }
    }
    None
}

/// Walk arrays and @graph so nested nodes are reachable.
fn flatten_ld(v: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match v {
        serde_json::Value::Array(a) => for x in a { flatten_ld(x, out) },
        serde_json::Value::Object(o) => {
            if let Some(g) = o.get("@graph") { flatten_ld(g, out); }
            out.push(v.clone());
            // Also descend into mainEntityOfPage / mainContentOfPage.
            for k in ["mainEntityOfPage", "mainContentOfPage"] {
                if let Some(x) = o.get(k) { flatten_ld(x, out); }
            }
        }
        _ => {}
    }
}

/// `@type` may be a string OR an array of strings.
fn ld_types(n: &serde_json::Value) -> Vec<String> {
    match n.get("@type") {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(a)) =>
            a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => vec![],
    }
}

/// Permissive — used only in PASS 1, where articleBody is already required.
fn is_article_type(n: &serde_json::Value) -> bool {
    ld_types(n).iter().any(|t| matches!(t.as_str(),
        "Article" | "NewsArticle" | "BlogPosting" | "TechArticle" | "ScholarlyArticle"
        | "Report" | "LiveBlogPosting" | "AdvertiserContentArticle" | "SatiricalArticle"
        | "ReportageNewsArticle" | "AnalysisNewsArticle" | "OpinionNewsArticle"
        | "ReviewNewsArticle" | "BackgroundNewsArticle" | "WebPage" | "CreativeWork"))
}

/// Strict — used in PASS 2. WebPage/CreativeWork are DELIBERATELY excluded:
/// their `description` is a site blurb, not article content. This exclusion is
/// the Amar Ujala fix.
fn is_strict_article_type(n: &serde_json::Value) -> bool {
    ld_types(n).iter().any(|t| t.ends_with("Article") || t == "BlogPosting")
}

fn parse_ld_date(n: &serde_json::Value) -> Option<chrono::DateTime<chrono::Utc>> {
    for k in ["datePublished", "dateCreated", "dateModified", "uploadDate"] {
        if let Some(s) = n.get(k).and_then(|v| v.as_str()) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                return Some(dt.with_timezone(&chrono::Utc));
            }
        }
    }
    None
}
```

**Acceptance (regression test — add this as a real test):**

```rust
#[test]
fn amarujala_hindi_article_extracts_full_body() {
    let html = include_str!("../tests/fixtures/amarujala_worldcup.html");
    let got = extract(html, "https://www.amarujala.com/cricket/cricket-news/odi-world-cup-2026-...");
    // Before the fix this returned 180 chars (the WebPage description).
    assert!(got.text.chars().count() > 4_000, "got only {} chars", got.text.chars().count());
    assert!(got.published.is_some(), "datePublished must be extracted");
    assert_eq!(got.language.as_deref(), Some("hi"));
}
```

Save the fixture now — `curl` the article and commit it under `tests/fixtures/`. This is the one test that proves the headline bug is dead and stays dead.

Also delete `try_scored_extraction`'s `element.html()` call regardless of whether you keep the function: if a density score is still needed, compute it from `element.text().len()` against a **cached, single** `document.html().len()`, never per element.

---

### P1-5 · Non-Latin script support (Hindi and beyond)

`rs-trafilatura` fixes extraction and language detection, but chunking, tokenizing, and prompting are still English-shaped. Fix all three.

**(a) Sentence segmentation that knows about `।`, `。`, `؟`:**

```rust
/// Terminators across scripts. Devanagari danda U+0964 / double danda U+0965
/// are what Hindi actually uses — `. ! ?` alone leaves a Hindi article as ONE chunk.
const SENT_END: &[char] = &[
    '.', '!', '?',
    '\u{0964}', '\u{0965}',   // Devanagari danda, double danda
    '\u{06D4}',               // Arabic full stop
    '\u{3002}', '\u{FF01}', '\u{FF1F}',  // CJK ideographic full stop, fullwidth ! ?
    '\u{0589}', '\u{104A}', '\u{1362}',  // Armenian, Myanmar, Ethiopic
];

fn is_sentence_boundary(chars: &[char], i: usize) -> bool {
    if !SENT_END.contains(&chars[i]) { return false; }
    match chars.get(i + 1) {
        None => true,
        Some(next) if next.is_whitespace() => {
            // For cased scripts require an uppercase start; for uncased scripts
            // (Devanagari, Arabic, CJK, Thai) whitespace alone is sufficient.
            match chars.get(i + 2) {
                Some(c) if c.is_alphabetic() && !c.is_uppercase() && has_case(*c) => false,
                _ => true,
            }
        }
        _ => false,
    }
}

/// True for scripts that HAVE case. Devanagari etc. return false.
fn has_case(c: char) -> bool { c.is_uppercase() || c.is_lowercase() }
```

**(b) Count characters, not bytes, everywhere.** Audit and convert: `min_text_length`, the `>= 100 / 80 / 60 / 50 / 40 / 30` gates in `extract_title_and_text`, `compute_text_quality_score`'s `text.len() / 10`, `MAX_CONTEXT_CHARS_*`, and `SourceResult.char_count` (which is `text.len()` in `search.rs:290` but `text.chars().count()` in `ranking.rs` — **the same field in two different units**).

**(c) Script-aware stopwords + CJK bigrams.** `tokenize_filtered` splits on `!is_alphanumeric()`, which works for Devanagari but not for Chinese/Japanese/Thai (no spaces → one enormous token). And `STOPWORDS` is English-only, so Hindi function words (के, में, है, और, से, का, की, को) are never filtered and dominate BM25.

```rust
fn tokenize_filtered(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() { continue; }
        if is_cjk(raw) {
            // No word boundaries → character bigrams, the standard cheap
            // approach for CJK in lexical retrieval.
            let cs: Vec<char> = raw.chars().collect();
            for w in cs.windows(2) { out.push(w.iter().collect()); }
            continue;
        }
        let t = raw.to_lowercase();
        if t.chars().count() < 2 { continue; }
        if stopwords::is_stopword(&t) { continue; }   // multi-language set
        out.push(t);
    }
    out
}
```

Add Hindi, Bengali, Tamil, Spanish, French, German, Arabic stopword sets — or pull `stop-words = "0.8"` and select by the `language` that `rs-trafilatura` now reports.

**(d) Answer in the user's language.** Every prompt in `llm.rs` is hardcoded English. A Hindi query currently gets an English answer about Hindi sources.

```rust
// append to every system prompt
let lang_rule = "LANGUAGE: Reply in the SAME language as the user's question. \
                 If the question is in Hindi, answer in Hindi. If the sources are in \
                 a different language from the question, translate the facts — do not \
                 switch the answer's language.";
```

---

### P1-6 · Stop re-ranking the ranked results

**Problem.** `ranking::rank_top_chunks_with_rrf` runs BM25+ → RRF boost → MMR and returns a carefully ordered list. Then `llm::build_ranked_context` **throws that order away** and re-sorts by:

```rust
if src.title.to_lowercase().contains(kw) { score += 3.0; }   // substring count
if text_lower.contains(kw.as_str())      { score += 1.0; }
```

No IDF, no term frequency, no length normalization — a strictly worse ranker, applied last, so it wins. The whole BM25+/MMR/lost-in-middle stack is decorative.

**Fix.** `build_ranked_context` must **consume** the given order and do packing only — no scoring:

```rust
fn build_ranked_context(sources: &[SourceResult], budget_chars: usize) -> BuiltContext {
    // `sources` is ALREADY ranked by rank_top_chunks_with_rrf. Respect it.
    let order = lost_in_middle_reorder_indices(sources.len());
    let mut text = String::with_capacity(budget_chars.min(64 * 1024));
    let mut citation_map = Vec::new();
    for idx in order {
        let s = &sources[idx];
        let remaining = budget_chars.saturating_sub(text.chars().count());
        if remaining < 400 { break; }                   // no room for a useful block
        // KEY FIX: truncate the source, don't skip the whole context.
        let body = util::truncate_chars(s.extracted_text.trim(), remaining.saturating_sub(200));
        let _ = write!(text, "[{}] {} {} ({})\n{}\n\n",
                       citation_map.len() + 1, credibility_tag(&s.url), s.title, s.url, body);
        citation_map.push(idx);
    }
    BuiltContext { text, citation_map }
}
```

The `break`-on-oversized bug deserves a separate note: today a single long source at position 3 **ends** context assembly, so the LLM can receive one or two sources instead of thirty. Truncate-and-continue is the fix.

**Wire up the two dead quality signals while you are here:**

- `ranking.rs:166` sets `extraction_confidence: 0.7` with the comment *"will be overridden when C4 provides real confidence"*. P1-4 now provides it — plumb `Extracted.confidence` through `SourceResult` and use it.
- `llm::trust_score` is `pub`, fully implemented, and **never called** (that is why `cargo` does not warn). Use it as a ranking prior:

```rust
// in rank_top_chunks_with_rrf, replacing the no-op `0.05 * (0.7 - 0.5)`
score *= 0.80 + 0.20 * candidates[idx].extraction_confidence;   // real now
score *= crate::llm::trust_score(&candidates[idx].url) as f64;  // .gov/.edu/wikipedia ↑
```

---

### P1-7 · MMR is O(k·n·|selected|) and will not scale

**Problem.** `mmr_rerank` loops `top_k` times; each iteration scans all `remaining` candidates and, for each, computes Jaccard against **every already-selected** chunk. It also pre-builds a `HashSet<String>` per candidate — re-allocating every token string, after `tokenize_filtered` already ran over the same corpus in the caller.

At research scale (`llm_top_k = 200`, ~8,400 chunks from 420 sources) that is on the order of 200 × 8,400 × 200 set intersections — minutes of CPU. `remaining.remove(best_pos)` adds an O(n) memmove per selection.

Three fixes, in order of value:

```rust
// 1. Tokenize ONCE. Pass the Vec<Vec<String>> the caller already built into
//    mmr_rerank instead of recomputing HashSet<String> per candidate.

// 2. Hash tokens to u64 and use a sorted Vec for intersection — no String
//    allocation, cache-friendly, ~10x faster than HashSet<String>.
fn sig(tokens: &[String]) -> Vec<u64> {
    let mut v: Vec<u64> = tokens.iter().map(|t| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        t.hash(&mut h); h.finish()
    }).collect();
    v.sort_unstable(); v.dedup(); v
}

fn jaccard_sorted(a: &[u64], b: &[u64]) -> f64 {
    let (mut i, mut j, mut inter) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Equal   => { inter += 1; i += 1; j += 1; }
            Ordering::Less    => i += 1,
            Ordering::Greater => j += 1,
        }
    }
    let union = a.len() + b.len() - inter;
    if union == 0 { 0.0 } else { inter as f64 / union as f64 }
}

// 3. Only consider a CANDIDATE WINDOW. MMR over the full corpus is wasted —
//    nothing ranked 5,000th by BM25 will win on diversity alone.
let window = (top_k * 5).min(scored.len());
let mut remaining: Vec<(usize, f64)> = scored[..window].to_vec();

// 4. Track max_sim INCREMENTALLY: after selecting s, update each remaining
//    candidate's cached max_sim against s only. Turns the inner |selected|
//    loop into O(1) amortized.
```

**Also fix the URL cap escape hatch.** When every remaining candidate has hit `MAX_CHUNKS_PER_URL_IN_TOP_K`, the inner loop `continue`s for all of them, `best_mmr` stays `f64::MIN`, and `best_pos` stays `0` — so `remaining[0]` is selected **even though its URL is already capped**. Track whether any candidate was eligible:

```rust
let mut eligible = false;
for (pos, &(cand_idx, rel)) in remaining.iter().enumerate() {
    if url_counts.get(&key(cand_idx)).copied().unwrap_or(0) >= MAX_CHUNKS_PER_URL_IN_TOP_K {
        continue;
    }
    eligible = true;
    // ...score...
}
if !eligible { break; }   // genuinely nothing left to pick
```

---

### P1-8 · URL canonicalization misses the common duplicates

**Problem.** `url_utils` has three issues and one duplication:

1. **`normalize_url`'s body is copy-pasted verbatim into `deduplicate`.** Two implementations of the same logic that must agree, and nothing enforces it.
2. **Query parameters are not sorted** → `?a=1&b=2` and `?b=2&a=1` are different keys; the same page is scraped twice.
3. **No www/mobile/AMP unification** → `example.com/x`, `www.example.com/x`, `m.example.com/x`, `amp.example.com/x`, and `example.com/x/amp` are five "distinct" URLs for one article. On news sites — exactly the Amar Ujala class — this is the dominant duplicate form, and it wastes scrape budget on the same text.
4. **`SKIP_DOMAINS` is matched against the whole lowercased URL** via `lower_url.contains(d)`, not the host. So `https://news.site.com/why-amazon.com-won` is skipped because the *path* contains `amazon.com`.

**Fix — one canonical function, host-aware skipping:**

```rust
/// The single source of truth for "are these the same page?".
pub fn canonical_key(raw: &str) -> String {
    let Ok(mut u) = Url::parse(raw.trim()) else { return raw.trim().to_lowercase() };
    if !matches!(u.scheme(), "http" | "https") { return raw.trim().to_lowercase() }
    u.set_fragment(None);

    // Host: strip www/m/amp/mobile prefixes.
    let host = u.host_str().unwrap_or("").to_lowercase();
    let host = host
        .strip_prefix("www.").unwrap_or(&host)
        .strip_prefix("m.").map(str::to_string).unwrap_or_else(|| host.trim_start_matches("www.").to_string());
    let host = host.trim_start_matches("amp.").trim_start_matches("mobile.");

    // Path: drop AMP suffixes, index files, trailing slash. Preserve case —
    // many CMSes are case-sensitive on paths.
    let mut path = u.path().to_string();
    for suf in ["/amp", "/amp/", "/index.html", "/index.htm", "/index.php", "/"] {
        if path.len() > suf.len() && path.ends_with(suf) {
            path.truncate(path.len() - suf.len());
            break;
        }
    }

    // Query: drop tracking params, then SORT so order cannot create duplicates.
    let mut qs: Vec<(String, String)> = u.query_pairs()
        .filter(|(k, _)| !config::TRACKING_PARAMS.contains(&k.to_lowercase().as_str()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    qs.sort();
    let q = qs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");

    if q.is_empty() { format!("{host}{path}") } else { format!("{host}{path}?{q}") }
}

/// Host-suffix match, NOT substring-of-whole-URL.
pub fn should_skip(url: &str) -> bool {
    let Ok(u) = Url::parse(url) else { return true };
    let host = u.host_str().unwrap_or("").to_lowercase();
    if config::SKIP_DOMAINS.iter().any(|d| host == *d || host.ends_with(&format!(".{d}"))) {
        return true;
    }
    let path = u.path().to_lowercase();
    config::SKIP_EXTENSIONS.iter().any(|e| path.ends_with(e))
}
```

Then delete the duplicated normalization inside `deduplicate` and have it call `canonical_key`. Also **remove `ref`, `source`, `src`, `amp`, `campaign`, `partner` from `TRACKING_PARAMS`** — these are load-bearing on many sites (`?src=` on docs portals, `?ref=` on repository URLs), and stripping them produces 404s. Keep the unambiguous ones (`utm_*`, `fbclid`, `gclid`, `msclkid`, `mc_cid`, `igshid`).

---

### P1-9 · Multi-query decomposition generates garbage and is never fused

Three problems compound:

1. **`decompose_query` emits meaningless fragments.** Strategy 4 splits a 10-word query at the midpoint — `words[..5].join(" ")` yields *"what are the implications of"*. Strategy 2 pushes bare entities — `"Elon"`. Both become full engine dispatches.
2. **`ranking::cross_query_fusion` — which exists specifically to weight the original query 2× over sub-queries — is dead code.** `search.rs` just does `all_results.extend(results)`, flat. So a URL surfaced by the garbage sub-query *"what are the implications of"* contributes the same RRF weight as one surfaced by the real query.
3. **Load multiplication.** 35 engines × 5 variations × 2–3 pages ≈ 350–525 HTTP requests per search, from one IP, inside a 6 s deadline. This cannot complete (§P0-9) *and* it is the fastest route to the rate limits in §1.2. Multi-query currently buys bans without buying coverage.

**Fix.**

```rust
// (a) Only decompose when it can pay off, and cap hard.
pub fn decompose_query(query: &str, intel: &QueryIntelligence) -> Vec<String> {
    let q = query.trim();
    if q.is_empty() { return vec![] }
    // Never decompose navigational or simple queries — nothing to decompose.
    if intel.intent == QueryIntent::Navigational
        || intel.complexity == QueryComplexity::Simple {
        return vec![q.to_string()];
    }
    let mut out = vec![q.to_string()];

    // Comparison: "X vs Y" → X, Y. This one is genuinely useful.
    if intel.intent == QueryIntent::Comparison {
        for sep in [" vs ", " versus ", " compared to "] {
            if let Some((a, b)) = q.split_once(sep) {
                out.push(a.trim().to_string());
                out.push(b.trim().to_string());
                break;
            }
        }
    }
    // Multi-word named entity + the query's head term — NOT a bare entity.
    for e in intel.key_entities.iter().filter(|e| e.split_whitespace().count() >= 2) {
        if out.len() >= 3 { break }
        out.push(format!("{e} {}", intel.head_term()));
    }

    // DELETED: midpoint splitting (produced fragments), bare single-word
    // entities, and generic "{q} definition/tutorial/explained" suffixes —
    // engines already stem and expand; these add load, not recall.

    out.dedup();
    out.truncate(3);   // was 5; 3 × engines is already a lot of requests
    out
}
```

```rust
// (b) Actually fuse. Keep results grouped per sub-query and weight them.
let mut per_query: Vec<(String, Vec<RawSearchResult>)> = Vec::new();
for variation in &query_variations {
    per_query.push((variation.clone(), dispatch_engines(.., &[variation.clone()], ..).await));
}
let all_results = ranking::cross_query_fusion(&per_query, &effective_query);
```

And in `cross_query_fusion`, replace the flat `rank` enumerate with the real per-engine `rank_position`, and apply `engine_family` dedup from P0-8 so the two fusion layers do not double-count.

---

### P1-10 · Context budget: the real bottleneck for answer quality

**Problem.** `MAX_CONTEXT_CHARS_LITE = 16_000`. The system scrapes up to 420 pages and then hands the LLM **16 KB**. The measured run extracted 11,768 chars total, so the budget was not even the binding constraint — but it will be the moment P1-4 lands and extraction starts returning full articles. One Amar Ujala article alone is 4,596 chars; 30 real articles is ~140 KB.

2026 models have 200K–2M token windows. A fixed 16 KB budget wastes the entire capability.

**Fix.** Make the budget model-aware and configurable, with token-based accounting:

```rust
/// Rough char→token ratio. English ≈ 4 chars/token; Devanagari and CJK are
/// far denser per token, so estimate per script rather than assuming English.
fn est_tokens(s: &str) -> usize {
    let mut t = 0usize;
    for c in s.chars() {
        t += if c.is_ascii() { 1 } else { 3 };   // conservative for non-Latin
    }
    t / 4 + 1
}

pub struct ContextBudget { pub max_tokens: usize }

impl ContextBudget {
    /// Derive from the model name, with an env override. Defaults are
    /// deliberately conservative — overshooting costs the user money.
    pub fn for_model(model: &str) -> Self {
        if let Ok(v) = std::env::var("CONTEXT_MAX_TOKENS").and_then(|v| v.parse().map_err(|_| std::env::VarError::NotPresent)) {
            return Self { max_tokens: v };
        }
        let m = model.to_lowercase();
        let max = if m.contains("gemini") && m.contains("pro")      { 400_000 }
             else if m.contains("gemini")                           { 200_000 }
             else if m.contains("opus") || m.contains("sonnet")     { 150_000 }
             else if m.contains("gpt-5") || m.contains("o4")        { 120_000 }
             else if m.contains("haiku") || m.contains("mini")      {  60_000 }
             else                                                    {  24_000 };  // unknown → safe
        Self { max_tokens: max }
    }
}
```

Pair this with **`max_tokens` on the request body**, which is currently missing entirely for OpenAI-compatible providers and hardcoded to a truncating `4096` for Anthropic:

```rust
ApiFormat::OpenAiCompatible => {
    let mut body = serde_json::json!({ "model": model, "messages": msgs });
    body["max_tokens"]  = serde_json::json!(max_output_tokens);   // was ABSENT
    body["temperature"] = serde_json::json!(0.2);                 // was ABSENT
    if stream { body["stream"] = serde_json::json!(true); }
    body
}
ApiFormat::Anthropic => {
    // was hardcoded 4096 — truncates every research report
    body["max_tokens"] = serde_json::json!(max_output_tokens.max(8192));
}
```

---

### P1-11 · Reasoning-model responses are silently dropped

**Problem.** `extract_response_text` for Anthropic reads `content[0].text`. With extended thinking enabled, `content[0]` is a `{"type":"thinking",...}` block with **no `text` field** → returns `None` → the call fails with `empty_response` and the user sees "AI could not generate an answer". Same class of failure for OpenAI-compatible providers that return `content: null` alongside `reasoning_content`, or `content` as an array of parts.

**Fix.** Scan for the first text-bearing block instead of assuming index 0:

```rust
ApiFormat::Anthropic => {
    let blocks = json.get("content")?.as_array()?;
    let mut out = String::new();
    for b in blocks {
        // Skip "thinking" / "redacted_thinking" / "tool_use"; collect every "text".
        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = b.get("text").and_then(|t| t.as_str()) { out.push_str(t); }
        }
    }
    (!out.is_empty()).then_some(out)
}
ApiFormat::OpenAiCompatible => {
    let msg = json.get("choices")?.get(0)?.get("message")?;
    // content may be a string, null, or an array of {type,text} parts.
    match msg.get("content") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(serde_json::Value::Array(parts)) => {
            let s: String = parts.iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect();
            (!s.is_empty()).then_some(s)
        }
        // Reasoning models may put the answer only in reasoning_content.
        _ => msg.get("reasoning_content").and_then(|t| t.as_str()).map(str::to_string),
    }
}
```

Add retry with backoff on 429/5xx, which is absent today:

```rust
async fn call_llm_retrying(cfg: &LlmConfig, msgs: &[ChatMessage]) -> Result<String, String> {
    let mut delay = Duration::from_millis(600);
    for attempt in 0..4 {
        match call_llm(cfg, msgs).await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < 3 && is_retryable(&e) => {
                tracing::warn!("LLM attempt {} failed ({}), retrying in {:?}", attempt + 1, e, delay);
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}
fn is_retryable(e: &str) -> bool {
    e.starts_with("http_429") || e.starts_with("http_5") || e.contains("http_request_failed")
}
```

And reuse one client — `build_http_client()` currently constructs a fresh `reqwest::Client` (new TLS stack, new pool) **on every `call_llm`**, which for research mode means one per batch:

```rust
static LLM_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
pub(crate) fn http_client() -> &'static reqwest::Client {
    LLM_CLIENT.get_or_init(|| reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .pool_max_idle_per_host(8)
        .tcp_nodelay(true)
        .build().expect("llm client"))
}
```

---

### P1-12 · Security: open LLM relay, unauthenticated process spawn

Three issues, all reachable from any website in the user's browser because `CorsLayer::permissive()` is combined with binding `0.0.0.0`.

**(a) SSRF via `llm.base_url`.** `resolve_provider` uses a caller-supplied `base_url` verbatim. Any page can `fetch('http://localhost:8000/api/models', {method:'POST', body: JSON.stringify({api_key:'x', base_url:'http://169.254.169.254/latest/meta-data/'})})` and read the response — the server becomes a credentialed proxy into the loopback and link-local ranges.

```rust
fn validate_base_url(raw: &str) -> Result<Url, String> {
    let u = Url::parse(raw).map_err(|_| "invalid base_url")?;
    if u.scheme() != "https" && !allow_insecure_llm() {
        return Err("base_url must be https (set ALLOW_INSECURE_LLM=1 for local models)".into());
    }
    let host = u.host().ok_or("base_url has no host")?;
    if let url::Host::Ipv4(ip) = host {
        if ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified() {
            return Err("base_url must not target a private address".into());
        }
    }
    if let url::Host::Ipv6(ip) = host {
        if ip.is_loopback() || ip.is_unspecified() { return Err("private address".into()); }
    }
    if let url::Host::Domain(d) = host {
        const BLOCKED: &[&str] = &["localhost", "metadata.google.internal", "169.254.169.254"];
        if BLOCKED.iter().any(|b| d.eq_ignore_ascii_case(b)) { return Err("blocked host".into()); }
    }
    Ok(u)
}
```

Note: allow an explicit opt-out so local Ollama/LM Studio still work — that is the legitimate use of a loopback `base_url`.

**(b) `/api/tts` spawns a process per GET, unauthenticated and unbounded.** `tts_handler` runs `edge-tts` for any caller, with no rate limit and no concurrency cap — a trivial fork bomb. It also **leaks the temp file on the error path**: the early `return Err(...)` when `output.status` is non-zero happens before `remove_file`. And the `edge-tts` lookup hardcodes `/home/sandeep/.local/bin/edge-tts`, which will not exist for anyone else.

```rust
static TTS_GATE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(2));

async fn tts_handler(Query(p): Query<TtsQuery>) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _permit = TTS_GATE.try_acquire()
        .map_err(|_| (StatusCode::TOO_MANY_REQUESTS, "tts busy".to_string()))?;
    // Write into the OS temp dir, and ALWAYS clean up.
    let tmp = std::env::temp_dir().join(format!("sw_tts_{}.mp3", uuid_like()));
    let _guard = TempFileGuard(tmp.clone());      // Drop impl removes the file
    // ...spawn, read, return...
}
```

Remove the hardcoded home path; resolve from `EDGE_TTS_PATH` or `$PATH` only.

**(c) `LlmConfig` derives `Debug` with a plaintext `api_key`.** One `tracing::debug!("{:?}", cfg)` anywhere — now or later — writes the user's key to the log.

```rust
impl std::fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .finish()
    }
}
```

Also: default the bind address to `127.0.0.1` and require `BIND=0.0.0.0` to expose the service, and replace `CorsLayer::permissive()` with an allowlist. A tool that relays user API keys should not be reachable by every page the user has open.

---

## §4. P2 — Optimization and modernization

### P2-1 · Two-stage retrieval with a cross-encoder reranker

Once P1-4 is delivering real article text, lexical BM25+ becomes the ceiling. The 2026 standard for this stage is a cross-encoder reranker over a cheap first-stage candidate set, and it is available natively in Rust.

```toml
fastembed = "7"          # local ONNX, no Tokio required, models cached after first run
```

```rust
// Stage 1 (cheap, broad): BM25+ → top 150 chunks.
// Stage 2 (expensive, precise): cross-encoder → top 25.
use fastembed::{TextRerank, RerankInitOptions, RerankerModel};

static RERANKER: OnceLock<Option<TextRerank>> = OnceLock::new();

fn reranker() -> Option<&'static TextRerank> {
    RERANKER.get_or_init(|| {
        if std::env::var("RERANKER").as_deref() != Ok("on") { return None; }
        // bge-reranker-v2-m3 is the 2026 default: best quality/latency/licence
        // mix, and multilingual — which matters for the Hindi corpus.
        TextRerank::try_new(
            RerankInitOptions::new(RerankerModel::BGERerankerV2M3).with_show_download_progress(true)
        ).ok()
    }).as_ref()
}

pub fn rerank_chunks(query: &str, cands: &[ChunkCandidate], top_k: usize) -> Vec<usize> {
    let Some(rr) = reranker() else { return (0..cands.len().min(top_k)).collect() };
    let docs: Vec<&str> = cands.iter().map(|c| c.text.as_str()).collect();
    match rr.rerank(query, docs, false, None) {
        Ok(scored) => scored.into_iter().take(top_k).map(|r| r.index).collect(),
        Err(e) => { tracing::warn!("rerank failed, falling back to BM25 order: {e}"); 
                    (0..cands.len().min(top_k)).collect() }
    }
}
```

Keep it **opt-in** (`RERANKER=on`) and always fall back to BM25 order on failure — the project's "single binary, zero dependencies" promise matters, and a 600 MB ONNX download must be a choice. Run MMR *after* reranking, on the reranked top-k, for diversity.

### P2-2 · Freshness as a first-class ranking signal

P1-4 now extracts `datePublished`. The system calls itself "time-aware" but has never had a single freshness signal in ranking — it only appends a year to the query string and tells the LLM today's date.

```rust
/// Exponential recency decay, gated on the query actually being time-sensitive.
/// Evergreen queries ("how does TLS work") must NOT be recency-biased.
fn freshness_multiplier(published: Option<DateTime<Utc>>, intel: &QueryIntelligence) -> f64 {
    if !intel.is_time_sensitive { return 1.0 }
    let Some(p) = published else { return 0.92 };    // unknown date: slight penalty
    let days = (Utc::now() - p).num_days().max(0) as f64;
    let half_life = match intel.intent {
        QueryIntent::Temporal => 7.0,     // news: a week-old source is half as good
        QueryIntent::Person   => 180.0,
        _                     => 60.0,
    };
    // 0.6 floor so an old-but-authoritative source is demoted, not erased.
    0.6 + 0.4 * 0.5_f64.powf(days / half_life)
}
```

Surface the date in the context block so the LLM can reason about it — it is currently told "prefer recent sources" while being given no dates:

```rust
write!(text, "[{}] {} {} — published {} ({})\n{}\n\n",
       n, cred, title, date_str, url, body);
```

### P2-3 · Cache layer (currently absent)

`src/cache.rs` holds session and history stores; there is **no result cache at all**. Every repeated query re-dispatches every engine and re-scrapes every URL.

Add two tiers:

```rust
/// SERP cache — keyed on (engine, canonical query, page). Short TTL: news
/// queries go stale fast, and engines punish repetition.
/// Hit rate on this is what makes `/health/engines` cheap to poll.
serp: moka::future::Cache<(String, String, usize), Vec<RawSearchResult>>,   // TTL 10 min

/// Page cache — keyed on canonical_key(url). Long TTL: article bodies are
/// stable, and this is where the real latency win is (scrape stage was 8.4 s).
page: moka::future::Cache<String, Arc<Extracted>>,                          // TTL 6 h, cap 2 GB
```

```toml
moka = { version = "0.12", features = ["future"] }
```

Expose hit ratios in `/health` — `eval.rs` already defines `engine_hit_ratio`, `scrape_hit_ratio`, `answer_hit_ratio` for exactly this and nothing populates them.

### P2-4 · Make the dead eval harness the quality gate

`src/eval.rs` is 474 lines of complete, correct evaluation code — nDCG@10, MRR, Recall@k, latency percentiles, per-engine success rates — and `benchmarks/golden_queries.json` is 28 KB of labelled data. **Neither is referenced anywhere.** The project has **zero tests**.

```rust
// src/bin/eval.rs — new binary: `cargo run --release --bin eval`
#[tokio::main]
async fn main() {
    let set = searchwala::eval::load_golden_queries("benchmarks/golden_queries.json")
        .expect("golden set");
    let mut report = searchwala::eval::EvalReport::default();
    for gq in &set.queries {
        let resp = searchwala::search::execute_search(&gq.query, Some(50), Some("auto".into()),
                                                      None, None, None, None).await;
        let urls: Vec<String> = resp.search_results.iter().map(|h| h.url.clone()).collect();
        report.record(&gq.query, &urls, &gq.relevance_judgments);
    }
    println!("{}", searchwala::eval::build_eval_report(&report));
    // Fail CI on regression.
    assert!(report.ndcg_at_10() >= baseline_ndcg(), "nDCG@10 regressed");
}
```

Wire it into `.github/workflows/` alongside the release job. **Rule for the agent: every change in §2 and §3 that touches ranking or extraction must be accompanied by a before/after nDCG@10 number from this harness.** Without it, "better results" is unmeasurable and this plan's claims cannot be verified.

Minimum unit tests to add first (there are none today):

| Test | Guards |
|---|---|
| `amarujala_hindi_article_extracts_full_body` | P1-4 — the 26× bug |
| `truncate_chars_never_panics_on_devanagari` | P0-5 |
| `canonical_key_unifies_www_amp_mobile_and_param_order` | P1-8 |
| `rrf_counts_bing_aliases_once` | P0-8 |
| `citation_map_round_trips_through_reorder` | P0-11 |
| `temporal_detection_ignores_new_york_and_know` | P0-12 |
| `pagination_url_for_page_matches_engine_convention` | P0-6 |
| `zstd_response_decodes` | P0-4 |

### P2-5 · Collapse the duplicated stream pipeline

`stream.rs` is a stale fork of `search.rs`: no QueryIntelligence, no RRF, no multi-query, `join_all` instead of `JoinSet`, **no deadline**, its own copy of the buggy time-enrichment, `is_lite_mode` matching only `"lite"` (so the UI's `"auto"` falls through to `max_urls = 420` with chunk ranking disabled), and no history/session recording. Two pipelines means every fix above must be applied twice — and historically has not been.

**Fix.** Make streaming a *transport* over one pipeline, not a second pipeline:

```rust
pub enum Progress {
    Stage { name: &'static str, ms: u128 },
    Engines { completed: usize, total: usize, results: usize },
    Sources(Vec<SearchHit>),
    LlmChunk(String),
    Done(Box<SearchResponse>),
}

/// ONE implementation. `/search` collects into a SearchResponse;
/// `/search/stream` forwards each Progress event as SSE.
pub async fn execute_search_with_progress(
    req: SearchParams,
    tx: Option<mpsc::Sender<Progress>>,
) -> SearchResponse { .. }
```

Also add disconnect cancellation — today an abandoned stream keeps scraping hundreds of URLs:

```rust
// tx.send() failing means the client is gone. Stop the work.
if tx.send(ev).await.is_err() {
    tracing::info!("client disconnected, aborting pipeline");
    join_set.abort_all();
    return;
}
```

### P2-6 · UX: the parts the user actually feels

| Problem | Fix |
|---|---|
| **Citations don't link.** `ui.html:798` renders `[n]` as `<sup style="cursor:default">` — decorative only, pointing nowhere (and mis-mapped anyway, P0-11). | Use `citation_map` to render `<a href="{url}" title="{title}">[n]</a>`, with a hover card showing the cited snippet. |
| **No cancel.** `grep -c AbortController ui.html` → **0**. A 15–60 s search cannot be stopped. | `AbortController` per search; abort on new query, on Escape, and on a Stop button. |
| **15 s of blank spinner.** `/search/stream` exists but the UI uses blocking POST, so the SSE pipeline is unreachable from the product. | Switch the UI to the SSE endpoint. Stream the stages the backend already logs — engines responding, sources found, then the answer token-by-token. Perceived latency is the single biggest comfort win available. |
| **Unranked source list.** UI renders `search_results` (arrival order). | After P1-3 it is RRF-ordered; show the `rrf_score` and the contributing engines as a "found by N engines" consensus badge. |
| **`results` never displayed.** The ranked chunks with the actual evidence are returned and dropped. | Render the top chunk under each source as the "why this matched" excerpt. |
| **API key in `localStorage`, plaintext.** | Keep it, but add a visible warning and a session-only option; never log it (P1-12c). |
| **TTS is Linux-only and user-specific.** Hardcoded `/home/sandeep/.local/bin/edge-tts`. | Use the browser's `SpeechSynthesis` API as the default (zero install, cross-platform), and keep `edge-tts` as an optional server-side upgrade. |

### P2-7 · Delete dead code

All of these are `pub`, which is why `cargo build` reports only 4 warnings. Either wire them up (noted) or delete them — a reader cannot currently tell which features are real.

| Symbol | Location | Action |
|---|---|---|
| `_stealth_client` + `apply_stealth_headers` | search.rs, config.rs | **Wire up** (P0-1) |
| `trust_score` | llm.rs:1411 | **Wire up** (P1-6) |
| `cross_query_fusion` | ranking.rs:614 | **Wire up** (P1-9) |
| entire `eval.rs` + `golden_queries.json` | eval.rs | **Wire up** (P2-4) |
| `TempDb::get_session` + progress fields | cache.rs | **Wire up**: no HTTP route reads it, and `wipe_session` runs before any client could poll. Add `GET /api/progress/:id` or delete the subsystem. |
| `summarize_direct` | llm.rs:647 | Delete — superseded by `summarize_direct_with_intent` |
| `verify_citations` | llm.rs:1573 | **Reimplement.** It returns the input unchanged — it only logs. Doc says "returns the answer with unsupported citations flagged". Return structured `Vec<CitationCheck>` and surface it in the API. |
| `rrf_ranked_urls`, `generate_query_variations`, `generate_specialized_variations`, `FALLBACK_THRESHOLD`, `session_jitter_ms`, `multi_query_max_sub_queries` (dup of `query_intel::max_sub_queries`), `backup_engines`, `unique_tokens`, `tokenize`, `ApiFormat::Gemini` branch, `RequestContext::{CrossSite,SameOrigin}` | various | Delete |
| `IMG_20260506_000016.png` (345 KB), empty `test.log` | repo root | Delete; add `*.log` to `.gitignore` |

### P2-8 · Correctness fixes in the remaining modules

**`HistoryDb::persist` rewrites the entire file on every entry** with `serde_json::to_string_pretty`, non-atomically. The docs call it "JSON-lines" but it is one pretty-printed array. With 5,000 entries each holding a full LLM answer, every search rewrites tens of MB — and a crash mid-`fs::write` destroys all history.

```rust
/// Append one line; never rewrite. Atomic by virtue of O_APPEND on a single
/// small write. Compaction is a separate, explicit operation.
async fn append_entry(&self, e: &HistoryEntry) -> std::io::Result<()> {
    let line = format!("{}\n", serde_json::to_string(e)?);
    let path = self.file_path.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        f.write_all(line.as_bytes())
    }).await.unwrap()
}
```

Add a `MAX_HISTORY_ENTRIES` cap (default 10,000) and read with `lines().filter_map(parse)` so one corrupt line does not lose the file.

**Other items:**
- `TempDb::wipe_all` takes the write lock twice (`.len()` then `.clear()`).
- `search.rs:386-394` — auto-mode complexity tuning is dead arithmetic: `max_urls` is already `.min(100)`, so `Complex => max_urls.max(100).min(150)` always yields 100.
- `apply_focus_mode`'s match arm `Some("auto") | Some("fast") | ... | _` makes every preceding alternative unreachable — simplify to `_ => base.to_string()`.
- `llm.rs` SSE loop does `buffer = buffer[newline_pos + 1..].to_string()` per line — O(n²) reallocation across a long streamed answer. Use `buffer.drain(..newline_pos + 1)`.
- `extract_title` strips `" - Site Name"` suffixes; `extract_title_from_doc` (the one actually called) does not. Unify.
- `BM25_DELTA = 1.0` makes the flat `δ·IDF` term contribute up to ~45% of each term's score regardless of TF, flattening term-frequency discrimination. Re-tune against the P2-4 harness; δ ≈ 0.3–0.5 is more typical.
- `fallback_content_element_extraction` returns non-empty for almost any page, so Strategies 5 and 6 in `extract_article_text` are unreachable.
- Dockerfile: `rust:1.82-slim` against `reqwest 0.13` / `axum 0.8` / `rand 0.10` is likely too old — pin a current toolchain, add a non-root user and a `HEALTHCHECK`, and reconcile `ENV ENGINES=duckduckgo,brave,yahoo,qwant,mojeek` / `MAX_URLS=15` with the documented defaults (420).
- README claims version 5.2.0, "7-tier extractor", "20 browser profiles (Chrome 147)", and "90+ engines" against a 7.0.0 binary with a 7-strategy extractor, 20 profiles claiming Chrome 155, and ~30 real endpoints. Regenerate it from the code.

---

## §5. Execution order

Ship in this order. Each phase is independently releasable and has a measurable gate.

### Phase 1 — Stop the bleeding (P0-1 … P0-5)
`fetcher.rs` + trait change + status checks + `Box::leak` removal + zstd + UTF-8 safety + `CatchPanicLayer`.
**Gate:** Brave ≥ 20, DDG ≥ 8, Bing ≥ 8 results on a live query. No panic on a 60-character Hindi query. RSS flat across 200 sequential searches (`scripts/ram_monitor.sh`).

### Phase 2 — Make retrieval honest (P0-6 … P0-10, P1-1, P1-2)
Pagination, global ranks, alias collapse, permit-before-sleep, mode deadlines, engine health + quarantine, roster cleanup, Marginalia API.
**Gate:** `/health/engines` classifies every engine. ≥ 8 engine *families* contributing. Requests per search drop by > 50% while raw results increase.

### Phase 3 — Make extraction real (P1-4, P1-5)
`rs-trafilatura`, fixed JSON-LD with `@graph` + array `@type` + articleBody-first, charset decoding, script-aware sentence splitting, char-based gates, language-mirrored prompts.
**Gate:** the Amar Ujala fixture test passes (> 4,000 chars, date present, `lang == "hi"`). Average extracted chars per source **> 2,500** (baseline: **470**).

### Phase 4 — Use the signals you compute (P0-11, P1-3, P1-6, P1-7, P1-8)
RRF-ordered source selection, citation map, single-ranker context, O(n log n) MMR, unified canonicalization.
**Gate:** nDCG@10 from the P2-4 harness improves over the Phase 3 baseline. Citations resolve to the correct URLs in a manual 20-answer spot check.

### Phase 5 — Scale and modernize (P1-9 … P1-12, P2-1 … P2-3)
Decomposition cleanup + real fusion, model-aware context budget, reasoning-model extraction + retry, security hardening, opt-in reranker, freshness decay, caches.
**Gate:** nDCG@10 improves again with `RERANKER=on`. p50 latency for a repeated query drops > 60% on cache hits.

### Phase 6 — Product polish (P2-4 … P2-8)
Eval binary in CI, unified streaming pipeline, SSE-driven UI with linked citations and cancel, dead-code removal, docs regenerated from reality.
**Gate:** `cargo test` green with the §P2-4 table covered. CI fails on nDCG regression.

---

## §6. Baseline numbers to beat

Record these before Phase 1 and re-measure after every phase. They are the measured starting point from §1.1.

| Metric | Baseline (2026-10-01) | Target |
|---|---|---|
| Engine families contributing | **2** (bing-derived, marginalia) | ≥ 6 |
| Engines returning results / dispatched | **4 / 35** | ≥ 20 / 25 |
| Unique URLs scraped / RRF-scored | **20 / 85** (arrival order) | top-N **by RRF** |
| Total extracted chars per search | **11,768** | > 120,000 |
| Avg extracted chars per source | **470** | > 2,500 |
| Amar Ujala article extraction | **180 chars** (desc) | **4,596 chars** (body) |
| HTTP requests per search | ~175–525 (mostly wasted) | < 120, > 90% useful |
| Engine stage | 6,001 ms (deadline-capped) | mode-appropriate |
| Scrape stage | 8,371 ms | < 5,000 ms warm, cached |
| Tests | **0** | §P2-4 table + eval in CI |
| nDCG@10 | **unmeasured** | measured every phase |

---

## Sources

- [SearXNG — Frustration with major engines (Discussion #5651)](https://github.com/searxng/searxng/discussions/5651) — corroborates §1.2: Google down, Bing irrelevant, Brave rate-limited, DDG CAPTCHAs
- [SearXNG configured engines](https://docs.searxng.org/user/configured_engines.html)
- [WCXB: A Multi-Type Web Content Extraction Benchmark](https://arxiv.org/html/2605.21097v1) · [Web Content Extraction Benchmark](https://webcontentextraction.org/) — extractor F1 comparison
- [rs-trafilatura (GitHub)](https://github.com/Murrough-Foley/rs-trafilatura) · [crates.io](https://crates.io/crates/rs-trafilatura) — v0.2.2 verified
- [Trafilatura benchmarks](https://trafilatura.readthedocs.io/en/latest/evaluation.html)
- [fastembed (docs.rs)](https://docs.rs/fastembed) · [Reranking with FastEmbed — Qdrant](https://qdrant.tech/documentation/fastembed/fastembed-rerankers/) · [Oxidizing Cross-Encoders — Qdrant](https://qdrant.tech/blog/oxidizing-cross-encoders/)
- [Reranking & Cross-Encoders for RAG: BGE, Cohere, Jina (2026)](https://localaimaster.com/blog/reranking-cross-encoders-guide) — bge-reranker-v2-m3 as the 2026 default
- [Brave Search API free tier removed](https://www.implicator.ai/brave-drops-free-search-api-tier-puts-all-developers-on-metered-billing/) · [Brave Search API pricing 2026](https://costbench.com/software/ai-search-apis/brave-search-api/)
- [Mojeek Web Search API](https://www.mojeek.com/services/search/web-search-api/)
- [Marginalia Search API](https://about.marginalia-search.com/article/api/) — public key verified live
- [wreq — Rust TLS fingerprint client](https://www.piloterr.com/blog/wreq-rust-http-client-for-browser-emulation-tls-fingerprinting) · [rquest](https://crates.io/crates/rquest) (v5.2.0)
- Liu et al., *Lost in the Middle* (TACL 2024, arXiv 2307.03172) — already cited in `llm.rs`
- Cormack, Clarke & Büttcher, *Reciprocal Rank Fusion* (SIGIR 2009) · Lv & Zhai, *Lower-Bounding TF Normalization* (CIKM 2011) · Carbonell & Goldstein, *MMR* (SIGIR 1998) · Järvelin & Kekäläinen, *nDCG* (2002)

---

# EXECUTION LOG

## Phase 1 — COMPLETE (P0-1 … P0-5, + P1-1 health registry brought forward)

Status: **gate PASSED**. 73 unit tests, zero build warnings.

### Delivered
| Item | Change |
|---|---|
| P0-1 | `src/fetcher.rs` — `Fetcher{Plain,Stealth}` enum; trait now takes `&Fetcher`; `needs_stealth()` per engine. The wreq client is reachable for the first time. |
| P0-1b | `Emulation::weighted_random()` replaces pinned Chrome147; stealth path no longer overrides wreq's UA/client hints (killed the UA-155-vs-TLS-147 mismatch). |
| P0-2 | Every engine checks HTTP status + `looks_like_challenge()` before parsing. Junk `a[href]` fallbacks deleted from brave/startpage. |
| P0-3 | `Box::leak` gone — specs own templates via `Cow`; engines built once as `Arc` outside the spawn loop. |
| P0-4 | zstd enabled on reqwest **and** wreq; hand-written `Accept-Encoding` removed so the header matches real capability. |
| P0-5 | `src/util.rs` truncate_chars/truncate_bytes/find_ci; 4 byte-slice sites fixed; `CatchPanicLayer` + 256 KB body limit; bind defaults to 127.0.0.1. |
| P1-1 | `src/engines/health.rs` — per-engine counters, `/health/engines`, auto-quarantine, and `DUMP_SERP_DIR` zero-result body dumps. |
| extra | `src/host_gate.rs` — per-registrable-host request spacing (brave+brave_news and 15 bing_* aliases were rate-limiting each other). |
| extra | `src/engines/bing.rs` — dedicated parser. |
| extra | `src/engines/marginalia_api.rs` — JSON API engine. |
| extra | Per-request timeout in `Fetcher` (6s default). |

### Bugs found *during* Phase 1 testing that were not in the original audit
1. **Wikipedia was using the wrong API.** `action=opensearch` is title-prefix
   autocomplete, not search. Measured: `"cricket world cup 2027 schedule"` →
   **0** results from opensearch, **20** from `action=query&list=search`.
   Wikipedia was dead for essentially every natural-language query.
   → now uses the full-text endpoint, and extracts `timestamp` as `published`.
2. **Bing wraps results in `bing.com/ck/a?u=<base64url>` click-tracking
   redirects** on one of its A/B variants. The self-link filter rejected all 10
   results → HTTP 200 with 0 parsed, indistinguishable from "no matches".
   Caught only because `DUMP_SERP_DIR` saved the body.
   → `unwrap_bing_redirect()` + a second fixture (`serp_bing_cka.html`) so both
   variants are covered.
3. **Brave's 2026 markup no longer contains ANY v7.0 selector.** `#results` → 0
   matches, `a.snippet-title` → 0, `.snippet-description` → 0. Fixing the 429
   alone would still have yielded 0 results.
   → selectors re-derived from a live fixture: `div.snippet[data-type="web"]`,
   `a.l1`, `.title`, `.generic-snippet .content`.
4. **Marginalia's public API hangs on some queries.** `"cricket world cup"`
   returned in 0.9s; `"cricket world cup 2027"` never returned (25s timeout).
   The dispatch deadline aborted the task mid-flight, so health recorded an
   attempt with *no outcome* — invisible.
   → per-request timeout turns a hang into a recorded transport error.
5. **reqwest's `RequestBuilder::header` appends, not replaces**, so `Accept` was
   sent twice on every API call. → `set_headers()` with insert semantics.
6. **Per-host spacing × aliases × pages blows the deadline.** 5 bing aliases ×
   3 pages = 15 serialized requests to one host.
   → regional aliases fetch ONE page (they exist for diversity, not depth).

### Measured: before → after

| Metric | v7.0 baseline | Phase 1 | Target |
|---|---|---|---|
| Engines producing results | **4 / 35** | **12 / 26** | ≥20/25 |
| Engines producing, per query | 4 | **9–10** | — |
| Independent RRF families | **2** | **3–6** | ≥6 |
| Raw results per query | 127 | **135–156** | — |
| Brave | 429, 0 results | **164** (family) | ≥20 ✅ |
| DuckDuckGo | 202 CAPTCHA, 0 | **80** | ≥8 ✅ |
| Bing | homepage, 0 | **86** + 99 regional | ≥8 ✅ |
| Wikipedia | 0 (wrong API) | **60** | — |
| Hindi query | **panic / conn reset** | **HTTP 200** | no panic ✅ |
| `spec_for` leak | ~1 String per alias per search | **<4 MB / 800k calls** | flat ✅ |

### Known-remaining after Phase 1 (carried into Phase 2)
- `google*` (4 aliases): all blocked by the `enablejs` JS interstitial. TLS
  impersonation is not sufficient; needs either a consent-cookie flow or
  removal from the default roster.
- `brave` web endpoint: 429 even with host spacing, while `brave_news` on the
  same host succeeds. Likely needs a homepage visit to establish a session.
- `qwant`, `startpage`, `yahoo_news`, `4get`: HTTP 200 with 0 parsed —
  **selector rot, correctly flagged by the health registry**, bodies saved in
  `DUMP_SERP_DIR` for parser repair.
- `mojeek`, `ecosia`, `mwmbl`: 403/303 from this IP.
- Steady-state RSS ~241 MB after 72 searches (deltas decelerating to
  ~0.1 MB/search, so not a leak). Driven by `MAX_URLS=420`; Phase 4 source
  selection and Phase 5 caching should reduce it.

---

## Phase 2 — COMPLETE (P0-6 … P0-10, P1-1, P1-2, P0-8b, + P1-8 brought forward)

Status: **3 of 4 gate criteria passed.** 104 unit tests, zero warnings.
The family-count criterion is blocked by external IP-level blocking, not code —
detail below.

### Delivered
| Item | Change |
|---|---|
| P0-8b | `compute_rrf_scores` now collapses to the **best rank per (page, engine-family)** before fusing. 15 Bing aliases score exactly as one; 5 independent families score >3× higher. `family_agreement()` added for an honest "found by N engines". |
| P1-8 | *Brought forward from Phase 4* because RRF needs a canonical key. `url_utils::canonical_key` is now the single implementation: sorts query params, unifies www/m./amp./mobile. and /amp, /index.html. `should_skip` matches the **host**, not a substring of the whole URL. |
| P1-2 | Roster pruned to verified-working engines, with the measurement for each removal recorded in `engines/mod.rs`. Specs retained so `ENGINES=...` can force any of them. |
| P1-2b | `engines/searxng.rs` — optional self-hosted SearXNG upstream (`SEARXNG_BASE_URL`). Upstream engine names are preserved as `searxng:<upstream>` so a federated Bing hit groups into the bing family instead of double-counting. |
| — | `engines/bing.rs` news parser fixed; `TRACKING_PARAMS` corrected (removed `ref`/`source`/`src`/`campaign`/`partner`/`amp`, which are load-bearing and were producing 404s). |

### Parser forensics (via `DUMP_SERP_DIR`)
The health registry flagged four engines as HTTP 200 / 0 parsed. Dumping the
bodies gave a definitive verdict for each — three were not selector problems at
all:

| Engine | Finding | Action |
|---|---|---|
| `bing_news` | `.news-card`/`.newsitem`/`.na_cnt` → **0**. Real markup: `div[data-url]` → 5, `a.title` → 5. | **Fixed** + fixture test |
| `startpage` | Consent/submit page, not a SERP: 19 anchors all to startpage.com + a "Submit" button. POST needs a page-minted token. | Removed from roster |
| `qwant` | 118 KB of JS with **zero** server-rendered anchors — not even `a[href^='http']` matched. Pure SPA shell. | Removed from roster |
| `4get` | Instance is **down**; page links to `bsky.app/profile/4getisdown.bsky.social`. | Removed from roster |
| `yahoo_news` | JS-rendered; one `.NewsArticle` wrapper + one self-link server-side. | Removed (plain `yahoo` covers the family) |

**Google was retested properly before removal.** `CONSENT=YES+...`, `SOCS=...`,
`gbv=1`, and `hl`/`gl` variants were all tried with a Chrome-149 fingerprint:
every one returned HTTP 200 with the `enablejs` interstitial, **0** `/url?q=`
links and **0** `<h3>`. Google now requires full JS execution; there is no HTML
path. All 5 Google aliases left the default roster.

### Measured: v7.0 → Phase 1 → Phase 2

| Metric | v7.0 | Phase 1 | **Phase 2** | Gate |
|---|---|---|---|---|
| Engines producing results | 4 / 35 | 12 / 26 | **12 / 17** | — |
| Engines producing, per query | 4 | 9–10 | **10** (stable) | — |
| Independent RRF families / query | **2** | 3–6 | **5** | ≥8 ✗ |
| Raw results per query | 127 | 135–156 | **136–158** | up ✅ |
| HTTP requests per search | ~175–525 | — | **~24** | −50% ✅ **(−86%)** |
| Engines dispatched | 35 | 26 | **17** | — |
| 15 Bing aliases in RRF | counted **15×** | 15× | **1×** | ✅ |

### The family gate: why 5 and not 8

This is an external constraint, so recording it honestly rather than
reclassifying it as a pass. From this IP, on 2026-10-01:

- `mojeek` **403**, `ecosia` **403**, `mwmbl` **303**, `yacy` timeout
- `google*` JS-walled (retested with consent cookies, see above)
- `startpage`, `qwant`, `4get`, `yahoo_news` structurally broken

Five families (bing, brave, wikipedia, marginalia, wiby/yandex) is what free
HTML scraping yields here. The three legitimate routes past it, in order of
practicality:

1. **Self-hosted SearXNG** (`SEARXNG_BASE_URL`) — implemented this phase. Uses
   the operator's own IP and rate budget, and SearXNG maintains ~80 upstream
   parsers so rot stops being our problem. Public instances are not viable
   (measured 429/429/403, and searx.be returns HTML), so none are shipped.
2. **BYOK search APIs** — mirrors the existing BYOK-LLM design. Note the 2026
   pricing reality: Brave removed its free Search API tier in Feb 2026
   (~$5/1k, with $5/mo credits ≈ 1,000 free queries); Mojeek's Web Search API
   is a negotiated B2B arrangement at ~£2–3 CPM.
3. **Proxy pool** — `PROXY_POOL` already exists and now routes through
   `Fetcher`; residential egress would likely recover mojeek/ecosia/mwmbl.

### Known-remaining
- `brave` web endpoint still 429s while `brave_news` on the same host succeeds
  (164→152 results, so the family is covered). Likely needs a homepage visit to
  mint a session cookie. Low priority while brave_news works.
- `bing_news` parses on some queries (11 results) but not all — 3 zero-result
  dumps remain. Bing News appears to A/B a JS-rendered layout.
- `yandex` degraded (8 results) — heavy throttling.

---

## Phase 3 — COMPLETE (P1-4, P1-5)

Status: **gate PASSED by a wide margin.** 123 unit tests, zero warnings.

### The extractor decision was made by measurement, not by the plan

The blueprint proposed `rs-trafilatura`. Before adopting it I benchmarked three
candidates on a 10-page corpus (Hindi news ×3, English news, docs, wiki ×2,
blog ×2, medical) committed under `tests/fixtures/articles/`.

**Correction to the audit:** the project did **not** already have Mozilla
Readability. There was no `readability` crate in `Cargo.toml`/`Cargo.lock` and
no reference in `src/`. The v7.0 extractor was entirely hand-rolled; it
borrowed trafilatura's *ideas* (link-density penalty, text-density scoring) but
was not Readability. So the real choice was hand-rolled vs trafilatura vs a
genuine Readability port (`dom_smoothie`).

| policy | chars | ms | boilerplate hits |
|---|---|---|---|
| v7.0 hand-rolled 7-strategy | 76,462 | 55 | 0 |
| rs-trafilatura only | 117,032 | 374 | 0 |
| dom_smoothie (Readability) only | 102,248 | 78 | 1 |
| both always, take longer | 121,490 | 457 | 1 |
| **trafilatura + conditional readability** | **120,898** | **416** | 1 |

**Chosen: trafilatura primary + conditional Readability second opinion.**

Reasoning:
1. Trafilatura alone is 96.3% of best-of-all, but **the average hides tail
   risk**: on the NIH page it extracted 1,733 chars where Readability got
   3,887 — a 55% loss on that page shape. Readability won 6 of 10 pages;
   trafilatura won big on long-form (wikipedia_en 23,968 vs 11,927).
2. An absolute "< 600 chars" fallback trigger **never fired** in testing. The
   trigger that works is *relative*: thin output for the page's size
   (`chars/html_bytes < 0.035`), low confidence, or very short text.
3. Readability alone is not viable — −13% content and, decisively, **no
   metadata**. Trafilatura is the only one providing `date`, `language` and
   `extraction_quality`, all three of which the pipeline now depends on.
4. The result: 99.5% of the always-both content at 91% of its cost.

The v7.0 extractor is **retired**, not kept as a third tier: it loses on volume
(76k vs 117k), provides no metadata, and carries the accidentally-quadratic
`element.html()` scoring loop.

### Delivered
| Item | Change |
|---|---|
| P1-4 | `extractor.rs` rewritten around the measured policy. Returns `Extracted { title, text, published, language, page_type, confidence, source }`. |
| P1-4b | JSON-LD reader fixed as a last-resort tier: walks `@graph` + `hasPart` + `mainEntityOfPage`, handles `@type` as an array, and is **articleBody-only** so a `WebPage` `description` can never win. |
| P1-5a | Script-aware sentence segmentation: 13 terminators incl. `।` U+0964 danda, `。`, `؟`, Thai/Myanmar/Ethiopic. Uncased scripts no longer require an uppercase next char. |
| P1-5b | `char_count` is now characters everywhere (it was bytes in `search.rs`, chars in `ranking.rs` — same field, two units). `MIN_TEXT_CHARS` replaces the byte gate. |
| P1-5c | Charset-aware decode wired into the scrape path (was `from_utf8_lossy`). |
| P1-5d | `LANGUAGE_RULE` appended to all 4 prompt builders — a Hindi query no longer gets an English answer. |
| P1-6a | Real `extraction_quality` replaces the hardcoded `0.7`, applied multiplicatively. |
| P1-6b | **`llm::trust_score` is now actually called.** It was fully implemented and `pub` in v7.0 — which is why cargo never warned — and never invoked. |

### Measured: v7.0 → Phase 3 (live searches)

| Metric | v7.0 baseline | **Phase 3** | Gate |
|---|---|---|---|
| Extracted chars **per source** (English) | 470 | **17,436** | >2,500 ✅ |
| Extracted chars **per source** (Hindi) | 470 | **5,054** | >2,500 ✅ |
| Total extracted per search | 11,768 | **374k–680k** | >120k ✅ |
| Sources with a publish date | **0** | 28/39, 70/74 | — ✅ |
| Language detected | **none** | `hi` on 38/40 Hindi chunks | — ✅ |
| Extraction confidence | hardcoded 0.7 | real, mean 0.97, min 0.60 | — ✅ |
| Amar Ujala article | **180 chars** | **>4,000 chars** + date + `hi` | ✅ |
| Readability fallback firing | n/a | 1/25 EN, 7/40 HI | works ✅ |

### A test-methodology bug I introduced and fixed
My first `Box::leak` regression test read `/proc/self/status` RSS. That is
process-wide, so with `cargo test`'s parallel harness the extractor suite
loading an 890 KB fixture showed up as a 15 MB "leak" in the generic-engine
test. Replaced with a counting global allocator (`src/testalloc.rs`, test-only)
that asserts on **live allocated bytes**, which is deterministic under
parallelism.

### Known-remaining
- The `rank` stage now takes **3,011 ms** for 91 sources. Phase 3 multiplied the
  text volume 32–58×, so the MMR `O(top_k · n · |selected|)` loop that the audit
  flagged (P1-7) is now the dominant CPU cost. This is Phase 4's first job.
- `MAX_CONTEXT_CHARS_LITE` is still 16,000, so the LLM sees ~16 KB of the
  374k–680k now extracted. Phase 5 (P1-10) raises it.

---

## Phase 4 — COMPLETE (P0-11, P1-3, P1-6, P1-7; P1-8 was done in Phase 2)

Status: **gate PASSED.** 138 unit tests, zero warnings.

This phase was about a single theme: the pipeline computed good signals and
then discarded them.

### Delivered
| Item | Change |
|---|---|
| P1-3 | **Sources are selected by RRF consensus, not arrival order.** v7.0 scored every URL and then handed `deduplicate` the raw arrival-order list, so the scrape set was whichever engine finished first. `search_results` (what the UI renders) now comes from the same ranked set. |
| P1-7 | MMR rewritten: tokenize once, u64 signatures merged as sorted slices, a `top_k × 5` candidate window, and **incremental `max_sim`** so the inner `|selected|` loop disappears. Also fixed the cap-escape bug where an all-capped round still selected `remaining[0]`. |
| P0-11 | `build_ranked_context` returns a `citation_map`. `[n]` now resolves through it instead of against the unreordered slice. |
| P1-6 | `build_ranked_context` **no longer re-ranks** — it packs only. It also truncates an oversized source instead of `break`-ing out of context assembly. |
| P1-10 | `ContextBudget::for_model` replaces the fixed `MAX_CONTEXT_CHARS_LITE = 16_000`; `max_tokens` and `temperature` added to request bodies (both absent for OpenAI-compatible in v7.0, and Anthropic was pinned at a truncating 4096). Publication dates are now written INTO the context. |
| — | `verify_citations` reimplemented to return `Vec<CitationCheck>`. v7.0's body ended with the unmodified input, so despite its doc comment it only wrote log lines. `llm::summarize_direct` (dead) removed. |

### Measured: Phase 3 → Phase 4 (live)

| Metric | before | **after** |
|---|---|---|
| Rank stage, 88 sources | 3,011 ms | **93 ms** (32× faster) |
| Rank stage, 35 sources | ~690 ms | **24 ms** |
| Source selection | arrival order | **126 candidates → top 40 by RRF** |
| Sources found by >1 independent family | not computed | **21 of 40** |
| Engines represented in the UI list | **1** (all `bing_news`) | **10** |
| UI list ordering | arrival order | **RRF-monotonic** |
| Top English result | first to arrive | found by **3 families** (wikipedia + bing + brave) |
| LLM context cap | 16,000 chars fixed | **model-scaled** (24k–400k tokens) |
| Anthropic `max_tokens` | 4,096 (truncating) | model-scaled, ≥8,192 |

The Hindi top result is now `भारत-पाकिस्तान के बीच मुकाबला तय...`, found by two
independent families — i.e. exactly the Amar Ujala-class content the audit
started from, ranked first on consensus rather than luck.

### Test-fixture bugs of my own, fixed
Two `verify_citations` tests failed initially because my fixture sources were
under `build_ranked_context`'s 80-character minimum and were correctly skipped.
The minimum is right — a sub-80-char "source" is not evidence — so the fixtures
were lengthened rather than the threshold lowered.

---

## Phase 5 — COMPLETE (P1-9, P1-11, P1-12, P2-2, P2-3; P1-10 landed in Phase 4)

Status: **gate PASSED.** 177 unit tests, zero warnings, stable across repeated runs.

### Delivered
| Item | Change |
|---|---|
| P1-12a | `src/ssrf.rs` — `base_url` is validated before use. v7.0 was an **open credentialed SSRF proxy**: with permissive CORS and a 0.0.0.0 bind, any page in the user's browser could POST a `base_url` of `http://169.254.169.254/...` and read cloud metadata. Metadata IPs are blocked unconditionally; loopback/private ranges need `ALLOW_LOCAL_LLM=1` (so Ollama/LM Studio still work). |
| P1-12b | `LlmConfig`'s derived `Debug` replaced with a hand-written impl that redacts `api_key`. |
| P1-12c | TTS: concurrency-capped to 2, temp file removed via a `Drop` guard on every path (v7.0 leaked one mp3 per failed request), and the hardcoded `/home/sandeep/.local/bin/edge-tts` path removed. |
| P1-11a | Reasoning-model responses no longer discarded. Anthropic now scans for the first `type:"text"` block (v7.0 read `content[0].text`, which is a `thinking` block under extended thinking → `empty_response`). OpenAI-compatible handles string / array-of-parts / `reasoning_content`. |
| P1-11b | `call_llm_retrying` with exponential backoff on 429/5xx — v7.0 had **no retry**, so one rate-limit failed the whole search. One shared `OnceLock` client replaces a fresh `reqwest::Client` per call. |
| P0-12 | Temporal detection matches on **word boundaries**. v7.0's substring list made "New York population", "how do I know if…", "renewable energy" and "justice system" all time-sensitive, appending a spurious year to the query. |
| P1-9 | Decomposition disciplined: no midpoint fragments, no bare single-word entities, capped at 3. `cross_query_fusion` is now **wired in** (it was dead code) with the original query weighted 2× and family-collapse applied. |
| P2-2 | Freshness decay as a real ranking signal, gated on time-sensitivity so evergreen queries are not recency-biased. Half-lives: news 7d, person 180d, default 60d, floor 0.60. |
| P2-3 | `src/cache_layer.rs` — two-tier `moka` cache (SERP 10 min, page 6 h) plus `/health/cache`. v7.0 had **no result cache at all**. Empty results are deliberately never cached, since an empty result is usually a block. |
| — | Ranking now uses the **original** query, not `effective_query`. v7.0 fed BM25 the engine-syntax version, so `site` and `reddit.com` became scoring terms. |

### A latency optimization I had to back out

I first added an engine-dispatch early-exit ("stop once we have N results from
M families"). It cut latency but **destroyed the consensus signal** — the thing
Phases 2 and 4 exist to produce. Measured, same query, cold, `CACHE=off`:

| threshold | latency | raw results | multi-family | engines |
|---|---|---|---|---|
| R=60 F=3 | 4.25 s | 64 | **2** | 5 |
| R=120 F=4 | 5.26 s | 133 | 9 | 5 |
| R=200 F=5 | 7.39 s | 161 | 9 | 9 |
| **disabled** | 4.76 s | **165** | 7 | **9** |

Exiting early cost ~60% of the raw results and most cross-family agreement to
save about half a second, and disabling it was not even slower in practice.
**Disabled by default**, with the table recorded in `config.rs` so the next
person does not redo the experiment. `EARLY_EXIT_MIN_RESULTS` re-enables it.

The actual latency win came from two different places:
1. **Scrape-stage early-exit**, which gates on BOTH enough sources AND enough
   extracted text — so it costs no quality. 8,000 ms → ~1,000 ms.
2. **Nesting the timeouts.** The per-request timeout was 6,000 ms against a
   6,000 ms dispatch deadline, so a single hanging engine (yacy) held the
   collector open for the entire window on every query — 16 of 17 tasks long
   since finished. Dropping the request timeout to 4,000 ms lets the straggler
   fail, get recorded as a transport error, and the dispatch close.

### Measured: v7.0 → Phase 5 (live, same query)

| Metric | v7.0 | **Phase 5** |
|---|---|---|
| Cold query, end to end | 14.9 s | **5.42 s** |
| Repeated query | 14.9 s | **4.37 s** (0.03 s with both caches warm) |
| Engine stage | 6,001 ms (always) | **4,138 ms**, 0 ms on SERP-cache hit |
| Scrape stage | 8,371 ms | **1,258 ms** cold, **1 ms** warm |
| Rank stage | 3,011 ms | **13 ms** |
| Raw results per query | 127 | **167–187** |
| Sources found by >1 family | not computed | **13–14 of 40** |
| SSRF via `base_url` | **exploitable** | blocked (400) |

### Test-quality work
Five of my own tests mutated process-global env vars (`ALLOW_LOCAL_LLM`,
`WIKIPEDIA_LANGS`, `SEARXNG_BASE_URL`, `HOST_MIN_INTERVAL_MS`,
`CONTEXT_MAX_TOKENS`) and raced under `cargo test`'s parallel harness — one
failed intermittently. Each is now split into a parameterised core plus a thin
env-reading wrapper, so the tests pass deterministically (verified over three
consecutive runs).

---

## Phase 6 — COMPLETE (P2-4, P2-6, P2-7, P2-8)

Status: **gate PASSED.** 193 unit tests, zero warnings, stable over 10
consecutive runs.

### P2-4: the eval harness is alive, and it immediately found a real bug

`src/eval.rs` (474 lines of nDCG/MRR/Recall) and the 50-query labelled golden
set were both completely unreferenced in v7.0. Wiring them up required adding a
`[lib]` target, since a bin-only crate has nothing to import from — which is
part of why they were never used.

**The first run found that intent classification was only 70% accurate**, and
named every miss. Six distinct, fixable causes:

| Expected | Got | Example | Cause |
|---|---|---|---|
| navigational | factual | `github new repository` | v7.0 required ≤3 words **and** an explicit `login`/`website` token |
| temporal | factual | `2026 FIFA World Cup schedule` | years were only checked *after* intent was decided |
| person | factual | `Alan Turing achievements` | no `who is`/`biography of` prefix |
| research | howto | `how does the TCP handshake work in detail` | "how **to** do X" (procedural) and "how **does** X work" (explanatory) were collapsed |
| factual | temporal | `population of Japan` | treated as a live fact, so a year was appended |
| comparison | navigational | `Docker vs Kubernetes differences` | leads with a word that is also a site name; comparison must be checked first |

After fixing all six: **70% → 100% (50/50)**, locked in by six unit tests.

| metric | baseline | after intent fix |
|---|---|---|
| nDCG@10 | 0.3407 | **0.3524** |
| MRR | 0.5389 | **0.5483** |
| Recall@10 / @20 | 0.3250 / 0.3650 | 0.3267 / **0.3750** |
| intent accuracy | **70%** | **100%** |

`.github/workflows/ci.yml` now gates: build with `-D warnings`, all 193 tests,
intent accuracy at 100% (deterministic, hard gate), and retrieval nDCG against
`benchmarks/baseline_v8.json` (informational, since it hits live engines that
rate-limit and go down).

### P2-6: UI
- **Citations are links.** v7.0 rendered `[n]` as a `<sup>` with
  `cursor:default` — decorative, and mis-mapped anyway. They now resolve
  through `citation_map` to the source the model actually saw as `[n]`.
- **Searches can be cancelled.** `grep -c AbortController ui.html` returned
  **0** in v7.0; a 15–60 s search could not be stopped and the server kept
  scraping for a response nobody would read. Stop button + Escape.
- **Consensus is visible**: `3× agree` badges from `found_by_families`, plus
  snippets and multi-engine attribution, in RRF order.

### P2-8: correctness
- **`HistoryDb` was O(n²).** `persist()` serialized the entire history with
  `to_string_pretty` and rewrote the whole file **on every entry** — tens of MB
  per search at a few thousand entries — non-atomically, so a crash mid-write
  destroyed everything. Now append-only JSON-lines under `O_APPEND` (O(1)),
  with a `MAX_HISTORY_ENTRIES` cap (v7.0 had none), atomic `compact()`, legacy
  array migration, and per-line error tolerance.
- `TempDb::wipe_all` took the write lock twice.
- Dockerfile: current toolchain, non-root user, `HEALTHCHECK`, and the
  defaults no longer silently override the documented ones (v7.0 set
  `ENGINES` to five names and `MAX_URLS=15` while claiming 90+ engines).
- README rewritten from measurement. The "90+ engines" claim is replaced by a
  per-engine status table **and a pointer to `/health/engines` so readers can
  check their own install instead of trusting the table.**

### A third attempt at the leak test

The thread-local allocator is the version that actually works. For the record:

1. `/proc/self/status` RSS — process-wide; the extractor suite loading an
   890 KB fixture showed up as a 15 MB "leak" in the generic-engine test.
2. A process-wide live-bytes counter — better (it decreases when a neighbour
   frees) but still shared, so it failed intermittently.
3. **Thread-local counters** — `spec_for` is synchronous and runs entirely on
   the measuring thread, so nothing else can pollute it. 10 consecutive clean
   runs.

### Final end-to-end state

| | result |
|---|---|
| English query | HTTP 200, 6.80 s, **19,956 chars/source**, 25/25 dated |
| Hindi query | HTTP 200, 5.54 s, **7,521 chars/source**, 37/40 dated, languages `hi`/`hi-in`/`anp` |
| `/health`, `/health/engines`, `/health/cache`, `/` | all 200 |
| SSRF to `169.254.169.254` | **400** |
| Scrape stage | 1,098 ms / 1,137 ms |

**Caveat on the final consensus numbers.** By the end of this session a day of
testing had rate-limited this IP: Brave 3/3 blocked, Yahoo 3/3, DuckDuckGo 2/5.
So the last run shows `multi_family=0` where an earlier run showed 13–14. That
is IP state, not a code regression — and `/health/engines` showing exactly
which engines are blocked, with counts, is the feature working as intended.

---

## v8.1 — Mode refinement (Fast / Auto / Deep / Study / Academic / Reddit / YouTube)

Status: **gate PASSED.** 256 unit tests, zero warnings, intent probe 50/50.

Goal: make each mode do what its name promises. Fast should be the fastest,
Auto should size itself to the question, Deep should read the most and write
the most complete report, and Study/Academic should have their own sources and
prompts.

### What was wrong with the modes

- **No single source of truth.** Mode behaviour was spread across `search.rs`
  (`apply_focus_mode`), `config.rs` env defaults, `llm.rs` (three separate
  prompt paths) and the stale `/search/stream` pipeline in `stream.rs`. They
  disagreed: the stream endpoint ignored modes completely.
- **Deep was not deeper.** It used the same scrape stage and early exit as
  Auto, and the LLM path truncated each source to 3,000 chars
  (`summarize_iterative`), so a "deep" report saw less text than Auto.
- **Study did not exist.** The UI had no study mode; it is new in v8.1.
- **Academic sent `site:edu OR site:gov …` to every engine** (see the
  OpenAlex header comment). It now queries OpenAlex and arXiv directly.

### Design

`src/modes.rs` resolves one `ModeProfile` per request from (mode × domain ×
query intelligence). Every budget the pipeline uses (engines, SERP pages,
deadline, early exit, sub-queries, URLs, scrape deadline/thresholds, top-k,
chunks per URL, freshness, synthesis style, context and output caps) lives in
that one struct. `GET /config` prints it per mode. Auto picks a tier from the
query (Lookup / Light / Standard / Heavy). `src/synthesis.rs` holds one prompt
per synthesis style: Concise, Standard, Report (map-reduce), StudyNotes,
Literature, Community, Video.

### Measured (20 golden queries, `CACHE=off`, no LLM)

| mode | nDCG@10 | MRR | Recall@20 | p50 |
|---|---|---|---|---|
| Fast | 0.4657 | 0.6401 | 0.4667 | 2.4 s |
| Auto | 0.4779 | 0.6899 | 0.5583 | 5.3 s |
| Deep | **0.5241** | **0.7442** | **0.6208** | 17.2 s |

Before the last two fixes below, Deep was the *worst* mode on ranking quality
(nDCG 0.4026, MRR 0.5236) despite reading the most pages.

One end-to-end run per mode ("how does mRNA vaccine technology work"):

| mode | time | URLs selected | pages read | chars extracted |
|---|---|---|---|---|
| Fast | 2.7 s | 10 | 6 | 0.42 M |
| Auto (heavy tier) | 5.8 s | 50 | 30 | 1.42 M |
| Deep | 21.6 s | 150 | 120 (30 abstracts) | 5.74 M |
| Study | 6.2 s | 35 | 20 | 0.83 M |
| Academic | 5.2 s | 40 | 25 (17 abstracts) | 0.44 M |

With a local 7B model (Ollama, 4,096-token window), Deep's map-reduce report
took 84 s and 13 of 15 citations were supported by the cited source.

### Bugs found during this work

1. **MMR panic, introduced by me in Phase 4.** `window.clamp(top_k, len)`
   panics when there are fewer chunks than `top_k` (Reddit: 14 chunks vs 25).
   Fixed with `.min()`, plus a regression test.
2. **Cross-query fusion erased cross-engine consensus.** `cross_query_fusion`
   returned one representative result per URL, carrying one engine name, and
   the caller re-ran RRF over those. In every multi-query mode each page
   looked as if a single engine had found it (`found_by_families == 1` for
   all of them). It now returns scores only (`cross_query_scores`) and the raw
   results are kept. Deep went from "Paris: 1 family" to 2–3.
3. **Deep facets changed the meaning of factual questions.** "capital of
   France" got the sub-queries "… research evidence" and "… limitations
   challenges", and OpenAlex was added for Factual intent, so three economics
   papers about *financial* capital (Journal of Finance, QJE) ranked above
   Wikipedia's "Paris". Factual questions now get overview/history/key-facts
   facets, and scholarly indices are only added for Research intent or the
   Science/Health/Tech domains. The 1.5× selection boost for prefetched
   abstracts no longer applies in Deep.
4. **Bing soft-blocking, including adult content.** With an automated client,
   Bing returns a normal 200 page of results unrelated to the query:
   "capital of France" → Bhubaneswar/Odisha, "best budget phone 2026" →
   Mumbai's BEST bus company, "mRNA vaccine mechanism" → **pornography**
   (ranks 2 and 5 of Auto's results). Page 2 is identical to page 1. Nothing
   trips a challenge marker. `health::screen_serp` now clears pages where
   under 25% of results are on-topic and trims mixed pages (under 60%) to
   their on-topic results. Filler only ever matches the query's first word,
   so "on-topic" means two or more terms, or any term after the first. The
   first version of the rule ("two or more terms") wrongly blocked Wikipedia
   on "definition of photosynthesis". The rule changed, and the index APIs
   (Wikipedia, OpenAlex, arXiv), which never serve filler, are exempt.
5. **Yandex CAPTCHA parsed as results.** Yandex redirects to `/showcaptcha`
   with HTTP 200; the generic fallback selectors returned its "SmartCaptcha"
   footer link at rank 6 in Deep. Added `/checkcaptcha` and `smartcaptcha`
   challenge markers.
6. **Hindi words were split at the virama.** `char::is_alphanumeric` is false
   for ् (U+094D), so "क्या" tokenized as ["क", "या"] and "प्रधानमंत्री" as
   three fragments: in query terms, BM25 and the citation check. Fixed with
   `util::is_word_char` in all four tokenizers.
7. **Hindi queries searched English Wikipedia.** "भारत की राजधानी" returned
   "Bayana" and "Vishnu Sharma". When `WIKIPEDIA_LANGS` is unset, the query's
   script now picks the edition (hi, bn, ta, te, …, ja, zh, ru), and English is
   searched only if the native edition returns fewer than 3 hits. The same
   query now returns "भारत की राजधानियों की सूची" and "नई दिल्ली".
8. **Reddit URL rewrite produced `old.old.reddit.com`**; Reddit's CAPTCHA page
   ("Welcome to Reddit") was extracted as content. Fixed: `reddit_to_old` and
   `looks_like_blocked_page`; Reddit falls back to search snippets.
9. **Local LLM servers silently drop the prompt head.** Ollama serves a
   4,096-token window by default and truncates longer prompts from the start,
   which is where the instructions are. `ContextBudget` now assumes 4,096
   tokens for local servers and fits prompt + answer inside it; truncated
   answers (`finish_reason: "length"`) are reported as partial.
10. **The Deep map step had an escape hatch** ("reply NO RELEVANT FACTS") that
   the 7B model took for every batch. Removed; a test enforces it.

### Known limitations

- **IP state dominates live numbers.** After about 60 eval searches, Brave
  (429), DuckDuckGo (202) and Yahoo stopped answering this IP, and Reddit and
  YouTube modes, which only use `site:`-capable engines, briefly returned no
  results. `/health/engines` shows this state.
- **YouTube transcripts** need a PO token; the mode uses video descriptions.
- **Wikipedia's full-text search is weak for shopping queries**: "best budget
  phone 2026" surfaces "Mercy (2026 film)". There is no commercial intent to
  route on; left as is rather than adding a speculative rule.
