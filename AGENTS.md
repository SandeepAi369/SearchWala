# AGENTS.md — SearchWala Contributor & AI Agent Guide

This file is the canonical reference for humans and AI coding agents working on `SandeepAi369/SearchWala`.

## Zero-Memory Release Process

Releasing a new version requires **zero manual git tags, zero manual `cargo publish`, and zero manual `mcp-publisher` runs**.

1. Bump `[package].version` in `Cargo.toml` (e.g., `8.3.1` -> `8.3.2`).
2. Keep `server.json` (`version` and `packages[].version`) and `Cargo.lock` in sync when convenient (CI auto-syncs both as a safety net if forgotten).
3. Run local verification before committing:
   ```bash
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```
4. Merge or push to `main`.

`.github/workflows/auto-publish.yml` triggers automatically on `Cargo.toml` changes on `main`:
- If tag `v<version>` already exists on `main`, it exits cleanly as a no-op (no duplicate releases).
- If `v<version>` is new, it automatically:
  1. Auto-syncs `server.json` and workspace `Cargo.lock` if needed.
  2. Runs the release quality gate (`cargo fmt --check`, `cargo clippy --release --all-targets -- -D warnings`, `cargo test --release`).
  3. Builds `searchwala-linux-amd64` + `checksums-sha256.txt`.
  4. Publishes `searchwala` to **crates.io** using the `CARGO_REGISTRY_TOKEN` GitHub Actions secret (idempotent).
  5. Creates the clean `v<version>` git tag and **GitHub Release** with auto-generated notes and binary assets.
  6. Publishes `io.github.SandeepAi369/searchwala` to the **Official MCP Registry** via GitHub Actions OIDC (`mcp-publisher login github-oidc`, idempotent).

If a downstream registry has a transient outage, re-run **Auto Publish Release** via `workflow_dispatch` — every step is idempotent and only completes unfinished targets.

## Protected `README.md` Metadata (Do Not Remove)

- **Official MCP Registry ownership marker:** `mcp-name: io.github.SandeepAi369/searchwala` (required for crates.io ownership verification by `registry.modelcontextprotocol.io`).
- **Glama MCP directory badge & backlink:** `https://glama.ai/mcp/servers/SandeepAi369/SearchWala` and its `/badges/score.svg` badge (required backlink for `punkpeye/awesome-mcp-servers`).

## Security & CI Guardrails

- **Secrets:** Never print, echo, log, or commit tokens or credentials (e.g., `~/.cargo/credentials.toml`, `CARGO_REGISTRY_TOKEN`, `GH_TOKEN`, or LLM API keys).
- **GitHub Actions (OpenSSF Scorecard & CodeQL `actions`):**
  - Pin every `uses:` action to a full 40-char commit SHA with a `# vX.Y.Z` comment.
  - Keep top-level `permissions: contents: read`; grant write/`id-token` permissions only on the specific job that needs them.
  - Use `persist-credentials: false` on `actions/checkout`.
  - Never interpolate `${{ ... }}` directly inside `run:` shell commands; pass values through `env:`.
  - Verify downloaded release binaries (like `mcp-publisher`) with a pinned SHA-256 checksum.
- **Docker:** Keep `Dockerfile` base images pinned by exact `@sha256:...` digest.
- **Runtime security:** Preserve SSRF enforcement in `src/ssrf.rs` (`scrape_url_allowed`, `PublicOnlyResolver`, per-hop redirect validation) and typed Axum `Json<T>` extraction + `DefaultBodyLimit` in `src/mcp/http.rs`.

## Repository Quick Map

- `src/main.rs` — CLI entrypoint (`serve`, `mcp`, `mcp --transport http`), Axum web server, `/api/*` routes
- `src/mcp/` — Native zero-dependency MCP server (`stdio.rs`, `http.rs`, `tools.rs`, `protocol.rs`)
- `src/search.rs` — Multi-engine fan-out, consensus ranking, domain controls, page fetch/extract pipeline
- `src/engines/` — 19 web, news, and scholarly search engine adapters
- `src/extractor.rs` & `src/deep_reader.rs` — `dom_smoothie` + `rs-trafilatura` extraction, JSON-LD/meta dates, SPA `<noscript>` hydration reader
- `src/llm.rs` & `src/synthesis.rs` — Optional BYOK LLM synthesis, intent prompts, extractive fallback
- `src/ssrf.rs` — SSRF & DNS-rebinding defense
- `ui.html` — Embedded single-file 3-column web UI
- `server.json` — Official MCP Registry manifest (`io.github.SandeepAi369/searchwala`)
