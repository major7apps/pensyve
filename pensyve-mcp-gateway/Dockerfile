# Pensyve MCP Gateway — production image
# Build the binary on the host first: cargo build --release -p pensyve-mcp-gateway
# Then: docker build -t pensyve-gateway -f pensyve-mcp-gateway/Dockerfile .

FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3t64 \
    ca-certificates \
    libstdc++6 \
    curl \
    postgresql-client \
    && rm -rf /var/lib/apt/lists/*

# Non-root user
RUN groupadd --gid 1001 pensyve \
    && useradd --uid 1001 --gid pensyve --shell /bin/false --create-home pensyve

# Data directory owned by non-root user (writable in Fargate ephemeral storage)
RUN mkdir -p /home/pensyve/data && chown -R pensyve:pensyve /home/pensyve

COPY target/release/pensyve-mcp-gateway /usr/local/bin/pensyve-mcp-gateway

ENV PORT=3000
ENV PENSYVE_PATH=/home/pensyve/data

EXPOSE 3000

HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:${PORT}/health || exit 1

COPY pensyve-mcp-gateway/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh
ENTRYPOINT ["entrypoint.sh"]
