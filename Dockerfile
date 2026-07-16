# syntax=docker/dockerfile:1

# ============================================================================
# nomifun-web — headless WebUI server image (no GUI / WebView; runs anywhere)
#   Stage 1  builds the React SPA (ui/dist)
#   Stage 2  compiles the nomifun-web Rust binary
#   Stage 3  slim runtime with bun (required by the agent engine)
#
# Authentication is ON by default, but first-run setup is a claim window: the
# first reachable browser creates the admin account unless NOMIFUN_ADMIN_PASSWORD
# pre-seeds it. Bind on trusted networks only until setup is complete, and put
# TLS in front (see Caddyfile) for anything internet-facing.
# ============================================================================

# ---- Stage 1: build the SPA -------------------------------------------------
FROM oven/bun:1 AS ui
WORKDIR /app
# Install deps first for layer caching (only re-runs when manifests change).
COPY package.json bun.lock ./
COPY ui/package.json ui/package.json
RUN bun install --frozen-lockfile
COPY . .
RUN bun run build:ui
# -> /app/ui/dist

# ---- Stage 2: compile nomifun-web ------------------------------------------
FROM rust:1-bookworm AS rust
# Optional Debian mirror to speed up apt fetches (e.g. in CN). Empty = upstream
# (deb.debian.org). Pass `--build-arg APT_MIRROR=http://mirrors.aliyun.com`.
# Use an http:// mirror so the same value also works in the slim runtime stage
# below, which has no ca-certificates until its own apt-get installs them.
ARG APT_MIRROR=""
RUN if [ -n "$APT_MIRROR" ]; then \
      for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do \
        [ -f "$f" ] && sed -i -E "s|https?://deb.debian.org|$APT_MIRROR|g; s|https?://security.debian.org|$APT_MIRROR|g" "$f" || true; \
      done; \
    fi
# Native build deps: rusqlite(bundled) needs cc; rustls/aws-lc-rs needs cmake+clang
# (+ nasm for its x86_64 assembly); libgit2-sys needs cmake. If a first build fails
# on a *-sys crate, add its dep here.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake clang pkg-config perl git nasm \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src

# Optional cargo registry mirror for faster dependency fetches (e.g. in CN):
#   docker build --build-arg CARGO_REGISTRY_MIRROR=https://rsproxy.cn/index/ .
# (The repo's own .cargo/ is .dockerignore'd, so the default is crates.io.)
ARG CARGO_REGISTRY_MIRROR=""
RUN if [ -n "$CARGO_REGISTRY_MIRROR" ]; then \
      printf '[source.crates-io]\nreplace-with = "mirror"\n[source.mirror]\nregistry = "sparse+%s"\n' \
        "$CARGO_REGISTRY_MIRROR" > "${CARGO_HOME:-/usr/local/cargo}/config.toml"; \
    fi

COPY . .
# Pair the Rust host build with the exact UI artifact produced by the UI stage.
COPY --from=ui /app/ui/dist/nomifun-build.json /src/ui/dist/nomifun-build.json
# BuildKit cache mounts persist the cargo registry + compiled artifacts across
# rebuilds, so a one-line source change recompiles in seconds, not minutes. The
# binary is copied OUT of the (ephemeral) target cache mount into a real layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p nomifun-web \
    && cp target/release/nomifun-web /usr/local/bin/nomifun-web
# -> /usr/local/bin/nomifun-web

# ---- Stage 3: slim runtime --------------------------------------------------
FROM debian:bookworm-slim
# Reuse the same optional Debian mirror as stage 2 (http:// so it works before
# ca-certificates is installed just below).
ARG APT_MIRROR=""
RUN if [ -n "$APT_MIRROR" ]; then \
      for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do \
        [ -f "$f" ] && sed -i -E "s|https?://deb.debian.org|$APT_MIRROR|g; s|https?://security.debian.org|$APT_MIRROR|g" "$f" || true; \
      done; \
    fi
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates git ripgrep \
    && rm -rf /var/lib/apt/lists/*
# bun is a hard runtime dependency of the agent engine (>= 1.3.13).
COPY --from=oven/bun:1 /usr/local/bin/bun /usr/local/bin/bun
# Optional: user-configured MCP stdio servers often launch via `npx`.
# RUN apt-get update && apt-get install -y --no-install-recommends nodejs npm \
#     && rm -rf /var/lib/apt/lists/*

COPY --from=rust /usr/local/bin/nomifun-web /usr/local/bin/nomifun-web
COPY --from=ui   /app/ui/dist                    /opt/nomifun/web

ENV NOMIFUN_WEB_HOST=0.0.0.0 \
    NOMIFUN_WEB_PORT=8787 \
    NOMIFUN_DATA_DIR=/data \
    NOMIFUN_WEB_DIST=/opt/nomifun/web \
    SHELL=/bin/bash
# Set NOMIFUN_HTTPS=true when a TLS proxy fronts the app (makes cookies Secure).
# Set NOMIFUN_ADMIN_PASSWORD (+ NOMIFUN_ADMIN_USERNAME) to pre-seed the admin
# and skip the interactive first-run setup.

VOLUME /data
EXPOSE 8787
# Liveness: once the server is up it answers an unauthenticated GET / with the
# SPA (200). bun is already in the image, so no extra curl/wget is needed. The
# start-period covers first-run data-dir + DB init.
HEALTHCHECK --interval=30s --timeout=5s --start-period=40s --retries=3 \
  CMD bun -e "fetch('http://127.0.0.1:'+(process.env.NOMIFUN_WEB_PORT||'8787')+'/').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"
CMD ["nomifun-web"]
