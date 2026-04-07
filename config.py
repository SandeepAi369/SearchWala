"""
Swift Search Agent - Centralized Configuration
==============================================
Auto-detects RAM tier and configures optimal settings.
Supports environment variable overrides for flexibility.
"""

from __future__ import annotations

import os
import sys
from dataclasses import dataclass, field
from enum import Enum
from typing import Optional

# ─────────────────────────── RAM Detection ────────────────────────────

def _detect_available_ram_mb() -> int:
    """Detect available system RAM in MB."""
    try:
        import psutil
        return int(psutil.virtual_memory().total / (1024 * 1024))
    except ImportError:
        pass
    
    # Fallback: Read from /proc/meminfo (Linux)
    try:
        with open("/proc/meminfo", "r") as f:
            for line in f:
                if line.startswith("MemTotal:"):
                    kb = int(line.split()[1])
                    return kb // 1024
    except (FileNotFoundError, PermissionError):
        pass
    
    # Default: Assume 512MB (safe for free tiers)
    return 512


# ─────────────────────────── Enums ────────────────────────────

class SearchMode(Enum):
    """Search operation mode."""
    UNIFIED = "unified"      # Merged optimized pipeline (default)
    SEPARATE = "separate"    # Use separate modules for customization


class ExtractionQuality(Enum):
    """Text extraction quality level."""
    HIGH = "high"        # Full extraction with tables, comments, fallbacks
    MEDIUM = "medium"    # Balanced (default) - no comments, with fallback
    FAST = "fast"        # Minimal extraction, no fallback (speed priority)


class RAMTier(Enum):
    """RAM tier for optimization tuning."""
    MICRO = "micro"      # 256MB or less
    SMALL = "small"      # 512MB
    MEDIUM = "medium"    # 1GB
    LARGE = "large"      # 2GB+


# ─────────────────────────── Tier Configurations ────────────────────────────

@dataclass
class TierConfig:
    """Configuration settings for a specific RAM tier."""
    semaphore_limit: int          # Max concurrent connections
    max_urls: int                 # Max URLs to scrape
    html_cap_bytes: int           # Max HTML size per page
    max_context_chars: int        # Max context for LLM
    scrape_timeout_sec: float     # Per-URL timeout
    enable_head_check: bool       # HEAD request before GET
    extraction_quality: ExtractionQuality


TIER_CONFIGS: dict[RAMTier, TierConfig] = {
    RAMTier.MICRO: TierConfig(
        semaphore_limit=3,
        max_urls=25,
        html_cap_bytes=256 * 1024,      # 256KB
        max_context_chars=50_000,
        scrape_timeout_sec=5.0,
        enable_head_check=True,
        extraction_quality=ExtractionQuality.FAST,
    ),
    RAMTier.SMALL: TierConfig(
        semaphore_limit=5,
        max_urls=40,
        html_cap_bytes=512 * 1024,      # 512KB
        max_context_chars=70_000,
        scrape_timeout_sec=6.0,
        enable_head_check=True,
        extraction_quality=ExtractionQuality.MEDIUM,
    ),
    RAMTier.MEDIUM: TierConfig(
        semaphore_limit=8,
        max_urls=50,
        html_cap_bytes=768 * 1024,      # 768KB
        max_context_chars=80_000,
        scrape_timeout_sec=7.0,
        enable_head_check=False,
        extraction_quality=ExtractionQuality.MEDIUM,
    ),
    RAMTier.LARGE: TierConfig(
        semaphore_limit=12,
        max_urls=60,
        html_cap_bytes=1024 * 1024,     # 1MB
        max_context_chars=100_000,
        scrape_timeout_sec=8.0,
        enable_head_check=False,
        extraction_quality=ExtractionQuality.HIGH,
    ),
}


# ─────────────────────────── URL Tracking Params ────────────────────────────

TRACKING_PARAMS_TO_REMOVE: frozenset[str] = frozenset({
    # Google Analytics
    "utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content", "utm_id",
    # Facebook
    "fbclid", "fb_action_ids", "fb_action_types", "fb_source", "fb_ref",
    # Google Ads
    "gclid", "gclsrc", "dclid",
    # Microsoft
    "msclkid",
    # Twitter
    "twclid",
    # General tracking
    "ref", "source", "src", "campaign", "affiliate", "partner",
    # AMP and mobile
    "amp", "amp_js_v", "usqp", "outputType",
    # Session / analytics
    "_ga", "_gl", "mc_cid", "mc_eid", "mkt_tok", "oly_enc_id", "oly_anon_id",
    # Mailchimp
    "mc_cid", "mc_eid",
    # HubSpot
    "_hsenc", "_hsmi", "hsCtaTracking",
    # Other
    "spm", "share_from", "scm", "algo_pvid", "algo_exp_id",
})


# ─────────────────────────── SearxNG Configuration ────────────────────────────
# Single endpoint — your own private SearxNG (local, HF Space, VPS, etc.)
SEARXNG_URL: str = os.environ.get("SEARXNG_URL", "http://localhost:8080")

# Datacenter-friendly engines — NO Google, NO Bing
# These 5 engines work reliably from any server without CAPTCHA blocks
SEARXNG_ENGINES: str = os.environ.get(
    "SEARXNG_ENGINES",
    "duckduckgo,brave,yahoo,qwant,mojeek",
)


# ─────────────────────────── Main Config Class ────────────────────────────

@dataclass
class SearchConfig:
    """
    Main configuration for Swift Search Agent.
    Auto-detects optimal settings based on available RAM.
    All values can be overridden via environment variables.
    """
    # Mode selection
    mode: SearchMode = field(default=SearchMode.UNIFIED)
    
    # Auto-detected tier
    ram_tier: RAMTier = field(default=RAMTier.SMALL)
    tier_config: TierConfig = field(default_factory=lambda: TIER_CONFIGS[RAMTier.SMALL])
    
    
    # HTTP settings
    user_agent: str = "Mozilla/5.0 (compatible; SwiftSearchBot/2.0)"
    request_timeout: float = 10.0
    
    # Early termination threshold (stop when this % of context filled)
    early_stop_threshold: float = 0.75
    
    # Content quality
    min_text_length: int = 50
    
    # SearxNG endpoint
    searxng_url: str = SEARXNG_URL
    searxng_engines: str = SEARXNG_ENGINES
    
    @classmethod
    def auto_detect(cls) -> "SearchConfig":
        """Create config with auto-detected RAM tier and env overrides."""
        ram_mb = _detect_available_ram_mb()
        
        # Determine tier
        if ram_mb <= 300:
            tier = RAMTier.MICRO
        elif ram_mb <= 600:
            tier = RAMTier.SMALL
        elif ram_mb <= 1200:
            tier = RAMTier.MEDIUM
        else:
            tier = RAMTier.LARGE
        
        # Environment overrides
        mode_str = os.environ.get("SEARCH_MODE", "unified").lower()
        mode = SearchMode.SEPARATE if mode_str == "separate" else SearchMode.UNIFIED
        
        # Tier override
        tier_override = os.environ.get("SEARCH_RAM_TIER", "").lower()
        if tier_override in ("micro", "small", "medium", "large"):
            tier = RAMTier(tier_override)
        
        tier_config = TIER_CONFIGS[tier]
        
        # Quality override
        quality_str = os.environ.get("SEARCH_QUALITY", "").lower()
        if quality_str in ("high", "medium", "fast"):
            tier_config = TierConfig(
                semaphore_limit=tier_config.semaphore_limit,
                max_urls=tier_config.max_urls,
                html_cap_bytes=tier_config.html_cap_bytes,
                max_context_chars=tier_config.max_context_chars,
                scrape_timeout_sec=tier_config.scrape_timeout_sec,
                enable_head_check=tier_config.enable_head_check,
                extraction_quality=ExtractionQuality(quality_str),
            )
        
        # Early stop threshold
        early_stop = float(os.environ.get("SEARCH_EARLY_STOP", "0.75"))
        
        return cls(
            mode=mode,
            ram_tier=tier,
            tier_config=tier_config,
            early_stop_threshold=early_stop,
        )
    
    def __post_init__(self):
        """Log configuration on creation."""
        import logging
        log = logging.getLogger("swift-search")
        log.info(
            "Config: tier=%s, mode=%s, semaphore=%d, max_urls=%d, quality=%s",
            self.ram_tier.value,
            self.mode.value,
            self.tier_config.semaphore_limit,
            self.tier_config.max_urls,
            self.tier_config.extraction_quality.value,
        )


# ─────────────────────────── Singleton Access ────────────────────────────

_config: Optional[SearchConfig] = None


def get_config() -> SearchConfig:
    """Get or create the global configuration singleton."""
    global _config
    if _config is None:
        _config = SearchConfig.auto_detect()
    return _config


def reset_config() -> None:
    """Reset configuration (useful for testing)."""
    global _config
    _config = None
