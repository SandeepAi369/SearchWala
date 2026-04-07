<p align="center">
  <h1 align="center">⚡ Swift-Search-Rs</h1>
  <p align="center">
    <strong>Ultra-fast native meta-search & web scraping API — a single Rust binary, 22MB RAM, ~4s per query.</strong>
  </p>
  <p align="center">
    <img src="https://img.shields.io/badge/version-1.0.0-blueviolet" alt="Version 1.0.0">
    <img src="https://img.shields.io/badge/language-Rust-orange?logo=rust&logoColor=white" alt="Rust">
    <img src="https://img.shields.io/badge/framework-Axum-blue" alt="Axum">
    <img src="https://img.shields.io/badge/binary-6.1MB-green" alt="6.1MB Binary">
    <img src="https://img.shields.io/badge/peak_RAM-22MB-critical" alt="22MB RAM">
    <img src="https://img.shields.io/badge/output-Raw_JSON-orange" alt="Raw JSON">
    <img src="https://img.shields.io/badge/license-Apache--2.0-brightgreen" alt="Apache 2.0">
  </p>
</p>

---

## 🌟 What Is Swift-Search-Rs?

**Swift-Search-Rs** is a production-ready **search & scrape API** compiled into a single Rust binary. It natively queries **5 privacy-focused search engines**, deduplicates the results, concurrently scrapes every URL, extracts clean article text using a multi-strategy readability algorithm, and returns structured JSON — all in about **4 seconds** using just **22MB of RAM**.

No external search infrastructure required. No heavyweight Python runtimes. No bloated dependency trees. Just one binary.

> **🔧 Bring Your Own LLM:** This API handles the hard part — finding, fetching, and cleaning web content. It returns raw extracted text, URLs, and titles. Connect **any LLM or AI system** on your client side.

---

## 🔄 How It Works

```
┌─────────────┐      ┌──────────────────────────────┐      ┌──────────────────┐      ┌──────────────┐
│  User Query │─────▶│  Native Engine Queries        │─────▶│  Readability     │─────▶│  Raw JSON    │
│  POST /search      │  (concurrent, ~1.5s)          │      │  Extractor       │      │  Response    │
└─────────────┘      │                               │      │  (3-strategy)    │      └──────────────┘
                     │  DuckDuckGo ──┐               │      └──────────────────┘
                     │  Brave ───────┤               │
                     │  Yahoo ───────┤ → Dedup URLs  │      Strategies:
                     │  Qwant ───────┤   (15 unique) │      1. <article> semantic HTML
                     │  Mojeek ──────┘               │      2. Readability container scoring
                     │                               │      3. Paragraph density fallback
                     │  All concurrent via Tokio      │
                     └───────────────────────────────┘
```

### Pipeline Breakdown

| Phase | What Happens | Time |
|---|---|---|
| **1. Meta-Search** | All 5 engines queried **concurrently** via Tokio async tasks. HTML responses parsed with CSS selectors. Redirect URLs decoded from DDG wrapper, Yahoo wrapper, etc. | ~1.5s |
| **2. URL Processing** | 30+ tracking parameters stripped. Domain/extension blocklist applied (social media, video, binaries). Order-preserving deduplication via normalized keys. | <1ms |
| **3. Concurrent Scrape** | Up to 8 URLs scraped simultaneously via semaphore-bounded `reqwest`. Auto gzip/brotli/deflate. HTML size capped at 500KB per page to prevent memory spikes. | ~2-3s |
| **4. Text Extraction** | 3-strategy readability heuristic: **(1)** `<article>` semantic HTML → **(2)** scored container selection (paragraph count, text density, class pattern matching) → **(3)** `<p>` paragraph fallback. | <100ms |

---

## 🏗️ Project Structure

```
Swift-Search-Rs/
├── Cargo.toml          # Dependencies & release optimizations (LTO, strip, single codegen)
├── Dockerfile          # Multi-stage Docker build (~15MB final image)
├── LICENSE             # Apache 2.0
├── README.md
└── src/
    ├── main.rs         # Axum HTTP server — /search, /health, /config, / endpoints
    ├── config.rs       # Environment variables, user-agent rotation, blocklists
    ├── models.rs       # Request/Response types (serde-powered JSON)
    ├── search.rs       # Pipeline orchestrator: engines → dedup → scrape → extract
    ├── extractor.rs    # Readability article extraction (3-strategy heuristic)
    ├── url_utils.rs    # URL normalization, dedup, tracking param removal
    └── engines/
        ├── mod.rs      # SearchEngine trait + engine factory
        ├── duckduckgo.rs  # POST html.duckduckgo.com (HTML scraping)
        ├── brave.rs       # GET search.brave.com (DOM + fallback)
        ├── yahoo.rs       # GET search.yahoo.com (redirect extraction)
        ├── qwant.rs       # GET qwant.com (HTML scraping)
        └── mojeek.rs      # GET mojeek.com (DOM extraction)
```

---

## ⚡ Quick Start

### Build from Source

```bash
# Clone
git clone https://github.com/SandeepAi369/Swift-Search-Agent.git
cd Swift-Search-Agent

# Build optimized release binary
cargo build --release

# Run
./target/release/swift-search-rs
```

### Docker

```bash
docker build -t swift-search-rs .
docker run -p 8000:7860 swift-search-rs
```

### Test

```bash
# Health check
curl http://localhost:8000/health

# Search
curl -X POST http://localhost:8000/search \
  -H "Content-Type: application/json" \
  -d '{"query": "quantum computing breakthroughs"}'
```

---

## 📡 API Reference

### `POST /search`

Search the web and extract article text from results.

**Request:**
```json
{
  "query": "artificial intelligence trends 2026",
  "max_results": 10
}
```

**Response:**
```json
{
  "query": "artificial intelligence trends 2026",
  "sources_found": 15,
  "sources_processed": 13,
  "results": [
    {
      "url": "https://www.nature.com/articles/...",
      "title": "AI breakthroughs reshape...",
      "extracted_text": "Full article text extracted via readability heuristics...",
      "char_count": 7270,
      "engine": "duckduckgo"
    }
  ],
  "elapsed_seconds": 4.28,
  "engine_stats": {
    "engines_queried": ["duckduckgo", "brave", "yahoo", "qwant", "mojeek"],
    "total_raw_results": 58,
    "deduplicated_urls": 15
  }
}
```

### `GET /health`

```json
{
  "status": "ok",
  "version": "1.0.0",
  "engines": ["duckduckgo", "brave", "yahoo", "qwant", "mojeek"],
  "uptime_seconds": 3600
}
```

### `GET /config`

Returns current runtime configuration (max URLs, concurrency, timeouts, etc.)

### `GET /`

Root endpoint for uptime monitoring pings.

---

## ⚙️ Environment Variables

All optional — sensible defaults built-in.

| Variable | Default | Description |
|---|---|---|
| `ENGINES` | `duckduckgo,brave,yahoo,qwant,mojeek` | Search engines to query |
| `MAX_URLS` | `15` | Max URLs to scrape per query |
| `CONCURRENCY` | `8` | Simultaneous scrape connections |
| `SCRAPE_TIMEOUT` | `10` | Per-URL timeout (seconds) |
| `MAX_HTML_BYTES` | `500000` | Max HTML download per page (500KB) |
| `PORT` | `8000` | HTTP server port |
| `RUST_LOG` | `swift_search_rs=info` | Log level |

---

## 🔒 Privacy & Design Principles

- **No Google, No Bing** — only privacy-respecting engines (DDG, Brave, Mojeek, Yahoo, Qwant)
- **No external infrastructure** — no SearxNG, no proxy servers, no API keys
- **No LLM inside** — returns raw text; you choose your AI stack
- **No telemetry** — zero tracking, zero analytics, zero phone-home
- **Domain blocklist** — automatically skips social media, video, app store, and binary file URLs
- **Tracking parameter removal** — strips 30+ UTM/analytics params from every URL

---

## 📄 License

Copyright 2026 Sandeep

Licensed under the [Apache License, Version 2.0](./LICENSE).

---

<p align="center">
  <strong>Built by <a href="https://xel-studio.vercel.app/">Sandeep</a></strong>
</p>
