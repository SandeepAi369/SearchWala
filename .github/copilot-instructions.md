# GitHub Copilot Instructions — SearchWala

Follow all guidelines in [`AGENTS.md`](../AGENTS.md). Key rules:

- **Releases are fully automatic:** Bumping `[package].version` in `Cargo.toml` and pushing/merging to `main` automatically runs tests, syncs `server.json`, builds the Linux binary, publishes to `crates.io`, cuts the `vX.Y.Z` GitHub Release, and updates the Official MCP Registry via `.github/workflows/auto-publish.yml`. Do not create manual `v*` tags or run `cargo publish` manually.
- **Protected `README.md` lines:** Never remove `mcp-name: io.github.SandeepAi369/searchwala` or the Glama badge/backlink (`https://glama.ai/mcp/servers/SandeepAi369/SearchWala`).
- **Security & CI:** Pin GitHub Actions to 40-char commit SHAs, pass workflow inputs via `env:` (never `${{ ... }}` in `run:`), keep `Dockerfile` images pinned by `@sha256:...`, preserve SSRF checks in `src/ssrf.rs`, and never print/log/commit secrets.
- **Verify changes:** Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`.
