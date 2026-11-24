# SearchWala MCP Server

SearchWala includes a built-in, zero-dependency **Model Context Protocol (MCP)** server in the same single Rust binary. Any MCP-compatible AI agent or IDE can plug SearchWala in with one command—**no LLM API key, no search API key, and no separate daemon required.**

Because your connected AI agent handles reasoning and synthesis, SearchWala focuses exclusively on what it does best: parallel multi-engine consensus search, BoringSSL stealth fetching, deep article extraction (`dom_smoothie` + `rs-trafilatura` + Deep Reader + AMP), and BM25+/MMR passage ranking.

---

## Transports

### 1. Local `stdio` (Recommended for desktop & IDE agents)

```bash
./target/release/searchwala mcp
```

- Speaks newline-delimited JSON-RPC 2.0 on `stdin`/`stdout` (MCP `2025-11-25`, `2025-06-18`, `2025-03-26`, `2024-11-05`).
- Zero startup or runtime noise on `stdout` or `stderr` (logs route exclusively to `stderr` at `error` level by default; adjust with `--log-level warn|info|debug` or `RUST_LOG`).
- Does not open `~/.searchwala/searchwala.db`, so multiple agents can spawn parallel `searchwala mcp` processes with zero SQLite lock contention.
- Keeps the in-memory SERP (10 min TTL) and page (6 h TTL) caches warm for the session lifetime.

### 2. Streamable HTTP (`/mcp`)

The standard `searchwala` HTTP server automatically mounts Streamable HTTP MCP at `http://127.0.0.1:8000/mcp` (disable with `searchwala serve --no-mcp`).

For a headless MCP-only HTTP server (no Web UI, no `/api/chats`, no `/api/tts`):

```bash
./target/release/searchwala mcp --http --port 8000 --bind 127.0.0.1
# or: ./target/release/searchwala mcp --transport http --port 8000
```

- `POST /mcp` — JSON-RPC 2.0 responses (`application/json`), or SSE progress streams (`text/event-stream`) when the client sends `Accept: text/event-stream` and `_meta.progressToken`. Returns `202 Accepted` for notifications (`notifications/initialized`, `notifications/cancelled`).
- `DELETE /mcp` — Terminates an active `Mcp-Session-Id` (`204 No Content`).
- `GET /mcp` — Returns `405 Method Not Allowed` (`Allow: POST, DELETE`).
- Supports both stateful (`Mcp-Session-Id`) and stateless clients.

---

## Client Configuration Examples

Replace `/path/to/searchwala` with the absolute path to your compiled binary (or `searchwala` if it is on `PATH`).

### Claude Desktop (`claude_desktop_config.json`)
```json
{
  "mcpServers": {
    "searchwala": {
      "command": "/path/to/searchwala",
      "args": ["mcp"]
    }
  }
}
```

### Claude Code (CLI)
```bash
claude mcp add --transport stdio searchwala -- /path/to/searchwala mcp
```

### Cursor (`.cursor/mcp.json`) / Windsurf / Cline / Antigravity
```json
{
  "mcpServers": {
    "searchwala": {
      "command": "/path/to/searchwala",
      "args": ["mcp"]
    }
  }
}
```

### VS Code Copilot (`.vscode/mcp.json`)
```json
{
  "servers": {
    "searchwala": {
      "type": "stdio",
      "command": "/path/to/searchwala",
      "args": ["mcp"]
    }
  }
}
```

### Gemini CLI (`~/.gemini/settings.json`)
```json
{
  "mcpServers": {
    "searchwala": {
      "command": "/path/to/searchwala",
      "args": ["mcp"]
    }
  }
}
```

### OpenAI Codex CLI (`~/.codex/config.toml`)
```toml
[mcp_servers.searchwala]
command = "/path/to/searchwala"
args = ["mcp"]
```

### Shared / Persistent Server over Streamable HTTP
```json
{
  "mcpServers": {
    "searchwala": {
      "url": "http://127.0.0.1:8000/mcp"
    }
  }
}
```

### Docker (`stdio` without a local Rust build)
```json
{
  "mcpServers": {
    "searchwala": {
      "command": "docker",
      "args": ["run", "-i", "--rm", "searchwala", "/app/searchwala", "mcp"]
    }
  }
}
```

---

## Tools Reference

All tools are **read-only** (`readOnlyHint: true`, `destructiveHint: false`), declare JSON Schema 2020-12 `inputSchema` and `outputSchema`, and return both concise Markdown (`content`) and typed JSON (`structuredContent`).

### 1. `search_web`
Fast multi-engine consensus discovery without full-page scraping (~1–3 s).

| Parameter | Type | Default | Notes |
|---|---|---|---|
| `query` | `string` (required) | — | Search query in any language |
| `mode` | `fast \| auto \| deep \| study \| academic \| reddit \| youtube` | `auto` | Controls engine roster, pagination, and sub-queries |
| `domain` | `general \| tech \| science \| finance \| health \| news` | `general` | Composable subject-area focus |
| `max_results` | `integer` (`1..=50`) | `15` | Maximum consensus-ranked hits returned |
| `include_domains` | `string[]` (max 10) | `[]` | Domains to query and prioritize (e.g. `["docs.rs", "github.com"]`) |
| `exclude_domains` | `string[]` (max 20) | `[]` | Domains to drop before triage and ranking |
| `strict_domains` | `boolean` | `false` | `false`: keeps SearchWala's organic consensus answers while prioritizing `include_domains`. `true`: restricts results strictly to `include_domains`. |

Returns consensus-ranked `results` (with `found_by_families`, `rrf_score`, `engines`, `snippet`, and `abstract_text` for OpenAlex/arXiv hits) plus `discovered_domains` and `total_discovered_domains`.

### 2. `search_passages`
SearchWala's flagship end-to-end grounding pipeline: consensus search → concurrent deep page extraction → BM25+ / RRF / authority / freshness / MMR passage ranking → deterministic verbatim quick-answer `highlight`.

| Parameter | Type | Default | Notes |
|---|---|---|---|
| `query` | `string` (required) | — | Question or research query |
| `mode` | `fast \| auto \| deep \| study \| academic \| reddit \| youtube` | `auto` | Depth profile |
| `domain` | `general \| tech \| science \| finance \| health \| news` | `general` | Subject-area authority overlay |
| `top_k` | `integer` (`1..=50`) | `8` | Max BM25+/MMR passages returned |
| `max_urls` | `integer` (`3..=150`) | per mode | Optional scrape-pool override (clamped to mode bounds) |
| `max_total_chars` | `integer` (`2000..=120000`) | `12000` | UTF-8 safe total character budget across returned passages |
| `include_domains` | `string[]` (max 10) | `[]` | Domains to query and prioritize |
| `exclude_domains` | `string[]` (max 20) | `[]` | Domains to exclude before scraping |
| `strict_domains` | `boolean` | `false` | `false`: organic consensus + `include_domains` prioritized; `true`: strictly scrape and return passages from `include_domains` |

Emits live `notifications/progress` updates when `_meta.progressToken` is supplied.

### 3. `extract_url`
SSRF-guarded deep content extraction for a specific public `http://` or `https://` URL (Readability + conditional Trafilatura + JSON-LD `articleBody` + Declarative Shadow DOM + Next.js/Nuxt/Remix/Apollo/Astro hydration + `old.reddit.com` rewrite + YouTube fast path + AMP fallback + `text/plain` & `text/markdown` support + 6-hour page cache).

| Parameter | Type | Default | Notes |
|---|---|---|---|
| `url` | `string` (required) | — | Public HTTP(S) URL |
| `max_chars` | `integer` (`500..=100000`) | `12000` | UTF-8 safe character cap (`truncated` flag set when clipped) |
| `query` | `string` | unset | Optional focus query — runs BM25+/MMR ranking on this page and returns the top matching passages |
| `top_k` | `integer` (`1..=20`) | `8` | Max passages when `query` is provided |
| `allow_amp_fallback` | `boolean` | `true` | Fall back to same-site AMP page if canonical DOM is thin |
| `bypass_cache` | `boolean` | `false` | Force a fresh upstream fetch |

### 4. `search_status`
No required arguments. Returns SearchWala version, supported modes and domains, per-engine health verdicts (`healthy`, `degraded`, `selector_rot`, `blocked`, `dead_host`, `unproven`), and SERP/page cache hit statistics. (Static mode budgets are exposed via the `searchwala://config/modes` resource.)

---

## Domain Control & Token-Efficient Drill-Down

Every `search_web` and `search_passages` response summarizes the full post-exclusion domain pool (before `max_urls` truncation) into:
- A zero-noise domain list at the bottom of `content[0].text` (one brief header line + clean comma-separated domain names).
- A structured `discovered_domains` array in `structuredContent` (`domain`, `result_count`, `max_families`, `top_rrf_score`, `top_url`, `top_title`).

Agents can either:
1. Answer immediately from SearchWala's consensus results/passages, or
2. Drill down into specific discovered domains using `search_passages(query=..., include_domains=["docs.rs"], strict_domains=true)` or `extract_url(url=..., query=...)`.

---

## Read-Only MCP Resources

| URI | MIME Type | Contents |
|---|---|---|
| `searchwala://config/modes` | `application/json` | All 7 search modes and their default budgets |
| `searchwala://health/engines` | `application/json` | Live per-engine metrics and health verdicts |
| `searchwala://health/cache` | `application/json` | SERP and page cache entry counts and hit ratios |

---

## Environment Variables & Security

- `MCP_STRICT_DOMAINS=1` — Changes the default for `strict_domains` from `false` to `true` when omitted by the caller.
- Standard SearchWala search/cache variables (`SEARXNG_BASE_URL`, `PROXY_POOL`, `OPENALEX_MAILTO`, `CACHE`, `STEALTH_MODE`, etc.) work identically in MCP mode.
- **SSRF protection:** `extract_url` enforces `ssrf::scrape_url_allowed`, `PublicOnlyResolver` (blocking DNS rebinding), and per-hop redirect validation unconditionally—loopback, private ranges, and cloud metadata endpoints (`169.254.169.254`, etc.) are always blocked.
- **Localhost HTTP guard:** When `/mcp` is bound to loopback (`127.0.0.1`, `localhost`, `::1`), non-loopback `Host` headers and cross-origin browser `Origin` headers receive `403 Forbidden`.
