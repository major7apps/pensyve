# ---- Builder stage ----
FROM rust:1.88-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    cmake \
    g++ \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy workspace manifests first for layer caching
COPY Cargo.toml Cargo.lock ./
COPY pensyve-core/Cargo.toml pensyve-core/Cargo.toml
COPY pensyve-mcp-tools/Cargo.toml pensyve-mcp-tools/Cargo.toml
COPY pensyve-mcp-gateway/Cargo.toml pensyve-mcp-gateway/Cargo.toml
COPY pensyve-python/Cargo.toml pensyve-python/Cargo.toml
COPY pensyve-mcp/Cargo.toml pensyve-mcp/Cargo.toml
COPY pensyve-cli/Cargo.toml pensyve-cli/Cargo.toml
COPY pensyve-benchmarks/Cargo.toml pensyve-benchmarks/Cargo.toml

# Create stub source files matching each crate's expected layout.
# This lets Cargo resolve the workspace and cache dependency compilation.

# Libraries
RUN mkdir -p pensyve-core/src && echo "" > pensyve-core/src/lib.rs \
    && mkdir -p pensyve-mcp-tools/src && echo "" > pensyve-mcp-tools/src/lib.rs \
    && mkdir -p pensyve-python/src && echo "" > pensyve-python/src/lib.rs

# Binaries
RUN mkdir -p pensyve-mcp-gateway/src && echo "fn main() {}" > pensyve-mcp-gateway/src/main.rs \
    && mkdir -p pensyve-mcp/src && echo "fn main() {}" > pensyve-mcp/src/main.rs \
    && mkdir -p pensyve-cli/src && echo "fn main() {}" > pensyve-cli/src/main.rs

# pensyve-benchmarks: lib + 4 bins
RUN mkdir -p pensyve-benchmarks/src/bin && echo "" > pensyve-benchmarks/src/lib.rs \
    && echo "fn main() {}" > pensyve-benchmarks/src/bin/monte_carlo_eval.rs \
    && echo "fn main() {}" > pensyve-benchmarks/src/bin/sensitivity_sweep.rs \
    && echo "fn main() {}" > pensyve-benchmarks/src/bin/judge_eval.rs \
    && echo "fn main() {}" > pensyve-benchmarks/src/bin/real_content_eval.rs

# pensyve-core has a bench file declared in Cargo.toml
RUN mkdir -p pensyve-core/benches && echo "fn main() {}" > pensyve-core/benches/cognitive_engine.rs

# Build dependencies only (cached unless Cargo.toml/lock change)
RUN cargo build --release -p pensyve-mcp-gateway

# Copy real source code
COPY pensyve-core/src pensyve-core/src
COPY pensyve-core/benches pensyve-core/benches
COPY pensyve-mcp-tools/src pensyve-mcp-tools/src
COPY pensyve-mcp-gateway/src pensyve-mcp-gateway/src

# Touch source files so Cargo detects they changed from stubs
RUN touch pensyve-core/src/lib.rs pensyve-mcp-tools/src/lib.rs pensyve-mcp-gateway/src/main.rs

# Full release build
RUN cargo build --release -p pensyve-mcp-gateway

# ---- Runtime stage ----
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3 \
    ca-certificates \
    libstdc++6 \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root user
RUN groupadd --gid 1001 pensyve \
    && useradd --uid 1001 --gid pensyve --shell /bin/false --create-home pensyve

# Persistent data directory
RUN mkdir -p /data/pensyve && chown pensyve:pensyve /data/pensyve

COPY --from=builder /build/target/release/pensyve-mcp-gateway /usr/local/bin/pensyve-mcp-gateway

ENV PORT=3000
ENV PENSYVE_PATH=/data/pensyve

EXPOSE 3000

HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:${PORT}/health || exit 1

USER pensyve

ENTRYPOINT ["pensyve-mcp-gateway"]
