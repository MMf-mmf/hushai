# Hushai workspace

A Cargo workspace for the Hushai data-intake + retrieval system.

| crate | role |
|-------|------|
| [`hushai-backend`](hushai-backend/) | Durable, idempotent **segment-ingest** server (camera→backend contract v0.1.0). Owns the DB schema. |
| [`hushai-worker`](hushai-worker/) | Durable, resumable, idempotent **transcription + embedding** worker: drains stored segments → `transcript_sentences`. |
| [`hushai-rag`](hushai-rag/) | **RAG** service: `POST /v1/rag/query` — grounded Q&A over the transcripts with source citations. |

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
```

## Test

```bash
export DATABASE_URL=postgres://localhost/hushai
cargo test --workspace                 # unit + live-DB integration (skips DB tests if unset)
```

## Notes

- **Embedding dimension is fixed at 1024** (`mxbai-embed-large` / BGE-large) to match
  `transcript_sentences.embedding vector(1024)`; every vector is dimension-checked before write.
- Vectors are bound to Postgres as text + `::vector` cast (avoids a `pgvector`/`sqlx` version
  conflict — `pgvector 0.4.2` pins `sqlx 0.9`, the workspace uses `sqlx 0.8`).
- The worker is crash-safe: `segment_transcription_status` + `FOR UPDATE SKIP LOCKED` + a claim
  lease mean a killed worker's in-flight segment is re-leased and finished on restart, with no
  duplicate sentences (atomic delete-then-insert per segment).
