# Stage 1: Build Rust binaries
FROM rust:bookworm AS rust-builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY pensyve-core/ pensyve-core/
COPY pensyve-python/ pensyve-python/
COPY pensyve-mcp/ pensyve-mcp/
COPY pensyve-cli/ pensyve-cli/
RUN cargo build --release -p pensyve-mcp -p pensyve-cli

# Stage 2: Build Python wheel
FROM rust:bookworm AS python-builder
COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv
RUN apt-get update && apt-get install -y python3 python3-venv && rm -rf /var/lib/apt/lists/*
WORKDIR /build
# Create a temporary venv for maturin
RUN uv venv /opt/venv && /opt/venv/bin/pip install maturin
COPY Cargo.toml Cargo.lock ./
COPY pensyve-core/ pensyve-core/
COPY pensyve-python/ pensyve-python/
COPY pensyve-mcp/ pensyve-mcp/
COPY pensyve-cli/ pensyve-cli/
RUN /opt/venv/bin/maturin build --release --manifest-path pensyve-python/Cargo.toml -o /wheels

# Stage 3: Runtime
FROM python:3.12-slim-bookworm
COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv
RUN useradd -m -s /bin/bash pensyve
WORKDIR /app

# Install deps via uv sync (reads pyproject.toml + uv.lock)
COPY pyproject.toml uv.lock ./
COPY pensyve_server/ pensyve_server/
RUN uv sync --frozen --no-dev --no-editable

# Install PyO3 wheel into the uv-managed venv
COPY --from=python-builder /wheels/*.whl /tmp/
RUN uv pip install /tmp/*.whl && rm /tmp/*.whl

# Copy Rust binaries
COPY --from=rust-builder /build/target/release/pensyve-mcp /usr/local/bin/
COPY --from=rust-builder /build/target/release/pensyve-cli /usr/local/bin/

USER pensyve
EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
  CMD python -c "import urllib.request; urllib.request.urlopen('http://localhost:8000/v1/health')" || exit 1

CMD ["uv", "run", "uvicorn", "pensyve_server.main:app", "--host", "0.0.0.0", "--port", "8000"]
