# Aether Blockchain - Multi-stage Docker Build
# Uses cargo-chef for dependency caching so code changes don't rebuild all deps.

# ---- Chef: prepare dependency recipe ----
FROM rust:1.90-slim AS chef
RUN cargo install cargo-chef --locked
WORKDIR /build

# ---- Planner: extract dependency recipe from source ----
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY ai-mesh ./ai-mesh
RUN cargo chef prepare --recipe-path recipe.json

# ---- Builder: cook deps (cached), then build source ----
FROM chef AS builder

# Install build dependencies (no libssl — project uses rustls, not openssl)
RUN apt-get update && apt-get install -y \
    pkg-config \
    clang \
    llvm-dev \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

# Cook dependencies (this layer is cached until Cargo.toml/Cargo.lock change)
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Build application (only this layer rebuilds on code changes)
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY ai-mesh ./ai-mesh
RUN cargo build --release --bin aether-node --bin genesis-ceremony

# ---- Runtime: minimal image ----
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Run as non-root for security
RUN groupadd --gid 1000 aether && \
    useradd --uid 1000 --gid aether --shell /bin/false aether

WORKDIR /app

# Copy binaries
COPY --from=builder /build/target/release/aether-node /usr/local/bin/
COPY --from=builder /build/target/release/genesis-ceremony /usr/local/bin/

# Copy config and scripts
COPY config/genesis.toml /app/config/
COPY scripts/docker-entrypoint.sh /app/scripts/
RUN chmod +x /app/scripts/docker-entrypoint.sh

# Create data directory owned by non-root user
RUN mkdir -p /app/data && chown -R aether:aether /app/data

EXPOSE 8545 9000

ENV AETHER_CONFIG_PATH=/app/config/genesis.toml
ENV AETHER_NODE_DB_PATH=/app/data

# Health check via the /health HTTP endpoint
HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=3 \
    CMD curl -sf http://localhost:8545/health || exit 1

USER aether

ENTRYPOINT ["aether-node"]
