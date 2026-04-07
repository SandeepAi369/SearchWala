<p align="center">
  <h1 align="center">⚡ Swift Search Agent v2.0</h1>
  <p align="center">
    <strong>A high-performance search & data extraction API powered by your own private SearxNG. Pure search + scrape — bring your own LLM.</strong>
  </p>
  <p align="center">
    <img src="https://img.shields.io/badge/version-2.0-blueviolet" alt="Version 2.0">
    <img src="https://img.shields.io/badge/python-3.10%2B-blue?logo=python&logoColor=white" alt="Python 3.10+">
    <img src="https://img.shields.io/badge/framework-FastAPI-009688?logo=fastapi&logoColor=white" alt="FastAPI">
    <img src="https://img.shields.io/badge/search-SearxNG-blue?logo=searxng&logoColor=white" alt="SearxNG">
    <img src="https://img.shields.io/badge/extraction-trafilatura-green" alt="trafilatura">
    <img src="https://img.shields.io/badge/output-Raw_Text-orange" alt="Raw Text Output">
    <img src="https://img.shields.io/badge/RAM-Auto--Tiered-critical" alt="Auto-Tiered RAM">
    <img src="https://img.shields.io/badge/license-MIT-brightgreen" alt="MIT License">
  </p>
</p>

---

## 🌟 What Is Swift Search Agent?

Swift Search Agent is a **production-ready API** that automates the search and extraction pipeline — from searching the web to extracting clean, structured text from web pages. It returns **raw extracted text** that you can feed into **any LLM or processing system** of your choice.

**v2.0** introduces a **multi-engine architecture** with **auto-adaptive RAM tiering** — the agent automatically detects your system's available memory and optimizes its concurrency, buffer sizes, and extraction strategy in real-time.

> **🔧 Pure Search & Scrape:** This API handles the hard part — finding, fetching, and cleaning web content. It returns raw extracted text, URLs, and titles. Connect **any LLM** on your client side to process the results however you want.

---

## 🔄 How It Works — Data Flow Pipeline

```
┌─────────────┐      ┌──────────────────┐      ┌──────────────────┐      ┌──────────────┐
│  User Query │─────▶│  Your Private    │─────▶│   trafilatura    │─────▶│  Raw JSON    │
│             │      │  SearxNG         │      │  + selectolax    │      │  Response    │
└─────────────┘      │  (localhost or   │      │  (Extraction)    │      └──────────────┘
                     │   HF Space)      │      └──────────────────┘             │
                     └──────────────────┘                                       │
                            │                                              Returns URLs,
                     Queries engines:                                      titles, and raw
                     DuckDuckGo, Brave,                                    extracted text
                     Yahoo, Qwant,
                     Mojeek
                     (NO Google, NO Bing)
```

### Phase-by-Phase Breakdown

| Phase | Component | Algorithm |
|---|---|---|
| **1. Meta-Search** | [**SearxNG**](https://github.com/searxng/searxng) | Queries your **private SearxNG instance** with explicit engine selection: **DuckDuckGo, Brave, Yahoo, Qwant, Mojeek** (no Google, no Bing). Results are deduplicated using **URL normalization** (tracking parameter removal, domain lowercasing, path normalization). Invalid URLs (social media, binary files) are filtered via domain/extension blocklists. |
| **2. Data Extraction** | [**trafilatura**](https://trafilatura.readthedocs.io/) + [**selectolax**](https://github.com/rushter/selectolax) | **Multi-strategy fallback chain**: (1) trafilatura `bare_extraction` for high-quality heuristic parsing → (2) selectolax Lexbor C-speed DOM parsing → (3) regex-based stripping as ultimate fallback. HTML is **streamed with hard caps** to prevent OOM. Extraction is bounded by `asyncio.Semaphore`. |
| **3. Context Building** | **StringIO + MD5 Dedup** | Extracted texts are **content-hash deduplicated** (MD5 of first 1000 chars) to eliminate near-identical content. **Early termination** stops scraping when 75% of the context buffer is filled. |

> **Key Insight:** The API returns raw extracted text per source (URL, title, text, quality score). You process this data however you want on your client — feed it to an LLM, build a RAG pipeline, or store it in a database.

---

## 🏗️ Architecture — Multi-Engine Design

```
Swift-Search-Agent/
├── config.py                     # Centralized configuration — auto-detects RAM tier
├── search_unified.py             # 🟢 Unified engine (recommended for most users)
├── search_optimized.py           # 🔵 Production engine (<60MB peak RAM)
├── search_ultra.py               # 🔴 Extreme engine (aiohttp + selectolax + DNS caching)
├── search_legacy.py              # ⚪ Legacy v1 engine (backup reference)
├── requirements.txt              # Python dependencies
├── .env.example                  # Environment variable template
├── Proxy_Integration_Guide.md    # Optional proxy & IP rotation guide
├── LICENSE
└── README.md
```

### Engine Comparison

| Engine | File | Best For | Peak RAM | Networking | Extraction | Key Feature |
|---|---|---|---|---|---|---|
| 🟢 **Unified** | `search_unified.py` | General purpose | Auto-tiered | `httpx` | trafilatura | Auto RAM detection, early termination, quality scoring |
| 🔵 **Optimized** | `search_optimized.py` | Low-RAM VPS | <60MB | `httpx` streaming | trafilatura + selectolax + regex | Recursive text chunking, 3-strategy fallback |
| 🔴 **Ultra** | `search_ultra.py` | Max performance | Tier-based | `aiohttp` + HTTP/2 | selectolax (C-speed) + trafilatura | DNS caching, Beast mode (200 concurrent), CLI args |
| ⚪ **Legacy** | `search_legacy.py` | Reference/backup | Fixed | `httpx` | trafilatura only | Simple 20-instance rotator |

### RAM Auto-Tiering System

The agents automatically detect your system's available RAM and configure optimal settings:

| Tier | RAM Range | Concurrency | Max URLs | HTML Cap | Context Limit |
|---|---|---|---|---|---|
| **Micro** | ≤512MB | 3–5 | 25–50 | 256KB | 50K chars |
| **Medium** | 512MB–2GB | 8–20 | 50–100 | 768KB | 80K chars |
| **Large / Beast** | >2GB | 12–200 | 60–∞ | 1MB+ | 100K chars |

> Set endpoint with: `SEARXNG_URL=https://your-searxng.hf.space python search_unified.py`

---

## ⚡ Quick Start

```bash
# Clone the repo
git clone https://github.com/SandeepAi369/Swift-Search-Agent.git
cd Swift-Search-Agent

# Install dependencies
pip install -r requirements.txt

# Copy environment template
cp .env.example .env

# Run the recommended engine
python search_unified.py
```

### Test the API

```bash
curl -X POST "http://localhost:8000/search" \
  -H "Content-Type: application/json" \
  -d '{"query": "What is machine learning?"}'
```

> **Note:** You must have a SearxNG instance running. Set `SEARXNG_URL` in your `.env` to point to it (default: `http://localhost:8080`).

### Choose Your Engine

```bash
# Recommended — auto-adapts to your system
python search_unified.py

# For extreme low-RAM environments (<60MB peak)
python search_optimized.py

# Maximum performance with CLI controls
python search_ultra.py --tier beast --port 8080
```

---

## ⚙️ Environment Variables

All variables are **optional** — sensible defaults are built-in.

| `SEARXNG_URL` | `http://localhost:8080` | Your private SearxNG endpoint |
| `SEARXNG_ENGINES` | `duckduckgo,brave,yahoo,qwant,mojeek` | Search engines to use (no Google/Bing) |
| `SEARCH_MODE` | `unified` | Engine mode: `unified` or `separate` |
| `SEARCH_RAM_TIER` | Auto-detected | Force tier: `micro`, `small`, `medium`, `large` |
| `SEARCH_QUALITY` | Tier-based | Extraction quality: `high`, `medium`, `fast` |
| `SEARCH_EARLY_STOP` | `0.75` | Stop scraping when N% of context buffer filled |
| `PORT` | `8000` | Server port |

---

## 📦 Core Dependencies

| Package | Purpose | Used By |
|---|---|---|
| [**FastAPI**](https://fastapi.tiangolo.com/) | Async API framework | All engines |
| [**httpx**](https://www.python-httpx.org/) | Async HTTP client | Unified, Optimized, Legacy |
| [**aiohttp**](https://docs.aiohttp.org/) | High-performance HTTP with HTTP/2 | Ultra engine |
| [**trafilatura**](https://trafilatura.readthedocs.io/) | Web scraping & text extraction | All engines |
| [**selectolax**](https://github.com/rushter/selectolax) | C-speed DOM parsing (Lexbor) | Optimized, Ultra |
| [**psutil**](https://github.com/giampaolo/psutil) | System RAM detection | Ultra, Config |
| [**aiodns**](https://github.com/saghul/aiodns) | Async DNS resolution | Ultra engine |
| [**cachetools**](https://github.com/tkem/cachetools) | TTL-based DNS caching | Ultra engine |
| [**pydantic**](https://docs.pydantic.dev/) | Data validation | All engines |
| [**uvicorn**](https://www.uvicorn.org/) | ASGI server | All engines |

```bash
pip install -r requirements.txt
```

---

## 🔐 Advanced: Proxy & IP Rotation

For users who need direct Google/Bing searching through personal proxies and IP rotation, see the [`Proxy_Integration_Guide.md`](./Proxy_Integration_Guide.md) for detailed instructions. This is entirely optional — the agent works perfectly out-of-the-box without any proxies.

---

## 🙏 Credits & Acknowledgements

| Project / Service | Description | Link |
|---|---|---|
| **SearxNG** | Privacy-respecting meta-search engine (AGPL-3.0) | [github.com/searxng/searxng](https://github.com/searxng/searxng) |
| **trafilatura** | Web scraping & text extraction library | [trafilatura.readthedocs.io](https://trafilatura.readthedocs.io/) |
| **selectolax** | Lightning-fast HTML parser (Lexbor C backend) | [github.com/rushter/selectolax](https://github.com/rushter/selectolax) |
| **FastAPI** | High-performance Python web framework | [fastapi.tiangolo.com](https://fastapi.tiangolo.com/) |
| **aiohttp** | Async HTTP client/server with HTTP/2 | [docs.aiohttp.org](https://docs.aiohttp.org/) |
| **psutil** | Cross-platform system monitoring | [github.com/giampaolo/psutil](https://github.com/giampaolo/psutil) |

> **Note:** SearxNG is licensed under [AGPL-3.0](https://github.com/searxng/searxng/blob/master/LICENSE). This project uses SearxNG as a **standalone service** and does not modify or redistribute its source code.

---

## 📄 License

This project is licensed under the [MIT License](./LICENSE).

SearxNG (used as a standalone service) is independently licensed under [AGPL-3.0](https://github.com/searxng/searxng/blob/master/LICENSE).

---

<p align="center">
  <strong>Developed & Enhanced by <a href="https://xel-studio.vercel.app/">Sandeep</a></strong>
</p>
