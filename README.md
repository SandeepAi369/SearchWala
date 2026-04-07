<p align="center">
  <h1 align="center">🔍 Swift Search Agent</h1>
  <p align="center">
    <strong>A high-performance, LLM-agnostic search & data extraction pipeline built for server deployments.</strong>
  </p>
  <p align="center">
    <img src="https://img.shields.io/badge/version-2.0-blueviolet" alt="Version 2.0">
    <img src="https://img.shields.io/badge/python-3.10%2B-blue?logo=python&logoColor=white" alt="Python 3.10+">
    <img src="https://img.shields.io/badge/framework-FastAPI-009688?logo=fastapi&logoColor=white" alt="FastAPI">
    <img src="https://img.shields.io/badge/search-SearxNG-blue?logo=searxng&logoColor=white" alt="SearxNG">
    <img src="https://img.shields.io/badge/extraction-trafilatura-green" alt="trafilatura">
    <img src="https://img.shields.io/badge/LLM-Agnostic-orange" alt="LLM Agnostic">
    <img src="https://img.shields.io/badge/license-MIT-brightgreen" alt="MIT License">
  </p>
</p>

---

## 🌟 What Is Swift Search Agent?

Swift Search Agent is a **production-ready API** that automates the entire research pipeline — from searching the web, to extracting clean text from web pages, to synthesizing answers using **any LLM of your choice**.

It is purpose-built for **server deployments** such as [Hugging Face Spaces](https://huggingface.co/spaces), VPS instances, and cloud platforms, while also running flawlessly on a local machine.

> **🧠 LLM Agnostic by Design:** This agent does **not** lock you into any specific LLM provider. You have complete freedom to connect **any LLM API** (OpenAI, Anthropic, Mistral, Groq, local Ollama, etc.) or **any local model** to process the extracted data. The agent handles the hard part — finding, fetching, and cleaning web content — so your LLM receives high-quality, ready-to-use context.

---

## 🔄 How It Works — Data Flow Pipeline

```
┌─────────────┐      ┌──────────────┐      ┌──────────────┐      ┌──────────────┐
│  User Query │─────▶│   SearxNG    │─────▶│ trafilatura  │─────▶│  Any LLM     │
│             │      │ (Meta-Search)│      │  (Scraping)  │      │ (Synthesis)  │
└─────────────┘      └──────────────┘      └──────────────┘      └──────────────┘
                           │                      │                      │
                     Fetches URLs           Extracts clean          Generates a
                     from multiple          text content from       comprehensive,
                     search engines         each web page           cited answer
```

### Phase-by-Phase Breakdown

| Phase | Component | What It Does |
|---|---|---|
| **1. Meta-Search** | [**SearxNG**](https://github.com/searxng/searxng) | Queries **20 public SearxNG instances** using a **shuffle rotator** (batches of 6 concurrent requests) to collect a massive, diverse pool of result URLs. Instances are randomized each request for load distribution. |
| **2. Data Extraction** | [**trafilatura**](https://trafilatura.readthedocs.io/) | Scrapes up to **60 discovered URLs** concurrently (semaphore-bounded at 12 connections) and extracts **clean, readable text** — stripping ads, navigation, and boilerplate. This is the core data engine of the pipeline. |
| **3. LLM Synthesis** | **Your LLM of Choice** | The cleaned, structured text context is sent to whichever LLM you configure. The default configuration uses a **2-tier Cerebras cascade** (`gpt-oss-120b → llama3.1-8b`) with automatic fallback. The API response includes the `model_used` field so you always know which model served the answer. |

> **Key Insight:** Phases 1 and 2 are fully handled by this agent. Phase 3 is a simple API call that **you control** — swap LLMs anytime without touching the search or scraping logic.

---

## ⚡ Core Dependencies

| Package | Purpose | Install |
|---|---|---|
| [**trafilatura**](https://trafilatura.readthedocs.io/) | **Web scraping & text extraction** — the heart of the data pipeline. Converts raw HTML into clean, structured text. | `pip install trafilatura` |
| [**FastAPI**](https://fastapi.tiangolo.com/) | High-performance async API framework | `pip install fastapi` |
| [**httpx**](https://www.python-httpx.org/) | Async HTTP client for concurrent requests | `pip install httpx` |
| [**uvicorn**](https://www.uvicorn.org/) | Lightning-fast ASGI server | `pip install uvicorn[standard]` |
| [**pydantic**](https://docs.pydantic.dev/) | Data validation via Python type hints | `pip install pydantic` |

**Quick install (all dependencies):**

```bash
pip install -r requirements.txt
```

---

## 🏗️ Architecture

```
Swift-Search-Agent/
├── spaces/
│   ├── private-searxng/              # SearxNG Docker Space (meta-search backend)
│   │   ├── Dockerfile
│   │   ├── README.md                 # HF Spaces metadata
│   │   ├── run.sh
│   │   └── settings.yml
│   └── swift-scraper-api/            # Main API Space (scraping + LLM pipeline)
│       ├── Dockerfile
│       ├── README.md                 # HF Spaces metadata
│       ├── app.py
│       └── requirements.txt
├── main.py                           # Standalone server (single-instance mode)
├── search.py                         # Production server (v2 — 20-instance shuffle rotator + LLM cascade)
├── requirements.txt                  # Python dependencies
├── .env.example                      # Environment variable template
├── Proxy_Integration_Guide.md        # Optional proxy & IP rotation guide
├── LICENSE
└── README.md
```

### Two-Service Architecture

| Service | Role |
|---|---|
| **Private SearxNG** | A self-hosted [SearxNG](https://github.com/searxng/searxng) instance running as a Docker container. Aggregates search results from multiple engines without rate limits. |
| **Swift Scraper API** | The main FastAPI service. Receives queries, calls SearxNG for URLs, uses **trafilatura** to extract text from each page concurrently, then forwards the clean context to your configured LLM. |

---

## 🚀 Deployment

> **While this agent runs flawlessly on a local machine, its architecture is highly optimized and perfect for server deployments** — including [Hugging Face Spaces](https://huggingface.co/spaces), VPS, Docker, and any cloud platform.

### Hosting on Hugging Face Spaces (Recommended)

#### Prerequisites
- A free [Hugging Face](https://huggingface.co/) account
- An API key for your preferred LLM provider

#### Step 1: Deploy Private SearxNG

1. Go to [huggingface.co/new-space](https://huggingface.co/new-space)
2. Name: `Private-SearxNG` → SDK: **Docker** → Create
3. Upload all files from `spaces/private-searxng/`
4. ⚠️ **Change the `secret_key`** in `settings.yml` to a random string before uploading
5. Wait for status: **Running**

#### Step 2: Deploy Swift Scraper API

1. Create another Space: `Swift-Scraper-API` → SDK: **Docker**
2. Upload all files from `spaces/swift-scraper-api/`
3. In **Settings → Variables**, set:
   - `SEARXNG_URL` = `https://YOUR_USERNAME-private-searxng.hf.space`

#### Step 3: Test

```bash
curl -X POST "https://YOUR_USERNAME-swift-scraper-api.hf.space/search" \
  -H "Content-Type: application/json" \
  -H "x-api-key: YOUR_LLM_API_KEY" \
  -d '{"query": "What is machine learning?"}'
```

#### Step 4: Keep It Alive (Optional)

Hugging Face free-tier Spaces sleep after inactivity. Set up [UptimeRobot](https://uptimerobot.com/) (free) to ping your `/health` endpoint every 5 minutes to prevent sleep:

```
https://YOUR_USERNAME-swift-scraper-api.hf.space/health
```

### Running Locally

```bash
# Clone the repo
git clone https://github.com/SandeepAi369/Swift-Search-Agent.git
cd Swift-Search-Agent

# Install dependencies
pip install -r requirements.txt

# Run the server
python search.py
```

---

## ⚙️ Environment Variables

| Variable | Required | Description |
|---|---|---|
| `SEARXNG_URL` | No | URL of your SearxNG instance (defaults to the bundled private instance) |
| `PORT` | No | Server port (defaults to `7860` on HF Spaces, `8000` locally) |

> **Note:** LLM API keys are passed per-request via the `x-api-key` header — they are **never** stored server-side.

---

## 🔐 Advanced: Proxy & IP Rotation

For users who want to unlock direct Google/Bing searching through personal proxies and IP rotation, see the [`Proxy_Integration_Guide.md`](./Proxy_Integration_Guide.md) for detailed instructions and code examples. This is entirely optional — the agent works perfectly out-of-the-box without any proxies.

---

## 🙏 Credits & Acknowledgements

This project is built on top of and utilizes the following open-source projects and services:

| Project / Service | Description | Link |
|---|---|---|
| **SearxNG** | Privacy-respecting meta-search engine (AGPL-3.0) | [github.com/searxng/searxng](https://github.com/searxng/searxng) |
| **trafilatura** | Web scraping & text extraction library | [trafilatura.readthedocs.io](https://trafilatura.readthedocs.io/) |
| **FastAPI** | High-performance Python web framework | [fastapi.tiangolo.com](https://fastapi.tiangolo.com/) |
| **httpx** | Async HTTP client for Python | [python-httpx.org](https://www.python-httpx.org/) |
| **Uvicorn** | Lightning-fast ASGI server | [uvicorn.org](https://www.uvicorn.org/) |
| **Pydantic** | Data validation using Python type hints | [docs.pydantic.dev](https://docs.pydantic.dev/) |
| **Hugging Face Spaces** | Free hosting platform for ML apps | [huggingface.co/spaces](https://huggingface.co/spaces) |
| **UptimeRobot** | Free uptime monitoring to prevent HF sleep | [uptimerobot.com](https://uptimerobot.com/) |

> **Note:** SearxNG is licensed under [AGPL-3.0](https://github.com/searxng/searxng/blob/master/LICENSE). This project uses SearxNG as a **standalone service** (Docker container) and does not modify or redistribute its source code.

---

## 📄 License

This project is licensed under the [MIT License](./LICENSE).

SearxNG (used as a standalone service) is independently licensed under [AGPL-3.0](https://github.com/searxng/searxng/blob/master/LICENSE).

---

<p align="center">
  <strong>Developed & Enhanced by <a href="https://xel-studio.vercel.app/">Sandeep</a></strong>
</p>
