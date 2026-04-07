# ══════════════════════════════════════════════════════════════════════════════
# Swift Search Agent v3.0 — Multi-stage Docker Build
# Final image: ~15MB, runs on 512MB VPS or HF Spaces
# ══════════════════════════════════════════════════════════════════════════════

# ── Stage 1: Build ────────────────────────────────────────────────────────────
FROM rust:1.94-slim AS builder

WORKDIR /app

# Copy manifests first for layer caching
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src

# Copy source and rebuild (only our code recompiles)
COPY src/ src/
RUN touch src/main.rs && cargo build --release

# ── Stage 2: Runtime ─────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# Install CA certificates for HTTPS
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binary from builder
COPY --from=builder /app/target/release/swift-search-rs /app/swift-search-rs

# Default environment
ENV PORT=7860
ENV RUST_LOG=swift_search_rs=info
ENV ENGINES=duckduckgo,brave,yahoo,qwant,mojeek
ENV MAX_URLS=15
ENV CONCURRENCY=8
ENV SCRAPE_TIMEOUT=10

EXPOSE 7860

CMD ["/app/swift-search-rs"]
