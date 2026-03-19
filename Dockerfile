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
RUN apt-get update && apt-get install -y python3 python3-pip python3-venv && rm -rf /var/lib/apt/lists/*
RUN pip3 install --break-system-packages maturin
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY pensyve-core/ pensyve-core/
COPY pensyve-python/ pensyve-python/
COPY pensyve-mcp/ pensyve-mcp/
COPY pensyve-cli/ pensyve-cli/
RUN maturin build --release --manifest-path pensyve-python/Cargo.toml -o /wheels

# Stage 3: Runtime
FROM python:3.12-slim-bookworm
RUN useradd -m -s /bin/bash pensyve
WORKDIR /app

# Copy server code + pyproject.toml, then install deps
COPY pensyve_server/ pensyve_server/
COPY pyproject.toml .
RUN pip install --no-cache-dir .

# Install PyO3 wheel
COPY --from=python-builder /wheels/*.whl /tmp/
RUN pip install --no-cache-dir /tmp/*.whl && rm /tmp/*.whl

# Copy Rust binaries
COPY --from=rust-builder /build/target/release/pensyve-mcp /usr/local/bin/
COPY --from=rust-builder /build/target/release/pensyve-cli /usr/local/bin/

USER pensyve
EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
  CMD python -c "import urllib.request; urllib.request.urlopen('http://localhost:8000/v1/health')" || exit 1

CMD ["uvicorn", "pensyve_server.main:app", "--host", "0.0.0.0", "--port", "8000"]
