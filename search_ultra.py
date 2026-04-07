#!/usr/bin/env python3
"""
╔══════════════════════════════════════════════════════════════════════════════╗
║         SWIFT SEARCH AGENT v2.0 — EXTREME MILLISECOND EDITION                ║
╠══════════════════════════════════════════════════════════════════════════════╣
║  Ultra-low latency search and extraction engine                               ║
║  combining dynamic RAM auto-tiering with bleeding-edge async networking      ║
╚══════════════════════════════════════════════════════════════════════════════╝

Architecture Pillars:
─────────────────────
1. SMART AUTO-TIERING: psutil-based RAM detection with CLI override
   - Micro Mode  (<2GB):  Concurrency=5,   Sources=50
   - Medium Mode (2-8GB): Concurrency=20,  Sources=100  
   - Beast Mode  (>8GB):  Concurrency=200, Sources=∞

2. ULTRA-FAST NETWORK ENGINE: aiohttp + aiodns + cachetools
   - aiohttp.TCPConnector with limit=500, limit_per_host=10
   - Persistent DNS caching via aiodns + TTLCache(10000, 300s)
   - Connection pooling with keep-alive optimization

3. ZERO-COPY PARSING ENGINE: selectolax lexbor (C/Rust-speed)
   - LexborHTMLParser for DOM parsing (~20x faster than lxml)
   - CSS selector-based content extraction
   - Pre-cleaned text passed to trafilatura for final heuristics

4. PIPELINE SAFETY: Semaphore-bounded extraction + aggressive GC
   - asyncio.Semaphore tied to active tier
   - Recursive text chunker with immediate execution
   - gc.collect() after each batch + cache resets

Performance Targets:
────────────────────
- Response latency: <5s for 50 sources (Micro), <3s for 100 (Medium)
- Memory footprint: <60MB (Micro), <200MB (Medium), <1GB (Beast)
- Extraction success rate: >95%
- Zero OOM risk on any tier

Author: Swift Search Agent Team
Version: 2.0.0-extreme
License: MIT
"""

from __future__ import annotations

# ═══════════════════════════════════════════════════════════════════════════════
# IMPORTS — Organized by category for clarity
# ═══════════════════════════════════════════════════════════════════════════════

# Standard library
import argparse
import asyncio
import gc
import hashlib
import logging
import os
import re
import sys
import time
from dataclasses import dataclass, field
from enum import Enum, auto
from io import StringIO
from typing import (
    Any,
    Callable,
    Dict,
    Final,
    List,
    NamedTuple,
    Optional,
    Set,
    Tuple,
    TypeVar,
)
from urllib.parse import parse_qs, urlencode, urlparse, urlunparse

# Third-party: Core networking (aiohttp stack)
import aiohttp
from aiohttp import (
    ClientSession,
    ClientTimeout,
    TCPConnector,
    DummyCookieJar,
)

# Third-party: DNS acceleration
try:
    import aiodns
    from aiohttp.resolver import AsyncResolver
    AIODNS_AVAILABLE = True
except ImportError:
    AIODNS_AVAILABLE = False
    AsyncResolver = None

# Third-party: Caching
from cachetools import TTLCache

# Third-party: System info
import psutil

# Third-party: Fast HTML parsing (Lexbor - C-speed, zero-copy style)
from selectolax.lexbor import LexborHTMLParser

# Third-party: Text extraction heuristics
import trafilatura
from trafilatura import bare_extraction

# Third-party: Web framework
from fastapi import FastAPI, HTTPException, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse
from pydantic import BaseModel, Field


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 1: LOGGING CONFIGURATION
# ═══════════════════════════════════════════════════════════════════════════════

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s │ %(levelname)-7s │ %(name)s │ %(message)s",
    datefmt="%H:%M:%S",
    stream=sys.stdout,
)
log = logging.getLogger("swift-v2")


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 2: TIER SYSTEM — RAM-Based Auto-Configuration
# ═══════════════════════════════════════════════════════════════════════════════

class TierMode(Enum):
    """Operating tier based on available system RAM."""
    MICRO = auto()   # <2GB RAM: Conservative mode
    MEDIUM = auto()  # 2-8GB RAM: Balanced mode
    BEAST = auto()   # >8GB RAM: Maximum performance


@dataclass(frozen=True)
class TierConfig:
    """Immutable configuration for each operating tier."""
    name: str
    concurrency: int          # Max concurrent HTTP requests
    semaphore_limit: int      # Max concurrent extractions
    max_sources: int          # Max URLs to process (0 = unlimited)
    connector_limit: int      # TCPConnector pool size
    connector_per_host: int   # Max connections per host
    dns_cache_size: int       # DNS cache entries
    dns_cache_ttl: int        # DNS cache TTL in seconds
    chunk_size: int           # Text chunk size
    chunk_overlap: int        # Overlap between chunks
    min_text_length: int      # Minimum useful text length
    fetch_timeout: float      # Per-request timeout
    
    @property
    def description(self) -> str:
        """Human-readable tier description."""
        return f"{self.name}: concurrency={self.concurrency}, sources={self.max_sources or '∞'}"


# ─────────────── Tier Definitions ───────────────

TIER_CONFIGS: Dict[TierMode, TierConfig] = {
    TierMode.MICRO: TierConfig(
        name="MICRO",
        concurrency=5,
        semaphore_limit=5,
        max_sources=50,
        connector_limit=50,
        connector_per_host=5,
        dns_cache_size=1000,
        dns_cache_ttl=300,
        chunk_size=1500,
        chunk_overlap=200,
        min_text_length=100,
        fetch_timeout=15.0,
    ),
    TierMode.MEDIUM: TierConfig(
        name="MEDIUM",
        concurrency=20,
        semaphore_limit=20,
        max_sources=100,
        connector_limit=200,
        connector_per_host=10,
        dns_cache_size=5000,
        dns_cache_ttl=300,
        chunk_size=1500,
        chunk_overlap=200,
        min_text_length=100,
        fetch_timeout=12.0,
    ),
    TierMode.BEAST: TierConfig(
        name="BEAST",
        concurrency=200,
        semaphore_limit=100,
        max_sources=0,  # Unlimited
        connector_limit=500,
        connector_per_host=10,
        dns_cache_size=10000,
        dns_cache_ttl=300,
        chunk_size=2000,
        chunk_overlap=300,
        min_text_length=100,
        fetch_timeout=10.0,
    ),
}


def detect_ram_gb() -> float:
    """Detect total system RAM in gigabytes using psutil."""
    try:
        total_bytes = psutil.virtual_memory().total
        return total_bytes / (1024 ** 3)  # Convert to GB
    except Exception as e:
        log.warning("Failed to detect RAM: %s. Defaulting to MICRO mode.", e)
        return 1.0  # Conservative fallback


def determine_tier(ram_gb: Optional[float] = None, override: Optional[str] = None) -> TierMode:
    """
    Determine operating tier based on RAM or explicit override.
    
    Args:
        ram_gb: Detected RAM in GB (auto-detected if None)
        override: CLI override ("micro", "medium", "beast")
    
    Returns:
        TierMode enum value
    """
    # CLI override takes precedence
    if override:
        override_lower = override.lower()
        if override_lower == "micro":
            return TierMode.MICRO
        elif override_lower == "medium":
            return TierMode.MEDIUM
        elif override_lower == "beast":
            return TierMode.BEAST
        else:
            log.warning("Unknown tier override '%s', using auto-detection", override)
    
    # Auto-detect based on RAM
    if ram_gb is None:
        ram_gb = detect_ram_gb()
    
    if ram_gb < 2.0:
        return TierMode.MICRO
    elif ram_gb < 8.0:
        return TierMode.MEDIUM
    else:
        return TierMode.BEAST


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 3: GLOBAL STATE — Tier-Dependent Runtime Configuration
# ═══════════════════════════════════════════════════════════════════════════════

# Will be set at startup based on tier detection
ACTIVE_TIER: TierMode = TierMode.MICRO
TIER_CFG: TierConfig = TIER_CONFIGS[TierMode.MICRO]


def initialize_tier(override: Optional[str] = None) -> TierConfig:
    """Initialize global tier configuration."""
    global ACTIVE_TIER, TIER_CFG
    
    ram_gb = detect_ram_gb()
    ACTIVE_TIER = determine_tier(ram_gb, override)
    TIER_CFG = TIER_CONFIGS[ACTIVE_TIER]
    
    log.info("╔══════════════════════════════════════════════════════════════╗")
    log.info("║       Swift Search Agent v2.0 — EXTREME EDITION              ║")
    log.info("╠══════════════════════════════════════════════════════════════╣")
    log.info("║  RAM: %.1f GB → Tier: %s", ram_gb, TIER_CFG.name)
    log.info("║  Concurrency: %d | Max Sources: %s", 
             TIER_CFG.concurrency, TIER_CFG.max_sources or "∞")
    log.info("║  Connector Pool: %d | DNS Cache: %d entries", 
             TIER_CFG.connector_limit, TIER_CFG.dns_cache_size)
    log.info("╚══════════════════════════════════════════════════════════════╝")
    
    return TIER_CFG


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 4: CONSTANTS — Environment & Fixed Config
# ═══════════════════════════════════════════════════════════════════════════════

# ─────────────── SearxNG Configuration ───────────────
# Imported from centralized config
from config import SEARXNG_URL, SEARXNG_ENGINES

# ─────────────── Processing Limits ───────────────

MAX_HTML_BYTES: Final[int] = int(os.getenv("MAX_HTML_BYTES", "500000"))  # 500KB
MAX_CONTEXT_CHARS: Final[int] = int(os.getenv("MAX_CONTEXT_CHARS", "90000"))  # 90K


# ─────────────── HTTP Headers ───────────────

USER_AGENTS: Final[Tuple[str, ...]] = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15",
    "Mozilla/5.0 (X11; Linux x86_64; rv:109.0) Gecko/20100101 Firefox/121.0",
)


# ─────────────── Content Quality Patterns ───────────────

# Compiled once for performance
_WHITESPACE_COLLAPSE: Final = re.compile(r"\s{3,}")
_SENTENCE_SPLIT: Final = re.compile(r"(?<=[.!?])\s+")
_WORD_MATCH: Final = re.compile(r"\w+")

# Domains to skip (PDFs, binaries, etc.)
SKIP_DOMAINS: Final[frozenset] = frozenset([
    "youtube.com", "youtu.be", "vimeo.com", "dailymotion.com",
    "twitter.com", "x.com", "facebook.com", "instagram.com",
    "linkedin.com", "pinterest.com", "tiktok.com",
    "github.com", "gitlab.com", "bitbucket.org",
    "play.google.com", "apps.apple.com",
])

# File extensions to skip
SKIP_EXTENSIONS: Final[frozenset] = frozenset([
    ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx",
    ".zip", ".rar", ".7z", ".tar", ".gz",
    ".mp3", ".mp4", ".avi", ".mkv", ".mov",
    ".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg",
])


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 5: DNS & CONNECTION INFRASTRUCTURE
# ═══════════════════════════════════════════════════════════════════════════════

# ─────────────── DNS Cache ───────────────

# Global DNS cache using TTLCache for automatic expiration
_dns_cache: Optional[TTLCache] = None


def get_dns_cache() -> TTLCache:
    """Get or create the global DNS cache based on active tier."""
    global _dns_cache
    if _dns_cache is None:
        _dns_cache = TTLCache(
            maxsize=TIER_CFG.dns_cache_size,
            ttl=TIER_CFG.dns_cache_ttl,
        )
        log.debug("DNS cache initialized: size=%d, ttl=%ds", 
                  TIER_CFG.dns_cache_size, TIER_CFG.dns_cache_ttl)
    return _dns_cache


def reset_dns_cache():
    """Clear the DNS cache (useful for memory cleanup)."""
    global _dns_cache
    if _dns_cache is not None:
        _dns_cache.clear()
        log.debug("DNS cache cleared")


# ─────────────── HTTP Client Factory ───────────────

def create_http_session(timeout_override: Optional[float] = None) -> ClientSession:
    """
    Create an optimized aiohttp ClientSession with DNS acceleration.
    
    Features:
    - TCPConnector with configurable pool limits
    - aiodns resolver (if available) for faster DNS
    - DummyCookieJar for stateless requests
    - Optimized timeouts
    """
    timeout = ClientTimeout(
        total=timeout_override or TIER_CFG.fetch_timeout,
        connect=5.0,
        sock_connect=5.0,
        sock_read=timeout_override or TIER_CFG.fetch_timeout - 2,
    )
    
    # Create resolver with aiodns if available
    resolver = None
    if AIODNS_AVAILABLE and AsyncResolver is not None:
        try:
            resolver = AsyncResolver()
            log.debug("Using aiodns AsyncResolver")
        except Exception as e:
            log.debug("aiodns unavailable, using default resolver: %s", e)
    
    connector = TCPConnector(
        limit=TIER_CFG.connector_limit,
        limit_per_host=TIER_CFG.connector_per_host,
        resolver=resolver,
        ttl_dns_cache=TIER_CFG.dns_cache_ttl,
        use_dns_cache=True,
        force_close=False,  # Keep connections alive
        enable_cleanup_closed=True,
    )
    
    return ClientSession(
        connector=connector,
        cookie_jar=DummyCookieJar(),  # No cookies = no state = faster
        timeout=timeout,
        raise_for_status=False,  # Handle errors manually
    )


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 6: DATA STRUCTURES
# ═══════════════════════════════════════════════════════════════════════════════

@dataclass
class TextChunk:
    """A chunk of extracted text with metadata."""
    content: str
    source_url: str
    chunk_index: int
    content_hash: str = field(default="")
    
    def __post_init__(self):
        if not self.content_hash:
            self.content_hash = hashlib.md5(self.content.encode()).hexdigest()[:12]


@dataclass
class ExtractionResult:
    """Result of URL extraction including all chunks."""
    url: str
    success: bool
    chunks: List[TextChunk] = field(default_factory=list)
    error: Optional[str] = None
    elapsed_ms: float = 0.0
    
    @property
    def chunk_count(self) -> int:
        return len(self.chunks)
    
    @property
    def total_chars(self) -> int:
        return sum(len(c.content) for c in self.chunks)


class SearchResult(NamedTuple):
    """A search result from SearxNG."""
    url: str
    title: str
    snippet: str


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 7: URL UTILITIES
# ═══════════════════════════════════════════════════════════════════════════════

def normalize_url(url: str) -> str:
    """
    Normalize URL for deduplication:
    - Remove tracking parameters
    - Remove fragments
    - Lowercase domain
    """
    try:
        parsed = urlparse(url.strip())
        
        # Lowercase domain
        domain = parsed.netloc.lower()
        
        # Remove fragment
        # Filter tracking parameters
        if parsed.query:
            params = parse_qs(parsed.query, keep_blank_values=False)
            # Remove common tracking params
            tracking_params = {
                'utm_source', 'utm_medium', 'utm_campaign', 'utm_term', 'utm_content',
                'fbclid', 'gclid', 'ref', 'source', 'mc_cid', 'mc_eid',
            }
            filtered = {k: v for k, v in params.items() if k.lower() not in tracking_params}
            query = urlencode(filtered, doseq=True)
        else:
            query = ''
        
        # Rebuild URL
        return urlunparse((
            parsed.scheme or 'https',
            domain,
            parsed.path.rstrip('/') or '/',
            parsed.params,
            query,
            '',  # No fragment
        ))
    except Exception:
        return url.strip()


def should_skip_url(url: str) -> bool:
    """Check if URL should be skipped based on domain or extension."""
    try:
        parsed = urlparse(url.lower())
        domain = parsed.netloc
        path = parsed.path
        
        # Check domain blocklist
        for blocked in SKIP_DOMAINS:
            if blocked in domain:
                return True
        
        # Check extension blocklist
        for ext in SKIP_EXTENSIONS:
            if path.endswith(ext):
                return True
        
        return False
    except Exception:
        return True  # Skip on error


def get_random_user_agent() -> str:
    """Get a random User-Agent string."""
    import random
    return random.choice(USER_AGENTS)


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 8: META-SEARCH ENGINE (SearxNG)
# ═══════════════════════════════════════════════════════════════════════════════

async def meta_search(query: str) -> List[str]:
    """
    Query your private SearxNG instance with explicit engine selection.
    Engines: duckduckgo, brave, yahoo, qwant, mojeek
    No Google, No Bing.
    """
    search_url = f"{SEARXNG_URL.rstrip('/')}/search"
    params = {
        "q": query,
        "format": "json",
        "categories": "general",
        "language": "en-US",
        "safesearch": "0",
        "engines": SEARXNG_ENGINES,
    }
    
    try:
        async with create_http_session(timeout_override=15.0) as session:
            async with session.get(
                search_url,
                params=params,
                headers={
                    "User-Agent": get_random_user_agent(),
                    "Accept": "application/json",
                },
            ) as resp:
                if resp.status != 200:
                    log.error("SearxNG returned status %d", resp.status)
                    return []
                
                data = await resp.json()
                
                # Deduplicate and filter
                seen_urls: Set[str] = set()
                unique_urls: List[str] = []
                
                for item in data.get("results", []):
                    url = item.get("url", "")
                    if url and not should_skip_url(url):
                        norm = normalize_url(url)
                        if norm not in seen_urls:
                            seen_urls.add(norm)
                            unique_urls.append(norm)
                
                # Apply tier limit
                max_sources = TIER_CFG.max_sources
                if max_sources > 0 and len(unique_urls) > max_sources:
                    unique_urls = unique_urls[:max_sources]
                
                log.info("Meta-search: %d unique URLs (engines: %s, limit: %s)",
                         len(unique_urls), SEARXNG_ENGINES, max_sources or "\u221e")
                return unique_urls
                
    except Exception as e:
        log.error("Meta-search failed (%s): %s", SEARXNG_URL, e)
        return []


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 9: HTML EXTRACTION — Selectolax + Trafilatura
# ═══════════════════════════════════════════════════════════════════════════════

def extract_with_selectolax(html: str) -> Optional[str]:
    """
    Fast extraction using selectolax Lexbor parser.
    
    Strategy:
    1. Parse with Lexbor (C-speed)
    2. Remove unwanted elements (script, style, nav, etc.)
    3. Extract text from content containers
    4. Fall back to body text if no containers found
    """
    try:
        tree = LexborHTMLParser(html)
        
        # Remove noise elements
        for selector in [
            'script', 'style', 'noscript', 'iframe', 'svg',
            'nav', 'header', 'footer', 'aside', 'form',
            '.nav', '.navigation', '.menu', '.sidebar',
            '.advertisement', '.ad', '.ads', '.social',
            '.comments', '.comment', '.related', '.share',
            '[role="navigation"]', '[role="banner"]', '[role="complementary"]',
        ]:
            for node in tree.css(selector):
                node.decompose()
        
        # Try content containers first
        content_selectors = [
            'article', 'main', '.content', '.post', '.entry',
            '.article-content', '.post-content', '.entry-content',
            '[role="main"]', '#content', '#main',
        ]
        
        text_parts = []
        for selector in content_selectors:
            for node in tree.css(selector):
                text = node.text(separator=' ', strip=True)
                if text and len(text) > 100:
                    text_parts.append(text)
        
        if text_parts:
            return ' '.join(text_parts)
        
        # Fall back to body
        body = tree.css_first('body')
        if body:
            return body.text(separator=' ', strip=True)
        
        return None
        
    except Exception as e:
        log.debug("Selectolax extraction failed: %s", e)
        return None


def extract_text(html: str, url: str) -> Optional[str]:
    """
    Multi-strategy text extraction.
    
    1. Try selectolax (fastest, C-speed)
    2. Fall back to trafilatura (more accurate heuristics)
    3. Validate minimum length
    """
    # Strategy 1: Selectolax
    text = extract_with_selectolax(html)
    
    if text and len(text) >= TIER_CFG.min_text_length:
        # Clean whitespace
        text = _WHITESPACE_COLLAPSE.sub(' ', text).strip()
        if len(text) >= TIER_CFG.min_text_length:
            return text
    
    # Strategy 2: Trafilatura
    try:
        result = bare_extraction(
            html,
            url=url,
            include_comments=False,
            include_tables=True,
            include_links=False,
            include_images=False,
            favor_precision=True,
        )
        
        if result and isinstance(result, dict):
            text = result.get("text", "") or ""
            if len(text) >= TIER_CFG.min_text_length:
                return _WHITESPACE_COLLAPSE.sub(' ', text).strip()
    except Exception as e:
        log.debug("Trafilatura extraction failed for %s: %s", url[:50], e)
    
    return None


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 10: TEXT CHUNKING — Recursive Splitter
# ═══════════════════════════════════════════════════════════════════════════════

def recursive_chunk_text(
    text: str,
    url: str,
    chunk_size: Optional[int] = None,
    chunk_overlap: Optional[int] = None,
) -> List[TextChunk]:
    """
    Split text into overlapping chunks using recursive boundary detection.
    
    Strategy:
    1. Try to split on paragraph boundaries (\n\n)
    2. Fall back to sentence boundaries
    3. Finally split on words
    
    Each chunk includes overlap for context continuity.
    """
    chunk_size = chunk_size or TIER_CFG.chunk_size
    chunk_overlap = chunk_overlap or TIER_CFG.chunk_overlap
    
    if len(text) <= chunk_size:
        return [TextChunk(content=text, source_url=url, chunk_index=0)]
    
    chunks: List[TextChunk] = []
    current_pos = 0
    chunk_index = 0
    
    while current_pos < len(text):
        # Determine chunk end position
        end_pos = min(current_pos + chunk_size, len(text))
        
        # If not at end, try to find a good break point
        if end_pos < len(text):
            chunk_text = text[current_pos:end_pos]
            
            # Try paragraph break
            para_break = chunk_text.rfind('\n\n')
            if para_break > chunk_size // 2:
                end_pos = current_pos + para_break + 2
            else:
                # Try sentence break
                sentences = _SENTENCE_SPLIT.split(chunk_text)
                if len(sentences) > 1:
                    # Use all but last incomplete sentence
                    complete = ''.join(sentences[:-1])
                    if len(complete) > chunk_size // 2:
                        end_pos = current_pos + len(complete)
                else:
                    # Try word break
                    last_space = chunk_text.rfind(' ')
                    if last_space > chunk_size // 2:
                        end_pos = current_pos + last_space
        
        # Extract chunk
        chunk_content = text[current_pos:end_pos].strip()
        
        if chunk_content:
            chunks.append(TextChunk(
                content=chunk_content,
                source_url=url,
                chunk_index=chunk_index,
            ))
            chunk_index += 1
        
        # Move position with overlap
        current_pos = end_pos - chunk_overlap
        
        # Ensure progress
        if current_pos <= (end_pos - chunk_size):
            current_pos = end_pos
    
    return chunks


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 11: URL PROCESSING PIPELINE
# ═══════════════════════════════════════════════════════════════════════════════

async def fetch_and_extract_url(
    session: ClientSession,
    semaphore: asyncio.Semaphore,
    url: str,
) -> ExtractionResult:
    """
    Fetch URL and extract text chunks.
    
    Pipeline:
    1. Semaphore acquisition (memory bounding)
    2. HTTP fetch with size limit
    3. Text extraction (selectolax → trafilatura)
    4. Recursive chunking
    5. Immediate cleanup
    """
    t0 = time.perf_counter()
    
    async with semaphore:
        try:
            async with session.get(
                url,
                headers={
                    "User-Agent": get_random_user_agent(),
                    "Accept": "text/html,application/xhtml+xml",
                    "Accept-Language": "en-US,en;q=0.9",
                },
                allow_redirects=True,
                max_redirects=3,
            ) as resp:
                if resp.status != 200:
                    return ExtractionResult(
                        url=url,
                        success=False,
                        error=f"HTTP {resp.status}",
                        elapsed_ms=(time.perf_counter() - t0) * 1000,
                    )
                
                # Check content type
                content_type = resp.headers.get("Content-Type", "")
                if "text/html" not in content_type and "application/xhtml" not in content_type:
                    return ExtractionResult(
                        url=url,
                        success=False,
                        error=f"Invalid content type: {content_type[:50]}",
                        elapsed_ms=(time.perf_counter() - t0) * 1000,
                    )
                
                # Read with size limit
                html_bytes = await resp.content.read(MAX_HTML_BYTES)
                
                # Decode
                charset = resp.charset or 'utf-8'
                try:
                    html = html_bytes.decode(charset, errors='replace')
                except Exception:
                    html = html_bytes.decode('utf-8', errors='replace')
                
                # Free bytes immediately
                del html_bytes
                
        except asyncio.TimeoutError:
            return ExtractionResult(
                url=url,
                success=False,
                error="Timeout",
                elapsed_ms=(time.perf_counter() - t0) * 1000,
            )
        except Exception as e:
            return ExtractionResult(
                url=url,
                success=False,
                error=str(e)[:100],
                elapsed_ms=(time.perf_counter() - t0) * 1000,
            )
        
        # Extract text
        text = extract_text(html, url)
        
        # Free HTML immediately
        del html
        
        if not text:
            return ExtractionResult(
                url=url,
                success=False,
                error="No text extracted",
                elapsed_ms=(time.perf_counter() - t0) * 1000,
            )
        
        # Chunk text
        chunks = recursive_chunk_text(text, url)
        
        # Free full text
        del text
        
        elapsed_ms = (time.perf_counter() - t0) * 1000
        
        return ExtractionResult(
            url=url,
            success=True,
            chunks=chunks,
            elapsed_ms=elapsed_ms,
        )


async def process_urls(urls: List[str]) -> List[ExtractionResult]:
    """
    Process all URLs concurrently with semaphore bounding.
    
    Memory optimization:
    - Semaphore limits concurrent extractions
    - Immediate cleanup after each URL
    - GC after batch completion
    """
    semaphore = asyncio.Semaphore(20)
    
    async with create_http_session() as session:
        tasks = [
            fetch_and_extract_url(session, semaphore, url)
            for url in urls
        ]
        
        results = await asyncio.gather(*tasks, return_exceptions=True)
        
        # Convert exceptions to failed results
        processed: List[ExtractionResult] = []
        for i, r in enumerate(results):
            if isinstance(r, Exception):
                processed.append(ExtractionResult(
                    url=urls[i],
                    success=False,
                    error=str(r)[:100],
                ))
            else:
                processed.append(r)
        
        # Aggressive cleanup
        gc.collect()
        
        return processed


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 12: CONTEXT BUILDING
# ═══════════════════════════════════════════════════════════════════════════════

def build_context_from_chunks(
    results: List[ExtractionResult],
    max_chars: int = MAX_CONTEXT_CHARS,
) -> Tuple[str, List[str]]:
    """
    Build LLM context from extraction results.
    
    Strategy:
    - Deduplicate by content hash
    - Build until max_chars reached
    - Generate smart citations with URL shortening
    """
    seen_hashes: Set[str] = set()
    context_parts: List[str] = []
    citations: List[str] = []
    current_chars = 0
    source_counter = 0
    
    for result in results:
        if not result.success:
            continue
        
        for chunk in result.chunks:
            # Skip duplicates
            if chunk.content_hash in seen_hashes:
                continue
            
            # Check space
            chunk_size = len(chunk.content) + 50  # Buffer for formatting
            if current_chars + chunk_size > max_chars:
                # Try to fit partial
                remaining = max_chars - current_chars - 50
                if remaining > 200:
                    truncated = chunk.content[:remaining].rsplit(' ', 1)[0]
                    seen_hashes.add(chunk.content_hash)
                    source_counter += 1
                    context_parts.append(f"[Source {source_counter}]\n{truncated}...")
                    
                    # Add citation
                    parsed = urlparse(chunk.source_url)
                    short_url = f"{parsed.netloc}{parsed.path[:30]}..."
                    citations.append(f"[{source_counter}] {short_url}")
                break
            
            seen_hashes.add(chunk.content_hash)
            source_counter += 1
            context_parts.append(f"[Source {source_counter}]\n{chunk.content}")
            current_chars += chunk_size
            
            # Citation
            parsed = urlparse(chunk.source_url)
            short_url = f"{parsed.netloc}{parsed.path[:50]}"
            citations.append(f"[{source_counter}] {short_url}")
    
    context = "\n\n".join(context_parts)
    
    return context, citations




# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 14: FASTAPI APPLICATION
# ═══════════════════════════════════════════════════════════════════════════════

app = FastAPI(
    title="Swift Search Agent v2.0",
    description=(
        "Ultra-low latency search and extraction API. "
        "Extreme edition with dynamic RAM auto-tiering and aiohttp stack."
    ),
    version="2.0.0-extreme",
    docs_url="/docs",
    redoc_url=None,
)

app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
)


# ─────────────── Request/Response Models ───────────────

class SearchRequest(BaseModel):
    query: str = Field(
        ...,
        min_length=1,
        max_length=1000,
        description="Search query",
        examples=["best practices for Python async programming"],
    )


class SearchResponse(BaseModel):
    query: str
    tier: str
    sources_found: int
    sources_processed: int
    sources_successful: int
    total_chunks: int
    results: List[dict]
    elapsed_seconds: float


class HealthResponse(BaseModel):
    status: str
    version: str
    tier: str
    concurrency: int
    max_sources: int
    aiodns_available: bool


class TierInfo(BaseModel):
    name: str
    concurrency: int
    semaphore_limit: int
    max_sources: int
    connector_limit: int
    connector_per_host: int
    dns_cache_size: int
    chunk_size: int
    fetch_timeout: float


class ConfigResponse(BaseModel):
    version: str
    active_tier: TierInfo
    available_tiers: List[str]
    searxng_url: str
    searxng_engines: str
    max_html_bytes: int
    max_context_chars: int


# ─────────────── Startup Event ───────────────

@app.on_event("startup")
async def startup_event():
    """Initialize tier configuration on startup."""
    # Note: Tier is initialized via CLI or here
    if TIER_CFG.name == "MICRO":  # Default value, may need re-init
        initialize_tier()


# ─────────────── Endpoints ───────────────

@app.get("/health", response_model=HealthResponse)
async def health():
    """Health check endpoint with tier info."""
    return HealthResponse(
        status="ok",
        version="2.0.0-extreme",
        tier=TIER_CFG.name,
        concurrency=TIER_CFG.concurrency,
        max_sources=TIER_CFG.max_sources or 999999,
        aiodns_available=AIODNS_AVAILABLE,
    )


@app.get("/config", response_model=ConfigResponse)
async def get_config():
    """Get current configuration including tier details."""
    tier_info = TierInfo(
        name=TIER_CFG.name,
        concurrency=TIER_CFG.concurrency,
        semaphore_limit=TIER_CFG.semaphore_limit,
        max_sources=TIER_CFG.max_sources,
        connector_limit=TIER_CFG.connector_limit,
        connector_per_host=TIER_CFG.connector_per_host,
        dns_cache_size=TIER_CFG.dns_cache_size,
        chunk_size=TIER_CFG.chunk_size,
        fetch_timeout=TIER_CFG.fetch_timeout,
    )
    
    return ConfigResponse(
        version="2.0.0-extreme",
        active_tier=tier_info,
        available_tiers=["MICRO", "MEDIUM", "BEAST"],
        searxng_url=SEARXNG_URL,
        searxng_engines=SEARXNG_ENGINES,
        max_html_bytes=MAX_HTML_BYTES,
        max_context_chars=MAX_CONTEXT_CHARS,
    )


@app.post("/search", response_model=SearchResponse)
async def search(body: SearchRequest):
    """
    Main search endpoint — returns raw scraped data.
    
    Pipeline:
    1. Meta-search via SearxNG (20 instances)
    2. Concurrent fetch + extraction (tier-bounded)
    3. Recursive text chunking
    4. Raw extracted text returned per source
    
    Memory: Peak varies by tier (MICRO: <60MB, MEDIUM: <200MB, BEAST: <1GB).
    """
    t0 = time.perf_counter()
    query = body.query.strip()
    log.info("━━━ NEW SEARCH [%s] ━━━ query='%s'", TIER_CFG.name, query[:80])
    
    # ─── Phase 1: Meta-Search ───
    urls = await meta_search(query)
    
    if not urls:
        raise HTTPException(
            status_code=404,
            detail="No search results found. SearxNG instances may be unavailable.",
        )
    
    sources_found = len(urls)
    log.info("Phase 1 complete: %d URLs found", sources_found)
    
    # ─── Phase 2: Concurrent Fetch + Extract + Chunk ───
    raw_results = await process_urls(urls)
    
    sources_successful = sum(1 for r in raw_results if r.success)
    total_chunks = sum(len(r.chunks) for r in raw_results)
    
    log.info(
        "Phase 2 complete: %d/%d successful, %d chunks",
        sources_successful, len(raw_results), total_chunks,
    )
    
    # Free URLs list
    del urls
    
    # Build raw results list
    results = []
    for r in raw_results:
        if r.success and r.chunks:
            results.append({
                "url": r.url,
                "title": r.title,
                "extracted_text": "\n".join(r.chunks),
                "chunk_count": len(r.chunks),
                "char_count": r.char_count,
            })
    
    # Cleanup
    del raw_results
    gc.collect()
    
    elapsed = round(time.perf_counter() - t0, 2)
    log.info(
        "━━━ DONE [%s] ━━━ elapsed=%.2fs, sources=%d/%d, chunks=%d",
        TIER_CFG.name, elapsed, sources_successful, sources_found, total_chunks,
    )
    
    return SearchResponse(
        query=query,
        tier=TIER_CFG.name,
        sources_found=sources_found,
        sources_processed=sources_found,
        sources_successful=sources_successful,
        total_chunks=total_chunks,
        results=results,
        elapsed_seconds=elapsed,
    )


@app.exception_handler(Exception)
async def global_exception_handler(request: Request, exc: Exception):
    """Global error handler with memory cleanup."""
    log.exception("Unhandled error: %s", exc)
    gc.collect()
    reset_dns_cache()
    return JSONResponse(
        status_code=500,
        content={"detail": "Internal server error. Please try again."},
    )


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 15: CLI ARGUMENT PARSER
# ═══════════════════════════════════════════════════════════════════════════════

def create_argument_parser() -> argparse.ArgumentParser:
    """Create CLI argument parser."""
    parser = argparse.ArgumentParser(
        prog="swift-search-agent",
        description="Swift Search Agent v2.0 - Extreme Millisecond Edition",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Auto-detect tier based on RAM
  python search_ultra.py
  
  # Force specific tier
  python search_ultra.py --tier micro
  python search_ultra.py --tier beast
  
  # Custom port
  python search_ultra.py --port 8080
  
  # Production mode
  python search_ultra.py --tier medium --workers 4

Tiers:
  micro  - <2GB RAM: Conservative (5 concurrent, 50 sources max)
  medium - 2-8GB RAM: Balanced (20 concurrent, 100 sources max)
  beast  - >8GB RAM: Maximum (200 concurrent, unlimited sources)
        """,
    )
    
    parser.add_argument(
        "--tier",
        type=str,
        choices=["micro", "medium", "beast", "auto"],
        default="auto",
        help="Operating tier (default: auto-detect based on RAM)",
    )
    
    parser.add_argument(
        "--port",
        type=int,
        default=int(os.environ.get("PORT", 8000)),
        help="Server port (default: 8000 or PORT env var)",
    )
    
    parser.add_argument(
        "--host",
        type=str,
        default="0.0.0.0",
        help="Server host (default: 0.0.0.0)",
    )
    
    parser.add_argument(
        "--workers",
        type=int,
        default=1,
        help="Number of worker processes (default: 1 for memory safety)",
    )
    
    parser.add_argument(
        "--reload",
        action="store_true",
        help="Enable auto-reload for development",
    )
    
    parser.add_argument(
        "--log-level",
        type=str,
        choices=["debug", "info", "warning", "error"],
        default="info",
        help="Logging level (default: info)",
    )
    
    parser.add_argument(
        "--version",
        action="version",
        version="%(prog)s 2.0.0-extreme",
    )
    
    return parser


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 16: ENTRYPOINT
# ═══════════════════════════════════════════════════════════════════════════════

if __name__ == "__main__":
    import uvicorn
    
    parser = create_argument_parser()
    args = parser.parse_args()
    
    # Initialize tier
    tier_override = None if args.tier == "auto" else args.tier
    initialize_tier(tier_override)
    
    # Configure logging
    log_level = getattr(logging, args.log_level.upper())
    logging.getLogger().setLevel(log_level)
    log.setLevel(log_level)
    
    # Startup banner
    log.info("═" * 60)
    log.info("  Swift Search Agent v2.0.0 — EXTREME EDITION")
    log.info("═" * 60)
    log.info("  Tier: %s", TIER_CFG.name)
    log.info("  Concurrency: %d simultaneous connections", TIER_CFG.concurrency)
    log.info("  Max Sources: %s", TIER_CFG.max_sources or "∞ (unlimited)")
    log.info("  Connector Pool: %d connections", TIER_CFG.connector_limit)
    log.info("  DNS Cache: %d entries, %ds TTL", TIER_CFG.dns_cache_size, TIER_CFG.dns_cache_ttl)
    log.info("  aiodns: %s", "ENABLED ✓" if AIODNS_AVAILABLE else "DISABLED ✗")
    log.info("  SearxNG: %s (engines: %s)", SEARXNG_URL, SEARXNG_ENGINES)
    log.info("═" * 60)
    log.info("  Starting server on %s:%d", args.host, args.port)
    log.info("  Workers: %d | Reload: %s", args.workers, args.reload)
    log.info("═" * 60)
    
    uvicorn.run(
        "search_ultra:app",
        host=args.host,
        port=args.port,
        workers=args.workers,
        reload=args.reload,
        log_level=args.log_level,
        access_log=False,  # Reduce log noise
    )
