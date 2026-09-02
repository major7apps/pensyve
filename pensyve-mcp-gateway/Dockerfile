# Pensyve MCP Gateway — production image
# Build the binary on the host first: cargo build --release -p pensyve-mcp-gateway
# Then: docker build --build-arg SOURCE_SHA=$(git rev-parse HEAD) \
#   -t pensyve-gateway -f pensyve-mcp-gateway/Dockerfile .

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

FROM ubuntu:24.04

ARG SOURCE_SHA=unknown
LABEL org.opencontainers.image.revision="${SOURCE_SHA}"

RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3t64 \
    libp11-kit0 \
    ca-certificates \
    libstdc++6 \
    curl \
    postgresql-client \
    && dpkg-query -W -f='${Version}\n' libp11-kit0 \
        | xargs -I '{}' dpkg --compare-versions '{}' ge 0.25.3-4ubuntu2.2 \
    && rm -rf /var/lib/apt/lists/*

# Non-root user
RUN groupadd --gid 1001 pensyve \
    && useradd --uid 1001 --gid pensyve --shell /bin/false --create-home pensyve

# Data directory owned by non-root user (writable in Fargate ephemeral storage)
RUN mkdir -p /home/pensyve/data && chown -R pensyve:pensyve /home/pensyve

COPY target/release/pensyve-mcp-gateway /usr/local/bin/pensyve-mcp-gateway
COPY --from=model-fetch --chown=1001:1001 /opt/pensyve/models /opt/pensyve/models

ENV PORT=3000
ENV PENSYVE_PATH=/home/pensyve/data
ENV HF_HOME=/opt/pensyve/models
ENV FASTEMBED_CACHE_DIR=/opt/pensyve/models

EXPOSE 3000

HEALTHCHECK --interval=10s --timeout=3s --start-period=180s --retries=6 \
    CMD curl -f http://localhost:${PORT}/health || exit 1

STOPSIGNAL SIGINT

COPY pensyve-mcp-gateway/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh
USER 1001:1001
ENTRYPOINT ["entrypoint.sh"]
