# Hushai workspace

A Cargo workspace for the Hushai data-intake + retrieval system.

| crate | role |
|-------|------|
| [`hushai-backend`](hushai-backend/) | Durable, idempotent **segment-ingest** server (camera→backend contract v0.1.0). Owns the DB schema. |
| [`hushai-worker`](hushai-worker/) | Durable, resumable, idempotent **transcription + embedding** worker: drains stored segments → `transcript_sentences`. |
| [`hushai-rag`](hushai-rag/) | **RAG** service: `POST /v1/rag/query` — grounded Q&A over the transcripts with source citations. |
| [`hushai-viewer`](hushai-viewer/) | The unified **browser app** (NVR timeline + chat-over-recordings, `127.0.0.1:8070`); reuses `hushai-backend` as a library. Includes a **🗄 Files** page for device & footage management — rename, per-date storage usage, retention, delete, export ([`docs/device-and-footage-management.md`](docs/device-and-footage-management.md)). |

The worker and RAG service are the **transcription-embedding-and-rag** ticket
(`Issues/transcription-embedding-and-rag.md`). Both reuse `hushai-backend` as a library
(DB `Config` + pool) and are **fully local / privacy-first**: ASR via whisper.cpp, embeddings +
LLM via a local Ollama server — captured media never leaves the machine.

## Prerequisites

```bash
# Postgres + pgvector (the backend's schema must be migrated)
createdb hushai            # or use the existing DB
export DATABASE_URL=postgres://localhost/hushai

# ffmpeg (audio extraction)
brew install ffmpeg

# Local models
brew install ollama && ollama serve &
ollama pull mxbai-embed-large    # embeddings, 1024-dim (hard requirement)
ollama pull llama3.2:3b          # RAG answer LLM (config-driven)

# whisper.cpp GGML model (local ASR)
mkdir -p models
curl -L -o models/ggml-base.en.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin
```

Configuration is env-driven (`.env` at the workspace root; see each crate's `.env.example`).

## Run

One command brings up all four services (backend, worker, rag, viewer) and tears the whole
thing down with a single Ctrl-C:

```bash
./local_dev/run_stack.sh
```

It preflights the infra deps (Postgres, Ollama), builds the workspace, launches every service,
health-checks the ports, and prints a URL map. See [`AGENTS.md`](AGENTS.md) "Run the full stack locally"
for flags (`--with-android`, `--no-build`, `--release`, `--pull`, `--tls`, `--add-camera`, `--down`)
and the manual per-terminal flow with its gotchas.

Run it on a shared network with `--tls` (HTTPS + admin IP-allowlist/password + per-device camera
tokens — see [`AGENTS.md`](AGENTS.md) "LAN security model"). To onboard a new camera, run
`./local_dev/run_stack.sh --add-camera <name>` and follow
[`docs/onboarding-a-camera.md`](docs/onboarding-a-camera.md).

To reach the admin viewer by a friendly, no-port name — **`https://hushai.local/`** — just run
**`./local_dev/serve.sh`** (macOS): one command that generates + trusts the TLS cert, sets the
Bonjour name + a 443→8070 redirect, then starts the stack on the LAN. It's idempotent (re-running
skips whatever's already set up; `./local_dev/serve.sh --check` reports status without changing
anything). **Linux/Windows:** see [`docs/friendly-url-linux.md`](docs/friendly-url-linux.md) and
[`docs/friendly-url-windows.md`](docs/friendly-url-windows.md).

Or run each service manually:

```bash
# 1. Ingest server (accepts segments; also applies migrations)
cargo run -p hushai-backend            # :8080

# 2. Transcription + embedding worker (drains backlog, then keeps up)
cargo run -p hushai-worker

# 3. RAG endpoint
cargo run -p hushai-rag                # :8090
curl -s -X POST localhost:8090/v1/rag/query \
  -H 'content-type: application/json' \
  -d '{"query":"what did they say about the cameras?","top_k":8}' | jq

# 4. Viewer (unified browser app: NVR timeline + chat)
cargo run -p hushai-viewer             # http://127.0.0.1:8070 (or https://hushai.local/ — see --lan / setup_hostname.sh)
```

## Test

```bash
export DATABASE_URL=postgres://localhost/hushai
cargo test --workspace                 # unit + live-DB integration (skips DB tests if unset)
```

## Notes

- **Embedding dimension is fixed at 1024** (`mxbai-embed-large` / BGE-large) to match
  `transcript_sentences.embedding vector(1024)`; every vector is dimension-checked before write.
- The workspace is on `sqlx 0.9` + `pgvector 0.4.2`; embeddings bind as native `pgvector::Vector`
  over the binary protocol — there are no `::vector` text casts left.
- The worker is crash-safe: `segment_transcription_status` + `FOR UPDATE SKIP LOCKED` + a claim
  lease mean a killed worker's in-flight segment is re-leased and finished on restart, with no
  duplicate sentences (atomic delete-then-insert per segment).
