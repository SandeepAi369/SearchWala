"""
Swift Scraper API — Hugging Face Spaces Edition
=================================================
FastAPI: Private SearxNG meta-search → concurrent scraping (trafilatura +
semaphore + asyncio.to_thread) → Cerebras LLM cascade (gpt-oss-120b → llama3.1-8b).

Deployed on: HF Spaces (cpu-basic, 16GB RAM)
Port: 7860 (HF requirement)
"""

from __future__ import annotations

import asyncio
import gc
import logging
import os
import sys
import time
from urllib.parse import urlparse

import httpx
import trafilatura
from fastapi import FastAPI, Header, HTTPException, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse
from pydantic import BaseModel, Field

# ─────────────────────────── Logging ────────────────────────────
logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s | %(levelname)-7s | %(message)s",
    datefmt="%H:%M:%S",
    stream=sys.stdout,
)
log = logging.getLogger("swift-scraper")

# ─────────────────────────── App ────────────────────────────────
app = FastAPI(
    title="Swift Scraper API",
    version="2.0.0",
    docs_url="/docs",
    redoc_url=None,
)

app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
)

# ═══════════════════════════════════════════════════════════════
# CONSTANTS & TUNABLES
# ═══════════════════════════════════════════════════════════════

# SearxNG instance URL — set via environment variable
# Deploy your own Private-SearxNG Space and set SEARXNG_URL accordingly
SEARXNG_URL: str = os.environ.get(
    "SEARXNG_URL",
    "https://YOUR_USERNAME-private-searxng.hf.space",
)

MAX_URLS: int = 50                 # hard cap — protects Cerebras context window
SCRAPE_SEMAPHORE_LIMIT: int = 12   # max concurrent outbound scrape connections
SCRAPE_TIMEOUT_SEC: float = 6.0    # per-URL hard timeout
MAX_CONTEXT_CHARS: int = 80_000    # hard-slice before LLM call
CEREBRAS_API_URL: str = "https://api.cerebras.ai/v1/chat/completions"

# LLM fallback cascade
CEREBRAS_MODEL_CASCADE: list[str] = [
    "gpt-oss-120b",    # Priority 1 — reasoning model (120B)
    "llama3.1-8b",     # Priority 2 — lightweight fallback (8B)
]

_UA = "Mozilla/5.0 (compatible; SwiftScraperBot/2.0)"
_HEADERS = {"User-Agent": _UA}


# ─────────────────── Pydantic Models ───────────────────────────
class SearchRequest(BaseModel):
    query: str = Field(..., min_length=1, max_length=1000)


class SearchResponse(BaseModel):
    query: str
    sources_found: int
    sources_scraped: int
    answer: str
    model_used: str
    citations: list[str]
    elapsed_seconds: float


# ═══════════════════════════════════════════════════════════════
# PHASE 1 — META-SEARCH (Private SearxNG)
# ═══════════════════════════════════════════════════════════════

async def meta_search(query: str) -> list[str]:
    """Query our private SearxNG instance.  Returns up to MAX_URLS unique URLs."""
    seen: set[str] = set()
    unique_urls: list[str] = []

    params = {
        "q": query,
        "format": "json",
        "categories": "general",
        "language": "en",
        "pageno": 1,
    }

    async with httpx.AsyncClient(follow_redirects=True) as client:
        try:
            resp = await client.get(
                f"{SEARXNG_URL.rstrip('/')}/search",
                params=params,
                headers=_HEADERS,
                timeout=15.0,
            )
            resp.raise_for_status()
            data = resp.json()
            for result in data.get("results", []):
                url = result.get("url", "").strip()
                if url and url.startswith("http"):
                    parsed = urlparse(url)
                    key = f"{parsed.netloc}{parsed.path}".lower().rstrip("/")
                    if key not in seen:
                        seen.add(key)
                        unique_urls.append(url)
                    if len(unique_urls) >= MAX_URLS:
                        break
        except Exception as exc:
            log.error("SearxNG query failed: %s", exc)

    log.info("Meta-search returned %d unique URLs for: %s", len(unique_urls), query[:80])
    return unique_urls


# ═══════════════════════════════════════════════════════════════
# PHASE 2 — CONCURRENT SCRAPING (OOM-Safe)
# ═══════════════════════════════════════════════════════════════

_scrape_semaphore: asyncio.Semaphore | None = None


def _get_semaphore() -> asyncio.Semaphore:
    global _scrape_semaphore
    if _scrape_semaphore is None:
        _scrape_semaphore = asyncio.Semaphore(SCRAPE_SEMAPHORE_LIMIT)
    return _scrape_semaphore


def _extract_text_sync(html: str, url: str) -> str:
    try:
        text = trafilatura.extract(
            html,
            include_comments=False,
            include_tables=False,
            no_fallback=True,
            url=url,
        )
        return text or ""
    except Exception:
        return ""


async def _scrape_single_url(client: httpx.AsyncClient, url: str) -> tuple[str, str]:
    sem = _get_semaphore()
    async with sem:
        try:
            resp = await client.get(
                url,
                headers=_HEADERS,
                timeout=SCRAPE_TIMEOUT_SEC,
                follow_redirects=True,
            )
            if resp.status_code != 200:
                return url, ""
            content_type = resp.headers.get("content-type", "")
            if "text/html" not in content_type and "text/plain" not in content_type:
                return url, ""
            html = resp.text
            text = await asyncio.to_thread(_extract_text_sync, html, url)
            return url, text
        except Exception:
            return url, ""


async def scrape_urls(urls: list[str]) -> list[tuple[str, str]]:
    results: list[tuple[str, str]] = []
    async with httpx.AsyncClient(
        follow_redirects=True,
        limits=httpx.Limits(max_connections=SCRAPE_SEMAPHORE_LIMIT, max_keepalive_connections=5),
    ) as client:
        tasks = [_scrape_single_url(client, url) for url in urls]
        raw = await asyncio.gather(*tasks, return_exceptions=True)
        for item in raw:
            if isinstance(item, BaseException):
                results.append(("", ""))
            else:
                results.append(item)

    # ── MANDATORY MEMORY CLEANUP ──
    del tasks, raw
    gc.collect()

    return results


# ═══════════════════════════════════════════════════════════════
# PHASE 3 — LLM SYNTHESIS (Cerebras Cascade)
# ═══════════════════════════════════════════════════════════════

def _build_context_block(scraped: list[tuple[str, str]]) -> tuple[str, list[str]]:
    parts: list[str] = []
    citations: list[str] = []
    char_count = 0

    for idx, (url, text) in enumerate(scraped, 1):
        if not text or len(text.strip()) < 50:
            continue
        snippet = text.strip()
        marker = f"\n\n--- Source [{idx}]: {url} ---\n{snippet}"
        if char_count + len(marker) > MAX_CONTEXT_CHARS:
            remaining = MAX_CONTEXT_CHARS - char_count
            if remaining > 200:
                parts.append(marker[:remaining])
                citations.append(url)
            break
        parts.append(marker)
        citations.append(url)
        char_count += len(marker)

    context = "".join(parts)
    del parts
    gc.collect()
    return context, citations


def _build_system_prompt() -> str:
    return (
        "You are an advanced research assistant. "
        "Using ONLY the provided source context below, write a comprehensive, "
        "highly detailed, and well-structured answer to the user's query. "
        "Include inline citations in the format [Source N](url) where possible. "
        "If the context is insufficient, state what is known and what could not be verified. "
        "Do NOT fabricate information beyond what the sources provide."
    )


async def _try_cerebras_model(model: str, query: str, context: str, api_key: str) -> str:
    payload = {
        "model": model,
        "messages": [
            {"role": "system", "content": _build_system_prompt()},
            {
                "role": "user",
                "content": (
                    f"## Query\n{query}\n\n"
                    f"## Source Context\n{context}\n\n"
                    "Now write your comprehensive answer with inline citations."
                ),
            },
        ],
        "temperature": 0.3,
        "max_tokens": 4096,
        "stream": False,
    }
    headers = {
        "Content-Type": "application/json",
        "Authorization": f"Bearer {api_key}",
    }

    async with httpx.AsyncClient() as client:
        try:
            resp = await client.post(CEREBRAS_API_URL, json=payload, headers=headers, timeout=30.0)
            if resp.status_code == 401:
                raise HTTPException(status_code=401, detail="Invalid Cerebras API key.")
            if resp.status_code == 429:
                raise HTTPException(status_code=429, detail="Cerebras rate limit hit.")
            resp.raise_for_status()
            data = resp.json()
            msg = data.get("choices", [{}])[0].get("message", {})
            answer = (msg.get("content", "") or msg.get("reasoning", "") or "").strip()
            if not answer:
                raise ValueError(f"Model {model} returned empty response")
            return answer
        except HTTPException:
            raise
        except Exception as exc:
            log.warning("Model '%s' failed: %s", model, exc)
            raise
        finally:
            del payload
            gc.collect()


async def synthesize_with_cerebras(
    query: str, context: str, citations: list[str], api_key: str
) -> tuple[str, str]:
    if not context.strip():
        return (
            "I was unable to extract meaningful content from the search results. "
            "Please try rephrasing your query or try again later.",
            "none",
        )

    last_error: Exception | None = None
    for model in CEREBRAS_MODEL_CASCADE:
        try:
            log.info("Trying model: %s", model)
            answer = await _try_cerebras_model(model, query, context, api_key)
            log.info("Model '%s' succeeded", model)
            return answer, model
        except HTTPException:
            raise
        except Exception as exc:
            last_error = exc
            log.warning("Model '%s' failed, trying next...", model)
            continue

    raise HTTPException(status_code=502, detail=f"All models failed. Last: {last_error}")


# ═══════════════════════════════════════════════════════════════
# ENDPOINTS
# ═══════════════════════════════════════════════════════════════

@app.api_route("/", methods=["GET", "HEAD"])
async def root():
    """Root endpoint for UptimeRobot pings."""
    return {"status": "ok", "service": "Swift Scraper API"}


@app.api_route("/health", methods=["GET", "HEAD"])
async def health():
    return {"status": "ok"}


@app.post("/search", response_model=SearchResponse)
async def search(body: SearchRequest, x_api_key: str = Header(..., alias="x-api-key")):
    t0 = time.perf_counter()
    query = body.query.strip()
    log.info("━━━ NEW SEARCH ━━━  query=%s", query[:100])

    # Phase 1: Meta-Search
    urls = await meta_search(query)
    if not urls:
        raise HTTPException(status_code=404, detail="No search results found.")
    sources_found = len(urls)

    # Phase 2: Scrape
    scraped = await scrape_urls(urls)
    sources_scraped = sum(1 for _, text in scraped if text and len(text.strip()) >= 50)
    log.info("Scraped %d / %d URLs", sources_scraped, sources_found)
    del urls
    gc.collect()

    # Phase 3: Synthesize
    context, citations = _build_context_block(scraped)
    del scraped
    gc.collect()

    answer, model_used = await synthesize_with_cerebras(query, context, citations, x_api_key)
    del context
    gc.collect()

    elapsed = round(time.perf_counter() - t0, 2)
    log.info("━━━ DONE ━━━  model=%s  elapsed=%.2fs  sources=%d/%d",
             model_used, elapsed, sources_scraped, sources_found)

    return SearchResponse(
        query=query,
        sources_found=sources_found,
        sources_scraped=sources_scraped,
        answer=answer,
        model_used=model_used,
        citations=citations,
        elapsed_seconds=elapsed,
    )


@app.exception_handler(Exception)
async def _global_exc_handler(request: Request, exc: Exception):
    log.exception("Unhandled error: %s", exc)
    gc.collect()
    return JSONResponse(status_code=500, content={"detail": "Internal server error."})


# ─────────────────── Entrypoint ─────────────────────────────────
if __name__ == "__main__":
    import uvicorn

    port = int(os.environ.get("PORT", 7860))
    log.info("Starting Swift Scraper API on port %d", port)
    log.info("SearxNG: %s", SEARXNG_URL)
    log.info("LLM cascade: %s", " → ".join(CEREBRAS_MODEL_CASCADE))
    uvicorn.run(
        "app:app",
        host="0.0.0.0",
        port=port,
        workers=1,
        log_level="info",
        access_log=False,
    )
