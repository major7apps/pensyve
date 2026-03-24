# Stage 1: Build Rust binaries
# Trixie (Debian 13) required — ort-sys 2.0.0-rc.11 prebuilt ONNX Runtime
# links against glibc 2.38+ (__isoc23_strtoull) and GCC 12+ (__cxa_call_terminate)
FROM rust:trixie AS rust-builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY pensyve-core/ pensyve-core/
COPY pensyve-python/ pensyve-python/
COPY pensyve-mcp/ pensyve-mcp/
COPY pensyve-cli/ pensyve-cli/
RUN cargo build --release -p pensyve-mcp -p pensyve-cli

# Stage 2: Build Python wheel via maturin
FROM rust:trixie AS python-builder
COPY --from=ghcr.io/astral-sh/uv:latest /uv /uvx /bin/
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-venv && rm -rf /var/lib/apt/lists/*
ENV UV_LINK_MODE=copy
WORKDIR /build
RUN uv venv /opt/venv
RUN --mount=type=cache,target=/root/.cache/uv \
    uv pip install --python /opt/venv/bin/python "maturin[patchelf]"
COPY Cargo.toml Cargo.lock ./
COPY pensyve-core/ pensyve-core/
COPY pensyve-python/ pensyve-python/
COPY pensyve-mcp/ pensyve-mcp/
COPY pensyve-cli/ pensyve-cli/
ENV PATH="/opt/venv/bin:$PATH"
RUN maturin build --release --manifest-path pensyve-python/Cargo.toml -o /wheels

# Stage 3: Pre-download ONNX embedding models for the Rust fastembed crate.
# Uses huggingface_hub (same cache format as hf_hub Rust crate) to avoid
# model downloads on first ECS task start.
FROM python:3.13-slim-trixie AS model-download
RUN pip install --no-cache-dir huggingface_hub
RUN python -c "from huggingface_hub import snapshot_download; snapshot_download('Alibaba-NLP/gte-base-en-v1.5', cache_dir='/model-cache', allow_patterns=['onnx/*', 'tokenizer*', 'config*', 'special_tokens*'])"
RUN python -c "from huggingface_hub import snapshot_download; snapshot_download('Qdrant/all-MiniLM-L6-v2-onnx', cache_dir='/model-cache', allow_patterns=['*.onnx', 'tokenizer*', 'config*', 'special_tokens*'])"

# Stage 4: Runtime
FROM python:3.13-slim-trixie
COPY --from=ghcr.io/astral-sh/uv:latest /uv /uvx /bin/

ENV UV_COMPILE_BYTECODE=1
ENV UV_LINK_MODE=copy

WORKDIR /app

# Install dependencies first (cached layer — only changes when pyproject.toml/uv.lock change)
RUN --mount=type=cache,target=/root/.cache/uv \
    --mount=type=bind,source=uv.lock,target=uv.lock \
    --mount=type=bind,source=pyproject.toml,target=pyproject.toml \
    uv sync --frozen --no-install-project --no-dev

# Copy source and install the project itself
COPY pyproject.toml uv.lock ./
COPY pensyve_server/ pensyve_server/
RUN --mount=type=cache,target=/root/.cache/uv \
    uv sync --frozen --no-dev --no-editable

# Install PyO3 wheel into the uv-managed venv
COPY --from=python-builder /wheels/*.whl /tmp/
RUN uv pip install /tmp/*.whl && rm /tmp/*.whl

# Copy Rust binaries
COPY --from=rust-builder /build/target/release/pensyve-mcp /usr/local/bin/
COPY --from=rust-builder /build/target/release/pensyve /usr/local/bin/pensyve-cli

# Non-root user
RUN useradd -m -s /bin/bash pensyve

# Copy pre-downloaded embedding models (hf_hub cache format)
COPY --from=model-download --chown=pensyve:pensyve /model-cache /home/pensyve/.cache/fastembed

USER pensyve
ENV FASTEMBED_CACHE_DIR=/home/pensyve/.cache/fastembed
EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
  CMD ["/app/.venv/bin/python", "-c", "import urllib.request; urllib.request.urlopen('http://localhost:8000/v1/health')"]

# Use venv directly instead of uv run to avoid overhead
ENV PATH="/app/.venv/bin:$PATH"
ENV PENSYVE_AUTH_MODE=required
CMD ["uvicorn", "pensyve_server.main:app", "--host", "0.0.0.0", "--port", "8000"]
