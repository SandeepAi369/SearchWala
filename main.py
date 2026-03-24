"""
Limitless Advanced Search API — Production-Ready, 512MB-Safe
=============================================================
Hyper-lightweight FastAPI service: SearxNG meta-search → massive concurrent
scraping (trafilatura + semaphore + asyncio.to_thread) → Cerebras LLM synthesis.

Hard constraints:
  • Render Free Tier: 512 MB RAM, 0.5 vCPU
  • Zero heavy frameworks, zero databases, 100 % stateless & in-memory
  • Explicit GC after every request cycle to prevent OOM
"""

from __future__ import annotations

import asyncio
import gc
import logging
import os
import sys
import time
from typing import Optional
from urllib.parse import quote_plus, urlparse

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
log = logging.getLogger("search-api")

# ─────────────────────────── App ────────────────────────────────
app = FastAPI(
    title="Limitless Advanced Search API",
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

# ─────────────────── Constants & Tunables ───────────────────────
# SearxNG public instances (rotated per request)
SEARXNG_INSTANCES: list[str] = [
    "https://search.sapti.me",
    "https://searxng.site",
    "https://search.ononoki.org",
    "https://searx.tiekoetter.com",
]

MAX_URLS: int = 60                 # cap unique URLs from meta-search
SCRAPE_SEMAPHORE_LIMIT: int = 12   # max concurrent outbound connections
SCRAPE_TIMEOUT_SEC: float = 6.0    # per-URL hard timeout
MAX_CONTEXT_CHARS: int = 80_000    # hard-slice before LLM call
CEREBRAS_API_URL: str = "https://api.cerebras.ai/v1/chat/completions"
CEREBRAS_MODEL: str = "llama3.1-8b"

# Shared HTTP client headers
_UA = "Mozilla/5.0 (compatible; LimitlessSearchBot/1.0)"
_HEADERS = {"User-Agent": _UA}


# ─────────────────── Pydantic Models ───────────────────────────
class SearchRequest(BaseModel):
    query: str = Field(..., min_length=1, max_length=1000, description="User search query")


class SearchResponse(BaseModel):
    query: str
    sources_found: int
    sources_scraped: int
    answer: str
    citations: list[str]
    elapsed_seconds: float


# ═══════════════════════════════════════════════════════════════
# PHASE 1 — META-SEARCH (SearxNG Rotator)
# ═══════════════════════════════════════════════════════════════

async def _query_searxng_instance(
    client: httpx.AsyncClient,
    instance_url: str,
    query: str,
) -> list[str]:
    """Hit one SearxNG instance and return a list of result URLs."""
    urls: list[str] = []
    params = {
        "q": query,
        "format": "json",
        "categories": "general",
        "language": "en",
        "pageno": 1,
    }
    try:
        resp = await client.get(
            f"{instance_url.rstrip('/')}/search",
            params=params,
            headers=_HEADERS,
            timeout=8.0,
        )
        resp.raise_for_status()
        data = resp.json()
        for result in data.get("results", []):
            url = result.get("url", "").strip()
            if url and url.startswith("http"):
                urls.append(url)
    except Exception as exc:
        log.warning("SearxNG instance %s failed: %s", instance_url, exc)
    return urls


async def meta_search(query: str) -> list[str]:
    """Query multiple SearxNG instances concurrently; return deduplicated URLs."""
    seen: set[str] = set()
    unique_urls: list[str] = []

    async with httpx.AsyncClient(follow_redirects=True) as client:
        tasks = [
            _query_searxng_instance(client, inst, query)
            for inst in SEARXNG_INSTANCES
        ]
        results = await asyncio.gather(*tasks, return_exceptions=True)

    for batch in results:
        if isinstance(batch, BaseException):
            continue
        for url in batch:
            # Normalise to domain+path for dedup
            parsed = urlparse(url)
            key = f"{parsed.netloc}{parsed.path}".lower().rstrip("/")
            if key not in seen:
                seen.add(key)
                unique_urls.append(url)
            if len(unique_urls) >= MAX_URLS:
                break
        if len(unique_urls) >= MAX_URLS:
            break

    log.info("Meta-search returned %d unique URLs for query: %s", len(unique_urls), query[:80])
    return unique_urls


# ═══════════════════════════════════════════════════════════════
# PHASE 2 — MASSIVE CONCURRENT SCRAPING (OOM-Safe)
# ═══════════════════════════════════════════════════════════════

_scrape_semaphore: asyncio.Semaphore | None = None


def _get_semaphore() -> asyncio.Semaphore:
    """Lazily create semaphore inside the running event loop."""
    global _scrape_semaphore
    if _scrape_semaphore is None:
        _scrape_semaphore = asyncio.Semaphore(SCRAPE_SEMAPHORE_LIMIT)
    return _scrape_semaphore


def _extract_text_sync(html: str, url: str) -> str:
    """Synchronous trafilatura extraction (CPU-bound)."""
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
    """Download + extract text from a single URL within the semaphore gate."""
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
            # trafilatura is synchronous & CPU-bound → offload to thread
            text = await asyncio.to_thread(_extract_text_sync, html, url)
            return url, text
        except Exception:
            return url, ""


async def scrape_urls(urls: list[str]) -> list[tuple[str, str]]:
    """
    Scrape all URLs concurrently (bounded by semaphore).
    Returns list of (url, extracted_text) tuples.
    Performs explicit GC afterwards to reclaim RAM.
    """
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
# PHASE 3 — TEXT CLEANING & LLM SYNTHESIS (Cerebras BYOK)
# ═══════════════════════════════════════════════════════════════

def _build_context_block(scraped: list[tuple[str, str]]) -> tuple[str, list[str]]:
    """
    Concatenate scraped texts with source markers.
    Hard-slice to MAX_CONTEXT_CHARS.  Return (context, citation_urls).
    """
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

    # Cleanup
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


async def synthesize_with_cerebras(
    query: str,
    context: str,
    citations: list[str],
    api_key: str,
) -> str:
    """Call Cerebras chat completions endpoint (OpenAI-compatible)."""
    if not context.strip():
        return (
            "I was unable to extract meaningful content from the search results. "
            "Please try rephrasing your query or try again later."
        )

    payload = {
        "model": CEREBRAS_MODEL,
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
            resp = await client.post(
                CEREBRAS_API_URL,
                json=payload,
                headers=headers,
                timeout=30.0,
            )
            resp.raise_for_status()
            data = resp.json()
            answer = (
                data.get("choices", [{}])[0]
                .get("message", {})
                .get("content", "")
                .strip()
            )
            if not answer:
                return "The LLM returned an empty response. Please try again."
            return answer
        except httpx.HTTPStatusError as e:
            status = e.response.status_code
            detail = e.response.text[:300]
            log.error("Cerebras API error %d: %s", status, detail)
            if status == 401:
                raise HTTPException(status_code=401, detail="Invalid Cerebras API key.")
            if status == 429:
                raise HTTPException(status_code=429, detail="Cerebras rate limit hit. Retry later.")
            raise HTTPException(status_code=502, detail=f"Cerebras upstream error ({status}).")
        except Exception as exc:
            log.error("Cerebras call failed: %s", exc)
            raise HTTPException(status_code=502, detail="Failed to reach Cerebras API.")
        finally:
            # Free payload from memory
            del payload
            gc.collect()


# ═══════════════════════════════════════════════════════════════
# ENDPOINTS
# ═══════════════════════════════════════════════════════════════

@app.get("/health")
async def health():
    """UptimeRobot ping-hack: keeps Render free tier alive."""
    return {"status": "ok"}


@app.post("/search", response_model=SearchResponse)
async def search(body: SearchRequest, x_api_key: str = Header(..., alias="x-api-key")):
    """
    Main endpoint — orchestrates the full pipeline:
      1. Meta-search via SearxNG
      2. Concurrent scraping with trafilatura (semaphore-bounded)
      3. LLM synthesis via Cerebras
    """
    t0 = time.perf_counter()
    query = body.query.strip()
    log.info("━━━ NEW SEARCH ━━━  query=%s", query[:100])

    # ── Phase 1: Meta-Search ──
    urls = await meta_search(query)
    if not urls:
        raise HTTPException(
            status_code=404,
            detail="No search results found. All SearxNG instances may be down.",
        )
    sources_found = len(urls)

    # ── Phase 2: Concurrent Scraping ──
    scraped = await scrape_urls(urls)
    sources_scraped = sum(1 for _, text in scraped if text and len(text.strip()) >= 50)
    log.info("Scraped %d / %d URLs successfully", sources_scraped, sources_found)

    # Free URL list immediately
    del urls
    gc.collect()

    # ── Phase 3: Synthesize ──
    context, citations = _build_context_block(scraped)

    # Free scraped data before LLM call (largest memory consumer)
    del scraped
    gc.collect()

    answer = await synthesize_with_cerebras(query, context, citations, x_api_key)

    # Final cleanup
    del context
    gc.collect()

    elapsed = round(time.perf_counter() - t0, 2)
    log.info("━━━ DONE ━━━  elapsed=%.2fs  sources=%d/%d", elapsed, sources_scraped, sources_found)

    return SearchResponse(
        query=query,
        sources_found=sources_found,
        sources_scraped=sources_scraped,
        answer=answer,
        citations=citations,
        elapsed_seconds=elapsed,
    )


# ─────────────────── Global Error Handler ───────────────────────
@app.exception_handler(Exception)
async def _global_exc_handler(request: Request, exc: Exception):
    log.exception("Unhandled error: %s", exc)
    gc.collect()  # Attempt RAM recovery even on crash
    return JSONResponse(
        status_code=500,
        content={"detail": "Internal server error. Please try again."},
    )


# ─────────────────── Entrypoint ─────────────────────────────────
if __name__ == "__main__":
    import uvicorn

    port = int(os.environ.get("PORT", 8000))
    log.info("Starting Limitless Search API on port %d", port)
    uvicorn.run(
        "main:app",
        host="0.0.0.0",
        port=port,
        workers=1,        # single worker — 512MB safety
        log_level="info",
        access_log=False,  # reduce log noise on free tier
    )
