"""
Limitless Advanced Search API v2 — Production-Ready, 512MB-Safe
================================================================
FastAPI: SearxNG meta-search (20-instance shuffle rotator) → massive concurrent
scraping (trafilatura + semaphore + asyncio.to_thread) → 3-tier Cerebras LLM
fallback cascade (gpt-oss-120b → zai-glm-4.7 → llama3.1-8b).

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
import random
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

# ═══════════════════════════════════════════════════════════════
# CONSTANTS & TUNABLES
# ═══════════════════════════════════════════════════════════════

# 20 public SearxNG instances — shuffled each request for load distribution
SEARXNG_INSTANCES: list[str] = [
    "https://search.sapti.me",
    "https://searxng.site",
    "https://search.ononoki.org",
    "https://searx.tiekoetter.com",
    "https://searx.be",
    "https://search.bus-hit.me",
    "https://searx.fmac.xyz",
    "https://searx.zhenyapav.com",
    "https://search.hbubli.cc",
    "https://searx.work",
    "https://search.mdosch.de",
    "https://searx.colbster937.dev",
    "https://searx.namejeff.xyz",
    "https://search.rowie.at",
    "https://searx.dresden.network",
    "https://searx.catfock.com",
    "https://searx.ox2.fr",
    "https://searx.mha.fi",
    "https://priv.au",
    "https://search.toolforge.org",
]

MAX_URLS: int = 60                 # cap unique URLs from meta-search
SEARXNG_CONCURRENT: int = 6        # query 6 instances at a time in batches
SCRAPE_SEMAPHORE_LIMIT: int = 12   # max concurrent outbound scrape connections
SCRAPE_TIMEOUT_SEC: float = 6.0    # per-URL hard timeout
MAX_CONTEXT_CHARS: int = 80_000    # hard-slice before LLM call
CEREBRAS_API_URL: str = "https://api.cerebras.ai/v1/chat/completions"

# LLM fallback cascade — tried in order
# (only models verified available on this Cerebras key)
CEREBRAS_MODEL_CASCADE: list[str] = [
    "gpt-oss-120b",    # Priority 1 — reasoning model (120B)
    "llama3.1-8b",     # Priority 2 — lightweight fallback (8B)
]

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
    model_used: str
    citations: list[str]
    elapsed_seconds: float


# ═══════════════════════════════════════════════════════════════
# PHASE 1 — META-SEARCH (Aggressive SearxNG Shuffle Rotator)
# ═══════════════════════════════════════════════════════════════

async def _query_searxng_instance(
    client: httpx.AsyncClient,
    instance_url: str,
    query: str,
) -> list[str]:
    """Hit one SearxNG instance and return result URLs. Silently handles 429/500."""
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
        if resp.status_code in (429, 500, 502, 503):
            log.debug("SearxNG %s → %d, skipping", instance_url, resp.status_code)
            return []
        resp.raise_for_status()
        data = resp.json()
        for result in data.get("results", []):
            url = result.get("url", "").strip()
            if url and url.startswith("http"):
                urls.append(url)
    except Exception as exc:
        log.debug("SearxNG %s failed: %s", instance_url, type(exc).__name__)
    return urls


async def meta_search(query: str) -> list[str]:
    """
    Shuffle 20 SearxNG instances, query in batches of 6 concurrently.
    Stop as soon as we collect >= MAX_URLS unique URLs.
    """
    seen: set[str] = set()
    unique_urls: list[str] = []

    # Shuffle to distribute load and avoid always hitting same instances
    shuffled = SEARXNG_INSTANCES.copy()
    random.shuffle(shuffled)

    async with httpx.AsyncClient(follow_redirects=True) as client:
        # Process in batches of SEARXNG_CONCURRENT
        for batch_start in range(0, len(shuffled), SEARXNG_CONCURRENT):
            if len(unique_urls) >= MAX_URLS:
                break

            batch = shuffled[batch_start : batch_start + SEARXNG_CONCURRENT]
            log.info(
                "SearxNG batch %d-%d of %d  (have %d URLs so far)",
                batch_start + 1,
                batch_start + len(batch),
                len(shuffled),
                len(unique_urls),
            )

            tasks = [_query_searxng_instance(client, inst, query) for inst in batch]
            results = await asyncio.gather(*tasks, return_exceptions=True)

            for result in results:
                if isinstance(result, BaseException):
                    continue
                for url in result:
                    parsed = urlparse(url)
                    key = f"{parsed.netloc}{parsed.path}".lower().rstrip("/")
                    if key not in seen:
                        seen.add(key)
                        unique_urls.append(url)
                    if len(unique_urls) >= MAX_URLS:
                        break
                if len(unique_urls) >= MAX_URLS:
                    break

    log.info("Meta-search returned %d unique URLs for: %s", len(unique_urls), query[:80])
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
# PHASE 3 — TEXT CLEANING & LLM SYNTHESIS (3-Tier Cerebras Cascade)
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


async def _try_cerebras_model(
    model: str,
    query: str,
    context: str,
    api_key: str,
) -> str:
    """
    Attempt a single Cerebras model call. Returns answer string on success.
    Raises on any failure so the cascade can try the next model.
    """
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
            resp = await client.post(
                CEREBRAS_API_URL,
                json=payload,
                headers=headers,
                timeout=30.0,
            )
            # 401 = bad key → don't cascade, fail immediately
            if resp.status_code == 401:
                raise HTTPException(status_code=401, detail="Invalid Cerebras API key.")
            # 429 = rate limit → don't cascade, fail immediately
            if resp.status_code == 429:
                raise HTTPException(status_code=429, detail="Cerebras rate limit hit. Retry later.")

            resp.raise_for_status()
            data = resp.json()
            msg = data.get("choices", [{}])[0].get("message", {})
            # gpt-oss-120b is a reasoning model → answer is in "reasoning" field
            # other models use standard "content" field
            answer = (
                msg.get("content", "")
                or msg.get("reasoning", "")
                or ""
            ).strip()
            if not answer:
                raise ValueError(f"Model {model} returned empty response")
            return answer

        except HTTPException:
            raise  # Re-raise auth/rate-limit errors — no fallback for these
        except Exception as exc:
            log.warning("Model '%s' failed: %s", model, exc)
            raise  # Let cascade handle it
        finally:
            del payload
            gc.collect()


async def synthesize_with_cerebras(
    query: str,
    context: str,
    citations: list[str],
    api_key: str,
) -> tuple[str, str]:
    """
    3-tier Cerebras fallback cascade.
    Tries each model in CEREBRAS_MODEL_CASCADE until one succeeds.
    Returns (answer, model_used).
    """
    if not context.strip():
        return (
            "I was unable to extract meaningful content from the search results. "
            "Please try rephrasing your query or try again later.",
            "none",
        )

    last_error: Exception | None = None

    for model in CEREBRAS_MODEL_CASCADE:
        try:
            log.info("🔄 Trying model: %s", model)
            answer = await _try_cerebras_model(model, query, context, api_key)
            log.info("✅ Model '%s' succeeded", model)
            return answer, model
        except HTTPException:
            raise  # Auth/rate-limit → stop cascade, fail fast
        except Exception as exc:
            last_error = exc
            log.warning("⚠️  Model '%s' failed, trying next in cascade...", model)
            continue

    # All models exhausted
    log.error("❌ All %d models in cascade failed", len(CEREBRAS_MODEL_CASCADE))
    raise HTTPException(
        status_code=502,
        detail=f"All Cerebras models failed. Last error: {last_error}",
    )


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
      1. Meta-search via SearxNG (20-instance shuffle rotator)
      2. Concurrent scraping with trafilatura (semaphore-bounded)
      3. LLM synthesis via Cerebras (3-tier fallback cascade)
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

    # ── Phase 3: Synthesize (3-tier cascade) ──
    context, citations = _build_context_block(scraped)

    # Free scraped data before LLM call (largest memory consumer)
    del scraped
    gc.collect()

    answer, model_used = await synthesize_with_cerebras(query, context, citations, x_api_key)

    # Final cleanup
    del context
    gc.collect()

    elapsed = round(time.perf_counter() - t0, 2)
    log.info(
        "━━━ DONE ━━━  model=%s  elapsed=%.2fs  sources=%d/%d",
        model_used, elapsed, sources_scraped, sources_found,
    )

    return SearchResponse(
        query=query,
        sources_found=sources_found,
        sources_scraped=sources_scraped,
        answer=answer,
        model_used=model_used,
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
    log.info("Starting Limitless Search API v2 on port %d", port)
    log.info("SearxNG pool: %d instances", len(SEARXNG_INSTANCES))
    log.info("LLM cascade: %s", " → ".join(CEREBRAS_MODEL_CASCADE))
    uvicorn.run(
        "search:app",
        host="0.0.0.0",
        port=port,
        workers=1,        # single worker — 512MB safety
        log_level="info",
        access_log=False,  # reduce log noise on free tier
    )
