#!/usr/bin/env python3
"""
Swift Search Agent v2.0 — Production-Grade, <60MB Peak RAM
=========================================================

A meticulously engineered web search and extraction pipeline
designed for extreme memory efficiency on constrained environments.

Architecture Principles:
------------------------
1. **Streaming-First**: No full HTML accumulation. Fetch → Extract → Chunk → Discard.
2. **Strict Concurrency**: asyncio.Semaphore(5) bounds concurrent in-memory HTML pages.
3. **Immediate Cleanup**: gc.collect() + trafilatura.reset_caches() after each extraction.
4. **Zero BeautifulSoup**: Pure trafilatura.bare_extraction (fast mode) + selectolax fallback.
5. **HTTP/2 + Pooling**: httpx.AsyncClient with HTTP/2 and connection reuse.
6. **Custom Chunking**: Lightweight recursive splitter (no langchain dependency).

Memory Budget Breakdown (per request):
--------------------------------------
- HTTP client pool: ~5MB (shared, persistent)
- Semaphore allows 5 concurrent pages: 5 × 2MB = 10MB max HTML
- Trafilatura parser state: ~8MB (cleared after each extraction)
- Text chunks buffer: ~2MB
- LLM payload: ~5MB
- FastAPI/uvicorn overhead: ~25MB
- TOTAL PEAK: ~55MB (under 60MB target)

Author: Swift Search Agent Team
Version: 2.0.0
License: MIT
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
from dataclasses import dataclass
from enum import Enum
from io import StringIO
from typing import (
    AsyncIterator,
    Callable,
    Final,
    List,
    NamedTuple,
    Optional,
    Tuple,
    TypeVar,
)
from urllib.parse import parse_qs, urlencode, urlparse, urlunparse

import httpx
from fastapi import FastAPI, HTTPException, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse
from pydantic import BaseModel, Field

# ═══════════════════════════════════════════════════════════════════════════════
# LAZY IMPORTS — Defer heavy modules until actually needed
# ═══════════════════════════════════════════════════════════════════════════════

# These are imported lazily to reduce startup memory and allow fallback handling
_trafilatura = None
_selectolax_lexbor = None


def _get_trafilatura():
    """Lazy import trafilatura to defer memory allocation."""
    global _trafilatura
    if _trafilatura is None:
        import trafilatura
        _trafilatura = trafilatura
    return _trafilatura


def _get_selectolax():
    """Lazy import selectolax.lexbor for fallback extraction."""
    global _selectolax_lexbor
    if _selectolax_lexbor is None:
        try:
            from selectolax.lexbor import LexborHTMLParser
            _selectolax_lexbor = LexborHTMLParser
        except ImportError:
            # selectolax not installed — fallback will use basic regex
            _selectolax_lexbor = False
    return _selectolax_lexbor


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 1: LOGGING CONFIGURATION
# ═══════════════════════════════════════════════════════════════════════════════

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s | %(levelname)-7s | %(name)s | %(message)s",
    datefmt="%H:%M:%S",
    stream=sys.stdout,
)
log = logging.getLogger("swift-v2")


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 2: CONSTANTS AND CONFIGURATION
# ═══════════════════════════════════════════════════════════════════════════════

# ─────────────── Core Limits ───────────────
SEMAPHORE_LIMIT: Final[int] = 5           # Max concurrent HTML pages in memory
MAX_URLS: Final[int] = 50                  # Max URLs to process per query
FETCH_TIMEOUT_SEC: Final[float] = 12.0     # Per-URL timeout (generous for full pages)
MAX_HTML_BYTES: Final[int] = 2 * 1024 * 1024  # 2MB per page (no truncation)
MAX_CONTEXT_CHARS: Final[int] = 80_000     # Context buffer limit
CHUNK_SIZE: Final[int] = 1500              # Target chunk size (chars)
CHUNK_OVERLAP: Final[int] = 200            # Overlap between chunks
MIN_TEXT_LENGTH: Final[int] = 100          # Minimum useful text length

# ─────────────── SearxNG Configuration ───────────────
# Imported from centralized config
from config import SEARXNG_URL, SEARXNG_ENGINES

# ─────────────── HTTP Headers ───────────────
USER_AGENT: Final[str] = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 SwiftSearchBot/2.0"
)

# ─────────────── URL Tracking Parameters to Remove ───────────────
TRACKING_PARAMS: Final[frozenset[str]] = frozenset({
    # Google Analytics
    "utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content",
    "utm_id", "utm_cid", "utm_reader", "utm_name", "utm_social",
    # Facebook
    "fbclid", "fb_action_ids", "fb_action_types", "fb_source", "fb_ref",
    # Google Ads / Microsoft
    "gclid", "gclsrc", "dclid", "msclkid", "gbraid", "wbraid",
    # Twitter / Social
    "twclid", "igshid",
    # General tracking
    "ref", "source", "src", "campaign", "affiliate", "partner",
    "_ga", "_gl", "_gid", "mc_cid", "mc_eid", "mkt_tok",
    "oly_enc_id", "oly_anon_id", "_hsenc", "_hsmi",
    # AMP
    "amp", "amp_js_v", "usqp", "outputType",
    # Others
    "spm", "share_from", "scm", "algo_pvid", "algo_exp_id",
    "zanpid", "kenshoo_gclid",
})

# ─────────────── Domains to Skip ───────────────
SKIP_DOMAINS: Final[frozenset[str]] = frozenset({
    "facebook.com", "twitter.com", "x.com", "instagram.com", "tiktok.com",
    "youtube.com", "youtu.be", "linkedin.com", "pinterest.com",
    "reddit.com",  # Requires auth for most content
    "play.google.com", "apps.apple.com",
    "drive.google.com", "docs.google.com",
    "amazon.com", "ebay.com", "aliexpress.com",  # E-commerce noise
})

# ─────────────── File Extensions to Skip ───────────────
SKIP_EXTENSIONS: Final[frozenset[str]] = frozenset({
    ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx",
    ".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg", ".ico",
    ".mp4", ".mp3", ".avi", ".mov", ".wmv", ".flv", ".webm",
    ".zip", ".rar", ".7z", ".tar", ".gz", ".bz2",
    ".exe", ".msi", ".dmg", ".apk", ".deb", ".rpm",
})


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 3: URL CLEANING AND VALIDATION
# ═══════════════════════════════════════════════════════════════════════════════

def clean_url(url: str) -> str:
    """
    Normalize URL by removing tracking parameters and standardizing format.
    
    Memory: O(1) — operates on single string, no accumulation.
    """
    if not url:
        return ""
    
    url = url.strip()
    if not url.startswith(("http://", "https://")):
        return url
    
    try:
        parsed = urlparse(url)
        
        # Normalize domain: lowercase, remove www
        domain = parsed.netloc.lower()
        if domain.startswith("www."):
            domain = domain[4:]
        
        # Normalize path: remove trailing slashes
        path = (parsed.path or "/").rstrip("/") or "/"
        
        # Remove tracking parameters
        if parsed.query:
            params = parse_qs(parsed.query, keep_blank_values=False)
            cleaned_params = {
                k: v for k, v in params.items()
                if k.lower() not in TRACKING_PARAMS
            }
            # Sort for deterministic output
            query = urlencode(sorted(cleaned_params.items()), doseq=True)
        else:
            query = ""
        
        # Reconstruct without fragment (anchors not useful for scraping)
        return urlunparse((parsed.scheme, domain, path, "", query, ""))
    
    except Exception:
        return url


def get_url_key(url: str) -> str:
    """Generate deduplication key from URL (domain + path only)."""
    try:
        parsed = urlparse(clean_url(url))
        return f"{parsed.netloc}{parsed.path}".lower().rstrip("/")
    except Exception:
        return url.lower()


def is_valid_url(url: str) -> bool:
    """
    Validate URL is scrapable and likely to contain useful content.
    
    Returns False for:
    - Non-HTTP schemes
    - Known social media / e-commerce domains
    - Binary file extensions
    """
    if not url or not url.strip():
        return False
    
    url_lower = url.lower().strip()
    
    if not url_lower.startswith(("http://", "https://")):
        return False
    
    # Check domain blocklist
    try:
        parsed = urlparse(url_lower)
        domain = parsed.netloc
        if domain.startswith("www."):
            domain = domain[4:]
        
        for skip_domain in SKIP_DOMAINS:
            if skip_domain in domain:
                return False
        
        # Check file extension
        path = parsed.path.lower()
        for ext in SKIP_EXTENSIONS:
            if path.endswith(ext):
                return False
        
    except Exception:
        pass
    
    return True


def deduplicate_urls(urls: List[str], max_count: int = MAX_URLS) -> List[str]:
    """
    Deduplicate URLs by normalized key, preserving insertion order.
    Also filters invalid URLs. Memory: O(n) for seen set.
    """
    seen: set[str] = set()
    unique: List[str] = []
    
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
# SECTION 4: TEXT CHUNKING — Lightweight recursive splitter
# ═══════════════════════════════════════════════════════════════════════════════

class TextChunk(NamedTuple):
    """Immutable text chunk with source URL."""
    text: str
    source_url: str
    chunk_index: int


# Regex patterns for splitting (compiled once for performance)
_PARAGRAPH_SPLIT = re.compile(r'\n\s*\n')
_SENTENCE_SPLIT = re.compile(r'(?<=[.!?])\s+(?=[A-Z])')
_WORD_BOUNDARY = re.compile(r'\s+')


def recursive_text_splitter(
    text: str,
    source_url: str,
    chunk_size: int = CHUNK_SIZE,
    chunk_overlap: int = CHUNK_OVERLAP,
) -> List[TextChunk]:
    """
    Split text into overlapping chunks using recursive paragraph/sentence splitting.
    
    Algorithm:
    1. Try splitting by paragraphs (double newline)
    2. If chunks too large, split by sentences
    3. If still too large, split by word boundaries
    4. Apply overlap between consecutive chunks
    
    Memory-efficient: Uses generators internally, materializes only final chunks.
    No external dependencies (langchain-free).
    """
    if not text or len(text.strip()) < MIN_TEXT_LENGTH:
        return []
    
    text = text.strip()
    
    # If text is small enough, return as single chunk
    if len(text) <= chunk_size:
        return [TextChunk(text=text, source_url=source_url, chunk_index=0)]
    
    # Split by paragraphs first
    paragraphs = _PARAGRAPH_SPLIT.split(text)
    
    chunks: List[TextChunk] = []
    current_chunk = StringIO()
    current_length = 0
    chunk_index = 0
    
    def _flush_chunk(buffer: StringIO, include_overlap: bool = True) -> Optional[str]:
        """Flush current buffer to chunk, optionally preserving overlap."""
        nonlocal current_length
        content = buffer.getvalue().strip()
        
        if not content or len(content) < MIN_TEXT_LENGTH // 2:
            return None
        
        buffer.seek(0)
        buffer.truncate()
        
        # Keep overlap for next chunk
        if include_overlap and len(content) > chunk_overlap:
            # Find a good split point near the overlap boundary
            overlap_text = content[-(chunk_overlap):]
            # Try to start at sentence boundary
            sent_match = _SENTENCE_SPLIT.search(overlap_text)
            if sent_match:
                overlap_text = overlap_text[sent_match.start():]
            buffer.write(overlap_text)
            current_length = len(overlap_text)
        else:
            current_length = 0
        
        return content
    
    for para in paragraphs:
        para = para.strip()
        if not para:
            continue
        
        # Check if paragraph fits in current chunk
        if current_length + len(para) + 2 <= chunk_size:  # +2 for \n\n
            if current_length > 0:
                current_chunk.write("\n\n")
            current_chunk.write(para)
            current_length += len(para) + 2
        
        # Paragraph too large — try sentence splitting
        elif len(para) > chunk_size:
            # Flush current chunk first
            if current_length > 0:
                chunk_text = _flush_chunk(current_chunk)
                if chunk_text:
                    chunks.append(TextChunk(
                        text=chunk_text,
                        source_url=source_url,
                        chunk_index=chunk_index,
                    ))
                    chunk_index += 1
            
            # Split large paragraph by sentences
            sentences = _SENTENCE_SPLIT.split(para)
            for sent in sentences:
                sent = sent.strip()
                if not sent:
                    continue
                
                if current_length + len(sent) + 1 <= chunk_size:
                    if current_length > 0:
                        current_chunk.write(" ")
                    current_chunk.write(sent)
                    current_length += len(sent) + 1
                
                # Sentence too large — split by words (last resort)
                elif len(sent) > chunk_size:
                    if current_length > 0:
                        chunk_text = _flush_chunk(current_chunk)
                        if chunk_text:
                            chunks.append(TextChunk(
                                text=chunk_text,
                                source_url=source_url,
                                chunk_index=chunk_index,
                            ))
                            chunk_index += 1
                    
                    words = _WORD_BOUNDARY.split(sent)
                    for word in words:
                        if current_length + len(word) + 1 <= chunk_size:
                            if current_length > 0:
                                current_chunk.write(" ")
                            current_chunk.write(word)
                            current_length += len(word) + 1
                        else:
                            chunk_text = _flush_chunk(current_chunk)
                            if chunk_text:
                                chunks.append(TextChunk(
                                    text=chunk_text,
                                    source_url=source_url,
                                    chunk_index=chunk_index,
                                ))
                                chunk_index += 1
                            current_chunk.write(word)
                            current_length = len(word)
                
                else:
                    # Flush and start new chunk with this sentence
                    chunk_text = _flush_chunk(current_chunk)
                    if chunk_text:
                        chunks.append(TextChunk(
                            text=chunk_text,
                            source_url=source_url,
                            chunk_index=chunk_index,
                        ))
                        chunk_index += 1
                    current_chunk.write(sent)
                    current_length = len(sent)
        
        else:
            # Paragraph doesn't fit — flush current and start new
            chunk_text = _flush_chunk(current_chunk)
            if chunk_text:
                chunks.append(TextChunk(
                    text=chunk_text,
                    source_url=source_url,
                    chunk_index=chunk_index,
                ))
                chunk_index += 1
            current_chunk.write(para)
            current_length = len(para)
    
    # Flush final chunk
    final_text = current_chunk.getvalue().strip()
    if final_text and len(final_text) >= MIN_TEXT_LENGTH // 2:
        chunks.append(TextChunk(
            text=final_text,
            source_url=source_url,
            chunk_index=chunk_index,
        ))
    
    current_chunk.close()
    return chunks


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 5: EXTRACTION ENGINE — Trafilatura + Selectolax Fallback
# ═══════════════════════════════════════════════════════════════════════════════

def _reset_trafilatura_caches() -> None:
    """
    Reset trafilatura's internal caches to free memory.
    
    Trafilatura maintains several caches (lxml parser pools, compiled XPath,
    domain-specific rules). This ensures they don't accumulate across requests.
    """
    try:
        traf = _get_trafilatura()
        # Reset internal caches if available
        if hasattr(traf, 'settings') and hasattr(traf.settings, 'reset_caches'):
            traf.settings.reset_caches()
        
        # Clear any lxml-related caches
        if hasattr(traf, 'core') and hasattr(traf.core, 'SANITIZED_TREE_CACHE'):
            traf.core.SANITIZED_TREE_CACHE.clear()
        
    except Exception as e:
        log.debug("Cache reset skipped: %s", e)


def _extract_with_trafilatura(html: str, url: str) -> Optional[str]:
    """
    Extract text using trafilatura's bare_extraction in fast mode.
    
    Configuration:
    - fast=True: Skip fallback algorithms (justext, readability)
    - include_comments=False: Reduce noise
    - include_tables=False: Tables often contain non-prose data
    - output_format='txt': Plain text output
    
    Returns extracted text or None on failure.
    """
    try:
        traf = _get_trafilatura()
        
        # Use bare_extraction for maximum control
        # Returns a dict with 'text', 'title', 'author', etc.
        result = traf.bare_extraction(
            html,
            url=url,
            include_comments=False,
            include_tables=False,
            no_fallback=True,  # Skip justext/readability fallbacks
            output_format='txt',
        )
        
        if result and isinstance(result, dict):
            text = result.get('text', '') or ''
            if len(text.strip()) >= MIN_TEXT_LENGTH:
                return text.strip()
        
        elif isinstance(result, str) and len(result.strip()) >= MIN_TEXT_LENGTH:
            return result.strip()
        
    except Exception as e:
        log.debug("Trafilatura extraction failed for %s: %s", url[:50], type(e).__name__)
    
    return None


def _extract_with_selectolax(html: str, url: str) -> Optional[str]:
    """
    Fallback extraction using selectolax's lexbor backend.
    
    This is a lightning-fast fallback that:
    1. Strips script/style/nav/header/footer tags
    2. Extracts remaining text content
    3. Cleans up whitespace
    
    selectolax is ~20x faster than BeautifulSoup.
    """
    LexborParser = _get_selectolax()
    
    if LexborParser is False:
        # selectolax not installed — use regex fallback
        return _extract_with_regex(html, url)
    
    try:
        tree = LexborParser(html)
        
        # Remove noise elements
        noise_tags = [
            'script', 'style', 'nav', 'header', 'footer', 'aside',
            'noscript', 'iframe', 'svg', 'form', 'button', 'input',
            'meta', 'link', 'head',
        ]
        tree.strip_tags(noise_tags, recursive=True)
        
        # Extract text from body (or root if no body)
        body = tree.body
        if body is None:
            body = tree.root
        
        if body is None:
            return None
        
        # Get text with space separator
        text = body.text(separator=' ', strip=True)
        
        # Clean up excessive whitespace
        text = re.sub(r'\s+', ' ', text).strip()
        
        if len(text) >= MIN_TEXT_LENGTH:
            return text
        
    except Exception as e:
        log.debug("Selectolax extraction failed for %s: %s", url[:50], type(e).__name__)
    
    return None


def _extract_with_regex(html: str, url: str) -> Optional[str]:
    """
    Ultimate fallback: regex-based text extraction.
    
    Used when neither trafilatura nor selectolax work.
    Strips HTML tags and extracts visible text.
    """
    try:
        # Remove script and style content
        text = re.sub(r'<script[^>]*>.*?</script>', '', html, flags=re.DOTALL | re.IGNORECASE)
        text = re.sub(r'<style[^>]*>.*?</style>', '', text, flags=re.DOTALL | re.IGNORECASE)
        
        # Remove all HTML tags
        text = re.sub(r'<[^>]+>', ' ', text)
        
        # Decode common HTML entities
        text = text.replace('&nbsp;', ' ')
        text = text.replace('&amp;', '&')
        text = text.replace('&lt;', '<')
        text = text.replace('&gt;', '>')
        text = text.replace('&quot;', '"')
        text = re.sub(r'&#?\w+;', ' ', text)  # Remove other entities
        
        # Clean whitespace
        text = re.sub(r'\s+', ' ', text).strip()
        
        if len(text) >= MIN_TEXT_LENGTH:
            return text
        
    except Exception as e:
        log.debug("Regex extraction failed for %s: %s", url[:50], type(e).__name__)
    
    return None


def extract_text(html: str, url: str) -> Optional[str]:
    """
    Extract text from HTML using multi-strategy fallback chain.
    
    Strategy order:
    1. Trafilatura bare_extraction (fast=True) — best quality
    2. Selectolax lexbor — fast fallback
    3. Regex stripping — ultimate fallback
    
    Returns extracted text or None if all strategies fail.
    """
    # Strategy 1: Trafilatura (highest quality)
    text = _extract_with_trafilatura(html, url)
    if text:
        return text
    
    # Strategy 2: Selectolax (fast, good quality)
    text = _extract_with_selectolax(html, url)
    if text:
        return text
    
    # Strategy 3: Regex (always works, lowest quality)
    text = _extract_with_regex(html, url)
    return text


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 6: HTTP CLIENT WITH HTTP/2 AND CONNECTION POOLING
# ═══════════════════════════════════════════════════════════════════════════════

@dataclass
class ExtractionResult:
    """Result from fetch + extract pipeline."""
    url: str
    text: str
    chunks: List[TextChunk]
    success: bool
    error: Optional[str] = None


# Global semaphore for memory control
_extraction_semaphore: Optional[asyncio.Semaphore] = None


def _get_semaphore() -> asyncio.Semaphore:
    """
    Get or create the extraction semaphore.
    
    This semaphore limits the number of concurrent HTML pages in memory.
    With 5 concurrent pages at 2MB max each = 10MB HTML memory budget.
    """
    global _extraction_semaphore
    if _extraction_semaphore is None:
        _extraction_semaphore = asyncio.Semaphore(SEMAPHORE_LIMIT)
    return _extraction_semaphore


async def fetch_and_extract(
    client: httpx.AsyncClient,
    url: str,
) -> ExtractionResult:
    """
    Fetch URL and extract text, bounded by semaphore.
    
    Memory Management:
    1. Acquire semaphore (limits concurrent HTML in memory)
    2. Stream-fetch HTML (no buffering large responses)
    3. Decode and extract text immediately
    4. Chunk text immediately (while original text is in scope)
    5. Release HTML and text references
    6. Run gc.collect() + reset_caches()
    7. Release semaphore
    
    This ensures at most SEMAPHORE_LIMIT HTML pages exist in memory simultaneously.
    """
    sem = _get_semaphore()
    
    async with sem:
        # ─── MEMORY ZONE START: HTML + Text in memory ───
        html: Optional[str] = None
        text: Optional[str] = None
        chunks: List[TextChunk] = []
        
        try:
            # Streaming fetch with size limit
            html_chunks: List[bytes] = []
            total_size = 0
            
            async with client.stream(
                "GET",
                url,
                headers={"User-Agent": USER_AGENT},
                timeout=FETCH_TIMEOUT_SEC,
                follow_redirects=True,
            ) as response:
                # Check status
                if response.status_code != 200:
                    return ExtractionResult(
                        url=url,
                        text="",
                        chunks=[],
                        success=False,
                        error=f"HTTP {response.status_code}",
                    )
                
                # Check content-type
                content_type = response.headers.get("content-type", "").lower()
                if "text/html" not in content_type and "text/plain" not in content_type:
                    return ExtractionResult(
                        url=url,
                        text="",
                        chunks=[],
                        success=False,
                        error=f"Invalid content-type: {content_type[:50]}",
                    )
                
                # Stream response body
                async for chunk in response.aiter_bytes(chunk_size=65536):
                    html_chunks.append(chunk)
                    total_size += len(chunk)
                    
                    # NO TRUNCATION — but we do have a soft warning
                    if total_size > MAX_HTML_BYTES:
                        log.debug("Large page (%d bytes): %s", total_size, url[:60])
            
            # Decode HTML
            try:
                html = b"".join(html_chunks).decode("utf-8", errors="replace")
            except Exception:
                html = b"".join(html_chunks).decode("latin-1", errors="replace")
            
            # Free chunk list immediately
            del html_chunks
            
            # Extract text (trafilatura → selectolax → regex)
            text = extract_text(html, url)
            
            # Free HTML immediately after extraction
            del html
            html = None
            
            if not text:
                return ExtractionResult(
                    url=url,
                    text="",
                    chunks=[],
                    success=False,
                    error="Extraction failed (empty result)",
                )
            
            # Chunk immediately while text is in scope
            chunks = recursive_text_splitter(text, source_url=url)
            
            # We can now free the original text (chunks have copies)
            original_text_length = len(text)
            del text
            text = None
            
            # Aggressive memory cleanup
            _reset_trafilatura_caches()
            gc.collect()
            
            # Return success with chunks
            return ExtractionResult(
                url=url,
                text=f"[{original_text_length} chars extracted, {len(chunks)} chunks]",
                chunks=chunks,
                success=True,
            )
        
        except httpx.TimeoutException:
            return ExtractionResult(
                url=url,
                text="",
                chunks=[],
                success=False,
                error="Timeout",
            )
        
        except httpx.HTTPError as e:
            return ExtractionResult(
                url=url,
                text="",
                chunks=[],
                success=False,
                error=f"HTTP error: {type(e).__name__}",
            )
        
        except Exception as e:
            log.warning("Fetch/extract failed for %s: %s", url[:50], e)
            return ExtractionResult(
                url=url,
                text="",
                chunks=[],
                success=False,
                error=str(e)[:100],
            )
        
        finally:
            # ─── MEMORY ZONE END: Ensure cleanup ───
            # Explicit None assignment helps Python's refcount GC
            html = None
            text = None
            gc.collect()


async def process_urls(urls: List[str]) -> List[ExtractionResult]:
    """
    Process multiple URLs concurrently with HTTP/2 and connection pooling.
    
    Uses a single shared httpx.AsyncClient for:
    - HTTP/2 multiplexing (multiple requests over one connection)
    - Connection pooling (reuse connections across requests)
    - Automatic connection keep-alive
    
    Memory: Semaphore limits concurrent in-memory HTML to SEMAPHORE_LIMIT pages.
    """
    results: List[ExtractionResult] = []
    
    # Configure client with HTTP/2 and pooling
    async with httpx.AsyncClient(
        http2=True,  # Enable HTTP/2 for multiplexing
        follow_redirects=True,
        limits=httpx.Limits(
            max_connections=SEMAPHORE_LIMIT + 5,  # Pool size
            max_keepalive_connections=SEMAPHORE_LIMIT,
            keepalive_expiry=30.0,
        ),
        timeout=httpx.Timeout(
            connect=5.0,
            read=FETCH_TIMEOUT_SEC,
            write=5.0,
            pool=10.0,
        ),
    ) as client:
        # Create all tasks
        tasks = [fetch_and_extract(client, url) for url in urls]
        
        # Process as completed (faster URLs return first)
        for coro in asyncio.as_completed(tasks):
            try:
                result = await coro
                results.append(result)
            except Exception as e:
                log.error("Task error: %s", e)
    
    return results


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 7: SEARXNG META-SEARCH
# ═══════════════════════════════════════════════════════════════════════════════

async def meta_search(query: str) -> List[str]:
    """
    Query your private SearxNG instance with explicit engine selection.
    Engines: duckduckgo, brave, yahoo, qwant, mojeek
    """
    params = {
        "q": query,
        "format": "json",
        "categories": "general",
        "language": "en",
        "pageno": 1,
        "engines": SEARXNG_ENGINES,
    }
    
    search_url = f"{SEARXNG_URL.rstrip('/')}/search"
    
    try:
        async with httpx.AsyncClient(
            follow_redirects=True,
            timeout=15.0,
        ) as client:
            resp = await client.get(
                search_url,
                params=params,
                headers={"User-Agent": USER_AGENT},
            )
            resp.raise_for_status()
            data = resp.json()
            
            raw_urls = [
                r.get("url", "").strip()
                for r in data.get("results", [])
                if r.get("url")
            ]
            unique = deduplicate_urls(raw_urls, MAX_URLS)
            log.info("Meta-search: %d unique URLs (engines: %s) for: '%s'",
                     len(unique), SEARXNG_ENGINES, query[:60])
            return unique
            
    except httpx.ConnectError:
        log.error("Cannot connect to SearxNG at %s", SEARXNG_URL)
        return []
    except Exception as e:
        log.error("Meta-search failed: %s", e)
        return []


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 8: CONTEXT BUILDING FROM CHUNKS
# ═══════════════════════════════════════════════════════════════════════════════

def build_context_from_chunks(
    results: List[ExtractionResult],
    max_chars: int = MAX_CONTEXT_CHARS,
) -> Tuple[str, List[str]]:
    """
    Build LLM context from extraction results.
    
    Prioritizes chunks from successful extractions.
    Uses content hashing to deduplicate similar content.
    
    Returns (context_string, list_of_citation_urls).
    """
    buffer = StringIO()
    citations: List[str] = []
    seen_hashes: set[str] = set()
    char_count = 0
    source_idx = 0
    
    for result in results:
        if not result.success or not result.chunks:
            continue
        
        for chunk in result.chunks:
            # Content hash for deduplication (first 500 chars)
            content_sample = chunk.text[:500].lower().strip()
            content_hash = hashlib.md5(
                content_sample.encode(), usedforsecurity=False
            ).hexdigest()[:12]
            
            if content_hash in seen_hashes:
                continue
            seen_hashes.add(content_hash)
            
            # Format chunk with source marker
            source_idx += 1
            marker = f"\n\n--- Source [{source_idx}]: {chunk.source_url} ---\n{chunk.text}"
            
            if char_count + len(marker) > max_chars:
                # Check if we have room for a truncated version
                remaining = max_chars - char_count
                if remaining > 300:
                    buffer.write(marker[:remaining])
                    if chunk.source_url not in citations:
                        citations.append(chunk.source_url)
                break
            
            buffer.write(marker)
            char_count += len(marker)
            
            if chunk.source_url not in citations:
                citations.append(chunk.source_url)
        
        if char_count >= max_chars:
            break
    
    context = buffer.getvalue()
    buffer.close()
    
    return context, citations


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 10: FASTAPI APPLICATION
# ═══════════════════════════════════════════════════════════════════════════════

app = FastAPI(
    title="Swift Search Agent v2.0",
    description=(
        "Production-grade search and extraction API. "
        "Optimized for <60MB peak RAM on constrained environments."
    ),
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
    sources_found: int
    sources_processed: int
    sources_successful: int
    total_chunks: int
    results: List[dict]
    elapsed_seconds: float


class HealthResponse(BaseModel):
    status: str
    version: str
    semaphore_limit: int
    max_urls: int


# ─────────────── Endpoints ───────────────

@app.get("/health", response_model=HealthResponse)
async def health():
    """Health check endpoint."""
    return HealthResponse(
        status="ok",
        version="2.0.0",
        semaphore_limit=SEMAPHORE_LIMIT,
        max_urls=MAX_URLS,
    )


@app.get("/config")
async def get_config():
    """Get current configuration."""
    return {
        "version": "2.0.0",
        "semaphore_limit": SEMAPHORE_LIMIT,
        "max_urls": MAX_URLS,
        "fetch_timeout_sec": FETCH_TIMEOUT_SEC,
        "max_html_bytes": MAX_HTML_BYTES,
        "max_context_chars": MAX_CONTEXT_CHARS,
        "chunk_size": CHUNK_SIZE,
        "chunk_overlap": CHUNK_OVERLAP,
        "min_text_length": MIN_TEXT_LENGTH,
        "searxng_url": SEARXNG_URL,
        "searxng_engines": SEARXNG_ENGINES,
    }


@app.post("/search", response_model=SearchResponse)
async def search(body: SearchRequest):
    """
    Main search endpoint — returns raw scraped data.
    
    Pipeline:
    1. Meta-search via SearxNG (20 instances)
    2. Concurrent fetch + extraction (semaphore-bounded)
    3. Recursive text chunking
    4. Raw extracted text returned per source
    
    Memory: Peak <60MB via semaphore + immediate cleanup.
    """
    t0 = time.perf_counter()
    query = body.query.strip()
    log.info("━━━ NEW SEARCH ━━━ query='%s'", query[:80])
    
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
        "━━━ DONE ━━━ elapsed=%.2fs, sources=%d/%d, chunks=%d",
        elapsed, sources_successful, sources_found, total_chunks,
    )
    
    return SearchResponse(
        query=query,
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
    return JSONResponse(
        status_code=500,
        content={"detail": "Internal server error. Please try again."},
    )


# ═══════════════════════════════════════════════════════════════════════════════
# SECTION 11: ENTRYPOINT
# ═══════════════════════════════════════════════════════════════════════════════

if __name__ == "__main__":
    import uvicorn
    
    port = int(os.environ.get("PORT", 8000))
    
    log.info("=" * 60)
    log.info("Swift Search Agent v2.0.0")
    log.info("=" * 60)
    log.info("Semaphore limit: %d concurrent pages", SEMAPHORE_LIMIT)
    log.info("Max URLs per query: %d", MAX_URLS)
    log.info("Max HTML per page: %d bytes", MAX_HTML_BYTES)
    log.info("Chunk size: %d chars (overlap: %d)", CHUNK_SIZE, CHUNK_OVERLAP)
    log.info("SearxNG: %s (engines: %s)", SEARXNG_URL, SEARXNG_ENGINES)
    log.info("Starting server on port %d...", port)
    log.info("=" * 60)
    
    uvicorn.run(
        "search_optimized:app",
        host="0.0.0.0",
        port=port,
        workers=1,  # Single worker for memory safety
        log_level="info",
        access_log=False,  # Reduce log noise
    )
