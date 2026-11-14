# Build from the workspace root:
#   docker build -f apps/tenant-saas/Dockerfile -t tenant-saas .
#
# One image, two binaries. The API is the default entrypoint; run the worker with
#   docker run --entrypoint ./tenant-saas-worker tenant-saas

# Overrides rust-toolchain.toml so rustup does not download extra components.
ARG RUST_VERSION=1.99.1

# ── Stage 1: cargo-chef base ──────────────────────────────────────────────
FROM rust:${RUST_VERSION}-slim-bookworm AS chef
ARG RUST_VERSION
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION}
# native-tls (sqlx) links against OpenSSL.
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /app

# ── Stage 2: dependency recipe ────────────────────────────────────────────
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 3: build ────────────────────────────────────────────────────────
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Cached until dependencies change.
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bins

# ── Stage 4: runtime ──────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --shell /usr/sbin/nologin appuser
WORKDIR /app
COPY --from=builder /app/target/release/tenant-saas-api .
COPY --from=builder /app/target/release/tenant-saas-worker .
USER appuser
EXPOSE 8080
ENTRYPOINT ["./tenant-saas-api"]
