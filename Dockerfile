# ══════════════════════════════════════════════════════════════════════════════
# SearchWala v8.0 — multi-stage build
# ══════════════════════════════════════════════════════════════════════════════

# ── Stage 1: build ────────────────────────────────────────────────────────────
# v8.0: bumped from rust:1.82. The dependency set (reqwest 0.13, axum 0.8,
# rand 0.10, LazyLock, is_none_or) needs a current toolchain.
# Builder and runtime must be the same Debian release: a binary linked
# against trixie's glibc 2.41 exits at once on bookworm (glibc 2.36). The
# untagged rust:1-slim moved to trixie and broke exactly that.
FROM rust:1-slim-trixie AS builder

WORKDIR /app

# wreq/BoringSSL build dependencies. btls-sys runs `git init` / `git apply`
# on its vendored BoringSSL to patch it (locally, no network); without git
# the build fails with "can't run git".
RUN apt-get update && apt-get install -y --no-install-recommends \
      cmake perl libclang-dev pkg-config build-essential git \
    && rm -rf /var/lib/apt/lists/*

# Dependency layer: cached unless the manifests change.
COPY Cargo.toml Cargo.lock ./
COPY patches/ patches/
RUN mkdir -p src src/bin \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && echo 'fn main() {}' > src/bin/eval.rs \
    && echo 'fn main() {}' > src/bin/intent_probe.rs \
    && cargo build --release --bin searchwala \
    && rm -rf src

COPY src/ src/
COPY ui.html ./
COPY tests/ tests/
COPY benchmarks/ benchmarks/
RUN touch src/main.rs src/lib.rs && cargo build --release --bin searchwala

# ── Stage 2: runtime ──────────────────────────────────────────────────────────
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# v8.0: run as a non-root user.
RUN useradd --system --create-home --shell /usr/sbin/nologin searchwala
WORKDIR /app
COPY --from=builder /app/target/release/searchwala /app/searchwala
RUN chown -R searchwala:searchwala /app
USER searchwala

# ── Defaults ──
# v7.0's Dockerfile set ENGINES to five names and MAX_URLS=15, silently
# overriding the documented defaults and contradicting its own "90+ engines"
# claim. v8.0 ships the real defaults and lets the roster come from code.
ENV PORT=7860
ENV BIND=0.0.0.0
ENV RUST_LOG=searchwala=info
# Container networking means localhost is the container itself; a local model
# runs elsewhere, so this stays off by default.
ENV ALLOW_LOCAL_LLM=0

EXPOSE 7860

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -fsS "http://127.0.0.1:${PORT}/health" > /dev/null || exit 1

CMD ["/app/searchwala"]
