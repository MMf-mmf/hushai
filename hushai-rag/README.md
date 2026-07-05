# hushai-rag

The **assistant service** for Hushai: an Axum server (`:8090`) that answers natural-language
questions over everything the worker extracted from your recordings — the transcript
embeddings, face/plate/object sightings, flagged events, and the reflection analytics. It
embeds a question, retrieves the relevant evidence from Postgres (pgvector + deterministic
rollups), grounds a local [Rig](https://docs.rig.rs) LLM on it so answers never hallucinate,
returns real source citations, and can read the answer aloud with on-device neural TTS. A
single chat box auto-routes each question to the right capability (recordings, people,
objects, plates, events, reflection) — there are no manual tabs.

It reuses [`hushai-backend`](../hushai-backend/README.md) as a library for the DB pool,
config, TLS, and observability, and runs the same migrations. The browser client is
[`hushai-viewer`](../hushai-viewer/README.md), whose chat panel reverse-proxies to this
service; the Android voice assistant is a second client over the same `/v1/rag/chat` SSE
stream. See the root [`AGENTS.md`](../AGENTS.md) `hushai-rag/` row for the deeper design notes.

## Endpoints

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/v1/rag/query` | Single-shot grounded Q&A: embed → retrieve → answer + citations. Optional `agent_id`, `filters`, `top_k`, `exhaustive`, `caller`, `tz_offset_secs`. |
| POST | `/v1/rag/chat` | Multi-turn chat over recordings, streamed as **SSE** (`session` → `sources` → `token`… → `done`). Auto-routes each message; persists sessions/messages. |
| GET | `/v1/rag/chat/sessions` | Recent conversations, newest first (for the UI history list). |
| GET | `/v1/rag/chat/sessions/{id}/messages` | Full transcript of a session incl. persisted citations, so a reopened chat re-renders its deep-links. |
| GET | `/v1/rag/agents` | The selectable agents (id/name/description) for the UI picker. |
| POST | `/v1/tts` | Synthesize `{ "text": … }` to a 16-bit PCM WAV (`audio/wav`). `503` when TTS isn't loaded. |
| GET | `/healthz` | Liveness (`"ok"`). |
| GET | `/readyz` | `200` when the DB is reachable, `503` otherwise (drain signal). |
| GET | `/metrics` | Prometheus metrics (shared `hushai_backend::observe`). |

Auth: when `RAG_TOKEN` is set every route requires `Authorization: Bearer <token>`
(constant-time compare). See [Configuration](#configuration) for the fail-closed rule on
non-loopback binds.

## Agent router

The chat box is bound to a synthetic **`auto`** agent. Per message, the handler
([`chat.rs`](src/chat.rs)) picks exactly one concrete capability and dispatches to it — the
user never chooses a tab:

1. **Deterministic pre-routes** first (exact-phrase intent detectors in [`routes.rs`](src/routes.rs)):
   "who was speaking", "what did we last discuss", and "what's my name / who am I" are pinned
   to `recordings` so their deterministic answers fire (the LLM router tends to mis-route these).
2. **Follow-up condensation** ([`RAG_QUERY_CONDENSE`](#configuration)) rewrites a follow-up
   into a standalone query before routing, carrying the subject/time from prior turns.
3. Everything else goes to `llm::classify_agent` — a cheap `qwen2.5:7b` classification prompt
   (`agents::ROUTER_PREAMBLE`) whose one-word reply is mapped to an id by
   `agents::parse_agent_label` (defaults to `recordings` on any ambiguity; never returns `auto`).

The routed capability is surfaced in the `session` SSE event as `routed_agent_id` and
persisted. (The single-shot `/v1/rag/query` path does **not** LLM-classify — it uses an explicit
`agent_id`, or the grounded default with the same deterministic identity/recency sub-routes.)

Agents live in code, not the DB ([`agents.rs`](src/agents.rs)) — each is a persona (system
prompt) + default retrieval scope + an `AgentKind` pipeline. Adding one is appending a struct.

| id | Name | Kind | Answers |
|----|------|------|---------|
| `auto` | Assistant | (router) | The unified box; classifies + dispatches to one below. |
| `recordings` | Recordings | Grounded | What was **said** — topics, summaries, who said what, who was speaking. Also handles the "last discuss" recency summary and the "what's my name" identity answer. |
| `reflection` | Reflection | Reflection | How **you** have been — mood, conversational skill, social patterns ("how have I been"). Narrates a deterministic analytics digest. |
| `objects` | Things seen | Objects | "When did I see a car / a red mug" — CLIP-text NN over `scene_objects`. |
| `people` | People | People | Face attribution: "when did I see Bob", "who was I with", "who have you seen". |
| `plates` | Plates | Plates | License plates: "when did I see plate ABC123" (matched by normalized string). |
| `events` | Activity | Events | The timeline of flagged events / alerts: "what happened yesterday", "any alerts". |

Every persona is strictly grounded (answer only from supplied evidence, decline otherwise)
and forbidden from emitting UUIDs, raw timestamps, or invented names. A `caller.kind:"voice"`
request appends `SPOKEN_STYLE_SUFFIX` (≤3 plain sentences, no markdown) for TTS.

## Module map

| Module | Responsibility |
|--------|----------------|
| [`lib.rs`](src/lib.rs) | Boot: config, DB pool + migrations, load LLM/embedder/TTS/CLIP, build router, serve (TLS + graceful shutdown). |
| [`routes.rs`](src/routes.rs) | `/v1/rag/query` + `/v1/tts` handlers; the deterministic intent detectors; auth; filter/owner resolution shared with chat. |
| [`chat.rs`](src/chat.rs) | `/v1/rag/chat` SSE pipeline + session/transcript read endpoints + the DB session store (`chat_sessions`/`chat_messages`). |
| [`agents.rs`](src/agents.rs) | The agent registry, personas, `AgentKind`, router preamble, and label parsing. |
| [`llm.rs`](src/llm.rs) | Rig/Ollama answer synthesis: per-agent `answer*`/`chat_stream`/`reflect*`, `classify_agent`, `condense`, and prompt assembly. |
| [`embed.rs`](src/embed.rs) | Query embedding via Rig's Ollama provider (1024-dim, same model as the worker). |
| [`retrieve.rs`](src/retrieve.rs) | pgvector NN + exhaustive listing SQL over `transcript_sentences`/`scene_objects`/`person_segments`/`plate_detections`/`events`; the `Source` citation type + display enrichment. |
| [`clip_text.rs`](src/clip_text.rs) | CLIP **text** tower (ORT `load-dynamic`) — embeds an object phrase into the worker's 512-d image space. |
| [`analytics.rs`](src/analytics.rs) | Deterministic "life digest" (talk balance, sentiment trend, social graph, rhythm) for the reflection agent — every number computed in SQL/Rust, not the LLM. |
| [`presence.rs`](src/presence.rs) | Deterministic count/timing/rhythm rollups for "how many times / when first-last / what times" (person/plate/object), so the model narrates a figure it's handed. |
| [`context.rs`](src/context.rs) | The assistant context layer: a "Facts" briefing (date, rosters, cameras) + same-segment vision annotation of retrieved passages. |
| [`speakers.rs`](src/speakers.rs) | Voice name↔id resolution, owner lookup, display labels, unnamed-speaker ordinals. |
| [`persons.rs`](src/persons.rs) | Face name↔id resolution + owner lookup (visual sibling of `speakers`). |
| [`plates.rs`](src/plates.rs) | Plate string↔id resolution by normalized exact + pg_trgm fuzzy match. |
| [`timeparse.rs`](src/timeparse.rs) | Tiny NL time-window parser (today/yesterday/this morning/last night/this-last week) for the recency path. |
| [`humanize.rs`](src/humanize.rs) | Render `start_unix_nanos` as "yesterday at 5:14 PM" once, server-side (so prompt, citation, and transcript agree). |
| [`config.rs`](src/config.rs) | `RagConfig::from_env()` — all env knobs + defaults. |
| [`state.rs`](src/state.rs) | `AppState` (pool, embedder, llm, cfg, optional tts, optional clip). |
| [`tts.rs`](src/tts.rs) | Kokoro TTS engine (sherpa-onnx) → 16-bit WAV. |

## Retrieval & context

The grounded path embeds the question in the same **1024-dim** space the worker used
(`mxbai-embed-large`), then runs a pgvector cosine (`<=>`) nearest-neighbour search over
`transcript_sentences` using the HNSW index (`ef_search` raised above `top_k` for filtered
recall). Matches beyond `RAG_DISTANCE_THRESHOLD` are pruned so weak passages are neither cited
nor grounded on. Retrieved passages are enriched once (`retrieve::enrich_for_display`) with the
resolved speaker name + humanized time, and optionally annotated with same-segment vision
(`context::enrich_sources_with_vision`). A per-turn "Facts" briefing (`context::assemble_briefing`)
is prepended to the chat prompt. All context layers are bounded and env-gated — disabling them
yields byte-identical prompts.

Multi-turn chat re-anchors retrieval on the **latest** message each turn; prior turns are given
to the model only as history for coreference. `RAG_QUERY_CONDENSE` (default on) first rewrites a
follow-up into a standalone query so "…and the week before?" doesn't re-embed bare and mis-route.
Non-semantic paths exist too: exhaustive "everything Bob said" listings, gap-grouped
last-conversation summaries, deterministic presence counts, and the analytics digest.

## Text-to-speech

Spoken answers use **Kokoro-82M** run on-device via the `sherpa-onnx` crate (fully offline). The
engine is loaded once at startup and held warm in `AppState`; a missing model is non-fatal
(`/v1/tts` just returns `503` and the client shows text). Fetch the bundle with
[`local_dev/fetch_tts_model.sh`](../local_dev/fetch_tts_model.sh) → `models/kokoro-en-v0_19`
(`model.onnx`, `voices.bin`, `tokens.txt`, `espeak-ng-data/`). Output is 24 kHz mono. Tune with
`RAG_TTS_*` (see below).

Audition speakers to pick `RAG_TTS_SID` with the dev example
([`examples/tts_audition.rs`](examples/tts_audition.rs)):

```bash
RAG_TTS_DIR=models/kokoro-en-v0_19 \
  cargo run -p hushai-rag --example tts_audition -- "Some sentence." 5 6 9 10
afplay /tmp/kokoro_sid6.wav   # am_michael (the default, sid 6)
```

## Configuration

Read from the environment (`RagConfig::from_env`; the root `.env` and `hushai-backend/.env` are
auto-loaded). DB config is reused from `hushai_backend::config::Config`, so **`DATABASE_URL`** is
required (with `BLOB_DIR`/`DEVICE_TOKEN` for the shared loader).

**LLM & embeddings**

| Var | Default | Effect |
|-----|---------|--------|
| `RAG_LLM_MODEL` | `qwen2.5:7b` | Ollama chat model for answers/routing (faithful extractive attribution; overrides the smaller worker model). |
| `EMBED_MODEL` | `mxbai-embed-large` | Query embedding model — **must match the worker's** (1024-dim). |
| `OLLAMA_BASE_URL` | `http://localhost:11434` | Base Ollama URL (fallback for the two below). |
| `EMBED_OLLAMA_BASE_URL` | = `OLLAMA_BASE_URL` | Ollama instance used to embed the query. |
| `LLM_OLLAMA_BASE_URL` | = `OLLAMA_BASE_URL` | Ollama instance used for answer generation (split from embed under load). |
| `RAG_LLM_TEMPERATURE` | `0.0` | Sampling temp for every Rig agent (0 = deterministic routing + answers). |
| `RAG_LLM_SEED` | *(unset)* | Optional Ollama `options.seed` for extra determinism. |
| `REFLECTION_LLM_MODEL` | *(unset)* | Larger model for reflection digest→coaching; falls back to `RAG_LLM_MODEL`. |

**Server, auth & TLS**

| Var | Default | Effect |
|-----|---------|--------|
| `RAG_BIND_ADDR` | `0.0.0.0:8090` | Listen address/port. |
| `RAG_TOKEN` | *(unset)* | Bearer token; when unset, auth is off. |
| `RAG_ALLOW_INSECURE` | `false` | Allow starting **without** a token on a non-loopback bind (otherwise it refuses — the archive would be world-open). |
| `RAG_TLS_CERT_PATH` / `RAG_TLS_KEY_PATH` | *(unset)* | Native TLS (fallback: `TLS_CERT_PATH`/`TLS_KEY_PATH`). Both set ⇒ HTTPS. |

**Retrieval & chat**

| Var | Default | Effect |
|-----|---------|--------|
| `RAG_TOP_K_DEFAULT` | `8` | Default passages retrieved. |
| `RAG_DISTANCE_THRESHOLD` | `0.6` | Cosine-distance cutoff for text matches. |
| `RAG_HNSW_EF_SEARCH` | `100` | HNSW `ef_search` for filtered recall. |
| `RAG_QUERY_TIMEOUT_MS` | `10000` | Per-query `statement_timeout`. |
| `RAG_QUERY_CONDENSE` | `true` | Rewrite follow-ups into standalone queries before routing/retrieval. |
| `RAG_CHAT_HISTORY_TURNS` | `8` | Trailing turns loaded into LLM context (`2×` messages). |
| `RAG_CHAT_MAX_MESSAGE_CHARS` | `4000` | Reject longer chat messages. |
| `RAG_RECENCY_SCAN_LIMIT` / `RAG_RECENCY_MAX_SENTENCES` / `RAG_RECENCY_MAX_CHARS` | `400` / `40` / `4000` | Bounds for the "what did we last discuss" summary. |

**Reflection / analytics & owner**

| Var | Default | Effect |
|-----|---------|--------|
| `ANALYSIS_WINDOW_DAYS_DEFAULT` | `90` | Reflection window when no time filter is given. |
| `CONVERSATION_GAP_SECS` | `300` | Silence gap that starts a new conversation. |
| `ANALYSIS_TZ_OFFSET_SECS` | `0` | Fixed UTC offset for hour/day bucketing + time phrasing (overridden per-request by `tz_offset_secs`). |
| `OWNER_SPEAKER_ID` / `OWNER_SPEAKER_NAME` | *(unset)* | Owner voice for reflection/identity (a DB "This is me" mark wins over these). |
| `OWNER_PERSON_ID` / `OWNER_PERSON_NAME` | *(unset)* | Owner face for "who was I with". |
| `RAG_PERSON_TOP_K_DEFAULT` / `RAG_PLATE_TOP_K_DEFAULT` | `50` / `50` | Default sightings per person/plate query. |

**TTS (Kokoro)**

| Var | Default | Effect |
|-----|---------|--------|
| `RAG_TTS_ENABLED` | `true` | Load TTS + serve `/v1/tts`. |
| `RAG_TTS_DIR` | `models/kokoro-en-v0_19` | Kokoro bundle directory. |
| `RAG_TTS_SID` | `6` | Speaker id (6 = am_michael; 5 am_adam; 9/10 British males). |
| `RAG_TTS_SPEED` | `1.0` | Speech-rate multiplier. |
| `RAG_TTS_THREADS` | `2` | onnxruntime intra-op threads. |

**Objects (CLIP text tower)**

| Var | Default | Effect |
|-----|---------|--------|
| `RAG_OBJECTS_ENABLED` | `true` | Load the CLIP text tower + enable the `objects` agent (else `503`). |
| `CLIP_TEXT_MODEL_PATH` | `./models/clip_vit_b32_text.onnx` | CLIP text tower ONNX (same checkpoint as the worker's image tower). |
| `CLIP_TOKENIZER_PATH` | `./models/clip_tokenizer.json` | HF CLIP tokenizer. |
| `ORT_DYLIB_PATH` | `…/libonnxruntime.1.20.0.dylib` | ONNX Runtime 1.20 dylib `ort` dlopen()s (distinct from sherpa's bundled 1.17.1). |
| `RAG_OBJECT_DISTANCE_THRESHOLD` | `0.75` | Cosine cutoff for object NN (CLIP is looser than text). |
| `RAG_OBJECT_TOP_K_DEFAULT` | `12` | Default sightings per object query. |

**Context layer**

| Var | Default | Effect |
|-----|---------|--------|
| `RAG_CONTEXT_BRIEFING_ENABLED` | `true` | Prepend the "Facts" briefing to the chat prompt. |
| `RAG_CONTEXT_MAX_CHARS` | `1200` | Cap on the briefing. |
| `RAG_CONTEXT_ROSTER_MAX` | `12` | Max named entries per roster (voices/people). |
| `RAG_CONTEXT_VISION_ENRICH_ENABLED` | `true` | Annotate passages with same-segment vision. |

## Running

Needs a running **Ollama** (with the models below) and the Hushai **Postgres** (migrations run
automatically on boot). From the workspace root:

```bash
export DATABASE_URL=postgres://localhost/hushai   # + BLOB_DIR / DEVICE_TOKEN via the shared config
cargo run -p hushai-rag                            # listens on :8090
curl -s localhost:8090/healthz                     # ok
curl -s localhost:8090/v1/rag/query \
  -H 'content-type: application/json' \
  -d '{"query":"what did we discuss yesterday?"}'
```

To bring up the whole stack (infra + backend + worker + **rag on :8090** + viewer), run
[`../local_dev/run_stack.sh`](../local_dev/run_stack.sh) — it starts Ollama, pulls the required
models with `--pull`, mints a `RAG_TOKEN`, and health-checks `:8090/healthz`.

## Testing

```bash
cargo test -p hushai-rag
```

Unit tests always run (agent routing/`parse_agent_label`, the intent detectors, LLM prompt
assembly, WAV encoding, humanize/presence). Integration tests under [`tests/`](tests) are
**skip-gated on `DATABASE_URL`** (they connect, run the backend migrations, seed under a unique
device, and clean up): `retrieve.rs` (pgvector ordering), `chat.rs` (session store round-trip),
`persons_retrieve.rs`, `presence.rs`, `reflection.rs`. `clip_text.rs` additionally gates on the
provisioned CLIP model/tokenizer (and `CLIP_TEST_IMAGE` for the decisive cross-modal proof).

## Models required

| Model | Provisioning | Needed for |
|-------|--------------|------------|
| `qwen2.5:7b` (LLM) | Ollama (`ollama pull qwen2.5:7b`) | Answers + auto-routing. Required. |
| `mxbai-embed-large` | Ollama | Query embedding. Required; must match the worker. |
| Kokoro-82M TTS | [`local_dev/fetch_tts_model.sh`](../local_dev/fetch_tts_model.sh) → `models/kokoro-en-v0_19` | `/v1/tts` (optional — `503` if absent). |
| CLIP ViT-B/32 text tower + tokenizer + ORT 1.20 | [`export_clip.py`](../local_dev/export_clip.py) + [`fetch_clip_tokenizer.sh`](../local_dev/fetch_clip_tokenizer.sh) + [`fetch_onnxruntime.sh`](../local_dev/fetch_onnxruntime.sh) | The `objects` agent (optional — `503` if absent). |

The LLM/embeddings are reached over HTTP through Rig's Ollama provider; the TTS and CLIP models
run in-process via ONNX Runtime.
