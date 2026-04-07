<p align="center">
  <h1 align="center">⚡ Swift Search Agent v3.0</h1>
  <p align="center">
    <strong>Ultra-fast search & data extraction API written in pure Rust. No Python. No SearxNG. No LLM. One binary, 22MB RAM.</strong>
  </p>
  <p align="center">
    <img src="https://img.shields.io/badge/version-3.0-blueviolet" alt="Version 3.0">
    <img src="https://img.shields.io/badge/language-Rust-orange?logo=rust&logoColor=white" alt="Rust">
    <img src="https://img.shields.io/badge/framework-Axum-blue" alt="Axum">
    <img src="https://img.shields.io/badge/binary-6.1MB-green" alt="6.1MB Binary">
    <img src="https://img.shields.io/badge/RAM-22MB-critical" alt="22MB RAM">
    <img src="https://img.shields.io/badge/output-Raw_JSON-orange" alt="Raw JSON">
    <img src="https://img.shields.io/badge/license-MIT-brightgreen" alt="MIT License">
  </p>
</p>

---

## 🌟 What Is Swift Search Agent?

A **single compiled Rust binary** that replaces an entire Python stack (SearxNG + Trafilatura + FastAPI). It queries **5 search engines natively**, scrapes the results, extracts article text, and returns clean JSON — all in **~4 seconds** using **22MB of RAM**.

> **🔧 Pure Search & Scrape:** No LLM inside. Returns raw extracted text, URLs, and titles. Connect **any LLM** on your client side.

---

## 🔄 How It Works

```
┌─────────────┐      ┌──────────────────────────────┐      ┌──────────────────┐      ┌──────────────┐
│  User Query │─────▶│  Native Engine Queries        │─────▶│  Readability     │─────▶│  Raw JSON    │
│  POST /search      │  (concurrent, ~1.5s)          │      │  Extractor       │      │  Response    │
└─────────────┘      │                               │      │  (Rust scraper)  │      └──────────────┘
                     │  DuckDuckGo ──┐               │      └──────────────────┘
                     │  Brave ───────┤               │              │
                     │  Yahoo ───────┤ → Dedup URLs  │         3-strategy:
                     │  Qwant ───────┤   (15 unique) │         1. <article> semantic
                     │  Mojeek ──────┘               │         2. Readability scoring
                     │                               │         3. Paragraph fallback
                     │  NO SearxNG, NO Google,       │
                     │  NO Bing, NO Python           │
                     └──────────────────────────────-┘
```

### How the Pipeline Executes

| Phase | What Happens | Time |
|---|---|---|
| **1. Meta-Search** | All 5 engines queried **concurrently** via tokio tasks. HTML scraped and parsed with `scraper` crate. URLs extracted from DDG redirects, Yahoo redirects, Brave DOM, Mojeek DOM. | ~1.5s |
| **2. URL Processing** | Tracking params stripped, domains normalized, blocklist applied (social, video, binary). Order-preserving deduplication. | <1ms |
| **3. Concurrent Scrape** | Up to 8 URLs scraped simultaneously via semaphore-bounded `reqwest` with gzip/brotli/deflate compression. | ~2-3s |
| **4. Text Extraction** | 3-strategy readability heuristic: (1) `<article>` semantic HTML → (2) scored container selection → (3) `<p>` paragraph fallback. | <100ms |

---

## 🏗️ Architecture

```
swift-search-rs/
├── Cargo.toml                    # Dependencies (axum, reqwest, scraper, tokio)
├── Dockerfile                    # Multi-stage Docker build (~15MB final image)
├── src/
│   ├── main.rs                   # Axum HTTP server (/search, /health, /config)
│   ├── config.rs                 # Env vars, user-agent rotation, blocklists
│   ├── models.rs                 # Request/Response data structures
│   ├── search.rs                 # Pipeline orchestrator
│   ├── extractor.rs              # Readability article extractor (replaces Trafilatura)
│   ├── url_utils.rs              # URL normalization & deduplication
│   └── engines/
│       ├── mod.rs                # Engine trait + registry
│       ├── duckduckgo.rs         # DDG HTML POST scraping
│       ├── brave.rs              # Brave HTML GET scraping
│       ├── yahoo.rs              # Yahoo HTML scraping + redirect extraction
│       ├── qwant.rs              # Qwant HTML scraping
│       └── mojeek.rs             # Mojeek HTML scraping
```

### Performance Comparison: Python v2.0 vs Rust v3.0

| Metric | Python v2.0 | Rust v3.0 | Improvement |
|---|---|---|---|
| **Binary size** | ~50MB (venv) | **6.1MB** | 8x smaller |
| **Idle RAM** | ~80-120MB | **3MB** | 30x less |
| **Peak RAM (under load)** | ~200-400MB | **22MB** | 10-18x less |
| **Search time** | ~10-15s | **~4s** | 3x faster |
| **External dependencies** | SearxNG + Python | **None** | Zero dependency |
| **Startup time** | ~3-5s | **<10ms** | 300x faster |

---

## ⚡ Quick Start

### Option 1: Build from Source

```bash
# Clone
git clone https://github.com/SandeepAi369/Swift-Search-Agent.git
cd Swift-Search-Agent/swift-search-rs

# Build (release, optimized)
cargo build --release

# Run
./target/release/swift-search-rs
```

### Option 2: Docker

```bash
cd swift-search-rs
docker build -t swift-search .
docker run -p 8000:7860 swift-search
```

### Test the API

```bash
curl -X POST "http://localhost:8000/search" \
  -H "Content-Type: application/json" \
  -d '{"query": "What is machine learning?"}'
```

---

## ⚙️ Environment Variables

All variables are **optional** — sensible defaults are built-in.

| Variable | Default | Description |
|---|---|---|
| `ENGINES` | `duckduckgo,brave,yahoo,qwant,mojeek` | Search engines to use |
| `MAX_URLS` | `15` | Max URLs to scrape per query |
| `CONCURRENCY` | `8` | Concurrent scrape limit |
| `SCRAPE_TIMEOUT` | `10` | Timeout per URL (seconds) |
| `MAX_HTML_BYTES` | `500000` | Max HTML download per page |
| `PORT` | `8000` | Server port |
| `RUST_LOG` | `swift_search_rs=info` | Log level |

---

## 📦 Rust Dependencies

| Crate | Purpose |
|---|---|
| [**axum**](https://github.com/tokio-rs/axum) | Async HTTP server |
| [**tokio**](https://tokio.rs/) | Async runtime |
| [**reqwest**](https://docs.rs/reqwest) | HTTP client (rustls TLS) |
| [**scraper**](https://docs.rs/scraper) | HTML DOM parsing + CSS selectors |
| [**serde**](https://serde.rs/) | JSON serialization |
| [**regex**](https://docs.rs/regex) | Pattern matching |
| [**futures**](https://docs.rs/futures) | Concurrent engine execution |
| [**tracing**](https://docs.rs/tracing) | Structured logging |

---

## 📄 API Endpoints

### `POST /search`

```json
// Request
{ "query": "quantum computing breakthroughs", "max_results": 10 }

// Response
{
  "query": "quantum computing breakthroughs",
  "sources_found": 15,
  "sources_processed": 13,
  "results": [
    {
      "url": "https://www.nature.com/articles/...",
      "title": "Quantum computing breakthroughs...",
      "extracted_text": "Full article text extracted...",
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
Returns server status, version, uptime.

### `GET /config`
Returns current configuration.

---

## 📄 License

This project is licensed under the [MIT License](./LICENSE).

---

<p align="center">
  <strong>Developed & Enhanced by <a href="https://xel-studio.vercel.app/">Sandeep</a></strong>
</p>
