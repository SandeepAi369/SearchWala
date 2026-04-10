<p align="center">
  <h1 align="center">⚡ Swift-Search-Rs</h1>
  <p align="center">
    <strong>Ultra-fast native meta-search, scraping, and optional LLM synthesis API with dual pipelines and Rust-native ranking.</strong>
  </p>
  <p align="center">
    <img src="https://img.shields.io/badge/version-4.0.0-blueviolet" alt="Version 4.0.0">
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

**Swift-Search-Rs** is a production-ready **search + scrape + optional synthesis API** compiled into a single Rust binary. It can query a large multi-engine set (Google/Bing/Yahoo/DDG and many regional alternatives), deduplicate and scrape sources concurrently, and route output through one of two data pipelines:

- **Research path:** returns comprehensive scraped sources.
- **Lite/LLM path:** runs Rust-native paragraph chunking + BM25 ranking, then keeps only the highest-signal chunks for token efficiency.

No external search infrastructure required. No heavyweight Python runtimes. No bloated dependency trees. Just one binary.

> **🔧 Bring Your Own LLM:** This API handles the hard part — finding, fetching, and cleaning web content. It returns raw extracted text, URLs, and titles. Connect **any LLM or AI system** on your client side.

---

## 🔄 How It Works

```
┌─────────────┐      ┌──────────────────────────────┐      ┌──────────────────┐
│  User Query │─────▶│  Engine Orchestrator         │─────▶│  Scrape/Extract  │
│ POST /search│      │ Snowball + Jitter + Proxy    │      │    HTML -> text  │
└─────────────┘      │ (many engines concurrently)   │      └──────────────────┘
                     └──────────────────────────────┘
                                   │
                 ┌─────────────────┴─────────────────┐
                 │                                   │
       ┌──────────────────────┐           ┌──────────────────────────┐
       │ Path A: Research     │           │ Path B: Lite / LLM       │
       │ Return full sources  │           │ BM25 top chunks (<= 25)  │
       └──────────────────────┘           └──────────────────────────┘
```

### Pipeline Breakdown

| Phase | What Happens | Time |
|---|---|---|
| **1. Snowball Meta-Search** | Query variations (base/news/forum) run across a large engine set with controlled concurrency and smart jitter. | ~1-3s |
| **2. URL Processing** | Tracking parameters removed, focus-mode-aware dedup applied, and source metadata preserved. | <5ms |
| **3. Concurrent Scrape** | Semaphore-bounded scraping, browser-style header rotation, optional proxy pool hints, and focus-mode URL rewrites. | ~2-6s |
| **4. Dual Pipeline Routing** | Research mode returns comprehensive sources; Lite/LLM mode applies Rust-native BM25 chunk ranking and keeps top 25 chunks. | <100ms |
| **5. Optional LLM Synthesis** | Ranked context is streamed/summarized via BYOK model settings for low-noise token-efficient answers. | model dependent |

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
    ├── search.rs       # Main orchestration + dual-path response logic
    ├── stream.rs       # Streaming orchestration for /search/stream
    ├── ranking.rs      # Paragraph chunking + BM25 ranking (Lite/LLM path)
    ├── llm.rs          # BYOK LLM synthesis and prompt pipeline
    ├── proxy_pool.rs   # Proxy rotation and cooldown management
    ├── extractor.rs    # Readability article extraction (3-strategy heuristic)
    ├── url_utils.rs    # URL normalization, dedup, tracking param removal
    ├── copilot.rs      # Query rewrite helper
    └── engines/
        ├── mod.rs         # SearchEngine trait + engine factory
        ├── generic.rs     # Generic engine template for regional variants
        ├── duckduckgo.rs  # DDG scraping
        ├── brave.rs       # Brave scraping
        ├── yahoo.rs       # Yahoo scraping
        ├── qwant.rs       # Qwant scraping
        ├── mojeek.rs      # Mojeek scraping
        ├── startpage.rs   # Startpage scraping
        └── wikipedia.rs   # Wikipedia API engine
```

---

## ⚡ Quick Start

### Build from Source

```bash
# Clone
git clone https://github.com/SandeepAi369/Swift-Search-Rs.git
cd Swift-Search-Rs

# Build optimized release binary
cargo build --release

# Run
./target/release/swift-search-rs
```

### Docker

```bash
docker build -t swift-search-rs .
docker run -p 8000:8000 swift-search-rs
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
    "engines_queried": ["wikipedia", "duckduckgo", "brave", "bing", "google", "..."],
    "total_raw_results": 322,
    "deduplicated_urls": 40
  }
}
```

### `GET /health`

```json
{
  "status": "ok",
  "version": "4.0.0",
  "engines": ["wikipedia", "duckduckgo", "brave", "yahoo", "bing", "google", "..."],
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
| `ENGINES` | large curated list (86 profiles) | Engine set to query |
| `MAX_URLS` | `420` | Max URLs to scrape per query |
| `CONCURRENCY` | `24` | Concurrent scrape workers |
| `ENGINE_CONCURRENCY` | `10` | Concurrent engine-request workers |
| `JITTER_MIN_MS` | `50` | Minimum dispatch jitter between engine requests |
| `JITTER_MAX_MS` | `200` | Maximum dispatch jitter between engine requests |
| `SCRAPE_TIMEOUT` | `0` | Per-URL timeout in seconds (`0` = no explicit timeout) |
| `MAX_HTML_BYTES` | `1500000` | Max HTML download bytes per page |
| `PROXY_COOLDOWN_SECS` | `120` | Cooldown window after proxy failure |
| `PROXY_POOL` | `` | Comma-separated proxy list |
| `PROXY_POOL_FILE` | `` | File path with one proxy per line |
| `TOR_PROXY_PORTS` | `` | Comma-separated local Tor SOCKS ports |
| `PORT` | `8000` | HTTP server port |
| `RUST_LOG` | `swift_search_rs=info` | Log level |

---

## 🔒 Privacy & Design Principles

- **Local-first orchestration** — native Rust engine querying, scraping, dedup, and ranking
- **No forced external infra** — can run standalone without managed search backends
- **Optional BYOK LLM** — LLM synthesis is opt-in; raw results are always available
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
