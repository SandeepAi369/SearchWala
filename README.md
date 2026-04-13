<div align="center">

# ⚡ Qrux

### The Fastest Open-Source Meta-Search Engine — Built in Pure Rust

[![Version](https://img.shields.io/badge/version-5.0.1-blue?style=flat-square)](https://github.com/SandeepAi369/Qrux)
[![Rust](https://img.shields.io/badge/rust-100%25-orange?style=flat-square&logo=rust)](https://www.rust-lang.org/)
[![Engines](https://img.shields.io/badge/search%20engines-90+-brightgreen?style=flat-square)](https://github.com/SandeepAi369/Qrux)
[![License](https://img.shields.io/badge/license-Apache%202.0-purple?style=flat-square)](./LICENSE)

**90+ search engines** · **Stealth WAF bypass** · **BM25 ranking** · **5-tier content extraction** · **BYOK LLM synthesis**

*A single Rust binary that queries 90+ search engines simultaneously, extracts clean article text from every result, ranks them with BM25, and optionally synthesizes answers using any LLM provider — all without a single line of Python, Node, or Java.*

[Quick Start](#-quick-start) · [How It's Different](#-how-its-different) · [Architecture](#-architecture) · [API Reference](#-api-reference) · [Configuration](#%EF%B8%8F-configuration)

</div>

---

## 🏆 How It's Different

Most meta-search tools (SearXNG, Searx, etc.) simply proxy queries and return URLs. Qrux goes **5 levels deeper**:

| Capability | SearXNG | Perplexity | **Qrux** |
|---|:---:|:---:|:---:|
| Meta-search across engines | ✅ ~70 | ❌ proprietary | ✅ **90+ engines** |
| Full article text extraction | ❌ | ❌ (summary only) | ✅ **5-tier extractor** |
| BM25 relevance ranking | ❌ | ❌ | ✅ **paragraph-level** |
| Anti-bot / WAF stealth | ❌ | N/A | ✅ **18 browser profiles** |
| Iterative deep research | ❌ | ✅ (paid) | ✅ **multi-batch** |
| Domain-specialized search | ❌ | ❌ | ✅ **5 domain modes** |
| Self-hosted / no API keys | ✅ | ❌ | ✅ **zero dependencies** |
| LLM provider (BYOK) | ❌ | ✅ (locked) | ✅ **15+ providers** |
| SSE real-time streaming | ❌ | ✅ | ✅ **native SSE** |
| Smart engine fallback | ❌ | N/A | ✅ **2-phase dispatch** |
| Proxy pool + Tor support | partial | ❌ | ✅ **round-robin + cooldown** |
| Pure Rust / single binary | ❌ (Python) | ❌ (cloud) | ✅ **~15MB binary** |

### What Makes This Unique

<details>
<summary><b>🔍 90+ Search Engines — The Widest Coverage in Any Open-Source Tool</b></summary>

Not just "supports" — actually **queries them in parallel** with query snowballing:
- **Major**: Google (14 regional variants), Bing (14 regional), DuckDuckGo, Brave, Yahoo
- **Privacy**: Startpage, Qwant, Mojeek, Swisscows, MetaGer, Search Encrypt, Presearch
- **Academic**: Google Scholar, Wikipedia (API-native)
- **Regional**: Yandex, Baidu, Sogou, Naver, Daum, Seznam, Rambler
- **Independent**: Wiby, Marginalia, Stract, Right DAO, Mwmbl, Yep
- **Aggregators**: Dogpile, WebCrawler, Info, Excite, Lycos, AOL
- **Vertical**: Google News, Bing News, Yahoo News, Brave News, DDG News/Images/Videos, Bing Images/Videos, Google Images/Videos

Each engine has a **dedicated HTML parser** — no API keys needed, no rate-limit dependencies.
</details>

<details>
<summary><b>🛡️ Military-Grade Stealth — 18 Browser Fingerprints</b></summary>

Every request rotates through **18 real browser profiles** with:
- Realistic `User-Agent` strings (Chrome 127–131, Firefox 128–133, Edge 131, Safari 18.2)
- Full `Sec-CH-UA` client hint suite (Arch, Bitness, Platform-Version, Full-Version-List)
- Randomized `Accept` header variants to evade fingerprint correlation
- Per-request cookie isolation (no cross-request state leaks)
- Configurable jitter timing between requests (50–200ms default)
- Optional proxy pool with health tracking and auto-cooldown
- Tor SOCKS5 proxy integration (multi-port)

This bypasses Cloudflare, Akamai, and Imperva WAFs consistently — something no other open-source search engine even attempts.
</details>

<details>
<summary><b>📖 5-Tier Content Extraction — Not Just URLs</b></summary>

While other search tools give you links, Qrux **scrapes and extracts the actual article text**:

1. **Structured Selectors** — `.entry-content`, `.article-body`, `#main-content` (35+ CMS patterns)
2. **Semantic HTML5** — `<article>`, `<main>`, `[role="main"]`, `[itemprop="articleBody"]`
3. **Scored Container** — Text-density scoring with link-ratio penalty (trafilatura-inspired)
4. **Content Elements** — `<p>`, `<li>`, `<blockquote>`, `<pre>` fallback collection
5. **Full Body** — Last-resort visible text extraction with boilerplate filtering

Plus: paragraph deduplication, boilerplate line regex filtering, and per-paragraph fingerprinting.
</details>

<details>
<summary><b>🧠 BM25 Paragraph-Level Ranking</b></summary>

Raw results aren't enough — relevance matters. Qrux breaks every scraped article into paragraph-sized chunks and scores them using the **Okapi BM25 algorithm** (the same ranking model underlying Elasticsearch):

- Term frequency (TF) analysis per chunk
- Inverse document frequency (IDF) across all chunks
- Document length normalization
- Exact phrase match bonus (+1.25 score)
- Configurable K1 (1.2) and B (0.75) parameters

The top-K most relevant chunks are passed to the LLM — not raw pages — giving dramatically better synthesis quality.
</details>

<details>
<summary><b>🔬 Deep Research Mode — Multi-Batch Iterative Synthesis</b></summary>

Unlike simple "search and summarize" tools, Deep Research mode:
1. Queries **all 90+ engines** with 3 query variations (snowballing)
2. Scrapes **200+ sources** concurrently
3. Splits results into **batches of 50**
4. Synthesizes each batch iteratively — each batch builds on the previous report
5. Produces a **comprehensive research paper** with proper source citations

This is the open-source equivalent of Perplexity Pro Search — without the subscription.
</details>

<details>
<summary><b>🎯 Domain-Specialized Search</b></summary>

5 curated domain modes with optimized engine sets and query handling:

| Domain | Focus Engines | Use Case |
|---|---|---|
| 💻 **Tech** | Stack Overflow, GitHub, HN, dev blogs | Programming, APIs, DevOps |
| 🧬 **Science** | Google Scholar, PubMed, arXiv, Nature | Research papers, studies |
| 📊 **Finance** | Bloomberg, Reuters, Yahoo Finance | Markets, earnings, macro |
| 🏥 **Health** | NIH, WHO, Mayo Clinic, medical journals | Medical, clinical data |
| 📰 **News** | All news-specific engine variants | Breaking news, current events |

Each mode uses a separate **Category pill** in the UI — composable with any search mode (Lite + Tech, Research + Science, etc.)
</details>

---

## 🏗️ Architecture

```
  Client Request           Qrux v5.0.1
  ┌──────────┐        ┌────────────────────────────────────────────────┐
  │ POST     │        │                                                │
  │ /search  │───────►│  1. Query Snowballing (3 variations)           │
  │          │        │  2. 90+ Engine Dispatch (semaphore-bounded)    │
  │          │        │  3. Smart Fallback (primary → backup engines)  │
  │          │        │  4. URL Dedup (single-parse pipeline)          │
  │          │        │  5. Concurrent Scrape (24 workers)             │
  │          │        │  6. 5-Tier Content Extraction                  │
  │          │        │  7. BM25 Paragraph Ranking                     │
  │          │        │  8. Optional LLM Synthesis (BYOK)              │
  │          │        │                                                │
  │  ◄───────┤────────│  Response: sources + extracted text + answer   │
  └──────────┘        └────────────────────────────────────────────────┘
```

### Smart Engine Fallback (2-Phase Dispatch)

```
Phase 1: Primary engines (fast, reliable)
    │
    ├── Results >= 8? ──► Continue to scraping
    │
    └── Results < 8? ──► Phase 2: Backup engines (18 alternatives)
                              └──► Guarantees data even when top engines fail
```

---

## 📁 Project Structure

```
Qrux/
├── Cargo.toml               # Dependencies & release optimizations (LTO, strip)
├── Dockerfile               # Multi-stage Docker build (~15MB final image)
├── LICENSE                   # Apache 2.0
├── README.md
├── ui.html                  # Perplexity-style search interface (embedded at compile time)
├── scripts/
│   ├── ram_monitor.sh       # Memory usage monitoring utility
│   └── test_fallback.py     # Engine fallback integration test
└── src/
    ├── main.rs              # Axum HTTP server — routes, middleware, SSE
    ├── config.rs            # 18 browser profiles, WAF bypass, env config
    ├── models.rs            # Request/Response types (serde JSON)
    ├── search.rs            # Search orchestration + 2-phase engine dispatch
    ├── stream.rs            # SSE streaming pipeline (/search/stream)
    ├── ranking.rs           # BM25 paragraph chunking & relevance ranking
    ├── llm.rs               # BYOK LLM: 15+ providers, iterative research synthesis
    ├── extractor.rs         # 5-tier content extraction (LazyLock optimized)
    ├── url_utils.rs         # URL normalization, single-parse dedup pipeline
    ├── cache.rs             # TempDb (in-memory) + HistoryDb (persistent JSON)
    ├── copilot.rs           # LLM-powered query rewriter
    ├── proxy_pool.rs        # Round-robin proxy rotation with health tracking
    └── engines/
        ├── mod.rs           # SearchEngine trait + engine factory + domain modes
        ├── generic.rs       # Template engine for 60+ regional variants
        ├── duckduckgo.rs    # DuckDuckGo HTML scraper
        ├── brave.rs         # Brave Search scraper
        ├── yahoo.rs         # Yahoo Search scraper
        ├── qwant.rs         # Qwant scraper
        ├── mojeek.rs        # Mojeek scraper
        ├── startpage.rs     # Startpage scraper
        ├── wikipedia.rs     # Wikipedia JSON API engine
        └── wiby.rs          # Wiby indie search engine
```

**Total**: ~6,200 lines of pure Rust · Zero Python · Zero Node · Zero Java

---

## ⚡ Quick Start

### Build from Source

```bash
git clone https://github.com/SandeepAi369/Qrux.git
cd Qrux

# Build optimized release binary
cargo build --release

# Run (starts on http://localhost:8000)
./target/release/qrux
```

### Docker

```bash
docker build -t qrux .
docker run -p 8000:8000 qrux
```

### Verify

```bash
# Health check
curl http://localhost:8000/health

# Basic search (returns sources + extracted text)
curl -X POST http://localhost:8000/search \
  -H "Content-Type: application/json" \
  -d '{"query": "quantum computing breakthroughs 2026"}'

# Search with LLM answer (BYOK — bring your own key)
curl -X POST http://localhost:8000/search/lite-llm \
  -H "Content-Type: application/json" \
  -d '{
    "query": "explain transformer architecture",
    "llm": {
      "provider": "groq",
      "api_key": "YOUR_KEY",
      "model": "llama-3.3-70b-versatile",
      "base_url": "https://api.groq.com/openai/v1"
    }
  }'
```

### Open the UI

Navigate to `http://localhost:8000` for the built-in Perplexity-style search interface with:
- Mode selector (Lite / Deep Research / Academic / Reddit / YouTube)
- Category selector (Tech / Science / Finance / Health / News)
- Real-time searching animation with orbital spinner
- Word-by-word typewriter LLM response rendering
- Text-to-Speech with chunk pre-loading
- Settings panel for LLM provider configuration

---

## 📡 API Reference

### `POST /search`

**Standard search** — returns sources with extracted article text.

```json
// Request
{
  "query": "artificial intelligence trends 2026",
  "max_results": 30,
  "focus_mode": "lite"
}

// Response
{
  "query": "artificial intelligence trends 2026",
  "sources_found": 287,
  "sources_processed": 142,
  "search_results": [
    {
      "url": "https://www.nature.com/articles/...",
      "title": "AI breakthroughs reshape scientific discovery",
      "extracted_text": "Full article text extracted via 5-tier heuristics...",
      "char_count": 7270,
      "engine": "google_scholar"
    }
  ],
  "elapsed_seconds": 4.28,
  "engine_stats": {
    "engines_queried": ["wikipedia", "duckduckgo", "brave", "google", "..."],
    "total_raw_results": 322,
    "deduplicated_urls": 142
  }
}
```

### `POST /search/lite-llm`

Search + LLM synthesis (fast, single-pass). Requires `llm` config in request body.

### `POST /search/research-llm`

Deep Research — iterative multi-batch LLM synthesis over 200+ sources.

### `POST /search/stream`

SSE streaming endpoint — real-time source delivery + LLM token streaming.

### `GET /health`

```json
{
  "status": "ok",
  "version": "5.0.1",
  "engines": ["wikipedia", "duckduckgo", "brave", "...90 total..."],
  "uptime_seconds": 3600
}
```

### `GET /config`

Returns current runtime configuration (concurrency, timeouts, engine list, proxy status).

### `POST /api/tts`

Text-to-Speech synthesis via external TTS provider.

### `POST /api/models`

Dynamic model discovery — fetches available models from any OpenAI-compatible endpoint.

---

## 🔌 Supported LLM Providers

| Provider | Default Model | Notes |
|---|---|---|
| **Cerebras** | `llama-3.3-70b` | Fastest inference |
| **Groq** | `llama-3.3-70b-versatile` | Free tier available |
| **OpenAI** | `gpt-4o-mini` | GPT family |
| **Anthropic** | `claude-3-5-haiku-latest` | Claude family |
| **Google Gemini** | `gemini-2.0-flash` | Gemini family |
| **xAI** | `grok-2-latest` | Grok family |
| **DeepSeek** | `deepseek-chat` | Cost-effective |
| **Ollama** | `llama3` | Local / self-hosted |
| **OpenRouter** | `openai/gpt-4o-mini` | Multi-model router |
| **Together AI** | `llama-3.1-70B-Instruct-Turbo` | Open-source models |
| **Fireworks AI** | `llama-v3p1-70b-instruct` | Fast open-source |
| **SambaNova** | `Meta-Llama-3.1-70B-Instruct` | Enterprise |
| **NVIDIA NIM** | `llama-3.1-70b-instruct` | GPU-optimized |
| **Any OpenAI-compatible** | Custom | Any `/v1/chat/completions` endpoint |

---

## ⚙️ Configuration

All environment variables are optional — sensible defaults built-in.

### Search & Scraping

| Variable | Default | Description |
|---|---|---|
| `ENGINES` | 90 engines (curated) | Comma-separated engine names to enable |
| `MAX_URLS` | `420` | Maximum URLs to scrape per query |
| `CONCURRENCY` | `24` | Concurrent scrape workers |
| `ENGINE_CONCURRENCY` | `10` | Concurrent engine-query workers |
| `JITTER_MIN_MS` | `50` | Min random delay between engine requests (stealth) |
| `JITTER_MAX_MS` | `200` | Max random delay between engine requests (stealth) |
| `SCRAPE_TIMEOUT` | `0` | Per-URL scrape timeout in seconds |
| `MAX_HTML_BYTES` | `1500000` | Max HTML download size per page |

### Proxy & Stealth

| Variable | Default | Description |
|---|---|---|
| `PROXY_POOL` | *(empty)* | Comma-separated proxy URLs |
| `PROXY_POOL_FILE` | *(empty)* | File path with one proxy URL per line |
| `TOR_PROXY_PORTS` | *(empty)* | Comma-separated local Tor SOCKS5 ports |
| `PROXY_COOLDOWN_SECS` | `120` | Cooldown window after proxy failure |

### Server

| Variable | Default | Description |
|---|---|---|
| `PORT` | `8000` | HTTP server listen port |
| `RUST_LOG` | `qrux=info` | Log verbosity level |

---

## 🔒 Privacy & Security

- **Zero telemetry** — no tracking, no analytics, no phone-home
- **No cloud dependencies** — runs entirely on your hardware
- **No API keys required** — all 90 engines work without any API registration
- **Cookie isolation** — every request uses a fresh HTTP client (no cross-request state)
- **Tracking param removal** — strips 30+ UTM/analytics parameters from every URL
- **Domain blocklist** — auto-skips social media feeds, app stores, and binary file URLs
- **Optional BYOK LLM** — AI synthesis is opt-in; raw results always available
- **No data persistence** — search history is optional and local-only

---

## 📊 Performance Characteristics

| Metric | Lite Mode | Deep Research Mode |
|---|---|---|
| Engines queried | 11 primary + fallback | 90+ (3 query variations) |
| Sources scraped | 30–80 | 200–400+ |
| Time to results | 3–8 seconds | 15–45 seconds |
| LLM context quality | Top 25 BM25 chunks | Full iterative batches |
| Memory footprint | ~30MB RSS | ~80MB RSS peak |
| Binary size | ~15MB (stripped, LTO) | Same binary |

---

## 🗺️ Roadmap

- [ ] Response compression (gzip/brotli for API responses)
- [ ] Built-in caching layer with TTL
- [ ] Citation graph visualization
- [ ] Plugin system for custom engines
- [ ] WebSocket streaming support
- [ ] Multi-language query support

---

## 📄 License

Copyright 2026 [Sandeep](https://xel-studio.vercel.app/)

Licensed under the [Apache License, Version 2.0](./LICENSE).

---

<p align="center">
  <strong>Built with 🦀 Rust by <a href="https://xel-studio.vercel.app/">Sandeep</a></strong>
  <br>
  <sub>6,200 lines of pure Rust · Zero external runtime dependencies · One binary to rule them all</sub>
</p>
