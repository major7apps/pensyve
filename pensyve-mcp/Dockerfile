# Pensyve MCP server (stdio) — image for the MCP registry and Smithery.
# Build from the repo root:
#   docker build -f pensyve-mcp/Dockerfile -t pensyve-mcp .
# Run as an MCP stdio server (memories persist in the named volume):
#   docker run -i --rm -v pensyve-data:/home/pensyve/data pensyve-mcp

# trixie ships GCC 14, which the prebuilt static ONNX Runtime (ort) links against.
FROM rust:1-trixie AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY pensyve-core pensyve-core
COPY pensyve-python pensyve-python
COPY pensyve-mcp pensyve-mcp
COPY pensyve-mcp-tools pensyve-mcp-tools
COPY pensyve-mcp-gateway pensyve-mcp-gateway
COPY pensyve-cli pensyve-cli
COPY pensyve-benchmarks pensyve-benchmarks
RUN cargo build --release --locked -p pensyve-mcp

FROM ubuntu:24.04 AS model-fetch

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    coreutils \
    curl \
    findutils \
    grep \
    && rm -rf /var/lib/apt/lists/*

COPY pensyve-mcp-gateway/models /bundle/models
COPY pensyve-mcp-gateway/scripts/fetch-model-bundle.sh /bundle/scripts/fetch-model-bundle.sh
RUN /bundle/scripts/fetch-model-bundle.sh --output /opt/pensyve/models

FROM debian:trixie-slim

ARG SOURCE_SHA=unknown
LABEL org.opencontainers.image.revision="${SOURCE_SHA}"
LABEL org.opencontainers.image.source="https://github.com/major7apps/pensyve"
LABEL io.modelcontextprotocol.server.name="io.github.major7apps/pensyve"

RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3t64 \
    ca-certificates \
    libstdc++6 \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --gid 1001 pensyve \
    && useradd --uid 1001 --gid pensyve --shell /bin/false --create-home pensyve \
    && mkdir -p /home/pensyve/data \
    && chown -R pensyve:pensyve /home/pensyve

COPY --from=build /src/target/release/pensyve-mcp /usr/local/bin/pensyve-mcp
COPY --from=model-fetch --chown=1001:1001 /opt/pensyve/models /opt/pensyve/models

ENV PENSYVE_PATH=/home/pensyve/data
ENV HF_HOME=/opt/pensyve/models
ENV FASTEMBED_CACHE_DIR=/opt/pensyve/models

USER 1001:1001
ENTRYPOINT ["pensyve-mcp"]
