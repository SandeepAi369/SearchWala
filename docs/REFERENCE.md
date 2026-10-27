# SearchWala reference

The full technical reference: every mode, engine, extractor, endpoint and
setting. The [README](../README.md) is the short version.

## Contents

- [The five modes](#the-five-modes)
- [Search engines](#search-engines)
- [Stealth layer: getting past WAFs](#stealth-layer-getting-past-wafs)
- [Extraction: reading the page](#extraction-reading-the-page)
- [Ranking and answers](#ranking-and-answers)
- [The interface](#the-interface)
- [Bring your own model](#bring-your-own-model)
- [Quick start](#quick-start)
- [API](#api)
- [Configuration](#configuration)
- [Security](#security)
- [Testing](#testing)
- [Architecture](#architecture)

---

## The five modes

Pick a mode for **how much work** to do. Each mode is a complete profile
(engines, result pages per engine, deadlines, sub-queries, pages read,
passages sent to the model, prompt style and answer length) defined in one
place, `src/modes.rs`. `GET /config` prints every profile.

| Mode | What it does | Engines | Pages read | Typical time* |
|---|---|---|---|---|
| ⚡ **Fast** | Five reliable engines, one results page each. Stops as soon as 3 independent indexes agree, but always waits for the top-ranked pages. Short, cited answer. | 5 | ~10 | ~2–4 s |
| 🤖 **Auto** | Sizes itself to the question. Lookups behave like Fast; short facts read ~20 pages; complex, comparative or explanatory questions get sub-queries and ~55 pages. Time-sensitive questions add news engines. | 5–17 | 8–55 | 2–6 s |
| 🔬 **Deep Research** | All 17 engines plus news and (for research questions) OpenAlex. Up to 5 sub-queries, planned by the LLM when one is configured. Reads ~150 pages. The report is written map-reduce: cited evidence notes per batch of sources, then one report from all the notes. | 17–18 | ~150 | 15–25 s |
| 📘 **Study** | Prefers educational sources (Wikipedia, Britannica, Khan Academy, OpenStax, LibreTexts, NCERT, `.edu`…) with "explained simply" and "examples" sub-queries and no recency bias. Writes study notes: overview, key concepts, worked example, common misconceptions, quick revision and a self-test. | 6 | ~35 | ~6 s |
| 🎓 **Academic** | Queries **OpenAlex** and **arXiv** directly and reads paper abstracts without fetching paywalled publisher pages. Writes a literature review; arXiv preprints are labelled as not peer-reviewed. | 6 | ~40 | ~4–6 s |

\* Search and reading only, on one home connection; the LLM's time comes on
top.

Also available: **Reddit** and **YouTube** modes, which run site-restricted
searches through the engines that support `site:`. Reddit threads are often
behind a CAPTCHA, so the search snippet is used when a page can't be read;
YouTube uses video descriptions, since transcripts aren't accessible.

**Domains** (`tech`, `science`, `finance`, `health`, `news`) combine with any
mode, so "Deep + Health" is valid. A domain adds relevant engines and prefers
that field's authorities when choosing and ranking sources: WHO/NIH/CDC/NHS
for health, regulators and market sources for finance. It also adds field
rules to the answer: disclaimers for health and finance, a dated timeline for
news.

Measured on 20 hand-labelled queries (search only, live engines):

| | nDCG@10 | MRR | Recall@20 | median time |
|---|---|---|---|---|
| Fast | 0.466 | 0.640 | 0.467 | 2.4 s |
| Auto | 0.478 | 0.690 | 0.558 | 5.3 s |
| Deep | **0.524** | **0.744** | **0.621** | 17.2 s |

---

## Search engines

**17 general web engines** plus 2 scholarly indexes:

| Group | Engines | Notes |
|---|---|---|
| Bing family | `bing`, `bing_uk`, `bing_in`, `bing_de`, `bing_news`, `duckduckgo`, `yahoo` | DuckDuckGo and Yahoo are Bing-derived |
| Brave | `brave`, `brave_news` | own index |
| Open / indie | `wikipedia`, `marginalia_api`, `wiby`, `mwmbl`, `yacy` | JSON APIs and independent indexes |
| Other indexes | `mojeek`, `yandex`, `ecosia` | Ecosia proxies Google; Mojeek and Yandex are their own |
| Scholarly | `openalex`, `arxiv` | Academic mode, and Deep for research questions |
| Optional | `searxng` | your self-hosted SearXNG instance (`SEARXNG_BASE_URL`) |

The engine list was cleaned up in v8.0. The old "90+ engines" claim counted
dead instances, JS-only sites and regional aliases of the same index.
Google, Startpage and Qwant were dropped: Google serves a JS interstitial with
no result links, Startpage a consent page, and Qwant a JS single-page app.

**Why the groups matter: consensus is counted by index family.** The 17
engines come from about 10 independent indexes, and RRF fusion collapses each
family to one vote. Seven Bing-derived engines agreeing count as **one** vote,
not seven. `found_by_families: 3` means three genuinely independent indexes
agreed, and the UI shows it as a "3× agree" badge.

**Availability depends on your IP.** Mojeek, Yandex, Ecosia, Mwmbl and YaCy
often refuse datacenter addresses. Check your own install:

```bash
curl localhost:8000/health/engines
```

It reports, per engine, attempts, HTTP successes, blocks, transport errors and
**zero-result responses**, with a verdict of `healthy`, `degraded`,
`selector_rot`, `blocked`, `dead_host` or `unproven`. Engines that produce
nothing are quarantined automatically.

---

## Stealth layer: getting past WAFs

Bing, Brave, DuckDuckGo, Yahoo, Yandex and Ecosia sit behind bot defences
that fingerprint the **TLS handshake and HTTP/2 framing**, not just the
User-Agent. A stock Rust HTTP client is blocked on sight: Bing serves its
homepage, Brave returns 429, DuckDuckGo a CAPTCHA.

SearchWala sends those requests through **[wreq](https://crates.io/crates/wreq)**,
a BoringSSL-based client, with `wreq-util` browser emulation:

- **Real browser fingerprints.** Each client takes a weighted-random real
  Chrome / Firefox / Safari / Edge profile. It reproduces that browser's JA4
  TLS fingerprint (cipher order, extensions, ALPN), HTTP/2 SETTINGS and
  pseudo-header order, User-Agent and client hints, all consistent with each
  other.
- **No mismatched headers.** v7.0 overrode the emulated User-Agent with
  hand-written strings claiming browser versions the TLS layer couldn't
  fingerprint. A UA/TLS mismatch is a stronger bot signal than no spoofing at
  all, so v8.0 lets wreq own the browser identity.
- **Polite pacing.** Requests to the same registrable host are spaced out
  (`HOST_MIN_INTERVAL_MS`), with jitter, so regional variants of one engine
  don't hit it simultaneously.
- **Honest failure handling.** Non-200 responses and challenge pages
  (Cloudflare, DataDome, PerimeterX, Yandex SmartCaptcha) are detected and
  never parsed as results.
- **Soft-block screening.** Engines that decide you're a bot sometimes return
  a normal-looking page of *unrelated* results instead of a CAPTCHA. We
  measured Bing answering "mRNA vaccine mechanism" with adult sites, and
  "capital of France" with pages about Odisha. Every results page is screened
  for query relevance: mostly-filler pages are discarded and mixed pages are
  trimmed.
- **Optional proxies.** `PROXY_POOL`, `PROXY_POOL_FILE` or `TOR_PROXY_PORTS`
  route traffic through a pool, with cooldown on failure.

Engines that don't need impersonation (Wikipedia, Marginalia, OpenAlex,
arXiv…) use a plain `reqwest` client. `STEALTH_MODE=on|off|auto` and
`STEALTH_ENGINES` override the choice.

---

## Extraction: reading the page

Choosing an extractor was a benchmark decision, run on a 10-page corpus
committed under `tests/fixtures/articles/` (Hindi and English news, docs,
wiki, blog):

| extractor | chars extracted | time |
|---|---|---|
| v7.0 hand-rolled | 76,462 | 55 ms |
| rs-trafilatura | 117,032 | 374 ms |
| Readability (`dom_smoothie`) | 102,248 | 78 ms |
| **rs-trafilatura + conditional `dom_smoothie`** | **120,898** | **416 ms** |

- **[rs-trafilatura](https://crates.io/crates/rs-trafilatura)** is the primary
  extractor. It also returns the publication date (used for freshness
  ranking), the language and an extraction-quality score.
- **[dom_smoothie](https://crates.io/crates/dom_smoothie)**, a Rust port of
  Mozilla Readability, runs as a **second opinion** when the primary result
  looks thin for the page (short, low quality, or a low text-to-markup
  ratio). The longer text wins; trafilatura's metadata is kept. On one NIH
  page this recovered 55% more text.
- **Charset-aware decoding.** windows-1252, GBK, Shift_JIS and other legacy
  encodings decode correctly, not as replacement characters.

**Deep reader** (`src/deep_reader.rs`) handles pages whose text isn't in the
ordinary DOM. Both extractors returned nothing for these shapes:

| page shape | how it's read |
|---|---|
| Declarative Shadow DOM (`<template shadowrootmode>`) | shadow roots unwrapped before extraction |
| Next.js `__NEXT_DATA__`, Next.js App Router (RSC `self.__next_f`), Nuxt, Remix, Redux/Apollo state, Astro islands | prose harvested from the hydration data, with nested payloads decoded and URLs/CSS/code filtered out |
| Paywall teaser in the DOM, full text in JSON-LD or microdata | `articleBody` wins when it's clearly longer |
| Everything thin | text-density / link-density block scoring |
| Canonical page unreadable (all modes except Fast) | one fetch of the page's AMP version |

Bot walls served with HTTP 200 (Reddit's "Welcome to Reddit", Cloudflare
interstitials) are detected and never extracted. Text extraction runs on a
blocking thread pool so it can't stall network I/O.

---

## Ranking and answers

```
query
  ├─ 1. intent analysis        factual / temporal / person / comparison / how-to /
  │                            navigational / research — 100% on a 50-query set
  ├─ 2. mode profile           engines, depth and budgets for this mode + tier
  ├─ 3. parallel dispatch      stealth where needed, per-host pacing, early exit
  │                            once enough independent families agree
  ├─ 4. consensus ranking      reciprocal-rank fusion by engine FAMILY, weighted
  │                            across sub-queries (original query counts 2×)
  ├─ 5. source selection       top-N by consensus × source authority for the mode
  ├─ 6. extraction             trafilatura → Readability → deep reader
  ├─ 7. passage ranking        BM25+ → consensus → trust → extraction confidence
  │                            → freshness → MMR diversity
  ├─ 8. quick answer           best verbatim sentence(s), streamed immediately
  └─ 9. synthesis (optional)   mode-specific prompt, context sized to the model,
                               citations that resolve to the right source
```

- **Quick answer.** A sentence or two quoted from the sources, chosen by
  query-term coverage, page leads, and agreement across sources (the name or
  number most pages share). It penalises filler, restated questions and
  navigation debris. Because it's a quotation it can't be hallucinated, but
  it can be the wrong sentence, so treat it like a featured snippet.
- **Citations.** `[n]` in an answer links to the exact source the model saw
  as `[n]`. Each citation is checked against the cited text.
- **Context budgeting.** The prompt and answer are sized to the model's
  window. Local servers (Ollama, LM Studio, llama.cpp) are assumed to have
  4,096 tokens, because they silently drop the start of a longer prompt, and
  that's where the instructions are. Answers cut off at the output limit are
  flagged as partial.
- **Languages.** Hindi and other Indic scripts are tokenised correctly (a
  virama doesn't split words). The query's script picks the Wikipedia
  edition. Answers come back in the language of the question.

---

## The interface

Open `http://127.0.0.1:8000`. The UI is one self-contained HTML file built
into the binary.

- **Live progress.** Results stream over Server-Sent Events, with real stages
  (Search → Read → Rank → Write) and live counts. Sources and the quick answer
  appear before the AI answer is finished.
- **Growing query box.** The box expands as you type (up to a limit, then
  scrolls). Enter searches, Shift+Enter adds a line, and the box clears once
  the query is sent.
- **Mode pills and domain picker** right in the search box.
- **Sources sidebar.** Favicons, snippets and "N× agree" consensus badges; on
  phones it becomes a swipeable strip above the answer.
- **Answer view.** Rendered Markdown (tables, lists, code), numbered citation
  chips that link to the source, copy (with links) and listen (neural TTS).
- **Panels.** A history panel (opt-in, stored on your machine) and a settings
  drawer with presets for 14 providers and "Fetch list" model discovery.
- **Look and feel.** Light, dark or system theme; `/` or Ctrl+K to search,
  Esc to stop; mobile layout; reduced-motion support.

---

## Bring your own model

Settings → choose a provider → paste a key → pick a model. Supported:
OpenAI, Anthropic, Gemini, Groq, Cerebras, DeepSeek, xAI, OpenRouter,
Together, Fireworks, SambaNova, NVIDIA NIM, **Ollama** (local, no key) and any
**OpenAI-compatible** server.

**Key handling:**

- Keys are stored only in your browser.
- Keys are never logged, and are redacted if a provider echoes them in an
  error.
- Pasted keys are cleaned up: whitespace and a "Bearer " prefix are removed.
  Keys with hidden characters are rejected with a reason.
- A custom endpoint that fails validation is an error. Your key is never
  re-routed to another provider.

**Using a local model:**

```bash
ALLOW_LOCAL_LLM=1 ./target/release/searchwala
# Settings → Ollama → base URL http://localhost:11434/v1 → Fetch list
```

For longer answers, raise Ollama's window (`OLLAMA_CONTEXT_LENGTH=16384`) and
set `CONTEXT_MAX_TOKENS` to the same value.

---

## Quick start

```bash
cargo build --release
./target/release/searchwala            # http://127.0.0.1:8000
```

```bash
curl -X POST localhost:8000/search -H 'Content-Type: application/json' \
  -d '{"query":"how does mRNA vaccine technology work","focus_mode":"study"}'
```

Docker (listens on 7860, binds `0.0.0.0` inside the container):

```bash
docker build -t searchwala . && docker run -p 7860:7860 searchwala
```

---

## API

| Endpoint | Purpose |
|---|---|
| `POST /search` | Search, extract, and synthesize when `llm` is given. Body: `query`, `focus_mode` (`fast` / `auto` / `deep` / `study` / `academic` / `reddit` / `youtube`), optional `domain`, `max_results` (clamped to the mode's range), `llm` `{provider, api_key, model, base_url?, timeout_ms?}` |
| `POST /search/stream` | The same pipeline as Server-Sent Events: `plan`, `engines`, `sources`, `extracted`, `highlight`, `synthesizing`, then `result` (the full response) and `done` |
| `POST /search/auto-llm`, `POST /search/research-llm` | legacy aliases (`research-llm` forces Deep) |
| `GET /health` · `GET /health/engines` · `GET /health/cache` | liveness, per-engine verdicts, cache hit ratios |
| `GET /config` | version, engine roster, and every mode's profile |
| `POST /api/models` | list a provider's models (`provider`, `api_key`, optional `base_url`) |
| `GET /api/history` · `POST /api/history/{enable,disable}` · `DELETE /api/history` | opt-in local history |
| `GET /api/tts?text=…` | neural voice readback (needs `edge-tts`) |

A response includes `search_results` (consensus-ranked hits with
`found_by_families`), `results` (ranked passages), `highlight`, `llm_answer`
with `citation_map` and `citation_checks`, and `mode`, `tier`, `sub_queries`,
`sources_extracted` and timing.

---

## Configuration

All optional; the modes choose sensible budgets on their own.

| Variable | Default | Effect |
|---|---|---|
| `PORT` / `BIND` | `8000` / `127.0.0.1` | listen address; set `BIND=0.0.0.0` only behind auth |
| `ALLOW_LOCAL_LLM` | `0` | `1` allows a loopback/private LLM `base_url` (local models) |
| `ENGINES` | built-in roster | replace the general engine pool used by Auto and Deep |
| `STEALTH_MODE` / `STEALTH_ENGINES` | `auto` / built-in | force TLS impersonation on/off, or choose which engines use it |
| `PROXY_POOL` / `PROXY_POOL_FILE` / `TOR_PROXY_PORTS` | unset | outbound proxies for engine requests |
| `HOST_MIN_INTERVAL_MS` | `400` | spacing between requests to one host |
| `ENGINE_REQUEST_TIMEOUT_MS` | `4000` | per-request engine timeout (keep it below the mode's deadline) |
| `ENGINE_QUARANTINE` | on | `off` keeps dispatching engines that return nothing |
| `CONCURRENCY` / `ENGINE_CONCURRENCY` | `32` / `20` | concurrent scrapes / engine requests |
| `SCRAPE_STAGE_DEADLINE`, `SCRAPE_MIN_SOURCES`, `SCRAPE_MIN_CHARS` | per mode | global overrides of the modes' reading budgets |
| `MAX_HTML_BYTES` / `MIN_TEXT_CHARS` | built-in | page size cap / minimum usable text |
| `MULTI_QUERY` / `MAX_SUB_QUERIES` | on / per mode | sub-query decomposition |
| `CACHE` | on | `off` disables the SERP and page caches |
| `SERP_CACHE_TTL_SECS` / `PAGE_CACHE_TTL_SECS` | `600` / `21600` | cache lifetimes |
| `CONTEXT_MAX_TOKENS` / `OLLAMA_CONTEXT_LENGTH` | per model / unset | the model's whole window (prompt + answer) |
| `WIKIPEDIA_LANGS` | auto | unset: the query's script picks the edition; or force a list, e.g. `en,hi` |
| `OPENALEX_MAILTO` | unset | your email, for OpenAlex's polite pool |
| `SEARXNG_BASE_URL` | unset | federate a self-hosted SearXNG |
| `MARGINALIA_API_KEY` | `public` | a dedicated Marginalia key |
| `MAX_HISTORY_ENTRIES` | built-in | history size cap |
| `DUMP_SERP_DIR` | unset | save zero-result engine responses for parser debugging |

---

## Security

The server relays your LLM key and fetches URLs taken from search results, so
both directions are guarded:

- **Localhost only by default.** It binds `127.0.0.1`.
- **LLM endpoints are validated.**
  - Cloud metadata addresses are always refused.
  - Private and loopback addresses need `ALLOW_LOCAL_LLM=1`.
  - Redirects are not followed.
  - A rejected endpoint is an error and never falls back to another provider.
- **Scraping never reaches local addresses.** Page fetches use a resolver that
  drops private, loopback, link-local and metadata addresses (which also
  defeats DNS rebinding), and every redirect hop is re-checked.
- **Keys are never logged.** The config's `Debug` output redacts them, and
  provider errors are scrubbed.
- **History is opt-in** and stays on your disk (`~/.searchwala/`).
- **TTS is capped.** `/api/tts` is concurrency-limited.

CORS is permissive for local use. If you expose the service, put it behind a
reverse proxy with authentication.

---

## Testing

```bash
cargo test --release                            # 303 unit tests
cargo run --release --bin intent_probe          # intent accuracy: 50/50
cargo run --release --bin eval -- --limit 20 --focus fast   # retrieval quality (live engines)
```

Parser tests run against **real search results HTML** committed under
`tests/fixtures/`, so a selector that stops matching fails the test suite
instead of silently returning nothing. The extraction tests use real Hindi and
English articles, plus synthetic Shadow-DOM, Next.js and RSC pages. CI builds
with `-D warnings`.

---

## Architecture

```
src/
  main.rs             axum server, routes, SSE stream, TTS
  modes.rs            Mode × Domain → ModeProfile (every per-mode budget)
  search.rs           orchestration: dispatch → select → scrape → rank → answer
  synthesis.rs        per-mode prompts, Deep map-reduce, LLM query planning
  highlight.rs        verbatim quick answer
  llm.rs              providers, key/config validation, context budget, citations
  extractor.rs        rs-trafilatura + conditional dom_smoothie
  deep_reader.rs      shadow DOM, hydration data, microdata, density, AMP
  ranking.rs          family-aware RRF, BM25+, freshness, MMR
  query_intel.rs      intent, time sensitivity, decomposition
  fetcher.rs          Fetcher {Plain (reqwest), Stealth (wreq + BoringSSL)}
  host_gate.rs        per-host request spacing
  ssrf.rs             LLM endpoint validation, public-only scraping resolver
  cache_layer.rs      SERP + page caches        cache.rs   sessions + history
  engines/            bing, brave, duckduckgo, yahoo, mojeek, wikipedia, wiby,
                      marginalia_api, openalex, arxiv, searxng, generic (template
                      engines), health (verdicts, challenge and soft-block detection)
  bin/eval.rs         nDCG / MRR / Recall harness   bin/intent_probe.rs
ui.html               the single-file web interface
```

[`IMPLEMENTATION_BLUEPRINT.md`](IMPLEMENTATION_BLUEPRINT.md) is the
full engineering log of the v7 → v8.1 rebuild: every bug found, how it was
measured, and what changed.
