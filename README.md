<div align="center">

# ⚡ SearchWala

### Open-source meta-search engine in pure Rust

[![Version](https://img.shields.io/badge/version-8.1.0-blue?style=flat-square)](https://github.com/SandeepAi369/SearchWala)
[![Rust](https://img.shields.io/badge/rust-100%25-orange?style=flat-square&logo=rust)](https://www.rust-lang.org/)
[![Tests](https://img.shields.io/badge/tests-190-brightgreen?style=flat-square)](#testing)
[![License](https://img.shields.io/badge/license-Apache%202.0-purple?style=flat-square)](./LICENSE)

**Cross-engine consensus ranking** · **trafilatura + Readability extraction** · **BYOK LLM** · **single binary**

</div>

---

## What it does

Queries several independent search indexes in parallel, fuses their rankings by
cross-engine consensus, extracts the full article text from the top sources,
and optionally synthesizes a cited answer with any LLM you bring a key for.

Everything runs on your machine as one binary. No API keys are required for
search itself.

---

## Honest status

This project previously advertised "90+ search engines". That was not true, and
the number is now replaced by something you can check yourself.

**Free HTML scraping of major search engines is largely over.** Measured on
2026-10-01, from a normal residential/datacenter IP:

| Engine | Status |
|---|---|
| Google (all regions) | HTTP 200 but a JS interstitial, **0** result links. Retested with consent cookies, `SOCS`, `gbv=1`, `hl`/`gl` — no HTML path exists. |
| Bing | works with TLS impersonation (serves its homepage without it) |
| Brave | works with TLS impersonation (HTTP 429 without it) |
| DuckDuckGo | works with TLS impersonation (CAPTCHA without it); rate-limits quickly |
| Yahoo | works with TLS impersonation |
| Wikipedia, Marginalia, Wiby | JSON APIs, reliable |
| Mojeek, Ecosia, Mwmbl, Yandex | IP-dependent — 403/303 from some addresses |
| Startpage | serves a consent page, not results |
| Qwant | JS single-page app, no server-rendered results |
| 4get, Alexandria, Whoogle, Librex, Right DAO, Stract, Ask, Presearch, Yep, Dogpile, Excite, Lycos, Pipilika, SearchEncrypt, Kiddle, Gibiru | dead instances, moved endpoints, or JS/token walls |

**Check your own install rather than trusting this table:**

```bash
curl localhost:8000/health/engines
```

It reports, per engine: attempts, HTTP successes, blocks, transport errors,
**zero-result responses**, and a verdict of `healthy` / `degraded` /
`selector_rot` / `blocked` / `dead_host` / `unproven`. `zero_results` is the
important one — HTTP 200 with nothing parsed means the selectors have rotted,
which is otherwise indistinguishable from "no matches for this query".

To get more independent indexes, see [Expanding coverage](#expanding-coverage).

---

## Modes

Pick a **mode** for how much work to do, and optionally a **domain** for what
kind of sources to prefer. They compose: "Deep + Health" is a valid search.

| Mode | What it does | Typical time* |
|---|---|---|
| **Fast** | 5 reliable engines, 1 results page each, ~10 pages read, stops as soon as 3 independent indexes agree. Short cited answer. | ~2 s |
| **Auto** | Sizes itself to the question: navigational lookups behave like Fast; short facts read ~20 pages; complex, comparative or explanatory questions read ~55 pages with 3 sub-queries. Time-sensitive questions add news engines. | 2–6 s |
| **Deep Research** | Every engine plus news (and scholarly indices for research-type questions), up to 5 sub-queries — LLM-planned when a model is configured — and ~150 pages read. The report is written map-reduce: cited evidence notes per batch of sources, then one report from all the notes. | 20–30 s |
| **Study** | Educational sources preferred (Wikipedia, Britannica, Khan Academy, OpenStax, LibreTexts, NCERT, `.edu`…), "explained simply" and "examples" sub-queries, no recency bias. Produces study notes: overview, key concepts, explanation, worked example, common misconceptions, quick revision and a self-test. | ~6 s |
| **Academic** | Searches **OpenAlex** and **arXiv** directly and uses paper abstracts without fetching paywalled publisher pages. Produces a literature review; arXiv preprints are labelled as not peer-reviewed. | ~4 s |
| **Reddit** / **YouTube** | Site-restricted search through engines that support `site:`. Reddit threads are often behind a CAPTCHA, so the search snippet is used when the page can't be read; YouTube uses video descriptions (transcripts are not accessible). | ~5 s |

\* Search and extraction only, measured on one connection; LLM time comes on top.

**Domains** — `tech`, `science`, `finance`, `health`, `news` — add relevant
engines (news verticals, OpenAlex/arXiv), prefer that field's authorities when
choosing pages to read and when ranking passages (e.g. health → WHO, NIH, CDC,
NHS; finance → regulators, Reuters, Bloomberg, Indian market sources), and add
field-appropriate rules to the answer (health and finance carry disclaimers;
news gets a dated timeline).

`GET /config` returns the exact engines and budgets each mode uses.

**Local models.** Ollama, LM Studio and llama.cpp serve a 4,096-token window
by default and *silently* drop the start of a longer prompt — the instructions.
SearchWala assumes 4,096 tokens for any local server (loopback/private
`base_url`, or provider `ollama`) and fits prompt + answer inside it. Raise the
server's window (e.g. `OLLAMA_CONTEXT_LENGTH=16384`) and set
`CONTEXT_MAX_TOKENS` to the same value for longer answers; if an answer is cut
off at the output limit, the response says so.

**Soft-blocked engines.** When an engine decides a client is a bot it may
not show a CAPTCHA; it returns a normal-looking page of unrelated results
instead. In our tests Bing answered "capital of France" with pages about
Odisha and "mRNA vaccine mechanism" with adult sites. Every result page is
screened: a page where fewer than a quarter of the results mention the query
is discarded (and logged as a block in `/health/engines`), and a mixed page
keeps only its on-topic results.

---

## Quick start

```bash
cargo build --release
./target/release/searchwala
# http://127.0.0.1:8000
```

```bash
curl -X POST localhost:8000/search \
  -H 'Content-Type: application/json' \
  -d '{"query":"cricket world cup 2027 schedule","focus_mode":"auto","max_results":40}'
```

Docker:

```bash
docker build -t searchwala . && docker run -p 7860:7860 searchwala
```

---

## How it works

```
query
  ├─ 1. intent analysis        factual / temporal / person / comparison /
  │                            howto / navigational / research
  │                            100% accurate on the 50-query golden set
  ├─ 2. parallel dispatch      independent indexes, per-host rate gating,
  │                            TLS impersonation where the engine requires it
  ├─ 3. consensus ranking      RRF (k=60) collapsed by ENGINE FAMILY, so 15
  │                            Bing regional variants count once, not 15 times
  ├─ 4. source selection       top-N by consensus score (not arrival order)
  ├─ 5. extraction             rs-trafilatura, with Readability as a
  │                            conditional second opinion; yields text,
  │                            publication date, language and a confidence
  ├─ 6. chunk ranking          BM25+ → trust prior → extraction confidence →
  │                            freshness decay → MMR diversity
  └─ 7. synthesis (optional)   model-scaled context, lost-in-middle ordering,
                               citations that resolve to the right source
```

### Cross-engine consensus

Many "meta-search" result counts are inflated because the same index is
counted repeatedly under different names. SearchWala groups engines into
**families** — all Bing regional variants, plus DuckDuckGo and Yahoo which are
Bing-derived, are one family; Startpage and Ecosia belong to Google's. RRF then
fuses across families, so `found_by_families: 3` means three genuinely
independent indexes agreed.

### Extraction

The extractor choice was benchmarked, not assumed, on a 10-page corpus
committed under `tests/fixtures/articles/`:

| extractor | chars extracted | time |
|---|---|---|
| hand-rolled (v7.0) | 76,462 | 55 ms |
| rs-trafilatura | 117,032 | 374 ms |
| Readability (`dom_smoothie`) | 102,248 | 78 ms |
| **trafilatura + conditional Readability** | **120,898** | **416 ms** |

Trafilatura leads overall and is the only one that also returns publication
date, language and a quality score. Readability wins on 6 of 10 pages though,
sometimes by 2× — so it runs as a second opinion when the primary output looks
thin for the page, and the longer result wins.

**Deep reader** (`src/deep_reader.rs`). Both extractors read the DOM, and
some pages keep their text elsewhere. Measured on synthetic pages, they
returned nothing for declarative Shadow DOM (`<template shadowrootmode>`),
Next.js `__NEXT_DATA__` pages and Next.js App Router (RSC `self.__next_f`)
pages. Those are now handled:

| page shape | how |
|---|---|
| Declarative Shadow DOM | shadow-root templates are unwrapped before extraction |
| JS shells: Next.js, Nuxt, Remix, Redux/Apollo state, Astro islands | prose strings harvested from the hydration data (nested RSC payloads decoded) |
| paywall teaser in the DOM, full text in JSON-LD / microdata | `articleBody` wins when it is clearly longer |
| everything thin | text-density / link-density block scoring |
| canonical page unreadable (non-Fast modes) | one fetch of the page's AMP variant |

`<noscript>`, `hidden`, `display:none` and `<details>` content was already
extracted and still is. On the 10-page corpus the output is unchanged
(+5% time).

### Quick answer

Every search returns a `highlight`: one or two sentences quoted verbatim from
the sources, chosen by query-term coverage, page leads, redundancy across
sources (the name or number most sources agree on), and penalties for
filler, restated questions and navigation debris. It streams before the AI
answer and is the answer when no model is configured. It is a quotation, so
it cannot be hallucinated, but it can be the wrong sentence: treat it like a
featured snippet.

### Non-Latin scripts

Sentence segmentation recognises 13 terminators including `।` (Devanagari
danda), `。`, `؟` and Thai/Myanmar/Ethiopic marks, and does not require an
uppercase next character for uncased scripts. Length gates count **characters**,
not bytes. Answers are returned in the language the question was asked in.

---

## Configuration

All optional. Defaults are chosen to work without configuration.

| Variable | Default | Purpose |
|---|---|---|
| `PORT` | `8000` | listen port |
| `BIND` | `127.0.0.1` | set `0.0.0.0` to expose on the network |
| `ENGINES` | from code | replaces the general web engine pool used by Auto and Deep |
| `CONCURRENCY` | `32` | concurrent scrapes |
| `SCRAPE_STAGE_DEADLINE`, `SCRAPE_MIN_SOURCES`, `SCRAPE_MIN_CHARS` | per mode | global overrides of the modes' scrape budgets |
| `EARLY_EXIT_MIN_RESULTS` / `EARLY_EXIT_MIN_FAMILIES` | per mode | force an engine-stage early exit (only Fast uses one by default) |
| `OPENALEX_MAILTO` | unset | your email, to join OpenAlex's polite pool |
| `ENGINE_REQUEST_TIMEOUT_MS` | `4000` | **must stay below the dispatch deadline** |
| `HOST_MIN_INTERVAL_MS` | `400` | per-host request spacing |
| `STEALTH_MODE` | `auto` | `on` / `off` / `auto` (engine decides) |
| `CACHE` | on | `off` disables both cache tiers |
| `SERP_CACHE_TTL_SECS` / `PAGE_CACHE_TTL_SECS` | `600` / `21600` | cache lifetimes |
| `CONTEXT_MAX_TOKENS` | per model; 4096 for local servers | the model's whole window (prompt + answer) |
| `OLLAMA_CONTEXT_LENGTH` | unset | honoured as the window for local servers when `CONTEXT_MAX_TOKENS` is unset |
| `ALLOW_LOCAL_LLM` | `0` | `1` permits a loopback/private `base_url` for a local model |
| `WIKIPEDIA_LANGS` | auto | Unset: the query's script picks the edition (Devanagari → `hi`, Tamil → `ta`, …) with English as fallback. Set e.g. `en,hi` to force a list |
| `SEARXNG_BASE_URL` | unset | a self-hosted SearXNG instance to federate |
| `MARGINALIA_API_KEY` | `public` | a dedicated Marginalia key |
| `DUMP_SERP_DIR` | unset | save zero-result response bodies for parser debugging |
| `MULTI_QUERY` | `true` | sub-query decomposition (count is per mode) |
| `ENGINE_QUARANTINE` | on | `off` keeps dispatching engines that produce nothing |

---

## Expanding coverage

Five independent families is what free HTML scraping yields from a typical IP.
Three legitimate routes past that, in order of practicality:

1. **Self-hosted SearXNG** — federates ~80 engines and maintains those parsers
   for you, using your own IP and rate budget.
   ```bash
   docker run -d -p 8080:8080 -e SEARXNG_SECRET=$(openssl rand -hex 32) searxng/searxng
   # enable the `json` format in settings.yml, then:
   export SEARXNG_BASE_URL=http://localhost:8080
   ```
   Public instances are not supported and none are shipped — measured 429/403,
   and pointing at someone else's instance without permission is rude.
2. **BYOK search APIs.** Note the 2026 pricing: Brave removed its free Search
   API tier in February 2026 (~$5/1k requests, with $5/month of credits ≈ 1,000
   free queries). Mojeek's Web Search API is a negotiated B2B arrangement.
3. **A proxy pool** — `PROXY_POOL` / `PROXY_POOL_FILE` / `TOR_PROXY_PORTS`.
   Residential egress typically recovers Mojeek, Ecosia and Mwmbl.

---

## API

| Endpoint | Purpose |
|---|---|
| `POST /search` | search + extract (+ synthesis when `llm` is supplied). Body: `query`, `focus_mode` (`fast`/`auto`/`deep`/`study`/`academic`/`reddit`/`youtube`), optional `domain`, optional `max_results` (clamped to the mode's range), optional `llm` |
| `POST /search/auto-llm`, `POST /search/research-llm` | legacy aliases (research-llm forces Deep) |
| `POST /search/stream` | Same pipeline as `/search`, as Server-Sent Events: `plan`, `engines`, `sources`, `extracted`, `synthesizing`, then `result` (the full response) and `done` |
| `GET /health` | liveness + version |
| `GET /health/engines` | **per-engine liveness and verdicts** |
| `GET /health/cache` | cache hit ratios |
| `GET /config` | effective configuration |
| `POST /api/models` | list a provider's models |
| `GET /api/history`, `POST /api/history/{enable,disable}`, `DELETE /api/history` | opt-in history |
| `GET /api/tts?text=…` | neural voice readback (needs `edge-tts`) |

`POST /search` returns, among other fields:

```jsonc
{
  "mode": "auto", "tier": "auto:light", "domain": "general",
  "sub_queries": ["capital of australia"],
  "sources_extracted": 18, "chars_extracted": 402113,
  "search_results": [{
    "url": "…", "title": "…", "snippet": "…",
    "engine": "wikipedia,duckduckgo,brave",  // every engine that found it
    "rrf_score": 0.0696,
    "found_by_families": 3                   // INDEPENDENT indexes agreeing
  }],
  "results": [{ "url": "…", "extracted_text": "…", "char_count": 5812,
                "published": "2026-10-01T15:41:02Z", "language": "hi",
                "confidence": 0.95, "extractor": "trafilatura" }],
  "citation_map": [4, 0, 7],      // citation [n] -> index into `results`
  "citation_checks": [{ "number": 1, "url": "…", "overlap": 0.62, "supported": true }]
}
```

---

## Security

This process relays your BYOK LLM key to whatever `base_url` a request names,
so a few things are deliberate:

- **Binds `127.0.0.1` by default.** Set `BIND=0.0.0.0` only when you mean it.
- **`base_url` is validated.** Cloud metadata addresses (`169.254.169.254` and
  friends) are refused unconditionally; loopback and private ranges need
  `ALLOW_LOCAL_LLM=1`, so local models still work.
- **API keys are never logged** — `LlmConfig`'s `Debug` redacts them.
- **`/api/tts` is concurrency-capped** and cleans up its temp file on every
  path.
- **Scraping never reaches local addresses.** Result URLs are
  attacker-influenced (anyone can publish a page that ranks and redirects).
  Page fetches use a resolver that drops private, loopback, link-local and
  metadata addresses (which also defeats DNS rebinding), and every redirect
  hop is re-checked. Before v8.1 the scraper had no such guard.

CORS is still permissive for local UI convenience. If you expose this service,
put it behind a reverse proxy with authentication.

---

## Testing

```bash
cargo test --release                              # 190 unit tests
cargo run --release --bin intent_probe            # intent accuracy on the golden set
cargo run --release --bin eval -- --limit 20      # retrieval quality (live engines)
cargo run --release --bin eval -- --baseline benchmarks/baseline_v8.json
```

Parser tests run against **real SERP HTML** committed under `tests/fixtures/`,
so selector rot fails the test suite instead of silently returning zero
results. The extraction test asserts against a real Hindi news article.

`cargo run --bin eval` reports nDCG@10, MRR, Recall@k and latency percentiles
over the 50 hand-labelled queries in `benchmarks/golden_queries.json`.

Current baseline (live engines, so it moves with engine availability):

| metric | value |
|---|---|
| nDCG@10 | 0.352 |
| MRR | 0.548 |
| Recall@10 / @20 | 0.327 / 0.375 |
| intent accuracy | **100%** (50/50) |
| extraction confidence | 0.979 mean |
| p50 / p95 latency | 5.9 s / 7.8 s |

---

## Architecture

```
src/
  main.rs            axum server, routes, TTS
  lib.rs             library target (so the eval harness can drive the pipeline)
  modes.rs           Mode/Domain → ModeProfile: every per-mode budget in one place
  search.rs          orchestration: dispatch → select → scrape → rank → synthesize
  synthesis.rs       per-mode prompts; Deep map-reduce; LLM query planning
  fetcher.rs         Fetcher{Plain,Stealth} — one surface over reqwest + wreq
  host_gate.rs       per-registrable-host request spacing
  engines/
    mod.rs           trait, roster, engine FAMILIES and weights
    health.rs        per-engine counters, verdicts, auto-quarantine
    bing.rs brave.rs duckduckgo.rs mojeek.rs qwant.rs startpage.rs yahoo.rs
    wikipedia.rs wiby.rs marginalia_api.rs searxng.rs openalex.rs arxiv.rs
    generic.rs       template-driven HTML engines
  extractor.rs       trafilatura + conditional Readability
  ranking.rs         RRF, BM25+, freshness, MMR, script-aware chunking
  llm.rs             provider-agnostic synthesis, context budget, citations
  query_intel.rs     intent, temporal detection, decomposition
  url_utils.rs       canonical_key — the single page-identity function
  cache_layer.rs     SERP + page caches
  cache.rs           session store + append-only history
  ssrf.rs            base_url validation
  eval.rs            nDCG/MRR/Recall metrics
  bin/eval.rs        evaluation runner
  bin/intent_probe.rs intent accuracy probe
```

See [`docs/IMPLEMENTATION_BLUEPRINT.md`](docs/IMPLEMENTATION_BLUEPRINT.md) for
the full v7→v8 audit: every bug found, how it was measured, and what changed.

---

## License

Apache-2.0
