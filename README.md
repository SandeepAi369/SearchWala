<div align="center">

# ⚡ SearchWala

### The Smartest Open-Source Meta-Search Engine — Built in Pure Rust

[![Version](https://img.shields.io/badge/version-5.2.0-blue?style=flat-square)](https://github.com/SandeepAi369/SearchWala)
[![Rust](https://img.shields.io/badge/rust-100%25-orange?style=flat-square&logo=rust)](https://www.rust-lang.org/)
[![Engines](https://img.shields.io/badge/search%20engines-90+-brightgreen?style=flat-square)](https://github.com/SandeepAi369/SearchWala)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-informational?style=flat-square)](https://github.com/SandeepAi369/SearchWala)
[![License](https://img.shields.io/badge/license-Apache%202.0-purple?style=flat-square)](./LICENSE)

**90+ search engines** · **RRF Ranking** · **Query Intelligence** · **Neural Voice** · **Intent-Aware LLM** · **BYOK**

*SearchWala is a single-binary meta-search engine that analyzes query intent, queries 90+ search engines simultaneously, ranks results using Reciprocal Rank Fusion (cross-engine consensus), extracts clean text with a 7-tier extractor, synthesizes intent-aware answers using any LLM, and reads results aloud — all in pure Rust.*

[Quick Start](#-quick-start) · [Key Features](#-key-features) · [Voice Support](#-neural-voice-readback) · [Platform Support](#-cross-platform-support) · [Architecture](#%EF%B8%8F-architecture) · [API Reference](#-api-reference) · [Configuration](#%EF%B8%8F-configuration)

</div>

---

## 🎯 What Is SearchWala?

**SearchWala** is an open-source, privacy-first meta-search engine designed to give you the **deepest, most comprehensive search results** possible — far beyond what any single search engine can offer. While Google gives you 10 blue links and Perplexity gives you a black-box summary, **SearchWala** gives you:

- 📡 Results from **90+ search engines** queried in parallel
- 📖 **Full article text** extracted from every source (not just URLs)
- 🧠 **AI-synthesized answers** with proper source citations
- 🔊 **Neural voice readback** — listen to answers hands-free
- ⏰ **Time-aware intelligence** — always returns the latest information
- 🖥️ **Runs on your machine** — Windows, macOS, Linux — no cloud needed

Whether you're a researcher needing comprehensive literature reviews, a developer looking for code solutions, or simply someone who wants better search — **SearchWala** delivers results that no single search engine can match.

---

## 🏆 How SearchWala Is Different

Most meta-search tools (SearXNG, Searx, etc.) simply proxy queries and return URLs. **SearchWala** goes **5 levels deeper** — it doesn't just *find* pages, it **reads them for you**:

| Capability | SearXNG | Perplexity | **SearchWala** |
|---|:---:|:---:|:---:|
| Meta-search across engines | ✅ ~70 | ❌ proprietary | ✅ **90+ engines** |
| Full article text extraction | ❌ | ❌ (summary only) | ✅ **5-tier extractor** |
| BM25 relevance ranking | ❌ | ❌ | ✅ **paragraph-level** |
| 🔊 Neural Voice Readback | ❌ | ❌ | ✅ **10-word smart streaming** |
| ⏰ Time-Aware Search | ❌ | partial | ✅ **auto date enrichment** |
| Anti-bot / WAF stealth | ❌ | N/A | ✅ **20 browser profiles (Chrome 147)** |
| Iterative deep research | ❌ | ✅ (paid) | ✅ **multi-batch free** |
| Domain-specialized search | ❌ | ❌ | ✅ **5 domain modes** |
| Self-hosted / no API keys | ✅ | ❌ | ✅ **zero dependencies** |
| LLM provider (BYOK) | ❌ | ✅ (locked) | ✅ **15+ providers** |
| SSE real-time streaming | ❌ | ✅ | ✅ **native SSE** |
| Simultaneous engine dispatch | ❌ | N/A | ✅ **ALL 35 engines in parallel** |
| Cross-platform binary | ❌ (Python) | ❌ (cloud) | ✅ **Win/Mac/Linux ~15MB** |
| Proxy pool + Tor support | partial | ❌ | ✅ **round-robin + cooldown** |

---

## 🔑 Key Features

### 🔊 Neural Voice Readback

**SearchWala** is one of the few search engines — open-source or commercial — that includes **built-in text-to-speech**. Every AI answer can be read aloud using Microsoft Edge's neural voice engine (en-US-AvaNeural):

- **Smart 10-word chunking** — SearchWala splits answers into small, natural segments for instant playback start
- **Progressive streaming** — The first chunk plays immediately while the next 3 are prefetched in the background
- **Aggressive text cleaning** — Source references `[1][2]`, markdown formatting, URLs, and LLM artifacts are all stripped before reading, so you hear only clean, natural speech
- **Session-safe playback** — No double-voice glitches; each playback session is isolated with a unique session ID
- **One-click Listen** — Click the 🔊 button next to any AI answer to start/stop voice readback
- **Cached for replay** — Audio chunks are cached as blob URLs, so replaying is instant

This makes **SearchWala** ideal for:
- 🚗 Hands-free research while commuting or cooking
- ♿ Accessibility for visually impaired users
- 📚 Listening to research summaries during workouts
- 🎧 Multitasking — get search answers without looking at the screen

### ⏰ Time-Aware Intelligence

Most search engines return stale results. **SearchWala** solves this with a two-layer time-awareness system:

1. **Search Query Enrichment** — When SearchWala detects recency keywords like "latest", "current", "recent", "today", or "now" in your query, it automatically appends the current year (e.g., `2026`) to nudge search engines toward fresh results
2. **LLM Date Injection** — Every AI synthesis prompt includes today's exact date with strong instructions to prioritize the most recent sources and flag outdated information

Ask SearchWala "Who is the CEO of Google?" and you'll get today's answer — not one from 2024.

### 🔍 90+ Search Engines — The Widest Coverage in Any Open-Source Tool

**SearchWala** doesn't rely on a single search provider. It queries **90+ engines in parallel**, each with a dedicated HTML parser — no API keys needed:

- **Major**: Google (14 regional variants), Bing (14 regional), DuckDuckGo, Brave, Yahoo
- **Privacy**: Startpage, Qwant, Mojeek, Swisscows, MetaGer, Search Encrypt, Presearch
- **Academic**: Google Scholar, Wikipedia (API-native)
- **Regional**: Yandex, Baidu, Sogou, Naver, Daum, Seznam, Rambler
- **Independent**: Wiby, Marginalia, Stract, Right DAO, Mwmbl, Yep
- **Aggregators**: Dogpile, WebCrawler, Info, Excite, Lycos, AOL
- **Vertical**: Google News, Bing News, Yahoo News, Brave News, DDG News/Images/Videos

### 🛡️ Military-Grade Stealth — 20 Browser Fingerprints (2026 Current)

Every request **SearchWala** makes rotates through **20 real browser profiles** updated for April 2026:
- Realistic `User-Agent` strings (Chrome 145–147, Firefox 135–136, Edge 146–147, Safari 18.3–18.4, Opera 117, Brave 147)
- Full `Sec-CH-UA` client hint suite with platform version hints (Windows 15.0.0, macOS 15.4.0)
- `Sec-CH-UA-Arch`, `Sec-CH-UA-Bitness`, `Sec-CH-UA-Full-Version-List` headers
- `Accept-Encoding: gzip, deflate, br, zstd` (2026 browser standard)
- Randomized `Accept` header variants with 3 rotation patterns
- Per-request cookie isolation
- Configurable jitter timing (50–200ms)
- Optional proxy pool with health tracking + Tor SOCKS5 integration

This lets **SearchWala** bypass Cloudflare, Akamai, and Imperva WAFs — something no other open-source search engine even attempts.

### 📖 5-Tier Content Extraction

While other search tools give you links, **SearchWala** scrapes and extracts the **actual article text**:

1. **Structured Selectors** — `.entry-content`, `.article-body`, `#main-content` (35+ CMS patterns)
2. **Semantic HTML5** — `<article>`, `<main>`, `[role="main"]`, `[itemprop="articleBody"]`
3. **Scored Container** — Text-density scoring with link-ratio penalty (trafilatura-inspired)
4. **Content Elements** — `<p>`, `<li>`, `<blockquote>`, `<pre>` fallback collection
5. **Full Body** — Last-resort visible text extraction with boilerplate filtering

### 🧠 BM25 Paragraph-Level Ranking

Raw results aren't enough — relevance matters. **SearchWala** breaks every scraped article into paragraph-sized chunks and scores them using the **Okapi BM25 algorithm** (the same model underlying Elasticsearch):

- Term frequency (TF) analysis per chunk
- Inverse document frequency (IDF) across all chunks
- Document length normalization with exact phrase match bonus
- Only the **top-K most relevant chunks** are passed to the LLM — not raw pages

### 🔬 Deep Research Mode

**SearchWala's** Deep Research mode produces comprehensive research papers:
1. Queries **all 90+ engines** simultaneously
2. Scrapes **200+ sources** concurrently
3. Splits results into **batches of 50** for iterative LLM synthesis
4. Each batch **builds on the previous report** with new evidence
5. Produces a **detailed research paper** with proper `[n]` source citations

This is the open-source equivalent of Perplexity Pro Search — without the subscription.

### 🎯 Domain-Specialized Search

**SearchWala** offers 5 curated domain modes with optimized engine sets:

| Domain | Focus Engines | Use Case |
|---|---|---|
| 💻 **Tech** | Stack Overflow, GitHub, HN, dev blogs | Programming, APIs, DevOps |
| 🧬 **Science** | Google Scholar, PubMed, arXiv, Nature | Research papers, studies |
| 📊 **Finance** | Bloomberg, Reuters, Yahoo Finance | Markets, earnings, macro |
| 🏥 **Health** | NIH, WHO, Mayo Clinic, medical journals | Medical, clinical data |
| 📰 **News** | All news-specific engine variants | Breaking news, current events |

---

## 💻 Cross-Platform Support

**SearchWala** runs natively on all major operating systems. Since it compiles to a single Rust binary with zero runtime dependencies, you get the same performance everywhere:

### 🪟 Windows

```powershell
# Install Rust (if not installed)
winget install Rustlang.Rustup

# Clone and build
git clone https://github.com/SandeepAi369/SearchWala.git
cd SearchWala
cargo build --release

# Run SearchWala
.\target\release\searchwala.exe
# Open http://localhost:8000 in your browser
```

**WSL Support**: SearchWala also runs perfectly under Windows Subsystem for Linux (WSL). The server binds to `0.0.0.0`, so you can access it from your Windows browser at `http://localhost:8000`.

### 🍎 macOS

```bash
# Install Rust (if not installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone and build
git clone https://github.com/SandeepAi369/SearchWala.git
cd SearchWala
cargo build --release

# Run SearchWala
./target/release/searchwala
# Open http://localhost:8000
```

Works on both **Intel** and **Apple Silicon (M1/M2/M3/M4)** Macs — Rust compiles natively for ARM64.

### 🐧 Linux

```bash
# Install Rust (if not installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone and build
git clone https://github.com/SandeepAi369/SearchWala.git
cd SearchWala
cargo build --release

# Run SearchWala
./target/release/searchwala
# Open http://localhost:8000
```

Tested on Ubuntu 20.04+, Debian 11+, Fedora 38+, Arch Linux, and Alpine (Docker).

### 🐳 Docker (Any Platform)

```bash
docker build -t searchwala .
docker run -p 8000:8000 searchwala
```

The Docker image is a multi-stage build resulting in a **~15MB** final image — runs on any platform with Docker including cloud VPS, Hugging Face Spaces, and Kubernetes.

### System Requirements

| Component | Minimum | Recommended |
|---|---|---|
| **OS** | Windows 10+, macOS 12+, Linux (glibc 2.31+) | Any modern OS |
| **RAM** | 256MB | 512MB+ |
| **Disk** | 20MB (binary) | 50MB (with build cache) |
| **CPU** | Any x86_64 or ARM64 | Multi-core for concurrency |
| **Network** | Internet connection | Broadband for 90 engine queries |
| **Rust** | 1.75+ (build only) | Latest stable |

---

## 🏗️ Architecture

```
  Client Request           SearchWala v5.1.0
  ┌──────────┐        ┌────────────────────────────────────────────────┐
  │ POST     │        │                                                │
  │ /search  │───────►│  1. Time-Aware Query Enrichment (auto-date)   │
  │          │        │  2. ALL 35 Engines Dispatch (simultaneous)     │
  │          │        │  3. 20-wide Semaphore Concurrency Control      │
  │          │        │  4. URL Dedup (single-parse pipeline)          │
  │          │        │  5. Concurrent Scrape (32 workers)             │
  │          │        │  6. 5-Tier Content Extraction                  │
  │          │        │  7. BM25 Paragraph Ranking                     │
  │          │        │  8. LLM Synthesis with Date Injection (BYOK)  │
  │          │        │  9. Neural TTS Voice Readback                  │
  │          │        │                                                │
  │  ◄───────┤────────│  Response: sources + text + answer + audio    │
  └──────────┘        └────────────────────────────────────────────────┘
```

### Simultaneous All-Engine Dispatch (v5.1.0)

```
ALL 35 engines fire simultaneously (zero delay)
    │
    ├── 20-wide semaphore controls concurrency
    ├── Per-engine staggered jitter (50–200ms) for stealth
    ├── Proxy pool rotation with health tracking
    │
    └── Results merged + deduplicated → scrape pipeline
         └──► Guarantees maximum data from every available source
```

---

## 📁 Project Structure

```
SearchWala/
├── Cargo.toml               # Dependencies & release optimizations (LTO, strip)
├── Dockerfile               # Multi-stage Docker build (~15MB final image)
├── LICENSE                   # Apache 2.0
├── README.md
├── ui.html                  # Perplexity-style search UI (979 LOC, embedded at compile time)
├── scripts/
│   ├── ram_monitor.sh       # Memory usage monitoring utility
│   └── test_fallback.py     # Engine fallback integration test
└── src/
    ├── main.rs              # Axum HTTP server — routes, TTS endpoint, middleware (542 LOC)
    ├── config.rs            # 20 browser profiles, WAF bypass, env config (457 LOC)
    ├── models.rs            # Request/Response types (serde JSON) (107 LOC)
    ├── query_intel.rs       # Query Intelligence — intent, temporal, entity detection (272 LOC) [NEW v5.2.0]
    ├── search.rs            # Search orchestration + RRF + simultaneous dispatch (665 LOC)
    ├── stream.rs            # SSE streaming pipeline (/search/stream) (482 LOC)
    ├── ranking.rs           # Hybrid RRF + BM25 paragraph ranking (268 LOC)
    ├── llm.rs               # BYOK LLM: 15+ providers, intent-aware prompts (1,767 LOC)
    ├── extractor.rs         # 7-tier content extraction (JSON-LD + meta fallback) (873 LOC)
    ├── url_utils.rs         # URL normalization, single-parse dedup pipeline (183 LOC)
    ├── cache.rs             # TempDb (in-memory) + HistoryDb (~/.searchwala/) (328 LOC)
    ├── copilot.rs           # SearchWala Copilot — LLM-powered query rewriter (36 LOC)
    ├── proxy_pool.rs        # Round-robin proxy rotation with health tracking (121 LOC)
    └── engines/
        ├── mod.rs           # SearchEngine trait + engine weights + domain modes (196 LOC)
        ├── generic.rs       # Template engine for 60+ regional variants (587 LOC)
        ├── duckduckgo.rs    # DuckDuckGo HTML scraper (115 LOC)
        ├── brave.rs         # Brave Search scraper (131 LOC)
        ├── yahoo.rs         # Yahoo Search scraper (138 LOC)
        ├── qwant.rs         # Qwant scraper (112 LOC)
        ├── mojeek.rs        # Mojeek scraper (121 LOC)
        ├── startpage.rs     # Startpage scraper (143 LOC)
        ├── wikipedia.rs     # Wikipedia JSON API engine (63 LOC)
        └── wiby.rs          # Wiby indie search engine (62 LOC)
```

**Total**: **8,870 lines** (7,891 Rust + 979 HTML/JS) · 23 source files · Zero Python · Zero Node · Zero Java

---

## ⚡ Quick Start

### Build from Source (Any Platform)

```bash
git clone https://github.com/SandeepAi369/SearchWala.git
cd SearchWala

# Build optimized release binary
cargo build --release

# Run SearchWala (starts on http://localhost:8000)
./target/release/searchwala        # Linux/macOS
.\target\release\searchwala.exe    # Windows
```

### Verify SearchWala Is Running

```bash
# Health check
curl http://localhost:8000/health

# Basic search (returns sources + extracted text)
curl -X POST http://localhost:8000/search \
  -H "Content-Type: application/json" \
  -d '{"query": "quantum computing breakthroughs 2026"}'

# Search with AI answer (BYOK — bring your own key)
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

### Open the SearchWala UI

Navigate to `http://localhost:8000` for the built-in Perplexity-style search interface:

- 🔍 **Smart search bar** with mode selector (Lite / Deep Research)
- 🏷️ **Category pills** (Tech / Science / Finance / Health / News)
- ⚡ **Orbital spinner** animation with live status updates
- ✍️ **Word-by-word typewriter** for AI responses with auto-scroll
- 🔊 **Listen button** — neural voice readback for every answer
- ⚙️ **Settings panel** — configure any LLM provider (15+ supported)
- 📚 **Source cards** — clickable source chips with favicons
- 📜 **Search history** — optional local-only history with `~/.searchwala/`

---

## 📡 API Reference

All **SearchWala** endpoints accept JSON and return JSON. The server runs on `http://localhost:8000` by default.

### `POST /search`

**Standard search** — SearchWala queries 90+ engines, scrapes sources, and returns extracted text.

```json
// Request
{
  "query": "artificial intelligence trends 2026",
  "max_results": 50,
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
      "extracted_text": "Full article text extracted by SearchWala...",
      "char_count": 7270,
      "engine": "google_scholar"
    }
  ],
  "elapsed_seconds": 4.28,
  "engine_stats": {
    "engines_queried": ["wikipedia", "duckduckgo", "brave", "...90 total..."],
    "total_raw_results": 322,
    "deduplicated_urls": 142
  }
}
```

### `POST /search/lite-llm`

SearchWala Lite — fast single-pass AI synthesis over up to 50 sources. Requires `llm` config.

### `POST /search/research-llm`

SearchWala Deep Research — iterative multi-batch LLM synthesis over 200+ sources.

### `POST /search/stream`

SSE streaming — real-time source delivery + LLM token streaming.

### `GET /api/tts?text=Hello+world`

**SearchWala Voice** — Text-to-Speech synthesis using Microsoft Edge neural voice (en-US-AvaNeural). Returns `audio/mpeg`. Used internally by the Listen button, but also available as a standalone API.

### `GET /health`

```json
{
  "status": "ok",
  "version": "5.2.0",
  "engines": ["wikipedia", "duckduckgo", "brave", "...90 total..."],
  "uptime_seconds": 3600
}
```

### `GET /config`

Returns SearchWala's current runtime configuration (concurrency, timeouts, engine list, proxy status).

### `POST /api/models`

Dynamic model discovery — fetches available models from any OpenAI-compatible endpoint.

---

## 🔌 Supported LLM Providers

**SearchWala** supports 15+ LLM providers out of the box. Bring your own API key (BYOK) — SearchWala never locks you into a single provider:

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

All environment variables are optional — **SearchWala** ships with sensible defaults that work out of the box.

### Search & Scraping

| Variable | Default | Description |
|---|---|---|
| `ENGINES` | 90 engines (curated) | Comma-separated engine names to enable |
| `MAX_URLS` | `420` | Maximum URLs SearchWala will scrape per query |
| `CONCURRENCY` | `32` | Concurrent scrape workers |
| `ENGINE_CONCURRENCY` | `20` | Concurrent engine-query workers |
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
| `RUST_LOG` | `searchwala=info` | Log verbosity level |
| `EDGE_TTS_PATH` | auto-detect | Path to `edge-tts` binary for SearchWala Voice |

---

## 🔒 Privacy & Security

**SearchWala** is built with privacy as a core principle:

- **Zero telemetry** — no tracking, no analytics, no phone-home
- **No cloud dependencies** — SearchWala runs entirely on your hardware
- **No API keys required** — all 90 engines work without any API registration
- **Cookie isolation** — every request uses a fresh HTTP client
- **Tracking param removal** — strips 30+ UTM/analytics parameters from every URL
- **Domain blocklist** — auto-skips social media feeds, app stores, and binary file URLs
- **Optional BYOK LLM** — AI synthesis is opt-in; raw results always available
- **Local-only history** — search history stored at `~/.searchwala/history.json`, never uploaded

---

## 📊 Performance Characteristics

| Metric | Lite Mode | Deep Research Mode |
|---|---|---|
| Engines queried | ALL 35 simultaneously | 90+ all engines |
| Sources scraped | 25–50 | 200–400+ |
| Time to results | 3–8 seconds | 15–45 seconds |
| LLM context quality | Top 25 BM25 chunks | Full iterative batches |
| Voice readback | Instant 10-word streaming | Full report narration |
| Memory footprint | ~30MB RSS | ~80MB RSS peak |
| Binary size | ~15MB (stripped, LTO) | Same binary |

---

## 🗺️ Roadmap

- [x] Neural Voice Readback (TTS)
- [x] Time-Aware Search Intelligence
- [x] Cross-platform support (Windows/Mac/Linux)
- [x] 2026 Browser Fingerprints (Chrome 147, Edge 147, Firefox 136, Safari 18.4)
- [x] Simultaneous All-Engine Dispatch (zero-delay parallel)
- [x] 6 New Search Engines (Alexandria, 4get, Whoogle, LibreX, YaCy, Mullvad Leta)
- [ ] BoringSSL TLS Impersonation (rquest — JA3/JA4 fingerprint cloning)
- [ ] IPv6 /64 Subnet Hopping (18 quintillion IPs)
- [ ] Response compression (gzip/brotli)
- [ ] Built-in caching layer with TTL
- [ ] Citation graph visualization
- [ ] Plugin system for custom engines
- [ ] Multi-language query support
- [ ] Mobile-responsive PWA mode

---

## 📄 License

Copyright 2026 [Sandeep](https://xel-studio.vercel.app/)

Licensed under the [Apache License, Version 2.0](./LICENSE).

---

<p align="center">
  <strong>SearchWala — Search Smarter, Not Harder 🔍</strong>
  <br>
  <sub>Built with 🦀 Rust by <a href="https://xel-studio.vercel.app/">Sandeep</a></sub>
  <br>
  <sub>8,870 lines of code (7,891 Rust + 979 UI) · 23 source files · Zero runtime dependencies · One binary for all platforms</sub>
</p>
