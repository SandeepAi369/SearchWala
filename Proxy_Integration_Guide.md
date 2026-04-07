# Proxy Integration Guide: Unlocking Premium Search Engines

This guide explains how you can manually modify the `Swift-Search-Agent` codebase to unlock premium search engines (Google Search and Microsoft Bing) by integrating your own proxies and enabling IP rotation.

> **Note:** The default codebase is heavily optimized for zero-configuration deployments (like Hugging Face free tier) to use robust fallback meta-search engines. Use this guide only if you have acquired your own datacenter or residential proxies.

---

## 1. Environment Variable Configuration

First, define your proxy list so the script can distribute traffic across them. 
In your deployment environment or `.env` file, add a `PROXIES` variable containing a comma-separated list of your HTTP/HTTPS proxies:

```env
PROXIES=http://user:pass@ip1:port,http://user:pass@ip2:port,http://user:pass@ip3:port
```

## 2. Implement the Proxy Loader

In your engine file (e.g., `search_unified.py`, `search_optimized.py`, or `search_ultra.py`), add a reliable mechanism to parse the `PROXIES` list at startup:

```python
import os
import random

# Load proxy list from environment variables
PROXY_LIST = []
raw_proxies = os.environ.get("PROXIES", "")
if raw_proxies:
    PROXY_LIST = [p.strip() for p in raw_proxies.split(",") if p.strip()]

def get_random_proxy() -> str | None:
    """Return a random proxy from the list, or None if no proxies are configured."""
    return random.choice(PROXY_LIST) if PROXY_LIST else None
```

## 3. Override Meta Search (Phase 1)

Since public SearxNG instances aggressively rate-limit direct Google/Bing queries, the most effective way to unlock premium search is to scrape them directly from the python code using your proxies. 

Add the following helper functions to query Google and extract links natively:

```python
import re
import httpx

async def direct_premium_search(query: str) -> list[str]:
    """Search Google directly using a random proxy."""
    if not PROXY_LIST:
        return []
        
    proxy = get_random_proxy()
    transport = httpx.AsyncHTTPTransport(proxy=proxy)
    urls = []
    
    # 1. Query Google
    try:
        async with httpx.AsyncClient(transport=transport, timeout=10.0) as client:
            resp = await client.get(
                "https://www.google.com/search",
                params={"q": query},
                headers={"User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64)"}
            )
            if resp.status_code == 200:
                # Basic Regex extraction for Google search results
                found = re.findall(r'href="(https?://[^"]+)"', resp.text)
                for link in found:
                    if "google.com" not in link and "youtube.com" not in link:
                        urls.append(link)
    except Exception as e:
        print(f"Direct Premium Search error: {e}")
        
    # Note: You can add Bing logic here similarly using https://www.bing.com/search?q={query}
    return urls
```

Next, update your `meta_search` function to seamlessly fallback to SearxNG if no proxies are provided (or if the proxy direct scrape fails):

```python
async def meta_search(query: str) -> list[str]:
    # Attempt Premium Proxy Search First
    urls = await direct_premium_search(query)
    
    # Smooth Fallback: If no proxies were configured or extraction failed, use default SearxNG
    if not urls:
        print("Falling back to default SearxNG meta-search...")
        urls = await default_searxng_search(query) # Your existing SearxNG logic
        
    return urls
```

## 4. Enable IP Rotation for Target Scraping (Phase 2)

To avoid getting IP-banned by the targets you are trying to read, distribute the `trafilatura` HTTP scrapes across your proxy list. 

Update your `_scrape_single_url` function to utilize the proxies:

```python
async def _scrape_single_url(client: httpx.AsyncClient, url: str) -> tuple[str, str]:
    sem = _get_semaphore()
    
    # Pick a random proxy for this specific scrape request
    proxy = get_random_proxy()
    transport = httpx.AsyncHTTPTransport(proxy=proxy) if proxy else None
    
    async with sem:
        try:
            # Overriding the client's transport for IP rotation
            async with httpx.AsyncClient(transport=transport, follow_redirects=True) as proxy_client:
                resp = await proxy_client.get(
                    url,
                    headers=_HEADERS,
                    timeout=SCRAPE_TIMEOUT_SEC
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
```

## Summary

By making these changes, your deployment will:
1. Identify when users have configured a `PROXIES` environment variable.
2. Route premium Google/Bing searches directly through those proxies.
3. Automatically rotate IPs for every individual scraped URL.
4. Fall back seamlessly to the default Hugging Face configuration out-of-the-box if the user leaves `PROXIES` blank.
