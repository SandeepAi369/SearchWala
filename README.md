# 🔍 Swift Search Agent

A high-performance, AI-powered search API that combines meta-search engine results with intelligent web scraping and LLM synthesis to deliver comprehensive, citation-backed answers to any query.

---

## 🌟 What It Does

Swift Search Agent orchestrates a three-phase pipeline:

1. **Meta-Search** — Queries [SearxNG](https://github.com/searxng/searxng) instances to gather search results from multiple engines (DuckDuckGo, Brave, Wikipedia, and more).
2. **Concurrent Web Scraping** — Scrapes the discovered URLs in parallel using [Trafilatura](https://trafilatura.readthedocs.io/) for clean text extraction, bounded by a semaphore to stay memory-safe.
3. **LLM Synthesis** — Sends the aggregated context to [Cerebras](https://cerebras.ai/) inference API (with a multi-model fallback cascade) to produce a detailed, well-cited answer.

The entire system is stateless, lightweight, and designed to run comfortably on free-tier hosting (512 MB RAM).

---

## 🏗️ Architecture

```
┌──────────────┐     ┌────────────────────┐     ┌──────────────┐
│   User       │────▶│  Swift Scraper API  │────▶│  Cerebras    │
│   (query)    │     │  (FastAPI + httpx)  │     │  LLM API     │
└──────────────┘     └────────┬───────────┘     └──────────────┘
                              │
                     ┌────────▼───────────┐
                     │  Private SearxNG   │
                     │  (Meta-Search)     │
                     └────────────────────┘
```

### Two Hugging Face Spaces

| Space | Purpose |
|---|---|
| **Private-SearxNG** | A private [SearxNG](https://github.com/searxng/searxng) Docker instance that acts as the meta-search backend. Aggregates results from DuckDuckGo, Brave, Wikipedia, Google, and Bing. |
| **Swift-Scraper-API** | The main FastAPI service. Receives user queries, calls Private-SearxNG for URLs, scrapes them concurrently, and synthesizes answers via Cerebras LLM. |

---

## 🙏 Credits & Acknowledgements

This project is built on top of and heavily utilizes the following open-source projects and services:

| Project / Service | Description | Link |
|---|---|---|
| **SearxNG** | Free, privacy-respecting meta-search engine (AGPL-3.0) | [github.com/searxng/searxng](https://github.com/searxng/searxng) |
| **FastAPI** | Modern, high-performance Python web framework | [fastapi.tiangolo.com](https://fastapi.tiangolo.com/) |
| **Trafilatura** | Web scraping and text extraction library | [trafilatura.readthedocs.io](https://trafilatura.readthedocs.io/) |
| **httpx** | Async HTTP client for Python | [python-httpx.org](https://www.python-httpx.org/) |
| **Cerebras** | Ultra-fast AI inference API | [cerebras.ai](https://cerebras.ai/) |
| **Hugging Face Spaces** | Free hosting platform for ML apps | [huggingface.co/spaces](https://huggingface.co/spaces) |
| **Uvicorn** | Lightning-fast ASGI server | [uvicorn.org](https://www.uvicorn.org/) |
| **Pydantic** | Data validation using Python type hints | [docs.pydantic.dev](https://docs.pydantic.dev/) |

> **Note:** SearxNG is licensed under [AGPL-3.0](https://github.com/searxng/searxng/blob/master/LICENSE). This project uses SearxNG as a **standalone service** (Docker container) and does not modify or redistribute its source code.

---

## 🚀 Hosting on Hugging Face Spaces (Step-by-Step)

### Prerequisites

- A free [Hugging Face](https://huggingface.co/) account
- A free [Cerebras](https://cloud.cerebras.ai/) API key

### Step 1: Create the Private SearxNG Space

1. Go to [huggingface.co/new-space](https://huggingface.co/new-space).
2. Set **Space name** to `Private-SearxNG`.
3. Select **Docker** as the SDK.
4. Set visibility to **Public** (required for free tier).
5. Click **Create Space**.
6. Upload all files from the `spaces/private-searxng/` folder:
   - `Dockerfile`
   - `run.sh`
   - `settings.yml` — ⚠️ **Change the `secret_key`** to a random string before uploading.
7. The Space will auto-build and deploy. Wait until the status shows **Running**.

### Step 2: Create the Swift Scraper API Space

1. Create another new Space named `Swift-Scraper-API`.
2. Select **Docker** as the SDK.
3. Upload all files from the `spaces/swift-scraper-api/` folder:
   - `Dockerfile`
   - `app.py`
   - `requirements.txt`
4. In your Space **Settings → Variables and secrets**, add:
   - `SEARXNG_URL` = `https://YOUR_USERNAME-private-searxng.hf.space` (replace `YOUR_USERNAME` with your HF username)
5. The Space will auto-build and start serving on port `7860`.

### Step 3: Test It

Send a POST request to your Swift Scraper API:

```bash
curl -X POST "https://YOUR_USERNAME-swift-scraper-api.hf.space/search" \
  -H "Content-Type: application/json" \
  -H "x-api-key: YOUR_CEREBRAS_API_KEY" \
  -d '{"query": "What is machine learning?"}'
```

### Step 4: Keep It Alive (Optional)

Hugging Face Spaces on the free tier go to sleep after inactivity. To prevent this, set up an [UptimeRobot](https://uptimerobot.com/) monitor (free) to ping your Space's `/health` endpoint every 5 minutes:

```
https://YOUR_USERNAME-swift-scraper-api.hf.space/health
```

---

## ⚙️ Environment Variables

| Variable | Required | Description |
|---|---|---|
| `SEARXNG_URL` | No | URL of your SearxNG instance (defaults to the author's private instance) |
| `PORT` | No | Server port (defaults to `7860` on HF, `8000` locally) |

The Cerebras API key is passed per-request via the `x-api-key` header — it is **never** stored server-side.

---

## 🔐 Advanced: Proxy & IP Rotation

For users who want to unlock direct Google/Bing searching through personal proxies and IP rotation, see the [`Proxy_Integration_Guide.md`](./Proxy_Integration_Guide.md) for detailed instructions and code examples. This is entirely optional — the agent works perfectly out-of-the-box without any proxies.

---

## 📁 Project Structure

```
Swift-Search-Agent/
├── spaces/
│   ├── private-searxng/          # SearxNG Docker Space
│   │   ├── Dockerfile
│   │   ├── run.sh
│   │   └── settings.yml
│   └── swift-scraper-api/        # Main API Space
│       ├── Dockerfile
│       ├── app.py
│       └── requirements.txt
├── main.py                       # Local dev server (v1 — single SearxNG)
├── search.py                     # Local dev server (v2 — 20-instance rotator)
├── requirements.txt              # Python dependencies
├── .env.example                  # Environment variable template
├── .gitignore
├── Proxy_Integration_Guide.md    # Optional proxy setup guide
├── LICENSE
└── README.md
```

---

## 📄 License

This project is licensed under the [MIT License](./LICENSE).

SearxNG (used as a service) is independently licensed under [AGPL-3.0](https://github.com/searxng/searxng/blob/master/LICENSE).
