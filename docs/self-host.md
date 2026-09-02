# Self-hosting Pensyve

Pensyve runs as a single binary over a SQLite file. This guide deploys the MCP
gateway on [Railway](https://railway.com) with a persistent volume, and shows
how to drop in a store exported from Pensyve Cloud so your existing memories
come across intact.

**You do not need `pensyve-cloud`.** That repository is the commercial web app —
marketing site, dashboard, Stripe billing, and the API-key console. It is not
part of the memory runtime. The gateway in this repository is the whole server:
it speaks MCP, holds your memories, and answers recall. Forking `pensyve-cloud`
to self-host would give you a billing dashboard with nothing behind it.

## What you need

| | |
|---|---|
| Storage | One SQLite file. No Postgres, no external vector database. |
| Models | Embedding model (~130 MB) baked into the image at build time. |
| Memory | ~1.5 GB RAM. The ONNX embedding session is the bulk of it. |
| Disk | The store, plus room to grow. 20k memories is roughly 250 MB. |

## Deploying to Railway

### 1. Dockerfile

The `Dockerfile` in `pensyve-mcp-gateway/` expects the binary to already exist
on the host (it is built by CI, which compiles first and copies the artifact
in). Railway builds from source, so use this self-contained one instead. Save
it as `Dockerfile` at the repository root:

```dockerfile
# ---- build ----
FROM rust:1.97-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p pensyve-mcp-gateway

# ---- models ----
# Baked in at build time so the container never reaches the network to embed.
FROM rust:1.97-bookworm AS models
WORKDIR /src
COPY pensyve-mcp-gateway/models /src/pensyve-mcp-gateway/models
COPY pensyve-mcp-gateway/scripts/fetch-model-bundle.sh /src/fetch-model-bundle.sh
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && /src/fetch-model-bundle.sh --output /opt/pensyve/models

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      libssl3 ca-certificates libstdc++6 curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build  /src/target/release/pensyve-mcp-gateway /usr/local/bin/
COPY --from=models /opt/pensyve/models /opt/pensyve/models

ENV PENSYVE_PATH=/data
ENV HF_HOME=/opt/pensyve/models
ENV FASTEMBED_CACHE_DIR=/opt/pensyve/models
ENV PORT=3000
EXPOSE 3000

HEALTHCHECK --interval=10s --timeout=3s --start-period=180s --retries=6 \
    CMD curl -f http://localhost:${PORT}/health || exit 1

STOPSIGNAL SIGINT
CMD ["pensyve-mcp-gateway"]
```

The first build takes 10–15 minutes; Rust release builds are not fast. Railway
caches layers, so subsequent deploys are quicker.

### 2. `railway.json`

```json
{
  "$schema": "https://railway.com/railway.schema.json",
  "build": {
    "builder": "DOCKERFILE",
    "dockerfilePath": "Dockerfile"
  },
  "deploy": {
    "startCommand": "pensyve-mcp-gateway",
    "healthcheckPath": "/health",
    "healthcheckTimeout": 300,
    "restartPolicyType": "ON_FAILURE",
    "numReplicas": 1
  }
}
```

Keep `numReplicas` at 1. The store is a SQLite file on one volume; a second
replica would be a second process writing the same file.

`healthcheckTimeout` is generous on purpose — the first request after a cold
start loads the ONNX embedding session, which takes well over a minute.

### 3. Volume and environment

Attach a Railway volume mounted at **`/data`**, then set:

| Variable | Value | Notes |
|---|---|---|
| `PENSYVE_PATH` | `/data` | A **directory**, not a file. See below. |
| `PENSYVE_API_KEYS` | `psy_...` | Comma-separated. Any opaque string works; `psy_` is only a convention. |
| `PENSYVE_KEY_USER_MAP` | `psy_...:<user-id>` | Only when restoring an export. See [Dropping in an exported store](#dropping-in-an-exported-store). |
| `PORT` | `3000` | Railway usually injects this. |
| `MCP_ALLOWED_HOSTS` | `your-app.up.railway.app` | Without it only loopback Host headers are accepted, and every request through the Railway URL is rejected. |

> **`PENSYVE_PATH` is a directory.** The gateway opens `$PENSYVE_PATH/memories.db`
> and creates it if absent. Pointing it at a file path — `/data/pensyve.db` —
> makes the gateway create a *directory* by that name and put `memories.db`
> inside it, which is almost certainly not what you meant.

Generate a key with something like `openssl rand -hex 24`, prefixed however you
like. Requests authenticate with `Authorization: Bearer <key>`.

### 4. Connect over MCP

The gateway serves Streamable HTTP MCP at **`/mcp`** on your Railway URL.

Claude Code:

```bash
claude mcp add --transport http pensyve https://your-app.up.railway.app/mcp \
  --header "Authorization: Bearer psy_your_key"
```

Claude Desktop (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "pensyve": {
      "type": "http",
      "url": "https://your-app.up.railway.app/mcp",
      "headers": { "Authorization": "Bearer psy_your_key" }
    }
  }
}
```

Health and readiness are on `/health` and `/ready`.

## Dropping in an exported store

An export from Pensyve Cloud arrives as `<namespace-id>.db`: a complete SQLite
store at the current schema version, holding your episodes, all four memory
types (including superseded rows), entities, graph edges, and the embedding
vectors under the exact embedding space that produced them. It is not an import
format — it *is* the store. Nothing needs to be replayed or re-embedded.

Alongside it is `<namespace-id>.json`, a plain-text JSONL sidecar in the same
frame shape as a GDPR data export. It exists so your data stays readable
without a Pensyve build at hand. **The `.db` is the lossless artifact**; the
sidecar is a summary and does not carry decay state, supersession, or vectors.

Installing it takes two steps, and skipping the second is the most common way
to end up staring at an empty instance.

**1. Put the file on the volume as `memories.db`.**

```bash
# With the service stopped, from a Railway shell on the volume:
mv /data/579fbd27-....db /data/memories.db
```

The filename matters — the gateway opens `$PENSYVE_PATH/memories.db` and will
silently create an empty store next to a file named anything else.

**2. Point your API key at the namespace inside the store.**

The gateway gives each API key its own namespace, named `tenant:<user-id>`. Your
exported store contains the namespace it had on Pensyve Cloud, still under its
original name. If the key you configure resolves to any other name, the gateway
creates a *fresh, empty* namespace and serves that — your memories are sitting
in the same file, untouched and unreachable.

`PENSYVE_KEY_USER_MAP` ties the two together. It maps an API key to the user id
whose namespace you want served, as `key:user-id`, comma-separated for more than
one:

```
PENSYVE_API_KEYS=psy_your_key
PENSYVE_KEY_USER_MAP=psy_your_key:4173c1db-6da1-4505-a7a9-0b0e437e94ee
```

The user id is the part after `tenant:` in your namespace name. Read it out of
the store if you do not have it to hand:

```bash
sqlite3 /data/memories.db "SELECT id, name FROM namespaces;"
```

**If recall comes back empty, this is almost always why.** Check that the name
in that query matches `tenant:` plus the id in your `PENSYVE_KEY_USER_MAP`.

`pensyve import` is for the JSON sidecar and you do not need it here.

### Verifying before you wire up a client

The quickest check is against the file itself:

```bash
sqlite3 /data/memories.db "
  SELECT 'episodic',  count(*) FROM episodic_memories
  UNION ALL SELECT 'semantic',   count(*) FROM semantic_memories
  UNION ALL SELECT 'episodes',   count(*) FROM episodes
  UNION ALL SELECT 'entities',   count(*) FROM entities
  UNION ALL SELECT 'embeddings', count(*) FROM memory_embeddings;"
```

Confirm the embedding generation is active, using the namespace id from the
query above:

```bash
pensyve embedding-space --storage-path /data inspect --namespace <namespace-id>
```

`phase: active` with an `active_read_space_id` means semantic recall is ready.

The `pensyve` CLI's own `stats` and `recall` resolve storage from the namespace
*name* under `~/.pensyve/<name>`, so to drive them against an exported store the
directory has to be named for the namespace it contains:

```bash
mkdir -p ~/.pensyve/'tenant:4173c1db-6da1-4505-a7a9-0b0e437e94ee'
cp /data/memories.db ~/.pensyve/'tenant:4173c1db-6da1-4505-a7a9-0b0e437e94ee'/
pensyve stats  --namespace 'tenant:4173c1db-6da1-4505-a7a9-0b0e437e94ee'
pensyve recall --namespace 'tenant:4173c1db-6da1-4505-a7a9-0b0e437e94ee' "something you remember writing"
```

Passing a name the store does not contain creates an empty namespace and
reports zero, so the quoting matters.

### Embeddings

Vectors are copied under the embedding space that produced them, recorded in
the store as an immutable identity: model name, revision, artifact hashes,
dimensions, pooling, and runtime. A build whose embedder reproduces that exact
identity uses the copied vectors as they are.

Pensyve Cloud embedded with `Alibaba-NLP/gte-base-en-v1.5` at revision
`a829fd0e060bb84554da0dfd354d0de0f7712b7f`, 768 dimensions, CLS pooling,
normalized. The Dockerfile above pins the same revision through
`pensyve-mcp-gateway/models/revisions.env`, so **an export from Cloud is
directly usable and needs no migration.**

If you change the embedding model later, the identity no longer matches and
semantic recall goes unavailable rather than silently returning nonsense from
mismatched vectors. Re-embed the store before serving, with the bounded
migration the CLI exposes:

```bash
pensyve embedding-space --storage-path /data backfill \
  --namespace <namespace-id> --space-manifest <manifest.json> --max-items 256
pensyve embedding-space --storage-path /data verify   --namespace <namespace-id> --space-id <new-space-id>
pensyve embedding-space --storage-path /data activate --namespace <namespace-id> --space-id <new-space-id>
```

`backfill` is resumable and re-runs until it reports nothing left; `verify`
refuses to pass until coverage is complete, and only `activate` flips reads onto
the new generation. Lexical recall keeps working throughout.

## Operating it

**Back up the volume.** One SQLite file is easy to copy and easy to lose. Snapshot
`/data` on a schedule; `sqlite3 /data/memories.db ".backup /tmp/backup.db"` is
safe against a running gateway.

**Watch disk.** The gateway writes WAL alongside the store. Leave headroom.

**Restarts are clean.** `SIGINT` (Railway's default stop signal) lets the gateway
checkpoint and close. A `SIGKILL` mid-write is recoverable — SQLite replays the
WAL — but a clean stop is better before copying the file anywhere.

**Consolidation runs in-process** on a schedule, decaying and promoting memories
the way the hosted service did. No extra worker is needed.

## Not using Railway

Nothing here is Railway-specific. The requirements are a persistent writable
directory at `PENSYVE_PATH`, one process, and a way to serve HTTP. Fly.io, a
Hetzner box with systemd, or Docker Compose on a VPS all work the same way. For
a single-user local setup you can skip containers entirely: build the binary,
point `PENSYVE_PATH` at a directory, and run it.
