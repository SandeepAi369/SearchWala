"""
Swift Search Agent v2.0 - Unified Optimized Search API
======================================================
Advanced meta-search + web scraping + LLM synthesis in a single optimized file.

Key Features:
- Auto-detects RAM tier and optimizes settings automatically
- Merged SearxNG + Trafilatura with advanced optimizations  
- Streaming HTML with hard caps (prevents OOM)
- Smart URL cleaning (removes tracking params)
- Multi-strategy text extraction with fallback chain
- Early termination when enough content collected
- Single GC pass at end (not per-operation)
- Quality scoring for extractions
- Content hash deduplication

Deployment Targets:
- Render Free Tier (512MB RAM, 0.5 vCPU)
- Railway Free Tier (512MB RAM)  
- Any VPS with 256MB-2GB RAM

Environment Variables (all optional):
- SEARCH_MODE: "unified" (default) or "separate"
- SEARCH_RAM_TIER: "micro", "small", "medium", "large" (auto-detected)
- SEARCH_QUALITY: "high", "medium", "fast" (tier-based default)
- SEARCH_EARLY_STOP: 0.75 (stop when 75% context filled)
- LLM_API_URL: Custom LLM endpoint
- LLM_MODEL: Custom model name
- PORT: Server port (default 8000)

Usage:
    # Auto-detect everything (recommended)
    python search_unified.py
    
    # Force low-RAM mode
    SEARCH_RAM_TIER=micro python search_unified.py
    
    # High quality mode
    SEARCH_QUALITY=high python search_unified.py
"""

from __future__ import annotations

import asyncio
import gc
import hashlib
import logging
import os
import re
import sys
import time
from dataclasses import dataclass, field
from enum import Enum
from io import StringIO
from typing import Optional, NamedTuple
from urllib.parse import parse_qs, urlencode, urlparse, urlunparse

import httpx
import trafilatura
from fastapi import FastAPI, Header, HTTPException, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse
from pydantic import BaseModel, Field


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 1: CONFIGURATION - Auto-detecting RAM tier and optimal settings
# ═══════════════════════════════════════════════════════════════════════════════

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s | %(levelname)-7s | %(message)s",
    datefmt="%H:%M:%S",
    stream=sys.stdout,
)
log = logging.getLogger("swift-search")


class RAMTier(Enum):
    MICRO = "micro"    # ≤256MB
    SMALL = "small"    # ≤512MB
    MEDIUM = "medium"  # ≤1GB
    LARGE = "large"    # >1GB


class ExtractionQuality(Enum):
    HIGH = "high"      # Full extraction (tables, comments, fallbacks)
    MEDIUM = "medium"  # Balanced (default)
    FAST = "fast"      # Minimal extraction


@dataclass
class TierConfig:
    """Optimized settings for a specific RAM tier."""
    semaphore_limit: int
    max_urls: int
    html_cap_bytes: int
    max_context_chars: int
    scrape_timeout_sec: float
    enable_head_check: bool
    quality: ExtractionQuality


TIER_CONFIGS: dict[RAMTier, TierConfig] = {
    RAMTier.MICRO: TierConfig(
        semaphore_limit=3,
        max_urls=25,
        html_cap_bytes=256 * 1024,
        max_context_chars=50_000,
        scrape_timeout_sec=5.0,
        enable_head_check=True,
        quality=ExtractionQuality.FAST,
    ),
    RAMTier.SMALL: TierConfig(
        semaphore_limit=5,
        max_urls=40,
        html_cap_bytes=512 * 1024,
        max_context_chars=70_000,
        scrape_timeout_sec=6.0,
        enable_head_check=True,
        quality=ExtractionQuality.MEDIUM,
    ),
    RAMTier.MEDIUM: TierConfig(
        semaphore_limit=8,
        max_urls=50,
        html_cap_bytes=768 * 1024,
        max_context_chars=80_000,
        scrape_timeout_sec=7.0,
        enable_head_check=False,
        quality=ExtractionQuality.MEDIUM,
    ),
    RAMTier.LARGE: TierConfig(
        semaphore_limit=12,
        max_urls=60,
        html_cap_bytes=1024 * 1024,
        max_context_chars=100_000,
        scrape_timeout_sec=8.0,
        enable_head_check=False,
        quality=ExtractionQuality.HIGH,
    ),
}


def _detect_ram_mb() -> int:
    """Detect available system RAM in MB."""
    try:
        import psutil
        return int(psutil.virtual_memory().total / (1024 * 1024))
    except ImportError:
        pass
    try:
        with open("/proc/meminfo", "r") as f:
            for line in f:
                if line.startswith("MemTotal:"):
                    return int(line.split()[1]) // 1024
    except (FileNotFoundError, PermissionError):
        pass
    return 512  # Safe default


def _determine_tier() -> RAMTier:
    """Determine optimal tier based on RAM or env override."""
    tier_override = os.environ.get("SEARCH_RAM_TIER", "").lower()
    if tier_override in ("micro", "small", "medium", "large"):
        return RAMTier(tier_override)
    
    ram_mb = _detect_ram_mb()
    if ram_mb <= 300:
        return RAMTier.MICRO
    elif ram_mb <= 600:
        return RAMTier.SMALL
    elif ram_mb <= 1200:
        return RAMTier.MEDIUM
    return RAMTier.LARGE


# Initialize configuration
_RAM_TIER = _determine_tier()
_CONFIG = TIER_CONFIGS[_RAM_TIER]

# Apply quality override if specified
_quality_override = os.environ.get("SEARCH_QUALITY", "").lower()
if _quality_override in ("high", "medium", "fast"):
    _CONFIG = TierConfig(
        semaphore_limit=_CONFIG.semaphore_limit,
        max_urls=_CONFIG.max_urls,
        html_cap_bytes=_CONFIG.html_cap_bytes,
        max_context_chars=_CONFIG.max_context_chars,
        scrape_timeout_sec=_CONFIG.scrape_timeout_sec,
        enable_head_check=_CONFIG.enable_head_check,
        quality=ExtractionQuality(_quality_override),
    )

EARLY_STOP_THRESHOLD = float(os.environ.get("SEARCH_EARLY_STOP", "0.75"))

log.info(
    "Initialized: tier=%s, semaphore=%d, max_urls=%d, quality=%s, early_stop=%.0f%%",
    _RAM_TIER.value,
    _CONFIG.semaphore_limit,
    _CONFIG.max_urls,
    _CONFIG.quality.value,
    EARLY_STOP_THRESHOLD * 100,
)


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 2: URL CLEANING - Tracking removal, normalization, deduplication
# ═══════════════════════════════════════════════════════════════════════════════

TRACKING_PARAMS: frozenset[str] = frozenset({
    # Analytics
    "utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content", "utm_id",
    # Social
    "fbclid", "fb_action_ids", "fb_action_types", "fb_source", "fb_ref",
    "gclid", "gclsrc", "dclid", "msclkid", "twclid",
    # General
    "ref", "source", "src", "campaign", "affiliate", "partner",
    "amp", "amp_js_v", "usqp", "outputType",
    "_ga", "_gl", "mc_cid", "mc_eid", "mkt_tok",
    "oly_enc_id", "oly_anon_id", "_hsenc", "_hsmi", "hsCtaTracking",
    "spm", "share_from", "scm", "algo_pvid", "algo_exp_id",
})

SKIP_DOMAINS: frozenset[str] = frozenset({
    "facebook.com", "twitter.com", "x.com", "instagram.com", "tiktok.com",
    "youtube.com", "youtu.be", "linkedin.com", "pinterest.com",
    "play.google.com", "apps.apple.com",
    "drive.google.com", "docs.google.com",
})

SKIP_EXTENSIONS: frozenset[str] = frozenset({
    ".pdf", ".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg",
    ".mp4", ".mp3", ".avi", ".mov", ".wmv", ".flv",
    ".zip", ".rar", ".7z", ".tar", ".gz",
    ".exe", ".msi", ".dmg", ".apk",
})


def clean_url(url: str) -> str:
    """Clean and normalize a URL, removing tracking parameters."""
    if not url:
        return ""
    
    url = url.strip()
    if not url.startswith(("http://", "https://")):
        return url
    
    try:
        parsed = urlparse(url)
        
        # Normalize domain
        domain = parsed.netloc.lower()
        if domain.startswith("www."):
            domain = domain[4:]
        
        # Clean path
        path = (parsed.path or "/").rstrip("/") or "/"
        
        # Remove tracking params
        if parsed.query:
            params = parse_qs(parsed.query, keep_blank_values=False)
            cleaned = {k: v for k, v in params.items() if k.lower() not in TRACKING_PARAMS}
            query = urlencode(sorted(cleaned.items()), doseq=True)
        else:
            query = ""
        
        return urlunparse((parsed.scheme, domain, path, "", query, ""))
    except Exception:
        return url


def get_url_key(url: str) -> str:
    """Get deduplication key for URL (domain + path)."""
    try:
        parsed = urlparse(clean_url(url))
        return f"{parsed.netloc}{parsed.path}".lower().rstrip("/")
    except Exception:
        return url.lower()


def is_valid_url(url: str) -> bool:
    """Check if URL is valid and worth scraping."""
    if not url or not url.strip():
        return False
    
    url_lower = url.lower().strip()
    
    if not url_lower.startswith(("http://", "https://")):
        return False
    
    # Check skip domains
    for domain in SKIP_DOMAINS:
        if domain in url_lower:
            return False
    
    # Check skip extensions
    for ext in SKIP_EXTENSIONS:
        if url_lower.endswith(ext):
            return False
    
    return True


def deduplicate_urls(urls: list[str], max_count: int) -> list[str]:
    """Deduplicate URLs by normalized key, preserving order."""
    seen: set[str] = set()
    unique: list[str] = []
    
    for url in urls:
        if not is_valid_url(url):
            continue
        
        cleaned = clean_url(url)
        key = get_url_key(cleaned)
        
        if key not in seen:
            seen.add(key)
            unique.append(cleaned)
            if len(unique) >= max_count:
                break
    
    return unique


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 3: SEARXNG META-SEARCH - Multiple instance querying with rotation
# ═══════════════════════════════════════════════════════════════════════════════

SEARXNG_INSTANCES: tuple[str, ...] = (
    "https://search.sapti.me",
    "https://searxng.site",
    "https://search.ononoki.org",
    "https://searx.tiekoetter.com",
    "https://paulgo.io",
    "https://search.mdosch.de",
)

_UA = "Mozilla/5.0 (compatible; SwiftSearchBot/2.0)"
_HEADERS = {"User-Agent": _UA}


async def _query_instance(
    client: httpx.AsyncClient,
    instance_url: str,
    query: str,
) -> list[str]:
    """Query a single SearxNG instance."""
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
        return [r.get("url", "").strip() for r in data.get("results", []) if r.get("url")]
    except Exception as e:
        log.debug("Instance %s failed: %s", instance_url, e)
        return []


async def meta_search(query: str) -> list[str]:
    """Query multiple SearxNG instances and return deduplicated URLs."""
    async with httpx.AsyncClient(follow_redirects=True) as client:
        tasks = [_query_instance(client, inst, query) for inst in SEARXNG_INSTANCES]
        results = await asyncio.gather(*tasks, return_exceptions=True)
    
    all_urls: list[str] = []
    for batch in results:
        if isinstance(batch, list):
            all_urls.extend(batch)
    
    unique = deduplicate_urls(all_urls, _CONFIG.max_urls)
    log.info("Meta-search: %d unique URLs for query: %s", len(unique), query[:60])
    return unique


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 4: TEXT EXTRACTION - Multi-strategy with fallback chain
# ═══════════════════════════════════════════════════════════════════════════════

class ExtractionResult(NamedTuple):
    """Result from text extraction."""
    url: str
    text: str
    quality_score: float  # 0.0-1.0
    char_count: int


def _extract_text_sync(html: str, url: str, quality: ExtractionQuality) -> tuple[str, float]:
    """
    Synchronous text extraction with quality-based strategy.
    Returns (extracted_text, quality_score).
    """
    if not html:
        return "", 0.0
    
    # Truncate HTML to cap (prevents OOM in trafilatura)
    html = html[:_CONFIG.html_cap_bytes]
    
    text = None
    score = 0.0
    
    try:
        if quality == ExtractionQuality.HIGH:
            # Strategy 1: Full extraction with tables and comments
            text = trafilatura.extract(
                html,
                include_comments=True,
                include_tables=True,
                no_fallback=False,
                url=url,
            )
            if text and len(text.strip()) >= 100:
                score = 1.0
        
        if not text or len(text.strip()) < 100:
            # Strategy 2: Standard extraction with fallback
            text = trafilatura.extract(
                html,
                include_comments=False,
                include_tables=quality != ExtractionQuality.FAST,
                no_fallback=quality != ExtractionQuality.FAST,
                url=url,
            )
            if text and len(text.strip()) >= 50:
                score = 0.8 if len(text) >= 200 else 0.6
        
        if not text or len(text.strip()) < 50:
            # Strategy 3: Aggressive fallback (for FAST mode or failures)
            text = trafilatura.extract(
                html,
                include_comments=False,
                include_tables=False,
                no_fallback=True,
                url=url,
            )
            score = 0.4 if text else 0.0
        
        return (text or "").strip(), score
        
    except Exception as e:
        log.debug("Extraction failed for %s: %s", url[:50], e)
        return "", 0.0


_scrape_semaphore: Optional[asyncio.Semaphore] = None


def _get_semaphore() -> asyncio.Semaphore:
    """Get or create scraping semaphore."""
    global _scrape_semaphore
    if _scrape_semaphore is None:
        _scrape_semaphore = asyncio.Semaphore(_CONFIG.semaphore_limit)
    return _scrape_semaphore


async def _scrape_single_url(
    client: httpx.AsyncClient,
    url: str,
    stop_event: asyncio.Event,
) -> ExtractionResult:
    """Scrape and extract text from a single URL."""
    sem = _get_semaphore()
    
    async with sem:
        # Check if we should stop early
        if stop_event.is_set():
            return ExtractionResult(url, "", 0.0, 0)
        
        try:
            # Optional HEAD check for content-type (saves bandwidth)
            if _CONFIG.enable_head_check:
                try:
                    head_resp = await client.head(url, headers=_HEADERS, timeout=3.0)
                    ct = head_resp.headers.get("content-type", "").lower()
                    if "text/html" not in ct and "text/plain" not in ct:
                        return ExtractionResult(url, "", 0.0, 0)
                    cl = head_resp.headers.get("content-length", "")
                    if cl and int(cl) > 5_000_000:  # Skip >5MB
                        return ExtractionResult(url, "", 0.0, 0)
                except Exception:
                    pass  # Continue to GET if HEAD fails
            
            # Stream HTML with size cap
            html_chunks: list[bytes] = []
            total_size = 0
            
            async with client.stream(
                "GET",
                url,
                headers=_HEADERS,
                timeout=_CONFIG.scrape_timeout_sec,
                follow_redirects=True,
            ) as resp:
                if resp.status_code != 200:
                    return ExtractionResult(url, "", 0.0, 0)
                
                ct = resp.headers.get("content-type", "").lower()
                if "text/html" not in ct and "text/plain" not in ct:
                    return ExtractionResult(url, "", 0.0, 0)
                
                async for chunk in resp.aiter_bytes(chunk_size=65536):
                    html_chunks.append(chunk)
                    total_size += len(chunk)
                    if total_size >= _CONFIG.html_cap_bytes:
                        break
            
            html = b"".join(html_chunks).decode("utf-8", errors="ignore")
            del html_chunks  # Free memory immediately
            
            # Extract text in thread pool
            text, score = await asyncio.to_thread(
                _extract_text_sync, html, url, _CONFIG.quality
            )
            
            return ExtractionResult(url, text, score, len(text))
            
        except Exception as e:
            log.debug("Scrape failed for %s: %s", url[:50], e)
            return ExtractionResult(url, "", 0.0, 0)


async def scrape_urls(urls: list[str]) -> list[ExtractionResult]:
    """
    Scrape all URLs with early termination when enough content collected.
    """
    results: list[ExtractionResult] = []
    stop_event = asyncio.Event()
    char_collected = 0
    threshold = int(_CONFIG.max_context_chars * EARLY_STOP_THRESHOLD)
    
    async with httpx.AsyncClient(
        follow_redirects=True,
        limits=httpx.Limits(
            max_connections=_CONFIG.semaphore_limit,
            max_keepalive_connections=5,
        ),
    ) as client:
        tasks = [_scrape_single_url(client, url, stop_event) for url in urls]
        
        # Process results as they complete
        for coro in asyncio.as_completed(tasks):
            try:
                result = await coro
                results.append(result)
                
                if result.char_count > 0:
                    char_collected += result.char_count
                    
                    # Check early termination
                    if char_collected >= threshold and not stop_event.is_set():
                        log.info("Early stop: collected %d chars (threshold: %d)", char_collected, threshold)
                        stop_event.set()
                        
            except Exception:
                pass
    
    # Sort by quality score (best first)
    results.sort(key=lambda r: r.quality_score, reverse=True)
    
    return results


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 5: CONTEXT BUILDING - Efficient string operations with StringIO
# ═══════════════════════════════════════════════════════════════════════════════

def build_context(results: list[ExtractionResult]) -> tuple[str, list[str]]:
    """
    Build context string from extraction results.
    Uses StringIO for efficient concatenation.
    Returns (context, citations).
    """
    buffer = StringIO()
    citations: list[str] = []
    content_hashes: set[str] = set()  # For content deduplication
    char_count = 0
    source_idx = 0
    
    for result in results:
        if not result.text or len(result.text.strip()) < 50:
            continue
        
        # Content hash for dedup (first 1000 chars)
        content_sample = result.text[:1000].strip().lower()
        content_hash = hashlib.md5(content_sample.encode(), usedforsecurity=False).hexdigest()[:16]
        
        if content_hash in content_hashes:
            log.debug("Skipping duplicate content from %s", result.url[:50])
            continue
        
        content_hashes.add(content_hash)
        source_idx += 1
        
        snippet = result.text.strip()
        marker = f"\n\n--- Source [{source_idx}]: {result.url} ---\n{snippet}"
        
        if char_count + len(marker) > _CONFIG.max_context_chars:
            remaining = _CONFIG.max_context_chars - char_count
            if remaining > 200:
                buffer.write(marker[:remaining])
                citations.append(result.url)
            break
        
        buffer.write(marker)
        citations.append(result.url)
        char_count += len(marker)
    
    context = buffer.getvalue()
    buffer.close()
    
    return context, citations


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 6: LLM SYNTHESIS - Cerebras/OpenAI-compatible API
# ═══════════════════════════════════════════════════════════════════════════════

CEREBRAS_API_URL = os.environ.get("LLM_API_URL", "https://api.cerebras.ai/v1/chat/completions")
CEREBRAS_MODEL = os.environ.get("LLM_MODEL", "llama-3.3-70b")


def _build_system_prompt() -> str:
    return (
        "You are an advanced research assistant. "
        "Using ONLY the provided source context below, write a comprehensive, "
        "highly detailed, and well-structured answer to the user's query. "
        "Include inline citations in the format [Source N](url) where possible. "
        "If the context is insufficient, state what is known and what could not be verified. "
        "Do NOT fabricate information beyond what the sources provide."
    )


async def synthesize(
    query: str,
    context: str,
    citations: list[str],
    api_key: str,
) -> str:
    """Call LLM API for synthesis."""
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
            return answer or "The LLM returned an empty response. Please try again."
            
        except httpx.HTTPStatusError as e:
            status = e.response.status_code
            if status == 401:
                raise HTTPException(status_code=401, detail="Invalid API key.")
            if status == 429:
                raise HTTPException(status_code=429, detail="Rate limit hit. Retry later.")
            raise HTTPException(status_code=502, detail=f"LLM upstream error ({status}).")
        except Exception as exc:
            log.error("LLM call failed: %s", exc)
            raise HTTPException(status_code=502, detail="Failed to reach LLM API.")


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 7: FASTAPI APPLICATION - Endpoints and middleware
# ═══════════════════════════════════════════════════════════════════════════════

app = FastAPI(
    title="Swift Search API v2.0",
    description="Advanced meta-search + scraping + LLM synthesis, optimized for low-RAM VPS",
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


class SearchRequest(BaseModel):
    query: str = Field(..., min_length=1, max_length=1000, description="Search query")


class SearchResponse(BaseModel):
    query: str
    sources_found: int
    sources_scraped: int
    answer: str
    citations: list[str]
    elapsed_seconds: float
    ram_tier: str
    quality: str


@app.get("/health")
async def health():
    """Health check endpoint."""
    return {
        "status": "ok",
        "ram_tier": _RAM_TIER.value,
        "quality": _CONFIG.quality.value,
        "semaphore_limit": _CONFIG.semaphore_limit,
    }


@app.get("/config")
async def get_config():
    """Get current configuration (for debugging)."""
    return {
        "ram_tier": _RAM_TIER.value,
        "semaphore_limit": _CONFIG.semaphore_limit,
        "max_urls": _CONFIG.max_urls,
        "html_cap_bytes": _CONFIG.html_cap_bytes,
        "max_context_chars": _CONFIG.max_context_chars,
        "scrape_timeout_sec": _CONFIG.scrape_timeout_sec,
        "enable_head_check": _CONFIG.enable_head_check,
        "quality": _CONFIG.quality.value,
        "early_stop_threshold": EARLY_STOP_THRESHOLD,
    }


@app.post("/search", response_model=SearchResponse)
async def search(body: SearchRequest, x_api_key: str = Header(..., alias="x-api-key")):
    """
    Main search endpoint - orchestrates the full pipeline:
    1. Meta-search via SearxNG (multiple instances)
    2. Concurrent scraping with streaming + early termination
    3. Context building with content deduplication
    4. LLM synthesis via Cerebras/compatible API
    """
    t0 = time.perf_counter()
    query = body.query.strip()
    log.info("━━━ NEW SEARCH ━━━ query=%s", query[:80])
    
    # Phase 1: Meta-Search
    urls = await meta_search(query)
    if not urls:
        raise HTTPException(
            status_code=404,
            detail="No search results found. All SearxNG instances may be down.",
        )
    sources_found = len(urls)
    
    # Phase 2: Concurrent Scraping (with early termination)
    results = await scrape_urls(urls)
    sources_scraped = sum(1 for r in results if r.char_count >= 50)
    log.info("Scraped %d / %d URLs", sources_scraped, sources_found)
    
    # Phase 3: Build Context
    context, citations = build_context(results)
    
    # Phase 4: Synthesize
    answer = await synthesize(query, context, citations, x_api_key)
    
    # Single GC at end (not per-operation)
    del urls, results, context
    gc.collect()
    
    elapsed = round(time.perf_counter() - t0, 2)
    log.info("━━━ DONE ━━━ elapsed=%.2fs sources=%d/%d", elapsed, sources_scraped, sources_found)
    
    return SearchResponse(
        query=query,
        sources_found=sources_found,
        sources_scraped=sources_scraped,
        answer=answer,
        citations=citations,
        elapsed_seconds=elapsed,
        ram_tier=_RAM_TIER.value,
        quality=_CONFIG.quality.value,
    )


@app.exception_handler(Exception)
async def global_exception_handler(request: Request, exc: Exception):
    log.exception("Unhandled error: %s", exc)
    gc.collect()
    return JSONResponse(
        status_code=500,
        content={"detail": "Internal server error. Please try again."},
    )


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 8: ENTRYPOINT
# ═══════════════════════════════════════════════════════════════════════════════

if __name__ == "__main__":
    import uvicorn
    
    port = int(os.environ.get("PORT", 8000))
    log.info("Starting Swift Search API v2.0 on port %d", port)
    log.info("Config: tier=%s, quality=%s, semaphore=%d", 
             _RAM_TIER.value, _CONFIG.quality.value, _CONFIG.semaphore_limit)
    
    uvicorn.run(
        "search_unified:app",
        host="0.0.0.0",
        port=port,
        workers=1,
        log_level="info",
        access_log=False,
    )
